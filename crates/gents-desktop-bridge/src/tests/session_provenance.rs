use gents::session_origin::SessionScope;

use crate::provenance::session_provenance;
use crate::tests::support::seed_provenance_fixture;
use crate::types::CausedRequestView;

fn scope(session_id: &str) -> SessionScope {
    SessionScope {
        agent_did: "did:test:operator".into(),
        session_id: session_id.into(),
        requester_did: None,
    }
}

fn ids(rows: &[CausedRequestView]) -> Vec<&str> {
    let mut ids: Vec<_> = rows.iter().map(|row| row.request_id.as_str()).collect();
    ids.sort_unstable();
    ids
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subagents_are_only_the_sessions_this_one_started() {
    let (core, _tmp, parent_doc_id) = seed_provenance_fixture().await;
    let view = session_provenance(&core, scope("sess_parent"))
        .await
        .expect("provenance");

    assert_eq!(view.session_id, "sess_parent");
    assert!(view.started_by.is_none());
    assert!(view.received.is_empty());
    assert_eq!(
        ids(&view.started),
        ["req_child", "req_peer"],
        "a message into an existing session never makes it a subagent"
    );
    assert_eq!(
        ids(&view.sent),
        ["req_child", "req_child_2", "req_existing_2", "req_peer"],
        "every request this session caused is still joined to its call"
    );
    for row in view.sent.iter().chain(&view.started) {
        assert_eq!(row.caused_by_request_id.as_deref(), Some("req_parent"));
        assert_eq!(
            row.caused_by_request_doc_id.as_deref(),
            Some(parent_doc_id.as_str())
        );
        assert_eq!(row.caused_by_session_id.as_deref(), Some("sess_parent"));
        assert_eq!(row.hop, Some(1));
    }
    let peer = view
        .started
        .iter()
        .find(|row| row.request_id == "req_peer")
        .unwrap();
    assert_eq!(peer.agent_did.as_deref(), Some("did:test:other"));
    assert_eq!(peer.caused_by_tool_call_id.as_deref(), Some("tc_peer"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn another_requester_scope_under_the_same_label_is_not_this_session() {
    let (core, _tmp, _) = seed_provenance_fixture().await;
    let view = session_provenance(&core, scope("sess_parent"))
        .await
        .expect("provenance");
    assert!(!ids(&view.sent).contains(&"req_leak"));

    let other = session_provenance(
        &core,
        SessionScope {
            requester_did: Some("did:test:someone-else".into()),
            ..scope("sess_parent")
        },
    )
    .await
    .expect("provenance");
    assert_eq!(ids(&other.started), ["req_leak"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_started_session_names_the_session_that_started_it() {
    let (core, _tmp, _) = seed_provenance_fixture().await;
    let view = session_provenance(&core, scope("sess_child"))
        .await
        .expect("provenance");

    assert!(view.sent.is_empty() && view.started.is_empty());
    let started_by = view.started_by.expect("caused origin");
    assert_eq!(started_by.request_id, "req_child");
    assert_eq!(
        started_by.caused_by_session_id.as_deref(),
        Some("sess_parent")
    );
    assert_eq!(ids(&view.received), ["req_child", "req_child_2"]);
    assert!(view
        .received
        .iter()
        .all(|row| row.caused_by_session_id.as_deref() == Some("sess_parent")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_messaged_session_was_not_started_by_its_sender() {
    let (core, _tmp, _) = seed_provenance_fixture().await;
    let view = session_provenance(&core, scope("sess_existing"))
        .await
        .expect("provenance");

    assert!(view.started_by.is_none());
    assert_eq!(ids(&view.received), ["req_existing_2"]);
    assert_eq!(
        view.received[0].caused_by_session_id.as_deref(),
        Some("sess_parent")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_root_session_has_no_provenance() {
    let (core, _tmp, _) = seed_provenance_fixture().await;
    let view = session_provenance(&core, scope("sess_unrelated"))
        .await
        .expect("provenance");
    assert!(view.started_by.is_none());
    assert!(view.received.is_empty() && view.sent.is_empty() && view.started.is_empty());
}
