use codex_app_server_protocol_types::ComputerUseRequirements;
use codex_app_server_protocol_types::ConfigRequirements;
use codex_app_server_protocol_types::ConfiguredHookHandler;
use codex_app_server_protocol_types::ConfiguredHookMatcherGroup;
use codex_app_server_protocol_types::ManagedHooksRequirements;
use codex_app_server_protocol_types::ModelsRequirements;
use codex_app_server_protocol_types::NetworkDomainPermission;
use codex_app_server_protocol_types::NetworkRequirements;
use codex_app_server_protocol_types::NetworkUnixSocketPermission;
use codex_app_server_protocol_types::NewThreadModelDefaults;
use codex_app_server_protocol_types::SandboxMode;
use codex_app_server_protocol_types::WindowsSandboxSetupMode;
use codex_config::ConfigRequirementsToml;
use codex_config::HookEventsToml;
use codex_config::HookHandlerConfig as CoreHookHandlerConfig;
use codex_config::ManagedHooksRequirementsToml;
use codex_config::MatcherGroup as CoreMatcherGroup;
use codex_config::ResidencyRequirement as CoreResidencyRequirement;
use codex_config::SandboxModeRequirement as CoreSandboxModeRequirement;
use codex_protocol::config_types::WebSearchMode;
use codex_app_server_protocol_types::ConfigLayer as ApiConfigLayer;
use codex_app_server_protocol_types::ConfigLayerMetadata as ApiConfigLayerMetadata;
use codex_app_server_protocol_types::ConfigLayerSource as ApiConfigLayerSource;
use codex_config::ConfigLayer;
use codex_config::ConfigLayerMetadata;
use codex_config::ConfigLayerSource;

/// Converts a config-layer source owned by `codex-config` into the app-server wire type owned by
/// `codex-app-server-protocol-types`.
///
/// The types stay separate so app-server protocol ownership does not leak into the config domain
/// crate. Because this crate owns neither type, Rust's orphan rules require an explicit conversion
/// function instead of a `From` implementation.
fn config_layer_source_to_api(source: ConfigLayerSource) -> ApiConfigLayerSource {
    match source {
        ConfigLayerSource::Mdm { domain, key } => ApiConfigLayerSource::Mdm { domain, key },
        ConfigLayerSource::System { file } => ApiConfigLayerSource::System { file },
        ConfigLayerSource::EnterpriseManaged { id, name } => {
            ApiConfigLayerSource::EnterpriseManaged { id, name }
        }
        ConfigLayerSource::User { file, profile } => ApiConfigLayerSource::User { file, profile },
        ConfigLayerSource::Project { dot_codex_folder } => {
            ApiConfigLayerSource::Project { dot_codex_folder }
        }
        ConfigLayerSource::SessionFlags => ApiConfigLayerSource::SessionFlags,
        ConfigLayerSource::LegacyManagedConfigTomlFromFile { file } => {
            ApiConfigLayerSource::LegacyManagedConfigTomlFromFile { file }
        }
        ConfigLayerSource::LegacyManagedConfigTomlFromMdm => {
            ApiConfigLayerSource::LegacyManagedConfigTomlFromMdm
        }
    }
}

/// Converts config-layer metadata owned by `codex-config` into the app-server wire type owned by
/// `codex-app-server-protocol-types`.
///
/// The types stay separate so app-server protocol ownership does not leak into the config domain
/// crate. Because this crate owns neither type, Rust's orphan rules require an explicit conversion
/// function instead of a `From` implementation.
pub(crate) fn config_layer_metadata_to_api(
    metadata: ConfigLayerMetadata,
) -> ApiConfigLayerMetadata {
    ApiConfigLayerMetadata {
        name: config_layer_source_to_api(metadata.name),
        version: metadata.version,
    }
}

/// Converts a config layer owned by `codex-config` into the app-server wire type owned by
/// `codex-app-server-protocol-types`.
///
/// The types stay separate so app-server protocol ownership does not leak into the config domain
/// crate. Because this crate owns neither type, Rust's orphan rules require an explicit conversion
/// function instead of a `From` implementation.
pub(crate) fn config_layer_to_api(layer: ConfigLayer) -> ApiConfigLayer {
    ApiConfigLayer {
        name: config_layer_source_to_api(layer.name),
        version: layer.version,
        config: layer.config,
        disabled_reason: layer.disabled_reason,
    }
}

/// Converts domain requirements into the app-server compatibility view.
pub fn requirements(requirements: ConfigRequirementsToml) -> ConfigRequirements {
    ConfigRequirements {
        allowed_approval_policies: requirements.allowed_approval_policies.map(|policies| {
            policies
                .into_iter()
                .map(codex_app_server_protocol_types::AskForApproval::from)
                .collect()
        }),
        allowed_sandbox_modes: requirements.allowed_sandbox_modes.map(|modes| {
            modes
                .into_iter()
                .filter_map(map_sandbox_mode_requirement_to_api)
                .collect()
        }),
        allowed_windows_sandbox_implementations: requirements.windows.and_then(|windows| {
            windows
                .allowed_sandbox_implementations
                .map(|implementations| {
                    implementations
                        .into_iter()
                        .map(|implementation| match implementation {
                            codex_config::types::WindowsSandboxModeToml::Elevated => {
                                WindowsSandboxSetupMode::Elevated
                            }
                            codex_config::types::WindowsSandboxModeToml::Unelevated => {
                                WindowsSandboxSetupMode::Unelevated
                            }
                        })
                        .collect()
                })
        }),
        allowed_permission_profiles: requirements.allowed_permission_profiles,
        default_permissions: requirements.default_permissions,
        allowed_web_search_modes: requirements.allowed_web_search_modes.map(|modes| {
            let mut normalized = modes
                .into_iter()
                .map(Into::into)
                .collect::<Vec<WebSearchMode>>();
            if !normalized.contains(&WebSearchMode::Disabled) {
                normalized.push(WebSearchMode::Disabled);
            }
            normalized
        }),
        allow_managed_hooks_only: requirements.allow_managed_hooks_only,
        allow_appshots: requirements.allow_appshots,
        allow_remote_control: requirements.allow_remote_control,
        computer_use: requirements
            .computer_use
            .map(map_computer_use_requirements_to_api),
        feature_requirements: requirements
            .feature_requirements
            .map(|requirements| requirements.entries),
        hooks: requirements.hooks.map(map_hooks_requirements_to_api),
        enforce_residency: requirements
            .enforce_residency
            .map(map_residency_requirement_to_api),
        network: requirements.network.map(map_network_requirements_to_api),
        models: requirements.models.map(|models| ModelsRequirements {
            new_thread: models.new_thread.map(|new_thread| NewThreadModelDefaults {
                model: new_thread.model,
                model_reasoning_effort: new_thread.model_reasoning_effort,
                service_tier: new_thread.service_tier,
            }),
        }),
    }
}

fn map_computer_use_requirements_to_api(
    computer_use: codex_config::ComputerUseRequirementsToml,
) -> ComputerUseRequirements {
    ComputerUseRequirements {
        allow_locked_computer_use: computer_use.allow_locked_computer_use,
    }
}

fn map_hooks_requirements_to_api(hooks: ManagedHooksRequirementsToml) -> ManagedHooksRequirements {
    let ManagedHooksRequirementsToml {
        managed_dir,
        windows_managed_dir,
        hooks,
    } = hooks;
    let HookEventsToml {
        pre_tool_use,
        permission_request,
        post_tool_use,
        pre_compact,
        post_compact,
        session_start,
        user_prompt_submit,
        subagent_start,
        subagent_stop,
        stop,
        interrupt,
    } = hooks;

    ManagedHooksRequirements {
        managed_dir,
        windows_managed_dir,
        pre_tool_use: map_hook_matcher_groups_to_api(pre_tool_use),
        permission_request: map_hook_matcher_groups_to_api(permission_request),
        post_tool_use: map_hook_matcher_groups_to_api(post_tool_use),
        pre_compact: map_hook_matcher_groups_to_api(pre_compact),
        post_compact: map_hook_matcher_groups_to_api(post_compact),
        session_start: map_hook_matcher_groups_to_api(session_start),
        user_prompt_submit: map_hook_matcher_groups_to_api(user_prompt_submit),
        subagent_start: map_hook_matcher_groups_to_api(subagent_start),
        subagent_stop: map_hook_matcher_groups_to_api(subagent_stop),
        stop: map_hook_matcher_groups_to_api(stop),
        interrupt: map_hook_matcher_groups_to_api(interrupt),
    }
}

fn map_hook_matcher_groups_to_api(
    groups: Vec<CoreMatcherGroup>,
) -> Vec<ConfiguredHookMatcherGroup> {
    groups
        .into_iter()
        .map(map_hook_matcher_group_to_api)
        .collect()
}

fn map_hook_matcher_group_to_api(group: CoreMatcherGroup) -> ConfiguredHookMatcherGroup {
    ConfiguredHookMatcherGroup {
        matcher: group.matcher,
        hooks: group
            .hooks
            .into_iter()
            .map(map_hook_handler_to_api)
            .collect(),
    }
}

fn map_hook_handler_to_api(handler: CoreHookHandlerConfig) -> ConfiguredHookHandler {
    match handler {
        CoreHookHandlerConfig::Command {
            command,
            command_windows,
            timeout_sec,
            r#async,
            status_message,
        } => ConfiguredHookHandler::Command {
            command,
            command_windows,
            timeout_sec,
            r#async,
            status_message,
        },
        CoreHookHandlerConfig::Prompt {} => ConfiguredHookHandler::Prompt {},
        CoreHookHandlerConfig::Agent {} => ConfiguredHookHandler::Agent {},
    }
}

fn map_sandbox_mode_requirement_to_api(mode: CoreSandboxModeRequirement) -> Option<SandboxMode> {
    match mode {
        CoreSandboxModeRequirement::ReadOnly => Some(SandboxMode::ReadOnly),
        CoreSandboxModeRequirement::WorkspaceWrite => Some(SandboxMode::WorkspaceWrite),
        CoreSandboxModeRequirement::DangerFullAccess => Some(SandboxMode::DangerFullAccess),
        CoreSandboxModeRequirement::ExternalSandbox => None,
    }
}

fn map_residency_requirement_to_api(
    residency: CoreResidencyRequirement,
) -> codex_app_server_protocol_types::ResidencyRequirement {
    match residency {
        CoreResidencyRequirement::Us => codex_app_server_protocol_types::ResidencyRequirement::Us,
    }
}

fn map_network_requirements_to_api(
    network: codex_config::NetworkRequirementsToml,
) -> NetworkRequirements {
    let allowed_domains = network
        .domains
        .as_ref()
        .and_then(codex_config::NetworkDomainPermissionsToml::allowed_domains);
    let denied_domains = network
        .domains
        .as_ref()
        .and_then(codex_config::NetworkDomainPermissionsToml::denied_domains);
    let allow_unix_sockets = network
        .unix_sockets
        .as_ref()
        .map(codex_config::NetworkUnixSocketPermissionsToml::allow_unix_sockets)
        .filter(|entries| !entries.is_empty());

    NetworkRequirements {
        enabled: network.enabled,
        http_port: network.http_port,
        socks_port: network.socks_port,
        allow_upstream_proxy: network.allow_upstream_proxy,
        dangerously_allow_non_loopback_proxy: network.dangerously_allow_non_loopback_proxy,
        dangerously_allow_all_unix_sockets: network.dangerously_allow_all_unix_sockets,
        domains: network.domains.map(|domains| {
            domains
                .entries
                .into_iter()
                .map(|(pattern, permission)| {
                    (pattern, map_network_domain_permission_to_api(permission))
                })
                .collect()
        }),
        managed_allowed_domains_only: network.managed_allowed_domains_only,
        allowed_domains,
        denied_domains,
        unix_sockets: network.unix_sockets.map(|unix_sockets| {
            unix_sockets
                .entries
                .into_iter()
                .map(|(path, permission)| {
                    (path, map_network_unix_socket_permission_to_api(permission))
                })
                .collect()
        }),
        allow_unix_sockets,
        allow_local_binding: network.allow_local_binding,
    }
}

fn map_network_domain_permission_to_api(
    permission: codex_config::NetworkDomainPermissionToml,
) -> NetworkDomainPermission {
    match permission {
        codex_config::NetworkDomainPermissionToml::Allow => NetworkDomainPermission::Allow,
        codex_config::NetworkDomainPermissionToml::Deny => NetworkDomainPermission::Deny,
    }
}

fn map_network_unix_socket_permission_to_api(
    permission: codex_config::NetworkUnixSocketPermissionToml,
) -> NetworkUnixSocketPermission {
    match permission {
        codex_config::NetworkUnixSocketPermissionToml::Allow => NetworkUnixSocketPermission::Allow,
        codex_config::NetworkUnixSocketPermissionToml::Deny => NetworkUnixSocketPermission::Deny,
    }
}
