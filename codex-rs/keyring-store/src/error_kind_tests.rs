//! Tests native cause classification without touching the real keyring.

use super::*;
use pretty_assertions::assert_eq;

#[test]
fn preserves_typed_causes_without_guessing_from_messages() {
    let cases = [
        (
            keyring::Error::NoStorageAccess(Box::new(std::io::Error::other(
                "locked timeout access denied",
            ))),
            ErrorKind::Other,
        ),
        (
            keyring::Error::PlatformFailure(Box::new(std::io::Error::new(
                ErrorKind::TimedOut,
                "private account",
            ))),
            ErrorKind::TimedOut,
        ),
        (
            keyring::Error::PlatformFailure(Box::new(std::sync::Arc::new(std::io::Error::new(
                ErrorKind::PermissionDenied,
                "private D-Bus socket",
            )))),
            ErrorKind::PermissionDenied,
        ),
        (
            keyring::Error::Invalid("private account".into(), "private reason".into()),
            ErrorKind::Other,
        ),
    ];
    for (error, expected) in cases {
        let error = std::io::Error::from(CredentialStoreError::new(error));
        assert_eq!(error.kind(), expected);
        assert!(
            error
                .get_ref()
                .unwrap()
                .downcast_ref::<CredentialStoreError>()
                .is_some()
        );
    }
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
#[test]
fn classifies_native_backend_errors() {
    #[cfg(target_os = "macos")]
    let native: Box<dyn Error + Send + Sync> = Box::new(
        security_framework::base::Error::from_code(/*code*/ -25291),
    );
    #[cfg(target_os = "linux")]
    let native: Box<dyn Error + Send + Sync> = Box::new(secret_service::Error::Locked);
    #[cfg(target_os = "windows")]
    let native: Box<dyn Error + Send + Sync> = Box::new(keyring::windows::Error(1312));
    assert_eq!(
        std::io::Error::from(CredentialStoreError::new(keyring::Error::NoStorageAccess(
            native
        )))
        .kind(),
        if cfg!(target_os = "linux") {
            ErrorKind::WouldBlock
        } else {
            ErrorKind::NotConnected
        },
    );
}

#[cfg(target_os = "windows")]
#[test]
fn classifies_windows_platform_failure_codes() {
    // 87 is ERROR_INVALID_PARAMETER, which has no portable kind.
    for (code, expected) in [
        (5, ErrorKind::PermissionDenied),
        (50, ErrorKind::Unsupported),
        (1460, ErrorKind::TimedOut),
        (87, ErrorKind::Other),
    ] {
        let error = CredentialStoreError::new(keyring::Error::PlatformFailure(Box::new(
            keyring::windows::Error(code),
        )));
        assert_eq!(std::io::Error::from(error).kind(), expected, "code {code}");
    }
}
