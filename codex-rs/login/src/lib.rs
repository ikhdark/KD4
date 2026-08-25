pub mod auth;
pub mod auth_env_telemetry;
pub mod test_support;
pub mod token_data;

mod callback_params;
mod device_code_auth;
mod outbound_proxy;
mod pkce;
mod server;
mod success_page;

pub(crate) fn form_urlencode<I, K, V>(pairs: I) -> String
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: AsRef<str>,
{
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in pairs {
        serializer.append_pair(key.as_ref(), value.as_ref());
    }
    serializer.finish()
}

pub use callback_params::LoginCallbackResult;
pub use callback_params::LoginOnboardingEntrypoint;
pub use codex_config::types::AuthCredentialsStoreMode;
pub use codex_http_client::BuildCustomCaTransportError as BuildLoginHttpClientError;
pub use device_code_auth::DeviceCode;
pub use device_code_auth::complete_device_code_login;
pub use device_code_auth::request_device_code;
pub use device_code_auth::run_device_code_login;
pub use server::LoginServer;
pub use server::ServerOptions;
pub use server::ShutdownHandle;
pub use server::run_login_server;
pub use success_page::CODEX_OPEN_APP_URL;
pub use success_page::LoginSuccessPage;
pub use success_page::LoginSuccessPageBrand;

pub use auth::AgentIdentityAuthPolicy;
pub use auth::AuthConfig;
pub use auth::AuthDotJson;
pub use auth::AuthHeaders;
pub use auth::AuthKeyringBackendKind;
pub use auth::AuthManager;
pub use auth::AuthManagerConfig;
pub use auth::CLIENT_ID;
pub use auth::CLIENT_ID_OVERRIDE_ENV_VAR;
pub use auth::CODEX_ACCESS_TOKEN_ENV_VAR;
pub use auth::CODEX_API_KEY_ENV_VAR;
pub use auth::CodexAuth;
pub use auth::ExternalAuth;
pub use auth::ExternalAuthFuture;
pub use auth::ExternalAuthRefreshContext;
pub use auth::ExternalAuthRefreshReason;
pub use auth::OPENAI_API_KEY_ENV_VAR;
pub use auth::REFRESH_TOKEN_URL_OVERRIDE_ENV_VAR;
pub use auth::REVOKE_TOKEN_URL_OVERRIDE_ENV_VAR;
pub use auth::RefreshTokenError;
pub use auth::UnauthorizedRecovery;
pub use auth::default_client;
pub use auth::enforce_login_restrictions;
pub use auth::load_auth_dot_json;
pub use auth::login_with_access_token;
pub use auth::login_with_api_key;
pub use auth::login_with_bedrock_api_key;
pub use auth::logout;
pub use auth::logout_with_revoke;
pub use auth::oauth_client_id;
pub use auth::read_codex_access_token_from_env;
pub use auth::read_openai_api_key_from_env;
pub use auth::save_auth;
pub use auth_env_telemetry::AuthEnvTelemetry;
pub use auth_env_telemetry::collect_auth_env_telemetry;
pub use outbound_proxy::AuthRouteConfig;
pub use token_data::TokenData;

#[cfg(test)]
mod tests {
    #[test]
    fn form_urlencode_uses_html_form_encoding() {
        assert_eq!(
            super::form_urlencode([
                ("label", "two words"),
                ("redirect_uri", "http://localhost/callback?a=b&c=d"),
            ]),
            "label=two+words&redirect_uri=http%3A%2F%2Flocalhost%2Fcallback%3Fa%3Db%26c%3Dd"
        );
    }
}
