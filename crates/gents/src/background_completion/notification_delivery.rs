use super::*;

pub(super) struct SideEffects {
    pub(super) wake_request_id: Option<String>,
    pub(super) created_wake: bool,
}

/// Append a native background tool's completion notification and its
/// coalesced wake, which continues the calling session at its own hop.
pub(crate) async fn append_background_tool_completion(
    node: &EmbeddedNode,
    parent_session_id: &str,
    parent_request_id: &str,
    tool_call_doc_id: &str,
    tool_name: &str,
    status: &str,
    result: &str,
    reason: Option<&str>,
) -> Result<()> {
    append_completion(
        node,
        parent_session_id,
        parent_request_id,
        tool_call_doc_id,
        tool_name,
        status,
        result,
        reason,
        None,
    )
    .await
}

/// Append the completion of a session-message row whose caused request, at
/// `caused_hop`, reached its terminal. Its wake is caused by that other
/// session (Lean `DurableLineage.ContinuationKind.sessionMessageCompletionWake`)
/// and is refused, never the notification, beyond the woken principal's
/// `max_request_hop` (Lean `CausalHop.completionWake`).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn append_session_message_completion(
    node: &EmbeddedNode,
    parent_session_id: &str,
    parent_request_id: &str,
    tool_call_doc_id: &str,
    tool_name: &str,
    status: &str,
    result: &str,
    reason: Option<&str>,
    caused_hop: u32,
) -> Result<()> {
    append_completion(
        node,
        parent_session_id,
        parent_request_id,
        tool_call_doc_id,
        tool_name,
        status,
        result,
        reason,
        Some(caused_hop),
    )
    .await
}

/// Marks a notification whose wake the hop bound refused. Its presence in a
/// published notice is what a replay reproduces.
const WAKE_REFUSED_MARKER: &str = "<wake-refused";

fn wake_refused_note(hop: u32, max_request_hop: u32) -> String {
    format!(
        "\n{WAKE_REFUSED_MARKER} reason=\"request_hop_exceeded\" hop=\"{hop}\" max_request_hop=\"{max_request_hop}\">This result did not start a new turn: the agent-to-agent hop limit was reached. The session waits for its user.</wake-refused>"
    )
}

#[allow(clippy::too_many_arguments)]
async fn append_completion(
    node: &EmbeddedNode,
    parent_session_id: &str,
    parent_request_id: &str,
    tool_call_doc_id: &str,
    tool_name: &str,
    status: &str,
    result: &str,
    reason: Option<&str>,
    cross_session_cause_hop: Option<u32>,
) -> Result<()> {
    // Load the parent request up front so the completion notification is stamped
    // with the parent session's owning agent_did.
    let parent_request = crate::request_binding::load_agent_request(node, parent_request_id)
        .await?
        .ok_or_else(|| anyhow!("parent AgentRequest {parent_request_id} not found"))?;

    anyhow::ensure!(
        parent_request.session_id == parent_session_id,
        "background completion parent session mismatch"
    );
    let existing =
        existing_tool_completion_notification(node, &parent_request, tool_call_doc_id).await?;
    let tool_call_id = load_tool_call_id(node, tool_call_doc_id).await?;
    let existing_text = match &existing {
        Some(existing) => stored_notification_text(node, &parent_request, &existing.doc_id).await,
        None => None,
    };
    let wake = match cross_session_cause_hop {
        None => crate::lifecycle::queue::CompletionWake::AtHop(parent_request.subagent_depth),
        Some(cause_hop) => {
            let hop = crate::lifecycle::next_request_hop(
                crate::lifecycle::RequestHopCause::CrossSession { cause_hop },
                parent_request.subagent_depth,
            );
            let max_request_hop =
                crate::document_config::load_agent_principal(node, &parent_request.agent_did)
                    .await?
                    .and_then(|principal| principal.max_request_hop)
                    .unwrap_or(crate::document_config::DEFAULT_MAX_REQUEST_HOP);
            // A replay reproduces the published decision, not today's bound.
            let refused = match &existing_text {
                Some(text) => text.contains(WAKE_REFUSED_MARKER),
                None => !crate::lifecycle::request_hop_within_bound(max_request_hop, hop),
            };
            if refused {
                crate::lifecycle::queue::CompletionWake::RefusedByHopBound {
                    hop,
                    max_request_hop,
                }
            } else {
                crate::lifecycle::queue::CompletionWake::AtHop(hop)
            }
        }
    };
    let note = match &wake {
        crate::lifecycle::queue::CompletionWake::RefusedByHopBound {
            hop,
            max_request_hop,
        } => {
            let note = match &existing_text {
                Some(text) => text
                    .find(&format!("\n{WAKE_REFUSED_MARKER}"))
                    .map(|at| text[at..].to_owned())
                    .unwrap_or_else(|| wake_refused_note(*hop, *max_request_hop)),
                None => wake_refused_note(*hop, *max_request_hop),
            };
            tracing::warn!(
                parent_session_id,
                tool_call_doc_id,
                hop,
                max_request_hop,
                "session-message completion wake refused by the hop bound; notification appended"
            );
            Some(note)
        }
        crate::lifecycle::queue::CompletionWake::AtHop(_) => None,
    };
    let render = |budget: usize| {
        let (mut text, mut parts) =
            tool_completion_presentation(&tool_call_id, tool_name, status, result, reason, budget);
        if let Some(note) = &note {
            text.push_str(note);
            parts.push(gents_protocol::output::PresentationPart::Literal { text: note.clone() });
        }
        (text, parts)
    };
    // A published notice is replayed exactly (Lean ToolDelivery
    // notification replay), so a redrive renders with the budget that notice
    // was rendered under, not whatever the configuration says now.
    let published_budget = existing_text
        .as_deref()
        .and_then(|stored| published_notification_budget(stored, &render));
    let output_budget = match published_budget {
        Some(budget) => budget,
        None => crate::tool_surface::configured_output_budget(
            node,
            &parent_request.agent_did,
            &parent_request.behavior_id,
            tool_name,
        )
        .await
        .min(super::rendering::NOTIFICATION_SUMMARY_BYTES),
    };
    let (notification, presentation) = render(output_budget);
    let key = background_completion_notification_message_key(tool_call_doc_id, "tool");
    let effects = ensure_notification_delivery(
        node,
        &parent_request,
        existing,
        &notification,
        &key,
        wake,
        Some(crate::lifecycle::queue::ToolNotificationPublication {
            tool_call_doc_id: tool_call_doc_id.to_owned(),
            presentation,
        }),
    )
    .await?;
    mark_background_tool_notification_delivered(
        node,
        &parent_request.agent_did,
        parent_request_id,
        tool_call_doc_id,
    )
    .await?;
    mark_background_tool_completion_side_effects_done(node, tool_call_doc_id).await?;
    tracing::debug!(
        parent_session_id, parent_request_id, tool_call_doc_id,
        wake_request_id = ?effects.wake_request_id,
        created_wake = effects.created_wake,
        "persisted background completion side effects"
    );
    Ok(())
}

/// The stored text of an already published notice, or `None` when it cannot
/// be read, which leaves the atomic publication owner to reject a
/// conflicting replay.
async fn stored_notification_text(
    node: &EmbeddedNode,
    parent: &crate::AgentRequest,
    notification_doc_id: &str,
) -> Option<String> {
    let (_, message) = crate::session::load_canonical_message_from_node(
        node,
        notification_doc_id,
        &parent.agent_did,
        parent.requester_did.as_deref(),
    )
    .await
    .map_err(|error| {
        tracing::warn!(
            notification_doc_id,
            error = %format!("{error:#}"),
            "published background notification could not be read for replay"
        )
    })
    .ok()?;
    message.rag_text()
}

/// The summary budget an already published notice was rendered under: the
/// unescaped length of its `<result>` body, with or without a truncation
/// marker, whichever re-renders the stored text exactly. `None` when it
/// matches neither.
fn published_notification_budget(
    stored: &str,
    render: &impl Fn(usize) -> (String, Vec<gents_protocol::output::PresentationPart>),
) -> Option<usize> {
    let body = stored.split_once("<result>")?.1.split_once("</result>")?.0;
    let unescaped = body
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&");
    let candidates = [
        unescaped.strip_suffix("...").map(str::len),
        Some(unescaped.len()),
    ];
    candidates
        .into_iter()
        .flatten()
        .filter(|budget| *budget <= super::rendering::NOTIFICATION_SUMMARY_BYTES)
        .find(|budget| render(*budget).0 == stored)
}

async fn load_tool_call_id(node: &EmbeddedNode, tool_call_doc_id: &str) -> Result<String> {
    let doc_id = escape_graphql_string(tool_call_doc_id);
    let response = node
        .execute(&format!(
            r#"{{ AgentToolCall(filter: {{ _docID: {{ _eq: "{doc_id}" }} }}, limit: 2) {{ _docID tool_call_id }} }}"#
        ))
        .await;
    anyhow::ensure!(
        !response.has_errors(),
        "query background tool notification identity failed: {:?}",
        response.errors
    );
    let rows = crate::graphql::rows::<serde_json::Value>(&response, "AgentToolCall")?;
    anyhow::ensure!(
        rows.len() == 1,
        "background tool notification requires one exact physical lifecycle row"
    );
    rows[0]
        .get("tool_call_id")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
        .context("background tool notification lifecycle omitted tool_call_id")
}

/// Marker discovery supplies only an ID; the atomic owner reloads receipt and
/// Goal together before deciding whether any wake can be published.
pub(super) async fn ensure_notification_delivery(
    node: &EmbeddedNode,
    parent: &crate::AgentRequest,
    existing: Option<side_effects::ExistingNotification>,
    content: &str,
    message_key: &str,
    wake: crate::lifecycle::queue::CompletionWake,
    native: Option<crate::lifecycle::queue::ToolNotificationPublication>,
) -> Result<SideEffects> {
    let native = native.context("background notification requires its canonical provenance")?;
    let enqueued = crate::lifecycle::queue::persist_background_completion_with_message_canonical(
        node,
        parent,
        content,
        message_key,
        BACKGROUND_COMPLETION_WAKE_PROMPT,
        RequestQueue {
            source: QueueSource::BackgroundCompletion,
            policy: QueuePolicy::Coalesce,
            key: Some(format!("background_completion:{}", parent.session_id)),
            queued_after_request_id: Some(parent.request_id.clone()),
            interrupted_request_id: None,
            background_completion_wake_version: None,
        },
        existing.as_ref().map(|receipt| receipt.doc_id.as_str()),
        &native,
        wake,
    )
    .await?;
    Ok(SideEffects {
        wake_request_id: enqueued.request.map(|request| request.request_id),
        created_wake: enqueued.created_request,
    })
}

async fn mark_background_tool_notification_delivered(
    node: &EmbeddedNode,
    agent_did: &str,
    parent_request_id: &str,
    tool_call_doc_id: &str,
) -> Result<()> {
    let agent_did = escape_graphql_string(agent_did);
    let parent_request_id = escape_graphql_string(parent_request_id);
    let tool_call_doc_id = escape_graphql_string(tool_call_doc_id);
    let delivered_at = escape_graphql_string(&Utc::now().to_rfc3339());
    let mutation = format!(
        r#"mutation {{
            update_AgentToolCall(
                filter: {{
                    agent_did: {{ _eq: "{agent_did}" }},
                    request_id: {{ _eq: "{parent_request_id}" }},
                    _docID: {{ _eq: "{tool_call_doc_id}" }},
                    completion_notification_delivered_at: {{ _eq: null }}
                }},
                input: {{
                    completion_notification_delivered_at: "{delivered_at}"
                }}
            ) {{ _docID }}
        }}"#
    );
    crate::config_client::ConfigAccess::write_local_response(
        node,
        "mark_background_tool_notification_delivered",
        &mutation,
    )
    .await?;
    Ok(())
}

async fn mark_background_tool_completion_side_effects_done(
    node: &EmbeddedNode,
    tool_call_doc_id: &str,
) -> Result<()> {
    let tool_call_doc_id = escape_graphql_string(tool_call_doc_id);
    let query = format!(
        r#"{{
            AgentToolCall(
                filter: {{ _docID: {{ _eq: "{tool_call_doc_id}" }} }},
                limit: 1
            ) {{ _docID status lifecycle_state }}
        }}"#
    );
    let response = node.execute(&query).await;
    if response.has_errors() {
        anyhow::bail!(
            "query background completion tool row failed: {:?}",
            response.errors
        );
    }
    let row = response
        .data
        .as_ref()
        .and_then(|data| data.get("AgentToolCall"))
        .and_then(serde_json::Value::as_array)
        .and_then(|rows| rows.first())
        .ok_or_else(|| anyhow!("background completion tool row {tool_call_doc_id} not found"))?;
    let doc_id = row
        .get("_docID")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow!("background completion tool row {tool_call_doc_id} not found"))?;
    let status = row
        .get("status")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if status == "completed" {
        return Ok(());
    }
    let lifecycle_state = row
        .get("lifecycle_state")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if !(status == "completionPending" || status.starts_with("completionPending:"))
        || !matches!(
            lifecycle_state,
            "completed" | "failed" | "timedOut" | "cancelled"
        )
    {
        anyhow::bail!(
            "background completion tool row {tool_call_doc_id} is not awaiting terminal side effects"
        );
    }
    let escaped_doc_id = escape_graphql_string(doc_id);
    let escaped_status = escape_graphql_string(status);
    let datetime_fields = agent_tool_call_datetime_update_fragment(node, doc_id, &[]).await?;
    let mutation = format!(
        r#"mutation {{
            update_AgentToolCall(
                filter: {{
                    _docID: {{ _eq: "{escaped_doc_id}" }},
                    status: {{ _eq: "{escaped_status}" }}
                }},
                input: {{ status: "completed"{datetime_fields} }}
            ) {{ _docID }}
        }}"#
    );
    crate::config_client::ConfigAccess::write_local_response(
        node,
        "mark_background_completion_side_effects_done",
        &mutation,
    )
    .await?;
    Ok(())
}
