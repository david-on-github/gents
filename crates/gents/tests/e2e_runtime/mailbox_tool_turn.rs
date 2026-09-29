use std::sync::Arc;
use std::time::Duration;

use gents::document_config::{DatastoreTools, SurfaceToolDecl, Tools};
use gents::mailbox::{canonical_mailbox_write_decl, list_mailbox_items, MailboxStatus};
use gents::{AgentIdentity, Collection, DatastoreToolSurfaceDocument};

use crate::support::accepted_turn::{
    boot_prepared_accepted_turn, prepare_accepted_turn, AcceptedTurnSpec,
};
use crate::support::fixtures::configure_behavior_tools;
use crate::support::interrupt::create_runtime_request_caused_by_source;
use crate::support::live_inference::wait_for_request_terminal;
use crate::support::streaming_backend::{StreamChunk, StreamPlan, StreamResponse};
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

async fn configure_engineer_mailbox(db: &crate::support::TestDb, did: &str) {
    configure_behavior_tools(
        db.node.as_ref(),
        did,
        BEHAVIOR,
        None,
        Tools {
            tools_id: format!("{BEHAVIOR}:tools"),
            agent_did: did.to_string(),
            datastore: Some(DatastoreTools {
                datastore_tool_surface_ids: Some(vec![SURFACE.into()]),
                ..Default::default()
            }),
            ..Default::default()
        },
        vec![(
            Collection::DatastoreToolSurface,
            serde_json::to_value(DatastoreToolSurfaceDocument {
                surface_id: SURFACE.into(),
                agent_did: did.to_string(),
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
}

/// A question filed without waiting is answered by the item's ordinary reply
/// request: the claim consumes the item and the answer reaches the model as
/// the session's next user message.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_filed_question_is_answered_through_the_reply_request() {
    use gents_protocol::mailbox_question::{
        MailboxQuestion, MailboxQuestionAnswer, MailboxQuestionOption, MAILBOX_QUESTION_VERSION,
    };
    let db = test_db("mailbox-question-turn").await;
    let did = db.node_identity.did().to_string();
    let session_id = "mailbox-question-session";
    let question = MailboxQuestion {
        version: MAILBOX_QUESTION_VERSION,
        prompt: "Ship the release?".into(),
        options: ["yes", "no"]
            .map(|id| MailboxQuestionOption {
                id: id.into(),
                label: id.to_uppercase(),
                description: None,
            })
            .to_vec(),
        multi_select: false,
        allow_free_text: true,
    };
    let reply = question
        .reply_content(
            "Release",
            &MailboxQuestionAnswer {
                option_ids: vec!["yes".into()],
                free_text: Some("after the tag".into()),
            },
        )
        .unwrap();
    let prepared = prepare_accepted_turn(
        &db,
        AcceptedTurnSpec {
            backend_id: "mailbox-question-backend",
            model: "mailbox-question-model",
            parent_behavior_id: BEHAVIOR,
            configured_behavior_ids: &[BEHAVIOR],
            request_id: "mailbox-question-request",
            session_id,
            prompt: "mailbox-question-prompt",
            accepted_chunks: vec![StreamChunk::tool_call(
                "mailbox-question-call",
                gents::mailbox::FILE_MAILBOX_ITEM_TOOL_NAME,
                serde_json::json!({"title": "Release", "question": question}).to_string(),
            )],
            child_plans: vec![StreamPlan::new(
                reply.clone(),
                vec![StreamResponse::completes(reply.clone(), ["noted"])],
            )],
            valid_until: None,
            subagent_depth: None,
            request_setup: None,
        },
    )
    .await;
    configure_engineer_mailbox(&db, &did).await;
    let identity: Arc<dyn AgentIdentity> = db.node_identity.clone();
    let agent = gents::Gents::from_default_behavior_documents(
        db.node.clone(),
        identity,
        gents::DocumentRuntimeOptions::default(),
    )
    .await
    .unwrap();
    let runtime = boot_prepared_accepted_turn(&db, prepared, agent).await;
    assert_eq!(
        wait_for_request_terminal(
            db.node.as_ref(),
            "mailbox-question-request",
            Duration::from_secs(60)
        )
        .await,
        "completed",
        "filing a question must not suspend or fail the asking turn"
    );
    let items = list_mailbox_items(db.node.as_ref(), &did, Some(MailboxStatus::Open))
        .await
        .unwrap();
    assert_eq!(items.len(), 1);
    let item = &items[0];
    assert_eq!(
        (item.kind.as_str(), item.action.as_str()),
        ("ask", "start_request")
    );
    assert_eq!(item.session_id.as_deref(), Some(session_id));
    assert_eq!(
        MailboxQuestion::from_payload(item.payload.as_deref().unwrap()).unwrap(),
        question
    );

    let reply_doc = create_runtime_request_caused_by_source(
        db.node.as_ref(),
        &did,
        BEHAVIOR,
        "mailbox-question-reply",
        session_id,
        &item.doc_id,
        &reply,
    )
    .await;
    assert_eq!(
        wait_for_request_terminal(
            db.node.as_ref(),
            "mailbox-question-reply",
            Duration::from_secs(60)
        )
        .await,
        "completed"
    );
    let answered = gents::mailbox::load_mailbox_item(db.node.as_ref(), &item.doc_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(answered.status, "acted");
    assert_eq!(
        answered.resolved_doc_id.as_deref(),
        Some(reply_doc.as_str())
    );
    let bodies = runtime.backend.observed_completion_bodies();
    let delivered = bodies.last().expect("reply provider turn").to_string();
    assert!(
        delivered.contains("Decision on Release: YES (yes)") && delivered.contains("after the tag"),
        "the answer must reach the model in the asking session: {delivered}"
    );
    runtime.shutdown().await;
}
