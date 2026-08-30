use super::*;
use crate::session::reasoning_governor::AuthoritativeWaitOwnerResult;
use crate::state::TaskKind;
use crate::tasks::SessionTask;
use crate::tasks::SessionTaskResult;
use crate::tools::exposure::AgentSurfaceStage;
use crate::tools::exposure::EnvironmentSurfaceMode;
use crate::tools::exposure::GoalSurfaceState;
use crate::tools::exposure::ToolExposureIdentity;
use crate::tools::registry::ToolRegistry;
use crate::tools::router::ToolRouter;
use anyhow::Result;
use codex_config::types::McpServerConfig;
use codex_config::types::McpServerTransportConfig;
use codex_extension_api::ExtensionData;
use codex_extension_api::TurnInputContributor;
use codex_extension_api::TurnItemContributor;
use codex_features::Feature;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_mcp::CODEX_APPS_MCP_SERVER_NAME;
use codex_mcp::ToolInfo;
use codex_model_provider_info::ModelProviderInfo;
use codex_model_provider_info::built_in_model_providers;
use codex_protocol::AgentPath;
use codex_protocol::ResponseItemId;
use codex_protocol::items::AgentMessageContent;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::PermissionProfile;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::openai_models::ModelsResponse;
use codex_protocol::protocol::AdditionalContextEntry;
use codex_protocol::protocol::AdditionalContextKind;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::user_input::UserInput;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_path_uri::PathUri;
use core_test_support::hooks::trust_discovered_hooks;
use core_test_support::responses;
use core_test_support::test_codex::test_codex;
use indexmap::IndexMap;
use pretty_assertions::assert_eq;
use rmcp::model::JsonObject;
use rmcp::model::Tool;
use std::collections::HashMap;
use std::fs;
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use wiremock::Mock;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

#[derive(Debug)]
struct ContendedCatalogModelsManager {
    inner: codex_models_manager::manager::SharedModelsManager,
    catalog_gate: Arc<tokio::sync::RwLock<()>>,
    read_attempted: Arc<std::sync::atomic::AtomicBool>,
}

impl codex_models_manager::manager::ModelsManager for ContendedCatalogModelsManager {
    fn model_catalog_activity(&self) -> Arc<codex_models_manager::manager::ModelCatalogActivity> {
        self.inner.model_catalog_activity()
    }

    fn refresh_models_for_background(
        &self,
        http_client_factory: codex_http_client::HttpClientFactory,
    ) -> codex_models_manager::manager::ModelsManagerFuture<'_, codex_protocol::error::Result<()>>
    {
        self.inner
            .refresh_models_for_background(http_client_factory)
    }

    fn list_models_shared(
        &self,
        refresh_strategy: codex_models_manager::manager::RefreshStrategy,
        http_client_factory: codex_http_client::HttpClientFactory,
    ) -> codex_models_manager::manager::ModelsManagerFuture<
        '_,
        codex_protocol::error::Result<Arc<Vec<codex_protocol::openai_models::ModelPreset>>>,
    > {
        let inner = Arc::clone(&self.inner);
        let catalog_gate = Arc::clone(&self.catalog_gate);
        let read_attempted = Arc::clone(&self.read_attempted);
        Box::pin(async move {
            read_attempted.store(true, Ordering::Release);
            let catalog_read = catalog_gate.read().await;
            drop(catalog_read);
            inner
                .list_models_shared(refresh_strategy, http_client_factory)
                .await
        })
    }

    fn raw_model_catalog(
        &self,
        refresh_strategy: codex_models_manager::manager::RefreshStrategy,
        http_client_factory: codex_http_client::HttpClientFactory,
    ) -> codex_models_manager::manager::ModelsManagerFuture<
        '_,
        codex_protocol::error::Result<ModelsResponse>,
    > {
        self.inner
            .raw_model_catalog(refresh_strategy, http_client_factory)
    }

    fn get_remote_models(
        &self,
    ) -> codex_models_manager::manager::ModelsManagerFuture<
        '_,
        Vec<codex_protocol::openai_models::ModelInfo>,
    > {
        self.inner.get_remote_models()
    }

    fn try_get_remote_models(
        &self,
    ) -> std::result::Result<Vec<codex_protocol::openai_models::ModelInfo>, tokio::sync::TryLockError>
    {
        self.inner.try_get_remote_models()
    }

    fn auth_manager(&self) -> Option<&AuthManager> {
        self.inner.auth_manager()
    }

    fn list_collaboration_modes(&self) -> Vec<codex_protocol::config_types::CollaborationModeMask> {
        self.inner.list_collaboration_modes()
    }

    fn try_list_models_shared(
        &self,
    ) -> std::result::Result<
        Arc<Vec<codex_protocol::openai_models::ModelPreset>>,
        tokio::sync::TryLockError,
    > {
        self.read_attempted.store(true, Ordering::Release);
        let _catalog_read = self.catalog_gate.try_read()?;
        self.inner.try_list_models_shared()
    }

    fn notify_etag(
        self: Arc<Self>,
        etag: String,
        http_client_factory: codex_http_client::HttpClientFactory,
    ) -> codex_models_manager::manager::ModelsManagerFuture<'static, ()> {
        Arc::clone(&self.inner).notify_etag(etag, http_client_factory)
    }
}

#[test]
fn turn_submission_type_distinguishes_queued_continuations() {
    assert!(matches!(
        turn_submission_type(&[]),
        TurnSubmissionType::Queued
    ));

    let input = [TurnInput::UserInput {
        content: Vec::new(),
        client_id: None,
    }];
    assert!(matches!(
        turn_submission_type(&input),
        TurnSubmissionType::Default
    ));
}

#[tokio::test]
async fn stopped_session_start_restores_input_and_requests_a_fresh_turn() {
    let (session, _) = crate::session::tests::make_session_and_context().await;
    let input = vec![TurnInput::UserInput {
        content: vec![UserInput::Text {
            text: "retry after session-start stop".to_string(),
            text_elements: Vec::new(),
        }],
        client_id: None,
    }];

    let result = finish_stopped_session_start(&session, input.clone()).await;

    assert!(result.defer_pending_input);
    assert_eq!(
        session
            .input_queue
            .get_pending_input(&session.active_turn)
            .await,
        input
    );
}

#[tokio::test]
async fn consecutive_turn_contexts_share_the_unchanged_picker_snapshot() {
    let (session, first_turn) = crate::session::tests::make_session_and_context().await;

    let second_turn = session
        .new_default_turn_with_sub_id("shared-picker-snapshot".to_string())
        .await;

    assert!(Arc::ptr_eq(
        &first_turn.available_models,
        &second_turn.available_models
    ));
}

#[tokio::test]
// This test intentionally keeps the writer guard while awaiting the blocked reader.
#[allow(clippy::await_holding_invalid_type, clippy::await_holding_lock)]
async fn turn_context_waits_for_a_contended_picker_snapshot() {
    let (mut session, first_turn) = crate::session::tests::make_session_and_context().await;
    let expected_models = Arc::clone(&first_turn.available_models);
    let catalog_gate = Arc::new(tokio::sync::RwLock::new(()));
    let read_attempted = Arc::new(std::sync::atomic::AtomicBool::new(false));
    session.services.models_manager = Arc::new(ContendedCatalogModelsManager {
        inner: Arc::clone(&session.services.models_manager),
        catalog_gate: Arc::clone(&catalog_gate),
        read_attempted: Arc::clone(&read_attempted),
    });
    let catalog_write = catalog_gate.write().await;
    let session = Arc::new(session);
    let mut next_turn = tokio::spawn({
        let session = Arc::clone(&session);
        async move {
            session
                .new_default_turn_with_sub_id("contended-picker-snapshot".to_string())
                .await
        }
    });

    tokio::time::timeout(Duration::from_secs(1), async {
        while !read_attempted.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("turn construction should attempt to read the picker catalog");
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut next_turn)
            .await
            .is_err(),
        "turn construction must wait for a contended catalog instead of publishing an empty one"
    );

    drop(catalog_write);
    let next_turn = tokio::time::timeout(Duration::from_secs(1), next_turn)
        .await
        .expect("turn construction should resume after the catalog writer exits")
        .expect("turn construction task should succeed");
    assert_eq!(
        next_turn.available_models.as_ref(),
        expected_models.as_ref()
    );
}

#[test]
fn projected_prompt_compaction_reuses_equal_and_incrementally_appended_projections() {
    fn message(text: &str) -> ResponseItem {
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: text.to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }
    }

    fn tool_search_output() -> ResponseItem {
        ResponseItem::ToolSearchOutput {
            id: None,
            call_id: None,
            status: "completed".to_string(),
            execution: "server".to_string(),
            tools: vec![serde_json::json!({
                "type": "function",
                "name": "search_repository",
                "description": "large acknowledged schema",
                "parameters": {"type": "object"}
            })],
            omitted_result_count: None,
            internal_chat_message_metadata_passthrough: None,
        }
    }

    fn prepare(items: Vec<ResponseItem>) -> PreparedPromptInput {
        let mut history = ContextManager::new();
        history.record_items(
            items.iter(),
            codex_utils_output_truncation::TruncationPolicy::Tokens(10_000),
        );
        history.prepare_for_sampling_prompt(&[], StableContextTarget::Sampling)
    }

    fn assert_matches_independent_compaction(
        prepared: &PreparedPromptInput,
        compacted: &CompactedProjectedPromptInputs,
    ) {
        assert_eq!(
            compacted.input,
            compact_acknowledged_tool_search_outputs(prepared.shared_items())
        );
        assert_eq!(
            compacted.stable_context_fallback_input,
            compact_acknowledged_tool_search_outputs(prepared.shared_fallback_items())
        );
        assert_eq!(
            compacted.tool_history_fallback_input,
            compact_acknowledged_tool_search_outputs(prepared.shared_unreplaced_items())
        );
        assert_eq!(
            compacted.stable_context_tool_history_fallback_input,
            compact_acknowledged_tool_search_outputs(prepared.shared_unreplaced_fallback_items())
        );
    }

    let equal_prepared = prepare(vec![tool_search_output(), message("continue")]);
    let equal = compact_projected_prompt_inputs(&equal_prepared);
    assert_eq!(equal.pass_count, 1);
    assert!(Arc::ptr_eq(
        &equal.input,
        &equal.stable_context_fallback_input
    ));
    assert!(Arc::ptr_eq(
        &equal.input,
        &equal.tool_history_fallback_input
    ));
    assert!(Arc::ptr_eq(
        &equal.input,
        &equal.stable_context_tool_history_fallback_input
    ));
    let ResponseItem::ToolSearchOutput { tools, .. } = &equal.input[0] else {
        panic!("expected historical tool-search output");
    };
    assert_eq!(
        tools,
        &[serde_json::json!({
            "type": "function",
            "name": "search_repository"
        })]
    );
    let equal_reused = compact_projected_prompt_inputs(&equal_prepared);
    assert!(
        Arc::ptr_eq(&equal.input, &equal_reused.input),
        "later generations must reuse the compacted projection",
    );
    assert_matches_independent_compaction(&equal_prepared, &equal);

    let old_repository =
        "# AGENTS.md instructions for /repo\n\n<INSTRUCTIONS>\nold\n</INSTRUCTIONS>";
    let current_repository =
        "# AGENTS.md instructions for /repo\n\n<INSTRUCTIONS>\ncurrent\n</INSTRUCTIONS>";
    let split_prepared = prepare(vec![
        message(old_repository),
        tool_search_output(),
        message("continue"),
        message(current_repository),
    ]);
    let split = compact_projected_prompt_inputs(&split_prepared);
    assert_eq!(split.pass_count, 2);
    assert!(Arc::ptr_eq(
        &split.input,
        &split.tool_history_fallback_input
    ));
    assert!(Arc::ptr_eq(
        &split.stable_context_fallback_input,
        &split.stable_context_tool_history_fallback_input
    ));
    assert!(!Arc::ptr_eq(
        &split.input,
        &split.stable_context_fallback_input
    ));
    assert_ne!(split.input, split.stable_context_fallback_input);
    assert_matches_independent_compaction(&split_prepared, &split);

    let mut advancing_history = ContextManager::new();
    let initial_items = [message("before"), tool_search_output()];
    advancing_history.record_items(initial_items.iter(), TruncationPolicy::Tokens(10_000));
    let before_append = advancing_history
        .clone()
        .prepare_for_sampling_prompt(&[], StableContextTarget::Sampling);
    let before_append_compacted = compact_projected_prompt_inputs(&before_append);
    let ResponseItem::ToolSearchOutput { tools, .. } = &before_append_compacted.input[1] else {
        panic!("expected final tool-search output");
    };
    assert_eq!(tools[0]["description"], "large acknowledged schema");

    let reasoning = ResponseItem::Reasoning {
        id: None,
        summary: Vec::new(),
        content: None,
        encrypted_content: None,
        internal_chat_message_metadata_passthrough: None,
    };
    advancing_history.record_items([&reasoning], TruncationPolicy::Tokens(10_000));
    let after_append = advancing_history
        .clone()
        .prepare_for_sampling_prompt(&[], StableContextTarget::Sampling);
    assert_eq!(
        after_append.compacted_tool_search_outputs_are_materialized(),
        Some([false; 4]),
        "a safe append must advance the cached model projections without flattening them",
    );

    let after_append_compacted = compact_projected_prompt_inputs(&after_append);
    assert_eq!(after_append_compacted.pass_count, 1);
    let ResponseItem::ToolSearchOutput { tools, .. } = &after_append_compacted.input[1] else {
        panic!("expected historical tool-search output");
    };
    assert_eq!(
        tools,
        &[serde_json::json!({
            "type": "function",
            "name": "search_repository"
        })]
    );
    assert_eq!(&after_append_compacted.input[2], &reasoning);
    assert_matches_independent_compaction(&after_append, &after_append_compacted);
}

#[test]
fn arc_identity_short_circuits_equivalence_check() {
    let comparison_count = AtomicUsize::new(0);
    let shared = Arc::new([1_u8, 2, 3]);
    let shared_alias = Arc::clone(&shared);

    assert!(arc_identity_or_equivalent(
        &shared,
        &shared_alias,
        |left, right| {
            comparison_count.fetch_add(1, Ordering::Relaxed);
            left == right
        }
    ));
    assert_eq!(comparison_count.load(Ordering::Relaxed), 0);

    let equal_but_distinct = Arc::new([1_u8, 2, 3]);
    assert!(arc_identity_or_equivalent(
        &shared,
        &equal_but_distinct,
        |left, right| {
            comparison_count.fetch_add(1, Ordering::Relaxed);
            left == right
        }
    ));
    assert_eq!(comparison_count.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn request_scaffold_reuses_stable_preparation_and_invalidates_only_owner_changes() {
    let (session, turn_context) = crate::session::tests::make_session_and_context().await;
    let session = Arc::new(session);
    let turn_context = Arc::new(turn_context);
    let step_context = session
        .capture_step_context(Arc::clone(&turn_context))
        .await;
    let equivalent_turn_context = session
        .new_default_turn_with_sub_id("equivalent-scaffold-owner".to_string())
        .await;
    assert!(
        Arc::ptr_eq(&turn_context.config, &equivalent_turn_context.config),
        "value-equivalent consecutive turns should share the interned config allocation"
    );
    assert_eq!(
        turn_context.config.as_ref(),
        equivalent_turn_context.config.as_ref(),
        "the cross-turn test requires value-equivalent config owners"
    );
    let equivalent_step_context = session.capture_step_context(equivalent_turn_context).await;
    let router = ToolRouter::from_parts(
        ToolRegistry::from_tools(std::iter::empty::<
            Arc<dyn crate::tools::registry::CoreToolRuntime>,
        >()),
        Vec::new(),
    );
    let base_instructions = session.get_base_instructions().await;
    let mut history = ContextManager::new();
    let first_user_message = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "first dynamic request".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    history.record_items(
        [&first_user_message],
        turn_context.model_info.truncation_policy.into(),
    );
    let first_prepared = history.clone().prepare_for_sampling_prompt(
        &turn_context.model_info.input_modalities,
        StableContextTarget::Sampling,
    );
    let second_user_message = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "second dynamic request".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    history.record_items(
        [&second_user_message],
        turn_context.model_info.truncation_policy.into(),
    );
    let second_prepared = history.prepare_for_sampling_prompt(
        &turn_context.model_info.input_modalities,
        StableContextTarget::Sampling,
    );
    assert_ne!(first_prepared.fingerprint(), second_prepared.fingerprint());
    assert!(stable_context_owner_matches(
        first_prepared.stable_context_manifest(),
        second_prepared.stable_context_manifest(),
    ));

    let mut cache = session
        .request_scaffold_cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let first = cache.resolve(
        &first_prepared,
        session.as_ref(),
        &router,
        step_context.as_ref(),
        &base_instructions,
        /*terminal_completion_only*/ false,
    );
    let first_scaffold = Arc::clone(&first.scaffold);
    let first_prompt = build_projected_prompt_from_scaffold(
        session.as_ref(),
        &first_prepared,
        step_context.as_ref(),
        &first,
    );
    drop(cache);
    let mut cache = session
        .request_scaffold_cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let second = cache.resolve(
        &second_prepared,
        session.as_ref(),
        &router,
        step_context.as_ref(),
        &base_instructions,
        /*terminal_completion_only*/ false,
    );
    assert!(second.locally_reused);
    assert!(Arc::ptr_eq(&first_scaffold, &second.scaffold));
    assert_eq!(cache.build_count(), 1);
    let second_prompt = build_projected_prompt_from_scaffold(
        session.as_ref(),
        &second_prepared,
        step_context.as_ref(),
        &second,
    );
    assert_ne!(first_prompt.input, second_prompt.input);
    assert_eq!(
        first_prompt.digests.instructions,
        second_prompt.digests.instructions
    );
    assert_eq!(first_prompt.digests.tools, second_prompt.digests.tools);
    assert_ne!(first_prompt.digests.history, second_prompt.digests.history);
    assert!(Arc::ptr_eq(&first_prompt.tools, &second_prompt.tools));

    let equivalent_turn = cache.resolve(
        &second_prepared,
        session.as_ref(),
        &router,
        equivalent_step_context.as_ref(),
        &base_instructions,
        /*terminal_completion_only*/ false,
    );
    assert!(equivalent_turn.locally_reused);
    assert!(Arc::ptr_eq(&second.scaffold, &equivalent_turn.scaffold));
    assert_eq!(
        cache.build_count(),
        1,
        "value-equivalent per-turn config owners must not rebuild stable request scaffolding"
    );

    let cold_second_prompt = build_projected_prompt(
        session.as_ref(),
        &second_prepared,
        &router,
        step_context.as_ref(),
        base_instructions.clone(),
    );
    assert_eq!(second_prompt.input, cold_second_prompt.input);
    assert_eq!(
        second_prompt.stable_context_fallback_input,
        cold_second_prompt.stable_context_fallback_input
    );
    assert_eq!(
        second_prompt.tool_history_fallback_input,
        cold_second_prompt.tool_history_fallback_input
    );
    assert_eq!(
        second_prompt.stable_context_tool_history_fallback_input,
        cold_second_prompt.stable_context_tool_history_fallback_input
    );
    assert_eq!(
        second_prompt.tool_history_substitutions,
        cold_second_prompt.tool_history_substitutions
    );
    assert_eq!(
        second_prompt.stable_context_fallback_tool_history_substitutions,
        cold_second_prompt.stable_context_fallback_tool_history_substitutions
    );
    assert_eq!(second_prompt.digests, cold_second_prompt.digests);
    assert_eq!(
        second_prompt.stable_context_manifest.projected_bytes(),
        cold_second_prompt.stable_context_manifest.projected_bytes()
    );
    assert_eq!(
        second_prompt.stable_context_manifest.projected_tokens(),
        cold_second_prompt
            .stable_context_manifest
            .projected_tokens()
    );
    assert!(stable_context_owner_matches(
        &second_prompt.stable_context_manifest,
        &cold_second_prompt.stable_context_manifest,
    ));
    assert_eq!(
        second_prompt.base_instructions,
        cold_second_prompt.base_instructions
    );
    assert_eq!(
        second_prompt.tools.serialized(),
        cold_second_prompt.tools.serialized()
    );
    assert_eq!(
        second_prompt.parallel_tool_calls,
        cold_second_prompt.parallel_tool_calls
    );
    assert_eq!(
        second_prompt.output_schema,
        cold_second_prompt.output_schema
    );
    assert_eq!(
        second_prompt.output_schema_strict,
        cold_second_prompt.output_schema_strict
    );
    let scaffold_measurements = crate::context::PromptContextBreakdown::from_response_items(
        &second_prompt.input,
        &second_prompt.prompt_provenance,
    )
    .expect("scaffold prompt provenance should measure")
    .measurements();
    let cold_measurements = crate::context::PromptContextBreakdown::from_response_items(
        &cold_second_prompt.input,
        &cold_second_prompt.prompt_provenance,
    )
    .expect("cold prompt provenance should measure")
    .measurements();
    assert_eq!(scaffold_measurements, cold_measurements);

    // Reconstructing an identical router does not change its model-visible
    // surface and must retain the same physical scaffold.
    let replacement_router = ToolRouter::from_parts(
        ToolRegistry::from_tools(std::iter::empty::<
            Arc<dyn crate::tools::registry::CoreToolRuntime>,
        >()),
        Vec::new(),
    );
    let replaced_surface = cache.resolve(
        &second_prepared,
        session.as_ref(),
        &replacement_router,
        step_context.as_ref(),
        &base_instructions,
        /*terminal_completion_only*/ false,
    );
    assert!(Arc::ptr_eq(&second.scaffold, &replaced_surface.scaffold));
    assert_eq!(cache.build_count(), 1);
    assert_eq!(
        second.scaffold.tools.serialized(),
        replaced_surface.scaffold.tools.serialized()
    );

    let changed_router = ToolRouter::from_parts(
        ToolRegistry::from_tools(std::iter::empty::<
            Arc<dyn crate::tools::registry::CoreToolRuntime>,
        >()),
        vec![codex_tools::ToolSpec::Function(
            codex_tools::ResponsesApiTool {
                name: "changed_surface".to_string(),
                description: "genuinely changed tool schema".to_string(),
                strict: false,
                defer_loading: None,
                parameters: codex_tools::JsonSchema::object(
                    Default::default(),
                    None,
                    Some(false.into()),
                ),
                output_schema: None,
            },
        )],
    );
    let changed_surface = cache.resolve(
        &second_prepared,
        session.as_ref(),
        &changed_router,
        step_context.as_ref(),
        &base_instructions,
        /*terminal_completion_only*/ false,
    );
    assert!(!Arc::ptr_eq(
        &replaced_surface.scaffold,
        &changed_surface.scaffold
    ));
    assert_eq!(cache.build_count(), 2);

    let changed_instructions = BaseInstructions {
        text: format!("{}\nchanged owner", base_instructions.text),
    };
    let replaced_instructions = cache.resolve(
        &second_prepared,
        session.as_ref(),
        &changed_router,
        step_context.as_ref(),
        &changed_instructions,
        /*terminal_completion_only*/ false,
    );
    assert!(!Arc::ptr_eq(
        &changed_surface.scaffold,
        &replaced_instructions.scaffold
    ));
    assert_eq!(cache.build_count(), 3);
}

#[tokio::test]
async fn request_scaffold_separates_terminal_and_ordinary_tool_surfaces() {
    let (session, turn_context) = crate::session::tests::make_session_and_context().await;
    let session = Arc::new(session);
    let turn_context = Arc::new(turn_context);
    let step_context = session
        .capture_step_context(Arc::clone(&turn_context))
        .await;
    let router = ToolRouter::from_parts(
        ToolRegistry::from_tools(std::iter::empty::<
            Arc<dyn crate::tools::registry::CoreToolRuntime>,
        >()),
        vec![codex_tools::ToolSpec::Function(
            codex_tools::ResponsesApiTool {
                name: "ordinary_surface".to_string(),
                description: "Visible only on ordinary generations.".to_string(),
                strict: false,
                defer_loading: None,
                parameters: codex_tools::JsonSchema::default(),
                output_schema: None,
            },
        )],
    );
    let mut history = ContextManager::new();
    let user_message = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "finish without tools when requested".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    history.record_items(
        [&user_message],
        turn_context.model_info.truncation_policy.into(),
    );
    let prepared = history.prepare_for_sampling_prompt(
        &turn_context.model_info.input_modalities,
        StableContextTarget::Sampling,
    );
    let base_instructions = session.get_base_instructions().await;
    let mut cache = RequestScaffoldCache::default();

    let ordinary = cache.resolve(
        &prepared,
        session.as_ref(),
        &router,
        step_context.as_ref(),
        &base_instructions,
        /*terminal_completion_only*/ false,
    );
    assert!(!ordinary.scaffold.tools.specs().is_empty());
    assert_eq!(
        ordinary.scaffold.digests.tools,
        Some(ordinary.scaffold.tools.digest())
    );

    let terminal = cache.resolve(
        &prepared,
        session.as_ref(),
        &router,
        step_context.as_ref(),
        &base_instructions,
        /*terminal_completion_only*/ true,
    );
    assert!(!terminal.locally_reused);
    assert!(terminal.scaffold.tools.specs().is_empty());
    assert_eq!(
        terminal.scaffold.digests.tools,
        Some(terminal.scaffold.tools.digest())
    );
    let mut terminal_prompt =
        build_projected_prompt_from_scaffold(&session, &prepared, step_context.as_ref(), &terminal);
    enforce_terminal_prompt_contract(&mut terminal_prompt, true);
    assert!(terminal_prompt.tools.specs().is_empty());
    assert_eq!(
        terminal_prompt.digests.tools,
        Some(terminal_prompt.tools.digest())
    );

    let ordinary_again = cache.resolve(
        &prepared,
        session.as_ref(),
        &router,
        step_context.as_ref(),
        &base_instructions,
        /*terminal_completion_only*/ false,
    );
    assert!(!ordinary_again.locally_reused);
    assert!(!ordinary_again.scaffold.tools.specs().is_empty());
    assert_eq!(cache.build_count(), 3);
}

#[tokio::test]
async fn projected_prompt_defers_dynamic_history_measurement_across_retries() {
    let (session, turn_context) = crate::session::tests::make_session_and_context().await;
    let session = Arc::new(session);
    let turn_context = Arc::new(turn_context);
    let step_context = session
        .capture_step_context(Arc::clone(&turn_context))
        .await;
    let router = ToolRouter::from_parts(
        ToolRegistry::from_tools(std::iter::empty::<
            Arc<dyn crate::tools::registry::CoreToolRuntime>,
        >()),
        Vec::new(),
    );
    let mut history = ContextManager::new();
    let user_message = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "dynamic history is measured after dispatch".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    history.record_items(
        [&user_message],
        turn_context.model_info.truncation_policy.into(),
    );
    let prepared = history.prepare_for_sampling_prompt(
        &turn_context.model_info.input_modalities,
        StableContextTarget::Sampling,
    );
    let prompt = build_projected_prompt(
        &session,
        &prepared,
        &router,
        step_context.as_ref(),
        session.get_base_instructions().await,
    );
    let retry_prompt = prompt.clone();

    assert_eq!(prompt.dynamic_history_measurement_count(), 0);
    assert!(
        prompt
            .stable_context_manifest
            .components()
            .iter()
            .all(|component| component.kind != StableContextKind::DynamicHistory)
    );

    let measured = prompt.measured_stable_context_manifest();
    assert_eq!(prompt.dynamic_history_measurement_count(), 1);
    assert!(
        measured
            .components()
            .iter()
            .any(|component| component.kind == StableContextKind::DynamicHistory)
    );
    let measured_retry = retry_prompt.measured_stable_context_manifest();
    assert_eq!(retry_prompt.dynamic_history_measurement_count(), 1);
    assert_eq!(measured.projected_bytes(), measured_retry.projected_bytes());
    assert_eq!(
        measured.projected_tokens(),
        measured_retry.projected_tokens()
    );
}

#[tokio::test]
async fn pending_turn_router_reuses_session_cache_until_planning_changes() -> Result<()> {
    let (session, turn_context, _events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let planning_generation = session.services.planning_generation();
    let first_step = session
        .capture_step_context(Arc::clone(&turn_context))
        .await;
    let first = built_tools_for_pending_turn(
        session.as_ref(),
        first_step.as_ref(),
        &[],
        planning_generation,
        &CancellationToken::new(),
    )
    .await?;
    let second_step = session
        .capture_step_context(Arc::clone(&turn_context))
        .await;
    let second = built_tools_for_pending_turn(
        session.as_ref(),
        second_step.as_ref(),
        &[],
        planning_generation,
        &CancellationToken::new(),
    )
    .await?;
    assert!(Arc::ptr_eq(&first, &second));

    let changed_generation = {
        let mut state_owner = session.state.lock().await;
        session
            .services
            .advance_planning_generation(&mut state_owner)
    };
    let changed_step = session.capture_step_context(turn_context).await;
    let changed = built_tools_for_pending_turn(
        session.as_ref(),
        changed_step.as_ref(),
        &[],
        changed_generation,
        &CancellationToken::new(),
    )
    .await?;
    assert!(!Arc::ptr_eq(&second, &changed));
    let counters = changed_step
        .turn
        .turn_timing_state
        .complete_snapshot()
        .protocol_timing()
        .counters;
    assert_eq!(
        counters.tool_router_reuse_count, 0,
        "pending-turn planning reuse is not a model-generation router decision",
    );
    Ok(())
}

#[tokio::test]
async fn built_tools_uses_the_revision_tagged_on_the_step_mcp_snapshot() -> Result<()> {
    let (session, turn_context, _events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let step_context = session.capture_step_context(turn_context).await;
    let snapshot_revision = step_context
        .mcp
        .manager()
        .tool_catalog_revision()
        .saturating_add(7);
    step_context
        .seed_mcp_tool_snapshot_for_test(snapshot_revision, Vec::new(), true)
        .await;

    let router = built_tools(
        session.as_ref(),
        step_context.as_ref(),
        &[],
        &CancellationToken::new(),
    )
    .await?;

    assert_eq!(
        router.exposure_identity().mcp_tool_catalog_revision,
        snapshot_revision
    );
    assert!(router.exposure_identity().mcp_resources_available);
    Ok(())
}

#[tokio::test]
async fn sampling_prompt_workspace_capture_is_skipped_without_workspace_evidence() {
    let (session, turn_context) = crate::session::tests::make_session_and_context().await;
    let git_workspace = crate::git_workspace::GitWorkspaceCache::with_noop_watcher_for_tests();
    let client_session = session.services.model_client.new_session();
    let mut history = ContextManager::new();
    let user_message = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "ordinary conversation".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    history.record_items([&user_message], TruncationPolicy::Tokens(10_000));

    let _prepared =
        prepare_sampling_prompt_for_client(history, &turn_context, &client_session, &git_workspace)
            .await;

    assert_eq!(git_workspace.workspace_evidence_capture_count(), 0);
}

#[tokio::test]
async fn sampling_prompt_workspace_capture_is_skipped_for_non_workspace_code_mode_exec() {
    let (session, turn_context) = crate::session::tests::make_session_and_context().await;
    let git_workspace = crate::git_workspace::GitWorkspaceCache::with_noop_watcher_for_tests();
    let client_session = session.services.model_client.new_session();
    let mut history = ContextManager::new();
    let call_id = "non-workspace-code-mode";
    let call = ResponseItem::FunctionCall {
        id: None,
        name: "functions.exec".to_string(),
        namespace: None,
        arguments: "await tools.list_mcp_resources({})".to_string(),
        call_id: call_id.to_string(),
        internal_chat_message_metadata_passthrough: None,
    };
    let output = ResponseItem::FunctionCallOutput {
        id: None,
        call_id: call_id.to_string(),
        output: FunctionCallOutputPayload::from_text("resources".to_string()),
        internal_chat_message_metadata_passthrough: None,
    };
    history.register_non_workspace_code_mode_call(call_id.to_string());
    history.record_items([&call, &output], TruncationPolicy::Tokens(10_000));

    let _prepared =
        prepare_sampling_prompt_for_client(history, &turn_context, &client_session, &git_workspace)
            .await;

    assert_eq!(git_workspace.workspace_evidence_capture_count(), 0);
}

#[tokio::test]
async fn sampling_prompt_workspace_capture_is_preserved_for_workspace_evidence() {
    let (session, turn_context) = crate::session::tests::make_session_and_context().await;
    let git_workspace = crate::git_workspace::GitWorkspaceCache::with_noop_watcher_for_tests();
    let client_session = session.services.model_client.new_session();
    let mut history = ContextManager::new();
    let call = ResponseItem::FunctionCall {
        id: None,
        name: "functions.exec".to_string(),
        namespace: None,
        arguments: r#"{"cmd":"git status --short"}"#.to_string(),
        call_id: "workspace-call".to_string(),
        internal_chat_message_metadata_passthrough: None,
    };
    let output = ResponseItem::FunctionCallOutput {
        id: None,
        call_id: "workspace-call".to_string(),
        output: FunctionCallOutputPayload::from_text("clean".to_string()),
        internal_chat_message_metadata_passthrough: None,
    };
    history.record_items([&call, &output], TruncationPolicy::Tokens(10_000));

    let _prepared =
        prepare_sampling_prompt_for_client(history, &turn_context, &client_session, &git_workspace)
            .await;

    assert_eq!(git_workspace.workspace_evidence_capture_count(), 1);
}

#[tokio::test]
async fn token_backfire_deferred_tool_schema_survives_multiple_same_turn_requests() {
    let (_, turn_context) = crate::session::tests::make_session_and_context().await;
    let advertised = codex_tools::ToolName::plain("advertised_deferred");
    let discovered_during_request = codex_tools::ToolName::plain("newly_discovered_deferred");
    turn_context.refresh_deferred_tool_capabilities(Arc::new(HashMap::from([
        (advertised.clone(), "revision-a".to_string()),
        (discovered_during_request.clone(), "revision-b".to_string()),
    ])));
    turn_context.activate_deferred_tools(std::iter::once(advertised.clone()));
    let advertised_for_request = turn_context.activated_deferred_tools();

    turn_context.activate_deferred_tools(std::iter::once(discovered_during_request.clone()));
    turn_context.release_advertised_deferred_tools(&advertised_for_request);

    assert!(
        turn_context.deferred_tool_is_activated(&advertised),
        "an intervening same-turn request must not discard an activated callable schema"
    );
    assert!(
        turn_context.deferred_tool_is_activated(&discovered_during_request),
        "discovery performed during the request must remain visible to its continuation"
    );
}

#[tokio::test]
async fn deferred_tool_schema_survives_sampling_error_exit() {
    let (_, turn_context, _) = crate::session::tests::make_session_and_context_with_rx().await;
    let advertised = codex_tools::ToolName::plain("advertised_deferred");
    let discovered_during_request = codex_tools::ToolName::plain("discovered_during_request");
    turn_context.refresh_deferred_tool_capabilities(Arc::new(HashMap::from([
        (advertised.clone(), "revision-a".to_string()),
        (discovered_during_request.clone(), "revision-b".to_string()),
    ])));
    turn_context.activate_deferred_tools(std::iter::once(advertised.clone()));

    {
        let _lease = AdvertisedDeferredToolLease::new(
            Arc::clone(&turn_context),
            turn_context.activated_deferred_tools(),
        );
        turn_context.activate_deferred_tools(std::iter::once(discovered_during_request.clone()));
    }

    assert!(turn_context.deferred_tool_is_activated(&advertised));
    assert!(turn_context.deferred_tool_is_activated(&discovered_during_request));
}

#[test]
fn sampling_retry_rebuilds_after_accepted_output() {
    let mut progress = SamplingAttemptProgress::default();
    assert!(!progress.requires_authoritative_retry_input());

    progress.accepted_output = true;

    assert!(progress.requires_authoritative_retry_input());
}

#[test]
fn sampling_success_retains_the_shared_accepted_input() {
    let accepted: Arc<[ResponseItem]> = Arc::from([ResponseItem::FunctionCall {
        id: None,
        name: "accepted".to_string(),
        namespace: None,
        arguments: "{}".to_string(),
        call_id: "accepted-call".to_string(),
        internal_chat_message_metadata_passthrough: None,
    }]);

    let retained = retain_accepted_sampling_input(Arc::clone(&accepted));

    assert!(Arc::ptr_eq(&retained, &accepted));
}

#[test]
fn kd4_latency_continuation_prefetch_rejects_stale_or_steered_state() {
    assert!(continuation_workspace_prefetch_is_current(7, 7, false));
    assert!(!continuation_workspace_prefetch_is_current(7, 8, false));
    assert!(!continuation_workspace_prefetch_is_current(7, 7, true));
}

#[tokio::test]
async fn kd4_latency_continuation_prefetch_skips_non_workspace_eager_read() {
    let (_, turn_context) = crate::session::tests::make_session_and_context().await;
    let git_workspace = crate::git_workspace::GitWorkspaceCache::with_noop_watcher_for_tests();
    let turn_diff_tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));
    let mut history = ContextManager::new();
    let call = ResponseItem::FunctionCall {
        id: None,
        name: "list_mcp_resources".to_string(),
        namespace: None,
        arguments: "{}".to_string(),
        call_id: "non-workspace-read".to_string(),
        internal_chat_message_metadata_passthrough: None,
    };
    let output = ResponseItem::FunctionCallOutput {
        id: None,
        call_id: "non-workspace-read".to_string(),
        output: FunctionCallOutputPayload::from_text("resources".to_string()),
        internal_chat_message_metadata_passthrough: None,
    };
    history.record_items([&call, &output], TruncationPolicy::Tokens(10_000));

    let prefetch = start_continuation_workspace_prefetch(
        &history,
        &turn_diff_tracker,
        Arc::clone(&git_workspace),
        turn_context.config.cwd.clone(),
    )
    .await;

    assert!(prefetch.is_none());
    assert_eq!(git_workspace.workspace_evidence_capture_count(), 0);
}

#[tokio::test]
async fn continuation_prefetch_skips_non_workspace_code_mode_exec() {
    let (_, turn_context) = crate::session::tests::make_session_and_context().await;
    let git_workspace = crate::git_workspace::GitWorkspaceCache::with_noop_watcher_for_tests();
    let turn_diff_tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));
    let mut history = ContextManager::new();
    let call_id = "non-workspace-code-mode-prefetch";
    let call = ResponseItem::FunctionCall {
        id: None,
        name: "functions.exec".to_string(),
        namespace: None,
        arguments: "await tools.list_mcp_resources({})".to_string(),
        call_id: call_id.to_string(),
        internal_chat_message_metadata_passthrough: None,
    };
    let output = ResponseItem::FunctionCallOutput {
        id: None,
        call_id: call_id.to_string(),
        output: FunctionCallOutputPayload::from_text("resources".to_string()),
        internal_chat_message_metadata_passthrough: None,
    };
    history.register_non_workspace_code_mode_call(call_id.to_string());
    history.record_items([&call, &output], TruncationPolicy::Tokens(10_000));

    let prefetch = start_continuation_workspace_prefetch(
        &history,
        &turn_diff_tracker,
        Arc::clone(&git_workspace),
        turn_context.config.cwd.clone(),
    )
    .await;

    assert!(prefetch.is_none());
    assert_eq!(git_workspace.workspace_evidence_capture_count(), 0);
}

#[tokio::test]
async fn kd4_latency_continuation_prefetch_preserves_workspace_evidence_read() {
    let (_, turn_context) = crate::session::tests::make_session_and_context().await;
    let git_workspace = crate::git_workspace::GitWorkspaceCache::with_noop_watcher_for_tests();
    let turn_diff_tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));
    let mut history = ContextManager::new();
    let call = ResponseItem::FunctionCall {
        id: None,
        name: "functions.exec".to_string(),
        namespace: None,
        arguments: r#"{"cmd":"git status --short"}"#.to_string(),
        call_id: "workspace-read".to_string(),
        internal_chat_message_metadata_passthrough: None,
    };
    let output = ResponseItem::FunctionCallOutput {
        id: None,
        call_id: "workspace-read".to_string(),
        output: FunctionCallOutputPayload::from_text("clean".to_string()),
        internal_chat_message_metadata_passthrough: None,
    };
    history.record_items([&call, &output], TruncationPolicy::Tokens(10_000));

    let (_, handle) = start_continuation_workspace_prefetch(
        &history,
        &turn_diff_tracker,
        Arc::clone(&git_workspace),
        turn_context.config.cwd.clone(),
    )
    .await
    .expect("workspace evidence should start a continuation prefetch");
    let _ = handle
        .await
        .expect("continuation workspace prefetch should join");

    assert_eq!(git_workspace.workspace_evidence_capture_count(), 1);
}

#[tokio::test]
async fn workspace_evidence_coalesces_mutating_calls_at_generation_boundary() {
    let (session, turn_context) = crate::session::tests::make_session_and_context().await;
    let tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));
    let batch = Arc::new(crate::tools::parallel::WorkspaceEvidenceGenerationBatch::new());
    let repo_root = codex_git_utils::get_git_repo_root(turn_context.config.cwd.as_path())
        .expect("turn workspace should belong to a Git repository");
    let dependency_paths = [
        repo_root.join("workspace-evidence-test/dependency-a.rs"),
        repo_root.join("workspace-evidence-test/dependency-b.rs"),
    ];
    let classifications = dependency_paths.clone().map(|dependency_path| {
        crate::tool_history::WorkspaceCallClassification {
            observes_workspace: true,
            workspace_cwd: turn_context.config.cwd.clone().to_path_buf(),
            source_dependencies: std::collections::BTreeSet::from([
                crate::tool_history::SourceDependencyV1::new(&dependency_path, false),
            ]),
        }
    });
    let mutation_paths = [
        dependency_paths[0].clone(),
        repo_root.join("workspace-evidence-test/mutation-b.rs"),
    ];
    let call_ids = ["generation-mutation-a", "generation-mutation-b"];
    let calls = call_ids.map(|call_id| ResponseItem::FunctionCall {
        id: None,
        name: "exec_command".to_string(),
        namespace: None,
        arguments: format!(r#"{{"cmd":"mutate {call_id}"}}"#),
        call_id: call_id.to_string(),
        internal_chat_message_metadata_passthrough: None,
    });
    let responses = call_ids.map(|call_id| ResponseInputItem::FunctionCallOutput {
        call_id: call_id.to_string(),
        output: FunctionCallOutputPayload::from_text(format!("completed {call_id}")),
    });
    let outputs = responses.clone().map(ResponseItem::from);
    session
        .record_conversation_items(
            &turn_context,
            &[
                calls[0].clone(),
                calls[1].clone(),
                outputs[0].clone(),
                outputs[1].clone(),
            ],
        )
        .await;

    for (index, (call_id, response)) in call_ids.iter().zip(&responses).enumerate() {
        let classification = &classifications[index];
        assert!(batch.register_call(call_id));
        tracker
            .lock()
            .await
            .activate_workspace_evidence_generation_batch(&batch);
        let source_path_observations = classification
            .source_dependencies
            .iter()
            .filter_map(|dependency| {
                session
                    .services
                    .git_workspace
                    .begin_source_path_change_observation(
                        &repo_root,
                        Path::new(&dependency.path),
                        dependency.recursive,
                    )
            })
            .collect::<Vec<_>>();
        assert_eq!(source_path_observations.len(), 1);
        assert!(
            session
                .services
                .git_workspace
                .source_path_change_observation_is_current(&source_path_observations[0])
        );
        session
            .services
            .git_workspace
            .note_host_workspace_mutation_paths(
                &repo_root,
                &[mutation_paths[index].to_string_lossy().into_owned()],
            );
        assert_eq!(
            session
                .services
                .git_workspace
                .source_path_change_observation_is_current(&source_path_observations[0]),
            index != 0,
            "only the dependency changed by its batched call should lose its pre-tool proof",
        );
        tracker.lock().await.record_unknown_mutation();
        assert!(batch.record_mutation(
            call_id,
            turn_context.config.cwd.clone().to_path_buf(),
            Some(std::collections::BTreeSet::from([
                mutation_paths[index].clone(),
            ])),
            /*observe_command_ledger*/ false,
        ));
        assert!(batch.queue_mutating_response_for_test(
            response,
            classification,
            source_path_observations,
        ));
    }

    let captures_before = session
        .services
        .git_workspace
        .workspace_evidence_capture_count();
    let flush = batch.flush(&session, &turn_context, &tracker).await;
    let final_identity = flush
        .prefetched_workspace_identity
        .expect("the generation flush should capture the primary workspace")
        .expect("the primary workspace should have a Git identity");

    assert_eq!(flush.authoritative_capture_count, 1);
    assert_eq!(
        flush.registered_call_ids,
        vec![call_ids[0].to_string(), call_ids[1].to_string()],
    );
    assert_eq!(
        session
            .services
            .git_workspace
            .workspace_evidence_capture_count()
            - captures_before,
        1,
    );

    let history = session.clone_history().await;
    let tool_history = history.tool_history_state();
    for call_id in call_ids {
        assert_eq!(
            tool_history.workspace_evidence_revision_for_test(call_id),
            Some(Some(final_identity.clone())),
        );
    }

    let captures_before_continuation = session
        .services
        .git_workspace
        .workspace_evidence_capture_count();
    let prepared = prepare_sampling_prompt_with_workspace_identity(
        history,
        &turn_context,
        Some(&final_identity),
        session.services.git_workspace.as_ref(),
    );
    assert_eq!(
        session
            .services
            .git_workspace
            .workspace_evidence_capture_count(),
        captures_before_continuation,
    );
    for call_id in call_ids {
        let output = prepared
            .items()
            .iter()
            .find_map(|item| match item {
                ResponseItem::FunctionCallOutput {
                    call_id: item_call_id,
                    output,
                    ..
                } if item_call_id == call_id => output.text_content(),
                _ => None,
            })
            .expect("the current batched output should remain in the projected history");
        assert_eq!(output, format!("completed {call_id}"));
    }

    session
        .services
        .git_workspace
        .note_host_workspace_mutation_paths(
            &repo_root,
            &[repo_root
                .join("workspace-evidence-test/later-disjoint.rs")
                .to_string_lossy()
                .into_owned()],
        );
    let mut later_identity = final_identity.clone();
    later_identity.worktree_identity = Some(format!(
        "{}:later-disjoint",
        final_identity
            .worktree_identity
            .as_deref()
            .unwrap_or("none")
    ));
    let later_prepared = prepare_sampling_prompt_with_workspace_identity(
        session.clone_history().await,
        &turn_context,
        Some(&later_identity),
        session.services.git_workspace.as_ref(),
    );
    let stale_output = later_prepared
        .items()
        .iter()
        .find_map(|item| match item {
            ResponseItem::FunctionCallOutput {
                call_id, output, ..
            } if call_id == call_ids[0] => output.text_content(),
            _ => None,
        })
        .expect("the changed dependency output should remain in the projected history");
    assert!(stale_output.contains("stale_workspace_evidence"));
    let disjoint_output = later_prepared
        .items()
        .iter()
        .find_map(|item| match item {
            ResponseItem::FunctionCallOutput {
                call_id, output, ..
            } if call_id == call_ids[1] => output.text_content(),
            _ => None,
        })
        .expect("the disjoint dependency output should remain in the projected history");
    assert_eq!(disjoint_output, format!("completed {}", call_ids[1]));
}

#[tokio::test]
async fn workspace_evidence_flushes_distinct_repositories_concurrently() {
    let (session, turn_context) = crate::session::tests::make_session_and_context().await;
    let tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));
    let batch = Arc::new(crate::tools::parallel::WorkspaceEvidenceGenerationBatch::new());
    let second_workspace = tempfile::tempdir().expect("create second workspace");
    let git_init = std::process::Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(second_workspace.path())
        .status()
        .expect("run git init for second workspace");
    assert!(git_init.success(), "initialize second Git workspace");

    for (call_id, cwd) in [
        (
            "primary-repository-mutation",
            turn_context.config.cwd.as_path(),
        ),
        ("second-repository-mutation", second_workspace.path()),
    ] {
        assert!(batch.register_call(call_id));
        assert!(batch.record_mutation(
            call_id,
            cwd.to_path_buf(),
            None,
            /* observe_command_ledger */ false,
        ));
    }

    let captures_before = session
        .services
        .git_workspace
        .workspace_evidence_capture_count();
    let pause = session
        .services
        .git_workspace
        .pause_next_workspace_evidence_capture();
    let flush = batch.flush(&session, &turn_context, &tracker);
    tokio::pin!(flush);
    tokio::select! {
        started = tokio::time::timeout(Duration::from_secs(10), pause.wait_until_started()) => {
            started.expect("the first authoritative capture should start");
        }
        _ = &mut flush => {
            panic!("flush completed before the paused capture was released");
        }
    }

    assert_eq!(
        session
            .services
            .git_workspace
            .workspace_evidence_capture_count()
            - captures_before,
        2,
        "a paused repository capture must not prevent another repository capture from starting",
    );
    pause.release();
    let completed = flush.await;
    assert_eq!(completed.authoritative_capture_count, 2);
}

#[tokio::test]
async fn workspace_evidence_flush_preserves_authoritative_non_git_identity() {
    let (session, mut turn_context) = crate::session::tests::make_session_and_context().await;
    let non_git_workspace = tempfile::tempdir().expect("create non-Git workspace");
    assert!(codex_git_utils::get_git_repo_root(non_git_workspace.path()).is_none());
    let mut config = (*turn_context.config).clone();
    config.cwd = non_git_workspace
        .path()
        .to_path_buf()
        .try_into()
        .expect("temporary workspace path should be absolute");
    turn_context.config = Arc::new(config);

    let tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));
    let batch = Arc::new(crate::tools::parallel::WorkspaceEvidenceGenerationBatch::new());
    let call_id = "non-git-mutation";
    let classification = crate::tool_history::WorkspaceCallClassification {
        observes_workspace: true,
        workspace_cwd: turn_context.config.cwd.clone().to_path_buf(),
        source_dependencies: std::collections::BTreeSet::new(),
    };
    let response = ResponseInputItem::FunctionCallOutput {
        call_id: call_id.to_string(),
        output: FunctionCallOutputPayload::from_text("completed non-Git mutation".to_string()),
    };

    assert!(batch.register_call(call_id));
    tracker
        .lock()
        .await
        .activate_workspace_evidence_generation_batch(&batch);
    tracker.lock().await.record_unknown_mutation();
    assert!(batch.record_mutation(
        call_id,
        classification.workspace_cwd.clone(),
        None,
        /*observe_command_ledger*/ false,
    ));
    assert!(batch.queue_mutating_response_for_test(&response, &classification, Vec::new(),));

    let flush = batch.flush(&session, &turn_context, &tracker).await;

    assert_eq!(
        flush.prefetched_workspace_identity,
        Some(None),
        "an authoritative non-Git capture must not look like a missing primary capture",
    );
    assert_eq!(flush.authoritative_capture_count, 1);
    assert_eq!(flush.registered_call_ids, vec![call_id.to_string()]);
}

#[test]
fn tool_relay_reconciliation_advances_without_watchdog() {
    let timing = TurnTimingState::default();
    timing.mark_turn_started();
    let mut orphan_passes = 0;

    timing.adjust_parallel_gate_waiters(1);
    assert_eq!(
        reconcile_turn_progress(&timing, 1, &mut orphan_passes),
        NextSampleBlockReason::WaitingForGate
    );
    timing.adjust_parallel_gate_waiters(-1);
    timing.adjust_relay_queue_depth(1);
    assert_eq!(
        reconcile_turn_progress(&timing, 1, &mut orphan_passes),
        NextSampleBlockReason::WaitingForDelivery
    );
    timing.adjust_relay_queue_depth(-1);
    timing.adjust_active_tools(1);
    assert_eq!(
        reconcile_turn_progress(&timing, 0, &mut orphan_passes),
        NextSampleBlockReason::WaitingForTool
    );
    assert_eq!(
        reconcile_turn_progress(&timing, 0, &mut orphan_passes),
        NextSampleBlockReason::WaitingForTool,
        "a retained or nested tool may remain active across reconciliation passes"
    );
    assert_eq!(orphan_passes, 2);
    timing.adjust_active_tools(-1);
    assert_eq!(
        reconcile_turn_progress(&timing, 0, &mut orphan_passes),
        NextSampleBlockReason::ReadyToSample
    );
    assert_eq!(orphan_passes, 0);
}

fn authoritative_wait_result(
    surfaceable_message: Option<&str>,
) -> crate::session::reasoning_governor::AuthoritativeWaitOwnerResult {
    crate::session::reasoning_governor::AuthoritativeWaitOwnerResult {
        adapter: "code_mode_cell".to_string(),
        value: serde_json::json!("arbitrary raw execution output"),
        surfaceable_message: surfaceable_message.map(ToOwned::to_owned),
    }
}

fn recommended_plugin_candidate(id: &str, name: &str) -> DiscoverableTool {
    codex_tools::DiscoverablePluginInfo {
        id: id.to_string(),
        remote_plugin_id: None,
        name: name.to_string(),
        description: None,
        has_skills: false,
        mcp_server_names: Vec::new(),
        app_connector_ids: Vec::new(),
    }
    .into()
}

#[test]
fn recommended_plugins_are_not_injected_for_unrelated_tasks() {
    let selected = task_relevant_recommended_plugins(
        &[ContentItem::InputText {
            text: "fix the parser".to_string(),
        }],
        vec![recommended_plugin_candidate("figma", "Figma")],
    );

    assert!(selected.is_empty());
}

#[test]
fn named_recommended_plugin_is_the_only_injected_candidate() {
    let selected = task_relevant_recommended_plugins(
        &[ContentItem::InputText {
            text: "use Figma for this mockup".to_string(),
        }],
        vec![
            recommended_plugin_candidate("figma", "Figma"),
            recommended_plugin_candidate("notion", "Notion"),
        ],
    );

    assert_eq!(
        selected
            .iter()
            .map(DiscoverableTool::name)
            .collect::<Vec<_>>(),
        vec!["Figma"]
    );
}

#[test]
fn generic_plugin_recommendation_request_injects_the_catalog() {
    let selected = task_relevant_recommended_plugins(
        &[ContentItem::InputText {
            text: "suggest a plugin".to_string(),
        }],
        vec![
            recommended_plugin_candidate("figma", "Figma"),
            recommended_plugin_candidate("notion", "Notion"),
        ],
    );

    assert_eq!(selected.len(), 2);
}

#[test]
fn recommended_plugin_catalog_is_bounded_by_rendered_bytes() {
    let candidates = (0..50)
        .map(|index| {
            recommended_plugin_candidate(
                &format!("plugin-{index}-{}", "x".repeat(180)),
                &format!("Plugin {index} {}", "y".repeat(180)),
            )
        })
        .collect();
    let instructions = RecommendedPluginsInstructions::from_plugins(candidates)
        .expect("at least one bounded plugin entry");
    let ResponseItem::Message { content, .. } = ContextualUserFragment::into(instructions) else {
        panic!("expected recommended plugin message");
    };
    let ContentItem::InputText { text } = &content[0] else {
        panic!("expected recommended plugin text");
    };

    assert!(text.len() <= 4_160);
    assert!(!text.contains("Plugin 49"));
}

#[test]
fn proven_loop_terminal_generation_ends_unless_new_input_arrives() {
    let request = GenerationRequestDisposition {
        purpose: Some(TurnTimingGenerationPurpose::TerminalCompletionReasoning),
        sampling: SamplingGenerationDisposition::DecisionBearing,
        relevant_state_fingerprint: "state".to_string(),
        failure_fingerprint: None,
        terminal_completion_only: true,
    };

    assert!(!generation_needs_follow_up(&request, true, false));
    assert!(generation_needs_follow_up(&request, false, true));
}

#[test]
fn convergence_directive_is_consumed_before_compaction_can_replace_the_request() {
    let mut decision = SamplingConvergenceDecision {
        continuation: ContinuationDisposition::ModelRequired,
        directive: Some("change strategy before continuing".to_string()),
        proven_loop_activated: true,
        authoritative_wait: None,
    };

    assert_eq!(
        take_convergence_observation(Some(&mut decision)),
        (true, Some("change strategy before continuing".to_string()))
    );
    assert_eq!(
        take_convergence_observation(Some(&mut decision)),
        (false, None)
    );
}

#[test]
fn compaction_rebases_but_preserves_a_terminal_completion_request() {
    let request = GenerationRequestDisposition {
        purpose: Some(TurnTimingGenerationPurpose::ValidationInterpretation),
        sampling: SamplingGenerationDisposition::DecisionBearing,
        relevant_state_fingerprint: "before-compaction".to_string(),
        failure_fingerprint: Some("failure".to_string()),
        terminal_completion_only: true,
    };

    let rebased =
        rebase_generation_request_after_compaction(request, "after-compaction".to_string());

    assert_eq!(
        rebased.purpose,
        Some(TurnTimingGenerationPurpose::TerminalCompletionReasoning)
    );
    assert_eq!(
        rebased.sampling,
        SamplingGenerationDisposition::DecisionBearing
    );
    assert_eq!(rebased.relevant_state_fingerprint, "after-compaction");
    assert_eq!(rebased.failure_fingerprint.as_deref(), Some("failure"));
    assert!(rebased.terminal_completion_only);
}

#[test]
fn server_end_turn_false_completes_on_first_unchanged_signal() {
    assert!(protocol_resample_completion_allowed(
        /*server_resample_eligible*/ true,
    ));
    assert!(!protocol_resample_completion_allowed(
        /*server_resample_eligible*/ false,
    ));
}

#[test]
fn verified_turn_contract_after_agent_abort_preserves_completed_output_metadata() {
    let surfaced_result = SurfacedToolResult {
        adapter: "code_mode_cell".to_string(),
        value: serde_json::json!({"answer": 42}),
        canonical_message: Some("completed answer".to_string()),
    };

    let result = after_agent_abort_result(
        Some("completed answer".to_string()),
        Some(surfaced_result.clone()),
        true,
    );

    assert_eq!(
        result.last_agent_message.as_deref(),
        Some("completed answer")
    );
    assert_eq!(result.surfaced_result, Some(surfaced_result));
    assert!(result.required_tool_terminal.is_none());
    assert!(result.defer_pending_input);
}

#[test]
fn logical_generation_budget_allows_thirty_two_regular_and_one_terminal_generation() {
    let mut budget = LogicalGenerationBudget::default();
    for _ in 0..MAX_REGULAR_LOGICAL_GENERATIONS {
        assert_eq!(
            budget.admit(/*terminal_requested*/ false),
            LogicalGenerationAdmission::Regular
        );
    }
    assert_eq!(
        budget.admit(/*terminal_requested*/ false),
        LogicalGenerationAdmission::Terminal { forced: true }
    );
    assert!(LOGICAL_GENERATION_BUDGET_FORCED_TERMINAL_DIRECTIVE.contains("final tool-free"));
    assert!(LOGICAL_GENERATION_BUDGET_FORCED_TERMINAL_DIRECTIVE.contains("forced terminal"));
    assert_eq!(
        budget.admit(/*terminal_requested*/ false),
        LogicalGenerationAdmission::Exhausted
    );
}

#[tokio::test]
async fn forced_terminal_budget_boundary_is_visible_in_history_and_events() {
    let (session, turn_context, events) =
        crate::session::tests::make_session_and_context_with_rx().await;

    record_forced_terminal_budget_boundary(session.as_ref(), turn_context.as_ref()).await;

    let history = session.clone_history().await;
    assert!(history.raw_items().iter().any(|item| {
        matches!(
            item,
            ResponseItem::Message { role, content, .. }
                if role == "developer"
                    && content.iter().any(|item| matches!(
                        item,
                        ContentItem::InputText { text }
                            if text == LOGICAL_GENERATION_BUDGET_FORCED_TERMINAL_DIRECTIVE
                    ))
        )
    }));
    let warning = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let event = events
                .recv()
                .await
                .expect("forced-terminal event channel remains open");
            if let EventMsg::Warning(warning) = event.msg {
                break warning;
            }
        }
    })
    .await
    .expect("forced-terminal boundary emits a warning");
    assert_eq!(
        warning.message,
        LOGICAL_GENERATION_BUDGET_FORCED_TERMINAL_DIRECTIVE
    );
}

#[test]
fn completion_pending_input_stays_in_the_sampling_loop_when_capacity_remains() {
    let available = LogicalGenerationBudget::default();
    assert_eq!(
        completion_pending_input_disposition(&available, true),
        CompletionPendingInputDisposition::Continue
    );
    assert_eq!(
        completion_pending_input_disposition(&available, false),
        CompletionPendingInputDisposition::None
    );

    let mut no_regular_capacity = LogicalGenerationBudget::default();
    for _ in 0..MAX_REGULAR_LOGICAL_GENERATIONS {
        assert_eq!(
            no_regular_capacity.admit(/*terminal_requested*/ false),
            LogicalGenerationAdmission::Regular
        );
    }
    assert_eq!(
        completion_pending_input_disposition(&no_regular_capacity, true),
        CompletionPendingInputDisposition::Defer
    );
}

#[test]
fn logical_generation_budget_terminal_attempt_is_exactly_once() {
    let mut budget = LogicalGenerationBudget::default();
    assert_eq!(
        budget.admit(/*terminal_requested*/ true),
        LogicalGenerationAdmission::Terminal { forced: false }
    );
    assert_eq!(
        budget.admit(/*terminal_requested*/ true),
        LogicalGenerationAdmission::Exhausted
    );
}

#[test]
fn orchestration_audit_regular_follow_up_admission_has_one_budget_precedence_table() {
    let available = LogicalGenerationBudget::default();
    assert_eq!(
        regular_follow_up_admission(&available, false),
        RegularFollowUpAdmission::Admit
    );
    assert_eq!(
        regular_follow_up_admission(&available, true),
        RegularFollowUpAdmission::Exhausted
    );

    let mut exhausted = LogicalGenerationBudget::default();
    assert_eq!(
        exhausted.admit(/*terminal_requested*/ true),
        LogicalGenerationAdmission::Terminal { forced: false }
    );
    for _ in 0..MAX_REGULAR_LOGICAL_GENERATIONS {
        assert_eq!(
            exhausted.admit(/*terminal_requested*/ false),
            LogicalGenerationAdmission::Regular
        );
    }
    assert_eq!(
        regular_follow_up_admission(&exhausted, false),
        RegularFollowUpAdmission::Exhausted
    );
}

#[test]
fn used_terminal_then_regular_generation_limit_blocks_follow_up_before_input_drain() {
    let mut budget = LogicalGenerationBudget::default();
    assert_eq!(
        budget.admit(/*terminal_requested*/ true),
        LogicalGenerationAdmission::Terminal { forced: false }
    );
    for _ in 0..MAX_REGULAR_LOGICAL_GENERATIONS {
        assert_eq!(
            budget.admit(/*terminal_requested*/ false),
            LogicalGenerationAdmission::Regular
        );
    }
    let next_request = GenerationRequestDisposition {
        purpose: Some(TurnTimingGenerationPurpose::TerminalCompletionReasoning),
        sampling: SamplingGenerationDisposition::DecisionBearing,
        relevant_state_fingerprint: "budget-exhausted".to_string(),
        failure_fingerprint: None,
        terminal_completion_only: false,
    };

    assert!(budget.is_exhausted());
    assert!(generation_budget_blocks_follow_up(
        &budget,
        Some(&next_request)
    ));
}

#[tokio::test]
async fn terminal_generation_boundary_does_not_poll_or_drain_pending_input() {
    let mut budget = LogicalGenerationBudget::default();
    for _ in 0..MAX_REGULAR_LOGICAL_GENERATIONS {
        assert_eq!(
            budget.admit(/*terminal_requested*/ false),
            LogicalGenerationAdmission::Regular
        );
    }
    assert!(!budget.is_exhausted());

    let polled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let future_polled = Arc::clone(&polled);
    let drained = drain_pending_input_if_generation_available(&budget, async move {
        future_polled.store(true, Ordering::Release);
        vec![TurnInput::UserInput {
            content: vec![UserInput::Text {
                text: "must remain queued for a fresh turn".to_string(),
                text_elements: Vec::new(),
            }],
            client_id: None,
        }]
    })
    .await;

    assert_eq!(drained, Some(Vec::new()));
    assert!(!polled.load(Ordering::Acquire));
    assert_eq!(
        budget.admit(/*terminal_requested*/ false),
        LogicalGenerationAdmission::Terminal { forced: true }
    );
}

#[tokio::test]
async fn exhausted_generation_budget_does_not_poll_or_drain_pending_input() {
    let mut budget = LogicalGenerationBudget::default();
    for _ in 0..MAX_REGULAR_LOGICAL_GENERATIONS {
        assert_eq!(
            budget.admit(/*terminal_requested*/ false),
            LogicalGenerationAdmission::Regular
        );
    }
    assert_eq!(
        budget.admit(/*terminal_requested*/ false),
        LogicalGenerationAdmission::Terminal { forced: true }
    );
    assert!(budget.is_exhausted());

    let polled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let future_polled = Arc::clone(&polled);
    let drained = drain_pending_input_if_generation_available(&budget, async move {
        future_polled.store(true, Ordering::Release);
        vec![TurnInput::UserInput {
            content: vec![UserInput::Text {
                text: "must remain queued".to_string(),
                text_elements: Vec::new(),
            }],
            client_id: None,
        }]
    })
    .await;

    assert!(drained.is_none());
    assert!(!polled.load(Ordering::Acquire));
}

#[tokio::test]
async fn untyped_status_affecting_error_is_recorded_as_terminal() {
    let (session, turn_context, events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let error = ErrorEvent {
        message: "untyped terminal error".to_string(),
        codex_error_info: None,
    };
    assert!(error.affects_turn_status());

    session
        .send_event(turn_context.as_ref(), EventMsg::Error(error.clone()))
        .await;

    let event = tokio::time::timeout(Duration::from_secs(2), events.recv())
        .await
        .expect("untyped terminal error emits an event")
        .expect("event channel remains open");
    let EventMsg::Error(emitted_error) = event.msg else {
        panic!("expected untyped terminal error event");
    };
    assert_eq!(emitted_error, error);
    assert_eq!(
        turn_context.terminal_error.lock().await.as_ref(),
        Some(&error)
    );
}

#[tokio::test]
async fn generation_budget_exhaustion_emits_one_status_affecting_error() {
    let (session, turn_context, events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let mut reported = false;

    report_logical_generation_budget_exhausted(
        session.as_ref(),
        turn_context.as_ref(),
        &mut reported,
    )
    .await;
    report_logical_generation_budget_exhausted(
        session.as_ref(),
        turn_context.as_ref(),
        &mut reported,
    )
    .await;

    let event = tokio::time::timeout(Duration::from_secs(2), events.recv())
        .await
        .expect("generation budget exhaustion emits an error event")
        .expect("event channel remains open");
    let EventMsg::Error(error) = event.msg else {
        panic!("expected generation budget error event");
    };
    assert_eq!(error.message, LOGICAL_GENERATION_BUDGET_EXHAUSTED_MESSAGE);
    assert!(error.affects_turn_status());
    assert_eq!(
        turn_context.terminal_error.lock().await.as_ref(),
        Some(&error)
    );
    assert!(events.try_recv().is_err(), "error must be reported once");
}

#[tokio::test]
async fn planning_failure_records_initial_input_and_emits_status_affecting_error() {
    let (session, turn_context, events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let input = vec![TurnInput::UserInput {
        content: vec![UserInput::Text {
            text: "preserve this initial prompt".to_string(),
            text_elements: Vec::new(),
        }],
        client_id: Some("planning-failure-input".to_string()),
    }];

    finish_pending_turn_planning_failure(
        &session,
        &turn_context,
        &input,
        planning_failure("injected test failure"),
    )
    .await
    .expect("planning failure is surfaced as a terminal turn result");

    let error = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let event = events
                .recv()
                .await
                .expect("planning failure event channel remains open");
            if let EventMsg::Error(error) = event.msg {
                break error;
            }
        }
    })
    .await
    .expect("planning failure emits an error event");
    assert!(error.message.contains("pending-turn planning failure"));
    assert!(error.affects_turn_status());
    assert_eq!(
        turn_context.terminal_error.lock().await.as_ref(),
        Some(&error)
    );

    let history = session.clone_history().await;
    assert!(history.raw_items().iter().any(|item| {
        matches!(
            item,
            ResponseItem::Message { role, content, .. }
                if role == "user"
                    && content.iter().any(|item| matches!(
                        item,
                        ContentItem::InputText { text }
                            if text == "preserve this initial prompt"
                    ))
        )
    }));
}

#[tokio::test]
async fn lightweight_terminal_prompt_contract_removes_tools_and_parallel_dispatch() {
    let (_, turn_context) = crate::session::tests::make_session_and_context().await;
    let registry = ToolRegistry::from_tools(std::iter::empty::<
        Arc<dyn crate::tools::registry::CoreToolRuntime>,
    >());
    let router = ToolRouter::from_parts(
        registry,
        vec![codex_tools::ToolSpec::Function(
            codex_tools::ResponsesApiTool {
                name: "read_only_probe".to_string(),
                description: "probe".to_string(),
                strict: false,
                defer_loading: None,
                parameters: codex_tools::JsonSchema::object(
                    Default::default(),
                    None,
                    Some(false.into()),
                ),
                output_schema: None,
            },
        )],
    );
    let mut prompt = build_prompt(
        Vec::<ResponseItem>::new(),
        &router,
        &turn_context,
        BaseInstructions::default(),
    );
    assert!(!prompt.tools.specs().is_empty());

    enforce_terminal_prompt_contract(&mut prompt, /*terminal_completion_only*/ true);

    assert!(prompt.tools.specs().is_empty());
    assert!(!prompt.parallel_tool_calls);
}

#[test]
fn structured_action_change_compares_the_full_generation_disposition() {
    let request = GenerationRequestDisposition {
        purpose: Some(TurnTimingGenerationPurpose::TerminalCompletionReasoning),
        sampling: SamplingGenerationDisposition::DecisionBearing,
        relevant_state_fingerprint: "state-a".to_string(),
        failure_fingerprint: None,
        terminal_completion_only: false,
    };

    assert!(generation_request_action_changed(&request, None));
    assert!(!generation_request_action_changed(&request, Some(&request)));

    let mut changed_state = request.clone();
    changed_state.relevant_state_fingerprint = "state-b".to_string();
    assert!(generation_request_action_changed(
        &request,
        Some(&changed_state)
    ));

    let mut changed_terminal_contract = request.clone();
    changed_terminal_contract.terminal_completion_only = true;
    assert!(generation_request_action_changed(
        &request,
        Some(&changed_terminal_contract)
    ));
}

#[test]
fn authoritative_wait_terminal_surface_requires_explicit_owner_projection() {
    let without_projection = SamplingConvergenceDecision {
        continuation: ContinuationDisposition::SurfaceExistingResult,
        authoritative_wait: Some(AuthoritativeWaitResolution::Terminal(
            authoritative_wait_result(None),
        )),
        ..Default::default()
    };
    assert_eq!(
        authoritative_wait_terminal_surface(&without_projection),
        Some(SurfacedToolResult {
            adapter: "code_mode_cell".to_string(),
            value: serde_json::json!("arbitrary raw execution output"),
            canonical_message: None,
        }),
        "raw code-mode output must not become last_agent_message"
    );

    let with_projection = SamplingConvergenceDecision {
        continuation: ContinuationDisposition::SurfaceExistingResult,
        authoritative_wait: Some(AuthoritativeWaitResolution::Terminal(
            authoritative_wait_result(Some("owner-designated completion")),
        )),
        ..Default::default()
    };
    assert_eq!(
        authoritative_wait_terminal_surface(&with_projection),
        Some(SurfacedToolResult {
            adapter: "code_mode_cell".to_string(),
            value: serde_json::json!("arbitrary raw execution output"),
            canonical_message: Some("owner-designated completion".to_string()),
        })
    );
}

#[test]
fn blocked_authoritative_wait_never_enters_terminal_surface() {
    let blocked = SamplingConvergenceDecision {
        continuation: ContinuationDisposition::ModelRequired,
        authoritative_wait: Some(AuthoritativeWaitResolution::Blocked(
            authoritative_wait_result(Some("must not surface")),
        )),
        ..Default::default()
    };
    assert_eq!(authoritative_wait_terminal_surface(&blocked), None);
}

fn run_turn_multi_thread_test_with_stack<F, Fut, T>(test_name: &'static str, test: F) -> T
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = T> + 'static,
    T: Send + 'static,
{
    std::thread::Builder::new()
        .name(test_name.to_string())
        .stack_size(8 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_stack_size(8 * 1024 * 1024)
                .enable_all()
                .build()
                .expect("turn test runtime")
                .block_on(test())
        })
        .expect("turn test thread")
        .join()
        .expect("turn test thread panicked")
}

struct RewriteAgentMessageContributor;

struct TurnInputBudgetContributor {
    text: String,
}

struct EnvironmentEchoContributor;

struct CountingTurnInputContributor {
    poll_count: Arc<AtomicUsize>,
}

struct ExposureOnlyTool {
    name: &'static str,
    exposure: codex_extension_api::ToolExposure,
}

impl codex_extension_api::ToolExecutor<codex_extension_api::ToolCall> for ExposureOnlyTool {
    fn tool_name(&self) -> codex_extension_api::ToolName {
        codex_extension_api::ToolName::plain(self.name)
    }

    fn spec(&self) -> codex_extension_api::ToolSpec {
        panic!("exposure identity tests do not build tool schemas")
    }

    fn exposure(&self) -> codex_extension_api::ToolExposure {
        self.exposure
    }

    fn handle(
        &self,
        _call: codex_extension_api::ToolCall,
    ) -> codex_extension_api::ToolExecutorFuture<'_> {
        Box::pin(async { panic!("exposure identity tests do not dispatch tools") })
    }
}

#[derive(Clone)]
struct SignalCompletingTask {
    finish: CancellationToken,
}

impl SessionTask for SignalCompletingTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Regular
    }

    fn span_name(&self) -> &'static str {
        "session_task.phase_68_signal_completing"
    }

    fn run(
        self: Arc<Self>,
        _session: Arc<Session>,
        _ctx: Arc<TurnContext>,
        _input: Vec<TurnInput>,
        cancellation_token: CancellationToken,
    ) -> futures::future::BoxFuture<'static, SessionTaskResult> {
        Box::pin(async move {
            tokio::select! {
                _ = self.finish.cancelled() => {}
                _ = cancellation_token.cancelled() => {}
            }
            Ok(crate::tasks::TurnTaskResult::default())
        })
    }
}

impl TurnItemContributor for RewriteAgentMessageContributor {
    fn contribute<'a>(
        &'a self,
        _thread_store: &'a ExtensionData,
        _turn_store: &'a ExtensionData,
        item: &'a mut TurnItem,
    ) -> codex_extension_api::ExtensionFuture<'a, Result<(), String>> {
        Box::pin(async move {
            if let TurnItem::AgentMessage(agent_message) = item {
                agent_message.content = vec![AgentMessageContent::Text {
                    text: "plan contributed assistant text".to_string(),
                }];
            }
            Ok(())
        })
    }
}

impl TurnInputContributor for TurnInputBudgetContributor {
    fn contribute<'a>(
        &'a self,
        _input: TurnInputContext,
        _session_store: &'a ExtensionData,
        _thread_store: &'a ExtensionData,
        _turn_store: &'a ExtensionData,
    ) -> codex_extension_api::ExtensionFuture<
        'a,
        Vec<Box<dyn codex_extension_api::ContextualUserFragment + Send>>,
    > {
        Box::pin(async move {
            vec![
                Box::new(codex_context_fragments::RenderedContextFragment::new(
                    "user",
                    self.text.clone(),
                )) as Box<dyn codex_extension_api::ContextualUserFragment + Send>,
            ]
        })
    }
}

impl TurnInputContributor for EnvironmentEchoContributor {
    fn contribute<'a>(
        &'a self,
        input: TurnInputContext,
        _session_store: &'a ExtensionData,
        _thread_store: &'a ExtensionData,
        _turn_store: &'a ExtensionData,
    ) -> codex_extension_api::ExtensionFuture<
        'a,
        Vec<Box<dyn codex_extension_api::ContextualUserFragment + Send>>,
    > {
        Box::pin(async move {
            let environment = input
                .environments
                .first()
                .expect("primary turn environment should reach contributors");
            vec![
                Box::new(codex_context_fragments::RenderedContextFragment::new(
                    "user",
                    format!(
                        "extension-environment:{}:{}:{}",
                        environment.environment_id, environment.cwd, environment.is_primary
                    ),
                )) as Box<dyn codex_extension_api::ContextualUserFragment + Send>,
            ]
        })
    }
}

impl TurnInputContributor for CountingTurnInputContributor {
    fn contribute<'a>(
        &'a self,
        _input: TurnInputContext,
        _session_store: &'a ExtensionData,
        _thread_store: &'a ExtensionData,
        _turn_store: &'a ExtensionData,
    ) -> codex_extension_api::ExtensionFuture<
        'a,
        Vec<Box<dyn codex_extension_api::ContextualUserFragment + Send>>,
    > {
        self.poll_count.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Vec::new() })
    }
}

fn assistant_output_text(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: Some(ResponseItemId::with_suffix("msg", "1")),
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

#[test]
fn ordinary_continuation_precedence_is_stable() {
    assert_eq!(
        ordinary_continuation_cause(true, true, true),
        Some(ContinuationCause::ToolResult)
    );
    assert_eq!(
        ordinary_continuation_cause(false, true, true),
        Some(ContinuationCause::ServerEndTurnFalse)
    );
    assert_eq!(
        ordinary_continuation_cause(false, false, true),
        Some(ContinuationCause::PendingInput)
    );
    assert_eq!(ordinary_continuation_cause(false, false, false), None);
}

#[test]
fn finalized_router_reuse_requires_identical_coarse_exposure_identity() {
    let disabled = ToolExposureIdentity {
        goal_surface_state: GoalSurfaceState::Disabled,
        environment_mode: EnvironmentSurfaceMode::None,
        ..ToolExposureIdentity::default()
    };
    let router = ToolRouter::from_parts_with_warnings_and_identity(
        ToolRegistry::empty_for_test(),
        Vec::new(),
        Vec::new(),
        disabled.clone(),
    );

    assert!(finalized_router_matches_exposure(&router, &disabled));

    let inactive = ToolExposureIdentity {
        goal_surface_state: GoalSurfaceState::Inactive,
        ..disabled.clone()
    };
    assert!(!finalized_router_matches_exposure(&router, &inactive));

    let active = ToolExposureIdentity {
        goal_surface_state: GoalSurfaceState::Active,
        ..disabled.clone()
    };
    assert!(!finalized_router_matches_exposure(&router, &active));

    let ready_environment = ToolExposureIdentity {
        environment_mode: EnvironmentSurfaceMode::One,
        ..disabled.clone()
    };
    assert!(!finalized_router_matches_exposure(
        &router,
        &ready_environment
    ));

    let refreshed_mcp_catalog = ToolExposureIdentity {
        mcp_tool_catalog_revision: disabled.mcp_tool_catalog_revision + 1,
        ..disabled.clone()
    };
    assert!(!finalized_router_matches_exposure(
        &router,
        &refreshed_mcp_catalog
    ));

    let starting_environment = ToolExposureIdentity {
        environment_starting: true,
        ..disabled
    };
    assert!(!finalized_router_matches_exposure(
        &router,
        &starting_environment
    ));
}

#[tokio::test]
async fn finalized_router_reuse_rejects_stale_request_user_input_eligibility() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let turn = Arc::new(turn);
    let step_context = StepContext::for_test(Arc::clone(&turn));
    let current = current_dynamic_tool_exposure_identity(&session, step_context.as_ref()).await;
    let matching_identity = ToolExposureIdentity {
        agent_surface_stage: current.agent_surface_stage,
        extension_tool_surface_revision: current.extension_tool_surface_revision,
        mcp_tool_catalog_revision: current.mcp_tool_catalog_revision,
        mcp_resources_available: current.mcp_resources_available,
        request_user_input_eligible: current.request_user_input_eligible,
        collaboration_mode: current.collaboration_mode,
        environment_mode: current.environment_mode,
        environment_starting: current.environment_starting,
        ..ToolExposureIdentity::default()
    };
    let matching_router = ToolRouter::from_parts_with_warnings_and_identity(
        ToolRegistry::empty_for_test(),
        Vec::new(),
        Vec::new(),
        matching_identity.clone(),
    );
    assert!(
        finalized_router_matches_current_exposure(
            &session,
            step_context.as_ref(),
            &matching_router,
        )
        .await
    );

    let stale_router = ToolRouter::from_parts_with_warnings_and_identity(
        ToolRegistry::empty_for_test(),
        Vec::new(),
        Vec::new(),
        ToolExposureIdentity {
            request_user_input_eligible: !matching_identity.request_user_input_eligible,
            ..matching_identity.clone()
        },
    );
    assert!(
        !finalized_router_matches_current_exposure(&session, step_context.as_ref(), &stale_router,)
            .await
    );

    let stale_collaboration_mode_router = ToolRouter::from_parts_with_warnings_and_identity(
        ToolRegistry::empty_for_test(),
        Vec::new(),
        Vec::new(),
        ToolExposureIdentity {
            collaboration_mode: ModeKind::Plan,
            ..matching_identity
        },
    );
    assert!(
        !finalized_router_matches_current_exposure(
            &session,
            step_context.as_ref(),
            &stale_collaboration_mode_router,
        )
        .await
    );
}

#[test]
fn goal_surface_state_has_disabled_inactive_and_active_transitions() {
    let tool = |name, exposure| {
        Arc::new(ExposureOnlyTool { name, exposure })
            as Arc<dyn codex_extension_api::ToolExecutor<codex_extension_api::ToolCall>>
    };

    assert_eq!(goal_surface_state(&[]), GoalSurfaceState::Disabled);
    assert_eq!(
        goal_surface_state(&[tool(
            "create_goal",
            codex_extension_api::ToolExposure::Deferred,
        )]),
        GoalSurfaceState::Inactive
    );
    assert_eq!(
        goal_surface_state(&[
            tool("create_goal", codex_extension_api::ToolExposure::Deferred,),
            tool("get_goal", codex_extension_api::ToolExposure::Direct),
            tool("update_goal", codex_extension_api::ToolExposure::Direct),
        ]),
        GoalSurfaceState::Active
    );
}

#[test]
fn agent_surface_stage_depends_only_on_coarse_graph_and_binding_state() {
    assert_eq!(
        agent_surface_stage_from_snapshot(false, false, false),
        AgentSurfaceStage::Prohibited
    );
    assert_eq!(
        agent_surface_stage_from_snapshot(true, false, false),
        AgentSurfaceStage::SpawnOnly
    );
    assert_eq!(
        agent_surface_stage_from_snapshot(false, true, false),
        AgentSurfaceStage::Lifecycle
    );
    assert_eq!(
        agent_surface_stage_from_snapshot(false, false, true),
        AgentSurfaceStage::TypedAdministration
    );
    assert_eq!(
        agent_surface_stage_from_snapshot(true, true, false),
        AgentSurfaceStage::Lifecycle
    );
    assert_eq!(
        agent_surface_stage_from_snapshot(true, false, true),
        AgentSurfaceStage::TypedAdministration
    );
    assert_eq!(
        agent_surface_stage_from_snapshot(true, true, true),
        AgentSurfaceStage::TypedAdministration
    );

    // Running/waiting status, gates, targets, and capacity are deliberately absent from this
    // snapshot, so those fine-grained transitions cannot change the schema identity.
    assert_eq!(
        agent_surface_stage_from_snapshot(true, true, false),
        agent_surface_stage_from_snapshot(true, true, false)
    );
}

fn response_input_texts(items: &[ResponseItem]) -> Vec<&str> {
    let mut texts = Vec::new();
    for item in items {
        if let ResponseItem::Message { content, .. } = item {
            for content_item in content {
                if let ContentItem::InputText { text } = content_item {
                    texts.push(text.as_str());
                }
            }
        }
    }
    texts
}

#[test]
fn reasoning_governor_resets_for_every_accepted_context_change() {
    let user_input = TurnInput::UserInput {
        content: vec![UserInput::Text {
            text: "new instruction".to_string(),
            text_elements: Vec::new(),
        }],
        client_id: None,
    };
    let empty_user_input = TurnInput::UserInput {
        content: Vec::new(),
        client_id: None,
    };
    let response_item = TurnInput::ResponseItem(assistant_output_text("context"));
    let mailbox_item = TurnInput::InterAgentCommunication(InterAgentCommunication::new(
        AgentPath::root(),
        AgentPath::try_from("/root/worker").expect("agent path"),
        Vec::new(),
        "mailbox context".to_string(),
        true,
    ));

    assert!(resets_reasoning_governor(&user_input));
    assert!(!resets_reasoning_governor(&empty_user_input));
    assert!(resets_reasoning_governor(&response_item));
    assert!(resets_reasoning_governor(&mailbox_item));
}

#[test]
fn legacy_explicit_skill_items_share_one_hard_budget() {
    let max_bytes = codex_utils_string::approx_bytes_for_tokens(
        codex_context_fragments::MAX_MODEL_CONTEXT_TOKENS,
    );
    let items = build_bounded_skill_context_items([
        (
            "user",
            format!("legacy-skill-budget-first:{}", "x".repeat(max_bytes)),
        ),
        ("user", "legacy-skill-budget-second".to_string()),
    ]);
    let texts = response_input_texts(&items);

    assert!(texts.iter().map(|text| text.len()).sum::<usize>() <= max_bytes);
    assert!(
        texts
            .iter()
            .any(|text| text.starts_with("legacy-skill-budget-first:"))
    );
    assert!(
        texts
            .iter()
            .all(|text| !text.contains("legacy-skill-budget-second"))
    );
}

#[tokio::test]
async fn extension_turn_input_contributors_share_one_hard_budget() {
    let max_bytes = codex_utils_string::approx_bytes_for_tokens(
        codex_context_fragments::MAX_MODEL_CONTEXT_TOKENS,
    );
    let (mut session, turn_context) = crate::session::tests::make_session_and_context().await;
    let mut builder = codex_extension_api::ExtensionRegistryBuilder::new();
    builder.turn_input_contributor(Arc::new(TurnInputBudgetContributor {
        text: format!("turn-input-budget-first:{}", "x".repeat(max_bytes)),
    }));
    builder.turn_input_contributor(Arc::new(TurnInputBudgetContributor {
        text: "turn-input-budget-second".to_string(),
    }));
    session.services.extensions = Arc::new(builder.build());
    let session = Arc::new(session);
    let step_context = StepContext::for_test(Arc::new(turn_context));

    let items =
        build_extension_turn_input_items(&session, &step_context, &[], &CancellationToken::new())
            .await
            .expect("turn-input contributors should render");
    let texts = response_input_texts(&items);

    assert!(texts.iter().map(|text| text.len()).sum::<usize>() <= max_bytes);
    assert!(
        texts
            .iter()
            .any(|text| text.starts_with("turn-input-budget-first:"))
    );
    assert!(
        texts
            .iter()
            .all(|text| !text.contains("turn-input-budget-second"))
    );
}

#[tokio::test]
async fn extension_turn_input_contributors_receive_foreign_environment_uris() {
    let foreign_cwd = PathUri::parse("file:///usr/local/project").expect("POSIX cwd URI");
    assert!(
        foreign_cwd.to_abs_path().is_err(),
        "test cwd must be foreign to the host"
    );

    let (mut session, mut turn_context) = crate::session::tests::make_session_and_context().await;
    let environment = turn_context.environments.turn_environments[0].clone();
    turn_context.environments.turn_environments[0] =
        crate::session::turn_context::TurnEnvironment::new(
            "remote".to_string(),
            environment.environment,
            foreign_cwd.clone(),
            environment.shell,
        );
    let mut builder = codex_extension_api::ExtensionRegistryBuilder::new();
    builder.turn_input_contributor(Arc::new(EnvironmentEchoContributor));
    session.services.extensions = Arc::new(builder.build());
    let session = Arc::new(session);
    let step_context = StepContext::for_test(Arc::new(turn_context));

    let items =
        build_extension_turn_input_items(&session, &step_context, &[], &CancellationToken::new())
            .await
            .expect("foreign environment should render through extension context");
    let texts = response_input_texts(&items);

    assert_eq!(
        texts,
        vec![format!("extension-environment:remote:{foreign_cwd}:true")]
    );
}

#[test]
fn streamed_item_with_empty_id_gets_a_generated_id() -> Result<()> {
    run_turn_multi_thread_test_with_stack(
        "streamed_item_with_empty_id_gets_a_generated_id",
        streamed_item_with_empty_id_gets_a_generated_id_impl,
    )
}

async fn streamed_item_with_empty_id_gets_a_generated_id_impl() -> Result<()> {
    core_test_support::skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let test = test_codex().build(&server).await?;
    let response_mock = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("response-1"),
            responses::ev_message_item_added("", ""),
            responses::ev_output_text_delta("streamed"),
            responses::ev_assistant_message("", "streamed"),
            responses::ev_completed("response-1"),
        ]),
    )
    .await;

    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "stream a response".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;

    let started_id = core_test_support::wait_for_event_match(&test.codex, |event| match event {
        EventMsg::ItemStarted(event) => match &event.item {
            TurnItem::AgentMessage(item) => Some(item.id.clone()),
            _ => None,
        },
        _ => None,
    })
    .await;
    let completed_id = core_test_support::wait_for_event_match(&test.codex, |event| match event {
        EventMsg::ItemCompleted(event) => match &event.item {
            TurnItem::AgentMessage(item) => Some(item.id.clone()),
            _ => None,
        },
        _ => None,
    })
    .await;

    assert!(started_id.starts_with("msg_"));
    assert_eq!(started_id, completed_id);
    response_mock.single_request();
    Ok(())
}

fn non_openai_model_provider(server: &wiremock::MockServer) -> ModelProviderInfo {
    let mut provider = built_in_model_providers(/*openai_base_url*/ None)["openai"].clone();
    provider.name = "OpenAI (phase 68 test)".to_string();
    provider.base_url = Some(format!("{}/v1", server.uri()));
    provider.supports_websockets = false;
    provider
}

fn complete_compaction_summary(state: &str) -> String {
    format!(
        "## Goal\nresume the pending turn\n\n## Current state\n{state}\n\n## Completed work\nseed turn completed\n\n## Unresolved work\npending input remains\n\n## Evidence\nmock compaction response\n\n## Next action\nsample the pending input"
    )
}

fn write_one_shot_stop_hook(home: &Path) -> Result<()> {
    let script_path = home.join("phase_68_stop_hook.py");
    let counter_path = home.join("phase_68_stop_hook.count");
    let counter_path = serde_json::to_string(&counter_path.to_string_lossy())?;
    let script = format!(
        r#"import json
from pathlib import Path
import sys

json.load(sys.stdin)
counter_path = Path({counter_path})
if not counter_path.exists():
    counter_path.write_text("1", encoding="utf-8")
    print(json.dumps({{"decision": "block", "reason": "continue after evidence warning"}}))
else:
    print(json.dumps({{"systemMessage": "stop hook continuation complete"}}))
"#,
    );
    let command = format!("python3 \"{}\"", script_path.display());
    let command_windows = format!("python \"{}\"", script_path.display());
    let hooks = serde_json::json!({
        "hooks": {
            "Stop": [{
                "hooks": [{
                    "type": "command",
                    "command": command,
                    "commandWindows": command_windows,
                }]
            }]
        }
    });
    fs::write(script_path, script)?;
    fs::write(home.join("hooks.json"), hooks.to_string())?;
    Ok(())
}

#[tokio::test]
async fn drain_in_flight_returns_first_error_after_draining_remaining_futures() {
    let (session, turn_context) = crate::session::tests::make_session_and_context().await;
    let session = Arc::new(session);
    let remaining_future_polled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let remaining_future_polled_clone = Arc::clone(&remaining_future_polled);
    let mut in_flight: FuturesOrdered<BoxFuture<'static, InFlightToolResult>> =
        FuturesOrdered::new();
    in_flight.push_back(Box::pin(
        InFlightToolCall::from_test_future(
            "first",
            Box::pin(async { Err(CodexErr::Fatal("first tool failure".to_string())) }),
        )
        .into_future(),
    ));
    in_flight.push_back(Box::pin(
        InFlightToolCall::from_test_future(
            "second",
            Box::pin(async move {
                remaining_future_polled_clone.store(true, std::sync::atomic::Ordering::SeqCst);
                Err(CodexErr::Fatal("second tool failure".to_string()))
            }),
        )
        .into_future(),
    ));

    let error = drain_in_flight(&mut in_flight, Arc::clone(&session), Arc::new(turn_context))
        .await
        .expect_err("the first in-flight tool error should be returned");

    assert!(remaining_future_polled.load(std::sync::atomic::Ordering::SeqCst));
    assert!(matches!(
        error,
        CodexErr::Fatal(message) if message == "first tool failure"
    ));
    let history = session.clone_history().await;
    let failure_outputs = history
        .raw_items()
        .iter()
        .filter_map(|item| match item {
            ResponseItem::FunctionCallOutput {
                call_id, output, ..
            } if call_id == "first" || call_id == "second" => {
                Some((call_id.as_str(), output.success))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        failure_outputs,
        vec![("first", Some(false)), ("second", Some(false))],
        "every accepted failed call must persist one ordered terminal output before the error escapes"
    );
}

#[tokio::test]
async fn drain_in_flight_persists_each_ordered_output_before_later_tools_finish() {
    let (session, turn_context) = crate::session::tests::make_session_and_context().await;
    let session = Arc::new(session);
    let turn_context = Arc::new(turn_context);
    let (release_second_tx, release_second_rx) = tokio::sync::oneshot::channel();
    let drain_session = Arc::clone(&session);
    let drain_turn_context = Arc::clone(&turn_context);
    let drain = tokio::spawn(async move {
        let mut in_flight: FuturesOrdered<BoxFuture<'static, InFlightToolResult>> =
            FuturesOrdered::new();
        in_flight.push_back(Box::pin(
            InFlightToolCall::from_test_future(
                "first",
                Box::pin(async { Ok(synthetic_tool_result("first")) }),
            )
            .into_future(),
        ));
        in_flight.push_back(Box::pin(
            InFlightToolCall::from_test_future(
                "second",
                Box::pin(async move {
                    let _ = release_second_rx.await;
                    Ok(synthetic_tool_result("second"))
                }),
            )
            .into_future(),
        ));
        drain_in_flight(&mut in_flight, drain_session, drain_turn_context).await
    });

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let history = session.clone_history().await;
            if history.raw_items().iter().any(|item| {
                matches!(
                    item,
                    ResponseItem::ToolSearchOutput { call_id, .. } if call_id.as_deref() == Some("first")
                )
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the first ordered output should be durable while a later tool is still running");

    release_second_tx
        .send(())
        .expect("the second tool should still be waiting");
    drain
        .await
        .expect("the drain task should join")
        .expect("both tool outputs should be delivered");
}

#[tokio::test]
async fn drain_in_flight_commits_post_tool_context_after_its_output() {
    let (session, turn_context) = crate::session::tests::make_session_and_context().await;
    let session = Arc::new(session);
    let turn_context = Arc::new(turn_context);
    turn_context
        .queue_post_tool_contexts(
            "annotated",
            vec![ResponseItem::Message {
                id: None,
                role: "developer".to_string(),
                content: vec![ContentItem::InputText {
                    text: "post-tool context".to_string(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            }],
        )
        .await;
    let mut in_flight: FuturesOrdered<BoxFuture<'static, InFlightToolResult>> =
        FuturesOrdered::new();
    in_flight.push_back(Box::pin(
        InFlightToolCall::from_test_future(
            "annotated",
            Box::pin(async { Ok(synthetic_tool_result("annotated")) }),
        )
        .into_future(),
    ));

    drain_in_flight(
        &mut in_flight,
        Arc::clone(&session),
        Arc::clone(&turn_context),
    )
    .await
    .expect("the annotated output should be delivered");

    let history = session.clone_history().await;
    let ordered = history
        .raw_items()
        .iter()
        .filter_map(|item| match item {
            ResponseItem::ToolSearchOutput { call_id, .. }
                if call_id.as_deref() == Some("annotated") =>
            {
                Some("output")
            }
            ResponseItem::Message { role, content, .. }
                if role == "developer"
                    && content.iter().any(|item| {
                        matches!(item, ContentItem::InputText { text } if text == "post-tool context")
                    }) =>
            {
                Some("context")
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(ordered, vec!["output", "context"]);
}

#[test]
fn function_calls_select_argument_diff_consumers() {
    let item = ResponseItem::FunctionCall {
        id: None,
        name: "apply_patch".to_string(),
        namespace: Some("workspace".to_string()),
        arguments: String::new(),
        call_id: "function-diff".to_string(),
        internal_chat_message_metadata_passthrough: None,
    };

    let (call_id, tool_name) = tool_argument_diff_target(&item)
        .expect("function tools should be eligible for argument diff streaming");

    assert_eq!(call_id, "function-diff");
    assert_eq!(
        tool_name,
        ToolName::new(Some("workspace".to_string()), "apply_patch")
    );
}

#[tokio::test]
async fn drain_in_flight_returns_earliest_required_terminal_after_persisting_all_outputs() {
    use crate::tools::context::RequiredToolTerminal;
    use crate::tools::context::RequiredToolTerminalCause;
    use crate::tools::parallel::ToolCallCompletion;

    let (session, turn_context) = crate::session::tests::make_session_and_context().await;
    let session = Arc::new(session);
    let turn_context = Arc::new(turn_context);
    let mut first = InFlightToolCall::from_test_future(
        "first-terminal",
        Box::pin(async { Ok(synthetic_tool_result("first-terminal")) }),
    )
    .into_future()
    .await;
    first.result = Ok(ToolCallCompletion {
        response: synthetic_tool_result("first-terminal"),
        required_terminal: Some(RequiredToolTerminal {
            call_id: "first-terminal".to_string(),
            cause: RequiredToolTerminalCause::Failure,
            message: "first required failure".to_string(),
        }),
    });
    let mut second = InFlightToolCall::from_test_future(
        "second-terminal",
        Box::pin(async { Ok(synthetic_tool_result("second-terminal")) }),
    )
    .into_future()
    .await;
    second.result = Ok(ToolCallCompletion {
        response: synthetic_tool_result("second-terminal"),
        required_terminal: Some(RequiredToolTerminal {
            call_id: "second-terminal".to_string(),
            cause: RequiredToolTerminalCause::TimedOut,
            message: "second required timeout".to_string(),
        }),
    });
    let mut in_flight: FuturesOrdered<BoxFuture<'static, InFlightToolResult>> =
        FuturesOrdered::new();
    in_flight.push_back(Box::pin(async move { first }));
    in_flight.push_back(Box::pin(async move { second }));

    let terminal = drain_in_flight(
        &mut in_flight,
        Arc::clone(&session),
        Arc::clone(&turn_context),
    )
    .await
    .expect("semantic failures are terminal results, not relay errors")
    .expect("a required terminal result should be returned");

    assert_eq!(terminal.call_id, "first-terminal");
    let history = session.clone_history().await;
    let persisted_call_ids = history
        .raw_items()
        .iter()
        .filter_map(|item| match item {
            ResponseItem::ToolSearchOutput { call_id, .. }
                if matches!(
                    call_id.as_deref(),
                    Some("first-terminal" | "second-terminal")
                ) =>
            {
                call_id.as_deref()
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        persisted_call_ids,
        vec!["first-terminal", "second-terminal"]
    );
    assert_eq!(
        turn_context
            .durable_history_completed_commits
            .lock()
            .await
            .len(),
        2,
        "each completed tool must commit its ordered output without waiting for later tools"
    );
}

#[tokio::test]
async fn drain_in_flight_keeps_successful_delivery_independent_of_telemetry_state() {
    let (session, turn_context) = crate::session::tests::make_session_and_context().await;
    let mut in_flight: FuturesOrdered<BoxFuture<'static, InFlightToolResult>> =
        FuturesOrdered::new();
    in_flight.push_back(Box::pin(
        InFlightToolCall::from_test_future(
            "successful",
            Box::pin(async { Ok(synthetic_tool_result("successful")) }),
        )
        .into_future(),
    ));

    drain_in_flight(&mut in_flight, Arc::new(session), Arc::new(turn_context))
        .await
        .expect("successful tool delivery must not depend on telemetry-only state");
}

#[tokio::test]
async fn drain_in_flight_rollout_failure_does_not_attest_persistence_or_mutate_history() {
    let (mut session, turn_context) = crate::session::tests::make_session_and_context().await;
    crate::session::tests::attach_thread_persistence(&mut session).await;
    session
        .live_thread()
        .expect("test session should have live persistence")
        .shutdown()
        .await
        .expect("test thread store should shut down");

    let session = Arc::new(session);
    let turn_context = Arc::new(turn_context);
    let call_id = "durability-failure";
    let call = InFlightToolCall::from_test_future(
        call_id,
        Box::pin(async move { Ok(synthetic_tool_result(call_id)) }),
    );
    let execution_id = call.execution_id.clone();
    call.timing.record_outcome("success");
    turn_context.turn_timing_state.mark_turn_started();
    turn_context.turn_timing_state.record_accepted_tool_call(
        call_id,
        &execution_id,
        codex_protocol::protocol::TurnTimingToolCallSource::Direct,
        None,
    );
    turn_context.turn_timing_state.record_tool_dispatch_timing(
        call_id,
        "test_tool",
        codex_protocol::protocol::TurnTimingToolCallSource::Direct,
        crate::turn_timing::ToolCallTimingLineage::default(),
        call.timing.snapshot(tokio::time::Instant::now()),
    );
    let history_before = session.clone_history().await.raw_items().to_vec();
    let mut in_flight: FuturesOrdered<BoxFuture<'static, InFlightToolResult>> =
        FuturesOrdered::new();
    in_flight.push_back(Box::pin(call.into_future()));

    let error = drain_in_flight(
        &mut in_flight,
        Arc::clone(&session),
        Arc::clone(&turn_context),
    )
    .await
    .expect_err("rollout append failure must fail the relay before history mutation");

    assert!(matches!(
        error,
        CodexErr::Fatal(message) if message.contains("failed to durably append tool output")
    ));
    let history_after = session.clone_history().await;
    assert_eq!(history_after.raw_items(), history_before.as_slice());
    let closure = turn_context.turn_timing_state.tool_closure_snapshot();
    assert_eq!(
        (
            closure.accepted_count,
            closure.timing_paired_count,
            closure.terminal_count,
            closure.persisted_count,
        ),
        (1, 1, 1, 0)
    );
    assert_eq!(closure.unresolved_calls.len(), 1);
    assert_eq!(closure.unresolved_calls[0].call_id, call_id);
    assert_eq!(closure.unresolved_calls[0].execution_id, execution_id);
    assert!(!closure.complete);
}

#[tokio::test]
async fn drain_in_flight_persists_stale_relay_as_terminal_failure() {
    let (session, turn_context) = crate::session::tests::make_session_and_context().await;
    let session = Arc::new(session);
    let mut stale = InFlightToolCall::from_test_future(
        "stale",
        Box::pin(async { Ok(synthetic_tool_result("stale")) }),
    )
    .into_future()
    .await;
    stale.execution_id = codex_protocol::protocol::ToolExecutionId("stale-execution".to_string());
    let mut in_flight: FuturesOrdered<BoxFuture<'static, InFlightToolResult>> =
        FuturesOrdered::new();
    in_flight.push_back(Box::pin(async move { stale }));

    let error = drain_in_flight(&mut in_flight, Arc::clone(&session), Arc::new(turn_context))
        .await
        .expect_err("a stale relay must fail the turn after closing its accepted call");

    assert!(matches!(
        error,
        CodexErr::Fatal(message) if message.contains("stale tool relay completion for stale")
    ));
    let history = session.clone_history().await;
    let stale_outputs = history
        .raw_items()
        .iter()
        .filter_map(|item| match item {
            ResponseItem::FunctionCallOutput {
                call_id, output, ..
            } if call_id == "stale" => Some(output.success),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        stale_outputs,
        vec![Some(false)],
        "a stale accepted call must persist exactly one terminal output"
    );
}

#[tokio::test]
async fn steering_applies_next_turn_settings_without_building_a_candidate_turn_context() {
    let (session, turn_context, _events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let next_model = codex_models_manager::bundled_models_response()
        .expect("bundled model catalog should parse")
        .models
        .into_iter()
        .find(|model| model.slug != turn_context.model_info.slug)
        .expect("bundled model catalog should contain an alternative model")
        .slug;
    let active_approval_policy = turn_context.approval_policy.value();
    let active_permission_profile = turn_context.permission_profile.clone();
    let next_approval_policy = if active_approval_policy == AskForApproval::Never {
        AskForApproval::OnRequest
    } else {
        AskForApproval::Never
    };
    let next_permission_profile = if active_permission_profile == PermissionProfile::Disabled {
        PermissionProfile::read_only()
    } else {
        PermissionProfile::Disabled
    };
    session
        .services
        .thread_extension_data
        .insert(turn_context.model_info.clone());
    let model_info_before = session
        .services
        .thread_extension_data
        .get::<codex_protocol::openai_models::ModelInfo>()
        .expect("thread model info should be initialized");
    let finish = CancellationToken::new();
    session
        .spawn_task(
            Arc::clone(&turn_context),
            Vec::new(),
            SignalCompletingTask {
                finish: finish.clone(),
            },
        )
        .await;

    crate::session::handlers::user_input_or_turn_inner(
        &session,
        "steering-submission".to_string(),
        Op::UserInput {
            items: vec![UserInput::Text {
                text: "steer the active turn".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: ThreadSettingsOverrides {
                approval_policy: Some(next_approval_policy),
                permission_profile: Some(next_permission_profile.clone()),
                collaboration_mode: Some(codex_protocol::config_types::CollaborationMode {
                    mode: codex_protocol::config_types::ModeKind::Default,
                    settings: codex_protocol::config_types::Settings {
                        model: next_model.clone(),
                        reasoning_effort: None,
                        developer_instructions: None,
                    },
                }),
                ..Default::default()
            },
        },
        /*client_user_message_id*/ None,
    )
    .await;

    let model_info_after = session
        .services
        .thread_extension_data
        .get::<codex_protocol::openai_models::ModelInfo>()
        .expect("thread model info should remain initialized");
    assert!(
        Arc::ptr_eq(&model_info_before, &model_info_after),
        "a successful steer must not build a candidate context or replace thread model metadata"
    );
    assert_eq!(session.collaboration_mode().await.model(), next_model);
    let active_context = session
        .turn_context_for_sub_id(&turn_context.sub_id)
        .await
        .expect("the original turn should remain active");
    assert!(Arc::ptr_eq(&active_context, &turn_context));
    assert_eq!(
        active_context.approval_policy.value(),
        active_approval_policy,
        "steering settings must not rebind the active turn's MCP approval policy"
    );
    assert_eq!(
        active_context.permission_profile, active_permission_profile,
        "steering settings must not rebind the active turn's MCP permission profile"
    );

    let terminal = session
        .active_turn
        .lock()
        .await
        .as_ref()
        .and_then(|active_turn| active_turn.terminal.clone())
        .expect("active turn should expose its terminal coordinator");
    finish.cancel();
    terminal.wait_completed().await;

    let next_context = session
        .new_default_turn_with_sub_id("next-turn-after-steer".to_string())
        .await;
    assert_eq!(next_context.model_info.slug, next_model);
    assert_eq!(
        next_context.approval_policy.value(),
        next_approval_policy,
        "the next actual turn must install the steered MCP approval policy"
    );
    assert_eq!(
        next_context.permission_profile, next_permission_profile,
        "the next actual turn must install the steered MCP permission profile"
    );
}

#[test]
fn initial_response_item_triggers_compaction_before_the_stream_request() -> Result<()> {
    run_turn_multi_thread_test_with_stack(
        "initial_response_item_triggers_compaction_before_the_stream_request",
        initial_response_item_triggers_compaction_before_the_stream_request_impl,
    )
}

async fn initial_response_item_triggers_compaction_before_the_stream_request_impl() -> Result<()> {
    core_test_support::skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let request_log = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("seed-response-item-response"),
                responses::ev_assistant_message("seed-response-item-message", "seed complete"),
                responses::ev_completed_with_tokens(
                    "seed-response-item-response",
                    /*total_tokens*/ 21_000,
                ),
            ]),
            responses::sse(vec![
                responses::ev_response_created("response-item-compact-response"),
                responses::ev_assistant_message(
                    "response-item-compact-message",
                    &complete_compaction_summary("initial response context compacted"),
                ),
                responses::ev_completed_with_tokens(
                    "response-item-compact-response",
                    /*total_tokens*/ 20,
                ),
            ]),
            responses::sse(vec![
                responses::ev_response_created("response-item-final-response"),
                responses::ev_assistant_message(
                    "response-item-final-message",
                    "initial response item sampled",
                ),
                responses::ev_completed_with_tokens(
                    "response-item-final-response",
                    /*total_tokens*/ 42,
                ),
            ]),
        ],
    )
    .await;
    let provider = non_openai_model_provider(&server);
    let mut builder = test_codex().with_config(move |config| {
        config.model_provider = provider;
        config.model_context_window = Some(100_000);
        config.model_auto_compact_token_limit = Some(22_000);
        config.model_auto_compact_token_limit_scope =
            codex_protocol::config_types::AutoCompactTokenLimitScope::Total;
        config.model_provider.request_max_retries = Some(0);
        config.model_provider.stream_max_retries = Some(0);
    });
    let test = builder.build(&server).await?;

    test.submit_turn("seed committed history near the compaction limit")
        .await?;
    while tokio::time::timeout(Duration::from_millis(10), test.codex.next_event())
        .await
        .is_ok()
    {}
    test.codex
        .submit(Op::UserInput {
            items: Vec::new(),
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: IndexMap::from([(
                "phase-68-large-initial-response-item".to_string(),
                AdditionalContextEntry {
                    value: "large model-visible response context ".repeat(128),
                    kind: AdditionalContextKind::Application,
                },
            )]),
            thread_settings: Default::default(),
        })
        .await?;

    tokio::time::timeout(Duration::from_secs(15), async {
        while request_log.requests().len() < 3 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the final sampling request should follow pre-turn compaction");
    let submitted_turn_id = request_log.requests()[2].body_json()["client_metadata"]["turn_id"]
        .as_str()
        .expect("final request turn id")
        .to_string();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            match test.codex.next_event().await.expect("turn event").msg {
                EventMsg::Error(error) => {
                    panic!(
                        "response-item turn failed during pre-turn compaction: {}",
                        error.message
                    )
                }
                EventMsg::TurnComplete(turn) if submitted_turn_id == turn.turn_id => {
                    assert!(
                        turn.error.is_none(),
                        "response-item turn completed with an error: {:?}",
                        turn.error
                    );
                    break;
                }
                _ => {}
            }
        }
    })
    .await
    .expect("the response-item turn should complete after pre-turn compaction");

    let request_count = request_log.requests().len();
    assert_eq!(
        request_count, 3,
        "the large initial ResponseItem must trigger compaction before the turn's sampling request"
    );
    Ok(())
}

#[test]
fn oversized_pending_input_compacts_once_when_committed_history_is_also_over_limit() -> Result<()> {
    run_turn_multi_thread_test_with_stack(
        "oversized_pending_input_compacts_once_when_committed_history_is_also_over_limit",
        oversized_pending_input_compacts_once_when_committed_history_is_also_over_limit_impl,
    )
}

async fn oversized_pending_input_compacts_once_when_committed_history_is_also_over_limit_impl()
-> Result<()> {
    core_test_support::skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let request_log = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("seed-response"),
                responses::ev_assistant_message("seed-message", "seed complete"),
                responses::ev_completed_with_tokens("seed-response", /*total_tokens*/ 23_000),
            ]),
            responses::sse(vec![
                responses::ev_response_created("compact-response"),
                responses::ev_assistant_message(
                    "compact-message",
                    &complete_compaction_summary("oversized pending input compacted"),
                ),
                responses::ev_completed_with_tokens("compact-response", /*total_tokens*/ 20),
            ]),
            responses::sse(vec![
                responses::ev_response_created("final-response"),
                responses::ev_assistant_message("final-message", "pending input sampled"),
                responses::ev_completed_with_tokens("final-response", /*total_tokens*/ 42),
            ]),
        ],
    )
    .await;
    let provider = non_openai_model_provider(&server);
    let pending_plan_builds = Arc::new(AtomicUsize::new(0));
    let mut extension_builder = codex_extension_api::ExtensionRegistryBuilder::new();
    extension_builder.turn_input_contributor(Arc::new(CountingTurnInputContributor {
        poll_count: Arc::clone(&pending_plan_builds),
    }));
    let mut builder = test_codex()
        .with_extensions(Arc::new(extension_builder.build()))
        .with_config(move |config| {
            config.model_provider = provider;
            config.model_context_window = Some(100_000);
            config.model_auto_compact_token_limit = Some(22_000);
            config.model_auto_compact_token_limit_scope =
                codex_protocol::config_types::AutoCompactTokenLimitScope::Total;
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(0);
        });
    let test = builder.build(&server).await?;

    test.submit_turn("seed committed history").await?;
    while tokio::time::timeout(Duration::from_millis(10), test.codex.next_event())
        .await
        .is_ok()
    {}
    pending_plan_builds.store(0, Ordering::SeqCst);
    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "oversized pending payload ".repeat(128),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    tokio::time::timeout(Duration::from_secs(15), async {
        while request_log.requests().len() < 3 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the final sampling request should follow pending-input compaction");
    let submitted_turn_id = request_log.requests()[2].body_json()["client_metadata"]["turn_id"]
        .as_str()
        .expect("final request turn id")
        .to_string();
    loop {
        match test.codex.next_event().await.expect("turn event").msg {
            EventMsg::Error(error) => {
                panic!(
                    "oversized pending-input turn failed during compaction: {}",
                    error.message
                )
            }
            EventMsg::TurnComplete(turn) if submitted_turn_id == turn.turn_id => {
                assert!(
                    turn.error.is_none(),
                    "oversized pending-input turn completed with an error: {:?}",
                    turn.error
                );
                break;
            }
            _ => {}
        }
    }

    assert_eq!(
        request_log.requests().len(),
        3,
        "the second turn should compact once, then sample instead of repeatedly compacting the same pending payload"
    );
    assert_eq!(
        pending_plan_builds.load(Ordering::SeqCst),
        2,
        "compaction should invalidate the initial pure projection and rebuild the pending plan against compacted history"
    );
    Ok(())
}

#[test]
fn pending_plan_and_router_reuse_one_step_mcp_inventory_snapshot() -> Result<()> {
    run_turn_multi_thread_test_with_stack(
        "pending_plan_and_router_reuse_one_step_mcp_inventory_snapshot",
        pending_plan_and_router_reuse_one_step_mcp_inventory_snapshot_impl,
    )
}

async fn pending_plan_and_router_reuse_one_step_mcp_inventory_snapshot_impl() -> Result<()> {
    let command = match core_test_support::stdio_server_bin() {
        Ok(command) => command,
        Err(err) => {
            tracing::warn!(
                %err,
                "test_stdio_server unavailable; skipping MCP snapshot regression"
            );
            return Ok(());
        }
    };
    let (mut session, mut turn_context, _events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let auth_manager =
        AuthManager::from_auth_for_testing(CodexAuth::create_dummy_chatgpt_auth_for_testing());
    Arc::get_mut(&mut session)
        .expect("test session should be uniquely owned")
        .services
        .auth_manager = Arc::clone(&auth_manager);
    let turn = Arc::get_mut(&mut turn_context).expect("test turn should be uniquely owned");
    turn.auth_manager = Some(auth_manager);
    turn.model_info.supports_search_tool = false;
    let config = Arc::make_mut(&mut turn.config);
    config
        .features
        .enable(Feature::Apps)
        .expect("apps feature should be configurable in tests");
    let _ = config.features.disable(Feature::ToolSuggest);
    config.orchestrator_mcp_enabled = true;
    let mut servers = config.mcp_servers.get().clone();
    servers.insert(
        "snapshot".to_string(),
        McpServerConfig {
            auth: Default::default(),
            transport: McpServerTransportConfig::Stdio {
                command,
                args: Vec::new(),
                env: None,
                env_vars: Vec::new(),
                cwd: None,
            },
            environment_id: codex_config::DEFAULT_MCP_SERVER_ENVIRONMENT_ID.to_string(),
            enabled: true,
            required: false,
            supports_parallel_tool_calls: false,
            disabled_reason: None,
            startup_timeout_sec: Some(Duration::from_secs(10)),
            tool_timeout_sec: None,
            default_tools_approval_mode: None,
            enabled_tools: None,
            disabled_tools: None,
            scopes: None,
            oauth: None,
            oauth_resource: None,
            tools: HashMap::new(),
        },
    );
    config
        .mcp_servers
        .set(servers)
        .expect("test MCP server configuration should be accepted");
    let refresh_config = config.clone();
    session
        .refresh_mcp_servers_now(
            turn_context.as_ref(),
            &refresh_config,
            Some(session.mcp_elicitation_reviewer()),
        )
        .await;
    assert!(
        session
            .services
            .latest_mcp_runtime()
            .manager()
            .wait_for_server_ready("snapshot", Duration::from_secs(10))
            .await,
        "snapshot MCP server should become ready"
    );

    let step_context = session
        .capture_step_context(Arc::clone(&turn_context))
        .await;
    const SNAPSHOT_APP_ID: &str = "phase68-snapshot-app";
    const SNAPSHOT_APP_NAME: &str = "Phase 68 Snapshot App";
    const SNAPSHOT_TOOL_NAMESPACE: &str = "mcp__codex_apps__phase_68_snapshot_app";
    assert!(
        !step_context
            .mcp
            .manager()
            .list_all_tools()
            .await
            .iter()
            .any(|tool| tool.connector_id.as_deref() == Some(SNAPSHOT_APP_ID)),
        "the live manager inventory must intentionally differ from the seeded step snapshot"
    );
    step_context
        .seed_mcp_tools_for_test(vec![ToolInfo {
            server_name: CODEX_APPS_MCP_SERVER_NAME.to_string(),
            supports_parallel_tool_calls: false,
            server_origin: None,
            callable_name: "search".to_string(),
            callable_namespace: SNAPSHOT_TOOL_NAMESPACE.to_string(),
            namespace_description: None,
            tool: Tool::new_with_raw("search".to_string(), None, Arc::new(JsonObject::default())),
            connector_id: Some(SNAPSHOT_APP_ID.to_string()),
            connector_name: Some(SNAPSHOT_APP_NAME.to_string()),
            plugin_display_names: Vec::new(),
        }])
        .await;
    let input = vec![TurnInput::UserInput {
        content: vec![UserInput::Text {
            text: format!("use [$snapshot](app://{SNAPSHOT_APP_ID})"),
            text_elements: Vec::new(),
        }],
        client_id: None,
    }];
    let cancellation_token = CancellationToken::new();
    let planning_generation = session.services.planning_generation();
    let PendingTurnPlanBuild::Ready(plan) = build_pure_pending_turn_plan(
        &session,
        Arc::clone(&step_context),
        &input,
        planning_generation,
        &cancellation_token,
    )
    .await?
    else {
        panic!("stable test inputs should produce a ready pending-turn plan");
    };
    assert!(
        plan.projected_prompt_pressure.total_tokens
            > estimate_pending_tokens(
                &input,
                &[],
                &[],
                plan.first_router.as_ref(),
                /*initial_context*/ true,
            )
            .total_tokens,
        "first-turn planning must account for full context before compaction"
    );
    assert!(plan.step_context.turn.apps_enabled());
    assert_eq!(
        plan.mentioned_apps,
        vec![(
            SNAPSHOT_APP_ID.to_string(),
            Some(SNAPSHOT_APP_NAME.to_string())
        )],
        "planning must resolve app mentions from the same seeded StepContext inventory as routing"
    );

    let (snapshot_ptr, snapshot_len) = {
        let tools = plan.step_context.mcp_tools().await;
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].connector_id.as_deref(), Some(SNAPSHOT_APP_ID));
        (tools.as_ptr(), tools.len())
    };
    let cached_tools = plan.step_context.mcp_tools().await;
    assert_eq!(cached_tools.as_ptr(), snapshot_ptr);
    assert_eq!(cached_tools.len(), snapshot_len);
    let router_tool_names = plan
        .first_router
        .model_visible_specs()
        .iter()
        .map(|spec| spec.name().to_string())
        .collect::<Vec<_>>();
    assert!(
        router_tool_names.iter().any(|name| {
            name == SNAPSHOT_TOOL_NAMESPACE || name == &format!("{SNAPSHOT_TOOL_NAMESPACE}.search")
        }),
        "the advertised router must be built from the seeded StepContext inventory; expected namespace {SNAPSHOT_TOOL_NAMESPACE:?}, got {router_tool_names:?}"
    );
    Ok(())
}

fn wait_for_concurrent_state_attempt(attempted: &std::sync::atomic::AtomicBool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !attempted.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert!(
        attempted.load(Ordering::Acquire),
        "concurrent state attempt did not start"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_plan_commit_and_invalidation_share_the_session_state_owner() {
    let (session, turn_context, _events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let step_context = session
        .capture_step_context(Arc::clone(&turn_context))
        .await;
    let stale_generation = session.services.planning_generation();
    let history_before = session.clone_history().await.into_raw_items();
    assert!(
        session
            .state
            .lock()
            .await
            .pending_context_baseline()
            .is_none()
    );

    // Invalidation owns SessionState first. The pending commit cannot perform
    // its final comparison until after generation N has been invalidated.
    let commit = {
        let mut state_owner = session.state.lock().await;
        let commit_attempted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let commit_attempted_task = Arc::clone(&commit_attempted);
        let commit_session = Arc::clone(&session);
        let commit_step_context = Arc::clone(&step_context);
        let commit = tokio::spawn(async move {
            commit_attempted_task.store(true, Ordering::Release);
            commit_session
                .compare_and_record_context_updates(commit_step_context.as_ref(), stale_generation)
                .await
        });
        wait_for_concurrent_state_attempt(&commit_attempted);
        assert!(!commit.is_finished());
        session
            .services
            .advance_planning_generation(&mut state_owner);
        commit
    };
    assert!(commit.await.expect("commit task completed").is_none());
    assert_eq!(
        session.clone_history().await.into_raw_items(),
        history_before
    );
    assert!(
        session
            .state
            .lock()
            .await
            .pending_context_baseline()
            .is_none()
    );

    // Prepare a real context candidate, then replay the exact synchronous
    // compare-and-commit primitive while retaining SessionState ownership.
    let current_generation = session.services.planning_generation();
    let step_context = session
        .capture_step_context(Arc::clone(&turn_context))
        .await;
    assert!(
        session
            .compare_and_record_context_updates(step_context.as_ref(), current_generation)
            .await
            .is_some()
    );
    let invalidation = {
        let mut state_owner = session.state.lock().await;
        let candidate = state_owner
            .pending_context_baseline()
            .expect("successful context commit stages its baseline");
        state_owner.clear_pending_context_baseline();
        let marker = ResponseItem::Message {
            id: None,
            role: "developer".to_string(),
            content: Vec::new(),
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        };
        let history_len_before_commit = state_owner.clone_history().into_raw_items().len();
        let invalidation_attempted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let invalidation_attempted_task = Arc::clone(&invalidation_attempted);
        let invalidation_session = Arc::clone(&session);
        let invalidation = tokio::spawn(async move {
            invalidation_attempted_task.store(true, Ordering::Release);
            let mut state_owner = invalidation_session.state.lock().await;
            invalidation_session
                .services
                .advance_planning_generation(&mut state_owner)
        });
        wait_for_concurrent_state_attempt(&invalidation_attempted);
        assert!(!invalidation.is_finished());

        assert!(
            session
                .compare_and_commit_planning_state(
                    &mut state_owner,
                    Some(current_generation),
                    |state| {
                        state.record_items(
                            std::iter::once(&marker),
                            turn_context.model_info.truncation_policy.into(),
                        );
                        state.stage_context_baseline(candidate);
                    },
                )
                .is_some()
        );
        assert_eq!(
            state_owner.clone_history().into_raw_items().len(),
            history_len_before_commit + 1
        );
        assert!(state_owner.pending_context_baseline().is_some());
        assert!(!invalidation.is_finished());
        invalidation
    };

    let next_generation = invalidation.await.expect("invalidation task completed");
    assert!(next_generation > current_generation);
}

#[tokio::test]
async fn realized_context_commits_only_the_bound_physical_attempt() {
    let (session, turn_context, _events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let step_context = session
        .capture_step_context(Arc::clone(&turn_context))
        .await;
    session
        .record_context_updates_and_set_reference_context_item(step_context.as_ref())
        .await;

    assert!(
        session
            .bind_context_baseline_candidate("request-1", "attempt-1")
            .await
    );
    assert!(
        session
            .bind_context_baseline_candidate("request-1", "attempt-2")
            .await
    );
    assert!(
        !session
            .commit_context_baseline_candidate("request-1", "attempt-1")
            .await
            .expect("stale attempt must be ignored")
    );
    assert!(session.reference_context_item().await.is_none());

    assert!(
        session
            .commit_context_baseline_candidate("request-1", "attempt-2")
            .await
            .expect("matching attempt commits")
    );
    let realized = session
        .reference_context_item()
        .await
        .expect("matching accepted attempt realizes context");
    let provenance = realized
        .context_provenance
        .expect("realized context records accepted attempt provenance");
    assert_eq!(provenance.accepted_attempt.sampling_request_id, "request-1");
    assert_eq!(provenance.accepted_attempt.physical_attempt_id, "attempt-2");
    assert!(!provenance.fragment_digests.is_empty());
    assert!(
        !session
            .commit_context_baseline_candidate("request-1", "attempt-2")
            .await
            .expect("duplicate Created must be ignored")
    );
}

#[tokio::test]
async fn pending_plan_rebuilds_after_generation_changes_during_planning() -> Result<()> {
    let (session, turn_context, _events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let history_before = session.clone_history().await.into_raw_items();
    let planning_generation = session.services.planning_generation();
    let stale_step_context = session
        .capture_step_context(Arc::clone(&turn_context))
        .await;
    {
        let mut state_owner = session.state.lock().await;
        session
            .services
            .advance_planning_generation(&mut state_owner);
    }

    let build = build_pure_pending_turn_plan(
        &session,
        stale_step_context,
        &[],
        planning_generation,
        &CancellationToken::new(),
    )
    .await?;
    assert!(matches!(build, PendingTurnPlanBuild::Stale));
    assert_eq!(
        session.clone_history().await.into_raw_items(),
        history_before
    );

    let rebuilt_generation = session.services.planning_generation();
    let rebuilt_step_context = session.capture_step_context(turn_context).await;
    let rebuilt = build_pure_pending_turn_plan(
        &session,
        rebuilt_step_context,
        &[],
        rebuilt_generation,
        &CancellationToken::new(),
    )
    .await?;
    let PendingTurnPlanBuild::Ready(rebuilt) = rebuilt else {
        panic!("a plan rebuilt from the current generation should be ready");
    };
    assert_eq!(rebuilt.planning_generation, rebuilt_generation);
    Ok(())
}

#[test]
fn pending_token_estimate_includes_model_visible_tool_schemas() {
    let empty_registry = crate::tools::registry::ToolRegistry::from_tools(std::iter::empty::<
        Arc<dyn crate::tools::registry::CoreToolRuntime>,
    >());
    let empty_router = ToolRouter::from_parts(empty_registry, Vec::new());
    let schema_description = "schema context ".repeat(1024);
    let schema_registry = crate::tools::registry::ToolRegistry::from_tools(std::iter::empty::<
        Arc<dyn crate::tools::registry::CoreToolRuntime>,
    >());
    let schema_router = ToolRouter::from_parts(
        schema_registry,
        vec![codex_tools::ToolSpec::Function(
            codex_tools::ResponsesApiTool {
                name: "large_schema_tool".to_string(),
                description: schema_description,
                strict: false,
                defer_loading: None,
                parameters: codex_tools::JsonSchema::object(
                    Default::default(),
                    None,
                    Some(false.into()),
                ),
                output_schema: None,
            },
        )],
    );

    let baseline =
        estimate_pending_tokens(&[], &[], &[], &empty_router, /*initial_context*/ false);
    let with_schema = estimate_pending_tokens(
        &[],
        &[],
        &[],
        &schema_router,
        /*initial_context*/ false,
    );

    assert!(
        with_schema.total_tokens > baseline.total_tokens + 3_000,
        "model-visible schema bytes must materially increase pre-turn context estimation"
    );
    assert_eq!(
        with_schema.body_growth_tokens, baseline.body_growth_tokens,
        "stable tool schemas must not count as body-after-prefix growth"
    );
}

#[test]
fn pending_token_estimate_excludes_stable_startup_injections_from_body_growth() {
    let empty_registry = crate::tools::registry::ToolRegistry::from_tools(std::iter::empty::<
        Arc<dyn crate::tools::registry::CoreToolRuntime>,
    >());
    let empty_router = ToolRouter::from_parts(empty_registry, Vec::new());
    let baseline =
        estimate_pending_tokens(&[], &[], &[], &empty_router, /*initial_context*/ false);
    let guidance = ContextualUserFragment::into(TaskModelGuidance);
    let with_guidance = estimate_pending_tokens(
        &[],
        &[guidance],
        &[],
        &empty_router,
        /*initial_context*/ false,
    );

    assert!(with_guidance.total_tokens > baseline.total_tokens);
    assert_eq!(
        with_guidance.body_growth_tokens,
        baseline.body_growth_tokens
    );
}

#[test]
fn pending_injection_byte_count_serializes_each_item_once_with_array_overhead() {
    let stable = ContextualUserFragment::into(TaskModelGuidance);
    let dynamic = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![codex_protocol::models::ContentItem::InputText {
            text: "dynamic injection".repeat(256),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    let items = [stable, dynamic.clone()];

    let (total, body) =
        injection_serialized_lengths(&items).expect("count injection serialization");

    assert_eq!(
        total,
        serde_json::to_vec(&items)
            .expect("serialize comparison injection")
            .len(),
    );
    assert_eq!(
        body,
        serde_json::to_vec(&dynamic)
            .expect("serialize comparison body item")
            .len(),
    );
}

#[test]
fn stop_hook_continuation_reaches_the_final_response() -> Result<()> {
    run_turn_multi_thread_test_with_stack(
        "stop_hook_continuation_reaches_the_final_response",
        stop_hook_continuation_reaches_the_final_response_impl,
    )
}

async fn stop_hook_continuation_reaches_the_final_response_impl() -> Result<()> {
    core_test_support::skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let response_log = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("plan-response"),
                responses::ev_function_call(
                    "plan-call",
                    "update_plan",
                    &serde_json::json!({
                        "plan": [{
                            "step": "exercise stop-hook continuation",
                            "status": "in_progress"
                        }]
                    })
                    .to_string(),
                ),
                responses::ev_completed("plan-response"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("draft-response"),
                responses::ev_assistant_message("draft-message", "draft answer"),
                responses::ev_completed("draft-response"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("final-response"),
                responses::ev_assistant_message("final-message", "final answer"),
                responses::ev_completed("final-response"),
            ]),
        ],
    )
    .await;
    let mut builder = test_codex()
        .with_pre_build_hook(|home| {
            write_one_shot_stop_hook(home).expect("write stop-hook fixture");
        })
        .with_config(trust_discovered_hooks);
    let test = builder.build(&server).await?;

    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "answer, then continue after the stop hook".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;

    let mut saw_final_response = false;
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let event = test.codex.next_event().await.expect("turn event");
            match event.msg {
                EventMsg::AgentMessage(message) if message.message == "final answer" => {
                    saw_final_response = true;
                }
                EventMsg::TurnComplete(_) => break,
                _ => {}
            }
        }
    })
    .await
    .expect("turn should finish after one stop-hook continuation");
    assert!(saw_final_response);
    assert_eq!(response_log.requests().len(), 3);
    Ok(())
}

#[test]
fn models_etag_refresh_does_not_block_tool_continuation() -> Result<()> {
    run_turn_multi_thread_test_with_stack(
        "models_etag_refresh_does_not_block_tool_continuation",
        models_etag_refresh_does_not_block_tool_continuation_impl,
    )
}

async fn models_etag_refresh_does_not_block_tool_continuation_impl() -> Result<()> {
    core_test_support::skip_if_no_network!(Ok(()));
    const REFRESH_ETAG: &str = "\"phase-68-models-2\"";

    let server = responses::start_mock_server().await;
    let mut builder = test_codex()
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_model("gpt-5.2")
        .with_config(|config| {
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(0);
            let _ = config.features.disable(Feature::Apps);
        });
    let test = builder.build(&server).await?;

    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(5))
                .insert_header("etag", REFRESH_ETAG)
                .set_body_json(ModelsResponse { models: Vec::new() }),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    let response_log = responses::mount_response_sequence(
        &server,
        vec![
            responses::sse_response(responses::sse(vec![
                responses::ev_response_created("etag-tool-response"),
                responses::ev_function_call(
                    "etag-plan-call",
                    "update_plan",
                    &serde_json::json!({
                        "plan": [{
                            "id": "etag-continuation",
                            "step": "continue while the model catalog refresh is pending",
                            "status": "implemented",
                            "acceptance_criteria": ["the second model request is dispatched"],
                            "runtime_paths": ["core/src/session/turn.rs"]
                        }]
                    })
                    .to_string(),
                ),
                responses::ev_completed("etag-tool-response"),
            ]))
            .insert_header("X-Models-Etag", REFRESH_ETAG),
            responses::sse_response(responses::sse(vec![
                responses::ev_response_created("etag-final-response"),
                responses::ev_assistant_message(
                    "etag-final-message",
                    "tool continuation completed",
                ),
                responses::ev_completed("etag-final-response"),
            ])),
        ],
    )
    .await;

    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "exercise ETag refresh during a tool continuation".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let model_requests = server
                .received_requests()
                .await
                .unwrap_or_default()
                .into_iter()
                .filter(|request| request.url.path() == "/v1/models")
                .count();
            if model_requests >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("background models refresh should start");

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let event = test.codex.next_event().await.expect("stream event");
            match event.msg {
                EventMsg::AgentMessage(ref message)
                    if message.message == "tool continuation completed" =>
                {
                    break;
                }
                EventMsg::TurnComplete(_) => {
                    panic!("turn completed without the continuation response")
                }
                _ => {}
            }
        }
    })
    .await
    .expect("the second model request must not wait for the delayed models refresh");
    assert_eq!(response_log.requests().len(), 2);
    Ok(())
}

#[tokio::test]
async fn unchanged_model_and_comp_hash_skip_previous_model_context_reconstruction() -> Result<()> {
    core_test_support::skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(ModelsResponse { models: Vec::new() }),
        )
        .mount(&server)
        .await;

    let (mut session, mut turn_context) = crate::session::tests::make_session_and_context().await;
    let auth_manager =
        AuthManager::from_auth_for_testing(CodexAuth::create_dummy_chatgpt_auth_for_testing());
    let mut config = (*turn_context.config).clone();
    config.model_provider.base_url = Some(format!("{}/v1", server.uri()));
    config.model_provider.supports_websockets = false;
    let config = Arc::new(config);
    session.services.auth_manager = Arc::clone(&auth_manager);
    session.services.models_manager = crate::test_support::models_manager_with_provider(
        config.codex_home.to_path_buf(),
        Arc::clone(&auth_manager),
        config.model_provider.clone(),
    );
    turn_context.auth_manager = Some(auth_manager);
    turn_context.config = config;
    session
        .set_previous_turn_settings(Some(crate::session::PreviousTurnSettings {
            model: turn_context.model_info.slug.clone(),
            comp_hash: turn_context.model_info.comp_hash.clone(),
        }))
        .await;
    let mut client_session = session.services.model_client.new_session();
    let session = Arc::new(session);
    let turn_context = Arc::new(turn_context);

    assert!(
        !maybe_run_previous_model_inline_compact(
            &session,
            &turn_context,
            &mut client_session,
            &CancellationToken::new(),
        )
        .await?
    );
    let model_requests = server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|request| request.url.path() == "/v1/models")
        .count();
    assert_eq!(
        model_requests, 0,
        "unchanged settings should return before TurnContext::with_model fetches the catalog"
    );
    Ok(())
}

#[tokio::test]
async fn plan_mode_uses_contributed_turn_item_for_last_agent_message() {
    let (mut session, turn_context) = crate::session::tests::make_session_and_context().await;
    let mut builder = codex_extension_api::ExtensionRegistryBuilder::new();
    builder.turn_item_contributor(Arc::new(RewriteAgentMessageContributor));
    session.services.extensions = Arc::new(builder.build());
    let turn_store = ExtensionData::new(turn_context.sub_id.clone());
    let mut state = PlanModeStreamState::new(&turn_context.sub_id);
    let mut last_agent_message = None;
    let item = assistant_output_text("original assistant text");

    let handled = handle_assistant_item_done_in_plan_mode(
        &session,
        &turn_context,
        &turn_store,
        &item,
        &mut state,
        /*previously_active_item*/ None,
        &mut last_agent_message,
    )
    .await;

    assert!(handled);
    assert_eq!(
        last_agent_message.as_deref(),
        Some("plan contributed assistant text")
    );
}

fn synthetic_tool_result(call_id: &str) -> ResponseInputItem {
    ResponseInputItem::ToolSearchOutput {
        call_id: call_id.to_string(),
        status: "completed".to_string(),
        execution: "client".to_string(),
        tools: Vec::new(),
        omitted_result_count: None,
    }
}

fn controlled_tool_future(
    call_id: &'static str,
    first_poll: tokio::sync::oneshot::Sender<tokio::time::Instant>,
    release: tokio::sync::oneshot::Receiver<()>,
) -> BoxFuture<'static, CodexResult<ResponseInputItem>> {
    Box::pin(async move {
        let _ = first_poll.send(tokio::time::Instant::now());
        let _ = release.await;
        Ok(synthetic_tool_result(call_id))
    })
}

fn controlled_tool_call(
    call_id: &'static str,
    first_poll: tokio::sync::oneshot::Sender<tokio::time::Instant>,
    release: tokio::sync::oneshot::Receiver<()>,
) -> InFlightToolCall {
    InFlightToolCall::from_test_future(
        call_id,
        controlled_tool_future(call_id, first_poll, release),
    )
}

#[tokio::test]
async fn non_eager_tool_future_waits_for_the_response_tail_to_close() {
    let response_tail_closed = CancellationToken::new();
    let (first_poll_tx, mut first_poll_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let mut in_flight: FuturesOrdered<BoxFuture<'static, InFlightToolResult>> =
        FuturesOrdered::new();
    in_flight.push_back(defer_tool_future_until_response_tail(
        controlled_tool_call("deferred", first_poll_tx, release_rx),
        response_tail_closed.clone(),
    ));

    let result_task = tokio::spawn(async move {
        in_flight
            .next()
            .await
            .expect("deferred tool result should exist")
    });
    tokio::task::yield_now().await;
    assert!(matches!(
        first_poll_rx.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));

    response_tail_closed.cancel();
    first_poll_rx
        .await
        .expect("tool execution should start after the response tail closes");
    release_tx
        .send(())
        .expect("deferred tool should still be attached");
    let result = result_task
        .await
        .expect("deferred tool task should finish")
        .result
        .expect("deferred tool should succeed")
        .response;
    assert_eq!(result, synthetic_tool_result("deferred"));
}

#[tokio::test(start_paused = true)]
async fn eligible_direct_tool_calls_overlap_a_response_tail_without_changing_results() {
    const RESPONSE_TAIL: Duration = Duration::from_millis(250);
    let baseline_item_accepted = tokio::time::Instant::now();
    let (baseline_first_poll_tx, mut baseline_first_poll_rx) = tokio::sync::oneshot::channel();
    let (baseline_release_tx, baseline_release_rx) = tokio::sync::oneshot::channel();
    let mut baseline: FuturesOrdered<BoxFuture<'static, CodexResult<ResponseInputItem>>> =
        FuturesOrdered::new();
    baseline.push_back(controlled_tool_future(
        "read-1",
        baseline_first_poll_tx,
        baseline_release_rx,
    ));

    tokio::task::yield_now().await;
    assert!(matches!(
        baseline_first_poll_rx.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));
    tokio::time::advance(RESPONSE_TAIL).await;
    let baseline_result_task =
        tokio::spawn(async move { baseline.next().await.expect("baseline result should exist") });
    let baseline_first_poll = baseline_first_poll_rx
        .await
        .expect("baseline future should be polled");
    assert_eq!(
        baseline_first_poll.duration_since(baseline_item_accepted),
        RESPONSE_TAIL
    );
    baseline_release_tx
        .send(())
        .expect("baseline tool should still be attached");
    let baseline_result = baseline_result_task
        .await
        .expect("baseline result task should finish")
        .expect("baseline tool should succeed");

    let eager_item_accepted = tokio::time::Instant::now();
    let (eager_first_poll_tx, eager_first_poll_rx) = tokio::sync::oneshot::channel();
    let (eager_release_tx, eager_release_rx) = tokio::sync::oneshot::channel();
    let mut eager: FuturesOrdered<BoxFuture<'static, InFlightToolResult>> = FuturesOrdered::new();
    eager.push_back(start_eager_tool_future(controlled_tool_call(
        "read-1",
        eager_first_poll_tx,
        eager_release_rx,
    )));

    let eager_first_poll = eager_first_poll_rx
        .await
        .expect("eager future should start before the response tail is released");
    assert_eq!(
        eager_first_poll.duration_since(eager_item_accepted),
        Duration::ZERO
    );
    let (stream_tail_completed_tx, stream_tail_completed_rx) = tokio::sync::oneshot::channel();
    let stream_tail = tokio::spawn(async move {
        tokio::time::sleep(RESPONSE_TAIL).await;
        let _ = stream_tail_completed_tx.send(tokio::time::Instant::now());
    });
    tokio::task::yield_now().await;
    tokio::time::advance(RESPONSE_TAIL).await;
    let eager_tail_completed = stream_tail_completed_rx
        .await
        .expect("the model-response tail should continue while the tool is blocked");
    stream_tail.await.expect("stream-tail task should finish");
    assert_eq!(
        eager_tail_completed.duration_since(eager_first_poll),
        RESPONSE_TAIL
    );

    // The simulated stream tail completed while tool work was still deliberately blocked.
    // Model continuation remains behind the ordered tool-result barrier.
    let next_sampling_started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let next_sampling_started_after_drain = Arc::clone(&next_sampling_started);
    let continuation = tokio::spawn(async move {
        let result = eager
            .next()
            .await
            .expect("eager result should exist")
            .result
            .expect("eager tool should succeed")
            .response;
        next_sampling_started_after_drain.store(true, Ordering::SeqCst);
        result
    });
    tokio::task::yield_now().await;
    assert!(!next_sampling_started.load(Ordering::SeqCst));
    eager_release_tx
        .send(())
        .expect("eager tool should still be attached");
    let eager_result = continuation
        .await
        .expect("continuation barrier task should finish");

    assert_eq!(eager_result, baseline_result);
    assert!(next_sampling_started.load(Ordering::SeqCst));
}

#[tokio::test]
async fn eager_tool_results_remain_in_call_order_after_reverse_completion() {
    let (first_started_tx, first_started_rx) = tokio::sync::oneshot::channel();
    let (first_release_tx, first_release_rx) = tokio::sync::oneshot::channel();
    let (second_started_tx, second_started_rx) = tokio::sync::oneshot::channel();
    let (second_release_tx, second_release_rx) = tokio::sync::oneshot::channel();
    let mut in_flight: FuturesOrdered<BoxFuture<'static, InFlightToolResult>> =
        FuturesOrdered::new();
    in_flight.push_back(start_eager_tool_future(controlled_tool_call(
        "first",
        first_started_tx,
        first_release_rx,
    )));
    in_flight.push_back(start_eager_tool_future(controlled_tool_call(
        "second",
        second_started_tx,
        second_release_rx,
    )));

    first_started_rx.await.expect("first tool should start");
    second_started_rx.await.expect("second tool should start");
    second_release_tx
        .send(())
        .expect("second tool should still be attached");
    tokio::task::yield_now().await;
    first_release_tx
        .send(())
        .expect("first tool should still be attached");

    let first = in_flight
        .next()
        .await
        .expect("first result")
        .result
        .expect("success")
        .response;
    let second = in_flight
        .next()
        .await
        .expect("second result")
        .result
        .expect("success")
        .response;
    assert_eq!(first, synthetic_tool_result("first"));
    assert_eq!(second, synthetic_tool_result("second"));
}

#[tokio::test(start_paused = true)]
async fn eager_tool_failure_can_be_collected_before_response_streaming_finishes() {
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let future: BoxFuture<'static, CodexResult<ResponseInputItem>> = Box::pin(async move {
        let _ = started_tx.send(());
        let _ = release_rx.await;
        Err(CodexErr::Fatal("synthetic eager tool failure".to_string()))
    });
    let mut in_flight: FuturesOrdered<BoxFuture<'static, InFlightToolResult>> =
        FuturesOrdered::new();
    in_flight.push_back(start_eager_tool_future(InFlightToolCall::from_test_future(
        "failure", future,
    )));

    started_rx.await.expect("eager tool should start");
    let (tail_finished_tx, mut tail_finished_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let _ = tail_finished_tx.send(());
    });

    release_tx
        .send(())
        .expect("failed eager tool should remain attached until collection");
    let error = in_flight
        .next()
        .await
        .expect("failed result should retain its ordered slot")
        .result
        .expect_err("synthetic tool should fail");
    assert!(error.to_string().contains("synthetic eager tool failure"));
    assert!(matches!(
        tail_finished_rx.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));
}

struct DropSignal(Option<tokio::sync::oneshot::Sender<()>>);

impl Drop for DropSignal {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

#[tokio::test]
async fn dropping_in_flight_collection_aborts_eager_tool_work() {
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
    let future: BoxFuture<'static, CodexResult<ResponseInputItem>> = Box::pin(async move {
        let _drop_signal = DropSignal(Some(dropped_tx));
        let _ = started_tx.send(());
        std::future::pending::<()>().await;
        unreachable!("aborted eager work must not resume")
    });
    let mut in_flight: FuturesOrdered<BoxFuture<'static, InFlightToolResult>> =
        FuturesOrdered::new();
    in_flight.push_back(start_eager_tool_future(InFlightToolCall::from_test_future(
        "drop", future,
    )));

    started_rx.await.expect("eager work should start");
    drop(in_flight);
    dropped_rx
        .await
        .expect("dropping the collection must abort, not detach, eager work");
}
#[test]
fn terminal_surface_preserves_typed_owner_result_without_json_message_synthesis() {
    let value = serde_json::json!({"status": "complete", "nested": {"answer": 42}});
    let decision = SamplingConvergenceDecision {
        continuation: ContinuationDisposition::SurfaceExistingResult,
        authoritative_wait: Some(AuthoritativeWaitResolution::Terminal(
            AuthoritativeWaitOwnerResult {
                adapter: "owner_adapter".to_string(),
                value: value.clone(),
                surfaceable_message: None,
            },
        )),
        ..Default::default()
    };

    let surfaced = authoritative_wait_terminal_surface(&decision).expect("typed surface");
    assert_eq!(surfaced.adapter, "owner_adapter");
    assert_eq!(surfaced.value, value);
    assert_eq!(surfaced.canonical_message, None);
}

#[test]
fn terminal_surface_preserves_owner_canonical_message_exactly() {
    let canonical = "  owner-authored completion\n";
    let decision = SamplingConvergenceDecision {
        continuation: ContinuationDisposition::SurfaceExistingResult,
        authoritative_wait: Some(AuthoritativeWaitResolution::Terminal(
            AuthoritativeWaitOwnerResult {
                adapter: "owner_adapter".to_string(),
                value: serde_json::json!({"message": "different structured value"}),
                surfaceable_message: Some(canonical.to_string()),
            },
        )),
        ..Default::default()
    };

    let surfaced = authoritative_wait_terminal_surface(&decision).expect("typed surface");
    assert_eq!(surfaced.canonical_message.as_deref(), Some(canonical));
}

#[test]
fn projected_prompt_pressure_does_not_add_stable_tools_to_server_usage_twice() {
    assert_eq!(
        projected_prompt_tokens_from_estimates(
            /*active_context_tokens*/ 900, /*committed_history_tokens*/ 500,
            /*pending_token_estimate*/ 450, /*pending_body_growth_tokens*/ 50,
        ),
        950
    );
    assert_eq!(
        projected_prompt_tokens_from_estimates(
            /*active_context_tokens*/ 1_200, /*committed_history_tokens*/ 500,
            /*pending_token_estimate*/ 450, /*pending_body_growth_tokens*/ 0,
        ),
        1_200
    );
}

#[test]
fn projected_prompt_pressure_adds_pending_body_growth_to_server_usage() {
    assert_eq!(
        projected_prompt_tokens_from_estimates(
            /*active_context_tokens*/ 1_200, /*committed_history_tokens*/ 500,
            /*pending_token_estimate*/ 450, /*pending_body_growth_tokens*/ 50,
        ),
        1_250
    );
}

#[test]
fn plan_mode_memory_citations_are_parsed_once_for_live_events() {
    let mut state = PlanModeStreamState::new("turn-1");
    let raw = "<citation_entries>\nMEMORY.md:1-2|note=[x]\n</citation_entries>\n<rollout_ids>\n019cc2ea-1dff-7902-8d40-c8f6e5d83cc4\n</rollout_ids>";

    let citation =
        take_new_memory_citation(&mut state, vec![raw.to_string()]).expect("valid memory citation");
    assert_eq!(citation.entries.len(), 1);
    assert_eq!(citation.entries[0].path, "MEMORY.md");
    assert_eq!(
        citation.rollout_ids,
        vec!["019cc2ea-1dff-7902-8d40-c8f6e5d83cc4"]
    );
    assert_eq!(
        take_new_memory_citation(&mut state, vec![raw.to_string()]),
        None
    );
}

#[test]
fn pending_turn_mechanism_retries_remain_bounded_without_fixed_point_state() {
    let mut iterations = 0;
    for _ in 0..MAX_PENDING_TURN_PLAN_ITERATIONS {
        advance_pending_turn_plan_iteration(&mut iterations).expect("within retry bound");
    }
    assert!(advance_pending_turn_plan_iteration(&mut iterations).is_err());
}

#[test]
fn pending_turn_stale_builds_consume_iteration_budget() {
    let mut iterations = 0;

    for _ in 0..MAX_PENDING_TURN_PLAN_ITERATIONS {
        charge_pending_turn_plan_build((), &mut iterations).expect("within retry bound");
    }

    assert!(charge_pending_turn_plan_build((), &mut iterations).is_err());
}

#[test]
fn pending_turn_mechanism_does_not_replay_the_completed_mcp_effect() {
    let completed = ("install:tool@v1".to_string(), None);
    assert!(mcp_dependency_effect_is_completed(
        Some(&completed),
        "install:tool@v1"
    ));
    assert!(!mcp_dependency_effect_is_completed(
        Some(&completed),
        "install:tool@v2"
    ));
}

#[test]
fn pending_turn_mechanism_inventory_effect_requires_a_newer_generation() {
    assert!(require_newer_planning_generation(4, 4).is_err());
    assert!(require_newer_planning_generation(4, 5).is_ok());
}

#[tokio::test]
async fn post_sampling_state_reads_start_concurrently() {
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let pending_barrier = Arc::clone(&barrier);
    let token_barrier = Arc::clone(&barrier);

    let values = tokio::time::timeout(
        Duration::from_millis(200),
        collect_post_sampling_state(
            async move {
                pending_barrier.wait().await;
                "pending"
            },
            async move {
                token_barrier.wait().await;
                "tokens"
            },
        ),
    )
    .await
    .expect("independent post-sampling reads should both start without waiting for the other");

    assert_eq!(values, ("pending", "tokens"));
}

#[tokio::test]
async fn plugin_recommendation_and_mcp_tool_reads_start_concurrently() {
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let recommendation_barrier = Arc::clone(&barrier);
    let mcp_barrier = Arc::clone(&barrier);

    let values = tokio::time::timeout(
        Duration::from_millis(200),
        join_recommendations_and_mcp(
            async move {
                recommendation_barrier.wait().await;
                "recommendations"
            },
            async move {
                mcp_barrier.wait().await;
                "mcp"
            },
        ),
    )
    .await
    .expect("independent recommendation and MCP reads should overlap");

    assert_eq!(values, ("recommendations", "mcp"));
}

#[tokio::test]
async fn projected_prompt_state_reads_start_concurrently() {
    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let active_barrier = Arc::clone(&barrier);
    let history_barrier = Arc::clone(&barrier);
    let compact_barrier = Arc::clone(&barrier);

    let values = tokio::time::timeout(
        Duration::from_millis(200),
        collect_projected_prompt_state(
            async move {
                active_barrier.wait().await;
                "active"
            },
            async move {
                history_barrier.wait().await;
                "history"
            },
            async move {
                compact_barrier.wait().await;
                "compact"
            },
        ),
    )
    .await
    .expect("independent prompt-pressure state reads should overlap");

    assert_eq!(values, ("active", "history", "compact"));
}
