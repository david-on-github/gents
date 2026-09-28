use super::*;
use crate::trigger_engine::durable;
use gents_protocol::trigger_delivery::{FireIdentity, TriggerFire};
use serde::Deserialize;

#[derive(Clone, Deserialize)]
struct Fire {
    identity: FireIdentity,
    session: String,
    serial: bool,
    emit_outcome: bool,
    goal_backed: bool,
}

#[derive(Clone, Deserialize)]
struct Request {
    fire: Fire,
    running: bool,
    terminal: bool,
    goal_status: String,
    goal_wrapup_completed: bool,
    goal_assignment_applied: bool,
}

#[derive(Deserialize)]
struct State {
    receipts: Vec<FireIdentity>,
    requests: Vec<Request>,
    outcomes: Vec<FireIdentity>,
}

fn contract() -> serde_json::Value {
    gents_lean_contract::load_contract_snapshot::<serde_json::Value>()
        .unwrap()["trigger_delivery"]
        .clone()
}

fn decode<T: serde::de::DeserializeOwned>(value: &serde_json::Value) -> T {
    serde_json::from_value(value.clone()).unwrap()
}

fn receipt(fire: &Fire, index: usize) -> TriggerFire {
    let key = durable::fire_key(&fire.identity);
    TriggerFire {
        fire_key: key.clone(),
        identity: fire.identity.clone(),
        task_id: "contract-task".into(),
        request_id: format!("trigger-request:{key}"),
        session_id: fire.session.clone(),
        goal_id: fire.goal_backed.then(|| format!("goal:{}", fire.session)),
        goal_objective: fire.goal_backed.then(|| "contract objective".into()),
        goal_token_budget: None,
        goal_assignment_applied: false,
        emit_outcome: fire.emit_outcome,
        queued_serial: fire.serial,
        source_handoff_id: Some(format!("source:{}", fire.identity.source_doc_id)),
        reply_session_id: Some("reply-session".into()),
        shard_id: None,
        attempt: None,
        created_at: format!("2030-01-01T00:00:{index:02}Z"),
    }
}

fn request_mutation(receipt: &TriggerFire) -> String {
    format!(r#"mutation {{ create_AgentRequest(input: {{
        request_id: "{}", agent_did: "{}", session_id: "{}",
        behavior_id: "general", purpose: "normal", lifecycle_state: "pending",
        created_at: "{}"
    }}) {{ _docID }} }}"#,
        escape_graphql_string(&receipt.request_id),
        escape_graphql_string(&receipt.identity.owner_did),
        escape_graphql_string(&receipt.session_id),
        escape_graphql_string(&receipt.created_at))
}

#[test]
fn durable_delivery_predicates_match_executable_lean_owners() {
    let cases = contract();
    for case in cases["identities"].as_array().unwrap() {
        let id: FireIdentity = decode(&case["identity"]);
        let key = durable::fire_key(&id);
        assert_eq!(key, case["key"].as_str().unwrap());
        assert_eq!(format!("trigger-request:{key}"), case["request_id"]);
        assert_eq!(format!("trigger-session:{key}"), case["session_id"]);
        assert_eq!(format!("outcome:{key}"), case["outcome_id"]);
    }
    for case in cases["sessions"].as_array().unwrap() {
        let id: FireIdentity = decode(&case["identity"]);
        assert_eq!(durable::resolve_session_id(&id, case["target"].as_str(),
            case["owned"].as_bool().unwrap()), decode::<Option<String>>(&case["resolved"]));
    }
    for case in cases["outcomes"].as_array().unwrap() {
        let state: State = decode(&case["pre"]);
        let r = &state.requests[0];
        assert_eq!((!r.fire.goal_backed || r.goal_assignment_applied) && durable::outcome_due(r.fire.emit_outcome, r.fire.goal_backed,
            &r.goal_status, r.goal_wrapup_completed, r.terminal), case["due"].as_bool().unwrap(), "{}", case["name"]);
    }
    for case in cases["queues"].as_array().unwrap() {
        let state: State = decode(&case["pre"]);
        let rows = state.requests.iter().map(|r| durable::FireQueueRow {
            identity: r.fire.identity.clone(), session_id: r.fire.session.clone(),
            queued_serial: r.fire.serial, running: r.running, terminal: r.terminal,
        }).collect::<Vec<_>>();
        assert_eq!(durable::queued_claim_allowed(&rows, &decode(&case["identity"])),
            case["can_claim"].as_bool().unwrap(), "{}", case["name"]);
    }

}

#[tokio::test]
async fn generated_fire_transactions_are_atomic_and_owner_scoped() {
    for case in contract()["admissions"].as_array().unwrap() {
        let node = defra_node::EmbeddedNode::builder().build().await.unwrap();
        ensure_runtime_schemas(&node).await.unwrap();
        let pre: State = decode(&case["pre"]);
        for (index, request) in pre.requests.iter().enumerate() {
            let fire = receipt(&request.fire, index);
            let mutation = request_mutation(&fire);
            crate::config_client::ConfigAccess::transact_local(&node, None, "test.seed_fire",
                |txn| Box::pin(async { durable::stage_fire_request(txn, &fire, &mutation).await }))
                .await.unwrap();
        }
        let f: Fire = decode(&case["fire"]);
        let fire = receipt(&f, pre.requests.len());
        let mutation = request_mutation(&fire);
        let commit = case["commit"].as_bool().unwrap();
        let result = crate::config_client::ConfigAccess::transact_local(&node, None,
            "test.fire_crash_boundary", |txn| Box::pin(async {
                durable::stage_fire_request(txn, &fire, &mutation).await?;
                anyhow::ensure!(commit, "injected pre-commit crash");
                Ok(())
            })).await;
        assert_eq!(result.is_ok(), commit, "{}", case["name"]);
        let response = crate::graphql::graphql_with_transaction_retry(&node,
            "{ TriggerFire { fire_key } AgentRequest { request_id } }", "test.fire_receipts")
            .await.unwrap();
        let post: State = decode(&case["post"]);
        let data = response.data.unwrap();
        assert_eq!(data["TriggerFire"].as_array().unwrap().len(), post.receipts.len(), "{}", case["name"]);
        assert_eq!(data["AgentRequest"].as_array().unwrap().len(), post.requests.len(), "{}", case["name"]);
        assert!(post.outcomes.is_empty());
    }
}

#[tokio::test]
async fn generated_terminal_outcomes_recover_once_without_chaining() {
    for case in contract()["outcomes"].as_array().unwrap() {
        let node = defra_node::EmbeddedNode::builder().build().await.unwrap();
        ensure_runtime_schemas(&node).await.unwrap();
        let pre: State = decode(&case["pre"]);
        let request = &pre.requests[0];
        let mut fire = receipt(&request.fire, 0);
        fire.goal_assignment_applied = request.goal_assignment_applied;
        let publish = || crate::config_client::ConfigAccess::transact_local(&node, None,
            "test.terminal_outcome", |txn| Box::pin(async {
                durable::stage_outcome(txn, &fire, &request.goal_status, request.goal_wrapup_completed, request.terminal,
                    if request.fire.goal_backed { &request.goal_status } else { "completed" },
                    "contract terminal", "2030-01-01T00:01:00Z").await
            }));
        if !pre.outcomes.is_empty() { publish().await.unwrap(); }
        let rollback: anyhow::Result<()> = crate::config_client::ConfigAccess::transact_local(
            &node, None, "test.outcome_precommit_crash", |txn| Box::pin(async {
                durable::stage_outcome(txn, &fire, &request.goal_status, request.goal_wrapup_completed, request.terminal,
                    "completed", "contract terminal", "2030-01-01T00:01:00Z").await?;
                anyhow::bail!("injected crash before outcome commit")
            })).await;
        assert!(rollback.is_err());
        publish().await.unwrap();
        publish().await.unwrap();
        let response = crate::graphql::graphql_with_transaction_retry(&node,
            "{ FireOutcome { handoff_id source_handoff_id } }", "test.outcomes").await.unwrap();
        let post: State = decode(&case["post"]);
        let data = response.data.unwrap();
        let outcomes = data["FireOutcome"].as_array().unwrap();
        assert_eq!(outcomes.len(), post.outcomes.len(), "{}", case["name"]);
        for outcome in outcomes {
            assert_eq!(outcome["handoff_id"], format!("outcome:{}", fire.fire_key));
            assert_eq!(outcome["source_handoff_id"], fire.source_handoff_id.as_deref().unwrap());
        }
    }
}

#[derive(Clone, Deserialize)]
struct Arrival {
    position: String,
    identity: FireIdentity,
}

async fn create_arrival_source(access: &crate::config_client::ConfigAccess, label: &str) -> String {
    let response = access
        .write(
            "test.arrival_document",
            &format!(
                "mutation {{ create_Work(input: {{label: \"{}\"}}) {{ _docID }} }}",
                escape_graphql_string(label),
            ),
        )
        .await
        .unwrap();
    let value = &response["data"]["create_Work"];
    let document = value
        .as_array()
        .and_then(|rows| rows.first())
        .unwrap_or(value);
    document["_docID"].as_str().unwrap().to_owned()
}

async fn saved_arrival_cursor(access: &crate::config_client::ConfigAccess) -> String {
    access
        .transact("test.read_arrival_cursor", |txn| {
            Box::pin(async move {
                Ok(crate::config_client::event_source_cursor::load_or_seed(
                    txn, "owner-a", "handoff",
                )
                .await?
                .cursor
                .after)
            })
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn generated_arrival_checkpoints_preserve_committed_delivery_across_crashes() {
    use crate::config_client::{event_source_cursor, ConfigAccess};
    for case in contract()["cursors"].as_array().unwrap() {
        let node = std::sync::Arc::new(defra_node::EmbeddedNode::builder().build().await.unwrap());
        ensure_runtime_schemas(&node).await.unwrap();
        let access = ConfigAccess::Local(node.clone());
        access
            .add_schema("type Work { label: String }")
            .await
            .unwrap();
        let source: Vec<Arrival> = decode(&case["source"]);
        let seed_head = case["seed_head"]
            .as_str()
            .unwrap()
            .parse::<usize>()
            .unwrap();
        let mut documents = std::collections::BTreeMap::new();
        for entry in source.iter().take(seed_head) {
            documents.insert(
                entry.identity.source_doc_id.clone(),
                create_arrival_source(&access, &entry.identity.source_doc_id).await,
            );
        }
        access.transact("test.arrival_config", |txn| Box::pin(async move {
            txn.execute_with_variables(
                "mutation($input:EventSourceMutationInputArg!){create_EventSource(input:$input){_docID}}",
                &serde_json::json!({"input":{"agent_did":"owner-a","event_source_id":"source",
                    "source_collection":"Work","event_kind":"created"}}),
            ).await?;
            txn.execute_with_variables(
                "mutation($input:TriggerMutationInputArg!){create_Trigger(input:$input){_docID}}",
                &serde_json::json!({"input":{"agent_did":"owner-a","trigger_id":"handoff",
                    "task_id":"contract-task","source":{"kind":"event","event_source_id":"source"},
                    "enabled":case["enabled"]}}),
            ).await?;
            Ok(())
        })).await.unwrap();
        assert_eq!(
            saved_arrival_cursor(&access).await,
            seed_head.to_string(),
            "{}",
            case["name"]
        );
        for entry in source.iter().skip(seed_head) {
            documents.insert(
                entry.identity.source_doc_id.clone(),
                create_arrival_source(&access, &entry.identity.source_doc_id).await,
            );
        }
        let pre_after = case["pre_cursor"]["after"].as_str().unwrap();
        access
            .transact("test.prior_checkpoint", |txn| {
                Box::pin(async move {
                    event_source_cursor::advance(txn, "owner-a", "handoff", "Work", pre_after).await
                })
            })
            .await
            .unwrap();
        if case["restart"].as_bool().unwrap() {
            assert_eq!(
                saved_arrival_cursor(&access).await,
                pre_after,
                "{}",
                case["name"]
            );
        }
        let adapt_fire = |mut fire: Fire| {
            fire.identity.source_doc_id = documents[&fire.identity.source_doc_id].clone();
            fire
        };
        let pre: State = decode(&case["pre"]);
        for (index, request) in pre.requests.iter().enumerate() {
            let receipt = receipt(&adapt_fire(request.fire.clone()), index);
            let mutation = request_mutation(&receipt);
            access
                .transact("test.prior_arrival_admission", |txn| {
                    let receipt = &receipt;
                    let mutation = &mutation;
                    Box::pin(async move {
                        durable::stage_fire_request(txn, &receipt, &mutation).await?;
                        Ok(())
                    })
                })
                .await
                .unwrap();
        }
        if let Some(commit) = case["admission_commit"].as_bool() {
            let receipt = receipt(&adapt_fire(decode(&case["fire"])), pre.requests.len());
            let mutation = request_mutation(&receipt);
            let admitted: anyhow::Result<()> = access
                .transact("test.arrival_admission_crash", |txn| {
                    let receipt = &receipt;
                    let mutation = &mutation;
                    Box::pin(async move {
                        durable::stage_fire_request(txn, &receipt, &mutation).await?;
                        anyhow::ensure!(commit, "injected crash before receipt/request commit");
                        Ok(())
                    })
                })
                .await;
            assert_eq!(admitted.is_ok(), commit, "{}: {admitted:?}", case["name"]);
        }
        if let Some(commit) = case["checkpoint_commit"].as_bool() {
            let entry: Arrival = decode(&case["entry"]);
            let doc_id = documents[&entry.identity.source_doc_id].clone();
            let matched = case["matches_filter"].as_bool().unwrap();
            let checkpointed: anyhow::Result<()> = access
                .transact("test.arrival_checkpoint_crash", |txn| {
                    let doc_id = &doc_id;
                    let entry = &entry;
                    Box::pin(async move {
                        if matched {
                            event_source_cursor::acknowledge_fire(
                                txn,
                                "owner-a",
                                "handoff",
                                "Work",
                                &doc_id,
                                &entry.position,
                            )
                            .await?;
                        } else {
                            event_source_cursor::advance(
                                txn,
                                "owner-a",
                                "handoff",
                                "Work",
                                &entry.position,
                            )
                            .await?;
                        }
                        anyhow::ensure!(commit, "injected crash before checkpoint commit");
                        Ok(())
                    })
                })
                .await;
            assert_eq!(
                checkpointed.is_ok(),
                case["checkpoint_succeeds"].as_bool().unwrap(),
                "{}: {checkpointed:?}",
                case["name"]
            );
        }
        let after = saved_arrival_cursor(&access).await;
        assert_eq!(
            after,
            case["post_cursor"]["after"].as_str().unwrap(),
            "{}",
            case["name"]
        );
        let page = access.execute(&format!(
            "{{ _documentArrivals(collection: \"Work\", after: \"{}\", limit: 128) {{ entries {{ cursor docID }} }} }}",
            escape_graphql_string(&after),
        )).await.unwrap();
        let actual = page["data"]["_documentArrivals"]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| {
                (
                    entry["cursor"].as_str().unwrap().to_owned(),
                    entry["docID"].as_str().unwrap().to_owned(),
                )
            })
            .collect::<Vec<_>>();
        let expected = decode::<Vec<Arrival>>(&case["journal_after"])
            .iter()
            .map(|entry| {
                (
                    entry.position.clone(),
                    documents[&entry.identity.source_doc_id].clone(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(actual, expected, "{}", case["name"]);
        let rows = access
            .execute("{ TriggerFire { source_doc_id } AgentRequest { request_id } }")
            .await
            .unwrap();
        let post: State = decode(&case["post"]);
        assert_eq!(
            rows["data"]["TriggerFire"].as_array().unwrap().len(),
            post.receipts.len(),
            "{}",
            case["name"]
        );
        assert_eq!(
            rows["data"]["AgentRequest"].as_array().unwrap().len(),
            post.requests.len(),
            "{}",
            case["name"]
        );
    }
}

#[test]
fn observed_claim_cohorts_match_lean() {
    let contract = gents_lean_contract::load_contract_snapshot::<serde_json::Value>().unwrap();
    for case in contract["trigger_delivery"]["observed_claims"]
        .as_array()
        .unwrap()
    {
        let candidate: durable::ClaimObservation =
            serde_json::from_value(case["candidate"].clone()).unwrap();
        let rows: Vec<durable::ClaimObservation> =
            serde_json::from_value(case["rows"].clone()).unwrap();
        assert_eq!(
            durable::observed_claim_allowed(&candidate, &rows),
            case["allowed"].as_bool().unwrap(),
            "{}",
            case["name"]
        );
    }
}
