use super::AccountRequestProcessor;
use codex_app_server_protocol::AccountRoutingOverride;
use codex_app_server_protocol::WorkspaceRouting;
use codex_backend_client::AccountEntry;
use codex_config::ResidencyRequirement;
use codex_login::CodexAuth;
use sha2::Digest;
use sha2::Sha256;
use std::time::Duration;
use tokio::time::Instant;

const ROUTING_TIMEOUT: Duration = Duration::from_secs(1);
const ROUTING_CACHE_TTL: Duration = Duration::from_secs(30);
const ROUTING_FAILURE_TTL: Duration = Duration::from_secs(5);

#[derive(PartialEq, Eq)]
struct RoutingPrincipal {
    account_id: String,
    user_id: Option<String>,
    token_hash: [u8; 32],
    fedramp: bool,
}

impl RoutingPrincipal {
    fn for_auth(auth: &CodexAuth) -> Option<Self> {
        if !auth.is_chatgpt_auth() {
            return None;
        }
        Some(Self {
            account_id: auth.get_account_id().filter(|id| !id.is_empty())?,
            user_id: auth.get_chatgpt_user_id(),
            token_hash: Sha256::digest(auth.get_token().ok()?.as_bytes()).into(),
            fedramp: auth.is_fedramp_account(),
        })
    }
}

pub(super) struct CachedWorkspaceRouting {
    principal: RoutingPrincipal,
    expires: Instant,
    routing: Option<WorkspaceRouting>,
}

impl AccountRequestProcessor {
    #[expect(clippy::await_holding_invalid_type, reason = "serialize bounded routing lookups to coalesce cache misses")]
    pub(super) async fn workspace_routing(&self, force_refresh: bool) -> Option<WorkspaceRouting> {
        let auth = self.auth_manager.auth_cached()?;
        let principal = RoutingPrincipal::for_auth(&auth)?;
        if self.auth_manager.refresh_failure_for_auth(&auth).is_some() {
            return None;
        }
        // One entry per processor: backend/residency are immutable here. The
        // mutex coalesces concurrent misses rather than spawning duplicate HTTP.
        let mut cached = self.workspace_routing_cache.lock().await;
        let current = self.auth_manager.auth_cached()?;
        if RoutingPrincipal::for_auth(&current).as_ref() != Some(&principal)
            || self.auth_manager.refresh_failure_for_auth(&current).is_some()
        {
            return None;
        }
        if !force_refresh
            && let Some(entry) = cached.as_ref()
            && entry.principal == principal
            && entry.expires > Instant::now()
        {
            return entry.routing.clone();
        }
        let client = self.backend_client_for_auth(&auth);
        let accounts = match tokio::time::timeout(ROUTING_TIMEOUT, client.get_accounts_check()).await {
            Ok(Ok(accounts)) => Some(accounts),
            _ => {
                tracing::warn!("workspace routing discovery failed; preserving account status");
                None
            }
        };
        // Never publish routing discovered for a principal that has since signed out or changed.
        let current = self.auth_manager.auth_cached()?;
        if RoutingPrincipal::for_auth(&current).as_ref() != Some(&principal)
            || self
                .auth_manager
                .refresh_failure_for_auth(&current)
                .is_some()
        {
            return None;
        }
        let routing = accounts
            .and_then(|accounts| {
                accounts.accounts.into_iter().find(|account| account.id == principal.account_id)
            })
            .and_then(|account| {
                routing_from_account(account, self.config.enforce_residency.value(), principal.fedramp)
            });
        let ttl = if routing.is_some() {
            ROUTING_CACHE_TTL
        } else {
            ROUTING_FAILURE_TTL
        };
        *cached = Some(CachedWorkspaceRouting {
            principal,
            expires: Instant::now() + ttl,
            routing: routing.clone(),
        });
        routing
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
