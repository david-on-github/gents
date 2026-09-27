use std::sync::Arc;

use gents::graphql::escape_graphql_string;
use gents_desktop_core::client::{ClientCore, ClientCoreOptions, DesktopPaths};
use gents_protocol::row::AgentRequestRow;
use tempfile::TempDir;

pub async fn boot_core() -> (Arc<ClientCore>, TempDir) {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let paths = DesktopPaths::from_root(tempdir.path());
    let core = ClientCore::start_with_paths_and_options(paths, ClientCoreOptions::local_only())
        .await
        .expect("core starts");
    (Arc::new(core), tempdir)
}

pub async fn seed_standalone_fixture() -> (Arc<ClientCore>, TempDir) {
    let (core, tmp) = boot_core().await;

    let mutation = r#"mutation {
        create_AgentRequest(input: { purpose: "normal",
            request_id: "req_solo",
            agent_did: "did:test:operator",
            behavior_id: "test-behavior",
            session_id: "sess_solo",
            content: "standalone fixture",
            lifecycle_state: "processing",
            backend_id: "",
            created_at: "2026-05-20T00:00:00Z",
            retry_count: 0
        }) { _docID }
    }"#;

    let response = core.node().execute(mutation).await;
    assert!(
        !response.has_errors(),
        "seed standalone AgentRequest failed: {:?}",
        response.errors
    );

    (core, tmp)
}

/// `req_parent` in `sess_parent` (null requester) caused, through its calls:
/// - `sess_child`: started (`req_child`, its origin), then messaged again
///   (`req_child_2`);
/// - `sess_peer` on another agent: started (`req_peer`);
/// - `sess_existing`: only messaged (`req_existing_2`); its origin
///   `req_existing_1` is the person's own.
///
/// `req_other_scope` shares the label `sess_parent` under another requester
/// and started `sess_leak`; it is not part of the null-requester scope.
/// Returns the core and `req_parent`'s document id.
pub async fn seed_provenance_fixture() -> (Arc<ClientCore>, TempDir, String) {
    let (core, tmp) = boot_core().await;

    let parent_doc_id = create_request(
        &core,
        "req_parent",
        "did:test:operator",
        "sess_parent",
        None,
        "lead",
        "processing",
        "2026-05-20T00:00:00Z",
        None,
    )
    .await;
    create_request(
        &core,
        "req_existing_1",
        "did:test:operator",
        "sess_existing",
        None,
        "lead",
        "completed",
        "2026-05-20T00:00:30Z",
        None,
    )
    .await;
    let other_doc_id = create_request(
        &core,
        "req_other_scope",
        "did:test:operator",
        "sess_parent",
        Some("did:test:someone-else"),
        "lead",
        "processing",
        "2026-05-20T00:00:40Z",
        None,
    )
    .await;
    let cause = Some(("req_parent", parent_doc_id.as_str()));
    create_request(
        &core,
        "req_child",
        "did:test:operator",
        "sess_child",
        None,
        "researcher",
        "completed",
        "2026-05-20T00:01:00Z",
        cause.map(|(r, d)| (r, d, "tc_start")),
    )
    .await;
    create_request(
        &core,
        "req_child_2",
        "did:test:operator",
        "sess_child",
        None,
        "researcher",
        "processing",
        "2026-05-20T00:02:00Z",
        cause.map(|(r, d)| (r, d, "tc_message")),
    )
    .await;
    create_request(
        &core,
        "req_peer",
        "did:test:other",
        "sess_peer",
        None,
        "reviewer",
        "processing",
        "2026-05-20T00:03:00Z",
        cause.map(|(r, d)| (r, d, "tc_peer")),
    )
    .await;
    create_request(
        &core,
        "req_existing_2",
        "did:test:operator",
        "sess_existing",
        None,
        "lead",
        "processing",
        "2026-05-20T00:04:00Z",
        cause.map(|(r, d)| (r, d, "tc_existing")),
    )
    .await;
    create_request(
        &core,
        "req_leak",
        "did:test:operator",
        "sess_leak",
        None,
        "researcher",
        "processing",
        "2026-05-20T00:05:00Z",
        Some(("req_other_scope", other_doc_id.as_str(), "tc_leak")),
    )
    .await;
    create_request(
        &core,
        "req_unrelated",
        "did:test:operator",
        "sess_unrelated",
        None,
        "lead",
        "processing",
        "2026-05-20T00:06:00Z",
        None,
    )
    .await;

    (core, tmp, parent_doc_id)
}

/// Create one public request and return its document id. `cause` is the
/// causing request id, its document id and the causing call.
#[allow(clippy::too_many_arguments)]
async fn create_request(
    core: &Arc<ClientCore>,
    request_id: &str,
    agent_did: &str,
    session_id: &str,
    requester_did: Option<&str>,
    behavior_id: &str,
    lifecycle_state: &str,
    created_at: &str,
    cause: Option<(&str, &str, &str)>,
) -> String {
    let requester = requester_did
        .map(|did| format!(r#"requester_did: "{did}","#))
        .unwrap_or_default();
    let caused = cause
        .map(|(request, doc, call)| {
            format!(
                r#"subagent_depth: 1, caused_by_parent_request_id: "{request}", caused_by_parent_request_doc_id: "{doc}", caused_by_parent_tool_call_id: "{call}","#
            )
        })
        .unwrap_or_default();
    let mutation = format!(
        r#"mutation {{ create_AgentRequest(input: {{ purpose: "normal", request_id: "{request_id}", agent_did: "{agent_did}", {requester} behavior_id: "{behavior_id}", session_id: "{session_id}", {caused} content: "work", lifecycle_state: "{lifecycle_state}", backend_id: "", created_at: "{created_at}", retry_count: 0 }}) {{ _docID }} }}"#
    );
    let response = core.node().execute(&mutation).await;
    assert!(
        !response.has_errors(),
        "seed {request_id} failed: {:?}",
        response.errors
    );
    let response = core
        .node()
        .execute(&format!(
            r#"{{ AgentRequest(filter: {{ request_id: {{ _eq: "{request_id}" }} }}) {{ _docID }} }}"#
        ))
        .await;
    response
        .data
        .as_ref()
        .and_then(|data| data.pointer("/AgentRequest/0/_docID"))
        .and_then(|value| value.as_str())
        .unwrap_or_else(|| panic!("{request_id} doc id"))
        .to_owned()
}

pub async fn fetch_request_row(core: &Arc<ClientCore>, request_id: &str) -> AgentRequestRow {
    let escaped = escape_graphql_string(request_id);
    let query = format!(
        r#"{{
            AgentRequest(
                filter: {{ request_id: {{ _eq: "{escaped}" }} }},
                limit: 1
            ) {{
                request_id
                interrupt_requested_at
            }}
        }}"#
    );

    let response = core.node().execute(&query).await;
    assert!(
        !response.has_errors(),
        "fetch_request_row query failed for {request_id}: {:?}",
        response.errors
    );

    let data = response.data.unwrap_or(serde_json::Value::Null);
    let row = data
        .get("AgentRequest")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .cloned()
        .unwrap_or_else(|| panic!("fetch_request_row: request {request_id} not found"));
    serde_json::from_value(row)
        .unwrap_or_else(|error| panic!("fetch_request_row: invalid request {request_id}: {error}"))
}
