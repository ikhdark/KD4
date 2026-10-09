use super::*;
use codex_plugin::AppConnectorId;
use pretty_assertions::assert_eq;
use std::collections::HashMap;

fn app(name: &str) -> AppDeclaration {
    AppDeclaration {
        name: name.to_string(),
        connector_id: AppConnectorId(format!("connector_{name}")),
        category: None,
    }
}

fn mcp_servers(mcp_servers: impl IntoIterator<Item = (&'static str, i32)>) -> HashMap<String, i32> {
    mcp_servers
        .into_iter()
        .map(|(name, value)| (name.to_string(), value))
        .collect::<HashMap<_, _>>()
}

#[test]
fn app_mcp_routing_tracks_auth_mode_and_plugin_activation() {
    for (auth_mode, route_available) in [
        (Some(AuthMode::Chatgpt), true),
        (Some(AuthMode::AgentIdentity), true),
        (Some(AuthMode::ApiKey), false),
        (None, false),
    ] {
        assert_eq!(apps_route_available(auth_mode), route_available);
        for plugin_active in [false, true] {
            for declared_apps in [
                vec![],
                vec![app("linear")],
                vec![app("linear"), app("notion")],
            ] {
                let mut apps = declared_apps.clone();
                let original_servers = mcp_servers([("linear", 1), ("docs", 2), ("notion", 3)]);
                let mut servers = original_servers.clone();

                apply_app_mcp_routing_policy(
                    &mut apps,
                    &mut servers,
                    auth_mode,
                    plugin_active,
                );

                assert_eq!(
                    apps,
                    if route_available {
                        declared_apps.clone()
                    } else {
                        vec![]
                    },
                    "auth={auth_mode:?}, active={plugin_active}"
                );
                let mut expected_servers = original_servers;
                if route_available && plugin_active && !declared_apps.is_empty() {
                    expected_servers.remove("linear");
                    if declared_apps.len() == 2 {
                        expected_servers.remove("notion");
                    }
                }
                assert_eq!(
                    servers, expected_servers,
                    "auth={auth_mode:?}, active={plugin_active}"
                );
            }
        }
    }
}
