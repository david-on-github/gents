//! The operator's per-call answer to "Allow this plugin to read this path?".
//!
//! A model's plugin call that names a path outside the working folder and the
//! allowed folders waits here when its request came from an interactive chat.
//! The question is one request file in the gents home, the answer one decision
//! file; the desktop app and `gents chat` list the pending questions and write
//! the decision, so the same home is the only channel and no pack, document or
//! model can answer for the operator. "Always allow" is applied by [`decide`]
//! on the answering side, adding the folder to the operator's list; the
//! waiting call only learns allow or deny.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::allowed;
use crate::pack::BindAccess;

/// How long a call waits for an answer before it gives up and fails.
pub const WAIT: Duration = Duration::from_secs(10 * 60);

tokio::task_local! {
    static INTERACTIVE: ();
}

/// Runs `future` as an interactive chat's call when `interactive`, so a
/// plugin call inside it may ask the operator.
pub async fn scope_interactive<F: std::future::Future>(interactive: bool, future: F) -> F::Output {
    if interactive {
        INTERACTIVE.scope((), future).await
    } else {
        future.await
    }
}

/// Whether the running call came from an interactive chat.
pub fn interactive() -> bool {
    INTERACTIVE.try_with(|_| ()).is_ok()
}

/// One pending question.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub id: String,
    pub plugin: String,
    /// The exact file or folder the call names.
    pub path: String,
    /// The folder "always allow" remembers.
    pub folder: PathBuf,
    pub access: BindAccess,
    #[serde(default)]
    pub session_id: Option<String>,
    pub created_at_ms: u64,
}

impl Request {
    pub fn new(
        plugin: &str,
        resolved: &allowed::Resolved,
        access: BindAccess,
        session_id: Option<String>,
    ) -> Self {
        Self {
            id: uuid::Uuid::new_v4().simple().to_string(),
            plugin: plugin.to_owned(),
            path: resolved.target.display().to_string(),
            folder: resolved.folder().to_path_buf(),
            access,
            session_id,
            created_at_ms: now_ms(),
        }
    }

    /// The question as the operator reads it.
    pub fn prompt(&self) -> String {
        let verb = match self.access {
            BindAccess::Read => "read",
            BindAccess::ReadWrite => "read and write",
        };
        format!("Allow {} to {verb} {}?", self.plugin, self.path)
    }
}

#[derive(Serialize, Deserialize)]
struct Decision {
    allow: bool,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

fn dir(home: &Path) -> PathBuf {
    home.join(crate::home::PLUGIN_APPROVALS_DIR_NAME)
}

fn request_file(home: &Path, id: &str) -> PathBuf {
    dir(home).join(format!("{id}.json"))
}

fn decision_file(home: &Path, id: &str) -> PathBuf {
    dir(home).join(format!("{id}.decision"))
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("approval file has no folder")?;
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    std::io::Write::write_all(&mut temp, bytes)?;
    temp.persist(path)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Removes a question's files whenever the waiting call ends, however it ends.
struct Cleanup<'a> {
    home: &'a Path,
    id: &'a str,
}

impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(request_file(self.home, self.id));
        let _ = std::fs::remove_file(decision_file(self.home, self.id));
    }
}

/// Files `request` and waits up to `wait` for the operator; true when they
/// allowed it.
pub async fn ask(home: &Path, request: &Request, wait: Duration) -> Result<bool> {
    write_atomic(
        &request_file(home, &request.id),
        &serde_json::to_vec(request)?,
    )?;
    let _cleanup = Cleanup {
        home,
        id: &request.id,
    };
    let deadline = tokio::time::Instant::now() + wait;
    let mut pause = Duration::from_millis(20);
    loop {
        match std::fs::read(decision_file(home, &request.id)) {
            Ok(bytes) => {
                let decision: Decision = serde_json::from_slice(&bytes)
                    .context("the operator's answer is unreadable")?;
                return Ok(decision.allow);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("reading the operator's answer"),
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "the operator did not answer in time"
        );
        tokio::time::sleep(pause).await;
        pause = (pause * 2).min(Duration::from_millis(250));
    }
}

/// The unanswered questions, oldest first. A question older than [`WAIT`]
/// belongs to a call that already gave up and is skipped.
pub fn pending(home: &Path) -> Result<Vec<Request>> {
    let entries = match std::fs::read_dir(dir(home)) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).context("listing plugin approval requests"),
    };
    let oldest = now_ms().saturating_sub(WAIT.as_millis() as u64);
    let mut requests = Vec::new();
    for entry in entries {
        let path = entry.context("listing plugin approval requests")?.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        // A file removed or half-written between listing and reading is not pending.
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let Ok(request) = serde_json::from_slice::<Request>(&bytes) else {
            continue;
        };
        if request.created_at_ms >= oldest && !decision_file(home, &request.id).exists() {
            requests.push(request);
        }
    }
    requests.sort_by_key(|request| request.created_at_ms);
    Ok(requests)
}

/// Answers question `id`. With `always` an allow also adds the question's
/// folder to the operator's list, keeping any wider access it already has.
pub fn decide(home: &Path, id: &str, allow: bool, always: bool) -> Result<()> {
    anyhow::ensure!(
        id.chars().all(|c| c.is_ascii_alphanumeric()),
        "{id:?} is not a question id"
    );
    let bytes = std::fs::read(request_file(home, id))
        .with_context(|| format!("question {id} is not waiting for an answer"))?;
    let request: Request = serde_json::from_slice(&bytes).context("the question is unreadable")?;
    if allow && always {
        let held = allowed::list(home)?
            .into_iter()
            .find(|entry| entry.path == request.folder)
            .map(|entry| entry.access);
        allowed::add(
            home,
            &request.folder,
            request.access.max(held.unwrap_or_default()),
        )?;
    }
    write_atomic(
        &decision_file(home, id),
        &serde_json::to_vec(&Decision { allow })?,
    )
}

#[cfg(test)]
#[path = "approval_tests.rs"]
mod tests;
