use crate::FunctionCallError;
use crate::agent::role::AgentRoleLocks;
use crate::config::Config;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use codex_config::types::WindowsSandboxModeToml;
use codex_features::Feature;
use codex_model_provider::create_model_provider;
use codex_models_manager::manager::RefreshStrategy;
use codex_models_manager::manager::SharedModelsManager;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::config_types::WindowsSandboxLevel;
use codex_protocol::error::CodexErr;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::openai_models::ReasoningEffortPreset;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_tools::ToolOutputProjectionMetadata;
use serde::Serialize;
use serde_json::Value as JsonValue;

pub(crate) const DEFAULT_SPAWN_AGENT_MODEL: &str = "gpt-6.1-sol";
pub(crate) const DEFAULT_SPAWN_AGENT_REASONING_EFFORT: ReasoningEffort = ReasoningEffort::XHigh;

pub(crate) fn function_arguments(payload: ToolPayload) -> Result<String, FunctionCallError> {
    match payload {
        ToolPayload::Function { arguments } => Ok(arguments),
        _ => Err(FunctionCallError::RespondToModel(
            "collab handler received unsupported payload".to_string(),
        )),
    }
}

pub(crate) fn tool_output_json_text<T>(value: &T, tool_name: &str) -> String
where
    T: Serialize,
{
    serde_json::to_string(value).unwrap_or_else(|err| {
        JsonValue::String(format!("failed to serialize {tool_name} result: {err}")).to_string()
    })
}

pub(crate) fn tool_output_response_item<T>(
    call_id: &str,
    payload: &ToolPayload,
    value: &T,
    success: Option<bool>,
    tool_name: &str,
) -> ResponseInputItem
where
    T: Serialize,
{
    FunctionToolOutput::from_text(tool_output_json_text(value, tool_name), success)
        .to_response_item(call_id, payload)
}

pub(crate) fn tool_output_code_mode_result<T>(value: &T, tool_name: &str) -> JsonValue
where
    T: Serialize,
{
    serde_json::to_value(value).unwrap_or_else(|err| {
        JsonValue::String(format!("failed to serialize {tool_name} result: {err}"))
    })
}

pub(crate) fn tool_output_projection_metadata<T>(
    value: &T,
    success: bool,
) -> Option<ToolOutputProjectionMetadata>
where
    T: Serialize,
{
    serde_json::to_value(value)
        .ok()
        .map(|value| ToolOutputProjectionMetadata::from_json(&value, success, None))
}

const AGENT_CAPACITY_RECOVERY: &str =
    "Capacity refusal is not evidence that the target's task or result is missing. \
     Do not immediately retry or spawn a duplicate. Continue independent work; wait for a real \
     completion or capacity change before one targeted retry. Use queue-only send_message only \
     for information when no new turn is needed; it does not start work.";

pub(crate) fn collab_spawn_error(err: CodexErr) -> FunctionCallError {
    match err {
        err @ CodexErr::AgentLimitReached { .. } => FunctionCallError::RespondToModel(format!(
            "collab spawn failed: {err}. {AGENT_CAPACITY_RECOVERY}"
        )),
        CodexErr::UnsupportedOperation(message) if message == "thread manager dropped" => {
            FunctionCallError::RespondToModel("collab manager unavailable".to_string())
        }
        CodexErr::UnsupportedOperation(message) => FunctionCallError::RespondToModel(message),
        err => FunctionCallError::RespondToModel(format!("collab spawn failed: {err}")),
    }
}

pub(crate) fn collab_agent_error(agent_id: ThreadId, err: CodexErr) -> FunctionCallError {
    match err {
        err @ CodexErr::AgentLimitReached { .. } => FunctionCallError::RespondToModel(format!(
            "collab tool failed: {err}. {AGENT_CAPACITY_RECOVERY}"
        )),
        CodexErr::ThreadNotFound(id) => {
            FunctionCallError::RespondToModel(format!("agent with id {id} not found"))
        }
        CodexErr::InternalAgentDied => {
            FunctionCallError::RespondToModel(format!("agent with id {agent_id} is closed"))
        }
        CodexErr::UnsupportedOperation(_) => {
            FunctionCallError::RespondToModel("collab manager unavailable".to_string())
        }
        err => FunctionCallError::RespondToModel(format!("collab tool failed: {err}")),
    }
}

pub(crate) fn thread_spawn_source(
    parent_thread_id: ThreadId,
    parent_session_source: &SessionSource,
    depth: i32,
    agent_role: Option<&str>,
    task_name: Option<String>,
) -> Result<SessionSource, FunctionCallError> {
    let agent_path = task_name
        .as_deref()
        .map(|task_name| {
            parent_session_source
                .get_agent_path()
                .unwrap_or_else(AgentPath::root)
                .join(task_name)
                .map_err(FunctionCallError::RespondToModel)
        })
        .transpose()?;
    Ok(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id,
        depth,
        agent_path,
        agent_nickname: None,
        agent_role: agent_role.map(str::to_string),
    }))
}


/// Builds the base config snapshot for a newly spawned sub-agent.
///
/// The returned config starts from the parent's effective config and then refreshes the
/// runtime-owned fields carried on `turn`, including the active base prompt, model selection,
/// reasoning settings, approval policy, sandbox, and cwd. Role-specific overrides are layered
/// after this step;
/// skipping this helper and cloning stale config state directly can send the child agent out with
/// the wrong provider or runtime policy.
pub(crate) fn build_agent_spawn_config(turn: &TurnContext) -> Result<Config, FunctionCallError> {
    let mut config = build_agent_shared_config(turn)?;
    config.base_instructions = Some(turn.base_instructions.text.clone());
    Ok(config)
}

/// New work uses the admitting step's cwd before role/model configuration is resolved.
/// Resume intentionally continues to use its separate stored-child configuration path.
pub(crate) fn build_agent_spawn_config_for_step(
    step: &crate::session::step_context::StepContext,
) -> Result<Config, FunctionCallError> {
    let mut config = build_agent_spawn_config(&step.turn)?;
    if let Some(cwd) = step
        .environments
        .primary()
        .and_then(|primary| primary.cwd().to_abs_path().ok())
    {
        config.cwd = cwd;
    }
    Ok(config)
}

pub(crate) fn build_agent_resume_config(turn: &TurnContext) -> Result<Config, FunctionCallError> {
    let mut config = build_agent_shared_config(turn)?;
    // For resume, keep base instructions sourced from rollout/session metadata.
    config.base_instructions = None;
    Ok(config)
}

fn build_agent_shared_config(turn: &TurnContext) -> Result<Config, FunctionCallError> {
    let base_config = turn.config.clone();
    let mut config = (*base_config).clone();
    config.model = Some(turn.model_info.slug.clone());
    config.model_provider = turn.provider.info().clone();
    config.model_reasoning_effort = turn
        .reasoning_effort
        .clone()
        .or_else(|| turn.model_info.default_reasoning_level.clone());
    config.model_reasoning_summary = Some(turn.reasoning_summary);
    config.developer_instructions = turn.developer_instructions.clone();
    apply_spawn_agent_runtime_overrides(&mut config, turn, AgentRoleLocks::default())?;

    Ok(config)
}

pub(crate) fn reject_full_fork_spawn_overrides(
    agent_type: Option<&str>,
    model: Option<&str>,
    reasoning_effort: Option<ReasoningEffort>,
) -> Result<(), FunctionCallError> {
    if agent_type.is_some() || model.is_some() || reasoning_effort.is_some() {
        return Err(FunctionCallError::RespondToModel(
            "Full-history forked agents use the built-in spawn defaults and do not accept per-call agent_type, model, or reasoning_effort overrides; omit those fields, or spawn without a full-history fork.".to_string(),
        ));
    }
    Ok(())
}

/// Copies runtime-only turn state onto a child config before it is handed to `AgentControl`.
///
/// These values are chosen by the live turn rather than persisted config, so leaving them stale
/// can make a child agent disagree with its parent about approval policy, cwd, or sandboxing. A
/// permission profile explicitly supplied by the role remains authoritative.
pub(crate) fn apply_spawn_agent_runtime_overrides(
    config: &mut Config,
    turn: &TurnContext,
    role_locks: AgentRoleLocks,
) -> Result<(), FunctionCallError> {
    let role_permissions = config.permissions.clone();
    if !role_locks.permissions {
        config.permissions = turn.config.permissions.clone();
        config
            .permissions
            .set_permission_profile(turn.permission_profile())
            .map_err(|err| {
                FunctionCallError::RespondToModel(format!("permission_profile is invalid: {err}"))
            })?;
    }
    config.permissions.allow_login_shell = if role_locks.allow_login_shell {
        role_permissions.allow_login_shell
    } else {
        turn.config.permissions.allow_login_shell
    };
    config.permissions.shell_environment_policy = if role_locks.shell_environment_policy {
        role_permissions.shell_environment_policy
    } else {
        turn.config.permissions.shell_environment_policy.clone()
    };
    config.permissions.windows_sandbox_private_desktop = if role_locks.windows_private_desktop {
        role_permissions.windows_sandbox_private_desktop
    } else {
        turn.config.permissions.windows_sandbox_private_desktop
    };
    if role_locks.windows_sandbox {
        config.permissions.windows_sandbox_mode = role_permissions.windows_sandbox_mode;
    } else {
        config.permissions.windows_sandbox_mode = match turn.windows_sandbox_level {
            WindowsSandboxLevel::Elevated => Some(WindowsSandboxModeToml::Elevated),
            WindowsSandboxLevel::RestrictedToken => Some(WindowsSandboxModeToml::Unelevated),
            WindowsSandboxLevel::Disabled => {
                config
                    .features
                    .disable(Feature::WindowsSandboxElevated)
                    .map_err(|err| {
                        FunctionCallError::RespondToModel(format!(
                            "windows sandbox feature state is invalid: {err}"
                        ))
                    })?;
                config
                    .features
                    .disable(Feature::WindowsSandbox)
                    .map_err(|err| {
                        FunctionCallError::RespondToModel(format!(
                            "windows sandbox feature state is invalid: {err}"
                        ))
                    })?;
                None
            }
        };
    }
    config
        .permissions
        .approval_policy
        .set(turn.approval_policy.value())
        .map_err(|err| {
            FunctionCallError::RespondToModel(format!("approval_policy is invalid: {err}"))
        })?;
    let turn_cwd = turn.cwd().clone();
    config.cwd = turn_cwd;
    Ok(())
}

/// Resolves role-locked model settings, explicit per-spawn overrides, and built-in defaults.
///
/// Provider catalogs may qualify the built-in model slug, so default resolution accepts a
/// provider prefix while explicit model
/// overrides continue to require an exact advertised model name. Role-locked values take
/// precedence; omitted settings fall back to the effective provider catalog and supported effort.
pub(crate) async fn apply_spawn_agent_model_defaults_and_overrides(
    session: &Session,
    turn: &TurnContext,
    config: &mut Config,
    requested_model: Option<&str>,
    requested_reasoning_effort: Option<ReasoningEffort>,
    role_locks: AgentRoleLocks,
) -> Result<(), FunctionCallError> {
    let models_manager = models_manager_for_spawn_config(session, turn, config);
    let selected_model_name = if role_locks.model {
        config.model.clone().ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "spawn_agent role did not resolve its configured model".to_string(),
            )
        })?
    } else {
        let available_models = models_manager
            .list_models_shared(RefreshStrategy::Offline, config.http_client_factory())
            .await
            .map_err(|error| {
                FunctionCallError::RespondToModel(format!(
                    "could not resolve the child model catalog: {error}"
                ))
            })?;
        match requested_model {
            Some(requested_model) => {
                find_spawn_agent_model_name(&available_models, requested_model)?
            }
            None => {
                find_default_spawn_agent_model_name(&available_models, config.model.as_deref())?
            }
        }
    };

    config.model = Some(selected_model_name.clone());
    let explicit_reasoning_effort = if role_locks.reasoning_effort {
        Some(config.model_reasoning_effort.clone().ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "spawn_agent role did not resolve its configured reasoning effort".to_string(),
            )
        })?)
    } else {
        requested_reasoning_effort
    };
    let selected_model_info = models_manager
        .get_model_info(&selected_model_name, &config.to_models_manager_config())
        .await;
    // Resolved catalog limits are not configuration overrides. Bounded forks
    // resolve their admission budget from the selected model at admission.
    let reasoning_effort = explicit_reasoning_effort.or_else(|| {
        default_spawn_reasoning_effort(
            &selected_model_info.supported_reasoning_levels,
            selected_model_info.default_reasoning_level.clone(),
        )
    });

    // Fallback metadata has no authoritative effort list. Preserve an explicit role-owned effort
    // for custom models while still validating every catalog-backed final pair.
    if let Some(reasoning_effort) = reasoning_effort.as_ref()
        && (!role_locks.reasoning_effort || !selected_model_info.used_fallback_model_metadata)
    {
        let wire_effort = crate::client::request_effort_for_model(
            &selected_model_info,
            Some(reasoning_effort.clone()),
        );
        crate::client::validate_request_effort_for_model(
            &selected_model_info,
            wire_effort.as_ref(),
        )
        .map_err(FunctionCallError::RespondToModel)?;
    }
    config.model_reasoning_effort = reasoning_effort;

    Ok(())
}

/// A child inherits the parent's service tier as the parent's next request
/// would use it. A settings update during the turn changes the session, not
/// the configuration captured when the turn started.
pub(crate) async fn inherit_live_service_tier(
    session: &Session,
    config: &mut Config,
) -> Option<String> {
    let live = session.configured_service_tier().await;
    config.service_tier = live.clone();
    live
}

/// An explicit `default` is a valid choice for every model: it selects the
/// default tier rather than inheriting one.
fn spawn_service_tier_is_supported(
    model_info: &codex_protocol::openai_models::ModelInfo,
    service_tier: &str,
) -> bool {
    service_tier == codex_protocol::config_types::SERVICE_TIER_DEFAULT_REQUEST_VALUE
        || model_info.supports_service_tier(service_tier)
}

pub(crate) async fn apply_spawn_agent_service_tier(
    session: &Session,
    turn: &TurnContext,
    config: &mut Config,
    parent_service_tier: Option<&str>,
    requested_service_tier: Option<&str>,
) -> Result<(), FunctionCallError> {
    let candidate_service_tiers = [
        config.service_tier.clone(),
        requested_service_tier.map(str::to_string),
        parent_service_tier.map(str::to_string),
    ];
    if candidate_service_tiers.iter().all(Option::is_none) {
        config.service_tier = None;
        return Ok(());
    }

    let model = config.model.clone().ok_or_else(|| {
        FunctionCallError::RespondToModel(
            "spawn_agent could not resolve the child model for service tier validation".to_string(),
        )
    })?;
    let models_manager = models_manager_for_spawn_config(session, turn, config);
    let model_info = models_manager
        .get_model_info(model.as_str(), &config.to_models_manager_config())
        .await;

    if let Some(requested_service_tier) = requested_service_tier
        && !spawn_service_tier_is_supported(&model_info, requested_service_tier)
    {
        let supported_service_tiers = if model_info.service_tiers.is_empty() {
            "none".to_string()
        } else {
            model_info
                .service_tiers
                .iter()
                .map(|tier| tier.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        return Err(FunctionCallError::RespondToModel(format!(
            "Service tier `{requested_service_tier}` is not supported for model `{model}`. Supported service tiers: {supported_service_tiers}"
        )));
    }

    config.service_tier =
        candidate_service_tiers
            .into_iter()
            .flatten()
            .find(|candidate_service_tier| {
                spawn_service_tier_is_supported(&model_info, candidate_service_tier.as_str())
            });
    Ok(())
}

fn models_manager_for_spawn_config(
    session: &Session,
    turn: &TurnContext,
    config: &Config,
) -> SharedModelsManager {
    if config.model_provider_id == turn.config.model_provider_id
        && config.model_provider == turn.config.model_provider
    {
        return session.services.models_manager.clone();
    }

    create_model_provider(config.model_provider.clone(), turn.auth_manager.clone()).models_manager(
        &config.model_provider_id,
        config.codex_home.to_path_buf(),
        config.model_catalog.clone(),
    )
}

fn find_spawn_agent_model_name(
    available_models: &[codex_protocol::openai_models::ModelPreset],
    requested_model: &str,
) -> Result<String, FunctionCallError> {
    available_models
        .iter()
        .find(|model| model.model == requested_model)
        .map(|model| model.model.clone())
        .ok_or_else(|| {
            let available = available_models
                .iter()
                .map(|model| model.model.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            FunctionCallError::RespondToModel(format!(
                "Unknown model `{requested_model}` for spawn_agent. Available models: {available}"
            ))
        })
}

fn find_default_spawn_agent_model_name(
    available_models: &[codex_protocol::openai_models::ModelPreset],
    effective_model: Option<&str>,
) -> Result<String, FunctionCallError> {
    if let Some(model) = available_models
        .iter()
        .find(|model| model.model == DEFAULT_SPAWN_AGENT_MODEL)
        .or_else(|| {
            available_models.iter().find(|model| {
                model
                    .model
                    .strip_suffix(DEFAULT_SPAWN_AGENT_MODEL)
                    .is_some_and(|prefix| {
                        prefix
                            .strip_suffix('.')
                            .is_some_and(|provider| !provider.is_empty())
                    })
            })
        })
    {
        return Ok(model.model.clone());
    }

    available_models
        .iter()
        .find(|model| Some(model.model.as_str()) == effective_model)
        .or_else(|| available_models.iter().find(|model| model.is_default))
        .or_else(|| available_models.first())
        .map(|model| model.model.clone())
        .ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "spawn_agent has no models in the effective provider catalog".to_string(),
            )
        })
}

fn default_spawn_reasoning_effort(
    supported: &[ReasoningEffortPreset],
    default: Option<ReasoningEffort>,
) -> Option<ReasoningEffort> {
    [Some(DEFAULT_SPAWN_AGENT_REASONING_EFFORT), default]
        .into_iter()
        .flatten()
        .find(|effort| supported.iter().any(|preset| &preset.effort == effort))
        .or_else(|| supported.first().map(|preset| preset.effort.clone()))
}


