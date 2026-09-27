//! `terminal_state`: whether the stage ran to completion.

use gents_protocol::request_lifecycle::RequestLifecycleState;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::eval::checks::{
    excerpt, graded, graded_reason_codes, grader, Check, CheckDescription, CheckVerdict,
    EXCERPT_CHARS,
};
use crate::eval::runner::executor::StageEvidence;

/// Params: `{}`. Scores 10000 when the stage completed and 0 otherwise, with
/// feedback naming why it did not.
pub struct TerminalState;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Params {}

impl Check for TerminalState {
    fn name(&self) -> &'static str {
        "terminal_state"
    }

    fn version(&self) -> &'static str {
        "1"
    }

    fn describe(&self) -> CheckDescription {
        CheckDescription {
            name: self.name().into(),
            version: self.version().into(),
            summary: "Scores 10000 when the stage completed, else 0 with the failure kind, provider reason and last tool error.".into(),
            params_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            reads: vec![
                "stage:terminal_state".into(),
                "stage:failure_kind".into(),
                "stage:provider_reason".into(),
                "stage:tool_calls".into(),
            ],
            reason_codes: graded_reason_codes(&[]),
        }
    }

    fn evaluate(&self, params: &Value, stage: &StageEvidence) -> CheckVerdict {
        // An omitted `params` reaches the check as null.
        if !params.is_null() {
            if let Err(error) = serde_json::from_value::<Params>(params.clone()) {
                return grader("bad_params", error.to_string());
            }
        }
        if stage.terminal_state == Some(RequestLifecycleState::Completed)
            && stage.failure_kind.is_none()
        {
            return graded(1, 1, None);
        }
        let mut text = format!(
            "stage ended {}",
            stage
                .terminal_state
                .map_or("unknown", |state| state.as_str())
        );
        if let Some(kind) = stage.failure_kind {
            text.push_str(&format!("; failure_kind {}", kind.as_str()));
        }
        if let Some(reason) = stage.provider_reason {
            text.push_str(&format!("; provider_reason {}", reason.as_str()));
        }
        let last_error = stage.tool_calls.iter().rev().find(|call| {
            call.status.as_deref() == Some("failed") || call.tool_failure_class.is_some()
        });
        if let Some(call) = last_error {
            let message = match &call.result {
                Value::String(result) => result.clone(),
                Value::Null => call.tool_failure_class.clone().unwrap_or_default(),
                result => result.to_string(),
            };
            text.push_str(&format!(
                "; last tool error: {}: {}",
                call.tool_name,
                excerpt(&message, EXCERPT_CHARS)
            ));
        }
        graded(0, 1, Some(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::runner::embedded::observe::ToolCallEvidence;
    use crate::eval::runner::scripted::ScriptedExecutor;
    use crate::eval::{OutcomeKind, ProviderReason};

    fn completed() -> StageEvidence {
        ScriptedExecutor::passed_evidence("did:x", "s1", "items", vec![])
            .stages
            .remove(0)
    }

    fn failed(kind: OutcomeKind, reason: Option<ProviderReason>) -> StageEvidence {
        ScriptedExecutor::failed_evidence("did:x", "s1", kind, reason)
            .stages
            .remove(0)
    }

    fn call(name: &str, status: &str, result: &str) -> ToolCallEvidence {
        ToolCallEvidence {
            tool_name: name.into(),
            status: Some(status.into()),
            lifecycle_state: None,
            tool_failure_class: None,
            started_at: None,
            completed_at: None,
            args: Value::Null,
            result: Value::String(result.into()),
        }
    }

    #[test]
    fn a_completed_stage_passes_with_no_feedback() {
        for params in [json!({}), Value::Null] {
            let verdict = TerminalState.evaluate(&params, &completed());
            assert_eq!(
                (verdict.kind, verdict.score_bp, verdict.feedback),
                (OutcomeKind::Passed, Some(10_000), None),
                "{params}"
            );
        }
    }

    #[test]
    fn a_failed_stage_scores_zero_and_names_its_failure() {
        let verdict = TerminalState.evaluate(
            &json!({}),
            &failed(OutcomeKind::Provider, Some(ProviderReason::Rejected)),
        );
        assert_eq!(
            (verdict.kind, verdict.score_bp),
            (OutcomeKind::ModelAcceptance, Some(0))
        );
        let text = verdict.feedback.unwrap_or_default();
        assert!(text.contains("stage ended failed"), "{text}");
        assert!(text.contains("failure_kind provider"), "{text}");
        assert!(text.contains("provider_reason rejected"), "{text}");
        assert!(!text.contains("tool error"), "{text}");
    }

    #[test]
    fn the_last_tool_error_is_quoted_bounded() {
        let mut stage = failed(OutcomeKind::Tool, None);
        stage.tool_calls = vec![
            call("read", "failed", "first error"),
            call(
                "write",
                "failed",
                &format!("permission denied {}", "x".repeat(5_000)),
            ),
            call("list", "completed", "ok"),
        ];
        let text = TerminalState
            .evaluate(&json!({}), &stage)
            .feedback
            .unwrap_or_default();
        assert!(
            text.contains("last tool error: write: permission denied"),
            "{text}"
        );
        assert!(!text.contains("first error"), "{text}");
        assert!(text.len() < 400, "{}", text.len());
    }

    #[test]
    fn params_other_than_empty_are_bad_params() {
        let verdict = TerminalState.evaluate(&json!({"state": "completed"}), &completed());
        assert_eq!(
            (verdict.kind, verdict.score_bp, &verdict.raw["reason_code"]),
            (OutcomeKind::Grader, None, &json!("bad_params"))
        );
    }
}
