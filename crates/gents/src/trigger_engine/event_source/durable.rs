use super::*;
use crate::config_client::ConfigAccess;
use crate::graphql::escape_graphql_string;

pub(super) struct PendingCheckpoint {
    owner: String,
    trigger_id: String,
    collection: String,
    position: String,
    result: tokio::sync::oneshot::Receiver<super::super::FireResult>,
}

impl EventSource {
    pub(super) async fn finish_durable_checkpoint(&mut self) {
        let Some(pending) = self.durable_checkpoint.take() else {
            return;
        };
        let result = tokio::select! {
            _ = self.cancel.cancelled() => return,
            result = pending.result => result,
        };
        let acknowledged = matches!(
            &result,
            Ok(super::super::FireResult::Fired { .. } | super::super::FireResult::Duplicate { .. })
        ) || matches!(&result, Ok(super::super::FireResult::Skipped { reason }) if reason == super::super::SERIAL_BUSY);
        if acknowledged {
            let advanced =
                ConfigAccess::transact_local(&self.node, None, "trigger.advance_arrival", |txn| {
                    Box::pin(async {
                        crate::config_client::event_source_cursor::checkpoint_prefix(
                            txn,
                            &pending.owner,
                            &pending.trigger_id,
                            &pending.collection,
                            &pending.position,
                            matches!(&result, Ok(super::super::FireResult::Skipped { reason })
                                if reason == super::super::SERIAL_BUSY),
                        )
                        .await
                    })
                })
                .await;
            self.durable_ready = advanced.as_ref().copied().unwrap_or(false);
            if let Err(error) = advanced {
                tracing::warn!(%error, trigger_id = %pending.trigger_id, "arrival checkpoint remains pending");
            }
        } else {
            self.durable_ready = false;
        }
    }

    pub(super) async fn next_durable_fire(&mut self) -> Option<FireIntent> {
        self.durable_ready = false;
        let snapshot = self.snapshot_rx.borrow().clone();
        let mut triggers = snapshot
            .active_event_triggers()
            .values()
            .filter(|t| {
                t.enabled
                    && t.event_kind == "created"
                    && t.fire_mode == crate::runtime_snapshot::EventTriggerFireMode::PerDocument
            })
            .cloned()
            .collect::<Vec<_>>();
        triggers.sort_by(|a, b| a.trigger_id.cmp(&b.trigger_id));
        if let Some(last) = &self.durable_after_trigger {
            let offset = triggers
                .iter()
                .position(|t| &t.trigger_id > last)
                .unwrap_or(0);
            triggers.rotate_left(offset);
        }
        for trigger in triggers {
            match self.next_trigger_arrival(&snapshot, &trigger).await {
                Ok(Some(intent)) => {
                    self.durable_after_trigger = Some(trigger.trigger_id.clone());
                    return Some(intent);
                }
                Ok(None) => {}
                Err(error) => tracing::warn!(%error, trigger_id = %trigger.trigger_id,
                    "durable trigger delivery remains pending"),
            }
        }
        None
    }

    async fn next_trigger_arrival(
        &mut self,
        snapshot: &ActiveRuntimeSnapshot,
        trigger: &crate::runtime_snapshot::ResolvedEventTrigger,
    ) -> anyhow::Result<Option<FireIntent>> {
        let owner = Self::delivery(snapshot, trigger)?.owner().to_owned();
        let record =
            ConfigAccess::transact_local(&self.node, None, "trigger.read_arrival_cursor", |txn| {
                Box::pin(async {
                    crate::config_client::event_source_cursor::load_or_seed_for_source(
                        txn,
                        &owner,
                        &trigger.trigger_id,
                        &trigger.source_collection,
                    )
                    .await
                })
            })
            .await?;
        let response = crate::graphql::graphql_with_transaction_retry(&self.node, &format!(
            "{{ _documentArrivals(collection: \"{}\", after: \"{}\", limit: 128) {{ head next entries {{ cursor docID }} }} }}",
            escape_graphql_string(&record.cursor.source_collection), escape_graphql_string(&record.cursor.after)), "trigger.read_arrivals").await?;
        let data = response.data.context("arrival query omitted data")?;
        let page = &data["_documentArrivals"];
        let entries = page["entries"]
            .as_array()
            .context("arrival query omitted entries")?;
        for entry in entries {
            let doc_id = entry["docID"]
                .as_str()
                .context("arrival lacks document ID")?;
            let position = entry["cursor"].as_str().context("arrival lacks cursor")?;
            if !self.probe_filter(doc_id, trigger).await? {
                self.advance_arrival(&owner, trigger, position).await?;
                continue;
            }
            let mut build = self
                .build_intents_for_candidates(
                    snapshot,
                    &trigger.source_collection,
                    doc_id,
                    vec![trigger.clone()],
                    false,
                )
                .await;
            let Some(mut intent) = build.intents.pop() else {
                if build.correlation_pending {
                    self.commit_delivery_seen_state(&trigger.source_collection, doc_id, &build);
                }
                anyhow::bail!("source document {doc_id} could not be rendered into a fire")
            };
            let (tx, rx) = tokio::sync::oneshot::channel();
            let observe = intent.on_result;
            intent.on_result = Box::new(move |result| {
                let _ = tx.send(result.clone());
                observe(result);
            });
            self.durable_checkpoint = Some(PendingCheckpoint {
                owner,
                trigger_id: trigger.trigger_id.clone(),
                collection: trigger.source_collection.clone(),
                position: position.into(),
                result: rx,
            });
            return Ok(Some(intent));
        }
        let next = page["next"]
            .as_str()
            .context("arrival query omitted next cursor")?;
        if next != record.cursor.after {
            self.advance_arrival(&owner, trigger, next).await?;
        }
        // The existing source timer provides the next bounded page; a source
        // with no matching documents cannot monopolize the trigger driver.
        Ok(None)
    }

    async fn advance_arrival(
        &self,
        owner: &str,
        trigger: &crate::runtime_snapshot::ResolvedEventTrigger,
        position: &str,
    ) -> anyhow::Result<()> {
        ConfigAccess::transact_local(&self.node, None, "trigger.exclude_arrival", |txn| {
            Box::pin(async {
                crate::config_client::event_source_cursor::exclude_arrival(
                    txn,
                    owner,
                    &trigger.trigger_id,
                    &trigger.source_collection,
                    position,
                )
                .await
            })
        })
        .await
    }
}
