use super::ConfigApplyTxn;
use crate::graphql::escape_graphql_string;
use anyhow::{Context, Result};
use gents_protocol::trigger_delivery::EventSourceCursor;
use serde_json::json;

/// Receiving-node offsets must not replicate with shared EventSource configuration.
pub(crate) struct CursorRecord {
    pub doc_id: String,
    pub cursor: EventSourceCursor,
}

async fn event_binding(
    txn: &ConfigApplyTxn<'_>,
    owner: &str,
    trigger_id: &str,
) -> Result<(
    crate::document_config::Trigger,
    crate::document_config::EventSource,
)> {
    let value = super::read_desired_state_document_in_txn(
        txn,
        crate::Collection::Trigger,
        owner,
        trigger_id,
    )
    .await?
    .context("cursor trigger disappeared")?;
    let trigger: crate::document_config::Trigger = serde_json::from_value(value)?;
    let crate::document_config::TriggerSource::Event { event_source_id } = &trigger.source else {
        anyhow::bail!("arrival cursor requires an event trigger")
    };
    let source = super::read_desired_state_document_in_txn(
        txn,
        crate::Collection::EventSource,
        owner,
        event_source_id,
    )
    .await?
    .context("arrival source is missing")?;
    Ok((trigger, serde_json::from_value(source)?))
}

pub(crate) async fn load_or_seed(
    txn: &ConfigApplyTxn<'_>,
    owner: &str,
    trigger_id: &str,
) -> Result<CursorRecord> {
    let (_, source) = event_binding(txn, owner, trigger_id).await?;
    load_or_seed_for_source(txn, owner, trigger_id, &source.source_collection).await
}

/// Cursor creation belongs to the configuration transaction, including a
/// source-only replacement while its consumers are disabled. Waiting until
/// runtime reconciliation would skip documents arriving after that replacement.
pub(crate) async fn seed_referencing_triggers(
    txn: &ConfigApplyTxn<'_>,
    owner: &str,
    event_source_id: &str,
) -> Result<()> {
    let response = txn
        .execute(&format!(
            "{{ Trigger(filter: {{agent_did: {{_eq: \"{}\"}}}}) {{trigger_id source}} }}",
            escape_graphql_string(owner),
        ))
        .await?;
    for row in response["data"]["Trigger"]
        .as_array()
        .context("source consumers omitted rows")?
    {
        if row["source"]["kind"] == "event" && row["source"]["event_source_id"] == event_source_id {
            let trigger_id = row["trigger_id"]
                .as_str()
                .context("source consumer lacks trigger ID")?;
            load_or_seed(txn, owner, trigger_id).await?;
        }
    }
    Ok(())
}

/// Cached runtime snapshots cannot authorize new admissions after a committed
/// disable or source replacement. Duplicate receipts bypass this check because
/// their original request already committed before the control-plane change.
pub(crate) async fn validate_event_admission(
    txn: &ConfigApplyTxn<'_>,
    fire: &gents_protocol::trigger_delivery::TriggerFire,
) -> Result<()> {
    let (trigger, source) =
        event_binding(txn, &fire.identity.owner_did, &fire.identity.trigger_id).await?;
    anyhow::ensure!(trigger.enabled, "event trigger is disabled");
    anyhow::ensure!(
        trigger.task_id == fire.task_id,
        "event trigger Task binding changed before admission"
    );
    anyhow::ensure!(
        if fire.identity.source_collection == "EventGroupState" {
            source.group.is_some()
        } else {
            source.group.is_none() && source.source_collection == fire.identity.source_collection
        },
        "event source binding changed before admission"
    );
    Ok(())
}

pub(crate) async fn exclude_arrival(
    txn: &ConfigApplyTxn<'_>,
    owner: &str,
    trigger_id: &str,
    expected_collection: &str,
    after: &str,
) -> Result<()> {
    let (trigger, source) = event_binding(txn, owner, trigger_id).await?;
    anyhow::ensure!(
        trigger.enabled,
        "disabled event trigger must retain pending arrivals"
    );
    anyhow::ensure!(
        source.source_collection == expected_collection && source.group.is_none(),
        "event source binding changed before exclusion"
    );
    advance(txn, owner, trigger_id, expected_collection, after).await
}

pub(crate) async fn load_or_seed_for_source(
    txn: &ConfigApplyTxn<'_>,
    owner: &str,
    trigger_id: &str,
    collection: &str,
) -> Result<CursorRecord> {
    let key =
        crate::trigger_engine::durable_fire_key("arrival-cursor", &[owner, trigger_id, collection]);
    let response = txn.execute(&format!("{{ EventSourceCursor(filter: {{cursor_key: {{_eq: \"{}\"}}}}, limit: 2) {{ _docID cursor_key owner_did trigger_id source_collection after }} }}", escape_graphql_string(&key))).await?;
    let rows = response["data"]["EventSourceCursor"]
        .as_array()
        .context("cursor query omitted rows")?;
    anyhow::ensure!(rows.len() <= 1, "arrival cursor must resolve uniquely");
    if let Some(row) = rows.first() {
        let doc_id = row["_docID"]
            .as_str()
            .context("arrival cursor lacks document ID")?
            .to_owned();
        let mut value = row.clone();
        value
            .as_object_mut()
            .context("cursor must be an object")?
            .remove("_docID");
        let cursor: EventSourceCursor =
            serde_json::from_value(value).context("invalid persisted arrival cursor")?;
        anyhow::ensure!(
            cursor.owner_did == owner
                && cursor.trigger_id == trigger_id
                && cursor.source_collection == collection,
            "arrival cursor scope disagrees with its key"
        );
        return Ok(CursorRecord { doc_id, cursor });
    }
    let response = txn
        .execute(&format!(
            "{{ _documentArrivals(collection: \"{}\", after: \"0\", limit: 1) {{ head }} }}",
            escape_graphql_string(collection)
        ))
        .await?;
    let after = response["data"]["_documentArrivals"]["head"]
        .as_str()
        .context("arrival journal omitted head")?
        .to_owned();
    let cursor = EventSourceCursor {
        cursor_key: key,
        owner_did: owner.into(),
        trigger_id: trigger_id.into(),
        source_collection: collection.into(),
        after,
    };
    let response = txn.execute_with_variables("mutation($input: EventSourceCursorMutationInputArg!) {create_EventSourceCursor(input: $input) {_docID}}", &json!({"input": cursor})).await?;
    let doc_id = crate::graphql::created_doc_id(&response, "EventSourceCursor")?;
    Ok(CursorRecord { doc_id, cursor })
}

pub(crate) async fn advance(
    txn: &ConfigApplyTxn<'_>,
    owner: &str,
    trigger_id: &str,
    expected_collection: &str,
    after: &str,
) -> Result<()> {
    let record = load_or_seed_for_source(txn, owner, trigger_id, expected_collection).await?;
    anyhow::ensure!(
        record.cursor.source_collection == expected_collection,
        "arrival source changed while fire was admitted"
    );
    let old: u64 = record
        .cursor
        .after
        .parse()
        .context("invalid saved arrival position")?;
    let next: u64 = after.parse().context("invalid new arrival position")?;
    if next > old {
        txn.execute(&format!("mutation {{update_EventSourceCursor(docID: \"{}\", input: {{after: \"{}\"}}) {{_docID}}}}",
            escape_graphql_string(&record.doc_id), escape_graphql_string(after))).await?;
    }
    Ok(())
}

pub(crate) async fn acknowledge_fire(
    txn: &ConfigApplyTxn<'_>,
    owner: &str,
    trigger_id: &str,
    source_collection: &str,
    source_doc_id: &str,
    after: &str,
) -> Result<()> {
    let identity = gents_protocol::trigger_delivery::FireIdentity {
        owner_did: owner.into(),
        trigger_id: trigger_id.into(),
        source_collection: source_collection.into(),
        source_doc_id: source_doc_id.into(),
    };
    let fire_key = crate::trigger_engine::durable::fire_key(&identity);
    let request_id = format!("trigger-request:{fire_key}");
    let receipt = txn
        .execute(&format!(
            "{{ TriggerFire(filter: {{fire_key: {{_eq: \"{}\"}}}}, limit: 2) {{ request_id }} }}",
            escape_graphql_string(&fire_key),
        ))
        .await?;
    let receipts = receipt["data"]["TriggerFire"]
        .as_array()
        .context("fire acknowledgment omitted receipt rows")?;
    anyhow::ensure!(
        receipts.len() == 1 && receipts[0]["request_id"].as_str() == Some(request_id.as_str()),
        "arrival acknowledgment requires its admitted fire receipt"
    );
    let request = txn.execute(&format!(
        "{{ AgentRequest(filter: {{agent_did: {{_eq: \"{}\"}}, request_id: {{_eq: \"{}\"}}}}, limit: 2) {{ _docID }} }}",
        escape_graphql_string(owner), escape_graphql_string(&request_id),
    )).await?;
    anyhow::ensure!(
        request["data"]["AgentRequest"]
            .as_array()
            .is_some_and(|rows| rows.len() == 1),
        "arrival acknowledgment requires its admitted request"
    );
    advance(txn, owner, trigger_id, source_collection, after).await
}

#[cfg(test)]
mod tests;
