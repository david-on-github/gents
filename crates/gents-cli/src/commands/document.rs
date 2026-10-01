use anyhow::{bail, Context, Result};
use gents::defra_query::{unknown_collection_message, CollectionSchema, CollectionScope};
use gents::graphql::validate_collection_identifier;
use gents_protocol::graphql::{extract_mutation_doc_id, graphql_input_literal};
use serde_json::{json, Value};

use crate::cli::args::{DocumentCommand, DocumentCreateArgs};
use crate::commands::query::fetch_collection_schema;
use crate::{print_json, resolve_config_access, resolve_graphql_endpoint};

pub(crate) async fn dispatch(command: DocumentCommand) -> Result<()> {
    match command {
        DocumentCommand::Create(args) => create(args).await,
    }
}

/// Parse `--json` into a non-empty object of document fields. Field names are
/// validated as GraphQL names by the shared literal renderer and values are
/// escaped by it.
fn parse_fields(collection: &str, fields: &str) -> Result<Value> {
    validate_collection_identifier(collection)?;
    CollectionScope::all().ensure_allowed(collection)?;
    let fields: Value = serde_json::from_str(fields).context("parsing --json as JSON")?;
    if !fields.as_object().is_some_and(|object| !object.is_empty()) {
        bail!("--json must be a non-empty JSON object of document fields");
    }
    // The shared literal renderer writes an empty array as null, which would
    // silently differ from what the operator typed.
    if contains_empty_array(&fields) {
        bail!("--json must not contain an empty array; omit the field instead");
    }
    Ok(fields)
}

/// DefraDB ignores input fields the collection does not declare, so an
/// unknown one is refused here against the introspected schema instead.
fn ensure_known_fields(collection: &str, fields: &Value, schema: &CollectionSchema) -> Result<()> {
    let unknown = fields
        .as_object()
        .into_iter()
        .flat_map(|object| object.keys())
        .find(|key| !schema.fields.iter().any(|field| &field.name == *key));
    match unknown {
        Some(key) => bail!("collection {collection:?} has no field {key:?}; check the field name"),
        None => Ok(()),
    }
}

fn create_mutation(collection: &str, fields: &Value) -> Result<String> {
    let input = graphql_input_literal(fields)?;
    Ok(format!(
        "mutation {{ create_{collection}(input: {input}) {{ _docID }} }}"
    ))
}

fn contains_empty_array(value: &Value) -> bool {
    match value {
        Value::Array(items) => items.is_empty() || items.iter().any(contains_empty_array),
        Value::Object(map) => map.values().any(contains_empty_array),
        _ => false,
    }
}

async fn create(args: DocumentCreateArgs) -> Result<()> {
    let fields = parse_fields(&args.collection, &args.json)?;
    let mutation = create_mutation(&args.collection, &fields)?;
    let (access, _) = resolve_config_access(args.home.as_deref(), args.graphql.as_deref()).await?;
    let endpoint = resolve_graphql_endpoint(args.graphql.as_deref(), args.home.as_deref())?;
    let schema = fetch_collection_schema(&endpoint, &args.collection)
        .await?
        .with_context(|| unknown_collection_message(&args.collection))?;
    ensure_known_fields(&args.collection, &fields, &schema)?;
    let response = access.write("cli.document.create", &mutation).await?;
    let doc_id = extract_mutation_doc_id(&response, &args.collection)?;
    print_json(&json!({ "collection": args.collection, "doc_id": doc_id }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_typed_fields_and_escapes_strings() {
        let fields = parse_fields("Goal", r#"{"goal_id":"a\"b","token_budget":5,"live":true}"#)
            .expect("fields");
        let mutation = create_mutation("Goal", &fields).expect("mutation");
        assert!(
            mutation.starts_with("mutation { create_Goal(input: {"),
            "{mutation}"
        );
        assert!(mutation.contains(r#"goal_id: "a\"b""#), "{mutation}");
        assert!(mutation.contains("token_budget: 5"), "{mutation}");
        assert!(mutation.contains("live: true"), "{mutation}");
    }

    #[test]
    fn refuses_bad_collections_and_inputs_before_any_write() {
        for (collection, json, needle) in [
            ("Goal { x }", r#"{"a":"b"}"#, "invalid identifier"),
            (
                gents_protocol::schemas::EVAL_VERDICT_NAME,
                r#"{"a":"b"}"#,
                "protected",
            ),
            ("Goal", "[1]", "non-empty JSON object"),
            ("Goal", "{}", "non-empty JSON object"),
            ("Goal", r#"{"tags":[]}"#, "empty array"),
            ("Goal", r#"{"a":{"b":[]}}"#, "empty array"),
            ("Goal", "not json", "parsing --json"),
        ] {
            let error = parse_fields(collection, json).expect_err("must refuse");
            assert!(
                format!("{error:#}").contains(needle),
                "{collection} {json}: {error:#}"
            );
        }
    }

    #[test]
    fn refuses_field_names_that_are_not_graphql_names() {
        let fields = parse_fields("Goal", r#"{"a b":"c"}"#).expect("fields");
        let error = create_mutation("Goal", &fields).expect_err("must refuse");
        assert!(
            format!("{error:#}").contains("invalid identifier"),
            "{error:#}"
        );
    }

    #[test]
    fn refuses_fields_the_collection_does_not_declare() {
        let schema = CollectionSchema {
            fields: vec![gents::defra_query::SchemaField {
                name: "goal_id".to_string(),
                type_name: "String".to_string(),
            }],
        };
        let known = serde_json::json!({"goal_id": "g"});
        assert!(ensure_known_fields("Goal", &known, &schema).is_ok());
        let unknown = serde_json::json!({"goal_id": "g", "nope": 1});
        let error = ensure_known_fields("Goal", &unknown, &schema).expect_err("must refuse");
        assert!(
            format!("{error:#}").contains("no field \"nope\""),
            "{error:#}"
        );
    }
}
