use std::fmt;

use http::HeaderMap;

/// Request headers returned by an external auth provider.
///
/// The provider owns credential validation, rotation, and persistence. Codex
/// keeps the resolved headers in memory and attaches them to backend requests.
#[derive(Clone, PartialEq, Eq)]
pub struct AuthHeaders {
    headers: HeaderMap,
}

impl AuthHeaders {
    pub fn new(mut headers: HeaderMap) -> Self {
        for value in headers.values_mut() {
            value.set_sensitive(true);
        }
        Self { headers }
    }

    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }
}

impl fmt::Debug for AuthHeaders {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthHeaders")
            .field("headers", &"<redacted>")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cloned_request_headers_remain_sensitive() {
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", http::HeaderValue::from_static("header-secret"));
        let auth = AuthHeaders::new(headers);
        let request_headers = auth.headers().clone();
        assert!(request_headers["x-api-key"].is_sensitive());
        assert!(!format!("{request_headers:?}").contains("header-secret"));
        assert_eq!(request_headers["x-api-key"], "header-secret");
    }
}
