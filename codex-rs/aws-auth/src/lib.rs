mod config;
mod signing;

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;

use aws_credential_types::Credentials;
use aws_credential_types::provider::ProvideCredentials;
use aws_credential_types::provider::SharedCredentialsProvider;
use bytes::Bytes;
use http::HeaderMap;
use http::Method;
use thiserror::Error;

/// Longest time one resolution of expiring credentials is reused. This also
/// bounds how long a profile re-pointed at another account keeps the old one.
const MAX_CREDENTIALS_REUSE: Duration = Duration::from_secs(5 * 60);
/// Credentials this close to their expiry are resolved again, not reused.
const CREDENTIALS_EXPIRY_BUFFER: Duration = Duration::from_secs(5 * 60);

/// AWS auth configuration used to resolve credentials and sign requests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AwsAuthConfig {
    pub profile: Option<String>,
    pub region: Option<String>,
    pub service: String,
}

/// Final unsigned HTTP request consumed by SigV4 signing.
/// Retries must start from these unsigned parts, not a previously signed result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AwsRequestToSign {
    pub method: Method,
    pub url: String,
    pub headers: HeaderMap,
    pub body: Bytes,
}

/// Signed request parts returned to the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AwsSignedRequest {
    pub url: String,
    pub headers: HeaderMap,
}

/// Errors returned by credential loading or SigV4 signing.
///
/// Callers surface these through `to_string()`, so SDK errors whose own
/// message is generic render their source chain instead of exposing it
/// through `source()`.
#[derive(Debug, Error)]
pub enum AwsAuthError {
    #[error("AWS service name must not be empty")]
    EmptyService,
    #[error("AWS SDK config did not resolve a credentials provider")]
    MissingCredentialsProvider,
    #[error("AWS SDK config did not resolve a region")]
    MissingRegion,
    #[error("failed to load AWS credentials: {}", ErrorChain(.0))]
    Credentials(aws_credential_types::provider::error::CredentialsError),
    #[error("request URL is not a valid URI: {0}")]
    InvalidUri(#[source] http::uri::InvalidUri),
    #[error("request URL must be an absolute HTTP(S) URI with an authority")]
    InvalidSigningUrl,
    #[error("failed to construct HTTP request for signing: {0}")]
    BuildHttpRequest(#[source] http::Error),
    #[error("request contains a non-UTF8 header value: {0}")]
    InvalidHeaderValue(#[source] http::header::ToStrError),
    #[error("failed to build signable request: {}", ErrorChain(.0))]
    SigningRequest(aws_sigv4::http_request::SigningError),
    #[error("failed to build SigV4 signing params: {0}")]
    SigningParams(String),
    #[error("SigV4 signing failed: {}", ErrorChain(.0))]
    SigningFailure(aws_sigv4::http_request::SigningError),
}

/// Renders an error followed by each of its sources. AWS SDK errors keep the
/// actionable cause (an expired SSO session, a missing profile, every provider
/// the default chain tried) behind a generic top-level message.
struct ErrorChain<'a>(&'a (dyn std::error::Error + 'static));

impl std::fmt::Display for ErrorChain<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)?;
        let mut source = self.0.source();
        while let Some(cause) = source {
            write!(f, ": {cause}")?;
            source = cause.source();
        }
        Ok(())
    }
}

/// Expiring credentials shared by the contexts one provider loads per request.
///
/// Contexts still reload SDK configuration for every request, so profile edits
/// and credentials without an expiry are read as before. Credentials that
/// report an expiry (SSO, assume-role, container, instance or process
/// credentials) are reused for at most five minutes, and never within five
/// minutes of expiring.
#[derive(Clone, Default)]
pub struct AwsCredentialsCache {
    cached: Arc<Mutex<Option<CachedCredentials>>>,
}

struct CachedCredentials {
    credentials: Credentials,
    resolved_at: Instant,
}

impl std::fmt::Debug for AwsCredentialsCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AwsCredentialsCache")
            .finish_non_exhaustive()
    }
}

impl AwsCredentialsCache {
    fn reusable(&self, now: SystemTime, now_instant: Instant) -> Option<Credentials> {
        let cached = self.cached.lock().unwrap_or_else(PoisonError::into_inner);
        cached
            .as_ref()
            .filter(|cached| {
                now_instant.saturating_duration_since(cached.resolved_at) < MAX_CREDENTIALS_REUSE
                    && outlives_expiry_buffer(&cached.credentials, now)
            })
            .map(|cached| cached.credentials.clone())
    }

    fn store(&self, credentials: &Credentials, now: SystemTime, now_instant: Instant) {
        *self.cached.lock().unwrap_or_else(PoisonError::into_inner) =
            outlives_expiry_buffer(credentials, now).then(|| CachedCredentials {
                credentials: credentials.clone(),
                resolved_at: now_instant,
            });
    }
}

fn outlives_expiry_buffer(credentials: &Credentials, now: SystemTime) -> bool {
    credentials
        .expiry()
        .and_then(|expiry| expiry.duration_since(now).ok())
        .is_some_and(|remaining| remaining > CREDENTIALS_EXPIRY_BUFFER)
}

/// Loaded AWS auth context that can sign outbound HTTP requests.
#[derive(Clone)]
pub struct AwsAuthContext {
    credentials_provider: SharedCredentialsProvider,
    credentials_cache: AwsCredentialsCache,
    region: String,
    service: String,
}

impl std::fmt::Debug for AwsAuthContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AwsAuthContext")
            .field("region", &self.region)
            .field("service", &self.service)
            .finish_non_exhaustive()
    }
}

impl AwsAuthContext {
    /// Loads SDK configuration for one request. Pass the same `credentials_cache`
    /// to every context loaded for a provider so expiring credentials are reused.
    pub async fn load(
        config: AwsAuthConfig,
        credentials_cache: AwsCredentialsCache,
    ) -> Result<Self, AwsAuthError> {
        let sdk_config = config::load_sdk_config(&config).await?;
        let credentials_provider = config::credentials_provider(&sdk_config)?;
        let region = config::resolved_region(&sdk_config)?;

        Ok(Self {
            credentials_provider,
            credentials_cache,
            region,
            service: config.service.trim().to_string(),
        })
    }

    pub fn region(&self) -> &str {
        &self.region
    }

    pub fn service(&self) -> &str {
        &self.service
    }

    pub async fn sign(&self, request: AwsRequestToSign) -> Result<AwsSignedRequest, AwsAuthError> {
        let credentials = self.credentials(SystemTime::now(), Instant::now()).await?;
        signing::sign_request(
            &credentials,
            &self.region,
            &self.service,
            request,
            SystemTime::now(),
        )
    }

    async fn credentials(
        &self,
        now: SystemTime,
        now_instant: Instant,
    ) -> Result<Credentials, AwsAuthError> {
        if let Some(credentials) = self.credentials_cache.reusable(now, now_instant) {
            return Ok(credentials);
        }
        let credentials = self
            .credentials_provider
            .provide_credentials()
            .await
            .map_err(AwsAuthError::Credentials)?;
        self.credentials_cache.store(&credentials, now, now_instant);
        Ok(credentials)
    }

    #[cfg(test)]
    async fn sign_at(
        &self,
        request: AwsRequestToSign,
        time: SystemTime,
        now_instant: Instant,
    ) -> Result<AwsSignedRequest, AwsAuthError> {
        let credentials = self.credentials(time, now_instant).await?;
        signing::sign_request(&credentials, &self.region, &self.service, request, time)
    }
}

impl AwsAuthError {
    /// Returns whether retrying the outbound request can reasonably recover from this auth error.
    pub fn is_retryable(&self) -> bool {
        match self {
            AwsAuthError::Credentials(error) => matches!(
                error,
                aws_credential_types::provider::error::CredentialsError::ProviderTimedOut(_)
                    | aws_credential_types::provider::error::CredentialsError::ProviderError(_)
            ),
            AwsAuthError::EmptyService
            | AwsAuthError::MissingCredentialsProvider
            | AwsAuthError::MissingRegion
            | AwsAuthError::InvalidUri(_)
            | AwsAuthError::InvalidSigningUrl
            | AwsAuthError::BuildHttpRequest(_)
            | AwsAuthError::InvalidHeaderValue(_)
            | AwsAuthError::SigningRequest(_)
            | AwsAuthError::SigningParams(_)
            | AwsAuthError::SigningFailure(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    use std::time::UNIX_EPOCH;

    use aws_credential_types::Credentials;
    use aws_credential_types::provider::error::CredentialsError;
    use aws_credential_types::provider::future;
    use pretty_assertions::assert_eq;

    use super::*;

    fn test_context(session_token: Option<&str>) -> AwsAuthContext {
        AwsAuthContext {
            credentials_provider: SharedCredentialsProvider::new(Credentials::new(
                "AKIDEXAMPLE",
                "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
                session_token.map(str::to_string),
                /*expires_after*/ None,
                "unit-test",
            )),
            credentials_cache: AwsCredentialsCache::default(),
            region: "us-east-1".to_string(),
            service: "bedrock".to_string(),
        }
    }

    /// Issues distinct credentials on every resolution so tests can see reuse.
    #[derive(Debug)]
    struct CountingCredentials {
        calls: Arc<AtomicUsize>,
        expiry: Option<SystemTime>,
    }

    impl ProvideCredentials for CountingCredentials {
        fn provide_credentials<'a>(&'a self) -> future::ProvideCredentials<'a>
        where
            Self: 'a,
        {
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            future::ProvideCredentials::ready(Ok(Credentials::new(
                format!("AKIDCALL{call}"),
                "secret",
                /*session_token*/ None,
                self.expiry,
                "counting-test",
            )))
        }
    }

    /// Mirrors one request: a fresh context sharing the provider's cache.
    async fn signing_key_id(
        calls: &Arc<AtomicUsize>,
        expiry: Option<SystemTime>,
        credentials_cache: &AwsCredentialsCache,
        time: SystemTime,
        now_instant: Instant,
    ) -> String {
        let context = AwsAuthContext {
            credentials_provider: SharedCredentialsProvider::new(CountingCredentials {
                calls: Arc::clone(calls),
                expiry,
            }),
            credentials_cache: credentials_cache.clone(),
            region: "us-east-1".to_string(),
            service: "bedrock".to_string(),
        };
        let signed = context
            .sign_at(test_request(), time, now_instant)
            .await
            .expect("request should sign");
        let authorization =
            signing::header_value(&signed.headers, http::header::AUTHORIZATION.as_str())
                .expect("request should carry an authorization header");
        authorization
            .split("Credential=")
            .nth(1)
            .and_then(|credential| credential.split('/').next())
            .expect("authorization should name its access key")
            .to_string()
    }

    fn test_request() -> AwsRequestToSign {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        headers.insert("x-test-header", http::HeaderValue::from_static("present"));
        AwsRequestToSign {
            method: Method::POST,
            url: "https://bedrock-runtime.us-east-1.amazonaws.com/v1/responses".to_string(),
            headers,
            body: Bytes::from_static(br#"{"model":"openai.gpt-oss-120b-1:0"}"#),
        }
    }

    #[tokio::test]
    async fn sign_adds_sigv4_headers_and_preserves_existing_headers() {
        let signed = test_context(/*session_token*/ None)
            .sign_at(
                test_request(),
                UNIX_EPOCH + Duration::from_secs(1_700_000_000),
                Instant::now(),
            )
            .await
            .expect("request should sign");

        assert_eq!(
            signing::header_value(&signed.headers, http::header::CONTENT_TYPE.as_str()),
            Some("application/json".to_string())
        );
        assert_eq!(
            signing::header_value(&signed.headers, "x-test-header"),
            Some("present".to_string())
        );
        assert_eq!(
            signed.url,
            "https://bedrock-runtime.us-east-1.amazonaws.com/v1/responses"
        );
        assert!(
            signing::header_value(&signed.headers, http::header::AUTHORIZATION.as_str())
                .is_some_and(|value| value.starts_with("AWS4-HMAC-SHA256 "))
        );
        assert!(signing::header_value(&signed.headers, "x-amz-date").is_some());
    }

    #[test]
    fn credentials_provider_failures_are_retryable() {
        assert!(
            AwsAuthError::Credentials(CredentialsError::provider_error("temporarily unavailable"))
                .is_retryable()
        );
        assert!(
            AwsAuthError::Credentials(CredentialsError::provider_timed_out(Duration::from_secs(1)))
                .is_retryable()
        );
    }

    #[test]
    fn credential_errors_display_their_cause() {
        let error = AwsAuthError::Credentials(CredentialsError::provider_error(
            "the SSO session has expired; run `aws sso login`",
        ));

        assert_eq!(
            error.to_string(),
            "failed to load AWS credentials: an error occurred while loading credentials: \
the SSO session has expired; run `aws sso login`"
        );
    }

    #[test]
    fn deterministic_aws_auth_errors_are_not_retryable() {
        assert!(!AwsAuthError::EmptyService.is_retryable());
        assert!(
            !AwsAuthError::Credentials(CredentialsError::not_loaded_no_source()).is_retryable()
        );
        assert!(
            !AwsAuthError::Credentials(CredentialsError::invalid_configuration("bad profile"))
                .is_retryable()
        );
        assert!(
            !AwsAuthError::Credentials(CredentialsError::unhandled("unexpected response"))
                .is_retryable()
        );
    }

    #[tokio::test]
    async fn sign_includes_session_token_when_credentials_have_one() {
        let signed = test_context(Some("session-token"))
            .sign_at(
                test_request(),
                UNIX_EPOCH + Duration::from_secs(1_700_000_000),
                Instant::now(),
            )
            .await
            .expect("request should sign");

        assert_eq!(
            signing::header_value(&signed.headers, "x-amz-security-token"),
            Some("session-token".to_string())
        );
    }

    #[tokio::test]
    async fn signature_depends_on_the_request_body() {
        let context = test_context(None);
        let time = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let now = Instant::now();
        let request = test_request();
        let original = context.sign_at(request.clone(), time, now).await.unwrap();
        let mut changed = request;
        changed.body = Bytes::from_static(b"different payload");
        let changed = context.sign_at(changed, time, now).await.unwrap();
        assert_ne!(
            original.headers[http::header::AUTHORIZATION],
            changed.headers[http::header::AUTHORIZATION]
        );
    }

    #[tokio::test]
    async fn sign_rejects_relative_and_non_http_urls() {
        for url in ["/v1/responses", "example.com:443", "ftp://example.com/v1"] {
            let mut request = test_request();
            request.url = url.to_string();
            let error = test_context(None).sign(request).await.unwrap_err();
            assert!(
                matches!(error, AwsAuthError::InvalidSigningUrl),
                "{url}: {error}"
            );
            assert!(!error.is_retryable());
        }
    }

    #[tokio::test]
    async fn load_rejects_empty_service_name() {
        let err = AwsAuthContext::load(
            AwsAuthConfig {
                profile: None,
                region: None,
                service: "   ".to_string(),
            },
            AwsCredentialsCache::default(),
        )
        .await
        .expect_err("empty service should be rejected");

        assert_eq!(err.to_string(), "AWS service name must not be empty");
    }

    #[tokio::test]
    async fn contexts_sharing_a_cache_reuse_expiring_credentials_within_bounds() {
        let time = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let start = Instant::now();
        let minutes = |count: u64| Duration::from_secs(count * 60);
        let expiry = Some(time + minutes(60));
        let calls = Arc::new(AtomicUsize::new(0));
        let cache = AwsCredentialsCache::default();

        for (elapsed, expected_key_id) in [(0, "AKIDCALL1"), (4, "AKIDCALL1"), (5, "AKIDCALL2")] {
            assert_eq!(
                signing_key_id(
                    &calls,
                    expiry,
                    &cache,
                    time + minutes(elapsed),
                    start + minutes(elapsed),
                )
                .await,
                expected_key_id,
                "after {elapsed} minutes"
            );
        }
        // Credentials within the expiry buffer are resolved again, and not kept.
        for expected_key_id in ["AKIDCALL3", "AKIDCALL4"] {
            assert_eq!(
                signing_key_id(
                    &calls,
                    expiry,
                    &cache,
                    time + minutes(56),
                    start + minutes(6),
                )
                .await,
                expected_key_id
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn credentials_without_expiry_are_resolved_for_every_request() {
        let time = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let now = Instant::now();
        let calls = Arc::new(AtomicUsize::new(0));
        let cache = AwsCredentialsCache::default();

        for expected_key_id in ["AKIDCALL1", "AKIDCALL2"] {
            assert_eq!(
                signing_key_id(&calls, /*expiry*/ None, &cache, time, now).await,
                expected_key_id
            );
        }
    }
}
