use codex_aws_auth::AwsAuthConfig;
use codex_model_provider_info::ModelProviderAwsAuthInfo;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result;

const BEDROCK_MANTLE_SERVICE_NAME: &str = "bedrock-mantle";
const BEDROCK_MANTLE_SUPPORTED_REGIONS: [&str; 12] = [
    "us-east-2",
    "us-east-1",
    "us-west-2",
    "ap-southeast-3",
    "ap-south-1",
    "ap-northeast-1",
    "eu-central-1",
    "eu-west-1",
    "eu-west-2",
    "eu-south-1",
    "eu-north-1",
    "sa-east-1",
];

pub(super) fn aws_auth_config(aws: &ModelProviderAwsAuthInfo) -> AwsAuthConfig {
    AwsAuthConfig {
        profile: aws.profile.clone(),
        region: region_from_config(aws),
        service: BEDROCK_MANTLE_SERVICE_NAME.to_string(),
    }
}

pub(super) fn region_from_config(aws: &ModelProviderAwsAuthInfo) -> Option<String> {
    aws.region
        .as_deref()
        .map(str::trim)
        .filter(|region| !region.is_empty())
        .map(str::to_string)
}

pub(super) fn base_url(region: &str) -> Result<String> {
    if BEDROCK_MANTLE_SUPPORTED_REGIONS.contains(&region) {
        Ok(format!("https://bedrock-mantle.{region}.api.aws/openai/v1"))
    } else {
        Err(CodexErr::Fatal(format!(
            "Amazon Bedrock Mantle does not support region `{region}`"
        )))
    }
}

