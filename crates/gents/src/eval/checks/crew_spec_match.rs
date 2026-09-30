//! `crew_spec_match`: how much of a declared configuration a subject built,
//! graded per requirement and reported per category.
//!
//! One check scores a whole specification because a stage names each check
//! once: the categories (completeness, templates, least privilege, receipt,
//! ...) are reported in `raw.categories` rather than as separate verdicts.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::{json, Value};

use crate::eval::checks::captured_fields_match::{test, Expectation, Test};
use crate::eval::checks::{
    bounded, excerpt, graded, graded_reason_codes, grader, Check, CheckDescription, CheckVerdict,
    EXCERPT_CHARS,
};
use crate::eval::runner::executor::{CaptureResult, StageEvidence};

/// Params (every list optional, at least one requirement overall):
///
/// - `present`: `[{capture, category?}]`, each capture must exist. A
///   documents capture that selects fields exists only when its collection
///   has them, so this checks a schema as well as rows.
/// - `rows`: `[{capture, key, id, category?, expect: [expectation]}]`: a row
///   whose `key` equals `id` exists (one requirement) and holds each
///   expectation (one requirement each).
/// - `links`: `[{capture, key, id, field, target_capture, target_key, expect, category?}]`
///   follows a stored ID in `field` and tests the selected target row.
/// - `agents`: `{behaviors, contexts, tools, expect: [{behavior_id, category?,
///   behavior: [...], context: [...], tools: [...]}]}`: the behavior exists,
///   and each expectation holds on it, on the Context it selects and on the
///   Tools that Context selects.
/// - `templates`: `[{capture, key, id, fields: [field], allowed: [name],
///   category?}]`: every `{{ doc.NAME }}` in each present template field of
///   that row names an allowed field (one requirement per field present).
/// - `receipt`: `{capture, fields: [field], ids: [id], category?}`: each id
///   appears in the text of some row's fields.
/// - `continuation`: `{max, category?}`: each of the `max` prods the stage
///   did not need is one requirement held.
///
/// An expectation is `captured_fields_match`'s: `{field, equals | contains |
/// matches}`, `field` a dotted path; a JSON string along the path is read as
/// the JSON it holds, and a missing path reads as `null`. A row's `id` (and a
/// behavior's) matches its stored ID exactly or as the `<scope>:<id>` suffix a
/// principal-scoped behavior ID carries. `raw.activity` counts the stage's tool calls, failed
/// tool calls and inference calls.
pub struct CrewSpecMatch;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Params {
    #[serde(default)]
    present: Vec<Present>,
    #[serde(default)]
    rows: Vec<RowSpec>,
    #[serde(default)]
    links: Vec<LinkSpec>,
    #[serde(default)]
    agents: Option<Agents>,
    #[serde(default)]
    templates: Vec<TemplateSpec>,
    #[serde(default)]
    receipt: Option<Receipt>,
    #[serde(default)]
    continuation: Option<Continuation>,
}

/// Grades how many continuation prods the stage needed: `max - prods` of
/// `max` requirements hold, so finishing unprompted scores full marks and a
/// prod costs a requirement rather than failing the trial.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Continuation {
    max: u32,
    #[serde(default)]
    category: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Present {
    capture: String,
    #[serde(default)]
    category: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RowSpec {
    capture: String,
    key: String,
    id: String,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    expect: Vec<Expectation>,
}

/// Follow the reference a source row actually stores; matching an unrelated
/// target row does not establish that the configured path reaches it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LinkSpec {
    capture: String,
    key: String,
    id: String,
    field: String,
    target_capture: String,
    target_key: String,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    expect: Vec<Expectation>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Agents {
    behaviors: String,
    contexts: String,
    tools: String,
    expect: Vec<AgentSpec>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentSpec {
    behavior_id: String,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    behavior: Vec<Expectation>,
    #[serde(default)]
    context: Vec<Expectation>,
    #[serde(default)]
    tools: Vec<Expectation>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TemplateSpec {
    capture: String,
    key: String,
    id: String,
    fields: Vec<String>,
    allowed: Vec<String>,
    #[serde(default)]
    category: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    capture: String,
    fields: Vec<String>,
    ids: Vec<String>,
    #[serde(default)]
    category: Option<String>,
}

/// Unmet requirements quoted in the verdict before the rest are only counted.
const SHOWN: usize = 60;

/// A dotted path into `row`, reading a JSON string on the way as its JSON.
/// A path the row lacks reads as `null`, so `equals: null` and `matches`
/// over `null` can require a field to be absent or unset.
fn lookup(row: &Value, field: &str) -> Value {
    let mut current = row.clone();
    for key in field.split('.') {
        if let Value::String(text) = &current {
            if let Ok(parsed) = serde_json::from_str::<Value>(text) {
                current = parsed;
            }
        }
        match current.get(key) {
            Some(next) => current = next.clone(),
            None => return Value::Null,
        }
    }
    current
}

/// Whether a stored ID is the declared one. Behavior IDs a principal creates
/// are stored scoped to it (`<DID>:<id>`), so a declared `id` also matches a
/// stored ID ending in `:<id>`; an exact match wins.
fn is_id(stored: &str, id: &str) -> bool {
    stored == id
        || stored
            .strip_suffix(id)
            .is_some_and(|scope| scope.ends_with(':'))
}

fn find<'a>(rows: &'a [Value], key: &str, id: &str) -> Option<&'a Value> {
    let stored = |row: &'a Value| row.get(key).and_then(Value::as_str);
    rows.iter().find(|row| stored(row) == Some(id)).or_else(|| {
        rows.iter()
            .find(|row| stored(row).is_some_and(|stored| is_id(stored, id)))
    })
}

/// Every `NAME` a template reads as `doc.NAME`.
fn doc_fields(template: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find("doc.") {
        let preceded = rest[..start]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.');
        let tail = &rest[start + 4..];
        let name: String = tail
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if !preceded && !name.is_empty() && !names.contains(&name) {
            names.push(name.clone());
        }
        rest = &tail[name.len()..];
    }
    names
}

#[derive(Default)]
struct Tally {
    categories: BTreeMap<String, (usize, usize)>,
    unmet: Vec<String>,
}

impl Tally {
    fn record(&mut self, category: Option<&str>, met: bool, what: impl FnOnce() -> String) {
        let entry = self
            .categories
            .entry(category.unwrap_or("completeness").to_owned())
            .or_default();
        entry.1 += 1;
        if met {
            entry.0 += 1;
        } else {
            self.unmet.push(format!(
                "[{}] {}",
                category.unwrap_or("completeness"),
                what()
            ));
        }
    }

    fn expectations(
        &mut self,
        category: Option<&str>,
        subject: &str,
        row: Option<&Value>,
        tests: &[(String, Test)],
    ) {
        for (field, test) in tests {
            let actual = row.map(|row| lookup(row, field));
            let met = actual.as_ref().is_some_and(|actual| test.holds(actual));
            self.record(category, met, || {
                format!(
                    "{subject}: {field} expected {}, got {}",
                    excerpt(&test.describe(), EXCERPT_CHARS),
                    actual.map_or("no row".to_string(), |actual| excerpt(
                        &actual.to_string(),
                        EXCERPT_CHARS
                    ))
                )
            });
        }
    }
}

fn tests(expectations: Vec<Expectation>) -> Result<Vec<(String, Test)>, String> {
    expectations.into_iter().map(test).collect()
}

impl Check for CrewSpecMatch {
    fn name(&self) -> &'static str {
        "crew_spec_match"
    }

    fn version(&self) -> &'static str {
        "2"
    }

    fn describe(&self) -> CheckDescription {
        let expectation = json!({
            "type": "object",
            "properties": {
                "field": {"type": "string"},
                "equals": {},
                "contains": {"type": "string"},
                "matches": {"type": "string"}
            },
            "required": ["field"],
            "additionalProperties": false
        });
        let expectations = json!({"type": "array", "items": expectation});
        let strings = json!({"type": "array", "items": {"type": "string"}});
        let category = json!({"type": "string"});
        CheckDescription {
            name: self.name().into(),
            version: self.version().into(),
            summary: "Scores the fraction of a declared configuration's requirements that the captured home holds: captures present, keyed rows and their fields, each behavior's Context and Tools, template fields against each source collection, and ids named in a receipt. raw.categories breaks the score down; raw.activity counts tool and inference calls.".into(),
            params_schema: json!({
                "type": "object",
                "properties": {
                    "present": {"type": "array", "items": {
                        "type": "object",
                        "properties": {"capture": {"type": "string"}, "category": category},
                        "required": ["capture"], "additionalProperties": false
                    }},
                    "rows": {"type": "array", "items": {
                        "type": "object",
                        "properties": {
                            "capture": {"type": "string"}, "key": {"type": "string"},
                            "id": {"type": "string"}, "category": category, "expect": expectations
                        },
                        "required": ["capture", "key", "id"], "additionalProperties": false
                    }},
                    "links": {"type": "array", "items": {
                        "type": "object",
                        "properties": {
                            "capture": {"type": "string"}, "key": {"type": "string"},
                            "id": {"type": "string"}, "field": {"type": "string"},
                            "target_capture": {"type": "string"}, "target_key": {"type": "string"},
                            "category": category, "expect": expectations
                        },
                        "required": ["capture", "key", "id", "field", "target_capture", "target_key"],
                        "additionalProperties": false
                    }},
                    "agents": {
                        "type": "object",
                        "properties": {
                            "behaviors": {"type": "string"}, "contexts": {"type": "string"},
                            "tools": {"type": "string"},
                            "expect": {"type": "array", "items": {
                                "type": "object",
                                "properties": {
                                    "behavior_id": {"type": "string"}, "category": category,
                                    "behavior": expectations, "context": expectations,
                                    "tools": expectations
                                },
                                "required": ["behavior_id"], "additionalProperties": false
                            }}
                        },
                        "required": ["behaviors", "contexts", "tools", "expect"],
                        "additionalProperties": false
                    },
                    "templates": {"type": "array", "items": {
                        "type": "object",
                        "properties": {
                            "capture": {"type": "string"}, "key": {"type": "string"},
                            "id": {"type": "string"}, "fields": strings, "allowed": strings,
                            "category": category
                        },
                        "required": ["capture", "key", "id", "fields", "allowed"],
                        "additionalProperties": false
                    }},
                    "continuation": {
                        "type": "object",
                        "properties": {"max": {"type": "integer", "minimum": 1}, "category": category},
                        "required": ["max"], "additionalProperties": false
                    },
                    "receipt": {
                        "type": "object",
                        "properties": {
                            "capture": {"type": "string"}, "fields": strings, "ids": strings,
                            "category": category
                        },
                        "required": ["capture", "fields", "ids"], "additionalProperties": false
                    }
                },
                "additionalProperties": false
            }),
            reads: vec!["capture:documents".into(), "stage:tool_calls".into()],
            reason_codes: graded_reason_codes(&[]),
        }
    }

    fn evaluate(&self, params: &Value, stage: &StageEvidence) -> CheckVerdict {
        let params: Params = match serde_json::from_value(params.clone()) {
            Ok(params) => params,
            Err(error) => return grader("bad_params", error.to_string()),
        };
        let rows = |name: &str| match stage.captures.get(name) {
            Some(CaptureResult::Documents { rows }) => Some(rows.as_slice()),
            _ => None,
        };
        let mut tally = Tally::default();

        for present in &params.present {
            tally.record(
                present.category.as_deref(),
                rows(&present.capture).is_some(),
                || format!("capture {} absent", present.capture),
            );
        }
        for spec in params.rows {
            let category = spec.category.as_deref();
            let tests = match tests(spec.expect) {
                Ok(tests) => tests,
                Err(detail) => return grader("bad_params", detail),
            };
            let row = rows(&spec.capture).and_then(|rows| find(rows, &spec.key, &spec.id));
            let subject = format!("{} {}", spec.capture, spec.id);
            tally.record(category, row.is_some(), || format!("{subject} absent"));
            tally.expectations(category, &subject, row, &tests);
        }
        for spec in params.links {
            let tests = match tests(spec.expect) {
                Ok(tests) => tests,
                Err(detail) => return grader("bad_params", detail),
            };
            let source = rows(&spec.capture).and_then(|rows| find(rows, &spec.key, &spec.id));
            let reference = source.map(|row| lookup(row, &spec.field));
            let target = reference.as_ref().and_then(Value::as_str).and_then(|id| {
                rows(&spec.target_capture).and_then(|rows| find(rows, &spec.target_key, id))
            });
            let subject = format!(
                "{} {}.{} -> {}",
                spec.capture, spec.id, spec.field, spec.target_capture
            );
            let category = spec.category.as_deref();
            tally.record(category, target.is_some(), || {
                format!("{subject}: unresolved reference")
            });
            tally.expectations(category, &subject, target, &tests);
        }
        if let Some(agents) = params.agents {
            let behaviors = rows(&agents.behaviors).unwrap_or_default();
            let contexts = rows(&agents.contexts).unwrap_or_default();
            let tools = rows(&agents.tools).unwrap_or_default();
            for spec in agents.expect {
                let category = spec.category.as_deref();
                let (behavior_tests, context_tests, tools_tests) =
                    match (tests(spec.behavior), tests(spec.context), tests(spec.tools)) {
                        (Ok(b), Ok(c), Ok(t)) => (b, c, t),
                        (Err(detail), _, _) | (_, Err(detail), _) | (_, _, Err(detail)) => {
                            return grader("bad_params", detail)
                        }
                    };
                let behavior = find(behaviors, "behavior_id", &spec.behavior_id);
                let context = behavior
                    .and_then(|row| row.get("context_id").and_then(Value::as_str))
                    .and_then(|id| find(contexts, "context_id", id));
                let tools_row = context
                    .and_then(|row| row.get("tools_id").and_then(Value::as_str))
                    .and_then(|id| find(tools, "tools_id", id));
                let id = &spec.behavior_id;
                tally.record(category, behavior.is_some(), || {
                    format!("behavior {id} absent")
                });
                tally.expectations(
                    category,
                    &format!("behavior {id}"),
                    behavior,
                    &behavior_tests,
                );
                tally.expectations(category, &format!("{id} context"), context, &context_tests);
                tally.expectations(category, &format!("{id} tools"), tools_row, &tools_tests);
            }
        }
        for spec in &params.templates {
            let category = spec.category.as_deref().or(Some("templates"));
            let row = rows(&spec.capture).and_then(|rows| find(rows, &spec.key, &spec.id));
            let Some(row) = row else {
                tally.record(category, false, || {
                    format!("{} {} absent, no template to check", spec.capture, spec.id)
                });
                continue;
            };
            for field in &spec.fields {
                let Some(template) = row.get(field).and_then(Value::as_str) else {
                    continue;
                };
                let unknown: Vec<String> = doc_fields(template)
                    .into_iter()
                    .filter(|name| !spec.allowed.contains(name))
                    .collect();
                tally.record(category, unknown.is_empty(), || {
                    format!(
                        "{} {}.{field} reads doc.{} outside its source collection",
                        spec.capture,
                        spec.id,
                        unknown.join(", doc.")
                    )
                });
            }
        }
        if let Some(receipt) = &params.receipt {
            let category = receipt.category.as_deref().or(Some("receipt"));
            let text: String = rows(&receipt.capture)
                .unwrap_or_default()
                .iter()
                .flat_map(|row| {
                    receipt.fields.iter().filter_map(|field| {
                        row.get(field).map(|value| match value {
                            Value::String(text) => text.clone(),
                            other => other.to_string(),
                        })
                    })
                })
                .collect::<Vec<_>>()
                .join("\n");
            for id in &receipt.ids {
                tally.record(category, text.contains(id.as_str()), || {
                    format!("receipt does not name {id}")
                });
            }
        }

        if let Some(continuation) = &params.continuation {
            let category = continuation.category.as_deref().or(Some("continuation"));
            for index in 0..continuation.max {
                tally.record(category, index >= stage.prods, || {
                    format!("continuation prod {} was needed", index + 1)
                });
            }
        }
        let (satisfied, total) = tally
            .categories
            .values()
            .fold((0, 0), |(s, t), (cs, ct)| (s + cs, t + ct));
        if total == 0 {
            return grader("bad_params", "params require nothing");
        }
        let failed_calls = stage
            .tool_calls
            .iter()
            .filter(|call| {
                call.status.as_deref() == Some("failed") || call.tool_failure_class.is_some()
            })
            .count();
        let mut by_tool: BTreeMap<&str, usize> = BTreeMap::new();
        for call in &stage.tool_calls {
            *by_tool.entry(call.tool_name.as_str()).or_default() += 1;
        }
        let feedback = (!tally.unmet.is_empty()).then(|| {
            let mut text = format!("{} of {total} requirements unmet:\n", tally.unmet.len());
            for line in tally.unmet.iter().take(20) {
                text.push_str(&format!("- {line}\n"));
            }
            bounded(text)
        });
        let mut verdict = graded(satisfied, total, feedback);
        verdict.raw["categories"] = tally
            .categories
            .iter()
            .map(|(name, (s, t))| (name.clone(), json!({"satisfied": s, "total": t})))
            .collect::<serde_json::Map<_, _>>()
            .into();
        verdict.raw["unmet"] = json!(tally.unmet.iter().take(SHOWN).collect::<Vec<_>>());
        verdict.raw["unmet_count"] = json!(tally.unmet.len());
        verdict.raw["activity"] = json!({
            "tool_calls": stage.tool_calls.len(),
            "failed_tool_calls": failed_calls,
            "by_tool": by_tool,
            "inference_calls": stage.inference_calls.len(),
            "messages": stage.messages.len(),
            "prods": stage.prods,
        });
        verdict
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::runner::scripted::ScriptedExecutor;
    use crate::eval::OutcomeKind;

    fn stage(captures: &[(&str, Vec<Value>)]) -> StageEvidence {
        let mut stage = ScriptedExecutor::passed_evidence("did:x", "s1", "unused", Vec::new())
            .stages
            .remove(0);
        stage.captures.clear();
        for (name, rows) in captures {
            stage.captures.insert(
                (*name).to_string(),
                CaptureResult::Documents { rows: rows.clone() },
            );
        }
        stage
    }

    fn home() -> StageEvidence {
        stage(&[
            (
                "behaviors",
                vec![
                    json!({"behavior_id": "worker-a", "context_id": "c1", "inference_profile_id": "glm-a"}),
                ],
            ),
            (
                "contexts",
                vec![
                    json!({"context_id": "c1", "tools_id": "t1", "system_prompt": "You are a GLM worker"}),
                ],
            ),
            (
                "tools",
                vec![
                    json!({"tools_id": "t1", "host": "{\"files\":{\"mode\":\"ReadWrite\"}}", "subagents": {"enabled": false}}),
                ],
            ),
            (
                "tasks",
                vec![
                    json!({"task_id": "work-a", "emit_outcome": true, "prompt_template": "Shard {{ doc.shard_id }} {{doc.owner}}"}),
                ],
            ),
            ("c_assignment", vec![]),
            (
                "mailbox",
                vec![json!({"title": "Setup receipt", "payload": "worker-a work-a"})],
            ),
        ])
    }

    fn params() -> Value {
        json!({
            "present": [{"capture": "c_assignment", "category": "collections"}, {"capture": "c_result", "category": "collections"}],
            "rows": [{"capture": "tasks", "key": "task_id", "id": "work-a", "category": "emit_outcome",
                      "expect": [{"field": "emit_outcome", "equals": true}]}],
            "agents": {"behaviors": "behaviors", "contexts": "contexts", "tools": "tools", "expect": [
                {"behavior_id": "worker-a", "category": "least_privilege",
                 "behavior": [{"field": "inference_profile_id", "equals": "glm-a"}],
                 "context": [{"field": "system_prompt", "contains": "GLM worker"}],
                 "tools": [{"field": "host.files.mode", "equals": "ReadWrite"}, {"field": "subagents.enabled", "equals": false}]}
            ]},
            "templates": [{"capture": "tasks", "key": "task_id", "id": "work-a", "fields": ["prompt_template"], "allowed": ["shard_id"]}],
            "receipt": {"capture": "mailbox", "fields": ["title", "payload"], "ids": ["worker-a", "work-a", "work-b"]}
        })
    }

    #[test]
    fn links_check_the_selected_source_and_fail_missing_or_wrong_references() {
        let params = json!({"links": [{
            "capture": "triggers", "key": "trigger_id", "id": "dispatch",
            "field": "source.event_source_id", "target_capture": "sources", "target_key": "event_source_id",
            "expect": [{"field": "source_collection", "equals": "Assignment"}]
        }]});
        let schema = CrewSpecMatch.describe().params_schema;
        assert!(jsonschema::validator_for(&schema)
            .unwrap()
            .is_valid(&params));
        for (source, expected) in [("wanted", 10000), ("decoy", 5000), ("missing", 0)] {
            let evidence = stage(&[
                (
                    "triggers",
                    vec![
                        json!({"trigger_id":"dispatch", "source": json!({"kind":"event", "event_source_id":source}).to_string()}),
                    ],
                ),
                (
                    "sources",
                    vec![
                        json!({"event_source_id":"wanted", "source_collection":"Assignment"}),
                        json!({"event_source_id":"decoy", "source_collection":"Result"}),
                    ],
                ),
            ]);
            let verdict = CrewSpecMatch.evaluate(&params, &evidence);
            assert_eq!(verdict.score_bp, Some(expected), "{}", verdict.raw);
        }
    }

    #[test]
    fn the_score_is_the_satisfied_fraction_broken_down_by_category() {
        let verdict = CrewSpecMatch.evaluate(&params(), &home());
        assert_eq!(
            verdict.kind,
            OutcomeKind::ModelAcceptance,
            "{}",
            verdict.raw
        );
        let categories = &verdict.raw["categories"];
        assert_eq!(
            categories["collections"],
            json!({"satisfied": 1, "total": 2})
        );
        assert_eq!(
            categories["emit_outcome"],
            json!({"satisfied": 2, "total": 2})
        );
        assert_eq!(
            categories["least_privilege"],
            json!({"satisfied": 5, "total": 5})
        );
        assert_eq!(categories["templates"], json!({"satisfied": 0, "total": 1}));
        assert_eq!(categories["receipt"], json!({"satisfied": 2, "total": 3}));
        // 10 of 13.
        assert_eq!(verdict.score_bp, Some(7_692));
        let feedback = verdict.feedback.unwrap();
        assert!(feedback.contains("doc.owner"), "{feedback}");
        assert!(feedback.contains("capture c_result absent"), "{feedback}");
        assert_eq!(verdict.raw["activity"]["tool_calls"], 0);
    }

    #[test]
    fn a_missing_behavior_fails_its_whole_chain() {
        let mut evidence = home();
        evidence.captures.insert(
            "behaviors".into(),
            CaptureResult::Documents { rows: vec![] },
        );
        let verdict = CrewSpecMatch.evaluate(&params(), &evidence);
        assert_eq!(
            verdict.raw["categories"]["least_privilege"],
            json!({"satisfied": 0, "total": 5})
        );
    }

    #[test]
    fn a_principal_scoped_behavior_id_matches_its_declared_id() {
        let mut evidence = home();
        evidence.captures.insert(
            "behaviors".into(),
            CaptureResult::Documents {
                rows: vec![json!({"behavior_id": "did:key:z6Mk:worker-a", "context_id": "c1", "inference_profile_id": "glm-a"})],
            },
        );
        let verdict = CrewSpecMatch.evaluate(&params(), &evidence);
        assert_eq!(
            verdict.raw["categories"]["least_privilege"],
            json!({"satisfied": 5, "total": 5})
        );
        assert!(is_id("did:key:z6Mk:worker-a", "worker-a"));
        assert!(!is_id("did:key:z6Mk:big-worker-a", "worker-a"));
        assert!(!is_id("worker-ab", "worker-a"));
    }

    #[test]
    fn each_prod_needed_costs_one_continuation_requirement() {
        let mut evidence = home();
        evidence.prods = 2;
        let verdict = CrewSpecMatch.evaluate(&json!({"continuation": {"max": 5}}), &evidence);
        assert_eq!(
            verdict.raw["categories"]["continuation"],
            json!({"satisfied": 3, "total": 5})
        );
        assert_eq!(verdict.raw["activity"]["prods"], 2);
    }

    #[test]
    fn an_absent_field_reads_as_null() {
        let row = json!({"subagents": {"target_ids": []}});
        assert_eq!(lookup(&row, "subagents.enabled"), Value::Null);
        assert_eq!(lookup(&row, "host.files.mode"), Value::Null);
    }

    #[test]
    fn doc_fields_reads_each_name_once_and_ignores_other_roots() {
        assert_eq!(
            doc_fields(
                "{{ doc.a }} {{doc.b_c}} {{ doc.a }} {{ session.session_id }} {{ mydoc.x }}"
            ),
            vec!["a".to_string(), "b_c".to_string()]
        );
    }

    #[test]
    fn params_that_require_nothing_are_the_grader() {
        let verdict = CrewSpecMatch.evaluate(&json!({}), &home());
        assert_eq!(verdict.kind, OutcomeKind::Grader);
    }
}
