use super::{
    effective_timeout_secs, HookAttempt, HookCommandResult, ManagedTaskHookExec,
    TaskHookCancellation, TaskHookExec, TaskHookRun, DEFAULT_TASK_HOOK_TIMEOUT_SECS,
    FAILURE_REASON_OUTPUT_CAP,
};
use crate::document_config::{TaskHook, TaskHookPhase};

fn hook(hook_id: &str, command: &[&str], timeout_secs: Option<i64>) -> TaskHook {
    phased(hook_id, TaskHookPhase::Before, command, timeout_secs)
}

fn phased(
    hook_id: &str,
    phase: TaskHookPhase,
    command: &[&str],
    timeout_secs: Option<i64>,
) -> TaskHook {
    TaskHook {
        hook_id: hook_id.to_string(),
        phase,
        command: command.iter().map(|part| (*part).to_string()).collect(),
        timeout_secs,
    }
}

fn exec() -> ManagedTaskHookExec {
    ManagedTaskHookExec::new(std::env::temp_dir(), TaskHookCancellation::default())
}

fn failed_run(attempt: HookAttempt) -> TaskHookRun {
    TaskHookRun {
        before_attempted: vec![attempt],
        ..Default::default()
    }
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
    assert!(bad.detail.contains("boom"), "{:?}", bad.detail);
}

#[tokio::test]
async fn exit_127_and_a_missing_executable_are_reported_differently() {
    let exited = exec()
        .attempt(&hook("shell", &["sh", "-c", "exit 127"], Some(30)))
        .await;
    assert_eq!(exited.result, HookCommandResult::Exited { code: Some(127) });

    let directory = tempfile::tempdir().expect("hook directory");
    let missing = directory.path().join("no-such-hook");
    let missing = missing.to_str().expect("utf-8 path");
    let unlaunched = exec().attempt(&hook("missing", &[missing], Some(30))).await;
    assert_eq!(unlaunched.result, HookCommandResult::LaunchFailed);
    assert!(!unlaunched.detail.is_empty());

    let exited_reason = failed_run(exited).hook_failure_reason("shell");
    let unlaunched_reason = failed_run(unlaunched).hook_failure_reason("missing");
    assert!(
        exited_reason.contains("exited with status 127"),
        "{exited_reason:?}"
    );
    assert!(
        unlaunched_reason.contains("failed to launch"),
        "{unlaunched_reason:?}"
    );
}

#[tokio::test]
async fn a_signal_killed_hook_is_not_a_success() {
    let attempt = exec()
        .attempt(&hook("killed", &["sh", "-c", "kill -9 $$"], Some(30)))
        .await;
    assert_eq!(attempt.result, HookCommandResult::Exited { code: None });
    assert!(!attempt.result.succeeded());
    assert!(failed_run(attempt)
        .hook_failure_reason("killed")
        .contains("terminated by a signal"));
}

#[tokio::test]
async fn a_hook_past_its_timeout_is_a_hook_error() {
    let attempt = exec()
        .attempt(&hook("slow", &["sh", "-c", "sleep 30"], Some(1)))
        .await;
    assert_eq!(attempt.result, HookCommandResult::TimedOut);
}

async fn unrepresentable_deadline_case(timeout_secs: i64) {
    let directory = tempfile::tempdir().expect("hook marker directory");
    let marker = directory.path().join("launched");
    let attempt = ManagedTaskHookExec::new(
        directory.path().to_path_buf(),
        TaskHookCancellation::default(),
    )
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
    assert_eq!(attempt.result, HookCommandResult::LaunchFailed);
    assert!(
        attempt.detail.contains(&timeout_secs.to_string()),
        "{:?}",
        attempt.detail
    );
    assert!(!marker.exists());
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
async fn an_interrupt_cancels_ordinary_phases_before_launch_but_not_cleanup() {
    let directory = tempfile::tempdir().expect("hook marker directory");
    let marker = |name: &str| directory.path().join(name);
    let touch = |name: &str| format!("touch {}", marker(name).display());
    let cancellation = TaskHookCancellation::default();
    cancellation.interrupt();
    let exec = ManagedTaskHookExec::new(directory.path().to_path_buf(), cancellation.clone());

    let before = exec
        .attempt(&hook("prepare", &["sh", "-c", &touch("prepare")], Some(30)))
        .await;
    assert_eq!(before.result, HookCommandResult::Interrupted);
    assert!(
        !marker("prepare").exists(),
        "a cancelled hook never launches"
    );

    let cleanup = exec
        .attempt(&phased(
            "sweep",
            TaskHookPhase::Finally,
            &["sh", "-c", &touch("sweep")],
            Some(30),
        ))
        .await;
    assert_eq!(cleanup.result, HookCommandResult::Exited { code: Some(0) });
    assert!(
        marker("sweep").exists(),
        "an interrupt never cancels cleanup"
    );

    cancellation.shutdown();
    let after_shutdown = exec
        .attempt(&phased(
            "late",
            TaskHookPhase::Finally,
            &["sh", "-c", &touch("late")],
            Some(30),
        ))
        .await;
    assert_eq!(after_shutdown.result, HookCommandResult::Interrupted);
    assert!(!marker("late").exists());
}

#[tokio::test]
async fn an_interrupt_stops_a_running_hook() {
    let directory = tempfile::tempdir().expect("hook marker directory");
    let started = directory.path().join("started");
    let finished = directory.path().join("finished");
    let cancellation = TaskHookCancellation::default();
    let exec = ManagedTaskHookExec::new(directory.path().to_path_buf(), cancellation.clone());
    let script = format!(
        "touch {}; sleep 30; touch {}",
        started.display(),
        finished.display()
    );
    let interrupt = async {
        while !started.exists() {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        cancellation.interrupt();
    };
    let held = hook("held", &["sh", "-c", &script], Some(60));
    let (attempt, ()) = tokio::join!(exec.attempt(&held), interrupt);
    assert_eq!(attempt.result, HookCommandResult::Interrupted);
    assert!(!finished.exists());
}

#[test]
fn a_failure_reason_keeps_only_the_tail_of_hook_output() {
    let output = format!("{}END", "x".repeat(10 * 1024));
    let reason = failed_run(HookAttempt {
        hook_id: "noisy".to_string(),
        result: HookCommandResult::Exited { code: Some(1) },
        detail: output,
    })
    .hook_failure_reason("noisy");
    assert!(reason.ends_with("END"));
    assert!(reason.starts_with("task hook noisy exited with status 1\n…"));
    assert!(
        reason.len() <= FAILURE_REASON_OUTPUT_CAP + 64,
        "{}",
        reason.len()
    );
}
