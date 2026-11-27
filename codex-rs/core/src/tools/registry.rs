mod output_projection;

pub(crate) use output_projection::ModelToolProjection;
use output_projection::consolidated_history_output_text;
use output_projection::generic_projection_is_exempt;
use output_projection::history_output_text;
use output_projection::prepare_model_projection;
use output_projection::preserved_non_text_content;
use output_projection::project_model_output;
use output_projection::valid_owner_drained_receipt;

use crate::FunctionCallError;
use crate::agent::task_capabilities::TypedToolClass;
use crate::client_common::ToolSchemaArtifact;
use crate::hook_runtime::PreToolUseHookResult;
use crate::hook_runtime::prepare_additional_context_items;
use crate::hook_runtime::record_additional_contexts;
use crate::hook_runtime::run_post_tool_use_hooks;
use crate::hook_runtime::run_pre_tool_use_hooks;
use crate::sandbox_tags::permission_profile_policy_tag;
use crate::sandbox_tags::permission_profile_sandbox_tag;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::session::turn_execution::PendingOwnerDrainedContinuation;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolCallSource;
use crate::tools::context::ToolDispatchState;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::flat_tool_name;
use crate::tools::handlers::ParsedFunctionArguments;
use crate::tools::handlers::parsed_function_argument_value;
use crate::tools::handlers::with_parsed_function_arguments;
use crate::tools::hook_names::HookToolName;
use crate::tools::lifecycle::notify_tool_finish;
use crate::tools::lifecycle::notify_tool_start;
use crate::tools::tool_dispatch_trace::mark_tool_handler_entry;
use crate::tools::tool_dispatch_trace::mark_tool_handler_exit;
use crate::tools::tool_dispatch_trace::record_history_persistence;
use crate::tools::tool_dispatch_trace::record_output_projection;
use crate::tools::tool_dispatch_trace::record_post_tool_hook;
use crate::tools::tool_dispatch_trace::record_pre_tool_hook;
use codex_config::schema::canonicalize as canonicalize_json;
use codex_extension_api::ToolCallOutcome;
use codex_hooks::PostToolUseOutcome;
use codex_otel::TOOL_LIFECYCLE_PHASE_DURATION_METRIC;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::HookEventName;
use codex_protocol::protocol::TurnTimingDeterministicContinuationReceipt;
use codex_tools::CanonicalToolResult;
use codex_tools::ToolName;
use codex_tools::ToolOutputOutcome;
use codex_tools::ToolOutputProjectionMetadata;
use codex_tools::ToolSpec;
use codex_utils_output_truncation::truncate_text_to_token_ceiling;
use futures::future::BoxFuture;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;

pub(crate) type ToolTelemetryTags = Vec<(&'static str, String)>;

fn record_lifecycle_phase(invocation: &ToolInvocation, phase: &'static str, started: Instant) {
    let tool_name = flat_tool_name(&invocation.tool_name);
    let elapsed = started.elapsed();
    invocation
        .step_context
        .turn
        .session_telemetry
        .record_duration(
            TOOL_LIFECYCLE_PHASE_DURATION_METRIC,
            elapsed,
            &[("phase", phase), ("tool", tool_name.as_ref())],
        );
    match phase {
        "pre_hooks" => record_pre_tool_hook(elapsed),
        "post_hooks" => record_post_tool_hook(elapsed),
        "projection" => record_output_projection(elapsed),
        _ => {}
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ToolExecutionTiming {
    /// Time the core handler future as the actual tool invocation.
    Handler,
    /// A narrower runtime boundary inside the handler owns execution timing.
    NestedRuntime,
    /// The handler is an interactive wait rather than machine tool execution.
    Interactive,
}

pub use codex_tools::ToolExecutor;
pub use codex_tools::ToolExposure;

#[derive(Clone, Copy, Debug)]
pub(crate) enum ToolCleanupPolicy {
    InterruptibleRead,
    ProcessTermination,
    ExternalOperation,
    CommitBarrier,
}

impl ToolCleanupPolicy {
    fn deadline(self) -> Duration {
        Duration::from_secs(match self {
            Self::InterruptibleRead => 2,
            Self::ProcessTermination => 60,
            Self::ExternalOperation => 30,
            Self::CommitBarrier => 120,
        })
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum TerminalFailureReuse {
    #[default]
    Never,
    /// Also permits reuse of a previously rejected malformed JSON payload.
    /// The runtime must parse JSON before any effects. Syntax is rechecked by
    /// the current parser; schema errors and transient failures do not qualify.
    RequestRevisionAndJsonSyntax,
}

/// Native argument contracts supported by command evidence classification.
#[derive(Clone, Copy, Debug)]
pub(crate) enum CommandArgumentFormat {
    Exec,
    Shell,
}

impl CommandArgumentFormat {
    pub(crate) fn canonical_name(self) -> ToolName {
        ToolName::plain(match self {
            Self::Exec => "exec_command",
            Self::Shell => "shell_command",
        })
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ToolSemanticCapabilities {
    pub(crate) mutation: bool,
    pub(crate) coordination: bool,
    pub(crate) command: Option<CommandArgumentFormat>,
}

/// Typed runtime contract for locally executed tools, including scheduling,
/// evidence, cancellation and host-owned delivery capabilities.
pub(crate) trait CoreToolRuntime: ToolExecutor<ToolInvocation> {
    /// Canonical input contract for local Code Mode discovery. API-facing
    /// compatibility projections remain in `spec()`.
    fn code_mode_input_schema(&self) -> Option<Value> {
        None
    }

    /// Pure preparation may overlap baseline capture only when the runtime
    /// awaits `wait_for_workspace_baseline` before reading or changing files.
    fn prepares_during_workspace_baseline(&self) -> bool {
        false
    }

    /// Trusted opt-in publication receipt; success alone is not completion.
    fn terminal_result_adapter(&self) -> Option<&'static str> {
        None
    }

    /// A producer-owned wire contract, not inferred from an alias's name.
    fn command_argument_format(&self) -> Option<CommandArgumentFormat> {
        None
    }

    fn cancellation_recovery(
        &self,
        result: Option<&Value>,
        error: Option<&str>,
    ) -> crate::tools::context::ToolEffectRecovery {
        crate::tools::context::ToolEffectRecovery::unknown(result, error)
    }

    fn terminal_failure_reuse(&self) -> TerminalFailureReuse {
        TerminalFailureReuse::Never
    }

    /// Pure native front-door expansion, before scheduling and evidence policy
    /// select a runtime. No I/O, authorization or effects belong here.
    fn resolve_call(
        &self,
        _payload: &ToolPayload,
    ) -> Result<Option<(ToolName, ToolPayload)>, FunctionCallError> {
        Ok(None)
    }

    /// Host-owned native operation metadata for the immutable Code Mode catalog.
    fn operation_routes(&self) -> BTreeMap<String, ToolName> {
        BTreeMap::new()
    }

    /// Resolve a structured request to its executable operation before hooks
    /// and command authorization inspect it. The target must be registered.
    fn prepare_invocation<'a>(
        &'a self,
        invocation: ToolInvocation,
    ) -> BoxFuture<'a, Result<ToolInvocation, FunctionCallError>> {
        Box::pin(async move { Ok(invocation) })
    }
    fn tool_execution_timing(&self) -> ToolExecutionTiming {
        ToolExecutionTiming::Handler
    }

    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(
            payload,
            ToolPayload::Function { .. } | ToolPayload::ToolSearch { .. }
        )
    }

    /// Whether cancellation should notify the handler and supervise its teardown.
    fn waits_for_runtime_cancellation(&self) -> bool {
        false
    }

    /// Whether an admitted state commit must finish before an aborted response.
    /// Unlike general teardown, this work has a consumer-visible ordering contract.
    fn cancellation_requires_commit_barrier(&self) -> bool {
        false
    }

    /// Whether this runtime can retain processes in the unified-exec manager.
    /// Routing cleanup by runtime capability keeps aliases and namespaced
    /// registrations on the same cancellation path as the canonical name.
    fn owns_unified_exec_processes(&self) -> bool {
        false
    }

    /// Opts this runtime into shared repository admission only for payloads
    /// it guarantees are observational. Workspace evidence classification is
    /// not proof of read-only execution: an opaque shell may both read and write.
    /// Unknown runtimes remain exclusive; aliases inherit their runtime.
    fn permits_shared_workspace_observation(&self, _payload: &ToolPayload) -> bool {
        false
    }

    /// Trusted passive runtimes neither read nor mutate the workspace and need
    /// no repository admission. Hooks retain their separate workspace leases.
    /// Unknown and external runtimes must not opt in through a public name.
    fn is_workspace_independent(&self) -> bool {
        false
    }

    /// Orchestrators acquire workspace leases through each nested dispatch.
    /// Holding an outer lease while waiting on them would deadlock. This is
    /// runtime-owned, never inferred from a tool's public name.
    fn delegates_workspace_admission(&self) -> bool {
        false
    }

    /// A trusted read-only notification waiter must allow the producers it
    /// waits for to mutate the workspace. It captures non-atomic state itself;
    /// authorization and independently admitted pre/post hooks still apply.
    /// Payload-sensitive so ordinary observations retain existing admission.
    fn waits_for_workspace_changes(&self, _payload: &ToolPayload) -> bool {
        false
    }

    /// Process control is effectful, not a read-only observation. It must be
    /// able to stop a process that holds the workspace lease, so admission is
    /// owned by the process manager rather than the repository gate. This does
    /// not bypass tool authorization or the handler's capability checks.
    fn controls_process(&self, _payload: &ToolPayload) -> bool {
        false
    }

    /// Native file mutations whose exact destinations the runtime enforces.
    /// Admission proves locality and parent/endpoint identity; plain updates
    /// can claim an existing regular file, while additions claim its parent.
    /// This is never inferred from a public tool name or opaque shell text.
    /// Some(empty) requires global mutation exclusion; None is not an opt-in.
    fn native_addition_paths(&self, _payload: &ToolPayload) -> Option<Vec<std::path::PathBuf>> {
        None
    }

    /// Identity used to classify this tool's calls for scheduling and evidence.
    /// A wrapper that registers a handler under another name or namespace
    /// returns the handler's identity, so the alias is classified the same way.
    fn semantic_tool_name(&self) -> ToolName {
        self.tool_name()
    }

    /// Bounded teardown allowance, distinct from the operation's execution timeout.
    /// Unknown runtimes keep the conservative shared deadline: aborting cleanup
    /// early turns their effects into uncertain ones. A runtime may override
    /// this when its teardown is known to be shorter or longer.
    fn cancellation_cleanup_deadline(&self) -> std::time::Duration {
        self.cancellation_cleanup_policy().deadline()
    }

    fn cancellation_cleanup_policy(&self) -> ToolCleanupPolicy {
        if self.cancellation_requires_commit_barrier() {
            ToolCleanupPolicy::CommitBarrier
        } else if self.owns_unified_exec_processes() {
            ToolCleanupPolicy::ProcessTermination
        } else {
            ToolCleanupPolicy::ExternalOperation
        }
    }

    fn telemetry_tags<'a>(
        &'a self,
        _invocation: &'a ToolInvocation,
    ) -> BoxFuture<'a, ToolTelemetryTags> {
        Box::pin(async { Vec::new() })
    }

    fn post_tool_use_hook_name(&self, invocation: &ToolInvocation) -> Option<HookToolName> {
        matches!(&invocation.payload, ToolPayload::Function { .. })
            .then(|| function_hook_tool_name(invocation))
    }

    fn post_tool_use_payload(
        &self,
        invocation: &ToolInvocation,
        result: &dyn ToolOutput,
    ) -> Option<PostToolUsePayload> {
        let ToolPayload::Function { arguments } = &invocation.payload else {
            return None;
        };

        Some(PostToolUsePayload {
            tool_name: self.post_tool_use_hook_name(invocation)?,
            tool_use_id: result.post_tool_use_id(&invocation.call_id),
            tool_input: result
                .post_tool_use_input(&invocation.payload)
                .unwrap_or_else(|| function_hook_tool_input(arguments)),
            tool_response: result
                .post_tool_use_response(&invocation.call_id, &invocation.payload)
                .or_else(|| {
                    // Most function tools can expose their model-facing output
                    // as the hook response. Outputs with a more stable hook
                    // contract should override post_tool_use_response above.
                    let ResponseInputItem::FunctionCallOutput {
                        output: FunctionCallOutputPayload { body, .. },
                        ..
                    } = result.to_response_item(&invocation.call_id, &invocation.payload)
                    else {
                        return None;
                    };

                    serde_json::to_value(body).ok()
                })?,
        })
    }

    /// Hook identity that `pre_tool_use_payload` uses for this call, without
    /// parsing hook input. `None` means PreToolUse never runs for the call.
    /// Overrides must return `Some` whenever `pre_tool_use_payload` does.
    fn pre_tool_use_hook_name(
        &self,
        tool_name: &ToolName,
        payload: &ToolPayload,
    ) -> Option<HookToolName> {
        matches!(payload, ToolPayload::Function { .. })
            .then(|| function_hook_tool_name_for(tool_name))
    }

    fn pre_tool_use_payload(&self, invocation: &ToolInvocation) -> Option<PreToolUsePayload> {
        let ToolPayload::Function { arguments } = &invocation.payload else {
            return None;
        };

        Some(PreToolUsePayload {
            tool_name: self.pre_tool_use_hook_name(&invocation.tool_name, &invocation.payload)?,
            tool_input: function_hook_tool_input(arguments),
        })
    }

    /// Local execution cwd reported as `PreToolUse` hook `cwd` for tools with a
    /// per-call working directory. `None` keeps the host cwd (remote commands,
    /// other tools). This never moves the hook process.
    fn pre_tool_use_cwd(
        &self,
        _invocation: &ToolInvocation,
    ) -> Option<codex_utils_path_uri::PathUri> {
        None
    }

    /// Rebuilds a tool invocation from hook-facing `tool_input`.
    ///
    /// Tools that opt into input-rewriting hooks should invert the same stable
    /// hook contract they expose from `pre_tool_use_payload`.
    fn with_updated_hook_input(
        &self,
        invocation: ToolInvocation,
        updated_input: Value,
    ) -> Result<ToolInvocation, FunctionCallError> {
        let ToolPayload::Function { .. } = &invocation.payload else {
            return Err(FunctionCallError::RespondToModel(
                "hook input rewrite received unsupported function tool payload".to_string(),
            ));
        };

        let arguments = serde_json::to_string(&updated_input).map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "failed to serialize rewritten {} arguments: {err}",
                flat_tool_name(&invocation.tool_name)
            ))
        })?;
        Ok(ToolInvocation {
            payload: ToolPayload::Function { arguments },
            ..invocation
        })
    }

    /// Creates an optional consumer for streamed tool argument diffs.
    fn create_diff_consumer(&self) -> Option<Box<dyn ToolArgumentDiffConsumer>> {
        None
    }
}

/// Consumes streamed argument diffs for a tool call and emits protocol events
/// derived from partial tool input.
pub(crate) trait ToolArgumentDiffConsumer: Send {
    /// Consume the next argument diff for a tool call.
    fn consume_diff(&mut self, turn: &TurnContext, call_id: String, diff: &str)
    -> Option<EventMsg>;

    /// Finish consuming argument diffs before the tool call completes.
    fn finish(&mut self) -> Result<Option<EventMsg>, FunctionCallError> {
        Ok(None)
    }
}

pub(crate) struct AnyToolResult {
    pub(crate) call_id: String,
    pub(crate) payload: ToolPayload,
    pub(crate) result: Box<dyn ToolOutput>,
    pub(crate) model_projection: Option<ModelToolProjection>,
    pub(crate) source_dependencies:
        Option<std::collections::BTreeSet<crate::tool_history::SourceDependencyV1>>,
    pub(crate) code_mode_feedback: Vec<FunctionCallOutputContentItem>,
    pub(crate) operation_history: Option<Value>,
    pub(crate) workspace_source_invalidated: bool,
    pub(crate) provider_read_offer_eligible: bool,
}

/// The response and artifact still settle normally. Only a matching immutable
/// provider input may replace this exact body with the recovery-only view.
#[derive(Debug)]
pub(crate) struct ProviderReadOffer {
    pub(crate) call_id: String,
    pub(crate) execution_id: String,
    pub(crate) full: String,
    pub(crate) offer: String,
    pub(crate) receipt: Value,
    pub(crate) history: Option<Value>,
}

fn exact_read_offer(output: &ResponseInputItem, receipt: &Value) -> Option<(String, String)> {
    let ResponseInputItem::FunctionCallOutput { output, .. } = output else {
        return None;
    };
    let FunctionCallOutputBody::Text(full) = &output.body else {
        return None;
    };
    let mut packet: Value = serde_json::from_str(full).ok()?;
    let artifact_id = packet["artifact_id"].as_str()?;
    let bytes = packet["canonical_bytes"].as_u64()?;
    if output.success != Some(true)
        || packet["complete"] != true
        || packet["retained_artifact_complete"] != true
        || packet["retained_bytes"] != bytes
        || packet["source_sha256"] != receipt["contentSha256"]
        || receipt["contentBytes"] != bytes
        || ["recovery", "continuation", "delivery_note", "selector_errors",
            "selector_bindings", "inspection_proof_error", "snapshot_error", "evidence_advisory"]
            .iter().any(|key| packet.get(*key).is_some())
    {
        return None;
    }
    let rows = packet["results"].as_array()?;
    if rows.is_empty() || rows.iter().any(|row| {
        row["selector"]["kind"] != "bytes" || row["complete"] != true || row["status"] != "ok"
        || !row["text"].is_string()
        || row.as_object().is_none_or(|row| row.keys().any(|key| !matches!(
            key.as_str(), "selector" | "status" | "complete" | "canonical_range" | "text" | "selector_index"
        )))
    }) {
        return None;
    }
    let selectors = receipt["ranges"].as_array()?.iter().map(|range| {
        let start = range[0].as_u64()?;
        let end = range[1].as_u64()?;
        (start < end && end <= bytes).then(|| serde_json::json!({"kind":"bytes", "start":start, "end":end}))
    }).collect::<Option<Vec<_>>>()?;
    if selectors.is_empty() || selectors.len() > 64 {
        return None;
    }
    let recovery = serde_json::json!({
        "tool":"read_tool_output",
        "arguments":{"artifact_id":artifact_id, "selectors":selectors},
        "selection_complete":false, "file_complete":false,
    });
    packet["complete"] = Value::Bool(false);
    packet["file_complete"] = Value::Bool(false);
    packet["delivered_selection_complete"] = Value::Bool(false);
    packet["selection_status"] = serde_json::json!("partial");
    packet["results"] = serde_json::json!([]);
    packet["recovery"] = recovery;
    packet["delivery_note"] = serde_json::json!(
        "The read succeeded, but this request carried no source bytes from it. Use recovery.arguments with read_tool_output for the exact retained ranges."
    );
    let offer = packet.to_string();
    (offer.len() < full.len()).then(|| (full.clone(), offer))
}

pub(crate) fn record_tool_result_measurement(
    turn: &TurnContext,
    execution_id: &str,
    call_id: &str,
    tool_name: &str,
    result: &Result<AnyToolResult, FunctionCallError>,
) {
    use sha2::Digest;
    let timing = &turn.turn_timing_state;
    let name = tool_name.strip_prefix("functions.").unwrap_or(tool_name);
    if matches!(name, "read_file" | "read_tool_output" | "exec_command" | "shell_command" | "shell") {
        let signal = result.as_ref().ok().and_then(|result| result.result.sampling_request_signal());
        let history = result.as_ref().ok().and_then(|result| result.operation_history.clone());
        let receipt = signal.as_ref().and_then(|signal| signal.get("native_read"));
        let staged = name == "read_file"
            && turn.extension_data.get::<crate::session::turn::ProviderReadOffers>()
                .zip(result.as_ref().ok())
                .zip(receipt)
                .is_some_and(|((offers, result), receipt)| {
                    result.provider_read_offer_eligible && !result.workspace_source_invalidated
                        && exact_read_offer(&result.response(), receipt).is_some_and(|(full, offer)| {
                            let digest = sha2::Sha256::digest(full.as_bytes()).into();
                            let staged = offers.stage(ProviderReadOffer {
                                call_id: call_id.to_owned(), execution_id: execution_id.to_owned(),
                                full, offer, receipt: receipt.clone(), history: history.clone(),
                            });
                            if staged {
                                turn.extension_data
                                    .get_or_init(crate::session::turn::terminal_read_delivery::TerminalReadDelivery::default)
                                    .remember_exact(call_id, digest);
                            }
                            staged
                        })
                });
        if name == "read_file"
            && let Ok(result) = result
            && result.provider_read_offer_eligible
            && !result.workspace_source_invalidated
            && signal.as_ref().is_some_and(|signal| signal["provider_terminal_read_eligible"] == true)
        {
            turn.extension_data
                .get_or_init(crate::session::turn::terminal_read_delivery::TerminalReadDelivery::default)
                .stage(&result.response());
        }
        if !staged {
            timing.record_native_operation(execution_id, call_id,
                if matches!(name, "read_file" | "read_tool_output") { "read" } else { "command" },
                receipt.cloned(), history);
        }
    }
    let (rendered, projection) = match result {
        Ok(result) => (serde_json::to_vec(&result.response()).ok(), result.model_projection.as_ref()),
        Err(error) => (serde_json::to_vec(&serde_json::json!({"error":error.to_string()})).ok(), None),
    };
    let Some(rendered) = rendered else {
        timing.record_tool_output_measurement(call_id, execution_id, serde_json::Value::Null);
        return;
    };
    let rendered_hash = hex::encode(sha2::Sha256::digest(&rendered));
    let (canonical_hash, original_bytes, truncated) = projection.map_or_else(
        || (rendered_hash.clone(), rendered.len() as u64, false),
        |projection| (projection.canonical_sha256.clone(), projection.canonical_bytes,
            projection.projection_truncated || projection.omitted_sections > 0),
    );
    // Preserve typed attribution independently of script/outer display, without
    // copying diagnostics, operation handles or arbitrary failure details.
    let failure = match result {
        Ok(result) => result.result.tool_failure(),
        Err(error) => Some(error.failure()),
    };
    let failure = failure.map(|failure| serde_json::json!({
        "code": failure.code,
        "execution_state": failure.execution_state,
        "effects": failure.effects,
        "retry_safe": failure.retry_safe,
    }));
    timing.record_tool_output_measurement(call_id, execution_id, serde_json::json!({
        "byteAccounting":"canonical_and_rendered_v1",
        "contentSha256":canonical_hash, "originalBytes":original_bytes,
        "renderedSha256":rendered_hash, "renderedBytes":rendered.len(),
        "truncated":truncated,
        "toolFailure":failure,
    }));
}

impl AnyToolResult {
    fn install_model_projection(
        &mut self,
        model_projection: Option<ModelToolProjection>,
        source_dependencies_override: Option<
            std::collections::BTreeSet<crate::tool_history::SourceDependencyV1>,
        >,
    ) {
        self.source_dependencies = source_dependencies_override
            .or_else(|| {
                model_projection
                    .as_ref()
                    .map(|projection| projection.source_dependencies.clone())
            })
            .or_else(precomputed_projection_source_dependencies);
        self.model_projection = model_projection;
    }

    pub(crate) fn response(&self) -> ResponseInputItem {
        let response = self.response_before_operation_history();
        let notice = self.operation_history.as_ref()
            .map(|history| serde_json::json!({"operation_history": history}).to_string());
        with_operation_history_notice(response, notice.as_deref())
    }

    pub(crate) fn response_before_operation_history(&self) -> ResponseInputItem {
        if let Some(projection) = self.model_projection.as_ref() {
            with_model_delivery_notice(
                projection.response(),
                self.result.model_delivery_notice(),
            )
        } else {
            self.result.to_response_item(&self.call_id, &self.payload)
        }
    }

    /// Register the final text only after both it and the producer's original
    /// canonical artifact have protected, independently recoverable identities.
    pub(crate) async fn reconcile_settled_history_candidate(&mut self, session: &Session) {
        if self.operation_history.is_none() || self.outcome_for_logging() != ToolOutputOutcome::Success {
            return;
        }
        let Some(mut candidate) = self.model_projection.as_ref().and_then(|p| p.candidate.clone()) else {
            return;
        };
        if !candidate.successful || !candidate.complete || !candidate.projection_eligible {
            return;
        }
        let response = self.response();
        if !matches!(&response, ResponseInputItem::FunctionCallOutput { .. } | ResponseInputItem::CustomToolCallOutput { .. })
            || !preserved_non_text_content(&response).is_empty()
        {
            return;
        }
        let delivered = history_output_text(&response)
            .unwrap_or_else(|| consolidated_history_output_text(&response));
        if delivered == candidate.bounded_model_output {
            return;
        }
        if session.try_register_tool_artifact_origin(
            candidate.artifact_id.clone(), candidate.call_id.clone(),
            candidate.artifact_bytes, candidate.artifact_sha256.clone(),
        ).await.is_err() {
            return;
        }
        // The visible projection can already refer to the producer artifact.
        // Keep its selector space and source identity intact; the new archive
        // adds the final settlement and links to that protected original.
        let archive = CanonicalToolResult::json(serde_json::json!({
            "kind": "settled_tool_output",
            "delivered_output": delivered,
            "producer_artifact": {
                "artifact_id": candidate.artifact_id,
                "bytes": candidate.artifact_bytes,
                "sha256": candidate.artifact_sha256,
            },
        }));
        let artifact = crate::tools::command_output_artifact::create_tool_history_output_artifact(
            session.codex_home().await.as_path(), &session.thread_id.to_string(), &archive,
        ).await;
        let Some(artifact_id) = artifact.artifact_id() else { return; };
        if !artifact.complete || artifact.retained_bytes != archive.exact_bytes
            || !artifact.unavailable_ranges.is_empty() {
            return;
        }
        candidate.artifact_id = artifact_id;
        candidate.artifact_bytes = archive.exact_bytes;
        candidate.artifact_sha256 = archive.sha256;
        candidate.bounded_model_output = delivered;
        session.register_tool_history_candidate(candidate).await;
    }

    pub(crate) fn success_for_logging(&self) -> bool {
        self.result.success_for_logging()
    }

    pub(crate) fn outcome_for_logging(&self) -> ToolOutputOutcome {
        self.result.outcome_for_logging()
    }

    pub(crate) fn cancellation_result(&self) -> Value {
        let result = self.result.code_mode_result(&self.payload);
        let children = self.result.projection_metadata().and_then(|metadata| {
            metadata.essential_inline.get("cancelled_nested_calls").cloned()
        });
        match children {
            Some(children) => {
                serde_json::json!({"result":result,"cancelled_nested_calls":children})
            }
            None => result,
        }
    }

    pub(crate) fn outcome_context(&self) -> codex_tools::ToolOutputOutcomeContext {
        self.result.outcome_context()
    }

    pub(crate) fn sampling_request_signal(&self) -> Option<serde_json::Value> {
        let mut signal = self.result.sampling_request_signal();
        if matches!(self.outcome_for_logging(), ToolOutputOutcome::Failure | ToolOutputOutcome::TimedOut) {
            let signal = signal.get_or_insert_with(|| serde_json::json!({}));
            let fingerprint = self.operation_history.as_ref()
                .and_then(|history| history.pointer("/instance/fingerprint"))
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| format!("tool_failure.unclassified_instance.{}", self.call_id));
            signal["failure_signature"] = serde_json::json!(fingerprint);
        }
        let mut signal = signal?;
        if signal.get("command_evidence") != Some(&Value::Bool(true))
            || self.outcome_for_logging() != ToolOutputOutcome::Success
        {
            return Some(signal);
        }
        // Admission/dispatch owns these dependencies, including repaired commands.
        // Text equality without provenance cannot establish cross-tool equivalence.
        let Some(dependencies) = self
            .source_dependencies
            .as_ref()
            .filter(|set| !set.is_empty())
        else {
            // Unknown provenance prevents cross-action reuse, not progress.
            // The collector binds unscoped evidence to its action identity.
            return Some(signal);
        };
        let ToolPayload::Function { arguments } = &self.payload else {
            return Some(signal);
        };
        let arguments: Value = serde_json::from_str(arguments).ok()?;
        let identity = signal.get("semantic_evidence")?.clone();
        let mut scope = serde_json::json!({"dependencies": dependencies});
        if let Some(environment) = arguments.get("environment_id") {
            scope["environment"] = environment.clone();
        }
        // Dependencies identify sources, not which query/selection was observed.
        // Preserve that distinction even when two queries return identical text.
        // Only the command-field alias differs between the two shell routes;
        // output limits and observation deadlines do not create coverage.
        let mut operation = serde_json::Map::new();
        for key in ["program", "args", "script_body", "shell", "login", "tty"] {
            if let Some(value) = arguments.get(key) {
                operation.insert(key.to_string(), value.clone());
            }
        }
        if let Some(command) = arguments.get("cmd").or_else(|| arguments.get("command")) {
            operation.insert("command".to_string(), command.clone());
        }
        scope["operation"] = Value::Object(operation);
        signal["semantic_evidence"] = serde_json::json!({
            "source": "workspace-command",
            "scope": scope,
            "identity": identity,
        });
        Some(signal)
    }

    pub(crate) fn requires_canonical_artifact(&self) -> bool {
        self.result.requires_canonical_artifact()
    }

    /// Dispatcher-authenticated identity of retained canonical output. Storage
    /// alone does not imply unread evidence; the cell compares delivered coverage.
    pub(crate) fn canonical_delivery_artifact(&self) -> Option<(String, String, u64)> {
        let candidate = self.model_projection.as_ref()?.candidate.as_ref()?;
        Some((
            candidate.artifact_id.clone(),
            candidate.artifact_sha256.clone(),
            candidate.artifact_bytes,
        ))
    }

    pub(crate) fn projected_source_dependencies(
        &self,
    ) -> Option<&std::collections::BTreeSet<crate::tool_history::SourceDependencyV1>> {
        self.source_dependencies.as_ref()
    }

    pub(crate) fn take_code_mode_feedback(&mut self) -> Vec<FunctionCallOutputContentItem> {
        std::mem::take(&mut self.code_mode_feedback)
    }

    pub(crate) fn deterministic_continuation_receipts(
        &self,
    ) -> Vec<TurnTimingDeterministicContinuationReceipt> {
        let mut receipts = self.result.deterministic_continuation_receipts();
        if let Some(receipt) = self
            .model_projection
            .as_ref()
            .and_then(|projection| projection.deterministic_continuation_receipt.clone())
        {
            receipts.push(receipt);
        }
        receipts
    }

    pub(crate) fn intrinsic_deterministic_continuation_receipts(
        &self,
    ) -> Vec<TurnTimingDeterministicContinuationReceipt> {
        let owner_drained_identity = self
            .owner_drained_continuation()
            .and_then(|continuation| continuation.receipt.runtime_identity());
        self.result
            .deterministic_continuation_receipts()
            .into_iter()
            .filter(|receipt| {
                owner_drained_identity
                    .as_ref()
                    .is_none_or(|identity| receipt.runtime_identity().as_ref() != Some(identity))
            })
            .collect()
    }

    pub(crate) fn deterministic_continuation_owner_key(&self) -> Option<String> {
        self.result.deterministic_continuation_owner_key()
    }

    pub(crate) fn owner_drained_continuation(&self) -> Option<PendingOwnerDrainedContinuation> {
        let result_content = self.result.deterministic_continuation_content();
        if !result_content.is_empty() {
            let mut receipts = self
                .result
                .deterministic_continuation_receipts()
                .into_iter();
            let receipt = receipts.next()?;
            if receipts.next().is_none() && valid_owner_drained_receipt(&receipt) {
                return Some(PendingOwnerDrainedContinuation {
                    preserved_content: result_content,
                    receipt,
                });
            }
        }
        let projection = self.model_projection.as_ref()?;
        let receipt = projection.deterministic_continuation_receipt.clone()?;
        if !valid_owner_drained_receipt(&receipt) {
            return None;
        }
        Some(PendingOwnerDrainedContinuation {
            preserved_content: (!projection.deterministic_continuation_content.is_empty())
                .then(|| projection.deterministic_continuation_content.clone())?,
            receipt,
        })
    }

    pub(crate) fn merge_owner_drained_continuations(
        &mut self,
        continuations: Vec<PendingOwnerDrainedContinuation>,
    ) -> Vec<TurnTimingDeterministicContinuationReceipt> {
        self.model_projection
            .as_mut()
            .map_or_else(Vec::new, |projection| {
                projection.merge_owner_drained_continuations(continuations)
            })
    }

    pub(crate) fn into_response(self) -> ResponseInputItem {
        let notice = self.operation_history.as_ref()
            .map(|history| serde_json::json!({"operation_history": history}).to_string());
        if let Some(projection) = self.model_projection {
            return with_operation_history_notice(
                with_model_delivery_notice(projection.into_response(), self.result.model_delivery_notice()),
                notice.as_deref(),
            );
        }
        let Self {
            call_id,
            payload,
            result,
            ..
        } = self;
        with_operation_history_notice(result.to_response_item(&call_id, &payload), notice.as_deref())
    }


    pub(crate) fn code_mode_result_with_budget(
        self,
        budget: usize,
    ) -> (serde_json::Value, Option<serde_json::Value>) {
        (
            self.result.code_mode_result_with_budget(&self.payload, budget),
            self.operation_history,
        )
    }

    pub(crate) fn code_mode_failure_is_error(&self) -> bool {
        self.result.code_mode_failure_is_error()
    }
}

/// Materialize the model-visible representation for a terminal result owned by
/// the cancellation path. That path deliberately bypasses post-tool hooks and
/// the ordinary registry finish notification, but it still owes the same
/// projection attribution as every other accepted tool call.
pub(crate) async fn record_synthetic_terminal_attempt(
    invocation: &ToolInvocation,
    result: &mut AnyToolResult,
    dispatch_state: &ToolDispatchState,
) {
    use codex_protocol::tool_failure::ToolFailureCode;

    let Some(mut failure) = result.result.tool_failure().filter(|failure|
        matches!(failure.code, ToolFailureCode::Cancelled | ToolFailureCode::TimedOut))
    else {
        return;
    };
    let cause = invocation.source.cancellation_cause();
    if let Some(started) = dispatch_state.cancelled_nested_attempt(cause.clone()) {
        if let Some(history) = dispatch_state.settled_history(&result.call_id) {
            // The existing registry path already recorded a known settlement.
            result.operation_history = Some(history.clone());
        } else {
            let mut attempt = started.clone();
            // Carry the real cleanup facts and handles without implying that
            // reaching the handler boundary established any external effects.
            let outcome = if cause == Some(codex_code_mode::CancellationCause::NestedDeadline) {
                failure.code = ToolFailureCode::TimedOut;
                ToolOutputOutcome::TimedOut
            } else {
                result.outcome_for_logging()
            };
            attempt.settle_outcome(outcome, Some(failure));
            result.operation_history = Some(invocation.session.record_operation_attempt(attempt).await);
        }
    } else {
        let mut attempt = crate::tools::operation_history::capture(invocation, None, None).await;
        // Cancellation may have interrupted a rewriting hook. Preserve the
        // requested parameters, but never certify them as the executed input.
        attempt.mark_requested_input();
        crate::tools::operation_history::settle_output(&mut attempt, result);
        result.operation_history = Some(invocation.session.record_operation_attempt(attempt).await);
    }
}

pub(crate) async fn install_synthetic_terminal_projection(
    invocation: &ToolInvocation,
    result: &mut AnyToolResult,
) {
    let parsed_function_arguments = ParsedFunctionArguments::from_payload(&invocation.payload);
    let provider_visible = projection_is_provider_visible(&invocation.source);
    let phase_started = Instant::now();
    let model_projection = match prepare_model_projection(
        invocation,
        result,
        parsed_function_arguments.as_ref(),
        /*source_dependencies_override*/ None,
        /*force_inline_carrier*/ false,
        /*track_for_admission*/ false,
    )
    .await
    {
        Some(input) => project_model_output(input).await,
        None => None,
    };
    record_lifecycle_phase(invocation, "projection", phase_started);
    if let Some(projection) = &model_projection {
        invocation
            .step_context
            .turn
            .turn_timing_state
            .record_tool_output_projection_facts_async(
                projection.canonical_bytes,
                projection.canonical_tokens,
                projection.model_bytes,
                projection.projected_tokens,
                projection.artifact_created,
                projection.artifact_reused,
                projection.projection_truncated,
                projection.omitted_sections,
                provider_visible,
            )
            .await;
    }
    result.install_model_projection(model_projection, /*source_dependencies_override*/ None);
}

struct PostToolUseFeedbackOutput {
    original: Box<dyn ToolOutput>,
    model_visible: FunctionToolOutput,
    context_error: Option<String>,
    accounting_error: Option<String>,
}

fn apply_post_tool_use_outcome(
    result: &mut AnyToolResult,
    outcome: PostToolUseOutcome,
) -> Vec<String> {
    // A post hook observes a call whose side effects have already happened. Its
    // block decision cannot undo the call, so returning a failed tool result
    // would only invite the model to repeat the mutation. Deliver the rejecting
    // owner's verdict separately; do not accept its optional additional context.
    if outcome.should_block {
        let reason = outcome.feedback_message.unwrap_or_else(|| {
            "The PostToolUse hook rejected the result without a reason.".to_string()
        });
        return vec![format!(
            "PostToolUse rejected this result after the tool completed. Its effects have not been undone; do not repeat the operation merely because of this rejection. Resolve the hook's objection: {reason}"
        )];
    }
    if let Some(feedback_message) = outcome.feedback_message {
        let mut model_visible =
            FunctionToolOutput::from_text(feedback_message, /*success*/ None);
        result.code_mode_feedback.extend(model_visible.body.clone());
        // Preserve earlier runtime warnings for direct callers as well as
        // code mode; a later hook must not hide a failed durable effect commit.
        model_visible.body = result.code_mode_feedback.clone();
        result.result = Box::new(PostToolUseFeedbackOutput {
            original: std::mem::replace(
                &mut result.result,
                Box::new(FunctionToolOutput::from_text(String::new(), None)),
            ),
            model_visible,
            context_error: None,
            accounting_error: None,
        });
    }
    outcome.additional_contexts
}

impl ToolOutput for PostToolUseFeedbackOutput {
    fn tool_failure(&self) -> Option<codex_protocol::tool_failure::ToolFailure> {
        if self.accounting_error.is_some() || self.context_error.is_some() {
            None
        } else {
            self.original.tool_failure()
        }
    }

    fn allows_generic_artifact_retention(&self) -> bool {
        self.original.allows_generic_artifact_retention()
    }

    fn model_delivery_unavailable(&self) -> bool {
        self.original.model_delivery_unavailable()
    }

    fn code_mode_failure_is_error(&self) -> bool {
        self.accounting_error.is_some() || self.original.code_mode_failure_is_error()
    }

    fn model_delivery_notice(&self) -> Option<&str> {
        self.accounting_error
            .as_deref()
            .or_else(|| self.original.model_delivery_notice())
    }

    fn log_preview(&self) -> String {
        self.original.log_preview()
    }

    fn log_output_stats(&self) -> Option<(usize, usize)> {
        self.original.log_output_stats()
    }

    fn success_for_logging(&self) -> bool {
        self.accounting_error.is_none() && self.original.success_for_logging()
    }

    fn outcome_for_logging(&self) -> ToolOutputOutcome {
        if self.accounting_error.is_some() {
            ToolOutputOutcome::Failure
        } else {
            self.original.outcome_for_logging()
        }
    }

    fn outcome_context(&self) -> codex_tools::ToolOutputOutcomeContext {
        if self.accounting_error.is_some() {
            codex_tools::ToolOutputOutcomeContext::new(ToolOutputOutcome::Failure)
        } else {
            self.original.outcome_context()
        }
    }

    fn requires_canonical_artifact(&self) -> bool {
        self.original.requires_canonical_artifact()
    }

    fn sampling_request_signal(&self) -> Option<Value> {
        let mut signal = self.original.sampling_request_signal();
        if let Some(error) = &self.context_error {
            signal.get_or_insert_with(|| serde_json::json!({}))["post_tool_context_error"] =
                Value::String(error.clone());
        }
        if let Some(error) = &self.accounting_error {
            signal.get_or_insert_with(|| serde_json::json!({}))["required_accounting_error"] =
                Value::String(error.clone());
        }
        signal
    }

    fn deterministic_continuation_receipts(
        &self,
    ) -> Vec<TurnTimingDeterministicContinuationReceipt> {
        self.original.deterministic_continuation_receipts()
    }

    fn deterministic_continuation_owner_key(&self) -> Option<String> {
        self.original.deterministic_continuation_owner_key()
    }

    fn deterministic_continuation_content(&self) -> Vec<Value> {
        self.original.deterministic_continuation_content()
    }

    fn projection_metadata(&self) -> Option<codex_tools::ToolOutputProjectionMetadata> {
        let mut metadata = self.original.projection_metadata()?;
        if let Some(feedback) = self.model_visible.projection_metadata() {
            metadata.spillable_text.extend(feedback.spillable_text);
        }
        Some(metadata)
    }

    fn canonical_result(&self, payload: &ToolPayload) -> Option<CanonicalToolResult> {
        self.original.canonical_result(payload)
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        let mut response = self.original.to_response_item(call_id, payload);
        if let ResponseInputItem::FunctionCallOutput { output, .. }
        | ResponseInputItem::CustomToolCallOutput { output, .. } = &mut response
        {
            match &mut output.body {
                FunctionCallOutputBody::Text(text) => {
                    for item in &self.model_visible.body {
                        if let FunctionCallOutputContentItem::InputText { text: feedback } = item {
                            text.push_str(if self.accounting_error.is_some() {
                                "\n\nHost settlement: "
                            } else {
                                "\n\nPostToolUse feedback: "
                            });
                            text.push_str(feedback);
                        }
                    }
                }
                FunctionCallOutputBody::ContentItems(items) => {
                    items.extend(self.model_visible.body.clone());
                }
            }
        }
        if matches!(&response, ResponseInputItem::McpToolCallOutput { .. }) {
            with_model_delivery_notice(response, self.model_delivery_notice())
        } else {
            response
        }
    }

    fn code_mode_result(&self, payload: &ToolPayload) -> Value {
        self.original.code_mode_result(payload)
    }

    fn code_mode_result_with_budget(&self, payload: &ToolPayload, budget: usize) -> Value {
        self.original.code_mode_result_with_budget(payload, budget)
    }
}

struct UnavailableModelProjectionOutput {
    original: Box<dyn ToolOutput>,
    model_visible: FunctionToolOutput,
}

const MODEL_DELIVERY_UNAVAILABLE_PREFIX: &str = "<tool_result_delivery_unavailable>";

/// Written beside the result in the existing ordered completion batch.
pub(crate) fn model_delivery_unavailable_receipt(call_id: &str) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "developer".into(),
        phase: None,
        internal_chat_message_metadata_passthrough: None,
        content: vec![codex_protocol::models::ContentItem::InputText {
            text: format!(
                "{MODEL_DELIVERY_UNAVAILABLE_PREFIX}{}",
                serde_json::json!({"version": 1, "call_id": call_id})
            ),
        }],
    }
}

pub(crate) fn is_model_delivery_unavailable_receipt(item: &ResponseItem, call_id: &str) -> bool {
    let ResponseItem::Message { role, content, .. } = item else {
        return false;
    };
    let [codex_protocol::models::ContentItem::InputText { text }] = content.as_slice() else {
        return false;
    };
    role == "developer"
        && text
            .strip_prefix(MODEL_DELIVERY_UNAVAILABLE_PREFIX)
            .and_then(|json| serde_json::from_str::<Value>(json).ok())
            .is_some_and(|value| value["version"] == 1 && value["call_id"] == call_id)
}

impl ToolOutput for UnavailableModelProjectionOutput {
    fn tool_failure(&self) -> Option<codex_protocol::tool_failure::ToolFailure> {
        self.original.tool_failure()
    }

    fn allows_generic_artifact_retention(&self) -> bool {
        self.original.allows_generic_artifact_retention()
    }

    fn model_delivery_unavailable(&self) -> bool {
        true
    }

    fn code_mode_failure_is_error(&self) -> bool {
        self.original.code_mode_failure_is_error()
    }

    fn log_preview(&self) -> String {
        self.original.log_preview()
    }

    fn log_output_stats(&self) -> Option<(usize, usize)> {
        self.original.log_output_stats()
    }

    fn success_for_logging(&self) -> bool {
        self.original.success_for_logging()
    }

    fn outcome_for_logging(&self) -> ToolOutputOutcome {
        self.original.outcome_for_logging()
    }

    fn outcome_context(&self) -> codex_tools::ToolOutputOutcomeContext {
        self.original.outcome_context()
    }

    fn sampling_request_signal(&self) -> Option<Value> {
        self.original.sampling_request_signal()
    }

    fn deterministic_continuation_receipts(
        &self,
    ) -> Vec<TurnTimingDeterministicContinuationReceipt> {
        self.original.deterministic_continuation_receipts()
    }

    fn deterministic_continuation_owner_key(&self) -> Option<String> {
        self.original.deterministic_continuation_owner_key()
    }

    fn deterministic_continuation_content(&self) -> Vec<Value> {
        self.original.deterministic_continuation_content()
    }

    fn projection_metadata(&self) -> Option<ToolOutputProjectionMetadata> {
        self.model_visible.projection_metadata()
    }

    fn canonical_result(&self, payload: &ToolPayload) -> Option<CanonicalToolResult> {
        self.model_visible.canonical_result(payload)
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        self.model_visible.to_response_item(call_id, payload)
    }

    /// Code running in a cell reads the handler's execution result even when a
    /// model projection was unavailable. The substitute exists to bound the
    /// model-facing text, not to replace what the call returned.
    fn code_mode_result(&self, payload: &ToolPayload) -> Value {
        self.original.code_mode_result(payload)
    }

    fn code_mode_result_with_budget(&self, payload: &ToolPayload, budget: usize) -> Value {
        self.original.code_mode_result_with_budget(payload, budget)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreToolUsePayload {
    /// Hook-facing tool name model.
    ///
    /// The canonical name is serialized to hook stdin, while aliases are used
    /// only for matcher compatibility.
    pub(crate) tool_name: HookToolName,
    /// Tool-specific input exposed at `tool_input`.
    ///
    /// Shell-like tools use `{ "command": ... }`; MCP tools use their resolved
    /// JSON arguments.
    pub(crate) tool_input: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PostToolUsePayload {
    /// Hook-facing tool name model.
    ///
    /// The canonical name is serialized to hook stdin, while aliases are used
    /// only for matcher compatibility.
    pub(crate) tool_name: HookToolName,
    /// The originating tool-use id exposed at `tool_use_id`.
    pub(crate) tool_use_id: String,
    /// Tool-specific input exposed at `tool_input`.
    pub(crate) tool_input: Value,
    /// Tool result exposed at `tool_response`.
    pub(crate) tool_response: Value,
}

pub(crate) struct RegisteredTool {
    provider_authority: Option<String>,
    tool_name: ToolName,
    runtime: Arc<dyn CoreToolRuntime>,
    exposure: ToolExposure,
    authorization_class: TypedToolClass,
    spec: ToolSpec,
    canonical_spec_sha256: OnceLock<String>,
    effective_input_schema: OnceLock<Option<Value>>,
    effective_contract_revision: OnceLock<String>,
    search_info: OnceLock<Option<codex_tools::ToolSearchInfo>>,
    code_mode_argument_preflight: Arc<CodeModeArgumentPreflight>,
}

impl RegisteredTool {
    pub(crate) fn new(
        runtime: Arc<dyn CoreToolRuntime>,
        authorization_class: TypedToolClass,
    ) -> Self {
        let exposure = runtime.exposure();
        Self::with_exposure(runtime, exposure, authorization_class)
    }

    pub(crate) fn with_exposure(
        runtime: Arc<dyn CoreToolRuntime>,
        exposure: ToolExposure,
        authorization_class: TypedToolClass,
    ) -> Self {
        assert_ne!(
            authorization_class,
            TypedToolClass::Unknown,
            "registered tools must declare an authorization class"
        );
        let spec = runtime.spec();
        let tool_name = spec.sole_callable_tool_name().unwrap_or_else(|| {
            panic!("registered runtime specs must declare exactly one callable tool")
        });
        Self {
            tool_name,
            runtime,
            provider_authority: None,
            exposure,
            authorization_class,
            spec,
            canonical_spec_sha256: OnceLock::new(),
            effective_input_schema: OnceLock::new(),
            effective_contract_revision: OnceLock::new(),
            search_info: OnceLock::new(),
            code_mode_argument_preflight: Arc::new(CodeModeArgumentPreflight::default()),
        }
    }

    pub(crate) fn runtime(&self) -> &Arc<dyn CoreToolRuntime> {
        &self.runtime
    }

    pub(crate) fn with_provider_authority(mut self, authority: String) -> Self {
        self.effective_contract_revision.take();
        self.provider_authority = Some(authority);
        self
    }

    pub(crate) fn provider_authority(&self) -> Option<&str> {
        self.provider_authority.as_deref()
    }

    pub(crate) fn tool_name(&self) -> &ToolName {
        &self.tool_name
    }

    pub(crate) fn exposure(&self) -> ToolExposure {
        self.exposure
    }

    pub(crate) fn authorization_class(&self) -> TypedToolClass {
        self.authorization_class
    }

    pub(crate) fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    pub(crate) fn spec_mut(&mut self) -> &mut ToolSpec {
        self.canonical_spec_sha256.take();
        self.effective_input_schema.take();
        self.effective_contract_revision.take();
        self.search_info.take();
        self.code_mode_argument_preflight = Arc::new(CodeModeArgumentPreflight::default());
        &mut self.spec
    }

    pub(crate) fn canonical_spec_sha256(&self) -> &str {
        self.canonical_spec_sha256.get_or_init(|| {
            let canonical_spec = self.spec.callable_contract();
            hex::encode(Sha256::digest(
                serde_json::to_vec(&canonical_spec).unwrap_or_default(),
            ))
        })
    }

    /// Capture the executable schema once, shared by validation and declarations.
    pub(crate) fn effective_input_schema(&self) -> Option<&Value> {
        self.effective_input_schema
            .get_or_init(|| {
                self.runtime
                    .code_mode_input_schema()
                    .or_else(|| self.spec.input_schema_for(&self.tool_name))
            })
            .as_ref()
    }

    pub(crate) fn effective_contract_revision(&self) -> &str {
        self.effective_contract_revision.get_or_init(|| {
            let contract = serde_json::json!({
                "version": 1,
                "identity": self.tool_name,
                "provider_authority": self.provider_authority(),
                "contract": self.spec.effective_callable_contract(
                    &self.tool_name, self.effective_input_schema(),
                ),
            });
            format!(
                "v1:{}",
                hex::encode(Sha256::digest(contract.to_string().as_bytes()))
            )
        })
    }

    pub(crate) fn search_info(&self) -> Option<codex_tools::ToolSearchInfo> {
        self.search_info_ref().cloned()
    }

    pub(crate) fn search_info_ref(&self) -> Option<&codex_tools::ToolSearchInfo> {
        self.search_info
            .get_or_init(|| self.runtime.search_info_for_registered_spec(&self.spec))
            .as_ref()
    }

    pub(crate) fn set_exposure(&mut self, exposure: ToolExposure) {
        self.exposure = exposure;
    }
}

pub struct ToolRegistry {
    tools: HashMap<ToolName, RegisteredTool>,
    model_visible_schemas: Arc<ToolSchemaArtifact>,
}

impl ToolRegistry {
    fn new(tools: HashMap<ToolName, RegisteredTool>) -> Self {
        Self {
            tools,
            model_visible_schemas: Arc::new(ToolSchemaArtifact::default()),
        }
    }


    pub(crate) fn from_unique_registered_tools(
        tools: impl IntoIterator<Item = RegisteredTool>,
    ) -> Self {
        let tools_by_name = tools
            .into_iter()
            .map(|tool| (tool.tool_name().clone(), tool))
            .collect();
        Self::new(tools_by_name)
    }


    fn tool(&self, name: &ToolName) -> Option<Arc<dyn CoreToolRuntime>> {
        self.tools.get(name).map(|tool| Arc::clone(tool.runtime()))
    }


    pub(crate) fn registered_tool(&self, name: &ToolName) -> Option<&RegisteredTool> {
        self.tools.get(name)
    }

    pub(crate) fn tool_exposure(&self, name: &ToolName) -> Option<ToolExposure> {
        self.tools.get(name).map(RegisteredTool::exposure)
    }

    pub(crate) fn tool_authorization_class(&self, name: &ToolName) -> Option<TypedToolClass> {
        self.tools
            .get(name)
            .map(RegisteredTool::authorization_class)
    }


    pub(crate) fn set_model_visible_specs(&mut self, specs: Vec<ToolSpec>) {
        self.model_visible_schemas = Arc::new(ToolSchemaArtifact::new(specs));
    }

    pub(crate) fn model_visible_specs(&self) -> Vec<ToolSpec> {
        self.model_visible_schemas.specs().to_vec()
    }

    pub(crate) fn model_visible_schemas(&self) -> Arc<ToolSchemaArtifact> {
        Arc::clone(&self.model_visible_schemas)
    }

    pub(crate) fn manifest_entries(&self) -> Vec<&RegisteredTool> {
        let mut entries = self.tools.values().collect::<Vec<_>>();
        entries.sort_by(|left, right| left.tool_name().cmp(right.tool_name()));
        entries
    }

    pub(crate) fn create_diff_consumer(
        &self,
        name: &ToolName,
    ) -> Option<Box<dyn ToolArgumentDiffConsumer>> {
        self.tool(name)?.create_diff_consumer()
    }

    pub(crate) fn post_tool_use_hook_may_run(&self, invocation: &ToolInvocation) -> bool {
        if !invocation
            .session
            .hooks()
            .has_handler_for(HookEventName::PostToolUse)
        {
            return false;
        }
        self.tools
            .get(&invocation.tool_name)
            .and_then(|tool| tool.runtime().post_tool_use_hook_name(invocation))
            .is_some_and(|name| {
                !invocation
                    .session
                    .hooks()
                    .plan_post_tool_use(name.name(), name.matcher_aliases())
                    .is_empty()
            })
    }

    /// Whether a configured PreToolUse handler can select this call. Matcher
    /// aware, so a hook scoped to one tool leaves every other tool's dispatch
    /// shortcuts enabled. Unregistered tools never reach PreToolUse.
    pub(crate) fn pre_tool_use_hook_may_run(
        &self,
        hooks: &codex_hooks::Hooks,
        name: &ToolName,
        payload: &ToolPayload,
    ) -> bool {
        if !hooks.has_handler_for(HookEventName::PreToolUse) {
            return false;
        }
        self.tools
            .get(name)
            .and_then(|tool| tool.runtime().pre_tool_use_hook_name(name, payload))
            .is_some_and(|hook_name| {
                hooks.has_pre_tool_use_handler_for(hook_name.name(), hook_name.matcher_aliases())
            })
    }

    pub(crate) fn supports_parallel_tool_calls(&self, name: &ToolName) -> Option<bool> {
        let tool = self.tools.get(name)?;
        Some(
            tool.exposure() != ToolExposure::Hidden
                && tool.runtime().supports_parallel_tool_calls(),
        )
    }

    pub(crate) fn waits_for_runtime_cancellation(&self, name: &ToolName) -> Option<bool> {
        let tool = self.tool(name)?;
        Some(tool.waits_for_runtime_cancellation())
    }

    pub(crate) fn cancellation_requires_commit_barrier(&self, name: &ToolName) -> Option<bool> {
        Some(self.tool(name)?.cancellation_requires_commit_barrier())
    }

    /// The registered runtime's classification identity; unregistered names
    /// classify as themselves.
    pub(crate) fn semantic_tool_name(&self, name: &ToolName) -> ToolName {
        self.tools
            .get(name)
            .map_or_else(|| name.clone(), |tool| tool.runtime().semantic_tool_name())
    }

    pub(crate) fn semantic_capabilities(&self, name: &ToolName) -> ToolSemanticCapabilities {
        let Some(tool) = self.tools.get(name) else {
            return ToolSemanticCapabilities {
                mutation: true,
                ..Default::default()
            };
        };
        ToolSemanticCapabilities {
            // External tool annotations are advisory, not read-only authority.
            mutation: matches!(
                tool.authorization_class(),
                TypedToolClass::StructuredEdit | TypedToolClass::DynamicExternal
            ),
            coordination: matches!(
                tool.authorization_class(),
                TypedToolClass::AgentCommunication | TypedToolClass::RootTaskControl
            ),
            command: tool.runtime().command_argument_format(),
        }
    }

    pub(crate) fn terminal_failure_reuse(&self, name: &ToolName) -> TerminalFailureReuse {
        self.tools
            .get(name)
            .map_or(TerminalFailureReuse::Never, |tool| {
                tool.runtime().terminal_failure_reuse()
            })
    }

    pub(crate) fn prepares_during_workspace_baseline(&self, name: &ToolName) -> bool {
        self.tool(name)
            .is_some_and(|tool| tool.prepares_during_workspace_baseline())
    }

    pub(crate) fn terminal_result_adapter(&self, name: &ToolName) -> Option<&'static str> {
        self.tool(name)?.terminal_result_adapter()
    }

    pub(crate) fn permits_shared_workspace_observation(
        &self,
        name: &ToolName,
        payload: &ToolPayload,
    ) -> bool {
        self.tool(name)
            .is_some_and(|tool| tool.permits_shared_workspace_observation(payload))
    }

    pub(crate) fn is_workspace_independent(&self, name: &ToolName) -> bool {
        self.tool(name)
            .is_some_and(|tool| tool.is_workspace_independent())
    }

    pub(crate) fn delegates_workspace_admission(
        &self,
        name: &ToolName,
        payload: &ToolPayload,
    ) -> bool {
        self.tool(name).is_some_and(|tool| {
            tool.delegates_workspace_admission() || tool.controls_process(payload)
                || tool.waits_for_workspace_changes(payload)
        })
    }

    pub(crate) fn native_addition_paths(
        &self,
        name: &ToolName,
        payload: &ToolPayload,
    ) -> Option<Vec<std::path::PathBuf>> {
        self.tool(name)?.native_addition_paths(payload)
    }

    pub(crate) fn cancellation_cleanup_deadline(&self, name: &ToolName) -> std::time::Duration {
        self.tool(name)
            .map(|tool| tool.cancellation_cleanup_deadline())
            .unwrap_or(crate::tools::parallel::TOOL_RUNTIME_CLEANUP_DEADLINE)
            .clamp(
                std::time::Duration::from_secs(1),
                std::time::Duration::from_secs(120),
            )
    }

    pub(crate) fn cancellation_recovery(
        &self,
        name: &ToolName,
        result: Option<&Value>,
        error: Option<&str>,
    ) -> crate::tools::context::ToolEffectRecovery {
        self.tool(name).map_or_else(
            || crate::tools::context::ToolEffectRecovery::unknown(result, error),
            |runtime| runtime.cancellation_recovery(result, error),
        )
    }

    pub(crate) fn owns_unified_exec_processes(&self, name: &ToolName) -> Option<bool> {
        let tool = self.tool(name)?;
        Some(tool.owns_unified_exec_processes())
    }


    pub(crate) async fn prepare_hook_input(
        &self,
        mut invocation: ToolInvocation,
    ) -> Result<(ToolInvocation, Option<String>), FunctionCallError> {
        if let Some(reason) = invocation.step_context.turn.post_tool_stop_reason.get() {
            return Err(FunctionCallError::RespondToModel(format!(
                "Tool call not run: a PostToolUse hook stopped this turn: {reason}"
            )));
        }
        let original_name = invocation.tool_name.clone();
        if let Some(registered) = self.tools.get(&invocation.tool_name) {
            invocation = registered.runtime().prepare_invocation(invocation).await?;
            if !self.tools.contains_key(&invocation.tool_name) {
                return Err(FunctionCallError::RespondToModel(
                    "expanded tool operation is not available in this session".into(),
                ));
            }
        }
        let expansion_notice = (invocation.tool_name != original_name).then(|| {
            format!(
                "Resolved `{original_name}` through `{}` with its normal execution policy.",
                invocation.tool_name
            )
        });
        if !self.pre_tool_use_hook_may_run(
            &invocation.session.hooks(),
            &invocation.tool_name,
            &invocation.payload,
        ) {
            return Ok((invocation, expansion_notice));
        }
        let Some(registered) = self.tools.get(&invocation.tool_name) else {
            return Ok((invocation, expansion_notice));
        };
        let tool = registered.runtime();
        let parsed = ParsedFunctionArguments::from_payload(&invocation.payload);
        let payload = with_parsed_function_arguments(parsed.clone(), async {
            tool.pre_tool_use_payload(&invocation)
        })
        .await;
        let Some(payload) = payload else {
            return Ok((invocation, expansion_notice));
        };
        let tool_cwd = tool.pre_tool_use_cwd(&invocation);
        let started = Instant::now();
        let hook_invocation = invocation.clone();
        let result = crate::tools::parallel::run_workspace_hook(
            Arc::clone(&invocation.session),
            Arc::clone(&invocation.step_context.turn),
            Arc::clone(&invocation.tracker),
            invocation.cancellation_token.clone(),
            async move {
                run_pre_tool_use_hooks(
                    &hook_invocation.session,
                    &hook_invocation.step_context.turn,
                    hook_invocation.call_id,
                    &payload.tool_name,
                    &payload.tool_input,
                    tool_cwd,
                )
                .await
            },
        )
        .await?;
        record_lifecycle_phase(&invocation, "pre_hooks", started);
        match result {
            PreToolUseHookResult::Blocked(message) => {
                Err(FunctionCallError::RespondToModel(message))
            }
            PreToolUseHookResult::Continue {
                updated_input: None,
            } => Ok((invocation, expansion_notice)),
            PreToolUseHookResult::Continue {
                updated_input: Some(updated),
            } => {
                let notice = format!(
                    "PreToolUse hook changed the input for tool `{}` (call `{}`). The tool will receive this input: {}",
                    invocation.tool_name,
                    invocation.call_id,
                    truncate_text_to_token_ceiling(&updated.to_string(), 512)
                );
                invocation = with_parsed_function_arguments(parsed, async {
                    tool.with_updated_hook_input(invocation, updated)
                })
                .await?;
                Ok((invocation, Some(notice)))
            }
        }
    }

    pub(crate) async fn dispatch_any_with_terminal_outcome(
        &self,
        invocation: ToolInvocation,
        dispatch_state: Arc<ToolDispatchState>,
    ) -> Result<AnyToolResult, FunctionCallError> {
        let mut attempt = self.capture_operation_attempt(&invocation).await;
        let mut result = self.dispatch_with_operation_attempt(
            invocation.clone(), Arc::clone(&dispatch_state), &mut attempt,
        ).await;
        // This is a reused recovery receipt, not a new execution/failure. Do
        // not overwrite the unresolved owner attempt with the preflight refusal.
        if attempt.recovery_deferred {
            return result;
        }
        let owner_failure = match &result {
            Ok(output) => output.result.tool_failure(),
            Err(error) => Some(error.failure()),
        };
        if dispatch_state.cancelled_nested_attempt(invocation.source.cancellation_cause()).is_some() {
            use codex_protocol::tool_failure::{ToolEffectsState, ToolExecutionState};
            let known_settlement = result.as_ref().is_ok_and(AnyToolResult::success_for_logging)
                || owner_failure.as_ref().is_some_and(|failure| {
                    failure.execution_state == ToolExecutionState::Completed
                        && failure.effects != ToolEffectsState::Unknown
                });
            if !known_settlement {
                // The cancellation owner joins cleanup and records the exact
                // timeout before admitting another exclusive dispatch.
                return result;
            }
        }
        crate::tools::operation_history::settle(&mut attempt, &result);
        let advisory = invocation.session.record_operation_attempt(attempt).await;
        match &mut result {
            Ok(output) => {
                if output.outcome_for_logging() != ToolOutputOutcome::Success
                    || advisory["previous_unsuccessful_attempt_count"].as_u64().is_some_and(|count| count > 0) {
                    output.operation_history = Some(advisory.clone());
                }
            }
            Err(FunctionCallError::Structured(failure)) => {
                crate::tools::operation_history::annotate_failure(failure, advisory.clone());
            }
            Err(error @ FunctionCallError::RespondToModel(_)) => {
                let mut failure = error.failure();
                crate::tools::operation_history::annotate_failure(&mut failure, advisory.clone());
                *error = FunctionCallError::structured(failure);
            }
            // Preserve terminal/refusal variants and their existing control flow.
            Err(_) => {}
        }
        dispatch_state.record_settlement(advisory, owner_failure, result.is_err());
        result
    }

    async fn capture_operation_attempt(
        &self, invocation: &ToolInvocation,
    ) -> crate::tools::operation_history::OperationAttempt {
        let registered = self.tools.get(&invocation.tool_name);
        crate::tools::operation_history::capture(
            invocation, registered.map(RegisteredTool::effective_contract_revision),
            registered.and_then(RegisteredTool::provider_authority),
        ).await
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "tool dispatch must keep active-turn accounting atomic"
    )]
    async fn dispatch_with_operation_attempt(
        &self,
        mut invocation: ToolInvocation,
        dispatch_state: Arc<ToolDispatchState>,
        attempt: &mut crate::tools::operation_history::OperationAttempt,
    ) -> Result<AnyToolResult, FunctionCallError> {
        // Direct registry callers do not pass through ToolCallRuntime's prepare
        // phase. Resolve their executable before selecting its handler/schema.
        let original_name = invocation.tool_name.clone();
        if PREPARED_HOOK_INPUT.try_with(|_| ()).is_err()
            && let Some(registered) = self.tools.get(&invocation.tool_name)
        {
            invocation = registered.runtime().prepare_invocation(invocation).await?;
            *attempt = self.capture_operation_attempt(&invocation).await;
        }
        let expansion_notice = (original_name != invocation.tool_name).then(|| {
            format!(
                "Resolved `{original_name}` through `{}` with its normal execution policy.",
                invocation.tool_name
            )
        });
        let tool_name = invocation.tool_name.clone();
        let mut parsed_function_arguments =
            ParsedFunctionArguments::from_payload(&invocation.payload);
        let mut rewritten_source_dependencies = None;
        let tool_name_flat = flat_tool_name(&tool_name);
        let call_id_owned = invocation.call_id.clone();
        let otel = invocation.step_context.turn.session_telemetry.clone();
        let base_tool_result_tags = [
            (
                "sandbox",
                permission_profile_sandbox_tag(
                    &invocation.step_context.turn.permission_profile,
                    invocation.step_context.turn.windows_sandbox_level,
                    false,
                ),
            ),
            (
                "sandbox_policy",
                permission_profile_policy_tag(
                    &invocation.step_context.turn.permission_profile,
                    invocation.step_context.turn.cwd().as_path(),
                ),
            ),
        ];

        {
            let mut active = invocation.session.active_turn.lock().await;
            if let Some(active_turn) = active.as_mut() {
                let mut turn_state = active_turn.turn_state.lock().await;
                turn_state.tool_calls = turn_state.tool_calls.saturating_add(1);
            }
        }
        let (tool, tool_spec, input_schema, code_mode_argument_preflight, external_tool) =
            match self.tools.get(&tool_name) {
                Some(registered) => (
                    Arc::clone(registered.runtime()),
                    registered.spec(),
                    registered.effective_input_schema(),
                    Arc::clone(&registered.code_mode_argument_preflight),
                    registered.authorization_class() == TypedToolClass::DynamicExternal,
                ),
                None => {
                    let message = unsupported_tool_call_message(&invocation.payload, &tool_name, &self.tools);
                    let log_payload = invocation.payload.log_payload();
                    otel.tool_result_with_tags(
                        tool_name_flat.as_ref(),
                        &call_id_owned,
                        log_payload.as_ref(),
                        Duration::ZERO,
                        /*success*/ false,
                        &message,
                        &base_tool_result_tags,
                        /*extra_trace_fields*/ &[],
                    );
                    let err = FunctionCallError::structured(
                        codex_protocol::tool_failure::ToolFailure::no_effects(
                            codex_protocol::tool_failure::ToolFailureCode::InvalidInput,
                            message,
                        ),
                    );
                    dispatch_state.try_complete();
                    return Err(err);
                }
            };

        let telemetry_tags = tool.telemetry_tags(&invocation).await;
        let mut tool_result_tags =
            Vec::with_capacity(base_tool_result_tags.len() + telemetry_tags.len());
        let mut extra_trace_fields = Vec::new();
        tool_result_tags.extend_from_slice(&base_tool_result_tags);
        for (key, value) in &telemetry_tags {
            if matches!(*key, "mcp_server" | "mcp_server_origin") {
                extra_trace_fields.push((*key, value.as_str()));
            } else {
                tool_result_tags.push((*key, value.as_str()));
            }
        }
        if !tool.matches_kind(&invocation.payload) {
            let expected = match tool_spec {
                ToolSpec::Freeform(_) => {
                    "raw text in a custom/freeform tool call, not JSON function arguments"
                }
                ToolSpec::Function(_) | ToolSpec::ToolSearch { .. } => {
                    "JSON arguments in a function tool call, not raw custom/freeform text"
                }
                _ => "the call format declared in its advertised tool schema",
            };
            let message = format!(
                "tool {tool_name} invoked with incompatible payload; expected {expected}. The tool was not run; retry with the declared format."
            );
            let log_payload = invocation.payload.log_payload();
            otel.tool_result_with_tags(
                tool_name_flat.as_ref(),
                &call_id_owned,
                log_payload.as_ref(),
                Duration::ZERO,
                /*success*/ false,
                &message,
                &tool_result_tags,
                &extra_trace_fields,
            );
            let err = FunctionCallError::structured(
                codex_protocol::tool_failure::ToolFailure::no_effects(
                    codex_protocol::tool_failure::ToolFailureCode::InvalidInput,
                    message,
                ),
            );
            dispatch_state.try_complete();
            return Err(err);
        }

        let validate_arguments =
            external_tool || matches!(invocation.source, ToolCallSource::CodeMode { .. });
        if validate_arguments
            && let Err(mut failure) = code_mode_argument_preflight.validate(
                &tool_name,
                input_schema,
                &invocation.payload,
                parsed_function_arguments.as_ref(),
                !external_tool,
            )
        {
            // A sub-agent's misaddressed progress report gets the ancestor
            // policy rather than a schema error it would retry around.
            let refusal = (!external_tool
                && matches!(tool_name.name.as_str(), "send_message" | "followup_task"))
            .then(|| parsed_function_arguments.as_ref()?.value().ok())
            .flatten()
            .and_then(|arguments| {
                crate::tools::handlers::multi_agents_v2::ancestor_refusal(
                    &invocation.step_context.turn,
                    arguments,
                )
            });
            if let Some(message) = refusal {
                failure = codex_protocol::tool_failure::ToolFailure::no_effects(
                    codex_protocol::tool_failure::ToolFailureCode::PermissionDenied, message,
                );
            }
            let message = failure.to_string();
            let log_payload = invocation.payload.log_payload();
            otel.tool_result_with_tags(
                tool_name_flat.as_ref(),
                &call_id_owned,
                log_payload.as_ref(),
                Duration::ZERO,
                /*success*/ false,
                &message,
                &tool_result_tags,
                &extra_trace_fields,
            );
            let err = FunctionCallError::structured(failure);
            dispatch_state.try_complete();
            return Err(err);
        }

        let phase_started = Instant::now();
        let start_result = notify_tool_start(&invocation).await;
        record_lifecycle_phase(&invocation, "notify_start", phase_started);
        if let Err(error) = start_result {
            notify_tool_finish_if_unclaimed(
                &invocation,
                dispatch_state.as_ref(),
                &error,
                ToolCallOutcome::Failed {
                    handler_executed: false,
                },
            )
            .await?;
            return Err(error);
        }

        let prepared = PREPARED_HOOK_INPUT.try_with(Clone::clone).ok();
        let (prepared_invocation, hook_input_notice) = match prepared {
            Some(notice) => (invocation, notice),
            None => match self.prepare_hook_input(invocation.clone()).await {
                Ok(prepared) => prepared,
                Err(err) => {
                    notify_tool_finish_if_unclaimed(
                        &invocation,
                        dispatch_state.as_ref(),
                        &err,
                        ToolCallOutcome::Blocked,
                    )
                    .await?;
                    return Err(err);
                }
            },
        };
        invocation = prepared_invocation;
        let hook_input_notice = hook_input_notice.or(expansion_notice);
        let hook_rewrote_input = hook_input_notice.is_some();
        if hook_rewrote_input {
            *attempt = self.capture_operation_attempt(&invocation).await;
        }
        if hook_rewrote_input {
            parsed_function_arguments = ParsedFunctionArguments::from_payload(&invocation.payload);
            let name = flat_tool_name(&invocation.tool_name);
            if crate::tool_history::tool_observes_workspace(name.as_ref()) {
                rewritten_source_dependencies = Some(
                    if let Some(native_cwd) = crate::tools::parallel::native_workspace_analysis_cwd(
                        &invocation.step_context,
                        &invocation.payload,
                    ) {
                        let name = name.into_owned();
                        let payload = invocation.payload.clone();
                        let parsed_arguments = parsed_function_arguments.clone();
                        match codex_command_analysis::run_blocking_command_analysis(move || {
                            crate::tool_history::source_dependencies_for_tool_call_with_parsed_arguments(
                                &name,
                                &payload,
                                parsed_arguments.as_ref().and_then(|parsed| parsed.value().ok()),
                                native_cwd.as_path(),
                                true,
                            )
                        })
                        .await
                        {
                            Ok(dependencies) => dependencies,
                            Err(error) => {
                                tracing::warn!(%error, "hook-rewritten workspace dependencies are unscoped");
                                Default::default()
                            }
                        }
                    } else {
                        // No host-native read authority: keep an authoritative,
                        // unscoped override rather than reusing the old payload.
                        Default::default()
                    },
                );
            }
        }
        // Recorded after PreToolUse hooks so a hook sees only earlier dispatches.
        invocation
            .step_context
            .turn
            .record_dispatched_tool_name(tool_name_flat.as_ref());

        // PreToolUse hooks may replace the arguments after the initial
        // admission check. Validate the final payload that will reach the
        // handler so a hook cannot turn a schema-valid call into an invalid one.
        if hook_rewrote_input
            && validate_arguments
            && let Err(failure) = code_mode_argument_preflight.validate(
                &tool_name,
                input_schema,
                &invocation.payload,
                parsed_function_arguments.as_ref(),
                !external_tool,
            )
        {
            let message = failure.to_string();
            let log_payload = invocation.payload.log_payload();
            otel.tool_result_with_tags(
                tool_name_flat.as_ref(),
                &call_id_owned,
                log_payload.as_ref(),
                Duration::ZERO,
                /*success*/ false,
                &message,
                &tool_result_tags,
                &extra_trace_fields,
            );
            let err = FunctionCallError::structured(failure);
            notify_tool_finish_if_unclaimed(
                &invocation,
                dispatch_state.as_ref(),
                &err,
                ToolCallOutcome::Failed {
                    handler_executed: false,
                },
            )
            .await?;
            return Err(err);
        }

        if let Some(notice) = hook_input_notice
            && let Err(error) = record_additional_contexts(
                &invocation.session,
                &invocation.step_context.turn,
                vec![notice],
            )
            .await
        {
            let error =
                FunctionCallError::Fatal(format!("Could not record hook input context: {error}"));
            // Start was already notified. Close the lifecycle like every other
            // exit before the handler; the failure itself stays fatal.
            let _ = notify_tool_finish_if_unclaimed(
                &invocation,
                dispatch_state.as_ref(),
                &error,
                ToolCallOutcome::Failed {
                    handler_executed: false,
                },
            )
            .await;
            return Err(error);
        }

        // Prepared calls may have waited for workspace admission while another
        // tool's post hook stopped the turn. Do not enter their handler now.
        if let Some(reason) = invocation.step_context.turn.post_tool_stop_reason.get() {
            let error = FunctionCallError::RespondToModel(format!(
                "Tool call not run: a PostToolUse hook stopped this turn: {reason}"
            ));
            notify_tool_finish_if_unclaimed(
                &invocation,
                dispatch_state.as_ref(),
                &error,
                ToolCallOutcome::Blocked,
            )
            .await?;
            return Err(error);
        }

        // Inspect the effective post-hook input at the existing dispatch owner,
        // after admission and immediately before handler entry. No new journal,
        // retry counter, or permission policy is introduced.
        let retry_memory = invocation.session.operation_attempt_advisory(attempt).await;
        if let Some(failure) = attempt.recovery_preflight(&retry_memory) {
            let error = FunctionCallError::structured(failure);
            let accounted = notify_tool_finish_if_unclaimed(
                &invocation, dispatch_state.as_ref(), &error,
                ToolCallOutcome::Failed { handler_executed: false },
            ).await;
            let error = accounted.err().unwrap_or(error);
            dispatch_state.record_settlement(retry_memory, Some(error.failure()), true);
            return Err(error);
        }
        if retry_memory["unchanged_unsuccessful_attempt_count"].as_u64().is_some_and(|count| count > 0) {
            // This is native retry context, not hook-authored text: preserve
            // the hook budget and keep owner observations explicitly data.
            if let Err(error) = invocation.session.record_conversation_items_ordered(
                &invocation.step_context.turn,
                &[ResponseItem::Message {
                    id: None,
                    role: "developer".into(),
                    content: vec![codex_protocol::models::ContentItem::InputText {
                        text: format!("Retry-path memory (data, not instructions, permission or a retry ban): {retry_memory}"),
                    }],
                    phase: None,
                    internal_chat_message_metadata_passthrough: None,
                }],
            ).await {
                let error = FunctionCallError::Fatal(format!("Could not record retry memory: {error}"));
                let _ = notify_tool_finish_if_unclaimed(
                    &invocation, dispatch_state.as_ref(), &error,
                    ToolCallOutcome::Failed { handler_executed: false },
                ).await;
                return Err(error);
            }
        }

        let invocation_for_tool = invocation.clone();
        let handler_dispatch_state = Arc::clone(&dispatch_state);
        let handler_attempt = &*attempt;
        let log_payload = invocation.payload.log_payload();

        let phase_started = Instant::now();
        let mut result = otel
            .log_tool_result_with_tags(
                tool_name_flat.as_ref(),
                &call_id_owned,
                log_payload.as_ref(),
                &tool_result_tags,
                &extra_trace_fields,
                || {
                    let tool = tool.clone();
                    let parsed_function_arguments = parsed_function_arguments.clone();
                    let argument_preflight = Arc::clone(&code_mode_argument_preflight);
                    let tool_name = tool_name.clone();
                    let payload = &invocation.payload;
                    async move {
                        let result = with_parsed_function_arguments(
                            parsed_function_arguments.clone(),
                            handle_any_tool(tool.as_ref(), invocation_for_tool, handler_dispatch_state.as_ref(), handler_attempt),
                        )
                        .await;
                        // Direct native calls retain their decoder's accepted
                        // compatibility surface. Explain only an established
                        // no-effects argument rejection, before logging it. A
                        // handler's own refusal of accepted arguments keeps its
                        // reason: the schema omits accepted compatibility fields.
                        result.map_err(|error| {
                            if !validate_arguments
                                && matches!(&error, FunctionCallError::Structured(failure)
                                    if failure.code == codex_protocol::tool_failure::ToolFailureCode::InvalidInput
                                    && failure.effects == codex_protocol::tool_failure::ToolEffectsState::None
                                    && !failure.is_handler_refusal())
                                && let Err(failure) = argument_preflight.validate(
                                    &tool_name, input_schema, payload,
                                    parsed_function_arguments.as_ref(), true,
                                )
                            {
                                return FunctionCallError::structured(failure);
                            }
                            error
                        })
                    }
                },
                AnyToolResult::success_for_logging,
                |result| {
                    result.result.log_output_stats()
                        .map(codex_otel::ToolResultOutputStats::from)
                        .unwrap_or_else(|| result.result.log_preview().into())
                },
            )
            .await;
        record_lifecycle_phase(&invocation, "handler", phase_started);
        let success = match &result {
            Ok(result) => result.success_for_logging(),
            Err(_) => false,
        };
        let post_tool_use_plan = success
            .then(|| tool.post_tool_use_hook_name(&invocation))
            .flatten()
            .and_then(|tool_name| {
                let plan = invocation
                    .session
                    .hooks()
                    .plan_post_tool_use(tool_name.name(), tool_name.matcher_aliases());
                (!plan.is_empty()).then_some((tool_name, plan))
            });
        let post_tool_use_payload = if let Some((tool_name, plan)) = post_tool_use_plan {
            let payload =
                with_parsed_function_arguments(parsed_function_arguments.clone(), async {
                    result.as_mut().ok().and_then(|result| {
                        CoreToolRuntime::post_tool_use_payload(
                            tool.as_ref(),
                            &invocation,
                            result.result.as_ref(),
                        )
                    })
                })
                .await;
            // The output owns the originating command cwd, so a result collected
            // after the turn cwd changed still reports where the command ran.
            // Synchronous tools without an output-owned cwd resolve it from this
            // same invocation, as PreToolUse did.
            let tool_cwd = result
                .as_ref()
                .ok()
                .and_then(|result| result.result.post_tool_use_cwd())
                .or_else(|| tool.pre_tool_use_cwd(&invocation));
            payload.map(|payload| {
                debug_assert_eq!(payload.tool_name, tool_name);
                (plan, payload, tool_cwd)
            })
        } else {
            None
        };
        let (post_tool_use_outcome, post_tool_workspace_mutated) =
            if let Some((plan, post_tool_use_payload, tool_cwd)) = post_tool_use_payload {
                let phase_started = Instant::now();
                let hook_invocation = invocation.clone();
                let (outcome, workspace_mutated) = crate::tools::parallel::run_workspace_hook_with_receipt(
                    Arc::clone(&invocation.session),
                    Arc::clone(&invocation.step_context.turn),
                    Arc::clone(&invocation.tracker),
                    invocation.cancellation_token.clone(),
                    async move {
                        run_post_tool_use_hooks(
                            &hook_invocation.session,
                            &hook_invocation.step_context.turn,
                            plan,
                            post_tool_use_payload.tool_use_id,
                            post_tool_use_payload.tool_name,
                            post_tool_use_payload.tool_input,
                            post_tool_use_payload.tool_response,
                            tool_cwd,
                        )
                        .await
                    },
                )
                .await
                .map(|(outcome, receipt)| (outcome, receipt.is_some()))
                .unwrap_or_else(|error| (PostToolUseOutcome {
                    hook_events: Vec::new(),
                    should_block: true,
                    stop_reason: None,
                    additional_contexts: Vec::new(),
                    feedback_message: Some(format!("PostToolUse could not finish: {error}")),
                }, true));
                record_lifecycle_phase(&invocation, "post_hooks", phase_started);
                (Some(outcome), workspace_mutated)
            } else {
                (None, false)
            };
        let post_hook_ran = post_tool_use_outcome.is_some();
        if let (Ok(result), Some(outcome)) = (&mut result, post_tool_use_outcome) {
            let phase_started = Instant::now();
            let additional_contexts = apply_post_tool_use_outcome(result, outcome);
            if matches!(&invocation.source, ToolCallSource::Direct) {
                let contexts = prepare_additional_context_items(
                    &invocation.session,
                    &invocation.step_context.turn,
                    additional_contexts,
                )
                .await;
                invocation
                    .step_context
                    .turn
                    .queue_post_tool_contexts(&invocation.call_id, contexts)
                    .await;
            } else {
                if let Err(error) = record_additional_contexts(
                    &invocation.session,
                    &invocation.step_context.turn,
                    additional_contexts,
                )
                .await
                {
                    let message = format!(
                        "Tool completed; post-tool context publication failed: {error}. Stop continuation. Its effects remain applied; do not repeat the tool. Durable context recovery is unavailable."
                    );
                    result
                        .code_mode_feedback
                        .push(FunctionCallOutputContentItem::InputText {
                            text: message.clone(),
                        });
                    result.result = Box::new(PostToolUseFeedbackOutput {
                        original: std::mem::replace(
                            &mut result.result,
                            Box::new(FunctionToolOutput::from_text(String::new(), None)),
                        ),
                        model_visible: FunctionToolOutput::from_text(message.clone(), Some(true)),
                        context_error: Some(message),
                        accounting_error: None,
                    });
                }
            }
            record_lifecycle_phase(&invocation, "additional_context", phase_started);
        }

        // A cooperative runtime can finish teardown after cancellation has
        // already claimed the terminal outcome. Do not continue into
        // projection, persistence, or finish notification for that result.
        if dispatch_state.is_aborted() {
            // Cancellation owns lifecycle publication, not the handler's value.
            // Return its typed result directly to the cleanup supervisor.
            return result;
        }

        // A PostToolUse block rejects the result, not the already-completed tool execution.
        let lifecycle_outcome = match &result {
            Ok(result) => match result.outcome_for_logging() {
                ToolOutputOutcome::Skipped => ToolCallOutcome::Skipped,
                ToolOutputOutcome::Success | ToolOutputOutcome::Yielded => {
                    ToolCallOutcome::Completed { success: true }
                }
                ToolOutputOutcome::Failure | ToolOutputOutcome::TimedOut => {
                    ToolCallOutcome::Completed { success: false }
                }
            },
            Err(_) => ToolCallOutcome::Failed {
                handler_executed: true,
            },
        };
        match result {
            Ok(mut result) => {
                result.workspace_source_invalidated |= post_tool_workspace_mutated;
                let force_inline_carrier = result
                    .deterministic_continuation_owner_key()
                    .is_some_and(|owner_key| {
                        !invocation
                            .session
                            .services
                            .code_mode_service
                            .owner_drained_continuation_snapshot(&owner_key)
                            .is_empty()
                    });
                let canonical_artifact_required = result.result.requires_canonical_artifact();
                let provider_visible = projection_is_provider_visible(&invocation.source);
                let projection_admission_required = projection_admission_required(
                    &invocation.source,
                    &invocation.tool_name,
                    force_inline_carrier,
                );
                let admission_tracking_enabled = admission_tracking_enabled(
                    &invocation.source,
                    invocation
                        .step_context
                        .turn
                        .config
                        .completed_tool_history_projection,
                );
                let phase_started = Instant::now();
                let projection_input = prepare_model_projection(
                    &invocation,
                    &mut result,
                    parsed_function_arguments.as_ref(),
                    rewritten_source_dependencies.as_ref(),
                    force_inline_carrier,
                    projection_admission_required,
                )
                .await;
                let admission_tracking_required =
                    projection_admission_required && projection_input.is_some();
                let mut model_projection = match projection_input {
                    Some(input) => project_model_output(input).await,
                    None => None,
                };
                // The output predates PostToolUse. A hook's unknown write has
                // already invalidated registered reads; do not register this
                // delayed candidate as fresh after that invalidation.
                if post_tool_workspace_mutated
                    && let Some(candidate) = model_projection.as_mut()
                        .and_then(|projection| projection.candidate.as_mut())
                    && (!candidate.source_dependencies.is_empty()
                        || crate::tool_history::tool_observes_workspace(&candidate.tool_identity))
                {
                    candidate.source_dependencies_current = false;
                }
                record_lifecycle_phase(&invocation, "projection", phase_started);
                if (canonical_artifact_required || admission_tracking_required)
                    && model_projection.is_none()
                {
                    tracing::error!(
                        tool_name = %flat_tool_name(&invocation.tool_name),
                        call_id = %invocation.call_id,
                        "tool execution completed but its canonical result could not be preserved"
                    );
                    let model_visible = FunctionToolOutput::from_text(
                        "Tool execution completed, but its full result could not be preserved for model delivery."
                            .to_string(),
                        Some(result.success_for_logging()),
                    );
                    result.result = Box::new(UnavailableModelProjectionOutput {
                        original: result.result,
                        model_visible,
                    });
                }
                if let Some(projection) = &model_projection {
                    invocation
                        .step_context
                        .turn
                        .turn_timing_state
                        .record_tool_output_projection_facts_async(
                            projection.canonical_bytes,
                            projection.canonical_tokens,
                            projection.model_bytes,
                            projection.projected_tokens,
                            projection.artifact_created,
                            projection.artifact_reused,
                            projection.projection_truncated,
                            projection.omitted_sections,
                            provider_visible,
                        )
                        .await;
                    if admission_tracking_enabled && let Some(candidate) = &projection.candidate {
                        let phase_started = Instant::now();
                        invocation
                            .session
                            .register_tool_history_candidate(candidate.clone())
                            .await;
                        record_history_persistence(phase_started.elapsed());
                    }
                }
                result.install_model_projection(model_projection, rewritten_source_dependencies);
                if provider_visible && !hook_rewrote_input && !post_hook_ran
                    && let Some(signal) = result.result.sampling_request_signal()
                {
                    result.provider_read_offer_eligible = signal["provider_exact_read_offer_eligible"] == true
                        || signal["provider_terminal_read_eligible"] == true;
                }
                // Cancellation may win while projection or its bookkeeping is
                // pending. Its owner publishes the aborted terminal outcome.
                if dispatch_state.is_aborted() {
                    return Ok(result);
                }
                if let Err(error) = crate::tools::lifecycle::notify_required_tool_finish(
                    &invocation,
                    lifecycle_outcome,
                )
                .await
                {
                    if dispatch_state.try_accounting_unconfirmed() {
                        let message = error.to_string();
                        result
                            .code_mode_feedback
                            .push(FunctionCallOutputContentItem::InputText {
                                text: message.clone(),
                            });
                        result.result = Box::new(PostToolUseFeedbackOutput {
                            original: result.result,
                            model_visible: FunctionToolOutput::from_text(
                                message.clone(),
                                Some(false),
                            ),
                            context_error: None,
                            accounting_error: Some(message),
                        });
                        notify_tool_finish(
                            &invocation,
                            ToolCallOutcome::Failed {
                                handler_executed: true,
                            },
                        )
                        .await;
                    }
                    // Cancellation may have elected its own terminal state;
                    // either way, keep the handler's typed result in custody.
                    return Ok(result);
                }
                if dispatch_state.try_complete() {
                    // The projected result and required commits are settled. A
                    // delayed observer must not turn these effects into an abort.
                    let phase_started = Instant::now();
                    notify_tool_finish(&invocation, lifecycle_outcome).await;
                    record_lifecycle_phase(&invocation, "notify_finish", phase_started);
                    Ok(result)
                } else {
                    Ok(result)
                }
            }
            Err(err) => {
                if let Err(accounting) = crate::tools::lifecycle::notify_required_tool_finish(
                    &invocation,
                    lifecycle_outcome,
                )
                .await
                {
                    if dispatch_state.try_accounting_unconfirmed() {
                        let error =
                            FunctionCallError::RespondToModel(format!("{err}\n\n{accounting}"));
                        notify_tool_finish(&invocation, lifecycle_outcome).await;
                        return Err(error);
                    }
                    return Err(err);
                }
                if dispatch_state.try_complete() {
                        let phase_started = Instant::now();
                    notify_tool_finish(&invocation, lifecycle_outcome).await;
                    record_lifecycle_phase(&invocation, "notify_finish", phase_started);
                    Err(err)
                } else if dispatch_state.is_aborted() {
                    // The cancellation owner retains every handler's recovery information
                    // and owns the single aborted lifecycle notification.
                    Err(err)
                } else {
                    Err(FunctionCallError::RespondToModel(
                        "tool cancelled after runtime cleanup".to_string(),
                    ))
                }
            }
        }
    }
}

fn projection_is_provider_visible(source: &ToolCallSource) -> bool {
    matches!(source, ToolCallSource::Direct)
}

fn with_operation_history_notice(
    mut response: ResponseInputItem,
    notice: Option<&str>,
) -> ResponseInputItem {
    // Preserve an existing JSON object as one parseable object, not JSON
    // followed by prose. Scalar/programmatic result shapes remain unchanged.
    if let (Some(notice), ResponseInputItem::FunctionCallOutput { output, .. }
        | ResponseInputItem::CustomToolCallOutput { output, .. }) = (notice, &mut response)
        && let FunctionCallOutputBody::Text(text) = &mut output.body
        && let Ok(mut value) = serde_json::from_str::<Value>(text)
        && let Some(fields) = value.as_object_mut()
        && let Ok(history) = serde_json::from_str::<Value>(notice)
    {
        fields.entry("operation_history").or_insert_with(|| history["operation_history"].clone());
        *text = value.to_string();
        return response;
    }
    // Appended notices are display. A first attempt without a fingerprint has
    // neither an earlier outcome nor a failure signature for this to carry.
    if notice
        .and_then(|notice| serde_json::from_str::<Value>(notice).ok())
        .is_some_and(|notice| {
            let history = &notice["operation_history"];
            history["previous_attempt_count"] == 0
                && history["last_unsuccessful_attempt"].is_null()
                && history["recovery_required"] != true
                && history["instance"]["fingerprint"].is_null()
        })
    {
        return response;
    }
    with_model_delivery_notice(response, notice)
}

fn with_model_delivery_notice(
    mut response: ResponseInputItem,
    notice: Option<&str>,
) -> ResponseInputItem {
    let Some(notice) = notice else {
        return response;
    };
    match &mut response {
        ResponseInputItem::FunctionCallOutput { output, .. }
        | ResponseInputItem::CustomToolCallOutput { output, .. } => match &mut output.body {
            FunctionCallOutputBody::Text(text) => {
                text.push_str("\n\nHost settlement: ");
                text.push_str(notice);
            }
            FunctionCallOutputBody::ContentItems(items) => {
                items.push(FunctionCallOutputContentItem::InputText {
                    text: notice.to_string(),
                })
            }
        },
        ResponseInputItem::McpToolCallOutput { output, .. } => output
            .content
            .push(serde_json::json!({"type":"text", "text":notice})),
        _ => {}
    }
    response
}

fn admission_tracking_enabled(source: &ToolCallSource, configured: bool) -> bool {
    configured && projection_is_provider_visible(source)
}

fn projection_admission_required(
    source: &ToolCallSource,
    tool_name: &ToolName,
    force_inline_carrier: bool,
) -> bool {
    projection_is_provider_visible(source)
        && (crate::tools::code_mode::is_orchestration_tool_name(tool_name)
            || !generic_projection_is_exempt(tool_name, force_inline_carrier))
}

async fn notify_tool_finish_if_unclaimed(
    invocation: &ToolInvocation,
    dispatch_state: &ToolDispatchState,
    error: &FunctionCallError,
    outcome: ToolCallOutcome,
) -> Result<bool, FunctionCallError> {
    if let Err(accounting) =
        crate::tools::lifecycle::notify_required_tool_finish(invocation, outcome).await
    {
        if dispatch_state.try_accounting_unconfirmed() {
            let error = FunctionCallError::RespondToModel(format!("{error}\n\n{accounting}"));
            notify_tool_finish(
                invocation,
                ToolCallOutcome::Failed {
                    handler_executed: dispatch_state.handler_started(),
                },
            )
            .await;
            return Err(error);
        }
        return Ok(false);
    }
    // Claim terminal ownership before an observer can yield to cancellation.
    if !dispatch_state.try_complete() {
        return Ok(false);
    }
    notify_tool_finish(invocation, outcome).await;
    Ok(true)
}

async fn handle_any_tool(
    tool: &dyn CoreToolRuntime,
    invocation: ToolInvocation,
    dispatch_state: &ToolDispatchState,
    attempt: &crate::tools::operation_history::OperationAttempt,
) -> Result<AnyToolResult, FunctionCallError> {
    if !tool.prepares_during_workspace_baseline() {
        crate::tools::parallel::wait_for_workspace_baseline().await;
    }
    if !dispatch_state.try_start_handler(attempt) {
        return Err(FunctionCallError::RespondToModel(
            "tool cancelled before handler entry".into(),
        ));
    }
    let _tool_execution_timing_guard =
        matches!(tool.tool_execution_timing(), ToolExecutionTiming::Handler).then(|| {
            invocation
                .step_context
                .turn
                .turn_timing_state
                .begin_tool_execution()
        });
    invocation
        .step_context
        .turn
        .turn_timing_state
        .record_tool_handler_entry(invocation.tool_name.name.as_str());
    mark_tool_handler_entry();
    // Keep the reservation through the first poll: constructing an async
    // handler future alone does not start its body.
    let mut handler = Box::pin(tool.handle(invocation.clone()));
    let first_poll = std::future::poll_fn(|cx| {
        let result = std::future::Future::poll(handler.as_mut(), cx);
        crate::elicitation::settle_pending_dispatch();
        std::task::Poll::Ready(result)
    }).await;
    let output = match first_poll {
        std::task::Poll::Ready(output) => output,
        std::task::Poll::Pending => handler.await,
    };
    // Early argument failures must also settle capture before post-tool hooks.
    crate::tools::parallel::wait_for_workspace_baseline().await;
    mark_tool_handler_exit();
    let output = output?;
    let result = AnyToolResult {
        call_id: invocation.call_id,
        payload: invocation.payload,
        result: output,
        model_projection: None,
        source_dependencies: None,
        code_mode_feedback: Vec::new(),
        operation_history: None,
        workspace_source_invalidated: false,
        provider_read_offer_eligible: false,
    };
    Ok(result)
}

pub(crate) async fn with_prepared_hook_input<F: std::future::Future>(
    notice: Option<String>,
    future: F,
) -> F::Output {
    PREPARED_HOOK_INPUT.scope(notice, future).await
}

tokio::task_local! {
    static PREPARED_HOOK_INPUT: Option<String>;
    static PRECOMPUTED_PROJECTION_SOURCE_DEPENDENCIES:
        std::collections::BTreeSet<crate::tool_history::SourceDependencyV1>;
}

pub(crate) async fn with_precomputed_projection_source_dependencies<F>(
    source_dependencies: Option<
        std::collections::BTreeSet<crate::tool_history::SourceDependencyV1>,
    >,
    future: F,
) -> F::Output
where
    F: std::future::Future,
{
    match source_dependencies {
        Some(source_dependencies) => {
            PRECOMPUTED_PROJECTION_SOURCE_DEPENDENCIES
                .scope(source_dependencies, future)
                .await
        }
        None => future.await,
    }
}

fn precomputed_projection_source_dependencies()
-> Option<std::collections::BTreeSet<crate::tool_history::SourceDependencyV1>> {
    PRECOMPUTED_PROJECTION_SOURCE_DEPENDENCIES
        .try_with(Clone::clone)
        .ok()
}

pub(crate) fn authorized_tool_invocation_sha256(
    turn: &TurnContext,
    payload: &ToolPayload,
    parsed_function_arguments: Option<&ParsedFunctionArguments>,
) -> Option<String> {
    let invocation_hash = canonical_tool_invocation_sha256(payload, parsed_function_arguments)?;
    serde_json::to_vec(&(
        invocation_hash,
        turn.cwd(),
        turn.permission_profile(),
        turn.approval_policy.get(),
    ))
    .ok()
    .map(|bytes| crate::tool_history::sha256(&bytes))
}

pub(crate) fn canonical_tool_invocation_sha256(
    payload: &ToolPayload,
    parsed_function_arguments: Option<&ParsedFunctionArguments>,
) -> Option<String> {
    let value = canonical_tool_invocation_value(payload, parsed_function_arguments)?;
    Some(crate::tool_history::sha256(&serde_json::to_vec(&value).ok()?))
}

pub(crate) fn canonical_tool_invocation_value(
    payload: &ToolPayload,
    parsed_function_arguments: Option<&ParsedFunctionArguments>,
) -> Option<Value> {
    let value = match payload {
        ToolPayload::Function { arguments } => serde_json::json!({
            "kind": "function",
            "arguments": match parsed_function_arguments {
                Some(parsed) => parsed
                    .value()
                    .map(canonicalize_json)
                    .unwrap_or_else(|_| Value::String(arguments.clone())),
                None => canonical_json_argument(arguments),
            },
        }),
        ToolPayload::ToolSearch { arguments } => serde_json::json!({
            "kind": "tool_search",
            "arguments": canonicalize_json(&serde_json::to_value(arguments).ok()?),
        }),
        ToolPayload::Custom { input } => serde_json::json!({
            "kind": "custom",
            // Freeform grammars own their text semantics. JSON-shaped text is
            // not permission to equate distinct raw invocations for replay or
            // failure resolution.
            "input": input,
        }),
    };
    Some(canonicalize_json(&value))
}

fn canonical_json_argument(value: &str) -> Value {
    serde_json::from_str(value)
        .map(|value| canonicalize_json(&value))
        .unwrap_or_else(|_| Value::String(value.to_string()))
}

fn function_hook_tool_name(invocation: &ToolInvocation) -> HookToolName {
    function_hook_tool_name_for(&invocation.tool_name)
}

fn function_hook_tool_name_for(tool_name: &ToolName) -> HookToolName {
    if tool_name.name == "spawn_agent" && tool_name.namespace.is_none() {
        return HookToolName::spawn_agent();
    }

    HookToolName::new(flat_tool_name(tool_name).into_owned())
}

/// A compiled argument validator plus the schema it was compiled from.
///
/// The schema is retained deliberately. A bare `jsonschema` message names what
/// was wrong ("Additional properties are not allowed ('kind' was unexpected)",
/// "is not valid under any of the schemas listed in the 'oneOf' keyword")
/// without naming what would be right, so a rejected call gives the model
/// nothing to correct towards and the next attempt can repeat the same mistake.
/// Keeping the schema lets the rejection state the accepted shape, which is
/// already known here, instead of spending another turn on a guess.
struct CompiledArgumentValidator {
    validator: jsonschema::Validator,
    schema: Value,
}

fn compile_code_mode_argument_validator(
    tool_name: &ToolName,
    schema: Option<&Value>,
) -> Result<Option<CompiledArgumentValidator>, String> {
    let Some(schema) = schema else {
        return Ok(None);
    };
    let validator = jsonschema::validator_for(schema).map_err(|err| {
        format!("tool `{tool_name}` rejected before execution: argument schema could not be compiled: {}", bounded_preflight_diagnostic(err.masked()))
    })?;
    Ok(Some(CompiledArgumentValidator {
        validator,
        schema: schema.clone(),
    }))
}

/// Property names an object schema accepts.
///
/// Order follows the serialized schema map, which is sorted rather than
/// declaration-ordered. That is deliberate here: a stable, reproducible list is
/// what makes two rejections of the same call comparable.
fn schema_property_names(schema: &Value) -> Vec<&str> {
    schema
        .get("properties")
        .and_then(Value::as_object)
        .map(|properties| properties.keys().map(String::as_str).collect())
        .unwrap_or_default()
}

/// Conservative instance-path fallback. Composition branches are selected by
/// the validator's schema path below, never guessed from an instance pointer.
fn schema_at<'a>(schema: &'a Value, pointer: &str) -> Option<&'a Value> {
    fn dereference<'a>(
        root: &'a Value,
        mut node: &'a Value,
        budget: &mut usize,
    ) -> Option<&'a Value> {
        while let Some(reference) = node.get("$ref") {
            *budget = budget.checked_sub(1)?;
            // Do not invent a merged shape for constrained reference siblings.
            if node.as_object()?.keys().any(|key| {
                !matches!(
                    key.as_str(),
                    "$ref"
                        | "$defs"
                        | "definitions"
                        | "$id"
                        | "$schema"
                        | "title"
                        | "description"
                        | "$comment"
                )
            }) {
                return None;
            }
            let reference = reference.as_str()?.strip_prefix('#')?;
            node = root.pointer(reference)?;
        }
        Some(node)
    }
    let mut node = schema;
    let mut budget = 64usize;
    if !pointer.is_empty() && !pointer.starts_with('/') {
        return None;
    }
    for segment in pointer.split('/').skip(1) {
        node = dereference(schema, node, &mut budget)?;
        let segment = segment.replace("~1", "/").replace("~0", "~");
        // Instance pointers address data, so an array index maps to the
        // schema's `items`, not to a sibling of the same ordinal.
        let index = segment.chars().all(|c| c.is_ascii_digit());
        let property = node.get("properties").and_then(|props| props.get(&segment));
        let next = match (property, node.get("items")) {
            (Some(property), _) => Some(property),
            (_, Some(items)) if index && !segment.is_empty() => Some(items),
            // Any other segment, a numeric map key included, is an object key:
            // a declared property, else the schema every undeclared key of a
            // map must match. A boolean `additionalProperties` has no shape.
            _ => node
                .get("properties")
                .and_then(|props| props.get(&segment))
                .or_else(|| {
                    node.get("additionalProperties")
                        .filter(|schema| schema.is_object())
                }),
        };
        node = next?;
    }
    dereference(schema, node, &mut budget)
}

/// The validator identifies the actual failing branch, including nested
/// oneOf/anyOf and referenced definitions. Instance paths alone lose that fact.
fn validation_schema<'a>(
    error: &jsonschema::ValidationError<'_>,
    schema: &'a Value,
) -> Option<&'a Value> {
    error.schema_path().as_str().rsplit_once('/')
        .and_then(|(parent, _)| schema.pointer(parent))
        .or_else(|| schema_at(schema, error.instance_path().as_str()))
}

/// Describe the accepted shape at the validator-selected schema node.
///
/// Only facts already present in the tool's own schema are reported: the
/// accepted property names, or the named `oneOf`/`anyOf` branches with the
/// required keys of each. Nothing is inferred about intent.
fn accepted_shape(node: &Value) -> Option<String> {
    for keyword in ["oneOf", "anyOf"] {
        if let Some(branches) = node.get(keyword).and_then(Value::as_array) {
            let described = branches
                .iter()
                .filter_map(|branch| {
                    // A `const`/`enum` discriminator is the branch's name when
                    // the schema supplies one; otherwise fall back to its keys.
                    let tag = branch
                        .get("properties")
                        .and_then(Value::as_object)
                        .and_then(|properties| {
                            properties.iter().find_map(|(key, value)| {
                                value.get("const")
                                    .or_else(|| {
                                        let values = value.get("enum")?.as_array()?;
                                        (values.len() == 1).then(|| &values[0])
                                    })
                                    .map(|constant| format!("{key}={}", bounded_preflight_diagnostic(constant)))
                            })
                        });
                    let keys = schema_property_names(branch);
                    let mut keys = if keys.is_empty() {
                        branch.get("enum").or_else(|| branch.get("const"))
                            .or_else(|| branch.get("type"))
                            .map(bounded_preflight_diagnostic).unwrap_or_default()
                    } else {
                        format!("{{{}}}", keys.join(", "))
                    };
                    if let Some(required) = branch.get("required")
                        && required.as_array().is_some_and(|items| !items.is_empty())
                    {
                        keys.push_str(&format!("; required: {}", bounded_preflight_diagnostic(required)));
                    }
                    if tag.is_none() && keys.is_empty() {
                        return None;
                    }
                    // Join only when both parts exist, so an undiscriminated
                    // branch does not render with a leading space.
                    Some(match tag {
                        Some(tag) if keys.is_empty() => tag,
                        Some(tag) => format!("{tag} {keys}"),
                        None => keys,
                    })
                })
                .collect::<Vec<_>>();
            if !described.is_empty() {
                return Some(format!("accepted forms: {}", described.join(" | ")));
            }
        }
    }

    let properties = schema_property_names(node);
    if properties.is_empty() {
        return None;
    }
    let required = node
        .get("required")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .filter(|required| !required.is_empty());
    let mut description = format!("accepted properties: {}", properties.join(", "));
    if let Some(required) = required {
        description.push_str(&format!("; required: {required}"));
    }
    Some(description)
}

#[derive(Default)]
struct CodeModeArgumentPreflight {
    validator: OnceLock<Result<Option<CompiledArgumentValidator>, String>>,
}

/// Bound formatting itself: truncating `error.to_string()` would first copy the
/// entire rejected argument, and can hide the constraint behind a huge value.
fn bounded_preflight_diagnostic(value: impl std::fmt::Display) -> String {
    struct Bounded(String);
    impl std::fmt::Write for Bounded {
        fn write_str(&mut self, value: &str) -> std::fmt::Result {
            let remaining = 1_024_usize.saturating_sub(self.0.len());
            if value.len() <= remaining {
                self.0.push_str(value);
                return Ok(());
            }
            let mut end = remaining;
            while !value.is_char_boundary(end) {
                end -= 1;
            }
            self.0.push_str(&value[..end]);
            Err(std::fmt::Error)
        }
    }
    let mut bounded = Bounded(String::new());
    if std::fmt::write(&mut bounded, format_args!("{value}")).is_err() {
        bounded.0.push_str("… [diagnostic truncated]");
    }
    bounded.0
}

impl CodeModeArgumentPreflight {
    fn validate(
        &self,
        tool_name: &ToolName,
        schema: Option<&Value>,
        payload: &ToolPayload,
        parsed_function_arguments: Option<&ParsedFunctionArguments>,
        native_compatibility: bool,
    ) -> Result<(), codex_protocol::tool_failure::ToolFailure> {
        use codex_protocol::tool_failure::{ToolFailure, ToolFailureCode, ToolFieldError};
        let reject = |message| ToolFailure::no_effects(ToolFailureCode::InvalidInput, message);
        let ToolPayload::Function { arguments } = payload else {
            return Ok(());
        };
        // Preserve the native decoders' compatibility rules. Consult the
        // advertised validator only to explain an already rejected payload.
        let native_rejection = if native_compatibility && tool_name.namespace.is_none() {
            match tool_name.name.as_str() {
                "exec_command" => Some(crate::tools::handlers::validate_exec_command_arguments(arguments)),
                "read_file" => Some(crate::tools::handlers::validate_read_file_arguments(arguments)),
                _ => None,
            }
        } else {
            None
        };
        let native_rejection = match native_rejection {
            Some(Ok(())) => return Ok(()),
            Some(Err(message)) => Some(message),
            None => None,
        };
        // The schema can only list snapshot selector kinds; name the section
        // selector that replaces a read_file-only one.
        if native_compatibility
            && tool_name.namespace.is_none()
            && tool_name.name == "read_tool_output"
            && let Some(hint) =
                crate::tools::handlers::read_tool_output_file_only_selector_hint(arguments)
        {
            return Err(reject(format!("tool `{tool_name}` rejected before execution: {hint}")));
        }
        let value = parsed_function_arguments
            .and_then(|parsed| parsed.value().ok())
            .cloned()
            .ok_or_else(|| {
                match serde_json::from_str::<Value>(arguments) {
                    Err(error) => ToolFailure::invalid_json(error.line(), error.column()),
                    Ok(_) => ToolFailure::no_effects(ToolFailureCode::Failed,
                        "Parsed argument representation is unavailable; inspect the original call receipt."),
                }
            })?;
        if !value.is_object() {
            let mut failure = reject(format!(
                "tool `{tool_name}` rejected before execution: $: expected a JSON object for arguments"
            ));
            failure.field_errors.push(ToolFieldError {
                path: String::new(), expected: "a JSON object".into(),
                correction: "Supply named arguments as a JSON object, not an array or scalar.".into(),
            });
            return Err(failure);
        }

        let validator = self
            .validator
            .get_or_init(|| compile_code_mode_argument_validator(tool_name, schema));
        let validator = validator.as_ref().map_err(|message| {
            ToolFailure::no_effects(ToolFailureCode::Failed, message.clone())
        })?;
        let diagnostic = validator.as_ref().and_then(|compiled| {
            argument_schema_diagnostic(&compiled.validator, &compiled.schema, &value)
        });
        match diagnostic.or_else(|| native_rejection.map(reject)) {
            Some(mut failure) => {
                // A malformed tail selector is one error on that item, not
                // a separate violation of every mutually exclusive branch.
                if native_compatibility && tool_name.namespace.is_none() && tool_name.name == "read_file"
                    && let Some(compiled) = validator.as_ref()
                    && let Some(indices) = read_file_snapshot_tail_errors(&compiled.validator, &value)
                {
                    let details = serde_json::json!({"message": failure.message, "field_errors": failure.field_errors,
                        "details": failure.details});
                    failure.message = "Invalid tail selector. Use kind=tail with optional integer lines (1..2000) and max_bytes (1..1048576). In Code Mode, the native error retains the full schema diagnostic in tool_failure.details.selector_branches.".into();
                    failure.field_errors = indices
                        .into_iter()
                        .map(|index| ToolFieldError {
                            path: format!("/selectors/{index}"),
                            expected: "tail with only optional bounded lines and max_bytes".into(),
                            correction: "Use the advertised read_file selector and its required fields.".into(),
                        })
                        .collect();
                    failure.details = Some(serde_json::json!({"selector_branches": details}));
                } else if native_compatibility && tool_name.namespace.is_none() && tool_name.name == "read_file"
                    && let Some(path) = misplaced_read_file_encoding(&value)
                {
                    let details = serde_json::json!({"message": failure.message, "field_errors": failure.field_errors,
                        "details": failure.details});
                    failure.message = "Bytes selectors do not accept encoding; pass it beside path and selectors. The native error retains the full schema diagnostic in tool_failure.details.selector_branches.".into();
                    failure.field_errors = vec![ToolFieldError {
                        path,
                        expected: "encoding at the top level of read_file arguments".into(),
                        correction: "Move encoding to /encoding; keep kind, start and end inside the bytes selector.".into(),
                    }];
                    failure.details = Some(serde_json::json!({"selector_branches": details}));
                } else if native_compatibility && tool_name.namespace.is_none() && tool_name.name == "read_file"
                    && misplaced_resume_filter(&value)
                {
                    let details = serde_json::json!({"message": failure.message, "field_errors": failure.field_errors,
                        "details": failure.details});
                    failure.message = "For context:resume-state, pass resume_filter beside path, not inside selectors. The native error retains the full schema diagnostic in tool_failure.details.selector_branches.".into();
                    failure.field_errors = vec![ToolFieldError {
                        path: "/selectors/0/resume_filter".into(),
                        expected: "a top-level resume_filter on context:resume-state".into(),
                        correction: "Move the filter to /resume_filter and omit the selector wrapper.".into(),
                    }];
                    failure.details = Some(serde_json::json!({"selector_branches": details}));
                }
                failure.message = format!("tool `{tool_name}` rejected before execution: {}", failure.message);
                Err(failure)
            }
            None => Ok(()),
        }
    }
}

// Diagnose the relocation only if it is the sole issue in the advertised
// shape and the relocated arguments pass the native parser. Never run them.
fn misplaced_read_file_encoding(value: &Value) -> Option<String> {
    if value.get("encoding").is_some() {
        return None;
    }
    let mut misplaced = None;
    for (index, selector) in value.get("selectors")?.as_array()?.iter().enumerate() {
        if let Some(encoding) = selector.get("encoding") {
            if misplaced.is_some()
                || selector["kind"] != "bytes"
                || !matches!(encoding.as_str(), Some("hex" | "base64"))
            {
                return None;
            }
            misplaced = Some(index);
        }
    }
    let index = misplaced?;
    let mut corrected = value.clone();
    let encoding = corrected["selectors"][index].as_object_mut()?.remove("encoding")?;
    corrected["encoding"] = encoding;
    crate::tools::handlers::validate_read_file_arguments(&corrected.to_string()).ok()?;
    Some(format!("/selectors/{index}/encoding"))
}

// A misplaced inventory filter is not a malformed data projection. Offer a
// relocation only for this unambiguous envelope and a valid filter; keep the
// general diagnostic for unrelated errors, siblings and invalid filters.
fn misplaced_resume_filter(value: &Value) -> bool {
    if value["path"] != crate::session::resume_inventory::LOCATOR
        || value.as_object().is_none_or(|args| args.len() != 2)
    {
        return false;
    }
    let Some([selector]) = value["selectors"].as_array().map(Vec::as_slice) else {
        return false;
    };
    selector["kind"] == "structured"
        && selector.as_object().is_some_and(|fields| fields.len() == 2)
        && selector.get("resume_filter").is_some_and(|filter| {
            use serde::Deserialize;
            crate::session::resume_inventory::ResumeInventoryFilter::deserialize(filter)
                .is_ok_and(|filter| filter.validate().is_ok())
        })
}

// Only summarize errors on tail selector items themselves. A root, array,
// sibling, or member error still needs its original independent diagnostic.
fn read_file_snapshot_tail_errors(
    validator: &jsonschema::Validator,
    value: &Value,
) -> Option<Vec<usize>> {
    let selectors = value.get("selectors")?.as_array()?;
    if !selectors.iter().any(|item| item["kind"] == "tail") {
        return None;
    }
    let mut indices = Vec::new();
    for error in validator.iter_errors(value) {
        let path = error.instance_path().as_str();
        let index = path.strip_prefix("/selectors/")?.parse::<usize>().ok()?;
        if selectors.get(index)?["kind"] != "tail" {
            return None;
        }
        indices.push(index);
    }
    indices.sort_unstable();
    indices.dedup();
    (!indices.is_empty()).then_some(indices)
}

/// Reuse validator facts, never rejected values. Shared by registry preflight
/// and MCP's canonical-schema check after hooks; neither path repairs arguments.
pub(crate) fn argument_schema_diagnostic(
    validator: &jsonschema::Validator,
    schema: &Value,
    value: &Value,
) -> Option<codex_protocol::tool_failure::ToolFailure> {
    use codex_protocol::tool_failure::{ToolFailure, ToolFailureCode, ToolFieldError};

    const MAX_FIELD_PATH_BYTES: usize = 1_024;

    // Do not put a truncated (and potentially different) JSON Pointer on the
    // wire. Compute the escaped length before copying any caller-provided key.
    fn member_path(parent: &str, name: &str) -> Option<String> {
        let remaining = MAX_FIELD_PATH_BYTES.checked_sub(parent.len().checked_add(1)?)?;
        if name.len() > remaining {
            return None;
        }
        let extra = name.bytes().filter(|byte| matches!(byte, b'~' | b'/')).count();
        if name.len() + extra > remaining {
            return None;
        }
        Some(format!("{parent}/{}", name.replace('~', "~0").replace('/', "~1")))
    }

    fn field_errors(
        error: &jsonschema::ValidationError<'_>,
        schema: &Value,
    ) -> (Vec<ToolFieldError>, bool) {
        use jsonschema::error::ValidationErrorKind;
        let pointer = error.instance_path().as_str();
        if pointer.len() > MAX_FIELD_PATH_BYTES {
            return (Vec::new(), true);
        }
        let mut omitted = false;
        let node = validation_schema(error, schema);
        let (paths, expected, correction) = match error.kind() {
            ValidationErrorKind::Required { property } => {
                let path = if let Some(name) = property.as_str() {
                    let Some(path) = member_path(pointer, name) else {
                        return (Vec::new(), true);
                    };
                    path
                } else {
                    pointer.to_string()
                };
                let property_schema = node.and_then(|node| node.get("properties"))
                    .and_then(|properties| properties.get(property.as_str()?))
                    .or_else(|| schema_at(schema, &path));
                let expected = property_schema.and_then(|node| node.get("enum").or_else(|| node.get("const")).or_else(|| node.get("type")))
                    .map(bounded_preflight_diagnostic)
                    .unwrap_or_else(|| "a value for this required property".into());
                (vec![path], expected, "Supply this required field using the advertised contract.")
            }
            ValidationErrorKind::AdditionalProperties { unexpected }
            | ValidationErrorKind::UnevaluatedProperties { unexpected } => (
                unexpected.iter().take(13).filter_map(|name| {
                    let path = member_path(pointer, name);
                    omitted |= path.is_none();
                    path
                }).collect(),
                node.and_then(accepted_shape).unwrap_or_else(|| "only properties accepted by the advertised schema".into()),
                "Remove this unsupported field; use an advertised property instead.",
            ),
            // Do not display the rejected key or the nested unmasked instance.
            ValidationErrorKind::PropertyNames { error } => (
                vec![pointer.to_string()], bounded_preflight_diagnostic(error.masked()),
                "Use property names satisfying the advertised propertyNames constraint.",
            ),
            ValidationErrorKind::AnyOf { .. } | ValidationErrorKind::OneOfNotValid { .. }
            | ValidationErrorKind::OneOfMultipleValid { .. } => (
                vec![pointer.to_string()], node.and_then(accepted_shape)
                    .unwrap_or_else(|| "the advertised schema alternatives".into()),
                "Choose an advertised form and satisfy its fields; branch diagnostics are alternatives, not cumulative requirements.",
            ),
            ValidationErrorKind::Custom { .. } => (
                vec![pointer.to_string()], "the advertised custom constraint".into(),
                "Correct this field using the advertised contract.",
            ),
            ValidationErrorKind::Enum { options } => (
                vec![pointer.to_string()], format!("allowed values: {}", bounded_preflight_diagnostic(options)),
                "Choose one of the allowed values without changing other arguments.",
            ),
            _ => (vec![pointer.to_string()], bounded_preflight_diagnostic(error.masked()),
                "Set this field to a value satisfying the stated constraint."),
        };
        (paths.into_iter().map(|path| ToolFieldError {
            path, expected: expected.clone(), correction: correction.into(),
        }).collect(), omitted)
    }

    fn describe(error: &jsonschema::ValidationError<'_>, schema: &Value) -> String {
        use jsonschema::error::ValidationErrorKind;

        let message = match error.kind() {
            // jsonschema's masked PropertyNames display delegates to the
            // unmasked nested error, whose instance is the rejected key.
            ValidationErrorKind::PropertyNames { error } => return describe(error, schema),
            ValidationErrorKind::Custom { .. } => "custom schema constraint failed".to_string(),
            ValidationErrorKind::Enum { options } => {
                format!("allowed values: {}", bounded_preflight_diagnostic(options))
            }
            _ => bounded_preflight_diagnostic(error.masked()),
        };
        let pointer = error.instance_path().as_str();
        let path = bounded_preflight_diagnostic(format_args!("${pointer}"));
        let constraint = bounded_preflight_diagnostic(error.schema_path());
        let mut description = format!("{path}: {message} (schema {constraint})");
        if let Some(shape) = validation_schema(error, schema).and_then(accepted_shape) {
            description.push_str(&format!(" ({})", bounded_preflight_diagnostic(shape)));
        }
        description
    }

    fn append(
        error: &jsonschema::ValidationError<'_>,
        schema: &Value,
        messages: &mut Vec<String>,
        fields: &mut Vec<ToolFieldError>,
        remaining: &mut usize,
    ) -> bool {
        use jsonschema::error::ValidationErrorKind;

        if *remaining == 0 {
            return true;
        }
        *remaining -= 1;
        messages.push(describe(error, schema));
        let (rejected_fields, incomplete_paths) = field_errors(error, schema);
        let mut omitted = incomplete_paths || fields.len() + rejected_fields.len() > 12;
        fields.extend(rejected_fields.into_iter().take(12_usize.saturating_sub(fields.len())));
        if let ValidationErrorKind::AnyOf { context }
        | ValidationErrorKind::OneOfNotValid { context } = error.kind()
        {
            // Branch errors are supplied by this validation, not another pass
            // or a guessed choice of the caller's intended branch.
            for branch in context {
                for error in branch {
                    if *remaining == 0 {
                        return true;
                    }
                    omitted |= append(error, schema, messages, fields, remaining);
                }
            }
        }
        omitted
    }

    let mut errors = validator.iter_errors(value);
    let mut messages = Vec::new();
    let mut fields = Vec::new();
    let mut remaining = 12;
    let mut omitted = false;
    for error in errors.by_ref().take(4) {
        omitted |= append(&error, schema, &mut messages, &mut fields, &mut remaining);
    }
    omitted |= errors.next().is_some();
    if messages.is_empty() {
        return None;
    }
    if omitted {
        messages.push("additional errors omitted".to_string());
    }
    let mut failure = ToolFailure::no_effects(ToolFailureCode::InvalidInput, messages.join("; "));
    failure.field_errors = fields;
    failure.details = Some(serde_json::json!({"field_errors_complete": !omitted}));
    Some(failure)
}

fn function_hook_tool_input(arguments: &str) -> Value {
    if arguments.trim().is_empty() {
        return Value::Object(serde_json::Map::new());
    }

    match parsed_function_argument_value(arguments) {
        Some(Ok(value)) => value,
        Some(Err(_)) => Value::String(arguments.to_string()),
        None => {
            serde_json::from_str(arguments).unwrap_or_else(|_| Value::String(arguments.to_string()))
        }
    }
}

fn unsupported_tool_call_message(
    payload: &ToolPayload,
    tool_name: &ToolName,
    tools: &HashMap<ToolName, RegisteredTool>,
) -> String {
    let message = match payload {
        ToolPayload::Custom { .. } => format!("unsupported custom tool call: {tool_name}"),
        _ => format!("unsupported call: {tool_name}"),
    };
    let leaf = tool_name.name.rsplit('.').next().unwrap_or(&tool_name.name);
    let mut alternatives = tools.keys()
        .filter(|name| name.name == tool_name.name || name.name == leaf)
        .map(ToString::to_string).collect::<Vec<_>>();
    alternatives.sort();
    let mut message = format!("{message}. Rejected before execution; no tool was run. Use an exact name and call form from the current tool declarations.");
    if !alternatives.is_empty() {
        message.push_str(&format!(" Current catalog names with this leaf: {}. These are name corrections, not proof of equivalent behavior or permission.", alternatives.join(", ")));
    }
    if tools.keys().any(|name| name.namespace.is_none() && name.name == "tool_search") {
        message.push_str(" The registered tool_search can discover and activate tools; inspect its current contract.");
    }
    message.push_str(" Preserve earlier results and effects; this rejection does not authorize replaying other operations.");
    message
}
