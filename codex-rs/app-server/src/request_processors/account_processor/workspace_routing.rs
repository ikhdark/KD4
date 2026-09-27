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
            self.config.enforce_residency.value(),
            auth.is_fedramp_account(),
        )
    }
}

fn routing_from_account(
    account: AccountEntry,
    residency: Option<ResidencyRequirement>,
    is_fedramp: bool,
) -> Option<WorkspaceRouting> {
    let backend_origin = account.workspace_backend_origin?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn workspace_routing_preserves_authoritative_origin_and_residency() {
        for (origin, override_, residency, fedramp, expected) in [
            (
                "https://chatgpt.com",
                "NO_CONSTRAINT",
                None,
                false,
                Some("NO_CONSTRAINT"),
            ),
            (
                "https://chatgpt.com",
                "us",
                Some(ResidencyRequirement::Us),
                false,
                Some("us"),
            ),
            (
                "https://chatgpt.com",
                "us_cr",
                Some(ResidencyRequirement::Us),
                true,
                Some("us_cr"),
            ),
            (
                "https://chatgpt.com",
                "NO_CONSTRAINT",
                Some(ResidencyRequirement::Us),
                false,
                None,
            ),
            ("https://chatgpt.com", "us", None, true, None),
            ("https://chatgpt.com", "future", None, false, None),
            ("http://chatgpt.com", "us", None, false, None),
            ("https://chatgpt.com/backend-api", "us", None, false, None),
            ("https://user:password@chatgpt.com", "us", None, false, None),
        ] {
            let account = serde_json::from_value(serde_json::json!({
                "id": "workspace", "workspace_backend_origin": origin,
                "account_routing_override": override_
            }))
            .unwrap();
            let routing = routing_from_account(account, residency, fedramp);
            assert_eq!(
                serde_json::to_value(routing).unwrap(),
                expected.map_or(serde_json::Value::Null, |value| serde_json::json!({
                    "chatgptAccountId": "workspace", "backendOrigin": origin,
                    "accountRoutingOverride": value
                })),
                "{origin}, {override_}, {residency:?}, fedramp={fedramp}"
            );
        }
        for payload in [
            serde_json::json!({"id":"workspace"}),
            serde_json::json!({"id":"workspace", "workspace_backend_origin":"https://chatgpt.com"}),
        ] {
            assert_eq!(
                routing_from_account(serde_json::from_value(payload).unwrap(), None, false),
                None
            );
        }
    }
}
