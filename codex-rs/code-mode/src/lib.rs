//! Codex's Code Mode client. Cells run in the standalone
//! `codex-code-mode-host` process; this crate never links V8.

mod remote_session;

pub use codex_code_mode_protocol::*;
pub use remote_session::DisabledCodeModeSessionProvider;
pub use remote_session::ProcessOwnedCodeModeSession;
pub use remote_session::ProcessOwnedCodeModeSessionProvider;
