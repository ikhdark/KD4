use super::AccountRequestProcessor;
use codex_app_server_protocol::AccountRoutingOverride;
use codex_app_server_protocol::WorkspaceRouting;
use codex_backend_client::AccountEntry;
use codex_config::ResidencyRequirement;
use std::time::Duration;

impl AccountRequestProcessor {
    pub(super) async fn workspace_routing(&self) -> Option<WorkspaceRouting> {
        let auth = self.auth_manager.auth_cached()?;
        if !auth.is_chatgpt_auth() {
            return None;
        }
        let account_id = auth.get_account_id().filter(|id| !id.is_empty())?;
        let client = self.backend_client_for_auth(&auth);
        let accounts = match tokio::time::timeout(
            Duration::from_secs(10),
            client.get_accounts_check(),
        )
        .await
        {
            Ok(Ok(accounts)) => accounts,
            _ => {
                tracing::warn!("workspace routing discovery failed; preserving account status");
                return None;
            }
        };
        // Never publish routing discovered for a principal that has since signed out or changed.
        let current = self.auth_manager.auth_cached()?;
        if current.get_account_id() != Some(account_id.clone())
            || current.get_chatgpt_user_id() != auth.get_chatgpt_user_id()
            || self
                .auth_manager
                .refresh_failure_for_auth(&current)
                .is_some()
        {
            return None;
        }
        let account = accounts
            .accounts
            .into_iter()
            .find(|account| account.id == account_id)?;
        routing_from_account(
            account,
            &self.config.chatgpt_base_url,
            self.config.enforce_residency.value(),
            auth.is_fedramp_account(),
        )
    }
}

fn routing_from_account(
    account: AccountEntry,
    chatgpt_base_url: &str,
    residency: Option<ResidencyRequirement>,
    is_fedramp: bool,
) -> Option<WorkspaceRouting> {
    let backend_origin = match account.workspace_backend_origin?.as_str() {
        // The accounts API uses this sentinel when the workspace does not
        // override the configured backend. Desktop still requires an origin URL.
        "NO_CONSTRAINT" => {
            let base_url = url::Url::parse(chatgpt_base_url).ok()?;
            if base_url.scheme() != "https"
                || !base_url.username().is_empty()
                || base_url.password().is_some()
            {
                return None;
            }
            base_url.origin().ascii_serialization()
        }
        origin => origin.to_string(),
    };
    let url = url::Url::parse(&backend_origin).ok()?;
    if url.scheme() != "https" || url.origin().ascii_serialization() != backend_origin {
        return None;
    }
    let account_routing_override = match account.account_routing_override.as_deref()? {
        "NO_CONSTRAINT" => AccountRoutingOverride::NoConstraint,
        "us" => AccountRoutingOverride::Us,
        "us_cr" => AccountRoutingOverride::UsCr,
        _ => return None,
    };
    if (residency == Some(ResidencyRequirement::Us)
        && account_routing_override == AccountRoutingOverride::NoConstraint)
        || (is_fedramp && account_routing_override != AccountRoutingOverride::UsCr)
    {
        return None;
    }
    Some(WorkspaceRouting {
        chatgpt_account_id: account.id,
        backend_origin,
        account_routing_override,
    })
}

