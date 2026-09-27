use crate::document_config::{TaskHook, TaskHookPhase};
use crate::lean_vocab_test::{
    lean_task_hook_admission_cases, lean_task_hook_run_cases, LeanCommandResult, LeanHookAttempt,
    LeanTaskHook, LeanTaskOutcome, LeanTaskPrimaryError,
};
use crate::task_hooks::{
    effective_timeout_secs, run_task_hooks, HookAttempt, HookCommandResult, HookPrimaryError,
    OwnedWorkObservation, TaskAgentResult, TaskHookExec, TaskHookOutcome,
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

/// One thing the orchestration asked an external owner to do, in the order it
/// asked. The returned traces are the orchestration's own account of itself;
/// only this records what the host and the owned execution were actually told.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Invocation {
    Hook(String),
    Work,
}

/// Stands in for the host execution owner exactly as `TaskHooks.scriptedExec`
/// does: an unscripted occurrence is observed to have exited zero.
struct ScriptedExec {
    script: Vec<(String, HookCommandResult)>,
    invoked: std::sync::Mutex<Vec<Invocation>>,
}

impl ScriptedExec {
    fn from_generated(script: &[crate::lean_vocab_test::LeanScriptedHookResult]) -> Self {
        Self {
            script: script
                .iter()
                .map(|entry| (entry.hook_id.clone(), command_result(&entry.result)))
                .collect(),
            invoked: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn record(&self, invocation: Invocation) {
        self.invoked
            .lock()
            .expect("hook invocation trace")
            .push(invocation);
    }

    fn invoked(&self) -> Vec<Invocation> {
        self.invoked.lock().expect("hook invocation trace").clone()
    }
}

#[async_trait::async_trait]
impl TaskHookExec for ScriptedExec {
    async fn attempt(&self, hook: &TaskHook) -> HookAttempt {
        self.record(Invocation::Hook(hook.hook_id.clone()));
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

fn attempted_hook_ids(attempts: &[LeanHookAttempt]) -> Vec<&str> {
    attempts
        .iter()
        .map(|attempt| attempt.hook_id.as_str())
        .collect()
}

fn configured_phase<'a>(hooks: &'a [LeanTaskHook], hook_id: &str) -> &'a str {
    hooks
        .iter()
        .find(|hook| hook.hook_id == hook_id)
        .map(|hook| hook.phase.as_str())
        .unwrap_or_else(|| panic!("the executor attempted an unconfigured hook {hook_id:?}"))
}

fn assert_attempts(case: &str, phase: &str, actual: &[HookAttempt], expected: &[LeanHookAttempt]) {
    assert_eq!(
        actual
            .iter()
            .map(|attempt| attempt.hook_id.as_str())
            .collect::<Vec<_>>(),
        attempted_hook_ids(expected),
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
        let recorder = &exec;
        let agent = agent_result(&case.agent);
        let run = run_task_hooks(&hooks, &exec, || async move {
            recorder.record(Invocation::Work);
            OwnedWorkObservation::Observed(agent)
        })
        .await
        .unwrap_or_else(|| {
            panic!(
                "{}: an observed agent result must produce a modeled run",
                case.name
            )
        });

        let invoked = exec.invoked();
        assert_eq!(
            invoked
                .iter()
                .filter(|invocation| **invocation == Invocation::Work)
                .count(),
            usize::from(case.expected_agent_ran),
            "{}: the owned work ran a different number of times than the model records an agent attempt",
            case.name
        );
        assert_eq!(
            run.agent_result.is_some(),
            case.expected_agent_ran,
            "{}: the executor disagrees on whether the owned work ran",
            case.name
        );
        for (phase, expected) in [
            ("before", &case.before_attempted),
            ("after_success", &case.after_success_attempted),
            ("after_failure", &case.after_failure_attempted),
            ("finally", &case.finally_attempted),
        ] {
            let attempted = invoked
                .iter()
                .filter_map(|invocation| match invocation {
                    Invocation::Hook(hook_id)
                        if configured_phase(&case.hooks, hook_id) == phase =>
                    {
                        Some(hook_id.as_str())
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(
                attempted,
                attempted_hook_ids(expected),
                "{}: the host was asked to run a different {phase} sequence than the model's trace",
                case.name
            );
        }
        if case.expected_agent_ran {
            let ahead_of_work = invoked
                .iter()
                .take_while(|invocation| **invocation != Invocation::Work)
                .filter_map(|invocation| match invocation {
                    Invocation::Hook(hook_id) => Some(hook_id.as_str()),
                    Invocation::Work => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(
                ahead_of_work,
                attempted_hook_ids(&case.before_attempted),
                "{}: only the modeled before phase may run ahead of the owned work",
                case.name
            );
        }
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
