use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::Utc;
use defra_node::EmbeddedNode;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::collection::Collection;
use crate::config_client::config_projection;
use crate::document_config::{Task, TaskHook, TaskHookPhase};
use crate::graphql::{escape_graphql_string, graphql_with_transaction_retry};
use crate::lifecycle::RequestTerminalOutcome;
use crate::managed_exec::{run_managed_exec, ManagedExecOutcome, ManagedExecRequest};
use crate::watcher::AgentRequest;

#[cfg(test)]
#[path = "task_hooks/tests.rs"]
mod tests;

/// `TaskHooks.defaultHookTimeoutSecs`: the executor's own bound for a hook that
/// configures none. Callers consume [`effective_timeout_secs`] rather than
/// re-deriving it.
pub(crate) const DEFAULT_TASK_HOOK_TIMEOUT_SECS: u64 = 120;

/// Captured stdout/stderr retained per attempt so a failing gate can name what
/// the command said without holding an unbounded host stream in memory.
const HOOK_OUTPUT_BYTE_CAP: usize = 64 * 1024;

/// `TaskHook.effectiveTimeout`. A nonpositive configured value is rejected by
/// `Task::validate`, so it cannot reach execution; this function still mirrors
/// the model there rather than substituting the default.
pub(crate) fn effective_timeout_secs(hook: &TaskHook) -> u64 {
    match hook.timeout_secs {
        None => DEFAULT_TASK_HOOK_TIMEOUT_SECS,
        Some(configured) => configured.max(0) as u64,
    }
}

/// `TaskHooks.CommandResult`. `Exited { code: None }` is a signal-terminated
/// child: no exit status exists, so it can never satisfy the modeled success
/// condition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HookCommandResult {
    Exited {
        code: Option<i64>,
    },
    LaunchFailed,
    TimedOut,
    /// Cancelled, or an outcome the executor cannot observe. Terminal and
    /// reported, never retried.
    Interrupted,
}

impl HookCommandResult {
    pub(crate) fn succeeded(&self) -> bool {
        matches!(self, Self::Exited { code: Some(0) })
    }

    fn describe(&self) -> String {
        match self {
            Self::Exited { code: Some(code) } => format!("exited with status {code}"),
            Self::Exited { code: None } => "was terminated by a signal".to_string(),
            Self::LaunchFailed => "failed to launch".to_string(),
            Self::TimedOut => "exceeded its timeout".to_string(),
            Self::Interrupted => "was interrupted".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HookAttempt {
    pub(crate) hook_id: String,
    pub(crate) result: HookCommandResult,
    /// Operator-facing detail: captured output, or the launch error.
    pub(crate) detail: String,
}

/// `TaskHooks.PrimaryError`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HookPrimaryError {
    Hook(String),
    Agent,
}

/// `TaskHooks.TaskOutcome`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) enum TaskHookOutcome {
    #[default]
    Success,
    Failure(HookPrimaryError),
    Interrupted,
}

impl TaskHookOutcome {
    /// `TaskOutcome.toRequestState`, expressed through the existing request
    /// terminal owner rather than a second terminal vocabulary.
    pub(crate) fn terminal_outcome(&self) -> RequestTerminalOutcome {
        match self {
            Self::Success => RequestTerminalOutcome::Completed,
            Self::Failure(_) => RequestTerminalOutcome::Failed,
            Self::Interrupted => RequestTerminalOutcome::Interrupted,
        }
    }
}

/// `TaskHooks.AgentResult`: one observation of the owned execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TaskAgentResult {
    Success,
    Failure,
    Cancelled,
    /// Active work whose completion state is unknown. No native owner reports
    /// it: distinguishing it from a cancellation needs a durable record of the
    /// interrupted execution's hook attempts, and nothing records one.
    #[allow(dead_code)]
    Interrupted,
}

/// What the owned execution reported to the after-phases. `Relinquished` is
/// outside `TaskHooks.runTask`: no agent result was observed and this execution
/// no longer owns the request, so remaining cleanup belongs to whichever owner
/// recovers it. No native owner selects that cleanup, because selecting it
/// needs durable attempt observations nothing records.
pub(crate) enum OwnedWorkObservation {
    Observed(TaskAgentResult),
    Relinquished,
}

/// `TaskHooks.RunResult`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct TaskHookRun {
    pub(crate) outcome: TaskHookOutcome,
    pub(crate) before_attempted: Vec<HookAttempt>,
    pub(crate) after_success_attempted: Vec<HookAttempt>,
    pub(crate) after_failure_attempted: Vec<HookAttempt>,
    pub(crate) finally_attempted: Vec<HookAttempt>,
    pub(crate) agent_result: Option<TaskAgentResult>,
}

impl TaskHookRun {
    /// `RunResult.cleanupErrors`.
    pub(crate) fn cleanup_errors(&self) -> Vec<String> {
        self.finally_attempted
            .iter()
            .filter(|attempt| !attempt.result.succeeded())
            .map(|attempt| attempt.hook_id.clone())
            .collect()
    }

    /// `RunResult.finalOutcome`.
    pub(crate) fn final_outcome(&self) -> TaskHookOutcome {
        match &self.outcome {
            TaskHookOutcome::Success => phase_outcome(&self.finally_attempted),
            other => other.clone(),
        }
    }

    /// What the operator's gate said, for the durable failure reason the
    /// terminal owner records. Captured output reaches the request result, not
    /// only the log.
    pub(crate) fn hook_failure_reason(&self, hook_id: &str) -> String {
        let attempt = self
            .before_attempted
            .iter()
            .chain(&self.after_success_attempted)
            .chain(&self.after_failure_attempted)
            .chain(&self.finally_attempted)
            .find(|attempt| attempt.hook_id == hook_id && !attempt.result.succeeded());
        match attempt {
            None => format!("task hook {hook_id} failed"),
            Some(attempt) => {
                let mut reason = format!("task hook {hook_id} {}", attempt.result.describe());
                if !attempt.detail.is_empty() {
                    reason.push('\n');
                    reason.push_str(&attempt.detail);
                }
                reason
            }
        }
    }
}

/// `TaskHooks.HookExec`: one attempt observation per configured occurrence.
/// Cwd, environment, launch, capture, timeout and process termination stay with
/// the host execution owner; implementations translate its outcome.
#[async_trait::async_trait]
pub(crate) trait TaskHookExec: Send + Sync {
    async fn attempt(&self, hook: &TaskHook) -> HookAttempt;
}

fn hooks_of_phase(hooks: &[TaskHook], phase: TaskHookPhase) -> impl Iterator<Item = &TaskHook> {
    hooks.iter().filter(move |hook| hook.phase == phase)
}

/// `TaskHooks.runPhase`.
async fn run_phase(
    hooks: &[TaskHook],
    phase: TaskHookPhase,
    exec: &dyn TaskHookExec,
) -> Vec<HookAttempt> {
    let mut attempts = Vec::new();
    for hook in hooks_of_phase(hooks, phase) {
        let attempt = exec.attempt(hook).await;
        let succeeded = attempt.result.succeeded();
        attempts.push(attempt);
        if !succeeded {
            break;
        }
    }
    attempts
}

/// `TaskHooks.runFinally`.
async fn run_finally(hooks: &[TaskHook], exec: &dyn TaskHookExec) -> Vec<HookAttempt> {
    let mut attempts = Vec::new();
    for hook in hooks_of_phase(hooks, TaskHookPhase::Finally) {
        attempts.push(exec.attempt(hook).await);
    }
    attempts
}

/// `TaskHooks.phaseOutcome`.
fn phase_outcome(attempts: &[HookAttempt]) -> TaskHookOutcome {
    match attempts.iter().find(|attempt| !attempt.result.succeeded()) {
        None => TaskHookOutcome::Success,
        Some(attempt) => match attempt.result {
            HookCommandResult::Interrupted => TaskHookOutcome::Interrupted,
            _ => TaskHookOutcome::Failure(HookPrimaryError::Hook(attempt.hook_id.clone())),
        },
    }
}

/// `TaskHooks.runTask`: the single orchestration. Preparation decides whether
/// the owned execution runs, its observation selects one ordinary after-phase,
/// then every cleanup hook is attempted.
pub(crate) async fn run_task_hooks<Work, Fut>(
    hooks: &[TaskHook],
    exec: &dyn TaskHookExec,
    work: Work,
) -> Option<TaskHookRun>
where
    Work: FnOnce() -> Fut,
    Fut: std::future::Future<Output = OwnedWorkObservation>,
{
    let mut run = TaskHookRun {
        before_attempted: run_phase(hooks, TaskHookPhase::Before, exec).await,
        ..Default::default()
    };
    match phase_outcome(&run.before_attempted) {
        TaskHookOutcome::Interrupted => run.outcome = TaskHookOutcome::Interrupted,
        TaskHookOutcome::Failure(error) => {
            run.outcome = TaskHookOutcome::Failure(error);
            run.after_failure_attempted = run_phase(hooks, TaskHookPhase::AfterFailure, exec).await;
        }
        TaskHookOutcome::Success => {
            let agent = match work().await {
                OwnedWorkObservation::Observed(agent) => agent,
                OwnedWorkObservation::Relinquished => return None,
            };
            run.agent_result = Some(agent);
            match agent {
                TaskAgentResult::Cancelled | TaskAgentResult::Interrupted => {
                    run.outcome = TaskHookOutcome::Interrupted
                }
                TaskAgentResult::Success => {
                    let after = run_phase(hooks, TaskHookPhase::AfterSuccess, exec).await;
                    run.outcome = phase_outcome(&after);
                    run.after_success_attempted = after;
                }
                TaskAgentResult::Failure => {
                    run.outcome = TaskHookOutcome::Failure(HookPrimaryError::Agent);
                    run.after_failure_attempted =
                        run_phase(hooks, TaskHookPhase::AfterFailure, exec).await;
                }
            }
        }
    }
    run.finally_attempted = run_finally(hooks, exec).await;
    Some(run)
}

/// Runs one configured occurrence through the host execution owner. The cwd is
/// the behavior's already-admitted host-tools root, so a hook cannot select a
/// workspace overlay of its own.
pub(crate) struct ManagedTaskHookExec {
    cwd: PathBuf,
    cancellation: CancellationToken,
}

impl ManagedTaskHookExec {
    pub(crate) fn new(cwd: PathBuf, cancellation: CancellationToken) -> Self {
        Self { cwd, cancellation }
    }
}

fn detail(stdout: &[u8], stderr: &[u8]) -> String {
    let mut detail = String::new();
    for stream in [stdout, stderr] {
        let text = String::from_utf8_lossy(stream);
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        if !detail.is_empty() {
            detail.push('\n');
        }
        detail.push_str(text);
    }
    detail
}

#[async_trait::async_trait]
impl TaskHookExec for ManagedTaskHookExec {
    /// Admission bounds a configured timeout only from below, so an admitted
    /// value can exceed what `chrono` can add to the current instant. Such a
    /// hook is refused before launch: the alternatives are a managed execution
    /// with no deadline at all, or a panic inside the addition.
    async fn attempt(&self, hook: &TaskHook) -> HookAttempt {
        let timeout_secs = effective_timeout_secs(hook);
        let Some(deadline_at) = i64::try_from(timeout_secs)
            .ok()
            .and_then(chrono::Duration::try_seconds)
            .and_then(|timeout| Utc::now().checked_add_signed(timeout))
        else {
            return HookAttempt {
                hook_id: hook.hook_id.clone(),
                result: HookCommandResult::LaunchFailed,
                detail: format!(
                    "a timeout of {timeout_secs}s has no representable deadline on this host"
                ),
            };
        };
        let outcome = run_managed_exec(ManagedExecRequest {
            argv: hook.command.clone(),
            cwd: self.cwd.clone(),
            deadline_at: Some(deadline_at),
            cancellation_token: self.cancellation.clone(),
            max_output_bytes: HOOK_OUTPUT_BYTE_CAP,
            stdin: Vec::new(),
            environment: None,
            tool_name: Some(format!("task_hook:{}", hook.hook_id)),
            live_output: None,
        })
        .await;
        let (result, detail) = match outcome {
            ManagedExecOutcome::Exited {
                code,
                stdout,
                stderr,
                ..
            } => (
                HookCommandResult::Exited {
                    code: code.map(i64::from),
                },
                detail(&stdout, &stderr),
            ),
            ManagedExecOutcome::TimedOut { stdout, stderr, .. } => {
                (HookCommandResult::TimedOut, detail(&stdout, &stderr))
            }
            ManagedExecOutcome::Cancelled { stdout, stderr, .. } => {
                (HookCommandResult::Interrupted, detail(&stdout, &stderr))
            }
            ManagedExecOutcome::SpawnFailed { error } => (HookCommandResult::LaunchFailed, error),
        };
        HookAttempt {
            hook_id: hook.hook_id.clone(),
            result,
            detail,
        }
    }
}

/// The hooks of the Task a claimed request came from, or none when no Task is
/// reachable. Only automated trigger lineage carries a Task reference, so a
/// manual run and a goal continuation resolve to no hooks.
pub(crate) async fn resolve_request_task_hooks(
    node: &EmbeddedNode,
    request: &AgentRequest,
) -> Result<Vec<TaskHook>> {
    if !request.has_automated_trigger_lineage() {
        return Ok(Vec::new());
    }
    let trigger_id = request
        .caused_by_trigger_id
        .as_deref()
        .context("automated trigger lineage has no trigger_id")?;
    let Some(task_id) = load_trigger_task_id(node, &request.agent_did, trigger_id).await? else {
        return Ok(Vec::new());
    };
    let Some(task) = load_task(node, &request.agent_did, &task_id).await? else {
        return Ok(Vec::new());
    };
    if task.hooks.is_empty() {
        return Ok(Vec::new());
    }
    // Admission is the sole entrance: rejected configuration runs no hook. The
    // owner is Task::validate, so a hook cannot be admitted by a second rule.
    task.validate()?;
    Ok(task.hooks)
}

async fn load_trigger_task_id(
    node: &EmbeddedNode,
    agent_did: &str,
    trigger_id: &str,
) -> Result<Option<String>> {
    #[derive(Deserialize)]
    struct TriggerTaskRow {
        task_id: Option<String>,
    }
    let response = graphql_with_transaction_retry(
        node,
        &format!(
            r#"{{ Trigger(filter: {{ agent_did: {{ _eq: "{}" }}, trigger_id: {{ _eq: "{}" }} }}, limit: 2) {{ task_id }} }}"#,
            escape_graphql_string(agent_did),
            escape_graphql_string(trigger_id),
        ),
        "load task hook trigger",
    )
    .await?;
    let rows: Vec<TriggerTaskRow> = crate::graphql::rows(&response, "Trigger")?;
    anyhow::ensure!(
        rows.len() <= 1,
        "ambiguous Trigger {trigger_id:?} for {agent_did:?}"
    );
    Ok(rows
        .into_iter()
        .next()
        .and_then(|row| row.task_id)
        .map(|task_id| task_id.trim().to_owned())
        .filter(|task_id| !task_id.is_empty()))
}

async fn load_task(node: &EmbeddedNode, agent_did: &str, task_id: &str) -> Result<Option<Task>> {
    let (fields, _) = config_projection(Collection::Task, None)?;
    let response = graphql_with_transaction_retry(
        node,
        &format!(
            "{{ Task(filter: {{ agent_did: {{_eq: \"{}\"}}, task_id: {{_eq: \"{}\"}} }}, limit: 2) {{ {} }} }}",
            escape_graphql_string(agent_did),
            escape_graphql_string(task_id),
            fields.join(" "),
        ),
        "load task hooks",
    )
    .await?;
    let rows: Vec<serde_json::Value> = crate::graphql::rows(&response, "Task")?;
    anyhow::ensure!(
        rows.len() <= 1,
        "ambiguous Task {task_id:?} for {agent_did:?}"
    );
    let Some(row) = rows.into_iter().next() else {
        return Ok(None);
    };
    let (_, canonical) = config_projection(Collection::Task, Some(&row))?;
    let canonical = canonical.context("Task row has no canonical configuration")?;
    serde_json::from_value(canonical)
        .with_context(|| format!("decoding Task {task_id:?}"))
        .map(Some)
}
