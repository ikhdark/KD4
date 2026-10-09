use super::*;

pub(super) fn with_thread_spawn_agent_metadata(
    source: codex_protocol::protocol::SessionSource,
    agent_nickname: Option<String>,
    agent_role: Option<String>,
) -> codex_protocol::protocol::SessionSource {
    if agent_nickname.is_none() && agent_role.is_none() {
        return source;
    }

    match source {
        codex_protocol::protocol::SessionSource::SubAgent(
            codex_protocol::protocol::SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth,
                agent_path,
                agent_nickname: existing_agent_nickname,
                agent_role: existing_agent_role,
            },
        ) => codex_protocol::protocol::SessionSource::SubAgent(
            codex_protocol::protocol::SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth,
                agent_path,
                agent_nickname: agent_nickname.or(existing_agent_nickname),
                agent_role: agent_role.or(existing_agent_role),
            },
        ),
        _ => source,
    }
}

pub(crate) fn thread_response_active_permission_profile(
    active_permission_profile: Option<codex_protocol::models::ActivePermissionProfile>,
) -> Option<codex_app_server_protocol::ActivePermissionProfile> {
    active_permission_profile.map(Into::into)
}

pub(crate) fn thread_response_sandbox_policy(
    permission_profile: &codex_protocol::models::PermissionProfile,
    cwd: &Path,
) -> codex_app_server_protocol::SandboxPolicy {
    let sandbox_policy = codex_sandboxing::compatibility_sandbox_policy_for_permission_profile(
        permission_profile,
        cwd,
    );
    sandbox_policy.into()
}

pub(crate) fn thread_settings_from_config_snapshot(
    config_snapshot: &ThreadConfigSnapshot,
) -> ThreadSettings {
    ThreadSettings {
        cwd: config_snapshot.cwd().clone(),
        approval_policy: config_snapshot.approval_policy.into(),
        sandbox_policy: thread_response_sandbox_policy(
            &config_snapshot.permission_profile,
            config_snapshot.cwd().as_path(),
        ),
        permission_profile: Some(config_snapshot.permission_profile.clone()),
        active_permission_profile: thread_response_active_permission_profile(
            config_snapshot.active_permission_profile.clone(),
        ),
        model: config_snapshot.model.clone(),
        model_provider: config_snapshot.model_provider_id.clone(),
        service_tier: config_snapshot.service_tier.clone(),
        effort: config_snapshot.reasoning_effort.clone(),
        summary: config_snapshot.reasoning_summary,
        collaboration_mode: config_snapshot.collaboration_mode.clone(),
        personality: config_snapshot.personality,
    }
}

pub(crate) fn thread_settings_from_core_snapshot(
    snapshot: codex_protocol::protocol::ThreadSettingsSnapshot,
) -> ThreadSettings {
    let codex_protocol::protocol::ThreadSettingsSnapshot {
        model,
        model_provider_id,
        service_tier,
        approval_policy,
        permission_profile,
        active_permission_profile,
        cwd,
        reasoning_effort,
        reasoning_summary,
        personality,
        collaboration_mode,
        ..
    } = snapshot;
    let sandbox_policy = thread_response_sandbox_policy(&permission_profile, cwd.as_path());
    ThreadSettings {
        sandbox_policy,
        permission_profile: Some(permission_profile),
        cwd,
        approval_policy: approval_policy.into(),
        active_permission_profile: thread_response_active_permission_profile(
            active_permission_profile.flatten(),
        ),
        model,
        model_provider: model_provider_id,
        service_tier: service_tier.flatten(),
        effort: reasoning_effort.flatten(),
        summary: reasoning_summary.flatten(),
        collaboration_mode,
        personality: personality.flatten(),
    }
}

pub(super) fn thread_started_notification(thread: &mut Thread) -> ThreadStartedNotification {
    // Clone only metadata; the response keeps ownership of the restored history.
    let turns = std::mem::take(&mut thread.turns);
    let notification = ThreadStartedNotification {
        thread: thread.clone(),
    };
    thread.turns = turns;
    notification
}

#[cfg(test)]
#[path = "thread_summary_tests.rs"]
mod thread_summary_tests;
