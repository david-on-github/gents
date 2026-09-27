//! Live end-to-end `agent_new`/`agent_message` tests against a real
//! inference target. An orchestrator agent, driven by the live model, starts a
//! session on an allowlisted agent; the started session runs its own behavior
//! (live model) and its result reaches the caller only as a background
//! completion notification plus a wake on the caller's session.
//!
//! Normal test runs skip these (they are `#[ignore]`-gated AND early-return
//! unless `GENTS_LIVE_SESSION_MESSAGE=1`). Inference comes from the target
//! named by `GENTS_EVAL_TARGET`. To run locally:
//!
//! ```bash
//! GENTS_LIVE_SESSION_MESSAGE=1 GENTS_EVAL_TARGET=workstation-1 \
//!   cargo test -p gents --features live-e2e --test e2e_live live_ -- --ignored --nocapture
//! ```
//!
//! The standard-path backgrounding test has its own gate:
//!
//! ```bash
//! GENTS_LIVE_BACKGROUNDING=1 GENTS_EVAL_TARGET=workstation-1 \
//!   cargo test -p gents --features live-e2e --test e2e_live \
//!   live_standard_backgrounding_uses_real_inference -- --ignored --nocapture
//! ```
//!
//! The cross-node test starts a session on another principal's node. The
//! caused `AgentRequest` is authored on the caller's node, replicated to the
//! target by the `subagent-coordinator` data-plane route, admitted there as a
//! Peer request under the target's enrollment authority, and its terminal
//! request, session, messages and output replicate back by `subagent-host`.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::sync::Once;
use std::time::{Duration, Instant};

use anyhow::Result;
use gents::agent::p2p_reconcile::resolve_template;
use gents::agent::p2p_reconcile::templates::{
    SUBAGENT_COORDINATOR_TEMPLATE, SUBAGENT_HOST_TEMPLATE,
};
use gents::config_client::ConfigAccess;
use gents::defra_node::EmbeddedNode;
use gents::document_config::{
    AgentBehavior, AgentContext, BashTools, HostTools, InferenceProfile, InferenceSampling,
    SubagentTools, Tools,
};
use gents::graphql::escape_graphql_string;
use gents::run_timeline_fetch::load_run_timeline_rows;
use gents::toolset::{AGENT_INTERRUPT_TOOL_NAME, AGENT_MESSAGE_TOOL_NAME, AGENT_NEW_TOOL_NAME};
use gents::{
    default_behavior_id_for_agent, default_inference_profile_id_for_behavior,
    ensure_agent_principal, AgentIdentity, BashMode, Collection, DocumentRuntimeOptions, Gents,
    ReasoningEffort, SubagentTargetDocument, ToolCeiling,
};
use gents_protocol::request_input::{QueuePolicy, QueueSource, RequestInput};
use gents_protocol::request_lifecycle::RequestLifecycleState;
use serde::Deserialize;

use crate::support::enrollment::{authorize_enrollment_peer, wait_for_peer_identity};
use crate::support::fixtures::{configure_behavior_tools, subagent_target, test_identity};
use crate::support::interrupt::{create_runtime_request, wait_for_runtime_ready, BootedAgent};
use crate::support::live_inference::{
    live_target, terminal_assistant_answer, wait_for_assistant_answer, wait_for_request_terminal,
    InferenceTarget,
};
use crate::support::{
    first_optional_row, snapshots::fetch_runtime_snapshot, test_db, test_p2p_db, TestDb,
};

const RESEARCHER_BEHAVIOR_ID: &str = "live-researcher";
const FAST_WORKER_BEHAVIOR_ID: &str = "live-fast-worker";
const BACKGROUND_WORKER_BEHAVIOR_ID: &str = "live-background-worker";
/// Model-facing agent names; the model never sees behavior ids.
const RESEARCHER_TARGET_NAME: &str = "researcher";
const FAST_WORKER_TARGET_NAME: &str = "fast-worker";
const BACKGROUND_WORKER_TARGET_NAME: &str = "background-worker";
const CROSS_NODE_NETWORK_ID: &str = "net-live-session-message";
const CROSS_NODE_NETWORK_NAME: &str = "Live Session Message Net";

static LIVE_TRACE_INIT: Once = Once::new();

fn init_live_test_tracing() {
    LIVE_TRACE_INIT.call_once(|| {
        let filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("gents=debug"));
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_test_writer()
            .try_init();
    });
}

fn live_enabled() -> bool {
    std::env::var("GENTS_LIVE_SESSION_MESSAGE").as_deref() == Ok("1")
}

fn backgrounding_live_enabled() -> bool {
    std::env::var("GENTS_LIVE_BACKGROUNDING").as_deref() == Ok("1")
}

fn completion_marker(tool_call_id: &str, tool_name: &str) -> String {
    format!(r#"<tool-completion tool_call_id="{tool_call_id}" tool_name="{tool_name}""#)
}

// ---------------------------------------------------------------------------
// Test 1: local agent_new (orchestrator + target on one node / one DID)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live: set GENTS_LIVE_SESSION_MESSAGE=1 and pass --ignored"]
async fn live_local_create_session() -> Result<()> {
    if !live_enabled() {
        tracing::info!("GENTS_LIVE_SESSION_MESSAGE is not 1; skipping live local agent_new");
        return Ok(());
    }

    let target = live_target();
    target.assert_reachable().await;

    let db = test_db("session-message-live-local").await;
    let identity: Arc<dyn AgentIdentity> = Arc::new(test_identity("session-message-live-local"));
    let agent_did = identity.did().to_string();
    let orchestrator_behavior_id = default_behavior_id_for_agent(&agent_did);
    let profile_id = default_inference_profile_id_for_behavior(&orchestrator_behavior_id);
    upsert_live_backend(db.node.as_ref(), &agent_did, &target).await;
    configure_behavior(
        db.node.as_ref(),
        &orchestrator_behavior_id,
        &agent_did,
        &target,
        &profile_id,
        ORCHESTRATOR_SYSTEM_PROMPT,
        None,
        true,
    )
    .await;
    configure_behavior(
        db.node.as_ref(),
        RESEARCHER_BEHAVIOR_ID,
        &agent_did,
        &target,
        &profile_id,
        "You answer the user's question concisely and factually in one short sentence.",
        Some("Researches factual questions and returns a concise factual answer."),
        false,
    )
    .await;
    authorize_session_targets(
        db.node.as_ref(),
        &agent_did,
        &orchestrator_behavior_id,
        vec![SubagentTargetDocument {
            description: Some("Researches factual questions.".to_string()),
            ..subagent_target(
                &agent_did,
                RESEARCHER_TARGET_NAME,
                agent_did.clone(),
                RESEARCHER_BEHAVIOR_ID,
            )
        }],
    )
    .await;

    let agent = boot_document_agent(&db, identity).await?;

    let request_id = "req-live-local-create-session";
    let session_id = "session-live-local-create-session";
    create_runtime_request(
        db.node.as_ref(),
        &agent_did,
        &orchestrator_behavior_id,
        request_id,
        session_id,
        "Use your research agent to find the capital of France, then tell me the answer.",
    )
    .await;

    wait_for_background_tool_call(
        &db.node,
        request_id,
        session_id,
        AGENT_NEW_TOOL_NAME,
        Duration::from_secs(180),
    )
    .await;
    let Some(caused) =
        wait_for_caused_request(db.node.as_ref(), request_id, Duration::from_secs(120)).await
    else {
        dump_session_diagnostics(db.node.as_ref(), session_id).await;
        panic!("agent_new must cause an AgentRequest linked to the orchestrator request");
    };
    tracing::info!("[live-local] caused request = {caused:?}");
    assert_eq!(
        caused.caused_by_parent_request_id.as_deref(),
        Some(request_id)
    );
    // The model may start more than one session; follow the call this
    // caused request names.
    let row = fetch_tool_call(
        &db.node,
        request_id,
        session_id,
        caused
            .caused_by_parent_tool_call_id
            .as_deref()
            .expect("a caused request names its agent_new call"),
    )
    .await
    .expect("the caused request must name an agent_new call of the orchestrator request");
    assert_eq!(caused.behavior_id, RESEARCHER_BEHAVIOR_ID);
    assert_eq!(caused.agent_did, agent_did);
    assert_eq!(caused.requester_did.as_deref(), Some(agent_did.as_str()));
    assert_eq!(caused.subagent_depth, Some(1));
    assert_ne!(
        caused.session_id, session_id,
        "agent_new must start a new session"
    );

    let caused_terminal = wait_for_request_terminal(
        db.node.as_ref(),
        &caused.request_id,
        Duration::from_secs(180),
    )
    .await;
    assert_eq!(caused_terminal, "completed");
    let caused_answer = wait_for_assistant_answer(
        db.node.as_ref(),
        &caused.request_id,
        Duration::from_secs(30),
    )
    .await;
    tracing::info!("[live-local] started session answer = {caused_answer:?}");
    assert!(
        !caused_answer.trim().is_empty(),
        "the started session must produce a non-empty assistant response"
    );
    if !caused_answer.to_lowercase().contains("paris") {
        tracing::warn!("[live-local] SOFT-WARN: answer did not contain 'Paris': {caused_answer:?}");
    }

    let settled = wait_for_tool_call_state(
        &db.node,
        request_id,
        session_id,
        &row.tool_call_id,
        "completed",
        Duration::from_secs(60),
    )
    .await;
    assert_eq!(
        settled.child_request_id.as_deref(),
        Some(caused.request_id.as_str()),
        "the run timeline must link the agent_new row to the request it caused"
    );
    let messages = load_session_messages(&db.node, request_id, session_id).await;
    let receipt = session_receipt(&messages, &caused.request_id)
        .unwrap_or_else(|| panic!("agent_new receipt missing; transcript={messages:#?}"));
    assert_eq!(receipt["session_id"], caused.session_id.as_str());
    assert_eq!(receipt["tool_call_id"], row.tool_call_id.as_str());
    assert_eq!(receipt["await_mode"], "background");
    wait_for_message_containing(
        &db.node,
        request_id,
        session_id,
        &completion_marker(&row.tool_call_id, AGENT_NEW_TOOL_NAME),
        Duration::from_secs(60),
    )
    .await;
    let wake = wait_for_background_wake(
        db.node.as_ref(),
        session_id,
        request_id,
        Duration::from_secs(60),
    )
    .await;
    let wake_state =
        wait_for_request_terminal(db.node.as_ref(), &wake.request_id, Duration::from_secs(180))
            .await;
    assert_eq!(wake_state, "completed");

    let parent_terminal =
        wait_for_request_terminal(db.node.as_ref(), request_id, Duration::from_secs(60)).await;
    assert_eq!(parent_terminal, "completed");

    agent.shutdown().await;
    Ok(())
}

// ---------------------------------------------------------------------------
// Test 2: standard background paths with real inference
// ---------------------------------------------------------------------------

/// Exercise both background-work lanes through the production owned loop:
///
/// 1. The resolved model-facing surface contains `agent_new`,
///    `agent_message` and every spawn/list/read/wait/cancel process tool.
/// 2. Fire-and-continue: the parent request completes while the session it
///    started (or the process it spawned) is still blocked; releasing it
///    settles the row and produces the completion notification and a
///    real-inference wake.
/// 3. Managed session: the model starts a session, sees its running row with
///    `list_processes`, and steers it with `agent_message` while it is busy.
/// 4. Managed process: the model spawns a blocked process, lists it, reads
///    partial output while it runs, waits for it, and reads the terminal
///    output.
///
/// Release files make the non-blocking assertions deterministic: background
/// work cannot finish until this test has observed the parent return.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live: set GENTS_LIVE_BACKGROUNDING=1 and pass --ignored"]
async fn live_standard_backgrounding_uses_real_inference() -> Result<()> {
    if !backgrounding_live_enabled() {
        return Ok(());
    }
    init_live_test_tracing();

    let target = live_target();
    assert_model_available(&target).await;

    let workspace = tempfile::tempdir().expect("backgrounding live workspace");
    let child_release = workspace.path().join("release-child");
    let tool_release = workspace.path().join("release-tool");
    let managed_child_release = workspace.path().join("release-managed-child");
    let managed_tool_release = workspace.path().join("release-managed-tool");
    let blocked_command = |started: &str, release: &Path, done: &str| {
        serde_json::json!({
            "command": format!(
                "printf {started}; printf {started} > '{}'; while [ ! -f '{}' ]; do sleep 0.2; done; printf {done}",
                started_path(release).display(),
                release.display()
            ),
            "args": [],
            "timeout_secs": 180
        })
    };
    let child_tool_args = blocked_command(
        "CHILD_BACKGROUND_STARTED",
        &child_release,
        "CHILD_BACKGROUND_DONE",
    );
    let native_tool_args = blocked_command(
        "NATIVE_BACKGROUND_STARTED",
        &tool_release,
        "NATIVE_BACKGROUND_DONE",
    );
    let managed_child_tool_args = blocked_command(
        "CHILD_MANAGED_STARTED",
        &managed_child_release,
        "CHILD_MANAGED_DONE",
    );
    let managed_native_tool_args = blocked_command(
        "NATIVE_MANAGED_STARTED",
        &managed_tool_release,
        "NATIVE_MANAGED_DONE",
    );

    let parent_system_prompt = format!(
        r#"You are the deterministic orchestrator in an integration test.

Apply these rules to the LATEST request:
- If the latest request begins RUN_BACKGROUND_AGENT:, call agent_new exactly once with agent "background-worker" and prompt exactly "RUN_CHILD_BACKGROUND_JOB". As soon as the tool returns its running receipt, do not call agent_message, list_processes, read_process, wait_process, cancel_process, or any other tool. Reply exactly PARENT_RETURNED_AGENT_BACKGROUND.
- If it is exactly RUN_BACKGROUND_TOOL, call spawn_process exactly once with tool_name "bash_unrestricted" and args exactly {native_tool_args}. As soon as the tool returns its running receipt, do not call wait_process, read_process, list_processes, cancel_process, bash_unrestricted, or any other tool. Reply exactly PARENT_RETURNED_TOOL_BACKGROUND.
- If the latest request begins MANAGE_BACKGROUND_AGENT_CREATE:, obey its explicit agent_new instruction, then reply exactly AGENT_BACKGROUND_CREATED.
- If the latest request begins MANAGE_BACKGROUND_AGENT_LIST:, obey its explicit list_processes instruction, then reply exactly AGENT_BACKGROUND_LISTED.
- If the latest request begins MANAGE_BACKGROUND_AGENT_MESSAGE:, obey its explicit agent_message instruction, then reply exactly AGENT_BACKGROUND_MESSAGED.
- If the latest request begins MANAGE_BACKGROUND_TOOL_SPAWN:, obey its explicit spawn_process instruction, then reply exactly TOOL_BACKGROUND_SPAWNED.
- If the latest request begins MANAGE_BACKGROUND_TOOL_LIST:, obey its explicit list_processes instruction, then reply exactly TOOL_BACKGROUND_LISTED.
- If the latest request begins MANAGE_BACKGROUND_TOOL_READ_RUNNING:, obey its explicit read_process instruction. After inspecting output containing NATIVE_MANAGED_STARTED with exited false, reply exactly TOOL_BACKGROUND_READ_RUNNING.
- If the latest request begins MANAGE_BACKGROUND_TOOL_WAIT:, obey its explicit wait_process instruction. After it completes, reply exactly TOOL_BACKGROUND_WAITED.
- If the latest request begins MANAGE_BACKGROUND_TOOL_READ_TERMINAL:, obey its explicit read_process instruction. After inspecting NATIVE_MANAGED_STARTED and NATIVE_MANAGED_DONE, reply exactly TOOL_BACKGROUND_REPORT NATIVE_MANAGED_STARTED NATIVE_MANAGED_DONE.
- If the latest request asks you to review pending background completion notifications, never repeat agent_new, agent_message or spawn_process. Reply exactly BACKGROUND_COMPLETION_OBSERVED.

Never call bash_unrestricted directly from this behavior."#
    );
    let child_system_prompt = format!(
        r#"You are the deterministic background worker in an integration test.
When the latest request is exactly RUN_CHILD_BACKGROUND_JOB, call bash_unrestricted exactly once with these arguments: {child_tool_args}
Wait for that foreground tool call to finish, then reply exactly CHILD_BACKGROUND_DONE. Do not call any other tool.
When the latest request is exactly RUN_MANAGED_CHILD_BACKGROUND_JOB, call bash_unrestricted exactly once with these arguments: {managed_child_tool_args}
Wait for that foreground tool call to finish, then reply exactly CHILD_MANAGED_STARTED CHILD_MANAGED_DONE. Do not call any other tool.
If you receive a message STEERING_NOTE, do not call any tool for it; append STEERING_ACK to your final reply."#
    );

    let db = test_db("backgrounding-live-standard-path").await;
    let identity: Arc<dyn AgentIdentity> =
        Arc::new(test_identity("backgrounding-live-standard-path"));
    let agent_did = identity.did().to_string();
    let orchestrator_behavior_id = default_behavior_id_for_agent(&agent_did);
    let profile_id = default_inference_profile_id_for_behavior(&orchestrator_behavior_id);
    upsert_live_backend(db.node.as_ref(), &agent_did, &target).await;
    configure_behavior(
        db.node.as_ref(),
        &orchestrator_behavior_id,
        &agent_did,
        &target,
        &profile_id,
        &parent_system_prompt,
        None,
        true,
    )
    .await;
    configure_behavior(
        db.node.as_ref(),
        BACKGROUND_WORKER_BEHAVIOR_ID,
        &agent_did,
        &target,
        &profile_id,
        &child_system_prompt,
        Some("Runs a deliberately blocked background integration-test job."),
        false,
    )
    .await;
    configure_standard_backgrounding_tools(
        db.node.as_ref(),
        &agent_did,
        &orchestrator_behavior_id,
        workspace.path(),
    )
    .await;

    // Runtime startup probes before resolving its runnable snapshot. This test
    // inspects the resolved surfaces before `run`, so perform the same probe
    // first rather than assuming an unobserved backend is already healthy.
    gents::backend_registry::probe_and_promote_enabled_backends(db.node.as_ref()).await;
    let loaded_agent = Gents::from_default_behavior_documents(
        db.node.clone(),
        identity,
        DocumentRuntimeOptions {
            tool_ceiling: ToolCeiling::readwrite(workspace.path()).with_command_timeout_secs(180),
            ..Default::default()
        },
    )
    .await?;
    assert_standard_backgrounding_tool_surfaces(
        &loaded_agent,
        &agent_did,
        &orchestrator_behavior_id,
    );
    let agent = boot_loaded_document_agent(&db, loaded_agent).await;

    // Lane 1: agent_new fire-and-continue.
    let agent_request_id = "req-live-standard-background-agent";
    let agent_session_id = "session-live-standard-background-agent";
    create_runtime_request(
        db.node.as_ref(),
        &agent_did,
        &orchestrator_behavior_id,
        agent_request_id,
        agent_session_id,
        "RUN_BACKGROUND_AGENT: invoke agent_new now for background-worker with prompt RUN_CHILD_BACKGROUND_JOB. Do not answer until its running receipt arrives.",
    )
    .await;

    let session_row = wait_for_background_tool_call(
        &db.node,
        agent_request_id,
        agent_session_id,
        AGENT_NEW_TOOL_NAME,
        Duration::from_secs(180),
    )
    .await;
    let parent_state =
        wait_for_request_terminal(db.node.as_ref(), agent_request_id, Duration::from_secs(180))
            .await;
    assert_eq!(parent_state, "completed");
    let parent_answer =
        wait_for_assistant_answer(db.node.as_ref(), agent_request_id, Duration::from_secs(30))
            .await;
    assert!(
        parent_answer.contains("PARENT_RETURNED_AGENT_BACKGROUND"),
        "parent did not acknowledge the agent_new receipt: {parent_answer:?}"
    );
    let caused =
        wait_for_caused_request(db.node.as_ref(), agent_request_id, Duration::from_secs(60))
            .await
            .expect("agent_new must cause a request");
    assert_eq!(caused.behavior_id, BACKGROUND_WORKER_BEHAVIOR_ID);
    assert_eq!(
        caused.caused_by_parent_tool_call_id.as_deref(),
        Some(session_row.tool_call_id.as_str())
    );
    let caused_state = caused
        .lifecycle_state
        .as_ref()
        .expect("caused request lifecycle")
        .as_str()
        .to_owned();
    assert!(
        !is_terminal(&caused_state),
        "parent blocked on the started session; it was already {caused_state}"
    );
    assert!(
        fetch_runtime_snapshot(db.node.as_ref(), &agent_did)
            .await
            .is_some_and(|snapshot| snapshot.process_state == "ready"),
        "runtime must remain ready while the started session runs; caused={caused:?}"
    );
    let running_row = fetch_tool_call(
        &db.node,
        agent_request_id,
        agent_session_id,
        &session_row.tool_call_id,
    )
    .await
    .expect("agent_new row after parent completion");
    assert_eq!(
        running_row.lifecycle_state, "running",
        "the agent_new row must stay running until its caused request terminalizes"
    );
    let messages = load_session_messages(&db.node, agent_request_id, agent_session_id).await;
    let receipt = session_receipt(&messages, &caused.request_id)
        .unwrap_or_else(|| panic!("agent_new receipt missing; transcript={messages:#?}"));
    assert_eq!(receipt["status"], "running");
    assert_eq!(receipt["session_id"], caused.session_id.as_str());
    assert_no_tool_call(
        db.node.as_ref(),
        agent_session_id,
        &[
            AGENT_MESSAGE_TOOL_NAME,
            "wait_process",
            "read_process",
            "cancel_process",
        ],
    )
    .await;
    assert_min_completed_inference_calls(db.node.as_ref(), agent_request_id, 2).await;

    std::fs::write(&child_release, b"release").expect("release started session");
    let caused_terminal = wait_for_request_terminal(
        db.node.as_ref(),
        &caused.request_id,
        Duration::from_secs(180),
    )
    .await;
    assert_eq!(caused_terminal, "completed");
    assert_min_completed_inference_calls(db.node.as_ref(), &caused.request_id, 2).await;
    let caused_answer = terminal_assistant_answer(db.node.as_ref(), &caused.request_id).await;
    assert!(
        caused_answer.contains("CHILD_BACKGROUND_DONE"),
        "completed session lacks its selected canonical terminal output: {caused_answer:?}"
    );
    wait_for_tool_call_state(
        &db.node,
        agent_request_id,
        agent_session_id,
        &session_row.tool_call_id,
        "completed",
        Duration::from_secs(60),
    )
    .await;
    wait_for_message_containing(
        &db.node,
        agent_request_id,
        agent_session_id,
        &completion_marker(&session_row.tool_call_id, AGENT_NEW_TOOL_NAME),
        Duration::from_secs(60),
    )
    .await;
    assert_wake_observed(db.node.as_ref(), agent_session_id, agent_request_id).await;

    // Lane 2: spawn_process fire-and-continue.
    let tool_request_id = "req-live-standard-background-tool";
    let tool_session_id = "session-live-standard-background-tool";
    create_runtime_request(
        db.node.as_ref(),
        &agent_did,
        &orchestrator_behavior_id,
        tool_request_id,
        tool_session_id,
        "RUN_BACKGROUND_TOOL",
    )
    .await;

    let background_tool = wait_for_background_tool_call(
        &db.node,
        tool_request_id,
        tool_session_id,
        "bash_unrestricted",
        Duration::from_secs(180),
    )
    .await;
    assert!(
        background_tool.child_request_id.is_none(),
        "native background tool must use the childless lane"
    );
    let persisted_tool_args: serde_json::Value =
        serde_json::from_str(&background_tool.args).expect("valid native background args");
    assert_eq!(
        persisted_tool_args["command"], native_tool_args["command"],
        "the live model did not invoke the deterministic long-running command"
    );

    let tool_parent_state =
        wait_for_request_terminal(db.node.as_ref(), tool_request_id, Duration::from_secs(180))
            .await;
    assert_eq!(tool_parent_state, "completed");
    let tool_parent_answer =
        wait_for_assistant_answer(db.node.as_ref(), tool_request_id, Duration::from_secs(30)).await;
    assert!(
        tool_parent_answer.contains("PARENT_RETURNED_TOOL_BACKGROUND"),
        "parent did not acknowledge the background process receipt: {tool_parent_answer:?}"
    );
    let still_running = fetch_tool_call(
        &db.node,
        tool_request_id,
        tool_session_id,
        &background_tool.tool_call_id,
    )
    .await
    .expect("background tool after parent completion");
    assert_eq!(
        still_running.lifecycle_state, "running",
        "parent blocked on the native background tool"
    );
    assert_no_tool_call(
        db.node.as_ref(),
        tool_session_id,
        &["wait_process", "cancel_process"],
    )
    .await;
    assert_min_completed_inference_calls(db.node.as_ref(), tool_request_id, 2).await;

    std::fs::write(&tool_release, b"release").expect("release native background tool");
    let completed_tool = wait_for_tool_call_state(
        &db.node,
        tool_request_id,
        tool_session_id,
        &background_tool.tool_call_id,
        "completed",
        Duration::from_secs(60),
    )
    .await;
    let tool_result = completed_tool.result.as_deref().unwrap_or_default();
    assert!(
        tool_result.contains("NATIVE_BACKGROUND_STARTED")
            && tool_result.contains("NATIVE_BACKGROUND_DONE"),
        "native background result was not durably persisted: {tool_result:?}"
    );
    wait_for_message_containing(
        &db.node,
        tool_request_id,
        tool_session_id,
        &format!(
            r#"<tool-completion tool_call_id="{}""#,
            background_tool.tool_call_id
        ),
        Duration::from_secs(60),
    )
    .await;
    assert_wake_observed(db.node.as_ref(), tool_session_id, tool_request_id).await;

    // Lane 3: a managed session. Each step is its own request so the test
    // observes the started session still blocked at every step.
    assert_not_started(&managed_child_release);
    let managed_agent_session_id = "session-live-managed-background-agent";
    let managed_create_request_id = "req-live-managed-background-agent-create";
    create_runtime_request(
        db.node.as_ref(),
        &agent_did,
        &orchestrator_behavior_id,
        managed_create_request_id,
        managed_agent_session_id,
        "MANAGE_BACKGROUND_AGENT_CREATE: Call agent_new exactly once now with agent background-worker and prompt RUN_MANAGED_CHILD_BACKGROUND_JOB. Do not call any other tool.",
    )
    .await;
    let managed_row = wait_for_background_tool_call(
        &db.node,
        managed_create_request_id,
        managed_agent_session_id,
        AGENT_NEW_TOOL_NAME,
        Duration::from_secs(180),
    )
    .await;
    assert_eq!(
        wait_for_request_terminal(
            db.node.as_ref(),
            managed_create_request_id,
            Duration::from_secs(180)
        )
        .await,
        "completed"
    );
    let managed_caused = wait_for_caused_request(
        db.node.as_ref(),
        managed_create_request_id,
        Duration::from_secs(60),
    )
    .await
    .expect("managed agent_new must cause a request");
    // The started session is busy once its shell reports it is blocking.
    wait_for_model_tool_call(
        &db.node,
        &managed_caused.request_id,
        &managed_caused.session_id,
        "bash_unrestricted",
        Duration::from_secs(180),
    )
    .await;
    wait_for_started_marker(
        &managed_child_release,
        "CHILD_MANAGED_STARTED",
        Duration::from_secs(120),
    )
    .await;

    let managed_list_request_id = "req-live-managed-background-agent-list";
    create_runtime_request(
        db.node.as_ref(),
        &agent_did,
        &orchestrator_behavior_id,
        managed_list_request_id,
        managed_agent_session_id,
        "MANAGE_BACKGROUND_AGENT_LIST: Call list_processes exactly once now. Do not call any other tool.",
    )
    .await;
    wait_for_tool_result_containing(
        &db.node,
        managed_list_request_id,
        managed_agent_session_id,
        &[managed_row.tool_call_id.as_str(), AGENT_NEW_TOOL_NAME],
        Duration::from_secs(180),
    )
    .await;
    assert_eq!(
        wait_for_request_terminal(
            db.node.as_ref(),
            managed_list_request_id,
            Duration::from_secs(180)
        )
        .await,
        "completed"
    );

    let managed_state = fetch_request_lifecycle(db.node.as_ref(), &managed_caused.request_id)
        .await
        .expect("managed caused lifecycle");
    assert!(
        !is_terminal(&managed_state),
        "the started session finished ({managed_state}) before it could be steered: the live model did not run the blocking command"
    );
    let managed_message_request_id = "req-live-managed-background-agent-message";
    let managed_message_prompt = format!(
        "MANAGE_BACKGROUND_AGENT_MESSAGE: Call agent_message exactly once now with session_id {:?} and message \"STEERING_NOTE\". Do not call any other tool.",
        managed_caused.session_id
    );
    create_runtime_request(
        db.node.as_ref(),
        &agent_did,
        &orchestrator_behavior_id,
        managed_message_request_id,
        managed_agent_session_id,
        &managed_message_prompt,
    )
    .await;
    let message_row = wait_for_background_tool_call(
        &db.node,
        managed_message_request_id,
        managed_agent_session_id,
        AGENT_MESSAGE_TOOL_NAME,
        Duration::from_secs(180),
    )
    .await;
    assert_eq!(
        wait_for_request_terminal(
            db.node.as_ref(),
            managed_message_request_id,
            Duration::from_secs(180)
        )
        .await,
        "completed"
    );
    let messages = load_session_messages(
        &db.node,
        managed_message_request_id,
        managed_agent_session_id,
    )
    .await;
    let message_receipt = session_receipts(&messages)
        .into_iter()
        .find(|receipt| receipt["tool_call_id"] == message_row.tool_call_id.as_str())
        .unwrap_or_else(|| panic!("agent_message receipt missing; transcript={messages:#?}"));
    assert_eq!(
        message_receipt["session_id"],
        managed_caused.session_id.as_str()
    );
    assert_eq!(
        message_receipt["delivery"], "steering",
        "a message to a busy started session must be delivered as steering"
    );
    assert!(
        !is_terminal(
            &fetch_request_lifecycle(db.node.as_ref(), &managed_caused.request_id)
                .await
                .expect("managed caused lifecycle")
        ),
        "agent_message must have been exercised against a live session"
    );
    assert_eq!(
        fetch_tool_call(
            &db.node,
            managed_create_request_id,
            managed_agent_session_id,
            &managed_row.tool_call_id,
        )
        .await
        .expect("managed agent_new row")
        .lifecycle_state,
        "running"
    );

    std::fs::write(&managed_child_release, b"release").expect("release managed session");
    assert_eq!(
        wait_for_request_terminal(
            db.node.as_ref(),
            &managed_caused.request_id,
            Duration::from_secs(240)
        )
        .await,
        "completed"
    );
    let managed_answer =
        terminal_assistant_answer(db.node.as_ref(), &managed_caused.request_id).await;
    assert!(
        managed_answer.contains("CHILD_MANAGED_DONE"),
        "managed session lacks its terminal output: {managed_answer:?}"
    );
    if !managed_answer.contains("STEERING_ACK") {
        tracing::warn!("[live-managed] SOFT-WARN: steering was not acknowledged in the active turn: {managed_answer:?}"
        );
    }
    wait_for_tool_call_state(
        &db.node,
        managed_create_request_id,
        managed_agent_session_id,
        &managed_row.tool_call_id,
        "completed",
        Duration::from_secs(60),
    )
    .await;
    wait_for_tool_call_settled(
        &db.node,
        managed_message_request_id,
        managed_agent_session_id,
        &message_row.tool_call_id,
        Duration::from_secs(180),
    )
    .await;
    wait_for_message_containing(
        &db.node,
        managed_create_request_id,
        managed_agent_session_id,
        &completion_marker(&managed_row.tool_call_id, AGENT_NEW_TOOL_NAME),
        Duration::from_secs(60),
    )
    .await;
    wait_for_message_containing(
        &db.node,
        managed_create_request_id,
        managed_agent_session_id,
        &completion_marker(&message_row.tool_call_id, AGENT_MESSAGE_TOOL_NAME),
        Duration::from_secs(60),
    )
    .await;
    assert_model_tool_call_count_at_least(
        &db.node,
        managed_create_request_id,
        managed_agent_session_id,
        AGENT_NEW_TOOL_NAME,
        1,
    )
    .await;
    assert_model_tool_call_count_at_least(
        &db.node,
        managed_list_request_id,
        managed_agent_session_id,
        "list_processes",
        1,
    )
    .await;
    assert_model_tool_call_count_at_least(
        &db.node,
        managed_message_request_id,
        managed_agent_session_id,
        AGENT_MESSAGE_TOOL_NAME,
        1,
    )
    .await;

    // Lane 4: a managed native background process. The release is withheld
    // until the model's read_process result contains the live STARTED marker,
    // proving it read actual output before wait_process.
    let managed_tool_spawn_prompt = format!(
        "MANAGE_BACKGROUND_TOOL_SPAWN: Call spawn_process exactly once now with tool_name bash_unrestricted and args exactly {managed_native_tool_args}. Do not call any other tool."
    );
    let managed_tool_request_id = "req-live-managed-background-tool-spawn";
    let managed_tool_session_id = "session-live-managed-background-tool";
    create_runtime_request(
        db.node.as_ref(),
        &agent_did,
        &orchestrator_behavior_id,
        managed_tool_request_id,
        managed_tool_session_id,
        &managed_tool_spawn_prompt,
    )
    .await;

    let managed_tool = wait_for_background_tool_call(
        &db.node,
        managed_tool_request_id,
        managed_tool_session_id,
        "bash_unrestricted",
        Duration::from_secs(180),
    )
    .await;
    assert_eq!(
        wait_for_request_terminal(
            db.node.as_ref(),
            managed_tool_request_id,
            Duration::from_secs(180),
        )
        .await,
        "completed"
    );
    let managed_process_handle = managed_tool.tool_call_id.clone();

    let managed_tool_list_request_id = "req-live-managed-background-tool-list";
    create_runtime_request(
        db.node.as_ref(),
        &agent_did,
        &orchestrator_behavior_id,
        managed_tool_list_request_id,
        managed_tool_session_id,
        "MANAGE_BACKGROUND_TOOL_LIST: Call list_processes exactly once now. Do not call any other tool.",
    )
    .await;
    wait_for_model_tool_call(
        &db.node,
        managed_tool_list_request_id,
        managed_tool_session_id,
        "list_processes",
        Duration::from_secs(180),
    )
    .await;
    assert_eq!(
        wait_for_request_terminal(
            db.node.as_ref(),
            managed_tool_list_request_id,
            Duration::from_secs(180)
        )
        .await,
        "completed"
    );

    let managed_tool_read_request_id = "req-live-managed-background-tool-read-running";
    let managed_tool_read_prompt = format!(
        "MANAGE_BACKGROUND_TOOL_READ_RUNNING: Call read_process exactly once now with tool_call_id {managed_process_handle:?} and offset 0. Do not call wait_process or any other tool."
    );
    create_runtime_request(
        db.node.as_ref(),
        &agent_did,
        &orchestrator_behavior_id,
        managed_tool_read_request_id,
        managed_tool_session_id,
        &managed_tool_read_prompt,
    )
    .await;
    wait_for_model_tool_call(
        &db.node,
        managed_tool_read_request_id,
        managed_tool_session_id,
        "read_process",
        Duration::from_secs(180),
    )
    .await;
    wait_for_tool_result_containing(
        &db.node,
        managed_tool_read_request_id,
        managed_tool_session_id,
        &["NATIVE_MANAGED_STARTED"],
        Duration::from_secs(60),
    )
    .await;
    assert_eq!(
        wait_for_request_terminal(
            db.node.as_ref(),
            managed_tool_read_request_id,
            Duration::from_secs(180)
        )
        .await,
        "completed"
    );

    let managed_tool_wait_request_id = "req-live-managed-background-tool-wait";
    let managed_tool_wait_prompt = format!(
        "MANAGE_BACKGROUND_TOOL_WAIT: Call wait_process exactly once now with tool_call_id {managed_process_handle:?}. Do not call any other tool."
    );
    create_runtime_request(
        db.node.as_ref(),
        &agent_did,
        &orchestrator_behavior_id,
        managed_tool_wait_request_id,
        managed_tool_session_id,
        &managed_tool_wait_prompt,
    )
    .await;
    // Observe the durably snapshotted wait call while it is blocked, then prove
    // the native process is still live before releasing it.
    wait_for_model_tool_call(
        &db.node,
        managed_tool_wait_request_id,
        managed_tool_session_id,
        "wait_process",
        Duration::from_secs(180),
    )
    .await;
    assert_eq!(
        fetch_tool_call(
            &db.node,
            managed_tool_request_id,
            managed_tool_session_id,
            &managed_tool.tool_call_id,
        )
        .await
        .expect("managed native tool during read_process")
        .lifecycle_state,
        "running",
        "read_process must observe output before the native process exits"
    );
    std::fs::write(&managed_tool_release, b"release")
        .expect("release managed native background tool");

    let managed_tool_wait_state = wait_for_request_terminal(
        db.node.as_ref(),
        managed_tool_wait_request_id,
        Duration::from_secs(240),
    )
    .await;
    assert_eq!(managed_tool_wait_state, "completed");

    let managed_tool_terminal_read_request_id = "req-live-managed-background-tool-read-terminal";
    let managed_tool_terminal_read_prompt = format!(
        "MANAGE_BACKGROUND_TOOL_READ_TERMINAL: Call read_process exactly once now with tool_call_id {managed_process_handle:?} and offset 0. Do not call any other tool."
    );
    create_runtime_request(
        db.node.as_ref(),
        &agent_did,
        &orchestrator_behavior_id,
        managed_tool_terminal_read_request_id,
        managed_tool_session_id,
        &managed_tool_terminal_read_prompt,
    )
    .await;
    wait_for_model_tool_call(
        &db.node,
        managed_tool_terminal_read_request_id,
        managed_tool_session_id,
        "read_process",
        Duration::from_secs(180),
    )
    .await;
    wait_for_tool_result_containing(
        &db.node,
        managed_tool_terminal_read_request_id,
        managed_tool_session_id,
        &["NATIVE_MANAGED_DONE"],
        Duration::from_secs(60),
    )
    .await;
    let managed_tool_state = wait_for_request_terminal(
        db.node.as_ref(),
        managed_tool_terminal_read_request_id,
        Duration::from_secs(180),
    )
    .await;
    assert_eq!(managed_tool_state, "completed");
    let managed_tool_answer = wait_for_assistant_answer(
        db.node.as_ref(),
        managed_tool_terminal_read_request_id,
        Duration::from_secs(30),
    )
    .await;
    assert!(
        managed_tool_answer
            .contains("TOOL_BACKGROUND_REPORT NATIVE_MANAGED_STARTED NATIVE_MANAGED_DONE"),
        "model did not report the inspected native background result: {managed_tool_answer:?}"
    );
    for (request_id, tool_name) in [
        (managed_tool_request_id, "spawn_process"),
        (managed_tool_list_request_id, "list_processes"),
        (managed_tool_read_request_id, "read_process"),
        (managed_tool_terminal_read_request_id, "read_process"),
        (managed_tool_wait_request_id, "wait_process"),
    ] {
        assert_model_tool_call_count_at_least(
            &db.node,
            request_id,
            managed_tool_session_id,
            tool_name,
            1,
        )
        .await;
    }

    agent.shutdown().await;
    Ok(())
}

// ---------------------------------------------------------------------------
// Test 3: cross-node agent_new (orchestrator on A -> agent on B)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live: set GENTS_LIVE_SESSION_MESSAGE=1 and pass --ignored"]
async fn live_cross_node_create_session() -> Result<()> {
    if !live_enabled() {
        tracing::info!("GENTS_LIVE_SESSION_MESSAGE is not 1; skipping live cross-node agent_new");
        return Ok(());
    }

    let target = live_target();
    target.assert_reachable().await;

    let db_a = test_p2p_db("session-message-live-a").await;
    let db_b = test_p2p_db("session-message-live-b").await;
    let identity_a: Arc<dyn AgentIdentity> = db_a.node_identity.clone();
    let identity_b: Arc<dyn AgentIdentity> = db_b.node_identity.clone();
    let did_a = identity_a.did().to_string();
    let did_b = identity_b.did().to_string();
    let orchestrator_behavior_id = default_behavior_id_for_agent(&did_a);

    // Node B hosts the fast-worker behavior owned by DID-B.
    let profile_b =
        default_inference_profile_id_for_behavior(&default_behavior_id_for_agent(&did_b));
    upsert_live_backend(db_b.node.as_ref(), &did_b, &target).await;
    configure_behavior(
        db_b.node.as_ref(),
        FAST_WORKER_BEHAVIOR_ID,
        &did_b,
        &target,
        &profile_b,
        "You answer the user's factual question in one short sentence. Do not call any tool.",
        Some("Answers factual questions."),
        true,
    )
    .await;

    // Node A hosts the orchestrator owned by DID-A. Its allowlist names the
    // (DID-B, fast-worker) pair; B's behavior is not mirrored onto A.
    let profile_a = default_inference_profile_id_for_behavior(&orchestrator_behavior_id);
    upsert_live_backend(db_a.node.as_ref(), &did_a, &target).await;
    configure_behavior(
        db_a.node.as_ref(),
        &orchestrator_behavior_id,
        &did_a,
        &target,
        &profile_a,
        CROSS_NODE_ORCHESTRATOR_SYSTEM_PROMPT,
        None,
        true,
    )
    .await;
    authorize_session_targets(
        db_a.node.as_ref(),
        &did_a,
        &orchestrator_behavior_id,
        vec![SubagentTargetDocument {
            description: Some("Answers factual questions on another node.".to_string()),
            ..subagent_target(
                &did_a,
                FAST_WORKER_TARGET_NAME,
                did_b.clone(),
                FAST_WORKER_BEHAVIOR_ID,
            )
        }],
    )
    .await;

    let agent_b = boot_document_agent(&db_b, identity_b.clone()).await?;
    let agent_a = boot_document_agent(&db_a, identity_a.clone()).await?;

    // Each node enrolls the other's principal. B's enrollment is the Peer
    // admission authority for requests DID-A authors for DID-B; both are the
    // transport gate for the data-plane routes below.
    let (peer_a, addr_a) = wait_for_peer_identity(db_a.node.as_ref()).await;
    let (peer_b, addr_b) = wait_for_peer_identity(db_b.node.as_ref()).await;
    authorize_enrollment_peer(
        db_a.node.clone(),
        CROSS_NODE_NETWORK_ID,
        CROSS_NODE_NETWORK_NAME,
        identity_a.clone(),
        identity_b.clone(),
        &peer_b,
        &addr_b,
    )
    .await;
    authorize_enrollment_peer(
        db_b.node.clone(),
        CROSS_NODE_NETWORK_ID,
        CROSS_NODE_NETWORK_NAME,
        identity_b.clone(),
        identity_a.clone(),
        &peer_a,
        &addr_a,
    )
    .await;
    write_data_plane_pairing(
        db_a.node.as_ref(),
        &peer_b,
        &did_a,
        &addr_b,
        SUBAGENT_COORDINATOR_TEMPLATE,
    )
    .await;
    write_data_plane_pairing(
        db_b.node.as_ref(),
        &peer_a,
        &did_b,
        &addr_a,
        SUBAGENT_HOST_TEMPLATE,
    )
    .await;
    wait_for_pairing_applied(
        db_a.node.as_ref(),
        &peer_b,
        "AgentRequest",
        Duration::from_secs(120),
    )
    .await;
    wait_for_pairing_applied(
        db_b.node.as_ref(),
        &peer_a,
        "AgentOutputSegment",
        Duration::from_secs(120),
    )
    .await;

    let request_id = "req-live-cross-node";
    let session_id = "session-live-cross-node";
    create_runtime_request(
        db_a.node.as_ref(),
        &did_a,
        &orchestrator_behavior_id,
        request_id,
        session_id,
        "Run the remote research workflow for the capital of France.",
    )
    .await;

    let row = wait_for_background_tool_call(
        &db_a.node,
        request_id,
        session_id,
        AGENT_NEW_TOOL_NAME,
        Duration::from_secs(180),
    )
    .await;
    let caused_a = wait_for_caused_request(db_a.node.as_ref(), request_id, Duration::from_secs(60))
        .await
        .expect("agent_new on A must author the caused request");
    tracing::info!("[live-cross] caused request on A = {caused_a:?}");
    assert_eq!(caused_a.agent_did, did_b);
    assert_eq!(caused_a.requester_did.as_deref(), Some(did_a.as_str()));
    assert_eq!(caused_a.behavior_id, FAST_WORKER_BEHAVIOR_ID);
    assert_eq!(caused_a.admission_kind.as_deref(), Some("peer"));
    assert_eq!(
        caused_a.caused_by_parent_tool_call_id.as_deref(),
        Some(row.tool_call_id.as_str())
    );

    let caused_b = wait_for_request_on_node(
        db_b.node.as_ref(),
        &caused_a.request_id,
        Duration::from_secs(120),
    )
    .await
    .unwrap_or_else(|| {
        panic!(
            "caused request {} must replicate to node B",
            caused_a.request_id
        )
    });
    tracing::info!("[live-cross] caused request on B = {caused_b:?}");
    assert_eq!(caused_b.agent_did, did_b);
    assert_eq!(caused_b.requester_did.as_deref(), Some(did_a.as_str()));
    assert_eq!(caused_b.admission_kind.as_deref(), Some("peer"));
    assert_eq!(
        caused_b.caused_by_parent_request_id.as_deref(),
        Some(request_id)
    );

    let terminal_b = wait_for_request_terminal(
        db_b.node.as_ref(),
        &caused_a.request_id,
        Duration::from_secs(180),
    )
    .await;
    assert_eq!(
        terminal_b, "completed",
        "B must admit and run the Peer request"
    );
    let answer_b = wait_for_assistant_answer(
        db_b.node.as_ref(),
        &caused_a.request_id,
        Duration::from_secs(30),
    )
    .await;
    tracing::info!("[live-cross] answer on B = {answer_b:?}");
    assert!(
        !answer_b.trim().is_empty(),
        "the started session must produce a non-empty live response on B"
    );
    if !answer_b.to_lowercase().contains("paris") {
        tracing::warn!("[live-cross] SOFT-WARN: answer did not contain 'Paris': {answer_b:?}");
    }

    let terminal_a = wait_for_request_terminal(
        db_a.node.as_ref(),
        &caused_a.request_id,
        Duration::from_secs(120),
    )
    .await;
    assert_eq!(
        terminal_a, "completed",
        "the terminal must replicate back to A"
    );
    let answer_a = wait_for_assistant_answer(
        db_a.node.as_ref(),
        &caused_a.request_id,
        Duration::from_secs(60),
    )
    .await;
    assert!(
        !answer_a.trim().is_empty(),
        "the started session's terminal output must replicate back to A"
    );
    wait_for_tool_call_state(
        &db_a.node,
        request_id,
        session_id,
        &row.tool_call_id,
        "completed",
        Duration::from_secs(60),
    )
    .await;
    wait_for_message_containing(
        &db_a.node,
        request_id,
        session_id,
        &completion_marker(&row.tool_call_id, AGENT_NEW_TOOL_NAME),
        Duration::from_secs(60),
    )
    .await;
    assert_eq!(
        wait_for_request_terminal(db_a.node.as_ref(), request_id, Duration::from_secs(60)).await,
        "completed"
    );

    // Restart both runtimes. Their reconcilers reconnect to the running P2P
    // nodes and re-project the same durable facts exactly once.
    agent_a.shutdown().await;
    agent_b.shutdown().await;
    let restarted_b = boot_document_agent(&db_b, identity_b).await?;
    let restarted_a = boot_document_agent(&db_a, identity_a).await?;
    wait_for_pairing_applied(
        db_a.node.as_ref(),
        &peer_b,
        "AgentRequest",
        Duration::from_secs(120),
    )
    .await;
    wait_for_pairing_applied(
        db_b.node.as_ref(),
        &peer_a,
        "AgentOutputSegment",
        Duration::from_secs(120),
    )
    .await;
    for (node, label) in [(db_a.node.as_ref(), "A"), (db_b.node.as_ref(), "B")] {
        let caused = fetch_caused_requests(node, request_id).await;
        assert_eq!(
            caused.len(),
            1,
            "reconnect must not duplicate the caused request on {label}: {caused:?}"
        );
    }
    assert_eq!(
        fetch_tool_call(&db_a.node, request_id, session_id, &row.tool_call_id)
            .await
            .expect("agent_new row after restart")
            .lifecycle_state,
        "completed"
    );

    restarted_a.shutdown().await;
    restarted_b.shutdown().await;
    // BootedAgent only stops Gents::run; P2P belongs to the embedded node.
    db_a.node.shutdown().await;
    db_b.node.shutdown().await;
    Ok(())
}

// ---------------------------------------------------------------------------
// Test 4: fan-out, agent_list and agent_message continuation
// ---------------------------------------------------------------------------

const ALPHA_BEHAVIOR_ID: &str = "live-alpha";
const BETA_BEHAVIOR_ID: &str = "live-beta";
const BLOCKER_BEHAVIOR_ID: &str = "live-blocker";
const RELAY_BEHAVIOR_ID: &str = "live-relay";

/// A code word only the worker's own system prompt holds. Its presence in
/// the parent's answer proves the worker's result reached the parent.
fn code_word(prefix: &str) -> String {
    format!(
        "{prefix}-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..8].to_uppercase()
    )
}

fn code_worker_prompt(name: &str, first: &str, second: &str) -> String {
    format!(
        "You are agent {name}. Your code word is {first}. Your second code word is {second}. \
When asked for your code word, reply with only {first}. When asked for your second code word, \
reply with only {second}. Never call any tool."
    )
}

const DELEGATING_ORCHESTRATOR_SYSTEM_PROMPT: &str = "You are an orchestrator in an integration \
test. Follow the latest user instruction exactly, calling only the tools it names. Never answer \
a code-word question yourself. When background completion notifications arrive, do not call any \
tool: reply with one short sentence that repeats, verbatim, every code word reported in all the \
notifications you have received so far in this conversation.";

/// One parent fans out to two agents, both results return as notifications
/// and wake the parent, which combines them. The parent then lists the
/// sessions it started and continues one of them with `agent_message`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live: set GENTS_LIVE_SESSION_MESSAGE=1 and pass --ignored"]
async fn live_fan_out_list_and_continue() -> Result<()> {
    if !live_enabled() {
        return Ok(());
    }
    init_live_test_tracing();
    let target = live_target();
    assert_model_available(&target).await;

    let alpha_first = code_word("ALPHA");
    let alpha_second = code_word("ALPHATWO");
    let beta_first = code_word("BETA");

    let db = test_db("session-message-live-fan-out").await;
    let identity: Arc<dyn AgentIdentity> = Arc::new(test_identity("session-message-live-fan-out"));
    let agent_did = identity.did().to_string();
    let orchestrator_behavior_id = default_behavior_id_for_agent(&agent_did);
    let profile_id = default_inference_profile_id_for_behavior(&orchestrator_behavior_id);
    upsert_live_backend(db.node.as_ref(), &agent_did, &target).await;
    configure_behavior(
        db.node.as_ref(),
        &orchestrator_behavior_id,
        &agent_did,
        &target,
        &profile_id,
        DELEGATING_ORCHESTRATOR_SYSTEM_PROMPT,
        None,
        true,
    )
    .await;
    for (behavior_id, name, first, second) in [
        (ALPHA_BEHAVIOR_ID, "alpha", &alpha_first, &alpha_second),
        (BETA_BEHAVIOR_ID, "beta", &beta_first, &code_word("BETATWO")),
    ] {
        configure_behavior(
            db.node.as_ref(),
            behavior_id,
            &agent_did,
            &target,
            &profile_id,
            &code_worker_prompt(name, first, second),
            Some("Knows a code word."),
            false,
        )
        .await;
    }
    authorize_session_targets(
        db.node.as_ref(),
        &agent_did,
        &orchestrator_behavior_id,
        vec![
            subagent_target(&agent_did, "alpha", agent_did.clone(), ALPHA_BEHAVIOR_ID),
            subagent_target(&agent_did, "beta", agent_did.clone(), BETA_BEHAVIOR_ID),
        ],
    )
    .await;
    let agent = boot_document_agent(&db, identity).await?;

    let session_id = "session-live-fan-out";
    let fan_out_request_id = "req-live-fan-out";
    create_runtime_request(
        db.node.as_ref(),
        &agent_did,
        &orchestrator_behavior_id,
        fan_out_request_id,
        session_id,
        "Call agent_new twice in this turn: once with agent \"alpha\" and prompt \"What is your code word?\", and once with agent \"beta\" and prompt \"What is your code word?\". After both running receipts arrive, reply exactly STARTED_BOTH and call no other tool.",
    )
    .await;
    assert_eq!(
        wait_for_request_terminal(
            db.node.as_ref(),
            fan_out_request_id,
            Duration::from_secs(240)
        )
        .await,
        "completed"
    );
    let caused = wait_for_caused_requests(
        db.node.as_ref(),
        fan_out_request_id,
        2,
        Duration::from_secs(60),
    )
    .await;
    let rows = session_tool_rows(db.node.as_ref(), session_id, AGENT_NEW_TOOL_NAME).await;
    assert_eq!(
        rows.len(),
        2,
        "fan-out must start exactly two sessions, one per agent_new call: {rows:?}"
    );
    assert_eq!(
        caused.len(),
        2,
        "each agent_new call must cause exactly one request: {caused:?}"
    );
    let alpha = caused
        .iter()
        .find(|row| row.behavior_id == ALPHA_BEHAVIOR_ID)
        .unwrap_or_else(|| panic!("no alpha session was started; caused={caused:?}"))
        .clone();
    let beta = caused
        .iter()
        .find(|row| row.behavior_id == BETA_BEHAVIOR_ID)
        .unwrap_or_else(|| panic!("no beta session was started; caused={caused:?}"))
        .clone();
    for started in [&alpha, &beta] {
        assert_eq!(started.subagent_depth, Some(1));
        assert_eq!(started.admission_kind.as_deref(), Some("local-self"));
        assert_ne!(started.session_id, session_id);
        let row = rows
            .iter()
            .find(|row| {
                Some(row.tool_call_id.as_str()) == started.caused_by_parent_tool_call_id.as_deref()
            })
            .unwrap_or_else(|| panic!("caused request names no agent_new row: {started:?}"));
        assert_eq!(row.await_mode.as_deref(), Some("background"));
    }
    assert_ne!(alpha.session_id, beta.session_id);
    for started in [&alpha, &beta] {
        assert_eq!(
            wait_for_request_terminal(
                db.node.as_ref(),
                &started.request_id,
                Duration::from_secs(240)
            )
            .await,
            "completed"
        );
        wait_for_message_containing(
            &db.node,
            fan_out_request_id,
            session_id,
            &completion_marker(
                started.caused_by_parent_tool_call_id.as_deref().unwrap(),
                AGENT_NEW_TOOL_NAME,
            ),
            Duration::from_secs(60),
        )
        .await;
    }
    let combined = wait_for_session_answer_containing(
        db.node.as_ref(),
        session_id,
        &[&alpha_first, &beta_first],
        Duration::from_secs(300),
    )
    .await;
    let combined_row = session_requests(db.node.as_ref(), session_id)
        .await
        .into_iter()
        .find(|row| row.request_id == combined)
        .expect("combining request row");
    assert!(
        combined_row.is_background_completion_wake(),
        "the combined answer must come from a completion wake, not the spawning turn: {combined_row:?}"
    );
    tracing::info!(
        parent_session = session_id,
        alpha_session = %alpha.session_id,
        beta_session = %beta.session_id,
        combining_wake = %combined,
        "[live-fan-out] both results reached the parent and were combined"
    );
    wait_for_session_quiescent(db.node.as_ref(), session_id, Duration::from_secs(240)).await;

    // agent_list reports both started sessions and their relationship.
    let list_request_id = "req-live-fan-out-list";
    create_runtime_request(
        db.node.as_ref(),
        &agent_did,
        &orchestrator_behavior_id,
        list_request_id,
        session_id,
        "Call agent_list exactly once now, then reply exactly LISTED and call no other tool.",
    )
    .await;
    let listed = wait_for_json_tool_result(
        &db.node,
        list_request_id,
        session_id,
        |value| value.get("sessions").is_some(),
        Duration::from_secs(240),
    )
    .await;
    for started in [&alpha, &beta] {
        let entry = listed["sessions"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|entry| entry["session_id"] == started.session_id.as_str())
            .unwrap_or_else(|| {
                panic!(
                    "agent_list omitted started session {}: {listed}",
                    started.session_id
                )
            });
        assert_eq!(entry["relationship"], "started_by_you");
        assert_eq!(entry["agent_did"], agent_did.as_str());
        assert_eq!(entry["can_message"], true);
        assert_eq!(entry["can_interrupt"], true);
        assert_eq!(entry["status"], "idle");
    }
    let listed_agents = listed["agents"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| entry["agent"].as_str())
        .collect::<HashSet<_>>();
    assert_eq!(listed_agents, HashSet::from(["alpha", "beta"]));
    assert_eq!(
        wait_for_request_terminal(db.node.as_ref(), list_request_id, Duration::from_secs(180))
            .await,
        "completed"
    );
    wait_for_session_quiescent(db.node.as_ref(), session_id, Duration::from_secs(120)).await;

    // agent_message continues the idle alpha session with a new request.
    let message_request_id = "req-live-fan-out-message";
    create_runtime_request(
        db.node.as_ref(),
        &agent_did,
        &orchestrator_behavior_id,
        message_request_id,
        session_id,
        &format!(
            "Call agent_message exactly once now with session_id {:?} and message \"What is your second code word?\". After its receipt arrives, reply exactly MESSAGED and call no other tool.",
            alpha.session_id
        ),
    )
    .await;
    let message_row = wait_for_background_tool_call(
        &db.node,
        message_request_id,
        session_id,
        AGENT_MESSAGE_TOOL_NAME,
        Duration::from_secs(240),
    )
    .await;
    let continued = wait_for_caused_request(
        db.node.as_ref(),
        message_request_id,
        Duration::from_secs(120),
    )
    .await
    .expect("agent_message must cause a request in the idle session");
    assert_eq!(continued.session_id, alpha.session_id);
    assert_eq!(continued.behavior_id, ALPHA_BEHAVIOR_ID);
    assert_eq!(
        continued.caused_by_parent_tool_call_id.as_deref(),
        Some(message_row.tool_call_id.as_str())
    );
    let messages = load_session_messages(&db.node, message_request_id, session_id).await;
    let receipt = session_receipt(&messages, &continued.request_id)
        .unwrap_or_else(|| panic!("agent_message receipt missing; transcript={messages:#?}"));
    assert_eq!(receipt["delivery"], "request");
    assert_eq!(
        wait_for_request_terminal(
            db.node.as_ref(),
            &continued.request_id,
            Duration::from_secs(240)
        )
        .await,
        "completed"
    );
    wait_for_message_containing(
        &db.node,
        message_request_id,
        session_id,
        &completion_marker(&message_row.tool_call_id, AGENT_MESSAGE_TOOL_NAME),
        Duration::from_secs(60),
    )
    .await;
    let continued_wake = wait_for_completion_wake(
        db.node.as_ref(),
        session_id,
        message_request_id,
        Duration::from_secs(300),
    )
    .await;
    assert_eq!(
        continued_wake.lifecycle_state.as_deref(),
        Some("completed"),
        "the wake for the agent_message completion must run: {continued_wake:?}"
    );
    let continued_answer =
        terminal_assistant_answer(db.node.as_ref(), &continued_wake.request_id).await;
    assert!(
        continued_answer.contains(&alpha_second),
        "the wake for the agent_message completion must use the continued session's result"
    );
    tracing::info!(
        continued_request = %continued.request_id,
        message_tool_call = %message_row.tool_call_id,
        wake = %continued_wake.request_id,
        "[live-fan-out] agent_message continued the idle session and its result reached the parent"
    );

    agent.shutdown().await;
    Ok(())
}

// ---------------------------------------------------------------------------
// Test 5: agent_interrupt is spawner-only
// ---------------------------------------------------------------------------

/// A session that did not start the busy worker is refused; the session
/// that started it interrupts its turn, and the interrupted result still
/// reaches the starting session as a notification.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live: set GENTS_LIVE_SESSION_MESSAGE=1 and pass --ignored"]
async fn live_agent_interrupt_is_spawner_only() -> Result<()> {
    if !live_enabled() {
        return Ok(());
    }
    init_live_test_tracing();
    let target = live_target();
    assert_model_available(&target).await;

    let workspace = tempfile::tempdir().expect("interrupt live workspace");
    let release = workspace.path().join("release-blocker");
    let blocked_args = serde_json::json!({
        "command": format!(
            "printf BLOCKER_STARTED; printf BLOCKER_STARTED > '{}'; while [ ! -f '{}' ]; do sleep 0.2; done; printf BLOCKER_DONE",
            started_path(&release).display(),
            release.display()
        ),
        "args": [],
        "timeout_secs": 600
    });

    let db = test_db("session-message-live-interrupt").await;
    let identity: Arc<dyn AgentIdentity> =
        Arc::new(test_identity("session-message-live-interrupt"));
    let agent_did = identity.did().to_string();
    let orchestrator_behavior_id = default_behavior_id_for_agent(&agent_did);
    let profile_id = default_inference_profile_id_for_behavior(&orchestrator_behavior_id);
    upsert_live_backend(db.node.as_ref(), &agent_did, &target).await;
    configure_behavior(
        db.node.as_ref(),
        &orchestrator_behavior_id,
        &agent_did,
        &target,
        &profile_id,
        DELEGATING_ORCHESTRATOR_SYSTEM_PROMPT,
        None,
        true,
    )
    .await;
    configure_behavior(
        db.node.as_ref(),
        BLOCKER_BEHAVIOR_ID,
        &agent_did,
        &target,
        &profile_id,
        &format!(
            "You are a worker in an integration test. When asked to run the blocked job, call \
bash_unrestricted exactly once with these arguments: {blocked_args}. Wait for it to finish, then \
reply exactly BLOCKED_JOB_DONE. Do not call any other tool."
        ),
        Some("Runs a blocked job."),
        false,
    )
    .await;
    authorize_session_targets(
        db.node.as_ref(),
        &agent_did,
        &orchestrator_behavior_id,
        vec![subagent_target(
            &agent_did,
            "blocker",
            agent_did.clone(),
            BLOCKER_BEHAVIOR_ID,
        )],
    )
    .await;
    configure_behavior_tools(
        db.node.as_ref(),
        &agent_did,
        BLOCKER_BEHAVIOR_ID,
        None,
        Tools {
            tools_id: format!("{BLOCKER_BEHAVIOR_ID}-bash-tools"),
            agent_did: agent_did.clone(),
            host: Some(HostTools {
                root: Some(workspace.path().display().to_string()),
                bash: Some(BashTools {
                    mode: BashMode::Unrestricted,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        },
        Vec::new(),
    )
    .await;
    let loaded = Gents::from_default_behavior_documents(
        db.node.clone(),
        identity,
        DocumentRuntimeOptions {
            tool_ceiling: ToolCeiling::readwrite(workspace.path()).with_command_timeout_secs(600),
            ..Default::default()
        },
    )
    .await?;
    let agent = boot_loaded_document_agent(&db, loaded).await;

    assert_not_started(&release);
    let spawner_session = "session-live-interrupt-spawner";
    let start_request_id = "req-live-interrupt-start";
    create_runtime_request(
        db.node.as_ref(),
        &agent_did,
        &orchestrator_behavior_id,
        start_request_id,
        spawner_session,
        "Call agent_new exactly once now with agent \"blocker\" and prompt \"Run the blocked job.\". After its running receipt arrives, reply exactly BLOCKER_STARTED and call no other tool.",
    )
    .await;
    let start_row = wait_for_background_tool_call(
        &db.node,
        start_request_id,
        spawner_session,
        AGENT_NEW_TOOL_NAME,
        Duration::from_secs(240),
    )
    .await;
    let worker =
        wait_for_caused_request(db.node.as_ref(), start_request_id, Duration::from_secs(120))
            .await
            .expect("agent_new must start the blocker");
    assert_eq!(worker.behavior_id, BLOCKER_BEHAVIOR_ID);
    wait_for_model_tool_call(
        &db.node,
        &worker.request_id,
        &worker.session_id,
        "bash_unrestricted",
        Duration::from_secs(240),
    )
    .await;
    wait_for_started_marker(&release, "BLOCKER_STARTED", Duration::from_secs(120)).await;
    assert_eq!(
        wait_for_request_terminal(db.node.as_ref(), start_request_id, Duration::from_secs(240))
            .await,
        "completed"
    );

    // Another root session of the same principal did not start the worker.
    let other_session = "session-live-interrupt-other";
    let refused_request_id = "req-live-interrupt-refused";
    create_runtime_request(
        db.node.as_ref(),
        &agent_did,
        &orchestrator_behavior_id,
        refused_request_id,
        other_session,
        &format!(
            "Call agent_interrupt exactly once now with session_id {:?}. Then reply exactly INTERRUPT_ATTEMPTED and call no other tool.",
            worker.session_id
        ),
    )
    .await;
    let refusal = wait_for_json_tool_result(
        &db.node,
        refused_request_id,
        other_session,
        |value| value["tool_name"] == AGENT_INTERRUPT_TOOL_NAME,
        Duration::from_secs(240),
    )
    .await;
    assert_eq!(refusal["ok"], false, "non-spawner interrupt: {refusal}");
    assert_eq!(refusal["code"], "interrupt_not_permitted");
    assert_eq!(
        wait_for_request_terminal(
            db.node.as_ref(),
            refused_request_id,
            Duration::from_secs(180)
        )
        .await,
        "completed"
    );
    let still = fetch_request_lifecycle(db.node.as_ref(), &worker.request_id)
        .await
        .expect("worker lifecycle");
    assert!(
        !is_terminal(&still),
        "a refused interrupt must leave the worker's turn running; it is {still}"
    );

    // The starting session may interrupt.
    let interrupt_request_id = "req-live-interrupt-spawner";
    create_runtime_request(
        db.node.as_ref(),
        &agent_did,
        &orchestrator_behavior_id,
        interrupt_request_id,
        spawner_session,
        &format!(
            "Call agent_interrupt exactly once now with session_id {:?}. Then reply exactly INTERRUPT_SENT and call no other tool.",
            worker.session_id
        ),
    )
    .await;
    let accepted = wait_for_json_tool_result(
        &db.node,
        interrupt_request_id,
        spawner_session,
        |value| {
            value["session_id"] == worker.session_id.as_str()
                && matches!(value["status"].as_str(), Some("interrupting" | "idle"))
        },
        Duration::from_secs(240),
    )
    .await;
    assert_eq!(accepted["ok"], true, "spawner interrupt: {accepted}");
    assert_eq!(accepted["status"], "interrupting");
    assert_eq!(accepted["request_id"], worker.request_id.as_str());
    let worker_terminal = wait_for_request_terminal(
        db.node.as_ref(),
        &worker.request_id,
        Duration::from_secs(120),
    )
    .await;
    assert_eq!(worker_terminal, "interrupted");
    let settled = wait_for_tool_call_settled(
        &db.node,
        start_request_id,
        spawner_session,
        &start_row.tool_call_id,
        Duration::from_secs(120),
    )
    .await;
    wait_for_message_containing(
        &db.node,
        start_request_id,
        spawner_session,
        &completion_marker(&start_row.tool_call_id, AGENT_NEW_TOOL_NAME),
        Duration::from_secs(120),
    )
    .await;
    assert!(
        !release.exists(),
        "the worker must have been interrupted while still blocked"
    );
    let wake = wait_for_completion_wake(
        db.node.as_ref(),
        spawner_session,
        start_request_id,
        Duration::from_secs(240),
    )
    .await;
    assert_eq!(
        wake.lifecycle_state.as_deref(),
        Some("completed"),
        "the interrupted result must wake the spawner: {wake:?}"
    );
    tracing::info!(
        worker_session = %worker.session_id,
        worker_request = %worker.request_id,
        agent_new_row = %start_row.tool_call_id,
        row_state = %settled.lifecycle_state,
        wake = %wake.request_id,
        "[live-interrupt] non-spawner refused; spawner interrupted; notification delivered and wake completed"
    );

    std::fs::write(&release, b"release").expect("release blocker");
    wait_for_session_quiescent(db.node.as_ref(), spawner_session, Duration::from_secs(240)).await;
    agent.shutdown().await;
    Ok(())
}

// ---------------------------------------------------------------------------
// Test 6: the hop bound stops a chain
// ---------------------------------------------------------------------------

/// With `max_request_hop` 1, the started session (hop 1) may not start
/// another; its refusal is a tool result, it still completes, and its result
/// notification still reaches the root session. The root's completion wake
/// would be hop 2, so it is refused and the chain stops there.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live: set GENTS_LIVE_SESSION_MESSAGE=1 and pass --ignored"]
async fn live_hop_bound_stops_chain() -> Result<()> {
    if !live_enabled() {
        return Ok(());
    }
    init_live_test_tracing();
    let target = live_target();
    assert_model_available(&target).await;

    let db = test_db("session-message-live-hop").await;
    let identity: Arc<dyn AgentIdentity> = Arc::new(test_identity("session-message-live-hop"));
    let agent_did = identity.did().to_string();
    let orchestrator_behavior_id = default_behavior_id_for_agent(&agent_did);
    let profile_id = default_inference_profile_id_for_behavior(&orchestrator_behavior_id);
    upsert_live_backend(db.node.as_ref(), &agent_did, &target).await;
    configure_behavior(
        db.node.as_ref(),
        &orchestrator_behavior_id,
        &agent_did,
        &target,
        &profile_id,
        DELEGATING_ORCHESTRATOR_SYSTEM_PROMPT,
        None,
        true,
    )
    .await;
    configure_behavior(
        db.node.as_ref(),
        RELAY_BEHAVIOR_ID,
        &agent_did,
        &target,
        &profile_id,
        "You are a relay in an integration test. For any request: first call agent_list exactly \
once. Then call agent_new exactly once with agent \"relay\" and prompt \"relay onward\". Whatever \
agent_new returns, including an error, then reply exactly RELAY_DONE and call no other tool.",
        Some("Relays work onward."),
        false,
    )
    .await;
    let relay = || subagent_target(&agent_did, "relay", agent_did.clone(), RELAY_BEHAVIOR_ID);
    authorize_session_targets(
        db.node.as_ref(),
        &agent_did,
        &orchestrator_behavior_id,
        vec![relay()],
    )
    .await;
    authorize_session_targets(
        db.node.as_ref(),
        &agent_did,
        RELAY_BEHAVIOR_ID,
        vec![relay()],
    )
    .await;
    let mut principal = ensure_agent_principal(db.node.as_ref(), &agent_did)
        .await
        .expect("principal");
    principal.max_request_hop = Some(1);
    apply_fixture_documents(
        db.node.as_ref(),
        vec![(
            Collection::AgentPrincipal,
            serde_json::to_value(principal).expect("serialize principal"),
        )],
    )
    .await;
    let agent = boot_document_agent(&db, identity).await?;

    let root_session = "session-live-hop-root";
    let root_request_id = "req-live-hop-root";
    create_runtime_request(
        db.node.as_ref(),
        &agent_did,
        &orchestrator_behavior_id,
        root_request_id,
        root_session,
        "Call agent_new exactly once now with agent \"relay\" and prompt \"start the relay\". After its running receipt arrives, reply exactly RELAY_STARTED and call no other tool.",
    )
    .await;
    let root_row = wait_for_background_tool_call(
        &db.node,
        root_request_id,
        root_session,
        AGENT_NEW_TOOL_NAME,
        Duration::from_secs(240),
    )
    .await;
    let first =
        wait_for_caused_request(db.node.as_ref(), root_request_id, Duration::from_secs(120))
            .await
            .expect("the root must start the relay");
    assert_eq!(first.behavior_id, RELAY_BEHAVIOR_ID);
    assert_eq!(first.subagent_depth, Some(1));
    assert_eq!(
        wait_for_request_terminal(
            db.node.as_ref(),
            &first.request_id,
            Duration::from_secs(300)
        )
        .await,
        "completed",
        "the relay must finish its turn after its onward start is refused"
    );

    // The relay saw who started it.
    let listed = wait_for_json_tool_result(
        &db.node,
        &first.request_id,
        &first.session_id,
        |value| value.get("sessions").is_some(),
        Duration::from_secs(30),
    )
    .await;
    let started_by = listed["sessions"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|entry| entry["session_id"] == root_session)
        .unwrap_or_else(|| panic!("the relay's agent_list omitted its starter: {listed}"));
    assert_eq!(started_by["relationship"], "started_you");
    assert_eq!(started_by["can_interrupt"], false);

    // Its onward start was refused at the bound; nothing was caused.
    let refused = wait_for_json_tool_result(
        &db.node,
        &first.request_id,
        &first.session_id,
        |value| value["tool_name"] == AGENT_NEW_TOOL_NAME,
        Duration::from_secs(30),
    )
    .await;
    assert_eq!(refused["ok"], false);
    assert_eq!(refused["code"], "request_hop_exceeded");
    assert_eq!(refused["hop"], 2);
    assert_eq!(refused["max_request_hop"], 1);
    let caused_by_relay = fetch_caused_requests(db.node.as_ref(), &first.request_id).await;
    assert!(
        caused_by_relay.is_empty(),
        "no session may be started beyond the hop bound: {caused_by_relay:?}"
    );

    // The relay's result still reaches the root as a notification.
    wait_for_message_containing(
        &db.node,
        root_request_id,
        root_session,
        &completion_marker(&root_row.tool_call_id, AGENT_NEW_TOOL_NAME),
        Duration::from_secs(120),
    )
    .await;
    let settled = wait_for_tool_call_settled(
        &db.node,
        root_request_id,
        root_session,
        &root_row.tool_call_id,
        Duration::from_secs(60),
    )
    .await;
    assert_eq!(settled.lifecycle_state, "completed");
    wait_for_session_quiescent(db.node.as_ref(), root_session, Duration::from_secs(240)).await;
    let wakes = session_requests(db.node.as_ref(), root_session)
        .await
        .into_iter()
        .filter(SessionRequestRow::is_background_completion_wake)
        .collect::<Vec<_>>();
    assert!(
        !wakes.is_empty(),
        "the notification must attempt a completion wake"
    );
    for wake in &wakes {
        assert_eq!(
            wake.lifecycle_state.as_deref(),
            Some("failed"),
            "a completion wake beyond the hop bound must not run: {wake:?}"
        );
        assert!(
            wake.failure_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("max_request_hop")),
            "the over-bound wake must be refused by the hop bound: {wake:?}"
        );
    }
    tracing::info!(
        root_session,
        relay_session = %first.session_id,
        relay_request = %first.request_id,
        root_agent_new = %root_row.tool_call_id,
        wakes = ?wakes.iter().map(|wake| (&wake.request_id, &wake.lifecycle_state, wake.subagent_depth, &wake.failure_reason)).collect::<Vec<_>>(),
        "[live-hop] chain stopped at the bound; notification delivered"
    );

    agent.shutdown().await;
    Ok(())
}

// ---------------------------------------------------------------------------
// System prompts
// ---------------------------------------------------------------------------

const ORCHESTRATOR_SYSTEM_PROMPT: &str = "You are an orchestrator agent. You can start a session on \
an agent named `researcher`. For ANY research or factual lookup the user asks for, you MUST call the \
`agent_new` tool with agent exactly \"researcher\" and a `prompt` describing the question, then \
tell the user the research is under way without calling any other tool. Do not answer factual \
questions yourself. When a background completion notification arrives, relay its answer to the user \
without calling any tool.";

const CROSS_NODE_ORCHESTRATOR_SYSTEM_PROMPT: &str = "You are the root of a deterministic remote \
session test. When asked to run the remote research workflow, call `agent_new` exactly once with \
agent exactly \"fast-worker\" and prompt exactly \"What is the capital of France? Answer in one short \
sentence.\" After its running receipt arrives, reply exactly REMOTE_SESSION_STARTED and do not call any \
other tool. Do not answer the question yourself. When a background completion notification arrives, \
report its answer without calling any tool.";

// ---------------------------------------------------------------------------
// Configuration and boot helpers
// ---------------------------------------------------------------------------

async fn assert_model_available(target: &InferenceTarget) {
    let model = target.model();
    let url = format!("{}/models", target.endpoint().trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .expect("reqwest client");
    let mut request = client.get(&url);
    if let Some(key) = target.auth().resolve_api_key().expect("target credential") {
        request = request.bearer_auth(key);
    }
    let response = tokio::time::timeout(Duration::from_secs(20), request.send())
        .await
        .unwrap_or_else(|_| panic!("live endpoint {url} timed out"))
        .unwrap_or_else(|error| panic!("live endpoint {url} unreachable: {error}"));
    assert!(
        response.status().is_success(),
        "live endpoint {url} returned status {}",
        response.status()
    );
    let payload: serde_json::Value = response
        .json()
        .await
        .unwrap_or_else(|error| panic!("live endpoint {url} returned invalid model JSON: {error}"));
    let available = payload
        .get("data")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.get("id").and_then(serde_json::Value::as_str))
        .collect::<Vec<_>>();
    assert!(
        available.contains(&model),
        "requested live model {model:?} is not served by {url}; available={available:?}"
    );
}

/// Boot a full Gents from the behavior documents owned by `identity`'s DID.
async fn boot_document_agent(db: &TestDb, identity: Arc<dyn AgentIdentity>) -> Result<BootedAgent> {
    let agent = Gents::from_default_behavior_documents(
        db.node.clone(),
        identity,
        DocumentRuntimeOptions {
            tool_ceiling: ToolCeiling::meta_only(),
            ..Default::default()
        },
    )
    .await?;
    Ok(boot_loaded_document_agent(db, agent).await)
}

async fn boot_loaded_document_agent(db: &TestDb, agent: Gents) -> BootedAgent {
    let agent_did = agent.agent_did().to_string();
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let handle = tokio::spawn(agent.run(shutdown_rx));
    wait_for_runtime_ready(db.node.as_ref(), &agent_did).await;
    BootedAgent::new(shutdown_tx, handle, agent_did)
}

fn assert_standard_backgrounding_tool_surfaces(
    agent: &Gents,
    agent_did: &str,
    parent_behavior_id: &str,
) {
    let active_behavior_ids = agent
        .behaviors()
        .iter()
        .map(|behavior| behavior.behavior_id.clone())
        .collect::<HashSet<_>>();
    let parent = agent
        .behaviors()
        .iter()
        .find(|behavior| behavior.behavior_id == parent_behavior_id)
        .unwrap_or_else(|| {
            panic!(
                "loaded orchestrator behavior {parent_behavior_id}; active behaviors: {active_behavior_ids:?}; unavailable: {:?}",
                agent.unavailable_behaviors()
            )
        });
    let parent_surface = parent
        .tools
        .explain_with_runtime(false, agent_did, &active_behavior_ids);
    for required in [
        "bash_unrestricted",
        AGENT_NEW_TOOL_NAME,
        AGENT_MESSAGE_TOOL_NAME,
        "spawn_process",
        "list_processes",
        "read_process",
        "wait_process",
        "cancel_process",
    ] {
        assert!(
            parent_surface.tool_names.iter().any(|name| name == required),
            "backgrounding-enabled behavior did not provision {required}; resolved={:?}; config={:?}",
            parent_surface.tool_names,
            parent.tools
        );
    }
    assert_eq!(
        parent_surface.included.get("subagent"),
        Some(&{
            let mut names = gents::toolset::AGENT_TOOL_NAMES
                .iter()
                .map(|name| name.to_string())
                .collect::<Vec<_>>();
            names.sort();
            names
        }),
        "enabled session targets must resolve exactly the agents tool group"
    );
    assert_eq!(
        parent_surface.included.get("background_process"),
        Some(&vec![
            "cancel_process".to_string(),
            "list_processes".to_string(),
            "read_process".to_string(),
            "spawn_process".to_string(),
            "wait_process".to_string(),
        ]),
        "native background allowlisting must resolve the complete process bundle"
    );

    let child = agent
        .behaviors()
        .iter()
        .find(|behavior| behavior.behavior_id == BACKGROUND_WORKER_BEHAVIOR_ID)
        .expect("loaded background worker behavior");
    let child_surface = child
        .tools
        .explain_with_runtime(false, agent_did, &active_behavior_ids);
    assert!(
        child_surface
            .tool_names
            .iter()
            .any(|name| name == "bash_unrestricted"),
        "background worker must receive its foreground bash tool"
    );
    for parent_only in [AGENT_NEW_TOOL_NAME, "spawn_process", "read_process"] {
        assert!(
            !child_surface
                .tool_names
                .iter()
                .any(|name| name == parent_only),
            "background worker must not inherit parent-only tool {parent_only}"
        );
    }
}

async fn upsert_live_backend(node: &EmbeddedNode, agent_did: &str, target: &InferenceTarget) {
    let backend = target.backend(agent_did);
    apply_fixture_documents(
        node,
        vec![(
            Collection::InferenceBackend,
            serde_json::to_value(backend).expect("serialize live backend"),
        )],
    )
    .await;
}

/// Upsert an `AgentBehavior` document backed by the live backend, with an
/// optional `description` (surfaced in the caller's agent list).
#[allow(clippy::too_many_arguments)]
async fn configure_behavior(
    node: &EmbeddedNode,
    behavior_id: &str,
    agent_did: &str,
    target: &InferenceTarget,
    inference_profile_id: &str,
    system_prompt: &str,
    description: Option<&str>,
    default_for_principal: bool,
) {
    let mut principal = ensure_agent_principal(node, agent_did)
        .await
        .expect("ensure live fixture principal");
    let context_id = format!("{behavior_id}:context");
    let sampling_id = format!("{behavior_id}:live-sampling");
    let profile = InferenceProfile {
        profile_id: inference_profile_id.to_string(),
        sampling_id: Some(sampling_id.clone()),
        reasoning_effort: Some(ReasoningEffort::High),
        ..target.profile(agent_did)
    };
    let sampling = InferenceSampling {
        agent_did: agent_did.to_string(),
        sampling_id,
        display_name: Some("live high-thinking sampling".to_string()),
        temperature: Some(1.0),
        top_p: Some(0.95),
        ..Default::default()
    };
    let context = AgentContext {
        context_id: context_id.clone(),
        agent_did: agent_did.to_string(),
        display_name: None,
        description: None,
        system_prompt: Some(system_prompt.to_string()),
        tools_id: None,
        compaction_id: None,
        skill_ids: Vec::new(),
        tags: Vec::new(),
    };
    let behavior = AgentBehavior {
        behavior_id: behavior_id.to_string(),
        agent_did: agent_did.to_string(),
        display_name: Some(behavior_id.to_string()),
        description: description.map(ToOwned::to_owned),
        context_id: Some(context_id),
        inference_profile_id: inference_profile_id.to_string(),
        enabled: true,
        tags: Vec::new(),
        created_at: Some("2026-06-02T00:00:00Z".to_string()),
    };
    let mut documents = vec![
        (
            Collection::InferenceSampling,
            serde_json::to_value(sampling).expect("serialize live inference sampling"),
        ),
        (
            Collection::InferenceProfile,
            serde_json::to_value(profile).expect("serialize live inference profile"),
        ),
        (
            Collection::AgentContext,
            serde_json::to_value(context).expect("serialize live agent context"),
        ),
        (
            Collection::AgentBehavior,
            serde_json::to_value(behavior).expect("serialize live behavior"),
        ),
    ];
    if default_for_principal {
        principal.default_behavior_id = Some(behavior_id.to_string());
        documents.push((
            Collection::AgentPrincipal,
            serde_json::to_value(principal).expect("serialize live principal"),
        ));
    }
    apply_fixture_documents(node, documents).await;
}

async fn apply_fixture_documents(
    node: &EmbeddedNode,
    documents: Vec<(Collection, serde_json::Value)>,
) {
    use gents::config_client::{
        apply_desired_state_plan, DesiredStateApplyDocument, DesiredStateApplyPlan,
    };

    let plan = DesiredStateApplyPlan::new(
        documents
            .into_iter()
            .map(|(collection, value)| DesiredStateApplyDocument {
                collection,
                add: value.clone(),
                update: value,
            })
            .collect(),
    )
    .expect("build live fixture plan");
    gents::ConfigAccess::transact_local(node, None, "test.live_session_message_fixture", |txn| {
        let plan = &plan;
        Box::pin(async move { apply_desired_state_plan(txn, plan).await.map(|_| ()) })
    })
    .await
    .expect("apply live fixture documents");
}

fn session_targets_group(targets: &[SubagentTargetDocument]) -> SubagentTools {
    SubagentTools {
        target_ids: targets
            .iter()
            .map(|target| target.target_id.clone())
            .collect(),
        enabled: Some(true),
    }
}

fn target_documents(targets: Vec<SubagentTargetDocument>) -> Vec<(Collection, serde_json::Value)> {
    targets
        .into_iter()
        .map(|target| {
            (
                Collection::SubagentTarget,
                serde_json::to_value(target).expect("serialize session target"),
            )
        })
        .collect()
}

/// Publish canonical tools enabling `agent_new`/`agent_message` over
/// `targets` for `behavior_id`.
async fn authorize_session_targets(
    node: &EmbeddedNode,
    agent_did: &str,
    behavior_id: &str,
    targets: Vec<SubagentTargetDocument>,
) {
    configure_behavior_tools(
        node,
        agent_did,
        behavior_id,
        None,
        Tools {
            tools_id: format!("{behavior_id}-session-tools"),
            agent_did: agent_did.to_string(),
            subagents: Some(session_targets_group(&targets)),
            ..Default::default()
        },
        target_documents(targets),
    )
    .await;
}

/// Configure the parent with both background lanes and the worker with a
/// foreground bash tool used to hold its request open until the test
/// releases it.
async fn configure_standard_backgrounding_tools(
    node: &EmbeddedNode,
    agent_did: &str,
    parent_behavior_id: &str,
    workspace: &Path,
) {
    let targets = vec![SubagentTargetDocument {
        description: Some("Runs a deliberately blocked background job.".to_string()),
        ..subagent_target(
            agent_did,
            BACKGROUND_WORKER_TARGET_NAME,
            agent_did,
            BACKGROUND_WORKER_BEHAVIOR_ID,
        )
    }];
    configure_behavior_tools(
        node,
        agent_did,
        parent_behavior_id,
        None,
        Tools {
            tools_id: format!("{parent_behavior_id}-standard-background-tools"),
            agent_did: agent_did.to_string(),
            host: Some(HostTools {
                root: Some(workspace.display().to_string()),
                bash: Some(BashTools {
                    mode: BashMode::Unrestricted,
                    background_enabled: true,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            subagents: Some(session_targets_group(&targets)),
            ..Default::default()
        },
        target_documents(targets),
    )
    .await;

    configure_behavior_tools(
        node,
        agent_did,
        BACKGROUND_WORKER_BEHAVIOR_ID,
        None,
        Tools {
            tools_id: format!("{BACKGROUND_WORKER_BEHAVIOR_ID}-foreground-bash-tools"),
            agent_did: agent_did.to_string(),
            host: Some(HostTools {
                root: Some(workspace.display().to_string()),
                bash: Some(BashTools {
                    mode: BashMode::Unrestricted,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        },
        Vec::new(),
    )
    .await;
}

// ---------------------------------------------------------------------------
// Request observation
// ---------------------------------------------------------------------------

fn is_terminal(state: &str) -> bool {
    RequestLifecycleState::is_terminal_str(Some(state))
}

async fn fetch_request_lifecycle(node: &EmbeddedNode, request_id: &str) -> Option<String> {
    let escaped = escape_graphql_string(request_id);
    let query = format!(
        r#"{{
            AgentRequest(filter: {{ request_id: {{ _eq: "{escaped}" }} }}, limit: 1) {{
                lifecycle_state
            }}
        }}"#
    );
    #[derive(Deserialize)]
    struct Row {
        lifecycle_state: Option<String>,
    }
    let resp = node.execute(&query).await;
    first_optional_row::<Row>(&resp, "AgentRequest").and_then(|r| r.lifecycle_state)
}

/// A request caused by an `agent_new`/`agent_message` call, identified by
/// its `caused_by_parent_*` edge naming a tool call. Completion wakes also
/// name their parent request, but no tool call.
#[derive(Debug, Clone, Deserialize)]
struct CausedRequestRow {
    request_id: String,
    session_id: String,
    agent_did: String,
    requester_did: Option<String>,
    behavior_id: String,
    lifecycle_state: Option<RequestLifecycleState>,
    admission_kind: Option<String>,
    subagent_depth: Option<i64>,
    caused_by_parent_request_id: Option<String>,
    caused_by_parent_tool_call_id: Option<String>,
}

const CAUSED_REQUEST_FIELDS: &str = "request_id session_id agent_did requester_did behavior_id \
    lifecycle_state admission_kind subagent_depth caused_by_parent_request_id \
    caused_by_parent_tool_call_id";

fn caused_request_rows(response: &gents::defra_node::QueryResponse) -> Vec<CausedRequestRow> {
    assert!(
        !response.has_errors(),
        "query caused requests failed: {:?}",
        response.errors
    );
    response
        .data
        .as_ref()
        .and_then(|data| data.get("AgentRequest"))
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .map(|row| {
            serde_json::from_value::<CausedRequestRow>(row.clone())
                .unwrap_or_else(|error| panic!("decode caused request {row}: {error}"))
        })
        .collect()
}

async fn fetch_caused_requests(
    node: &EmbeddedNode,
    parent_request_id: &str,
) -> Vec<CausedRequestRow> {
    let escaped = escape_graphql_string(parent_request_id);
    let query = format!(
        r#"{{ AgentRequest(filter: {{ caused_by_parent_request_id: {{ _eq: "{escaped}" }}, caused_by_parent_tool_call_id: {{ _ne: null }} }}) {{ {CAUSED_REQUEST_FIELDS} }} }}"#
    );
    caused_request_rows(&node.execute(&query).await)
}

async fn wait_for_caused_request(
    node: &EmbeddedNode,
    parent_request_id: &str,
    timeout: Duration,
) -> Option<CausedRequestRow> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(caused) = fetch_caused_requests(node, parent_request_id)
            .await
            .into_iter()
            .next()
        {
            return Some(caused);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Where a blocked test command records its started marker. A running
/// foreground tool's stdout is not durable until it exits, so the command
/// also writes the marker here.
fn started_path(release: &Path) -> std::path::PathBuf {
    release.with_extension("started")
}

/// The sentinel lives in this run's own temporary workspace; it must not
/// exist before the blocked command is launched, so only that shell can
/// create it.
fn assert_not_started(release: &Path) {
    assert!(
        !started_path(release).exists(),
        "started sentinel exists before its command was launched"
    );
}

async fn wait_for_started_marker(release: &Path, marker: &str, timeout: Duration) {
    let path = started_path(release);
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if std::fs::read_to_string(&path).is_ok_and(|text| text.contains(marker)) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the blocked command never reported {marker}; it did not start blocking"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn wait_for_caused_requests(
    node: &EmbeddedNode,
    parent_request_id: &str,
    count: usize,
    timeout: Duration,
) -> Vec<CausedRequestRow> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let caused = fetch_caused_requests(node, parent_request_id).await;
        if caused.len() >= count {
            return caused;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {count} requests caused by {parent_request_id}; have {caused:?}"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[derive(Debug, Clone, Deserialize)]
struct SessionToolRow {
    tool_call_id: String,
    await_mode: Option<String>,
}

async fn session_tool_rows(
    node: &EmbeddedNode,
    session_id: &str,
    tool_name: &str,
) -> Vec<SessionToolRow> {
    let query = format!(
        r#"{{ AgentToolCall(filter: {{ session_id: {{ _eq: "{}" }}, tool_name: {{ _eq: "{}" }} }}) {{ tool_call_id await_mode }} }}"#,
        escape_graphql_string(session_id),
        escape_graphql_string(tool_name),
    );
    let response = node.execute(&query).await;
    assert!(
        !response.has_errors(),
        "query session tool rows failed: {:?}",
        response.errors
    );
    response
        .data
        .as_ref()
        .and_then(|data| data["AgentToolCall"].as_array())
        .into_iter()
        .flatten()
        .map(|row| serde_json::from_value(row.clone()).expect("decode session tool row"))
        .collect()
}

/// One request of a session, with the lineage and queue facts the live
/// assertions read.
#[derive(Debug, Clone, Deserialize)]
struct SessionRequestRow {
    request_id: String,
    lifecycle_state: Option<String>,
    subagent_depth: Option<i64>,
    failure_reason: Option<String>,
    input: Option<RequestInput>,
    caused_by_parent_request_id: Option<String>,
}

impl SessionRequestRow {
    fn is_background_completion_wake(&self) -> bool {
        self.input
            .as_ref()
            .and_then(|input| input.queue.as_ref())
            .is_some_and(|queue| queue.source == QueueSource::BackgroundCompletion)
    }
}

async fn session_requests(node: &EmbeddedNode, session_id: &str) -> Vec<SessionRequestRow> {
    let query = format!(
        r#"{{ AgentRequest(filter: {{ session_id: {{ _eq: "{}" }} }}, order: {{ created_at: ASC }}) {{ request_id lifecycle_state subagent_depth failure_reason input caused_by_parent_request_id }} }}"#,
        escape_graphql_string(session_id),
    );
    let response = node.execute(&query).await;
    assert!(
        !response.has_errors(),
        "query session requests failed: {:?}",
        response.errors
    );
    response
        .data
        .as_ref()
        .and_then(|data| data["AgentRequest"].as_array())
        .into_iter()
        .flatten()
        .map(|row| {
            serde_json::from_value(row.clone())
                .unwrap_or_else(|error| panic!("decode session request {row}: {error}"))
        })
        .collect()
}

/// The terminal completion wake caused by a background row of
/// `owning_request_id` settling: a background-completion wake whose cause
/// names that request.
async fn wait_for_completion_wake(
    node: &EmbeddedNode,
    session_id: &str,
    owning_request_id: &str,
    timeout: Duration,
) -> SessionRequestRow {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let requests = session_requests(node, session_id).await;
        let wakes = requests
            .iter()
            .filter(|row| {
                row.is_background_completion_wake()
                    && row.caused_by_parent_request_id.as_deref() == Some(owning_request_id)
            })
            .collect::<Vec<_>>();
        assert!(
            wakes.len() <= 1,
            "one completion must cause at most one wake: {wakes:?}"
        );
        if let Some(wake) = wakes
            .first()
            .filter(|row| row.lifecycle_state.as_deref().is_some_and(is_terminal))
        {
            return (*wake).clone();
        }
        if tokio::time::Instant::now() >= deadline {
            dump_session_diagnostics(node, session_id).await;
            panic!(
                "no terminal completion wake caused by {owning_request_id} in session {session_id}; requests={requests:?}"
            );
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Wait until every request of the session is terminal.
async fn wait_for_session_quiescent(node: &EmbeddedNode, session_id: &str, timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let requests = session_requests(node, session_id).await;
        if requests
            .iter()
            .all(|row| row.lifecycle_state.as_deref().is_some_and(is_terminal))
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for session {session_id} to settle; requests={requests:?}"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Wait for a completed request of the session whose terminal answer holds
/// every needle, and return its request id.
async fn wait_for_session_answer_containing(
    node: &EmbeddedNode,
    session_id: &str,
    needles: &[&str],
    timeout: Duration,
) -> String {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let requests = session_requests(node, session_id).await;
        for row in requests
            .iter()
            .filter(|row| row.lifecycle_state.as_deref() == Some("completed"))
        {
            let answer = terminal_assistant_answer(node, &row.request_id).await;
            if needles.iter().all(|needle| answer.contains(needle)) {
                return row.request_id.clone();
            }
        }
        if tokio::time::Instant::now() >= deadline {
            dump_session_diagnostics(node, session_id).await;
            panic!(
                "no completed request in session {session_id} answered with {needles:?}; requests={requests:?}"
            );
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Wait for a JSON tool result of `request_id` that satisfies `matches`.
async fn wait_for_json_tool_result(
    node: &Arc<EmbeddedNode>,
    request_id: &str,
    session_id: &str,
    matches: impl Fn(&serde_json::Value) -> bool,
    timeout: Duration,
) -> serde_json::Value {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let messages = load_session_messages(node, request_id, session_id).await;
        if let Some(found) = tool_result_texts(&messages)
            .into_iter()
            .filter_map(|text| serde_json::from_str::<serde_json::Value>(text).ok())
            .find(|value| matches(value))
        {
            return found;
        }
        if tokio::time::Instant::now() >= deadline {
            dump_session_diagnostics(node.as_ref(), session_id).await;
            panic!("timed out waiting for a matching tool result of {request_id} in session {session_id}");
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn wait_for_request_on_node(
    node: &EmbeddedNode,
    request_id: &str,
    timeout: Duration,
) -> Option<CausedRequestRow> {
    let escaped = escape_graphql_string(request_id);
    let query = format!(
        r#"{{ AgentRequest(filter: {{ request_id: {{ _eq: "{escaped}" }} }}, limit: 1) {{ {CAUSED_REQUEST_FIELDS} }} }}"#
    );
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(row) = caused_request_rows(&node.execute(&query).await)
            .into_iter()
            .next()
        {
            return Some(row);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Dump tool calls + messages for a session to stderr.
async fn dump_session_diagnostics(node: &EmbeddedNode, session_id: &str) {
    let escaped = escape_graphql_string(session_id);
    let query = format!(
        r#"{{
            AgentToolCall(filter: {{ session_id: {{ _eq: "{escaped}" }} }}, order: {{ message_sequence: ASC }}) {{
                _docID tool_name tool_call_id lifecycle_state status await_mode tool_failure_class
            }}
            AgentMessage(filter: {{ session_id: {{ _eq: "{escaped}" }} }}, order: {{ sequence: ASC }}) {{
                _docID sequence role publication outcome native_id blocks request_doc_id
            }}
        }}"#
    );
    let resp = node.execute(&query).await;
    tracing::info!(
        "[diag] session {session_id}: {}",
        serde_json::to_string_pretty(&resp.data.unwrap_or_default()).unwrap_or_default()
    );
}

// ---------------------------------------------------------------------------
// Tool-call rows and transcript observation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct ToolCallRow {
    tool_call_id: String,
    lifecycle_state: String,
    args: String,
    result: Option<String>,
    child_request_id: Option<String>,
}

async fn timeline_tools(
    node: &Arc<EmbeddedNode>,
    request_id: &str,
) -> Vec<gents::TimelineToolCallRow> {
    load_run_timeline_rows(&ConfigAccess::Local(node.clone()), request_id)
        .await
        .unwrap_or_else(|error| panic!("load canonical timeline for {request_id}: {error:#}"))
        .tool_calls
}

fn tool_row(row: gents::TimelineToolCallRow) -> ToolCallRow {
    assert!(
        row.doc_id.is_some(),
        "canonical timeline tool row omitted physical identity"
    );
    ToolCallRow {
        tool_call_id: row.tool_call_id,
        lifecycle_state: row.lifecycle_state.unwrap_or(row.status),
        args: row.args,
        result: row.result,
        child_request_id: row.child_request_id,
    }
}

async fn fetch_tool_call(
    node: &Arc<EmbeddedNode>,
    request_id: &str,
    session_id: &str,
    tool_call_id: &str,
) -> Option<ToolCallRow> {
    timeline_tools(node, request_id)
        .await
        .into_iter()
        .filter(|row| row.session_id == session_id && row.tool_call_id == tool_call_id)
        .map(tool_row)
        .next()
}

async fn wait_for_tool_call_state(
    node: &Arc<EmbeddedNode>,
    request_id: &str,
    session_id: &str,
    tool_call_id: &str,
    expected_state: &str,
    timeout: Duration,
) -> ToolCallRow {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut last = None;
    loop {
        if let Some(row) = fetch_tool_call(node, request_id, session_id, tool_call_id).await {
            if row.lifecycle_state == expected_state {
                return row;
            }
            last = Some(row);
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for tool call {tool_call_id} state={expected_state}; last={last:?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn wait_for_tool_call_settled(
    node: &Arc<EmbeddedNode>,
    request_id: &str,
    session_id: &str,
    tool_call_id: &str,
    timeout: Duration,
) -> ToolCallRow {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut last = None;
    loop {
        if let Some(row) = fetch_tool_call(node, request_id, session_id, tool_call_id).await {
            if matches!(
                row.lifecycle_state.as_str(),
                "completed" | "failed" | "cancelled"
            ) {
                return row;
            }
            last = Some(row);
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for tool call {tool_call_id} to settle; last={last:?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn wait_for_background_tool_call(
    node: &Arc<EmbeddedNode>,
    request_id: &str,
    session_id: &str,
    tool_name: &str,
    timeout: Duration,
) -> ToolCallRow {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(row) = timeline_tools(node, request_id)
            .await
            .into_iter()
            .filter(|row| {
                row.session_id == session_id
                    && row.tool_name == tool_name
                    && row.await_mode.as_deref() == Some("background")
            })
            .map(tool_row)
            .next()
        {
            return row;
        }
        if let Some(state) = fetch_request_lifecycle(node.as_ref(), request_id).await {
            if is_terminal(&state) {
                dump_session_diagnostics(node.as_ref(), session_id).await;
                panic!(
                    "request {request_id} terminalized as {state} before accepted background tool {tool_name} in session {session_id}"
                );
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for background tool {tool_name} in session {session_id}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn assert_no_tool_call(node: &EmbeddedNode, session_id: &str, tool_names: &[&str]) {
    let session_id = escape_graphql_string(session_id);
    for tool_name in tool_names {
        let tool_name = escape_graphql_string(tool_name);
        let query = format!(
            r#"{{
                AgentToolCall(
                    filter: {{
                        session_id: {{ _eq: "{session_id}" }},
                        tool_name: {{ _eq: "{tool_name}" }}
                    }}
                ) {{ tool_call_id }}
            }}"#
        );
        let response = node.execute(&query).await;
        assert!(
            !response.has_errors(),
            "query forbidden tool calls failed: {:?}",
            response.errors
        );
        let count = response
            .data
            .as_ref()
            .and_then(|data| data.get("AgentToolCall"))
            .and_then(serde_json::Value::as_array)
            .map_or(0, Vec::len);
        assert_eq!(count, 0, "model called forbidden control tool {tool_name}");
    }
}

async fn load_session_messages(
    node: &Arc<EmbeddedNode>,
    request_id: &str,
    session_id: &str,
) -> Vec<gents_protocol::message::Message> {
    load_run_timeline_rows(&ConfigAccess::Local(node.clone()), request_id)
        .await
        .unwrap_or_else(|error| {
            panic!("load canonical session timeline for {request_id}: {error:#}")
        })
        .messages
        .into_iter()
        .filter(|row| row.session_id == session_id)
        .map(|row| row.message)
        .collect()
}

fn tool_result_texts(messages: &[gents_protocol::message::Message]) -> Vec<&str> {
    use gents_protocol::message::{Message, ToolResultContent, UserContent};
    messages
        .iter()
        .filter_map(|message| match message {
            Message::User { content } => Some(content.iter()),
            _ => None,
        })
        .flatten()
        .filter_map(|item| match item {
            UserContent::ToolResult(result) => Some(result.content.iter()),
            _ => None,
        })
        .flatten()
        .filter_map(|part| match part {
            ToolResultContent::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect()
}

/// Every `agent_new`/`agent_message` receipt the model received.
fn session_receipts(messages: &[gents_protocol::message::Message]) -> Vec<serde_json::Value> {
    tool_result_texts(messages)
        .into_iter()
        .filter_map(|text| serde_json::from_str::<serde_json::Value>(text).ok())
        .filter(|value| value.get("request_doc_id").is_some() && value["ok"] == true)
        .collect()
}

fn session_receipt(
    messages: &[gents_protocol::message::Message],
    caused_request_id: &str,
) -> Option<serde_json::Value> {
    session_receipts(messages)
        .into_iter()
        .find(|receipt| receipt["request_id"] == caused_request_id)
}

fn model_tool_call_count(messages: &[gents_protocol::message::Message], tool_name: &str) -> usize {
    use gents_protocol::message::{AssistantContent, Message};
    messages
        .iter()
        .filter_map(|message| match message {
            Message::Assistant { content, .. } => Some(content.iter()),
            _ => None,
        })
        .flatten()
        .filter(|content| {
            matches!(content, AssistantContent::ToolCall(call) if call.function.name == tool_name)
        })
        .count()
}

async fn wait_for_model_tool_call(
    node: &Arc<EmbeddedNode>,
    request_id: &str,
    session_id: &str,
    tool_name: &str,
    timeout: Duration,
) {
    let deadline = tokio::time::Instant::now() + timeout;
    let escaped_request_id = escape_graphql_string(request_id);
    let escaped_session_id = escape_graphql_string(session_id);
    let escaped_tool_name = escape_graphql_string(tool_name);
    let query = format!(
        r#"{{
            AgentToolCall(
                filter: {{
                    request_id: {{ _eq: "{escaped_request_id}" }},
                    session_id: {{ _eq: "{escaped_session_id}" }},
                    tool_name: {{ _eq: "{escaped_tool_name}" }}
                }}
            ) {{ tool_call_id }}
        }}"#
    );
    loop {
        let response = node.execute(&query).await;
        assert!(
            !response.has_errors(),
            "query accepted model tool call {tool_name} failed: {:?}",
            response.errors
        );
        let observed = response
            .data
            .as_ref()
            .and_then(|data| data.get("AgentToolCall"))
            .and_then(serde_json::Value::as_array)
            .is_some_and(|rows| !rows.is_empty());
        if observed {
            return;
        }
        if let Some(state) = fetch_request_lifecycle(node.as_ref(), request_id).await {
            assert!(
                !is_terminal(&state),
                "request {request_id} terminalized as {state} before model tool call {tool_name} in session {session_id}"
            );
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for model tool call {tool_name} in session {session_id}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Wait for one tool-result text part that contains every needle.
async fn wait_for_tool_result_containing(
    node: &Arc<EmbeddedNode>,
    request_id: &str,
    session_id: &str,
    needles: &[&str],
    timeout: Duration,
) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let messages = load_session_messages(node, request_id, session_id).await;
        if tool_result_texts(&messages)
            .iter()
            .any(|text| needles.iter().all(|needle| text.contains(needle)))
        {
            return;
        }
        if let Some(state) = fetch_request_lifecycle(node.as_ref(), request_id).await {
            assert!(
                !is_terminal(&state),
                "request {request_id} terminalized as {state} before a tool result contained {needles:?}; transcript={messages:#?}"
            );
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for a tool result containing {needles:?} in session {session_id}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn assert_model_tool_call_count_at_least(
    node: &Arc<EmbeddedNode>,
    request_id: &str,
    session_id: &str,
    tool_name: &str,
    expected: usize,
) {
    let messages = load_session_messages(node, request_id, session_id).await;
    let actual = model_tool_call_count(&messages, tool_name);
    assert!(
        actual >= expected,
        "expected at least {expected} model call(s) to {tool_name} in session {session_id}, got {actual}; transcript={messages:#?}"
    );
}

async fn wait_for_message_containing(
    node: &Arc<EmbeddedNode>,
    request_id: &str,
    session_id: &str,
    needle: &str,
    timeout: Duration,
) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let found = load_session_messages(node, request_id, session_id)
            .await
            .iter()
            .map(|message| gents_protocol::transcript::present_message(message).body_markdown)
            .any(|body| body.contains(needle));
        if found {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for session {session_id} message containing {needle:?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[derive(Debug, Clone, Deserialize)]
struct WakeRequestRow {
    request_id: String,
    input: Option<RequestInput>,
}

async fn wait_for_background_wake(
    node: &EmbeddedNode,
    session_id: &str,
    queued_after_request_id: &str,
    timeout: Duration,
) -> WakeRequestRow {
    let deadline = tokio::time::Instant::now() + timeout;
    let escaped_session_id = escape_graphql_string(session_id);
    let expected_queue_key = format!("background_completion:{session_id}");
    let query = format!(
        r#"{{
            AgentRequest(
                filter: {{ session_id: {{ _eq: "{escaped_session_id}" }} }},
                order: {{ created_at: ASC }}
            ) {{ request_id input }}
        }}"#
    );
    loop {
        let response = node.execute(&query).await;
        assert!(
            !response.has_errors(),
            "query background wake requests failed: {:?}",
            response.errors
        );
        let wake = response
            .data
            .as_ref()
            .and_then(|data| data.get("AgentRequest"))
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|row| serde_json::from_value::<WakeRequestRow>(row.clone()).ok())
            .find(|row| {
                row.input
                    .as_ref()
                    .and_then(|input| input.queue.as_ref())
                    .is_some_and(|queue| {
                        queue.source == QueueSource::BackgroundCompletion
                            && queue.policy == QueuePolicy::Coalesce
                            && queue.key.as_deref() == Some(expected_queue_key.as_str())
                            && queue.queued_after_request_id.as_deref()
                                == Some(queued_after_request_id)
                    })
            });
        if let Some(wake) = wake {
            return wake;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for coalesced background wake in session {session_id}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// The completion wake after `parent_request_id` runs real inference and
/// processes the notification.
async fn assert_wake_observed(node: &EmbeddedNode, session_id: &str, parent_request_id: &str) {
    let wake =
        wait_for_background_wake(node, session_id, parent_request_id, Duration::from_secs(60))
            .await;
    let state = wait_for_request_terminal(node, &wake.request_id, Duration::from_secs(180)).await;
    assert_eq!(state, "completed");
    assert_min_completed_inference_calls(node, &wake.request_id, 1).await;
    let answer = wait_for_assistant_answer(node, &wake.request_id, Duration::from_secs(30)).await;
    assert!(
        answer.contains("BACKGROUND_COMPLETION_OBSERVED"),
        "real-inference wake did not process the completion notification: {answer:?}"
    );
}

async fn assert_min_completed_inference_calls(
    node: &EmbeddedNode,
    request_id: &str,
    expected: usize,
) {
    let request_id = escape_graphql_string(request_id);
    let query = format!(
        r#"{{
            InferenceCall(
                filter: {{
                    request_id: {{ _eq: "{request_id}" }},
                    call_state: {{ _eq: "completed" }}
                }}
            ) {{ call_id }}
        }}"#
    );
    let response = node.execute(&query).await;
    assert!(
        !response.has_errors(),
        "query completed inference calls failed: {:?}",
        response.errors
    );
    let completed = response
        .data
        .as_ref()
        .and_then(|data| data.get("InferenceCall"))
        .and_then(serde_json::Value::as_array)
        .map_or(0, Vec::len);
    assert!(
        completed >= expected,
        "request {request_id} completed with only {completed} real inference call(s); expected at least {expected}"
    );
}

// ---------------------------------------------------------------------------
// Cross-node pairing
// ---------------------------------------------------------------------------

/// Author the local data-plane layer for one enrolled peer. Enrollment remains
/// the transport and identity gate; this row only selects the scope template.
async fn write_data_plane_pairing(
    node: &EmbeddedNode,
    peer_id: &str,
    self_did: &str,
    peer_addr: &str,
    template: &str,
) {
    let collections = resolve_template(template)
        .unwrap_or_else(|| panic!("template {template} should resolve"))
        .collections
        .iter()
        .map(|collection| format!("\"{}\"", escape_graphql_string(collection)))
        .collect::<Vec<_>>()
        .join(", ");
    let peer_id = escape_graphql_string(peer_id);
    let self_did = escape_graphql_string(self_did);
    let template = escape_graphql_string(template);
    let peer_addr = escape_graphql_string(peer_addr);
    let now = escape_graphql_string(&chrono::Utc::now().to_rfc3339());
    let mutation = format!(
        r#"mutation {{
            create_DataPlanePairingDesired(input: {{
                peer_id: "{peer_id}",
                agent_did: "{self_did}",
                collections: [{collections}],
                replicator_addresses: ["{peer_addr}"],
                template: "{template}",
                source: "test-session-message",
                created_at: "{now}",
                updated_at: "{now}"
            }}) {{ _docID }}
        }}"#
    );
    let resp = node.execute(&mutation).await;
    assert!(
        !resp.has_errors(),
        "create DataPlanePairingDesired failed: {:?}",
        resp.errors
    );
}

/// Wait until the applied route for `peer_id` has an address and a replicator
/// filter for `collection`.
async fn wait_for_pairing_applied(
    node: &EmbeddedNode,
    peer_id: &str,
    collection: &str,
    timeout: Duration,
) {
    let deadline = Instant::now() + timeout;
    let escaped_peer_id = escape_graphql_string(peer_id);
    let query = format!(
        r#"{{
            PeerPairingApplied(filter: {{ peer_id: {{ _eq: "{escaped_peer_id}" }} }}, limit: 1) {{
                peer_id
                collections
                replicator_addresses
                replicator_filter
            }}
        }}"#
    );
    let mut last = String::from("<none>");
    loop {
        let response = node.execute(&query).await;
        if let Some(row) = first_optional_row::<serde_json::Value>(&response, "PeerPairingApplied")
        {
            last = row.to_string();
            let addressed = row
                .get("replicator_addresses")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|addresses| {
                    addresses
                        .iter()
                        .any(|address| address.as_str().is_some_and(|s| !s.trim().is_empty()))
                });
            let filtered = row
                .get("replicator_filter")
                .and_then(serde_json::Value::as_str)
                .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
                .is_some_and(|filter| filter.get(collection).is_some());
            if addressed && filtered {
                return;
            }
        }
        if Instant::now() >= deadline {
            panic!(
                "timed out waiting for PeerPairingApplied({peer_id}) to route {collection}; last row={last}"
            );
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}
