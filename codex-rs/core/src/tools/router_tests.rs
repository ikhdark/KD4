use std::collections::HashSet;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use crate::agent::task_capabilities::TypedToolClass;
use crate::config::Config;
use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context;
use crate::session::turn_context::TurnEnvironment;
use crate::tools::context::ToolDispatchState;
use crate::tools::context::ToolPayload;
use crate::tools::exposure::GoalSurfaceState;
use crate::tools::exposure::ToolExposureIdentity;
use crate::tools::registry::ToolRegistry;
use crate::turn_diff_tracker::TurnDiffTracker;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionRegistry;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::ResponsesApiTool;
use codex_extension_api::ToolCall as ExtensionToolCall;
use codex_extension_api::ToolExecutor;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::dynamic_tools::DynamicToolFunctionSpec;
use codex_protocol::dynamic_tools::DynamicToolNamespaceSpec;
use codex_protocol::dynamic_tools::DynamicToolNamespaceTool;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::PermissionProfile;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ApplyPatchToolType;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_tools::ResponsesApiNamespace;
use codex_tools::ResponsesApiNamespaceTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use codex_tools::default_namespace_description;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use pretty_assertions::assert_eq;
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn admitted_tool_dispatch_state() -> Arc<ToolDispatchState> {
    let state = Arc::new(ToolDispatchState::new());
    assert!(state.try_admit());
    state
}

use super::ExternalMutationIntent;
use super::ToolCall;
use super::ToolCallBuildError;
use super::ToolCallSource;
use super::ToolRouter;
use super::ToolRouterParams;
use super::authorize_bound_typed_tool_call;
use super::authorize_independent_review_tool_call;
use super::extension_tool_executors;

#[tokio::test]
async fn serialized_tool_manifest_fingerprint_includes_exposure_identity() {
    let (_session, turn) = make_session_and_context().await;
    let disabled = ToolExposureIdentity {
        goal_surface_state: GoalSurfaceState::Disabled,
        ..ToolExposureIdentity::default()
    };
    let inactive = ToolExposureIdentity {
        goal_surface_state: GoalSurfaceState::Inactive,
        ..disabled.clone()
    };
    let router = |identity| {
        ToolRouter::from_parts_with_warnings_and_identity(
            ToolRegistry::empty_for_test(),
            Vec::new(),
            Vec::new(),
            identity,
        )
    };

    let disabled_hash = router(disabled.clone()).tool_manifest(&turn).hash;
    assert_eq!(disabled_hash, router(disabled).tool_manifest(&turn).hash);
    assert_ne!(disabled_hash, router(inactive).tool_manifest(&turn).hash);
}

#[tokio::test]
async fn deferred_capability_revision_depends_only_on_provenance_and_schema() {
    let (_session, mut turn) = make_session_and_context().await;
    turn.model_info.supports_search_tool = true;
    let turn = Arc::new(turn);
    let step_context = StepContext::for_test(Arc::clone(&turn));
    let dynamic_tools = vec![DynamicToolSpec::Function(DynamicToolFunctionSpec {
        name: "stable_deferred_revision".to_string(),
        description: "A deferred tool with a stable schema.".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false,
        }),
        defer_loading: true,
    })];
    let router = |identity| {
        ToolRouter::from_context(
            step_context.as_ref(),
            ToolRouterParams {
                tool_suggest_candidates: None,
                deferred_mcp_tools: None,
                mcp_tools: None,
                extension_tool_executors: Vec::new(),
                dynamic_tools: &dynamic_tools,
                exposure_identity: identity,
            },
            &Default::default(),
        )
    };
    let first = router(ToolExposureIdentity {
        goal_surface_state: GoalSurfaceState::Disabled,
        ..ToolExposureIdentity::default()
    })
    .deferred_tool_capability_revisions();
    let second = router(ToolExposureIdentity {
        goal_surface_state: GoalSurfaceState::Inactive,
        ..ToolExposureIdentity::default()
    })
    .deferred_tool_capability_revisions();

    assert_eq!(first, second);
    assert_eq!(first.len(), 1);
}

#[tokio::test]
async fn serialized_tool_manifest_cache_invalidates_on_activation_revision() {
    let (_session, mut turn) = make_session_and_context().await;
    turn.model_info.supports_search_tool = true;
    let turn = Arc::new(turn);
    let step_context = StepContext::for_test(Arc::clone(&turn));
    let dynamic_tools = vec![DynamicToolSpec::Function(DynamicToolFunctionSpec {
        name: "activate_manifest_tool".to_string(),
        description: "Visible after activation.".to_string(),
        input_schema: json!({"type": "object", "properties": {}}),
        defer_loading: true,
    })];
    let router = ToolRouter::from_context(
        step_context.as_ref(),
        ToolRouterParams {
            tool_suggest_candidates: None,
            deferred_mcp_tools: None,
            mcp_tools: None,
            extension_tool_executors: Vec::new(),
            dynamic_tools: &dynamic_tools,
            exposure_identity: Default::default(),
        },
        &Default::default(),
    );

    let first = router.tool_manifest(&turn);
    let exposes_tool = |manifest: &codex_protocol::protocol::ToolManifestItem| {
        manifest.manifest.as_ref().expect("full manifest")["model_visible"]
            .as_array()
            .expect("model-visible tools")
            .iter()
            .any(|tool| tool["type"] == "function" && tool["name"] == "activate_manifest_tool")
    };
    assert!(!exposes_tool(&first));
    turn.refresh_deferred_tool_capabilities(router.deferred_tool_capability_revisions());
    turn.activate_deferred_tools([ToolName::plain("activate_manifest_tool")]);
    let second = router.tool_manifest(&turn);
    assert_ne!(first.hash, second.hash);
    assert!(exposes_tool(&second));
    assert_eq!(turn.deferred_tool_activation_revision(), 1);
}

#[tokio::test]
async fn unchanged_rollout_tool_manifest_uses_a_compact_reference() {
    let (_session, turn) = make_session_and_context().await;
    let router = ToolRouter::from_parts_with_warnings_and_identity(
        ToolRegistry::empty_for_test(),
        Vec::new(),
        Vec::new(),
        ToolExposureIdentity::default(),
    );

    let definition = router.tool_manifest_for_rollout(&turn, None);
    assert_eq!(definition, router.tool_manifest_for_rollout(&turn, None));
    let reference = router.tool_manifest_for_rollout(&turn, Some(definition.hash.as_str()));

    assert!(definition.manifest.is_some());
    assert!(reference.is_reference());
    assert_eq!(reference.hash, definition.hash);
}

#[tokio::test]
async fn model_visible_schema_lookup_does_not_materialize_rollout_manifest() -> anyhow::Result<()> {
    let (_, mut turn) = make_session_and_context().await;
    turn.model_info.supports_search_tool = true;
    let turn = Arc::new(turn);
    let step_context = StepContext::for_test(Arc::clone(&turn));
    let hidden_tool = "hidden_manifest_counter_tool";
    let visible_tool = "visible_manifest_counter_tool";
    let dynamic_tools = vec![DynamicToolSpec::Namespace(DynamicToolNamespaceSpec {
        name: "codex_app".to_string(),
        description: "Codex app tools.".to_string(),
        tools: vec![
            DynamicToolNamespaceTool::Function(DynamicToolFunctionSpec {
                name: hidden_tool.to_string(),
                description: "Hidden until discovered.".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false,
                }),
                defer_loading: true,
            }),
            DynamicToolNamespaceTool::Function(DynamicToolFunctionSpec {
                name: visible_tool.to_string(),
                description: "Visible immediately.".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false,
                }),
                defer_loading: false,
            }),
        ],
    })];
    let router = ToolRouter::from_context(
        step_context.as_ref(),
        ToolRouterParams {
            tool_suggest_candidates: None,
            deferred_mcp_tools: None,
            mcp_tools: None,
            extension_tool_executors: Vec::new(),
            dynamic_tools: &dynamic_tools,
            exposure_identity: Default::default(),
        },
        &Default::default(),
    );

    let base_schemas = router.model_visible_schemas_for_turn(turn.as_ref());
    assert_eq!(router.schema_snapshot_build_count(), 1);
    assert_eq!(router.manifest_snapshot_build_count(), 0);

    let definition = router.tool_manifest_for_rollout(turn.as_ref(), None);
    assert_eq!(router.schema_snapshot_build_count(), 1);
    assert_eq!(router.manifest_snapshot_build_count(), 1);
    assert_eq!(
        definition
            .manifest
            .as_ref()
            .expect("the first rollout item must define the manifest")["model_visible"],
        serde_json::to_value(base_schemas.specs())?
    );
    let reference = router.tool_manifest_for_rollout(turn.as_ref(), Some(definition.hash.as_str()));
    assert!(reference.is_reference());
    assert_eq!(router.manifest_snapshot_build_count(), 1);

    let hidden_name = router
        .registered_tool_names_for_test()
        .into_iter()
        .find(|name| name.to_string().contains(hidden_tool))
        .expect("registered deferred dynamic tool name");
    let first_capability_revisions = router.deferred_tool_capability_revisions();
    let second_capability_revisions = router.deferred_tool_capability_revisions();
    assert!(Arc::ptr_eq(
        &first_capability_revisions,
        &second_capability_revisions
    ));
    assert_eq!(router.deferred_tool_capability_revision_build_count(), 1);
    turn.refresh_deferred_tool_capabilities(first_capability_revisions);
    turn.activate_deferred_tools([hidden_name.clone()]);
    let activated_schemas = router.model_visible_schemas_for_turn(turn.as_ref());
    assert_eq!(router.schema_snapshot_build_count(), 2);
    assert_eq!(router.manifest_snapshot_build_count(), 1);
    assert!(
        namespace_function_names(activated_schemas.specs(), "codex_app")
            .iter()
            .any(|name| name == hidden_tool)
    );

    let activated_definition =
        router.tool_manifest_for_rollout(turn.as_ref(), Some(definition.hash.as_str()));
    assert!(!activated_definition.is_reference());
    assert_ne!(activated_definition.hash, definition.hash);
    assert_eq!(router.schema_snapshot_build_count(), 2);
    assert_eq!(router.manifest_snapshot_build_count(), 2);
    assert_eq!(
        activated_definition
            .manifest
            .as_ref()
            .expect("the activated surface must define its manifest")["model_visible"],
        serde_json::to_value(activated_schemas.specs())?
    );

    turn.release_advertised_deferred_tools(&HashSet::from([hidden_name]));
    let base_again =
        router.tool_manifest_for_rollout(turn.as_ref(), Some(activated_definition.hash.as_str()));
    assert!(!base_again.is_reference());
    assert_eq!(base_again.hash, definition.hash);
    assert_eq!(router.schema_snapshot_build_count(), 2);
    assert_eq!(router.manifest_snapshot_build_count(), 2);

    Ok(())
}

#[tokio::test]
async fn serialized_tool_surface_cache_reuses_identical_activation_sets_across_turns() {
    let (_first_session, first_turn) = make_session_and_context().await;
    let (_second_session, mut second_turn) = make_session_and_context().await;
    second_turn.sub_id = "second-turn".to_string();
    assert_ne!(first_turn.sub_id, second_turn.sub_id);
    let router = ToolRouter::from_parts_with_warnings_and_identity(
        ToolRegistry::empty_for_test(),
        Vec::new(),
        Vec::new(),
        ToolExposureIdentity::default(),
    );

    let first = router.model_visible_schemas_for_turn(&first_turn);
    let second = router.model_visible_schemas_for_turn(&second_turn);

    assert!(Arc::ptr_eq(&first, &second));
    assert_eq!(
        router.tool_manifest(&first_turn),
        router.tool_manifest(&second_turn)
    );
}

struct ExtensionEchoContributor;

impl codex_extension_api::ToolContributor for ExtensionEchoContributor {
    fn tools(
        &self,
        _session_store: &ExtensionData,
        _thread_store: &ExtensionData,
    ) -> Vec<Arc<dyn ToolExecutor<ExtensionToolCall>>> {
        vec![Arc::new(ExtensionEchoExecutor)]
    }
}

struct ExtensionEchoExecutor;

impl ToolExecutor<ExtensionToolCall> for ExtensionEchoExecutor {
    fn tool_name(&self) -> ToolName {
        ToolName::namespaced("extension/", "echo")
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Namespace(ResponsesApiNamespace {
            name: "extension/".to_string(),
            description: default_namespace_description("extension/"),
            tools: vec![ResponsesApiNamespaceTool::Function(ResponsesApiTool {
                name: "echo".to_string(),
                description: "Echoes arguments through an extension tool.".to_string(),
                strict: true,
                parameters: codex_extension_api::parse_tool_input_schema(&json!({
                    "type": "object",
                    "properties": {
                        "message": { "type": "string" },
                    },
                    "required": ["message"],
                    "additionalProperties": false,
                }))
                .expect("extension schema should parse"),
                output_schema: None,
                defer_loading: None,
            })],
        })
    }

    fn handle(&self, call: ExtensionToolCall) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(self.handle_call(call))
    }
}

impl ExtensionEchoExecutor {
    async fn handle_call(
        &self,
        call: ExtensionToolCall,
    ) -> Result<Box<dyn codex_tools::ToolOutput>, codex_tools::FunctionCallError> {
        let arguments: serde_json::Value =
            serde_json::from_str(call.function_arguments()?).expect("test arguments should parse");
        Ok(Box::new(codex_tools::JsonToolOutput::new(json!({
            "arguments": arguments,
            "callId": call.call_id,
            "conversationHistory": call.conversation_history.items(),
            "ok": true,
        }))) as Box<dyn codex_tools::ToolOutput>)
    }
}

fn extension_tool_test_registry() -> Arc<ExtensionRegistry<Config>> {
    let mut builder = ExtensionRegistryBuilder::new();
    builder.tool_contributor(Arc::new(ExtensionEchoContributor));
    Arc::new(builder.build())
}

async fn enable_typed_router_task(
    session: &mut crate::session::session::Session,
    turn: &mut crate::session::turn_context::TurnContext,
    repo: &Path,
    write_paths: &[&str],
) -> (
    codex_agent_task_store::AttemptId,
    Arc<codex_agent_task_store::LocalAgentTaskStore>,
) {
    enable_typed_router_task_with_state_home(
        session,
        turn,
        repo,
        write_paths,
        &repo.join(".typed-task-home"),
    )
    .await
}

async fn enable_typed_router_task_with_state_home(
    session: &mut crate::session::session::Session,
    turn: &mut crate::session::turn_context::TurnContext,
    repo: &Path,
    write_paths: &[&str],
    state_home: &Path,
) -> (
    codex_agent_task_store::AttemptId,
    Arc<codex_agent_task_store::LocalAgentTaskStore>,
) {
    let root_session_id = "router-apply-patch-root".to_string();
    let state_runtime =
        codex_state::StateRuntime::init(state_home.to_path_buf(), "test-provider".to_string())
            .await
            .expect("typed task state initializes");
    let coordinator = session.services.agent_control.task_coordinator();
    coordinator
        .initialize(state_runtime, root_session_id.clone())
        .await
        .expect("typed task coordinator initializes");
    let (assignment, attempt) = coordinator
        .create_assignment(
            repo,
            codex_agent_task_store::AssignmentDraft {
                root_session_id,
                admission_origin: codex_agent_task_store::AssignmentAdmissionOrigin::Typed,
                role: codex_agent_task_store::AgentRole::Worker,
                capability_profile: codex_agent_task_store::CapabilityProfile::ScopedSourceWrite,
                objective: "exercise router apply_patch mutation evidence".to_string(),
                acceptance_criteria: vec![codex_agent_task_store::AcceptanceCriterion {
                    id: "router-mutation-evidence".to_string(),
                    text: "router-dispatched apply_patch finalizes mutation evidence".to_string(),
                }],
                read_scope: Vec::new(),
                write_scope: write_paths
                    .iter()
                    .map(|path| codex_agent_task_store::RepoScope {
                        path: (*path).to_string(),
                        recursive: false,
                    })
                    .collect(),
                stop_condition: "mutation evidence finalized".to_string(),
                dependencies: Vec::new(),
                risk_hints: Vec::new(),
                required_evidence: vec!["router boundary test".to_string()],
                prohibited_changes: Vec::new(),
                contract_claims: Vec::new(),
                workspace_strategy: codex_agent_task_store::WorkspaceStrategy::Auto,
                relation: None,
                architecture_contract_ref: None,
            },
        )
        .await
        .expect("typed assignment is created");
    let agent_path =
        AgentPath::try_from("/root/router_apply_patch_worker").expect("valid agent path");
    let binding = coordinator
        .bind_agent_task(codex_agent_task_store::AgentTaskBindingDraft {
            assignment_id: assignment.assignment_id,
            attempt_id: attempt.attempt_id,
            agent_path: agent_path.to_string(),
            task_name: "router_apply_patch_worker".to_string(),
            thread_id: Some(session.thread_id.to_string()),
        })
        .await
        .expect("typed assignment is bound");
    assert!(
        coordinator
            .heartbeat_typed_actor_binding(&binding)
            .await
            .expect("bound fixture actor heartbeat is persisted"),
        "normal typed routing requires an active assignment bound to this session thread"
    );
    turn.session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: ThreadId::new(),
        depth: 1,
        agent_path: Some(agent_path),
        agent_nickname: None,
        agent_role: Some("worker".to_string()),
    });
    (
        attempt.attempt_id,
        coordinator.store().expect("typed task store is available"),
    )
}

fn set_router_environment(turn: &mut crate::session::turn_context::TurnContext, repo: &Path) {
    let template = turn
        .environments
        .primary()
        .expect("primary environment")
        .clone();
    let cwd = AbsolutePathBuf::from_absolute_path(repo).expect("absolute repository path");
    turn.environments.turn_environments = vec![TurnEnvironment::new(
        codex_exec_server::LOCAL_ENVIRONMENT_ID.to_string(),
        Arc::clone(&template.environment),
        PathUri::from_abs_path(&cwd),
        template.shell,
    )];
}

#[tokio::test]
async fn parallel_support_does_not_match_namespaced_local_tool_names() -> anyhow::Result<()> {
    let (session, turn) = make_session_and_context().await;
    let turn = Arc::new(turn);
    let step_context = StepContext::for_test(Arc::clone(&turn));
    let mcp_tools = session
        .services
        .latest_mcp_runtime()
        .manager()
        .list_all_tools()
        .await;
    let router = ToolRouter::from_context(
        step_context.as_ref(),
        ToolRouterParams {
            tool_suggest_candidates: None,
            deferred_mcp_tools: None,
            mcp_tools: Some(mcp_tools),
            extension_tool_executors: Vec::new(),
            dynamic_tools: turn.dynamic_tools.as_slice(),
            exposure_identity: Default::default(),
        },
        &Default::default(),
    );

    let parallel_tool_name = ["exec_command", "shell_command"]
        .into_iter()
        .find(|name| {
            router.tool_supports_parallel(&ToolCall {
                tool_name: ToolName::plain(*name),
                call_id: "call-parallel-tool".to_string(),
                payload: ToolPayload::Function {
                    arguments: "{}".to_string(),
                },
            })
        })
        .expect("test session should expose a parallel shell-like tool");

    assert!(!router.tool_supports_parallel(&ToolCall {
        tool_name: ToolName::namespaced("mcp__server__", parallel_tool_name),
        call_id: "call-namespaced-tool".to_string(),
        payload: ToolPayload::Function {
            arguments: "{}".to_string(),
        },
    }));

    Ok(())
}

#[tokio::test]
async fn build_tool_call_uses_namespace_for_registry_name() -> anyhow::Result<()> {
    let tool_name = "create_event".to_string();

    let call = ToolRouter::build_tool_call(ResponseItem::FunctionCall {
        id: None,
        name: tool_name.clone(),
        namespace: Some("mcp__codex_apps__calendar".to_string()),
        arguments: "{}".to_string(),
        call_id: "call-namespace".to_string(),
        internal_chat_message_metadata_passthrough: None,
    })?
    .expect("function_call should produce a tool call");

    assert_eq!(
        call.tool_name,
        ToolName::namespaced("mcp__codex_apps__calendar", tool_name)
    );
    assert_eq!(call.call_id, "call-namespace");
    match call.payload {
        ToolPayload::Function { arguments } => {
            assert_eq!(arguments, "{}");
        }
        other => panic!("expected function payload, got {other:?}"),
    }

    Ok(())
}

#[tokio::test]
async fn build_custom_tool_call_uses_namespace_for_registry_name() -> anyhow::Result<()> {
    let tool_name = "exec".to_string();

    let call = ToolRouter::build_tool_call(ResponseItem::CustomToolCall {
        id: None,
        status: None,
        call_id: "call-namespace".to_string(),
        name: tool_name.clone(),
        namespace: Some("mcp__python".to_string()),
        input: "print('hello')".to_string(),
        internal_chat_message_metadata_passthrough: None,
    })?
    .expect("custom_tool_call should produce a tool call");

    assert_eq!(
        call,
        ToolCall {
            tool_name: ToolName::namespaced("mcp__python", tool_name),
            call_id: "call-namespace".to_string(),
            payload: ToolPayload::Custom {
                input: "print('hello')".to_string(),
            },
        }
    );

    Ok(())
}

#[test]
fn malformed_client_tool_search_call_retains_output_correlation() {
    let error = ToolRouter::build_tool_call(ResponseItem::ToolSearchCall {
        id: None,
        call_id: Some("search-malformed".to_string()),
        status: None,
        execution: "client".to_string(),
        arguments: json!({"query": 42}),
        internal_chat_message_metadata_passthrough: None,
    })
    .expect_err("malformed tool_search arguments should fail to build");

    let ToolCallBuildError::ToolSearchArguments { call_id, message } = error;
    assert_eq!(call_id, "search-malformed");
    assert!(
        message.starts_with("failed to parse tool_search arguments:"),
        "unexpected build error: {message}"
    );
}

#[tokio::test]
async fn mcp_parallel_support_uses_handler_data() -> anyhow::Result<()> {
    let (_, turn) = make_session_and_context().await;
    let turn = Arc::new(turn);
    let step_context = StepContext::for_test(Arc::clone(&turn));
    let router = ToolRouter::from_context(
        step_context.as_ref(),
        ToolRouterParams {
            tool_suggest_candidates: None,
            deferred_mcp_tools: None,
            mcp_tools: Some(vec![
                mcp_tool_info(
                    "echo",
                    /*supports_parallel_tool_calls*/ true,
                    "mcp__echo__",
                    "query_with_delay",
                ),
                mcp_tool_info(
                    "hello_echo",
                    /*supports_parallel_tool_calls*/ false,
                    "mcp__hello_echo__",
                    "query_with_delay",
                ),
            ]),
            extension_tool_executors: Vec::new(),
            dynamic_tools: turn.dynamic_tools.as_slice(),
            exposure_identity: Default::default(),
        },
        &Default::default(),
    );

    let call = ToolCall {
        tool_name: ToolName::namespaced("mcp__echo__", "query_with_delay"),
        call_id: "call-handler".to_string(),
        payload: ToolPayload::Function {
            arguments: "{}".to_string(),
        },
    };
    assert!(router.tool_supports_parallel(&call));

    let different_server_call = ToolCall {
        tool_name: ToolName::namespaced("mcp__hello_echo__", "query_with_delay"),
        call_id: "call-other-server".to_string(),
        payload: ToolPayload::Function {
            arguments: "{}".to_string(),
        },
    };
    assert!(!router.tool_supports_parallel(&different_server_call));

    Ok(())
}

#[tokio::test]
async fn tools_without_handlers_do_not_support_parallel() -> anyhow::Result<()> {
    let (_, turn) = make_session_and_context().await;
    let turn = Arc::new(turn);
    let step_context = StepContext::for_test(Arc::clone(&turn));
    let router = ToolRouter::from_context(
        step_context.as_ref(),
        ToolRouterParams {
            tool_suggest_candidates: None,
            deferred_mcp_tools: None,
            mcp_tools: None,
            extension_tool_executors: Vec::new(),
            dynamic_tools: turn.dynamic_tools.as_slice(),
            exposure_identity: Default::default(),
        },
        &Default::default(),
    );

    assert!(!router.tool_supports_parallel(&ToolCall {
        tool_name: ToolName::plain("web_search"),
        call_id: "call-web-search".to_string(),
        payload: ToolPayload::Function {
            arguments: "{}".to_string(),
        },
    }));

    Ok(())
}

#[tokio::test]
async fn specs_filter_deferred_dynamic_tools() -> anyhow::Result<()> {
    let (_, mut turn) = make_session_and_context().await;
    turn.model_info.supports_search_tool = true;
    let turn = Arc::new(turn);
    let step_context = StepContext::for_test(Arc::clone(&turn));
    let hidden_tool = "hidden_dynamic_tool";
    let visible_tool = "visible_dynamic_tool";
    let dynamic_tools = vec![DynamicToolSpec::Namespace(DynamicToolNamespaceSpec {
        name: "codex_app".to_string(),
        description: "Codex app tools.".to_string(),
        tools: vec![
            DynamicToolNamespaceTool::Function(DynamicToolFunctionSpec {
                name: hidden_tool.to_string(),
                description: "Hidden until discovered.".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false,
                }),
                defer_loading: true,
            }),
            DynamicToolNamespaceTool::Function(DynamicToolFunctionSpec {
                name: visible_tool.to_string(),
                description: "Visible immediately.".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false,
                }),
                defer_loading: false,
            }),
        ],
    })];

    let router = ToolRouter::from_context(
        step_context.as_ref(),
        ToolRouterParams {
            tool_suggest_candidates: None,
            deferred_mcp_tools: None,
            mcp_tools: None,
            extension_tool_executors: Vec::new(),
            dynamic_tools: &dynamic_tools,
            exposure_identity: Default::default(),
        },
        &Default::default(),
    );

    assert_eq!(
        namespace_function_names(&router.model_visible_specs(), "codex_app"),
        vec![visible_tool.to_string()]
    );
    let manifest = router
        .tool_manifest(turn.as_ref())
        .manifest
        .expect("runtime router emits a full manifest snapshot");
    let registered = manifest["registered"]
        .as_array()
        .expect("registered tool manifest entries");
    assert!(!registered.is_empty());
    assert!(registered.iter().all(|entry| entry.get("spec").is_none()));
    assert!(registered.iter().all(|entry| {
        entry["spec_sha256"]
            .as_str()
            .is_some_and(|digest| digest.len() == 64)
    }));
    let hidden_manifest_entry = registered
        .iter()
        .find(|entry| {
            entry["name"]
                .as_str()
                .is_some_and(|name| name.contains(hidden_tool))
        })
        .expect("deferred dynamic tool manifest entry");
    assert_eq!(hidden_manifest_entry["exposure"], "deferred");
    assert_eq!(hidden_manifest_entry["activated"], false);

    let base_schemas = router.model_visible_schemas_for_turn(turn.as_ref());
    assert!(Arc::ptr_eq(
        &base_schemas,
        &router.model_visible_schemas_for_turn(turn.as_ref())
    ));
    let hidden_name = router
        .registered_tool_names_for_test()
        .into_iter()
        .find(|name| name.to_string().contains(hidden_tool))
        .expect("registered deferred dynamic tool name");
    turn.refresh_deferred_tool_capabilities(router.deferred_tool_capability_revisions());
    turn.activate_deferred_tools([hidden_name]);
    let (activation_revision, activated) = turn.deferred_tool_activation_snapshot();
    assert_eq!(activation_revision, 1);
    assert_eq!(activated.len(), 1);
    let activated_schemas = router.model_visible_schemas_for_turn(turn.as_ref());
    assert_eq!(
        namespace_function_names(activated_schemas.specs(), "codex_app"),
        vec![visible_tool.to_string(), hidden_tool.to_string()]
    );
    assert!(Arc::ptr_eq(
        &activated_schemas,
        &router.model_visible_schemas_for_turn(turn.as_ref())
    ));
    let activated_manifest = router
        .tool_manifest(turn.as_ref())
        .manifest
        .expect("activated tool surface emits one manifest snapshot");
    assert!(
        activated_manifest["registered"]
            .as_array()
            .is_some_and(|entries| {
                entries.iter().any(|entry| {
                    entry["name"]
                        .as_str()
                        .is_some_and(|name| name.contains(hidden_tool))
                        && entry["activated"] == true
                })
            })
    );
    assert!(
        activated_manifest["model_visible"]
            .to_string()
            .contains(hidden_tool),
        "the manifest and activated schema snapshot must expose the same deferred tool"
    );

    Ok(())
}

fn mcp_tool_info(
    server_name: &str,
    supports_parallel_tool_calls: bool,
    callable_namespace: &str,
    tool_name: &str,
) -> codex_mcp::ToolInfo {
    codex_mcp::ToolInfo {
        server_name: server_name.to_string(),
        supports_parallel_tool_calls,
        server_origin: None,
        callable_name: tool_name.to_string(),
        callable_namespace: callable_namespace.to_string(),
        namespace_description: None,
        tool: rmcp::model::Tool::new(
            tool_name.to_string(),
            "Test MCP tool",
            Arc::new(rmcp::model::object(json!({
                "type": "object",
            }))),
        ),
        connector_id: None,
        connector_name: None,
        plugin_display_names: Vec::new(),
    }
}

#[test]
fn independent_review_policy_allows_inspection_and_denies_mutation() {
    let sources = [
        SessionSource::SubAgent(SubAgentSource::Review),
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: ThreadId::new(),
            depth: 1,
            agent_path: None,
            agent_nickname: None,
            agent_role: Some("reviewer".to_string()),
        }),
    ];
    for source in sources {
        for (tool_name, class) in [
            (
                ToolName::plain("read_tool_output"),
                TypedToolClass::ReadSearch,
            ),
            (ToolName::plain("shell_command"), TypedToolClass::Shell),
            (ToolName::plain("exec_command"), TypedToolClass::Shell),
            (ToolName::plain("write_stdin"), TypedToolClass::Shell),
        ] {
            let call = ToolCall {
                tool_name,
                call_id: "review-read".to_string(),
                payload: ToolPayload::Function {
                    arguments: "{}".to_string(),
                },
            };
            authorize_independent_review_tool_call(
                &source,
                class,
                &call,
                ExternalMutationIntent::ProvenReadOnly,
            )
            .expect("review inspection tool should be authorized");
        }

        let repo_atlas_call = ToolCall {
            tool_name: ToolName::namespaced("mcp__repo_atlas", "context_for"),
            call_id: "review-atlas".to_string(),
            payload: ToolPayload::Function {
                arguments: "{}".to_string(),
            },
        };
        authorize_independent_review_tool_call(
            &source,
            TypedToolClass::DynamicExternal,
            &repo_atlas_call,
            ExternalMutationIntent::ProvenReadOnly,
        )
        .expect("allowlisted Repo Atlas inspection should be authorized");

        for (tool_name, class) in [
            (
                ToolName::plain("apply_patch"),
                TypedToolClass::StructuredEdit,
            ),
            (
                ToolName::namespaced("mcp__repo_atlas", "write_file"),
                TypedToolClass::DynamicExternal,
            ),
            (
                ToolName::namespaced("mcp__codex_apps__github", "create_branch"),
                TypedToolClass::DynamicExternal,
            ),
        ] {
            let call = ToolCall {
                tool_name,
                call_id: "review-write".to_string(),
                payload: ToolPayload::Function {
                    arguments: "{}".to_string(),
                },
            };
            assert!(matches!(
                authorize_independent_review_tool_call(
                    &source,
                    class,
                    &call,
                    ExternalMutationIntent::MayMutate,
                ),
                Err(crate::FunctionCallError::DeniedToModel(_))
            ));
        }
    }
}

#[tokio::test]
async fn inactive_typed_assignment_is_a_blocked_tool_call() {
    let temp = tempfile::tempdir().expect("temporary repository");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("create repository");
    let (mut session, mut turn) = make_session_and_context().await;
    let (_, store) =
        enable_typed_router_task(&mut session, &mut turn, &repo, &["tracked.txt"]).await;
    let assignment_id = session
        .services
        .agent_control
        .task_coordinator()
        .binding_for_source(&turn.session_source)
        .expect("typed task binding")
        .assignment_id;
    store
        .abandon_agent_task(
            codex_agent_task_store::TaskActor::Root,
            assignment_id,
            "test terminal assignment".to_string(),
        )
        .await
        .expect("abandon typed assignment");

    let turn = Arc::new(turn);
    let step_context = StepContext::for_test(Arc::clone(&turn));
    let error = authorize_bound_typed_tool_call(
        &session,
        step_context.as_ref(),
        TypedToolClass::ReadSearch,
        &ToolCall {
            tool_name: ToolName::plain("read_tool_output"),
            call_id: "inactive-read".to_string(),
            payload: ToolPayload::Function {
                arguments: "{}".to_string(),
            },
        },
        ExternalMutationIntent::ProvenReadOnly,
    )
    .await
    .expect_err("inactive typed assignments must be blocked before dispatch");

    assert!(matches!(
        error,
        crate::FunctionCallError::DeniedToModel(message)
            if message.contains("no longer active")
    ));
}

struct PatchReplyBarrierFileSystem {
    first: PathUri,
    writes: std::sync::Mutex<Vec<PathUri>>,
    committed: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    release: Arc<tokio::sync::Notify>,
    reply_dropped: Arc<std::sync::atomic::AtomicBool>,
}

struct PatchReplyLifetime(Arc<std::sync::atomic::AtomicBool>);

impl Drop for PatchReplyLifetime {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }
}

impl codex_exec_server::ExecutorFileSystem for PatchReplyBarrierFileSystem {
    fn canonicalize<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a codex_exec_server::FileSystemSandboxContext>,
    ) -> codex_exec_server::ExecutorFileSystemFuture<'a, PathUri> {
        codex_exec_server::LOCAL_FS.canonicalize(path, sandbox)
    }

    fn read_file<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a codex_exec_server::FileSystemSandboxContext>,
    ) -> codex_exec_server::ExecutorFileSystemFuture<'a, Vec<u8>> {
        codex_exec_server::LOCAL_FS.read_file(path, sandbox)
    }

    fn read_file_stream<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a codex_exec_server::FileSystemSandboxContext>,
    ) -> codex_exec_server::ExecutorFileSystemFuture<'a, codex_exec_server::FileSystemReadStream>
    {
        codex_exec_server::LOCAL_FS.read_file_stream(path, sandbox)
    }

    fn write_file<'a>(
        &'a self,
        path: &'a PathUri,
        contents: Vec<u8>,
        sandbox: Option<&'a codex_exec_server::FileSystemSandboxContext>,
    ) -> codex_exec_server::ExecutorFileSystemFuture<'a, ()> {
        Box::pin(async move {
            self.writes.lock().expect("write log").push(path.clone());
            codex_exec_server::LOCAL_FS
                .write_file(path, contents, sandbox)
                .await?;
            if path == &self.first {
                let _reply_lifetime = PatchReplyLifetime(Arc::clone(&self.reply_dropped));
                if let Some(committed) = self.committed.lock().expect("commit signal").take() {
                    let _ = committed.send(());
                }
                self.release.notified().await;
            }
            Ok(())
        })
    }

    fn create_directory<'a>(
        &'a self,
        path: &'a PathUri,
        options: codex_exec_server::CreateDirectoryOptions,
        sandbox: Option<&'a codex_exec_server::FileSystemSandboxContext>,
    ) -> codex_exec_server::ExecutorFileSystemFuture<'a, ()> {
        codex_exec_server::LOCAL_FS.create_directory(path, options, sandbox)
    }

    fn get_metadata<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a codex_exec_server::FileSystemSandboxContext>,
    ) -> codex_exec_server::ExecutorFileSystemFuture<'a, codex_exec_server::FileMetadata> {
        codex_exec_server::LOCAL_FS.get_metadata(path, sandbox)
    }

    fn read_directory<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a codex_exec_server::FileSystemSandboxContext>,
    ) -> codex_exec_server::ExecutorFileSystemFuture<'a, Vec<codex_exec_server::ReadDirectoryEntry>>
    {
        codex_exec_server::LOCAL_FS.read_directory(path, sandbox)
    }

    fn remove<'a>(
        &'a self,
        path: &'a PathUri,
        options: codex_exec_server::RemoveOptions,
        sandbox: Option<&'a codex_exec_server::FileSystemSandboxContext>,
    ) -> codex_exec_server::ExecutorFileSystemFuture<'a, ()> {
        codex_exec_server::LOCAL_FS.remove(path, options, sandbox)
    }

    fn copy<'a>(
        &'a self,
        source_path: &'a PathUri,
        destination_path: &'a PathUri,
        options: codex_exec_server::CopyOptions,
        sandbox: Option<&'a codex_exec_server::FileSystemSandboxContext>,
    ) -> codex_exec_server::ExecutorFileSystemFuture<'a, ()> {
        codex_exec_server::LOCAL_FS.copy(source_path, destination_path, options, sandbox)
    }
}

#[tokio::test]
async fn router_apply_patch_cancellation_settles_committed_write_and_skips_tail()
-> anyhow::Result<()> {
    for (cancel, drop_caller) in [(true, false), (true, true), (false, false)] {
        let temp = tempfile::tempdir()?;
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo)?;
        assert!(
            Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(&repo)
                .status()?
                .success()
        );
        std::fs::write(repo.join("first.txt"), "before\n")?;
        let first = PathUri::from_abs_path(&AbsolutePathBuf::from_absolute_path(
            repo.join("first.txt"),
        )?);
        let tail =
            PathUri::from_abs_path(&AbsolutePathBuf::from_absolute_path(repo.join("tail.txt"))?);
        let (committed_tx, committed_rx) = tokio::sync::oneshot::channel();
        let release = Arc::new(tokio::sync::Notify::new());
        let reply_dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let filesystem = Arc::new(PatchReplyBarrierFileSystem {
            first: first.clone(),
            writes: std::sync::Mutex::new(Vec::new()),
            committed: std::sync::Mutex::new(Some(committed_tx)),
            release: Arc::clone(&release),
            reply_dropped: Arc::clone(&reply_dropped),
        });
        let (mut session, mut turn) = make_session_and_context().await;
        set_router_environment(&mut turn, &repo);
        turn.environments.turn_environments[0].environment = Arc::new(
            codex_exec_server::Environment::default_for_tests_with_filesystem(filesystem.clone()),
        );
        turn.permission_profile = PermissionProfile::Disabled;
        turn.model_info.apply_patch_tool_type = Some(ApplyPatchToolType::Freeform);
        let (attempt_id, store) =
            enable_typed_router_task(&mut session, &mut turn, &repo, &["first.txt", "tail.txt"])
                .await;
        let turn = Arc::new(turn);
        let step = StepContext::for_test(Arc::clone(&turn));
        let router = Arc::new(ToolRouter::from_context(
            step.as_ref(),
            ToolRouterParams {
                tool_suggest_candidates: None,
                deferred_mcp_tools: None,
                mcp_tools: None,
                extension_tool_executors: Vec::new(),
                dynamic_tools: turn.dynamic_tools.as_slice(),
                exposure_identity: Default::default(),
            },
            &Default::default(),
        ));
        assert!(
            router
                .registered_tool_names_for_test()
                .contains(&ToolName::plain("apply_patch"))
        );
        let step = step.with_tool_router_for_test(router);
        let tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));
        let session = Arc::new(session);
        let runtime = crate::tools::parallel::ToolCallRuntime::new(
            Arc::clone(&session),
            step,
            Arc::clone(&tracker),
        );
        let call_id = if drop_caller {
            "dropped-caller-patch-prefix"
        } else if cancel {
            "cancelled-patch-prefix"
        } else {
            "successful-two-hunk-patch"
        };
        let call = ToolRouter::build_tool_call(ResponseItem::CustomToolCall {
            id: None, status: None, call_id: call_id.to_string(), name: "apply_patch".to_string(), namespace: None,
            input: "*** Begin Patch\n*** Update File: first.txt\n@@\n-before\n+after\n*** Add File: tail.txt\n+must-not-be-written\n*** End Patch".to_string(),
            internal_chat_message_metadata_passthrough: None,
        })?.expect("normal custom apply_patch call");
        let initial_gate =
            crate::workspace_operation_gate::acquire_workspace_operation(&repo).await;
        let gate = Arc::clone(tokio::sync::OwnedMutexGuard::mutex(&initial_gate));
        drop(initial_gate);
        let cancellation = CancellationToken::new();
        let mut response = Box::pin(runtime.handle_tool_call(call, cancellation.clone()));
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            tokio::select! {
                committed = committed_rx => committed.expect("real first write committed"),
                result = response.as_mut() => panic!("patch finished before held write reply: {result:?}"),
            }
        }).await.expect("patch reaches actual first filesystem write");
        assert_eq!(std::fs::read_to_string(repo.join("first.txt"))?, "after\n");
        assert!(!repo.join("tail.txt").exists());
        assert_eq!(
            *filesystem.writes.lock().expect("write log"),
            vec![first.clone()]
        );
        assert!(
            gate.try_lock().is_err(),
            "in-flight committed write owns the workspace gate"
        );
        if cancel {
            cancellation.cancel();
            assert!(
                futures::poll!(response.as_mut()).is_pending(),
                "cancelled dispatch must retain the admitted filesystem response and its delta"
            );
            assert!(
                gate.try_lock().is_err(),
                "cancellation must not release the active write gate"
            );
            assert!(!reply_dropped.load(std::sync::atomic::Ordering::Acquire));
            assert!(!repo.join("tail.txt").exists());
        }
        if drop_caller {
            // The commit-barrier supervisor is inline in this caller. Its owned
            // AbortOnDropHandle aborts dispatch when dropped, while the Session's
            // terminal task must retain the admitted patch operation itself.
            drop(response);
            assert!(gate.try_lock().is_err());
            assert!(!reply_dropped.load(std::sync::atomic::Ordering::Acquire));
            release.notify_one();
            session.terminal_tasks.close();
            tokio::time::timeout(
                std::time::Duration::from_secs(10),
                session.terminal_tasks.wait(),
            )
            .await
            .expect("Session owns committed patch settlement after caller drop");
        } else {
            release.notify_one();
            let result = tokio::time::timeout(std::time::Duration::from_secs(10), response)
                .await
                .expect("released filesystem operation must settle")?;
            let ResponseInputItem::CustomToolCallOutput {
                call_id: result_id,
                output,
                ..
            } = result
            else {
                anyhow::bail!("apply_patch must return custom tool output");
            };
            assert_eq!(result_id, call_id);
            let FunctionCallOutputBody::Text(text) = output.body else {
                anyhow::bail!("patch output must be text");
            };
            if cancel {
                assert!(text.contains("aborted by user"), "{text}");
            }
        }
        assert!(reply_dropped.load(std::sync::atomic::Ordering::Acquire));
        assert!(
            gate.try_lock().is_ok(),
            "settled mutation releases the exact workspace gate"
        );
        assert_eq!(std::fs::read_to_string(repo.join("first.txt"))?, "after\n");
        let expected_writes = if cancel {
            vec![first.clone()]
        } else {
            vec![first.clone(), tail.clone()]
        };
        assert_eq!(
            *filesystem.writes.lock().expect("write log"),
            expected_writes
        );
        if cancel {
            assert!(!repo.join("tail.txt").exists());
        } else {
            assert_eq!(
                std::fs::read_to_string(repo.join("tail.txt"))?,
                "must-not-be-written\n"
            );
        }
        let diff = tracker
            .lock()
            .await
            .get_unified_diff()
            .expect("committed patch must be visible in the turn diff");
        assert!(
            diff.contains("first.txt") && diff.contains("-before") && diff.contains("+after"),
            "{diff}"
        );
        assert_eq!(diff.contains("tail.txt"), !cancel, "{diff}");
        let evidence = store
            .list_mutation_evidence(
                attempt_id,
                Some(codex_agent_task_store::MAX_MUTATION_EVIDENCE_LIMIT),
            )
            .await?;
        let first_evidence = evidence
            .iter()
            .find(|entry| entry.path == "first.txt")
            .expect("first mutation evidence");
        assert_eq!(first_evidence.attempt_id, attempt_id);
        assert!(first_evidence.pre_write_existed);
        assert_eq!(first_evidence.final_write_existed, Some(true));
        assert_eq!(
            first_evidence.pre_write_hash.as_deref(),
            Some("9160d4be34c8695bd172a76c7c7966587ea5a4d991ad22c87b2b91af54aa9ebb")
        );
        assert_eq!(
            first_evidence.final_hash.as_deref(),
            Some("7b9a72466d3960eb2aacccfc848939453490db0678bd4725def3f789b891c919")
        );
        assert!(first_evidence.finalized_at.is_some() && first_evidence.end_epoch.is_some());
        let tail_evidence = evidence
            .iter()
            .find(|entry| entry.path == "tail.txt")
            .expect("both intended paths were registered before mutation");
        assert_eq!(tail_evidence.attempt_id, attempt_id);
        assert!(!tail_evidence.pre_write_existed);
        assert!(tail_evidence.pre_write_hash.is_none());
        assert!(tail_evidence.finalized_at.is_some() && tail_evidence.end_epoch.is_some());
        if cancel {
            assert_eq!(tail_evidence.final_write_existed, Some(false));
            assert!(
                tail_evidence.final_hash.is_none(),
                "unapplied tail cannot claim written content"
            );
        } else {
            assert_eq!(tail_evidence.final_write_existed, Some(true));
            assert_eq!(
                tail_evidence.final_hash.as_deref(),
                Some("34bb655f4c80ce8343296f6427f36e9f49e2061548f709a10d022da25e819441")
            );
        }
        store.close().await;
    }
    Ok(())
}

#[tokio::test]
async fn router_apply_patch_partial_mutation_admission_failure_finalizes_begun_paths()
-> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo)?;
    assert!(
        Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&repo)
            .status()?
            .success()
    );
    for path in ["a.txt", "b.txt"] {
        std::fs::write(repo.join(path), "before\n")?;
    }
    let (mut session, mut turn) = make_session_and_context().await;
    set_router_environment(&mut turn, &repo);
    turn.permission_profile = PermissionProfile::Disabled;
    turn.model_info.apply_patch_tool_type = Some(ApplyPatchToolType::Freeform);
    let (attempt_id, store) =
        enable_typed_router_task(&mut session, &mut turn, &repo, &["a.txt", "b.txt"]).await;
    // A prior completed mutation makes the second begin fail through the real
    // store contract, after the first path's new admission has committed.
    store
        .begin_mutation(
            attempt_id,
            &repo,
            "b.txt".to_string(),
            codex_agent_task_store::AttributionConfidence::Definitive,
        )
        .await?;
    let previous = store
        .finalize_mutation(attempt_id, &repo, "b.txt".to_string())
        .await?;
    let session = Arc::new(session);
    let step = StepContext::for_test(Arc::new(turn));
    let router = Arc::new(ToolRouter::from_context(
        step.as_ref(),
        ToolRouterParams {
            tool_suggest_candidates: None,
            deferred_mcp_tools: None,
            mcp_tools: None,
            extension_tool_executors: Vec::new(),
            dynamic_tools: &[],
            exposure_identity: Default::default(),
        },
        &Default::default(),
    ));
    assert!(step.set_tool_router(router).is_ok());
    let runtime = crate::tools::parallel::ToolCallRuntime::new(
        session,
        step,
        Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
    );
    let response = runtime.handle_tool_call(
        ToolCall {
            tool_name: ToolName::plain("apply_patch"),
            call_id: "partial-mutation-admission".to_string(),
            payload: ToolPayload::Custom {
                input: "*** Begin Patch\n*** Update File: a.txt\n@@\n-before\n+after\n*** Update File: b.txt\n@@\n-before\n+after\n*** End Patch".to_string(),
            },
        },
        CancellationToken::new(),
    ).await?;
    let response_text = serde_json::to_string(&response)?;
    assert!(
        response_text.contains("mutation evidence could not be recorded for `b.txt`"),
        "{response_text}"
    );
    assert!(
        !response_text.contains("Success. Updated"),
        "{response_text}"
    );
    for path in ["a.txt", "b.txt"] {
        assert_eq!(
            std::fs::read_to_string(repo.join(path))?,
            "before\n",
            "admission failure must prevent every patch write"
        );
    }
    let evidence = store
        .list_mutation_evidence(
            attempt_id,
            Some(codex_agent_task_store::MAX_MUTATION_EVIDENCE_LIMIT),
        )
        .await?;
    assert_eq!(evidence.len(), 2);
    let begun = evidence
        .iter()
        .find(|entry| entry.path == "a.txt")
        .expect("first begin committed");
    assert!(
        begun.finalized_at.is_some(),
        "failed second admission must not strand first admission"
    );
    assert!(begun.end_epoch.is_some());
    assert_eq!(
        begun.final_hash, begun.pre_write_hash,
        "no-write finalization must retain the original bytes"
    );
    let retained = evidence
        .iter()
        .find(|entry| entry.path == "b.txt")
        .expect("prior completed evidence retained");
    assert_eq!(retained.finalized_at, previous.finalized_at);
    assert_eq!(retained.final_hash, previous.final_hash);
    store.close().await;
    Ok(())
}

#[tokio::test]
async fn router_apply_patch_finalizes_typed_mutation_evidence() -> anyhow::Result<()> {
    let temp = tempfile::tempdir().expect("temporary repository");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("create repository");
    let status = Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(&repo)
        .status()
        .expect("launch git init");
    assert!(status.success(), "git init failed");
    std::fs::write(repo.join("tracked.txt"), "before\n").expect("write patch fixture");
    std::fs::write(repo.join("executable.sh"), "echo before\n")
        .expect("write executable deletion fixture");
    for args in [
        vec!["add", "tracked.txt", "executable.sh"],
        vec!["update-index", "--chmod=+x", "executable.sh"],
    ] {
        assert!(Command::new("git").args(args).current_dir(&repo).status()?.success());
    }

    let (mut session, mut turn) = make_session_and_context().await;
    set_router_environment(&mut turn, &repo);
    turn.permission_profile = PermissionProfile::Disabled;
    turn.model_info.apply_patch_tool_type = Some(ApplyPatchToolType::Freeform);
    let (attempt_id, store) =
        enable_typed_router_task(&mut session, &mut turn, &repo, &["tracked.txt", "executable.sh"]).await;
    let assignment_id = session
        .services
        .agent_control
        .task_coordinator()
        .binding_for_source(&turn.session_source)
        .expect("typed task binding")
        .assignment_id;
    let capsule_dir = repo
        .join(".typed-task-home")
        .join("agent-task-coordination")
        .join("task_capsules");
    std::fs::create_dir_all(&capsule_dir).expect("create capsule directory");
    std::fs::write(
        capsule_dir.join(format!("{assignment_id}.json")),
        "{not-json",
    )
    .expect("write corrupt task capsule");
    let turn = Arc::new(turn);
    let step_context = StepContext::for_test(Arc::clone(&turn));
    let router = ToolRouter::from_context(
        step_context.as_ref(),
        ToolRouterParams {
            tool_suggest_candidates: None,
            deferred_mcp_tools: None,
            mcp_tools: None,
            extension_tool_executors: Vec::new(),
            dynamic_tools: turn.dynamic_tools.as_slice(),
            exposure_identity: Default::default(),
        },
        &Default::default(),
    );
    assert!(
        router
            .registered_tool_names_for_test()
            .contains(&ToolName::plain("apply_patch")),
        "production tool planning must register apply_patch"
    );

    let call = ToolRouter::build_tool_call(ResponseItem::CustomToolCall {
        id: None,
        status: None,
        call_id: "router-apply-patch".to_string(),
        name: "apply_patch".to_string(),
        namespace: None,
        input: "*** Begin Patch\n*** Update File: tracked.txt\n@@\n-before\n+after\n*** Delete File: executable.sh\n*** End Patch"
            .to_string(),
        internal_chat_message_metadata_passthrough: None,
    })?
    .expect("custom tool call");
    let terminal_outcome_reached = admitted_tool_dispatch_state();
    let tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));
    router
        .dispatch_tool_call_with_terminal_outcome(
            Arc::new(session),
            step_context,
            CancellationToken::new(),
            Arc::clone(&tracker),
            call,
            ToolCallSource::Direct,
            Arc::clone(&terminal_outcome_reached),
        )
        .await?;
    assert!(terminal_outcome_reached.is_terminal());
    assert_eq!(
        std::fs::read_to_string(repo.join("tracked.txt")).expect("read patched file"),
        "after\n"
    );
    assert!(!repo.join("executable.sh").exists());
    let diff = tracker.lock().await.get_unified_diff().expect("published patch diff");
    assert!(diff.contains("deleted file mode 100755"), "{diff}");
    assert!(diff.contains("executable.sh"), "{diff}");

    let evidence = store
        .list_mutation_evidence(
            attempt_id,
            Some(codex_agent_task_store::MAX_MUTATION_EVIDENCE_LIMIT),
        )
        .await
        .expect("mutation evidence remains queryable");
    assert_eq!(evidence.len(), 2);
    let updated = evidence.iter().find(|item| item.path == "tracked.txt").expect("updated-file evidence");
    assert_ne!(updated.pre_write_hash, updated.final_hash);
    let deleted = evidence.iter().find(|item| item.path == "executable.sh").expect("deleted-file evidence");
    assert!(deleted.pre_write_hash.is_some());
    assert!(deleted.final_hash.is_none());
    for item in evidence {
        assert!(item.finalized_at.is_some());
        assert!(item.end_epoch.is_some());
    }

    Ok(())
}

#[tokio::test]
async fn extension_tool_executors_are_model_visible_and_dispatchable() -> anyhow::Result<()> {
    let (mut session, turn) = make_session_and_context().await;
    session.services.extensions = extension_tool_test_registry();
    let turn = Arc::new(turn);
    let step_context = StepContext::for_test(Arc::clone(&turn));
    let history_item = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "extension history".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    session
        .record_conversation_items(&turn, std::slice::from_ref(&history_item))
        .await;
    let mut expected_history_item = history_item.clone();
    expected_history_item.set_turn_id_if_missing(&turn.sub_id);

    let router = ToolRouter::from_context(
        step_context.as_ref(),
        ToolRouterParams {
            tool_suggest_candidates: None,
            deferred_mcp_tools: None,
            mcp_tools: None,
            extension_tool_executors: extension_tool_executors(&session),
            dynamic_tools: turn.dynamic_tools.as_slice(),
            exposure_identity: Default::default(),
        },
        &Default::default(),
    );

    assert!(
        router.model_visible_specs().iter().any(
            |spec| matches!(spec, ToolSpec::Namespace(namespace)
            if namespace.name == "extension/"
                && namespace.tools.iter().any(|tool| matches!(
                    tool,
                    ResponsesApiNamespaceTool::Function(tool) if tool.name == "echo"
                )))
        ),
        "expected extension-provided tool to be visible to the model"
    );

    let call = ToolRouter::build_tool_call(ResponseItem::FunctionCall {
        id: None,
        name: "echo".to_string(),
        namespace: Some("extension/".to_string()),
        arguments: json!({ "message": "hello" }).to_string(),
        call_id: "call-extension".to_string(),
        internal_chat_message_metadata_passthrough: None,
    })?
    .expect("function_call should produce a tool call");
    let terminal_outcome_reached = admitted_tool_dispatch_state();
    let result = router
        .dispatch_tool_call_with_terminal_outcome(
            Arc::new(session),
            step_context,
            CancellationToken::new(),
            Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
            call,
            ToolCallSource::Direct,
            Arc::clone(&terminal_outcome_reached),
        )
        .await?;
    assert!(terminal_outcome_reached.is_terminal());

    let response = result.into_response();
    match response {
        ResponseInputItem::FunctionCallOutput { call_id, output } => {
            assert_eq!(call_id, "call-extension");
            let FunctionCallOutputBody::Text(text) = output.body else {
                panic!("expected text function call output")
            };
            let value: serde_json::Value =
                serde_json::from_str(&text).expect("extension tool output should be json");
            assert_eq!(
                core_test_support::responses::strip_response_item_ids_from_json(value),
                core_test_support::responses::strip_response_item_ids_from_json(json!({
                    "arguments": { "message": "hello" },
                    "callId": "call-extension",
                    "conversationHistory": [expected_history_item],
                    "ok": true,
                }))
            );
        }
        other => panic!("expected function call output, got {other:?}"),
    }

    Ok(())
}

fn namespace_function_names(specs: &[ToolSpec], namespace_name: &str) -> Vec<String> {
    specs
        .iter()
        .find_map(|spec| match spec {
            ToolSpec::Namespace(namespace) if namespace.name == namespace_name => Some(
                namespace
                    .tools
                    .iter()
                    .map(|tool| match tool {
                        ResponsesApiNamespaceTool::Function(tool) => tool.name.clone(),
                    })
                    .collect(),
            ),
            ToolSpec::Function(_)
            | ToolSpec::Freeform(_)
            | ToolSpec::ToolSearch { .. }
            | ToolSpec::WebSearch { .. }
            | ToolSpec::Namespace(_) => None,
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn router_apply_patch_cancel_during_approval_has_no_mutation() -> anyhow::Result<()> {
    use codex_protocol::protocol::{AskForApproval, EventMsg, ReviewDecision, TurnAbortReason};
    use std::time::Duration;

    struct ApprovalWaitTask;
    impl crate::tasks::SessionTask for ApprovalWaitTask {
        fn kind(&self) -> crate::state::TaskKind {
            crate::state::TaskKind::Regular
        }
        fn span_name(&self) -> &'static str {
            "test.patch_approval_owner"
        }
        fn run(
            self: Arc<Self>,
            _session: Arc<crate::session::Session>,
            _turn: Arc<crate::TurnContext>,
            _input: Vec<crate::session::TurnInput>,
            cancellation: CancellationToken,
        ) -> futures::future::BoxFuture<'static, crate::tasks::SessionTaskResult> {
            Box::pin(async move {
                cancellation.cancelled().await;
                Ok(crate::tasks::TurnTaskResult::default())
            })
        }
    }

    let temp = tempfile::tempdir()?;
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo)?;
    assert!(
        Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&repo)
            .status()?
            .success()
    );
    std::fs::write(repo.join("tracked.txt"), "before\n")?;
    let (session, mut turn, events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let turn_mut = Arc::get_mut(&mut turn).expect("uniquely owned turn fixture");
    set_router_environment(turn_mut, &repo);
    turn_mut.permission_profile = PermissionProfile::Disabled;
    let mut config = (*turn_mut.config).clone();
    config.approvals_reviewer = codex_protocol::config_types::ApprovalsReviewer::User;
    turn_mut.config = Arc::new(config);
    turn_mut
        .approval_policy
        .set(AskForApproval::UnlessTrusted)
        .expect("configure real approval policy");
    turn_mut.model_info.apply_patch_tool_type = Some(ApplyPatchToolType::Freeform);
    // Register a live normal turn so request_patch_approval retains its actual response sender.
    session
        .spawn_task(Arc::clone(&turn), Vec::new(), ApprovalWaitTask)
        .await;
    let step = StepContext::for_test(Arc::clone(&turn));
    let router = Arc::new(ToolRouter::from_context(
        step.as_ref(),
        ToolRouterParams {
            tool_suggest_candidates: None,
            deferred_mcp_tools: None,
            mcp_tools: None,
            extension_tool_executors: Vec::new(),
            dynamic_tools: turn.dynamic_tools.as_slice(),
            exposure_identity: Default::default(),
        },
        &Default::default(),
    ));
    assert!(
        router
            .registered_tool_names_for_test()
            .contains(&ToolName::plain("apply_patch"))
    );
    let step = step.with_tool_router_for_test(router);
    let tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));
    let runtime = crate::tools::parallel::ToolCallRuntime::new(
        Arc::clone(&session),
        step,
        Arc::clone(&tracker),
    );
    let call_id = "cancel-patch-before-approval";
    let call = ToolRouter::build_tool_call(ResponseItem::CustomToolCall {
        id: None, status: None, call_id: call_id.to_string(), name: "apply_patch".to_string(), namespace: None,
        input: "*** Begin Patch\n*** Update File: tracked.txt\n@@\n-before\n+after\n*** Add File: tail.txt\n+must-not-be-written\n*** End Patch".to_string(),
        internal_chat_message_metadata_passthrough: None,
    })?.expect("normal custom apply_patch call");
    let initial_gate = crate::workspace_operation_gate::acquire_workspace_operation(&repo).await;
    let gate = Arc::clone(tokio::sync::OwnedMutexGuard::mutex(&initial_gate));
    drop(initial_gate);
    let paused = session.services.elicitations.subscribe();
    let cancellation = CancellationToken::new();
    let mut response = Box::pin(runtime.handle_tool_call(call, cancellation.clone()));
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            tokio::select! {
                event = events.recv() => {
                    if let EventMsg::ApplyPatchApprovalRequest(request) = event.expect("live event stream").msg {
                        assert_eq!(request.call_id, call_id);
                        assert_eq!(request.changes.len(), 2);
                        break;
                    }
                }
                result = response.as_mut() => panic!("patch completed before approval: {result:?}"),
            }
        }
    }).await.expect("normal handler requests actual user approval");
    assert!(
        *paused.borrow(),
        "actual approval registration remains live"
    );
    assert!(futures::poll!(response.as_mut()).is_pending());
    assert_eq!(
        std::fs::read_to_string(repo.join("tracked.txt"))?,
        "before\n"
    );
    assert!(!repo.join("tail.txt").exists());
    assert!(
        gate.try_lock().is_ok(),
        "approval waits do not own the mutation gate"
    );
    cancellation.cancel();
    let result = tokio::time::timeout(Duration::from_secs(5), response)
        .await
        .expect(
            "cancel must not await unanswered approval or the thirty-second cleanup deadline",
        )?;
    let ResponseInputItem::CustomToolCallOutput {
        call_id: output_id,
        output,
        ..
    } = result
    else {
        panic!("normal apply_patch response expected");
    };
    assert_eq!(output_id, call_id);
    let FunctionCallOutputBody::Text(text) = output.body else {
        panic!("text output expected");
    };
    assert!(text.contains("aborted by user"), "{text}");
    assert!(
        !*paused.borrow(),
        "cancelled approval must release the live elicitation lease"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("tracked.txt"))?,
        "before\n"
    );
    assert!(!repo.join("tail.txt").exists());
    assert!(tracker.lock().await.get_unified_diff().is_none());
    assert!(gate.try_lock().is_ok());
    // A stale UI approval cannot revive the cancelled registered operation.
    session
        .notify_approval(call_id, ReviewDecision::Approved)
        .await;
    assert_eq!(
        std::fs::read_to_string(repo.join("tracked.txt"))?,
        "before\n"
    );
    assert!(!repo.join("tail.txt").exists());
    assert!(tracker.lock().await.get_unified_diff().is_none());
    session.abort_all_tasks(TurnAbortReason::Interrupted).await;
    Ok(())
}

struct TaskAuthorityFixture {
    _temp: tempfile::TempDir,
    repo: std::path::PathBuf,
    runtime: crate::tools::parallel::ToolCallRuntime,
    call: ToolCall,
    store: Arc<codex_agent_task_store::LocalAgentTaskStore>,
    worker: codex_agent_task_store::AgentTask,
    actor_assignment_id: codex_agent_task_store::AssignmentId,
}

async fn task_authority_fixture(
    review: bool,
    wrong_workspace: bool,
) -> anyhow::Result<TaskAuthorityFixture> {
    use codex_agent_task_store::*;
    let temp = tempfile::tempdir()?;
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo)?;
    assert!(
        Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&repo)
            .status()?
            .success()
    );
    // A real Git pointer preserves the original repository identity when the Windows
    // scheduling scenario temporarily substitutes its commondir read with a native pipe.
    std::fs::rename(repo.join(".git"), temp.path().join(".git-authority"))?;
    std::fs::write(
        repo.join(".git"),
        format!("gitdir: {}\n", temp.path().join(".git-authority").display()),
    )?;
    std::fs::write(repo.join("tracked.txt"), "before\n")?;
    assert!(
        Command::new("git")
            .args(["add", "tracked.txt"])
            .current_dir(&repo)
            .status()?
            .success()
    );
    let (mut session, mut turn) = make_session_and_context().await;
    Arc::make_mut(&mut turn.config).cwd = AbsolutePathBuf::from_absolute_path(&repo)?;
    turn.multi_agent_version = codex_protocol::protocol::MultiAgentVersion::V2;
    turn.permission_profile = PermissionProfile::Disabled;
    set_router_environment(&mut turn, &repo);
    // Keep mutable state and Git metadata outside the captured source workspace.
    let (worker_attempt, store) = enable_typed_router_task_with_state_home(
        &mut session,
        &mut turn,
        &repo,
        &["tracked.txt"],
        &temp.path().join("task-home"),
    )
    .await;
    let coordinator = session.services.agent_control.task_coordinator();
    let worker_binding = coordinator
        .binding_for_source(&turn.session_source)
        .expect("real worker binding");
    // These are persisted inputs to the authority consumer, not substitutes for its logic.
    store
        .begin_mutation(
            worker_attempt,
            &repo,
            "tracked.txt".to_string(),
            AttributionConfidence::Definitive,
        )
        .await?;
    std::fs::write(repo.join("tracked.txt"), "after\n")?;
    store
        .finalize_mutation(worker_attempt, &repo, "tracked.txt".to_string())
        .await?;
    // Fulfill the real persisted validation prerequisite with an actual command;
    // the authority tests below still enter through the registered tool router.
    let validation_id = "authority-diff-check";
    let mut validation = ValidationCall {
        call_id: validation_id.to_string(),
        attempt_id: worker_attempt,
        command_summary: "router boundary test".to_string(),
        evidence: ValidationEvidence::default(),
        status: ValidationCallStatus::Running,
        recorded_at: chrono::Utc::now(),
    };
    store.record_validation_call(validation.clone()).await?;
    let validation_started = std::time::Instant::now();
    let validation_output = Command::new("git")
        .args(["diff", "--check"])
        .current_dir(&repo)
        .output()?;
    assert!(validation_output.status.success(), "{validation_output:?}");
    assert_eq!(
        std::fs::read_to_string(repo.join("tracked.txt"))?,
        "after\n"
    );
    validation.status = ValidationCallStatus::Succeeded;
    validation.recorded_at = chrono::Utc::now();
    validation.evidence.validation_result = Some(json!({
        "argv": ["git", "diff", "--check"], "coveredPaths": ["tracked.txt"],
        "callId": validation_id, "processId": null, "status": "succeeded",
        "durationMs": validation_started.elapsed().as_millis() as u64,
    }));
    store.record_validation_call(validation).await?;
    let receipt_args = json!({"status":"completed", "summary":"authority-checked persisted receipt", "criterion_results":[{"criterion_id":"router-mutation-evidence", "status":"passed", "evidence":"tracked bytes changed before to after"}], "declared_changes":[{"path":"tracked.txt", "summary":"before to after"}], "validation_call_ids":[validation_id], "blockers":[], "risks":[], "next_action":null});
    if review {
        // Reviewer admission requires a sealed target with its review gate pending.
        // This is persisted input to get_agent_task; the non-review arm separately
        // proves normal submit_agent_receipt registration and persistence.
        let draft: ReceiptDraft = serde_json::from_value(receipt_args.clone())?;
        store
            .submit_agent_receipt_with_review(
                worker_attempt,
                draft,
                "authority fixture awaits cold review".to_string(),
            )
            .await?;
    }
    let worker = store
        .get_agent_task(worker_binding.assignment_id, Some(0))
        .await?;
    assert_eq!(worker.receipt.is_some(), review);
    assert_eq!(
        worker.current_attempt.state,
        if review {
            AttemptState::Completed
        } else {
            AttemptState::Active
        }
    );
    let assignment_id = if review {
        let (mut reviewer_session, mut reviewer_turn) = make_session_and_context().await;
        reviewer_session.services.agent_control = session.services.agent_control.clone();
        Arc::make_mut(&mut reviewer_turn.config).cwd = AbsolutePathBuf::from_absolute_path(&repo)?;
        reviewer_turn.multi_agent_version = codex_protocol::protocol::MultiAgentVersion::V2;
        reviewer_turn.permission_profile = PermissionProfile::Disabled;
        set_router_environment(&mut reviewer_turn, &repo);
        let (assignment, attempt) = coordinator
            .create_assignment(
                &repo,
                AssignmentDraft {
                    root_session_id: worker.assignment.root_session_id.clone(),
                    admission_origin: AssignmentAdmissionOrigin::Typed,
                    role: AgentRole::Reviewer,
                    capability_profile: CapabilityProfile::ReadSearchDiff,
                    objective: "review the persisted tracked-file mutation".to_string(),
                    acceptance_criteria: vec![AcceptanceCriterion {
                        id: "review".to_string(),
                        text: "review exact target evidence".to_string(),
                    }],
                    read_scope: vec![RepoScope {
                        path: "tracked.txt".to_string(),
                        recursive: false,
                    }],
                    write_scope: Vec::new(),
                    stop_condition: "target evidence inspected".to_string(),
                    dependencies: vec![worker.assignment.assignment_id],
                    risk_hints: Vec::new(),
                    required_evidence: Vec::new(),
                    prohibited_changes: Vec::new(),
                    contract_claims: Vec::new(),
                    workspace_strategy: WorkspaceStrategy::Shared,
                    relation: Some(AssignmentRelation {
                        kind: RelationKind::Review,
                        target_assignment_ids: vec![worker.assignment.assignment_id],
                    }),
                    architecture_contract_ref: None,
                },
            )
            .await?;
        let path =
            AgentPath::try_from("/root/authority_reviewer").expect("valid reviewer fixture path");
        let binding = coordinator
            .bind_agent_task(AgentTaskBindingDraft {
                assignment_id: assignment.assignment_id,
                attempt_id: attempt.attempt_id,
                agent_path: path.to_string(),
                task_name: "authority_reviewer".to_string(),
                thread_id: Some(reviewer_session.thread_id.to_string()),
            })
            .await?;
        assert!(coordinator.heartbeat_typed_actor_binding(&binding).await?);
        session = reviewer_session;
        turn = reviewer_turn;
        turn.session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: ThreadId::new(),
            depth: 1,
            agent_path: Some(path),
            agent_nickname: None,
            agent_role: Some("reviewer".to_string()),
        });
        assignment.assignment_id
    } else {
        worker.assignment.assignment_id
    };
    if wrong_workspace {
        let wrong = temp.path().join("wrong-repository");
        std::fs::create_dir_all(wrong.join(".git"))?;
        Arc::make_mut(&mut turn.config).cwd = AbsolutePathBuf::from_absolute_path(&wrong)?;
        set_router_environment(&mut turn, &wrong);
    }
    let turn = Arc::new(turn);
    let step = StepContext::for_test(Arc::clone(&turn));
    let router = Arc::new(ToolRouter::from_context(
        step.as_ref(),
        ToolRouterParams {
            tool_suggest_candidates: None,
            deferred_mcp_tools: None,
            mcp_tools: None,
            extension_tool_executors: Vec::new(),
            dynamic_tools: turn.dynamic_tools.as_slice(),
            exposure_identity: ToolExposureIdentity {
                agent_surface_stage: crate::tools::exposure::AgentSurfaceStage::TypedAdministration,
                ..Default::default()
            },
        },
        &Default::default(),
    ));
    let name = if review {
        "get_agent_task"
    } else {
        "submit_agent_receipt"
    };
    let registered = router
        .registered_tool_names_for_test()
        .into_iter()
        .find(|tool| tool.name == name)
        .expect("normal typed administration registration");
    let args = if review {
        json!({"assignment_id": assignment_id.to_string(), "observation_limit": 0})
    } else {
        receipt_args
    };
    let call = ToolRouter::build_tool_call(ResponseItem::FunctionCall {
        id: None,
        name: registered.name,
        namespace: registered.namespace,
        arguments: args.to_string(),
        call_id: "task-authority".to_string(),
        internal_chat_message_metadata_passthrough: None,
    })?
    .expect("normal function tool call");
    let runtime = crate::tools::parallel::ToolCallRuntime::new(
        Arc::new(session),
        step.with_tool_router_for_test(router),
        Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
    );
    Ok(TaskAuthorityFixture {
        _temp: temp,
        repo,
        runtime,
        call,
        store,
        worker,
        actor_assignment_id: assignment_id,
    })
}

fn task_authority_response_text(response: ResponseInputItem) -> String {
    let ResponseInputItem::FunctionCallOutput { call_id, output } = response else {
        panic!("normal function response expected")
    };
    assert_eq!(call_id, "task-authority");
    let FunctionCallOutputBody::Text(text) = output.body else {
        panic!("text function output expected")
    };
    text
}

async fn assert_task_authority_success(
    review: bool,
    text: &str,
    store: &codex_agent_task_store::LocalAgentTaskStore,
    worker: &codex_agent_task_store::AgentTask,
    actor_assignment_id: codex_agent_task_store::AssignmentId,
) -> anyhow::Result<()> {
    // Normal registry output may frame selected JSON with a projection header.
    // Decode that contract without bypassing the consumer-visible payload checks.
    let mut documents = serde_json::Deserializer::from_str(text).into_iter::<serde_json::Value>();
    let first = documents
        .next()
        .transpose()?
        .ok_or_else(|| anyhow::anyhow!("empty authority response"))?;
    let value = if first["selected_text_follows"] == true {
        assert_eq!(first["outcome"], "success");
        assert_eq!(first["canonical_complete"], true);
        assert_eq!(first["artifact"]["complete"], true);
        documents
            .next()
            .transpose()?
            .ok_or_else(|| anyhow::anyhow!("authority header promised absent selected JSON"))?
    } else {
        first
    };
    assert!(
        documents.next().is_none(),
        "unexpected trailing authority output: {text}"
    );
    let persisted = store
        .get_agent_task(worker.assignment.assignment_id, Some(0))
        .await?;
    if review {
        let context = &value["cold_review_context"];
        assert_eq!(
            context["assignment"]["assignment_id"],
            worker.assignment.assignment_id.to_string()
        );
        assert_eq!(
            context["attempt_id"],
            worker.current_attempt.attempt_id.to_string()
        );
        assert_eq!(context["nearest_tests"], json!(["router boundary test"]));
        let diff = context["attempt_specific_diff"]
            .as_str()
            .expect("persisted snapshot diff");
        assert!(diff.contains("-before\n"), "{diff}");
        assert!(diff.contains("+after\n"), "{diff}");
        assert_eq!(
            context["observed_writes"]
                .as_array()
                .expect("write evidence")
                .len(),
            1
        );
        assert_eq!(context["observed_writes"][0]["path"], "tracked.txt");
        assert_eq!(context["observed_writes"][0]["pre_write_existed"], true);
        assert_eq!(context["observed_writes"][0]["final_write_existed"], true);
        assert_ne!(
            context["observed_writes"][0]["pre_write_hash"],
            context["observed_writes"][0]["final_hash"]
        );
        assert!(context.get("worker_reasoning").is_none());
        assert!(context.get("conversation_history").is_none());
        assert_eq!(
            persisted.receipt, worker.receipt,
            "cold review must preserve its sealed target receipt"
        );
        let reviewer = store.get_agent_task(actor_assignment_id, Some(0)).await?;
        assert!(reviewer.receipt.is_none());
        assert_eq!(
            reviewer.current_attempt.state,
            codex_agent_task_store::AttemptState::Active
        );
    } else {
        let receipt = persisted
            .receipt
            .expect("normal receipt handler must persist the receipt");
        assert_eq!(receipt.summary, "authority-checked persisted receipt");
        assert_eq!(
            receipt.status,
            codex_agent_task_store::AgentStatusClaim::Completed
        );
        assert_eq!(receipt.declared_changes.len(), 1);
        assert_eq!(receipt.declared_changes[0].path, "tracked.txt");
        assert_eq!(
            value["receipt"]["assignment_id"],
            receipt.assignment_id.to_string()
        );
        assert_eq!(
            value["receipt"]["attempt_id"],
            receipt.attempt_id.to_string()
        );
        assert_eq!(value["receipt"]["summary"], receipt.summary);
    }
    Ok(())
}

#[tokio::test]
async fn router_task_authority_returns_persisted_receipt_and_cold_review() -> anyhow::Result<()> {
    for review in [false, true] {
        let fixture = task_authority_fixture(review, false).await?;
        let text = task_authority_response_text(
            fixture
                .runtime
                .handle_tool_call(fixture.call, CancellationToken::new())
                .await?,
        );
        assert_task_authority_success(
            review,
            &text,
            fixture.store.as_ref(),
            &fixture.worker,
            fixture.actor_assignment_id,
        )
        .await?;
    }
    Ok(())
}

#[tokio::test]
async fn router_task_authority_rejects_wrong_repository_without_receipt() -> anyhow::Result<()> {
    for review in [false, true] {
        let fixture = task_authority_fixture(review, true).await?;
        let text = task_authority_response_text(
            fixture
                .runtime
                .handle_tool_call(fixture.call, CancellationToken::new())
                .await?,
        );
        assert!(
            text.contains("evidence is invalid: assignment repository"),
            "{text}"
        );
        let persisted = fixture
            .store
            .get_agent_task(fixture.worker.assignment.assignment_id, Some(0))
            .await?;
        assert_eq!(
            persisted.receipt, fixture.worker.receipt,
            "authority rejection must preserve the target's prior receipt state"
        );
        assert_eq!(
            persisted.current_attempt.state,
            fixture.worker.current_attempt.state
        );
        let actor = fixture
            .store
            .get_agent_task(fixture.actor_assignment_id, Some(0))
            .await?;
        assert!(
            actor.receipt.is_none(),
            "authority failure must not seal the caller"
        );
        assert_eq!(
            actor.current_attempt.state,
            codex_agent_task_store::AttemptState::Active
        );
        assert_eq!(
            std::fs::read_to_string(fixture.repo.join("tracked.txt"))?,
            "after\n"
        );
    }
    Ok(())
}

#[cfg(windows)]
#[tokio::test(flavor = "current_thread")]
async fn router_task_authority_git_read_keeps_executor_responsive() -> anyhow::Result<()> {
    use std::time::Duration;
    use tokio::io::AsyncWriteExt;
    use tokio::net::windows::named_pipe::ServerOptions;
    for review in [false, true] {
        let fixture = task_authority_fixture(review, false).await?;
        let marker = fixture.repo.join(".git");
        let original = std::fs::read(&marker)?;
        let common_dir = fixture
            .repo
            .parent()
            .expect("fixture repository has a temp parent")
            .join(".git-authority")
            .to_string_lossy()
            .into_owned();
        let pipe_root = format!(r"\\.\pipe\codex-task-authority-{}", uuid::Uuid::new_v4());
        let pipe_name = format!(r"{pipe_root}\commondir");
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        // Receipt finalization performs exactly one already-offloaded identity read.
        // Keep the pipe marker installed through it so that only the subsequent
        // risk-policy read can satisfy the gated handshake. Cold review has no prior read.
        let prior_reads = usize::from(!review);
        let server = std::thread::spawn(move || -> anyhow::Result<bool> {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let _entered_runtime = runtime.enter();
            let mut pipes = Vec::new();
            for index in 0..=prior_reads {
                pipes.push(
                    ServerOptions::new()
                        .first_pipe_instance(index == 0)
                        .create(&pipe_name)?,
                );
            }
            ready_tx.send(()).ok();
            let mut entered_tx = Some(entered_tx);
            let mut responsive = false;
            for (index, mut pipe) in pipes.into_iter().enumerate() {
                runtime.block_on(tokio::time::timeout(
                    Duration::from_secs(10),
                    pipe.connect(),
                ))??;
                if index == prior_reads {
                    entered_tx
                        .take()
                        .expect("single policy-read handshake")
                        .send(())
                        .ok();
                    // Independent OS watchdog releases the real read even when the old
                    // inline implementation blocks the only async executor thread.
                    responsive = release_rx.recv_timeout(Duration::from_secs(3)).is_ok();
                    std::fs::write(&marker, &original)?;
                }
                runtime.block_on(pipe.write_all(common_dir.as_bytes()))?;
            }
            Ok(responsive)
        });
        ready_rx.await?;
        std::fs::write(fixture.repo.join(".git"), format!("gitdir: {pipe_root}\n"))?;
        let mut response = Box::pin(
            fixture
                .runtime
                .handle_tool_call(fixture.call, CancellationToken::new()),
        );
        let mut early = None;
        let entered = tokio::select! {
            result = entered_rx => result.is_ok(),
            result = &mut response => { early = Some(result); false },
        };
        let _ = release_tx.send(());
        let responsive = tokio::task::spawn_blocking(move || {
            server.join().expect("native pipe thread must not panic")
        })
        .await??;
        assert!(
            entered,
            "registered authority handler must reach the gated native Git read; early result: {early:?}"
        );
        assert!(
            responsive,
            "the policy Git read blocked the only async executor until the OS watchdog released it"
        );
        let result = match early {
            Some(result) => result,
            None => tokio::time::timeout(Duration::from_secs(10), response).await?,
        }?;
        let text = task_authority_response_text(result);
        assert_task_authority_success(
            review,
            &text,
            fixture.store.as_ref(),
            &fixture.worker,
            fixture.actor_assignment_id,
        )
        .await?;
    }
    Ok(())
}
