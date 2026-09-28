use super::*;

#[tokio::test]
async fn scoped_missing_document_reload_preserves_other_owner_survivors() {
    let tempdir = tempfile::tempdir().unwrap();
    let node = Arc::new(NodeBuilder::default().build().await.unwrap());
    ensure_runtime_schemas(node.as_ref()).await.unwrap();
    seed_principal(node.as_ref(), "did:alpha").await;
    let mut raw = node.subscribe(&[EventName::Update]);
    seed_principal(node.as_ref(), "did:beta").await;
    let update = raw.recv().await.unwrap().as_update().unwrap().clone();
    node.event_bus().unsubscribe(raw.id());

    let (store, mut changes) = ObservedStore::new(ClientStore::default());
    let peer_dir = crate::client::peer_directory::PeerDirectory::open_writer(
        tempdir.path().join("peers.json"),
    )
    .await
    .unwrap();
    let peers = ClientSyncStateOwner::new(
        crate::client::core::P2PHealth::default(),
        peer_dir,
        Vec::new(),
    );
    let subscription = node.subscribe_document_changes();
    let (_selection, selection) = watch::channel(Some("did:alpha".to_owned()));
    let handle = spawn_observer_with_selection(
        node.clone(),
        store.clone(),
        peers,
        "did:test:requester".to_owned(),
        subscription,
        selection,
    );
    // Publishing both invalidations before this single-threaded executor yields
    // makes one mixed batch, independent of native commit notification timing.
    node.event_bus()
        .publish(events::Message::update(update.clone()));
    let mut absent = update;
    absent.doc_id = "absent-principal-document".to_owned();
    node.event_bus().publish(events::Message::update(absent));

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = store.snapshot();
            let has = |did| {
                snapshot
                    .agent_principals
                    .iter()
                    .any(|row| row.agent_did == did)
            };
            if has("did:alpha") && has("did:beta") {
                break;
            }
            changes.changed().await.unwrap();
        }
    })
    .await
    .expect("scoped deletion recovery must retain surviving rows outside the selected owner");
    assert_eq!(handle.metrics_snapshot().document_change_batches, 1);
    assert_eq!(handle.metrics_snapshot().docs_fetched, 1);
    handle.shutdown().await;
}
