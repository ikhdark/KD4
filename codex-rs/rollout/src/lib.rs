//! Rollout persistence and discovery for Codex session files.

use std::sync::LazyLock;

use codex_protocol::protocol::SessionSource;

pub(crate) mod compression;
pub(crate) mod config;
pub(crate) mod list;
pub(crate) mod metadata;
mod payload_artifact;
mod persistence_metrics;
pub(crate) mod policy;
pub(crate) mod recorder;
pub(crate) mod search;
pub(crate) mod session_index;
mod sqlite_metrics;
mod staging;
pub mod state_integration;
mod tool_manifest;
mod writer_lock;
pub use compression::rollout_files_same_identity;
pub use compression::try_lock_rollout_for_maintenance;
pub use payload_artifact::PayloadDependencyVersion;
pub use payload_artifact::named_payload_versions;
pub use payload_artifact::payload_dependencies_unchanged;
pub use payload_artifact::payload_dependency_versions;
pub use payload_artifact::payload_versions_match;
pub use payload_artifact::reclaim_payload_artifacts;
pub use recorder::ensure_rollout_source_integrity;
pub use staging::RECOVERABLE_DELETE_PREFIX;
pub use staging::has_recoverable_delete_staging;
pub use writer_lock::acquire_thread_writer;

pub(crate) use codex_protocol::protocol;

pub const SESSIONS_SUBDIR: &str = "sessions";
pub const ARCHIVED_SESSIONS_SUBDIR: &str = "archived_sessions";
pub static INTERACTIVE_SESSION_SOURCES: LazyLock<Vec<SessionSource>> = LazyLock::new(|| {
    vec![
        SessionSource::Cli,
        SessionSource::VSCode,
        SessionSource::Custom("atlas".to_string()),
        SessionSource::Custom("chatgpt".to_string()),
    ]
});

pub use codex_protocol::protocol::SessionMeta;
pub use compression::RolloutCompressionWorker;
pub use compression::RolloutLineReader;
pub use compression::existing_rollout_path;
pub use compression::lock_rollout_moves;
pub use compression::move_rollout_to_directory;
pub use compression::move_rollout_to_directory_with_owner;
pub use compression::open_rollout_line_reader;
pub use compression::plain_rollout_path;
pub use compression::read_rollout_for_feedback;
pub use compression::spawn_rollout_compression_worker;
pub use compression::try_lock_rollout_moves;
pub use config::Config;
pub use config::RolloutConfig;
pub use config::RolloutConfigView;
pub use list::Cursor;
pub use list::SortDirection;
pub use list::ThreadItem;
pub use list::ThreadListConfig;
pub use list::ThreadListLayout;
pub use list::ThreadSortKey;
pub use list::ThreadsPage;
pub use list::find_archived_thread_path_by_id_str;
pub use list::find_thread_path_by_id_str;
pub use list::find_thread_rollout_paths;
pub use list::get_threads;
pub use list::get_threads_in_root;
pub use list::parse_cursor;
pub use list::read_head_for_summary;
pub use list::read_session_meta_line;
pub use list::read_thread_item_from_rollout;
pub use list::rollout_created_at_key;
pub use list::rollout_date_parts;
pub use list::verified_rollout_thread_id;
pub use metadata::builder_from_items;
pub use persistence_metrics::RolloutPersistenceBatchMeasurement;
pub use persistence_metrics::RolloutPersistenceTelemetry;
pub use persistence_metrics::measure_and_filter_rollout_items;
pub use persistence_metrics::measure_and_filter_rollout_items_with_activity;
pub use policy::is_persisted_rollout_item;
pub use policy::is_rollout_activity_event;
pub use policy::persisted_rollout_items;
pub use policy::persisted_rollout_items_with_activity;
pub use policy::should_persist_event_msg;
pub use recorder::RolloutMaintenanceGuard;
pub use recorder::RolloutRecorder;
pub use recorder::RolloutRecorderParams;
pub use recorder::append_rollout_item_to_path;
pub use recorder::current_harness_build_info;
pub use recorder::is_rollout_reconstruction_notice;
pub use recorder::is_rollout_reconstruction_notice_id;
pub use search::RolloutSearchQuery;
pub use search::first_rollout_content_match_snippet;
pub use search::search_rollout_matches;
pub use search::search_rollout_paths;
pub use session_index::append_thread_name;
pub use session_index::find_thread_meta_by_name_str;
pub use session_index::find_thread_name_by_id;
pub use session_index::find_thread_names_by_ids;
pub use session_index::remove_thread_name_entries;
pub use state_integration::StateDbHandle;
pub use state_integration::sqlite_telemetry_recorder;
pub use tool_manifest::ToolManifestDictionary;

#[cfg(test)]
mod tests;
