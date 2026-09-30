#[path = "lean_vocab_test/support.rs"]
mod support;

pub(crate) use support::*;

#[cfg(test)]
#[path = "lean_vocab_test/tests.rs"]
mod tests;

#[cfg(test)]
#[path = "lean_vocab_test/request_execution_lease_policy.rs"]
mod request_execution_lease_policy;

#[cfg(test)]
#[path = "lean_vocab_test/task_hooks_policy.rs"]
mod task_hooks_policy;

#[cfg(test)]
#[path = "lean_vocab_test/task_hook_executor.rs"]
mod task_hook_executor;

#[cfg(test)]
#[path = "lean_vocab_test/task_hook_recovery.rs"]
mod task_hook_recovery;

/// Generated TaskHooks vocabulary mapped onto production types, for the
/// crate-internal consumers.
#[cfg(test)]
mod task_hook_vocab {
    use super::{LeanCommandResult, LeanTaskHook};
    /// The production phase each generated phase name encodes.
    pub(crate) fn lean_hook_phase(name: &str) -> crate::document_config::TaskHookPhase {
        use crate::document_config::TaskHookPhase;
        match name {
            "before" => TaskHookPhase::Before,
            "after_success" => TaskHookPhase::AfterSuccess,
            "after_failure" => TaskHookPhase::AfterFailure,
            "finally" => TaskHookPhase::Finally,
            other => panic!("generated contract emitted an unknown hook phase {other:?}"),
        }
    }

    impl LeanTaskHook {
        pub(crate) fn to_task_hook(&self) -> crate::document_config::TaskHook {
            crate::document_config::TaskHook {
                hook_id: self.hook_id.clone(),
                phase: lean_hook_phase(&self.phase),
                command: self.command.clone(),
                timeout_secs: self.timeout_secs,
            }
        }
    }

    impl LeanCommandResult {
        pub(crate) fn to_native(&self) -> crate::task_hooks::HookCommandResult {
            use crate::task_hooks::HookCommandResult;
            match self {
                Self::Exited { code } => HookCommandResult::Exited { code: Some(*code) },
                Self::LaunchFailed => HookCommandResult::LaunchFailed,
                Self::TimedOut => HookCommandResult::TimedOut,
                Self::Interrupted => HookCommandResult::Interrupted,
            }
        }
    }
}
#[cfg(test)]
pub(crate) use task_hook_vocab::lean_hook_phase;
