#![recursion_limit = "256"]
#![deny(clippy::print_stdout, clippy::print_stderr)]

use codex_app_server_config::{config_manager, config_manager_service};
use codex_app_server_config::config_warnings::exec_policy_config_warning;
use codex_app_server_rpc::{connection_rpc_gate, error_code, outgoing_message, request_serialization, server_request_error};
use codex_app_server_transport as transport;

mod app_server_tracing;
mod app_info;
mod attestation;
mod bespoke_event_handling;
mod build_info;
mod dynamic_tools;
mod extensions;
mod external_auth;
mod filters;
mod fs_watch;
mod fuzzy_file_search;
mod image_url;
mod mcp_refresh;
mod message_processor;
mod models;
mod models_refresh_worker;
mod request_processors;
mod runtime_provenance;
mod skills_watcher;
mod thread_state;
mod thread_status;

pub use message_processor::{MessageProcessor, MessageProcessorArgs};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginStartupTasks {
    Start,
    Skip,
}
