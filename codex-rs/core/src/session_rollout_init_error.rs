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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_kind_does_not_imply_session_storage_failure() {
        for kind in [
            std::io::ErrorKind::InvalidInput,
            std::io::ErrorKind::InvalidData,
            std::io::ErrorKind::PermissionDenied,
            std::io::ErrorKind::NotFound,
            std::io::ErrorKind::AlreadyExists,
        ] {
            let err = anyhow::Error::new(std::io::Error::new(kind, "network policy rejected"))
                .context("managed proxy initialization");
            let CodexErr::Fatal(message) = map_session_init_error(&err, Path::new("home")) else {
                panic!("expected fatal initialization error");
            };
            assert_eq!(
                message,
                "Failed to initialize session: managed proxy initialization: network policy rejected"
            );
        }
    }
}
