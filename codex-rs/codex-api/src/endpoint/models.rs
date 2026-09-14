use crate::auth::SharedAuthProvider;
use crate::endpoint::session::EndpointSession;
use crate::error::ApiError;
use crate::provider::Provider;
use codex_client::HttpTransport;
use codex_client::RequestTelemetry;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ModelsResponse;
use http::HeaderMap;
use http::Method;
use http::StatusCode;
use http::header::ETAG;
use std::sync::Arc;

#[derive(Debug)]
pub enum ModelsListResult {
    Modified {
        models: Vec<ModelInfo>,
        etag: Option<String>,
    },
    NotModified,
}

pub struct ModelsClient<T: HttpTransport> {
    session: EndpointSession<T>,
}

impl<T: HttpTransport> ModelsClient<T> {
    pub fn new(transport: T, provider: Provider, auth: SharedAuthProvider) -> Self {
        Self {
            session: EndpointSession::new(transport, provider, auth),
        }
    }

    pub fn with_telemetry(self, request: Option<Arc<dyn RequestTelemetry>>) -> Self {
        Self {
            session: self.session.with_request_telemetry(request),
        }
    }

    fn path() -> &'static str {
        "models"
    }

    fn append_client_version_query(req: &mut codex_client::Request, client_version: &str) {
        let separator = if req.url.contains('?') { '&' } else { '?' };
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("client_version", client_version)
            .finish();
        req.url = format!("{}{separator}{query}", req.url);
    }

    pub fn request_url(provider: &Provider, client_version: &str) -> String {
        let mut request = provider.build_request(Method::GET, Self::path());
        Self::append_client_version_query(&mut request, client_version);
        request.url
    }

    pub async fn list_models(
        &self,
        request_url: String,
        extra_headers: HeaderMap,
    ) -> Result<(Vec<ModelInfo>, Option<String>), ApiError> {
        match self
            .list_models_conditional(request_url, extra_headers)
            .await?
        {
            ModelsListResult::Modified { models, etag } => Ok((models, etag)),
            ModelsListResult::NotModified => Err(ApiError::Stream(
                "models endpoint returned 304 without a conditional request".to_string(),
            )),
        }
    }

    pub async fn list_models_conditional(
        &self,
        request_url: String,
        extra_headers: HeaderMap,
    ) -> Result<ModelsListResult, ApiError> {
        let resp = self
            .session
            .execute_with(
                Method::GET,
                Self::path(),
                extra_headers,
                /*body*/ None,
                move |req| {
                    req.url.clone_from(&request_url);
                },
            )
            .await?;

        if resp.status == StatusCode::NOT_MODIFIED {
            return Ok(ModelsListResult::NotModified);
        }

        let header_etag = resp
            .headers
            .get(ETAG)
            .and_then(|value| value.to_str().ok())
            .map(ToString::to_string);

        let ModelsResponse { models } = serde_json::from_slice::<ModelsResponse>(&resp.body)
            .map_err(|e| {
                ApiError::Stream(format!(
                    "failed to decode models response: {:?} at line {} column {}; status: {}; body length: {}",
                    e.classify(),
                    e.line(),
                    e.column(),
                    resp.status,
                    resp.body.len()
                ))
            })?;

        Ok(ModelsListResult::Modified {
            models,
            etag: header_etag,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthProvider;
    use crate::provider::RetryConfig;
    use codex_client::Request;
    use codex_client::Response;
    use codex_client::StreamResponse;
    use codex_client::TransportError;
    use http::HeaderMap;
    use http::StatusCode;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::time::Duration;

    #[derive(Clone)]
    struct CapturingTransport {
        last_request: Arc<Mutex<Option<Request>>>,
        body: Arc<Vec<u8>>,
        etag: Option<String>,
        status: StatusCode,
    }

    impl Default for CapturingTransport {
        fn default() -> Self {
            Self {
                last_request: Arc::new(Mutex::new(None)),
                body: Arc::new(br#"{"models":[]}"#.to_vec()),
                etag: None,
                status: StatusCode::OK,
            }
        }
    }

    impl HttpTransport for CapturingTransport {
        async fn execute(&self, req: Request) -> Result<Response, TransportError> {
            *self.last_request.lock().unwrap() = Some(req);
            let body = if self.status == StatusCode::NOT_MODIFIED {
                Vec::new()
            } else {
                self.body.as_ref().clone()
            };
            let mut headers = HeaderMap::new();
            if let Some(etag) = &self.etag {
                headers.insert(ETAG, etag.parse().unwrap());
            }
            Ok(Response {
                status: self.status,
                headers,
                body: body.into(),
            })
        }

        async fn stream(&self, _req: Request) -> Result<StreamResponse, TransportError> {
            Err(TransportError::Build("stream should not run".to_string()))
        }
    }

    #[derive(Clone, Default)]
    struct DummyAuth;

    impl AuthProvider for DummyAuth {
        fn add_auth_headers(&self, _headers: &mut HeaderMap) {}
    }

    fn provider(base_url: &str) -> Provider {
        Provider {
            name: "test".to_string(),
            base_url: base_url.to_string(),
            query_params: None,
            headers: HeaderMap::new(),
            retry: RetryConfig {
                max_retries: 1,
                base_delay: Duration::from_millis(1),
                retry_429: false,
                retry_5xx: true,
                retry_transport: true,
            },
            stream_idle_timeout: Duration::from_secs(1),
        }
    }

    #[tokio::test]
    async fn appends_client_version_query() {
        let response = ModelsResponse { models: Vec::new() };

        let transport = CapturingTransport {
            last_request: Arc::new(Mutex::new(None)),
            body: Arc::new(serde_json::to_vec(&response).unwrap()),
            etag: None,
            status: StatusCode::OK,
        };

        let provider = provider("https://example.com/api/codex");
        let request_url = ModelsClient::<CapturingTransport>::request_url(&provider, "0.99.0");
        let client = ModelsClient::new(transport.clone(), provider, Arc::new(DummyAuth));

        let (models, _) = client
            .list_models(request_url, HeaderMap::new())
            .await
            .expect("request should succeed");

        assert_eq!(models.len(), 0);

        let url = transport
            .last_request
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .url
            .clone();
        assert_eq!(
            url,
            "https://example.com/api/codex/models?client_version=0.99.0"
        );
    }

    #[tokio::test]
    async fn parses_models_response() {
        let response = ModelsResponse {
            models: vec![
                serde_json::from_value(json!({
                    "slug": "gpt-test",
                    "display_name": "gpt-test",
                    "description": "desc",
                    "default_reasoning_level": "medium",
                    "supported_reasoning_levels": [{"effort": "low", "description": "low"}, {"effort": "medium", "description": "medium"}, {"effort": "high", "description": "high"}],
                    "shell_type": "shell_command",
                    "visibility": "list",
                    "minimal_client_version": [0, 99, 0],
                    "supported_in_api": true,
                    "priority": 1,
                    "upgrade": null,
                    "base_instructions": "base instructions",
                    "supports_reasoning_summaries": false,
                    "support_verbosity": false,
                    "default_verbosity": null,
                    "apply_patch_tool_type": null,
                    "truncation_policy": {"mode": "bytes", "limit": 10_000},
                    "supports_parallel_tool_calls": false,
                    "supports_image_detail_original": false,
                    "context_window": 272_000,
                    "experimental_supported_tools": [],
                }))
                .unwrap(),
            ],
        };

        let transport = CapturingTransport {
            last_request: Arc::new(Mutex::new(None)),
            body: Arc::new(serde_json::to_vec(&response).unwrap()),
            etag: None,
            status: StatusCode::OK,
        };

        let provider = provider("https://example.com/api/codex");
        let request_url = ModelsClient::<CapturingTransport>::request_url(&provider, "0.99.0");
        let client = ModelsClient::new(transport, provider, Arc::new(DummyAuth));

        let (models, _) = client
            .list_models(request_url, HeaderMap::new())
            .await
            .expect("request should succeed");

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].slug, "gpt-test");
        assert_eq!(models[0].supported_in_api, true);
        assert_eq!(models[0].priority, 1);
    }

    #[tokio::test]
    async fn list_models_includes_etag() {
        let response = ModelsResponse { models: Vec::new() };

        let transport = CapturingTransport {
            last_request: Arc::new(Mutex::new(None)),
            body: Arc::new(serde_json::to_vec(&response).unwrap()),
            etag: Some("\"abc\"".to_string()),
            status: StatusCode::OK,
        };

        let provider = provider("https://example.com/api/codex");
        let request_url = ModelsClient::<CapturingTransport>::request_url(&provider, "0.1.0");
        let client = ModelsClient::new(transport, provider, Arc::new(DummyAuth));

        let (models, etag) = client
            .list_models(request_url, HeaderMap::new())
            .await
            .expect("request should succeed");

        assert_eq!(models.len(), 0);
        assert_eq!(etag, Some("\"abc\"".to_string()));
    }

    #[tokio::test]
    async fn encodes_client_version_without_changing_existing_query() {
        let transport = CapturingTransport::default();
        let mut provider = provider("https://example.com/api/codex");
        provider.query_params = Some(std::collections::HashMap::from([(
            "api-version".to_string(),
            "test".to_string(),
        )]));
        let version = "1.0+dev&extra=value #/?";
        let request_url = ModelsClient::<CapturingTransport>::request_url(&provider, version);
        let client = ModelsClient::new(transport.clone(), provider, Arc::new(DummyAuth));
        client
            .list_models(request_url, HeaderMap::new())
            .await
            .unwrap();
        let request = transport.last_request.lock().unwrap();
        let url = url::Url::parse(&request.as_ref().unwrap().url).unwrap();
        assert_eq!(
            url.query_pairs().collect::<Vec<_>>(),
            vec![
                ("api-version".into(), "test".into()),
                ("client_version".into(), version.into())
            ]
        );
        assert_eq!(url.fragment(), None);
    }

    #[tokio::test]
    async fn malformed_models_response_does_not_include_body_in_error() {
        let transport = CapturingTransport {
            // A valid JSON value of the wrong type makes Serde's Display include
            // the string itself; diagnostics must not copy it into the error.
            body: Arc::new(serde_json::to_vec(&"private-proxy-content".repeat(10_000)).unwrap()),
            ..Default::default()
        };
        let body_length = transport.body.len();
        let provider = provider("https://example.com/api/codex");
        let request_url = ModelsClient::<CapturingTransport>::request_url(&provider, "1.0");
        let client = ModelsClient::new(transport, provider, Arc::new(DummyAuth));
        let error = client
            .list_models(request_url, HeaderMap::new())
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("failed to decode models response"));
        assert!(error.contains("status: 200 OK"));
        assert!(error.contains(&format!("body length: {body_length}")));
        assert!(!error.contains("private-proxy-content"));
        assert!(error.len() < 300);
    }

    #[tokio::test]
    async fn conditional_list_models_accepts_not_modified_without_a_body() {
        let transport = CapturingTransport {
            status: StatusCode::NOT_MODIFIED,
            ..Default::default()
        };
        let provider = provider("https://example.com/api/codex");
        let request_url = ModelsClient::<CapturingTransport>::request_url(&provider, "0.1.0");
        let client = ModelsClient::new(transport, provider, Arc::new(DummyAuth));

        let result = client
            .list_models_conditional(request_url, HeaderMap::new())
            .await
            .expect("304 should be a successful conditional response");

        assert!(matches!(result, ModelsListResult::NotModified));
    }
}
