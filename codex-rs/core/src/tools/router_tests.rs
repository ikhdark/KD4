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
    let (_session, turn) = make_session_and_context().await;
    let router = ToolRouter::from_parts_with_warnings_and_identity(
        ToolRegistry::empty_for_test(),
        Vec::new(),
        Vec::new(),
        ToolExposureIdentity::default(),
    );

    let first = router.tool_manifest(&turn);
    let second = router.tool_manifest(&turn);
    assert_eq!(first, second);
    assert_eq!(turn.deferred_tool_activation_revision(), 0);
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
    write_path: &str,
) -> (
    codex_agent_task_store::AttemptId,
    Arc<codex_agent_task_store::LocalAgentTaskStore>,
) {
    let root_session_id = "router-apply-patch-root".to_string();
    let state_runtime =
        codex_state::StateRuntime::init(repo.join(".typed-task-home"), "test-provider".to_string())
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
                write_scope: vec![codex_agent_task_store::RepoScope {
                    path: write_path.to_string(),
                    recursive: false,
                }],
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
    coordinator
        .bind_agent_task(codex_agent_task_store::AgentTaskBindingDraft {
            assignment_id: assignment.assignment_id,
            attempt_id: attempt.attempt_id,
            agent_path: agent_path.to_string(),
            task_name: "router_apply_patch_worker".to_string(),
            thread_id: None,
        })
        .await
        .expect("typed assignment is bound");
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
            assert!(
                authorize_independent_review_tool_call(
                    &source,
                    class,
                    &call,
                    ExternalMutationIntent::MayMutate,
                )
                .is_err()
            );
        }
    }
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

    let (mut session, mut turn) = make_session_and_context().await;
    set_router_environment(&mut turn, &repo);
    turn.permission_profile = PermissionProfile::Disabled;
    turn.model_info.apply_patch_tool_type = Some(ApplyPatchToolType::Freeform);
    let (attempt_id, store) =
        enable_typed_router_task(&mut session, &mut turn, &repo, "tracked.txt").await;
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
        input: "*** Begin Patch\n*** Update File: tracked.txt\n@@\n-before\n+after\n*** End Patch"
            .to_string(),
        internal_chat_message_metadata_passthrough: None,
    })?
    .expect("custom tool call");
    let terminal_outcome_reached = admitted_tool_dispatch_state();
    router
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
    assert_eq!(
        std::fs::read_to_string(repo.join("tracked.txt")).expect("read patched file"),
        "after\n"
    );

    let evidence = store
        .list_mutation_evidence(
            attempt_id,
            Some(codex_agent_task_store::MAX_MUTATION_EVIDENCE_LIMIT),
        )
        .await
        .expect("mutation evidence remains queryable");
    assert_eq!(evidence.len(), 1);
    assert_eq!(evidence[0].path, "tracked.txt");
    assert_ne!(evidence[0].pre_write_hash, evidence[0].final_hash);
    assert!(evidence[0].finalized_at.is_some());
    assert!(evidence[0].end_epoch.is_some());

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
