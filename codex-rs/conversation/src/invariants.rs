use tracing::error;

pub fn error_or_panic(message: impl ToString) {
    if cfg!(debug_assertions) {
        panic!("{}", message.to_string());
    } else {
        error!(target: "codex_core::invariants", "{}", message.to_string());
    }
}
