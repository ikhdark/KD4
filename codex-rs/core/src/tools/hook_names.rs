//! Hook-facing tool names and matcher compatibility aliases.
//!
//! Hook stdin exposes one canonical `tool_name`, but matcher selection may also
//! need to recognize names from adjacent tool ecosystems. Keeping those two
//! concepts together prevents handlers from accidentally serializing a
//! compatibility alias, such as `Write`, as the stable hook payload name.

/// Identifies a tool in hook payloads and hook matcher selection.
///
/// `name` is the canonical value serialized into hook stdin. Matcher aliases are
/// internal-only compatibility names that may select the same hook handlers but
/// must not change the payload seen by hook processes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HookToolName {
    name: String,
    matcher_aliases: Vec<String>,
    mcp_identity: Option<(String, String)>,
}

impl HookToolName {
    /// Builds a hook tool name with no matcher aliases.
    pub(crate) fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            matcher_aliases: Vec::new(),
            mcp_identity: None,
        }
    }

    /// Use routing identity, not the lossy/model-specific callable aliases, at
    /// every hook stage. Only unambiguous legacy names retain their spelling.
    pub(crate) fn mcp(server: &str, tool: &str) -> Self {
        fn legacy_component(value: &str) -> bool {
            !value.is_empty()
                && !value.starts_with('_')
                && !value.ends_with('_')
                && !value.contains("__")
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        }
        let name = if legacy_component(server) && legacy_component(tool) {
            format!("mcp__{server}__{tool}")
        } else {
            // The extra separator cannot occur in an admitted legacy pair.
            // Hex is reversible and cannot introduce delimiter ambiguity.
            format!("mcp__v1__{}__{}", hex::encode(server), hex::encode(tool))
        };
        Self {
            name,
            matcher_aliases: Vec::new(),
            mcp_identity: Some((server.to_string(), tool.to_string())),
        }
    }

    /// Returns the hook identity for file edits performed through `apply_patch`.
    ///
    /// The serialized name remains `apply_patch` so logs and policies can key
    /// off the actual Codex tool. `Write` and `Edit` are accepted as matcher
    /// aliases for compatibility with hook configurations that describe edits
    /// using Claude Code-style names.
    pub(crate) fn apply_patch() -> Self {
        Self {
            name: "apply_patch".to_string(),
            matcher_aliases: vec!["Write".to_string(), "Edit".to_string()],
            mcp_identity: None,
        }
    }

    /// Returns the hook identity for spawning sub-agents.
    ///
    /// The serialized name remains `spawn_agent`, while `Agent` is accepted as
    /// a matcher alias for compatibility with hook configurations that describe
    /// sub-agent creation using Claude Code-style names.
    pub(crate) fn spawn_agent() -> Self {
        Self {
            name: "spawn_agent".to_string(),
            matcher_aliases: vec!["Agent".to_string()],
            mcp_identity: None,
        }
    }

    /// Returns the hook identity historically used for shell-like tools.
    pub(crate) fn bash() -> Self {
        Self::new("Bash")
    }

    /// Returns the stable hook identity for `shell_command`.
    ///
    /// Shell hooks historically receive `Bash` in hook stdin. Keep that
    /// serialized contract while accepting the concrete Codex tool name as an
    /// additional matcher input.
    pub(crate) fn shell_command() -> Self {
        Self {
            name: "Bash".to_string(),
            matcher_aliases: vec!["shell_command".to_string()],
            mcp_identity: None,
        }
    }

    /// Returns the stable hook identity for `exec_command`.
    ///
    /// Unified exec is another shell transport, so preserve the same `Bash`
    /// payload contract and expose the concrete tool name only as a matcher
    /// alias.
    pub(crate) fn exec_command() -> Self {
        Self {
            name: "Bash".to_string(),
            matcher_aliases: vec!["exec_command".to_string()],
            mcp_identity: None,
        }
    }

    /// Returns the canonical hook name serialized into hook stdin.
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// Returns additional matcher inputs that should select the same handlers.
    pub(crate) fn matcher_aliases(&self) -> &[String] {
        &self.matcher_aliases
    }

    pub(crate) fn mcp_server_name(&self) -> Option<String> {
        self.mcp_identity.as_ref().map(|(server, _)| server.clone())
    }

    pub(crate) fn mcp_tool_name(&self) -> Option<String> {
        self.mcp_identity.as_ref().map(|(_, tool)| tool.clone())
    }
}

