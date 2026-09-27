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
        if let Some(field) = source
            .correlation_field
            .as_deref()
            .map(str::trim)
            .filter(|field| !field.is_empty())
        {
            match declared.get(field) {
                Some(declared) if declared.named_type() == "String" => {}
                Some(declared) => errors.push(format!(
                    "event source {} correlation_field {} must be String, found {}",
                    source_id, field, declared.type_name
                )),
                None => errors.push(format!(
                    "event source {} correlation_field {} does not exist on {}",
                    source_id, field, source_collection
                )),
            }
        }
        if let Some(field) = expected_count_field
            .map(str::trim)
            .filter(|field| !field.is_empty())
        {
            match declared.get(field) {
                Some(declared)
                    if gents::defra_write::can_hold_canonical_count(declared.named_type()) => {}
                Some(declared) => errors.push(format!(
                    "event source {} expected_count_field {} names a {} field of {}, which cannot \
                     carry the count; the runtime parses an integer or its canonical decimal \
                     spelling out of the source document",
                    source_id, field, declared.type_name, source_collection
                )),
                None => errors.push(format!(
                    "event source {} expected_count_field {} does not exist on {}",
                    source_id, field, source_collection
                )),
            }
        }
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

    /// Declares each shape the count and correlation rules have to decide:
    /// non-nillable scalars, a float, a JSON column, and a boolean that no
    /// canonical count can come back through.
    const PROBE_SDL: &str = r#"
        type CountProbeNonNull {
            batch: String!
            expected_total: Int!
        }
        type CountProbeNumeric {
            batch: String
            expected_total: Float
        }
        type CountProbeJson {
            batch: String
            expected_total: JSON
        }
        type CountProbeUncountable {
            batch: String
            expected_total: Boolean
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

    fn manifest(sources: serde_json::Value) -> Result<DesiredStateManifest> {
        Ok(serde_json::from_value(json!({
            "agent_principal": { "agent_did": OWNER },
            "event_sources": sources,
        }))?)
    }

    fn grouped_source(id: &str, collection: &str, count_field: &str) -> serde_json::Value {
        json!({
            "agent_did": OWNER,
            "event_source_id": id,
            "source_collection": collection,
            "correlation_field": "batch",
            "group": { "expected_count": { "source_field": count_field } },
        })
    }

    #[tokio::test]
    async fn accepts_non_nillable_count_and_correlation_fields() -> Result<()> {
        let tempdir = tempfile::tempdir()?;
        let access = probe_access(&tempdir).await?;
        let manifest = manifest(json!([grouped_source(
            "non-null",
            "CountProbeNonNull",
            "expected_total"
        )]))?;
        let errors = validate_manifest_against_live(&manifest, &access).await?;
        assert!(errors.is_empty(), "{errors:?}");
        Ok(())
    }

    #[tokio::test]
    async fn accepts_every_declared_type_a_canonical_count_can_come_back_through() -> Result<()> {
        let tempdir = tempfile::tempdir()?;
        let access = probe_access(&tempdir).await?;
        let manifest = manifest(json!([
            grouped_source("numeric", "CountProbeNumeric", "expected_total"),
            grouped_source("json", "CountProbeJson", "expected_total"),
        ]))?;
        let errors = validate_manifest_against_live(&manifest, &access).await?;
        assert!(errors.is_empty(), "{errors:?}");
        Ok(())
    }

    #[tokio::test]
    async fn refuses_a_count_field_no_count_can_come_back_through() -> Result<()> {
        let tempdir = tempfile::tempdir()?;
        let access = probe_access(&tempdir).await?;
        let manifest = manifest(json!([grouped_source(
            "uncountable",
            "CountProbeUncountable",
            "expected_total"
        )]))?;
        let errors = validate_manifest_against_live(&manifest, &access).await?;
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("expected_total"), "{errors:?}");
        assert!(errors[0].contains("Boolean"), "{errors:?}");
        assert!(!errors[0].contains("does not exist"), "{errors:?}");
        Ok(())
    }

    #[tokio::test]
    async fn refuses_a_count_field_the_collection_does_not_declare() -> Result<()> {
        let tempdir = tempfile::tempdir()?;
        let access = probe_access(&tempdir).await?;
        let manifest = manifest(json!([grouped_source(
            "absent",
            "CountProbeNonNull",
            "missing_total"
        )]))?;
        let errors = validate_manifest_against_live(&manifest, &access).await?;
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("missing_total"), "{errors:?}");
        assert!(errors[0].contains("does not exist"), "{errors:?}");
        Ok(())
    }
}
