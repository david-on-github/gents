//! `captured_fields_match`: whether a documents capture's rows hold the
//! expected field values, graded per row and expectation.

use regex::Regex;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::eval::checks::{
    excerpt, graded, graded_reason_codes, grader, Check, CheckDescription, CheckVerdict,
    EXCERPT_CHARS,
};
use crate::eval::runner::executor::{CaptureResult, StageEvidence};

/// Params: `{ "name": <capture>, "expect": [{ "field", "equals" | "contains"
/// | "matches" }], "min_rows": <u64>? }`. Every (row, expectation) pair is
/// one requirement, over at least `min_rows` rows (default 1): a row the
/// capture lacks fails every expectation. `field` is a dotted path into the
/// row; `contains` and `matches` read a string field as its text and any
/// other value as its JSON.
pub struct CapturedFieldsMatch;

/// Rows whose mismatches feedback spells out.
const SHOWN_ROWS: usize = 3;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Params {
    name: String,
    expect: Vec<Expectation>,
    #[serde(default = "one")]
    min_rows: usize,
}

fn one() -> usize {
    1
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Expectation {
    field: String,
    #[serde(default)]
    equals: Option<Value>,
    #[serde(default)]
    contains: Option<String>,
    #[serde(default)]
    matches: Option<String>,
}

enum Test {
    Equals(Value),
    Contains(String),
    Matches(Regex),
}

impl Test {
    fn holds(&self, actual: &Value) -> bool {
        match self {
            Self::Equals(expected) => actual == expected,
            Self::Contains(needle) => text(actual).contains(needle.as_str()),
            Self::Matches(pattern) => pattern.is_match(&text(actual)),
        }
    }

    fn describe(&self) -> String {
        match self {
            Self::Equals(expected) => format!("equals {expected}"),
            Self::Contains(needle) => format!("contains {needle:?}"),
            Self::Matches(pattern) => format!("matches /{pattern}/"),
        }
    }
}

fn text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// `expectation` as a test, or why it is not one.
fn test(expectation: Expectation) -> Result<(String, Test), String> {
    let field = expectation.field;
    match (
        expectation.equals,
        expectation.contains,
        expectation.matches,
    ) {
        (Some(expected), None, None) => Ok((field, Test::Equals(expected))),
        (None, Some(needle), None) => Ok((field, Test::Contains(needle))),
        (None, None, Some(pattern)) => Regex::new(&pattern)
            .map(|pattern| (field.clone(), Test::Matches(pattern)))
            .map_err(|error| format!("field {field}: {error}")),
        _ => Err(format!(
            "field {field}: give exactly one of equals, contains or matches"
        )),
    }
}

fn lookup<'a>(row: &'a Value, field: &str) -> Option<&'a Value> {
    field.split('.').try_fold(row, |value, key| value.get(key))
}

impl Check for CapturedFieldsMatch {
    fn name(&self) -> &'static str {
        "captured_fields_match"
    }

    fn version(&self) -> &'static str {
        "1"
    }

    fn describe(&self) -> CheckDescription {
        CheckDescription {
            name: self.name().into(),
            version: self.version().into(),
            summary: "Scores the fraction of (row, expectation) pairs that hold over a documents capture's rows, counting rows below min_rows (default 1) as failing every expectation.".into(),
            params_schema: json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "the capture name"},
                    "expect": {
                        "type": "array",
                        "minItems": 1,
                        "items": {
                            "type": "object",
                            "properties": {
                                "field": {"type": "string", "description": "dotted path into the row"},
                                "equals": {},
                                "contains": {"type": "string"},
                                "matches": {"type": "string", "description": "a regex"}
                            },
                            "required": ["field"],
                            "oneOf": [
                                {"required": ["equals"]},
                                {"required": ["contains"]},
                                {"required": ["matches"]}
                            ],
                            "additionalProperties": false
                        }
                    },
                    "min_rows": {"type": "integer", "minimum": 0}
                },
                "required": ["name", "expect"],
                "additionalProperties": false
            }),
            reads: vec!["capture:documents".into()],
            reason_codes: graded_reason_codes(&[(
                "missing_capture",
                "grader: no documents capture of that name",
            )]),
        }
    }

    fn evaluate(&self, _params: &Value, _stage: &StageEvidence) -> CheckVerdict {
        grader("bad_params", "unimplemented")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::runner::scripted::ScriptedExecutor;
    use crate::eval::OutcomeKind;

    /// One completed stage whose `items` capture holds `rows`.
    fn stage(rows: Vec<Value>) -> StageEvidence {
        let mut evidence = ScriptedExecutor::passed_evidence("did:x", "s1", "items", rows);
        evidence.stages.remove(0)
    }

    fn feedback(verdict: &CheckVerdict) -> &str {
        verdict.feedback.as_deref().unwrap_or_default()
    }

    #[test]
    fn rows_that_hold_every_expectation_pass_with_no_feedback() {
        let verdict = CapturedFieldsMatch.evaluate(
            &json!({"name": "items", "expect": [
                {"field": "sku", "equals": "A1"},
                {"field": "note", "contains": "disk"},
                {"field": "payload.level", "matches": "^(high|critical)$"}
            ]}),
            &stage(vec![
                json!({"sku": "A1", "note": "disk full", "payload": {"level": "high"}}),
            ]),
        );
        assert_eq!(
            (verdict.kind, verdict.score_bp),
            (OutcomeKind::Passed, Some(10_000))
        );
        assert_eq!(verdict.feedback, None);
    }

    #[test]
    fn the_score_counts_every_row_and_expectation_pair() {
        let verdict = CapturedFieldsMatch.evaluate(
            &json!({"name": "items", "expect": [{"field": "sku", "equals": "A1"}, {"field": "qty", "equals": 2}]}),
            &stage(vec![json!({"sku": "A1", "qty": 2}), json!({"sku": "B2", "qty": 2})]),
        );
        assert_eq!(
            (verdict.kind, verdict.score_bp),
            (OutcomeKind::ModelAcceptance, Some(7_500))
        );
        let text = feedback(&verdict);
        assert!(
            text.contains(r#"row 1: sku expected equals "A1", got "B2""#),
            "{text}"
        );
        assert!(!text.contains("row 0"), "only what is missing: {text}");
    }

    #[test]
    fn an_absent_field_is_reported_as_absent() {
        let verdict = CapturedFieldsMatch.evaluate(
            &json!({"name": "items", "expect": [{"field": "sku", "contains": "A"}]}),
            &stage(vec![json!({})]),
        );
        assert_eq!(verdict.score_bp, Some(0));
        assert!(feedback(&verdict).contains("row 0: sku expected contains \"A\", got absent"));
    }

    #[test]
    fn rows_below_min_rows_fail_every_expectation_and_are_named_missing() {
        let verdict = CapturedFieldsMatch.evaluate(
            &json!({"name": "items", "expect": [{"field": "sku", "equals": "A1"}], "min_rows": 3}),
            &stage(vec![json!({"sku": "A1"})]),
        );
        assert_eq!(verdict.score_bp, Some(3_333));
        let text = feedback(&verdict);
        assert!(
            text.contains("items holds 1 rows, 3 required; rows 1..2 absent"),
            "{text}"
        );

        let empty = CapturedFieldsMatch.evaluate(
            &json!({"name": "items", "expect": [{"field": "sku", "equals": "A1"}]}),
            &stage(vec![]),
        );
        assert_eq!(empty.score_bp, Some(0), "min_rows defaults to 1");
        let vacuous = CapturedFieldsMatch.evaluate(
            &json!({"name": "items", "expect": [{"field": "sku", "equals": "A1"}], "min_rows": 0}),
            &stage(vec![]),
        );
        assert_eq!(vacuous.score_bp, Some(10_000));
    }

    #[test]
    fn feedback_details_the_first_rows_and_counts_the_rest() {
        let rows = (0..50).map(|_| json!({"sku": "x".repeat(1_000)})).collect();
        let verdict = CapturedFieldsMatch.evaluate(
            &json!({"name": "items", "expect": [{"field": "sku", "equals": "A1"}]}),
            &stage(rows),
        );
        let text = feedback(&verdict);
        assert!(text.len() <= 2_048, "{}", text.len());
        assert!(
            text.contains("row 2:") && !text.contains("row 3:"),
            "{text}"
        );
        assert!(text.contains("47 more rows with a mismatch"), "{text}");
    }

    #[test]
    fn a_missing_or_files_capture_is_a_grader_outcome() {
        let verdict = CapturedFieldsMatch.evaluate(
            &json!({"name": "other", "expect": [{"field": "sku", "equals": "A1"}]}),
            &stage(vec![]),
        );
        assert_eq!(
            (verdict.kind, verdict.score_bp, &verdict.raw["reason_code"]),
            (OutcomeKind::Grader, None, &json!("missing_capture"))
        );
    }

    #[test]
    fn malformed_expectations_are_bad_params() {
        for params in [
            json!({"name": "items", "expect": []}),
            json!({"name": "items", "expect": [{"field": "sku"}]}),
            json!({"name": "items", "expect": [{"field": "sku", "equals": 1, "contains": "1"}]}),
            json!({"name": "items", "expect": [{"field": "sku", "matches": "("}]}),
            json!({"name": "items", "expect": [{"field": "sku", "equals": 1}], "extra": 1}),
        ] {
            let verdict = CapturedFieldsMatch.evaluate(&params, &stage(vec![json!({})]));
            assert_eq!(
                (verdict.kind, verdict.score_bp, &verdict.raw["reason_code"]),
                (OutcomeKind::Grader, None, &json!("bad_params")),
                "{params}"
            );
        }
    }

    #[test]
    fn the_schema_requires_exactly_one_test_per_expectation() {
        let validator =
            jsonschema::validator_for(&CapturedFieldsMatch.describe().params_schema).unwrap();
        assert!(validator.is_valid(
            &json!({"name": "items", "expect": [{"field": "sku", "equals": null}], "min_rows": 2})
        ));
        assert!(!validator.is_valid(&json!({"name": "items", "expect": [{"field": "sku"}]})));
        assert!(!validator.is_valid(
            &json!({"name": "items", "expect": [{"field": "sku", "equals": 1, "matches": "1"}]})
        ));
        assert!(!validator.is_valid(&json!({"name": "items", "expect": []})));
    }
}
