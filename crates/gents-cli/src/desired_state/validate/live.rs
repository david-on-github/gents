use std::collections::{BTreeSet, HashMap};

use anyhow::Result;
use gents::parse_template_for_validation;

use crate::config_writes::ConfigAccess;

use super::super::DesiredStateManifest;

/// Validate live state that cannot be checked from the manifest alone.
///
/// Apply validates trigger filter syntax and top-level `doc.*` template fields.
/// Resolving fields below the top level remains outside this contract.
pub(crate) async fn validate_manifest_against_live(
    manifest: &DesiredStateManifest,
    access: &ConfigAccess,
) -> Result<Vec<String>> {
    let mut errors = Vec::new();
    for source in &manifest.event_sources {
        let source_collection = source.source_collection.trim();
        let source_id = source.event_source_id.trim();
        if source_collection.is_empty() || source_id.is_empty() {
            continue;
        }
        if let Err(error) = gents::graphql::validate_collection_identifier(source_collection) {
            errors.push(format!(
                "event source {} has invalid source_collection {:?}: {}",
                source_id, source.source_collection, error
            ));
            continue;
        }

        if let Some(filter) = source.filter.as_deref().map(str::trim) {
            if !filter.is_empty() {
                // `filter` is interpolated into the probe query as a raw filter
                // fragment; validate it like the runtime trigger engine does
                // (`trigger_engine::event_source`) before building the probe.
                // `source_collection` is already validated by the guard above.
                if let Err(err) = gents::graphql::validate_graphql_filter_fragment(filter) {
                    errors.push(format!(
                        "event source {} filter is not a valid filter fragment: {}",
                        source_id, err
                    ));
                } else {
                    let probe = format!(
                        r#"query {{ {collection}(filter: {filter}, limit: 1) {{ _docID }} }}"#,
                        collection = source_collection,
                        filter = filter,
                    );
                    match access.execute(&probe).await {
                        Ok(_) => {}
                        Err(err) => {
                            errors.push(format!(
                                "event source {} filter syntax error: {}",
                                source_id, err
                            ));
                        }
                    }
                }
            }
        }

        let mut doc_paths = Vec::new();
        for trigger in &manifest.triggers {
            if trigger.agent_did != source.agent_did
                || !matches!(&trigger.source,
                    gents::document_config::TriggerSource::Event { event_source_id }
                    if event_source_id == &source.event_source_id)
            {
                continue;
            }
            let Some(task) = manifest.tasks.iter().find(|task| {
                task.agent_did == trigger.agent_did && task.task_id == trigger.task_id
            }) else {
                continue;
            };
            if let Ok(refs) = parse_template_for_validation(&task.prompt_template) {
                doc_paths.extend(
                    refs.into_iter()
                        .filter(|reference| reference.root() == Some("doc"))
                        .map(|reference| reference.path),
                );
            }
        }
        let expected_count_field = source
            .group
            .as_ref()
            .and_then(|group| group.expected_count.as_ref())
            .and_then(|count| match count {
                gents::document_config::EventGroupCount::SourceField { source_field } => {
                    Some(source_field.as_str())
                }
                gents::document_config::EventGroupCount::Fixed(_) => None,
            });
        if doc_paths.is_empty()
            && source.correlation_field.is_none()
            && expected_count_field.is_none()
        {
            continue;
        }

        let introspect = match gents::defra_query::introspection_query(source_collection) {
            Ok(introspect) => introspect,
            Err(err) => {
                errors.push(format!(
                    "event source {} has invalid source_collection {:?}: {}",
                    source_id, source.source_collection, err
                ));
                continue;
            }
        };
        let response = match access.execute(&introspect).await {
            Ok(response) => response,
            Err(err) => {
                errors.push(format!(
                    "event source {} introspection of source_collection {} failed: {}",
                    source_id, source_collection, err
                ));
                continue;
            }
        };
        let Some(schema) = gents::defra_query::parse_collection_schema(response.get("data")) else {
            errors.push(format!(
                "event source {} references unknown source_collection {}",
                source_id, source_collection
            ));
            continue;
        };
        let declared: HashMap<&str, &gents::defra_query::SchemaField> = schema
            .fields
            .iter()
            .map(|field| (field.name.as_str(), field))
            .collect();
        let mut reported: BTreeSet<String> = BTreeSet::new();
        for path in &doc_paths {
            let Some(first) = path.get(1).map(String::as_str) else {
                continue;
            };
            if declared.contains_key(first) {
                continue;
            }
            if !reported.insert(first.to_string()) {
                continue;
            }
            errors.push(format!(
                "event source {} template references doc.{} but {} has no such field",
                source_id, first, source_collection
            ));
        }
    }

    Ok(errors)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use anyhow::Result;
    use defra_node::EmbeddedNode;
    use serde_json::json;

    use super::*;

    const OWNER: &str = "did:key:z6MkhaXgBZDvotDkL5257faiztiGiC2QtKLGpbnnEGta2doK";

    const PROBE_SDL: &str = r#"
        type LiveProbe {
            batch: String
        }
    "#;

    async fn probe_access(tempdir: &tempfile::TempDir) -> Result<ConfigAccess> {
        let node = Arc::new(
            EmbeddedNode::builder()
                .data_path(tempdir.path().join("data"))
                .build()
                .await?,
        );
        let access = ConfigAccess::Local(node);
        access.add_schema(PROBE_SDL).await?;
        Ok(access)
    }

    fn manifest(
        sources: serde_json::Value,
        tasks: serde_json::Value,
        triggers: serde_json::Value,
    ) -> Result<DesiredStateManifest> {
        Ok(serde_json::from_value(json!({
            "agent_principal": { "agent_did": OWNER },
            "event_sources": sources,
            "tasks": tasks,
            "triggers": triggers,
        }))?)
    }

    fn source(id: &str, extra: serde_json::Value) -> serde_json::Value {
        let mut source = json!({
            "agent_did": OWNER,
            "event_source_id": id,
            "source_collection": "LiveProbe",
        });
        let object = source.as_object_mut().expect("source object");
        for (field, value) in extra.as_object().expect("source fields") {
            object.insert(field.clone(), value.clone());
        }
        source
    }

    #[tokio::test]
    async fn refuses_a_filter_the_probe_query_cannot_execute() -> Result<()> {
        let tempdir = tempfile::tempdir()?;
        let access = probe_access(&tempdir).await?;
        let manifest = manifest(
            json!([source(
                "filtered",
                json!({"filter": "{undeclared: {_eq: \"x\"}}"})
            )]),
            json!([]),
            json!([]),
        )?;
        let errors = validate_manifest_against_live(&manifest, &access).await?;
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].contains("filter syntax error"),
            "a filter that names no declared field must fail the probe: {errors:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn refuses_a_template_path_the_source_collection_does_not_declare() -> Result<()> {
        let tempdir = tempfile::tempdir()?;
        let access = probe_access(&tempdir).await?;
        let manifest = manifest(
            json!([source("templated", json!({"correlation_field": "batch"}))]),
            json!([{
                "agent_did": OWNER,
                "task_id": "summarize",
                "behavior_id": "beh",
                "prompt_template": "Summarize {{ doc.nope }}"
            }]),
            json!([{
                "agent_did": OWNER,
                "trigger_id": "on-templated",
                "task_id": "summarize",
                "source": {"kind": "event", "event_source_id": "templated"}
            }]),
        )?;
        let errors = validate_manifest_against_live(&manifest, &access).await?;
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].contains("template references doc.nope but LiveProbe has no such field"),
            "{errors:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn refuses_a_source_collection_the_node_does_not_have() -> Result<()> {
        let tempdir = tempfile::tempdir()?;
        let access = probe_access(&tempdir).await?;
        let mut missing = source("missing", json!({"correlation_field": "batch"}));
        missing["source_collection"] = json!("LiveProbeLater");
        let manifest = manifest(json!([missing]), json!([]), json!([]))?;
        let errors = validate_manifest_against_live(&manifest, &access).await?;
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].contains("references unknown source_collection LiveProbeLater"),
            "{errors:?}"
        );
        Ok(())
    }
}
