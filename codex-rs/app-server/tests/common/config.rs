use codex_features::FEATURES;
use codex_features::Feature;
use std::collections::BTreeMap;
use std::path::Path;

pub fn write_mock_responses_config_toml(
    codex_home: &Path,
    server_uri: &str,
    feature_flags: &BTreeMap<Feature, bool>,
    auto_compact_limit: i64,
    requires_openai_auth: Option<bool>,
    model_provider_id: &str,
    compact_prompt: &str,
) -> std::io::Result<()> {
    // Phase 1: build the features block for config.toml.
    let mut features = BTreeMap::new();
    for (feature, enabled) in feature_flags {
        features.insert(*feature, *enabled);
    }
    let feature_entries = features
        .into_iter()
        .map(|(feature, enabled)| {
            let key = FEATURES
                .iter()
                .find(|spec| spec.id == feature)
                .map(|spec| spec.key)
                .expect("feature should have a config key");
            format!("{key} = {enabled}")
        })
        .collect::<Vec<_>>()
        .join("\n");
    // Phase 2: build provider-specific config bits.
    let requires_line = match requires_openai_auth {
        Some(true) => "requires_openai_auth = true\n".to_string(),
        Some(false) | None => String::new(),
    };
    let provider_name = if matches!(requires_openai_auth, Some(true)) {
        "OpenAI"
    } else {
        "Mock provider for test"
    };
    let provider_block = format!(
        r#"
[model_providers.{model_provider_id}]
name = "{provider_name}"
base_url = "{server_uri}/v1"
wire_api = "responses"
request_max_retries = 0
stream_max_retries = 0
supports_websockets = false
{requires_line}
"#
    );
    let openai_base_url_line = if model_provider_id == "openai" {
        format!("openai_base_url = \"{server_uri}/v1\"\n")
    } else {
        String::new()
    };
    // Phase 3: write the final config file.
    let config_toml = codex_home.join("config.toml");
    std::fs::write(
        config_toml,
        format!(
            r#"
model = "mock-model"
approval_policy = "never"
sandbox_mode = "read-only"
compact_prompt = "{compact_prompt}"
model_auto_compact_token_limit = {auto_compact_limit}

model_provider = "{model_provider_id}"
{openai_base_url_line}

[features]
{feature_entries}
{provider_block}
"#
        ),
    )
}

/// Writes the minimal config that routes Codex to a mock Responses server.
pub fn write_mock_provider_config_toml(codex_home: &Path, server_uri: &str) -> std::io::Result<()> {
    write_mock_provider_config(codex_home, server_uri, /*extra_settings*/ "")
}

pub fn write_mock_responses_config_toml_with_chatgpt_base_url(
    codex_home: &Path,
    server_uri: &str,
    chatgpt_base_url: &str,
) -> std::io::Result<()> {
    write_mock_provider_config(
        codex_home,
        server_uri,
        &format!("chatgpt_base_url = \"{chatgpt_base_url}\"\n"),
    )
}

/// Writes the minimal mock-provider config backed by an in-memory thread
/// store. The provider is unreachable because these tests never sample.
pub fn write_in_memory_thread_store_config_toml(
    codex_home: &Path,
    store_id: &str,
) -> std::io::Result<()> {
    write_mock_provider_config(
        codex_home,
        "http://127.0.0.1:1",
        &format!("experimental_thread_store = {{ type = \"in_memory\", id = \"{store_id}\" }}\n"),
    )
}

fn write_mock_provider_config(
    codex_home: &Path,
    server_uri: &str,
    extra_settings: &str,
) -> std::io::Result<()> {
    std::fs::write(
        codex_home.join("config.toml"),
        format!(
            r#"model = "mock-model"
approval_policy = "never"
sandbox_mode = "read-only"
{extra_settings}
model_provider = "mock_provider"

[model_providers.mock_provider]
name = "Mock provider for test"
base_url = "{server_uri}/v1"
wire_api = "responses"
request_max_retries = 0
stream_max_retries = 0
"#
        ),
    )
}
