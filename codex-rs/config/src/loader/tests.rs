use super::*;
use codex_file_system::CopyOptions;
use codex_file_system::CreateDirectoryOptions;
use codex_file_system::ExecutorFileSystemFuture;
use codex_file_system::FileMetadata;
use codex_file_system::FileSystemReadStream;
use codex_file_system::FileSystemSandboxContext;
use codex_file_system::ReadDirectoryEntry;
use codex_file_system::RemoveOptions;
use codex_utils_path_uri::PathUri;
use pretty_assertions::assert_eq;
use tempfile::tempdir;

#[test]
fn unversioned_config_is_migrated_to_current_canonical_shape() {
    let path = Path::new("config.toml");
    let value: TomlValue = toml::from_str(
        r#"
experimental_use_unified_exec_tool = true
model_supports_reasoning_summaries = true

[features]
connectors = true
memory_tool = true
terminal_resize_reflow = true
enable_experimental_windows_sandbox = true
chronicle = true

[notice]
hide_full_access_warning = true

[notice.external_config_migration_prompts]
home = true

[profiles.work]
experimental_use_unified_exec_tool = false

[profiles.work.features]
telepathy = true
elevated_windows_sandbox = true
"#,
    )
    .expect("valid legacy config");

    let migrated = migrate_config_toml(value, path).expect("migration succeeds");
    assert_eq!(migrated["config_version"].as_integer(), Some(1));
    assert_eq!(migrated["features"]["unified_exec"].as_bool(), Some(true));
    assert_eq!(migrated["features"]["apps"].as_bool(), Some(true));
    assert_eq!(migrated["features"]["memories"].as_bool(), Some(true));
    assert!(migrated["features"].get("memory_tool").is_none());
    assert!(migrated["features"].get("terminal_resize_reflow").is_none());
    assert!(
        migrated["features"]
            .get("enable_experimental_windows_sandbox")
            .is_none()
    );
    assert_eq!(migrated["windows"]["sandbox"].as_str(), Some("unelevated"));
    assert!(migrated.get("model_supports_reasoning_summaries").is_none());
    assert_eq!(
        migrated["profiles"]["work"]["features"]["unified_exec"].as_bool(),
        Some(false)
    );
    assert!(migrated["notice"].get("hide_full_access_warning").is_none());
    assert!(
        migrated["notice"]
            .get("external_config_migration_prompts")
            .is_none()
    );
    assert!(migrated["features"].get("chronicle").is_none());
    assert!(
        migrated["profiles"]["work"]["features"]
            .get("telepathy")
            .is_none()
    );
    assert!(
        migrated["profiles"]["work"]["features"]
            .get("elevated_windows_sandbox")
            .is_none()
    );
    assert_eq!(
        migrated["profiles"]["work"]["windows"]["sandbox"].as_str(),
        Some("elevated")
    );
}

#[test]
fn config_rejects_versions_outside_current_boundary() {
    let value: TomlValue = toml::from_str("config_version = 2").expect("valid TOML");
    let error = migrate_config_toml(value, Path::new("config.toml"))
        .expect_err("future config version must fail");
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("supports version 1"));
}

#[test]
fn current_config_discards_obsolete_settings() {
    let path = Path::new("config.toml");
    let value: TomlValue = toml::from_str(
        r#"
config_version = 1

[features]
chronicle = true
telepathy = true

[notice]
hide_full_access_warning = true

[notice.external_config_migration_prompts]
home = true

[profiles.work.features]
chronicle = true

[profiles.work.notice]
hide_full_access_warning = true

[profiles.work.notice.external_config_migration_prompts]
project = true
"#,
    )
    .expect("valid current config");

    let migrated = migrate_config_toml(value, path).expect("cleanup succeeds");
    assert_eq!(migrated["config_version"].as_integer(), Some(1));
    assert!(migrated["features"].get("chronicle").is_none());
    assert!(migrated["features"].get("telepathy").is_none());
    assert!(migrated["notice"].get("hide_full_access_warning").is_none());
    assert!(
        migrated["notice"]
            .get("external_config_migration_prompts")
            .is_none()
    );
    assert!(
        migrated["profiles"]["work"]["features"]
            .get("chronicle")
            .is_none()
    );
    assert!(
        migrated["profiles"]["work"]["notice"]
            .get("hide_full_access_warning")
            .is_none()
    );
    assert!(
        migrated["profiles"]["work"]["notice"]
            .get("external_config_migration_prompts")
            .is_none()
    );
}

#[derive(Default)]
struct TestFileSystem {
    metadata_error: Option<(AbsolutePathBuf, io::ErrorKind)>,
}

impl ExecutorFileSystem for TestFileSystem {
    fn canonicalize<'a>(
        &'a self,
        path: &'a PathUri,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, PathUri> {
        Box::pin(async move {
            let path = path.to_abs_path()?;
            let canonicalized = path.canonicalize()?;
            Ok(PathUri::from_abs_path(&canonicalized))
        })
    }

    fn read_file<'a>(
        &'a self,
        path: &'a PathUri,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Vec<u8>> {
        Box::pin(async move {
            let path = path.to_abs_path()?;
            tokio::fs::read(path.as_path()).await
        })
    }

    fn read_file_stream<'a>(
        &'a self,
        _path: &'a PathUri,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, FileSystemReadStream> {
        Box::pin(async {
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "test filesystem does not support streaming reads",
            ))
        })
    }

    fn write_file<'a>(
        &'a self,
        _path: &'a PathUri,
        _contents: Vec<u8>,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Box::pin(async move { unimplemented!("test filesystem only supports reads") })
    }

    fn create_directory<'a>(
        &'a self,
        _path: &'a PathUri,
        _create_directory_options: CreateDirectoryOptions,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Box::pin(async move { unimplemented!("test filesystem only supports reads") })
    }

    fn get_metadata<'a>(
        &'a self,
        path: &'a PathUri,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, FileMetadata> {
        Box::pin(async move {
            let path = path.to_abs_path()?;
            if let Some((failed_path, kind)) = &self.metadata_error
                && &path == failed_path
            {
                return Err(io::Error::new(*kind, "injected metadata failure"));
            }
            let metadata = tokio::fs::symlink_metadata(path.as_path()).await?;
            let file_type = metadata.file_type();
            let to_millis = |time: std::io::Result<std::time::SystemTime>| {
                time.ok()
                    .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|duration| duration.as_millis() as i64)
                    .unwrap_or_default()
            };
            Ok(FileMetadata {
                is_directory: file_type.is_dir(),
                is_file: file_type.is_file(),
                is_symlink: file_type.is_symlink(),
                size: metadata.len(),
                created_at_ms: to_millis(metadata.created()),
                modified_at_ms: to_millis(metadata.modified()),
            })
        })
    }

    fn read_directory<'a>(
        &'a self,
        _path: &'a PathUri,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Vec<ReadDirectoryEntry>> {
        Box::pin(async move { unimplemented!("test filesystem only supports reads") })
    }

    fn remove<'a>(
        &'a self,
        _path: &'a PathUri,
        _remove_options: RemoveOptions,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Box::pin(async move { unimplemented!("test filesystem only supports reads") })
    }

    fn copy<'a>(
        &'a self,
        _source_path: &'a PathUri,
        _destination_path: &'a PathUri,
        _copy_options: CopyOptions,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Box::pin(async move { unimplemented!("test filesystem only supports reads") })
    }
}

#[tokio::test]
async fn profile_v2_rejects_matching_legacy_profile_in_base_user_config() {
    let tmp = tempdir().expect("tempdir");
    let selected_config = tmp.path().join("work.config.toml");

    std::fs::write(
        tmp.path().join(CONFIG_TOML_FILE),
        r#"
model = "gpt-main"

[profiles.work]
model = "gpt-work"
"#,
    )
    .expect("write default user config");
    std::fs::write(&selected_config, r#"model = "gpt-work-v2""#)
        .expect("write selected user config");

    let mut overrides = LoaderOverrides::without_managed_config_for_tests();
    overrides.user_config_path = Some(AbsolutePathBuf::resolve_path_against_base(
        "work.config.toml",
        tmp.path(),
    ));
    overrides.user_config_profile = Some("work".parse().expect("profile-v2 name"));

    let err = load_config_layers_state(
        &TestFileSystem::default(),
        tmp.path(),
        /*cwd*/ None,
        &[],
        overrides,
        &crate::NoopThreadConfigLoader,
    )
    .await
    .expect_err("profile-v2 should reject a matching legacy profile in base user config");

    assert_eq!(
        err.kind(),
        io::ErrorKind::InvalidData,
        "a matching legacy profile should be a hard config error"
    );
    let message = err.to_string();
    assert!(
        message.contains("--profile `work` cannot be used"),
        "unexpected error message: {message}"
    );
    assert!(
        message.contains("config.toml"),
        "unexpected error message: {message}"
    );
    assert!(
        message.contains("[profiles.work]"),
        "unexpected error message: {message}"
    );
    assert!(
        message.contains("https://developers.openai.com/codex/config-advanced#profiles"),
        "unexpected error message: {message}"
    );
}

#[tokio::test]
async fn profile_v2_rejects_matching_legacy_profile_selector_in_base_user_config() {
    let tmp = tempdir().expect("tempdir");
    let selected_config = tmp.path().join("work.config.toml");

    std::fs::write(
        tmp.path().join(CONFIG_TOML_FILE),
        r#"
profile = "work"
model = "gpt-main"
"#,
    )
    .expect("write default user config");
    std::fs::write(&selected_config, r#"model = "gpt-work-v2""#)
        .expect("write selected user config");

    let mut overrides = LoaderOverrides::without_managed_config_for_tests();
    overrides.user_config_path = Some(AbsolutePathBuf::resolve_path_against_base(
        "work.config.toml",
        tmp.path(),
    ));
    overrides.user_config_profile = Some("work".parse().expect("profile-v2 name"));

    let err = load_config_layers_state(
        &TestFileSystem::default(),
        tmp.path(),
        /*cwd*/ None,
        &[],
        overrides,
        &crate::NoopThreadConfigLoader,
    )
    .await
    .expect_err("profile-v2 should reject a matching legacy profile selector");

    assert_eq!(
        err.kind(),
        io::ErrorKind::InvalidData,
        "a matching legacy profile selector should be a hard config error"
    );
    let message = err.to_string();
    assert!(
        message.contains("--profile `work` cannot be used"),
        "unexpected error message: {message}"
    );
    assert!(
        message.contains("profile = \"work\""),
        "unexpected error message: {message}"
    );
    assert!(
        message.contains("work.config.toml"),
        "unexpected error message: {message}"
    );
}

#[tokio::test]
async fn profile_v2_allows_unrelated_legacy_profiles_in_base_user_config() {
    let tmp = tempdir().expect("tempdir");
    let selected_config = tmp.path().join("work.config.toml");

    std::fs::write(
        tmp.path().join(CONFIG_TOML_FILE),
        r#"
model = "gpt-main"
model_reasoning_effort = "high"

[profiles.dev]
model = "gpt-dev"
"#,
    )
    .expect("write default user config");
    std::fs::write(&selected_config, r#"model = "gpt-work-v2""#)
        .expect("write selected user config");

    let mut overrides = LoaderOverrides::without_managed_config_for_tests();
    overrides.user_config_path = Some(AbsolutePathBuf::resolve_path_against_base(
        "work.config.toml",
        tmp.path(),
    ));
    overrides.user_config_profile = Some("work".parse().expect("profile-v2 name"));

    let stack = load_config_layers_state(
        &TestFileSystem::default(),
        tmp.path(),
        /*cwd*/ None,
        &[],
        overrides,
        &crate::NoopThreadConfigLoader,
    )
    .await
    .expect("profile-v2 should allow unrelated legacy profiles in base user config");
    let effective = stack.effective_config();
    assert_eq!(effective["model"].as_str(), Some("gpt-work-v2"));
    assert_eq!(effective["model_reasoning_effort"].as_str(), Some("high"));
    let user_layers = stack
        .layers_high_to_low()
        .into_iter()
        .filter(|layer| matches!(layer.name, ConfigLayerSource::User { .. }))
        .map(|layer| layer.config["model"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(user_layers, vec!["gpt-work-v2", "gpt-main"]);
}

#[tokio::test]
async fn config_layer_stack_preserves_project_discovery_context() {
    let codex_home = tempdir().expect("codex home");
    let workspace = tempdir().expect("workspace");
    let nested = workspace.path().join("src").join("nested");
    std::fs::create_dir_all(&nested).expect("nested cwd");
    std::fs::write(workspace.path().join(".codex-root"), "").expect("project marker");
    std::fs::create_dir(workspace.path().join(".git")).expect("git marker");
    let cwd = AbsolutePathBuf::from_absolute_path(&nested).expect("absolute cwd");
    let project_root = AbsolutePathBuf::from_absolute_path(workspace.path()).expect("project root");
    let fs = TestFileSystem::default();
    let cwd_key = toml::Value::String(cwd.as_path().to_string_lossy().into_owned()).to_string();
    let user_config = format!("[projects.{cwd_key}]\ntrust_level = \"trusted\"\n");
    std::fs::write(codex_home.path().join(CONFIG_TOML_FILE), &user_config).expect("user config");

    let stack = load_config_layers_state(
        &fs,
        codex_home.path(),
        Some(cwd.clone()),
        &[(
            "project_root_markers".to_string(),
            toml::Value::Array(vec![toml::Value::String(".codex-root".to_string())]),
        )],
        LoaderOverrides::without_managed_config_for_tests(),
        &crate::NoopThreadConfigLoader,
    )
    .await
    .expect("load config with project discovery");

    let discovery = stack.project_discovery().expect("project discovery");
    assert!(discovery.matches(&cwd, &fs));
    assert_eq!(discovery.cwd(), &cwd);
    assert_eq!(discovery.project_root(), &project_root);
    assert_eq!(discovery.project_root_markers(), &[".codex-root"]);
    assert_eq!(discovery.git_checkout_root(), Some(&project_root));
    let lookup_keys = discovery
        .active_project_lookup_keys()
        .expect("normalized active-project keys");
    assert!(lookup_keys.starts_with(&normalized_project_lookup_keys(cwd.as_path())));
    let config: ConfigToml = toml::from_str(&user_config).expect("typed user config");
    assert_eq!(
        config
            .get_active_project_for_lookup_keys(lookup_keys)
            .expect("active project")
            .trust_level,
        Some(TrustLevel::Trusted)
    );

    let updated = stack.with_user_config(
        &AbsolutePathBuf::resolve_path_against_base(CONFIG_TOML_FILE, codex_home.path()),
        toml::Value::Table(Default::default()),
    );
    assert_eq!(updated.project_discovery(), Some(discovery));
}

#[tokio::test]
async fn user_marker_edits_invalidate_discovery_but_model_edits_preserve_it() {
    let home = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    let cwd = AbsolutePathBuf::from_absolute_path(workspace.path()).unwrap();
    let config_path = AbsolutePathBuf::resolve_path_against_base(CONFIG_TOML_FILE, home.path());
    std::fs::write(&config_path, "project_root_markers = ['.git']\n").unwrap();
    // Legacy managed layers arrive after project discovery and cannot select
    // its boundary, including when a later user edit considers reusing it.
    let managed_config_path = home.path().join("managed_config.toml");
    std::fs::write(
        &managed_config_path,
        "project_root_markers = ['.managed-root']\n",
    )
    .unwrap();
    let fs = TestFileSystem::default();
    let stack = load_config_layers_state(
        &fs,
        home.path(),
        Some(cwd),
        &[],
        LoaderOverrides::with_managed_config_path_for_tests(managed_config_path),
        &crate::NoopThreadConfigLoader,
    )
    .await
    .unwrap();
    let discovery = stack.project_discovery().expect("loaded discovery");
    assert_eq!(discovery.project_root_markers(), &[".git"]);
    for (contents, reusable) in [
        ("model = 'different'", true),
        ("project_root_markers = ['.git']", true),
        ("project_root_markers = []", false),
        ("project_root_markers = ['.new-root']", false),
        ("project_root_markers = false", false),
    ] {
        let updated = stack.with_user_config(&config_path, toml::from_str(contents).unwrap());
        assert_eq!(
            updated.project_discovery(),
            reusable.then_some(discovery),
            "{contents}"
        );
        assert_eq!(
            stack.with_user_layer_from(&updated).project_discovery(),
            reusable.then_some(discovery),
            "{contents}"
        );
    }
}

#[tokio::test]
async fn discovery_reports_operational_metadata_errors() {
    let home = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    let cwd = AbsolutePathBuf::from_absolute_path(workspace.path()).unwrap();
    for (relative_path, markers) in [
        (".root", vec![".root"]),
        (".git", vec![]),
        (".codex", vec![]),
        ("", vec![]),
    ] {
        let failed_path = if relative_path.is_empty() {
            cwd.clone()
        } else {
            cwd.join(relative_path)
        };
        for kind in [io::ErrorKind::PermissionDenied, io::ErrorKind::Other] {
            let fs = TestFileSystem {
                metadata_error: Some((failed_path.clone(), kind)),
            };
            let error = load_config_layers_state(
                &fs,
                home.path(),
                Some(cwd.clone()),
                &[(
                    "project_root_markers".to_string(),
                    TomlValue::try_from(&markers).unwrap(),
                )],
                LoaderOverrides::without_managed_config_for_tests(),
                &crate::NoopThreadConfigLoader,
            )
            .await
            .unwrap_err();
            assert_eq!(error.kind(), kind);
            assert!(
                error
                    .to_string()
                    .contains(&failed_path.display().to_string()),
                "{error}"
            );
        }
    }
}

#[tokio::test]
async fn config_file_boundaries_migrate_before_strict_validation() {
    for source in ["user", "system", "managed", "project"] {
        let home = tempdir().unwrap();
        let workspace = tempdir().unwrap();
        let cwd = AbsolutePathBuf::from_absolute_path(workspace.path()).unwrap();
        let mut overrides = LoaderOverrides::without_managed_config_for_tests();
        let config_path = match source {
            "system" => {
                let path = home.path().join("system.toml");
                overrides.system_config_path = Some(path.clone());
                path
            }
            "managed" => {
                let path = home.path().join("managed_config.toml");
                overrides.managed_config_path = Some(path.clone());
                path
            }
            "project" => {
                std::fs::create_dir(cwd.join(".codex")).unwrap();
                let key = TomlValue::String(cwd.to_string_lossy().into_owned());
                std::fs::write(
                    home.path().join(CONFIG_TOML_FILE),
                    format!("[projects.{key}]\ntrust_level = 'trusted'\n"),
                )
                .unwrap();
                cwd.join(".codex").join(CONFIG_TOML_FILE).to_path_buf()
            }
            _ => home.path().join(CONFIG_TOML_FILE),
        };
        std::fs::write(
            &config_path,
            "[features]\nexperimental_use_unified_exec_tool = false\ntelepathy = true\n",
        )
        .unwrap();
        let mut options = ConfigLoadOptions::from(overrides.clone());
        options.strict_config = true;
        let stack = load_config_layers_state(
            &TestFileSystem::default(),
            home.path(),
            Some(cwd.clone()),
            &[("project_root_markers".to_string(), TomlValue::Array(vec![]))],
            options,
            &crate::NoopThreadConfigLoader,
        )
        .await
        .unwrap();
        let effective = stack.effective_config();
        assert_eq!(
            effective["features"]["unified_exec"].as_bool(),
            Some(false),
            "{source}"
        );
        assert!(
            effective["features"]
                .get("experimental_use_unified_exec_tool")
                .is_none(),
            "{source}"
        );
        assert!(effective["features"].get("telepathy").is_none(), "{source}");
        std::fs::write(&config_path, "config_version = 999\n").unwrap();
        let error = load_config_layers_state(
            &TestFileSystem::default(),
            home.path(),
            Some(cwd),
            &[("project_root_markers".to_string(), TomlValue::Array(vec![]))],
            overrides,
            &crate::NoopThreadConfigLoader,
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(
            error.to_string().contains("unsupported config_version 999"),
            "{source}: {error}"
        );
    }
}

#[tokio::test]
async fn session_feature_tables_from_newer_clients_do_not_block_config_loading() {
    let home = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    let config_path = home.path().join(CONFIG_TOML_FILE);
    let saved_config = "[features]\nunified_exec = true\nmemories = false\n";
    std::fs::write(&config_path, saved_config).unwrap();
    let stack = load_config_layers_state(
        &TestFileSystem::default(),
        home.path(),
        Some(AbsolutePathBuf::from_absolute_path(workspace.path()).unwrap()),
        &[
            (
                "features.tool_registry.error_on_tool_collisions".to_string(),
                TomlValue::Boolean(true),
            ),
            (
                "features.token_budget".to_string(),
                toml::toml! { enabled = true }.into(),
            ),
            (
                "features.unified_exec".to_string(),
                TomlValue::Boolean(false),
            ),
        ],
        LoaderOverrides::without_managed_config_for_tests(),
        &crate::NoopThreadConfigLoader,
    )
    .await
    .expect("session overrides from a newer client should load");
    let config: ConfigToml = stack.effective_config().try_into().unwrap();
    let features = config.features.unwrap().entries();
    assert_eq!(features.get("unified_exec"), Some(&false));
    assert_eq!(features.get("memories"), Some(&false));
    assert!(!features.contains_key("tool_registry"));
    assert!(!features.contains_key("token_budget"));
    assert_eq!(std::fs::read_to_string(config_path).unwrap(), saved_config);
}

#[tokio::test]
async fn overridden_invalid_field_does_not_change_relative_path_base() {
    let home = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    std::fs::write(
        home.path().join(CONFIG_TOML_FILE),
        "model = 42\nlog_dir = 'logs'\n",
    )
    .unwrap();
    let stack = load_config_layers_state(
        &TestFileSystem::default(),
        home.path(),
        Some(AbsolutePathBuf::from_absolute_path(workspace.path()).unwrap()),
        &[(
            "model".to_string(),
            TomlValue::String("valid-model".to_string()),
        )],
        LoaderOverrides::without_managed_config_for_tests(),
        &crate::NoopThreadConfigLoader,
    )
    .await
    .unwrap();
    let config: ConfigToml = stack.effective_config().try_into().unwrap();
    assert_eq!(config.model.as_deref(), Some("valid-model"));
    assert_eq!(config.log_dir.unwrap().as_path(), home.path().join("logs"));
}

#[tokio::test]
async fn disabled_project_config_ignores_unsupported_version() {
    let home = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    std::fs::create_dir(workspace.path().join(".codex")).unwrap();
    std::fs::write(
        workspace.path().join(".codex/config.toml"),
        "config_version = 999\nmodel = 'untrusted'\n",
    )
    .unwrap();
    let stack = load_config_layers_state(
        &TestFileSystem::default(),
        home.path(),
        Some(AbsolutePathBuf::from_absolute_path(workspace.path()).unwrap()),
        &[("project_root_markers".to_string(), TomlValue::Array(vec![]))],
        LoaderOverrides::without_managed_config_for_tests(),
        &crate::NoopThreadConfigLoader,
    )
    .await
    .unwrap();
    assert!(stack.effective_config().get("model").is_none());
    let layers = stack.get_layers(crate::ConfigLayerStackOrdering::LowestPrecedenceFirst, true);
    let project = layers
        .iter()
        .find(|layer| matches!(layer.name, ConfigLayerSource::Project { .. }))
        .unwrap();
    assert!(project.is_disabled());
    assert!(project.config.as_table().unwrap().is_empty());
}

#[tokio::test]
async fn root_checkout_hook_config_obeys_version_boundary() {
    let root = tempdir().unwrap();
    let folder = AbsolutePathBuf::from_absolute_path(root.path()).unwrap();
    let file = folder.join(CONFIG_TOML_FILE);
    std::fs::write(&file, "config_version = 999\n").unwrap();
    let config = TomlValue::Table(toml::map::Map::new());
    let error = merge_root_checkout_project_hooks(
        &TestFileSystem::default(),
        config.clone(),
        Some(&folder),
        true,
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("unsupported config_version 999"));
    assert_eq!(
        merge_root_checkout_project_hooks(
            &TestFileSystem::default(),
            config.clone(),
            Some(&folder),
            false
        )
        .await
        .unwrap(),
        config
    );
}
