use crate::auth::SharedAuthProvider;
use crate::common::CompactionInput;
use crate::endpoint::session::EndpointSession;
use crate::error::ApiError;
use crate::provider::Provider;
use crate::responses_stream::X_CODEX_TURN_STATE_HEADER;
use codex_client::EncodedJsonBody;
use codex_client::HttpTransport;
use codex_client::RequestTelemetry;
use codex_protocol::models::ResponseItem;
use http::HeaderMap;
use http::Method;
use serde::Deserialize;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

pub struct CompactClient<T: HttpTransport> {
    session: EndpointSession<T>,
}

impl<T: HttpTransport> CompactClient<T> {
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
        "responses/compact"
    }

    pub async fn compact(
        &self,
        body: serde_json::Value,
        extra_headers: HeaderMap,
        request_timeout: Duration,
        turn_state: Option<&OnceLock<String>>,
    ) -> Result<Vec<ResponseItem>, ApiError> {
        let body = EncodedJsonBody::encode(&body).map_err(|error| {
            ApiError::Stream(format!("failed to encode compaction input: {error}"))
        })?;
        self.compact_encoded(body, extra_headers, request_timeout, turn_state)
            .await
    }

    async fn compact_encoded(
        &self,
        body: EncodedJsonBody,
        extra_headers: HeaderMap,
        request_timeout: Duration,
        turn_state: Option<&OnceLock<String>>,
    ) -> Result<Vec<ResponseItem>, ApiError> {
        let resp = self
            .session
            .execute_encoded_json_with(Method::POST, Self::path(), extra_headers, body, |req| {
                req.timeout = Some(request_timeout);
            })
            .await?;
        if let Some(turn_state) = turn_state
            && let Some(header_value) = resp
                .headers
                .get(X_CODEX_TURN_STATE_HEADER)
                .and_then(|value| value.to_str().ok())
        {
            let _ = turn_state.set(header_value.to_string());
        }
        let parsed: CompactHistoryResponse = serde_json::from_slice(&resp.body).map_err(|e| {
            ApiError::Stream(crate::responses_stream::decode_diagnostic(
                "failed to parse compaction response",
                &e,
            ))
        })?;
        Ok(parsed.output)
    }

    pub async fn compact_input(
        &self,
        input: &CompactionInput<'_>,
        extra_headers: HeaderMap,
        request_timeout: Duration,
        turn_state: Option<&OnceLock<String>>,
    ) -> Result<Vec<ResponseItem>, ApiError> {
        let body = EncodedJsonBody::encode(input)
            .map_err(|e| ApiError::Stream(format!("failed to encode compaction input: {e}")))?;
        self.compact_encoded(body, extra_headers, request_timeout, turn_state)
            .await
    }
}

#[derive(Debug, Deserialize)]
struct CompactHistoryResponse {
    output: Vec<ResponseItem>,
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
    use http::StatusCode;
    use serde_json::json;
    use std::sync::Mutex;

    #[derive(Clone, Default)]
    struct CapturingTransport(Arc<Mutex<Option<Request>>>);

    impl HttpTransport for CapturingTransport {
        async fn execute(&self, req: Request) -> Result<Response, TransportError> {
            *self.0.lock().unwrap() = Some(req);
            Ok(Response {
                status: StatusCode::OK,
                headers: HeaderMap::from_iter([(
                    X_CODEX_TURN_STATE_HEADER.parse().unwrap(),
                    "next-turn-state".parse().unwrap(),
                )]),
                body: serde_json::to_vec(&json!({"output": [
                    {"type": "compaction", "encrypted_content": "compacted-history"}
                ]}))
                .unwrap()
                .into(),
            })
        }

        async fn stream(&self, _req: Request) -> Result<StreamResponse, TransportError> {
            Err(TransportError::Build("stream should not run".to_string()))
        }
    }

    struct DummyAuth;

    impl AuthProvider for DummyAuth {
        fn add_auth_headers(&self, headers: &mut HeaderMap) {
            headers.insert("authorization", "Bearer test-token".parse().unwrap());
        }
    }

    #[tokio::test]
    async fn compact_input_preserves_request_and_response_contracts() {
        let transport = CapturingTransport::default();
        let client = CompactClient::new(
            transport.clone(),
            Provider {
                name: "test".to_string(),
                base_url: "https://example.com/api/codex".to_string(),
                query_params: None,
                headers: HeaderMap::new(),
                retry: RetryConfig {
                    max_retries: 0,
                    base_delay: Duration::ZERO,
                    retry_429: false,
                    retry_5xx: false,
                    retry_transport: false,
                },
                stream_idle_timeout: Duration::from_secs(1),
            },
            Arc::new(DummyAuth),
        );
        let input = CompactionInput {
            model: "gpt-test",
            input: &[],
            instructions: "Preserve project decisions.",
            tools: None,
            parallel_tool_calls: true,
            reasoning: None,
            service_tier: None,
            prompt_cache_key: None,
            text: None,
        };
        let timeout = Duration::from_secs(73);
        let turn_state = OnceLock::new();
        let output = client
            .compact_input(
                &input,
                HeaderMap::from_iter([(
                    "x-request-id".parse().unwrap(),
                    "compact-request".parse().unwrap(),
                )]),
                timeout,
                Some(&turn_state),
            )
            .await
            .unwrap();
        assert_eq!(
            serde_json::to_value(output).unwrap(),
            json!([
                {"type": "compaction", "encrypted_content": "compacted-history"}
            ])
        );
        assert_eq!(
            turn_state.get().map(String::as_str),
            Some("next-turn-state")
        );
        let stored_request = transport.0.lock().unwrap();
        let request = stored_request.as_ref().unwrap();
        assert_eq!(request.method, Method::POST);
        assert_eq!(
            request.url,
            "https://example.com/api/codex/responses/compact"
        );
        assert_eq!(request.timeout, Some(timeout));
        assert_eq!(request.headers["x-request-id"], "compact-request");
        assert_eq!(request.headers["authorization"], "Bearer test-token");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(
                &request.prepare_body_for_send().unwrap().body_bytes()
            )
            .unwrap(),
            json!({
                "model": "gpt-test", "input": [], "instructions": "Preserve project decisions.",
                "parallel_tool_calls": true
            })
        );
    }
}
