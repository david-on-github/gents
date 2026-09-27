use super::*;

/// Append an authenticated same-session steering request beneath an exact
/// committed parent. External adapters provide the user input and physical
/// parent binding; the runtime remains the sole owner of request admission and
/// signing. Transcript publication belongs to the owned execution boundary.
pub async fn enqueue_local_steering_request(
    node: &EmbeddedNode,
    parent_request_id: &str,
    parent_request_doc_id: &str,
    content: &str,
    input: RequestInput,
) -> Result<EnqueuedAgentRequest> {
    let parent = crate::request_binding::load_agent_request_by_doc_id(node, parent_request_doc_id)
        .await?
        .with_context(|| format!("steering parent request {parent_request_doc_id} not found"))?;
    anyhow::ensure!(
        parent.request_id == parent_request_id,
        "steering parent request changed logical binding"
    );
    enqueue_steering_request(node, &parent, content, input).await
}

/// Re-admits a steering append inside the transaction that writes it, so the
/// admission and the append observe one serialized state.
#[async_trait::async_trait]
pub(crate) trait SteeringAdmission: Send + Sync {
    async fn admit(&self, txn: &ConfigApplyTxn<'_>) -> Result<()>;
}

/// Atomically persist the signed steering request. Its admission content is
/// displayed while queued and is published to the transcript only when owned
/// execution starts.
pub(crate) async fn enqueue_steering_request(
    node: &EmbeddedNode,
    parent: &AgentRequest,
    content: &str,
    input: RequestInput,
) -> Result<EnqueuedAgentRequest> {
    enqueue_admitted_steering_request(node, parent, content, input, None).await
}

pub(crate) async fn enqueue_admitted_steering_request(
    node: &EmbeddedNode,
    parent: &AgentRequest,
    content: &str,
    input: RequestInput,
    admission: Option<&dyn SteeringAdmission>,
) -> Result<EnqueuedAgentRequest> {
    let queue = input
        .queue
        .as_ref()
        .context("atomic steering enqueue requires queue input")?;
    anyhow::ensure!(
        matches!(queue.source, QueueSource::Steering | QueueSource::User)
            && queue.policy == QueuePolicy::Append
            && queue.key.is_none(),
        "atomic steering enqueue requires an unkeyed user or steering append"
    );
    anyhow::ensure!(
        queue.background_completion_wake_version.is_none(),
        "steering enqueue must not carry the background wake marker"
    );

    let behavior_id = parent_behavior_id(parent)?;
    let request_id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let request_mutation = session_request_create_mutation(
        parent,
        &behavior_id,
        content,
        ExecutionOrigin::Interactive,
        input,
        &request_id,
        &now,
        None,
    )
    .await?;
    let request_id = &request_id;
    let request_mutation = &request_mutation;

    let enqueued = crate::config_client::ConfigAccess::transact_local(
        node,
        None,
        "lifecycle.enqueue_steering",
        move |txn| {
            Box::pin(async move {
                if let Some(admission) = admission {
                    admission.admit(txn).await?;
                }
                steering_transaction_attempt(txn, parent, request_id, request_mutation).await
            })
        },
    )
    .await?;

    Ok(enqueued)
}

/// Append a `send_message` request to a busy session. The caller signed it
/// through the session-message writer as a user-origin append queued after
/// the session's active request; the append shares the steering transaction.
pub(crate) async fn enqueue_session_message_steering(
    node: &EmbeddedNode,
    active: &AgentRequest,
    create: &gents_protocol::request_admission::AgentRequestCreate,
) -> Result<EnqueuedAgentRequest> {
    let queue = create
        .input
        .queue
        .as_ref()
        .context("session-message steering requires queue input")?;
    anyhow::ensure!(
        queue.source == QueueSource::User
            && queue.policy == QueuePolicy::Append
            && queue.key.is_none()
            && queue.queued_after_request_id.as_deref() == Some(active.request_id.as_str())
            && create.session_id == active.session_id
            && create.agent_did == active.agent_did,
        "session-message steering must append after the session's active request"
    );
    let request_mutation = create.graphql_mutation().map_err(anyhow::Error::msg)?;
    let request_mutation = &request_mutation;
    let request_id = create.request_id.as_str();
    crate::config_client::ConfigAccess::transact_local(
        node,
        None,
        "lifecycle.enqueue_session_message_steering",
        move |txn| {
            Box::pin(async move {
                steering_transaction_attempt(txn, active, request_id, request_mutation).await
            })
        },
    )
    .await
}
