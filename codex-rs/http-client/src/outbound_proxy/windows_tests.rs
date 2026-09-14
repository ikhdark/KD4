//! Windows proxy parsing tests.

use super::*;
use windows_sys::Win32::Networking::WinHttp::ERROR_WINHTTP_CANNOT_CONNECT;
use windows_sys::Win32::Networking::WinHttp::ERROR_WINHTTP_CONNECTION_ERROR;
use windows_sys::Win32::Networking::WinHttp::ERROR_WINHTTP_NAME_NOT_RESOLVED;

#[test]
fn winhttp_session_is_reused() {
    let first = with_shared_winhttp_session(|session| Ok(session.0))
        .expect("open the shared WinHTTP session");
    let retained = WINHTTP_SESSION
        .lock()
        .expect("session lock")
        .as_ref()
        .expect("cached session")
        .clone();
    assert_eq!(first, retained.0);
    let reused =
        with_shared_winhttp_session(|session| Ok(std::ptr::eq(session, Arc::as_ref(&retained))))
            .expect("reuse the shared WinHTTP session");

    assert!(reused);
}

#[test]
fn resolution_preserves_specific_failures_after_fallbacks() {
    let origin = RequestOrigin {
        scheme: "https".into(),
        host: "example.com".into(),
        port: 443,
    };
    for failure in [
        RouteFailureClass::TlsError,
        RouteFailureClass::ConnectTimeout,
        RouteFailureClass::ProxyAuthenticationRequired,
        RouteFailureClass::InvalidProxyConfig,
    ] {
        for static_proxy in [
            None,
            Some("proxy.example:8080"),
            Some("SOCKS socks.example:1080"),
        ] {
            let decision = resolve_with_config(
                "https://example.com/path",
                &origin,
                IeProxyConfig {
                    auto_config_url: Some("https://example.com/proxy.pac".into()),
                    auto_detect: true,
                    static_proxy: static_proxy.map(str::to_owned),
                    ..Default::default()
                },
                |url, _, pac| {
                    assert_eq!(url, "https://example.com/path");
                    assert_eq!(pac, "https://example.com/proxy.pac");
                    SystemProxyDecision::Unavailable { failure }
                },
                |_, _| SystemProxyDecision::Unavailable {
                    failure: RouteFailureClass::ProxyResolutionUnavailable,
                },
            );
            assert_eq!(
                decision,
                if static_proxy == Some("proxy.example:8080") {
                    SystemProxyDecision::Proxy {
                        url: "http://proxy.example:8080".into(),
                    }
                } else {
                    SystemProxyDecision::Unavailable { failure }
                }
            );
        }
    }
}

#[test]
fn auto_detection_can_recover_or_supply_a_specific_failure() {
    let origin = RequestOrigin {
        scheme: "https".into(),
        host: "example.com".into(),
        port: 443,
    };
    for decision in [
        SystemProxyDecision::Direct,
        SystemProxyDecision::Unavailable {
            failure: RouteFailureClass::ConnectTimeout,
        },
    ] {
        assert_eq!(
            resolve_with_config(
                "https://example.com/",
                &origin,
                IeProxyConfig {
                    auto_config_url: Some("https://example.com/proxy.pac".into()),
                    auto_detect: true,
                    ..Default::default()
                },
                |_, _, _| SystemProxyDecision::Unavailable {
                    failure: RouteFailureClass::ProxyResolutionUnavailable
                },
                |_, _| decision.clone()
            ),
            decision
        );
    }
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
