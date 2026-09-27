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
    Request,
    Steering(crate::AgentRequest),
    Goal {
        objective: String,
        token_budget: Option<i64>,
    },
}

/// A signed session-message request and how it will be delivered, decided
/// before the tool row publishes its receipt.
pub(crate) struct Plan {
    create: gents_protocol::request_admission::AgentRequestCreate,
    write: PlannedWrite,
}

impl Plan {
    pub(crate) fn request_id(&self) -> &str {
        &self.create.request_id
    }

    pub(crate) fn session_id(&self) -> &str {
        &self.create.session_id
    }

    pub(crate) fn delivery(&self) -> Delivery {
        match self.write {
            PlannedWrite::Steering(_) => Delivery::Steering,
            PlannedWrite::Request | PlannedWrite::Goal { .. } => Delivery::Request,
        }
    }
}

/// Plan one session-message request. An idle or remote session gets a new
/// request; a busy local session gets a user-origin append queued after its
/// active request. A Task Goal is set on a local idle target session with its
/// request in one transaction.
pub(crate) async fn plan(
    node: &EmbeddedNode,
    cause: &SessionMessageCause,
    target: &SessionMessageTarget,
    rendered: RenderedBody,
    title: Option<&str>,
) -> Result<Result<Plan, String>> {
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
    if let Some((objective, token_budget)) = rendered.goal {
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
            None,
            Some(format!("session-message:{}", cause.tool_call_doc_id)),
        )
        .await?;
        return Ok(Ok(Plan {
            create,
            write: PlannedWrite::Goal {
                objective,
                token_budget,
            },
        }));
    }
    if let Some(active) = active {
        let active_doc_id = active
            .doc_id
            .as_deref()
            .context("active session request lacks physical identity")?;
        let active = crate::request_binding::load_agent_request_by_doc_id(node, active_doc_id)
            .await?
            .context("active session request disappeared")?;
        let create = crate::lifecycle::build_session_message_request(
            cause,
            target,
            &rendered.content,
            None,
            Some(gents_protocol::request_input::RequestQueue {
                source: gents_protocol::request_input::QueueSource::User,
                policy: gents_protocol::request_input::QueuePolicy::Append,
                key: None,
                queued_after_request_id: Some(active.request_id.clone()),
                interrupted_request_id: None,
                background_completion_wake_version: None,
            }),
            None,
        )
        .await?;
        return Ok(Ok(Plan {
            create,
            write: PlannedWrite::Steering(active),
        }));
    }
    let create = crate::lifecycle::build_session_message_request(
        cause,
        target,
        &rendered.content,
        title,
        None,
        None,
    )
    .await?;
    Ok(Ok(Plan {
        create,
        write: PlannedWrite::Request,
    }))
}

/// Persist a planned session-message request.
pub(crate) async fn commit(
    node: &EmbeddedNode,
    cause: &SessionMessageCause,
    plan: Plan,
) -> Result<crate::lifecycle::EnqueuedAgentRequest> {
    match plan.write {
        PlannedWrite::Request => {
            crate::lifecycle::write_session_message_request(node, &plan.create).await
        }
        PlannedWrite::Steering(active) => {
            crate::lifecycle::queue::enqueue_session_message_steering(node, &active, &plan.create)
                .await
        }
        PlannedWrite::Goal {
            objective,
            token_budget,
        } => {
            let actor = ::identity::Did::new(cause.caller_agent_did.clone())
                .context("caller DID is not ACP-addressable")?;
            crate::goal::submit_goal_backed_request_local(
                node,
                actor,
                &plan.create.agent_did,
                &plan.create.session_id,
                &objective,
                token_budget,
                &plan.create,
            )
            .await
        }
    }
}

/// `cancel_process` on a session-message row interrupts only the one request
/// that row caused; the row settles when that request reaches its terminal.
/// Returns the interrupted request id, or `None` when it has no live request.
pub(crate) async fn interrupt_caused_request(
    node: &EmbeddedNode,
    tool_call_doc_id: &str,
    caller_agent_did: &str,
) -> Result<Option<String>> {
    let query = format!(
        r#"{{ AgentRequest(filter: {{ caused_by_parent_tool_call_doc_id: {{ _eq: "{}" }}, requester_did: {{ _eq: "{}" }} }}, limit: 2) {{
            _docID request_id agent_did requester_did lifecycle_state
        }} }}"#,
        escape_graphql_string(tool_call_doc_id),
        escape_graphql_string(caller_agent_did),
    );
    let response = crate::graphql::graphql_with_transaction_retry(
        node,
        &query,
        "load the request a session-message row caused",
    )
    .await?;
    let rows =
        crate::graphql::rows::<gents_protocol::row::AgentRequestRow>(&response, "AgentRequest")?;
    let [caused] = rows.as_slice() else {
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
    Ok(Some(caused.request_id.clone()))
}
