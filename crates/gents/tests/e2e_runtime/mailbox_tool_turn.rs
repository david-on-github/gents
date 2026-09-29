use std::sync::Arc;
use std::time::Duration;

use gents::document_config::{DatastoreTools, SurfaceToolDecl, Tools};
use gents::mailbox::{canonical_mailbox_write_decl, list_mailbox_items, MailboxStatus};
use gents::{AgentIdentity, Collection, DatastoreToolSurfaceDocument};

use crate::support::accepted_turn::{
    boot_prepared_accepted_turn, prepare_accepted_turn, AcceptedTurnSpec,
};
use crate::support::fixtures::configure_behavior_tools;
use crate::support::live_inference::wait_for_request_terminal;
use crate::support::streaming_backend::StreamChunk;
use crate::support::test_db;

const BEHAVIOR: &str = "mailbox-turn-engineer";
const SURFACE: &str = "engineer-mailbox";

/// Filing an informational mailbox item through a surface shaped like the
/// Engineer's (`gents init --setup-steward`) is an ordinary tool call: the
/// receipt reaches the model and the request completes on the next turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn filing_a_mailbox_item_returns_a_receipt_and_the_turn_continues() {
    let db = test_db("mailbox-tool-turn").await;
    let did = db.node_identity.did().to_string();
    let request_id = "mailbox-tool-turn-request";
    let prompt = "mailbox-tool-turn-prompt";
    let prepared = prepare_accepted_turn(
        &db,
        AcceptedTurnSpec {
            backend_id: "mailbox-tool-turn-backend",
            model: "mailbox-tool-turn-model",
            parent_behavior_id: BEHAVIOR,
            configured_behavior_ids: &[BEHAVIOR],
            request_id,
            session_id: "mailbox-tool-turn-session",
            prompt,
            accepted_chunks: vec![StreamChunk::tool_call(
                "mailbox-tool-turn-call",
                gents::mailbox::FILE_MAILBOX_ITEM_TOOL_NAME,
                serde_json::json!({
                    "title": "Crew ready",
                    "summary": "Two agents configured.\n\n- builder\n- reviewer",
                })
                .to_string(),
            )],
            child_plans: Vec::new(),
            valid_until: None,
            subagent_depth: None,
            request_setup: None,
        },
    )
    .await;
    configure_behavior_tools(
        db.node.as_ref(),
        &did,
        BEHAVIOR,
        None,
        Tools {
            tools_id: format!("{BEHAVIOR}:tools"),
            agent_did: did.clone(),
            datastore: Some(DatastoreTools {
                enable_defra_query: Some(true),
                datastore_tool_surface_ids: Some(vec![SURFACE.into()]),
                ..Default::default()
            }),
            subagents: Some(gents::document_config::SubagentTools {
                target_ids: Vec::new(),
                enabled: Some(true),
            }),
            built_ins: Some(gents::document_config::BuiltInTools {
                enable_session_history_tool: Some(true),
                enable_graph_tools: Some(true),
                ..Default::default()
            }),
            self_config: Some(gents::agent::persona_ops::setup_steward_self_config()),
            ..Default::default()
        },
        vec![(
            Collection::DatastoreToolSurface,
            serde_json::to_value(DatastoreToolSurfaceDocument {
                surface_id: SURFACE.into(),
                agent_did: did.clone(),
                display_name: Some("Engineer escalations".into()),
                enabled: true,
                entries: Some(vec![
                    SurfaceToolDecl::Create(canonical_mailbox_write_decl()),
                ]),
                created_at: None,
                tags: Vec::new(),
            })
            .unwrap(),
        )],
    )
    .await;
    let identity: Arc<dyn AgentIdentity> = db.node_identity.clone();
    let agent = gents::Gents::from_default_behavior_documents(
        db.node.clone(),
        identity,
        gents::DocumentRuntimeOptions::default(),
    )
    .await
    .unwrap();
    let runtime = boot_prepared_accepted_turn(&db, prepared, agent).await;

    let state =
        wait_for_request_terminal(db.node.as_ref(), request_id, Duration::from_secs(60)).await;
    let failure = db
        .node
        .execute(&format!(
            r#"{{ AgentRequest(filter: {{ request_id: {{ _eq: "{request_id}" }} }}) {{ lifecycle_state failure_reason }} }}"#
        ))
        .await;
    let tool_calls = db
        .node
        .execute(r#"{ AgentToolCall { tool_name lifecycle_state status tool_failure_class } }"#)
        .await;
    assert_eq!(
        state, "completed",
        "request: {:?}; tool calls: {:?} {:?}",
        failure.data, tool_calls.data, tool_calls.errors
    );
    let items = list_mailbox_items(db.node.as_ref(), &did, Some(MailboxStatus::Open))
        .await
        .unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].title, "Crew ready");
    // The first filing's outcome comes from the Lean-bound notification owner.
    let outcome = serde_json::to_value(
        gents::mailbox::NotificationIdentity::Event.write_outcome(false, false),
    )
    .unwrap();
    let receipt_marker = format!("\"outcome\":{outcome}").replace('"', "\\\"");
    let bodies = runtime.backend.observed_completion_bodies();
    let followup = bodies.last().expect("follow-up provider turn").to_string();
    assert!(
        followup.contains(&receipt_marker),
        "the receipt must reach the model: {followup}"
    );
    runtime.shutdown().await;
}
