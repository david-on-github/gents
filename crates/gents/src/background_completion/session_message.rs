use super::*;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SessionMessageSettlementReport {
    /// Rows settled from their caused request's durable terminal.
    pub(crate) settled: usize,
    /// Rows whose own deadline passed first.
    pub(crate) timed_out: usize,
}

impl SessionMessageSettlementReport {
    pub(crate) fn total(&self) -> usize {
        self.settled + self.timed_out
    }
}

#[derive(Debug, Deserialize)]
struct SessionMessageRow {
    #[serde(rename = "_docID")]
    doc_id: String,
    request_id: String,
    session_id: String,
    agent_did: String,
    #[serde(default)]
    requester_did: Option<String>,
    tool_name: String,
}

const SESSION_MESSAGE_ROW_FIELDS: &str =
    "_docID request_id session_id agent_did requester_did tool_name";

fn running_session_message_filter(local_did: &str) -> String {
    format!(
        r#"agent_did: {{ _eq: "{}" }}, lifecycle_state: {{ _eq: "running" }}, await_mode: {{ _eq: "background" }}, spawned_by_tool_call_doc_id: {{ _eq: null }}, tool_name: {{ _in: ["{}", "{}"] }}"#,
        escape_graphql_string(local_did),
        crate::toolset::CREATE_SESSION_TOOL_NAME,
        crate::toolset::SEND_MESSAGE_TOOL_NAME,
    )
}

/// Settle every running local `create_session`/`send_message` row whose own
/// deadline passed or whose caused request reached a durable terminal (Lean
/// `Recovery.sessionMessageRecoverySweep`). A row with neither observation
/// keeps running: no parent fate settles it. The winner of a caused-terminal
/// compare appends its completion notification and wake.
pub(crate) async fn settle_running_session_message_rows(
    node: &Arc<EmbeddedNode>,
    local_did: &str,
) -> Result<SessionMessageSettlementReport> {
    let query = format!(
        r#"{{ AgentToolCall(filter: {{ {} }}) {{ {SESSION_MESSAGE_ROW_FIELDS} }} }}"#,
        running_session_message_filter(local_did)
    );
    let response = crate::graphql::graphql_with_transaction_retry(
        node.as_ref(),
        &query,
        "load running session-message rows",
    )
    .await?;
    let rows = crate::graphql::rows::<SessionMessageRow>(&response, "AgentToolCall")?;
    let mut report = SessionMessageSettlementReport::default();
    for row in rows {
        match settle_row(node, &row).await {
            Ok(Some(Settled::Terminal)) => report.settled += 1,
            Ok(Some(Settled::TimedOut)) => report.timed_out += 1,
            Ok(None) => {}
            Err(error) => tracing::warn!(
                tool_call_doc_id = %row.doc_id,
                error = %format!("{error:#}"),
                "session-message row settlement failed; will retry"
            ),
        }
    }
    Ok(report)
}

/// Observer arm: a request that reached a durable terminal may be the one a
/// local running session-message row caused. Each running row names its
/// caused request in its receipt, so the arm settles through those rows.
pub(super) async fn settle_rows_after_request_update(
    node: &Arc<EmbeddedNode>,
    local_did: &str,
    request_doc_id: &str,
) -> Result<usize> {
    let query = format!(
        r#"{{ AgentRequest(filter: {{ _docID: {{ _eq: "{}" }} }}, limit: 1) {{
            _docID request_id lifecycle_state
        }} }}"#,
        escape_graphql_string(request_doc_id)
    );
    let response = crate::graphql::graphql_with_transaction_retry(
        node.as_ref(),
        &query,
        "load updated request for session-message settlement",
    )
    .await?;
    let terminal = crate::graphql::first_row::<AgentRequestRow>(&response, "AgentRequest")?
        .and_then(|request| request.lifecycle_state)
        .is_some_and(RequestLifecycleState::is_terminal);
    if !terminal {
        return Ok(0);
    }
    Ok(settle_running_session_message_rows(node, local_did)
        .await?
        .total())
}

enum Settled {
    Terminal,
    TimedOut,
}

async fn settle_row(node: &Arc<EmbeddedNode>, row: &SessionMessageRow) -> Result<Option<Settled>> {
    let Some(mut lifecycle) = ToolCallLifecycle::load_by_doc_id(
        node.clone(),
        &row.doc_id,
        &row.agent_did,
        &row.session_id,
        row.requester_did.as_deref(),
    )
    .await?
    else {
        return Ok(None);
    };
    if !lifecycle.is_running() || !lifecycle.is_session_message() {
        return Ok(None);
    }
    // Lean `RestartRow.notification` owes no completion for an expired
    // session-message row: only the caused request's terminal is reported.
    if lifecycle.is_deadline_expired(Utc::now()) {
        return Ok(lifecycle.timeout().await?.then_some(Settled::TimedOut));
    }
    let Some(caused_doc_id) = crate::session_message::load_caused_request(node, &lifecycle)
        .await?
        .and_then(|caused| caused.doc_id)
    else {
        return Ok(None);
    };
    let Some(terminal) =
        crate::background_tools::load_caused_request_terminal(node.as_ref(), &caused_doc_id)
            .await?
    else {
        return Ok(None);
    };
    if !lifecycle.settle_session_message(&terminal).await? {
        return Ok(None);
    }
    append_background_tool_completion(
        node.as_ref(),
        &row.session_id,
        &row.request_id,
        &row.doc_id,
        &row.tool_name,
        terminal.notification_status(),
        terminal.output(),
        terminal.completion_reason(),
    )
    .await?;
    Ok(Some(Settled::Terminal))
}
