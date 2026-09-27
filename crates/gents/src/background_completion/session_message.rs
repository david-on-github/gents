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
    #[serde(default)]
    request_doc_id: Option<String>,
    session_id: String,
    agent_did: String,
    #[serde(default)]
    requester_did: Option<String>,
    tool_call_id: String,
    tool_name: String,
}

const SESSION_MESSAGE_ROW_FIELDS: &str =
    "_docID request_id request_doc_id session_id agent_did requester_did tool_call_id tool_name";

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
/// keeps running: no parent fate settles it. The winner of the row's terminal
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
        match settle_row(node, local_did, &row).await {
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

/// Observer arm: a terminal AgentRequest whose
/// `caused_by_parent_tool_call_doc_id` names a local running session-message
/// row settles that row with the request's terminal output.
pub(super) async fn settle_row_caused_by(
    node: &Arc<EmbeddedNode>,
    local_did: &str,
    caused_request_doc_id: &str,
) -> Result<bool> {
    let query = format!(
        r#"{{ AgentRequest(filter: {{ _docID: {{ _eq: "{}" }} }}, limit: 1) {{
            _docID request_id lifecycle_state caused_by_parent_tool_call_doc_id
        }} }}"#,
        escape_graphql_string(caused_request_doc_id)
    );
    let response = crate::graphql::graphql_with_transaction_retry(
        node.as_ref(),
        &query,
        "load caused request for session-message settlement",
    )
    .await?;
    let Some(request) = crate::graphql::first_row::<AgentRequestRow>(&response, "AgentRequest")?
    else {
        return Ok(false);
    };
    if !request
        .lifecycle_state
        .is_some_and(RequestLifecycleState::is_terminal)
    {
        return Ok(false);
    }
    let Some(tool_doc_id) = request
        .caused_by_parent_tool_call_doc_id
        .as_deref()
        .filter(|id| !id.trim().is_empty())
    else {
        return Ok(false);
    };
    let query = format!(
        r#"{{ AgentToolCall(filter: {{ _docID: {{ _eq: "{}" }}, {} }}, limit: 1) {{ {SESSION_MESSAGE_ROW_FIELDS} }} }}"#,
        escape_graphql_string(tool_doc_id),
        running_session_message_filter(local_did)
    );
    let response = crate::graphql::graphql_with_transaction_retry(
        node.as_ref(),
        &query,
        "load session-message row named by a caused request",
    )
    .await?;
    let Some(row) = crate::graphql::first_row::<SessionMessageRow>(&response, "AgentToolCall")?
    else {
        return Ok(false);
    };
    Ok(settle_row(node, local_did, &row).await?.is_some())
}

enum Settled {
    Terminal,
    TimedOut,
}

async fn settle_row(
    node: &Arc<EmbeddedNode>,
    local_did: &str,
    row: &SessionMessageRow,
) -> Result<Option<Settled>> {
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
    if lifecycle.is_deadline_expired(Utc::now()) {
        if !lifecycle.timeout().await? {
            return Ok(None);
        }
        append_background_tool_completion(
            node.as_ref(),
            &row.session_id,
            &row.request_id,
            &row.doc_id,
            &row.tool_name,
            "failed",
            "",
            Some("deadline_exceeded"),
        )
        .await?;
        return Ok(Some(Settled::TimedOut));
    }
    let Some(caused_doc_id) = caused_request_doc_id(node, local_did, row).await? else {
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

/// The one request this row caused. Its lineage is signed by this principal
/// as requester, so a request naming this row under another requester or
/// another caller request is not this row's result.
async fn caused_request_doc_id(
    node: &Arc<EmbeddedNode>,
    local_did: &str,
    row: &SessionMessageRow,
) -> Result<Option<String>> {
    let query = format!(
        r#"{{ AgentRequest(filter: {{ caused_by_parent_tool_call_doc_id: {{ _eq: "{}" }} }}, limit: 2) {{
            _docID request_id requester_did caused_by_parent_request_doc_id caused_by_parent_tool_call_id
        }} }}"#,
        escape_graphql_string(&row.doc_id)
    );
    let response = crate::graphql::graphql_with_transaction_retry(
        node.as_ref(),
        &query,
        "load the request a session-message row caused",
    )
    .await?;
    let rows = crate::graphql::rows::<AgentRequestRow>(&response, "AgentRequest")?;
    let [caused] = rows.as_slice() else {
        if rows.len() > 1 {
            tracing::warn!(
                tool_call_doc_id = %row.doc_id,
                "session-message row names more than one caused request; left running"
            );
        }
        return Ok(None);
    };
    let linked = caused.requester_did.as_deref() == Some(local_did)
        && caused.caused_by_parent_request_doc_id.as_deref() == row.request_doc_id.as_deref()
        && caused.caused_by_parent_tool_call_id.as_deref() == Some(row.tool_call_id.as_str());
    if !linked {
        tracing::warn!(
            tool_call_doc_id = %row.doc_id,
            caused_request_id = %caused.request_id,
            "caused request lineage does not match its session-message row; left running"
        );
        return Ok(None);
    }
    Ok(caused.doc_id.clone())
}
