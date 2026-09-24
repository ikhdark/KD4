mod auth;
mod catalog;
mod error;
mod mantle;

use std::path::PathBuf;
use std::sync::Arc;

use codex_api::ApiError;
use codex_api::Provider;
use codex_api::SharedAuthProvider;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_login::auth::BedrockApiKeyAuth;
use codex_model_provider_info::ModelProviderAwsAuthInfo;
use codex_model_provider_info::ModelProviderInfo;
use codex_models_manager::manager::SharedModelsManager;
use codex_models_manager::manager::StaticModelsManager;
use codex_protocol::account::AmazonBedrockCredentialSource;
use codex_protocol::account::ProviderAccount;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result;
use codex_protocol::openai_models::ModelsResponse;

use crate::auth::ProviderAuthScope;
use crate::auth::ResolvedProviderAuth;
use crate::provider::ModelProvider;
use crate::provider::ModelProviderFuture;
use crate::provider::ProviderAccountResult;
use crate::provider::ProviderAccountState;
use crate::provider::ProviderCapabilities;
use crate::provider::ResolvedModelProviderClientSetup;
use auth::resolve_provider_auth;
pub(crate) use catalog::static_model_catalog;
use catalog::with_default_only_service_tier;
use mantle::runtime_base_url;

/// Runtime provider for Amazon Bedrock's OpenAI-compatible Mantle endpoint.
#[derive(Clone, Debug)]
pub(crate) struct AmazonBedrockModelProvider {
    pub(crate) info: ModelProviderInfo,
    pub(crate) aws: ModelProviderAwsAuthInfo,
    auth_manager: Option<Arc<AuthManager>>,
}

impl AmazonBedrockModelProvider {
    pub(crate) fn new(
        provider_info: ModelProviderInfo,
        auth_manager: Option<Arc<AuthManager>>,
    ) -> Self {
        let aws = provider_info
            .aws
            .clone()
            .unwrap_or(ModelProviderAwsAuthInfo {
                profile: None,
                region: None,
            });
        Self {
            info: provider_info,
            aws,
            auth_manager,
        }
    }

    fn managed_auth(&self) -> Option<BedrockApiKeyAuth> {
        self.auth_manager
            .as_ref()
            .and_then(|auth_manager| auth_manager.auth_cached())
            .and_then(|auth| match auth {
                CodexAuth::BedrockApiKey(auth) => Some(auth),
                CodexAuth::ApiKey(_)
                | CodexAuth::Chatgpt(_)
                | CodexAuth::ChatgptAuthTokens(_)
                | CodexAuth::Headers(_)
                | CodexAuth::AgentIdentity(_)
                | CodexAuth::PersonalAccessToken(_) => None,
            })
    }

    async fn auth(&self) -> Option<CodexAuth> {
        self.managed_auth().map(CodexAuth::BedrockApiKey)
    }

    async fn api_provider(&self) -> Result<Provider> {
        let managed_auth = self.managed_auth();
        let mut api_provider_info = self.info.clone();
        api_provider_info.base_url =
            Some(runtime_base_url(managed_auth.as_ref(), &self.aws).await?);
        api_provider_info.to_api_provider(/*auth_mode*/ None)
    }

    async fn runtime_base_url(&self) -> Result<Option<String>> {
        let managed_auth = self.managed_auth();
        Ok(Some(
            runtime_base_url(managed_auth.as_ref(), &self.aws).await?,
        ))
    }

    async fn api_auth(&self) -> Result<SharedAuthProvider> {
        let managed_auth = self.managed_auth();
        resolve_provider_auth(managed_auth.as_ref(), &self.aws).await
    }
}

impl ModelProvider for AmazonBedrockModelProvider {
    fn info(&self) -> &ModelProviderInfo {
        &self.info
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            namespace_tools: true,
            image_generation: false,
            web_search: false,
        }
    }

    fn auth_manager(&self) -> Option<Arc<AuthManager>> {
        self.managed_auth()
            .and_then(|_| self.auth_manager.as_ref().cloned())
    }

    fn auth(&self) -> ModelProviderFuture<'_, Option<CodexAuth>> {
        Box::pin(AmazonBedrockModelProvider::auth(self))
    }

    fn account_state(&self) -> ProviderAccountResult {
        let credential_source = if self.managed_auth().is_some() {
            AmazonBedrockCredentialSource::CodexManaged
        } else {
            AmazonBedrockCredentialSource::AwsManaged
        };
        Ok(ProviderAccountState {
            account: Some(ProviderAccount::AmazonBedrock { credential_source }),
            requires_openai_auth: false,
        })
    }

    fn map_api_error(&self, error: ApiError) -> CodexErr {
        error::map_api_error(error)
    }

    fn api_provider(&self) -> ModelProviderFuture<'_, Result<Provider>> {
        Box::pin(AmazonBedrockModelProvider::api_provider(self))
    }

    fn runtime_base_url(&self) -> ModelProviderFuture<'_, Result<Option<String>>> {
        Box::pin(AmazonBedrockModelProvider::runtime_base_url(self))
    }

    fn api_auth(&self) -> ModelProviderFuture<'_, Result<SharedAuthProvider>> {
        Box::pin(AmazonBedrockModelProvider::api_auth(self))
    }

    fn resolve_client_setup(
        &self,
        _scope: ProviderAuthScope,
    ) -> ModelProviderFuture<'_, Result<ResolvedModelProviderClientSetup>> {
        Box::pin(async move {
            let managed_auth = self.managed_auth();
            let method = auth::resolve_auth_method(managed_auth.as_ref(), &self.aws).await?;
            let mut info = self.info.clone();
            info.base_url = Some(mantle::base_url(method.region())?);
            Ok(ResolvedModelProviderClientSetup {
                auth: managed_auth.map(CodexAuth::BedrockApiKey),
                api_provider: info.to_api_provider(None)?,
                resolved_auth: ResolvedProviderAuth::new(method.into_provider()),
            })
        })
    }

    fn models_manager(
        &self,
        _model_provider_id: &str,
        _codex_home: PathBuf,
        config_model_catalog: Option<ModelsResponse>,
    ) -> SharedModelsManager {
        Arc::new(StaticModelsManager::new(
            /*auth_manager*/ None,
            config_model_catalog.map_or_else(static_model_catalog, with_default_only_service_tier),
        ))
    }
}

#[cfg(test)]
#[path = "error_tests.rs"]
mod error_tests;

#[cfg(test)]
mod tests {
    use http::HeaderValue;
    use pretty_assertions::assert_eq;

    use super::*;

    #[tokio::test]
    async fn managed_auth_takes_precedence_over_aws_auth() {
        let managed_auth = BedrockApiKeyAuth {
            api_key: "managed-bedrock-api-key".to_string(),
            region: "us-east-1".to_string(),
        };
        let auth_manager =
            AuthManager::from_auth_for_testing(CodexAuth::BedrockApiKey(managed_auth.clone()));
        let provider = AmazonBedrockModelProvider::new(
            ModelProviderInfo::create_amazon_bedrock_provider(Some(ModelProviderAwsAuthInfo {
                profile: Some("aws-profile-that-should-not-be-loaded".to_string()),
                region: Some("us-west-2".to_string()),
            })),
            Some(auth_manager.clone()),
        );

        assert!(Arc::ptr_eq(
            &provider
                .auth_manager()
                .expect("managed Bedrock auth manager should be exposed"),
            &auth_manager,
        ));
        assert_eq!(
            provider.auth().await,
            Some(CodexAuth::BedrockApiKey(managed_auth.clone()))
        );
        let setup = provider
            .resolve_client_setup(ProviderAuthScope {
                agent_identity_policy: codex_login::auth::AgentIdentityAuthPolicy::JwtOnly,
                session_source: codex_protocol::protocol::SessionSource::Cli,
                agent_identity_session_fallback: Default::default(),
            })
            .await
            .expect("combined setup");
        assert_eq!(setup.auth, Some(CodexAuth::BedrockApiKey(managed_auth)));
        assert_eq!(
            setup.api_provider.base_url,
            "https://bedrock-mantle.us-east-1.api.aws/openai/v1"
        );
        assert_eq!(
            provider.api_provider().await.unwrap().base_url,
            setup.api_provider.base_url
        );
        assert_eq!(
            setup
                .resolved_auth
                .auth
                .to_auth_headers()
                .get(http::header::AUTHORIZATION),
            Some(&HeaderValue::from_static("Bearer managed-bedrock-api-key"))
        );
        assert_eq!(
            provider.account_state(),
            Ok(ProviderAccountState {
                account: Some(ProviderAccount::AmazonBedrock {
                    credential_source: AmazonBedrockCredentialSource::CodexManaged,
                }),
                requires_openai_auth: false,
            })
        );
        assert_eq!(
            provider
                .runtime_base_url()
                .await
                .expect("managed Bedrock region should resolve"),
            Some("https://bedrock-mantle.us-east-1.api.aws/openai/v1".to_string())
        );
        assert_eq!(
            provider
                .api_auth()
                .await
                .expect("managed Bedrock auth should resolve")
                .to_auth_headers()
                .get(http::header::AUTHORIZATION),
            Some(&HeaderValue::from_static("Bearer managed-bedrock-api-key"))
        );
    }

    #[tokio::test]
    async fn openai_auth_is_not_exposed_to_bedrock() {
        let provider = AmazonBedrockModelProvider::new(
            ModelProviderInfo::create_amazon_bedrock_provider(/*aws*/ None),
            Some(AuthManager::from_auth_for_testing(CodexAuth::from_api_key(
                "openai-api-key",
            ))),
        );

        assert!(provider.auth_manager().is_none());
        assert_eq!(provider.auth().await, None);
        assert_eq!(
            provider.account_state(),
            Ok(ProviderAccountState {
                account: Some(ProviderAccount::AmazonBedrock {
                    credential_source: AmazonBedrockCredentialSource::AwsManaged,
                }),
                requires_openai_auth: false,
            })
        );
    }

    #[test]
    fn capabilities_disable_unsupported_hosted_tools() {
        let provider = AmazonBedrockModelProvider::new(
            ModelProviderInfo::create_amazon_bedrock_provider(/*aws*/ None),
            /*auth_manager*/ None,
        );

        assert_eq!(
            provider.capabilities(),
            ProviderCapabilities {
                namespace_tools: true,
                image_generation: false,
                web_search: false,
            }
        );
    }
}
