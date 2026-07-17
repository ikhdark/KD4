use super::*;
use crate::manifest::load_plugin_manifest;
use crate::test_support::write_file;
use codex_config::ConfigLayerEntry;
use codex_config::ConfigLayerSource;
use codex_config::ConfigRequirements;
use codex_config::ConfigRequirementsToml;
use codex_plugin::AppConnectorId;
use codex_plugin::AppDeclaration;
use codex_plugin::PluginId;
use codex_plugin::PluginLoadOutcome;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::sync::Mutex;
use tempfile::TempDir;
use tokio::sync::oneshot;

fn user_config_path(temp_dir: &TempDir, file_name: &str) -> AbsolutePathBuf {
    AbsolutePathBuf::from_absolute_path(temp_dir.path().join(file_name))
        .expect("test user config path should be absolute")
}

fn user_layer(path: AbsolutePathBuf, config: &str) -> ConfigLayerEntry {
    ConfigLayerEntry::new(
        ConfigLayerSource::User {
            file: path,
            profile: None,
        },
        toml::from_str(config).expect("user config toml"),
    )
}

fn ordered_test_plugin(config_name: &str, mcp_server: &str) -> LoadedPlugin<String> {
    LoadedPlugin {
        config_name: config_name.to_string(),
        manifest_name: Some(config_name.to_string()),
        plugin_namespace: Some(config_name.to_string()),
        manifest_description: Some(format!("{config_name} prompt")),
        root: AbsolutePathBuf::from_absolute_path(std::env::temp_dir().join(config_name))
            .expect("absolute plugin root"),
        enabled: true,
        skill_roots: Vec::new(),
        disabled_skill_paths: HashSet::new(),
        has_enabled_skills: true,
        mcp_servers: HashMap::from([
            ("app".to_string(), format!("{config_name}-legacy-app-route")),
            ("shared".to_string(), mcp_server.to_string()),
        ]),
        apps: vec![AppDeclaration {
            name: "app".to_string(),
            connector_id: AppConnectorId(format!("connector_{config_name}")),
            category: None,
        }],
        hook_sources: Vec::new(),
        hook_load_warnings: Vec::new(),
        error: None,
    }
}

#[tokio::test]
async fn reverse_completion_preserves_plugin_conflict_prompt_and_router_order() {
    let (alpha_tx, alpha_rx) = oneshot::channel();
    let (beta_tx, beta_rx) = oneshot::channel();
    let (alpha_started_tx, alpha_started_rx) = oneshot::channel();
    let (beta_started_tx, beta_started_rx) = oneshot::channel();
    let (alpha_done_tx, alpha_done_rx) = oneshot::channel();
    let (beta_done_tx, beta_done_rx) = oneshot::channel();
    let completion_order = Arc::new(Mutex::new(Vec::new()));
    let job_completion_order = Arc::clone(&completion_order);
    let jobs = vec![
        (
            "alpha",
            "alpha-route",
            alpha_started_tx,
            alpha_rx,
            alpha_done_tx,
        ),
        (
            "beta",
            "beta-route",
            beta_started_tx,
            beta_rx,
            beta_done_tx,
        ),
    ]
    .into_iter()
    .map(move |(config_name, mcp_server, started, release, done)| {
        let completion_order = Arc::clone(&job_completion_order);
        async move {
            started.send(()).expect("mark plugin started");
            release.await.expect("release plugin");
            completion_order
                .lock()
                .expect("completion order")
                .push(config_name);
            done.send(()).expect("mark plugin complete");
            ordered_test_plugin(config_name, mcp_server)
        }
    });
    let load = tokio::spawn(collect_bounded_in_order(jobs, /*concurrency*/ 2));
    alpha_started_rx.await.expect("alpha started");
    beta_started_rx.await.expect("beta started");
    beta_tx.send(()).expect("release beta");
    beta_done_rx.await.expect("beta complete");
    alpha_tx.send(()).expect("release alpha");
    alpha_done_rx.await.expect("alpha complete");
    let mut plugins = load.await.expect("join ordered load");

    assert_eq!(
        completion_order.lock().expect("completion order").as_slice(),
        &["beta", "alpha"]
    );
    assert_eq!(
        plugins
            .iter()
            .map(|plugin| plugin.config_name.as_str())
            .collect::<Vec<_>>(),
        vec!["alpha", "beta"]
    );
    for plugin in &mut plugins {
        crate::app_mcp_routing::apply_app_mcp_routing_policy(
            &mut plugin.apps,
            &mut plugin.mcp_servers,
            Some(AuthMode::Chatgpt),
            /*plugin_active*/ true,
        );
    }
    let outcome = PluginLoadOutcome::from_plugins(plugins);
    assert_eq!(
        outcome
            .capability_summaries()
            .iter()
            .map(|summary| (
                summary.config_name.clone(),
                summary.description.clone(),
                summary.mcp_server_names.clone(),
                summary.app_connector_ids.clone(),
            ))
            .collect::<Vec<_>>(),
        vec![
            (
                "alpha".to_string(),
                Some("alpha prompt".to_string()),
                vec!["shared".to_string()],
                vec![AppConnectorId("connector_alpha".to_string())],
            ),
            (
                "beta".to_string(),
                Some("beta prompt".to_string()),
                vec!["shared".to_string()],
                vec![AppConnectorId("connector_beta".to_string())],
            ),
        ]
    );
    assert_eq!(
        outcome.effective_mcp_servers(),
        HashMap::from([("shared".to_string(), "alpha-route".to_string())])
    );
    assert_eq!(
        outcome.effective_apps(),
        vec![
            AppConnectorId("connector_alpha".to_string()),
            AppConnectorId("connector_beta".to_string()),
        ]
    );
}

#[test]
fn configured_plugins_from_stack_merges_user_layers() {
    let temp_dir = TempDir::new().expect("tempdir");
    let stack = ConfigLayerStack::new(
        vec![
            user_layer(
                user_config_path(&temp_dir, "config.toml"),
                "[plugins.base]\nenabled = true\n",
            ),
            user_layer(
                user_config_path(&temp_dir, "work.config.toml"),
                "[plugins.profile]\nenabled = false\n",
            ),
        ],
        ConfigRequirements::default(),
        ConfigRequirementsToml::default(),
    )
    .expect("valid config layer stack");

    let plugins = configured_plugins_from_stack(&stack, temp_dir.path());

    assert_eq!(
        plugins,
        HashMap::from([
            (
                "base".to_string(),
                PluginConfig {
                    enabled: true,
                    mcp_servers: HashMap::new(),
                },
            ),
            (
                "profile".to_string(),
                PluginConfig {
                    enabled: false,
                    mcp_servers: HashMap::new(),
                },
            ),
        ])
    );
}

#[tokio::test]
async fn hooks_only_scope_shares_plugin_resolution_without_loading_other_capabilities() {
    let temp_dir = TempDir::new().expect("tempdir");
    let plugin_root = temp_dir.path().join("plugins/cache/test/valid/local");
    write_file(
        &plugin_root.join(".codex-plugin/plugin.json"),
        r#"{"name":"valid"}"#,
    );
    write_file(
        &plugin_root.join("skills/example/SKILL.md"),
        "---\nname: example\ndescription: example skill\n---\n",
    );
    write_file(
        &plugin_root.join(".mcp.json"),
        r#"{"mcpServers":{"example":{"command":"echo"}}}"#,
    );
    write_file(
        &plugin_root.join(".app.json"),
        r#"{"apps":{"example":{"id":"connector_example"}}}"#,
    );
    write_file(
        &plugin_root.join("hooks/hooks.json"),
        r#"{
  "hooks": {
    "SessionStart": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "echo startup"
          }
        ]
      }
    ]
  }
}"#,
    );

    let disabled_root = temp_dir.path().join("plugins/cache/test/disabled/local");
    write_file(
        &disabled_root.join(".codex-plugin/plugin.json"),
        r#"{"name":"disabled"}"#,
    );
    write_file(
        &disabled_root.join("hooks/hooks.json"),
        r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"echo disabled"}]}]}}"#,
    );

    let malformed_root = temp_dir.path().join("plugins/cache/test/malformed/local");
    write_file(
        &malformed_root.join(".codex-plugin/plugin.json"),
        "not valid json",
    );

    let warning_root = temp_dir.path().join("plugins/cache/test/warning/local");
    write_file(
        &warning_root.join(".codex-plugin/plugin.json"),
        r#"{"name":"warning"}"#,
    );
    write_file(&warning_root.join("hooks/hooks.json"), "not valid json");

    let stack = ConfigLayerStack::new(
        vec![user_layer(
            user_config_path(&temp_dir, "config.toml"),
            r#"
[plugins."valid@test"]
enabled = true

[plugins."disabled@test"]
enabled = false

[plugins.invalid]
enabled = true

[plugins."malformed@test"]
enabled = true

[plugins."missing@test"]
enabled = true

[plugins."warning@test"]
enabled = true
"#,
        )],
        ConfigRequirements::default(),
        ConfigRequirementsToml::default(),
    )
    .expect("valid config layer stack");
    let store = PluginStore::new(temp_dir.path().to_path_buf());

    let full = load_plugins_from_layer_stack(
        &stack,
        HashMap::new(),
        &store,
        /*plugin_skill_snapshots*/ None,
        Some(Product::Codex),
        /*remote_global_catalog_active*/ false,
    )
    .await;
    let hooks_only = load_plugins_from_layer_stack_with_scope(
        &stack,
        HashMap::new(),
        &store,
        /*remote_global_catalog_active*/ false,
        PluginLoadScope::HooksOnly,
    )
    .await;

    let validation_state = |plugins: &[LoadedPlugin<McpServerConfig>]| {
        plugins
            .iter()
            .map(|plugin| {
                (
                    plugin.config_name.clone(),
                    plugin.enabled,
                    plugin.root.clone(),
                    plugin.error.clone(),
                    plugin.hook_sources.clone(),
                    plugin.hook_load_warnings.clone(),
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(validation_state(&hooks_only), validation_state(&full));

    let full_valid = full
        .iter()
        .find(|plugin| plugin.config_name == "valid@test")
        .expect("full load should include valid plugin");
    assert!(full_valid.manifest_name.is_some());
    assert!(!full_valid.skill_roots.is_empty());
    assert!(!full_valid.mcp_servers.is_empty());
    assert!(!full_valid.apps.is_empty());

    let hooks_only_valid = hooks_only
        .iter()
        .find(|plugin| plugin.config_name == "valid@test")
        .expect("hooks-only load should include valid plugin");
    assert_eq!(hooks_only_valid.manifest_name, None);
    assert!(hooks_only_valid.skill_roots.is_empty());
    assert!(hooks_only_valid.mcp_servers.is_empty());
    assert!(hooks_only_valid.apps.is_empty());
}

#[test]
fn curated_plugin_cache_version_shortens_full_git_sha() {
    assert_eq!(
        curated_plugin_cache_version("0123456789abcdef0123456789abcdef01234567"),
        "01234567"
    );
}

#[test]
fn curated_plugin_cache_version_preserves_non_git_sha_versions() {
    assert_eq!(
        curated_plugin_cache_version("export-backup"),
        "export-backup"
    );
    assert_eq!(curated_plugin_cache_version("0123456"), "0123456");
}

fn plugin_id() -> PluginId {
    PluginId::parse("demo-plugin@test-marketplace").expect("plugin id")
}

fn plugin_root() -> (tempfile::TempDir, AbsolutePathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let plugin_root =
        AbsolutePathBuf::try_from(tmp.path().join("demo-plugin")).expect("plugin root");
    fs::create_dir_all(plugin_root.join(".codex-plugin")).expect("create manifest dir");
    fs::create_dir_all(plugin_root.join("hooks")).expect("create hooks dir");
    (tmp, plugin_root)
}

fn write_manifest(plugin_root: &AbsolutePathBuf, manifest: &str) {
    fs::write(plugin_root.join(".codex-plugin/plugin.json"), manifest).expect("write manifest");
}

fn write_hook_file(plugin_root: &AbsolutePathBuf, relative_path: &str, event: &str, command: &str) {
    fs::write(
        plugin_root.join(relative_path),
        format!(
            r#"{{
  "hooks": {{
    "{event}": [
      {{
        "hooks": [{{ "type": "command", "command": "{command}" }}]
      }}
    ]
  }}
}}"#
        ),
    )
    .expect("write hooks");
}

fn load_sources(plugin_root: &AbsolutePathBuf) -> (Vec<PluginHookSource>, Vec<String>) {
    let manifest = load_plugin_manifest(plugin_root.as_path()).expect("manifest");
    let plugin_data_root = AbsolutePathBuf::try_from(
        plugin_root
            .as_path()
            .parent()
            .expect("plugin root parent")
            .join("plugin-data"),
    )
    .expect("plugin data root");
    load_plugin_hooks(
        plugin_root,
        &plugin_id(),
        &plugin_data_root,
        &manifest.paths,
    )
}

fn assert_sources(sources: &[PluginHookSource], expected_relative_paths: &[&str]) {
    assert_eq!(
        sources
            .iter()
            .map(|source| source.plugin_id.clone())
            .collect::<Vec<_>>(),
        vec![plugin_id(); expected_relative_paths.len()]
    );
    assert_eq!(
        sources
            .iter()
            .map(|source| source.source_relative_path.as_str())
            .collect::<Vec<_>>(),
        expected_relative_paths
    );
    assert_eq!(
        sources
            .iter()
            .map(|source| source.hooks.handler_count())
            .collect::<Vec<_>>(),
        vec![1; expected_relative_paths.len()]
    );
}

#[test]
fn load_plugin_hooks_discovers_default_hooks_file() {
    let (_tmp, plugin_root) = plugin_root();
    write_manifest(&plugin_root, r#"{ "name": "demo-plugin" }"#);
    fs::write(
        plugin_root.join("hooks/hooks.json"),
        r#"{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [{ "type": "command", "command": "echo default" }]
      }
    ]
  }
}"#,
    )
    .expect("write hooks");

    let (sources, warnings) = load_sources(&plugin_root);

    assert_eq!(warnings, Vec::<String>::new());
    assert_sources(&sources, &["hooks/hooks.json"]);
}

#[test]
fn load_plugin_hooks_supports_manifest_hook_path() {
    let (_tmp, plugin_root) = plugin_root();
    write_manifest(
        &plugin_root,
        r#"{
  "name": "demo-plugin",
  "hooks": "./hooks/one.json"
}"#,
    );
    write_hook_file(&plugin_root, "hooks/one.json", "PreToolUse", "echo one");

    let (sources, warnings) = load_sources(&plugin_root);

    assert_eq!(warnings, Vec::<String>::new());
    assert_sources(&sources, &["hooks/one.json"]);
}

#[test]
fn load_plugin_hooks_manifest_paths_replace_default_hooks_file() {
    let (_tmp, plugin_root) = plugin_root();
    write_manifest(
        &plugin_root,
        r#"{
  "name": "demo-plugin",
  "hooks": ["./hooks/one.json", "./hooks/two.json"]
}"#,
    );
    write_hook_file(
        &plugin_root,
        "hooks/hooks.json",
        "PreToolUse",
        "echo ignored",
    );
    write_hook_file(&plugin_root, "hooks/one.json", "PreToolUse", "echo one");
    write_hook_file(&plugin_root, "hooks/two.json", "PostToolUse", "echo two");

    let (sources, warnings) = load_sources(&plugin_root);

    assert_eq!(warnings, Vec::<String>::new());
    assert_sources(&sources, &["hooks/one.json", "hooks/two.json"]);
}

#[test]
fn load_plugin_hooks_supports_inline_manifest_hooks() {
    let (_tmp, plugin_root) = plugin_root();
    write_manifest(
        &plugin_root,
        r#"{
  "name": "demo-plugin",
  "hooks": {
    "hooks": {
      "SessionStart": [
        {
          "matcher": "startup",
          "hooks": [{ "type": "command", "command": "echo inline" }]
        }
      ]
    }
  }
}"#,
    );

    let (sources, warnings) = load_sources(&plugin_root);

    assert_eq!(warnings, Vec::<String>::new());
    assert_sources(&sources, &["plugin.json#hooks[0]"]);
}

#[test]
fn load_plugin_hooks_reports_invalid_hook_file() {
    let (_tmp, plugin_root) = plugin_root();
    write_manifest(&plugin_root, r#"{ "name": "demo-plugin" }"#);
    fs::write(plugin_root.join("hooks/hooks.json"), "{ not-json").expect("write invalid hooks");

    let (sources, warnings) = load_sources(&plugin_root);

    assert_eq!(sources, Vec::<PluginHookSource>::new());
    assert_eq!(
        warnings,
        vec![format!(
            "failed to parse plugin hooks config {}: key must be a string at line 1 column 3",
            plugin_root.join("hooks/hooks.json").display()
        )]
    );
}

#[test]
fn load_plugin_hooks_supports_inline_manifest_hook_list() {
    let (_tmp, plugin_root) = plugin_root();
    write_manifest(
        &plugin_root,
        r#"{
  "name": "demo-plugin",
  "hooks": [
    {
      "hooks": {
        "SessionStart": [
          {
            "hooks": [{ "type": "command", "command": "echo inline one" }]
          }
        ]
      }
    },
    {
      "hooks": {
        "Stop": [
          {
            "hooks": [{ "type": "command", "command": "echo inline two" }]
          }
        ]
      }
    }
  ]
}"#,
    );

    let (sources, warnings) = load_sources(&plugin_root);

    assert_eq!(warnings, Vec::<String>::new());
    assert_sources(&sources, &["plugin.json#hooks[0]", "plugin.json#hooks[1]"]);
}

#[test]
fn materialize_git_subdir_uses_sparse_checkout() {
    let codex_home = tempfile::tempdir().expect("create codex home");
    let repo = tempfile::tempdir().expect("create git repo");
    let plugin_dir = repo.path().join("plugins/toolkit");
    fs::create_dir_all(&plugin_dir).expect("create plugin directory");
    fs::create_dir_all(repo.path().join("plugins/other")).expect("create other plugin");
    fs::write(plugin_dir.join("marker.txt"), "toolkit").expect("write plugin marker");
    fs::write(repo.path().join("plugins/other/marker.txt"), "other").expect("write other marker");
    fs::write(repo.path().join("root.txt"), "root").expect("write root marker");

    run_git(&["init"], Some(repo.path())).expect("init git repo");
    run_git(
        &["config", "user.email", "test@example.com"],
        Some(repo.path()),
    )
    .expect("configure git email");
    run_git(&["config", "user.name", "Test User"], Some(repo.path())).expect("configure git name");
    run_git(&["add", "."], Some(repo.path())).expect("stage git repo");
    run_git(&["commit", "-m", "init"], Some(repo.path())).expect("commit git repo");

    let materialized = materialize_marketplace_plugin_source(
        codex_home.path(),
        &MarketplacePluginSource::Git {
            url: repo.path().display().to_string(),
            path: Some("plugins/toolkit".to_string()),
            ref_name: None,
            sha: None,
        },
    )
    .expect("materialize git source");

    assert_eq!(
        plugin_dir.file_name(),
        materialized.path.as_path().file_name()
    );
    assert!(materialized.path.as_path().join("marker.txt").is_file());
    let checkout_root = materialized
        .path
        .as_path()
        .parent()
        .and_then(Path::parent)
        .expect("materialized path should be nested under checkout root");
    assert!(!checkout_root.join("root.txt").exists());
    assert!(!checkout_root.join("plugins/other/marker.txt").exists());
}
