use codex_core::config::Config;
use codex_login::AuthManager;
use serde::Deserialize;

use crate::chatgpt_client::chatgpt_get_request;
use crate::chatgpt_client::chatgpt_http_clients;

#[derive(Debug, Deserialize)]
pub struct GetTaskResponse {
    pub current_diff_task_turn: Option<AssistantTurn>,
}

// Only relevant fields for our extraction
#[derive(Debug, Deserialize)]
pub struct AssistantTurn {
    pub output_items: Vec<OutputItem>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum OutputItem {
    #[serde(rename = "pr")]
    Pr(PrOutputItem),

    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
pub struct PrOutputItem {
    pub output_diff: OutputDiff,
}

#[derive(Debug, Deserialize)]
pub struct OutputDiff {
    pub diff: String,
}

pub(crate) async fn get_task(config: &Config, task_id: String) -> anyhow::Result<GetTaskResponse> {
    let auth_manager =
        AuthManager::shared_from_config(config, /*enable_codex_api_key_env*/ false).await;
    let auth = auth_manager
        .auth()
        .await
        .ok_or_else(|| anyhow::anyhow!("ChatGPT auth not available"))?;
    let http_clients = chatgpt_http_clients(config);
    let task_id = crate::workspace_settings::encode_path_segment(&task_id);
    let path = format!("/wham/tasks/{task_id}");
    chatgpt_get_request(&config.chatgpt_base_url, &auth, &http_clients, path).await
}

#[cfg(test)]
mod tests {
    use super::get_task;
    use codex_core::config::ConfigBuilder;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    #[tokio::test]
    async fn get_task_encodes_id_as_one_path_segment() {
        let home = tempfile::tempdir().expect("Codex home");
        let cwd = tempfile::tempdir().expect("cwd");
        let mut config = ConfigBuilder::default()
            .codex_home(home.path().to_path_buf())
            .fallback_cwd(Some(cwd.path().to_path_buf()))
            .build()
            .await
            .expect("config");
        codex_login::auth::login_with_chatgpt_auth_tokens(
            home.path(),
            "e30.e30.signature",
            "test-account",
            Some("enterprise"),
        )
        .expect("register auth");
        let server = MockServer::start().await;
        config.chatgpt_base_url = server.uri();
        Mock::given(method("GET"))
            .and(path("/wham/tasks/task_%2Fa%3Fb%23c%25%20d%C3%A9"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"current_diff_task_turn":{"output_items":[]}}"#),
            )
            .expect(1)
            .mount(&server)
            .await;
        let task = get_task(&config, "task_/a?b#c% dé".to_string())
            .await
            .expect("get task");
        assert!(
            task.current_diff_task_turn
                .expect("turn")
                .output_items
                .is_empty()
        );
        let requests = server.received_requests().await.expect("requests");
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].url.query(), None);
        assert_eq!(requests[0].url.fragment(), None);
        server.verify().await;
    }
}
