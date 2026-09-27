use crate::document_config::{TaskHook, TaskHookPhase};
use crate::lean_vocab_test::{
    lean_task_hook_admission_cases, lean_task_hook_recovery_cases, lean_task_hook_run_cases,
    LeanCommandResult, LeanHookAttempt, LeanTaskHook, LeanTaskOutcome, LeanTaskPrimaryError,
};
use crate::task_hooks::{
    effective_timeout_secs, recover_interrupted_task_hooks, run_task_hooks, HookAttempt,
    HookCommandResult, HookPrimaryError, OwnedWorkObservation, TaskAgentResult, TaskHookExec,
    TaskHookOutcome,
};

fn phase(name: &str) -> TaskHookPhase {
    match name {
        "before" => TaskHookPhase::Before,
        "after_success" => TaskHookPhase::AfterSuccess,
        "after_failure" => TaskHookPhase::AfterFailure,
        "finally" => TaskHookPhase::Finally,
        other => panic!("generated contract emitted an unknown hook phase {other:?}"),
    }
}

fn hook(generated: &LeanTaskHook) -> TaskHook {
    TaskHook {
        hook_id: generated.hook_id.clone(),
        phase: phase(&generated.phase),
        command: generated.command.clone(),
        timeout_secs: generated.timeout_secs,
    }
}

fn command_result(generated: &LeanCommandResult) -> HookCommandResult {
    match generated {
        LeanCommandResult::Exited { code } => HookCommandResult::Exited { code: Some(*code) },
        LeanCommandResult::LaunchFailed => HookCommandResult::LaunchFailed,
        LeanCommandResult::TimedOut => HookCommandResult::TimedOut,
        LeanCommandResult::Interrupted => HookCommandResult::Interrupted,
    }
}

fn outcome(generated: &LeanTaskOutcome) -> TaskHookOutcome {
    match generated {
        LeanTaskOutcome::Success => TaskHookOutcome::Success,
        LeanTaskOutcome::Failure { error } => TaskHookOutcome::Failure(match error {
            LeanTaskPrimaryError::Hook { hook_id } => HookPrimaryError::Hook(hook_id.clone()),
            LeanTaskPrimaryError::Agent => HookPrimaryError::Agent,
        }),
        LeanTaskOutcome::Interrupted => TaskHookOutcome::Interrupted,
    }
}

fn agent_result(name: &str) -> TaskAgentResult {
    match name {
        "success" => TaskAgentResult::Success,
        "failure" => TaskAgentResult::Failure,
        "cancelled" => TaskAgentResult::Cancelled,
        "interrupted" => TaskAgentResult::Interrupted,
        other => panic!("generated contract emitted an unknown agent result {other:?}"),
    }
}

/// Stands in for the host execution owner exactly as `TaskHooks.scriptedExec`
/// does: an unscripted occurrence is observed to have exited zero.
struct ScriptedExec {
    script: Vec<(String, HookCommandResult)>,
}

impl ScriptedExec {
    fn from_generated(script: &[crate::lean_vocab_test::LeanScriptedHookResult]) -> Self {
        Self {
            script: script
                .iter()
                .map(|entry| (entry.hook_id.clone(), command_result(&entry.result)))
                .collect(),
        }
    }
}

#[async_trait::async_trait]
impl TaskHookExec for ScriptedExec {
    async fn attempt(&self, hook: &TaskHook) -> HookAttempt {
        let result = self
            .script
            .iter()
            .find(|(hook_id, _)| hook_id == &hook.hook_id)
            .map(|(_, result)| result.clone())
            .unwrap_or(HookCommandResult::Exited { code: Some(0) });
        HookAttempt {
            hook_id: hook.hook_id.clone(),
            result,
            detail: String::new(),
        }
    }
}

fn assert_attempts(case: &str, phase: &str, actual: &[HookAttempt], expected: &[LeanHookAttempt]) {
    assert_eq!(
        actual
            .iter()
            .map(|attempt| attempt.hook_id.as_str())
            .collect::<Vec<_>>(),
        expected
            .iter()
            .map(|attempt| attempt.hook_id.as_str())
            .collect::<Vec<_>>(),
        "{case}: {phase} attempt order disagrees with the model's trace"
    );
    for (actual, expected) in actual.iter().zip(expected) {
        assert_eq!(
            actual.result,
            command_result(&expected.result),
            "{case}: {phase} attempt {:?} observed a different command result",
            expected.hook_id
        );
        assert_eq!(
            actual.result.succeeded(),
            expected.succeeded,
            "{case}: {phase} attempt {:?} disagrees on success",
            expected.hook_id
        );
    }
}

#[tokio::test]
async fn generated_task_hook_run_cases_drive_the_production_hook_executor() {
    let cases = lean_task_hook_run_cases();
    assert!(!cases.is_empty(), "task hook run cases must not be empty");
    for case in cases {
        let hooks = case.hooks.iter().map(hook).collect::<Vec<_>>();
        for (configured, generated) in hooks.iter().zip(&case.hooks) {
            assert_eq!(
                effective_timeout_secs(configured),
                generated.effective_timeout_secs,
                "{}: production timeout resolution disagrees with the model for hook {:?}",
                case.name,
                generated.hook_id
            );
        }
        let exec = ScriptedExec::from_generated(&case.script);
        let agent = agent_result(&case.agent);
        let run = run_task_hooks(&hooks, &exec, || async move {
            OwnedWorkObservation::Observed(agent)
        })
        .await
        .unwrap_or_else(|| {
            panic!(
                "{}: an observed agent result must produce a modeled run",
                case.name
            )
        });

        assert_eq!(
            run.agent_result.is_some(),
            case.expected_agent_ran,
            "{}: the executor disagrees on whether the owned work ran",
            case.name
        );
        assert_attempts(
            &case.name,
            "before",
            &run.before_attempted,
            &case.before_attempted,
        );
        assert_attempts(
            &case.name,
            "after_success",
            &run.after_success_attempted,
            &case.after_success_attempted,
        );
        assert_attempts(
            &case.name,
            "after_failure",
            &run.after_failure_attempted,
            &case.after_failure_attempted,
        );
        assert_attempts(
            &case.name,
            "finally",
            &run.finally_attempted,
            &case.finally_attempted,
        );
        assert_eq!(
            run.cleanup_errors(),
            case.cleanup_errors,
            "{}: cleanup errors disagree with the model",
            case.name
        );
        assert_eq!(
            run.outcome,
            outcome(&case.expected_outcome),
            "{}: primary outcome disagrees with the model",
            case.name
        );
        let final_outcome = run.final_outcome();
        assert_eq!(
            final_outcome,
            outcome(&case.expected_final_outcome),
            "{}: final outcome disagrees with the model",
            case.name
        );
        assert_eq!(
            final_outcome
                .terminal_outcome()
                .request_lifecycle_state()
                .as_str(),
            case.expected_request_state,
            "{}: the terminal owner would write a different request state",
            case.name
        );
    }
}

#[tokio::test]
async fn generated_task_hook_recovery_cases_drive_the_production_hook_executor() {
    let cases = lean_task_hook_recovery_cases();
    assert!(
        !cases.is_empty(),
        "task hook recovery cases must not be empty"
    );
    for case in cases {
        let hooks = case.hooks.iter().map(hook).collect::<Vec<_>>();
        let observed = case
            .observed
            .iter()
            .map(|attempt| HookAttempt {
                hook_id: attempt.hook_id.clone(),
                result: command_result(&attempt.result),
                detail: String::new(),
            })
            .collect::<Vec<_>>();
        let exec = ScriptedExec::from_generated(&case.script);
        let (terminal, attempted) =
            recover_interrupted_task_hooks(case.started, &hooks, &observed, &exec).await;
        assert_eq!(
            terminal.request_lifecycle_state().as_str(),
            case.expected_request_state,
            "{}: recovery would write a different request state",
            case.name
        );
        assert_eq!(
            attempted
                .iter()
                .map(|attempt| attempt.hook_id.as_str())
                .collect::<Vec<_>>(),
            case.expected_remaining
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            "{}: recovery selected different remaining cleanup",
            case.name
        );
        assert_attempts(&case.name, "recovery", &attempted, &case.attempted);
    }
}

#[test]
fn generated_task_hook_admission_cases_fence_production_timeout_resolution() {
    let cases = lean_task_hook_admission_cases();
    assert!(
        !cases.is_empty(),
        "task hook admission cases must not be empty"
    );
    for case in cases {
        for generated in &case.hooks {
            assert_eq!(
                effective_timeout_secs(&hook(generated)),
                generated.effective_timeout_secs,
                "{}: production timeout resolution disagrees with the model for hook {:?}",
                case.name,
                generated.hook_id
            );
        }
    }
}
