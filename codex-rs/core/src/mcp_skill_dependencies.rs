use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::hash_map::Entry;

use codex_config::ConfigEditsBuilder;
use codex_config::McpServerConfig;
use codex_config::McpServerTransportConfig;
use codex_config::load_global_mcp_servers;
use codex_login::default_client::is_first_party_originator;
use codex_login::default_client::originator;
use codex_protocol::request_user_input::RequestUserInputArgs;
use codex_protocol::request_user_input::RequestUserInputQuestion;
use codex_protocol::request_user_input::RequestUserInputQuestionOption;
use codex_rmcp_client::perform_oauth_login;
use sha2::Digest;
use sha2::Sha256;
use tokio_util::sync::CancellationToken;

use crate::SkillMetadata;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::skills::model::SkillToolDependency;
use codex_mcp::ElicitationReviewerHandle;
use codex_mcp::McpOAuthLoginSupport;
use codex_mcp::McpPermissionPromptAutoApproveContext;
use codex_mcp::mcp_permission_prompt_is_auto_approved;
use codex_mcp::oauth_login_support;
use codex_mcp::resolve_oauth_scopes;
use codex_mcp::should_retry_without_scopes;

const SKILL_MCP_DEPENDENCY_PROMPT_ID: &str = "skill_mcp_dependency_install";
const MCP_DEPENDENCY_OPTION_INSTALL: &str = "Install";
const MCP_DEPENDENCY_OPTION_SKIP: &str = "Continue anyway";

#[derive(Clone, Debug)]
pub(crate) struct PlannedMcpDependencyEffect {
    pub(crate) id: String,
    missing: HashMap<String, McpServerConfig>,
    pub(crate) expected_inventory_keys: HashSet<String>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct PlannedMcpDependencies {
    pub(crate) effect: Option<PlannedMcpDependencyEffect>,
    pub(crate) warnings: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum McpDependencyEffectOutcome {
    Skipped,
    InventoryChanged {
        expected_inventory_keys: HashSet<String>,
    },
}

/// Read-only dependency planning. The returned effect ID hashes the exact
/// dependency names and requested configurations, not merely the effect type.
pub(crate) async fn plan_mcp_dependencies(
    sess: &Session,
    turn_context: &TurnContext,
    mentioned_skills: &[SkillMetadata],
) -> PlannedMcpDependencies {
    let originator_value = originator().value;
    if !is_first_party_originator(originator_value.as_str())
        || mentioned_skills.is_empty()
        || !turn_context
            .config
            .features
            .enabled(codex_features::Feature::SkillMcpDependencyInstall)
    {
        return PlannedMcpDependencies::default();
    }

    let installed = sess.runtime_mcp_servers(turn_context.config.as_ref()).await;
    let (missing, warnings) = collect_missing_mcp_dependencies(mentioned_skills, &installed);
    let unprompted_missing = filter_prompted_mcp_dependencies(sess, &missing).await;
    if unprompted_missing.is_empty() {
        return PlannedMcpDependencies {
            effect: None,
            warnings,
        };
    }

    let expected_inventory_keys = unprompted_missing
        .iter()
        .map(|(name, config)| canonical_mcp_server_key(name, config))
        .collect::<HashSet<_>>();
    let id = semantic_install_effect_id(&unprompted_missing);
    PlannedMcpDependencies {
        effect: Some(PlannedMcpDependencyEffect {
            id,
            missing: unprompted_missing,
            expected_inventory_keys,
        }),
        warnings,
    }
}

pub(crate) async fn apply_mcp_dependency_effect(
    sess: &Session,
    turn_context: &TurnContext,
    cancellation_token: &CancellationToken,
    effect: &PlannedMcpDependencyEffect,
    elicitation_reviewer: Option<ElicitationReviewerHandle>,
) -> Result<McpDependencyEffectOutcome, String> {
    let should_install = should_install_planned_mcp_dependencies(
        sess,
        turn_context,
        &effect.missing,
        cancellation_token,
    )
    .await?;
    if !should_install {
        return Ok(McpDependencyEffectOutcome::Skipped);
    }

    install_planned_mcp_dependencies(
        sess,
        turn_context,
        turn_context.config.as_ref(),
        &effect.missing,
        elicitation_reviewer,
    )
    .await?;
    if !inventory_contains_expected(sess, &effect.expected_inventory_keys).await {
        return Err(format!(
            "completed MCP dependency effect `{}` was not observable in the refreshed inventory",
            effect.id
        ));
    }
    Ok(McpDependencyEffectOutcome::InventoryChanged {
        expected_inventory_keys: effect.expected_inventory_keys.clone(),
    })
}

pub(crate) async fn inventory_contains_expected(
    sess: &Session,
    expected_inventory_keys: &HashSet<String>,
) -> bool {
    if expected_inventory_keys.is_empty() {
        return true;
    }
    let runtime = sess.services.latest_mcp_runtime();
    let installed = codex_mcp::configured_mcp_servers(runtime.config());
    let installed_keys = installed
        .iter()
        .map(|(name, config)| canonical_mcp_server_key(name, config))
        .collect::<HashSet<_>>();
    expected_inventory_keys.is_subset(&installed_keys)
}

fn semantic_install_effect_id(missing: &HashMap<String, McpServerConfig>) -> String {
    let mut entries = missing.iter().collect::<Vec<_>>();
    entries.sort_by_key(|(left, _)| *left);
    let mut hasher = Sha256::new();
    for (name, config) in entries {
        hasher.update(name.as_bytes());
        hasher.update([0]);
        hasher.update(canonical_mcp_server_key(name, config).as_bytes());
        hasher.update([0]);
        if let Ok(serialized) = serde_json::to_vec(config) {
            hasher.update(serialized);
        }
        hasher.update([0xff]);
    }
    format!("install_mcp_dependencies:{:x}", hasher.finalize())
}

async fn install_planned_mcp_dependencies(
    sess: &Session,
    turn_context: &TurnContext,
    config: &crate::config::Config,
    missing: &HashMap<String, McpServerConfig>,
    elicitation_reviewer: Option<ElicitationReviewerHandle>,
) -> Result<(), String> {
    let codex_home = config.codex_home.clone();
    let mut servers = load_global_mcp_servers(&codex_home).await.map_err(|err| {
        format!("failed to load MCP servers while installing dependencies: {err}")
    })?;
    let mut added = Vec::new();
    let mut entries = missing.iter().collect::<Vec<_>>();
    entries.sort_by_key(|(left, _)| *left);
    for (name, server_config) in entries {
        if servers.contains_key(name) {
            continue;
        }
        servers.insert(name.clone(), server_config.clone());
        added.push((name.clone(), server_config.clone()));
    }

    for (name, server_config) in &added {
        let oauth_config = match oauth_login_support(&server_config.transport).await {
            McpOAuthLoginSupport::Supported(config) => config,
            McpOAuthLoginSupport::Unsupported => continue,
            McpOAuthLoginSupport::Unknown(err) => {
                return Err(format!(
                    "could not determine OAuth requirements for MCP dependency {name}: {err}"
                ));
            }
        };
        let resolved_scopes = resolve_oauth_scopes(
            /*explicit_scopes*/ None,
            server_config.scopes.clone(),
            oauth_config.discovered_scopes.clone(),
        );
        let oauth_client_id = server_config.oauth_client_id();
        let first_attempt = perform_oauth_login(
            name,
            &oauth_config.url,
            config.mcp_oauth_credentials_store_mode,
            config.auth_keyring_backend_kind(),
            oauth_config.http_headers.clone(),
            oauth_config.env_http_headers.clone(),
            &resolved_scopes.scopes,
            oauth_client_id,
            server_config.oauth_resource.as_deref(),
            config.mcp_oauth_callback_port,
            config.mcp_oauth_callback_url.as_deref(),
        )
        .await;
        if let Err(err) = first_attempt {
            if should_retry_without_scopes(&resolved_scopes, &err) {
                perform_oauth_login(
                    name,
                    &oauth_config.url,
                    config.mcp_oauth_credentials_store_mode,
                    config.auth_keyring_backend_kind(),
                    oauth_config.http_headers,
                    oauth_config.env_http_headers,
                    &[],
                    oauth_client_id,
                    server_config.oauth_resource.as_deref(),
                    config.mcp_oauth_callback_port,
                    config.mcp_oauth_callback_url.as_deref(),
                )
                .await
                .map_err(|err| format!("failed to login to MCP dependency {name}: {err}"))?;
            } else {
                return Err(format!("failed to login to MCP dependency {name}: {err}"));
            }
        }
    }

    if !added.is_empty() {
        ConfigEditsBuilder::new(&codex_home)
            .replace_mcp_servers(&servers)
            .apply()
            .await
            .map_err(|err| format!("failed to persist planned MCP dependencies: {err}"))?;
    }

    let mut refresh_config = config.clone();
    let mut configured_servers = config.mcp_servers.get().clone();
    for (name, server_config) in &servers {
        configured_servers
            .entry(name.clone())
            .or_insert_with(|| server_config.clone());
    }
    refresh_config
        .mcp_servers
        .set(configured_servers)
        .map_err(|err| format!("failed to prepare refreshed MCP dependency inventory: {err}"))?;
    sess.refresh_mcp_servers_now(turn_context, &refresh_config, elicitation_reviewer)
        .await;
    Ok(())
}

async fn should_install_planned_mcp_dependencies(
    sess: &Session,
    turn_context: &TurnContext,
    missing: &HashMap<String, McpServerConfig>,
    cancellation_token: &CancellationToken,
) -> Result<bool, String> {
    if mcp_permission_prompt_is_auto_approved(
        turn_context.approval_policy.value(),
        &turn_context.permission_profile(),
        McpPermissionPromptAutoApproveContext::default(),
    ) {
        return Ok(true);
    }

    let server_list = format_missing_mcp_dependencies(missing);
    let args = RequestUserInputArgs {
        questions: vec![RequestUserInputQuestion {
            id: SKILL_MCP_DEPENDENCY_PROMPT_ID.to_string(),
            header: "Install MCP servers?".to_string(),
            question: format!(
                "The following MCP servers are required by the selected skills but are not installed yet: {server_list}. Install them now?"
            ),
            is_other: false,
            is_secret: false,
            options: Some(vec![
                RequestUserInputQuestionOption {
                    label: MCP_DEPENDENCY_OPTION_INSTALL.to_string(),
                    description: "Install and enable the missing MCP servers in your global config."
                        .to_string(),
                },
                RequestUserInputQuestionOption {
                    label: MCP_DEPENDENCY_OPTION_SKIP.to_string(),
                    description: "Skip installation for now and do not show again for these MCP servers in this session."
                        .to_string(),
                },
            ]),
        }],
        auto_resolution_ms: None,
    };
    let sub_id = &turn_context.sub_id;
    let call_id = format!("mcp-deps-{sub_id}");
    let response = tokio::select! {
        biased;
        _ = cancellation_token.cancelled() => {
            return Err("MCP dependency effect was cancelled".to_string());
        }
        response = sess.request_user_input(turn_context, call_id, args) => {
            response.ok_or_else(|| "MCP dependency prompt closed without a response".to_string())?
        }
    };
    let answers = response
        .answers
        .get(SKILL_MCP_DEPENDENCY_PROMPT_ID)
        .map(|answer| answer.answers.as_slice())
        .ok_or_else(|| "MCP dependency prompt returned no answer".to_string())?;
    let install = answers
        .iter()
        .any(|entry| entry == MCP_DEPENDENCY_OPTION_INSTALL);
    let skip = answers
        .iter()
        .any(|entry| entry == MCP_DEPENDENCY_OPTION_SKIP);
    match (install, skip) {
        (true, false) => Ok(true),
        (false, true) => {
            let prompted_keys = missing
                .iter()
                .map(|(name, config)| canonical_mcp_server_key(name, config));
            sess.record_mcp_dependency_prompted(prompted_keys).await;
            Ok(false)
        }
        _ => Err("MCP dependency prompt returned an invalid answer".to_string()),
    }
}

async fn filter_prompted_mcp_dependencies(
    sess: &Session,
    missing: &HashMap<String, McpServerConfig>,
) -> HashMap<String, McpServerConfig> {
    let prompted = sess.mcp_dependency_prompted().await;
    if prompted.is_empty() {
        return missing.clone();
    }

    missing
        .iter()
        .filter(|(name, config)| !prompted.contains(&canonical_mcp_server_key(name, config)))
        .map(|(name, config)| (name.clone(), config.clone()))
        .collect()
}

fn format_missing_mcp_dependencies(missing: &HashMap<String, McpServerConfig>) -> String {
    let mut names = missing.keys().cloned().collect::<Vec<_>>();
    names.sort();
    names.join(", ")
}

fn canonical_mcp_key(transport: &str, identifier: &str, fallback: &str) -> String {
    let identifier = identifier.trim();
    if identifier.is_empty() {
        fallback.to_string()
    } else {
        format!("mcp__{transport}__{identifier}")
    }
}

fn canonical_mcp_server_key(name: &str, config: &McpServerConfig) -> String {
    match &config.transport {
        McpServerTransportConfig::Stdio { command, .. } => {
            canonical_mcp_key("stdio", command, name)
        }
        McpServerTransportConfig::StreamableHttp { url, .. } => {
            canonical_mcp_key("streamable_http", url, name)
        }
    }
}

fn canonical_mcp_dependency_key(dependency: &SkillToolDependency) -> Result<String, String> {
    let transport = dependency.transport.as_deref().unwrap_or("streamable_http");
    if transport.eq_ignore_ascii_case("streamable_http") {
        let url = dependency
            .url
            .as_ref()
            .ok_or_else(|| "missing url for streamable_http dependency".to_string())?;
        return Ok(canonical_mcp_key("streamable_http", url, &dependency.value));
    }
    if transport.eq_ignore_ascii_case("stdio") {
        let command = dependency
            .command
            .as_ref()
            .ok_or_else(|| "missing command for stdio dependency".to_string())?;
        return Ok(canonical_mcp_key("stdio", command, &dependency.value));
    }
    Err(format!("unsupported transport {transport}"))
}

fn mcp_dependency_to_server_config(
    dependency: &SkillToolDependency,
) -> Result<McpServerConfig, String> {
    let transport = dependency.transport.as_deref().unwrap_or("streamable_http");
    if transport.eq_ignore_ascii_case("streamable_http") {
        let url = dependency
            .url
            .as_ref()
            .ok_or_else(|| "missing url for streamable_http dependency".to_string())?;
        return Ok(McpServerConfig {
            auth: Default::default(),
            transport: McpServerTransportConfig::StreamableHttp {
                url: url.clone(),
                bearer_token_env_var: None,
                http_headers: None,
                env_http_headers: None,
            },
            environment_id: codex_config::DEFAULT_MCP_SERVER_ENVIRONMENT_ID.to_string(),
            enabled: true,
            required: false,
            supports_parallel_tool_calls: false,
            disabled_reason: None,
            startup_timeout_sec: None,
            tool_timeout_sec: None,
            default_tools_approval_mode: None,
            enabled_tools: None,
            disabled_tools: None,
            scopes: None,
            oauth: None,
            oauth_resource: None,
            tools: HashMap::new(),
        });
    }

    if transport.eq_ignore_ascii_case("stdio") {
        let command = dependency
            .command
            .as_ref()
            .ok_or_else(|| "missing command for stdio dependency".to_string())?;
        return Ok(McpServerConfig {
            auth: Default::default(),
            transport: McpServerTransportConfig::Stdio {
                command: command.clone(),
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
            startup_timeout_sec: None,
            tool_timeout_sec: None,
            default_tools_approval_mode: None,
            enabled_tools: None,
            disabled_tools: None,
            scopes: None,
            oauth: None,
            oauth_resource: None,
            tools: HashMap::new(),
        });
    }

    Err(format!("unsupported transport {transport}"))
}

struct DeclaredMcpDependency {
    config: McpServerConfig,
    skill_names: HashSet<String>,
}

fn format_mcp_dependency_transport(config: &McpServerConfig) -> String {
    match &config.transport {
        McpServerTransportConfig::Stdio { command, .. } => {
            format!("stdio command {command:?}")
        }
        McpServerTransportConfig::StreamableHttp { url, .. } => {
            format!("streamable_http URL {url:?}")
        }
    }
}

fn format_mcp_dependency_skills(skill_names: &HashSet<String>) -> String {
    let has_multiple_skills = skill_names.len() > 1;
    let mut skill_names = skill_names.iter().collect::<Vec<_>>();
    skill_names.sort();
    let skill_names = skill_names
        .into_iter()
        .map(|skill_name| format!("{skill_name:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    if has_multiple_skills {
        format!("skills {skill_names}")
    } else {
        format!("skill {skill_names}")
    }
}

fn conflicting_mcp_dependency_warning(
    name: &str,
    declarations: &[(String, DeclaredMcpDependency)],
) -> String {
    let configurations = declarations
        .iter()
        .map(|(_, declaration)| {
            format!(
                "{} requests {}",
                format_mcp_dependency_skills(&declaration.skill_names),
                format_mcp_dependency_transport(&declaration.config)
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    format!(
        "Unable to auto-install MCP dependency {name:?}: the same name has conflicting configurations ({configurations})"
    )
}

fn collect_missing_mcp_dependencies(
    mentioned_skills: &[SkillMetadata],
    installed: &HashMap<String, McpServerConfig>,
) -> (HashMap<String, McpServerConfig>, Vec<String>) {
    let installed_keys: HashSet<String> = installed
        .iter()
        .map(|(name, config)| canonical_mcp_server_key(name, config))
        .collect();
    let mut declared_by_name: HashMap<String, HashMap<String, DeclaredMcpDependency>> =
        HashMap::new();
    let mut warnings = Vec::new();

    for skill in mentioned_skills {
        let Some(dependencies) = skill.dependencies.as_ref() else {
            continue;
        };

        for tool in &dependencies.tools {
            if !tool.r#type.eq_ignore_ascii_case("mcp") {
                continue;
            }
            let dependency_key = match canonical_mcp_dependency_key(tool) {
                Ok(key) => key,
                Err(err) => {
                    let dependency = tool.value.as_str();
                    let skill_name = skill.name.as_str();
                    warnings.push(format!(
                        "Unable to auto-install MCP dependency {dependency} for skill {skill_name}: {err}"
                    ));
                    continue;
                }
            };
            if installed_keys.contains(&dependency_key) {
                continue;
            }

            let config = match mcp_dependency_to_server_config(tool) {
                Ok(config) => config,
                Err(err) => {
                    let dependency = dependency_key.as_str();
                    let skill_name = skill.name.as_str();
                    warnings.push(format!(
                        "Unable to auto-install MCP dependency {dependency} for skill {skill_name}: {err}"
                    ));
                    continue;
                }
            };

            let declarations = declared_by_name.entry(tool.value.clone()).or_default();
            let declaration =
                declarations
                    .entry(dependency_key)
                    .or_insert_with(|| DeclaredMcpDependency {
                        config,
                        skill_names: HashSet::new(),
                    });
            declaration.skill_names.insert(skill.name.clone());
        }
    }

    let mut seen_canonical_keys = HashSet::new();
    let mut missing = HashMap::new();
    let mut names = declared_by_name.into_iter().collect::<Vec<_>>();
    names.sort_by(|(left, _), (right, _)| left.cmp(right));

    for (name, declarations) in names {
        let mut declarations = declarations.into_iter().collect::<Vec<_>>();
        declarations.sort_by(|(left, _), (right, _)| left.cmp(right));
        if declarations.len() > 1 {
            warnings.push(conflicting_mcp_dependency_warning(&name, &declarations));
            continue;
        }

        let Some((dependency_key, declaration)) = declarations.pop() else {
            continue;
        };
        if let Some(installed_config) = installed.get(&name) {
            let installed_key = canonical_mcp_server_key(&name, installed_config);
            if installed_key != dependency_key {
                let installed_transport = format_mcp_dependency_transport(installed_config);
                let requested_transport = format_mcp_dependency_transport(&declaration.config);
                warnings.push(format!(
                    "Unable to auto-install MCP dependency {name:?}: the installed server uses {installed_transport}, but {} requests {requested_transport}",
                    format_mcp_dependency_skills(&declaration.skill_names)
                ));
            }
            continue;
        }
        if !seen_canonical_keys.insert(dependency_key.clone()) {
            continue;
        }

        match missing.entry(name.clone()) {
            Entry::Vacant(entry) => {
                entry.insert(declaration.config);
            }
            Entry::Occupied(entry) => {
                let existing_transport = format_mcp_dependency_transport(entry.get());
                let requested_transport = format_mcp_dependency_transport(&declaration.config);
                warnings.push(format!(
                    "Unable to auto-install MCP dependency {name:?}: the same name has conflicting configurations ({existing_transport}; {requested_transport})"
                ));
                entry.remove();
            }
        }
    }

    warnings.sort();

    (missing, warnings)
}
