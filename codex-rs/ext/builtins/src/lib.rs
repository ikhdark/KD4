//! Shared installation profile for the built-in runtime extensions.

use std::sync::Arc;
use std::sync::Weak;

use codex_analytics::AnalyticsEventsClient;
use codex_core::StateDbHandle;
use codex_core::ThreadManager;
use codex_core::config::Config;
use codex_exec_server::EnvironmentManager;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_protocol::protocol::SessionSource;

pub use codex_goal_extension::GoalService;

/// Host capabilities used by the common built-in extension profile.
pub struct BuiltinExtensionDependencies {
    pub auth_manager: Arc<codex_login::AuthManager>,
    pub state_db: Option<StateDbHandle>,
    pub analytics_events_client: Option<AnalyticsEventsClient>,
    pub thread_manager: Weak<ThreadManager>,
    pub goal_service: Arc<GoalService>,
    pub environment_manager: Arc<EnvironmentManager>,
    pub session_source: SessionSource,
}

/// Installs every built-in extension supported by the core runtime.
///
/// Hosts supply capabilities and their own event sink, but do not maintain
/// independent lists of built-in extensions.
pub fn install_builtin_extensions(
    builder: &mut ExtensionRegistryBuilder<Config>,
    dependencies: BuiltinExtensionDependencies,
) {
    let BuiltinExtensionDependencies {
        auth_manager,
        state_db,
        analytics_events_client,
        thread_manager,
        goal_service,
        environment_manager,
        session_source,
    } = dependencies;

    if let Some(state_db) = state_db {
        codex_goal_extension::install_with_backend(
            builder,
            state_db,
            analytics_events_client.unwrap_or_else(AnalyticsEventsClient::disabled),
            codex_otel::global(),
            thread_manager,
            goal_service,
            |config: &Config| config.features.enabled(codex_features::Feature::Goals),
        );
    }
    codex_memories_extension::install(builder, codex_otel::global());
    codex_mcp_extension::install(builder);
    codex_mcp_extension::install_executor_plugins(builder, Arc::clone(&environment_manager));
    codex_web_search_extension::install(builder, Arc::clone(&auth_manager));
    codex_image_generation_extension::install(builder, auth_manager, |config: &Config| {
        Some(config.codex_home.clone())
    });

    let executor_skill_provider: Arc<dyn codex_skills_extension::SkillProvider> = Arc::new(
        codex_skills_extension::ExecutorSkillProvider::new_with_restriction_product(
            environment_manager,
            session_source.restriction_product(),
        ),
    );
    let skill_providers = codex_skills_extension::SkillProviders::new()
        .with_executor_provider(executor_skill_provider)
        .with_orchestrator_provider(Arc::new(
            codex_skills_extension::OrchestratorSkillProvider::new(),
        ));
    codex_skills_extension::install_with_providers(builder, skill_providers, |config: &Config| {
        codex_skills_extension::SkillsExtensionConfig {
            include_instructions: config.include_skill_instructions,
            bundled_skills_enabled: config.bundled_skills_enabled(),
            orchestrator_skills_enabled: config.orchestrator_skills_enabled,
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_login::CodexAuth;
    use pretty_assertions::assert_eq;

    #[test]
    fn common_profile_installs_the_complete_non_persistent_surface() {
        let mut builder = ExtensionRegistryBuilder::<Config>::new();
        install_builtin_extensions(
            &mut builder,
            BuiltinExtensionDependencies {
                auth_manager: codex_login::AuthManager::from_auth_for_testing(
                    CodexAuth::from_api_key("test"),
                ),
                state_db: None,
                analytics_events_client: None,
                thread_manager: Weak::new(),
                goal_service: Arc::new(GoalService::new()),
                environment_manager: Arc::new(EnvironmentManager::default_for_tests()),
                session_source: SessionSource::Exec,
            },
        );

        let registry = builder.build();
        assert_eq!(registry.thread_lifecycle_contributors().len(), 4);
        assert_eq!(registry.config_contributors().len(), 4);
        assert_eq!(registry.context_contributors().len(), 2);
        assert_eq!(registry.turn_input_contributors().len(), 1);
        assert_eq!(registry.tool_contributors().len(), 4);
        assert_eq!(
            registry
                .mcp_server_contributors()
                .iter()
                .map(|contributor| contributor.id())
                .collect::<Vec<_>>(),
            vec!["hosted_plugin_runtime", "selected_executor_plugin_mcp"]
        );
    }
}
