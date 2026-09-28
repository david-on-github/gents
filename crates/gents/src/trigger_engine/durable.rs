mod recovery;
use crate::config_client::ConfigApplyTxn;
use crate::graphql::escape_graphql_string;
use anyhow::{ensure, Result};
use gents_protocol::trigger_delivery::{FireIdentity, TriggerFire};
pub(crate) use recovery::recover_outcomes;

#[derive(Clone)]
pub(crate) struct PreparedFire {
    pub receipt: TriggerFire,
    pub target_existing: bool,
}

pub(crate) fn fire_key(identity: &FireIdentity) -> String {
    [
        &identity.owner_did,
        &identity.trigger_id,
        &identity.source_collection,
        &identity.source_doc_id,
    ]
    .into_iter()
    .map(|value| format!("{}:{value}", value.chars().count()))
    .collect()
}

pub(crate) fn resolve_session_id(
    identity: &FireIdentity,
    target: Option<&str>,
    owned: bool,
) -> Option<String> {
    match target {
        None => Some(format!("trigger-session:{}", fire_key(identity))),
        Some(value) if !value.is_empty() && owned => Some(value.to_owned()),
        Some(_) => None,
    }
}

pub(crate) fn outcome_due(
    emit: bool,
    goal_backed: bool,
    goal_status: &str,
    _wrapup_completed: bool,
    terminal: bool,
) -> bool {
    emit && if goal_backed {
        matches!(goal_status, "complete" | "blocked" | "budget_limited")
    } else {
        terminal
    }
}

pub(crate) struct FireQueueRow {
    pub identity: FireIdentity,
    pub session_id: String,
    pub queued_serial: bool,
    pub running: bool,
    pub terminal: bool,
}

pub(crate) fn queued_claim_allowed(rows: &[FireQueueRow], identity: &FireIdentity) -> bool {
    let Some(index) = rows.iter().position(|r| &r.identity == identity) else {
        return false;
    };
    let candidate = &rows[index];
    let conflicts = |other: &FireQueueRow| {
        candidate.identity.owner_did == other.identity.owner_did
            && (candidate.session_id == other.session_id
                || (candidate.queued_serial
                    && candidate.identity.trigger_id == other.identity.trigger_id))
    };
    !candidate.running
        && !candidate.terminal
        && !rows
            .iter()
            .any(|r| r.running && !r.terminal && conflicts(r))
        && !rows[..index].iter().any(|r| !r.terminal && conflicts(r))
}

/// The receipt's unique index arbitrates concurrent admissions. Both writes
/// use the caller's transaction, so rollback cannot leave an admitted fire
/// without its request. A duplicate never executes its supplied mutation.
#[cfg(test)]
pub(crate) async fn stage_fire_request(
    txn: &ConfigApplyTxn<'_>,
    fire: &TriggerFire,
    request_mutation: &str,
) -> Result<bool> {
    if !stage_fire_receipt(txn, fire).await? {
        return Ok(false);
    }
    txn.execute(request_mutation).await?;
    Ok(true)
}

pub(crate) fn outcome_source_allowed(source_collection: &str, emit_outcome: bool) -> bool {
    source_collection != "FireOutcome" || !emit_outcome
}

pub(crate) async fn stage_fire_receipt(
    txn: &ConfigApplyTxn<'_>,
    fire: &TriggerFire,
) -> Result<bool> {
    ensure!(
        outcome_source_allowed(&fire.identity.source_collection, fire.emit_outcome),
        "a Task sourced from FireOutcome cannot emit another FireOutcome"
    );
    ensure!(
        fire.fire_key == fire_key(&fire.identity),
        "noncanonical fire identity"
    );
    ensure!(
        fire.request_id == format!("trigger-request:{}", fire.fire_key),
        "noncanonical fire request ID"
    );
    let prior = txn
        .execute(&format!(
            "{{ TriggerFire(filter: {{fire_key: {{_eq: \"{}\"}}}}) {{ request_id }} }}",
            escape_graphql_string(&fire.fire_key)
        ))
        .await?;
    if prior["data"]["TriggerFire"]
        .as_array()
        .is_some_and(|rows| !rows.is_empty())
    {
        return Ok(false);
    }
    txn.execute_with_variables("mutation($input: TriggerFireMutationInputArg!) { create_TriggerFire(input: $input) { _docID } }", &serde_json::json!({"input":fire})).await?;
    Ok(true)
}

/// Called in the terminal owner's transaction. Recovery uses the same unique
/// outcome key; an acknowledgement lost after commit cannot extend the chain.
pub(crate) async fn stage_outcome(
    txn: &ConfigApplyTxn<'_>,
    fire: &TriggerFire,
    goal_status: &str,
    wrapup_completed: bool,
    request_terminal: bool,
    terminal_state: &str,
    reason: &str,
    now: &str,
) -> Result<bool> {
    if fire.goal_id.is_some() && !fire.goal_assignment_applied {
        return Ok(false);
    }
    if !outcome_due(
        fire.emit_outcome,
        fire.goal_id.is_some(),
        goal_status,
        wrapup_completed,
        request_terminal,
    ) {
        return Ok(false);
    }
    ensure!(
        fire.fire_key == fire_key(&fire.identity),
        "noncanonical outcome fire identity"
    );
    let handoff_id = format!("outcome:{}", fire.fire_key);
    let prior = txn
        .execute(&format!(
            "{{ FireOutcome(filter: {{handoff_id: {{_eq: \"{}\"}}}}) {{ handoff_id }} }}",
            escape_graphql_string(&handoff_id)
        ))
        .await?;
    if prior["data"]["FireOutcome"]
        .as_array()
        .is_some_and(|rows| !rows.is_empty())
    {
        return Ok(false);
    }
    let source_handoff_id = fire
        .source_handoff_id
        .as_ref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("outcome-enabled fire lacks its source handoff identity"))?;
    let outcome = gents_protocol::trigger_delivery::FireOutcome {
        handoff_id,
        fire_key: fire.fire_key.clone(),
        identity: fire.identity.clone(),
        request_id: fire.request_id.clone(),
        session_id: fire.session_id.clone(),
        goal_id: fire.goal_id.clone(),
        terminal_state: terminal_state.into(),
        reason: reason.into(),
        source_handoff_id: source_handoff_id.clone(),
        reply_session_id: fire.reply_session_id.clone(),
        shard_id: fire.shard_id.clone(),
        attempt: fire.attempt,
        created_at: now.into(),
    };
    txn.execute_with_variables("mutation($input: FireOutcomeMutationInputArg!) { create_FireOutcome(input: $input) { _docID } }", &serde_json::json!({"input":outcome})).await?;
    Ok(true)
}

const FIRE_FIELDS: &str = "fire_key owner_did trigger_id source_collection source_doc_id task_id request_id session_id goal_id goal_objective goal_token_budget goal_assignment_applied emit_outcome queued_serial source_handoff_id reply_session_id shard_id attempt created_at";

pub(crate) async fn publish_request_outcome(
    txn: &ConfigApplyTxn<'_>,
    owner: &str,
    request_id: &str,
    terminal_state: &str,
    reason: &str,
    now: &str,
) -> Result<()> {
    let response = txn.execute(&format!("{{ TriggerFire(filter: {{owner_did: {{_eq: \"{}\"}}, request_id: {{_eq: \"{}\"}}}}) {{ {FIRE_FIELDS} }} }}", escape_graphql_string(owner), escape_graphql_string(request_id))).await?;
    let rows: Vec<TriggerFire> = serde_json::from_value(response["data"]["TriggerFire"].clone())?;
    for fire in rows {
        // Goal-backed requests notify only through the Goal terminal owner.
        if fire.goal_id.is_none() {
            stage_outcome(
                txn,
                &fire,
                "active",
                false,
                true,
                terminal_state,
                reason,
                now,
            )
            .await?;
        }
    }
    Ok(())
}

pub(crate) async fn publish_goal_outcomes(
    txn: &ConfigApplyTxn<'_>,
    owner: &str,
    goal_id: &str,
    status: &str,
    wrapup_completed: bool,
    reason: &str,
    now: &str,
) -> Result<()> {
    let response = txn.execute(&format!("{{ TriggerFire(filter: {{owner_did: {{_eq: \"{}\"}}, goal_id: {{_eq: \"{}\"}}}}) {{ {FIRE_FIELDS} }} }}", escape_graphql_string(owner), escape_graphql_string(goal_id))).await?;
    let rows: Vec<TriggerFire> = serde_json::from_value(response["data"]["TriggerFire"].clone())?;
    for fire in rows {
        stage_outcome(
            txn,
            &fire,
            status,
            wrapup_completed,
            false,
            status,
            reason,
            now,
        )
        .await?;
    }
    Ok(())
}

/// Read the native first-arrival journal in the caller's claim snapshot.
/// Missing arrivals are not replaced with content-ID or wall-clock ordering.
pub(crate) async fn request_arrival_order(
    txn: &ConfigApplyTxn<'_>,
    doc_ids: &[String],
) -> Result<Vec<String>> {
    if doc_ids.is_empty() {
        return Ok(Vec::new());
    }
    let ids = crate::graphql::graphql_string_list_literal(doc_ids.iter().map(String::as_str));
    let mut cursor = "0".to_owned();
    let mut ordered = Vec::new();
    loop {
        let response = txn.execute(&format!("{{ _documentArrivals(collection: \"AgentRequest\", after: \"{}\", limit: 256, docID: {ids}) {{ head next entries {{ cursor docID }} }} }}", escape_graphql_string(&cursor))).await?;
        let page = &response["data"]["_documentArrivals"];
        let entries = page["entries"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("arrival query omitted entries"))?;
        for entry in entries {
            ordered.push(
                entry["docID"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("arrival entry omitted docID"))?
                    .to_owned(),
            );
        }
        let next = page["next"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("arrival query omitted next cursor"))?;
        let head = page["head"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("arrival query omitted snapshot head"))?;
        if next == head || entries.is_empty() {
            break;
        }
        ensure!(next != cursor, "arrival cursor did not advance");
        cursor = next.into();
    }
    Ok(ordered)
}

#[derive(Clone, serde::Deserialize)]
pub(crate) struct ClaimObservation {
    pub document: String,
    pub owner: String,
    pub session: String,
    pub trigger: String,
    pub serial: bool,
    pub receipt: bool,
    pub arrival: Option<u64>,
    pub running: bool,
    pub terminal: bool,
}

pub(crate) fn claim_conflict(candidate: &ClaimObservation, other: &ClaimObservation) -> bool {
    candidate.owner == other.owner
        && (candidate.session == other.session
            || (candidate.serial
                && candidate.receipt
                && other.receipt
                && candidate.trigger == other.trigger))
}

pub(crate) fn observed_claim_allowed(
    candidate: &ClaimObservation,
    rows: &[ClaimObservation],
) -> bool {
    !candidate.running
        && !candidate.terminal
        && !(candidate.receipt && candidate.arrival.is_none())
        && !rows.iter().any(|other| {
            other.document != candidate.document
                && !other.terminal
                && claim_conflict(candidate, other)
                && ((other.receipt && other.arrival.is_none())
                    || other.running
                    || match (candidate.arrival, other.arrival) {
                        (Some(_), None) => true,
                        (Some(next), Some(prior)) => prior < next,
                        (None, _) => false,
                    })
        })
}
