use std::sync::Arc;
use std::time::Duration;

use gents::defra_node::EmbeddedNode;
use gents::graphql::escape_graphql_string;
use gents::{AgentIdentity, DocumentRuntimeOptions, Gents, ToolCeiling};
use serde_json::json;

use crate::support::accepted_turn::{
    boot_prepared_accepted_turn, prepare_accepted_turn, AcceptedTurnSpec,
};
use crate::support::fixtures::{
    bind_behavior_backend, configure_subagent_behavior, subagent_target,
};
use crate::support::interrupt::{create_runtime_request, wait_for_runtime_ready, BootedAgent};
use crate::support::streaming_backend::{
    MockStreamingBackend, StreamChunk, StreamPlan, StreamResponse, StreamScript,
};
use crate::support::{test_db, TestDb};

const PARENT_BEHAVIOR_ID: &str = "e2e-session-order-parent";
const CHILD_BEHAVIOR_ID: &str = "e2e-session-order-child";
const BACKEND_ID: &str = "e2e-session-order-backend";
const MODEL: &str = "e2e-session-order-model";
const RESUME_BACKEND_ID: &str = "e2e-session-order-resume-backend";
const RESUME_MODEL: &str = "e2e-session-order-resume-model";
const SESSION_ID: &str = "e2e-session-order-session";
const PARENT_PROMPT: &str = "e2e-session-order-parent-prompt";
const CHILD_PROMPT: &str = "e2e-session-order-child-prompt";
const CHILD_ANSWER: &str = "e2e-session-order-notification-must-survive";
const RESUMED_PROMPT: &str = "e2e-session-order-parent-resumes";

struct TranscriptRow {
    sequence: u32,
    content: String,
}

async fn configure_session_chain(db: &TestDb) {
    let agent_did = db.node_identity.did().to_string();
    configure_subagent_behavior(
        db.node.as_ref(),
        &agent_did,
        CHILD_BEHAVIOR_ID,
        "e2e-session-order-child-tools",
        Vec::new(),
        false,
    )
    .await;
    configure_subagent_behavior(
        db.node.as_ref(),
        &agent_did,
        PARENT_BEHAVIOR_ID,
        "e2e-session-order-parent-tools",
        vec![subagent_target(
            &agent_did,
            CHILD_BEHAVIOR_ID,
            &agent_did,
            CHILD_BEHAVIOR_ID,
        )],
        true,
    )
    .await;
}

/// Every message of the session in sequence order, reconstructed through the
/// canonical message owner.
async fn session_transcript(node: &EmbeddedNode, agent_did: &str) -> Vec<TranscriptRow> {
    let session_id = escape_graphql_string(SESSION_ID);
    let response = node
        .execute(&format!(
            r#"{{ AgentMessage(filter: {{ session_id: {{ _eq: "{session_id}" }} }}, order: {{ sequence: ASC }}) {{ _docID }} }}"#
        ))
        .await;
    assert!(
        !response.has_errors(),
        "message query failed: {:?}",
        response.errors
    );
    let header_ids = response
        .data
        .as_ref()
        .and_then(|data| data.get("AgentMessage"))
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .map(|row| row["_docID"].as_str().expect("message identity").to_owned())
        .collect::<Vec<_>>();
    let mut rows = Vec::with_capacity(header_ids.len());
    for header_id in header_ids {
        let (header, message) = gents::session::load_canonical_message_from_node(
            node,
            &header_id,
            agent_did,
            Some(agent_did),
        )
        .await
        .unwrap_or_else(|error| panic!("reconstruct message {header_id}: {error:#}"));
        rows.push(TranscriptRow {
            sequence: header.sequence,
            content: serde_json::to_string(&message).expect("serialize message"),
        });
    }
    rows
}

async fn wait_for_transcript_marker(node: &EmbeddedNode, agent_did: &str, marker: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while !session_transcript(node, agent_did)
        .await
        .iter()
        .any(|row| row.content.contains(marker))
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {marker} in the caller session"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The observer appends the started session's completion notification to the
/// caller's session. A prompt authored after a restart must be sequenced after
/// that notification, not over it: the resumed runtime's hook sequence is
/// read from the durable transcript rather than from the stopped runtime.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resumed_prompt_sorts_after_an_observer_appended_notification() {
    let db = test_db("e2e-session-order").await;
    let agent_did = db.node_identity.did().to_string();
    configure_session_chain(&db).await;

    let prepared = prepare_accepted_turn(
        &db,
        AcceptedTurnSpec {
            backend_id: BACKEND_ID,
            model: MODEL,
            parent_behavior_id: PARENT_BEHAVIOR_ID,
            configured_behavior_ids: &[PARENT_BEHAVIOR_ID, CHILD_BEHAVIOR_ID],
            request_id: "e2e-session-order-request",
            session_id: SESSION_ID,
            prompt: PARENT_PROMPT,
            accepted_chunks: vec![StreamChunk::tool_call(
                "e2e-session-order-tool-call",
                gents::toolset::CREATE_SESSION_TOOL_NAME,
                json!({ "agent": CHILD_BEHAVIOR_ID, "prompt": CHILD_PROMPT }).to_string(),
            )],
            child_plans: vec![StreamPlan::new(
                CHILD_PROMPT,
                vec![StreamResponse::completes(CHILD_PROMPT, [CHILD_ANSWER])],
            )],
            valid_until: None,
            subagent_depth: Some(0),
            request_setup: None,
        },
    )
    .await;
    let identity: Arc<dyn AgentIdentity> = db.node_identity.clone();
    let agent = Gents::from_default_behavior_documents(
        db.node.clone(),
        identity,
        DocumentRuntimeOptions {
            tool_ceiling: ToolCeiling::meta_only(),
            ..Default::default()
        },
    )
    .await
    .expect("build first runtime");
    let first = boot_prepared_accepted_turn(&db, prepared, agent).await;
    wait_for_transcript_marker(db.node.as_ref(), &agent_did, CHILD_ANSWER).await;
    first.shutdown().await;

    let resume_backend = MockStreamingBackend::start(
        RESUME_MODEL,
        vec![StreamScript::completes(RESUMED_PROMPT, ["resumed"])],
    )
    .expect("start resume backend");
    bind_behavior_backend(
        db.node.as_ref(),
        db.node_identity.did(),
        PARENT_BEHAVIOR_ID,
        RESUME_BACKEND_ID,
        resume_backend.endpoint(),
        RESUME_MODEL,
    )
    .await;
    create_runtime_request(
        db.node.as_ref(),
        &agent_did,
        PARENT_BEHAVIOR_ID,
        "e2e-session-order-resume-request",
        SESSION_ID,
        RESUMED_PROMPT,
    )
    .await;
    let identity: Arc<dyn AgentIdentity> = db.node_identity.clone();
    let agent = Gents::from_default_behavior_documents(
        db.node.clone(),
        identity,
        DocumentRuntimeOptions {
            tool_ceiling: ToolCeiling::meta_only(),
            ..Default::default()
        },
    )
    .await
    .expect("build resumed runtime");
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let handle = tokio::spawn(agent.run(shutdown_rx));
    wait_for_runtime_ready(db.node.as_ref(), &agent_did).await;
    let resumed = BootedAgent::new(shutdown_tx, handle, agent_did.clone());
    wait_for_transcript_marker(db.node.as_ref(), &agent_did, RESUMED_PROMPT).await;
    resumed.shutdown().await;

    let transcript = session_transcript(db.node.as_ref(), &agent_did).await;
    let notification = transcript
        .iter()
        .find(|row| row.content.contains(CHILD_ANSWER))
        .expect("completion notification");
    let prompt = transcript
        .iter()
        .find(|row| row.content.contains(RESUMED_PROMPT))
        .expect("resumed authored prompt");
    assert!(
        prompt.sequence > notification.sequence,
        "resumed prompt {} must follow notification {}",
        prompt.sequence,
        notification.sequence
    );
}
