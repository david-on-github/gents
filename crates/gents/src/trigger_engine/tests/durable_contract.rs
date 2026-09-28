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
        assert_eq!(durable::outcome_due(r.fire.emit_outcome, r.fire.goal_backed,
            &r.goal_status, r.terminal), case["due"].as_bool().unwrap(), "{}", case["name"]);
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
    for case in cases["cursors"].as_array().unwrap() {
        assert_eq!(durable::pending_delivery_ids(case["seeded"].as_bool().unwrap(),
            &decode::<Vec<FireIdentity>>(&case["baseline"]),
            &decode::<Vec<FireIdentity>>(&case["committed"]),
            &decode::<Vec<FireIdentity>>(&case["source"]),
            case["enabled"].as_bool().unwrap()), decode::<Vec<FireIdentity>>(&case["pending"]),
            "{}", case["name"]);
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
