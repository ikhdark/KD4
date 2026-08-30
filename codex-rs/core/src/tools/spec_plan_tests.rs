use std::collections::BTreeMap;
use std::sync::Arc;

use codex_features::Feature;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_mcp::ToolInfo;
use codex_model_provider::create_model_provider;
use codex_model_provider_info::AMAZON_BEDROCK_PROVIDER_ID;
use codex_model_provider_info::ModelProviderInfo;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::WebSearchMode;
use codex_protocol::config_types::WindowsSandboxLevel;
use codex_protocol::dynamic_tools::DynamicToolFunctionSpec;
use codex_protocol::dynamic_tools::DynamicToolNamespaceSpec;
use codex_protocol::dynamic_tools::DynamicToolNamespaceTool;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use codex_protocol::models::PermissionProfile;
use codex_protocol::openai_models::ApplyPatchToolType;
use codex_protocol::openai_models::ConfigShellToolType;
use codex_protocol::openai_models::InputModality;
use codex_protocol::openai_models::ToolMode;
use codex_protocol::openai_models::WebSearchToolType;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_tools::DiscoverablePluginInfo;
use codex_tools::DiscoverableTool;
use codex_tools::ResponsesApiNamespaceTool;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolCall as ExtensionToolCall;
use codex_tools::ToolExecutor;
use codex_tools::ToolExposure;
use codex_tools::ToolName;
use codex_tools::ToolOutput;
use codex_tools::ToolSpec;
use codex_utils_path_uri::PathUri;
use pretty_assertions::assert_eq;
use serde_json::json;

use super::active_collaboration_namespace;
use super::merge_into_namespaces;
use crate::agent::task_capabilities::ExternalMutationIntent;
use crate::agent::task_capabilities::TypedToolClass;
use crate::config::CurrentTimeReminderConfig;
use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context;
use crate::session::turn_context::TurnContext;
use crate::tools::exposure::AgentSurfaceStage;
use crate::tools::exposure::ToolExposureIdentity;
use crate::tools::handlers::ToolSearchHandlerCache;
use crate::tools::handlers::multi_agents_spec::MULTI_AGENT_V1_NAMESPACE;
use crate::tools::router::ToolRouter;
use crate::tools::router::ToolRouterParams;
use crate::tools::router::ToolSuggestCandidates;
use crate::tools::router::ToolSuggestPresentation;

const MULTI_AGENT_V2_NAMESPACE: &str = "agents";

#[test]
fn merged_namespace_descriptions_share_one_hard_budget_in_stable_spec_order() {
    let specs = vec![
        ToolSpec::Namespace(codex_tools::ResponsesApiNamespace {
            name: "zeta".to_string(),
            description: "z".repeat(60_000),
            tools: Vec::new(),
        }),
        ToolSpec::Namespace(codex_tools::ResponsesApiNamespace {
            name: "alpha".to_string(),
            description: "Short alpha description.".to_string(),
            tools: Vec::new(),
        }),
        ToolSpec::Namespace(codex_tools::ResponsesApiNamespace {
            name: "beta".to_string(),
            description: String::new(),
            tools: Vec::new(),
        }),
    ];

    let merged = merge_into_namespaces(specs);
    let ToolSpec::Namespace(first_namespace) = &merged[0] else {
        panic!("expected first merged spec to be a namespace");
    };
    assert_eq!(first_namespace.name, "zeta");
    assert!(!first_namespace.description.is_empty());
    assert!(first_namespace.description.len() < 60_000);
    let ToolSpec::Namespace(second_namespace) = &merged[1] else {
        panic!("expected second merged spec to be a namespace");
    };
    assert_eq!(second_namespace.name, "alpha");
    assert_eq!(second_namespace.description, "Short alpha description.");
    let ToolSpec::Namespace(third_namespace) = &merged[2] else {
        panic!("expected third merged spec to be a namespace");
    };
    assert_eq!(third_namespace.name, "beta");
    assert!(!third_namespace.description.is_empty());
    let merged_bytes = merged
        .iter()
        .map(|spec| match spec {
            ToolSpec::Namespace(namespace) => namespace.description.len(),
            _ => 0,
        })
        .sum::<usize>();
    assert!(merged_bytes <= 40_000);
}

#[derive(Default)]
struct ToolPlanInputs {
    mcp_tools: Option<Vec<ToolInfo>>,
    deferred_mcp_tools: Option<Vec<ToolInfo>>,
    tool_suggest_candidates: Option<ToolSuggestCandidates>,
    extension_tool_executors: Vec<Arc<dyn ToolExecutor<ExtensionToolCall>>>,
    dynamic_tools: Vec<DynamicToolSpec>,
    exposure_identity: ToolExposureIdentity,
}

struct ToolPlanProbe {
    visible_specs: Vec<ToolSpec>,
    visible_names: Vec<String>,
    namespace_functions: BTreeMap<String, Vec<String>>,
    registered_names: Vec<String>,
    exposures: BTreeMap<String, ToolExposure>,
    authorization_classes: BTreeMap<String, TypedToolClass>,
    external_mutation_intents: BTreeMap<String, ExternalMutationIntent>,
    tool_search_texts: Vec<String>,
    tool_search_namespace_descriptions: BTreeMap<String, String>,
    warnings: Vec<String>,
}

impl ToolPlanProbe {
    fn from_router(router: ToolRouter, tool_search_cache: &ToolSearchHandlerCache) -> Self {
        let visible_specs = router.model_visible_specs();
        let warnings = router.planning_warnings().to_vec();
        let visible_names = visible_specs
            .iter()
            .map(|spec| spec.name().to_string())
            .collect::<Vec<_>>();
        let namespace_functions = visible_specs
            .iter()
            .filter_map(|spec| match spec {
                ToolSpec::Namespace(namespace) => Some((
                    namespace.name.clone(),
                    namespace
                        .tools
                        .iter()
                        .map(|tool| match tool {
                            ResponsesApiNamespaceTool::Function(tool) => tool.name.clone(),
                        })
                        .collect::<Vec<_>>(),
                )),
                ToolSpec::Function(_)
                | ToolSpec::ToolSearch { .. }
                | ToolSpec::WebSearch { .. }
                | ToolSpec::Freeform(_) => None,
            })
            .collect::<BTreeMap<_, _>>();
        let registered_tool_names = router.registered_tool_names_for_test();
        let registered_names = registered_tool_names
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        let exposures = registered_tool_names
            .iter()
            .filter_map(|name| {
                router
                    .tool_exposure_for_test(name)
                    .map(|exposure| (name.to_string(), exposure))
            })
            .collect::<BTreeMap<_, _>>();
        let authorization_classes = registered_tool_names
            .iter()
            .filter_map(|name| {
                router
                    .tool_authorization_class_for_test(name)
                    .map(|class| (name.to_string(), class))
            })
            .collect::<BTreeMap<_, _>>();
        let external_mutation_intents = registered_tool_names
            .iter()
            .filter_map(|name| {
                router
                    .tool_external_mutation_intent_for_test(name)
                    .map(|intent| (name.to_string(), intent))
            })
            .collect::<BTreeMap<_, _>>();
        let search_infos = tool_search_cache.search_infos_for_test();
        let tool_search_texts = search_infos
            .iter()
            .map(|info| info.entry.search_text.clone())
            .collect();
        let tool_search_namespace_descriptions = search_infos
            .into_iter()
            .filter_map(|info| match info.entry.output {
                codex_tools::LoadableToolSpec::Namespace(namespace) => {
                    Some((namespace.name, namespace.description))
                }
                codex_tools::LoadableToolSpec::Function(_) => None,
            })
            .collect();

        Self {
            visible_specs,
            visible_names,
            namespace_functions,
            registered_names,
            exposures,
            authorization_classes,
            external_mutation_intents,
            tool_search_texts,
            tool_search_namespace_descriptions,
            warnings,
        }
    }

    fn assert_visible_contains(&self, expected: &[&str]) {
        for name in expected {
            assert!(
                self.visible_names.iter().any(|visible| visible == name),
                "expected visible tool `{name}` in {:?}",
                self.visible_names
            );
        }
    }

    fn assert_visible_lacks(&self, expected_absent: &[&str]) {
        for name in expected_absent {
            assert!(
                !self.visible_names.iter().any(|visible| visible == name),
                "expected visible tool `{name}` to be absent from {:?}",
                self.visible_names
            );
        }
    }

    fn assert_registered_contains(&self, expected: &[&str]) {
        for name in expected {
            assert!(
                self.registered_names
                    .iter()
                    .any(|registered| registered == name),
                "expected registered tool `{name}` in {:?}",
                self.registered_names
            );
        }
    }

    fn assert_registered_lacks(&self, expected_absent: &[&str]) {
        for name in expected_absent {
            assert!(
                !self
                    .registered_names
                    .iter()
                    .any(|registered| registered == name),
                "expected registered tool `{name}` to be absent from {:?}",
                self.registered_names
            );
        }
    }

    fn namespace_function_names(&self, namespace: &str) -> &[String] {
        self.namespace_functions
            .get(namespace)
            .map_or(&[], Vec::as_slice)
    }

    fn visible_spec(&self, name: &str) -> &ToolSpec {
        self.visible_specs
            .iter()
            .find(|spec| spec.name() == name)
            .unwrap_or_else(|| panic!("expected visible spec `{name}` in {:?}", self.visible_names))
    }

    fn exposure(&self, name: &str) -> ToolExposure {
        *self
            .exposures
            .get(name)
            .unwrap_or_else(|| panic!("expected registered tool `{name}`"))
    }

    fn authorization_class(&self, name: &str) -> TypedToolClass {
        *self
            .authorization_classes
            .get(name)
            .unwrap_or_else(|| panic!("expected registered tool `{name}`"))
    }

    fn assert_no_unknown_authorization_classes(&self) {
        let unknown = self
            .authorization_classes
            .iter()
            .filter_map(|(name, class)| (*class == TypedToolClass::Unknown).then_some(name))
            .collect::<Vec<_>>();
        assert!(
            unknown.is_empty(),
            "production tool registrations require an explicit authorization class: {unknown:?}"
        );
    }
}

async fn probe_with(
    configure_turn: impl FnOnce(&mut TurnContext),
    inputs: ToolPlanInputs,
) -> ToolPlanProbe {
    let (_session, mut turn) = make_session_and_context().await;
    configure_turn(&mut turn);
    let turn = Arc::new(turn);
    let step_context = StepContext::for_test(Arc::clone(&turn));
    let tool_search_cache = ToolSearchHandlerCache::default();
    let router = ToolRouter::from_context(
        step_context.as_ref(),
        ToolRouterParams {
            tool_suggest_candidates: inputs.tool_suggest_candidates,
            mcp_tools: inputs.mcp_tools,
            deferred_mcp_tools: inputs.deferred_mcp_tools,
            extension_tool_executors: inputs.extension_tool_executors,
            dynamic_tools: inputs.dynamic_tools.as_slice(),
            exposure_identity: inputs.exposure_identity,
        },
        &tool_search_cache,
    );
    ToolPlanProbe::from_router(router, &tool_search_cache)
}

async fn probe(configure_turn: impl FnOnce(&mut TurnContext)) -> ToolPlanProbe {
    probe_with(configure_turn, ToolPlanInputs::default()).await
}

#[tokio::test]
async fn update_plan_is_not_exposed_or_registered_in_plan_mode() {
    let default_mode = probe(|_| {}).await;
    default_mode.assert_visible_contains(&["update_plan"]);
    default_mode.assert_registered_contains(&["update_plan"]);

    let plan_mode = probe(|turn| {
        turn.collaboration_mode.mode = ModeKind::Plan;
    })
    .await;
    plan_mode.assert_visible_lacks(&["update_plan"]);
    plan_mode.assert_registered_lacks(&["update_plan"]);
}

#[tokio::test]
async fn read_turn_timing_is_absent_from_tool_plans() {
    let searchable = probe(|turn| {
        turn.model_info.supports_search_tool = true;
    })
    .await;
    searchable.assert_registered_lacks(&["read_turn_timing"]);
    searchable.assert_visible_lacks(&["read_turn_timing"]);

    let unsearchable = probe(|turn| {
        turn.model_info.supports_search_tool = false;
    })
    .await;
    unsearchable.assert_registered_lacks(&["read_turn_timing"]);
    unsearchable.assert_visible_lacks(&["read_turn_timing"]);
}

#[tokio::test]
async fn production_tool_registrations_have_explicit_authorization_classes() {
    let utilities = probe(|turn| {
        set_features(
            turn,
            &[
                Feature::CurrentTimeReminder,
                Feature::RequestPermissionsTool,
            ],
        );
        let mut config = (*turn.config).clone();
        config.current_time_reminder = Some(CurrentTimeReminderConfig {
            sleep_tool: true,
            ..CurrentTimeReminderConfig::default()
        });
        turn.config = Arc::new(config);
        turn.model_info
            .experimental_supported_tools
            .push("test_sync_tool".to_string());
    })
    .await;
    utilities.assert_no_unknown_authorization_classes();

    let plugins = probe_with(
        |turn| {
            set_features(
                turn,
                &[Feature::ToolSuggest, Feature::Apps, Feature::Plugins],
            );
        },
        ToolPlanInputs {
            tool_suggest_candidates: Some(plugin_candidates(ToolSuggestPresentation::ListTool)),
            ..ToolPlanInputs::default()
        },
    )
    .await;
    plugins.assert_no_unknown_authorization_classes();
}

fn set_feature(turn: &mut TurnContext, feature: Feature, enabled: bool) {
    let mut config = (*turn.config).clone();
    if enabled {
        config
            .features
            .enable(feature)
            .expect("test feature should be enableable in config");
    } else {
        config
            .features
            .disable(feature)
            .expect("test feature should be disableable in config");
    }
    turn.multi_agent_version = config.multi_agent_version_from_features();
    turn.config = Arc::new(config);
}

fn set_features(turn: &mut TurnContext, features: &[Feature]) {
    for feature in features {
        set_feature(turn, *feature, /*enabled*/ true);
    }
}

fn update_config(turn: &mut TurnContext, update: impl FnOnce(&mut crate::config::Config)) {
    let mut config = (*turn.config).clone();
    update(&mut config);
    turn.config = Arc::new(config);
}

fn set_web_search_mode(turn: &mut TurnContext, mode: WebSearchMode) {
    update_config(turn, |config| {
        config
            .web_search_mode
            .set(mode)
            .expect("test web search mode should be accepted");
    });
}

fn use_chatgpt_auth(turn: &mut TurnContext) {
    turn.auth_manager = Some(AuthManager::from_auth_for_testing(
        CodexAuth::create_dummy_chatgpt_auth_for_testing(),
    ));
    turn.provider = create_model_provider(
        turn.config.model_provider.clone(),
        turn.auth_manager.clone(),
    );
}

fn use_bedrock_provider(turn: &mut TurnContext) {
    let provider_info = ModelProviderInfo::create_amazon_bedrock_provider(/*aws*/ None);
    update_config(turn, |config| {
        config.model_provider_id = AMAZON_BEDROCK_PROVIDER_ID.to_string();
        config.model_provider = provider_info.clone();
    });
    turn.provider = create_model_provider(provider_info, turn.auth_manager.clone());
}

struct TestNamespaceExtensionTool {
    namespace: &'static str,
    tool_name: &'static str,
}

impl ToolExecutor<ExtensionToolCall> for TestNamespaceExtensionTool {
    fn tool_name(&self) -> ToolName {
        ToolName::namespaced(self.namespace, self.tool_name)
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Namespace(codex_tools::ResponsesApiNamespace {
            name: self.namespace.to_string(),
            description: "Test namespace.".to_string(),
            tools: vec![ResponsesApiNamespaceTool::Function(ResponsesApiTool {
                name: self.tool_name.to_string(),
                description: "Test namespace tool.".to_string(),
                strict: false,
                defer_loading: None,
                parameters: codex_tools::JsonSchema::default(),
                output_schema: None,
            })],
        })
    }

    fn handle(&self, _call: ExtensionToolCall) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async {
            Ok(Box::new(codex_tools::JsonToolOutput::new(json!({}))) as Box<dyn ToolOutput>)
        })
    }
}

struct DeferredExtensionTool {
    description: &'static str,
}

impl ToolExecutor<ExtensionToolCall> for DeferredExtensionTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("extension_echo")
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(ResponsesApiTool {
            name: "extension_echo".to_string(),
            description: self.description.to_string(),
            strict: true,
            defer_loading: None,
            parameters: codex_tools::JsonSchema::object(
                BTreeMap::from([(
                    "message".to_string(),
                    codex_tools::JsonSchema::string(/*description*/ None),
                )]),
                Some(vec!["message".to_string()]),
                Some(false.into()),
            ),
            output_schema: None,
        })
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::Deferred
    }

    fn handle(&self, _call: ExtensionToolCall) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async { panic!("spec planning should not execute extension tools") })
    }
}

struct DeferredExtensionWithoutSearchInfo;

impl ToolExecutor<ExtensionToolCall> for DeferredExtensionWithoutSearchInfo {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("undiscoverable_extension")
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(ResponsesApiTool {
            name: "undiscoverable_extension".to_string(),
            description: "Synthetic deferred tool without search metadata.".to_string(),
            strict: true,
            defer_loading: None,
            parameters: codex_tools::JsonSchema::default(),
            output_schema: None,
        })
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::Deferred
    }

    fn search_info(&self) -> Option<codex_tools::ToolSearchInfo> {
        None
    }

    fn handle(&self, _call: ExtensionToolCall) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async { panic!("spec planning should not execute extension tools") })
    }
}

struct MalformedExtensionTool {
    namespace: &'static str,
    tool_name: &'static str,
}

impl ToolExecutor<ExtensionToolCall> for MalformedExtensionTool {
    fn tool_name(&self) -> ToolName {
        ToolName::namespaced(self.namespace, self.tool_name)
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Namespace(codex_tools::ResponsesApiNamespace {
            name: self.namespace.to_string(),
            description: "Malformed empty namespace.".to_string(),
            tools: Vec::new(),
        })
    }

    fn handle(&self, _call: ExtensionToolCall) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async { panic!("spec planning should not execute extension tools") })
    }
}

struct DeferredNamespaceExtensionTool {
    namespace: &'static str,
    tool_name: &'static str,
}

impl ToolExecutor<ExtensionToolCall> for DeferredNamespaceExtensionTool {
    fn tool_name(&self) -> ToolName {
        ToolName::namespaced(self.namespace, self.tool_name)
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Namespace(codex_tools::ResponsesApiNamespace {
            name: self.namespace.to_string(),
            description: "Test deferred namespace.".to_string(),
            tools: vec![ResponsesApiNamespaceTool::Function(ResponsesApiTool {
                name: self.tool_name.to_string(),
                description: "Test deferred namespace tool.".to_string(),
                strict: false,
                defer_loading: None,
                parameters: codex_tools::JsonSchema::default(),
                output_schema: None,
            })],
        })
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::Deferred
    }

    fn handle(&self, _call: ExtensionToolCall) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async { panic!("spec planning should not execute extension tools") })
    }
}

fn duplicate_primary_environment(turn: &mut TurnContext) {
    let mut second_environment = turn.environments.turn_environments[0].clone();
    second_environment.environment_id = "secondary".to_string();
    turn.environments.turn_environments.push(second_environment);
}

fn set_foreign_primary_environment(turn: &mut TurnContext) {
    let remote_environment = Arc::new(
        codex_exec_server::Environment::create_for_tests(Some(
            "ws://127.0.0.1:1/foreign-primary".to_string(),
        ))
        .expect("remote test environment"),
    );
    let foreign_cwd = PathUri::parse("file:///tmp/codex-foreign-primary").expect("POSIX cwd URI");
    let shell = turn.environments.turn_environments[0].shell.clone();
    turn.environments.turn_environments[0] = crate::session::turn_context::TurnEnvironment::new(
        "remote-primary".to_string(),
        remote_environment,
        foreign_cwd,
        shell,
    );
}

fn mcp_tool(server: &str, namespace: &str, name: &str) -> ToolInfo {
    ToolInfo {
        server_name: server.to_string(),
        supports_parallel_tool_calls: false,
        server_origin: None,
        callable_name: name.to_string(),
        callable_namespace: namespace.to_string(),
        namespace_description: Some(format!("Tools from {server}.")),
        tool: rmcp::model::Tool::new(
            name.to_string(),
            format!("{name} test tool"),
            Arc::new(rmcp::model::object(json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false,
            }))),
        ),
        connector_id: None,
        connector_name: None,
        plugin_display_names: Vec::new(),
    }
}

#[tokio::test]
async fn direct_and_discovered_namespaces_share_the_budgeted_description() {
    let long_description = "namespace context ".repeat(4_000);
    let mut direct = mcp_tool("shared", "mcp__shared", "direct_lookup");
    direct.namespace_description = Some(long_description.clone());
    let mut deferred = mcp_tool("shared", "mcp__shared", "deferred_lookup");
    deferred.namespace_description = Some(long_description.clone());

    let plan = probe_with(
        |turn| turn.model_info.supports_search_tool = true,
        ToolPlanInputs {
            mcp_tools: Some(vec![direct]),
            deferred_mcp_tools: Some(vec![deferred]),
            ..ToolPlanInputs::default()
        },
    )
    .await;

    let ToolSpec::Namespace(visible) = plan.visible_spec("mcp__shared") else {
        panic!("expected direct namespace");
    };
    let discovered = plan
        .tool_search_namespace_descriptions
        .get("mcp__shared")
        .expect("deferred namespace description");
    assert_eq!(&visible.description, discovered);
    assert!(visible.description.len() < long_description.len());
}

fn invalid_mcp_tool(server: &str, namespace: &str, name: &str) -> ToolInfo {
    let mut tool = mcp_tool(server, namespace, name);
    tool.tool.input_schema = Arc::new(rmcp::model::object(json!({
        "type": "null",
    })));
    tool
}

fn dynamic_tool(namespace: Option<&str>, name: &str, defer_loading: bool) -> DynamicToolSpec {
    let function = codex_protocol::dynamic_tools::DynamicToolFunctionSpec {
        name: name.to_string(),
        description: format!("{name} dynamic tool"),
        input_schema: json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false,
        }),
        defer_loading,
    };
    match namespace {
        Some(namespace) => {
            DynamicToolSpec::Namespace(codex_protocol::dynamic_tools::DynamicToolNamespaceSpec {
                name: namespace.to_string(),
                description: format!("{namespace} dynamic tools"),
                tools: vec![
                    codex_protocol::dynamic_tools::DynamicToolNamespaceTool::Function(function),
                ],
            })
        }
        None => DynamicToolSpec::Function(function),
    }
}

fn plugin_candidates(presentation: ToolSuggestPresentation) -> ToolSuggestCandidates {
    ToolSuggestCandidates {
        tools: vec![DiscoverableTool::Plugin(Box::new(DiscoverablePluginInfo {
            id: "github@openai-curated-remote".to_string(),
            remote_plugin_id: None,
            name: "GitHub".to_string(),
            description: Some("Work with GitHub repositories".to_string()),
            has_skills: true,
            mcp_server_names: Vec::new(),
            app_connector_ids: Vec::new(),
        }))],
        presentation,
    }
}

fn has_parameter(spec: &ToolSpec, parameter_name: &str) -> bool {
    serde_json::to_value(spec)
        .expect("tool spec should serialize")
        .pointer(&format!("/parameters/properties/{parameter_name}"))
        .is_some()
}

fn apply_patch_accepts_environment_id(spec: &ToolSpec) -> bool {
    match spec {
        ToolSpec::Freeform(tool) if tool.name == "apply_patch" => {
            tool.format.definition.contains("Environment ID")
        }
        _ => false,
    }
}

#[tokio::test]
async fn request_user_input_tool_respects_experimental_config_gate() {
    let enabled = probe(|_| {}).await;
    enabled.assert_visible_contains(&["request_user_input"]);
    enabled.assert_registered_contains(&["request_user_input"]);
    assert_eq!(
        enabled.exposure("request_user_input"),
        ToolExposure::DirectModelOnly
    );

    let disabled = probe(|turn| {
        update_config(turn, |config| {
            config.experimental_request_user_input_enabled = false;
        });
    })
    .await;
    disabled.assert_visible_lacks(&["request_user_input"]);
    disabled.assert_registered_lacks(&["request_user_input"]);
}

#[tokio::test]
async fn request_user_input_respects_coarse_mode_and_role_eligibility() {
    for request_user_input_eligible in [false, true] {
        let identity = ToolExposureIdentity {
            request_user_input_eligible,
            ..ToolExposureIdentity::default()
        };
        let plan = probe_with(
            |_| {},
            ToolPlanInputs {
                exposure_identity: identity,
                ..ToolPlanInputs::default()
            },
        )
        .await;
        if request_user_input_eligible {
            plan.assert_visible_contains(&["request_user_input"]);
            plan.assert_registered_contains(&["request_user_input"]);
        } else {
            plan.assert_visible_lacks(&["request_user_input"]);
            plan.assert_registered_lacks(&["request_user_input"]);
        }
    }
}

#[tokio::test]
async fn wait_is_always_registered_when_code_mode_is_enabled() {
    let plan = probe_with(
        |turn| set_features(turn, &[Feature::CodeMode, Feature::CodeModeOnly]),
        ToolPlanInputs::default(),
    )
    .await;

    plan.assert_visible_contains(&[
        codex_code_mode::PUBLIC_TOOL_NAME,
        codex_code_mode::WAIT_TOOL_NAME,
    ]);
    plan.assert_registered_contains(&[codex_code_mode::WAIT_TOOL_NAME]);
}

#[tokio::test]
async fn code_mode_identifier_collision_is_disambiguated_during_router_planning() {
    let (_session, mut turn) = make_session_and_context().await;
    set_feature(&mut turn, Feature::CodeMode, /*enabled*/ true);
    let turn = Arc::new(turn);
    let step_context = StepContext::for_test(Arc::clone(&turn));
    let tool_search_cache = ToolSearchHandlerCache::default();
    let function = |name: &str| DynamicToolFunctionSpec {
        name: name.to_string(),
        description: format!("{name} dynamic tool"),
        input_schema: json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false,
        }),
        defer_loading: false,
    };
    let dynamic_tools = vec![
        DynamicToolSpec::Function(function("acme__lookup")),
        DynamicToolSpec::Namespace(DynamicToolNamespaceSpec {
            name: "acme".to_string(),
            description: "Acme tools".to_string(),
            tools: vec![DynamicToolNamespaceTool::Function(function("lookup"))],
        }),
    ];

    let _router = ToolRouter::try_from_context(
        step_context.as_ref(),
        ToolRouterParams {
            mcp_tools: None,
            deferred_mcp_tools: None,
            tool_suggest_candidates: None,
            extension_tool_executors: Vec::new(),
            dynamic_tools: dynamic_tools.as_slice(),
            exposure_identity: ToolExposureIdentity::default(),
        },
        &tool_search_cache,
    )
    .expect("colliding flattened names should receive distinct code-mode globals");
}

#[tokio::test]
async fn request_user_input_stays_direct_in_code_mode_only() {
    let plan = probe(|turn| {
        set_features(turn, &[Feature::CodeMode, Feature::CodeModeOnly]);
    })
    .await;

    plan.assert_visible_contains(&[
        "request_user_input",
        codex_code_mode::PUBLIC_TOOL_NAME,
        codex_code_mode::WAIT_TOOL_NAME,
    ]);
    plan.assert_registered_contains(&["request_user_input"]);
    assert_eq!(
        plan.exposure("request_user_input"),
        ToolExposure::DirectModelOnly
    );

    let ToolSpec::Freeform(exec) = plan.visible_spec(codex_code_mode::PUBLIC_TOOL_NAME) else {
        panic!("expected code mode exec tool");
    };
    assert!(exec.description.contains("direct-only tools stay outside"));
    assert!(exec.description.contains("request_user_input"));
}

#[tokio::test]
async fn token_efficiency_code_mode_direct_tools_do_not_repeat_nested_declarations() {
    let plan = probe_with(
        |turn| set_feature(turn, Feature::CodeMode, /*enabled*/ true),
        ToolPlanInputs {
            dynamic_tools: vec![dynamic_tool(None, "lookup", /*defer_loading*/ false)],
            ..ToolPlanInputs::default()
        },
    )
    .await;

    let ToolSpec::Function(tool) = plan.visible_spec("lookup") else {
        panic!("expected directly visible lookup tool");
    };
    assert_eq!(tool.description, "lookup dynamic tool");
    assert!(!tool.description.contains("exec tool declaration"));
    let serialized = serde_json::to_value(plan.visible_spec("lookup"))
        .expect("direct tool schema should serialize");
    assert_eq!(serialized["parameters"]["type"], "object");

    let ToolSpec::Freeform(exec) = plan.visible_spec(codex_code_mode::PUBLIC_TOOL_NAME) else {
        panic!("expected code mode exec tool");
    };
    assert!(exec.description.contains("lookup(args:"));
}

#[tokio::test]
async fn shell_family_registers_visible_unified_exec_and_hidden_legacy_shell() {
    let plan = probe(|turn| {
        set_features(turn, &[Feature::ShellTool, Feature::UnifiedExec]);
        turn.model_info.shell_type = ConfigShellToolType::ShellCommand;
    })
    .await;

    plan.assert_visible_contains(&["exec_command", "write_stdin"]);
    plan.assert_visible_lacks(&["shell_command"]);
    plan.assert_registered_contains(&["exec_command", "write_stdin", "shell_command"]);
    assert_eq!(plan.exposure("shell_command"), ToolExposure::Hidden);
    assert!(has_parameter(plan.visible_spec("exec_command"), "shell"));
}

#[tokio::test]
async fn shell_family_follows_default_unified_exec_policy() {
    let plan = probe(|turn| {
        turn.model_info.shell_type = ConfigShellToolType::ShellCommand;
    })
    .await;

    if codex_utils_pty::conpty_supported() {
        plan.assert_visible_contains(&["exec_command", "write_stdin"]);
        plan.assert_visible_lacks(&["shell_command"]);
        plan.assert_registered_contains(&["exec_command", "write_stdin", "shell_command"]);
        assert_eq!(plan.exposure("shell_command"), ToolExposure::Hidden);
    } else {
        plan.assert_visible_contains(&["shell_command"]);
        plan.assert_visible_lacks(&["exec_command", "write_stdin"]);
    }
}

#[tokio::test]
async fn foreign_primary_hides_exec_command_only_when_platform_sandboxing_is_required() {
    let sandboxed = probe(|turn| {
        set_foreign_primary_environment(turn);
        turn.permission_profile = PermissionProfile::workspace_write();
        turn.windows_sandbox_level = WindowsSandboxLevel::RestrictedToken;
        set_features(turn, &[Feature::ShellTool, Feature::UnifiedExec]);
        turn.model_info.shell_type = ConfigShellToolType::ShellCommand;
    })
    .await;
    sandboxed.assert_visible_lacks(&["exec_command"]);
    sandboxed.assert_registered_lacks(&["exec_command"]);
    sandboxed.assert_registered_contains(&["write_stdin", "shell_command"]);

    let unsandboxed = probe(|turn| {
        set_foreign_primary_environment(turn);
        turn.permission_profile = PermissionProfile::Disabled;
        set_features(turn, &[Feature::ShellTool, Feature::UnifiedExec]);
        turn.model_info.shell_type = ConfigShellToolType::ShellCommand;
    })
    .await;
    unsandboxed.assert_visible_contains(&["exec_command", "write_stdin"]);
    unsandboxed.assert_registered_contains(&["exec_command", "write_stdin", "shell_command"]);
}

#[tokio::test]
async fn environment_count_controls_environment_backed_tools() {
    let no_environment = probe(|turn| {
        turn.environments.turn_environments.clear();
        set_feature(turn, Feature::ShellTool, /*enabled*/ true);
        set_feature(turn, Feature::RequestPermissionsTool, /*enabled*/ true);
        turn.model_info.apply_patch_tool_type = Some(ApplyPatchToolType::Freeform);
    })
    .await;
    no_environment.assert_visible_lacks(&[
        "shell_command",
        "exec_command",
        "apply_patch",
        "view_image",
        "request_permissions",
    ]);
    no_environment.assert_registered_lacks(&[
        "shell_command",
        "exec_command",
        "apply_patch",
        "view_image",
        "request_permissions",
    ]);

    let multiple_environments = probe(|turn| {
        duplicate_primary_environment(turn);
        set_feature(turn, Feature::ShellTool, /*enabled*/ true);
        set_feature(turn, Feature::UnifiedExec, /*enabled*/ true);
        set_feature(turn, Feature::RequestPermissionsTool, /*enabled*/ true);
        turn.model_info.apply_patch_tool_type = Some(ApplyPatchToolType::Freeform);
    })
    .await;
    multiple_environments.assert_visible_contains(&[
        "exec_command",
        "apply_patch",
        "view_image",
        "request_permissions",
    ]);
    assert!(has_parameter(
        multiple_environments.visible_spec("exec_command"),
        "environment_id"
    ));
    assert!(apply_patch_accepts_environment_id(
        multiple_environments.visible_spec("apply_patch")
    ));
    assert!(has_parameter(
        multiple_environments.visible_spec("view_image"),
        "environment_id"
    ));
}

#[tokio::test]
async fn view_image_registration_requires_image_input_support() {
    let image_capable = probe(|turn| {
        turn.model_info.input_modalities = vec![InputModality::Text, InputModality::Image];
    })
    .await;
    image_capable.assert_visible_contains(&["view_image"]);
    image_capable.assert_registered_contains(&["view_image"]);

    let review_image_capable = probe(|turn| {
        turn.session_source = SessionSource::SubAgent(SubAgentSource::Review);
        turn.model_info.input_modalities = vec![InputModality::Text, InputModality::Image];
    })
    .await;
    review_image_capable.assert_visible_lacks(&["view_image"]);
    review_image_capable.assert_registered_lacks(&["view_image"]);

    let text_only = probe(|turn| {
        turn.model_info.input_modalities = vec![InputModality::Text];
    })
    .await;
    text_only.assert_visible_lacks(&["view_image"]);
    text_only.assert_registered_lacks(&["view_image"]);

    let guardian_image_capable = probe(|turn| {
        turn.session_source = SessionSource::SubAgent(SubAgentSource::Other(
            crate::guardian::GUARDIAN_REVIEWER_NAME.to_string(),
        ));
        turn.model_info.input_modalities = vec![InputModality::Text, InputModality::Image];
    })
    .await;
    guardian_image_capable.assert_visible_contains(&["view_image"]);
    guardian_image_capable.assert_registered_contains(&["view_image"]);

    let guardian_text_only = probe(|turn| {
        turn.session_source = SessionSource::SubAgent(SubAgentSource::Other(
            crate::guardian::GUARDIAN_REVIEWER_NAME.to_string(),
        ));
        turn.model_info.input_modalities = vec![InputModality::Text];
    })
    .await;
    guardian_text_only.assert_visible_contains(&["exec_command", "write_stdin"]);
    guardian_text_only.assert_registered_contains(&["exec_command", "write_stdin"]);
    guardian_text_only.assert_visible_lacks(&["view_image"]);
    guardian_text_only.assert_registered_lacks(&["view_image"]);
}

#[tokio::test]
async fn environment_tools_follow_the_step_context() {
    let (_session, mut turn) = make_session_and_context().await;
    set_feature(&mut turn, Feature::UnifiedExec, /*enabled*/ true);
    turn.model_info.apply_patch_tool_type = Some(ApplyPatchToolType::Freeform);

    let environments = turn.environments.clone();
    turn.environments.turn_environments.clear();
    let turn = Arc::new(turn);
    let step_context = Arc::new(StepContext::new(
        Arc::clone(&turn),
        environments,
        Vec::new(),
        crate::session::McpRuntimeSnapshot::new_uninitialized_for_test(&turn.config),
        /*loaded_agents_md*/ None,
    ));

    let tool_search_cache = ToolSearchHandlerCache::default();
    let plan = ToolPlanProbe::from_router(
        ToolRouter::from_context(
            step_context.as_ref(),
            ToolRouterParams {
                mcp_tools: None,
                deferred_mcp_tools: None,
                tool_suggest_candidates: None,
                extension_tool_executors: Vec::new(),
                dynamic_tools: &[],
                exposure_identity: Default::default(),
            },
            &tool_search_cache,
        ),
        &tool_search_cache,
    );

    plan.assert_visible_contains(&["exec_command", "apply_patch", "view_image"]);
}

#[tokio::test]
async fn host_context_gates_agent_job_tools() {
    let normal_agent_job = probe(|turn| {
        set_feature(turn, Feature::SpawnCsv, /*enabled*/ true);
    })
    .await;
    normal_agent_job.assert_visible_contains(&["spawn_agents_on_csv"]);
    normal_agent_job.assert_visible_lacks(&["report_agent_job_result"]);
    assert_eq!(
        normal_agent_job.authorization_class("spawn_agents_on_csv"),
        TypedToolClass::RootTaskControl
    );

    let worker_agent_job = probe(|turn| {
        set_feature(turn, Feature::SpawnCsv, /*enabled*/ true);
        turn.session_source =
            SessionSource::SubAgent(SubAgentSource::Other("agent_job:42".to_string()));
    })
    .await;
    worker_agent_job.assert_visible_contains(&["spawn_agents_on_csv", "report_agent_job_result"]);
    assert_eq!(
        worker_agent_job.authorization_class("report_agent_job_result"),
        TypedToolClass::OwnTask
    );

    let remote_agent_job = probe(|turn| {
        set_feature(turn, Feature::SpawnCsv, /*enabled*/ true);
        set_foreign_primary_environment(turn);
    })
    .await;
    remote_agent_job.assert_visible_lacks(&["spawn_agents_on_csv", "report_agent_job_result"]);
    remote_agent_job.assert_registered_lacks(&["spawn_agents_on_csv", "report_agent_job_result"]);
}

#[tokio::test]
async fn foreign_primary_hides_spawn_but_keeps_existing_agent_lifecycle_tools() {
    let v1 = probe(|turn| {
        set_foreign_primary_environment(turn);
        set_feature(turn, Feature::Collab, /*enabled*/ true);
        set_feature(turn, Feature::MultiAgentV2, /*enabled*/ false);
    })
    .await;
    assert!(
        !v1.namespace_function_names(MULTI_AGENT_V1_NAMESPACE)
            .contains(&"spawn_agent".to_string())
    );
    assert!(
        v1.namespace_function_names(MULTI_AGENT_V1_NAMESPACE)
            .contains(&"wait_agent".to_string())
    );

    let v2 = probe(|turn| {
        set_foreign_primary_environment(turn);
        set_feature(turn, Feature::MultiAgentV2, /*enabled*/ true);
    })
    .await;
    assert!(
        !v2.namespace_function_names(MULTI_AGENT_V2_NAMESPACE)
            .contains(&"spawn_agent".to_string())
    );
    assert!(
        v2.namespace_function_names(MULTI_AGENT_V2_NAMESPACE)
            .contains(&"wait_agent".to_string())
    );
}

#[tokio::test]
async fn sleep_tool_follows_current_time_config() {
    let disabled = probe(|turn| {
        set_feature(turn, Feature::CurrentTimeReminder, /*enabled*/ true);
    })
    .await;
    assert_eq!(disabled.namespace_function_names("clock"), ["curr_time"]);
    assert_eq!(
        disabled.authorization_class(&ToolName::namespaced("clock", "curr_time").to_string()),
        TypedToolClass::ReadSearch
    );

    let enabled = probe(|turn| {
        set_feature(turn, Feature::CurrentTimeReminder, /*enabled*/ true);
        let mut config = (*turn.config).clone();
        config.current_time_reminder = Some(CurrentTimeReminderConfig {
            sleep_tool: true,
            ..CurrentTimeReminderConfig::default()
        });
        turn.config = Arc::new(config);
    })
    .await;
    assert_eq!(
        enabled.namespace_function_names("clock"),
        ["curr_time", "sleep"]
    );
}

#[tokio::test]
async fn mcp_and_tool_search_follow_direct_and_deferred_tool_exposure() {
    let direct_mcp = probe_with(
        |_| {},
        ToolPlanInputs {
            mcp_tools: Some(vec![mcp_tool("direct", "mcp__direct", "lookup")]),
            ..ToolPlanInputs::default()
        },
    )
    .await;
    direct_mcp.assert_visible_contains(&[
        "list_mcp_resources",
        "list_mcp_resource_templates",
        "read_mcp_resource",
    ]);
    assert_eq!(
        direct_mcp.namespace_function_names("mcp__direct"),
        &["lookup".to_string()]
    );

    let searchable_mcp = ToolPlanInputs {
        deferred_mcp_tools: Some(vec![mcp_tool("searchable", "mcp__searchable", "lookup")]),
        ..ToolPlanInputs::default()
    };

    let missing_model_capability = probe_with(
        |turn| {
            turn.model_info.supports_search_tool = false;
        },
        ToolPlanInputs {
            deferred_mcp_tools: searchable_mcp.deferred_mcp_tools.clone(),
            ..ToolPlanInputs::default()
        },
    )
    .await;
    missing_model_capability.assert_visible_lacks(&["tool_search"]);

    let missing_deferred_tools = probe(|turn| {
        set_feature(turn, Feature::Collab, /*enabled*/ false);
        turn.model_info.supports_search_tool = true;
    })
    .await;
    missing_deferred_tools.assert_visible_lacks(&["tool_search"]);
    missing_deferred_tools.assert_visible_lacks(&[
        "list_mcp_resources",
        "list_mcp_resource_templates",
        "read_mcp_resource",
    ]);

    let bedrock_namespace_capability = probe_with(
        |turn| {
            turn.model_info.supports_search_tool = true;
            use_bedrock_provider(turn);
        },
        ToolPlanInputs {
            deferred_mcp_tools: searchable_mcp.deferred_mcp_tools.clone(),
            ..ToolPlanInputs::default()
        },
    )
    .await;
    bedrock_namespace_capability.assert_visible_contains(&["tool_search"]);

    let enabled = probe_with(
        |turn| {
            turn.model_info.supports_search_tool = true;
        },
        searchable_mcp,
    )
    .await;
    enabled.assert_visible_contains(&["tool_search"]);
    enabled.assert_registered_contains(&[
        "tool_search",
        &ToolName::namespaced("mcp__searchable", "lookup").to_string(),
    ]);
}

#[tokio::test]
async fn mcp_resource_tools_follow_the_aggregate_ready_server_capability() {
    for mcp_resources_available in [false, true] {
        let identity = ToolExposureIdentity {
            mcp_resources_available,
            tool_search_available: true,
            ..ToolExposureIdentity::default()
        };
        let plan = probe_with(
            |turn| {
                turn.model_info.supports_search_tool = true;
            },
            ToolPlanInputs {
                mcp_tools: Some(vec![mcp_tool("direct", "mcp__direct", "lookup")]),
                exposure_identity: identity,
                ..ToolPlanInputs::default()
            },
        )
        .await;
        let resource_tools = [
            "list_mcp_resources",
            "list_mcp_resource_templates",
            "read_mcp_resource",
        ];
        if mcp_resources_available {
            plan.assert_visible_lacks(&resource_tools);
            plan.assert_registered_contains(&resource_tools);
            for tool in resource_tools {
                assert_eq!(plan.exposure(tool), ToolExposure::Deferred);
            }
        } else {
            plan.assert_visible_lacks(&resource_tools);
            plan.assert_registered_lacks(&resource_tools);
        }
    }
}

#[tokio::test]
async fn mcp_resource_tools_remain_direct_without_tool_search() {
    let plan = probe_with(
        |_| {},
        ToolPlanInputs {
            mcp_tools: Some(vec![mcp_tool("direct", "mcp__direct", "lookup")]),
            exposure_identity: ToolExposureIdentity {
                mcp_resources_available: true,
                tool_search_available: false,
                ..ToolExposureIdentity::default()
            },
            ..ToolPlanInputs::default()
        },
    )
    .await;
    plan.assert_visible_contains(&[
        "list_mcp_resources",
        "list_mcp_resource_templates",
        "read_mcp_resource",
    ]);
}

#[tokio::test]
async fn deferred_extension_tools_are_discoverable_with_tool_search() {
    let plan = probe_with(
        |turn| {
            turn.model_info.supports_search_tool = true;
        },
        ToolPlanInputs {
            extension_tool_executors: vec![Arc::new(DeferredExtensionTool {
                description: "Echoes arguments through an extension tool.",
            })],
            ..ToolPlanInputs::default()
        },
    )
    .await;

    plan.assert_visible_contains(&["tool_search"]);
    plan.assert_visible_lacks(&["extension_echo"]);
    plan.assert_registered_contains(&["extension_echo"]);
    assert_eq!(plan.exposure("extension_echo"), ToolExposure::Deferred);
}

#[tokio::test]
async fn deferred_extension_without_search_metadata_is_promoted_with_a_warning() {
    let plan = probe_with(
        |turn| turn.model_info.supports_search_tool = true,
        ToolPlanInputs {
            extension_tool_executors: vec![Arc::new(DeferredExtensionWithoutSearchInfo)],
            ..ToolPlanInputs::default()
        },
    )
    .await;

    plan.assert_visible_contains(&["undiscoverable_extension"]);
    assert_eq!(
        plan.exposure("undiscoverable_extension"),
        ToolExposure::Direct
    );
    assert!(
        plan.warnings
            .iter()
            .any(|warning| warning.contains("no search metadata"))
    );
}

#[tokio::test]
async fn malformed_extension_specs_are_skipped_without_panicking() {
    let plan = probe_with(
        |_| {},
        ToolPlanInputs {
            extension_tool_executors: vec![Arc::new(MalformedExtensionTool {
                namespace: "broken",
                tool_name: "run",
            })],
            ..ToolPlanInputs::default()
        },
    )
    .await;

    plan.assert_registered_lacks(&["broken.run"]);
    assert!(
        plan.warnings
            .iter()
            .any(|warning| warning.contains("exactly one callable tool"))
    );
}

#[tokio::test]
async fn winning_registered_runtime_owns_its_external_mutation_intent() {
    let mut read_only = mcp_tool("reader", "mcp__shared", "lookup");
    read_only.tool.annotations = Some(rmcp::model::ToolAnnotations::new().read_only(true));
    let plan = probe_with(
        |_| {},
        ToolPlanInputs {
            mcp_tools: Some(vec![read_only]),
            extension_tool_executors: vec![Arc::new(TestNamespaceExtensionTool {
                namespace: "mcp__shared",
                tool_name: "lookup",
            })],
            ..ToolPlanInputs::default()
        },
    )
    .await;
    assert_eq!(
        plan.external_mutation_intents.get("mcp__shared.lookup"),
        Some(&ExternalMutationIntent::ProvenReadOnly)
    );

    let mut mutating = mcp_tool("writer", "mcp__shared", "lookup");
    mutating.tool.annotations = Some(rmcp::model::ToolAnnotations::new().read_only(false));
    let plan = probe_with(
        |_| {},
        ToolPlanInputs {
            mcp_tools: Some(vec![mutating]),
            extension_tool_executors: vec![Arc::new(TestNamespaceExtensionTool {
                namespace: "mcp__shared",
                tool_name: "lookup",
            })],
            ..ToolPlanInputs::default()
        },
    )
    .await;
    assert_eq!(
        plan.external_mutation_intents.get("mcp__shared.lookup"),
        Some(&ExternalMutationIntent::MayMutate)
    );
}

#[tokio::test]
async fn duplicate_extension_tool_names_surface_an_actionable_warning() {
    let plan = probe_with(
        |turn| turn.model_info.supports_search_tool = true,
        ToolPlanInputs {
            extension_tool_executors: vec![
                Arc::new(DeferredExtensionTool {
                    description: "winning deferred schema marker",
                }),
                Arc::new(DeferredExtensionTool {
                    description: "losing deferred schema marker",
                }),
            ],
            ..ToolPlanInputs::default()
        },
    )
    .await;

    assert_eq!(
        plan.registered_names
            .iter()
            .filter(|name| name.as_str() == "extension_echo")
            .count(),
        1
    );
    let [warning] = plan.warnings.as_slice() else {
        panic!("duplicate extension tool should produce exactly one warning");
    };
    assert!(warning.contains("extension_echo"));
    assert!(warning.contains("Rename or disable"));
    assert!(
        plan.tool_search_texts
            .iter()
            .any(|text| text.contains("winning deferred schema marker"))
    );
    assert!(
        plan.tool_search_texts
            .iter()
            .all(|text| !text.contains("losing deferred schema marker"))
    );
}

#[tokio::test]
async fn dynamic_tool_name_collisions_use_the_shared_first_registration_policy() {
    let plan = probe_with(
        |_| {},
        ToolPlanInputs {
            dynamic_tools: vec![
                dynamic_tool(None, "update_plan", /*defer_loading*/ false),
                dynamic_tool(None, "duplicate_dynamic", /*defer_loading*/ false),
                dynamic_tool(None, "duplicate_dynamic", /*defer_loading*/ false),
            ],
            ..ToolPlanInputs::default()
        },
    )
    .await;

    for name in ["update_plan", "duplicate_dynamic"] {
        assert_eq!(
            plan.registered_names
                .iter()
                .filter(|registered| registered.as_str() == name)
                .count(),
            1,
            "the first registration should be the only reachable `{name}` tool"
        );
    }
    assert_eq!(plan.warnings.len(), 2);
    for name in ["update_plan", "duplicate_dynamic"] {
        assert!(
            plan.warnings
                .iter()
                .any(|warning| { warning.contains(name) && warning.contains("Rename or disable") }),
            "expected an actionable collision warning for `{name}`"
        );
    }
}

#[tokio::test]
async fn deferred_web_run_stays_reachable_with_and_without_tool_search() {
    let web_run = || {
        Arc::new(DeferredNamespaceExtensionTool {
            namespace: "web",
            tool_name: "run",
        }) as Arc<dyn ToolExecutor<ExtensionToolCall>>
    };

    let without_tool_search = probe_with(
        |turn| {
            set_feature(turn, Feature::StandaloneWebSearch, /*enabled*/ true);
            set_web_search_mode(turn, WebSearchMode::Live);
            turn.model_info.supports_search_tool = false;
        },
        ToolPlanInputs {
            extension_tool_executors: vec![web_run()],
            ..ToolPlanInputs::default()
        },
    )
    .await;
    without_tool_search.assert_visible_contains(&["web"]);
    without_tool_search.assert_visible_lacks(&["tool_search", "web_search"]);
    assert_eq!(without_tool_search.namespace_function_names("web"), ["run"]);
    assert_eq!(
        without_tool_search.exposure(&ToolName::namespaced("web", "run").to_string()),
        ToolExposure::Direct
    );

    let with_tool_search = probe_with(
        |turn| {
            set_feature(turn, Feature::StandaloneWebSearch, /*enabled*/ true);
            set_web_search_mode(turn, WebSearchMode::Live);
            turn.model_info.supports_search_tool = true;
        },
        ToolPlanInputs {
            extension_tool_executors: vec![web_run()],
            ..ToolPlanInputs::default()
        },
    )
    .await;
    with_tool_search.assert_visible_contains(&["tool_search"]);
    with_tool_search.assert_visible_contains(&["web_search"]);
    with_tool_search.assert_visible_lacks(&["web"]);
    with_tool_search.assert_registered_contains(&[&ToolName::namespaced("web", "run").to_string()]);
    assert_eq!(
        with_tool_search.exposure(&ToolName::namespaced("web", "run").to_string()),
        ToolExposure::Deferred
    );
}

#[tokio::test]
async fn tool_search_cache_rebuilds_when_deferred_sources_change() {
    let cache = ToolSearchHandlerCache::default();

    let (_session, mut first_turn) = make_session_and_context().await;
    first_turn.model_info.supports_search_tool = true;
    let first_turn = Arc::new(first_turn);
    let first_step_context = StepContext::for_test(Arc::clone(&first_turn));
    let first_router = ToolRouter::from_context(
        first_step_context.as_ref(),
        ToolRouterParams {
            mcp_tools: None,
            deferred_mcp_tools: Some(vec![mcp_tool("first", "mcp__first", "lookup")]),
            tool_suggest_candidates: None,
            extension_tool_executors: Vec::new(),
            dynamic_tools: &[],
            exposure_identity: Default::default(),
        },
        &cache,
    );
    let first_plan = ToolPlanProbe::from_router(first_router, &cache);

    let (_session, mut second_turn) = make_session_and_context().await;
    second_turn.model_info.supports_search_tool = true;
    let second_turn = Arc::new(second_turn);
    let second_step_context = StepContext::for_test(Arc::clone(&second_turn));
    let second_router = ToolRouter::from_context(
        second_step_context.as_ref(),
        ToolRouterParams {
            mcp_tools: None,
            deferred_mcp_tools: Some(vec![mcp_tool("second", "mcp__second", "lookup")]),
            tool_suggest_candidates: None,
            extension_tool_executors: Vec::new(),
            dynamic_tools: &[],
            exposure_identity: Default::default(),
        },
        &cache,
    );
    let second_plan = ToolPlanProbe::from_router(second_router, &cache);
    let third_router = ToolRouter::from_context(
        first_step_context.as_ref(),
        ToolRouterParams {
            mcp_tools: None,
            deferred_mcp_tools: Some(vec![mcp_tool("first", "mcp__first", "lookup")]),
            tool_suggest_candidates: None,
            extension_tool_executors: Vec::new(),
            dynamic_tools: &[],
            exposure_identity: Default::default(),
        },
        &cache,
    );
    let third_plan = ToolPlanProbe::from_router(third_router, &cache);

    let ToolSpec::ToolSearch {
        description: first_description,
        ..
    } = first_plan.visible_spec("tool_search")
    else {
        panic!("expected first tool_search spec");
    };
    assert!(first_description.contains("- first: Tools from first."));
    assert!(!first_description.contains("- second: Tools from second."));

    let ToolSpec::ToolSearch {
        description: second_description,
        ..
    } = second_plan.visible_spec("tool_search")
    else {
        panic!("expected second tool_search spec");
    };
    assert!(second_description.contains("- second: Tools from second."));
    assert!(!second_description.contains("- first: Tools from first."));

    let ToolSpec::ToolSearch {
        description: third_description,
        ..
    } = third_plan.visible_spec("tool_search")
    else {
        panic!("expected third tool_search spec");
    };
    assert!(third_description.contains("- first: Tools from first."));
    assert!(!third_description.contains("- second: Tools from second."));
}

#[tokio::test]
async fn invalid_mcp_tools_are_not_registered() {
    let plan = probe_with(
        |_| {},
        ToolPlanInputs {
            mcp_tools: Some(vec![invalid_mcp_tool("invalid", "mcp__invalid", "lookup")]),
            ..ToolPlanInputs::default()
        },
    )
    .await;

    plan.assert_visible_lacks(&["mcp__invalid"]);
    plan.assert_registered_lacks(&[&ToolName::namespaced("mcp__invalid", "lookup").to_string()]);
}

#[tokio::test]
async fn request_plugin_install_requires_all_discovery_features() {
    for disabled_feature in [Feature::ToolSuggest, Feature::Apps, Feature::Plugins] {
        let plan = probe_with(
            |turn| {
                set_features(
                    turn,
                    &[Feature::ToolSuggest, Feature::Apps, Feature::Plugins],
                );
                set_feature(turn, disabled_feature, /*enabled*/ false);
            },
            ToolPlanInputs {
                tool_suggest_candidates: Some(plugin_candidates(ToolSuggestPresentation::ListTool)),
                ..ToolPlanInputs::default()
            },
        )
        .await;
        plan.assert_visible_lacks(&[
            "list_available_plugins_to_install",
            "request_plugin_install",
        ]);
    }

    for tool_suggest_candidates in [
        None,
        Some(ToolSuggestCandidates {
            tools: Vec::new(),
            presentation: ToolSuggestPresentation::RecommendationContext,
        }),
    ] {
        let plan = probe_with(
            |turn| {
                set_features(
                    turn,
                    &[Feature::ToolSuggest, Feature::Apps, Feature::Plugins],
                );
            },
            ToolPlanInputs {
                tool_suggest_candidates,
                ..ToolPlanInputs::default()
            },
        )
        .await;
        plan.assert_visible_lacks(&[
            "list_available_plugins_to_install",
            "request_plugin_install",
        ]);
    }

    let enabled = probe_with(
        |turn| {
            set_features(
                turn,
                &[Feature::ToolSuggest, Feature::Apps, Feature::Plugins],
            );
        },
        ToolPlanInputs {
            tool_suggest_candidates: Some(plugin_candidates(ToolSuggestPresentation::ListTool)),
            ..ToolPlanInputs::default()
        },
    )
    .await;
    enabled.assert_visible_contains(&[
        "list_available_plugins_to_install",
        "request_plugin_install",
    ]);
}

#[tokio::test]
async fn request_plugin_install_stays_visible_without_tool_search() {
    let plan = probe_with(
        |turn| {
            turn.model_info.supports_search_tool = false;
            set_features(
                turn,
                &[Feature::ToolSuggest, Feature::Apps, Feature::Plugins],
            );
        },
        ToolPlanInputs {
            tool_suggest_candidates: Some(plugin_candidates(ToolSuggestPresentation::ListTool)),
            ..ToolPlanInputs::default()
        },
    )
    .await;

    plan.assert_visible_contains(&[
        "list_available_plugins_to_install",
        "request_plugin_install",
    ]);
    plan.assert_visible_lacks(&["tool_search"]);
}

#[tokio::test]
async fn request_plugin_install_description_requires_exhausting_tool_search() {
    let plan = probe_with(
        |turn| {
            set_features(
                turn,
                &[Feature::ToolSuggest, Feature::Apps, Feature::Plugins],
            );
        },
        ToolPlanInputs {
            tool_suggest_candidates: Some(plugin_candidates(
                ToolSuggestPresentation::RecommendationContext,
            )),
            ..ToolPlanInputs::default()
        },
    )
    .await;

    let request_spec = plan.visible_spec("request_plugin_install");
    let ToolSpec::Function(ResponsesApiTool {
        description: request_description,
        ..
    }) = request_spec
    else {
        panic!("expected request_plugin_install function spec");
    };
    assert!(request_description.contains("listed in `<recommended_plugins>`"));
    assert!(request_description.contains("explicitly asks to use a specific plugin"));
    assert!(request_description.contains("Tool search has already been exhausted"));
    assert!(!request_description.contains("`tool_search`"));
    assert!(request_description.contains("DO NOT call this tool in parallel with other tools"));
    assert!(!request_description.contains("list_available_plugins_to_install"));
    assert!(!request_description.contains("github"));
    assert!(has_parameter(request_spec, "plugin_id"));
    assert!(has_parameter(request_spec, "suggest_reason"));
    assert!(!has_parameter(request_spec, "tool_id"));
    assert!(!has_parameter(request_spec, "tool_type"));
    assert!(!has_parameter(request_spec, "action_type"));
    plan.assert_visible_lacks(&["list_available_plugins_to_install"]);
    plan.assert_registered_lacks(&["list_available_plugins_to_install"]);
}

#[tokio::test]
async fn code_mode_only_exposes_code_executor_and_hides_nested_tools() {
    let input = ToolPlanInputs {
        dynamic_tools: vec![dynamic_tool(
            Some("codex_app"),
            "lookup",
            /*defer_loading*/ false,
        )],
        ..ToolPlanInputs::default()
    };
    let plain = probe_with(
        |turn| {
            set_feature(turn, Feature::CodeMode, /*enabled*/ false);
            set_feature(turn, Feature::CodeModeOnly, /*enabled*/ false);
        },
        input,
    )
    .await;
    assert_eq!(
        plain.namespace_function_names("codex_app"),
        &["lookup".to_string()]
    );
    plain.assert_visible_lacks(&[
        codex_code_mode::PUBLIC_TOOL_NAME,
        codex_code_mode::WAIT_TOOL_NAME,
    ]);

    let code_mode_only = probe_with(
        |turn| {
            set_features(turn, &[Feature::CodeMode, Feature::CodeModeOnly]);
        },
        ToolPlanInputs {
            dynamic_tools: vec![dynamic_tool(
                Some("codex_app"),
                "lookup",
                /*defer_loading*/ false,
            )],
            ..ToolPlanInputs::default()
        },
    )
    .await;
    code_mode_only.assert_visible_contains(&[
        codex_code_mode::PUBLIC_TOOL_NAME,
        codex_code_mode::WAIT_TOOL_NAME,
    ]);
    assert_eq!(
        code_mode_only.namespace_function_names("codex_app"),
        Vec::<String>::new().as_slice()
    );
}

#[tokio::test]
async fn code_mode_only_exposes_configured_dynamic_namespace_directly() {
    let plan = probe_with(
        |turn| {
            set_features(turn, &[Feature::CodeMode, Feature::CodeModeOnly]);
            turn.model_info.supports_search_tool = true;
            update_config(turn, |config| {
                config.code_mode.direct_only_tool_namespaces = vec!["direct_only".to_string()];
            });
        },
        ToolPlanInputs {
            dynamic_tools: vec![dynamic_tool(
                Some("direct_only"),
                "lookup",
                /*defer_loading*/ true,
            )],
            ..ToolPlanInputs::default()
        },
    )
    .await;

    plan.assert_visible_contains(&[
        codex_code_mode::PUBLIC_TOOL_NAME,
        codex_code_mode::WAIT_TOOL_NAME,
        "direct_only",
    ]);
    plan.assert_visible_lacks(&["tool_search"]);
    assert_eq!(
        plan.exposure(&ToolName::namespaced("direct_only", "lookup").to_string()),
        ToolExposure::DirectModelOnly
    );
    let ToolSpec::Namespace(namespace) = plan.visible_spec("direct_only") else {
        panic!("expected direct-only namespace spec");
    };
    let ResponsesApiNamespaceTool::Function(tool) = &namespace.tools[0];
    assert_eq!(tool.defer_loading, None);
    let ToolSpec::Freeform(exec) = plan.visible_spec(codex_code_mode::PUBLIC_TOOL_NAME) else {
        panic!("expected code mode exec tool");
    };
    assert!(!exec.description.contains("direct_only_lookup(args:"));
}

#[tokio::test]
async fn deferred_tools_enable_nested_tool_guidance_without_prompt_inventory() {
    let plan = probe_with(
        |turn| {
            set_feature(turn, Feature::CodeMode, /*enabled*/ true);
            set_feature(turn, Feature::Collab, /*enabled*/ false);
            turn.model_info.supports_search_tool = true;
        },
        ToolPlanInputs {
            dynamic_tools: vec![dynamic_tool(
                Some("deferred"),
                "lookup",
                /*defer_loading*/ true,
            )],
            ..ToolPlanInputs::default()
        },
    )
    .await;

    let ToolSpec::Freeform(exec) = plan.visible_spec(codex_code_mode::PUBLIC_TOOL_NAME) else {
        panic!("expected code mode exec tool");
    };
    assert!(
        exec.description
            .contains("Some deferred nested tools may be omitted")
    );
    assert!(!exec.description.contains("deferred_lookup(args:"));
}

#[tokio::test]
async fn excluded_deferred_namespaces_do_not_enable_nested_tool_guidance() {
    let plan = probe_with(
        |turn| {
            set_features(turn, &[Feature::CodeMode, Feature::CodeModeOnly]);
            set_feature(turn, Feature::Collab, /*enabled*/ false);
            turn.model_info.supports_search_tool = true;
            update_config(turn, |config| {
                config.code_mode.excluded_tool_namespaces = vec!["excluded".to_string()];
            });
        },
        ToolPlanInputs {
            dynamic_tools: vec![dynamic_tool(
                Some("excluded"),
                "lookup",
                /*defer_loading*/ true,
            )],
            ..ToolPlanInputs::default()
        },
    )
    .await;

    let ToolSpec::Freeform(exec) = plan.visible_spec(codex_code_mode::PUBLIC_TOOL_NAME) else {
        panic!("expected code mode exec tool");
    };
    assert!(
        !exec
            .description
            .contains("Some deferred nested tools may be omitted")
    );
    plan.assert_registered_contains(&[
        &ToolName::namespaced("excluded", "lookup").to_string(),
        "tool_search",
    ]);
}

#[tokio::test]
async fn multi_agent_feature_selects_one_agent_tool_family() {
    let v1 = probe(|turn| {
        set_feature(turn, Feature::Collab, /*enabled*/ true);
        set_feature(turn, Feature::MultiAgentV2, /*enabled*/ false);
    })
    .await;
    v1.assert_visible_contains(&[MULTI_AGENT_V1_NAMESPACE]);
    v1.assert_visible_lacks(&[
        "spawn_agent",
        "send_input",
        "resume_agent",
        "wait_agent",
        "close_agent",
        "interrupt_agent",
        "send_message",
        "followup_task",
        "assign_task",
        "list_agents",
    ]);
    assert_eq!(
        v1.namespace_function_names(MULTI_AGENT_V1_NAMESPACE),
        &[
            "close_agent".to_string(),
            "resume_agent".to_string(),
            "send_input".to_string(),
            "spawn_agent".to_string(),
            "wait_agent".to_string(),
        ]
    );
    let ToolSpec::Namespace(namespace) = v1.visible_spec(MULTI_AGENT_V1_NAMESPACE) else {
        panic!("expected v1 multi-agent namespace");
    };
    let Some(ResponsesApiNamespaceTool::Function(spawn_agent)) =
        namespace.tools.iter().find(|tool| {
            matches!(
                tool,
                ResponsesApiNamespaceTool::Function(tool) if tool.name == "spawn_agent"
            )
        })
    else {
        panic!("expected v1 spawn_agent function");
    };
    let properties = spawn_agent
        .parameters
        .properties
        .as_ref()
        .expect("spawn_agent should use object params");
    for property in ["agent_type", "model", "reasoning_effort", "service_tier"] {
        assert!(
            properties.contains_key(property),
            "expected v1 spawn_agent to expose `{property}`"
        );
    }

    let v2 = probe(|turn| {
        set_feature(turn, Feature::MultiAgentV2, /*enabled*/ true);
        update_config(turn, |config| {
            config.multi_agent_v2.max_concurrent_threads_per_session = 17;
        });
    })
    .await;
    v2.assert_visible_contains(&[MULTI_AGENT_V2_NAMESPACE]);
    v2.assert_visible_lacks(&[
        "spawn_agent",
        "send_message",
        "followup_task",
        "wait_agent",
        "interrupt_agent",
        "list_agents",
        "get_agent_task",
        "submit_agent_receipt",
        "amend_agent_task",
        "waive_agent_gate",
        "abandon_agent_task",
        "send_input",
        "resume_agent",
        "assign_task",
        "close_agent",
    ]);
    for tool_name in [
        "spawn_agent",
        "send_message",
        "followup_task",
        "wait_agent",
        "interrupt_agent",
        "list_agents",
        "get_agent_task",
        "amend_agent_task",
        "waive_agent_gate",
        "abandon_agent_task",
    ] {
        assert!(
            v2.namespace_function_names(MULTI_AGENT_V2_NAMESPACE)
                .iter()
                .any(|name| name == tool_name),
            "expected {tool_name} in {MULTI_AGENT_V2_NAMESPACE} namespace"
        );
    }
    assert!(
        !v2.namespace_function_names(MULTI_AGENT_V2_NAMESPACE)
            .iter()
            .any(|name| name == "submit_agent_receipt"),
        "expected submit_agent_receipt to be hidden from the root agent"
    );
    let ToolSpec::Namespace(namespace) = v2.visible_spec(MULTI_AGENT_V2_NAMESPACE) else {
        panic!("expected {MULTI_AGENT_V2_NAMESPACE} namespace");
    };
    let Some(ResponsesApiNamespaceTool::Function(spawn_agent)) =
        namespace.tools.iter().find(|tool| {
            matches!(
                tool,
                ResponsesApiNamespaceTool::Function(tool) if tool.name == "spawn_agent"
            )
        })
    else {
        panic!("expected spawn_agent in {MULTI_AGENT_V2_NAMESPACE} namespace");
    };
    let spawn_agent_description = spawn_agent.description.as_str();
    assert!(!spawn_agent_description.contains("max_concurrent_threads_per_session"));
    assert!(spawn_agent_description.contains("lineage-preserving TaskCapsule fork"));
    assert!(spawn_agent_description.contains("no parent conversation"));

    let v2_subagent = probe(|turn| {
        set_feature(turn, Feature::MultiAgentV2, /*enabled*/ true);
        turn.session_source =
            SessionSource::SubAgent(SubAgentSource::Other("typed-task-test".to_string()));
    })
    .await;
    for tool_name in ["get_agent_task", "submit_agent_receipt"] {
        assert!(
            v2_subagent
                .namespace_function_names(MULTI_AGENT_V2_NAMESPACE)
                .iter()
                .any(|name| name == tool_name),
            "expected {tool_name} in the subagent {MULTI_AGENT_V2_NAMESPACE} namespace"
        );
    }
    for tool_name in ["amend_agent_task", "waive_agent_gate", "abandon_agent_task"] {
        assert!(
            !v2_subagent
                .namespace_function_names(MULTI_AGENT_V2_NAMESPACE)
                .iter()
                .any(|name| name == tool_name),
            "expected {tool_name} to be hidden from subagents"
        );
    }

    let direct_model_only = probe(|turn| {
        set_features(
            turn,
            &[
                Feature::CodeMode,
                Feature::CodeModeOnly,
                Feature::MultiAgentV2,
            ],
        );
        update_config(turn, |config| {
            config.multi_agent_v2.non_code_mode_only = true;
        });
    })
    .await;
    direct_model_only.assert_visible_contains(&[MULTI_AGENT_V2_NAMESPACE]);
    direct_model_only.assert_visible_lacks(&["spawn_agent", "send_message", "wait_agent"]);
    assert_eq!(
        direct_model_only
            .exposure(&ToolName::namespaced(MULTI_AGENT_V2_NAMESPACE, "spawn_agent").to_string()),
        ToolExposure::DirectModelOnly
    );
}

#[tokio::test]
async fn collaboration_namespace_selection_tracks_the_active_agent_version_and_surface() {
    let (_session, mut turn) = make_session_and_context().await;
    set_feature(&mut turn, Feature::Collab, /*enabled*/ true);
    set_feature(&mut turn, Feature::MultiAgentV2, /*enabled*/ false);

    let namespace = active_collaboration_namespace(&turn, AgentSurfaceStage::TypedAdministration);
    assert_eq!(namespace, Some(MULTI_AGENT_V1_NAMESPACE));

    set_feature(&mut turn, Feature::MultiAgentV2, /*enabled*/ true);
    let namespace = active_collaboration_namespace(&turn, AgentSurfaceStage::TypedAdministration);
    assert_eq!(namespace, Some(MULTI_AGENT_V2_NAMESPACE));
    assert_eq!(
        active_collaboration_namespace(&turn, AgentSurfaceStage::Prohibited),
        None
    );
}

#[tokio::test]
async fn multi_agent_v2_surface_changes_only_at_the_four_coarse_stages() {
    let stages = [
        (AgentSurfaceStage::Prohibited, Vec::<&str>::new()),
        (AgentSurfaceStage::SpawnOnly, vec!["spawn_agent"]),
        (
            AgentSurfaceStage::Lifecycle,
            vec![
                "spawn_agent",
                "send_message",
                "followup_task",
                "wait_agent",
                "interrupt_agent",
                "list_agents",
            ],
        ),
        (
            AgentSurfaceStage::TypedAdministration,
            vec![
                "spawn_agent",
                "send_message",
                "followup_task",
                "wait_agent",
                "interrupt_agent",
                "list_agents",
                "get_agent_task",
                "set_agent_gate",
                "amend_agent_task",
                "waive_agent_gate",
                "abandon_agent_task",
            ],
        ),
    ];

    for (stage, expected) in stages {
        let identity = ToolExposureIdentity {
            agent_surface_stage: stage,
            ..ToolExposureIdentity::default()
        };
        let plan = probe_with(
            |turn| set_feature(turn, Feature::MultiAgentV2, /*enabled*/ true),
            ToolPlanInputs {
                exposure_identity: identity,
                ..ToolPlanInputs::default()
            },
        )
        .await;
        if stage == AgentSurfaceStage::Prohibited {
            plan.assert_visible_lacks(&[MULTI_AGENT_V2_NAMESPACE]);
            for tool_name in [
                "spawn_agent",
                "send_message",
                "followup_task",
                "wait_agent",
                "interrupt_agent",
                "list_agents",
                "get_agent_task",
                "set_agent_gate",
            ] {
                plan.assert_registered_lacks(&[&ToolName::namespaced(
                    MULTI_AGENT_V2_NAMESPACE,
                    tool_name,
                )
                .to_string()]);
            }
            continue;
        }

        plan.assert_visible_contains(&[MULTI_AGENT_V2_NAMESPACE]);
        let actual = plan.namespace_function_names(MULTI_AGENT_V2_NAMESPACE);
        assert_eq!(
            actual.len(),
            expected.len(),
            "unexpected tools for {stage:?}"
        );
        for tool_name in expected {
            assert!(
                actual.iter().any(|actual| actual == tool_name),
                "expected `{tool_name}` for {stage:?}, got {actual:?}"
            );
        }
    }
}

#[tokio::test]
async fn multi_agent_v2_message_schemas_are_encrypted() {
    let plan = probe(|turn| {
        set_feature(turn, Feature::MultiAgentV2, /*enabled*/ true);
    })
    .await;
    let ToolSpec::Namespace(namespace) = plan.visible_spec(MULTI_AGENT_V2_NAMESPACE) else {
        panic!("expected {MULTI_AGENT_V2_NAMESPACE} namespace");
    };
    for tool_name in ["spawn_agent", "send_message", "followup_task"] {
        let Some(ResponsesApiNamespaceTool::Function(tool)) = namespace.tools.iter().find(|tool| {
            matches!(
                tool,
                ResponsesApiNamespaceTool::Function(tool) if tool.name == tool_name
            )
        }) else {
            panic!("expected {tool_name} in {MULTI_AGENT_V2_NAMESPACE} namespace");
        };
        let properties = tool
            .parameters
            .properties
            .as_ref()
            .expect("tool should use object params");
        assert_eq!(
            properties
                .get("message")
                .and_then(|schema| schema.encrypted),
            Some(true)
        );
    }
}

#[tokio::test]
async fn tool_mode_selector_overrides_feature_flags() {
    let direct = probe(|turn| {
        set_features(turn, &[Feature::CodeMode, Feature::CodeModeOnly]);
        turn.model_info.tool_mode = Some(ToolMode::Direct);
    })
    .await;
    direct.assert_visible_lacks(&[
        codex_code_mode::PUBLIC_TOOL_NAME,
        codex_code_mode::WAIT_TOOL_NAME,
    ]);
}

#[tokio::test]
async fn v1_multi_agent_tools_defer_when_tool_search_available() {
    let plan = probe(|turn| {
        turn.model_info.supports_search_tool = true;
        set_feature(turn, Feature::Collab, /*enabled*/ true);
        set_feature(turn, Feature::MultiAgentV2, /*enabled*/ false);
    })
    .await;

    plan.assert_visible_contains(&["tool_search"]);
    plan.assert_visible_lacks(&[
        "spawn_agent",
        "send_input",
        "resume_agent",
        "wait_agent",
        "close_agent",
        "interrupt_agent",
    ]);
    for tool_name in [
        "spawn_agent",
        "send_input",
        "resume_agent",
        "wait_agent",
        "close_agent",
    ] {
        let namespaced_tool_name = ToolName::namespaced(MULTI_AGENT_V1_NAMESPACE, tool_name);
        let namespaced_tool_name = namespaced_tool_name.to_string();
        assert!(
            plan.registered_names.contains(&namespaced_tool_name),
            "expected namespaced runtime for {tool_name}"
        );
        assert!(
            !plan
                .registered_names
                .contains(&ToolName::plain(tool_name).to_string()),
            "expected no plain runtime for deferred {tool_name}"
        );
        assert_eq!(plan.exposure(&namespaced_tool_name), ToolExposure::Deferred);
    }
    let ToolSpec::ToolSearch { description, .. } = plan.visible_spec("tool_search") else {
        panic!("expected visible tool_search spec");
    };
    assert!(description.contains("- Multi-agent tools: Spawn and manage sub-agents."));
}

#[tokio::test]
async fn token_efficiency_v2_multi_agent_tools_defer_when_tool_search_available() {
    let plan = probe(|turn| {
        turn.model_info.supports_search_tool = true;
        set_feature(turn, Feature::MultiAgentV2, /*enabled*/ true);
    })
    .await;

    plan.assert_visible_contains(&["tool_search"]);
    plan.assert_visible_lacks(&[MULTI_AGENT_V2_NAMESPACE, "spawn_agent", "send_message"]);
    for tool_name in ["spawn_agent", "send_message", "wait_agent", "list_agents"] {
        let name = ToolName::namespaced(MULTI_AGENT_V2_NAMESPACE, tool_name).to_string();
        assert!(plan.registered_names.contains(&name));
        assert_eq!(plan.exposure(&name), ToolExposure::Deferred);
    }
}

#[tokio::test]
async fn multi_agent_v2_can_use_configured_tool_namespace() {
    let namespaced = probe(|turn| {
        set_feature(turn, Feature::MultiAgentV2, /*enabled*/ true);
        update_config(turn, |config| {
            config.multi_agent_v2.tool_namespace = Some("agents".to_string());
        });
    })
    .await;

    namespaced.assert_visible_contains(&["agents"]);
    namespaced.assert_visible_lacks(&["assign_task"]);
    assert!(
        !namespaced
            .registered_names
            .contains(&ToolName::namespaced("agents", "assign_task").to_string()),
        "expected no namespaced runtime for assign_task"
    );
    assert!(
        !namespaced
            .namespace_function_names("agents")
            .iter()
            .any(|name| name == "assign_task"),
        "expected assign_task to be absent from agents namespace"
    );
    for tool_name in [
        "spawn_agent",
        "send_message",
        "followup_task",
        "wait_agent",
        "interrupt_agent",
        "list_agents",
        "get_agent_task",
        "amend_agent_task",
        "waive_agent_gate",
        "abandon_agent_task",
    ] {
        namespaced.assert_visible_lacks(&[tool_name]);
        assert!(
            namespaced
                .registered_names
                .contains(&ToolName::namespaced("agents", tool_name).to_string()),
            "expected namespaced runtime for {tool_name}"
        );
        assert!(
            !namespaced
                .registered_names
                .contains(&ToolName::plain(tool_name).to_string()),
            "expected no plain runtime for {tool_name}"
        );
        assert!(
            namespaced
                .namespace_function_names("agents")
                .iter()
                .any(|name| name == tool_name),
            "expected {tool_name} in agents namespace"
        );
    }
}

#[tokio::test]
async fn multi_agent_v2_namespace_is_supported_by_bedrock_provider() {
    let plan = probe(|turn| {
        set_feature(turn, Feature::MultiAgentV2, /*enabled*/ true);
        update_config(turn, |config| {
            config.multi_agent_v2.tool_namespace = Some("agents".to_string());
        });
        use_bedrock_provider(turn);
    })
    .await;

    plan.assert_visible_contains(&["agents"]);
    plan.assert_visible_lacks(&["spawn_agent", "send_message", "list_agents"]);
    assert!(
        !plan
            .registered_names
            .contains(&ToolName::plain("spawn_agent").to_string())
    );
    assert!(
        plan.registered_names
            .contains(&ToolName::namespaced("agents", "spawn_agent").to_string())
    );
}

#[tokio::test]
async fn code_mode_only_can_expose_namespaced_multi_agent_v2_as_normal_tools() {
    let plan = probe(|turn| {
        set_features(
            turn,
            &[
                Feature::CodeMode,
                Feature::CodeModeOnly,
                Feature::MultiAgentV2,
            ],
        );
        update_config(turn, |config| {
            config.multi_agent_v2.non_code_mode_only = true;
            config.multi_agent_v2.tool_namespace = Some("agents".to_string());
        });
    })
    .await;

    assert_eq!(
        plan.visible_names,
        vec![
            "exec",
            "wait",
            "request_user_input",
            "agents",
            // Hosted Responses tool.
            "web_search",
        ]
    );
    assert!(
        !plan
            .namespace_function_names("agents")
            .iter()
            .any(|name| name == "assign_task"),
        "expected assign_task to be absent from agents namespace"
    );
    for tool_name in [
        "spawn_agent",
        "send_message",
        "followup_task",
        "wait_agent",
        "interrupt_agent",
        "list_agents",
        "get_agent_task",
        "amend_agent_task",
        "waive_agent_gate",
        "abandon_agent_task",
    ] {
        assert!(
            plan.namespace_function_names("agents")
                .iter()
                .any(|name| name == tool_name),
            "expected {tool_name} in agents namespace"
        );
    }
}

#[tokio::test]
async fn hosted_web_search_and_standalone_image_generation_follow_runtime_gates() {
    let image_generation_tool = Arc::new(TestNamespaceExtensionTool {
        namespace: "image_gen",
        tool_name: "imagegen",
    });
    let image_generation = probe_with(
        |turn| {
            use_chatgpt_auth(turn);
            turn.model_info.input_modalities = vec![InputModality::Image];
        },
        ToolPlanInputs {
            extension_tool_executors: vec![image_generation_tool.clone()],
            ..Default::default()
        },
    )
    .await;
    image_generation.assert_visible_contains(&["image_gen"]);

    let extension_disabled = probe_with(
        |turn| {
            use_chatgpt_auth(turn);
            set_feature(turn, Feature::ImageGeneration, /*enabled*/ false);
            turn.model_info.input_modalities = vec![InputModality::Image];
        },
        ToolPlanInputs {
            extension_tool_executors: vec![image_generation_tool.clone()],
            ..Default::default()
        },
    )
    .await;
    extension_disabled.assert_visible_lacks(&["image_gen"]);

    let text_only_model = probe_with(
        |turn| {
            use_chatgpt_auth(turn);
            turn.model_info.input_modalities = vec![];
        },
        ToolPlanInputs {
            extension_tool_executors: vec![image_generation_tool.clone()],
            ..Default::default()
        },
    )
    .await;
    text_only_model.assert_visible_lacks(&["image_gen"]);

    let unsupported_provider = probe_with(
        |turn| {
            use_bedrock_provider(turn);
            turn.model_info.input_modalities = vec![InputModality::Image];
        },
        ToolPlanInputs {
            extension_tool_executors: vec![image_generation_tool],
            ..Default::default()
        },
    )
    .await;
    unsupported_provider.assert_visible_lacks(&["image_gen"]);

    let live_web_search = probe(|turn| {
        set_web_search_mode(turn, WebSearchMode::Live);
        turn.model_info.web_search_tool_type = WebSearchToolType::TextAndImage;
    })
    .await;
    assert_eq!(
        live_web_search.visible_spec("web_search"),
        &ToolSpec::WebSearch {
            external_web_access: Some(true),
            indexed_web_access: None,
            filters: None,
            user_location: None,
            search_context_size: None,
            search_content_types: Some(vec!["text".to_string(), "image".to_string()]),
        }
    );

    let code_mode_only = probe(|turn| {
        use_chatgpt_auth(turn);
        set_features(turn, &[Feature::CodeModeOnly, Feature::MultiAgentV2]);
        set_web_search_mode(turn, WebSearchMode::Live);
        turn.model_info.input_modalities = vec![InputModality::Image];
    })
    .await;
    assert_eq!(
        code_mode_only.visible_names,
        vec![
            // Code-mode entrypoints.
            codex_code_mode::PUBLIC_TOOL_NAME,
            codex_code_mode::WAIT_TOOL_NAME,
            "request_user_input",
            // Multi-agent v2 tools.
            MULTI_AGENT_V2_NAMESPACE,
            // Hosted Responses tools.
            "web_search",
        ]
    );

    let standalone_web_search_without_web_run = probe(|turn| {
        set_feature(turn, Feature::StandaloneWebSearch, /*enabled*/ true);
        set_web_search_mode(turn, WebSearchMode::Live);
    })
    .await;
    standalone_web_search_without_web_run.assert_visible_contains(&["web_search"]);

    let standalone_web_search = probe_with(
        |turn| {
            set_feature(turn, Feature::StandaloneWebSearch, /*enabled*/ true);
            set_web_search_mode(turn, WebSearchMode::Live);
        },
        ToolPlanInputs {
            extension_tool_executors: vec![Arc::new(TestNamespaceExtensionTool {
                namespace: "web",
                tool_name: "run",
            })],
            ..Default::default()
        },
    )
    .await;
    standalone_web_search.assert_visible_lacks(&["web_search"]);

    let unsupported_provider = probe(|turn| {
        set_web_search_mode(turn, WebSearchMode::Live);
        use_bedrock_provider(turn);
    })
    .await;
    unsupported_provider.assert_visible_lacks(&["web_search"]);
}
