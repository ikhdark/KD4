//! Windows proxy parsing tests.

use super::*;
use windows_sys::Win32::Networking::WinHttp::ERROR_WINHTTP_CANNOT_CONNECT;
use windows_sys::Win32::Networking::WinHttp::ERROR_WINHTTP_CONNECTION_ERROR;
use windows_sys::Win32::Networking::WinHttp::ERROR_WINHTTP_NAME_NOT_RESOLVED;

#[test]
fn winhttp_session_is_reused() {
    let first = with_shared_winhttp_session(|session| Ok(session.0))
        .expect("open the shared WinHTTP session");
    let second = with_shared_winhttp_session(|session| Ok(session.0))
        .expect("reuse the shared WinHTTP session");

    assert_eq!(first, second);
}

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
fn automatic_proxy_info_honors_matching_bypass() {
    let proxy_info = ProxyInfo {
        access_type: WINHTTP_ACCESS_TYPE_NAMED_PROXY,
        proxy: Some("proxy.example:8080".to_string()),
        proxy_bypass: Some("<local>;*.corp".to_string()),
    };
    let origin = RequestOrigin {
        scheme: "https".to_string(),
        host: "service.corp".to_string(),
        port: 443,
    };

    assert_eq!(
        proxy_info_decision(&proxy_info, &origin),
        SystemProxyDecision::Direct
    );
}

#[test]
fn automatic_proxy_info_uses_proxy_when_bypass_does_not_match() {
    let proxy_info = ProxyInfo {
        access_type: WINHTTP_ACCESS_TYPE_NAMED_PROXY,
        proxy: Some("proxy.example:8080".to_string()),
        proxy_bypass: Some("<local>;*.corp".to_string()),
    };
    let origin = RequestOrigin {
        scheme: "https".to_string(),
        host: "api.example.com".to_string(),
        port: 443,
    };

    assert_eq!(
        proxy_info_decision(&proxy_info, &origin),
        SystemProxyDecision::Proxy {
            url: "http://proxy.example:8080".to_string(),
        }
    );
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
