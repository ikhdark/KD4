//! Agent-role lookup shared by spawn paths.
//!
//! New subagents use the generic runtime profile; role configs are not applied as config layers.
//! This module owns the default role name, the role-owned settings locks consumed by spawn
//! defaults, and role lookup for metadata such as nickname candidates. It does not decide when to
//! spawn a sub-agent; the multi-agent tool handler owns that orchestration.

use crate::config::AgentRoleConfig;
use crate::config::Config;
use std::collections::BTreeMap;
use std::sync::LazyLock;

/// The role name used when a caller omits `agent_type`.
pub const DEFAULT_ROLE_NAME: &str = "default";

/// Settings that a role explicitly owns and that spawn defaults must not replace.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct AgentRoleLocks {
    pub(crate) model: bool,
    pub(crate) reasoning_effort: bool,
    pub(crate) permissions: bool,
    pub(crate) allow_login_shell: bool,
    pub(crate) shell_environment_policy: bool,
    pub(crate) windows_sandbox: bool,
    pub(crate) windows_private_desktop: bool,
}

pub(crate) fn resolve_role_config<'a>(
    config: &'a Config,
    role_name: &str,
) -> Option<&'a AgentRoleConfig> {
    config
        .agent_roles
        .get(role_name)
        .or_else(|| built_in::configs().get(role_name))
}

mod built_in {
    use super::*;

    /// Only the generic profile is available for new subagents. Historical task
    /// roles remain persisted task metadata, not selectable runtime profiles.
    pub(super) fn configs() -> &'static BTreeMap<String, AgentRoleConfig> {
        static CONFIG: LazyLock<BTreeMap<String, AgentRoleConfig>> = LazyLock::new(|| {
            BTreeMap::from([(
                DEFAULT_ROLE_NAME.to_string(),
                AgentRoleConfig {
                    description: Some("Default agent.".to_string()),
                    config_file: None,
                    nickname_candidates: None,
                },
            )])
        });
        &CONFIG
    }
}
