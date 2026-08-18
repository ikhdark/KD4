use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Instant;

use tokio::sync::RwLock;
use tokio::task::JoinError;
use tokio::time::Instant as TokioInstant;
use tokio_util::either::Either;
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;
use tracing::Instrument;
use tracing::info;
use tracing::instrument;
use tracing::trace_span;

use crate::agent::task_capabilities::TypedToolClass;
use crate::agent::task_capabilities::classify_typed_tool;
use crate::function_tool::FunctionCallError;
use crate::session::reasoning_governor::CodeModeToolResult;
use crate::session::reasoning_governor::SamplingRequestSignalCollector;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::turn_context::TurnContext;
use crate::tools::context::AbortedToolOutput;
use crate::tools::context::SharedTurnDiffTracker;
use crate::tools::context::ToolPayload;
use crate::tools::lifecycle::notify_tool_aborted;
use crate::tools::registry::AnyToolResult;
use crate::tools::registry::ToolArgumentDiffConsumer;
use crate::tools::router::ToolCall;
use crate::tools::router::ToolCallSource;
use crate::tools::tool_dispatch_trace::ToolDispatchTiming;
use crate::tools::tool_dispatch_trace::scope_tool_dispatch_timing;
use codex_protocol::error::CodexErr;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::ResponseItem;

fn reused_failure_diagnosis(
    tool_name: &codex_tools::ToolName,
    failure_fingerprint: &str,
) -> String {
    let next_action = if tool_name.name == "write_stdin" {
        "Do not poll this session again with unchanged arguments; inspect process state, wait materially longer, or use a different observation route."
    } else {
        "Do not repeat this call with unchanged arguments; change the action or relevant state before the next call."
    };
    serde_json::json!({
        "kind": "reused_failure_diagnosis",
        "failure_fingerprint": failure_fingerprint,
        "retryable": false,
        "required_action": "change_route_or_state",
        "reason": "this exact action already produced the same stable failure against unchanged state; the prior diagnosis remains authoritative",
        "next_action": next_action,
    })
    .to_string()
}

struct ToolCallTimingGuard {
    timing: Arc<ToolDispatchTiming>,
    conversation_id: String,
    turn_id: String,
    call_id: String,
    tool_name: codex_tools::ToolName,
}

#[derive(Clone)]
pub(crate) struct ToolCallRuntime {
    session: Arc<Session>,
    // Tool calls may run later, so retain the step whose tool list advertised them.
    step_context: Arc<StepContext>,
    tracker: SharedTurnDiffTracker,
    parallel_execution: Arc<RwLock<()>>,
    sampling_request_signals: Option<SamplingRequestSignalCollector>,
}

fn workspace_tool_may_use_parallel_gate(supports_parallel: bool, observes_workspace: bool) -> bool {
    supports_parallel && !observes_workspace
}

fn guarded_workspace_evidence_is_current(
    revision_before: Option<&crate::git_workspace::WorkspaceEvidenceIdentity>,
    revision_after: Option<&crate::git_workspace::WorkspaceEvidenceIdentity>,
) -> bool {
    revision_before.is_some() && revision_before == revision_after
}

impl ToolCallRuntime {
    pub(crate) fn new(
        session: Arc<Session>,
        step_context: Arc<StepContext>,
        tracker: SharedTurnDiffTracker,
    ) -> Self {
        Self {
            session,
            step_context,
            tracker,
            parallel_execution: Arc::new(RwLock::new(())),
            sampling_request_signals: None,
        }
    }

    pub(crate) fn with_sampling_request_signals(
        mut self,
        collector: SamplingRequestSignalCollector,
    ) -> Self {
        self.sampling_request_signals = Some(collector);
        self
    }

    async fn register_workspace_evidence_for_response(
        &self,
        call: &ToolCall,
        response: &ResponseInputItem,
        revision_before: Option<Option<crate::git_workspace::WorkspaceEvidenceIdentity>>,
        _mutation_advanced: bool,
        source_dependencies_override: Option<
            std::collections::BTreeSet<crate::tool_history::SourceDependencyV1>,
        >,
    ) {
        let _guard = Arc::clone(&self.parallel_execution).read_owned().await;
        if !crate::tool_history::tool_call_observes_workspace(
            call.tool_name.name.as_str(),
            &call.payload,
        ) {
            return;
        }
        let workspace_cwd = crate::tool_history::workspace_evidence_cwd_for_tool_call(
            call.tool_name.name.as_str(),
            &call.payload,
            self.step_context.turn.config.cwd.as_path(),
        );
        let revision = self
            .session
            .services
            .git_workspace
            .workspace_evidence_identity(&workspace_cwd)
            .await;
        // A mutation revision may belong to a different call that acquired the
        // execution gate before this response could register its evidence.
        // Only matching content identities prove that this output was captured
        // against the revision being recorded.
        let captured_current =
            revision_before.is_some_and(|before| before.is_some() && before == revision);
        Self::register_workspace_evidence_observation(
            self.session.as_ref(),
            self.step_context.turn.as_ref(),
            call,
            response,
            revision,
            captured_current,
            source_dependencies_override,
        )
        .await;
    }

    async fn register_workspace_evidence_while_guarded(
        session: &Session,
        turn: &TurnContext,
        call: &ToolCall,
        response: &ResponseInputItem,
        revision_before: Option<crate::git_workspace::WorkspaceEvidenceIdentity>,
        _mutation_advanced: bool,
    ) {
        if !crate::tool_history::tool_call_observes_workspace(
            call.tool_name.name.as_str(),
            &call.payload,
        ) {
            return;
        }
        let workspace_cwd = crate::tool_history::workspace_evidence_cwd_for_tool_call(
            call.tool_name.name.as_str(),
            &call.payload,
            turn.config.cwd.as_path(),
        );
        let revision = session
            .services
            .git_workspace
            .workspace_evidence_identity(&workspace_cwd)
            .await;
        let captured_current =
            guarded_workspace_evidence_is_current(revision_before.as_ref(), revision.as_ref());
        Self::register_workspace_evidence_observation(
            session,
            turn,
            call,
            response,
            revision,
            captured_current,
            None,
        )
        .await;
    }

    async fn register_workspace_evidence_observation(
        session: &Session,
        turn: &TurnContext,
        call: &ToolCall,
        response: &ResponseInputItem,
        mut revision: Option<crate::git_workspace::WorkspaceEvidenceIdentity>,
        mut captured_current: bool,
        source_dependencies_override: Option<
            std::collections::BTreeSet<crate::tool_history::SourceDependencyV1>,
        >,
    ) {
        let workspace_cwd = crate::tool_history::workspace_evidence_cwd_for_tool_call(
            call.tool_name.name.as_str(),
            &call.payload,
            turn.config.cwd.as_path(),
        );
        let source_dependencies = source_dependencies_override.unwrap_or_else(|| {
            crate::tool_history::source_dependencies_for_tool_call(
                call.tool_name.name.as_str(),
                &call.payload,
                turn.config.cwd.as_path(),
            )
        });
        let source_path_observations = revision
            .as_ref()
            .and_then(|revision| revision.repository_root.as_deref())
            .and_then(|repo_root| {
                source_dependencies
                    .iter()
                    .map(|dependency| {
                        session
                            .services
                            .git_workspace
                            .begin_source_path_change_observation(
                                std::path::Path::new(repo_root),
                                std::path::Path::new(&dependency.path),
                                dependency.recursive,
                            )
                    })
                    .collect::<Option<Vec<_>>>()
            })
            .unwrap_or_default();
        if !source_path_observations.is_empty() {
            let verified_revision = session
                .services
                .git_workspace
                .workspace_evidence_identity(&workspace_cwd)
                .await;
            captured_current &= verified_revision == revision;
            revision = verified_revision;
        }
        let Some(observation) =
            crate::tool_history::WorkspaceEvidenceObservation::from_response_item_with_freshness(
                revision,
                &ResponseItem::from(response.clone()),
                source_dependencies,
                captured_current,
            )
            .map(|observation| observation.with_source_path_observations(source_path_observations))
        else {
            return;
        };
        session
            .register_workspace_evidence(turn.config.codex_home.as_path(), observation)
            .await;
    }

    pub(crate) fn record_code_mode_result(
        &self,
        mut result: CodeModeToolResult<'_>,
        receipts: &[codex_protocol::protocol::TurnTimingDeterministicContinuationReceipt],
    ) {
        let Some(collector) = &self.sampling_request_signals else {
            return;
        };
        result.source_dependencies = crate::tool_history::tool_call_observes_workspace(
            result.tool_name.name.as_str(),
            result.payload,
        )
        .then(|| {
            crate::tool_history::source_dependencies_for_tool_call(
                result.tool_name.name.as_str(),
                result.payload,
                self.step_context.turn.config.cwd.as_path(),
            )
        });
        collector.record_code_mode_result(result);
        collector.record_accepted_deterministic_continuation_receipts(receipts);
    }

    pub(crate) fn record_code_mode_failure(
        &self,
        cell_id: &str,
        tool_name: &codex_tools::ToolName,
        payload: Option<&ToolPayload>,
        failure_fingerprint: String,
    ) {
        let Some(collector) = &self.sampling_request_signals else {
            return;
        };
        let source_dependencies =
            crate::tool_history::tool_observes_workspace(tool_name.name.as_str()).then(|| {
                payload.map_or_else(std::collections::BTreeSet::new, |payload| {
                    crate::tool_history::source_dependencies_for_tool_call(
                        tool_name.name.as_str(),
                        payload,
                        self.step_context.turn.config.cwd.as_path(),
                    )
                })
            });
        collector.record_code_mode_failure(
            cell_id,
            tool_name,
            payload,
            source_dependencies,
            failure_fingerprint,
        );
    }

    pub(crate) fn create_diff_consumer(
        &self,
        tool_name: &codex_tools::ToolName,
    ) -> Option<Box<dyn ToolArgumentDiffConsumer>> {
        self.step_context
            .tool_router()?
            .create_diff_consumer(tool_name)
    }

    /// Centralized eligibility predicate for starting a read while the current
    /// model response is still streaming. A rejection closes the eligible prefix.
    pub(crate) fn take_eager_read_eligibility(
        &self,
        call: &ToolCall,
        earlier_calls_eligible: &mut bool,
    ) -> bool {
        let collaboration_namespace = self
            .step_context
            .turn
            .provider
            .capabilities()
            .namespace_tools
            .then_some(
                self.step_context
                    .turn
                    .config
                    .multi_agent_v2
                    .tool_namespace
                    .as_deref(),
            )
            .flatten();
        let eligible = *earlier_calls_eligible
            && matches!(
                classify_typed_tool(
                    call.tool_name.namespace.as_deref(),
                    &call.tool_name.name,
                    collaboration_namespace,
                ),
                TypedToolClass::ReadSearch
            )
            && self
                .step_context
                .tool_router()
                .is_some_and(|router| router.tool_supports_parallel(call));
        *earlier_calls_eligible = eligible;
        eligible
    }

    #[cfg(test)]
    #[instrument(level = "trace", skip_all)]
    pub(crate) fn handle_tool_call(
        self,
        call: ToolCall,
        cancellation_token: CancellationToken,
    ) -> impl std::future::Future<Output = Result<ResponseInputItem, CodexErr>> {
        self.handle_tool_call_with_timing(
            call,
            cancellation_token,
            TokioInstant::now(),
            /*eager*/ false,
        )
    }

    pub(crate) fn handle_tool_call_with_timing(
        self,
        call: ToolCall,
        cancellation_token: CancellationToken,
        item_accepted_at: TokioInstant,
        eager: bool,
    ) -> impl std::future::Future<Output = Result<ResponseInputItem, CodexErr>> {
        let timing = Arc::new(ToolDispatchTiming::new(item_accepted_at, eager));
        timing.mark_first_poll();
        let collaboration_namespace = self
            .step_context
            .turn
            .provider
            .capabilities()
            .namespace_tools
            .then_some(
                self.step_context
                    .turn
                    .config
                    .multi_agent_v2
                    .tool_namespace
                    .as_deref(),
            )
            .flatten();
        let tool_class = classify_typed_tool(
            call.tool_name.namespace.as_deref(),
            &call.tool_name.name,
            collaboration_namespace,
        );
        let authoritative_direct_wait = matches!(&tool_class, TypedToolClass::AgentCommunication);
        let signal_registration = self.sampling_request_signals.as_ref().map(|collector| {
            collector.register_deterministic_tool_call(
                &call.tool_name,
                &call.payload,
                &call.call_id,
            )
        });
        let signal_collector = self.sampling_request_signals.clone();
        async move {
            if let Some(registration) = signal_registration.as_ref()
                && let Some(guard) = registration.suppressed_failure.as_ref()
            {
                let mut output = FunctionCallOutputPayload::from_text(reused_failure_diagnosis(
                    &call.tool_name,
                    &guard.failure_fingerprint,
                ));
                output.success = Some(false);
                let response = ResponseInputItem::FunctionCallOutput {
                    call_id: call.call_id.clone(),
                    output,
                };
                if let Some(signal_collector) = signal_collector.as_ref() {
                    signal_collector.record_suppressed_failure(
                        registration.ordinal,
                        &guard.failure_fingerprint,
                    );
                }
                self.register_workspace_evidence_for_response(&call, &response, None, false, None)
                    .await;
                return Ok(response);
            }
            if let Some(registration) = signal_registration.as_ref()
                && let Some(guard) = registration.suppressed_source_pass.as_ref()
            {
                let mut output = FunctionCallOutputPayload::from_text(
                    serde_json::json!({
                        "kind": "unchanged_source_pass_suppression",
                        "reason": "the same broad source action is blocked because the active obligation, evidence identity, and action are unchanged; change one of them before another broad source pass",
                    })
                    .to_string(),
                );
                output.success = Some(true);
                let response = ResponseInputItem::FunctionCallOutput {
                    call_id: call.call_id.clone(),
                    output,
                };
                if let Some(signal_collector) = signal_collector.as_ref() {
                    signal_collector.record_suppressed_source_pass(
                        registration.ordinal,
                        &guard.evidence_identity,
                    );
                }
                self.register_workspace_evidence_for_response(&call, &response, None, false, None)
                    .await;
                return Ok(response);
            }
            if let Some(registration) = signal_registration.as_ref()
                && let Some(guard) = registration.blocked_wait_guard.as_ref()
                && let Some(snapshot) = crate::tools::handlers::multi_agents_v2::wait::inspect_authoritative_wait_snapshot(
                        self.session.as_ref(),
                        self.step_context.turn.as_ref(),
                        &guard.assignment_ids,
                    )
                    .await
            {
                if snapshot.owner == guard.owner
                    && snapshot.state_revision == guard.state_revision
                {
                    let mut output = FunctionCallOutputPayload::from_text(
                        serde_json::json!({
                            "kind": "authoritative_wait_suppression",
                            "disposition": "blocked",
                            "owner": guard.owner,
                            "state_revision": guard.state_revision,
                            "reason": "the exact authoritative wait remains blocked at the same owner revision; act on the blocker or report it",
                        })
                        .to_string(),
                    );
                    output.success = Some(true);
                    let response = ResponseInputItem::FunctionCallOutput {
                        call_id: call.call_id.clone(),
                        output,
                    };
                    if let Some(signal_collector) = signal_collector.as_ref() {
                        signal_collector
                            .record_suppressed_result(registration.ordinal, &response);
                    }
                    return Ok(response);
                }
                if let Some(signal_collector) = signal_collector.as_ref() {
                    signal_collector
                        .clear_blocked_wait_guard(&guard.owner, &guard.state_revision);
                }
            }
            let signal_ordinal = signal_registration
                .as_ref()
                .map(|registration| registration.ordinal);
            let observes_workspace = crate::tool_history::tool_call_observes_workspace(
                call.tool_name.name.as_str(),
                &call.payload,
            );
            let workspace_cwd = crate::tool_history::workspace_evidence_cwd_for_tool_call(
                call.tool_name.name.as_str(),
                &call.payload,
                self.step_context.turn.config.cwd.as_path(),
            );
            let workspace_revision_before = if observes_workspace {
                Some(
                    self.session
                        .services
                        .git_workspace
                        .workspace_evidence_identity(&workspace_cwd)
                        .await,
                )
            } else {
                None
            };
            let mutation_revision_before = if signal_ordinal.is_some() || observes_workspace {
                Some(self.tracker.lock().await.current_mutation_revision())
            } else {
                None
            };
            let mutation_tracker = Arc::clone(&self.tracker);
            let error_call = call.clone();
            let owner_tool_name = call.tool_name.clone();
            let owner_payload = call.payload.clone();
            let future = self.clone().handle_tool_call_with_source_and_timing(
                call,
                ToolCallSource::Direct,
                cancellation_token,
                timing,
            );
            match future.await {
                Ok(mut response) => {
                    let mutation_advanced = if let Some(before) = mutation_revision_before {
                        mutation_tracker.lock().await.current_mutation_revision() > before
                    } else {
                        false
                    };
                    let outcome_context = response.outcome_context();
                    let signal = response.sampling_request_signal();
                    let owner_key = response.deterministic_continuation_owner_key();
                    if let Some(owner_key) = owner_key.as_deref() {
                        let continuations = self
                            .session
                            .services
                            .code_mode_service
                            .owner_drained_continuation_snapshot(owner_key);
                        let accepted = response.merge_owner_drained_continuations(continuations);
                        self.session
                            .services
                            .code_mode_service
                            .acknowledge_owner_drained_continuations(owner_key, &accepted);
                        if let Some(collector) = &signal_collector {
                            collector
                                .record_accepted_deterministic_continuation_receipts(&accepted);
                        }
                    }
                    let receipts = response.deterministic_continuation_receipts();
                    if let Some(collector) = &signal_collector {
                        collector.record_accepted_deterministic_continuation_receipts(&receipts);
                    }
                    let canonical_artifact_required = response.requires_canonical_artifact();
                    let source_dependencies_override = owner_key.as_deref().and_then(|owner_key| {
                        signal_collector.as_ref().and_then(|collector| {
                            collector.code_mode_source_dependencies(owner_key)
                        })
                    });
                    let response = response.into_response();
                    self.register_workspace_evidence_for_response(
                        &error_call,
                        &response,
                        workspace_revision_before,
                        mutation_advanced,
                        source_dependencies_override,
                    )
                    .await;
                    if let (Some(collector), Some(ordinal)) = (&signal_collector, signal_ordinal) {
                        collector.record_direct_wait_owner_result(
                            authoritative_direct_wait,
                            &owner_tool_name,
                            &owner_payload,
                            signal.as_ref(),
                            &response,
                        );
                        collector.record_response_result_with_mutation(
                            ordinal,
                            outcome_context,
                            signal,
                            &response,
                            canonical_artifact_required,
                            mutation_advanced,
                        );
                    }
                    Ok(response)
                }
                Err(FunctionCallError::Fatal(message)) => {
                    let mutation_advanced = if let Some(before) = mutation_revision_before {
                        mutation_tracker.lock().await.current_mutation_revision() > before
                    } else {
                        false
                    };
                    if let (Some(collector), Some(ordinal)) = (&signal_collector, signal_ordinal) {
                        collector.record_failure_with_mutation(
                            ordinal,
                            &format!("fatal:{message}"),
                            mutation_advanced,
                        );
                    }
                    Err(CodexErr::Fatal(message))
                }
                Err(other) => {
                    let mutation_advanced = if let Some(before) = mutation_revision_before {
                        mutation_tracker.lock().await.current_mutation_revision() > before
                    } else {
                        false
                    };
                    if let (Some(collector), Some(ordinal)) = (&signal_collector, signal_ordinal) {
                        collector.record_failure_with_mutation(
                            ordinal,
                            &format!("model:{other}"),
                            mutation_advanced,
                        );
                    }
                    let response = Self::failure_response(error_call.clone(), other);
                    self.register_workspace_evidence_for_response(
                        &error_call,
                        &response,
                        workspace_revision_before,
                        mutation_advanced,
                        None,
                    )
                    .await;
                    Ok(response)
                }
            }
        }
        .in_current_span()
    }

    #[instrument(level = "trace", skip_all)]
    pub(crate) fn handle_tool_call_with_source(
        self,
        call: ToolCall,
        source: ToolCallSource,
        cancellation_token: CancellationToken,
    ) -> impl std::future::Future<Output = Result<AnyToolResult, FunctionCallError>> {
        let timing = Arc::new(ToolDispatchTiming::new(
            TokioInstant::now(),
            /*eager*/ false,
        ));
        timing.mark_first_poll();
        self.handle_tool_call_with_source_and_timing(call, source, cancellation_token, timing)
    }

    fn handle_tool_call_with_source_and_timing(
        self,
        call: ToolCall,
        source: ToolCallSource,
        cancellation_token: CancellationToken,
        timing: Arc<ToolDispatchTiming>,
    ) -> impl std::future::Future<Output = Result<AnyToolResult, FunctionCallError>> {
        let Some(router) = self.step_context.tool_router() else {
            return Either::Left(std::future::ready(Err(FunctionCallError::Fatal(
                "step tool router was not finalized before tool execution".to_string(),
            ))));
        };
        let supports_parallel = router.tool_supports_parallel(&call);
        let wait_for_runtime_cancellation = router.tool_waits_for_runtime_cancellation(&call);
        let router = Arc::clone(router);
        let session = Arc::clone(&self.session);
        let step_context = Arc::clone(&self.step_context);
        let turn = Arc::clone(&step_context.turn);
        turn.turn_timing_state
            .record_tool_call(call.tool_name.name.as_str());
        let tracker = Arc::clone(&self.tracker);
        let lock = Arc::clone(&self.parallel_execution);
        let invocation_cancellation_token = cancellation_token.clone();
        let started = Instant::now();
        let tool_call_timing_guard = ToolCallTimingGuard::capture(
            Arc::clone(&timing),
            &session.thread_id,
            &turn.sub_id,
            &call,
            &source,
        );
        let abort_session = Arc::clone(&session);
        let abort_source = source.clone();
        let abort_turn = Arc::clone(&turn);
        let terminal_outcome_reached = Arc::new(AtomicBool::new(false));
        let dispatch_terminal_outcome_reached = Arc::clone(&terminal_outcome_reached);
        let dispatch_call = call.clone();
        let dispatch_tool_name = call.tool_name.name.clone();
        let evidence_call = call.clone();
        let serializes_workspace =
            crate::tool_history::tool_observes_workspace(evidence_call.tool_name.name.as_str());
        let observes_workspace = crate::tool_history::tool_call_observes_workspace(
            evidence_call.tool_name.name.as_str(),
            &evidence_call.payload,
        );
        let evidence_tracker = Arc::clone(&tracker);

        let dispatch_span = trace_span!(
            "dispatch_tool_call_with_terminal_outcome",
            otel.name = %call.tool_name,
            tool_name = %call.tool_name,
            call_id = call.call_id.as_str(),
            aborted = false,
        );
        let abort_dispatch_span = dispatch_span.clone();

        let mut dispatch_handle: AbortOnDropHandle<Result<AnyToolResult, FunctionCallError>> =
            AbortOnDropHandle::new(tokio::spawn(async move {
                let _guard = if workspace_tool_may_use_parallel_gate(
                    supports_parallel,
                    serializes_workspace,
                ) {
                    Either::Left(lock.read().await)
                } else {
                    Either::Right(lock.write().await)
                };
                // Gate admission is distinct from authorization and actual
                // handler entry; keep each boundary independently observable.
                timing.mark_parallel_gate_admitted();
                turn.turn_timing_state
                    .record_tool_gate_admitted(dispatch_tool_name.as_str());

                let evidence_revision_before = if observes_workspace {
                    let workspace_cwd = crate::tool_history::workspace_evidence_cwd_for_tool_call(
                        evidence_call.tool_name.name.as_str(),
                        &evidence_call.payload,
                        turn.config.cwd.as_path(),
                    );
                    session
                        .services
                        .git_workspace
                        .workspace_evidence_identity(&workspace_cwd)
                        .await
                } else {
                    None
                };
                let evidence_mutation_revision_before = if observes_workspace {
                    Some(evidence_tracker.lock().await.current_mutation_revision())
                } else {
                    None
                };

                let result = scope_tool_dispatch_timing(
                    timing,
                    router
                        .dispatch_tool_call_with_terminal_outcome(
                            Arc::clone(&session),
                            Arc::clone(&step_context),
                            invocation_cancellation_token,
                            tracker,
                            dispatch_call,
                            source,
                            dispatch_terminal_outcome_reached,
                        )
                        .instrument(dispatch_span.clone()),
                )
                .await;
                let successful = result.as_ref().is_ok_and(|result| {
                    result.outcome_for_logging() == codex_tools::ToolOutputOutcome::Success
                });
                turn.turn_timing_state
                    .record_tool_completion(dispatch_tool_name.as_str(), successful);
                let evidence_response = match result.as_ref() {
                    Ok(result) => Some(result.response()),
                    Err(FunctionCallError::Fatal(_)) => None,
                    Err(err) => Some(Self::failure_response_for_message(
                        &evidence_call,
                        err.to_string(),
                    )),
                };
                if let Some(response) = evidence_response.as_ref() {
                    let mutation_advanced = if let Some(before) = evidence_mutation_revision_before
                    {
                        evidence_tracker.lock().await.current_mutation_revision() > before
                    } else {
                        false
                    };
                    Self::register_workspace_evidence_while_guarded(
                        session.as_ref(),
                        turn.as_ref(),
                        &evidence_call,
                        response,
                        evidence_revision_before,
                        mutation_advanced,
                    )
                    .await;
                }
                result
            }));

        Either::Right(
            async move {
                let _tool_call_timing_guard = tool_call_timing_guard;
                tokio::select! {
                res = &mut dispatch_handle => res.map_err(Self::tool_task_join_error)?,
                _ = cancellation_token.cancelled() => {
                    if terminal_outcome_reached.load(Ordering::Acquire) || dispatch_handle.is_finished() {
                        dispatch_handle.await.map_err(Self::tool_task_join_error)?
                    } else {
                        let secs = started.elapsed().as_secs_f32().max(0.1);
                        abort_dispatch_span.record("aborted", true);
                        if wait_for_runtime_cancellation {
                            if terminal_outcome_reached.swap(true, Ordering::AcqRel) {
                                return dispatch_handle.await.map_err(Self::tool_task_join_error)?;
                            }
                            // The abort owns the terminal outcome; await only so
                            // the runtime can finish process teardown.
                            match dispatch_handle.await {
                                Ok(_) => {}
                                Err(err) if err.is_cancelled() => {}
                                Err(err) => return Err(Self::tool_task_join_error(err)),
                            }
                        } else {
                            dispatch_handle.abort();
                            match dispatch_handle.await {
                                Ok(result) => return result,
                                Err(err) if err.is_cancelled() => {}
                                Err(err) => return Err(Self::tool_task_join_error(err)),
                            }
                        }
                        let response = Self::aborted_response(&call, secs);
                        notify_tool_aborted(
                            abort_session.as_ref(),
                            abort_turn.as_ref(),
                            call.call_id.as_str(),
                            &call.tool_name,
                            abort_source,
                        )
                        .await;
                        Ok(response)
                    }
                },
            }
        }
            .in_current_span(),
        )
    }
}

impl ToolCallRuntime {
    fn tool_task_join_error(err: JoinError) -> FunctionCallError {
        FunctionCallError::Fatal(format!("tool task failed to receive: {err:?}"))
    }

    fn failure_response(call: ToolCall, err: FunctionCallError) -> ResponseInputItem {
        Self::failure_response_for_message(&call, err.to_string())
    }

    fn failure_response_for_message(call: &ToolCall, message: String) -> ResponseInputItem {
        match &call.payload {
            ToolPayload::ToolSearch { .. } => ResponseInputItem::ToolSearchOutput {
                call_id: call.call_id.clone(),
                status: "incomplete".to_string(),
                execution: "client".to_string(),
                tools: Vec::new(),
                omitted_result_count: None,
            },
            ToolPayload::Custom { .. } => ResponseInputItem::CustomToolCallOutput {
                call_id: call.call_id.clone(),
                name: None,
                output: codex_protocol::models::FunctionCallOutputPayload {
                    body: codex_protocol::models::FunctionCallOutputBody::Text(message),
                    success: Some(false),
                },
            },
            _ => ResponseInputItem::FunctionCallOutput {
                call_id: call.call_id.clone(),
                output: codex_protocol::models::FunctionCallOutputPayload {
                    body: codex_protocol::models::FunctionCallOutputBody::Text(message),
                    success: Some(false),
                },
            },
        }
    }

    fn aborted_response(call: &ToolCall, secs: f32) -> AnyToolResult {
        AnyToolResult {
            call_id: call.call_id.clone(),
            payload: call.payload.clone(),
            result: Box::new(AbortedToolOutput {
                message: Self::abort_message(call, secs),
            }),
            post_tool_use_payload: None,
            model_projection: None,
        }
    }

    fn abort_message(call: &ToolCall, secs: f32) -> String {
        if call.tool_name.namespace.is_none()
            && matches!(
                call.tool_name.name.as_str(),
                "shell_command" | "unified_exec"
            )
        {
            format!("Wall time: {secs:.1} seconds\naborted by user")
        } else {
            format!("aborted by user after {secs:.1}s")
        }
    }
}

impl ToolCallTimingGuard {
    fn capture(
        timing: Arc<ToolDispatchTiming>,
        conversation_id: &impl std::fmt::Display,
        turn_id: &str,
        call: &ToolCall,
        source: &ToolCallSource,
    ) -> Option<Self> {
        // Code-mode calls are nested within a direct code-mode tool call whose
        // timing already includes them. Suppress nested guards so consumers do
        // not mistake overlapping events for independent tool-call latency.
        if !matches!(source, ToolCallSource::Direct) || !tracing::enabled!(tracing::Level::INFO) {
            return None;
        }

        Some(Self {
            timing,
            conversation_id: conversation_id.to_string(),
            turn_id: turn_id.to_string(),
            call_id: call.call_id.clone(),
            tool_name: call.tool_name.clone(),
        })
    }
}

impl Drop for ToolCallTimingGuard {
    fn drop(&mut self) {
        let completed_at = TokioInstant::now();
        // Snapshot once so concurrent boundary updates cannot make one event
        // internally inconsistent. Keep the legacy dispatch field as a
        // compatibility alias for parallel-gate wait.
        let snapshot = self.timing.snapshot(completed_at);
        info!(
            event.name = "codex.tool_call",
            trace_id = %codex_otel::current_span_trace_id().unwrap_or_default(),
            conversation.id = %self.conversation_id,
            turn_id = %self.turn_id,
            tool_name = %self.tool_name,
            call_id = %self.call_id,
            tool_source = "direct",
            eager = snapshot.eager,
            execution_started = snapshot.parallel_gate_admitted,
            item_to_first_poll_ms = snapshot.item_to_first_poll_ms.unwrap_or(0),
            parallel_gate_wait_ms = snapshot.parallel_gate_wait_ms.unwrap_or(0),
            authorization_state_coordination_ms = snapshot
                .authorization_state_coordination_ms
                .unwrap_or(0),
            first_poll_to_handler_entry_ms = snapshot
                .first_poll_to_handler_entry_ms
                .unwrap_or(0),
            dispatch_duration_ms = snapshot.parallel_gate_wait_ms.unwrap_or(0),
            handler_duration_ms = snapshot.handler_duration_ms.unwrap_or(0),
            total_duration_ms = snapshot.total_duration_ms.unwrap_or(0),
            "tool call completed"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use crate::session::step_context::StepContext;
    use crate::tools::ToolRouter;
    use crate::tools::context::FunctionToolOutput;
    use crate::tools::context::ToolInvocation;
    use crate::tools::registry::CoreToolRuntime;
    use crate::tools::registry::ToolExecutionTiming;
    use crate::tools::registry::ToolExecutor;
    use crate::tools::registry::ToolRegistry;
    use crate::turn_diff_tracker::TurnDiffTracker;

    use codex_extension_api::ToolCallOutcome;
    use codex_protocol::models::FunctionCallOutputBody;
    use codex_protocol::models::FunctionCallOutputPayload;
    use pretty_assertions::assert_eq;

    #[test]
    fn rollout_workflow_guardrails_change_route_after_reused_session_failure() {
        let diagnosis: serde_json::Value = serde_json::from_str(&reused_failure_diagnosis(
            &codex_tools::ToolName::plain("write_stdin"),
            "stable-failure",
        ))
        .expect("valid diagnosis");

        assert_eq!(diagnosis["retryable"], false);
        assert_eq!(diagnosis["required_action"], "change_route_or_state");
        assert!(
            diagnosis["next_action"]
                .as_str()
                .expect("next action")
                .contains("Do not poll this session again with unchanged arguments")
        );
    }
    use tokio::sync::Notify;
    use tokio::sync::oneshot;
    use tracing_test::internal::MockWriter;

    fn workspace_identity(label: &str) -> crate::git_workspace::WorkspaceEvidenceIdentity {
        crate::git_workspace::WorkspaceEvidenceIdentity {
            repository_root: Some("/repo".to_string()),
            head_identity: Some(format!("head-{label}")),
            index_identity: Some(format!("index-{label}")),
            worktree_identity: Some(format!("worktree-{label}")),
        }
    }

    #[test]
    fn workspace_observers_are_serialized_and_external_drift_is_stale() {
        assert!(!workspace_tool_may_use_parallel_gate(true, true));
        assert!(workspace_tool_may_use_parallel_gate(true, false));

        let before = workspace_identity("before");
        let after = workspace_identity("after");
        assert!(!guarded_workspace_evidence_is_current(
            Some(&before),
            Some(&after),
        ));
        assert!(guarded_workspace_evidence_is_current(
            Some(&after),
            Some(&after),
        ));
        assert!(!guarded_workspace_evidence_is_current(None, None));
    }

    #[test]
    fn tool_search_failure_response_is_incomplete() {
        let call = ToolCall {
            tool_name: codex_tools::ToolName::plain("tool_search"),
            call_id: "search-failed".to_string(),
            payload: ToolPayload::ToolSearch {
                arguments: codex_protocol::models::SearchToolCallParams {
                    query: "calendar".to_string(),
                    limit: None,
                },
            },
        };

        assert_eq!(
            ToolCallRuntime::failure_response(
                call,
                FunctionCallError::RespondToModel("failed".to_string()),
            ),
            ResponseInputItem::ToolSearchOutput {
                call_id: "search-failed".to_string(),
                status: "incomplete".to_string(),
                execution: "client".to_string(),
                tools: Vec::new(),
                omitted_result_count: None,
            }
        );
    }

    #[test]
    fn tool_call_timing_guard_ignores_code_mode_source() {
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let call = ToolCall {
                tool_name: codex_tools::ToolName::plain("test_tool"),
                call_id: "call-1".to_string(),
                payload: ToolPayload::Function {
                    arguments: "{}".to_string(),
                },
            };
            let direct_timing = Arc::new(ToolDispatchTiming::new(
                TokioInstant::now(),
                /*eager*/ false,
            ));
            direct_timing.mark_first_poll();
            let direct_guard = ToolCallTimingGuard::capture(
                direct_timing,
                &"conversation-id",
                "turn-id",
                &call,
                &ToolCallSource::Direct,
            );
            assert!(
                direct_guard.is_some(),
                "direct tool calls should create a timing guard"
            );
            drop(direct_guard);

            let code_mode_timing = Arc::new(ToolDispatchTiming::new(
                TokioInstant::now(),
                /*eager*/ false,
            ));
            code_mode_timing.mark_first_poll();
            let code_mode_guard = ToolCallTimingGuard::capture(
                code_mode_timing,
                &"conversation-id",
                "turn-id",
                &call,
                &ToolCallSource::CodeMode {
                    cell_id: "cell-1".to_string(),
                    runtime_tool_call_id: "runtime-call-1".to_string(),
                },
            );
            assert!(
                code_mode_guard.is_none(),
                "nested code-mode calls should not create overlapping timing events"
            );
        });
    }

    #[tokio::test]
    async fn cancellation_before_dispatch_admission_logs_dispatch_only_timing() -> anyhow::Result<()>
    {
        let (session, turn_context) = crate::session::tests::make_session_and_context().await;
        let session = Arc::new(session);
        let turn_context = Arc::new(turn_context);
        let tool_name = codex_tools::ToolName::plain("test_tool");
        let handler = Arc::new(ImmediateHandler {
            tool_name: tool_name.clone(),
        }) as Arc<dyn CoreToolRuntime>;
        let step_context = StepContext::for_test(Arc::clone(&turn_context));
        let router = Arc::new(ToolRouter::from_parts(
            ToolRegistry::from_tools([handler]),
            Vec::new(),
        ));
        let step_context = step_context.with_tool_router_for_test(router);
        let tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));
        let runtime = ToolCallRuntime::new(session, step_context, tracker);
        let execution_gate = Arc::clone(&runtime.parallel_execution);
        let execution_gate_guard = execution_gate
            .try_write_owned()
            .expect("execution gate should be available before dispatch starts");
        let (release_execution_gate_tx, release_execution_gate_rx) = std::sync::mpsc::channel();
        let execution_gate_task = tokio::task::spawn_blocking(move || {
            let _execution_gate_guard = execution_gate_guard;
            release_execution_gate_rx
                .recv()
                .expect("test should release the execution gate");
        });

        let buffer: &'static std::sync::Mutex<Vec<u8>> =
            Box::leak(Box::new(std::sync::Mutex::new(Vec::new())));
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .with_writer(MockWriter::new(buffer))
            .finish();
        let _subscriber_guard = tracing::subscriber::set_default(subscriber);

        let cancellation_token = CancellationToken::new();
        let call = ToolCall {
            tool_name,
            call_id: "call-1".to_string(),
            payload: ToolPayload::Function {
                arguments: "{}".to_string(),
            },
        };
        let response_task =
            tokio::spawn(runtime.handle_tool_call(call, cancellation_token.clone()));
        cancellation_token.cancel();
        tokio::time::timeout(Duration::from_secs(1), response_task)
            .await
            .expect("timed out waiting for cancelled tool response")
            .expect("cancelled tool response task should join")
            .expect("cancelled tool call should produce a response");

        let logs = String::from_utf8(
            buffer
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
        )?;
        let timing_events = logs
            .lines()
            .filter(|line| line.contains("event.name=\"codex.tool_call\""))
            .collect::<Vec<_>>();
        assert_eq!(
            timing_events.len(),
            1,
            "cancelled tool call should emit exactly one timing event; logs:\n{logs}"
        );
        let timing_event = timing_events[0];
        assert!(
            timing_event.contains("execution_started=false"),
            "tool cancelled before admission should not report execution started: {timing_event}"
        );
        assert!(
            timing_event.contains("handler_duration_ms=0"),
            "tool cancelled before admission should report zero handler duration: {timing_event}"
        );
        let duration_field = |name: &str| {
            timing_event.split_whitespace().find_map(|field| {
                field
                    .strip_prefix(&format!("{name}="))
                    .and_then(|value| value.parse::<u64>().ok())
            })
        };
        let parallel_gate_wait_ms = duration_field("parallel_gate_wait_ms")
            .expect("timing event should include parallel_gate_wait_ms");
        let dispatch_duration_ms = duration_field("dispatch_duration_ms")
            .expect("compatibility timing should include dispatch_duration_ms");
        let total_duration_ms = duration_field("total_duration_ms")
            .expect("timing event should include total_duration_ms");
        assert_eq!(
            dispatch_duration_ms, parallel_gate_wait_ms,
            "legacy dispatch timing should alias parallel-gate wait: {timing_event}"
        );
        assert!(
            total_duration_ms >= parallel_gate_wait_ms
                && total_duration_ms - parallel_gate_wait_ms <= 1,
            "tool cancelled before admission should spend its polled lifetime at the gate: {timing_event}"
        );
        release_execution_gate_tx
            .send(())
            .expect("execution gate task should remain available");
        execution_gate_task
            .await
            .expect("execution gate task should join");

        Ok(())
    }

    #[tokio::test]
    async fn runtime_respects_non_handler_tool_execution_timing() {
        let (session, turn_context) = crate::session::tests::make_session_and_context().await;
        let session = Arc::new(session);
        let turn_context = Arc::new(turn_context);
        let interactive_tool = codex_tools::ToolName::plain("interactive_timing_tool");
        let nested_runtime_tool = codex_tools::ToolName::plain("nested_runtime_timing_tool");
        let handlers = [
            Arc::new(DeclaredTimingHandler {
                tool_name: interactive_tool.clone(),
                timing: ToolExecutionTiming::Interactive,
            }) as Arc<dyn CoreToolRuntime>,
            Arc::new(DeclaredTimingHandler {
                tool_name: nested_runtime_tool.clone(),
                timing: ToolExecutionTiming::NestedRuntime,
            }) as Arc<dyn CoreToolRuntime>,
        ];
        let step_context = StepContext::for_test(Arc::clone(&turn_context));
        let router = Arc::new(ToolRouter::from_parts(
            ToolRegistry::from_tools(handlers),
            Vec::new(),
        ));
        let step_context = step_context.with_tool_router_for_test(router);
        let tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));
        let runtime = ToolCallRuntime::new(session, step_context, tracker);
        turn_context.turn_timing_state.mark_turn_started();

        for (index, tool_name) in [interactive_tool, nested_runtime_tool]
            .into_iter()
            .enumerate()
        {
            runtime
                .clone()
                .handle_tool_call(
                    ToolCall {
                        tool_name,
                        call_id: format!("timing-call-{index}"),
                        payload: ToolPayload::Function {
                            arguments: "{}".to_string(),
                        },
                    },
                    CancellationToken::new(),
                )
                .await
                .expect("non-handler timing tool should complete");
        }

        let profile = turn_context.turn_timing_state.complete_snapshot().profile;
        assert_eq!(
            profile.unions.tool_active_ns, 0,
            "ToolCallRuntime must not override Interactive or NestedRuntime timing ownership"
        );
    }

    struct ImmediateHandler {
        tool_name: codex_tools::ToolName,
    }

    impl ToolExecutor<ToolInvocation> for ImmediateHandler {
        fn tool_name(&self) -> codex_tools::ToolName {
            self.tool_name.clone()
        }

        fn spec(&self) -> codex_tools::ToolSpec {
            codex_tools::ToolSpec::Function(codex_tools::ResponsesApiTool {
                name: self.tool_name.name.clone(),
                description: "Immediate test tool.".to_string(),
                strict: false,
                defer_loading: None,
                parameters: codex_tools::JsonSchema::default(),
                output_schema: None,
            })
        }

        fn handle(&self, _invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
            Box::pin(async {
                Ok(
                    Box::new(FunctionToolOutput::from_text("ok".to_string(), Some(true)))
                        as Box<dyn crate::tools::context::ToolOutput>,
                )
            })
        }
    }

    impl CoreToolRuntime for ImmediateHandler {}

    struct ParallelImmediateHandler {
        tool_name: codex_tools::ToolName,
    }

    impl ToolExecutor<ToolInvocation> for ParallelImmediateHandler {
        fn tool_name(&self) -> codex_tools::ToolName {
            self.tool_name.clone()
        }

        fn spec(&self) -> codex_tools::ToolSpec {
            codex_tools::ToolSpec::Function(codex_tools::ResponsesApiTool {
                name: self.tool_name.name.clone(),
                description: "Parallel immediate test tool.".to_string(),
                strict: false,
                defer_loading: None,
                parameters: codex_tools::JsonSchema::default(),
                output_schema: None,
            })
        }

        fn supports_parallel_tool_calls(&self) -> bool {
            true
        }

        fn handle(&self, _invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
            Box::pin(async {
                Ok(
                    Box::new(FunctionToolOutput::from_text("ok".to_string(), Some(true)))
                        as Box<dyn crate::tools::context::ToolOutput>,
                )
            })
        }
    }

    impl CoreToolRuntime for ParallelImmediateHandler {}

    #[tokio::test]
    async fn eager_read_eligibility_uses_classification_registration_and_prefix_order() {
        let (session, turn_context) = crate::session::tests::make_session_and_context().await;
        let session = Arc::new(session);
        let parallel_read = Arc::new(ParallelImmediateHandler {
            tool_name: codex_tools::ToolName::plain("read_tool_output"),
        }) as Arc<dyn CoreToolRuntime>;
        let parallel_shell = Arc::new(ParallelImmediateHandler {
            tool_name: codex_tools::ToolName::plain("shell_command"),
        }) as Arc<dyn CoreToolRuntime>;
        let serial_read = Arc::new(ImmediateHandler {
            tool_name: codex_tools::ToolName::plain("view_image"),
        }) as Arc<dyn CoreToolRuntime>;
        let router = Arc::new(ToolRouter::from_parts(
            ToolRegistry::from_tools([parallel_read, parallel_shell, serial_read]),
            Vec::new(),
        ));
        let step_context =
            StepContext::for_test(Arc::new(turn_context)).with_tool_router_for_test(router);
        let runtime = ToolCallRuntime::new(
            session,
            step_context,
            Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
        );
        let call = |name: &str| ToolCall {
            tool_name: codex_tools::ToolName::plain(name),
            call_id: format!("{name}-call"),
            payload: ToolPayload::Function {
                arguments: "{}".to_string(),
            },
        };

        let mut eager_prefix_open = true;
        assert!(
            runtime.take_eager_read_eligibility(&call("read_tool_output"), &mut eager_prefix_open)
        );
        assert!(eager_prefix_open);

        // Parallel capability alone cannot admit an ExecShell-classified tool.
        assert!(
            !runtime.take_eager_read_eligibility(&call("shell_command"), &mut eager_prefix_open)
        );
        assert!(!eager_prefix_open);
        // Once a deferred call appears, a later otherwise-eligible read cannot overtake it.
        assert!(
            !runtime.take_eager_read_eligibility(&call("read_tool_output"), &mut eager_prefix_open)
        );

        let mut serial_prefix = true;
        // ReadSearch classification alone cannot admit a serial registered handler.
        assert!(!runtime.take_eager_read_eligibility(&call("view_image"), &mut serial_prefix));
        assert!(!serial_prefix);

        let mut unknown_prefix = true;
        assert!(
            !runtime.take_eager_read_eligibility(&call("unregistered_tool"), &mut unknown_prefix)
        );
        assert!(!unknown_prefix);
    }

    struct DeclaredTimingHandler {
        tool_name: codex_tools::ToolName,
        timing: ToolExecutionTiming,
    }

    impl ToolExecutor<ToolInvocation> for DeclaredTimingHandler {
        fn tool_name(&self) -> codex_tools::ToolName {
            self.tool_name.clone()
        }

        fn spec(&self) -> codex_tools::ToolSpec {
            codex_tools::ToolSpec::Function(codex_tools::ResponsesApiTool {
                name: self.tool_name.name.clone(),
                description: "Declared timing test tool.".to_string(),
                strict: false,
                defer_loading: None,
                parameters: codex_tools::JsonSchema::default(),
                output_schema: None,
            })
        }

        fn handle(&self, _invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
            Box::pin(async {
                tokio::time::sleep(Duration::from_millis(10)).await;
                Ok(
                    Box::new(FunctionToolOutput::from_text("ok".to_string(), Some(true)))
                        as Box<dyn crate::tools::context::ToolOutput>,
                )
            })
        }
    }

    impl CoreToolRuntime for DeclaredTimingHandler {
        fn tool_execution_timing(&self) -> ToolExecutionTiming {
            self.timing
        }
    }

    struct CancellationCleanupHandler {
        tool_name: codex_tools::ToolName,
        started: std::sync::Mutex<Option<oneshot::Sender<()>>>,
        cleanup_started: std::sync::Mutex<Option<oneshot::Sender<()>>>,
        allow_cleanup: Arc<Notify>,
    }

    impl ToolExecutor<ToolInvocation> for CancellationCleanupHandler {
        fn tool_name(&self) -> codex_tools::ToolName {
            self.tool_name.clone()
        }

        fn spec(&self) -> codex_tools::ToolSpec {
            codex_tools::ToolSpec::Function(codex_tools::ResponsesApiTool {
                name: self.tool_name.name.clone(),
                description: "Cancellation cleanup test tool.".to_string(),
                strict: false,
                defer_loading: None,
                parameters: codex_tools::JsonSchema::default(),
                output_schema: None,
            })
        }

        fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
            Box::pin(self.handle_call(invocation))
        }
    }

    impl CancellationCleanupHandler {
        async fn handle_call(
            &self,
            invocation: ToolInvocation,
        ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
            let started = self
                .started
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            if let Some(started) = started {
                let _ = started.send(());
            }
            invocation.cancellation_token.cancelled().await;
            let cleanup_started = self
                .cleanup_started
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            if let Some(cleanup_started) = cleanup_started {
                let _ = cleanup_started.send(());
            }
            self.allow_cleanup.notified().await;
            Ok(Box::new(FunctionToolOutput::from_text(
                "cleanup complete".to_string(),
                Some(false),
            )) as Box<dyn crate::tools::context::ToolOutput>)
        }
    }

    impl CoreToolRuntime for CancellationCleanupHandler {
        fn waits_for_runtime_cancellation(&self) -> bool {
            true
        }
    }

    struct FinishRecorder {
        records: Arc<std::sync::Mutex<Vec<ToolCallOutcome>>>,
    }

    impl codex_extension_api::ToolLifecycleContributor for FinishRecorder {
        fn on_tool_finish<'a>(
            &'a self,
            input: codex_extension_api::ToolFinishInput<'a>,
        ) -> codex_extension_api::ToolLifecycleFuture<'a> {
            let records = Arc::clone(&self.records);
            let outcome = input.outcome;
            Box::pin(async move {
                records
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(outcome);
            })
        }
    }

    struct BlockingFinishContributor {
        records: Arc<std::sync::Mutex<Vec<ToolCallOutcome>>>,
        finish_started: std::sync::Mutex<Option<oneshot::Sender<()>>>,
        allow_finish: Arc<Notify>,
    }

    impl codex_extension_api::ToolLifecycleContributor for BlockingFinishContributor {
        fn on_tool_finish<'a>(
            &'a self,
            input: codex_extension_api::ToolFinishInput<'a>,
        ) -> codex_extension_api::ToolLifecycleFuture<'a> {
            let records = Arc::clone(&self.records);
            let allow_finish = Arc::clone(&self.allow_finish);
            let finish_started = self
                .finish_started
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            let outcome = input.outcome;
            Box::pin(async move {
                if let Some(finish_started) = finish_started {
                    let _ = finish_started.send(());
                }
                allow_finish.notified().await;
                records
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(outcome);
            })
        }
    }

    #[tokio::test]
    async fn cancellation_after_handler_finishes_preserves_completed_lifecycle()
    -> anyhow::Result<()> {
        let (mut session, turn_context) = crate::session::tests::make_session_and_context().await;
        let records = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (finish_started_tx, finish_started_rx) = oneshot::channel();
        let allow_finish = Arc::new(Notify::new());
        let mut builder =
            codex_extension_api::ExtensionRegistryBuilder::<crate::config::Config>::new();
        builder.tool_lifecycle_contributor(Arc::new(BlockingFinishContributor {
            records: Arc::clone(&records),
            finish_started: std::sync::Mutex::new(Some(finish_started_tx)),
            allow_finish: Arc::clone(&allow_finish),
        }));
        session.services.extensions = Arc::new(builder.build());

        let session = Arc::new(session);
        let turn_context = Arc::new(turn_context);
        let tool_name = codex_tools::ToolName::plain("test_tool");
        let handler = Arc::new(ImmediateHandler {
            tool_name: tool_name.clone(),
        }) as Arc<dyn CoreToolRuntime>;
        let step_context = StepContext::for_test(Arc::clone(&turn_context));
        let router = Arc::new(ToolRouter::from_parts(
            ToolRegistry::from_tools([handler]),
            Vec::new(),
        ));
        let step_context = step_context.with_tool_router_for_test(router);
        let tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));
        let runtime = ToolCallRuntime::new(session, step_context, tracker);
        let cancellation_token = CancellationToken::new();
        let call = ToolCall {
            tool_name,
            call_id: "call-1".to_string(),
            payload: ToolPayload::Function {
                arguments: "{}".to_string(),
            },
        };

        let response_task =
            tokio::spawn(runtime.handle_tool_call(call, cancellation_token.clone()));
        tokio::time::timeout(Duration::from_secs(1), finish_started_rx)
            .await
            .expect("timed out waiting for lifecycle notification to start")
            .expect("lifecycle notification should start");
        cancellation_token.cancel();
        tokio::time::sleep(Duration::from_millis(10)).await;
        allow_finish.notify_waiters();

        let response = tokio::time::timeout(Duration::from_secs(1), response_task)
            .await
            .expect("timed out waiting for tool response")
            .expect("tool response task should join")?;
        let expected_response = ResponseInputItem::FunctionCallOutput {
            call_id: "call-1".to_string(),
            output: FunctionCallOutputPayload {
                body: FunctionCallOutputBody::Text("ok".to_string()),
                success: Some(true),
            },
        };
        assert_eq!(expected_response, response);

        let actual = records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain(..)
            .collect::<Vec<_>>();
        assert_eq!(vec![ToolCallOutcome::Completed { success: true }], actual);

        Ok(())
    }

    #[tokio::test]
    async fn cancellation_waiting_for_runtime_cleanup_emits_only_aborted_lifecycle()
    -> anyhow::Result<()> {
        let (mut session, turn_context) = crate::session::tests::make_session_and_context().await;
        let records = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut builder =
            codex_extension_api::ExtensionRegistryBuilder::<crate::config::Config>::new();
        builder.tool_lifecycle_contributor(Arc::new(FinishRecorder {
            records: Arc::clone(&records),
        }));
        session.services.extensions = Arc::new(builder.build());

        let session = Arc::new(session);
        let turn_context = Arc::new(turn_context);
        let tool_name = codex_tools::ToolName::plain("cleanup_tool");
        let (started_tx, started_rx) = oneshot::channel();
        let (cleanup_started_tx, cleanup_started_rx) = oneshot::channel();
        let allow_cleanup = Arc::new(Notify::new());
        let handler = Arc::new(CancellationCleanupHandler {
            tool_name: tool_name.clone(),
            started: std::sync::Mutex::new(Some(started_tx)),
            cleanup_started: std::sync::Mutex::new(Some(cleanup_started_tx)),
            allow_cleanup: Arc::clone(&allow_cleanup),
        }) as Arc<dyn CoreToolRuntime>;
        let step_context = StepContext::for_test(Arc::clone(&turn_context));
        let router = Arc::new(ToolRouter::from_parts(
            ToolRegistry::from_tools([handler]),
            Vec::new(),
        ));
        let step_context = step_context.with_tool_router_for_test(router);
        let tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));
        let runtime = ToolCallRuntime::new(session, step_context, tracker);
        let cancellation_token = CancellationToken::new();
        let call = ToolCall {
            tool_name,
            call_id: "call-1".to_string(),
            payload: ToolPayload::Function {
                arguments: "{}".to_string(),
            },
        };

        let response_task =
            tokio::spawn(runtime.handle_tool_call(call, cancellation_token.clone()));
        started_rx.await.expect("handler should start");
        cancellation_token.cancel();
        cleanup_started_rx
            .await
            .expect("handler should start cleanup");
        tokio::time::sleep(Duration::from_millis(10)).await;
        allow_cleanup.notify_one();

        let response = tokio::time::timeout(Duration::from_secs(1), response_task)
            .await
            .expect("timed out waiting for tool response")
            .expect("tool response task should join")?;
        let ResponseInputItem::FunctionCallOutput { output, .. } = response else {
            anyhow::bail!("cancelled tool should return function output");
        };
        let FunctionCallOutputBody::Text(text) = output.body else {
            anyhow::bail!("cancelled tool output should be text");
        };
        assert!(text.contains("aborted by user"));

        let actual = records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain(..)
            .collect::<Vec<_>>();
        assert_eq!(vec![ToolCallOutcome::Aborted], actual);

        Ok(())
    }
}
