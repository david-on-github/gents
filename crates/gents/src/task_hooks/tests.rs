use tokio_util::sync::CancellationToken;

use super::{
    effective_timeout_secs, HookCommandResult, ManagedTaskHookExec, TaskHookExec,
    DEFAULT_TASK_HOOK_TIMEOUT_SECS,
};
use crate::document_config::{TaskHook, TaskHookPhase};

fn hook(hook_id: &str, command: &[&str], timeout_secs: Option<i64>) -> TaskHook {
    TaskHook {
        hook_id: hook_id.to_string(),
        phase: TaskHookPhase::Before,
        command: command.iter().map(|part| (*part).to_string()).collect(),
        timeout_secs,
    }
}

fn exec() -> ManagedTaskHookExec {
    ManagedTaskHookExec::new(std::env::temp_dir(), CancellationToken::new())
}

#[test]
fn absent_timeout_resolves_to_the_executor_default() {
    assert_eq!(
        effective_timeout_secs(&hook("h", &["true"], None)),
        DEFAULT_TASK_HOOK_TIMEOUT_SECS
    );
    assert_eq!(effective_timeout_secs(&hook("h", &["true"], Some(7))), 7);
}

#[tokio::test]
async fn host_exit_status_decides_hook_success() {
    let ok = exec()
        .attempt(&hook("ok", &["sh", "-c", "exit 0"], Some(30)))
        .await;
    assert_eq!(ok.hook_id, "ok");
    assert_eq!(ok.result, HookCommandResult::Exited { code: Some(0) });
    assert!(ok.result.succeeded());

    let bad = exec()
        .attempt(&hook(
            "bad",
            &["sh", "-c", "echo boom >&2; exit 3"],
            Some(30),
        ))
        .await;
    assert_eq!(bad.result, HookCommandResult::Exited { code: Some(3) });
    assert!(!bad.result.succeeded());
    assert!(
        bad.detail.contains("boom"),
        "hook detail must carry captured output, got {:?}",
        bad.detail
    );
}

#[tokio::test]
async fn a_hook_that_cannot_launch_is_a_hook_error() {
    let attempt = exec()
        .attempt(&hook(
            "missing",
            &["gents-task-hook-command-that-does-not-exist"],
            Some(30),
        ))
        .await;
    assert_eq!(attempt.result, HookCommandResult::LaunchFailed);
    assert!(!attempt.result.succeeded());
    assert!(!attempt.detail.is_empty());
}

#[tokio::test]
async fn a_hook_past_its_timeout_is_a_hook_error() {
    let attempt = exec()
        .attempt(&hook("slow", &["sh", "-c", "sleep 30"], Some(1)))
        .await;
    assert_eq!(attempt.result, HookCommandResult::TimedOut);
    assert!(!attempt.result.succeeded());
}

/// Both arms observe the host through the command itself: a launched command
/// leaves the marker file, so its absence is the evidence nothing ran.
async fn unrepresentable_deadline_case(timeout_secs: i64) {
    let directory = tempfile::tempdir().expect("hook marker directory");
    let marker = directory.path().join("launched");
    let attempt =
        ManagedTaskHookExec::new(directory.path().to_path_buf(), CancellationToken::new())
            .attempt(&hook(
                "unbounded",
                &[
                    "sh",
                    "-c",
                    &format!("touch {}", marker.to_str().expect("utf-8 marker path")),
                ],
                Some(timeout_secs),
            ))
            .await;
    assert_eq!(
        attempt.result,
        HookCommandResult::LaunchFailed,
        "a timeout of {timeout_secs}s must refuse the launch, not run unbounded"
    );
    assert!(!attempt.result.succeeded());
    assert!(
        attempt.detail.contains(&timeout_secs.to_string()),
        "the operator must see the timeout that could not be applied, got {:?}",
        attempt.detail
    );
    assert!(
        !marker.exists(),
        "no command may launch when its deadline cannot be represented"
    );
}

#[tokio::test]
async fn a_timeout_with_no_representable_duration_refuses_to_launch() {
    unrepresentable_deadline_case(i64::MAX).await;
}

#[tokio::test]
async fn a_timeout_past_the_representable_deadline_refuses_to_launch() {
    unrepresentable_deadline_case(10_000_000_000_000).await;
}

#[tokio::test]
async fn a_cancelled_hook_reports_an_unknown_outcome() {
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let attempt = ManagedTaskHookExec::new(std::env::temp_dir(), cancellation)
        .attempt(&hook("cancelled", &["sh", "-c", "sleep 30"], Some(30)))
        .await;
    assert_eq!(attempt.result, HookCommandResult::Interrupted);
    assert!(!attempt.result.succeeded());
}
