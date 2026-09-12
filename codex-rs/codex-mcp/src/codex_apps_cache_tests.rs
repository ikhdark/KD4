use super::*;
use crate::mcp::CODEX_APPS_MCP_SERVER_NAME;
use crate::tools::ToolInfo;
use codex_protocol::mcp::McpServerInfo;
use pretty_assertions::assert_eq;
use rmcp::model::JsonObject;
use rmcp::model::Tool;

use std::path::PathBuf;
use std::sync::Arc;
use tempfile::tempdir;

fn create_test_tool(server_name: &str, tool_name: &str) -> ToolInfo {
    ToolInfo {
        server_name: server_name.to_string(),
        supports_parallel_tool_calls: false,
        server_origin: None,
        callable_name: tool_name.to_string(),
        callable_namespace: server_name.to_string(),
        namespace_description: None,
        tool: Tool::new(
            tool_name.to_string(),
            format!("Test tool: {tool_name}"),
            Arc::new(JsonObject::default()),
        ),
        connector_id: None,
        connector_name: None,
        plugin_display_names: Vec::new(),
    }
}

fn create_test_tool_with_connector(
    server_name: &str,
    tool_name: &str,
    connector_id: &str,
    connector_name: Option<&str>,
) -> ToolInfo {
    let mut tool = create_test_tool(server_name, tool_name);
    tool.connector_id = Some(connector_id.to_string());
    tool.connector_name = connector_name.map(ToOwned::to_owned);
    tool
}

async fn create_codex_apps_tools_cache_context(
    codex_home: PathBuf,
    account_id: Option<&str>,
    chatgpt_user_id: Option<&str>,
) -> CodexAppsToolsCacheContext {
    CodexAppsToolsCache::default()
        .context(
            codex_home,
            CodexAppsToolsCacheKey {
                account_id: account_id.map(ToOwned::to_owned),
                chatgpt_user_id: chatgpt_user_id.map(ToOwned::to_owned),
                is_workspace_account: false,
                chatgpt_base_url: "https://chatgpt.com".to_string(),
                product_sku: DEFAULT_CODEX_APPS_MCP_PRODUCT_SKU.to_string(),
            },
        )
        .await
}

fn create_test_server_info(title: &str) -> McpServerInfo {
    McpServerInfo {
        name: "codex-apps".to_string(),
        title: Some(title.to_string()),
        version: "1.0.0".to_string(),
        description: None,
        icons: None,
        website_url: None,
    }
}

#[tokio::test]
async fn codex_apps_tools_cache_is_overwritten_by_last_write() {
    let codex_home = tempdir().expect("tempdir");
    let cache_context = create_codex_apps_tools_cache_context(
        codex_home.path().to_path_buf(),
        Some("account-one"),
        Some("user-one"),
    )
    .await;
    let tools_gateway_1 = vec![create_test_tool(CODEX_APPS_MCP_SERVER_NAME, "one")];
    let tools_gateway_2 = vec![create_test_tool(CODEX_APPS_MCP_SERVER_NAME, "two")];

    write_cached_codex_apps_tools(&cache_context, &tools_gateway_1).expect("write first cache");
    let cached_gateway_1 =
        read_cached_codex_apps_tools(&cache_context).expect("cache entry exists for first write");
    assert_eq!(cached_gateway_1[0].callable_name, "one");

    write_cached_codex_apps_tools(&cache_context, &tools_gateway_2).expect("write second cache");
    let cached_gateway_2 =
        read_cached_codex_apps_tools(&cache_context).expect("cache entry exists for second write");
    assert_eq!(cached_gateway_2[0].callable_name, "two");
}

#[tokio::test]
async fn codex_apps_tools_cache_is_scoped_per_user() {
    let codex_home = tempdir().expect("tempdir");
    let cache_context_user_1 = create_codex_apps_tools_cache_context(
        codex_home.path().to_path_buf(),
        Some("account-one"),
        Some("user-one"),
    )
    .await;
    let cache_context_user_2 = create_codex_apps_tools_cache_context(
        codex_home.path().to_path_buf(),
        Some("account-two"),
        Some("user-two"),
    )
    .await;
    let tools_user_1 = vec![create_test_tool(CODEX_APPS_MCP_SERVER_NAME, "one")];
    let tools_user_2 = vec![create_test_tool(CODEX_APPS_MCP_SERVER_NAME, "two")];

    write_cached_codex_apps_tools(&cache_context_user_1, &tools_user_1)
        .expect("write user one cache");
    write_cached_codex_apps_tools(&cache_context_user_2, &tools_user_2)
        .expect("write user two cache");

    let read_user_1 =
        read_cached_codex_apps_tools(&cache_context_user_1).expect("cache entry for user one");
    let read_user_2 =
        read_cached_codex_apps_tools(&cache_context_user_2).expect("cache entry for user two");

    assert_eq!(read_user_1[0].callable_name, "one");
    assert_eq!(read_user_2[0].callable_name, "two");
    assert_ne!(
        cache_context_user_1.tools_cache_path(),
        cache_context_user_2.tools_cache_path(),
        "each user should get an isolated cache file"
    );
}

#[tokio::test]
async fn codex_apps_tools_cache_is_scoped_per_endpoint_and_product() {
    let codex_home = tempdir().expect("tempdir");
    let cache = CodexAppsToolsCache::default();
    let endpoint_one = cache
        .context(
            codex_home.path().to_path_buf(),
            codex_apps_tools_cache_key(None, "https://one.example/", None),
        )
        .await;
    let endpoint_two = cache
        .context(
            codex_home.path().to_path_buf(),
            codex_apps_tools_cache_key(None, "https://two.example", None),
        )
        .await;
    let other_product = cache
        .context(
            codex_home.path().to_path_buf(),
            codex_apps_tools_cache_key(None, "https://one.example", Some("other-product")),
        )
        .await;
    let tools = vec![create_test_tool(CODEX_APPS_MCP_SERVER_NAME, "one")];

    endpoint_one.store_current_tools_for_test(tools.clone());
    write_cached_codex_apps_tools(&endpoint_one, &tools).expect("write endpoint one cache");

    assert!(endpoint_two.current_tools().is_none());
    assert!(other_product.current_tools().is_none());
    assert!(read_cached_codex_apps_tools(&endpoint_two).is_none());
    assert!(read_cached_codex_apps_tools(&other_product).is_none());
    assert_ne!(
        endpoint_one.tools_cache_path(),
        endpoint_two.tools_cache_path()
    );
    assert_ne!(
        endpoint_one.tools_cache_path(),
        other_product.tools_cache_path()
    );
    assert_ne!(
        endpoint_one.server_info_cache_path(),
        endpoint_two.server_info_cache_path()
    );
    assert_ne!(
        endpoint_one.server_info_cache_path(),
        other_product.server_info_cache_path()
    );
}

#[tokio::test]
async fn codex_apps_tools_cache_preserves_formerly_disallowed_connectors() {
    let codex_home = tempdir().expect("tempdir");
    let cache_context = create_codex_apps_tools_cache_context(
        codex_home.path().to_path_buf(),
        Some("account-one"),
        Some("user-one"),
    )
    .await;
    let tools = vec![
        create_test_tool_with_connector(
            CODEX_APPS_MCP_SERVER_NAME,
            "formerly_blocked_tool",
            "connector_2b0a9009c9c64bf9933a3dae3f2b1254",
            Some("Formerly Blocked"),
        ),
        create_test_tool_with_connector(
            CODEX_APPS_MCP_SERVER_NAME,
            "calendar_tool",
            "calendar",
            Some("Calendar"),
        ),
    ];

    write_cached_codex_apps_tools(&cache_context, &tools).expect("write cache");
    let cached = read_cached_codex_apps_tools(&cache_context).expect("cache entry exists for user");

    assert_eq!(
        cached
            .iter()
            .map(|tool| (tool.callable_name.as_str(), tool.connector_id.as_deref()))
            .collect::<Vec<_>>(),
        vec![
            (
                "formerly_blocked_tool",
                Some("connector_2b0a9009c9c64bf9933a3dae3f2b1254")
            ),
            ("calendar_tool", Some("calendar")),
        ]
    );
}

#[tokio::test]
async fn codex_apps_tools_cache_is_ignored_when_schema_version_mismatches() {
    let codex_home = tempdir().expect("tempdir");
    let cache_context = create_codex_apps_tools_cache_context(
        codex_home.path().to_path_buf(),
        Some("account-one"),
        Some("user-one"),
    )
    .await;
    let cache_path = cache_context.tools_cache_path();
    if let Some(parent) = cache_path.parent() {
        std::fs::create_dir_all(parent).expect("create parent");
    }
    let bytes = serde_json::to_vec_pretty(&serde_json::json!({
        "schema_version": CODEX_APPS_TOOLS_CACHE_SCHEMA_VERSION + 1,
        "tools": [create_test_tool(CODEX_APPS_MCP_SERVER_NAME, "one")],
    }))
    .expect("serialize");
    std::fs::write(cache_path, bytes).expect("write");

    assert!(read_cached_codex_apps_tools(&cache_context).is_none());
}

#[tokio::test]
async fn codex_apps_tools_cache_is_ignored_when_json_is_invalid() {
    let codex_home = tempdir().expect("tempdir");
    let cache_context = create_codex_apps_tools_cache_context(
        codex_home.path().to_path_buf(),
        Some("account-one"),
        Some("user-one"),
    )
    .await;
    let cache_path = cache_context.tools_cache_path();
    if let Some(parent) = cache_path.parent() {
        std::fs::create_dir_all(parent).expect("create parent");
    }
    std::fs::write(cache_path, b"{not json").expect("write");

    assert!(read_cached_codex_apps_tools(&cache_context).is_none());
}

#[tokio::test]
async fn startup_cached_codex_apps_tools_loads_from_disk_cache() {
    let codex_home = tempdir().expect("tempdir");
    let writer_cache_context = create_codex_apps_tools_cache_context(
        codex_home.path().to_path_buf(),
        Some("account-one"),
        Some("user-one"),
    )
    .await;
    let cached_tools = vec![create_test_tool(
        CODEX_APPS_MCP_SERVER_NAME,
        "calendar_search",
    )];
    let server_info = create_test_server_info("Codex Apps");
    write_cached_codex_apps_tools_for_test(&writer_cache_context, &server_info, &cached_tools);
    let cache_context = create_codex_apps_tools_cache_context(
        codex_home.path().to_path_buf(),
        Some("account-one"),
        Some("user-one"),
    )
    .await;

    let startup_tools = cache_context
        .current_tools()
        .expect("expected startup snapshot to load from cache");
    let cached_server_info = load_startup_cached_codex_apps_server_info(&cache_context);

    assert_eq!(startup_tools.len(), 1);
    assert_eq!(startup_tools[0].server_name, CODEX_APPS_MCP_SERVER_NAME);
    assert_eq!(startup_tools[0].callable_name, "calendar_search");
    assert_eq!(cached_server_info, Some(server_info));
}

#[tokio::test]
async fn startup_cached_codex_apps_tools_loads_without_server_info_cache() {
    let codex_home = tempdir().expect("tempdir");
    let writer_cache_context = create_codex_apps_tools_cache_context(
        codex_home.path().to_path_buf(),
        Some("account-one"),
        Some("user-one"),
    )
    .await;
    let cache_path = writer_cache_context.tools_cache_path();
    if let Some(parent) = cache_path.parent() {
        std::fs::create_dir_all(parent).expect("create parent");
    }
    let bytes = serde_json::to_vec_pretty(&serde_json::json!({
        "schema_version": CODEX_APPS_TOOLS_CACHE_SCHEMA_VERSION,
        "tools": [create_test_tool(CODEX_APPS_MCP_SERVER_NAME, "calendar_search")],
    }))
    .expect("serialize");
    std::fs::write(cache_path, bytes).expect("write");
    let cache_context = create_codex_apps_tools_cache_context(
        codex_home.path().to_path_buf(),
        Some("account-one"),
        Some("user-one"),
    )
    .await;

    let startup_tools = cache_context
        .current_tools()
        .expect("legacy startup snapshot should remain available");
    let cached_server_info = load_startup_cached_codex_apps_server_info(&cache_context);

    assert_eq!(startup_tools.len(), 1);
    assert_eq!(startup_tools[0].callable_name, "calendar_search");
    assert_eq!(cached_server_info, None);
}

#[tokio::test]
async fn codex_apps_server_info_cache_survives_legacy_tools_cache_write() {
    let codex_home = tempdir().expect("tempdir");
    let cache_context = create_codex_apps_tools_cache_context(
        codex_home.path().to_path_buf(),
        Some("account-one"),
        Some("user-one"),
    )
    .await;
    let server_info = create_test_server_info("Codex Apps");
    write_cached_codex_apps_tools_for_test(
        &cache_context,
        &server_info,
        &[create_test_tool(
            CODEX_APPS_MCP_SERVER_NAME,
            "calendar_search",
        )],
    );

    let cache_path = cache_context.tools_cache_path();
    if let Some(parent) = cache_path.parent() {
        std::fs::create_dir_all(parent).expect("create parent");
    }
    let bytes = serde_json::to_vec_pretty(&serde_json::json!({
        "schema_version": CODEX_APPS_TOOLS_CACHE_SCHEMA_VERSION - 1,
        "tools": [create_test_tool(CODEX_APPS_MCP_SERVER_NAME, "calendar_search")],
    }))
    .expect("serialize");
    std::fs::write(cache_path, bytes).expect("write legacy tools cache");
    let startup_cache_context = create_codex_apps_tools_cache_context(
        codex_home.path().to_path_buf(),
        Some("account-one"),
        Some("user-one"),
    )
    .await;

    assert_eq!(
        load_startup_cached_codex_apps_server_info(&startup_cache_context),
        Some(server_info)
    );
    assert!(startup_cache_context.current_tools().is_none());
}

#[tokio::test]
async fn codex_apps_tools_cache_context_does_not_reread_disk_after_creation() {
    let codex_home = tempdir().expect("tempdir");
    let writer_cache_context = create_codex_apps_tools_cache_context(
        codex_home.path().to_path_buf(),
        Some("account-one"),
        Some("user-one"),
    )
    .await;
    let cached_tools = vec![create_test_tool(CODEX_APPS_MCP_SERVER_NAME, "cached")];
    write_cached_codex_apps_tools(&writer_cache_context, &cached_tools).expect("write cache");
    let reader_cache_context = create_codex_apps_tools_cache_context(
        codex_home.path().to_path_buf(),
        Some("account-one"),
        Some("user-one"),
    )
    .await;
    let updated_tools = vec![create_test_tool(CODEX_APPS_MCP_SERVER_NAME, "updated")];
    write_cached_codex_apps_tools(&writer_cache_context, &updated_tools).expect("rewrite cache");

    assert_eq!(
        reader_cache_context
            .current_tools()
            .expect("in-memory tools")[0]
            .callable_name,
        "cached"
    );
    assert_eq!(
        read_cached_codex_apps_tools(&writer_cache_context).expect("disk tools")[0].callable_name,
        "updated"
    );
}

#[tokio::test]
async fn codex_apps_tools_cache_publishes_newest_shared_snapshot() {
    let codex_home = tempdir().expect("tempdir");
    let cache = CodexAppsToolsCache::default();
    let cache_context_1 = cache
        .context(
            codex_home.path().to_path_buf(),
            CodexAppsToolsCacheKey {
                account_id: Some("account-one".to_string()),
                chatgpt_user_id: Some("user-one".to_string()),
                is_workspace_account: false,
                chatgpt_base_url: "https://chatgpt.com".to_string(),
                product_sku: DEFAULT_CODEX_APPS_MCP_PRODUCT_SKU.to_string(),
            },
        )
        .await;
    let cache_context_2 = cache
        .context(
            codex_home.path().to_path_buf(),
            CodexAppsToolsCacheKey {
                account_id: Some("account-one".to_string()),
                chatgpt_user_id: Some("user-one".to_string()),
                is_workspace_account: false,
                chatgpt_base_url: "https://chatgpt.com".to_string(),
                product_sku: DEFAULT_CODEX_APPS_MCP_PRODUCT_SKU.to_string(),
            },
        )
        .await;
    let older_ticket = cache_context_1.begin_fetch(CodexAppsToolsFetchSource::Startup);
    let newer_ticket = cache_context_2.begin_fetch(CodexAppsToolsFetchSource::HardRefresh);
    let server_info = create_test_server_info("Codex Apps");
    let newer_tools = vec![create_test_tool(CODEX_APPS_MCP_SERVER_NAME, "newer")];
    let older_tools = vec![create_test_tool(CODEX_APPS_MCP_SERVER_NAME, "older")];

    let published_tools = cache_context_2
        .publish_if_newest_accepted(newer_ticket, &server_info, newer_tools)
        .await;
    assert_eq!(published_tools.len(), 1);
    assert_eq!(published_tools[0].callable_name, "newer");
    let shared = cache_context_1
        .current_tools()
        .expect("new snapshot should publish");
    assert_eq!(shared.len(), 1);
    assert_eq!(shared[0].callable_name, "newer");
    let current_tools = cache_context_1
        .publish_if_newest_accepted(older_ticket, &server_info, older_tools)
        .await;

    assert_eq!(current_tools[0].callable_name, "newer");
    assert_eq!(
        cache_context_2.current_tools().expect("shared snapshot")[0].callable_name,
        "newer"
    );
    assert_eq!(
        read_cached_codex_apps_tools(&cache_context_1).expect("persisted snapshot")[0]
            .callable_name,
        "newer"
    );
}

#[tokio::test]
async fn codex_apps_tools_cache_keeps_live_publish_when_disk_persistence_fails() {
    let codex_home = tempdir().expect("tempdir");
    let codex_home_file = codex_home.path().join("not-a-directory");
    std::fs::write(&codex_home_file, b"occupied").expect("create codex home file");
    let cache_context = CodexAppsToolsCache::default()
        .context(
            codex_home_file,
            CodexAppsToolsCacheKey {
                account_id: Some("account-one".to_string()),
                chatgpt_user_id: Some("user-one".to_string()),
                is_workspace_account: false,
                chatgpt_base_url: "https://chatgpt.com".to_string(),
                product_sku: DEFAULT_CODEX_APPS_MCP_PRODUCT_SKU.to_string(),
            },
        )
        .await;
    let tools = vec![create_test_tool(CODEX_APPS_MCP_SERVER_NAME, "live")];
    let older_ticket = cache_context.begin_fetch(CodexAppsToolsFetchSource::Startup);
    let published_tools = cache_context
        .publish_if_newest_accepted(
            cache_context.begin_fetch(CodexAppsToolsFetchSource::HardRefresh),
            &create_test_server_info("Codex Apps"),
            tools.clone(),
        )
        .await;

    assert_eq!(published_tools.len(), 1);
    assert_eq!(published_tools[0].callable_name, "live");
    let live = cache_context.current_tools().expect("live snapshot");
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].callable_name, "live");
    let blocked_home = &cache_context.entry.identity.codex_home;
    assert_eq!(std::fs::read(blocked_home).unwrap(), b"occupied");
    std::fs::remove_file(blocked_home).expect("remove external persistence blocker");
    std::fs::create_dir(blocked_home).expect("restore writable cache home");
    let stale = cache_context
        .publish_if_newest_accepted(
            older_ticket,
            &create_test_server_info("Obsolete server"),
            vec![create_test_tool(CODEX_APPS_MCP_SERVER_NAME, "obsolete")],
        )
        .await;
    assert_eq!(stale.len(), 1);
    assert_eq!(stale[0].callable_name, "live");
    assert!(
        !cache_context.tools_cache_path().exists(),
        "failed disk persistence must still commit live generation ordering"
    );
    assert!(
        !cache_context.server_info_cache_path().exists(),
        "stale server information must not become cold-start data"
    );
}

#[tokio::test]
async fn shared_codex_apps_tools_cache_exposes_live_publish_when_persistence_fails() {
    let codex_home = tempdir().expect("tempdir");
    let codex_home_file = codex_home.path().join("not-a-directory");
    std::fs::write(&codex_home_file, b"occupied").expect("create codex home file");
    let auth_key = CodexAppsToolsCacheKey {
        account_id: Some("account-one".to_string()),
        chatgpt_user_id: Some("user-one".to_string()),
        is_workspace_account: false,
        chatgpt_base_url: "https://chatgpt.com".to_string(),
        product_sku: DEFAULT_CODEX_APPS_MCP_PRODUCT_SKU.to_string(),
    };
    let publisher_cache = CodexAppsToolsCache::shared();
    let cache_context = publisher_cache
        .context(codex_home_file.clone(), auth_key.clone())
        .await;
    let tools = vec![create_test_tool(CODEX_APPS_MCP_SERVER_NAME, "live")];

    cache_context
        .publish_if_newest_accepted(
            cache_context.begin_fetch(CodexAppsToolsFetchSource::HardRefresh),
            &create_test_server_info("Codex Apps"),
            tools.clone(),
        )
        .await;

    let reader_cache = CodexAppsToolsCache::shared();
    let snapshot = reader_cache
        .current_snapshot(codex_home_file, auth_key)
        .await
        .expect("shared live snapshot");
    assert_eq!(snapshot.tools().len(), 1);
    assert_eq!(snapshot.tools()[0].callable_name, "live");
    assert!(snapshot.codex_apps_ready());
}

#[tokio::test]
async fn codex_apps_tools_cache_snapshot_tracks_startup_and_live_publication_state() {
    let codex_home = tempdir().expect("tempdir");
    let cache = CodexAppsToolsCache::default();
    let key = CodexAppsToolsCacheKey {
        account_id: Some("account-one".to_string()),
        chatgpt_user_id: Some("user-one".to_string()),
        is_workspace_account: false,
        chatgpt_base_url: "https://chatgpt.com".to_string(),
        product_sku: DEFAULT_CODEX_APPS_MCP_PRODUCT_SKU.to_string(),
    };
    let cache_context = cache
        .context(codex_home.path().to_path_buf(), key.clone())
        .await;
    cache_context.store_current_tools_for_test(vec![create_test_tool(
        CODEX_APPS_MCP_SERVER_NAME,
        "startup",
    )]);

    let startup_snapshot = cache
        .current_snapshot(codex_home.path().to_path_buf(), key.clone())
        .await
        .expect("startup snapshot");
    assert_eq!(startup_snapshot.tools()[0].callable_name, "startup");
    assert!(!startup_snapshot.codex_apps_ready());
    assert!(!startup_snapshot.is_fresh_for(std::time::Duration::MAX));

    cache_context
        .publish_if_newest_accepted(
            cache_context.begin_fetch(CodexAppsToolsFetchSource::HardRefresh),
            &create_test_server_info("Codex Apps"),
            vec![create_test_tool(CODEX_APPS_MCP_SERVER_NAME, "live")],
        )
        .await;

    let live_snapshot = cache
        .current_snapshot(codex_home.path().to_path_buf(), key)
        .await
        .expect("live snapshot");
    assert_eq!(live_snapshot.tools()[0].callable_name, "live");
    assert!(live_snapshot.codex_apps_ready());
    assert!(live_snapshot.is_fresh_for(std::time::Duration::from_secs(1)));
}

#[tokio::test]
async fn codex_apps_tools_cache_evicts_only_idle_lru_identities() {
    let codex_home = tempdir().expect("tempdir");
    let cache = CodexAppsToolsCache::default();
    let key = |index: usize| CodexAppsToolsCacheKey {
        account_id: Some(format!("account-{index}")),
        chatgpt_user_id: Some(format!("user-{index}")),
        is_workspace_account: false,
        chatgpt_base_url: "https://chatgpt.com".to_string(),
        product_sku: DEFAULT_CODEX_APPS_MCP_PRODUCT_SKU.to_string(),
    };

    let pinned = cache.context(codex_home.path().to_path_buf(), key(0)).await;
    let idle = cache.context(codex_home.path().to_path_buf(), key(1)).await;
    let idle_entry = Arc::downgrade(&idle.entry);
    drop(idle);
    for index in 2..=CODEX_APPS_TOOLS_CACHE_CAPACITY + 1 {
        drop(
            cache
                .context(codex_home.path().to_path_buf(), key(index))
                .await,
        );
    }

    assert!(
        idle_entry.upgrade().is_none(),
        "oldest idle entry was retained"
    );
    let pinned_again = cache.context(codex_home.path().to_path_buf(), key(0)).await;
    assert!(Arc::ptr_eq(&pinned.entry, &pinned_again.entry));
    assert_eq!(
        lock_unpoisoned(&cache.entries).by_identity.len(),
        CODEX_APPS_TOOLS_CACHE_CAPACITY
    );
}

#[tokio::test(flavor = "current_thread")]
async fn cold_apps_snapshot_yields_under_registry_contention_and_seeds_once() {
    use std::future::Future;
    use std::task::Context;
    use std::task::Poll;
    use std::task::Waker;
    use std::time::Duration;

    let home = tempdir().expect("cache home");
    let key = codex_apps_tools_cache_key(None, "https://contention.example", None);
    let writer = CodexAppsToolsCache::default()
        .context(home.path().to_path_buf(), key.clone())
        .await;
    write_cached_codex_apps_tools(
        &writer,
        &[create_test_tool(CODEX_APPS_MCP_SERVER_NAME, "disk-seed")],
    )
    .expect("write cold cache fixture");
    let cache = CodexAppsToolsCache::default();
    let entries = Arc::clone(&cache.entries);
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        let _guard = lock_unpoisoned(&entries);
        started_tx.send(()).expect("registry contention receiver");
        release_rx.recv_timeout(Duration::from_secs(2)).is_ok()
    });
    started_rx.await.expect("registry held by OS thread");
    let mut snapshot = Box::pin(cache.current_snapshot(home.path().to_path_buf(), key.clone()));
    let pending = matches!(
        snapshot
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop())),
        Poll::Pending
    );
    // This timer must run while the external thread owns the real registry lock.
    tokio::time::sleep(Duration::from_millis(20)).await;
    let heartbeat_delivered = release_tx.send(()).is_ok();
    let holder_released = tokio::task::spawn_blocking(move || holder.join())
        .await
        .expect("join holder task")
        .expect("registry holder");
    assert!(pending, "cold lookup must yield under registry contention");
    assert!(
        heartbeat_delivered && holder_released,
        "runtime heartbeat missed the two-second watchdog"
    );
    let seeded = tokio::time::timeout(Duration::from_secs(2), snapshot)
        .await
        .expect("cold lookup completes")
        .expect("disk-seeded snapshot");
    assert_eq!(seeded.tools().len(), 1);
    assert_eq!(seeded.tools()[0].callable_name, "disk-seed");
    assert!(!seeded.codex_apps_ready());
    assert!(!seeded.is_fresh_for(Duration::MAX));

    write_cached_codex_apps_tools(
        &writer,
        &[create_test_tool(CODEX_APPS_MCP_SERVER_NAME, "later-disk")],
    )
    .expect("replace disk after cold load");
    let retained = cache
        .current_snapshot(home.path().to_path_buf(), key)
        .await
        .expect("retained snapshot");
    assert_eq!(retained.tools().len(), 1);
    assert_eq!(retained.tools()[0].callable_name, "disk-seed");
    assert_eq!(
        read_cached_codex_apps_tools(&writer).expect("updated disk")[0].callable_name,
        "later-disk"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn cancelled_apps_publish_finishes_persistence_and_rejects_stale_generation() {
    use std::future::Future;
    use std::task::Context;
    use std::task::Poll;
    use std::task::Waker;
    use std::time::Duration;

    let home = tempdir().expect("cache home");
    let cache = CodexAppsToolsCache::default();
    let key = codex_apps_tools_cache_key(None, "https://publish.example", None);
    let context = cache.context(home.path().to_path_buf(), key.clone()).await;
    let older = context.begin_fetch(CodexAppsToolsFetchSource::Startup);
    let newest = context.begin_fetch(CodexAppsToolsFetchSource::HardRefresh);
    let newest_server = create_test_server_info("Newest server");
    let entry = Arc::clone(&context.entry);
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        let _guard = lock_unpoisoned(&entry.last_accepted_generation);
        started_tx
            .send(())
            .expect("publication contention receiver");
        release_rx.recv_timeout(Duration::from_secs(2)).is_ok()
    });
    started_rx
        .await
        .expect("publication lock held by OS thread");
    let mut publish = Box::pin(context.publish_if_newest_accepted(
        newest,
        &newest_server,
        vec![create_test_tool(CODEX_APPS_MCP_SERVER_NAME, "newest")],
    ));
    let pending = matches!(
        publish
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop())),
        Poll::Pending
    );
    drop(publish);
    assert!(
        context.current_tools().is_none(),
        "blocked publisher must not change the snapshot early"
    );
    tokio::time::sleep(Duration::from_millis(20)).await;
    let heartbeat_delivered = release_tx.send(()).is_ok();
    let holder_released = tokio::task::spawn_blocking(move || holder.join())
        .await
        .expect("join holder task")
        .expect("publication holder");
    assert!(
        pending,
        "publication must yield under generation-lock contention"
    );
    assert!(
        heartbeat_delivered && holder_released,
        "runtime heartbeat missed the two-second watchdog"
    );
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if context
                .current_tools()
                .is_some_and(|tools| tools.len() == 1 && tools[0].callable_name == "newest")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("dropping the caller must not cancel its admitted publication");
    // Acquiring this same production guard waits for both accepted disk writes.
    let disk_context = context.clone();
    let (persisted, persisted_server) = tokio::task::spawn_blocking(move || {
        let _guard = lock_unpoisoned(&disk_context.entry.last_accepted_generation);
        (
            read_cached_codex_apps_tools(&disk_context),
            load_startup_cached_codex_apps_server_info(&disk_context),
        )
    })
    .await
    .expect("read completed persistence");
    let persisted = persisted.expect("accepted tools persisted after caller cancellation");
    assert_eq!(persisted.len(), 1);
    assert_eq!(persisted[0].callable_name, "newest");
    assert_eq!(
        persisted_server,
        Some(create_test_server_info("Newest server"))
    );
    let tools_bytes = std::fs::read(context.tools_cache_path()).expect("tools bytes");
    let server_bytes = std::fs::read(context.server_info_cache_path()).expect("server bytes");
    let live = cache
        .current_snapshot(home.path().to_path_buf(), key.clone())
        .await
        .expect("live snapshot");
    assert!(live.codex_apps_ready());
    let accepted_at = live.published_at;

    let stale_result = context
        .publish_if_newest_accepted(
            older,
            &create_test_server_info("Obsolete server"),
            vec![create_test_tool(CODEX_APPS_MCP_SERVER_NAME, "obsolete")],
        )
        .await;
    assert_eq!(stale_result.len(), 1);
    assert_eq!(stale_result[0].callable_name, "newest");
    let after_stale = cache
        .current_snapshot(home.path().to_path_buf(), key)
        .await
        .expect("accepted snapshot");
    assert_eq!(after_stale.tools().len(), 1);
    assert_eq!(after_stale.tools()[0].callable_name, "newest");
    assert_eq!(
        after_stale.published_at, accepted_at,
        "stale fetch must not renew freshness"
    );
    assert_eq!(
        std::fs::read(context.tools_cache_path()).unwrap(),
        tools_bytes
    );
    assert_eq!(
        std::fs::read(context.server_info_cache_path()).unwrap(),
        server_bytes
    );
}
