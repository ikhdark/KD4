use codex_client::Request;
use codex_client::RequestCompression;
use codex_client::RetryOn;
use codex_client::RetryPolicy;
use http::Method;
use http::header::HeaderMap;
use std::collections::HashMap;
use std::time::Duration;
use url::Url;

/// High-level retry configuration for a provider.
///
/// This is converted into a `RetryPolicy` used by `codex-client` to drive
/// transport-level retries for both unary and streaming calls.
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// Maximum number of retries after the initial request.
    pub max_retries: u64,
    pub base_delay: Duration,
    pub retry_429: bool,
    pub retry_5xx: bool,
    pub retry_transport: bool,
}

impl RetryConfig {
    pub fn to_policy(&self) -> RetryPolicy {
        RetryPolicy {
            max_retries: self.max_retries,
            base_delay: self.base_delay,
            retry_on: RetryOn {
                retry_429: self.retry_429,
                retry_5xx: self.retry_5xx,
                retry_transport: self.retry_transport,
            },
        }
    }
}

/// HTTP endpoint configuration used to talk to a concrete API deployment.
///
/// Encapsulates base URL, default headers, query params, retry policy, and
/// stream idle timeout, plus helper methods for building requests.
#[derive(Debug, Clone)]
pub struct Provider {
    pub name: String,
    pub base_url: String,
    pub query_params: Option<HashMap<String, String>>,
    pub headers: HeaderMap,
    pub retry: RetryConfig,
    pub stream_idle_timeout: Duration,
}

impl Provider {
    pub fn url_for_path(&self, path: &str) -> String {
        let base = self.base_url.trim_end_matches('/');
        let path = path.trim_start_matches('/');
        if let Ok(mut url) = Url::parse(&self.base_url) {
            let base_path = url.path().trim_end_matches('/');
            let joined_path = if path.is_empty() {
                base_path.to_string()
            } else {
                format!("{base_path}/{path}")
            };
            url.set_path(&joined_path);
            if let Some(params) = &self.query_params
                && !params.is_empty()
            {
                url.query_pairs_mut().extend_pairs(params);
            }
            return url.into();
        }
        // Preserve invalid configurations for the fallible transport boundary.
        let mut url = if path.is_empty() {
            base.to_string()
        } else {
            format!("{base}/{path}")
        };

        if let Some(params) = &self.query_params
            && !params.is_empty()
        {
            let qs = url::form_urlencoded::Serializer::new(String::new())
                .extend_pairs(params)
                .finish();
            url.push('?');
            url.push_str(&qs);
        }

        url
    }

    pub fn build_request(&self, method: Method, path: &str) -> Request {
        Request {
            method,
            url: self.url_for_path(path),
            headers: self.headers.clone(),
            body: None,
            compression: RequestCompression::None,
            timeout: None,
        }
    }

    pub fn is_azure_responses_endpoint(&self) -> bool {
        is_azure_responses_provider(&self.name, Some(&self.base_url))
    }

    pub fn websocket_url_for_path(&self, path: &str) -> Result<Url, url::ParseError> {
        let mut url = Url::parse(&self.url_for_path(path))?;

        let scheme = match url.scheme() {
            "http" => "ws",
            "https" => "wss",
            "ws" | "wss" => return Ok(url),
            _ => return Ok(url),
        };
        let _ = url.set_scheme(scheme);
        Ok(url)
    }
}

pub fn is_azure_responses_provider(name: &str, base_url: Option<&str>) -> bool {
    if name.eq_ignore_ascii_case("azure") {
        true
    } else if let Some(base_url) = base_url {
        matches_azure_responses_base_url(base_url)
    } else {
        false
    }
}

fn matches_azure_responses_base_url(base_url: &str) -> bool {
    let Ok(url) = Url::parse(base_url) else {
        return false;
    };
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host.trim_end_matches('.');
    let matches_domain = |domain: &str| {
        host == domain
            || host
                .strip_suffix(domain)
                .is_some_and(|prefix| prefix.ends_with('.'))
    };
    ["com", "us", "cn"].iter().any(|suffix| {
        ["openai", "cognitiveservices", "aoai"]
            .iter()
            .any(|service| matches_domain(&format!("{service}.azure.{suffix}")))
    }) || matches_domain("azure-api.net")
        || matches_domain("azurefd.net")
        || (matches_domain("windows.net")
            && (url.path() == "/openai" || url.path().starts_with("/openai/")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_azure_responses_base_urls() {
        let positive_cases = [
            "https://foo.openai.azure.com/openai",
            "https://foo.openai.azure.us/openai/deployments/bar",
            "https://foo.cognitiveservices.azure.cn/openai",
            "https://foo.aoai.azure.com/openai",
            "https://foo.openai.azure-api.net/openai",
            "https://foo.z01.azurefd.net/",
            "https://foo.windows.net/openai/deployments/bar",
        ];

        for base_url in positive_cases {
            assert!(
                is_azure_responses_provider("test", Some(base_url)),
                "expected {base_url} to be detected as Azure"
            );
        }

        assert!(is_azure_responses_provider(
            "Azure",
            Some("https://example.com")
        ));

        let negative_cases = [
            "https://api.openai.com/v1",
            "https://example.com/openai",
            "https://myproxy.azurewebsites.net/openai",
            "https://proxy.example/v1?note=openai.azure.com",
            "https://proxy.example/openai.azure.com",
            "https://foo.openai.azure.com.example/v1",
            "https://notazurefd.net/openai",
            "https://foo.windows.net/openai-other",
        ];

        for base_url in negative_cases {
            assert!(
                !is_azure_responses_provider("test", Some(base_url)),
                "expected {base_url} not to be detected as Azure"
            );
        }
    }

    #[test]
    fn request_and_websocket_urls_preserve_base_query_and_fragment() {
        let provider = Provider {
            name: "test".into(),
            base_url: "https://proxy.example/v1/?tenant=a#section".into(),
            query_params: Some(HashMap::from([("api-version".into(), "2026 09".into())])),
            headers: HeaderMap::new(),
            retry: RetryConfig {
                max_retries: 0,
                base_delay: Duration::ZERO,
                retry_429: false,
                retry_5xx: false,
                retry_transport: false,
            },
            stream_idle_timeout: Duration::from_secs(1),
        };
        let request = provider.build_request(Method::POST, "/responses");
        assert_eq!(
            request.url,
            "https://proxy.example/v1/responses?tenant=a&api-version=2026+09#section"
        );
        assert_eq!(
            provider
                .websocket_url_for_path("responses")
                .expect("valid URL")
                .as_str(),
            "wss://proxy.example/v1/responses?tenant=a&api-version=2026+09#section"
        );
    }
}
