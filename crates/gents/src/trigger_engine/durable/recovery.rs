use super::{stage_outcome, FIRE_FIELDS};
use anyhow::{Context, Result};
use defra_node::EmbeddedNode;
use gents_protocol::trigger_delivery::TriggerFire;

/// Startup repairs missing terminal observations through the same outcome
/// transaction owner. Current terminal state is re-read inside each transaction;
/// the unique outcome identity makes post-commit replay a no-op.
pub(crate) async fn recover_outcomes(node: &EmbeddedNode, owner: &str) -> Result<usize> {
    let query = format!(
        r#"{{ TriggerFire(filter: {{owner_did: {{_eq: "{}"}}, emit_outcome: {{_eq: true}}}}) {{ fire_key }} }}"#,
        crate::graphql::escape_graphql_string(owner),
    );
    let response =
        crate::graphql::graphql_with_transaction_retry(node, &query, "recover Task fire outcomes")
            .await?;
    let rows: Vec<serde_json::Value> = crate::graphql::rows(&response, "TriggerFire")?;
    let mut recovered = 0;
    for row in rows {
        let key = row["fire_key"]
            .as_str()
            .context("Task fire is missing its identity")?;
        recovered += usize::from(crate::config_client::ConfigAccess::transact_local(
            node, None, "trigger.recover_outcome", |txn| Box::pin(async move {
                let query = format!(r#"{{ TriggerFire(filter: {{owner_did: {{_eq: "{}"}}, fire_key: {{_eq: "{}"}}}}) {{ {FIRE_FIELDS} }} }}"#,
                    crate::graphql::escape_graphql_string(owner), crate::graphql::escape_graphql_string(key));
                let response = txn.execute_local_response(&query).await?;
                let fires: Vec<TriggerFire> = crate::graphql::rows(&response, "TriggerFire")?;
                let Some(fire) = fires.first() else { return Ok(false) };
                anyhow::ensure!(fires.len() == 1, "ambiguous Task fire identity during outcome recovery");
                let now = chrono::Utc::now().to_rfc3339();
                if let Some(goal_id) = &fire.goal_id {
                    if !fire.goal_assignment_applied { return Ok(false) }
                    let goal = crate::goal::load_canonical_goal_in_txn(txn, owner, &fire.session_id).await?;
                    let Some(goal) = goal else { return Ok(false) };
                    anyhow::ensure!(&goal.goal_id == goal_id, "Task fire Goal identity changed during outcome recovery");
                    stage_outcome(txn, fire, &goal.status, goal.wrapup_completed.unwrap_or(false), false,
                        &goal.status, goal.last_blocked_reason.as_deref().unwrap_or(&goal.status), &now).await
                } else {
                    let response = txn.execute_local_response(&format!(r#"{{ AgentRequest(filter: {{
                        agent_did: {{_eq: "{}"}}, request_id: {{_eq: "{}"}}
                    }}) {{request_id lifecycle_state failure_reason}} }}"#,
                        crate::graphql::escape_graphql_string(owner), crate::graphql::escape_graphql_string(&fire.request_id))).await?;
                    let requests: Vec<gents_protocol::row::AgentRequestRow> = crate::graphql::rows(&response, "AgentRequest")?;
                    anyhow::ensure!(requests.len() == 1, "Task fire has no unique admitted request during outcome recovery");
                    let request = &requests[0];
                    let status = request.lifecycle_state.context("Task request is missing lifecycle state")?;
                    stage_outcome(txn, fire, "active", false, status.is_terminal(), status.as_str(),
                        request.failure_reason.as_deref().filter(|reason| !reason.is_empty()).unwrap_or(status.as_str()), &now).await
                }
            }),
        ).await?);
    }
    Ok(recovered)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_client::ConfigAccess;
    use crate::trigger_engine::durable::{fire_key, stage_fire_request};

    #[tokio::test]
    async fn restart_outcome_recovery_matches_modeled_crash_boundaries() {
        let contract = gents_lean_contract::load_contract_snapshot::<serde_json::Value>().unwrap();
        for name in [
            "crash_after_terminal_before_outcome",
            "crash_after_outcome_retry",
            "continuing_goal_has_no_outcome",
        ] {
            let case = contract["trigger_delivery"]["outcomes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|case| case["name"] == name)
                .unwrap();
            let modeled = &case["pre"]["requests"][0];
            let modeled_fire = &modeled["fire"];
            let identity = serde_json::from_value(modeled_fire["identity"].clone()).unwrap();
            let key = fire_key(&identity);
            let goal_backed = modeled_fire["goal_backed"].as_bool().unwrap();
            let session = modeled_fire["session"].as_str().unwrap().to_owned();
            let fire = TriggerFire {
                fire_key: key.clone(),
                identity,
                task_id: "recovery-task".into(),
                request_id: format!("trigger-request:{key}"),
                session_id: session.clone(),
                goal_id: goal_backed.then(|| "recovery-goal".into()),
                goal_objective: goal_backed.then(|| "finish assignment".into()),
                goal_token_budget: None,
                goal_assignment_applied: goal_backed,
                emit_outcome: modeled_fire["emit_outcome"].as_bool().unwrap(),
                queued_serial: modeled_fire["serial"].as_bool().unwrap(),
                source_handoff_id: Some("source-assignment".into()),
                reply_session_id: None,
                shard_id: None,
                attempt: None,
                created_at: "2026-01-01T00:00:00Z".into(),
            };
            let directory = tempfile::tempdir().unwrap();
            let node = EmbeddedNode::builder()
                .data_path(directory.path())
                .build()
                .await
                .unwrap();
            crate::ensure_runtime_schemas(&node).await.unwrap();
            let terminal = modeled["terminal"].as_bool().unwrap();
            let state = if terminal { "completed" } else { "pending" };
            let request_mutation = format!(
                r#"mutation {{create_AgentRequest(input: {{
                request_id: "{}", agent_did: "{}", session_id: "{}", behavior_id: "behavior",
                purpose: "normal", lifecycle_state: "{state}", created_at: "2026-01-01T00:00:00Z"
            }}) {{_docID}} }}"#,
                crate::graphql::escape_graphql_string(&fire.request_id),
                crate::graphql::escape_graphql_string(&fire.identity.owner_did),
                crate::graphql::escape_graphql_string(&session)
            );
            ConfigAccess::transact_local(&node, None, "test.seed_terminal_outcome_boundary", |txn| {
                let fire = &fire;
                let request_mutation = &request_mutation;
                Box::pin(async move {
                    stage_fire_request(txn, fire, request_mutation).await?;
                    if goal_backed {
                        txn.execute_with_variables("mutation($input: GoalMutationInputArg!) {create_Goal(input: $input) {_docID}}",
                            &serde_json::json!({"input": {
                                "goal_id": fire.goal_id, "agent_did": fire.identity.owner_did,
                                "session_id": fire.session_id, "objective": "finish assignment",
                                "status": modeled["goal_status"], "wrapup_completed": modeled["goal_wrapup_completed"],
                            }})).await?;
                    }
                    if !case["pre"]["outcomes"].as_array().unwrap().is_empty() {
                        stage_outcome(txn, fire, modeled["goal_status"].as_str().unwrap(), false, terminal,
                            state, state, "2026-01-01T00:00:01Z").await?;
                    }
                    Ok(())
                })
            }).await.unwrap();
            node.shutdown().await;
            drop(node);
            let node = EmbeddedNode::builder()
                .data_path(directory.path())
                .build()
                .await
                .unwrap();
            let expected = case["post"]["outcomes"].as_array().unwrap().len();
            let before = case["pre"]["outcomes"].as_array().unwrap().len();
            assert_eq!(
                recover_outcomes(&node, &fire.identity.owner_did)
                    .await
                    .unwrap(),
                expected - before,
                "{name}"
            );
            assert_eq!(
                recover_outcomes(&node, &fire.identity.owner_did)
                    .await
                    .unwrap(),
                0,
                "{name}"
            );
            let response = crate::graphql::graphql_with_transaction_retry(
                &node,
                "{ FireOutcome { handoff_id } }",
                "verify recovered outcome uniqueness",
            )
            .await
            .unwrap();
            let outcomes: Vec<serde_json::Value> =
                crate::graphql::rows(&response, "FireOutcome").unwrap();
            assert_eq!(outcomes.len(), expected, "{name}");
            node.shutdown().await;
        }
    }
}
