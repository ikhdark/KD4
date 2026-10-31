//! Per-turn validation mode policy.

use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::tools::sandboxing::ToolCtx;

/// A distinct historical receipt, never an exec result with a fabricated exit
/// status. Keep its structured shape in Code Mode as well as direct delivery.
struct CompilationEvidenceOutput {
    inner: crate::tools::context::FunctionToolOutput,
    receipt: serde_json::Value,
}

impl codex_tools::ToolOutput for CompilationEvidenceOutput {
    fn log_preview(&self) -> String { self.receipt.to_string() }
    fn success_for_logging(&self) -> bool { true }
    fn sampling_request_signal(&self) -> Option<serde_json::Value> { Some(self.receipt.clone()) }
    fn projection_metadata(&self) -> Option<codex_tools::ToolOutputProjectionMetadata> {
        self.inner.projection_metadata()
    }
    fn canonical_result(&self, _: &codex_tools::ToolPayload) -> Option<codex_tools::CanonicalToolResult> {
        Some(codex_tools::CanonicalToolResult::json(self.receipt.clone()))
    }
    fn to_response_item(&self, call_id: &str, payload: &codex_tools::ToolPayload)
        -> codex_protocol::models::ResponseInputItem
    { self.inner.to_response_item(call_id, payload) }
    fn code_mode_result(&self, _: &codex_tools::ToolPayload) -> serde_json::Value {
        self.receipt.clone()
    }
}

/// A policy refusal is not an execution failure or a busy validation lane.
pub(crate) fn policy_refusal(
    turn: &TurnContext,
    reason: impl Into<String>,
) -> crate::tools::context::FunctionToolOutput {
    let signal = serde_json::json!({
        "validation_policy_refusal": true,
        "mode": turn.validation_mode,
        "execution_state": "not_started",
        "status": "unvalidated",
        "reason": reason.into(),
    });
    let mut output = crate::tools::context::FunctionToolOutput::from_text(
        signal.to_string(), Some(false),
    ).with_sampling_request_signal(signal.clone());
    output.essential_inline = signal.as_object().cloned().unwrap_or_default();
    output
}

pub(crate) fn mode_refusal(
    turn: &TurnContext,
    classification: codex_command_analysis::validation::ValidationPolicyClassification,
    declared: bool,
) -> Option<crate::tools::context::FunctionToolOutput> {
    use codex_command_analysis::validation::ValidationPolicyClassification as Class;
    use codex_protocol::validation::ValidationMode;
    if classification == Class::ReadOnly { return None; }
    let recognized = classification.is_validation();
    if !recognized && !declared { return None; }
    let reason = match turn.validation_mode {
        ValidationMode::Off => "Validation is disabled for this turn. No command was launched; report unvalidated rather than retrying.",
        ValidationMode::Compile if classification != Class::TargetedCompilation =>
            "COMPILE permits only targeted compile checks, not tests, gates, formatting, non-compilation checks, or full builds. No command was launched.",
        ValidationMode::FullHalf if matches!(classification, Class::Tests | Class::Checks) =>
            "Full .5 allows compile checks, permitted non-test KD4 scripts workflows, and other task-authorized operations, but not tests/full test suites or other non-compilation checks in any language. Only the user can select FULL, by starting a new message with `/validation full` on its own first line; it lasts until their next message. No command was launched.",
        _ => return None,
    };
    Some(policy_refusal(turn, reason))
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn reserve_policy(
    session: &Session,
    turn: &TurnContext,
    call_id: &str,
    classification: codex_command_analysis::validation::ValidationPolicyClassification,
    declared: bool,
    selection: String,
    execution_scope: &str,
    source_revision: Option<&str>,
    environment_id: &str,
    cwd: &codex_utils_path_uri::PathUri,
    command: &[String],
    environment: &std::collections::HashMap<String, String>,
    fs: &dyn codex_exec_server::ExecutorFileSystem,
) -> Result<Option<codex_command_analysis::command_preflight::FullSuitePermit>, Box<dyn codex_tools::ToolOutput>> {
    use codex_command_analysis::validation::ValidationPolicyClassification as Class;
    if classification == Class::ReadOnly { return Ok(None); }
    if let Some(refusal) = mode_refusal(turn, classification, declared) { return Err(Box::new(refusal)); }
    if !declared && !classification.is_validation() {
        return Ok(None);
    }
    let sandbox = turn.file_system_sandbox_context(None, cwd);
    let closed = if classification == Class::TargetedCompilation
        && environment_id == codex_exec_server::LOCAL_ENVIRONMENT_ID
    {
        closed_syntax_inputs(command, cwd, environment, fs, &sandbox).await
    } else { None };
    let revision = if let Some(paths) = closed {
        let named = paths;
        let fingerprints = compilation_reuse_fingerprints(fs, &sandbox, &named).await;
        let scope = serde_json::json!([environment_id, execution_scope, command, sandbox]).to_string();
        let revision = turn.full_suite_budget.compilation_revision(
            &scope, environment_id, &named, true, fingerprints,
        );
        codex_command_analysis::command_preflight::FullSuiteBudget::bind_compilation_reuse(
            &revision, &scope, command,
        ).unwrap_or(revision)
    } else if classification == Class::TargetedCompilation {
        // Cache invalidation is not source-change evidence. Keep it for other
        // command observations, but use exact edits/named inputs for repairs.
        let source_selection = codex_command_analysis::validation::compilation_source_selection(&selection);
        let scope = serde_json::json!([environment_id, execution_scope, source_selection]).to_string();
        let unambiguous = command.split_first().is_some_and(|(program, args)|
            codex_shell_command::validation::validation_resource_cwd_is_unambiguous(program, args));
        let native_cwd = (environment_id == codex_exec_server::LOCAL_ENVIRONMENT_ID)
            .then(|| cwd.to_abs_path().ok()).flatten();
        let explicit_inputs = if !unambiguous { Some(Vec::new()) } else {
            codex_command_analysis::validation::explicit_compilation_inputs(&selection, command).map(|paths|
                paths.into_iter().map(|path| match native_cwd.as_ref() {
                    Some(base) => codex_utils_absolute_path::AbsolutePathBuf::resolve_literal_path_against_base(path, base)
                        .to_path_buf(),
                    None if path.has_root() => path,
                    None => std::path::PathBuf::from(cwd.inferred_native_path_string()).join(path),
                }).collect::<Vec<_>>())
        };
        let mut paths = explicit_inputs.clone().unwrap_or_default();
        let mut fingerprints = if native_cwd.is_some() && unambiguous {
            let paths = paths.clone();
            tokio::task::spawn_blocking(move || crate::git_workspace::compilation_input_fingerprints(paths))
                .await.unwrap_or_default()
        } else { Default::default() };
        if explicit_inputs.is_none() && unambiguous
            && let Some(cwd) = native_cwd.as_ref()
        {
            let (selection, command, cwd) = (selection.clone(), command.to_vec(), cwd.to_path_buf());
            if let Ok(Some((named, current))) = tokio::task::spawn_blocking(move || {
                let cwd = codex_command_analysis::validation::compilation_cargo_cwd(&command, &cwd)?;
                let (manifests, roots) = codex_command_analysis::command_preflight::compilation_sources(&selection, &cwd)?;
                Some(crate::git_workspace::compilation_rust_inputs(manifests, roots))
            }).await {
                paths = named;
                fingerprints = current;
            }
        }
        turn.full_suite_budget.compilation_revision(
            &scope, environment_id, &paths, explicit_inputs.is_some(), fingerprints,
        )
    } else {
        let epoch = session.services.command_execution
            .observe_repository_revision(&turn.sub_id, 0).await;
        serde_json::json!([source_revision, epoch]).to_string()
    };
    if classification == Class::TargetedCompilation
        && let Some(evidence) = turn.full_suite_budget.reusable_compilation_evidence(&revision)
    {
        let signal = serde_json::json!({
            "compilation_evidence_reused": true,
            "execution_state": "not_started",
            "did_run": false,
            "process_exited": false,
            "exit_code": null,
            "output": "",
            "output_complete": false,
            "output_reduced": false,
            "raw_output_artifact_retention_limit_hit": false,
            "status": "reused",
            "dependency_coverage": "complete",
            "original_evidence": evidence,
            "notice": "No command was launched. This is the original successful compilation receipt, not a fresh execution or artifact.",
        });
        let mut output = crate::tools::context::FunctionToolOutput::from_text(
            signal.to_string(), Some(true),
        ).with_sampling_request_signal(signal.clone());
        output.essential_inline = signal.as_object().cloned().unwrap_or_default();
        return Err(Box::new(CompilationEvidenceOutput { inner: output, receipt: signal }));
    }
    turn.full_suite_budget.reserve_policy(
        reservation_owner(session, turn, call_id),
        serde_json::json!([execution_scope, selection]).to_string(), revision,
        classification == Class::TargetedCompilation,
    ).map(Some).map_err(|reason| crate::tools::context::boxed_tool_output(policy_refusal(turn, reason)))
}

/// CommonJS syntax checking consumes one source without executing its imports.
/// Other compiler graphs (Cargo macros/build scripts, Java/TS resolution, Python
/// startup/imports, shell profiles and remote tools) are not closed by the named
/// repair graph. Do not promote those observations to reusable passing proof.
async fn closed_syntax_inputs(
    command: &[String], cwd: &codex_utils_path_uri::PathUri,
    environment: &std::collections::HashMap<String, String>,
    fs: &dyn codex_exec_server::ExecutorFileSystem,
    sandbox: &codex_exec_server::FileSystemSandboxContext,
) -> Option<Vec<std::path::PathBuf>> {
    let [program, flag, source] = command else { return None; };
    let tool = std::path::PathBuf::from(program);
    if !tool.is_absolute() || !tool.file_name()?.to_str()?.eq_ignore_ascii_case("node.exe")
        || !matches!(flag.as_str(), "--check" | "-c")
        || environment.iter().any(|(key, value)| !value.is_empty()
            && (key.to_ascii_uppercase().starts_with("NODE_")
                || key.eq_ignore_ascii_case("OPENSSL_CONF")))
    { return None; }
    let source = codex_utils_absolute_path::AbsolutePathBuf::resolve_literal_path_against_base(
        source, cwd.to_abs_path().ok()?,
    );
    let source = fs.canonicalize(&codex_utils_path_uri::PathUri::from_abs_path(&source), Some(sandbox)).await.ok()?;
    let source = source.to_abs_path().ok()?.to_path_buf();
    if source.extension()?.to_str()? != "cjs" { return None; }
    let tool = codex_utils_absolute_path::AbsolutePathBuf::try_from(tool).ok()?;
    let tool = fs.canonicalize(&codex_utils_path_uri::PathUri::from_abs_path(&tool), Some(sandbox)).await.ok()?;
    Some(vec![source, tool.to_abs_path().ok()?.to_path_buf()])
}

async fn compilation_reuse_fingerprints(
    fs: &dyn codex_exec_server::ExecutorFileSystem,
    sandbox: &codex_exec_server::FileSystemSandboxContext,
    paths: &[std::path::PathBuf],
) -> std::collections::BTreeMap<std::path::PathBuf, String> {
    let mut result = std::collections::BTreeMap::new();
    let mut remaining = 256 * 1024 * 1024u64;
    for path in paths.iter().take(256) {
        let Ok(absolute) = codex_utils_absolute_path::AbsolutePathBuf::try_from(path.clone()) else { continue; };
        let uri = codex_utils_path_uri::PathUri::from_abs_path(&absolute);
        let Ok(metadata) = fs.get_metadata(&uri, Some(sandbox)).await else { continue; };
        if !metadata.is_file || metadata.size > remaining.min(128 * 1024 * 1024) { continue; }
        // The existing filesystem owner checks opened-object and path revision
        // stability while streaming; no raw input bytes or native bypass here.
        if let Ok(Some(digest)) = fs.checksum_file(&uri, false, Some(sandbox)).await
            && digest.file_bytes <= remaining.min(128 * 1024 * 1024)
        {
            remaining -= digest.file_bytes;
            use sha2::Digest;
            // Preserve the opened-object revision too: an edit-and-revert
            // during compilation must not look like an unchanged input.
            let identity = serde_json::json!([digest.sha256, digest.revision]).to_string();
            result.insert(path.clone(), hex::encode(sha2::Sha256::digest(identity.as_bytes())));
        }
    }
    result
}

pub(crate) async fn retain_exec_compilation(
    session: &Session, turn: &TurnContext, call_id: &str,
    response: &mut crate::tools::context::ExecCommandToolOutput,
    environment: &crate::session::turn_context::TurnEnvironment,
) {
    if response.exit_code != Some(0) || !response.process_exited || response.error.is_some()
        || !response.process_output.as_ref().is_some_and(|output|
            output.output_closed && output.aggregated_output_is_exact)
        || turn.full_suite_budget.compilation_evidence_inputs(
            &reservation_owner(session, turn, call_id),
        ).is_none()
    { return; }
    if !response.raw_output_artifact.as_ref().is_some_and(|artifact| artifact.capture_finalized_complete())
        && let Some(output) = response.process_output.as_ref()
    {
        response.raw_output_artifact = Some(crate::tools::command_output_artifact::create_raw_output_artifact(
            turn.config.codex_home.as_path(), &session.thread_id().to_string(),
            &output.aggregated_output,
        ).await);
    }
    let reference = crate::tools::command_output_artifact::RawOutputArtifact::output_reference(
        response.raw_output_artifact.as_ref(),
    );
    if reference["capture_complete"] == true {
        let owner = reservation_owner(session, turn, call_id);
        let Some(paths) = turn.full_suite_budget.compilation_evidence_inputs(&owner) else { return; };
        let sandbox = turn.file_system_sandbox_context(None, environment.cwd());
        let current = compilation_reuse_fingerprints(
            environment.environment.get_filesystem().as_ref(), &sandbox, &paths,
        ).await;
        turn.full_suite_budget.record_compilation_success(&owner, current, serde_json::json!({
            "call_id": call_id,
            "exit_code": 0, "output_reference": reference,
            "incarnation": response.session_capabilities.as_ref().map(|caps| caps.incarnation),
        }));
    }
}

pub(crate) fn reservation_owner(session: &Session, turn: &TurnContext, call: &str) -> String {
    format!("{}:{}:{call}", session.thread_id(), turn.sub_id)
}

pub(crate) async fn acquire_execution_resources(
    ctx: &ToolCtx,
    command: &[String],
    cwd: Option<&std::path::Path>,
) -> Result<Option<crate::validation_resources::ValidationResourceLease>, String> {
    let owner = reservation_owner(&ctx.session, &ctx.turn, &ctx.call_id);
    if !ctx.turn.full_suite_budget.reserved_for(&owner)
        && !ctx.turn.full_suite_budget.started_for(&owner)
    {
        return Ok(None);
    }
    let Some(cwd) = cwd else { return Ok(None); };
    crate::validation_resources::acquire_python_test_resource(command, cwd).await
}

/// Checkpoint consumption before launch. Only proven nonexecution permits retry.
#[expect(
    clippy::await_holding_invalid_type,
    reason = "The checkpoint guard serializes the budget transition with its durable publication"
)]
pub(crate) async fn begin_policy_attempt(ctx: &ToolCtx) -> Result<(), String> {
    let owner = reservation_owner(&ctx.session, &ctx.turn, &ctx.call_id);
    let budget = &ctx.turn.full_suite_budget;
    if !budget.reserved_for(&owner) && !budget.started_for(&owner) {
        return Ok(());
    }
    let _checkpoint = budget.checkpoint_guard().await;
    if budget.started_for(&owner) {
        return Err("Validation already crossed the possible-start boundary for this call; retain its result or resume its live process instead of relaunching.".into());
    }
    if !budget.reserved_for(&owner) {
        return Ok(());
    }
    budget.start(&owner, None)?;
    if let Err(error) = persist_policy_checkpoint(&ctx.session, &ctx.turn).await {
        budget.acknowledge_not_spawned(&owner);
        return Err(error);
    }
    Ok(())
}

#[expect(
    clippy::await_holding_invalid_type,
    reason = "The checkpoint guard serializes the budget transition with its durable publication"
)]
pub(crate) async fn policy_attempt_not_started(ctx: &ToolCtx) {
    let owner = reservation_owner(&ctx.session, &ctx.turn, &ctx.call_id);
    if !ctx.turn.full_suite_budget.started_for(&owner) {
        return;
    }
    let _checkpoint = ctx.turn.full_suite_budget.checkpoint_guard().await;
    ctx.turn.full_suite_budget.acknowledge_not_spawned(
        &owner,
    );
    let _ = persist_policy_checkpoint(&ctx.session, &ctx.turn).await;
}

async fn persist_policy_checkpoint(session: &Session, turn: &TurnContext) -> Result<(), String> {
    session.services.agent_control.persist_root_validation_checkpoint(turn).await?;
    session.persist_rollout_items_durable(&[
        codex_protocol::protocol::RolloutItem::TurnContext(turn.to_turn_context_item()),
    ]).await.map_err(|error| format!("validation admission checkpoint failed before launch: {error}"))
}
