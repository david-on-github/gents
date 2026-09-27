// Session-message provenance for one session scope, read from the immutable
// `AgentRequest.caused_by_parent_*` lineage through the `gents::session_origin`
// owner: public requests only, exact session scopes, no truncating limit.
// Read only; it confers no hierarchy, cascade or authority.

use std::collections::BTreeMap;
use std::sync::Arc;

use gents::session::{public_request_filter, session_scope_filter};
use gents::session_origin::{
    load_request_scope, load_session_request_doc_ids, retain_session_origins, OriginReader,
    SessionScope, ORIGIN_FIELDS,
};
use gents_desktop_core::client::ClientCore;
use serde_json::Value;

use crate::types::{CausedRequestView, SessionProvenanceView};

/// Fields the view reads beyond the origin owner's.
const VIEW_FIELDS: &str = "interrupt_requested_at subagent_depth";

pub async fn session_provenance(
    core: &Arc<ClientCore>,
    scope: SessionScope,
) -> Result<SessionProvenanceView, String> {
    let reader = OriginReader::Node(core.node());
    let fail = |error: anyhow::Error| format!("session provenance: {error:#}");

    let own: Vec<String> = load_session_request_doc_ids(reader, std::slice::from_ref(&scope))
        .await
        .map_err(fail)?
        .into_iter()
        .map(|(doc_id, _)| doc_id)
        .collect();

    // Every request this session's calls caused: starts and messages alike.
    let sent_rows = if own.is_empty() {
        Vec::new()
    } else {
        request_rows(
            reader,
            &public_request_filter(&format!(
                "caused_by_parent_request_doc_id: {{ _in: [{}] }}",
                quoted_list(&own)
            )),
            "session provenance: caused requests",
        )
        .await
        .map_err(fail)?
    };
    // A subagent is a session whose origin this session caused; a message
    // into an existing session never makes it one.
    let started_rows = retain_session_origins(reader, sent_rows.clone())
        .await
        .map_err(fail)?;

    let received_rows = request_rows(
        reader,
        &public_request_filter(&format!(
            "{}, caused_by_parent_request_doc_id: {{ _ne: null }}",
            session_scope_filter(
                &scope.agent_did,
                &scope.session_id,
                scope.requester_did.as_deref()
            )
        )),
        "session provenance: received requests",
    )
    .await
    .map_err(fail)?;
    let origin_rows = retain_session_origins(reader, received_rows.clone())
        .await
        .map_err(fail)?;

    let mut causing = BTreeMap::new();
    for doc_id in received_rows
        .iter()
        .filter_map(|row| string_field(row, "caused_by_parent_request_doc_id"))
    {
        if causing.contains_key(&doc_id) {
            continue;
        }
        let scope = load_request_scope(reader, &doc_id).await.map_err(fail)?;
        causing.insert(doc_id, scope);
    }
    let received_view = |row: &Value| {
        let causing_scope = string_field(row, "caused_by_parent_request_doc_id")
            .and_then(|doc_id| causing.get(&doc_id).cloned().flatten());
        caused_request_view(row, causing_scope.as_ref())
    };

    Ok(SessionProvenanceView {
        session_id: scope.session_id.clone(),
        started_by: origin_rows.first().and_then(received_view),
        received: received_rows.iter().filter_map(received_view).collect(),
        started: started_rows
            .iter()
            .filter_map(|row| caused_request_view(row, Some(&scope)))
            .collect(),
        sent: sent_rows
            .iter()
            .filter_map(|row| caused_request_view(row, Some(&scope)))
            .collect(),
    })
}

async fn request_rows(
    reader: OriginReader<'_>,
    filter: &str,
    operation: &str,
) -> anyhow::Result<Vec<Value>> {
    let query = format!("{{AgentRequest(filter:{{{filter}}}){{{ORIGIN_FIELDS} {VIEW_FIELDS}}}}}");
    Ok(reader
        .data(&query, operation)
        .await?
        .get("AgentRequest")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default())
}

fn caused_request_view(row: &Value, causing: Option<&SessionScope>) -> Option<CausedRequestView> {
    Some(CausedRequestView {
        request_id: string_field(row, "request_id")?,
        request_doc_id: string_field(row, "_docID")?,
        session_id: string_field(row, "session_id"),
        agent_did: string_field(row, "agent_did"),
        requester_did: string_field(row, "requester_did"),
        behavior_id: string_field(row, "behavior_id"),
        lifecycle_state: string_field(row, "lifecycle_state"),
        interrupt_requested_at: string_field(row, "interrupt_requested_at"),
        created_at: string_field(row, "created_at"),
        hop: row.get("subagent_depth").and_then(Value::as_i64),
        caused_by_request_id: string_field(row, "caused_by_parent_request_id"),
        caused_by_request_doc_id: string_field(row, "caused_by_parent_request_doc_id"),
        caused_by_tool_call_id: string_field(row, "caused_by_parent_tool_call_id"),
        caused_by_tool_call_doc_id: string_field(row, "caused_by_parent_tool_call_doc_id"),
        caused_by_session_id: causing.map(|scope| scope.session_id.clone()),
    })
}

fn quoted_list(values: &[String]) -> String {
    values
        .iter()
        .map(|value| format!("\"{}\"", gents::graphql::escape_graphql_string(value)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn string_field(row: &Value, field: &str) -> Option<String> {
    row.get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}
