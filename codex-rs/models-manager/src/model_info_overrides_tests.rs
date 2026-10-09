use crate::ModelsManagerConfig;
use crate::manager::ModelsManager;
use codex_protocol::openai_models::TruncationPolicyConfig;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

use super::TestModelsEndpoint;
use super::openai_manager_for_tests;

#[tokio::test]
async fn offline_model_info_preserves_or_overrides_tool_output_limits() {
    let codex_home = TempDir::new().expect("create temp dir");
    let manager = openai_manager_for_tests(
        codex_home.path().to_path_buf(),
        TestModelsEndpoint::new(Vec::new()),
    );

    for (limit, tokens, bytes) in [(None, 10_000, 10_000), (Some(123), 123, 492)] {
        let config = ModelsManagerConfig {
            tool_output_token_limit: limit,
            ..Default::default()
        };
        assert_eq!(
            manager.get_model_info("gpt-5.5", &config).await.truncation_policy,
            TruncationPolicyConfig::tokens(tokens)
        );
        assert_eq!(
            manager.get_model_info("gpt-5.2", &config).await.truncation_policy,
            TruncationPolicyConfig::bytes(bytes)
        );
    }
}
