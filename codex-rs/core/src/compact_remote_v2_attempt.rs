use std::sync::Arc;

use super::AsyncCompactionTraceContext as CompactionTraceContext;
use super::RemoteCompactionV2Output;
use super::prepare_v2_retained_input_source;
use super::run_remote_compaction_request_v2;
use crate::Prompt;
use crate::client::ModelClientSession;
use crate::compact::CompactionAnalyticsDetails;
use crate::compact_remote::trim_function_call_history_to_fit_context_window_for_prompt;
use crate::responses_metadata::CodexResponsesRequestKind;
use crate::responses_metadata::CompactionTurnMetadata;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::turn::build_projected_prompt;
use crate::session::turn::built_tools;
use crate::session::turn::prepare_sampling_prompt_with_workspace_identity;
use crate::session::turn::prompt_token_estimates;
use crate::session::turn::with_sampling_workspace_identity;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::TokenUsage;
use tokio_util::sync::CancellationToken;
use tracing::info;

pub(super) struct RemoteCompactV2Attempt {
    pub(super) trace_input_history: Option<Vec<ResponseItem>>,
    pub(super) retained_input: crate::compact::TaskInputCheckpoint,
    #[cfg(test)]
    pub(super) retention_input: Vec<ResponseItem>,
    pub(super) compaction_output: ResponseItem,
    pub(super) token_usage: Option<TokenUsage>,
    /// Keeps a session created for standalone compaction alive through lifecycle completion.
    pub(super) owned_client_session: Option<ModelClientSession>,
}

pub(super) async fn run_remote_compact_v2_attempt(
    sess: &Arc<Session>,
    step_context: &Arc<StepContext>,
    client_session: Option<&mut ModelClientSession>,
    compaction_trace: &CompactionTraceContext,
    compaction_metadata: CompactionTurnMetadata,
    analytics_details: &mut CompactionAnalyticsDetails,
    cancellation_token: &CancellationToken,
) -> CodexResult<RemoteCompactV2Attempt> {
    let turn_context = &step_context.turn;
    let mut history = sess.clone_history().await;
    let base_instructions = turn_context.base_instructions.as_ref().clone();
    let tool_router = built_tools(sess.as_ref(), step_context, &[], cancellation_token).await?;
    let mut owned_client_session = None;
    let client_session = match client_session {
        Some(client_session) => client_session,
        None => owned_client_session.insert(sess.services.model_client.new_session()),
    };
    let build_prompt = |prepared: &crate::context_manager::PreparedPromptInput| {
        let mut prompt = build_projected_prompt(
            sess.as_ref(),
            prepared,
            &tool_router,
            step_context.as_ref(),
            base_instructions.clone(),
        );
        prompt.output_schema = None;
        prompt.output_schema_strict = true;
        append_compaction_trigger(&mut prompt);
        prompt
    };
    // Only tool-search schemas can be rewritten by this trimmer. Size those
    // before observing the workspace: preliminary sizing does not authorize
    // source-evidence reuse and must not freeze an early workspace identity.
    let preliminary_deleted_tokens = if history
        .raw_items()
        .iter()
        .any(|item| matches!(item, ResponseItem::ToolSearchOutput { .. }))
    {
        let prepared = history
            .clone()
            .prepare_for_prompt(&turn_context.model_info.input_modalities);
        let sizing_prompt = build_prompt(&prepared);
        let measured_input = largest_compaction_request_input(&sizing_prompt);
        let tool_tokens =
            i64::try_from(sizing_prompt.tools.serialized().len().div_ceil(4)).unwrap_or(i64::MAX);
        let (rewritten_outputs, deleted_tokens) =
            trim_function_call_history_to_fit_context_window_for_prompt(
                &mut history,
                turn_context.as_ref(),
                &base_instructions,
                Some(&measured_input),
                tool_tokens,
            );
        if rewritten_outputs > 0 {
            info!(
                turn_id = %turn_context.sub_id,
                rewritten_outputs,
                "rewrote history outputs while sizing remote compaction v2"
            );
        }
        deleted_tokens
    } else {
        0
    };
    let (history, mut prompt, final_deleted_tokens) = with_sampling_workspace_identity(
        history,
        turn_context,
        sess.services.git_workspace.as_ref(),
        |mut history, workspace_identity| {
            let project = |history| {
                let prepared = prepare_sampling_prompt_with_workspace_identity(
                    history,
                    turn_context,
                    workspace_identity,
                    sess.services.git_workspace.as_ref(),
                );
                build_prompt(&prepared)
            };
            let mut prompt = project(history.clone());
            let measured_input = largest_compaction_request_input(&prompt);
            let tool_tokens =
                i64::try_from(prompt.tools.serialized().len().div_ceil(4)).unwrap_or(i64::MAX);
            let (rewritten_outputs, estimated_deleted_tokens) =
                trim_function_call_history_to_fit_context_window_for_prompt(
                    &mut history,
                    turn_context.as_ref(),
                    &base_instructions,
                    Some(&measured_input),
                    tool_tokens,
                );
            if rewritten_outputs > 0 {
                info!(
                    turn_id = %turn_context.sub_id,
                    rewritten_outputs,
                    "rewrote history outputs before remote compaction v2"
                );
                // Freshness notices can make the authoritative request larger
                // than the preliminary estimate. Recheck that exact projection;
                // only tool-search schemas change, with no intervening await.
                prompt = project(history.clone());
            }
            (history, prompt, estimated_deleted_tokens)
        },
    )
    .await;
    let estimated_deleted_tokens = preliminary_deleted_tokens.saturating_add(final_deleted_tokens);
    if estimated_deleted_tokens > 0 {
        let max_local_deleted_tokens = sess
            .estimated_tokens_after_last_model_generated_item()
            .await;
        analytics_details.active_context_tokens_before = analytics_details
            .active_context_tokens_before
            .map(|active_context_tokens_before| {
                active_context_tokens_before
                    .saturating_sub(estimated_deleted_tokens.min(max_local_deleted_tokens))
            });
    }

    // Trimming is best effort, not a fit receipt. Required text may still be
    // oversized; do not spend a compaction request that cannot fit either. The
    // byte estimate overstates recorded text-heavy prompts by 11-45 % (557,192
    // tokens for a measured 442,400), so refuse only when sampling admission's
    // provider-calibrated estimate does not fit either.
    let largest_input = largest_compaction_request_input(&prompt);
    if let Some(limit) = turn_context.model_context_window()
        && prompt.budget_tokens_for_input(&largest_input) >= limit
        && prompt_token_estimates(sess, turn_context, &prompt)
            .await
            .calibrated_tokens
            >= limit
    {
        return Err(codex_protocol::error::CodexErr::ContextWindowExceeded);
    }

    let trace_input_history = compaction_trace
        .is_enabled()
        .then(|| history.raw_items().to_vec());

    // Required recovery must be available before spending a provider request.
    // Use source IDs, not the lossy sampling projection or synthetic trigger.
    // Retention is awaited to completion: cancellation must not detach writes.
    let retained_input = prepare_v2_retained_input_source(sess, history.raw_items()).await?;
    {
        let current_history = sess.clone_history().await;
        let mut reference_items = current_history.raw_items().to_vec();
        reference_items.extend(retained_input.items.iter().cloned());
        if let Some(plan) = crate::compact::retained_plan_context(sess).await? {
            reference_items.push(plan);
        }
        // Installation rechecks the live plan and pins after the provider wait.
        // Existing content-addressed retention reuses unchanged recovery bytes.
        sess.compaction_artifact_pins(&current_history.tool_history_state(), &reference_items)
            .await?;
    }
    if cancellation_token.is_cancelled() {
        return Err(codex_protocol::error::CodexErr::TurnAborted);
    }

    let window_id = sess.current_window_id().await;
    let responses_metadata = turn_context.turn_metadata_state.to_responses_metadata(
        sess.installation_id.clone(),
        window_id,
        CodexResponsesRequestKind::Compaction(compaction_metadata),
    );
    prompt.stream_progress_timeout = Some(super::remote_compaction_progress_timeout(
        turn_context.provider.info(),
    ));
    let compaction_output_result = run_remote_compaction_request_v2(
        sess,
        turn_context.as_ref(),
        client_session,
        &prompt,
        &responses_metadata,
        compaction_trace,
        cancellation_token,
    )
    .await;
    let RemoteCompactionV2Output {
        compaction_output,
        token_usage,
    } = compaction_output_result?;
    Ok(RemoteCompactV2Attempt {
        trace_input_history,
        retained_input,
        #[cfg(test)]
        retention_input: history.into_raw_items(),
        compaction_output,
        token_usage,
        owned_client_session,
    })
}

fn largest_compaction_request_input(prompt: &Prompt) -> Arc<[ResponseItem]> {
    // Transport fallback still needs the largest complete representation, but
    // shared representations need only one estimate.
    let mut measured: Vec<(&Arc<[ResponseItem]>, i64)> = Vec::with_capacity(4);
    for input in [
        &prompt.input,
        &prompt.stable_context_fallback_input,
        &prompt.tool_history_fallback_input,
        &prompt.stable_context_tool_history_fallback_input,
    ] {
        let tokens = measured
            .iter()
            .find(|(previous, _)| Arc::ptr_eq(previous, input))
            .map(|(_, tokens)| *tokens)
            .unwrap_or_else(|| {
                input
                    .iter()
                    .map(crate::context_manager::estimate_item_token_count)
                    .fold(0_i64, i64::saturating_add)
            });
        measured.push((input, tokens));
    }
    measured
        .into_iter()
        .max_by_key(|(_, tokens)| *tokens)
        .map(|(input, _)| Arc::clone(input))
        .unwrap_or_else(|| Arc::clone(&prompt.input))
}

fn append_compaction_trigger(prompt: &mut Prompt) {
    prompt.prompt_provenance = prompt.prompt_provenance.with_appended_items(
        &[ResponseItem::CompactionTrigger {}],
        &prompt.stable_context_manifest,
    );
    // The suffix changes History even though the projected prefix is unchanged.
    prompt.digests.history = None;
    // Preserve sharing established by prompt projection. Pointer identity is
    // sufficient; do not compare large distinct transcripts for equality.
    type SharedInput = Arc<[ResponseItem]>;
    let mut appended: Vec<(SharedInput, SharedInput)> = Vec::with_capacity(4);
    for input in [
        &mut prompt.input,
        &mut prompt.stable_context_fallback_input,
        &mut prompt.tool_history_fallback_input,
        &mut prompt.stable_context_tool_history_fallback_input,
    ] {
        if let Some((_, replacement)) = appended
            .iter()
            .find(|(original, _)| Arc::ptr_eq(original, input))
        {
            *input = Arc::clone(replacement);
        } else {
            let mut items = input.to_vec();
            items.push(ResponseItem::CompactionTrigger {});
            let replacement: Arc<[ResponseItem]> = items.into();
            appended.push((Arc::clone(input), Arc::clone(&replacement)));
            *input = replacement;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn replace_source_preserving_metadata(
        path: &std::path::Path,
        text: &str,
    ) -> std::io::Result<()> {
        let before = std::fs::metadata(path)?;
        assert_eq!(before.len(), text.len() as u64);
        let modified = before.modified()?;
        std::fs::write(path, text)?;
        std::fs::File::options()
            .write(true)
            .open(path)?
            .set_times(std::fs::FileTimes::new().set_modified(modified))?;
        assert_eq!(std::fs::metadata(path)?.modified()?, modified);
        Ok(())
    }

    #[test_case::test_case(false, false, false; "no evidence without trimming")]
    #[test_case::test_case(false, true, false; "no evidence with trimming")]
    #[test_case::test_case(true, false, false; "digest evidence without trimming")]
    #[test_case::test_case(true, true, false; "digest evidence with trimming")]
    #[test_case::test_case(true, true, true; "source edit after sizing")]
    #[tokio::test]
    async fn remote_compaction_captures_workspace_once_per_request(
        evidence: bool,
        trim: bool,
        edit_after_sizing: bool,
    ) -> anyhow::Result<()> {
        use crate::session::turn::prepare_sampling_prompt_for_client;
        use crate::test_harness::responses;
        use crate::tool_history::WorkspaceEvidenceObservation;
        use codex_analytics::CompactionImplementation;
        use codex_analytics::CompactionPhase;
        use codex_analytics::CompactionReason;
        use codex_analytics::CompactionTrigger;
        use codex_protocol::models::FunctionCallOutputPayload;

        crate::test_harness::require_network!();
        let server = responses::start_mock_server().await;
        let home = tempfile::tempdir()?;
        let workspace = tempfile::tempdir()?;
        assert!(
            std::process::Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(workspace.path())
                .status()?
                .success()
        );
        let source = workspace.path().join("source.txt");
        std::fs::write(&source, "original evidence")?;
        let cwd = codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(workspace.path())?;
        let (session, mut turn, _events) =
            crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
                codex_login::CodexAuth::from_api_key("test"),
                Vec::new(),
                home.path(),
                |config| {
                    config.cwd = cwd;
                    config.model_provider.base_url = Some(format!("{}/v1", server.uri()));
                    config.model_provider.supports_websockets = false;
                },
            )
            .await;
        let turn_mut = Arc::get_mut(&mut turn).unwrap();
        turn_mut.model_info.context_window = Some(32_000);
        turn_mut.model_info.effective_context_window_percent = 100;
        let mut items = Vec::new();
        if evidence {
            let output = ResponseItem::FunctionCallOutput {
                id: None,
                call_id: "read-source".into(),
                output: FunctionCallOutputPayload::from_text("original evidence".into()),
                internal_chat_message_metadata_passthrough: None,
            };
            let revision = session
                .services
                .git_workspace
                .workspace_evidence_for_turn(&turn, workspace.path())
                .await
                .identity;
            assert!(
                revision
                    .as_ref()
                    .is_some_and(|revision| !revision.unavailable)
            );
            let observation = WorkspaceEvidenceObservation::from_response_item(
                revision,
                &output,
                Default::default(),
            )
            .unwrap();
            session.register_workspace_evidence(observation, ()).await;
            items.push(ResponseItem::FunctionCall {
                id: None,
                name: "read_file".into(),
                namespace: None,
                arguments: r#"{"path":"source.txt"}"#.into(),
                call_id: "read-source".into(),
                internal_chat_message_metadata_passthrough: None,
            });
            items.push(output);
        }
        items.extend([
            ResponseItem::ToolSearchCall {
                id: None,
                call_id: Some("search".into()),
                status: Some("completed".into()),
                execution: "client".into(),
                arguments: serde_json::json!({"query":"calendar", "namespace":"apps"}),
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::ToolSearchOutput {
                id: None,
                call_id: Some("search".into()),
                status: "completed".into(),
                execution: "client".into(),
                tools: vec![serde_json::json!({
                    "namespace":"apps", "name":"calendar.search",
                    "description": if trim { "schema detail ".repeat(30_000) } else { "small".into() },
                })],
                omitted_result_count: Some(0),
                internal_chat_message_metadata_passthrough: None,
            },
        ]);
        // Preserve the oversized input until the actual compaction owner trims it.
        session.replace_history(items, None).await;
        let original = session.clone_history().await;
        assert_eq!(original.requires_workspace_evidence_validation(), evidence);
        assert!(!original.can_use_root_only_workspace_identity());
        let step = session.capture_step_context(Arc::clone(&turn)).await?;
        let log = responses::mount_sse_once(
            &server,
            responses::sse(vec![
                serde_json::json!({"type":"response.output_item.done",
                    "item":{"type":"compaction","encrypted_content":"opaque"}}),
                responses::ev_completed("compacted"),
            ]),
        )
        .await;
        let captures_before = session
            .services
            .git_workspace
            .workspace_evidence_capture_count();
        let source_edit = edit_after_sizing.then(|| {
            let pause = session
                .services
                .git_workspace
                .pause_next_workspace_evidence_capture();
            let source = source.clone();
            tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async move {
                tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    pause.wait_until_started(),
                )
                .await?;
                let result = replace_source_preserving_metadata(&source, "modified evidence");
                pause.release();
                result.map_err(anyhow::Error::from)
            }))
        });
        let result = run_remote_compact_v2_attempt(
            &session,
            &step,
            None,
            &CompactionTraceContext::disabled(),
            CompactionTurnMetadata::new(
                CompactionTrigger::Manual,
                CompactionReason::UserRequested,
                CompactionImplementation::ResponsesCompactionV2,
                CompactionPhase::StandaloneTurn,
            ),
            &mut CompactionAnalyticsDetails::default(),
            &CancellationToken::new(),
        )
        .await?;
        if let Some(source_edit) = source_edit {
            source_edit.await??;
        }
        assert_eq!(
            session
                .services
                .git_workspace
                .workspace_evidence_capture_count()
                - captures_before,
            u64::from(evidence),
            "sizing and trimming one request must share one observation"
        );
        let search = result
            .retention_input
            .iter()
            .find_map(|item| match item {
                ResponseItem::ToolSearchOutput { tools, .. } => Some(tools),
                _ => None,
            })
            .expect("retained search output");
        assert_eq!(search[0].get("description").is_none(), trim);
        let mut rewritten = original.clone();
        rewritten.replace(result.retention_input.clone());
        assert_eq!(rewritten.requires_workspace_evidence_validation(), evidence);
        assert!(!rewritten.can_use_root_only_workspace_identity());
        assert!(matches!(
            result.compaction_output,
            ResponseItem::Compaction { .. }
        ));
        assert!(
            !log.single_request()
                .body_json()
                .to_string()
                .contains("schema detail")
        );
        let request = log.single_request();
        let notices = request
            .input()
            .iter()
            .filter(|item| item["role"] == "developer")
            .flat_map(|item| item["content"].as_array().into_iter().flatten())
            .filter_map(|part| part["text"].as_str())
            .filter(|text| text.starts_with("<workspace_evidence_invalidation>\n"))
            .flat_map(str::lines)
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .flat_map(crate::tool_history::expand_workspace_notices)
            .filter(|notice| notice["call_id"] == "read-source")
            .collect::<Vec<_>>();
        assert_eq!(notices.len(), usize::from(evidence));
        if evidence {
            // Legacy digest-only provenance is unverified even without an edit.
            // The final capture must distinguish that from an actual change
            // made after sizing, without upgrading either to current proof.
            assert_eq!(notices[0]["valid_for_current_workspace"], false);
            assert_eq!(
                notices[0]["reason_code"],
                if edit_after_sizing {
                    "workspace_identity_changed"
                } else {
                    "workspace_freshness_unverified"
                }
            );
        }
        session.flush_tool_history_persistence().await?;

        if evidence {
            // A later request must capture again, including equal-length edits.
            replace_source_preserving_metadata(&source, "changed! evidence")?;
            let next = prepare_sampling_prompt_for_client(
                original,
                &turn,
                session.services.git_workspace.as_ref(),
            )
            .await;
            assert_eq!(
                session
                    .services
                    .git_workspace
                    .workspace_evidence_capture_count()
                    - captures_before,
                2
            );
            assert!(next.items().iter().any(|item| matches!(item,
                ResponseItem::Message { content, .. } if content.iter().any(|content| matches!(content,
                    codex_protocol::models::ContentItem::InputText { text }
                        if text.contains("<workspace_evidence_invalidation>"))))));
        }
        Ok(())
    }

    #[tokio::test]
    async fn remote_compaction_sends_the_selected_turn_base() -> anyhow::Result<()> {
        use crate::test_harness::responses;
        use codex_analytics::CompactionImplementation;
        use codex_analytics::CompactionPhase;
        use codex_analytics::CompactionReason;
        use codex_analytics::CompactionTrigger;
        crate::test_harness::require_network!();
        let server = responses::start_mock_server().await;
        let home = tempfile::tempdir()?;
        let (session, mut turn, _events) =
            crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
                codex_login::CodexAuth::from_api_key("test"),
                Vec::new(),
                home.path(),
                |config| {
                    config.model_provider.base_url = Some(format!("{}/v1", server.uri()));
                    config.model_provider.supports_websockets = false;
                },
            )
            .await;
        let selected_base = "SELECTED_COMPACTION_TURN_BASE";
        Arc::get_mut(&mut turn).unwrap().base_instructions =
            Arc::new(codex_protocol::models::BaseInstructions {
                text: selected_base.into(),
            });
        assert_ne!(session.get_base_instructions().await.text, selected_base);
        let step = session.capture_step_context(Arc::clone(&turn)).await?;
        let log = responses::mount_sse_once(
            &server,
            responses::sse(vec![
                serde_json::json!({"type":"response.output_item.done",
                "item":{"type":"compaction","encrypted_content":"opaque"}}),
                responses::ev_completed("compacted"),
            ]),
        )
        .await;
        turn.turn_timing_state.mark_turn_started();
        run_remote_compact_v2_attempt(
            &session,
            &step,
            None,
            &CompactionTraceContext::disabled(),
            CompactionTurnMetadata::new(
                CompactionTrigger::Manual,
                CompactionReason::UserRequested,
                CompactionImplementation::ResponsesCompactionV2,
                CompactionPhase::StandaloneTurn,
            ),
            &mut CompactionAnalyticsDetails::default(),
            &CancellationToken::new(),
        )
        .await?;
        assert_eq!(
            log.single_request().body_json()["instructions"],
            selected_base
        );
        let timing = turn.turn_timing_state.complete_snapshot().protocol_timing();
        let request = timing
            .model_requests
            .last()
            .expect("standalone compaction request");
        assert!(request.dispatch_ms.is_some());
        assert_eq!(request.physical_attempt_ids.len(), 1);
        assert_eq!(timing.counters.model_request_count, 1);
        assert_eq!(timing.milestones.first_visible_output_ms, None);
        Ok(())
    }

    /// e6b3: the byte estimate put a prompt the provider measured at 442,400
    /// tokens at 557,192, so the fit check refused a compaction request that
    /// fit the 475,000-token window and the turn compacted locally instead.
    #[tokio::test]
    async fn remote_compaction_fit_check_uses_the_provider_calibrated_size() -> anyhow::Result<()> {
        use crate::client_common::ResponseEvent;
        use crate::session::turn::prepare_sampling_prompt_for_client;
        use crate::test_harness::responses;
        use codex_analytics::CompactionImplementation;
        use codex_analytics::CompactionPhase;
        use codex_analytics::CompactionReason;
        use codex_analytics::CompactionTrigger;
        use codex_protocol::error::CodexErr;
        use codex_protocol::models::ContentItem;

        const WINDOW: i64 = 40_000;
        crate::test_harness::require_network!();
        let server = responses::start_mock_server().await;
        let home = tempfile::tempdir()?;
        let (session, mut turn, _events) =
            crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
                codex_login::CodexAuth::from_api_key("test"),
                Vec::new(),
                home.path(),
                |config| {
                    config.model_provider.base_url = Some(format!("{}/v1", server.uri()));
                    config.model_provider.supports_websockets = false;
                },
            )
            .await;
        let turn_mut = Arc::get_mut(&mut turn).unwrap();
        turn_mut.model_info.context_window = Some(WINDOW);
        turn_mut.model_info.max_context_window = None;
        turn_mut.model_info.effective_context_window_percent = 100;
        let message = |role: &str, text: String| ResponseItem::Message {
            id: None,
            role: role.into(),
            content: vec![ContentItem::InputText { text }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        };
        // The checkpoint keeps no developer context, so only the fit check
        // decides whether the request is spent.
        session
            .replace_history(
                vec![
                    message("user", "compact this history".into()),
                    message("developer", "x".repeat(4 * WINDOW as usize)),
                ],
                None,
            )
            .await;
        let step = session.capture_step_context(Arc::clone(&turn)).await?;
        let metadata = CompactionTurnMetadata::new(
            CompactionTrigger::Auto,
            CompactionReason::ContextLimit,
            CompactionImplementation::ResponsesCompactionV2,
            CompactionPhase::MidTurn,
        );
        let log = responses::mount_sse_once(
            &server,
            responses::sse(vec![
                serde_json::json!({"type":"response.output_item.done",
                "item":{"type":"compaction","encrypted_content":"opaque"}}),
                responses::ev_completed("compacted"),
            ]),
        )
        .await;

        // With no provider measurement, the byte estimate alone cannot fit.
        let refused = run_remote_compact_v2_attempt(
            &session,
            &step,
            None,
            &CompactionTraceContext::disabled(),
            metadata,
            &mut CompactionAnalyticsDetails::default(),
            &CancellationToken::new(),
        )
        .await;
        assert!(
            matches!(refused, Err(CodexErr::ContextWindowExceeded)),
            "an unmeasured oversized prompt must be refused locally"
        );
        assert!(log.requests().is_empty());

        // The previous sampling request sent this prompt, and the provider
        // measured it at half the byte estimate.
        let router = built_tools(session.as_ref(), &step, &[], &CancellationToken::new()).await?;
        let prepared = prepare_sampling_prompt_for_client(
            session.clone_history().await,
            &turn,
            session.services.git_workspace.as_ref(),
        )
        .await;
        let sampled = build_projected_prompt(
            session.as_ref(),
            &prepared,
            &router,
            step.as_ref(),
            turn.base_instructions.as_ref().clone(),
        );
        let basis = sampled.calibration_basis(Arc::clone(&sampled.input));
        assert!(basis.local_tokens >= WINDOW, "{}", basis.local_tokens);
        let timing = &turn.turn_timing_state;
        timing.mark_turn_started();
        drop(timing.begin_model_request_wait());
        timing.record_model_attempt_identity("sampled", "physical");
        let model = serde_json::json!({"model": turn.model_info.slug}).to_string();
        timing.record_model_request_payload("sampled", "physical", model.as_bytes());
        timing.record_model_request_sections("sampled", "physical", model.as_bytes());
        timing.record_model_request_token_categories(
            "sampled",
            "physical",
            codex_protocol::protocol::TurnTimingRequestTokenCategories {
                local_input_estimate: u64::try_from(basis.local_tokens)?,
                ..Default::default()
            },
        );
        let usage = TokenUsage {
            input_tokens: basis.local_tokens / 2,
            total_tokens: basis.local_tokens / 2,
            ..Default::default()
        };
        timing.record_generation_token_usage(Some(&usage));
        timing.record_response_event_milestones(&ResponseEvent::Completed {
            response_id: "sampled".to_string(),
            token_usage: Some(usage.clone()),
            end_turn: Some(true),
        });
        turn.extension_data.insert(basis);
        session.record_token_usage_info(&turn, Some(&usage)).await?;

        let attempt = run_remote_compact_v2_attempt(
            &session,
            &step,
            None,
            &CompactionTraceContext::disabled(),
            metadata,
            &mut CompactionAnalyticsDetails::default(),
            &CancellationToken::new(),
        )
        .await?;
        assert!(matches!(
            attempt.compaction_output,
            ResponseItem::Compaction { .. }
        ));
        assert_eq!(log.requests().len(), 1);
        Ok(())
    }

    #[test]
    fn compaction_variants_preserve_sharing_and_measure_distinct_fallbacks() {
        let small: Arc<[ResponseItem]> = vec![ResponseItem::Compaction {
            id: None,
            encrypted_content: "x".repeat(1_000),
            internal_chat_message_metadata_passthrough: None,
        }]
        .into();
        let large: Arc<[ResponseItem]> = vec![ResponseItem::Compaction {
            id: None,
            encrypted_content: "y".repeat(10_000),
            internal_chat_message_metadata_passthrough: None,
        }]
        .into();
        let mut prompt = Prompt {
            input: Arc::clone(&small),
            stable_context_fallback_input: Arc::clone(&small),
            tool_history_fallback_input: Arc::clone(&large),
            stable_context_tool_history_fallback_input: Arc::clone(&large),
            ..Default::default()
        };
        append_compaction_trigger(&mut prompt);
        assert_eq!(small.len(), 1);
        assert_eq!(large.len(), 1);
        assert!(Arc::ptr_eq(
            &prompt.input,
            &prompt.stable_context_fallback_input
        ));
        assert!(Arc::ptr_eq(
            &prompt.tool_history_fallback_input,
            &prompt.stable_context_tool_history_fallback_input
        ));
        assert!(!Arc::ptr_eq(
            &prompt.input,
            &prompt.tool_history_fallback_input
        ));
        for input in [
            &prompt.input,
            &prompt.stable_context_fallback_input,
            &prompt.tool_history_fallback_input,
            &prompt.stable_context_tool_history_fallback_input,
        ] {
            assert_eq!(input.len(), 2);
            assert!(matches!(
                input.last(),
                Some(ResponseItem::CompactionTrigger {})
            ));
        }
        assert!(Arc::ptr_eq(
            &largest_compaction_request_input(&prompt),
            &prompt.stable_context_tool_history_fallback_input
        ));
        for largest_index in 0..4 {
            let inputs: [Arc<[ResponseItem]>; 4] = std::array::from_fn(|index| {
                Arc::clone(if index == largest_index {
                    &large
                } else {
                    &small
                })
            });
            let [
                input,
                stable_context_fallback_input,
                tool_history_fallback_input,
                stable_context_tool_history_fallback_input,
            ] = inputs;
            let prompt = Prompt {
                input,
                stable_context_fallback_input,
                tool_history_fallback_input,
                stable_context_tool_history_fallback_input,
                ..Default::default()
            };
            assert!(Arc::ptr_eq(
                &largest_compaction_request_input(&prompt),
                &large
            ));
        }
    }

    #[test]
    fn compaction_trigger_preserves_existing_provenance() {
        use crate::context::PromptContextBreakdown;
        use crate::context::PromptContextCategory;
        use crate::context::PromptProvenanceSidecar;
        use codex_protocol::models::ContentItem;

        let message = |role: &str, text: &str| ResponseItem::Message {
            id: None,
            role: role.into(),
            content: vec![ContentItem::InputText { text: text.into() }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        };
        // Identical prose has different occurrence-level origins.
        let items: Arc<[ResponseItem]> = vec![
            message("developer", "same text"),
            message("user", "same text"),
        ]
        .into();
        let mut prompt = Prompt {
            input: Arc::clone(&items),
            stable_context_fallback_input: Arc::clone(&items),
            tool_history_fallback_input: Arc::clone(&items),
            stable_context_tool_history_fallback_input: Arc::clone(&items),
            ..Default::default()
        };
        prompt.prompt_provenance =
            PromptProvenanceSidecar::from_assembled_items(&items, &prompt.stable_context_manifest)
                .with_exact_fragment(&items, "same text", PromptContextCategory::Repository);
        prompt.digests.history = Some([1; 32]);
        let repo_bytes = serde_json::to_vec(&items[0]).unwrap().len() as u64;
        let task_bytes = serde_json::to_vec(&items[1]).unwrap().len() as u64;
        append_compaction_trigger(&mut prompt);
        assert_eq!(prompt.digests.history, None);
        for input in [
            &prompt.input,
            &prompt.stable_context_fallback_input,
            &prompt.tool_history_fallback_input,
            &prompt.stable_context_tool_history_fallback_input,
        ] {
            let encoded = serde_json::to_string(input.as_ref()).unwrap();
            let raw: Vec<&serde_json::value::RawValue> = serde_json::from_str(&encoded).unwrap();
            let measured = PromptContextBreakdown::from_serialized_response_items(
                input,
                &raw,
                &prompt.prompt_provenance,
                None,
            )
            .unwrap();
            assert_eq!(
                measured.bytes(PromptContextCategory::Repository),
                repo_bytes
            );
            assert_eq!(measured.bytes(PromptContextCategory::TaskInput), task_bytes);
            assert_eq!(
                measured.bytes(PromptContextCategory::History),
                serde_json::to_vec(&ResponseItem::CompactionTrigger {})
                    .unwrap()
                    .len() as u64
            );
            assert_eq!(measured.total_bytes(), encoded.len() as u64);
            assert_eq!(&input[..2], items.as_ref());
        }
    }

    #[test]
    #[ignore = "manual local microbenchmark; timing is descriptive"]
    #[expect(
        clippy::print_stderr,
        reason = "This opt-in benchmark reports timing samples and their median"
    )]
    fn benchmark_compaction_shared_variants() {
        let input: Arc<[ResponseItem]> = vec![ResponseItem::Compaction {
            id: None,
            encrypted_content: "x".repeat(2_000_000),
            internal_chat_message_metadata_passthrough: None,
        }]
        .into();
        let mut samples = Vec::new();
        for _ in 0..25 {
            let mut prompt = Prompt {
                input: Arc::clone(&input),
                stable_context_fallback_input: Arc::clone(&input),
                tool_history_fallback_input: Arc::clone(&input),
                stable_context_tool_history_fallback_input: Arc::clone(&input),
                ..Default::default()
            };
            let started = std::time::Instant::now();
            append_compaction_trigger(&mut prompt);
            std::hint::black_box(largest_compaction_request_input(&prompt));
            samples.push(started.elapsed().as_micros());
        }
        samples.sort_unstable();
        eprintln!(
            "compaction_shared_variants_us={samples:?} median={}",
            samples[12]
        );
        if let Some(directory) = std::env::var_os("COMPACTION_BENCHMARK_DIR") {
            std::fs::write(
                std::path::PathBuf::from(directory).join("shared-variants.txt"),
                format!("microseconds={samples:?}\nmedian={}\n", samples[12]),
            )
            .unwrap();
        }
    }
}
