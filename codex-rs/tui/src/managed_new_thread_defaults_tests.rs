use super::*;
use crate::legacy_core::config::ConfigBuilder;
use codex_protocol::openai_models::ReasoningEffort;
use pretty_assertions::assert_eq;

async fn test_config() -> (tempfile::TempDir, Config) {
    let codex_home = tempfile::tempdir().expect("tempdir");
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .build()
        .await
        .expect("config");
    (codex_home, config)
}

fn defaults() -> NewThreadModelDefaults {
    NewThreadModelDefaults {
        model: Some("managed-model".to_string()),
        model_reasoning_effort: Some(ReasoningEffort::High),
        service_tier: Some("priority".to_string()),
    }
}

#[tokio::test]
async fn applies_managed_defaults_to_a_new_thread_config() {
    let (_codex_home, mut actual) = test_config().await;
    actual.model = Some("configured-model".to_string());
    actual.model_reasoning_effort = Some(ReasoningEffort::Low);
    actual.service_tier = Some("flex".to_string());
    let mut expected = actual.clone();
    expected.model = Some("managed-model".to_string());
    expected.model_reasoning_effort = Some(ReasoningEffort::High);
    expected.service_tier = Some(ServiceTier::Fast.request_value().to_string());

    apply_managed_new_thread_defaults(
        &mut actual,
        Some(&defaults()),
        &[],
        &ConfigOverrides::default(),
    );

    assert_eq!(actual, expected);
}

#[tokio::test]
async fn explicit_model_skips_managed_model_and_reasoning_effort() {
    let (_codex_home, mut actual) = test_config().await;
    actual.model = Some("explicit-model".to_string());
    actual.model_reasoning_effort = None;
    actual.service_tier = Some("flex".to_string());
    let mut expected = actual.clone();
    expected.service_tier = Some(ServiceTier::Fast.request_value().to_string());
    let harness_overrides = ConfigOverrides {
        model: Some("explicit-model".to_string()),
        ..ConfigOverrides::default()
    };

    apply_managed_new_thread_defaults(&mut actual, Some(&defaults()), &[], &harness_overrides);

    assert_eq!(actual, expected);
}

#[tokio::test]
async fn explicit_reasoning_effort_skips_managed_model_and_reasoning_effort() {
    let (_codex_home, mut actual) = test_config().await;
    actual.model = Some("configured-model".to_string());
    actual.model_reasoning_effort = Some(ReasoningEffort::Low);
    actual.service_tier = Some("flex".to_string());
    let mut expected = actual.clone();
    expected.service_tier = Some(ServiceTier::Fast.request_value().to_string());
    let cli_kv_overrides = vec![(
        "model_reasoning_effort".to_string(),
        TomlValue::String("low".to_string()),
    )];

    apply_managed_new_thread_defaults(
        &mut actual,
        Some(&defaults()),
        &cli_kv_overrides,
        &ConfigOverrides::default(),
    );

    assert_eq!(actual, expected);
}

#[tokio::test]
async fn explicit_launch_overrides_take_precedence() {
    let (_codex_home, mut actual) = test_config().await;
    actual.model = Some("explicit-model".to_string());
    actual.model_reasoning_effort = Some(ReasoningEffort::Low);
    actual.service_tier = Some("flex".to_string());
    let expected = actual.clone();
    let cli_kv_overrides = vec![(
        "model_reasoning_effort".to_string(),
        TomlValue::String("low".to_string()),
    )];
    let harness_overrides = ConfigOverrides {
        model: Some("explicit-model".to_string()),
        service_tier: Some(Some("flex".to_string())),
        ..ConfigOverrides::default()
    };

    apply_managed_new_thread_defaults(
        &mut actual,
        Some(&defaults()),
        &cli_kv_overrides,
        &harness_overrides,
    );

    assert_eq!(actual, expected);
}

#[tokio::test]
async fn generic_model_and_service_tier_overrides_win() {
    let (_home, mut actual) = test_config().await;
    actual.model = Some("chosen".into());
    actual.model_reasoning_effort = Some(ReasoningEffort::Low);
    actual.service_tier = Some("flex".into());
    let expected = actual.clone();
    let overrides = vec![
        ("model".into(), TomlValue::String("chosen".into())),
        ("service_tier".into(), TomlValue::String("flex".into())),
    ];
    apply_managed_new_thread_defaults(
        &mut actual,
        Some(&defaults()),
        &overrides,
        &ConfigOverrides::default(),
    );
    assert_eq!(actual, expected);
}

#[tokio::test]
async fn explicitly_cleared_service_tier_stays_clear() {
    let (_home, mut actual) = test_config().await;
    actual.service_tier = None;
    let mut expected = actual.clone();
    expected.model = defaults().model;
    expected.model_reasoning_effort = defaults().model_reasoning_effort;
    apply_managed_new_thread_defaults(
        &mut actual,
        Some(&defaults()),
        &[],
        &ConfigOverrides {
            service_tier: Some(None),
            ..ConfigOverrides::default()
        },
    );
    assert_eq!(actual, expected);
}
