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
            .get("chronicle")
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

struct TestFileSystem;

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
            let metadata = tokio::fs::symlink_metadata(path.to_abs_path()?.as_path()).await?;
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
        &TestFileSystem,
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
        &TestFileSystem,
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

    load_config_layers_state(
        &TestFileSystem,
        tmp.path(),
        /*cwd*/ None,
        &[],
        overrides,
        &crate::NoopThreadConfigLoader,
    )
    .await
    .expect("profile-v2 should allow unrelated legacy profiles in base user config");
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
    let fs = TestFileSystem;
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
