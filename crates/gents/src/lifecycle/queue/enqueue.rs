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
    let prepared = prepare_steering_append(parent, content, input).await?;
    let prepared = &prepared;
    crate::config_client::ConfigAccess::transact_local(
        node,
        None,
        "lifecycle.enqueue_steering",
        move |txn| {
            Box::pin(async move {
                if let Some(admission) = admission {
                    admission.admit(txn).await?;
                }
                append_prepared_steering_in_txn(txn, parent, prepared).await
            })
        },
    )
    .await
}

/// A signed steering append beneath one exact committed parent, ready to be
/// written inside a caller's transaction.
pub(crate) struct PreparedSteering {
    request_id: String,
    mutation: String,
}

/// Build and sign one unkeyed user- or steering-sourced append. A user
/// append is external input; a steering append is agent-authored.
pub(crate) async fn prepare_steering_append(
    parent: &AgentRequest,
    content: &str,
    input: RequestInput,
) -> Result<PreparedSteering> {
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
    let mutation = session_request_create_mutation(
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
    Ok(PreparedSteering {
        request_id,
        mutation,
    })
}

pub(crate) async fn append_prepared_steering_in_txn(
    txn: &ConfigApplyTxn<'_>,
    parent: &AgentRequest,
    prepared: &PreparedSteering,
) -> Result<EnqueuedAgentRequest> {
    steering_transaction_attempt(txn, parent, &prepared.request_id, &prepared.mutation).await
}
