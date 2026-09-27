//! The agents tools. `agent_new` and `agent_message` start or continue
//! another agent's session; both materialize one request through the
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
pub(crate) struct AgentNewArgs {
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
pub(crate) struct AgentMessageArgs {
    pub session_id: String,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub task: Option<TaskBody>,
    /// Stop the session's current turn first; the message then arrives as a
    /// new request rather than steering.
    #[serde(default)]
    pub interrupt: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AgentInterruptArgs {
    pub session_id: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AgentListArgs {}

pub(crate) enum MessageBody<'a> {
    Prompt(&'a str),
    Task(&'a TaskBody),
}

fn message_body<'a>(
    field: &str,
    prompt: Option<&'a String>,
    task: Option<&'a TaskBody>,
) -> Result<MessageBody<'a>, String> {
    match (prompt.map(|prompt| prompt.trim()), task) {
        (Some(prompt), None) if !prompt.is_empty() => Ok(MessageBody::Prompt(prompt)),
        (None, Some(task)) if !task.task_id.trim().is_empty() => Ok(MessageBody::Task(task)),
        (Some(_), None) => Err(format!("{field} must be non-empty")),
        (None, Some(_)) => Err("task.task_id must be non-empty".to_owned()),
        _ => Err(format!("provide exactly one of {field} or task")),
    }
}

impl AgentNewArgs {
    pub(crate) fn body(&self) -> Result<MessageBody<'_>, String> {
        message_body("prompt", self.prompt.as_ref(), self.task.as_ref())
    }
}

impl AgentMessageArgs {
    pub(crate) fn body(&self) -> Result<MessageBody<'_>, String> {
        message_body("message", self.message.as_ref(), self.task.as_ref())
    }
}

/// How an `agent_message` reached its session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Delivery {
    /// An idle session received a new request.
    Request,
    /// A busy session received an agent-authored steering continuation
    /// queued after its active request.
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

/// Resolve the session `agent_message` addresses. The caller may address a
/// session in its own requester scope: one of its own agent's sessions, or a
/// session its `agent_new` started on a target that is still in its
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
        "resolve agent_message session origin",
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
    hop: u32,
    write: PlannedWrite,
}

impl Plan {
    /// The causal hop the planned request or continuation is written with.
    pub(crate) fn hop(&self) -> u32 {
        self.hop
    }

    pub(crate) fn delivery(&self) -> Delivery {
        match self.write {
            PlannedWrite::Steering { .. } => Delivery::Steering,
            PlannedWrite::Request(_) | PlannedWrite::Goal { .. } => Delivery::Request,
        }
    }
}

/// Plan one session-message request. An idle or remote session gets a new
/// request; a busy local session gets a steering append queued after its
/// active request, unless the caller interrupts that request first, in which
/// case the message is a new request. A Task Goal is set on a local idle target session with its
/// request in one transaction.
pub(crate) async fn plan(
    node: &EmbeddedNode,
    cause: &SessionMessageCause,
    target: &SessionMessageTarget,
    rendered: RenderedBody,
    title: Option<&str>,
    interrupt: bool,
) -> Result<Result<Plan, String>> {
    let request_id = uuid::Uuid::new_v4().to_string();
    let local = target.agent_did == cause.caller_agent_did;
    let active = if local {
        match crate::interrupt::active_session_request(
            node,
            &target.session_id,
            &target.agent_did,
            Some(&cause.caller_agent_did),
        )
        .await?
        {
            Some(active) => {
                let active_doc_id = active
                    .doc_id
                    .as_deref()
                    .context("active session request lacks physical identity")?;
                Some(
                    crate::request_binding::load_agent_request_by_doc_id(node, active_doc_id)
                        .await?
                        .context("active session request disappeared")?,
                )
            }
            None => None,
        }
    } else {
        None
    };
    // Lean `CausalHop.nextHop` of a cross-session cause: past the caller, and
    // never below the addressed session's current hop.
    let own_hop = match &active {
        Some(active) => active.subagent_depth.max(
            crate::lifecycle::load_session_current_hop(node, &target.agent_did, &target.session_id)
                .await?,
        ),
        None => {
            crate::lifecycle::load_session_current_hop(node, &target.agent_did, &target.session_id)
                .await?
        }
    };
    let hop = crate::lifecycle::next_request_hop(
        crate::lifecycle::RequestHopCause::CrossSession {
            cause_hop: cause.caller_hop,
        },
        own_hop,
    );
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
            hop,
        )
        .await?;
        PlannedWrite::Goal {
            create,
            objective,
            token_budget,
        }
    } else if let Some(active) = active.filter(|_| !interrupt) {
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
        let prepared = crate::lifecycle::queue::prepare_steering_append(
            &active,
            &rendered.content,
            input,
            hop,
        )
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
                hop,
            )
            .await?,
        )
    };
    Ok(Ok(Plan {
        session_id: target.session_id.clone(),
        hop,
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

/// Start the pending row, persist its planned request and publish the
/// receipt naming that request, all in one transaction: a running
/// session-message row always has a receipt and a caused request.
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
    let start = lifecycle.dispatch_start()?;
    let start = &start;
    let session_id = plan.session_id;
    let (receipt, started_at) = match plan.write {
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
                        let started_at = start_dispatch(txn, start).await?;
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
                        Ok((receipt, started_at))
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
                        let started_at = start_dispatch(txn, start).await?;
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
                        Ok((receipt, started_at))
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
            let (create, objective, session_id, binding, receipt_for) =
                (&create, &objective, &session_id, &binding, &receipt_for);
            crate::config_client::ConfigAccess::transact_local(
                node,
                Some(actor),
                "session_message.commit_goal",
                move |txn| {
                    Box::pin(async move {
                        let started_at = start_dispatch(txn, start).await?;
                        let enqueued = crate::goal::stage_goal_backed_request_in_txn(
                            txn,
                            &create.agent_did,
                            session_id,
                            objective,
                            token_budget,
                            create,
                        )
                        .await?;
                        let receipt = receipt_for(&enqueued);
                        crate::tool_call_lifecycle::publish_background_receipt_in_txn(
                            txn,
                            binding,
                            &serde_json::to_string(&receipt)?,
                        )
                        .await?;
                        Ok((receipt, started_at))
                    })
                },
            )
            .await
        }
    }?;
    lifecycle.mark_started(started_at);
    Ok(receipt)
}

async fn start_dispatch(
    txn: &crate::config_client::ConfigApplyTxn<'_>,
    start: &crate::tool_call_lifecycle::delivery::DispatchStart,
) -> Result<chrono::DateTime<chrono::Utc>> {
    crate::tool_call_lifecycle::delivery::start_running_in_txn(txn, start, None)
        .await?
        .context("session-message row was altered or is no longer pending")
}

/// The receipt a session-message row published: `Ok(None)` when the row has
/// no invocation reply, or its reply is not a session-message receipt.
pub(crate) async fn load_receipt(
    node: &std::sync::Arc<EmbeddedNode>,
    tool_call_doc_id: &str,
    agent_did: &str,
    session_id: &str,
    requester_did: Option<&str>,
) -> Result<Option<SessionMessageReceipt>> {
    let read = crate::tool_call_lifecycle::query::load_tool_call_read(
        &crate::config_client::ConfigAccess::Local(node.clone()),
        tool_call_doc_id,
        agent_did,
        session_id,
        requester_did,
    )
    .await?;
    let Some(message) = read.result else {
        return Ok(None);
    };
    let text = crate::tool_call_lifecycle::query::render_tool_result(&message)?;
    Ok(serde_json::from_str::<SessionMessageReceipt>(&text)
        .ok()
        .filter(|receipt| receipt.ok))
}

/// What a session-message row can observe of the one request it caused.
pub(crate) enum CausedObservation {
    /// The receipt names a request carrying the row's lineage.
    Bound(gents_protocol::row::AgentRequestRow),
    /// The named request is not visible here yet: a peer has not replicated
    /// it back (Lean premise on `SessionMessageRecoveryCause`).
    NotVisible,
    /// Lean `SessionMessageRecoveryCause.causedRequestUnbound`: the receipt
    /// is missing, or names a request that fails the row's lineage.
    Unbound(&'static str),
}

/// The one request a session-message row caused, read through the row's
/// receipt and checked against its lineage: either a session-message request
/// carrying the row's full calling edge under this requester, or a steering
/// continuation in the addressed session under this principal.
pub(crate) async fn observe_caused_request(
    node: &std::sync::Arc<EmbeddedNode>,
    lifecycle: &crate::tool_call_lifecycle::ToolCallLifecycle,
) -> Result<CausedObservation> {
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
        return Ok(CausedObservation::Unbound(
            "the row has no receipt naming the request it caused",
        ));
    };
    let query = format!(
        r#"{{ AgentRequest(filter: {{ _docID: {{ _eq: "{}" }} }}, limit: 1) {{
            _docID request_id agent_did requester_did session_id lifecycle_state input
            subagent_depth caused_by_parent_request_doc_id caused_by_parent_tool_call_id
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
        return Ok(CausedObservation::NotVisible);
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
        return Ok(CausedObservation::Unbound(
            "the receipt names a request that does not carry this call's lineage",
        ));
    }
    Ok(CausedObservation::Bound(caused))
}

/// The hop of a bound caused request, which its completion wake climbs past.
pub(crate) fn caused_hop(caused: &gents_protocol::row::AgentRequestRow) -> u32 {
    caused
        .subagent_depth
        .and_then(|hop| u32::try_from(hop).ok())
        .unwrap_or(0)
}

/// Lean `DurableLineage.interruptAllowed`: in 0.20 an agent may interrupt
/// another session only when that session's origin names the caller's
/// session as its cause, that is, when the caller started it. General
/// interrupt permissions come in a later release.
pub fn agent_interrupt_allowed(
    caller_session: &str,
    target_session: &str,
    target_origin_cause: Option<&str>,
) -> bool {
    target_session != caller_session && target_origin_cause == Some(caller_session)
}

/// The session that started `target`: the session of the request its origin
/// names in `caused_by_parent_request_doc_id` (`gents::session_origin`).
/// `None` for a root session or when that request is not visible here.
pub(crate) async fn origin_cause_session(
    node: &EmbeddedNode,
    target: &SessionMessageTarget,
) -> Result<Option<String>> {
    use crate::session_origin::{load_caused_session_origins, load_request_scope, OriginReader};
    let origins =
        load_caused_session_origins(OriginReader::Node(node), &target.session_id, "").await?;
    let Some(parent_doc_id) = origins
        .iter()
        .find(|row| row["agent_did"].as_str() == Some(target.agent_did.as_str()))
        .and_then(|row| row["caused_by_parent_request_doc_id"].as_str())
    else {
        return Ok(None);
    };
    Ok(load_request_scope(OriginReader::Node(node), parent_doc_id)
        .await?
        .map(|scope| scope.session_id))
}

/// Refusal reason when the calling session may not interrupt `target`.
pub(crate) async fn interrupt_refusal(
    node: &EmbeddedNode,
    caller_session: &str,
    target: &SessionMessageTarget,
) -> Result<Option<String>> {
    let cause = origin_cause_session(node, target).await?;
    Ok(
        (!agent_interrupt_allowed(caller_session, &target.session_id, cause.as_deref()))
            .then(|| "only the session that started this session may interrupt it".to_owned()),
    )
}

/// The one active request of `target`, if its session is mid-turn.
async fn active_request(
    node: &EmbeddedNode,
    target: &SessionMessageTarget,
) -> Result<Option<gents_protocol::row::AgentRequestRow>> {
    let query = format!(
        r#"{{ AgentRequest(filter: {{ session_id: {{ _eq: "{}" }}, agent_did: {{ _eq: "{}" }}, purpose: {{ _eq: "normal" }}, lifecycle_state: {{ _in: ["claimed", "processing"] }} }}, limit: 2) {{ _docID request_id agent_did requester_did session_id lifecycle_state }} }}"#,
        escape_graphql_string(&target.session_id),
        escape_graphql_string(&target.agent_did),
    );
    let response =
        crate::graphql::graphql_with_transaction_retry(node, &query, "load the active request")
            .await?;
    let mut rows =
        crate::graphql::rows::<gents_protocol::row::AgentRequestRow>(&response, "AgentRequest")?;
    anyhow::ensure!(rows.len() <= 1, "multiple active requests in one session");
    Ok(rows.pop())
}

/// Stop `target`'s current turn through the single-session interrupt owner.
/// Returns the interrupted request id, or `None` when the session is idle.
pub(crate) async fn interrupt_session(
    node: &EmbeddedNode,
    target: &SessionMessageTarget,
) -> Result<Option<String>> {
    let Some(active) = active_request(node, target).await? else {
        return Ok(None);
    };
    crate::interrupt::interrupt_request_by_doc_id(
        node,
        active
            .doc_id
            .as_deref()
            .context("active request lacks physical identity")?,
        &target.agent_did,
        active.requester_did.as_deref(),
    )
    .await?;
    Ok(Some(active.request_id))
}

/// A session `agent_list` reports and how the calling session relates to it.
#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub(crate) struct ReachableSession {
    pub session_id: String,
    pub agent_did: String,
    pub relationship: &'static str,
    pub status: String,
    pub can_message: bool,
    pub can_interrupt: bool,
}

/// `agent_list`: the agents the caller may start and the sessions it can
/// reach, read through `gents::session_origin` and the session-message
/// receipts of the calling session.
pub(crate) async fn agent_list(
    node: &std::sync::Arc<EmbeddedNode>,
    caller: &crate::AgentRequest,
    tools: &CallerSessionTools,
) -> Result<serde_json::Value> {
    use crate::session_origin::{
        load_caused_session_origins, load_request_scope, load_session_origins,
        load_session_request_doc_ids, OriginReader, SessionScope,
    };
    let reader = OriginReader::Node(node.as_ref());
    let mut found: Vec<(String, String, &'static str)> = Vec::new();
    let own = SessionScope {
        agent_did: caller.agent_did.clone(),
        session_id: caller.session_id.clone(),
        requester_did: caller.requester_did.clone(),
    };
    let own_doc_ids = load_session_request_doc_ids(reader, std::slice::from_ref(&own))
        .await?
        .into_iter()
        .map(|(doc_id, _)| doc_id)
        .collect::<Vec<_>>();
    for origin in load_session_origins(reader, &own_doc_ids, "").await? {
        if let (Some(session), Some(agent)) =
            (origin["session_id"].as_str(), origin["agent_did"].as_str())
        {
            if session != caller.session_id {
                found.push((session.to_owned(), agent.to_owned(), "started_by_you"));
            }
        }
    }
    for origin in load_caused_session_origins(reader, &caller.session_id, "").await? {
        if origin["agent_did"].as_str() != Some(caller.agent_did.as_str()) {
            continue;
        }
        if let Some(parent) = origin["caused_by_parent_request_doc_id"].as_str() {
            if let Some(scope) = load_request_scope(reader, parent).await? {
                found.push((scope.session_id, scope.agent_did, "started_you"));
            }
        }
    }
    for receipt in own_session_message_receipts(node, caller).await? {
        if receipt.session_id != caller.session_id
            && !found
                .iter()
                .any(|(session, _, _)| *session == receipt.session_id)
        {
            let agent = load_session_agent(node, &receipt.session_id).await?;
            if let Some(agent) = agent {
                found.push((receipt.session_id, agent, "messaged"));
            }
        }
    }
    let mut sessions = Vec::new();
    for (session_id, agent_did, relationship) in found {
        if sessions
            .iter()
            .any(|entry: &ReachableSession| entry.session_id == session_id)
        {
            continue;
        }
        let can_message = resolve_send_target(node, &caller.agent_did, tools, &session_id)
            .await?
            .is_some();
        let target = SessionMessageTarget {
            agent_did: agent_did.clone(),
            behavior_id: String::new(),
            session_id: session_id.clone(),
        };
        let can_interrupt = interrupt_refusal(node, &caller.session_id, &target)
            .await?
            .is_none();
        let status = match active_request(node, &target).await? {
            Some(_) => "busy".to_owned(),
            None => "idle".to_owned(),
        };
        sessions.push(ReachableSession {
            session_id,
            agent_did,
            relationship,
            status,
            can_message,
            can_interrupt,
        });
    }
    let agents = tools
        .targets
        .iter()
        .map(|target| {
            serde_json::json!({
                "agent": target.name,
                "agent_did": target.target_agent_did,
                "behavior_id": target.behavior_id,
                "description": target.description,
            })
        })
        .collect::<Vec<_>>();
    Ok(serde_json::json!({ "ok": true, "agents": agents, "sessions": sessions }))
}

/// The receipts of the calling session's `agent_new`/`agent_message` calls.
async fn own_session_message_receipts(
    node: &std::sync::Arc<EmbeddedNode>,
    caller: &crate::AgentRequest,
) -> Result<Vec<SessionMessageReceipt>> {
    #[derive(Deserialize)]
    struct Row {
        #[serde(rename = "_docID")]
        doc_id: String,
    }
    let query = format!(
        r#"{{ AgentToolCall(filter: {{ session_id: {{ _eq: "{}" }}, agent_did: {{ _eq: "{}" }}, tool_name: {{ _in: ["{}", "{}"] }} }}) {{ _docID }} }}"#,
        escape_graphql_string(&caller.session_id),
        escape_graphql_string(&caller.agent_did),
        crate::toolset::AGENT_NEW_TOOL_NAME,
        crate::toolset::AGENT_MESSAGE_TOOL_NAME,
    );
    let response = crate::graphql::graphql_with_transaction_retry(
        node.as_ref(),
        &query,
        "load the calling session's agent calls",
    )
    .await?;
    let mut receipts = Vec::new();
    for row in crate::graphql::rows::<Row>(&response, "AgentToolCall")? {
        let receipt = load_receipt(
            node,
            &row.doc_id,
            &caller.agent_did,
            &caller.session_id,
            caller.requester_did.as_deref(),
        )
        .await;
        if let Ok(Some(receipt)) = receipt {
            receipts.push(receipt);
        }
    }
    Ok(receipts)
}

/// The principal that runs `session_id`, from its visible requests.
async fn load_session_agent(node: &EmbeddedNode, session_id: &str) -> Result<Option<String>> {
    let query = format!(
        r#"{{ AgentRequest(filter: {{ session_id: {{ _eq: "{}" }}, purpose: {{ _eq: "normal" }} }}, limit: 1) {{ agent_did }} }}"#,
        escape_graphql_string(session_id),
    );
    let response =
        crate::graphql::graphql_with_transaction_retry(node, &query, "load a session's agent")
            .await?;
    Ok(
        crate::graphql::first_row::<gents_protocol::row::AgentRequestRow>(
            &response,
            "AgentRequest",
        )?
        .and_then(|row| row.agent_did),
    )
}

/// What a kill did (Lean `Recovery.KillAction`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum KillOutcome {
    /// The caused request had already ended; the row settled from it.
    Settled,
    /// The local caused request was interrupted; its terminal settles the row.
    Interrupting { request_id: String },
    /// The row was cancelled now, with a cancelled completion notification.
    Cancelled,
}

/// `cancel_process` and the operator kill on a running session-message row
/// (Lean `Recovery.killAction`). A kill never waits on a peer or on a request
/// it cannot name: those rows are cancelled directly.
pub(crate) async fn kill(
    node: &std::sync::Arc<EmbeddedNode>,
    lifecycle: &mut crate::tool_call_lifecycle::ToolCallLifecycle,
) -> Result<KillOutcome> {
    let caused = match observe_caused_request(node, lifecycle).await? {
        CausedObservation::Bound(caused) => Some(caused),
        CausedObservation::NotVisible | CausedObservation::Unbound(_) => None,
    };
    if let Some(caused) = &caused {
        let terminal = caused
            .lifecycle_state
            .is_some_and(gents_protocol::request_lifecycle::RequestLifecycleState::is_terminal);
        if terminal
            && crate::background_completion::settle_session_message_row(node, lifecycle).await?
        {
            return Ok(KillOutcome::Settled);
        }
        let doc_id = caused
            .doc_id
            .as_deref()
            .context("caused request lacks physical identity")?;
        let agent_did = caused
            .agent_did
            .as_deref()
            .context("caused request lacks agent_did")?;
        let local = agent_did == lifecycle.agent_did();
        if !terminal {
            let interrupted = crate::interrupt::interrupt_request_by_doc_id(
                node,
                doc_id,
                agent_did,
                caused.requester_did.as_deref(),
            )
            .await;
            match interrupted {
                Ok(_) if local => {
                    return Ok(KillOutcome::Interrupting {
                        request_id: caused.request_id.clone(),
                    })
                }
                Ok(_) => {}
                Err(error) if local => return Err(error),
                Err(error) => tracing::warn!(
                    caused_request_doc_id = doc_id,
                    error = %format!("{error:#}"),
                    "peer interrupt of a killed session message was not written"
                ),
            }
        }
    }
    if lifecycle
        .cancel_during_run_owned(
            crate::tool_call_lifecycle::CancelCause::UserCancelled,
            "explicit_cancel",
        )
        .await?
    {
        let calling_request_id = calling_request_id(node, lifecycle).await?;
        crate::background_completion::append_background_tool_completion(
            node.as_ref(),
            lifecycle.session_id(),
            &calling_request_id,
            lifecycle
                .doc_id()
                .context("session-message row lacks physical identity")?,
            lifecycle.tool_name(),
            "cancelled",
            "",
            Some("explicit_cancel"),
        )
        .await?;
    }
    Ok(KillOutcome::Cancelled)
}

/// The logical id of the request that made a session-message call, read from
/// its physical binding when the row was not loaded with it.
pub(crate) async fn calling_request_id(
    node: &EmbeddedNode,
    lifecycle: &crate::tool_call_lifecycle::ToolCallLifecycle,
) -> Result<String> {
    if !lifecycle.request_id().trim().is_empty() {
        return Ok(lifecycle.request_id().to_owned());
    }
    let doc_id = lifecycle
        .request_doc_id()
        .context("session-message row lacks its calling request document")?;
    Ok(
        crate::request_binding::load_agent_request_by_doc_id(node, doc_id)
            .await?
            .context("session-message calling request disappeared")?
            .request_id,
    )
}

#[cfg(test)]
mod hop_tests {
    use super::*;
    use crate::tool_call_lifecycle::admission_fixture::{
        published_session_message, PublishedAdmissionOptions,
    };
    use crate::tool_call_lifecycle::AwaitMode;

    fn cause(
        message: &crate::tool_call_lifecycle::admission_fixture::PublishedSessionMessage,
        caller_hop: u32,
    ) -> SessionMessageCause {
        SessionMessageCause {
            caller_agent_did: message.admission.agent_did.clone(),
            caller_request_id: "caller-request".to_owned(),
            caller_request_doc_id: message.admission.tool.request_doc_id().unwrap().to_owned(),
            caller_hop,
            tool_call_id: "hop-tool".to_owned(),
            tool_call_doc_id: "hop-tool-doc".to_owned(),
            correlation: None,
        }
    }

    /// Lean `DurableLineage.ContinuationKind.agentSteering` and
    /// `CausalHop.nextHop`: a message into a busy session steers it past its
    /// caller, and one into an idle session never lowers that session's hop.
    #[tokio::test]
    async fn session_messages_climb_past_their_caller_and_keep_the_target_hop() {
        let message = published_session_message(PublishedAdmissionOptions {
            name: "session-message-hops".to_owned(),
            real_identity: true,
            await_mode: AwaitMode::Background,
            ..Default::default()
        })
        .await
        .unwrap();
        let node = message.admission.node.clone();
        let did = message.admission.agent_did.clone();
        let caused = crate::request_binding::load_agent_request_by_doc_id(
            &node,
            &message.caused_request_doc_id,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(caused.subagent_depth, 1);
        let target = SessionMessageTarget {
            agent_did: did.clone(),
            behavior_id: caused.behavior_id.clone(),
            session_id: caused.session_id.clone(),
        };
        let body = || RenderedBody {
            content: "again".to_owned(),
            goal: None,
        };

        let idle = plan(&node, &cause(&message, 0), &target, body(), None, false)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(idle.delivery(), Delivery::Request);
        assert_eq!(idle.hop(), 1, "never below the addressed session's own hop");
        let idle = plan(&node, &cause(&message, 5), &target, body(), None, false)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(idle.hop(), 6);

        let response = node
            .execute(&format!(
                r#"mutation {{ update_AgentRequest(filter: {{ _docID: {{ _eq: "{}" }} }}, input: {{ lifecycle_state: "processing" }}) {{ _docID }} }}"#,
                escape_graphql_string(&message.caused_request_doc_id)
            ))
            .await;
        assert!(!response.has_errors(), "{:?}", response.errors);
        let busy = plan(&node, &cause(&message, 3), &target, body(), None, false)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(busy.delivery(), Delivery::Steering);
        assert_eq!(busy.hop(), 4, "steering climbs past its caller");
        let busy = plan(&node, &cause(&message, 0), &target, body(), None, false)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(busy.hop(), 1, "steering never lowers the active hop");
        node.shutdown().await;
        std::fs::remove_dir_all(&message.admission.path).unwrap();
    }
}

#[cfg(test)]
mod agent_tools_tests {
    use super::*;
    use crate::tool_call_lifecycle::admission_fixture::{
        published_session_message, PublishedAdmissionOptions, PublishedSessionMessage,
    };
    use crate::tool_call_lifecycle::AwaitMode;

    async fn started(
        name: &str,
    ) -> (
        PublishedSessionMessage,
        crate::AgentRequest,
        crate::AgentRequest,
    ) {
        let message = published_session_message(PublishedAdmissionOptions {
            name: name.to_owned(),
            real_identity: true,
            await_mode: AwaitMode::Background,
            ..Default::default()
        })
        .await
        .unwrap();
        let node = message.admission.node.clone();
        let caller = crate::request_binding::load_agent_request_by_doc_id(
            &node,
            message.admission.tool.request_doc_id().unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
        let caused = crate::request_binding::load_agent_request_by_doc_id(
            &node,
            &message.caused_request_doc_id,
        )
        .await
        .unwrap()
        .unwrap();
        (message, caller, caused)
    }

    fn target_of(caused: &crate::AgentRequest) -> SessionMessageTarget {
        SessionMessageTarget {
            agent_did: caused.agent_did.clone(),
            behavior_id: caused.behavior_id.clone(),
            session_id: caused.session_id.clone(),
        }
    }

    async fn set_state(node: &EmbeddedNode, doc_id: &str, state: &str) {
        let response = node
            .execute(&format!(
                r#"mutation {{ update_AgentRequest(filter: {{ _docID: {{ _eq: "{}" }} }}, input: {{ lifecycle_state: "{state}" }}) {{ _docID }} }}"#,
                escape_graphql_string(doc_id)
            ))
            .await;
        assert!(!response.has_errors(), "{:?}", response.errors);
    }

    /// Lean `DurableLineage.interruptAllowed` through `gents::session_origin`:
    /// only the session that started a session may interrupt it.
    #[tokio::test]
    async fn only_the_starting_session_may_interrupt() {
        let (message, caller, caused) = started("agent-interrupt-permission").await;
        let node = message.admission.node.clone();
        let target = target_of(&caused);
        assert_eq!(
            origin_cause_session(&node, &target)
                .await
                .unwrap()
                .as_deref(),
            Some(caller.session_id.as_str())
        );
        assert!(interrupt_refusal(&node, &caller.session_id, &target)
            .await
            .unwrap()
            .is_none());
        assert!(interrupt_refusal(&node, "another-session", &target)
            .await
            .unwrap()
            .is_some());
        // The started session may not interrupt the session that started it.
        let parent = SessionMessageTarget {
            agent_did: caller.agent_did.clone(),
            behavior_id: caller.behavior_id.clone(),
            session_id: caller.session_id.clone(),
        };
        assert!(interrupt_refusal(&node, &caused.session_id, &parent)
            .await
            .unwrap()
            .is_some());
        node.shutdown().await;
        std::fs::remove_dir_all(&message.admission.path).unwrap();
    }

    /// `agent_interrupt` stops the busy session's turn; `agent_message` with
    /// `interrupt` plans a new request rather than steering.
    #[tokio::test]
    async fn interrupt_stops_the_turn_and_the_steer_is_a_new_request() {
        let (message, caller, caused) = started("agent-interrupt-steer").await;
        let node = message.admission.node.clone();
        let target = target_of(&caused);
        assert_eq!(interrupt_session(&node, &target).await.unwrap(), None);
        set_state(&node, &message.caused_request_doc_id, "processing").await;

        let cause = SessionMessageCause {
            caller_agent_did: caller.agent_did.clone(),
            caller_request_id: caller.request_id.clone(),
            caller_request_doc_id: caller.doc_id.clone(),
            caller_hop: caller.subagent_depth,
            tool_call_id: "steer-tool".to_owned(),
            tool_call_doc_id: "steer-tool-doc".to_owned(),
            correlation: None,
        };
        let body = || RenderedBody {
            content: "change course".to_owned(),
            goal: None,
        };
        let steering = plan(&node, &cause, &target, body(), None, false)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(steering.delivery(), Delivery::Steering);
        let steer = plan(&node, &cause, &target, body(), None, true)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(steer.delivery(), Delivery::Request);
        assert_eq!(steer.hop(), 1);

        assert_eq!(
            interrupt_session(&node, &target).await.unwrap(),
            Some(caused.request_id.clone())
        );
        assert!(crate::interrupt::fetch_interrupt_requested_at_by_doc_id(
            &node,
            &message.caused_request_doc_id
        )
        .await
        .unwrap()
        .is_some());
        node.shutdown().await;
        std::fs::remove_dir_all(&message.admission.path).unwrap();
    }

    /// `agent_list` reports the allowlist and each reachable session with its
    /// relationship to the calling session.
    #[tokio::test]
    async fn agent_list_reports_agents_and_reachable_sessions() {
        let (message, caller, caused) = started("agent-list").await;
        let node = message.admission.node.clone();
        let tools = CallerSessionTools {
            enabled: true,
            targets: vec![SubagentTargetDocument {
                target_id: "worker".to_owned(),
                agent_did: caller.agent_did.clone(),
                target_agent_did: caused.agent_did.clone(),
                behavior_id: caused.behavior_id.clone(),
                name: "worker".to_owned(),
                description: Some("does the work".to_owned()),
                tags: Vec::new(),
            }],
        };
        let listed = agent_list(&node, &caller, &tools).await.unwrap();
        assert_eq!(listed["agents"][0]["agent"], "worker");
        assert_eq!(listed["agents"][0]["behavior_id"], caused.behavior_id);
        let sessions = listed["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 1, "{listed}");
        assert_eq!(sessions[0]["session_id"], caused.session_id);
        assert_eq!(sessions[0]["relationship"], "started_by_you");
        assert_eq!(sessions[0]["can_message"], true);
        assert_eq!(sessions[0]["can_interrupt"], true);
        assert_eq!(sessions[0]["status"], "idle");

        let from_child = agent_list(&node, &caused, &tools).await.unwrap();
        let sessions = from_child["sessions"].as_array().unwrap();
        assert!(
            sessions
                .iter()
                .any(|session| session["session_id"] == caller.session_id
                    && session["relationship"] == "started_you"
                    && session["can_interrupt"] == false),
            "{from_child}"
        );
        node.shutdown().await;
        std::fs::remove_dir_all(&message.admission.path).unwrap();
    }
}
