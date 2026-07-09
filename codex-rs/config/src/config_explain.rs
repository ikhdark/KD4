//! Plain-English reference text for user-facing config inspection.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigOptionDoc {
    pub name: &'static str,
    pub group: &'static str,
    pub summary: &'static str,
}

pub const CONFIG_OPTION_DOCS: &[ConfigOptionDoc] = &[
    doc(
        "model",
        "Model and provider",
        "Default model used for new turns.",
    ),
    doc(
        "review_model",
        "Model and provider",
        "Model used by the `/review` workflow.",
    ),
    doc(
        "model_provider",
        "Model and provider",
        "Provider entry to use from the model provider map.",
    ),
    doc(
        "model_context_window",
        "Model and provider",
        "Context window size in tokens.",
    ),
    doc(
        "model_auto_compact_token_limit",
        "Model and provider",
        "Token threshold that triggers automatic history compaction.",
    ),
    doc(
        "model_auto_compact_token_limit_scope",
        "Model and provider",
        "Whether compaction counts the full context or only the body after the carried prefix.",
    ),
    doc(
        "model_providers",
        "Model and provider",
        "Custom provider entries; built-in provider IDs are reserved.",
    ),
    doc(
        "model_catalog_json",
        "Model and provider",
        "Path to a JSON model catalog loaded at startup.",
    ),
    doc(
        "oss_provider",
        "Model and provider",
        "Preferred local OSS provider, such as `lmstudio` or `ollama`.",
    ),
    doc(
        "openai_base_url",
        "Model and provider",
        "Base URL override for the built-in OpenAI provider.",
    ),
    doc(
        "chatgpt_base_url",
        "Model and provider",
        "Base URL override for ChatGPT-backed requests.",
    ),
    doc(
        "service_tier",
        "Model and provider",
        "Optional service tier request, such as `default`, `priority`, or `flex`.",
    ),
    doc(
        "model_reasoning_effort",
        "Reasoning and output",
        "Reasoning effort for normal model calls.",
    ),
    doc(
        "plan_mode_reasoning_effort",
        "Reasoning and output",
        "Reasoning effort to use while in plan mode.",
    ),
    doc(
        "model_reasoning_summary",
        "Reasoning and output",
        "Reasoning summary style: `auto`, `concise`, `detailed`, or `none`.",
    ),
    doc(
        "model_verbosity",
        "Reasoning and output",
        "GPT-5 output detail level: `low`, `medium`, or `high`.",
    ),
    doc(
        "model_supports_reasoning_summaries",
        "Reasoning and output",
        "Force-enable reasoning summaries for the configured model.",
    ),
    doc(
        "hide_agent_reasoning",
        "Reasoning and output",
        "Hide reasoning events from UI and output.",
    ),
    doc(
        "show_raw_agent_reasoning",
        "Reasoning and output",
        "Show raw reasoning-content events in UI and output.",
    ),
    doc(
        "tool_output_token_limit",
        "Reasoning and output",
        "Maximum model-facing tool output tokens retained per tool call.",
    ),
    doc(
        "approval_policy",
        "Approvals and sandbox",
        "Default policy for when command execution asks for approval.",
    ),
    doc(
        "approvals_reviewer",
        "Approvals and sandbox",
        "Where escalated approval requests are routed.",
    ),
    doc(
        "auto_review.policy",
        "Approvals and sandbox",
        "Extra policy text inserted into guardian auto-review prompts.",
    ),
    doc(
        "auto_review",
        "Approvals and sandbox",
        "Guardian auto-review policy settings.",
    ),
    doc(
        "sandbox_mode",
        "Approvals and sandbox",
        "Command sandbox level: `read-only`, `workspace-write`, or `danger-full-access`.",
    ),
    doc(
        "sandbox_workspace_write",
        "Approvals and sandbox",
        "Workspace-write sandbox details, such as writable roots and network access.",
    ),
    doc(
        "default_permissions",
        "Approvals and sandbox",
        "Default named permission profile.",
    ),
    doc(
        "permissions",
        "Approvals and sandbox",
        "Custom permission profiles keyed by name.",
    ),
    doc(
        "shell_environment_policy",
        "Approvals and sandbox",
        "Controls inherited, excluded, included, and forced environment variables for shell tools.",
    ),
    doc(
        "allow_login_shell",
        "Approvals and sandbox",
        "Whether shell tools may request a login shell.",
    ),
    doc(
        "notify",
        "User experience",
        "External command run for end-user notifications.",
    ),
    doc(
        "instructions",
        "Prompt context",
        "Custom system instructions.",
    ),
    doc(
        "developer_instructions",
        "Prompt context",
        "Custom developer-role instructions.",
    ),
    doc(
        "model_instructions_file",
        "Prompt context",
        "File that replaces the built-in model instructions.",
    ),
    doc(
        "compact_prompt",
        "Prompt context",
        "Prompt text used when compacting conversation history.",
    ),
    doc(
        "experimental_compact_prompt_file",
        "Prompt context",
        "File-based compact-prompt override.",
    ),
    doc(
        "include_permissions_instructions",
        "Prompt context",
        "Include the permissions developer block.",
    ),
    doc(
        "include_apps_instructions",
        "Prompt context",
        "Include the apps developer block.",
    ),
    doc(
        "include_collaboration_mode_instructions",
        "Prompt context",
        "Include the collaboration-mode developer block.",
    ),
    doc(
        "include_environment_context",
        "Prompt context",
        "Include the environment-context user block.",
    ),
    doc(
        "forced_chatgpt_workspace_id",
        "Auth and login",
        "Restrict ChatGPT login to one or more workspace IDs.",
    ),
    doc(
        "forced_login_method",
        "Auth and login",
        "Force login through `chatgpt` or `api`.",
    ),
    doc(
        "cli_auth_credentials_store",
        "Auth and login",
        "Where CLI auth credentials are stored: `file`, `keyring`, or `auto`.",
    ),
    doc(
        "mcp_oauth_credentials_store",
        "Auth and login",
        "Where MCP OAuth credentials are stored.",
    ),
    doc(
        "mcp_oauth_callback_port",
        "Auth and login",
        "Fixed local OAuth callback port for MCP login.",
    ),
    doc(
        "mcp_oauth_callback_url",
        "Auth and login",
        "OAuth redirect URL override for MCP login.",
    ),
    doc(
        "mcp_servers",
        "Tools, apps, and plugins",
        "External MCP servers Codex can use for tools.",
    ),
    doc(
        "apps_mcp_product_sku",
        "Tools, apps, and plugins",
        "Product SKU forwarded on host-owned app MCP requests.",
    ),
    doc(
        "apps",
        "Tools, apps, and plugins",
        "Connector/app defaults and per-app tool approval settings.",
    ),
    doc(
        "plugins",
        "Tools, apps, and plugins",
        "User plugin settings keyed by plugin name.",
    ),
    doc(
        "marketplaces",
        "Tools, apps, and plugins",
        "Marketplace settings keyed by marketplace name.",
    ),
    doc(
        "skills",
        "Tools, apps, and plugins",
        "Skill discovery, instruction, and enablement settings.",
    ),
    doc(
        "tool_suggest",
        "Tools, apps, and plugins",
        "Extra installable plugins/connectors to suggest, plus disabled suggestions.",
    ),
    doc(
        "tools.web_search",
        "Tools, apps, and plugins",
        "Web search tool details such as context size, domains, and location.",
    ),
    doc(
        "tools.experimental_request_user_input",
        "Tools, apps, and plugins",
        "Enables the request-user-input tool surface.",
    ),
    doc(
        "tools",
        "Tools, apps, and plugins",
        "Nested tool feature toggles and tool-specific settings.",
    ),
    doc(
        "background_terminal_max_timeout",
        "Tools, apps, and plugins",
        "Maximum allowed timeout for background terminal commands.",
    ),
    doc(
        "hooks",
        "Tools, apps, and plugins",
        "Lifecycle hooks configured inline in config.toml.",
    ),
    doc(
        "profile",
        "Project and profile",
        "Selected named profile from the profiles map.",
    ),
    doc(
        "profiles",
        "Project and profile",
        "Named config profiles for switching settings.",
    ),
    doc(
        "projects",
        "Project and profile",
        "Per-project settings such as trust level.",
    ),
    doc(
        "project_doc_max_bytes",
        "Project and profile",
        "Maximum bytes read from an `AGENTS.md` project doc.",
    ),
    doc(
        "project_doc_fallback_filenames",
        "Project and profile",
        "Fallback project instruction filenames when `AGENTS.md` is missing.",
    ),
    doc(
        "project_root_markers",
        "Project and profile",
        "Markers used to find project roots for `.codex` discovery.",
    ),
    doc(
        "history",
        "Persistence and logs",
        "Controls history file persistence and maximum size.",
    ),
    doc(
        "sqlite_home",
        "Persistence and logs",
        "Directory for the Codex SQLite state database.",
    ),
    doc(
        "log_dir",
        "Persistence and logs",
        "Directory for logs; setting it explicitly enables TUI text logs.",
    ),
    doc(
        "debug.config_lockfile",
        "Persistence and logs",
        "Exports or replays effective config lockfiles for debugging.",
    ),
    doc(
        "file_opener",
        "User experience",
        "URI opener for file links, such as VS Code, Windsurf, Cursor, or none.",
    ),
    doc("tui", "User experience", "Terminal UI settings."),
    doc(
        "personality",
        "User experience",
        "Model personality: `none`, `friendly`, or `pragmatic`.",
    ),
    doc(
        "check_for_update_on_startup",
        "User experience",
        "Whether Codex checks for updates on startup.",
    ),
    doc(
        "disable_paste_burst",
        "User experience",
        "Disable burst-paste buffering for typed input.",
    ),
    doc("notice", "User experience", "In-product notices."),
    doc(
        "desktop",
        "User experience",
        "Opaque desktop-app settings stored in config TOML.",
    ),
    doc(
        "web_search",
        "Search, memory, and agents",
        "Global web search mode: `disabled`, `cached`, or `live`.",
    ),
    doc(
        "memories",
        "Search, memory, and agents",
        "Memory generation, injection, limits, and consolidation models.",
    ),
    doc(
        "agents",
        "Search, memory, and agents",
        "Agent thread limits, nesting, runtime, interrupt messages, and roles.",
    ),
    doc(
        "audio",
        "Realtime",
        "Preferred microphone and speaker for realtime voice.",
    ),
    doc(
        "realtime",
        "Realtime",
        "Realtime architecture, version, session type, transport, and voice.",
    ),
    doc(
        "experimental_realtime_ws_base_url",
        "Realtime",
        "Realtime websocket base URL override.",
    ),
    doc(
        "experimental_realtime_webrtc_call_base_url",
        "Realtime",
        "WebRTC call creation URL override.",
    ),
    doc(
        "experimental_realtime_ws_model",
        "Realtime",
        "Realtime websocket model override.",
    ),
    doc(
        "experimental_realtime_ws_backend_prompt",
        "Realtime",
        "Realtime websocket backend prompt override.",
    ),
    doc(
        "experimental_realtime_ws_startup_context",
        "Realtime",
        "Realtime websocket startup context override.",
    ),
    doc(
        "experimental_realtime_start_instructions",
        "Realtime",
        "Realtime start-instructions override.",
    ),
    doc(
        "analytics",
        "Telemetry and features",
        "Analytics enablement.",
    ),
    doc(
        "feedback",
        "Telemetry and features",
        "Feedback collection enablement.",
    ),
    doc(
        "debug",
        "Telemetry and features",
        "Debugging and reproducibility settings.",
    ),
    doc(
        "otel",
        "Telemetry and features",
        "OpenTelemetry exporters, environment, prompt logging, span attributes, and tracestate.",
    ),
    doc(
        "features",
        "Telemetry and features",
        "Central feature flags.",
    ),
    doc(
        "suppress_unstable_features_warning",
        "Telemetry and features",
        "Suppress warnings about unstable features.",
    ),
    doc("windows", "Windows", "Windows-specific sandbox settings."),
    doc(
        "experimental_thread_config_endpoint",
        "Experimental and compatibility",
        "Remote endpoint for thread-scoped config.",
    ),
    doc(
        "experimental_thread_store",
        "Experimental and compatibility",
        "Thread store implementation selector.",
    ),
    doc(
        "experimental_thread_store_endpoint",
        "Experimental and compatibility",
        "Removed thread-store endpoint setting kept to fail fast.",
    ),
    doc(
        "experimental_use_unified_exec_tool",
        "Experimental and compatibility",
        "Experimental unified exec tool toggle.",
    ),
    doc(
        "ghost_snapshot",
        "Experimental and compatibility",
        "Legacy no-op settings retained so old config files still load.",
    ),
    doc(
        "js_repl_node_path",
        "Experimental and compatibility",
        "Deprecated and ignored JavaScript REPL node path.",
    ),
    doc(
        "js_repl_node_module_dirs",
        "Experimental and compatibility",
        "Deprecated and ignored JavaScript REPL module directories.",
    ),
];

const fn doc(name: &'static str, group: &'static str, summary: &'static str) -> ConfigOptionDoc {
    ConfigOptionDoc {
        name,
        group,
        summary,
    }
}

pub fn render_config_explain(filter: Option<&str>) -> String {
    let filter = filter.map(str::trim).filter(|value| !value.is_empty());
    let docs = matching_docs(filter);
    if docs.is_empty() {
        let filter = filter.unwrap_or_default();
        return format!(
            "No config options matched `{filter}`.\nTry `codex config explain` to list all known options."
        );
    }

    let mut output = String::new();
    output.push_str("Codex config options\n");
    output.push_str("Set these in config.toml unless a command-line override is documented.\n");

    let mut current_group = "";
    for doc in docs {
        if doc.group != current_group {
            current_group = doc.group;
            output.push('\n');
            output.push_str(current_group);
            output.push('\n');
        }
        output.push_str("- ");
        output.push_str(doc.name);
        output.push_str(": ");
        output.push_str(doc.summary);
        output.push('\n');
    }

    output
}

fn matching_docs(filter: Option<&str>) -> Vec<ConfigOptionDoc> {
    let Some(filter) = filter else {
        return CONFIG_OPTION_DOCS.to_vec();
    };
    let needle = filter.to_ascii_lowercase();
    CONFIG_OPTION_DOCS
        .iter()
        .copied()
        .filter(|doc| {
            doc.name.to_ascii_lowercase().contains(&needle)
                || doc.group.to_ascii_lowercase().contains(&needle)
                || doc.summary.to_ascii_lowercase().contains(&needle)
        })
        .collect()
}

#[cfg(test)]
#[path = "config_explain_tests.rs"]
mod tests;
