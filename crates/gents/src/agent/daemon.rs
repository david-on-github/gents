use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use rig::completion::CompletionModel;
use tokio::sync::{mpsc, Mutex};
use tracing::Instrument;

mod inference;
mod request;
mod title;

use super::runtime::StartupBarrier;
use crate::agent::worker_capacity::{
    bind_current_claim, current_slot_capacity, scope_request_capacity, WorkerTicket,
};
use crate::compaction::{ProviderReductionEngine, ReductionEngine, ReductionOptions};
use crate::config::ResolvedBehavior;
use crate::hook::FailurePolicy;
use crate::lifecycle::{ClaimOutcome, RequestLifecycle, RequestTerminalOutcome, TerminalizeResult};
use crate::prompt::LayeredPromptBuilder;
use crate::runtime_trace::{
    record_current_claim_outcome, record_current_failure_class, record_current_request_outcome,
    RequestTraceAttrs,
};
use crate::streaming::DefraStreamWriter;
use crate::watcher::AgentRequest;

/// Only the winning terminal CAS authorizes follow-up effects. A matching
/// durable terminal row is an observation, not a second completion event.
async fn terminalize_request(
    lifecycle: &mut RequestLifecycle,
    stream_writer: &DefraStreamWriter,
    outcome: RequestTerminalOutcome,
    reason: Option<&str>,
) -> Result<bool> {
    let selection = stream_writer
        .terminal_output(&lifecycle.request().doc_id)
        .await;
    match lifecycle
        .terminalize_owned(outcome, selection, reason)
        .await?
    {
        TerminalizeResult::Won => Ok(true),
        TerminalizeResult::AlreadySame => Ok(false),
        TerminalizeResult::Lost => anyhow::bail!(
            "request {} lost its execution generation before terminalization",
            lifecycle.request().request_id,
        ),
    }
}

async fn finalize_request_failure(
    lifecycle: &mut RequestLifecycle,
    stream_writer: &DefraStreamWriter,
    reason: &str,
    request_id: &str,
) -> bool {
    match terminalize_request(
        lifecycle,
        stream_writer,
        RequestTerminalOutcome::Failed,
        Some(reason),
    )
    .await
    {
        Ok(won) => won,
        Err(error) => {
            record_current_request_outcome("terminalization_failed");
            record_current_failure_class(&error);
            tracing::error!(request_id, error = %error,
                "failed to atomically terminalize request and response; durable lease recovery remains pending");
            false
        }
    }
}

/// Authenticate the exact durable request immediately before claim. Admission
/// rejection is terminalized here so no caller can accidentally continue into
/// `claim_with_identity` or provider execution with the stale queued snapshot.
pub(crate) async fn verify_request_at_claim_boundary(
    verifier: &crate::request_admission::AgentRequestAdmissionVerifier,
    node: Arc<defra_node::EmbeddedNode>,
    behavior_id: &str,
    request: AgentRequest,
) -> Option<AgentRequest> {
    match verifier.verify_fresh(&request, behavior_id).await {
        Ok(verified) => Some(verified),
        Err(error) if error.is_denied() => {
            let reason = format!("request admission denied: {error:#}");
            record_current_claim_outcome("admission_denied");
            record_current_request_outcome("admission_denied");
            if let Err(persist_error) =
                crate::request_admission::terminalize_pending_request_rejection(
                    node.as_ref(),
                    &request.doc_id,
                    &request.agent_did,
                    &reason,
                    "terminalize_request_admission_rejection",
                )
                .await
            {
                tracing::error!(
                    request_id = %request.request_id,
                    error = %persist_error,
                    "failed to terminalize rejected AgentRequest after bounded retries"
                );
            }
            None
        }
        Err(error) => {
            record_current_claim_outcome("admission_unavailable");
            record_current_request_outcome("admission_retry");
            tracing::warn!(
                request_id = %request.request_id,
                error = %error,
                "request admission authority is temporarily unavailable; leaving request pending"
            );
            None
        }
    }
}

pub(super) struct BehaviorDaemon<M: CompletionModel> {
    node: Arc<defra_node::EmbeddedNode>,
    behavior: Arc<ResolvedBehavior>,
    provider_family: Option<String>,
    replay_issuer: Option<gents_loop::claude_messages_body::ReplayIssuer>,
    compaction_provider_family: Option<String>,
    model: Arc<M>,
    preamble: String,
    loop_tools: Arc<Vec<Box<dyn crate::llm::tool::ToolDyn>>>,
    prompt_builder: LayeredPromptBuilder,
    compactor: Arc<dyn ReductionEngine>,
    compaction_options: ReductionOptions,
    hook_failure_policy: FailurePolicy,
    rendered_request_capture_factory:
        Option<crate::rendered_request::RenderedRequestCaptureFactory>,
    background_tool_registry: crate::hook::BackgroundToolRegistry,
    background_execution_registry: crate::hook::BackgroundExecutionRegistry,
    remote_tools: Option<crate::document_config::RemoteTools>,
    output_obligations: Arc<Vec<(String, crate::document_config::WriteToolOutputObligation)>>,
    startup_barrier: Arc<StartupBarrier>,
    runtime_status: crate::runtime_status::RuntimeStatusHandle,
    slot_generation: u64,
    operator_tool_root: Option<PathBuf>,
    request_admission: crate::request_admission::AgentRequestAdmissionVerifier,
    root_execution_guard: Option<crate::tool_surface::RootExecutionGuard>,
}

enum HandleRequestOutcome {
    Completed,
    FailedAfterResponse(anyhow::Error),
    Interrupted,
}

/// The terminal this execution's owned work decided, before the task hook
/// phases react to it. `release_writer_binding` keeps each arm's existing
/// choice: an integrate failure retains its Active binding so a retry can
/// observe the pending commit-tree. An absent outcome means the work never
/// started, which releases the binding rather than retaining it.
struct OwnedWorkOutcome {
    observation: crate::task_hooks::OwnedWorkObservation,
    reason: Option<String>,
    release_writer_binding: bool,
}

impl OwnedWorkOutcome {
    fn relinquished() -> Self {
        Self {
            observation: crate::task_hooks::OwnedWorkObservation::Relinquished,
            reason: None,
            release_writer_binding: false,
        }
    }

    fn observed(
        result: crate::task_hooks::TaskAgentResult,
        reason: Option<String>,
        release_writer_binding: bool,
    ) -> Self {
        Self {
            observation: crate::task_hooks::OwnedWorkObservation::Observed(result),
            reason,
            release_writer_binding,
        }
    }
}

impl<M: CompletionModel + 'static> BehaviorDaemon<M> {
    pub(super) fn new(
        node: Arc<defra_node::EmbeddedNode>,
        behavior: Arc<ResolvedBehavior>,
        provider_family: Option<String>,
        model: Arc<M>,
        preamble: String,
        loop_tools: Arc<Vec<Box<dyn crate::llm::tool::ToolDyn>>>,
        prompt_builder: LayeredPromptBuilder,
        hook_failure_policy: FailurePolicy,
        rendered_request_capture_factory: Option<
            crate::rendered_request::RenderedRequestCaptureFactory,
        >,
        background_tool_registry: crate::hook::BackgroundToolRegistry,
        background_execution_registry: crate::hook::BackgroundExecutionRegistry,
        startup_barrier: Arc<StartupBarrier>,
        runtime_status: crate::runtime_status::RuntimeStatusHandle,
        slot_generation: u64,
        request_admission: crate::request_admission::AgentRequestAdmissionVerifier,
    ) -> Result<Self> {
        let mut compaction_config = crate::completion_factory::loop_config(
            behavior.as_ref(),
            preamble.clone(),
            0,
            crate::rendered_request::CaptureScopeKind::Compaction,
        );
        compaction_config.max_turns = 0;
        let compactor = Arc::new(ProviderReductionEngine::new(
            model.clone(),
            compaction_config,
        ));
        let compaction_options = crate::compaction::reduction_options_for_behavior(&behavior)?;

        Ok(Self {
            node,
            behavior,
            provider_family: provider_family.clone(),
            replay_issuer: None,
            compaction_provider_family: provider_family,
            model,
            preamble,
            loop_tools,
            prompt_builder,
            compactor,
            compaction_options,
            hook_failure_policy,
            rendered_request_capture_factory,
            background_tool_registry,
            background_execution_registry,
            remote_tools: None,
            output_obligations: Arc::new(Vec::new()),
            startup_barrier,
            runtime_status,
            slot_generation,
            operator_tool_root: None,
            request_admission,
            root_execution_guard: None,
        })
    }

    pub(super) fn with_compactor(
        mut self,
        compactor: Arc<dyn ReductionEngine>,
        provider_family: String,
    ) -> Self {
        self.compactor = compactor;
        self.compaction_provider_family = Some(provider_family);
        self
    }

    pub(super) fn with_replay_issuer(
        mut self,
        issuer: Option<gents_loop::claude_messages_body::ReplayIssuer>,
    ) -> Self {
        self.replay_issuer = issuer;
        self
    }

    pub(super) fn with_operator_tool_root(mut self, root: Option<PathBuf>) -> Self {
        crate::workspace::install_process_operator_tool_root(root.clone());
        self.operator_tool_root = root;
        self
    }

    pub(super) fn with_root_execution_guard(
        mut self,
        guard: Option<crate::tool_surface::RootExecutionGuard>,
    ) -> Self {
        self.root_execution_guard = guard;
        self
    }

    /// Attach the filesystem-policy observations assembled with a tool
    /// surface. Production RuntimeContext and focused owned-loop tests share
    /// this step so request-time root revalidation cannot be test-only wiring.
    pub(super) fn with_tool_surface_runtime_policy(
        self,
        guard: Option<crate::tool_surface::RootExecutionGuard>,
        operator_root: Option<PathBuf>,
    ) -> Self {
        self.with_root_execution_guard(guard)
            .with_operator_tool_root(operator_root)
    }

    /// Request-scoped compaction options: the daemon-lifetime knobs plus the
    /// claimed deadline of the request this compaction serves. The deadline is
    /// a required argument so no call site can omit it — the compactor's
    /// stored config is daemon-lifetime and carries no deadline, so this is
    /// the only path by which compaction recovery becomes deadline-aware
    /// (#1016). Both entry points force summarization: they fire only after
    /// the caller has established the assembled input is over budget.
    pub(super) fn compaction_options_for_request(
        &self,
        deadline: Option<chrono::DateTime<chrono::Utc>>,
        aggregate_token_budget: Option<crate::agent::loop_stream::AggregateTokenBudget>,
        sampling_seed: Option<i64>,
    ) -> ReductionOptions {
        ReductionOptions {
            deadline,
            aggregate_token_budget,
            sampling_seed,
            ..self.compaction_options.clone()
        }
    }

    pub(super) fn with_remote_tools(
        mut self,
        tools: Option<crate::document_config::RemoteTools>,
    ) -> Self {
        self.remote_tools = tools;
        self
    }

    pub(super) fn with_output_obligations(
        mut self,
        obligations: Vec<(String, crate::document_config::WriteToolOutputObligation)>,
    ) -> Self {
        self.output_obligations = Arc::new(obligations);
        self
    }

    pub(super) async fn run(
        &mut self,
        request_rx: Arc<Mutex<mpsc::Receiver<AgentRequest>>>,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> Result<()> {
        tracing::info!(
            behavior_id = %self.behavior.behavior_id,
            did = %self.behavior.agent_did(),
            model = %self.behavior.model_name,
            context_window = self.behavior.context_window,
            "gents behavior started"
        );

        if self
            .runtime_status
            .readiness()
            .mark_slot_ready(&self.behavior.behavior_id, self.slot_generation)
            .await?
        {
            self.startup_barrier
                .mark_behavior_ready(&self.behavior.behavior_id, self.slot_generation)
                .await;
        }
        tracing::info!(
            behavior_id = %self.behavior.behavior_id,
            did = %self.behavior.agent_did(),
            "gents behavior executor online"
        );

        loop {
            let request = tokio::select! {
                biased;

                _ = shutdown.changed() => {
                    tracing::info!(behavior_id = %self.behavior.behavior_id, "shutdown signal received");
                    return Ok(());
                }

                req = async {
                    let mut receiver = request_rx.lock().await;
                    receiver.recv().await
                } => {
                    match req {
                        Some(req) => req,
                        None => return Ok(()),
                    }
                }
            };

            if request.purpose == gents_protocol::request_admission::RequestPurpose::TitleAudit {
                self.spawn_title_audit_request(request, shutdown.clone());
                continue;
            }

            let trace_attrs = RequestTraceAttrs::from_request(&request);
            let behavior_id = self.behavior.behavior_id.clone();
            let backend_id = self.behavior.backend_id.clone().unwrap_or_default();

            // The slot's fixed workers may be idle or waiting on this shared
            // active semaphore. Dequeue happens first; no idle worker holds
            // active capacity. The unbound guard is retained across the whole
            // process future and bound to the generation only after claim.
            let active_guard = if let Some(capacity) = current_slot_capacity() {
                let cancellation = tokio_util::sync::CancellationToken::new();
                let guard = tokio::select! {
                    biased;
                    _ = shutdown.changed() => return Ok(()),
                    guard = capacity.acquire_unbound(&cancellation) => guard,
                };
                match guard {
                    Ok(guard) => Some(guard),
                    Err(error) => {
                        tracing::warn!(behavior_id, error = %error, "request worker capacity admission stopped");
                        return Ok(());
                    }
                }
            } else {
                None
            };

            let process =
                self.process_request(request, shutdown.clone())
                    .instrument(tracing::info_span!(
                        "agent.request",
                        request_doc_id = %trace_attrs.request_doc_id,
                        request_id = %trace_attrs.request_id,
                        session_id = %trace_attrs.session_id,
                        agent_did = %trace_attrs.agent_did,
                        behavior_id = %behavior_id,
                        requested_behavior_id = %trace_attrs.requested_behavior_id,
                        backend_id = %backend_id,
                        execution_origin = %trace_attrs.execution_origin,
                        persisted_execution_origin = %trace_attrs.execution_origin,
                        deadline_at = %trace_attrs.deadline_at,
                        has_deadline = trace_attrs.has_deadline,
                        subagent_depth = trace_attrs.subagent_depth,
                        is_subagent = trace_attrs.is_subagent,
                        parent_request_id = %trace_attrs.parent_request_id,
                        parent_tool_call_id = %trace_attrs.parent_tool_call_id,
                        selected_skill_count = trace_attrs.selected_skill_count,
                        workspace_cwd_set = trace_attrs.workspace_cwd_set,
                        claim_outcome = tracing::field::Empty,
                        request_outcome = tracing::field::Empty,
                        failure_class = tracing::field::Empty,
                    ));
            if let Some(guard) = active_guard {
                scope_request_capacity(guard, process).await;
            } else {
                process.await;
            }
        }
    }

    pub(in crate::agent) async fn process_request(
        &mut self,
        request: AgentRequest,
        shutdown: tokio::sync::watch::Receiver<bool>,
    ) {
        // Publication and terminal selection belong to this one execution.
        // Dropping the request future also drops its in-memory projection;
        // durable transcript and recovery state remain in DefraDB.
        let stream_writer = DefraStreamWriter::new(
            self.node.clone(),
            self.behavior.agent_did(),
            Duration::from_millis(self.behavior.stream_batch_ms),
        );
        let Some(request) = verify_request_at_claim_boundary(
            &self.request_admission,
            self.node.clone(),
            &self.behavior.behavior_id,
            request,
        )
        .await
        else {
            return;
        };
        let execution_origin =
            crate::lifecycle::ExecutionOrigin::from_persisted(request.execution_origin.as_deref())
                .expect("fresh request admission requires a canonical execution_origin");
        let mut lifecycle = RequestLifecycle::new_with_execution_binding(
            self.node.clone(),
            &self.behavior.behavior_id,
            self.behavior.agent_did(),
            request.clone(),
            self.behavior.deadline_duration.as_secs(),
            execution_origin,
            self.behavior.backend_id.clone().unwrap_or_default(),
        );
        lifecycle.set_execution_lease_duration(self.behavior.stream_liveness_timeout);
        lifecycle.set_configured_max_total_tokens(self.behavior.max_total_tokens);

        let claim_result = lifecycle
            .claim_with_identity()
            .instrument(tracing::info_span!(
                "request.claim",
                request_id = %request.request_id,
                session_id = %request.session_id,
                agent_did = %request.agent_did,
                behavior_id = %self.behavior.behavior_id,
            ))
            .await;

        match claim_result {
            Ok(ClaimOutcome::Claimed) => {
                record_current_claim_outcome("claimed");
                let generation = match lifecycle.execution_generation() {
                    Ok(generation) => generation.to_owned(),
                    Err(error) => {
                        record_current_request_outcome("worker_ticket_missing");
                        record_current_failure_class(&error);
                        let _ = finalize_request_failure(
                            &mut lifecycle,
                            &stream_writer,
                            &error.to_string(),
                            &request.request_id,
                        )
                        .await;
                        return;
                    }
                };
                let ticket = WorkerTicket::new(lifecycle.request().doc_id.clone(), generation);
                if let Err(error) = bind_current_claim(ticket) {
                    let error = anyhow::Error::new(error);
                    record_current_request_outcome("worker_ticket_refused");
                    record_current_failure_class(&error);
                    let _ = finalize_request_failure(
                        &mut lifecycle,
                        &stream_writer,
                        &error.to_string(),
                        &request.request_id,
                    )
                    .await;
                    return;
                }
            }
            Ok(ClaimOutcome::Queued) => {
                record_current_claim_outcome("queued");
                record_current_request_outcome("queued");
                tracing::info!(
                    behavior_id = %self.behavior.behavior_id,
                    request_id = %request.request_id,
                    session_id = %request.session_id,
                    "request queued behind an earlier same-session request"
                );
                return;
            }
            Ok(ClaimOutcome::Interrupted) => {
                record_current_claim_outcome("interrupted");
                record_current_request_outcome("interrupted_pre_claim");
                tracing::info!(
                    behavior_id = %self.behavior.behavior_id,
                    request_id = %request.request_id,
                    session_id = %request.session_id,
                    cancellation_source = "pre_claim",
                    "request interrupted before claim"
                );
                return;
            }
            Ok(ClaimOutcome::Expired) => {
                record_current_claim_outcome("expired");
                record_current_request_outcome("expired_pre_claim");
                tracing::info!(
                    behavior_id = %self.behavior.behavior_id,
                    request_id = %request.request_id,
                    session_id = %request.session_id,
                    cancellation_source = "stale_ttl",
                    "request expired (valid_until passed) before claim; marked dead"
                );
                return;
            }
            Err(error) => {
                record_current_claim_outcome("error");
                let deterministic_rejection = crate::lifecycle::is_claim_admission_error(&error);
                record_current_request_outcome(if deterministic_rejection {
                    "admission_rejected"
                } else {
                    "claim_error"
                });
                record_current_failure_class(&error);
                if deterministic_rejection {
                    tracing::warn!(
                        behavior_id = %self.behavior.behavior_id,
                        request_id = %request.request_id,
                        error = %error,
                        "rejecting request with an invalid canonical admission binding"
                    );
                    if let Err(rejection_error) =
                        lifecycle.reject_admission(&error.to_string()).await
                    {
                        tracing::error!(
                            behavior_id = %self.behavior.behavior_id,
                            request_id = %request.request_id,
                            error = %rejection_error,
                            "failed to persist request admission rejection"
                        );
                    }
                    return;
                }
                tracing::warn!(
                    behavior_id = %self.behavior.behavior_id,
                    request_id = %request.request_id,
                    error = %error,
                    "failed to claim request; leaving it pending for retry"
                );
                return;
            }
        }

        let requested_behavior_id = request.behavior_id.as_str();
        if requested_behavior_id != self.behavior.behavior_id {
            let error = anyhow::anyhow!(
                "request targets behavior {} but runtime is serving behavior {}",
                requested_behavior_id,
                self.behavior.behavior_id
            );
            record_current_request_outcome("rejected_behavior_mismatch");
            record_current_failure_class(&error);
            tracing::warn!(
                behavior_id = %self.behavior.behavior_id,
                request_id = %request.request_id,
                session_id = %request.session_id,
                requested_behavior_id = %requested_behavior_id,
                "rejecting request for unroutable behavior"
            );
            finalize_request_failure(
                &mut lifecycle,
                &stream_writer,
                &error.to_string(),
                &request.request_id,
            )
            .await;
            return;
        }

        match crate::workspace::writer_request_already_sealed(self.node.as_ref(), &request).await {
            // This completes work an earlier execution already observed and
            // sealed. Task hooks stay out of it: re-running their host commands
            // would replay effects the hook contract refuses to replay, and no
            // durable attempt record exists to select the unobserved ones.
            Ok(true) => {
                if let Err(error) = lifecycle.begin_owned_execution(&stream_writer).await {
                    finalize_request_failure(
                        &mut lifecycle,
                        &stream_writer,
                        &error.to_string(),
                        &request.request_id,
                    )
                    .await;
                    return;
                }
                record_current_request_outcome("completed");
                if let Err(error) = lifecycle.validate_owned_execution().await {
                    tracing::warn!(request_id = %request.request_id, %error, "stopping workspace completion after execution ownership loss");
                    return;
                }
                if let Err(error) = crate::workspace::seal_on_writer_success(
                    self.node.as_ref(),
                    &request,
                    self.operator_tool_root.as_deref(),
                )
                .await
                {
                    record_current_failure_class(&error);
                    tracing::error!(
                        request_id = %request.request_id,
                        error = %error,
                        "failed to repair workspace seal after writer success"
                    );
                    finalize_request_failure(
                        &mut lifecycle,
                        &stream_writer,
                        &error.to_string(),
                        &request.request_id,
                    )
                    .await;
                    return;
                }
                if let Err(error) = terminalize_request(
                    &mut lifecycle,
                    &stream_writer,
                    RequestTerminalOutcome::Completed,
                    None,
                )
                .await
                {
                    record_current_request_outcome("terminalization_failed");
                    record_current_failure_class(&error);
                    tracing::error!(request_id = %request.request_id, error = %error,
                        "failed to atomically terminalize completed request and response");
                }
                return;
            }
            Ok(false) => {}
            Err(error) => {
                record_current_failure_class(&error);
                tracing::error!(
                    request_id = %request.request_id,
                    error = %error,
                    "failed to inspect writer workspace seal state"
                );
                finalize_request_failure(
                    &mut lifecycle,
                    &stream_writer,
                    &error.to_string(),
                    &request.request_id,
                )
                .await;
                return;
            }
        }

        let hooks =
            match crate::task_hooks::resolve_request_task_hooks(self.node.as_ref(), &request).await
            {
                Ok(hooks) => hooks,
                Err(error) => {
                    record_current_request_outcome("task_hooks_unresolved");
                    record_current_failure_class(&error);
                    tracing::error!(
                        behavior_id = %self.behavior.behavior_id,
                        request_id = %request.request_id,
                        error = %error,
                        "refusing to run a request whose configured task hooks cannot be resolved"
                    );
                    self.finalize_failure_before_work(
                        &mut lifecycle,
                        &stream_writer,
                        &error.to_string(),
                        &request,
                    )
                    .await;
                    return;
                }
            };
        // No configured occurrence reaches the host executor when the task has
        // none, so the root is resolved and revalidated only when one exists.
        let hook_cwd = if hooks.is_empty() {
            PathBuf::new()
        } else {
            match self.task_hook_cwd().await {
                Ok(cwd) => cwd,
                Err(error) => {
                    record_current_request_outcome("task_hook_root_unavailable");
                    record_current_failure_class(&error);
                    tracing::error!(
                        behavior_id = %self.behavior.behavior_id,
                        request_id = %request.request_id,
                        error = %error,
                        "refusing to run task hooks without an admitted host-tools root"
                    );
                    self.finalize_failure_before_work(
                        &mut lifecycle,
                        &stream_writer,
                        &error.to_string(),
                        &request,
                    )
                    .await;
                    return;
                }
            }
        };
        let hook_cancellation = tokio_util::sync::CancellationToken::new();
        let hook_shutdown = (!hooks.is_empty()).then(|| {
            let cancellation = hook_cancellation.clone();
            let mut shutdown = shutdown.clone();
            tokio::spawn(async move {
                while shutdown.changed().await.is_ok() {
                    if *shutdown.borrow() {
                        cancellation.cancel();
                        return;
                    }
                }
            })
        });
        let hook_exec = crate::task_hooks::ManagedTaskHookExec::new(hook_cwd, hook_cancellation);

        let mut work_decision: Option<(Option<String>, bool)> = None;
        let run = crate::task_hooks::run_task_hooks(&hooks, &hook_exec, || async {
            let outcome = self
                .observe_owned_work(&mut lifecycle, &stream_writer, &request, shutdown)
                .await;
            work_decision = Some((outcome.reason, outcome.release_writer_binding));
            outcome.observation
        })
        .await;
        if let Some(handle) = hook_shutdown {
            handle.abort();
        }
        let Some(run) = run else {
            return;
        };
        let (work_reason, release_writer_binding) = work_decision.unwrap_or((None, true));

        let final_outcome = run.final_outcome();
        if run.agent_result == Some(crate::task_hooks::TaskAgentResult::Success)
            && final_outcome != crate::task_hooks::TaskHookOutcome::Success
        {
            record_current_request_outcome("task_hook_gate_failed");
        }
        let mut reason = match &final_outcome {
            crate::task_hooks::TaskHookOutcome::Failure(
                crate::task_hooks::HookPrimaryError::Hook(hook_id),
            ) => Some(run.hook_failure_reason(hook_id)),
            _ => work_reason,
        };
        let cleanup_errors = run.cleanup_errors();
        if !cleanup_errors.is_empty() {
            let note = format!("task cleanup hooks failed: {}", cleanup_errors.join(", "));
            tracing::error!(
                behavior_id = %self.behavior.behavior_id,
                request_id = %request.request_id,
                cleanup_errors = %cleanup_errors.join(","),
                "task cleanup hooks failed"
            );
            reason = Some(reason.map_or(note.clone(), |reason| format!("{reason}\n{note}")));
        }

        let terminal = final_outcome.terminal_outcome();
        match final_outcome {
            crate::task_hooks::TaskHookOutcome::Success => {
                if let Err(error) =
                    terminalize_request(&mut lifecycle, &stream_writer, terminal, None).await
                {
                    record_current_request_outcome("terminalization_failed");
                    record_current_failure_class(&error);
                    tracing::error!(request_id = %request.request_id, error = %error,
                        "failed to atomically terminalize completed request and response");
                }
            }
            crate::task_hooks::TaskHookOutcome::Failure(_) => {
                let reason = reason.unwrap_or_else(|| "request failed".to_string());
                if finalize_request_failure(
                    &mut lifecycle,
                    &stream_writer,
                    &reason,
                    &request.request_id,
                )
                .await
                    && release_writer_binding
                {
                    self.release_failed_writer_binding(&request).await;
                }
            }
            crate::task_hooks::TaskHookOutcome::Interrupted => {
                record_current_request_outcome("interrupted");
                let reason = reason.unwrap_or_else(|| "interrupted".to_string());
                match terminalize_request(&mut lifecycle, &stream_writer, terminal, Some(&reason))
                    .await
                {
                    Ok(true) => {}
                    Ok(false) => return,
                    Err(error) => {
                        record_current_request_outcome("terminalization_failed");
                        record_current_failure_class(&error);
                        tracing::error!(request_id = %request.request_id, error = %error,
                            "failed to atomically terminalize interrupted request and response");
                        return;
                    }
                }
                if let Err(error) =
                    crate::workspace::release_writer_binding(self.node.as_ref(), &request).await
                {
                    tracing::warn!(
                        request_id = %request.request_id,
                        error = %error,
                        "failed to release writer workspace binding after interrupt"
                    );
                }
                tracing::info!(
                    behavior_id = %self.behavior.behavior_id,
                    request_id = %request.request_id,
                    session_id = %request.session_id,
                    cancellation_source = "mid_flight",
                    "request interrupted mid-flight"
                );
            }
        }
    }

    /// A workspace-bound automated request receives its writer binding in the
    /// trigger materializer, before this execution exists. Base freeze refuses
    /// an Active writer binding even once its request is terminal, so a failure
    /// that terminalizes before the owned work ran must release it, and only
    /// the winning terminal CAS may.
    async fn finalize_failure_before_work(
        &self,
        lifecycle: &mut RequestLifecycle,
        stream_writer: &DefraStreamWriter,
        reason: &str,
        request: &AgentRequest,
    ) {
        if finalize_request_failure(lifecycle, stream_writer, reason, &request.request_id).await {
            self.release_failed_writer_binding(request).await;
        }
    }

    async fn release_failed_writer_binding(&self, request: &AgentRequest) {
        if let Err(error) =
            crate::workspace::release_writer_binding(self.node.as_ref(), request).await
        {
            tracing::warn!(
                request_id = %request.request_id,
                error = %error,
                "failed to release writer workspace binding after failure"
            );
        }
    }

    /// Host commands run from the behavior's host-tools root, revalidated
    /// through its existing admission owner, so a task hook cannot select a
    /// workspace overlay of its own; workspace association for general hooks
    /// remains an open design question.
    async fn task_hook_cwd(&self) -> Result<PathBuf> {
        if let Some(guard) = &self.root_execution_guard {
            guard.validate(&self.node).await?;
            if let Some(root) = guard.selected_root.clone() {
                return Ok(root);
            }
        }
        std::env::current_dir()
            .map_err(|error| anyhow::anyhow!("resolving the runtime cwd for a task hook: {error}"))
    }

    async fn observe_owned_work(
        &mut self,
        lifecycle: &mut RequestLifecycle,
        stream_writer: &DefraStreamWriter,
        request: &AgentRequest,
        shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> OwnedWorkOutcome {
        use crate::task_hooks::TaskAgentResult;

        let (interrupt_tx, interrupt_rx) =
            tokio::sync::watch::channel::<Option<crate::interrupt::InterruptIntent>>(None);
        let observer = crate::interrupt::spawn_request_interrupt_observer(
            self.node.clone(),
            request.doc_id.clone(),
            interrupt_tx,
            shutdown.clone(),
        );

        let result = self
            .handle_request(lifecycle, stream_writer, shutdown, interrupt_rx)
            .await;
        observer.abort();

        match result {
            Ok(HandleRequestOutcome::Completed) => {
                record_current_request_outcome("completed");
                if let Err(error) = lifecycle.validate_owned_execution().await {
                    tracing::warn!(request_id = %request.request_id, %error, "stopping workspace completion after execution ownership loss");
                    return OwnedWorkOutcome::relinquished();
                }
                if let Err(error) = crate::workspace::seal_on_writer_success(
                    self.node.as_ref(),
                    request,
                    self.operator_tool_root.as_deref(),
                )
                .await
                {
                    record_current_failure_class(&error);
                    tracing::error!(
                        request_id = %request.request_id,
                        error = %error,
                        "failed to seal workspace after writer success"
                    );
                    return OwnedWorkOutcome::observed(
                        TaskAgentResult::Failure,
                        Some(error.to_string()),
                        true,
                    );
                }
                if let Err(error) = lifecycle.validate_owned_execution().await {
                    tracing::warn!(request_id = %request.request_id, %error, "stopping workspace integration after execution ownership loss");
                    return OwnedWorkOutcome::relinquished();
                }
                if let Err(error) = crate::workspace::integrate_on_integrator_success(
                    self.node.as_ref(),
                    request,
                    self.operator_tool_root.as_deref(),
                )
                .await
                {
                    record_current_failure_class(&error);
                    tracing::error!(
                        request_id = %request.request_id,
                        error = %error,
                        "failed to integrate workspace after integrator success"
                    );
                    // Keep the Active Integrate binding so a retry can observe
                    // a pending commit-tree and write the durable receipt.
                    return OwnedWorkOutcome::observed(
                        TaskAgentResult::Failure,
                        Some(error.to_string()),
                        false,
                    );
                }
                OwnedWorkOutcome::observed(TaskAgentResult::Success, None, false)
            }
            // The owned loop returns this only with an interrupt latch
            // present, so the observation is a cancellation of active work,
            // not an unknown outcome.
            Ok(HandleRequestOutcome::Interrupted) => {
                OwnedWorkOutcome::observed(TaskAgentResult::Cancelled, None, false)
            }
            Ok(HandleRequestOutcome::FailedAfterResponse(error)) => {
                record_current_request_outcome("failed_after_response");
                record_current_failure_class(&error);
                tracing::error!(
                    behavior_id = %self.behavior.behavior_id,
                    request_id = %request.request_id,
                    error = %error,
                    "request failed after response started"
                );
                OwnedWorkOutcome::observed(TaskAgentResult::Failure, Some(error.to_string()), true)
            }
            Err(error) => {
                record_current_request_outcome("failed");
                record_current_failure_class(&error);
                tracing::error!(
                    behavior_id = %self.behavior.behavior_id,
                    request_id = %request.request_id,
                    error = %error,
                    "request handling failed"
                );
                OwnedWorkOutcome::observed(TaskAgentResult::Failure, Some(error.to_string()), true)
            }
        }
    }
}
