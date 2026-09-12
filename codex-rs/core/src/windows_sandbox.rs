use crate::config::Config;
use crate::config::edit::ConfigEditsBuilder;
use codex_config::config_toml::ConfigToml;
use codex_config::types::WindowsSandboxModeToml;
use codex_features::Feature;
use codex_features::Features;
use codex_features::FeaturesToml;
use codex_login::default_client::originator;
use codex_otel::sanitize_metric_tag_value;
use codex_protocol::config_types::WindowsSandboxLevel;
use codex_protocol::models::PermissionProfile;
use codex_utils_absolute_path::AbsolutePathBuf;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::time::Instant;

pub trait WindowsSandboxLevelExt {
    fn from_config(config: &Config) -> WindowsSandboxLevel;
    fn from_features(features: &Features) -> WindowsSandboxLevel;
}

impl WindowsSandboxLevelExt for WindowsSandboxLevel {
    fn from_config(config: &Config) -> WindowsSandboxLevel {
        match config.permissions.windows_sandbox_mode {
            Some(WindowsSandboxModeToml::Elevated) => WindowsSandboxLevel::Elevated,
            Some(WindowsSandboxModeToml::Unelevated) => WindowsSandboxLevel::RestrictedToken,
            None => Self::from_features(&config.features),
        }
    }

    fn from_features(features: &Features) -> WindowsSandboxLevel {
        if features.enabled(Feature::WindowsSandboxElevated) {
            return WindowsSandboxLevel::Elevated;
        }
        if features.enabled(Feature::WindowsSandbox) {
            WindowsSandboxLevel::RestrictedToken
        } else {
            WindowsSandboxLevel::Disabled
        }
    }
}

pub fn windows_sandbox_level_from_config(config: &Config) -> WindowsSandboxLevel {
    WindowsSandboxLevel::from_config(config)
}

pub fn windows_sandbox_level_from_features(features: &Features) -> WindowsSandboxLevel {
    WindowsSandboxLevel::from_features(features)
}

pub fn resolve_windows_sandbox_mode(cfg: &ConfigToml) -> Option<WindowsSandboxModeToml> {
    cfg.windows
        .as_ref()
        .and_then(|windows| windows.sandbox)
        .or_else(|| legacy_windows_sandbox_mode(cfg.features.as_ref()))
}

pub fn resolve_windows_sandbox_private_desktop(cfg: &ConfigToml) -> bool {
    cfg.windows
        .as_ref()
        .and_then(|windows| windows.sandbox_private_desktop)
        .unwrap_or(true)
}

pub fn legacy_windows_sandbox_mode(
    features: Option<&FeaturesToml>,
) -> Option<WindowsSandboxModeToml> {
    let entries = features.map(FeaturesToml::entries)?;
    legacy_windows_sandbox_mode_from_entries(&entries)
}

pub fn legacy_windows_sandbox_mode_from_entries(
    entries: &BTreeMap<String, bool>,
) -> Option<WindowsSandboxModeToml> {
    if entries
        .get(Feature::WindowsSandboxElevated.key())
        .copied()
        .unwrap_or(false)
    {
        return Some(WindowsSandboxModeToml::Elevated);
    }
    if entries
        .get(Feature::WindowsSandbox.key())
        .copied()
        .unwrap_or(false)
        || entries
            .get("enable_experimental_windows_sandbox")
            .copied()
            .unwrap_or(false)
    {
        Some(WindowsSandboxModeToml::Unelevated)
    } else {
        None
    }
}

pub fn sandbox_setup_is_complete(codex_home: &Path) -> bool {
    codex_windows_sandbox::sandbox_setup_is_complete(codex_home)
}

pub fn elevated_setup_failure_details(err: &anyhow::Error) -> Option<(String, String)> {
    let failure = codex_windows_sandbox::extract_setup_failure(err)?;
    let code = failure.code.as_str().to_string();
    let message = codex_windows_sandbox::sanitize_setup_metric_tag_value(&failure.message);
    Some((code, message))
}

pub fn elevated_setup_failure_metric_name(err: &anyhow::Error) -> &'static str {
    if codex_windows_sandbox::extract_setup_failure(err).is_some_and(|failure| {
        matches!(
            failure.code,
            codex_windows_sandbox::SetupErrorCode::OrchestratorHelperLaunchCanceled
        )
    }) {
        "codex.windows_sandbox.elevated_setup_canceled"
    } else {
        "codex.windows_sandbox.elevated_setup_failure"
    }
}

pub fn run_elevated_setup(
    permission_profile: &PermissionProfile,
    workspace_roots: &[AbsolutePathBuf],
    command_cwd: &Path,
    env_map: &HashMap<String, String>,
    codex_home: &Path,
) -> anyhow::Result<()> {
    let permissions =
        codex_windows_sandbox::ResolvedWindowsSandboxPermissions::try_from_permission_profile_for_workspace_roots(
            permission_profile,
            workspace_roots,
        )?;
    codex_windows_sandbox::run_elevated_setup(
        codex_windows_sandbox::SandboxSetupRequest {
            permissions: &permissions,
            command_cwd,
            env_map,
            codex_home,
            proxy_enforced: false,
        },
        codex_windows_sandbox::SetupRootOverrides::default(),
    )
}

pub fn run_elevated_provisioning_setup(codex_home: &Path, real_user: &str) -> anyhow::Result<()> {
    codex_windows_sandbox::run_elevated_provisioning_setup(codex_home, real_user)
}

pub fn run_legacy_setup_preflight(
    permission_profile: &PermissionProfile,
    workspace_roots: &[AbsolutePathBuf],
    command_cwd: &Path,
    env_map: &HashMap<String, String>,
    codex_home: &Path,
) -> anyhow::Result<()> {
    codex_windows_sandbox::run_windows_sandbox_legacy_preflight(
        permission_profile,
        workspace_roots,
        codex_home,
        command_cwd,
        env_map,
    )
}

pub fn run_setup_refresh_with_extra_read_roots(
    permission_profile: &PermissionProfile,
    workspace_roots: &[AbsolutePathBuf],
    command_cwd: &Path,
    env_map: &HashMap<String, String>,
    codex_home: &Path,
    extra_read_roots: Vec<PathBuf>,
) -> anyhow::Result<()> {
    codex_windows_sandbox::run_setup_refresh_with_extra_read_roots(
        permission_profile,
        workspace_roots,
        command_cwd,
        env_map,
        codex_home,
        extra_read_roots,
        /*proxy_enforced*/ false,
    )
}

pub fn run_strict_read_root_grant(
    permission_profile: &PermissionProfile,
    workspace_roots: &[AbsolutePathBuf],
    command_cwd: &Path,
    env_map: &HashMap<String, String>,
    codex_home: &Path,
    read_root: PathBuf,
) -> anyhow::Result<()> {
    codex_windows_sandbox::run_strict_read_root_grant(
        permission_profile,
        workspace_roots,
        command_cwd,
        env_map,
        codex_home,
        read_root,
        /*proxy_enforced*/ false,
    )
}

pub use codex_protocol::config_types::WindowsSandboxSetupMode;

#[derive(Debug, Clone)]
pub struct WindowsSandboxSetupRequest {
    pub mode: WindowsSandboxSetupMode,
    pub permission_profile: PermissionProfile,
    pub workspace_roots: Vec<AbsolutePathBuf>,
    pub command_cwd: PathBuf,
    pub env_map: HashMap<String, String>,
    pub codex_home: PathBuf,
}

pub async fn run_windows_sandbox_setup(request: WindowsSandboxSetupRequest) -> anyhow::Result<()> {
    let start = Instant::now();
    let mode = request.mode;
    let originator_tag = sanitize_metric_tag_value(originator().value.as_str());
    let result = run_windows_sandbox_setup_and_persist(request).await;

    match result {
        Ok(()) => {
            emit_windows_sandbox_setup_success_metrics(
                mode,
                originator_tag.as_str(),
                start.elapsed(),
            );
            Ok(())
        }
        Err(err) => {
            emit_windows_sandbox_setup_failure_metrics(
                mode,
                originator_tag.as_str(),
                start.elapsed(),
                &err,
                codex_otel::global().as_ref(),
            );
            Err(err)
        }
    }
}

async fn run_windows_sandbox_setup_and_persist(
    request: WindowsSandboxSetupRequest,
) -> anyhow::Result<()> {
    let mode = request.mode;
    let permission_profile = request.permission_profile;
    let workspace_roots = request.workspace_roots;
    let command_cwd = request.command_cwd;
    let env_map = request.env_map;
    let codex_home = request.codex_home;
    let setup_codex_home = codex_home.clone();

    let setup_native = move || -> anyhow::Result<()> {
        match mode {
            WindowsSandboxSetupMode::Elevated => {
                if !sandbox_setup_is_complete(setup_codex_home.as_path()) {
                    run_elevated_setup(
                        &permission_profile,
                        workspace_roots.as_slice(),
                        command_cwd.as_path(),
                        &env_map,
                        setup_codex_home.as_path(),
                    )?;
                }
            }
            WindowsSandboxSetupMode::Unelevated => {
                run_legacy_setup_preflight(
                    &permission_profile,
                    workspace_roots.as_slice(),
                    command_cwd.as_path(),
                    &env_map,
                    setup_codex_home.as_path(),
                )?;
            }
        }
        Ok(())
    };
    #[cfg(test)]
    let setup_native = SETUP_NATIVE_OVERRIDE
        .with(|override_fn| override_fn.borrow_mut().take())
        .unwrap_or_else(|| Box::new(setup_native));

    // Native setup and its persisted mode are one owned blocking operation.
    // Dropping the awaiting RPC must not leave a successful setup unrecorded.
    tokio::task::spawn_blocking(move || {
        setup_native()?;
        ConfigEditsBuilder::new(codex_home.as_path())
            .set_windows_sandbox_mode(windows_sandbox_setup_mode_tag(mode))
            .clear_legacy_windows_sandbox_keys()
            .apply_blocking()
            .map_err(|err| anyhow::anyhow!("failed to persist windows sandbox mode: {err}"))
    })
    .await
    .map_err(|join_err| anyhow::anyhow!("windows sandbox setup task failed: {join_err}"))?
}

#[cfg(test)]
thread_local! {
    static SETUP_NATIVE_OVERRIDE: std::cell::RefCell<Option<Box<dyn FnOnce() -> anyhow::Result<()> + Send>>> =
        const { std::cell::RefCell::new(None) };
}

fn emit_windows_sandbox_setup_success_metrics(
    mode: WindowsSandboxSetupMode,
    originator_tag: &str,
    duration: std::time::Duration,
) {
    let Some(metrics) = codex_otel::global() else {
        return;
    };
    let mode_tag = windows_sandbox_setup_mode_tag(mode);
    let _ = metrics.record_duration(
        "codex.windows_sandbox.setup_duration_ms",
        duration,
        &[
            ("result", "success"),
            ("originator", originator_tag),
            ("mode", mode_tag),
        ],
    );
    let _ = metrics.counter(
        "codex.windows_sandbox.setup_success",
        /*inc*/ 1,
        &[("originator", originator_tag), ("mode", mode_tag)],
    );
}

fn emit_windows_sandbox_setup_failure_metrics(
    mode: WindowsSandboxSetupMode,
    originator_tag: &str,
    duration: std::time::Duration,
    err: &anyhow::Error,
    metrics: Option<&codex_otel::MetricsClient>,
) {
    tracing::warn!(
        error = %err,
        mode = windows_sandbox_setup_mode_tag(mode),
        "Windows sandbox setup failed"
    );
    let Some(metrics) = metrics else {
        return;
    };
    let originator_tag = codex_otel::bounded_originator_tag_value(originator_tag);
    let mode_tag = windows_sandbox_setup_mode_tag(mode);
    let _ = metrics.record_duration(
        "codex.windows_sandbox.setup_duration_ms",
        duration,
        &[
            ("result", "failure"),
            ("originator", originator_tag),
            ("mode", mode_tag),
        ],
    );
    let _ = metrics.counter(
        "codex.windows_sandbox.setup_failure",
        /*inc*/ 1,
        &[("originator", originator_tag), ("mode", mode_tag)],
    );

    if matches!(mode, WindowsSandboxSetupMode::Elevated) {
        {
            let mut failure_tags: Vec<(&str, &str)> = vec![("originator", originator_tag)];
            if let Some(failure) = codex_windows_sandbox::extract_setup_failure(err) {
                // Error messages contain workspace paths and belong in diagnostics, not labels.
                failure_tags.push(("code", failure.code.as_str()));
            }
            let _ = metrics.counter(
                elevated_setup_failure_metric_name(err),
                /*inc*/ 1,
                &failure_tags,
            );
        }
    } else {
        let _ = metrics.counter(
            "codex.windows_sandbox.legacy_setup_preflight_failed",
            /*inc*/ 1,
            &[("originator", originator_tag)],
        );
    }
}

fn windows_sandbox_setup_mode_tag(mode: WindowsSandboxSetupMode) -> &'static str {
    match mode {
        WindowsSandboxSetupMode::Elevated => "elevated",
        WindowsSandboxSetupMode::Unelevated => "unelevated",
    }
}

#[cfg(test)]
mod setup_ownership_tests {
    use super::*;

    fn request(home: &std::path::Path) -> WindowsSandboxSetupRequest {
        WindowsSandboxSetupRequest {
            mode: WindowsSandboxSetupMode::Unelevated,
            permission_profile: PermissionProfile::read_only(),
            workspace_roots: Vec::new(),
            command_cwd: home.to_path_buf(),
            env_map: HashMap::new(),
            codex_home: home.to_path_buf(),
        }
    }

    #[test]
    fn setup_persists_native_success_after_caller_and_runtime_are_dropped() {
        let home = tempfile::tempdir().unwrap();
        let marker = home.path().join("native-setup-completed");
        let native_marker = marker.clone();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        SETUP_NATIVE_OVERRIDE.with(|override_fn| {
            *override_fn.borrow_mut() = Some(Box::new(move || {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                std::fs::write(native_marker, "native setup succeeded")?;
                Ok(())
            }));
        });
        runtime.block_on(async {
            let mut setup = Box::pin(run_windows_sandbox_setup(request(home.path())));
            assert!(futures::poll!(&mut setup).is_pending());
            started_rx.await.unwrap();
            assert!(!home.path().join("config.toml").exists());
            drop(setup);
        });
        runtime.shutdown_background();
        release_tx.send(()).unwrap();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let config = loop {
            if let Ok(config) = std::fs::read_to_string(home.path().join("config.toml")) {
                break config;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "successful native setup must persist its mode after cancellation"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        let config: toml::Value = toml::from_str(&config).unwrap();
        assert_eq!(config["windows"]["sandbox"].as_str(), Some("unelevated"));
        assert_eq!(
            std::fs::read_to_string(marker).unwrap(),
            "native setup succeeded"
        );
    }

    #[tokio::test]
    async fn setup_native_failure_does_not_persist_a_successful_mode() {
        let home = tempfile::tempdir().unwrap();
        SETUP_NATIVE_OVERRIDE.with(|override_fn| {
            *override_fn.borrow_mut() = Some(Box::new(|| anyhow::bail!("native setup refused")));
        });
        let error = run_windows_sandbox_setup(request(home.path()))
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "native setup refused");
        assert!(!home.path().join("config.toml").exists());
    }
}

#[cfg(test)]
#[path = "windows_sandbox_tests.rs"]
mod tests;
