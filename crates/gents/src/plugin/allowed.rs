//! What a data-chosen plugin path may reach.
//!
//! A path that comes from data (a graph node's source document, a model's tool
//! arguments) is untrusted. A call reads exactly what it names: one file is
//! exposed alone, a folder as that folder. The path is reachable without a
//! question when, symlinks resolved, it lies inside the session's working
//! folder (read-only) or inside a folder in the operator's list. The list is
//! one operator-owned file in the gents home, written by `gents plugin dirs`
//! and the desktop "Allowed folders" panel; no pack, document or model writes
//! it. Anything else needs the operator's approval for that call (see
//! [`super::approval`]) or is refused. The gents home is never reachable.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::BoundDir;
use crate::pack::BindAccess;

/// One operator-allowed folder and the most access plugins get in it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllowedDir {
    pub path: PathBuf,
    pub access: BindAccess,
}

#[derive(Default, Serialize, Deserialize)]
struct Stored {
    #[serde(default)]
    dirs: Vec<AllowedDir>,
}

fn file(home: &Path) -> PathBuf {
    home.join(crate::home::ALLOWED_DIRS_FILE_NAME)
}

/// The folders the operator configured, sorted by path; empty means the
/// default applies.
pub fn list(home: &Path) -> Result<Vec<AllowedDir>> {
    let path = file(home);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    let mut stored: Stored = serde_json::from_slice(&bytes)
        .with_context(|| format!("{} is not a valid allowed-folders file", path.display()))?;
    stored.dirs.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(stored.dirs)
}

fn save(home: &Path, dirs: Vec<AllowedDir>) -> Result<()> {
    std::fs::create_dir_all(home).with_context(|| format!("creating {}", home.display()))?;
    let bytes = serde_json::to_vec_pretty(&Stored { dirs })?;
    let mut temp = tempfile::NamedTempFile::new_in(home).context("staging the allowed folders")?;
    std::io::Write::write_all(&mut temp, &bytes).context("staging the allowed folders")?;
    let path = file(home);
    temp.persist(&path)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Allows `path` (an existing folder, stored in canonical form) with
/// `access`, replacing any earlier entry for it.
pub fn add(home: &Path, path: &Path, access: BindAccess) -> Result<AllowedDir> {
    let path = path
        .canonicalize()
        .with_context(|| format!("{} does not exist or cannot be read", path.display()))?;
    anyhow::ensure!(path.is_dir(), "{} is not a folder", path.display());
    let entry = AllowedDir { path, access };
    let mut dirs = list(home)?;
    dirs.retain(|existing| existing.path != entry.path);
    dirs.push(entry.clone());
    save(home, dirs)?;
    Ok(entry)
}

/// Stops allowing `path`; false when it was not in the list. The path is
/// matched as given and, when it still exists, in canonical form.
pub fn remove(home: &Path, path: &Path) -> Result<bool> {
    let canonical = path.canonicalize().ok();
    let mut dirs = list(home)?;
    let before = dirs.len();
    dirs.retain(|entry| entry.path != path && Some(&entry.path) != canonical.as_ref());
    let removed = dirs.len() != before;
    if removed {
        save(home, dirs)?;
    }
    Ok(removed)
}

/// The operator's own home folder, resolved the same way on macOS and Linux.
pub fn user_home() -> Option<PathBuf> {
    dirs::home_dir().and_then(|home| home.canonicalize().ok())
}

/// A path resolved to the canonical file or folder it names.
#[derive(Clone, Debug)]
pub struct Resolved {
    pub target: PathBuf,
    pub is_dir: bool,
}

impl Resolved {
    /// The folder an "always allow" remembers: the folder itself, or the
    /// folder holding the file.
    pub fn folder(&self) -> &Path {
        if self.is_dir {
            &self.target
        } else {
            self.target.parent().unwrap_or(&self.target)
        }
    }
}

/// Canonicalizes `requested` (`~` expanded, a relative path taken from
/// `workdir`) and refuses the gents home and anything containing it. The
/// error is one sentence for the model or the invocation record.
pub fn resolve(
    requested: &str,
    workdir: Option<&Path>,
    user_home: Option<&Path>,
    gents_home: &Path,
) -> Result<Resolved, String> {
    let requested = requested.trim();
    let path = if requested == "~" || requested.starts_with("~/") {
        let home = user_home.ok_or("this system has no home folder to expand ~ against")?;
        home.join(requested.trim_start_matches('~').trim_start_matches('/'))
    } else if Path::new(requested).is_absolute() {
        PathBuf::from(requested)
    } else {
        workdir
            .ok_or_else(|| format!("{requested} is not an absolute path; give the full path"))?
            .join(requested)
    };
    let target = path
        .canonicalize()
        .map_err(|_| format!("{requested} does not exist or cannot be read"))?;
    if let Ok(home) = gents_home.canonicalize() {
        if target.starts_with(&home) || home.starts_with(&target) {
            return Err(format!("{requested} is the gents home or contains it"));
        }
    }
    Ok(Resolved {
        is_dir: target.is_dir(),
        target,
    })
}

/// What a session reaches without asking: its working folder read-only and
/// the operator's list.
#[derive(Clone, Debug)]
pub struct Scope {
    entries: Vec<AllowedDir>,
}

impl Scope {
    pub fn load(gents_home: &Path, workdir: Option<&Path>) -> Result<Self> {
        let mut entries = list(gents_home)?;
        if let Some(workdir) = workdir.and_then(|dir| dir.canonicalize().ok()) {
            entries.push(AllowedDir {
                path: workdir,
                access: BindAccess::Read,
            });
        }
        Ok(Self { entries })
    }

    /// The most access `target` is allowed, or `None` outside every folder.
    pub fn granted(&self, target: &Path) -> Option<BindAccess> {
        self.entries
            .iter()
            .filter(|entry| {
                entry
                    .path
                    .canonicalize()
                    .is_ok_and(|root| target.starts_with(root))
            })
            .map(|entry| entry.access)
            .max()
    }
}

/// Binds `resolved` for one call with `access` of it: the folder itself, or
/// the one file alone.
pub fn bind(resolved: &Resolved, access: BindAccess) -> Result<BoundDir, String> {
    if resolved.is_dir {
        return Ok(BoundDir {
            dir: resolved.target.clone(),
            target: resolved.target.clone(),
            access,
            _private: None,
        });
    }
    if !resolved.target.is_file() {
        return Err(format!(
            "{} is not a file or folder",
            resolved.target.display()
        ));
    }
    BoundDir::for_file(&resolved.target, access).map_err(|error| format!("{error:#}"))
}

#[cfg(test)]
#[path = "allowed_tests.rs"]
mod tests;
