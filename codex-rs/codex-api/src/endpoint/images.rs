use crate::auth::SharedAuthProvider;
use crate::endpoint::session::EndpointSession;
use crate::error::ApiError;
use crate::images::ImageEditRequest;
use crate::images::ImageGenerationRequest;
use crate::images::ImageResponse;
use crate::provider::Provider;
use codex_client::EncodedJsonBody;
use codex_client::HttpTransport;
use codex_client::RequestTelemetry;
use http::HeaderMap;
use http::Method;
use serde::Serialize;
use std::sync::Arc;

pub struct ImagesClient<T: HttpTransport> {
    session: EndpointSession<T>,
}

impl<T: HttpTransport> ImagesClient<T> {
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

    pub async fn generate(
        &self,
        request: &ImageGenerationRequest,
        extra_headers: HeaderMap,
    ) -> Result<ImageResponse, ApiError> {
        self.post_image_request(
            "images/generations",
            request,
            extra_headers,
            "image generation",
            request.n.unwrap_or(1),
        )
        .await
    }

    pub async fn edit(
        &self,
        request: &ImageEditRequest,
        extra_headers: HeaderMap,
    ) -> Result<ImageResponse, ApiError> {
        self.post_image_request(
            "images/edits",
            request,
            extra_headers,
            "image edit",
            request.n.unwrap_or(1),
        )
        .await
    }

    async fn post_image_request<R: Serialize>(
        &self,
        path: &str,
        request: &R,
        extra_headers: HeaderMap,
        operation: &str,
        image_count: u64,
    ) -> Result<ImageResponse, ApiError> {
        // Each supported image may contain 64 MiB of decoded data. Allow base64
        // plus worst-case JSON Unicode escapes and one MiB of envelope metadata.
        let per_image = (64_usize * 1024 * 1024).div_ceil(3) * 4 * 6;
        let max_response_bytes = usize::try_from(image_count.max(1))
            .ok()
            .and_then(|count| per_image.checked_mul(count))
            .and_then(|bytes| bytes.checked_add(1024 * 1024))
            .ok_or_else(|| ApiError::InvalidRequest {
                message: "image response budget overflow".into(),
            })?;
        let body = EncodedJsonBody::encode(request)
            .map_err(|e| ApiError::Stream(format!("failed to encode {operation} request: {e}")))?;
        let resp = self
            .session
            .execute_non_idempotent(
                Method::POST,
                path,
                extra_headers,
                Some(body),
                max_response_bytes,
            )
            .await?;
        serde_json::from_slice(&resp.body)
            .map_err(|error| ApiError::invalid_response(crate::responses_stream::decode_diagnostic(
                &format!("failed to decode {operation} response"), &error,
            )))
    }
}

