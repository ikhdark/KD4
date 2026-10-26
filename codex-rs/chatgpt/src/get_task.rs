use codex_core::config::Config;
use codex_login::AuthManager;

use crate::chatgpt_client::chatgpt_get_request;
use crate::chatgpt_client::chatgpt_http_clients;

/// Task details as modeled for `codex cloud`, so `codex apply` finds the same diff.
pub use codex_backend_client::CodeTaskDetailsResponse as GetTaskResponse;

pub(crate) async fn get_task(config: &Config, task_id: String) -> anyhow::Result<GetTaskResponse> {
    anyhow::ensure!(
        !matches!(task_id.as_str(), "" | "." | ".."),
        "Task ID must not be empty or a URL dot segment"
    );
    let auth_manager =
        AuthManager::shared_from_config(config, /*enable_codex_api_key_env*/ false).await;
    let auth = auth_manager
        .auth()
        .await
        .ok_or_else(|| anyhow::anyhow!("ChatGPT auth not available"))?;
    let http_clients = chatgpt_http_clients(config);
    let encoded = crate::workspace_settings::encode_path_segment(&task_id);
    let path = format!("/wham/tasks/{encoded}");
    let details: GetTaskResponse =
        chatgpt_get_request(&config.chatgpt_base_url, &auth, &http_clients, path).await?;
    details.validate_task_id(&task_id)?;
    Ok(details)
}

