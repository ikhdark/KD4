//! Shared installation profile for the built-in runtime extensions.

use std::sync::Arc;

use codex_core::config::Config;
use codex_exec_server::EnvironmentManager;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_protocol::protocol::SessionSource;

/// Host capabilities used by the common built-in extension profile.
pub struct BuiltinExtensionDependencies {
    pub auth_manager: Arc<codex_login::AuthManager>,
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
        environment_manager,
        session_source,
    } = dependencies;

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
        .with_host_provider(Arc::new(codex_skills_extension::HostSkillProvider::new()))
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
