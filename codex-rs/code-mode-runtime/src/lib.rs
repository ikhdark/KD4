//! The V8 Code Mode runtime. Only `codex-code-mode-host` links it in
//! production; Codex itself reaches it through `codex-code-mode`'s
//! process-owned sessions.

mod cell_actor;
mod runtime;
mod service;
mod session_runtime;
mod v8_init;

pub(crate) type TaskFailureHandler = std::sync::Arc<dyn Fn(String) + Send + Sync>;

pub use codex_code_mode_protocol::*;
pub use service::InProcessCodeModeSession;
pub use service::InProcessCodeModeSessionProvider;
