//! `tool_calls_expected`: which tools a stage called, graded per requirement.

use serde::Deserialize;
use serde_json::{json, Value};

use crate::eval::checks::{
    excerpt, graded, graded_reason_codes, grader, Check, CheckDescription, CheckVerdict,
};
use crate::eval::runner::executor::StageEvidence;

/// Params: `{ "required": [<tool>], "forbidden": [<tool>], "max_calls": <u64>? }`.
/// Each required tool called, each forbidden tool not called, and the call
/// count within `max_calls` is one requirement; the score is the satisfied
/// fraction.
pub struct ToolCallsExpected;

/// Calls listed in feedback before the rest are only counted.
const LISTED_CALLS: usize = 12;
/// Chars of a call's arguments quoted in feedback.
const ARGS_CHARS: usize = 60;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Params {
    #[serde(default)]
    required: Vec<String>,
    #[serde(default)]
    forbidden: Vec<String>,
    #[serde(default)]
    max_calls: Option<usize>,
}

impl Check for ToolCallsExpected {
    fn name(&self) -> &'static str {
        "tool_calls_expected"
    }

    fn version(&self) -> &'static str {
        "1"
    }

    fn describe(&self) -> CheckDescription {
        CheckDescription {
            name: self.name().into(),
            version: self.version().into(),
            summary: "Scores the fraction of tool requirements met: each required tool called, each forbidden tool not called, and at most max_calls calls.".into(),
            params_schema: json!({
                "type": "object",
                "properties": {
                    "required": {"type": "array", "items": {"type": "string"}},
                    "forbidden": {"type": "array", "items": {"type": "string"}},
                    "max_calls": {"type": ["integer", "null"], "minimum": 0}
                },
                "additionalProperties": false
            }),
            reads: vec!["stage:tool_calls".into()],
            reason_codes: graded_reason_codes(&[]),
        }
    }

    fn evaluate(&self, params: &Value, stage: &StageEvidence) -> CheckVerdict {
        let params: Params = match serde_json::from_value(params.clone()) {
            Ok(params) => params,
            Err(error) => return grader("bad_params", error.to_string()),
        };
        let total = params.required.len()
            + params.forbidden.len()
            + usize::from(params.max_calls.is_some());
        if total == 0 {
            return grader("bad_params", "no required, forbidden or max_calls to check");
        }
        let calls = &stage.tool_calls;
        let called = |name: &str| calls.iter().filter(|call| call.tool_name == name).count();
        let mut satisfied = 0;
        let mut problems = Vec::new();
        for name in &params.required {
            if called(name) == 0 {
                problems.push(if calls.is_empty() {
                    format!("{name}: not called")
                } else {
                    format!("{name}: wrong tool, the calls below never used it")
                });
            } else {
                satisfied += 1;
            }
        }
        for name in &params.forbidden {
            match called(name) {
                0 => satisfied += 1,
                count => problems.push(format!("{name}: forbidden but called {count} times")),
            }
        }
        if let Some(max) = params.max_calls {
            if calls.len() > max {
                problems.push(format!(
                    "{} tool calls, more than the {max} allowed",
                    calls.len()
                ));
            } else {
                satisfied += 1;
            }
        }
        let feedback = (!problems.is_empty()).then(|| {
            let mut text = "missing or extra:\n".to_string();
            for problem in problems {
                text.push_str(&format!("- {problem}\n"));
            }
            text.push_str(&match calls.len() {
                0 => "tool calls made: none\n".to_string(),
                count => format!("tool calls made: {count}\n"),
            });
            for (index, call) in calls.iter().take(LISTED_CALLS).enumerate() {
                let args = match &call.args {
                    Value::Null => String::new(),
                    Value::String(args) => args.clone(),
                    args => args.to_string(),
                };
                let status = call
                    .status
                    .as_deref()
                    .or(call.lifecycle_state.as_deref())
                    .unwrap_or("unknown");
                text.push_str(&format!(
                    "{}. {} {} -> {status}",
                    index + 1,
                    call.tool_name,
                    excerpt(&args, ARGS_CHARS)
                ));
                if let Some(class) = &call.tool_failure_class {
                    text.push_str(&format!(" ({class})"));
                }
                text.push('\n');
            }
            if calls.len() > LISTED_CALLS {
                text.push_str(&format!("… {} more\n", calls.len() - LISTED_CALLS));
            }
            text
        });
        graded(satisfied, total, feedback)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::runner::embedded::observe::ToolCallEvidence;
    use crate::eval::runner::scripted::ScriptedExecutor;
    use crate::eval::OutcomeKind;

    fn call(name: &str, args: &str, status: &str) -> ToolCallEvidence {
        ToolCallEvidence {
            tool_name: name.into(),
            status: Some(status.into()),
            lifecycle_state: None,
            tool_failure_class: None,
            started_at: None,
            completed_at: None,
            args: Value::String(args.into()),
            result: Value::Null,
        }
    }

    /// One completed stage that made `calls`.
    fn stage(calls: Vec<ToolCallEvidence>) -> StageEvidence {
        let mut evidence = ScriptedExecutor::passed_evidence("did:x", "s1", "items", vec![]);
        let mut stage = evidence.stages.remove(0);
        stage.tool_calls = calls;
        stage
    }

    fn feedback(verdict: &CheckVerdict) -> &str {
        verdict.feedback.as_deref().unwrap_or_default()
    }

    #[test]
    fn every_requirement_met_passes_with_full_score_and_no_feedback() {
        let verdict = ToolCallsExpected.evaluate(
            &json!({"required": ["write"], "forbidden": ["rm"], "max_calls": 2}),
            &stage(vec![
                call("search", "{}", "completed"),
                call("write", "{}", "completed"),
            ]),
        );
        assert_eq!(
            (verdict.kind, verdict.score_bp),
            (OutcomeKind::Passed, Some(10_000))
        );
        assert_eq!(verdict.raw["reason_code"], "met");
        assert_eq!(verdict.feedback, None);
    }

    #[test]
    fn the_score_is_the_satisfied_fraction_in_basis_points() {
        let verdict = ToolCallsExpected.evaluate(
            &json!({"required": ["a", "b"], "forbidden": ["c"]}),
            &stage(vec![call("a", "{}", "completed")]),
        );
        assert_eq!(
            (verdict.kind, verdict.score_bp),
            (OutcomeKind::ModelAcceptance, Some(6_666))
        );
        assert_eq!(
            (
                &verdict.raw["reason_code"],
                &verdict.raw["satisfied"],
                &verdict.raw["total"]
            ),
            (&json!("unmet"), &json!(2), &json!(3))
        );
    }

    #[test]
    fn a_stage_that_called_no_tool_is_told_the_tool_was_not_called() {
        let verdict = ToolCallsExpected.evaluate(&json!({"required": ["write"]}), &stage(vec![]));
        assert_eq!(verdict.score_bp, Some(0));
        let text = feedback(&verdict);
        assert!(text.contains("tool calls made: none"), "{text}");
        assert!(text.contains("write: not called"), "{text}");
    }

    #[test]
    fn calls_to_other_tools_are_listed_and_named_the_wrong_tool() {
        let verdict = ToolCallsExpected.evaluate(
            &json!({"required": ["write_finding"]}),
            &stage(vec![call("search", r#"{"q":"disk"}"#, "failed")]),
        );
        assert_eq!(verdict.score_bp, Some(0));
        let text = feedback(&verdict);
        assert!(
            text.contains(r#"1. search {"q":"disk"} -> failed"#),
            "{text}"
        );
        assert!(text.contains("write_finding: wrong tool"), "{text}");
    }

    #[test]
    fn forbidden_calls_and_calls_over_the_limit_are_named_as_extra() {
        let verdict = ToolCallsExpected.evaluate(
            &json!({"forbidden": ["rm"], "max_calls": 1}),
            &stage(vec![
                call("rm", "a", "completed"),
                call("rm", "b", "completed"),
            ]),
        );
        assert_eq!(verdict.score_bp, Some(0));
        let text = feedback(&verdict);
        assert!(text.contains("rm: forbidden but called 2 times"), "{text}");
        assert!(
            text.contains("2 tool calls, more than the 1 allowed"),
            "{text}"
        );
    }

    #[test]
    fn feedback_stays_bounded_however_many_calls_were_made() {
        let long = "x".repeat(5_000);
        let calls = (0..500)
            .map(|_| call("search", &long, "completed"))
            .collect();
        let verdict = ToolCallsExpected.evaluate(&json!({"required": ["write"]}), &stage(calls));
        let text = feedback(&verdict);
        assert!(text.len() <= 2_048, "{}", text.len());
        assert!(text.contains("500"), "the count survives: {text}");
        assert!(text.contains("write: wrong tool"), "{text}");
    }

    #[test]
    fn params_that_do_not_parse_or_require_nothing_are_a_grader_outcome() {
        for params in [
            json!({}),
            json!({"required": "write"}),
            json!({"required": ["write"], "extra": 1}),
        ] {
            let verdict = ToolCallsExpected.evaluate(&params, &stage(vec![]));
            assert_eq!(
                (verdict.kind, verdict.score_bp),
                (OutcomeKind::Grader, None),
                "{params}"
            );
            assert_eq!(verdict.raw["reason_code"], "bad_params", "{params}");
        }
    }

    #[test]
    fn the_schema_accepts_its_params_and_rejects_unknown_fields() {
        let validator =
            jsonschema::validator_for(&ToolCallsExpected.describe().params_schema).unwrap();
        assert!(validator.is_valid(&json!({"required": ["a"], "forbidden": [], "max_calls": 3})));
        assert!(!validator.is_valid(&json!({"required": ["a"], "extra": 1})));
        assert!(!validator.is_valid(&json!({"max_calls": -1})));
    }
}
