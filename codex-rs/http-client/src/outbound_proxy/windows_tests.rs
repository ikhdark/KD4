//! Windows proxy parsing tests.

use super::*;
use windows_sys::Win32::Networking::WinHttp::ERROR_WINHTTP_CANNOT_CONNECT;
use windows_sys::Win32::Networking::WinHttp::ERROR_WINHTTP_CONNECTION_ERROR;
use windows_sys::Win32::Networking::WinHttp::ERROR_WINHTTP_NAME_NOT_RESOLVED;

#[test]
fn proxy_bypass_matches_whitespace_separated_winhttp_entries() {
    let local_origin = RequestOrigin {
        scheme: "https".to_string(),
        host: "intranet".to_string(),
        port: 443,
    };
    assert!(proxy_bypass_matches_origin("<local> *.corp", &local_origin));

    let corp_origin = RequestOrigin {
        scheme: "https".to_string(),
        host: "service.corp".to_string(),
        port: 443,
    };
    assert!(proxy_bypass_matches_origin("<local> *.corp", &corp_origin));
}

#[test]
fn winhttp_error_classification_preserves_specific_failures_and_resolver_fallback() {
    let cases = [
        (ERROR_WINHTTP_TIMEOUT, RouteFailureClass::ConnectTimeout),
        (
            ERROR_WINHTTP_LOGIN_FAILURE,
            RouteFailureClass::ProxyAuthenticationRequired,
        ),
        (
            ERROR_WINHTTP_AUTODETECTION_FAILED,
            RouteFailureClass::ProxyResolutionUnavailable,
        ),
        (ERROR_WINHTTP_SECURE_FAILURE, RouteFailureClass::TlsError),
        (
            ERROR_WINHTTP_INVALID_URL,
            RouteFailureClass::InvalidProxyConfig,
        ),
        (
            ERROR_WINHTTP_CANNOT_CONNECT,
            RouteFailureClass::ResolverError,
        ),
        (
            ERROR_WINHTTP_CONNECTION_ERROR,
            RouteFailureClass::ResolverError,
        ),
        (
            ERROR_WINHTTP_NAME_NOT_RESOLVED,
            RouteFailureClass::ResolverError,
        ),
        (u32::MAX, RouteFailureClass::ResolverError),
    ];

    for (code, expected) in cases {
        assert_eq!(
            classify_winhttp_error(code),
            expected,
            "WinHTTP code {code}"
        );
    }
}

#[test]
fn resolver_fallback_does_not_enumerate_equivalent_winhttp_codes() {
    let source = include_str!("windows.rs");

    for redundant_code in [
        "ERROR_WINHTTP_CANNOT_CONNECT",
        "ERROR_WINHTTP_CONNECTION_ERROR",
        "ERROR_WINHTTP_NAME_NOT_RESOLVED",
    ] {
        assert!(
            !source.contains(redundant_code),
            "{redundant_code} should use the resolver-error fallback"
        );
    }
}
