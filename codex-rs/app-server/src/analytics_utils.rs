use std::sync::Arc;

use codex_analytics::AnalyticsEventsClient;
use codex_core::config::Config;
use codex_login::AuthManager;

pub(crate) fn analytics_events_client_from_config(
    auth_manager: Arc<AuthManager>,
    config: &Config,
) -> AnalyticsEventsClient {
    AnalyticsEventsClient::new(
        auth_manager,
        config.chatgpt_base_url.clone(),
        Some(app_server_analytics_enabled(config.analytics_enabled)),
        config.http_client_factory(),
    )
}

fn app_server_analytics_enabled(configured: Option<bool>) -> bool {
    configured.unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::app_server_analytics_enabled;

    #[test]
    fn unset_analytics_uses_the_app_server_disabled_default() {
        assert!(!app_server_analytics_enabled(None));
        assert!(!app_server_analytics_enabled(Some(false)));
        assert!(app_server_analytics_enabled(Some(true)));
    }
}
