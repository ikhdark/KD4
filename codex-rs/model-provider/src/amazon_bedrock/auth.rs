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

/// Planning can use only region data already resolved by the request auth owner.
/// Unknown profile-only regions remain ineligible until SDK context initialization.
pub(super) fn known_region(
    managed_auth: Option<&BedrockApiKeyAuth>,
    aws: &ModelProviderAwsAuthInfo,
    sdk_context: &tokio::sync::OnceCell<AwsAuthContext>,
) -> Option<String> {
    if let Some(auth) = managed_auth {
        return Some(auth.region.clone());
    }
    if non_empty_env_var_from(AWS_BEARER_TOKEN_BEDROCK_ENV_VAR, std::env::var).is_some() {
        return bearer_token_region(aws, std::env::var).ok();
    }
    sdk_context
        .get()
        .map(|context| context.region().to_string())
        .or_else(|| bearer_token_region(aws, std::env::var).ok())
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

    // Cache the SDK context, not credentials. Signing still asks its refreshable
    // provider for credentials on every request. Failed/cancelled loads are retried.
    let context = sdk_context
        .get_or_try_init(|| {
            let config = match endpoint {
                BedrockEndpoint::Mantle => aws_auth_config(aws),
                BedrockEndpoint::Runtime => runtime::aws_auth_config(aws),
            };
            AwsAuthContext::load(config)
        })
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

