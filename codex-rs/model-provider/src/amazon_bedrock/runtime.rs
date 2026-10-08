use codex_aws_auth::AwsAuthConfig;
use codex_model_provider_info::ModelProviderAwsAuthInfo;

use super::mantle::region_from_config;

const BEDROCK_RUNTIME_SERVICE_NAME: &str = "bedrock";

pub(super) fn aws_auth_config(aws: &ModelProviderAwsAuthInfo) -> AwsAuthConfig {
    AwsAuthConfig {
        profile: aws.profile.clone(),
        region: region_from_config(aws),
        service: BEDROCK_RUNTIME_SERVICE_NAME.to_string(),
    }
}

pub(super) fn base_url(region: &str) -> String {
    format!("https://bedrock-runtime.{region}.amazonaws.com/openai/v1")
}

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;
