use std::path::Path;

use codex_protocol::error::CodexErr;
use codex_thread_store::ThreadStoreError;

pub(crate) fn map_session_init_error(err: &anyhow::Error, _codex_home: &Path) -> CodexErr {
    if let Some(ThreadStoreError::Unsupported { operation }) = err
        .chain()
        .find_map(|cause| cause.downcast_ref::<ThreadStoreError>())
    {
        return CodexErr::UnsupportedOperation(format!("{operation} is not supported yet"));
    }

    // Session startup also performs network-policy and configuration I/O.
    // An I/O kind alone cannot identify a storage path or justify data repair.
    CodexErr::Fatal(format!("Failed to initialize session: {err:#}"))
}
