// Aggregates all former standalone integration tests as modules.
use codex_apply_patch::CODEX_CORE_APPLY_PATCH_ARG1;
use codex_exec_server::CODEX_FS_HELPER_ARG1;
use codex_test_binary_support::TestBinaryDispatchGuard;
use codex_test_binary_support::TestBinaryDispatchMode;
use codex_test_binary_support::configure_test_binary_dispatch;
use ctor::ctor;

// This code runs before any other tests are run.
// It allows the test binary to dispatch to the bundled helper entrypoints.
// NOTE: this doesn't work on ARM
#[ctor(unsafe)]
pub static CODEX_ALIASES_TEMP_DIR: Option<TestBinaryDispatchGuard> = {
    configure_test_binary_dispatch("codex-core-tests", |_exe_name, argv1| {
        if argv1 == Some(CODEX_CORE_APPLY_PATCH_ARG1) {
            return TestBinaryDispatchMode::DispatchArg0Only;
        }
        if argv1 == Some(CODEX_FS_HELPER_ARG1) {
            return TestBinaryDispatchMode::DispatchArg0Only;
        }
        TestBinaryDispatchMode::InstallAliases
    })
};

mod additional_context;
mod agent_execution;
mod agent_jobs;
mod agent_websocket;
mod agents_md;
mod apply_patch_cli;
mod approvals;
mod auto_review;
mod cli_stream;
mod client;
mod client_websockets;
mod code_mode;
mod code_mode_elicitation;
mod codex_delegate;
mod collaboration_instructions;
mod compact;
mod compact_remote;
mod compact_resume_fork;
mod completion_review;
mod current_time_reminder;
mod deprecation_notice;
mod exec_policy;
mod extension_sandbox;
mod external_auth;
mod fork_thread;

mod hooks_windows;
mod image_rollout;
mod investigation_evidence_schema;
mod live_cli;
mod mcp_auth_elicitation;
mod mcp_auth_refresh;
mod mcp_refresh_cleanup;
mod mcp_tool_exposure;
mod model_overrides;
mod model_runtime_selectors;
mod model_switching;
mod model_visible_layout;
mod models_cache_ttl;
mod multi_agent_mode;
mod otel;
mod override_updates;
mod pending_input;
mod permissions_messages;
mod personality;
mod prompt_caching;
mod prompt_debug_tests;
mod quota_exceeded;
mod remote_env;
mod request_permissions;
mod request_user_input;
mod responses_api_proxy_headers;
mod responses_lite;
mod resume;
mod resume_warning;
mod review;
mod rmcp_client;
mod rollout_list_find;
mod safety_buffering;
mod safety_check_downgrade;
mod shell_command;
mod shell_snapshot;
mod sqlite_state;
mod stream_error_allows_next_turn;
mod stream_no_completed;
mod subagent_notifications;
mod turn_state;
mod unified_exec;
mod unified_exec_process_events;

mod user_shell_cmd;
mod web_search;
mod websocket_fallback;
mod window_headers;

mod windows_sandbox;
