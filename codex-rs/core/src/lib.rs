//! Root of the `codex-core` library.

// Prevent accidental direct writes to stdout/stderr in library code. All
// user-visible output must go through the appropriate abstraction (e.g.,
// the TUI or the tracing stack).
#![deny(clippy::print_stdout, clippy::print_stderr)]

mod apply_patch;
mod apps;
mod client;
mod client_common;
mod responses_metadata;
mod responses_retry;
pub(crate) mod session;
pub use responses_metadata::CodexResponsesMetadata;
pub use session::SteerInputError;
pub use turn_metadata::detached_memory_responses_metadata;
mod codex_thread;
mod compact_model_fallback;
mod compact_remote;
mod compact_remote_v2;
mod config_lock;
pub use codex_thread::BackgroundTerminalInfo;
pub use codex_thread::CodexThread;
pub use codex_thread::CodexThreadSettingsOverrides;
pub use codex_thread::ThreadConfigSnapshot;
pub use codex_thread::TryStartTurnIfIdleError;
pub use codex_thread::TryStartTurnIfIdleRejectionReason;
pub use elicitation::OutOfBandElicitationLeaseId;
pub use session::turn_context::TurnContext;
mod agent;
mod agent_communication;
mod attestation;
mod codex_delegate;
mod command_canonicalization;
pub mod config;
pub mod connectors;
pub mod context;
mod context_manager;
mod continuity;
mod current_time;
mod elicitation;
mod environment_selection;
pub mod exec;
pub mod exec_env;
mod exec_policy;
#[cfg(test)]
mod git_info_tests;
mod git_workspace;
mod guardian;
mod hook_runtime;
mod image_preparation;
mod installation_id;
pub(crate) mod mcp;
mod mcp_skill_dependencies;
mod mcp_tool_approval_templates;
mod mcp_tool_exposure;
mod network_policy_decision;
pub(crate) mod network_proxy_loader;
pub use codex_mcp::SandboxState;
pub use mcp::McpManager;
pub use network_proxy_loader::MtimeConfigReloader;
pub use network_proxy_loader::build_network_proxy_state;
pub use network_proxy_loader::build_network_proxy_state_and_reloader;
mod mcp_openai_file;
mod mcp_tool_call;
pub use codex_plugin::mention_syntax::PLUGIN_TEXT_MENTION_SIGIL;
pub use codex_plugin::mention_syntax::TOOL_MENTION_SIGIL;
pub(crate) mod plugins;
#[doc(hidden)]
pub(crate) mod prompt_debug;
#[doc(hidden)]
pub use prompt_debug::build_prompt_input;
pub(crate) mod mentions {
    pub(crate) use crate::plugins::build_connector_slug_counts;
    pub(crate) use crate::plugins::build_skill_name_counts;
    pub(crate) use crate::plugins::collect_explicit_app_ids;
    pub(crate) use crate::plugins::collect_explicit_plugin_mentions;
    pub(crate) use crate::plugins::collect_tool_mentions_from_messages;
}
mod sandbox_tags;
pub mod sandboxing;
mod session_prefix;
mod session_startup_prewarm;
pub mod skills;
pub(crate) use skills::SkillMetadata;
pub(crate) use skills::SkillsService;
pub(crate) use skills::apply_skill_injection_observability;
pub(crate) use skills::build_available_skills;
pub(crate) use skills::build_skill_name_counts;
pub(crate) use skills::collect_explicit_skill_mentions;
pub(crate) use skills::default_skill_metadata_budget;
pub(crate) use skills::injection;
pub(crate) use skills::maybe_emit_implicit_skill_invocation;
pub(crate) use skills::plan_skill_injections;
pub(crate) use skills::skills_load_input_from_config;
mod stream_events_utils;
pub use stream_events_utils::image_generation_artifact_path;
mod stable_context;
mod startup_timing;
pub mod test_support;
mod unified_exec;
pub mod windows_sandbox;
pub use client::X_RESPONSESAPI_INCLUDE_TIMING_METRICS_HEADER;
pub use codex_protocol::config_types::ModelProviderAuthInfo;
pub use codex_protocol::models::web_search_action_detail;
pub use codex_protocol::models::web_search_detail;
pub use codex_rollout::state_integration::StateDbHandle;
mod event_mapping;
pub use codex_prompts as review_prompts;
mod thread_manager;
pub(crate) mod windows_sandbox_read_grants;
pub use thread_manager::ForkSnapshot;
pub use thread_manager::NewThread;
pub use thread_manager::StartThreadOptions;
pub use thread_manager::ThreadCreatedThreadGuard;
pub use thread_manager::ThreadManager;
pub use thread_manager::ThreadSettingsReconstruction;
pub use thread_manager::ThreadShutdownReport;
pub use thread_manager::build_models_manager;
pub use thread_manager::local_agent_graph_store_from_state_db;
pub use thread_manager::thread_store_from_config;
pub use windows_sandbox_read_grants::grant_read_root_non_elevated;
pub(crate) mod agents_md;
mod agents_md_manager;
pub use agents_md::DEFAULT_AGENTS_MD_FILENAME;
pub use agents_md::LOCAL_AGENTS_MD_FILENAME;
pub use agents_md::LoadedAgentsMd;
pub use agents_md::project_doc_candidate_filenames;
mod rollout;
pub(crate) mod safety;
mod session_rollout_init_error;
pub mod shell;
pub(crate) mod shell_snapshot;
pub mod spawn;
mod thread_rollout_truncation;
pub use thread_rollout_truncation::truncate_rollout_after_turn_id;
pub(crate) mod task_evidence;
mod tool_history;
mod tools;
pub(crate) mod turn_diff_tracker;
mod turn_metadata;
mod turn_timing;
mod validation_admission;
mod workspace_operation_gate;
pub(crate) use codex_tools::FunctionCallError;

pub async fn init_state_db(config: &config::Config) -> Option<StateDbHandle> {
    codex_rollout::state_integration::init(config).await
}
pub use rollout::ARCHIVED_SESSIONS_SUBDIR;
pub use rollout::Cursor;
pub use rollout::INTERACTIVE_SESSION_SOURCES;
pub use rollout::RolloutRecorder;
pub use rollout::RolloutRecorderParams;
pub use rollout::SESSIONS_SUBDIR;
pub use rollout::SessionMeta;
pub use rollout::SortDirection;
pub use rollout::ThreadItem;
pub use rollout::ThreadSortKey;
pub use rollout::ThreadsPage;
pub use rollout::append_thread_name;
pub use rollout::find_archived_thread_path_by_id_str;
pub use rollout::find_thread_meta_by_name_str;
pub use rollout::find_thread_name_by_id;
pub use rollout::find_thread_names_by_ids;
pub use rollout::find_thread_path_by_id_str;
pub use rollout::parse_cursor;
pub use rollout::read_head_for_summary;
pub use rollout::read_session_meta_line;
pub use rollout::rollout_date_parts;
mod feedback;
mod invariants;
mod plan_store;
pub mod retry;
mod state;
mod tasks;
mod terminal_event;
mod user_shell_command;
pub use terminal_event::terminal_event_fingerprint;

pub use attestation::AttestationContext;
pub use attestation::AttestationProvider;
pub use attestation::GenerateAttestationFuture;
pub use client::ModelClient;
pub use client::ModelClientSession;
pub use client::X_CODEX_INSTALLATION_ID_HEADER;
pub use client::X_CODEX_TURN_METADATA_HEADER;
pub use client_common::Prompt;
pub use client_common::ResponseEvent;
pub use client_common::ResponseStream;
pub use codex_prompts::REVIEW_PROMPT;
pub use compact::content_items_to_text;
pub use current_time::SleepFuture;
pub use current_time::TimeFuture;
pub use current_time::TimeProvider;
pub use event_mapping::parse_turn_item;
pub use exec_policy::ExecPolicyError;
pub use exec_policy::check_execpolicy_for_warnings;
pub use exec_policy::format_exec_policy_error_with_source;
pub use exec_policy::load_exec_policy;
pub use installation_id::resolve_installation_id;
pub mod compact;
mod memory_usage;
pub mod otel_init;

#[cfg(test)]
mod completed_migration_tests {
    #[test]
    fn conversation_to_thread_rename_has_no_legacy_public_exports() {
        let core_lib = include_str!("lib.rs");
        let core_rollout = include_str!("rollout.rs");
        let rollout_lib = include_str!("../../rollout/src/lib.rs");

        for name_parts in [
            ["Conversation", "Manager"],
            ["New", "Conversation"],
            ["Codex", "Conversation"],
        ] {
            let obsolete_alias = name_parts.concat();
            assert!(
                !core_lib.contains(&format!("pub type {obsolete_alias} =")),
                "the completed conversation-to-thread rename must not retain {obsolete_alias}"
            );
        }

        let obsolete_lookup = ["find_", "conversation_path_by_id_str"].concat();
        assert!(
            !core_lib.contains(&format!("pub use rollout::{obsolete_lookup};")),
            "codex-core must not re-export the obsolete conversation lookup"
        );
        assert!(
            !core_rollout.contains(&format!("pub use codex_rollout::{obsolete_lookup};")),
            "the core rollout facade must not re-export the obsolete conversation lookup"
        );
        assert!(
            !rollout_lib.contains(&format!(" as {obsolete_lookup};")),
            "codex-rollout must not retain the obsolete conversation lookup alias"
        );
    }
}

#[cfg(test)]
mod tiny_module_collapse_tests {
    #[test]
    fn extracted_owners_do_not_keep_forwarding_modules() {
        let core_lib = include_str!("lib.rs");
        let context_mod = include_str!("context/mod.rs");

        for obsolete_module in [
            "function_tool",
            "original_image_detail",
            "mention_syntax",
            "state_db_bridge",
            "utils",
            "web_search",
        ] {
            let obsolete_declaration = format!("mod {obsolete_module};");
            assert!(
                !core_lib.contains(&obsolete_declaration),
                "core must not retain the forwarding module {obsolete_declaration}"
            );
        }
        let permissions_forwarder = ["mod permissions_", "instructions;"].concat();
        assert!(
            !context_mod.contains(&permissions_forwarder),
            "context must re-export permission prompt types from their owner directly"
        );
    }
}
