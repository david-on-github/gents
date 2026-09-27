//! `create_session` and `send_message`: the two model tools that start or
//! continue another agent's session. Both materialize one request through the
//! session-message writer in `lifecycle::materialize`, which records the
//! calling edge and the causal hop. Every agent is an ordinary agent addressed
//! directly: the started session runs under its own behavior, and its result
//! reaches the caller only as a background completion message.

use anyhow::{Context, Result};
use defra_node::EmbeddedNode;
use serde::Deserialize;

use crate::document_config::SubagentTargetDocument;
use crate::graphql::escape_graphql_string;
use crate::lifecycle::{SessionMessageCause, SessionMessageTarget};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskBody {
    pub task_id: String,
    #[serde(default)]
    pub input: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateSessionArgs {
    pub agent: String,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub task: Option<TaskBody>,
    #[serde(default)]
    pub title: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SendMessageArgs {
    pub session_id: String,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub task: Option<TaskBody>,
}

pub(crate) enum MessageBody<'a> {
    Prompt(&'a str),
    Task(&'a TaskBody),
}

fn message_body<'a>(
    prompt: Option<&'a String>,
    task: Option<&'a TaskBody>,
) -> Result<MessageBody<'a>, String> {
    match (prompt.map(|prompt| prompt.trim()), task) {
        (Some(prompt), None) if !prompt.is_empty() => Ok(MessageBody::Prompt(prompt)),
        (None, Some(task)) if !task.task_id.trim().is_empty() => Ok(MessageBody::Task(task)),
        (Some(_), None) => Err("prompt must be non-empty".to_owned()),
        (None, Some(_)) => Err("task.task_id must be non-empty".to_owned()),
        _ => Err("provide exactly one of prompt or task".to_owned()),
    }
}

impl CreateSessionArgs {
    pub(crate) fn body(&self) -> Result<MessageBody<'_>, String> {
        message_body(self.prompt.as_ref(), self.task.as_ref())
    }
}

impl SendMessageArgs {
    pub(crate) fn body(&self) -> Result<MessageBody<'_>, String> {
        message_body(self.prompt.as_ref(), self.task.as_ref())
    }
}

/// How a `send_message` reached its session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Delivery {
    /// An idle session received a new request.
    Request,
    /// A busy session received a user-origin append queued after its active
    /// request.
    Steering,
}

impl Delivery {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Request => "request",
            Self::Steering => "steering",
        }
    }
}

/// A message body rendered for its target: the content, and the Goal a Task
/// declares, if any.
pub(crate) struct RenderedBody {
    pub content: String,
    pub goal: Option<(String, Option<i64>)>,
}

/// The caller's session-message configuration, read through the canonical
/// configuration owner: the `SubagentTools.enabled` flag and the resolved
/// target allowlist.
pub(crate) struct CallerSessionTools {
    pub enabled: bool,
    pub targets: Vec<SubagentTargetDocument>,
}

impl CallerSessionTools {
    pub(crate) fn target(&self, name: &str) -> Option<&SubagentTargetDocument> {
        self.targets.iter().find(|target| target.name == name)
    }

    pub(crate) fn names(&self) -> Vec<String> {
        self.targets
            .iter()
            .map(|target| target.name.clone())
            .collect()
    }

    fn allows(&self, agent_did: &str, behavior_id: &str) -> bool {
        self.targets
            .iter()
            .any(|target| target.target_agent_did == agent_did && target.behavior_id == behavior_id)
    }
}

pub(crate) async fn load_caller_session_tools(
    node: &EmbeddedNode,
    agent_did: &str,
    behavior_id: &str,
) -> Result<CallerSessionTools> {
    use crate::collection::Collection;
    use crate::config_client::{read_desired_state_document_in_txn as read, ConfigAccess};
    use crate::document_config::{AgentBehavior, AgentContext, Tools};
    let owner = agent_did.to_owned();
    let behavior_id = behavior_id.to_owned();
    ConfigAccess::transact_local(node, None, "session_message.caller_tools", move |txn| {
        let owner = owner.clone();
        let behavior_id = behavior_id.clone();
        Box::pin(async move {
            let disabled = CallerSessionTools {
                enabled: false,
                targets: Vec::new(),
            };
            let Some(behavior) = read(txn, Collection::AgentBehavior, &owner, &behavior_id).await?
            else {
                return Ok(disabled);
            };
            let behavior: AgentBehavior = serde_json::from_value(behavior)?;
            let Some(context_id) = behavior.context_id else {
                return Ok(disabled);
            };
            let Some(context) = read(txn, Collection::AgentContext, &owner, &context_id).await?
            else {
                return Ok(disabled);
            };
            let context: AgentContext = serde_json::from_value(context)?;
            let Some(tools_id) = context.tools_id else {
                return Ok(disabled);
            };
            let Some(tools) = read(txn, Collection::Tools, &owner, &tools_id).await? else {
                return Ok(disabled);
            };
            let tools: Tools = serde_json::from_value(tools)?;
            let Some(group) = tools.subagents else {
                return Ok(disabled);
            };
            let mut targets = Vec::new();
            for id in &group.target_ids {
                if let Some(target) = read(txn, Collection::SubagentTarget, &owner, id).await? {
                    targets.push(serde_json::from_value::<SubagentTargetDocument>(target)?);
                }
            }
            Ok(CallerSessionTools {
                enabled: group.enabled.unwrap_or(false),
                targets,
            })
        })
    })
    .await
}

/// Render a message body. A Task is the caller's own configuration: its
/// prompt template is rendered with `task.input` as `args`, and its Goal
/// declaration, if any, is rendered for the target session.
pub(crate) async fn render_body(
    node: &EmbeddedNode,
    caller_agent_did: &str,
    target_behavior_id: &str,
    body: MessageBody<'_>,
) -> Result<Result<RenderedBody, String>> {
    let task = match body {
        MessageBody::Prompt(prompt) => {
            return Ok(Ok(RenderedBody {
                content: prompt.to_owned(),
                goal: None,
            }))
        }
        MessageBody::Task(task) => task,
    };
    let task_id = task.task_id.trim();
    let loaded = crate::config_client::ConfigAccess::transact_local(
        node,
        None,
        "session_message.load_task",
        {
            let owner = caller_agent_did.to_owned();
            let task_id = task_id.to_owned();
            move |txn| {
                let owner = owner.clone();
                let task_id = task_id.clone();
                Box::pin(async move {
                    crate::config_client::read_desired_state_document_in_txn(
                        txn,
                        crate::collection::Collection::Task,
                        &owner,
                        &task_id,
                    )
                    .await
                })
            }
        },
    )
    .await?;
    let Some(loaded) = loaded else {
        return Ok(Err(format!("task '{task_id}' is not configured")));
    };
    let task_document: crate::document_config::Task =
        serde_json::from_value(loaded).context("decode configured Task")?;
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let (node_scope, ctx_scope) =
        crate::template::task_node_ctx(caller_agent_did, target_behavior_id, &now);
    let scope = crate::template::TemplateScope {
        event: serde_json::Value::Null,
        doc: None,
        args: Some(task.input.clone().unwrap_or(serde_json::json!({}))),
        group: None,
        node: node_scope,
        ctx: ctx_scope,
    };
    let content = match crate::template::render_template(&task_document.prompt_template, &scope) {
        Ok(content) => content,
        Err(error) => return Ok(Err(format!("task prompt template: {error}"))),
    };
    let goal = match task_document.goal_objective_template.as_deref() {
        Some(template) => match crate::template::render_template(template, &scope) {
            Ok(objective) => Some((objective, task_document.goal_token_budget)),
            Err(error) => return Ok(Err(format!("task goal template: {error}"))),
        },
        None => None,
    };
    if let Err(error) = crate::goal::validate_task_goal_declaration(
        goal.as_ref().map(|(objective, _)| objective.as_str()),
        task_document.goal_token_budget,
    ) {
        return Ok(Err(format!("task goal declaration: {error}")));
    }
    Ok(Ok(RenderedBody { content, goal }))
}

/// Resolve the session `send_message` addresses. The caller may address a
/// session in its own requester scope: one of its own agent's sessions, or a
/// session its `create_session` started on a target that is still in its
/// allowlist. ACP decides whether the resulting write is accepted.
pub(crate) async fn resolve_send_target(
    node: &EmbeddedNode,
    caller_agent_did: &str,
    tools: &CallerSessionTools,
    session_id: &str,
) -> Result<Option<SessionMessageTarget>> {
    #[derive(Deserialize)]
    struct OriginRow {
        agent_did: Option<String>,
        behavior_id: Option<String>,
        #[serde(default)]
        caused_by_parent_tool_call_doc_id: Option<String>,
    }
    let query = format!(
        r#"{{ AgentRequest(filter: {{ session_id: {{ _eq: "{}" }}, requester_did: {{ _eq: "{}" }}, purpose: {{ _eq: "normal" }} }}, order: [{{ created_at: ASC }}, {{ request_id: ASC }}], limit: 1) {{
            agent_did behavior_id caused_by_parent_tool_call_doc_id
        }} }}"#,
        escape_graphql_string(session_id),
        escape_graphql_string(caller_agent_did),
    );
    let response = crate::graphql::graphql_with_transaction_retry(
        node,
        &query,
        "resolve send_message session origin",
    )
    .await?;
    let Some(origin) = crate::graphql::first_row::<OriginRow>(&response, "AgentRequest")? else {
        return Ok(None);
    };
    let (Some(agent_did), Some(behavior_id)) = (origin.agent_did, origin.behavior_id) else {
        return Ok(None);
    };
    let own = agent_did == caller_agent_did;
    let started = origin.caused_by_parent_tool_call_doc_id.is_some()
        && tools.allows(&agent_did, &behavior_id);
    if !own && !started {
        return Ok(None);
    }
    Ok(Some(SessionMessageTarget {
        agent_did,
        behavior_id,
        session_id: session_id.to_owned(),
    }))
}

enum PlannedWrite {
    Request(gents_protocol::request_admission::AgentRequestCreate),
    /// A control continuation of the busy session's active request, signed
    /// by that session's own principal (Lean `DurableLineage.steeringContinuation`).
    Steering {
        active: crate::AgentRequest,
        prepared: crate::lifecycle::queue::PreparedSteering,
    },
    Goal {
        create: gents_protocol::request_admission::AgentRequestCreate,
        objective: String,
        token_budget: Option<i64>,
    },
}

/// A session-message request and how it will be delivered, decided before
/// the tool row starts running.
pub(crate) struct Plan {
    session_id: String,
    write: PlannedWrite,
}

impl Plan {
    pub(crate) fn delivery(&self) -> Delivery {
        match self.write {
            PlannedWrite::Steering { .. } => Delivery::Steering,
            PlannedWrite::Request(_) | PlannedWrite::Goal { .. } => Delivery::Request,
        }
    }
}

/// Plan one session-message request. An idle or remote session gets a new
/// request; a busy local session gets a steering append queued after its
/// active request. A Task Goal is set on a local idle target session with its
/// request in one transaction.
pub(crate) async fn plan(
    node: &EmbeddedNode,
    cause: &SessionMessageCause,
    target: &SessionMessageTarget,
    rendered: RenderedBody,
    title: Option<&str>,
) -> Result<Result<Plan, String>> {
    let request_id = uuid::Uuid::new_v4().to_string();
    let local = target.agent_did == cause.caller_agent_did;
    let active = if local {
        crate::interrupt::active_session_request(
            node,
            &target.session_id,
            &target.agent_did,
            Some(&cause.caller_agent_did),
        )
        .await?
    } else {
        None
    };
    let write = if let Some((objective, token_budget)) = rendered.goal {
        if !local {
            return Ok(Err(
                "a Task that declares a Goal can only address this principal's own sessions"
                    .to_owned(),
            ));
        }
        if active.is_some() {
            return Ok(Err(
                "a Task that declares a Goal requires an idle session".to_owned()
            ));
        }
        let create = crate::lifecycle::build_session_message_request(
            cause,
            target,
            &rendered.content,
            title,
            &request_id,
            Some(format!("session-message:{}", cause.tool_call_doc_id)),
        )
        .await?;
        PlannedWrite::Goal {
            create,
            objective,
            token_budget,
        }
    } else if let Some(active) = active {
        let active_doc_id = active
            .doc_id
            .as_deref()
            .context("active session request lacks physical identity")?;
        let active = crate::request_binding::load_agent_request_by_doc_id(node, active_doc_id)
            .await?
            .context("active session request disappeared")?;
        let input = gents_protocol::request_input::RequestInput {
            queue: Some(gents_protocol::request_input::RequestQueue {
                source: gents_protocol::request_input::QueueSource::Steering,
                policy: gents_protocol::request_input::QueuePolicy::Append,
                key: None,
                queued_after_request_id: Some(active.request_id.clone()),
                interrupted_request_id: None,
                background_completion_wake_version: None,
            }),
            ..Default::default()
        };
        let prepared =
            crate::lifecycle::queue::prepare_steering_append(&active, &rendered.content, input)
                .await?;
        PlannedWrite::Steering { active, prepared }
    } else {
        PlannedWrite::Request(
            crate::lifecycle::build_session_message_request(
                cause,
                target,
                &rendered.content,
                title,
                &request_id,
                None,
            )
            .await?,
        )
    };
    Ok(Ok(Plan {
        session_id: target.session_id.clone(),
        write,
    }))
}

/// The receipt a session-message row answers its invocation with. It names
/// the exact request the call caused; settlement and cancellation read that
/// request's document from here.
#[derive(Debug, Clone, serde::Serialize, Deserialize)]
pub(crate) struct SessionMessageReceipt {
    pub ok: bool,
    pub session_id: String,
    pub request_id: String,
    pub request_doc_id: String,
    pub tool_call_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<String>,
    pub await_mode: String,
    pub status: String,
}

/// Persist a planned session-message request and, in the same transaction
/// where the writer allows, the running row's receipt naming it.
pub(crate) async fn commit(
    node: &EmbeddedNode,
    cause: &SessionMessageCause,
    lifecycle: &mut crate::tool_call_lifecycle::ToolCallLifecycle,
    plan: Plan,
    report_delivery: bool,
) -> Result<SessionMessageReceipt> {
    let delivery = report_delivery.then(|| plan.delivery().as_str().to_owned());
    let tool_call_id = lifecycle.tool_call_id().to_owned();
    let receipt_for =
        move |enqueued: &crate::lifecycle::EnqueuedAgentRequest| SessionMessageReceipt {
            ok: true,
            session_id: enqueued.session_id.clone(),
            request_id: enqueued.request_id.clone(),
            request_doc_id: enqueued.doc_id.clone(),
            tool_call_id: tool_call_id.clone(),
            delivery: delivery.clone(),
            await_mode: "background".to_owned(),
            status: "running".to_owned(),
        };
    let binding = lifecycle.background_receipt_binding()?;
    let session_id = plan.session_id;
    match plan.write {
        PlannedWrite::Request(create) => {
            let mutation = create.graphql_mutation().map_err(anyhow::Error::msg)?;
            let (mutation, binding, create, receipt_for) =
                (&mutation, &binding, &create, &receipt_for);
            crate::config_client::ConfigAccess::transact_local(
                node,
                None,
                "session_message.commit_request",
                move |txn| {
                    Box::pin(async move {
                        let response = txn.execute(mutation).await?;
                        let doc_id = crate::graphql::created_doc_id(&response, "AgentRequest")?;
                        let receipt = receipt_for(&crate::lifecycle::EnqueuedAgentRequest {
                            doc_id,
                            request_id: create.request_id.clone(),
                            session_id: create.session_id.clone(),
                        });
                        crate::tool_call_lifecycle::publish_background_receipt_in_txn(
                            txn,
                            binding,
                            &serde_json::to_string(&receipt)?,
                        )
                        .await?;
                        Ok(receipt)
                    })
                },
            )
            .await
        }
        PlannedWrite::Steering { active, prepared } => {
            let (active, prepared, binding, receipt_for) =
                (&active, &prepared, &binding, &receipt_for);
            crate::config_client::ConfigAccess::transact_local(
                node,
                None,
                "session_message.commit_steering",
                move |txn| {
                    Box::pin(async move {
                        let enqueued = crate::lifecycle::queue::append_prepared_steering_in_txn(
                            txn, active, prepared,
                        )
                        .await?;
                        let receipt = receipt_for(&enqueued);
                        crate::tool_call_lifecycle::publish_background_receipt_in_txn(
                            txn,
                            binding,
                            &serde_json::to_string(&receipt)?,
                        )
                        .await?;
                        Ok(receipt)
                    })
                },
            )
            .await
        }
        PlannedWrite::Goal {
            create,
            objective,
            token_budget,
        } => {
            let actor = ::identity::Did::new(cause.caller_agent_did.clone())
                .context("caller DID is not ACP-addressable")?;
            let enqueued = crate::goal::submit_goal_backed_request_local(
                node,
                actor,
                &create.agent_did,
                &session_id,
                &objective,
                token_budget,
                &create,
            )
            .await?;
            let receipt = receipt_for(&enqueued);
            lifecycle
                .publish_background_receipt(&serde_json::to_string(&receipt)?)
                .await?;
            Ok(receipt)
        }
    }
}

/// The receipt a session-message row published, if any.
pub(crate) async fn load_receipt(
    node: &std::sync::Arc<EmbeddedNode>,
    tool_call_doc_id: &str,
    agent_did: &str,
    session_id: &str,
    requester_did: Option<&str>,
) -> Result<Option<SessionMessageReceipt>> {
    let message = crate::tool_call_lifecycle::query::load_tool_call_result(
        &crate::config_client::ConfigAccess::Local(node.clone()),
        tool_call_doc_id,
        agent_did,
        session_id,
        requester_did,
    )
    .await?;
    let text = crate::tool_call_lifecycle::query::render_tool_result(&message)?;
    Ok(serde_json::from_str::<SessionMessageReceipt>(&text)
        .ok()
        .filter(|receipt| receipt.ok))
}

/// The one request a session-message row caused, read through the row's
/// receipt and checked against its lineage: either a session-message request
/// carrying the row's full calling edge under this requester, or a steering
/// continuation in the addressed session under this principal.
pub(crate) async fn load_caused_request(
    node: &std::sync::Arc<EmbeddedNode>,
    lifecycle: &crate::tool_call_lifecycle::ToolCallLifecycle,
) -> Result<Option<gents_protocol::row::AgentRequestRow>> {
    let tool_call_doc_id = lifecycle
        .doc_id()
        .context("session-message row lacks physical identity")?;
    let Some(receipt) = load_receipt(
        node,
        tool_call_doc_id,
        lifecycle.agent_did(),
        lifecycle.session_id(),
        lifecycle.requester_did(),
    )
    .await?
    else {
        return Ok(None);
    };
    let query = format!(
        r#"{{ AgentRequest(filter: {{ _docID: {{ _eq: "{}" }} }}, limit: 1) {{
            _docID request_id agent_did requester_did session_id lifecycle_state input
            caused_by_parent_request_doc_id caused_by_parent_tool_call_id
            caused_by_parent_tool_call_doc_id
        }} }}"#,
        escape_graphql_string(&receipt.request_doc_id),
    );
    let response = crate::graphql::graphql_with_transaction_retry(
        node,
        &query,
        "load the request a session-message row caused",
    )
    .await?;
    let Some(caused) = crate::graphql::first_row::<gents_protocol::row::AgentRequestRow>(
        &response,
        "AgentRequest",
    )?
    else {
        return Ok(None);
    };
    let caller = lifecycle.agent_did();
    let session_message = caused.requester_did.as_deref() == Some(caller)
        && caused.caused_by_parent_tool_call_doc_id.as_deref() == Some(tool_call_doc_id)
        && caused.caused_by_parent_tool_call_id.as_deref() == Some(lifecycle.tool_call_id())
        && caused.caused_by_parent_request_doc_id.as_deref() == lifecycle.request_doc_id();
    let steering = caused.agent_did.as_deref() == Some(caller)
        && caused.caused_by_parent_tool_call_doc_id.is_none()
        && caused
            .input
            .as_ref()
            .and_then(|input| input.queue.as_ref())
            .is_some_and(|queue| {
                queue.source == gents_protocol::request_input::QueueSource::Steering
            });
    if caused.request_id != receipt.request_id
        || caused.session_id.as_deref() != Some(receipt.session_id.as_str())
        || !(session_message || steering)
    {
        tracing::warn!(
            tool_call_doc_id,
            caused_request_id = %caused.request_id,
            "caused request does not match its session-message receipt"
        );
        return Ok(None);
    }
    Ok(Some(caused))
}

/// `cancel_process` on a session-message row interrupts only the one request
/// that row caused; the row settles when that request reaches its terminal.
/// Returns the interrupted request id, or `None` when it has no live request.
pub(crate) async fn interrupt_caused_request(
    node: &std::sync::Arc<EmbeddedNode>,
    lifecycle: &crate::tool_call_lifecycle::ToolCallLifecycle,
) -> Result<Option<String>> {
    let Some(caused) = load_caused_request(node, lifecycle).await? else {
        return Ok(None);
    };
    if caused
        .lifecycle_state
        .is_some_and(gents_protocol::request_lifecycle::RequestLifecycleState::is_terminal)
    {
        return Ok(None);
    }
    crate::interrupt::interrupt_request_by_doc_id(
        node,
        caused
            .doc_id
            .as_deref()
            .context("caused request lacks physical identity")?,
        caused
            .agent_did
            .as_deref()
            .context("caused request lacks agent_did")?,
        caused.requester_did.as_deref(),
    )
    .await?;
    Ok(Some(caused.request_id))
}
