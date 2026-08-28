use std::collections::HashMap;

use codex_app_server_protocol::ApprovalsReviewer;
use codex_app_server_protocol::SandboxMode;
use codex_app_server_protocol::ThreadForkParams;
use codex_app_server_protocol::ThreadResumeParams;
use codex_app_server_protocol::ThreadSource;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartSource;
use codex_core::config::Config;
use codex_protocol::config_types::SERVICE_TIER_DEFAULT_REQUEST_VALUE;
use codex_protocol::models::PermissionProfile;
use serde_json::Value;

/// Caller-owned values that vary between embedded and remote app-server transports.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ThreadLifecycleOverrides {
    pub model_provider: Option<String>,
    pub cwd: Option<String>,
    pub permissions: Option<String>,
    pub developer_instructions: Option<String>,
    pub exclude_turns: bool,
}

/// Builds the common thread/start request from the resolved runtime configuration.
pub fn thread_start_params_from_config(
    config: &Config,
    overrides: ThreadLifecycleOverrides,
    session_start_source: Option<ThreadStartSource>,
) -> ThreadStartParams {
    ThreadStartParams {
        model: config.model.clone(),
        model_provider: overrides.model_provider,
        service_tier: service_tier_override_from_config(config),
        cwd: overrides.cwd,
        runtime_workspace_roots: Some(config.workspace_roots.clone()),
        approval_policy: Some(config.permissions.approval_policy.value().into()),
        approvals_reviewer: Some(config.approvals_reviewer.into()),
        sandbox: sandbox_override_from_config(config, overrides.permissions.as_ref()),
        permission_profile: permission_profile_override_from_config(
            config,
            overrides.permissions.as_ref(),
        ),
        permissions: overrides.permissions,
        config: thread_config_overrides_from_config(config),
        developer_instructions: overrides.developer_instructions,
        ephemeral: Some(config.ephemeral),
        session_start_source,
        thread_source: Some(ThreadSource::User),
        ..ThreadStartParams::default()
    }
}

/// Builds the common thread/resume request.
///
/// Persisted reviewer selection remains authoritative unless the caller supplies
/// an explicit override for this resume operation.
pub fn thread_resume_params_from_config(
    config: &Config,
    thread_id: String,
    overrides: ThreadLifecycleOverrides,
    approvals_reviewer_override: Option<ApprovalsReviewer>,
) -> ThreadResumeParams {
    ThreadResumeParams {
        thread_id,
        model: config.model.clone(),
        model_provider: overrides.model_provider,
        service_tier: service_tier_override_from_config(config),
        cwd: overrides.cwd,
        runtime_workspace_roots: Some(config.workspace_roots.clone()),
        approval_policy: Some(config.permissions.approval_policy.value().into()),
        approvals_reviewer: approvals_reviewer_override,
        sandbox: sandbox_override_from_config(config, overrides.permissions.as_ref()),
        permission_profile: permission_profile_override_from_config(
            config,
            overrides.permissions.as_ref(),
        ),
        permissions: overrides.permissions,
        config: thread_config_overrides_from_config(config),
        developer_instructions: overrides.developer_instructions,
        exclude_turns: overrides.exclude_turns,
        ..ThreadResumeParams::default()
    }
}

/// Builds the common thread/fork request from the resolved runtime configuration.
pub fn thread_fork_params_from_config(
    config: &Config,
    thread_id: String,
    overrides: ThreadLifecycleOverrides,
) -> ThreadForkParams {
    ThreadForkParams {
        thread_id,
        model: config.model.clone(),
        model_provider: overrides.model_provider,
        service_tier: service_tier_override_from_config(config),
        cwd: overrides.cwd,
        runtime_workspace_roots: Some(config.workspace_roots.clone()),
        approval_policy: Some(config.permissions.approval_policy.value().into()),
        approvals_reviewer: Some(config.approvals_reviewer.into()),
        sandbox: sandbox_override_from_config(config, overrides.permissions.as_ref()),
        permission_profile: permission_profile_override_from_config(
            config,
            overrides.permissions.as_ref(),
        ),
        permissions: overrides.permissions,
        config: thread_config_overrides_from_config(config),
        base_instructions: config.base_instructions.clone(),
        developer_instructions: overrides.developer_instructions,
        ephemeral: config.ephemeral,
        thread_source: Some(ThreadSource::User),
        exclude_turns: overrides.exclude_turns,
        ..ThreadForkParams::default()
    }
}

pub fn thread_config_overrides_from_config(config: &Config) -> Option<HashMap<String, Value>> {
    let mut overrides = HashMap::new();
    let mut insert = |key: &str, value: Option<String>| {
        if let Some(value) = value {
            overrides.insert(key.to_string(), Value::String(value));
        }
    };
    insert(
        "model_reasoning_effort",
        config
            .model_reasoning_effort
            .as_ref()
            .map(std::string::ToString::to_string),
    );
    insert(
        "model_reasoning_summary",
        config
            .model_reasoning_summary
            .map(|summary| summary.to_string()),
    );
    insert(
        "model_verbosity",
        config
            .model_verbosity
            .map(|verbosity| verbosity.to_string()),
    );
    insert(
        "personality",
        config
            .personality
            .map(|personality| personality.to_string()),
    );
    insert(
        "web_search",
        Some(config.web_search_mode.value().to_string()),
    );
    if config.bypass_hook_trust {
        overrides.insert("bypass_hook_trust".to_string(), Value::Bool(true));
    }
    Some(overrides)
}

fn service_tier_override_from_config(config: &Config) -> Option<Option<String>> {
    config.service_tier.clone().map(Some).or_else(|| {
        (config.notices.fast_default_opt_out == Some(true))
            .then(|| Some(SERVICE_TIER_DEFAULT_REQUEST_VALUE.to_string()))
    })
}

fn sandbox_override_from_config(
    config: &Config,
    permissions: Option<&String>,
) -> Option<SandboxMode> {
    if permissions.is_some() {
        None
    } else {
        SandboxMode::from_permission_profile(
            &config.permissions.effective_permission_profile(),
            config.cwd.as_path(),
        )
    }
}

fn permission_profile_override_from_config(
    config: &Config,
    permissions: Option<&String>,
) -> Option<PermissionProfile> {
    permissions
        .is_none()
        .then(|| config.permissions.effective_permission_profile())
}
