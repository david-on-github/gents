use super::*;
use gents::session::canonical_rows::{
    decode_output_segment_row, decode_transcript_message_row, AGENT_MESSAGE_FIELDS,
    AGENT_OUTPUT_SEGMENT_FIELDS,
};

/// An exact prompt-owner lookup is independent of session-history coverage.
/// Only an unfinished request needs live output facts. Keep the existing
/// canonical dependency cap: overflow must not masquerade as complete input
/// to the live target selector or as evidence that a prompt is absent.
const MAX_TIP_REQUEST_ROWS: usize = 2_048;

pub async fn load_session_tip_store(
    node: &EmbeddedNode,
    request: &AgentRequestRow,
) -> Result<ClientStore> {
    let query = tip_query(request)?;
    let data = execute_local_graphql_query(node, &query, "session tip").await?;
    tip_store(&data)
}

pub async fn load_session_tip_store_on(
    access: &gents::config_client::ConfigAccess,
    request: &AgentRequestRow,
) -> Result<ClientStore> {
    let query = tip_query(request)?;
    let data = execute_access_graphql_query(access, &query, "session tip").await?;
    tip_store(&data)
}

fn tip_query(request: &AgentRequestRow) -> Result<String> {
    let doc = escape_graphql_string(
        request
            .doc_id
            .as_deref()
            .context("tip request lacks physical identity")?,
    );
    let agent = escape_graphql_string(
        request
            .agent_did
            .as_deref()
            .context("tip request lacks principal")?,
    );
    let session = escape_graphql_string(
        request
            .session_id
            .as_deref()
            .context("tip request lacks session")?,
    );
    let requester = request
        .requester_did
        .as_deref()
        .map(|value| format!("\"{}\"", escape_graphql_string(value)))
        .unwrap_or_else(|| "null".into());
    let scope = format!(
        r#"request_doc_id: {{ _eq: "{doc}" }}, agent_did: {{ _eq: "{agent}" }}, session_id: {{ _eq: "{session}" }}, requester_did: {{ _eq: {requester} }}"#
    );
    let live = request
        .lifecycle_state
        .is_some_and(|state| !state.is_terminal());
    let limit = MAX_TIP_REQUEST_ROWS + 1;
    let header_filter = if live {
        scope.clone()
    } else {
        let key = escape_graphql_string(&format!(
            "authored:{}:prompt",
            request.doc_id.as_deref().unwrap()
        ));
        format!(r#"{scope}, message_key: {{ _eq: "{key}" }}"#)
    };
    let headers_limit = if live { limit } else { 2 };
    let segments = if live {
        format!(
            r#"AgentOutputSegment(filter: {{ {scope} }}, limit: {limit}) {{ {AGENT_OUTPUT_SEGMENT_FIELDS} }}"#
        )
    } else {
        String::new()
    };
    Ok(format!(
        r#"query DesktopSessionTip {{
        AgentMessage(filter: {{ {header_filter} }}, limit: {headers_limit}) {{ {AGENT_MESSAGE_FIELDS} }}
        {segments}
    }}"#
    ))
}

fn tip_store(data: &Value) -> Result<ClientStore> {
    let messages = parse_canonical_rows(data, AGENT_MESSAGE_NAME, decode_transcript_message_row)?;
    let segments = if data.get(AGENT_OUTPUT_SEGMENT_NAME).is_some() {
        parse_canonical_rows(data, AGENT_OUTPUT_SEGMENT_NAME, decode_output_segment_row)?
    } else {
        Vec::new()
    };
    anyhow::ensure!(
        messages.len() <= MAX_TIP_REQUEST_ROWS && segments.len() <= MAX_TIP_REQUEST_ROWS,
        "active request exceeds bounded session-tip read of {MAX_TIP_REQUEST_ROWS} rows"
    );
    Ok(ClientStore::from_rows(ClientStoreRows {
        transcript_messages: messages,
        output_segments: segments,
        ..Default::default()
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gents::config_client::ConfigAccess;
    use gents_protocol::request_lifecycle::RequestLifecycleState;

    #[tokio::test]
    async fn reopening_completed_request_reads_only_its_prompt_and_active_reads_stay_request_scoped(
    ) {
        let node = defra_node::NodeBuilder::default().build().await.unwrap();
        crate::client::schema::ensure_runtime_schemas(&node)
            .await
            .unwrap();
        for request in ["completed", "active"] {
            let count = if request == "completed" { 120 } else { 1 };
            let mutations = (0..count)
                .map(|sequence| {
                    let key = if sequence == 0 {
                        format!("authored:{request}:prompt")
                    } else {
                        format!("{request}:{sequence}")
                    };
                    let key = escape_graphql_string(&key);
                    let request = escape_graphql_string(request);
                    format!(
                        r#"m{sequence}: create_AgentMessage(input: {{
                    message_key: "{key}", session_id: "session", agent_did: "agent",
                    request_doc_id: "{request}", requester_did: null,
                    publication: {{kind: "request_execution", execution_generation: "generation"}},
                    outcome: "complete", sequence: {sequence}, role: "user", blocks: null,
                    created_at: "2026-09-30T00:00:00Z"
                }}) {{ _docID }}"#
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            ConfigAccess::write_local(
                &node,
                "test.tip_seed",
                &format!("mutation {{ {mutations} }}"),
            )
            .await
            .unwrap();
        }
        // A historic payload is deliberately undecodable: selecting the whole
        // session would fail even though neither tip needs these bytes.
        ConfigAccess::write_local(&node, "test.tip_history", r#"mutation {
            create_AgentOutputSegment(input: {agent_did:"agent", session_id:"session",
                request_doc_id:"completed", source:{kind:"not_a_source"}, payload:"historic"}) { _docID }
        }"#).await.unwrap();
        for (id, state) in [
            ("completed", RequestLifecycleState::Completed),
            ("active", RequestLifecycleState::Processing),
        ] {
            let request = AgentRequestRow {
                doc_id: Some(id.into()),
                request_id: id.into(),
                agent_did: Some("agent".into()),
                session_id: Some("session".into()),
                lifecycle_state: Some(state),
                ..Default::default()
            };
            let tip = load_session_tip_store(&node, &request).await.unwrap();
            assert_eq!(tip.transcript_messages.len(), 1);
            assert_eq!(
                tip.transcript_messages[0].message.message_key,
                format!("authored:{id}:prompt")
            );
            assert!(tip.output_segments.is_empty());
        }
        node.shutdown().await;
    }
}
