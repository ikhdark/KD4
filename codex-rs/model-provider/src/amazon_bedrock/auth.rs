use std::sync::Arc;

use codex_api::AuthError;
use codex_api::AuthProvider;
use codex_api::SharedAuthProvider;
use codex_aws_auth::AwsAuthContext;
use codex_aws_auth::AwsAuthError;
use codex_aws_auth::AwsRequestToSign;
use codex_http_client::Request;
use codex_http_client::RequestBody;
use codex_http_client::RequestCompression;
use codex_login::auth::BedrockApiKeyAuth;
use codex_model_provider_info::ModelProviderAwsAuthInfo;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result;
use http::HeaderMap;

use crate::BearerAuthProvider;

use super::BedrockEndpoint;
use super::mantle::aws_auth_config;
use super::mantle::region_from_config;
use super::runtime;

const AWS_BEARER_TOKEN_BEDROCK_ENV_VAR: &str = "AWS_BEARER_TOKEN_BEDROCK";
const AWS_REGION_ENV_VAR: &str = "AWS_REGION";
const AWS_DEFAULT_REGION_ENV_VAR: &str = "AWS_DEFAULT_REGION";

pub(super) enum BedrockAuthMethod {
    ManagedBearerToken { token: String, region: String },
    EnvBearerToken { token: String, region: String },
    AwsSdkAuth { context: AwsAuthContext },
}

pub(super) async fn resolve_auth_method(
    managed_auth: Option<&BedrockApiKeyAuth>,
    aws: &ModelProviderAwsAuthInfo,
    endpoint: BedrockEndpoint,
    sdk_context: &tokio::sync::OnceCell<AwsAuthContext>,
) -> Result<BedrockAuthMethod> {
    if let Some(managed_auth) = managed_auth {
        return Ok(BedrockAuthMethod::ManagedBearerToken {
            token: managed_auth.api_key.clone(),
            region: managed_auth.region.clone(),
        });
    }

    if let Some(token) = non_empty_env_var_from(AWS_BEARER_TOKEN_BEDROCK_ENV_VAR, std::env::var) {
        let region = bearer_token_region(aws, std::env::var)?;
        return Ok(BedrockAuthMethod::EnvBearerToken { token, region });
    }

    let config = match endpoint {
        BedrockEndpoint::Mantle => aws_auth_config(aws),
        BedrockEndpoint::Runtime => runtime::aws_auth_config(aws),
    };
    // Cache the SDK context, not credentials. Signing still asks its refreshable
    // provider for credentials on every request. Failed/cancelled loads are retried.
    let context = sdk_context
        .get_or_try_init(|| AwsAuthContext::load(config))
        .await
        .map_err(aws_auth_error_to_codex_error)?
        .clone();
    Ok(BedrockAuthMethod::AwsSdkAuth { context })
}

impl BedrockAuthMethod {
    pub(super) fn region(&self) -> &str {
        match self {
            Self::ManagedBearerToken { region, .. } | Self::EnvBearerToken { region, .. } => region,
            Self::AwsSdkAuth { context } => context.region(),
        }
    }

    pub(super) fn into_provider(self, endpoint: BedrockEndpoint) -> SharedAuthProvider {
        match self {
            BedrockAuthMethod::ManagedBearerToken { token, .. }
            | BedrockAuthMethod::EnvBearerToken { token, .. } => Arc::new(BearerAuthProvider {
                token: Some(token),
                account_id: None,
                is_fedramp_account: false,
            }),
            BedrockAuthMethod::AwsSdkAuth { context } => {
                Arc::new(BedrockSigV4AuthProvider::new(context, endpoint))
            }
        }
    }
}

fn non_empty_env_var_from(
    name: &'static str,
    env_var: impl Fn(&'static str) -> std::result::Result<String, std::env::VarError>,
) -> Option<String> {
    env_var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn bearer_token_region(
    aws: &ModelProviderAwsAuthInfo,
    env_var: impl Fn(&'static str) -> std::result::Result<String, std::env::VarError> + Copy,
) -> Result<String> {
    region_from_config(aws)
        .or_else(|| non_empty_env_var_from(AWS_REGION_ENV_VAR, env_var))
        .or_else(|| non_empty_env_var_from(AWS_DEFAULT_REGION_ENV_VAR, env_var))
        .ok_or_else(|| {
            CodexErr::Fatal(
                "Amazon Bedrock bearer token auth requires \
`model_providers.amazon-bedrock.aws.region`, `AWS_REGION`, or `AWS_DEFAULT_REGION`"
                    .to_string(),
            )
        })
}

fn aws_auth_error_to_codex_error(error: AwsAuthError) -> CodexErr {
    CodexErr::Fatal(format!("failed to resolve Amazon Bedrock auth: {error}"))
}

fn aws_auth_error_to_auth_error(error: AwsAuthError) -> AuthError {
    if error.is_retryable() {
        AuthError::Transient(error.to_string())
    } else {
        AuthError::Build(error.to_string())
    }
}

fn remove_headers_not_preserved_by_bedrock_mantle(headers: &mut HeaderMap) {
    // The Bedrock Mantle front door does not preserve legacy OpenAI
    // compatibility headers that use snake_case, such as `session_id` and
    // `thread_id`, before SigV4 verification. Signing that header class makes
    // richer Codex agent requests fail even though raw Responses requests work.
    let headers_to_remove = headers
        .keys()
        .filter(|name| name.as_str().contains('_'))
        .cloned()
        .collect::<Vec<_>>();
    for name in headers_to_remove {
        headers.remove(name);
    }
}

/// AWS SigV4 auth provider for Bedrock OpenAI-compatible requests.
#[derive(Debug)]
struct BedrockSigV4AuthProvider {
    context: AwsAuthContext,
    endpoint: BedrockEndpoint,
}

impl BedrockSigV4AuthProvider {
    fn new(context: AwsAuthContext, endpoint: BedrockEndpoint) -> Self {
        Self { context, endpoint }
    }

    async fn apply_auth(&self, request: Request) -> std::result::Result<Request, AuthError> {
        let mut request = request;
        if self.endpoint == BedrockEndpoint::Mantle {
            remove_headers_not_preserved_by_bedrock_mantle(&mut request.headers);
        }
        let prepared = request.prepare_body_for_send().map_err(AuthError::Build)?;
        let signed = self
            .context
            .sign(AwsRequestToSign {
                method: request.method.clone(),
                url: request.url.clone(),
                headers: prepared.headers.clone(),
                body: prepared.body_bytes(),
            })
            .await
            .map_err(aws_auth_error_to_auth_error)?;

        request.url = signed.url;
        request.headers = signed.headers;
        request.body = prepared.body.map(RequestBody::Raw);
        request.compression = RequestCompression::None;
        Ok(request)
    }
}

impl AuthProvider for BedrockSigV4AuthProvider {
    fn add_auth_headers(&self, _headers: &mut HeaderMap) {}

    fn apply_auth(&self, request: Request) -> codex_api::AuthProviderFuture<'_> {
        Box::pin(BedrockSigV4AuthProvider::apply_auth(self, request))
    }
}

#[cfg(test)]
mod tests {
    use codex_api::AuthProvider;
    use http::HeaderValue;
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn sdk_context_is_reused_and_managed_auth_bypasses_it() {
        // A configured bearer token legitimately bypasses SDK initialization.
        // Isolate this fixture rather than mutate process-global credentials.
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "amazon_bedrock::auth::tests::sdk_context_environment_worker",
                "--ignored",
                "--nocapture",
            ])
            .env_remove(AWS_BEARER_TOKEN_BEDROCK_ENV_VAR)
            .env("AWS_EC2_METADATA_DISABLED", "true")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("test result: ok. 1 passed"));
    }

    #[tokio::test]
    #[ignore = "Runs in a child process without ambient Bedrock bearer auth"]
    async fn sdk_context_environment_worker() {
        let aws = ModelProviderAwsAuthInfo {
            profile: Some("codex-transport-test-no-credentials".into()),
            region: Some("us-west-2".into()),
        };
        let cell = tokio::sync::OnceCell::new();
        let (first, second) = tokio::join!(
            resolve_auth_method(None, &aws, BedrockEndpoint::Runtime, &cell),
            resolve_auth_method(None, &aws, BedrockEndpoint::Runtime, &cell),
        );
        assert_eq!(first.unwrap().region(), "us-west-2");
        assert_eq!(second.unwrap().region(), "us-west-2");
        let initialized = cell.get().expect("SDK context initialized");
        resolve_auth_method(None, &aws, BedrockEndpoint::Runtime, &cell).await.unwrap();
        assert!(std::ptr::eq(initialized, cell.get().unwrap()));
        let managed = BedrockApiKeyAuth { api_key: "rotated".into(), region: "eu-west-1".into() };
        let method = resolve_auth_method(
            Some(&managed),
            &ModelProviderAwsAuthInfo { profile: None, region: None },
            BedrockEndpoint::Runtime,
            &cell,
        ).await.unwrap();
        assert_eq!(method.region(), "eu-west-1");
        assert_eq!(
            method.into_provider(BedrockEndpoint::Runtime).to_auth_headers()[http::header::AUTHORIZATION],
            "Bearer rotated",
        );
        assert_eq!(cell.get().unwrap().region(), "us-west-2");
    }

    #[tokio::test]
    #[ignore = "narrow local SDK configuration benchmark; no credential network calls"]
    async fn sdk_context_reuse_benchmark() {
        let config = codex_aws_auth::AwsAuthConfig {
            profile: Some("codex-transport-test-no-credentials".into()),
            region: Some("us-west-2".into()),
            service: "bedrock".into(),
        };
        let mut rows = Vec::new();
        for reuse in [false, true] {
            let cell = tokio::sync::OnceCell::new();
            let mut samples = Vec::new();
            for _ in 0..20 {
                let start = std::time::Instant::now();
                if reuse {
                    let _ = cell.get_or_try_init(|| AwsAuthContext::load(config.clone())).await.unwrap().clone();
                } else {
                    let _ = AwsAuthContext::load(config.clone()).await.unwrap();
                }
                samples.push(start.elapsed().as_nanos());
            }
            samples.sort_unstable();
            rows.push(serde_json::json!({"reuse": reuse, "samples": samples.len(), "median_ns": samples[10]}));
        }
        let report = serde_json::to_string(&rows).unwrap();
        eprintln!("bedrock_sdk_context_benchmark {report}");
        if let Ok(path) = std::env::var("KD4_BEDROCK_BENCHMARK_OUTPUT")
            && !path.is_empty()
        {
            std::fs::write(path, report).unwrap();
        }
    }

    fn missing_env_var(_: &'static str) -> std::result::Result<String, std::env::VarError> {
        Err(std::env::VarError::NotPresent)
    }

    #[test]
    fn bedrock_bearer_auth_prefers_configured_region_and_uses_header() {
        let token = "bedrock-api-key-test".to_string();
        let region = bearer_token_region(
            &ModelProviderAwsAuthInfo {
                profile: None,
                region: Some(" us-west-2 ".to_string()),
            },
            |name| match name {
                AWS_REGION_ENV_VAR => Ok("eu-west-1".to_string()),
                _ => Err(std::env::VarError::NotPresent),
            },
        )
        .expect("configured region should resolve");
        let provider = BearerAuthProvider {
            token: Some(token),
            account_id: None,
            is_fedramp_account: false,
        };
        let mut headers = http::HeaderMap::new();

        provider.add_auth_headers(&mut headers);

        assert_eq!(region, "us-west-2");
        assert!(
            headers
                .get(http::header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.starts_with("Bearer bedrock-api-key-"))
        );
    }

    #[test]
    fn bedrock_bearer_auth_uses_aws_region_env() {
        let region = bearer_token_region(
            &ModelProviderAwsAuthInfo {
                profile: None,
                region: None,
            },
            |name| match name {
                AWS_REGION_ENV_VAR => Ok(" eu-central-1 ".to_string()),
                _ => Err(std::env::VarError::NotPresent),
            },
        )
        .expect("AWS_REGION should resolve");

        assert_eq!(region, "eu-central-1");
    }

    #[test]
    fn bedrock_bearer_auth_uses_aws_default_region_env() {
        let region = bearer_token_region(
            &ModelProviderAwsAuthInfo {
                profile: None,
                region: None,
            },
            |name| match name {
                AWS_DEFAULT_REGION_ENV_VAR => Ok("ap-northeast-1".to_string()),
                _ => Err(std::env::VarError::NotPresent),
            },
        )
        .expect("AWS_DEFAULT_REGION should resolve");

        assert_eq!(region, "ap-northeast-1");
    }

    #[test]
    fn bedrock_bearer_auth_rejects_missing_configured_region() {
        let err = bearer_token_region(
            &ModelProviderAwsAuthInfo {
                profile: None,
                region: None,
            },
            missing_env_var,
        )
        .expect_err("missing region should fail");

        assert_eq!(
            err.to_string(),
            "Fatal error: Amazon Bedrock bearer token auth requires \
`model_providers.amazon-bedrock.aws.region`, `AWS_REGION`, or `AWS_DEFAULT_REGION`"
        );
    }

    #[test]
    fn bedrock_mantle_sigv4_strips_headers_not_preserved_by_mantle() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "session_id",
            HeaderValue::from_static("019dae79-15c3-70c3-8736-3219b8602b37"),
        );
        headers.insert(
            "thread_id",
            HeaderValue::from_static("019dae79-15c3-70c3-8736-3219b8602b37"),
        );
        headers.insert(
            "future_identity_header",
            HeaderValue::from_static("019dae79-15c3-70c3-8736-3219b8602b37"),
        );
        headers.insert(
            "x-client-request-id",
            HeaderValue::from_static("request-id"),
        );

        remove_headers_not_preserved_by_bedrock_mantle(&mut headers);

        assert!(!headers.contains_key("session_id"));
        assert!(!headers.contains_key("thread_id"));
        assert!(!headers.contains_key("future_identity_header"));
        assert_eq!(
            headers
                .get("x-client-request-id")
                .and_then(|value| value.to_str().ok()),
            Some("request-id")
        );
    }
}
