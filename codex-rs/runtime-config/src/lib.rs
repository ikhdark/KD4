//! Resolved runtime configuration, independent of session orchestration.
#![deny(clippy::print_stdout, clippy::print_stderr)]
pub mod config;
pub mod config_lock;
pub mod windows_sandbox;
pub use config::*;

impl codex_rollout::RolloutConfigView for Config {
    fn codex_home(&self) -> &std::path::Path {
        self.codex_home.as_path()
    }

    fn sqlite_home(&self) -> &std::path::Path {
        self.sqlite_home.as_path()
    }

    fn cwd(&self) -> &std::path::Path {
        self.cwd.as_path()
    }

    fn model_provider_id(&self) -> &str {
        self.model_provider_id.as_str()
    }
}
