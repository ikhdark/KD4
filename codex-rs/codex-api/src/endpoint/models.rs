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
        if let Ok(mut url) = url::Url::parse(&req.url) {
            url.query_pairs_mut()
                .append_pair("client_version", client_version);
            req.url = url.into();
            return;
        }
        // Preserve invalid configurations for the fallible transport boundary.
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
                    req.max_response_bytes = Some(32 * 1024 * 1024);
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

