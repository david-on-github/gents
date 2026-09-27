// Included in inference.rs's test module to reuse its real daemon harness.

/// Parks the request inside provider-input estimation. `build_request` awaits
/// every tool definition, so this future holds the daemon in the window between
/// the claim and the first provider call for as long as a test needs.
#[derive(Clone)]
struct PreInferenceBlockingTool {
    entered: Arc<std::sync::atomic::AtomicBool>,
}

impl crate::llm::tool::ToolDyn for PreInferenceBlockingTool {
    fn name(&self) -> String {
        "pre_inference_block".into()
    }

    fn definition<'a>(
        &'a self,
        _: String,
    ) -> crate::llm::tool::BoxFuture<'a, crate::llm::tool::ToolDefinition> {
        Box::pin(async move {
            self.entered.store(true, Ordering::SeqCst);
            std::future::pending().await
        })
    }

    fn call<'a>(
        &'a self,
        _: String,
    ) -> crate::llm::tool::BoxFuture<'a, Result<String, crate::llm::tool::ToolError>> {
        Box::pin(async { unreachable!("the pre-inference window never dispatches a tool") })
    }
}

#[derive(Clone)]
struct ProviderCallCountingModel(Arc<AtomicUsize>);

#[allow(refining_impl_trait)]
impl CompletionModel for ProviderCallCountingModel {
    type Response = ();
    type StreamingResponse = ();
    type Client = ();

    fn make(_: &(), _: impl Into<String>) -> Self {
        Self(Arc::new(AtomicUsize::new(0)))
    }

    async fn completion(
        &self,
        _: CompletionRequest,
    ) -> Result<CompletionResponse<()>, CompletionError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(CompletionError::ProviderError("streaming only".into()))
    }

    async fn stream(
        &self,
        _: CompletionRequest,
    ) -> Result<StreamingCompletionResponse<()>, CompletionError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(StreamingCompletionResponse::stream(Box::pin(stream::iter(
            vec![Ok(RawStreamingChoice::FinalResponse(()))],
        ))))
    }
}

fn pre_inference_behavior() -> Arc<ResolvedBehavior> {
    let behavior = test_behavior();
    let mut behavior = ResolvedBehavior::clone(behavior.as_ref());
    // The owner may only finalize under a live lease. A lease long enough to
    // outlast the whole test keeps expiry out of the assertion: terminalization
    // here is the owner's, never a lapsed generation's.
    behavior.stream_liveness_timeout = Duration::from_secs(600);
    behavior.deadline_duration = Duration::from_secs(1_200);
    Arc::new(behavior)
}

async fn seed_titled_session(
    node: &defra_node::EmbeddedNode,
    request: &AgentRequest,
    behavior: &ResolvedBehavior,
) -> anyhow::Result<()> {
    // A session that already has a title keeps the owner from dispatching a
    // detached title request while this one is being interrupted.
    let title = gents_protocol::session::SessionTitle {
        text: "pre-inference interrupt".into(),
        source: gents_protocol::session::SessionTitleSource::Task,
    };
    let created = crate::config_client::ConfigAccess::transact_local(
        node,
        None,
        "session.create",
        move |txn| {
            let title = title.clone();
            Box::pin(async move {
                crate::session::ensure_session_in_txn(
                    txn,
                    &request.session_id,
                    &request.agent_did,
                    &behavior.behavior_id,
                    request.requester_did.as_deref(),
                    Some(title),
                    None,
                    &request.created_at,
                )
                .await
            })
        },
    )
    .await?;
    // The writer preserves an existing document untouched, so only a create
    // establishes the title this test depends on.
    anyhow::ensure!(created, "seeded session already existed without this title");
    Ok(())
}

async fn request_terminal_row(
    node: &defra_node::EmbeddedNode,
    request_doc_id: &str,
) -> anyhow::Result<serde_json::Value> {
    use anyhow::Context;

    let doc_id = crate::graphql::escape_graphql_string(request_doc_id);
    let response = crate::graphql::graphql_with_transaction_retry(
        node,
        &format!(
            r#"{{ AgentRequest(filter: {{ _docID: {{ _eq: "{doc_id}" }} }}, limit: 1) {{
                lifecycle_state failure_reason terminalized_at interrupt_requested_at
            }} }}"#
        ),
        "pre_inference_interrupt_request_row",
    )
    .await?;
    response
        .data
        .as_ref()
        .and_then(|data| data.get("AgentRequest"))
        .and_then(|rows| rows.as_array())
        .and_then(|rows| rows.first())
        .cloned()
        .context("AgentRequest read selected no row for the claimed request")
}

/// An interrupt latched while a claimed request is still preparing its provider
/// input must be terminalized by the owned completion loop, not left for the
/// recovery sweep.
///
/// Nothing in this test can terminalize the request except the owner: no
/// recovery sweep runs in-process, the lease outlives the assertion window, and
/// the sweep's own terminal write stamps `execution lease expired` instead of
/// the reason asserted here.
#[tokio::test]
async fn interrupt_while_preparing_provider_input_terminalizes_through_the_owner() {
    let node = Arc::new(defra_node::EmbeddedNode::builder().build().await.unwrap());
    crate::ensure_runtime_schemas(&node).await.unwrap();
    let behavior = pre_inference_behavior();
    let agent_did = behavior.agent_did().to_owned();
    let identity = behavior.principal_identity().clone();
    let prompt = LayeredPromptBuilder::for_behavior(
        &behavior.system_prompt,
        &behavior.behavior_id,
        &[],
        false,
        &[],
    );
    let entered = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let mut daemon = BehaviorDaemon::new(
        node.clone(),
        behavior.clone(),
        None,
        Arc::new(ProviderCallCountingModel(provider_calls.clone())),
        prompt.preamble().to_owned(),
        Arc::new(vec![Box::new(PreInferenceBlockingTool {
            entered: entered.clone(),
        }) as Box<dyn crate::llm::tool::ToolDyn>]),
        prompt,
        FailurePolicy::default(),
        None,
        BackgroundToolRegistry::default(),
        BackgroundExecutionRegistry::default(),
        Arc::new(StartupBarrier::ready_for_test()),
        crate::runtime_status::RuntimeStatusHandle::new(node.clone(), agent_did.clone()),
        1,
        crate::request_admission::AgentRequestAdmissionVerifier::new(
            node.clone(),
            identity,
            crate::agent::p2p_reconcile::enrollment_authority_channel().1,
        ),
    )
    .unwrap();
    let request = create_routed_request(&node, &behavior, &agent_did).await;
    let request_doc_id = request.doc_id.clone();
    let requester_did = request.requester_did.clone();
    seed_titled_session(node.as_ref(), &request, &behavior)
        .await
        .expect("seed a titled session through the session writer");

    let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let process = daemon.process_request(request, shutdown_rx);
    tokio::pin!(process);

    let settled_in = tokio::time::timeout(Duration::from_secs(20), async {
        let reach_pre_inference_window = async {
            loop {
                let row = request_terminal_row(node.as_ref(), &request_doc_id)
                    .await
                    .expect("read the owned request row");
                // The owner fences prompt assembly behind the same processing
                // state as inference, so the pre-inference window is owned and
                // nonterminal rather than still claimed.
                if entered.load(Ordering::SeqCst) && row["lifecycle_state"] == "processing" {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        tokio::pin!(reach_pre_inference_window);
        tokio::select! {
            _ = &mut reach_pre_inference_window => {}
            _ = &mut process => panic!("daemon exited before reaching the pre-inference window"),
        }

        crate::interrupt::interrupt_request_by_doc_id(
            node.as_ref(),
            &request_doc_id,
            &agent_did,
            requester_did.as_deref(),
        )
        .await
        .expect("latch interrupt");
        let latched_at = std::time::Instant::now();

        (&mut process).await;
        latched_at.elapsed()
    })
    .await
    .expect("owner must terminalize the interrupted claim without waiting for recovery");

    // Recovery cannot terminalize a claim before its execution lease expires, so
    // a settle time far under the default lease is the owner's write and nothing
    // else's — the sweep would also stamp a different failure_reason.
    assert!(
        settled_in < Duration::from_secs(10),
        "owner terminalization took {settled_in:?}, which no longer excludes the \
         recovery path waiting out the {}s default execution lease",
        crate::config::DEFAULT_STREAM_LIVENESS_TIMEOUT_SECS,
    );

    let row = request_terminal_row(node.as_ref(), &request_doc_id)
        .await
        .expect("read the terminalized request row");
    assert_eq!(row["lifecycle_state"], "interrupted");
    assert!(
        !row["terminalized_at"].is_null(),
        "owner terminalization must stamp terminalized_at: {row}"
    );
    assert_eq!(
        row["failure_reason"],
        gents_protocol::request_lifecycle::interrupt_terminal_reason::BEFORE_ANY_PROVIDER_CALL,
        "runtime must record that the interrupt caught the request before any provider call: {row}"
    );
    assert_eq!(
        provider_calls.load(Ordering::SeqCst),
        0,
        "no provider call may run in the pre-inference window"
    );
}

/// `prepare_unless_interrupted` evaluates `borrow_and_update().is_some()` ahead
/// of its `biased` select, so a latch already observable on the receiver handed
/// to `handle_request` is decided when a window is entered rather than raced
/// against the durable reads inside it. Which window decided shows up only in
/// durable state: the admission window returns with the request still
/// `claimed`, while reaching the assembly window means execution has begun and
/// the row reads `processing`.
#[tokio::test]
async fn interrupt_latched_at_the_claim_terminalizes_before_execution_begins() {
    let node = Arc::new(defra_node::EmbeddedNode::builder().build().await.unwrap());
    crate::ensure_runtime_schemas(&node).await.unwrap();
    let behavior = pre_inference_behavior();
    let agent_did = behavior.agent_did().to_owned();
    let identity = behavior.principal_identity().clone();
    let prompt = LayeredPromptBuilder::for_behavior(
        &behavior.system_prompt,
        &behavior.behavior_id,
        &[],
        false,
        &[],
    );
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let mut daemon = BehaviorDaemon::new(
        node.clone(),
        behavior.clone(),
        None,
        Arc::new(ProviderCallCountingModel(provider_calls.clone())),
        prompt.preamble().to_owned(),
        Arc::new(Vec::<Box<dyn crate::llm::tool::ToolDyn>>::new()),
        prompt,
        FailurePolicy::default(),
        None,
        BackgroundToolRegistry::default(),
        BackgroundExecutionRegistry::default(),
        Arc::new(StartupBarrier::ready_for_test()),
        crate::runtime_status::RuntimeStatusHandle::new(node.clone(), agent_did.clone()),
        1,
        crate::request_admission::AgentRequestAdmissionVerifier::new(
            node.clone(),
            identity,
            crate::agent::p2p_reconcile::enrollment_authority_channel().1,
        ),
    )
    .unwrap();
    let request = create_routed_request(&node, &behavior, &agent_did).await;
    let request_doc_id = request.doc_id.clone();
    let requester_did = request.requester_did.clone();
    seed_titled_session(node.as_ref(), &request, &behavior)
        .await
        .expect("seed a titled session through the session writer");

    let stream_writer = crate::streaming::DefraStreamWriter::new(
        node.clone(),
        behavior.agent_did(),
        Duration::from_millis(behavior.stream_batch_ms),
    );
    let origin =
        crate::lifecycle::ExecutionOrigin::from_persisted(request.execution_origin.as_deref())
            .expect("a created request carries a canonical execution origin");
    let mut lifecycle = crate::lifecycle::RequestLifecycle::new_with_execution_binding(
        node.clone(),
        &behavior.behavior_id,
        behavior.agent_did(),
        request.clone(),
        behavior.deadline_duration.as_secs(),
        origin,
        behavior.backend_id.clone().unwrap_or_default(),
    );
    lifecycle.set_execution_lease_duration(behavior.stream_liveness_timeout);
    lifecycle.set_configured_max_total_tokens(behavior.max_total_tokens);
    assert_eq!(
        lifecycle
            .claim_with_identity()
            .await
            .expect("claim the fresh request"),
        crate::lifecycle::ClaimOutcome::Claimed,
        "this runtime must win the claim to own the pre-execution window"
    );
    let claimed = request_terminal_row(node.as_ref(), &request_doc_id)
        .await
        .expect("read the claimed request row");
    assert_eq!(
        claimed["lifecycle_state"], "claimed",
        "the window under test runs before execution begins: {claimed}"
    );

    let (interrupt_tx, mut interrupt_rx) =
        tokio::sync::watch::channel::<Option<crate::interrupt::InterruptIntent>>(None);
    let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let _observer = crate::interrupt::ClaimedRequestInterruptObserver::new(
        crate::interrupt::spawn_request_interrupt_observer(
            node.clone(),
            request_doc_id.clone(),
            interrupt_tx,
            shutdown_rx.clone(),
        ),
    );
    crate::interrupt::interrupt_request_by_doc_id(
        node.as_ref(),
        &request_doc_id,
        &agent_did,
        requester_did.as_deref(),
    )
    .await
    .expect("latch interrupt");
    tokio::time::timeout(Duration::from_secs(20), async {
        interrupt_rx
            .wait_for(|intent| intent.is_some())
            .await
            .map(|_| ())
    })
    .await
    .expect("the claim's observer must surface the durable latch")
    .expect("the observer channel stays open while the claim is held");

    let latched = request_terminal_row(node.as_ref(), &request_doc_id)
        .await
        .expect("read the latched request row");
    assert!(
        !latched["interrupt_requested_at"].is_null(),
        "the window decides on the durable latch: {latched}"
    );

    let outcome = daemon
        .handle_request(&mut lifecycle, &stream_writer, shutdown_rx, interrupt_rx)
        .await
        .expect("the claimed request's owner returns an outcome");
    let crate::agent::daemon::HandleRequestOutcome::Interrupted(evidence) = outcome else {
        panic!("an observable latch must interrupt the claimed request");
    };

    let undecided = request_terminal_row(node.as_ref(), &request_doc_id)
        .await
        .expect("read the pre-terminal request row");
    assert_eq!(
        undecided["lifecycle_state"], "claimed",
        "the admission window must decide before execution begins: {undecided}"
    );

    daemon
        .finish_interrupted_request(
            &mut lifecycle,
            &stream_writer,
            &request,
            evidence,
            "pre_inference",
        )
        .await;

    let terminal = request_terminal_row(node.as_ref(), &request_doc_id)
        .await
        .expect("read the terminalized request row");
    assert_eq!(
        terminal["lifecycle_state"], "interrupted",
        "the owner terminalizes the claimed request rather than leaving it for recovery: {terminal}"
    );
    assert!(
        !terminal["terminalized_at"].is_null(),
        "owner terminalization must stamp terminalized_at: {terminal}"
    );
    assert_eq!(
        terminal["failure_reason"],
        gents_protocol::request_lifecycle::interrupt_terminal_reason::BEFORE_ANY_PROVIDER_CALL,
        "a claim interrupted before execution began attempted no provider call: {terminal}"
    );
    assert_eq!(
        provider_calls.load(Ordering::SeqCst),
        0,
        "no provider call may run before execution begins"
    );
}
