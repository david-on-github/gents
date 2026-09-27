use super::*;
use anyhow::Context;

use crate::session_message::{CreateSessionArgs, SendMessageArgs};

impl DefraSessionHook {
    /// Dispatch `create_session`/`send_message`. The accepted call was
    /// published in background: it returns its receipt immediately and stays
    /// running until the request it caused reaches a durable terminal, which
    /// the background completion observer delivers as a notification.
    pub(super) async fn persist_session_message_tool_call(
        &self,
        tool_name: &str,
        tool_call_id: Option<String>,
        internal_call_id: &str,
        args: &str,
    ) -> anyhow::Result<ToolCallHookAction> {
        let (session_id, request_id, deadline_at, _seq) =
            self.ensure_assistant_turn_sequence().await?;
        let mut lifecycle = self
            .adopt_accepted_tool_dispatch(
                internal_call_id,
                tool_call_id.as_deref(),
                &request_id,
                &session_id,
                tool_name,
                args,
                deadline_at,
                AwaitMode::Background,
            )
            .await?;
        macro_rules! refuse {
            ($class:expr, $payload:expr) => {{
                let payload = $payload;
                lifecycle.spawn_failed($class, &payload).await?;
                return Ok(self.skip_tool_result(tool_name, payload));
            }};
        }

        let caller_doc_id = lifecycle
            .request_doc_id()
            .context("session-message dispatch lacks its calling request document")?
            .to_owned();
        let caller =
            crate::request_binding::load_agent_request_by_doc_id(&self.node, &caller_doc_id)
                .await?
                .context("session-message calling request disappeared")?;
        let tools = crate::session_message::load_caller_session_tools(
            &self.node,
            &caller.agent_did,
            &caller.behavior_id,
        )
        .await?;
        if !tools.enabled {
            refuse!(
                FailureClass::ServiceUnavailable,
                tool_not_allowed_payload(
                    tool_name,
                    "/",
                    tool_name,
                    "create_session and send_message are not enabled for this behavior",
                    tools.names(),
                )
            );
        }

        let create = tool_name == CREATE_SESSION_TOOL_NAME;
        let (target, body, title) = if create {
            let parsed = match serde_json::from_str::<CreateSessionArgs>(args) {
                Ok(parsed) => parsed,
                Err(error) => refuse!(
                    FailureClass::ArgumentInvalid,
                    invalid_tool_arguments_payload(
                        tool_name,
                        "/",
                        format!("invalid create_session arguments: {error}"),
                    )
                ),
            };
            let agent = parsed.agent.trim().to_owned();
            let Some(target) = tools.target(&agent).cloned() else {
                refuse!(
                    FailureClass::ArgumentInvalid,
                    tool_not_allowed_payload(
                        tool_name,
                        "/agent",
                        &agent,
                        format!("'{agent}' is not an allowed agent for this behavior"),
                        tools.names(),
                    )
                );
            };
            if target.target_agent_did == caller.agent_did
                && load_agent_behavior(&self.node, &target.behavior_id)
                    .await?
                    .is_none()
            {
                refuse!(
                    FailureClass::ServiceUnavailable,
                    service_unavailable_payload(
                        tool_name,
                        "/agent",
                        format!(
                            "agent '{agent}' refers to behavior '{}' which no longer exists",
                            target.behavior_id
                        ),
                        false,
                    )
                );
            }
            (
                crate::lifecycle::SessionMessageTarget {
                    agent_did: target.target_agent_did,
                    behavior_id: target.behavior_id,
                    session_id: uuid::Uuid::new_v4().to_string(),
                },
                (parsed.prompt, parsed.task),
                parsed.title,
            )
        } else {
            let parsed = match serde_json::from_str::<SendMessageArgs>(args) {
                Ok(parsed) => parsed,
                Err(error) => refuse!(
                    FailureClass::ArgumentInvalid,
                    invalid_tool_arguments_payload(
                        tool_name,
                        "/",
                        format!("invalid send_message arguments: {error}"),
                    )
                ),
            };
            let target_session = parsed.session_id.trim().to_owned();
            let Some(target) = crate::session_message::resolve_send_target(
                &self.node,
                &caller.agent_did,
                &tools,
                &target_session,
            )
            .await?
            else {
                refuse!(
                    FailureClass::ArgumentInvalid,
                    tool_not_allowed_payload(
                        tool_name,
                        "/session_id",
                        &target_session,
                        "session is neither this agent's own nor one it started on an allowed agent",
                        tools.names(),
                    )
                );
            };
            (target, (parsed.prompt, parsed.task), None)
        };
        let body = match owned_body(body.0, body.1) {
            Ok(body) => body,
            Err(message) => refuse!(
                FailureClass::ArgumentInvalid,
                invalid_tool_arguments_payload(tool_name, "/", message)
            ),
        };

        let live = count_live_backgrounded_rows(&self.node, &request_id).await?;
        if live >= MAX_BACKGROUNDED_TOOLS_PER_PARENT {
            refuse!(
                FailureClass::ArgumentInvalid,
                background_budget_exceeded_payload(live)
            );
        }
        let caller_hop = caller.subagent_depth;
        let hop = crate::lifecycle::next_request_hop(
            crate::lifecycle::RequestHopCause::ToolCall,
            caller_hop,
        );
        if target.agent_did == caller.agent_did {
            let max_request_hop =
                crate::document_config::load_agent_principal(&self.node, &caller.agent_did)
                    .await?
                    .and_then(|principal| principal.max_request_hop)
                    .unwrap_or(crate::document_config::DEFAULT_MAX_REQUEST_HOP);
            if !crate::lifecycle::request_hop_within_bound(max_request_hop, hop) {
                refuse!(
                    FailureClass::ArgumentInvalid,
                    hop_exceeded_payload(tool_name, hop, max_request_hop)
                );
            }
        }
        let rendered = match crate::session_message::render_body(
            &self.node,
            &caller.agent_did,
            &target.behavior_id,
            body.as_body(),
        )
        .await?
        {
            Ok(rendered) => rendered,
            Err(message) => refuse!(
                FailureClass::ArgumentInvalid,
                invalid_tool_arguments_payload(tool_name, "/task", message)
            ),
        };

        let tool_call_doc_id = lifecycle
            .doc_id()
            .context("session-message dispatch lacks its physical row")?
            .to_owned();
        let cause = crate::lifecycle::SessionMessageCause {
            caller_agent_did: caller.agent_did.clone(),
            caller_request_id: caller.request_id.clone(),
            caller_request_doc_id: caller.doc_id.clone(),
            caller_hop,
            tool_call_id: lifecycle.tool_call_id().to_owned(),
            tool_call_doc_id,
            correlation: caller.caused_by_correlation.clone(),
        };
        let plan = match crate::session_message::plan(
            &self.node,
            &cause,
            &target,
            rendered,
            title.as_deref(),
        )
        .await?
        {
            Ok(plan) => plan,
            Err(message) => refuse!(
                FailureClass::ArgumentInvalid,
                invalid_tool_arguments_payload(tool_name, "/task", message)
            ),
        };
        // The row outlives the calling request; its own deadline is only the
        // background backstop. The caused request settles it first.
        lifecycle.set_deadline_at(
            chrono::Utc::now()
                + chrono::Duration::seconds(crate::toolset::BACKGROUND_COMMAND_TIMEOUT_SECS as i64),
        );
        lifecycle.start_running().await?;
        let receipt =
            match crate::session_message::commit(&self.node, &cause, &mut lifecycle, plan, !create)
                .await
            {
                Ok(receipt) => serde_json::to_string(&receipt)?,
                Err(error) => {
                    // Nothing was delivered, so the invocation reply is the failure.
                    let payload = service_unavailable_payload(
                        tool_name,
                        "/",
                        format!("the message could not be delivered: {error:#}"),
                        true,
                    );
                    lifecycle
                        .fail_owned(&payload, FailureClass::ServiceUnavailable, None)
                        .await?;
                    return Ok(self.skip_tool_result(tool_name, payload));
                }
            };
        Ok(self.skip_tool_result(tool_name, receipt))
    }
}

/// An owned message body, parsed once from the accepted arguments.
enum OwnedBody {
    Prompt(String),
    Task(crate::session_message::TaskBody),
}

impl OwnedBody {
    fn as_body(&self) -> crate::session_message::MessageBody<'_> {
        match self {
            Self::Prompt(prompt) => crate::session_message::MessageBody::Prompt(prompt),
            Self::Task(task) => crate::session_message::MessageBody::Task(task),
        }
    }
}

fn owned_body(
    prompt: Option<String>,
    task: Option<crate::session_message::TaskBody>,
) -> Result<OwnedBody, String> {
    match (prompt, task) {
        (Some(prompt), None) if !prompt.trim().is_empty() => {
            Ok(OwnedBody::Prompt(prompt.trim().to_owned()))
        }
        (None, Some(task)) if !task.task_id.trim().is_empty() => Ok(OwnedBody::Task(task)),
        (Some(_), None) => Err("prompt must be non-empty".to_owned()),
        (None, Some(_)) => Err("task.task_id must be non-empty".to_owned()),
        _ => Err("provide exactly one of prompt or task".to_owned()),
    }
}

fn hop_exceeded_payload(tool_name: &str, hop: u32, max_request_hop: u32) -> String {
    json_string(json!({
        "ok": false,
        "failure_class": "invalid_tool_arguments",
        "code": "request_hop_exceeded",
        "path": "/",
        "message": format!(
            "this message would be hop {hop}, beyond the principal's max_request_hop {max_request_hop}"
        ),
        "retryable": false,
        "service_id": "session",
        "tool_name": tool_name,
        "hop": hop,
        "max_request_hop": max_request_hop
    }))
}
