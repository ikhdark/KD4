use std::io;
use std::path::Path;
use std::path::PathBuf;

use codex_app_server_protocol_types::DESKTOP_CLIENT_NAME;
use codex_app_server_protocol_types::DESKTOP_RUNTIME_RECEIPT_RELATIVE_PATH;
use codex_app_server_protocol_types::DesktopRuntimeReceipt;
#[cfg(windows)]
use codex_app_server_protocol_types::DesktopStdioPeer;
use codex_app_server_protocol_types::ServerPackageLayout;
use codex_app_server_protocol_types::ServerRuntimeInfo;
use codex_app_server_protocol_types::ServerRuntimeWarning;
use codex_app_server_transport::AppServerTransport;
use codex_app_server_transport::RemoteControlPolicy;
use codex_app_server_transport::RemoteControlStartupMode;
use codex_core::config::Config;
use codex_install_context::CodexPackageLayout;
use codex_install_context::InstallContext;
use codex_install_context::InstallMethod;
use codex_utils_absolute_path as path_utils;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_protocol::models::PermissionProfile;
use codex_protocol::timing::observations;
use codex_protocol::timing::process_monotonic_ns;
use codex_utils_build_info::BuildInfo;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;

const LOCAL_PUBLISH_DIR_ENV: &str = "CODEX_LOCAL_PUBLISH_DIR";
const LOCAL_CLI_PATH_ENV: &str = "CODEX_CLI_PATH";
const ACTION_PUBLISH_LOCAL_CODEX: &str = "publishLocalCodex";
const ACTION_RESTART_CODEX_DESKTOP: &str = "restartCodexDesktop";

// Domain-separate the digest from other projections and bump this version if
// the included facts or their canonical encoding change.
const NATIVE_STARTUP_POLICY_SCHEMA: &str = "app_server_startup_policy.v1";

pub struct NativeCaptureAdmission<'a> {
    pub transport: &'a AppServerTransport,
    pub remote_control_mode: RemoteControlStartupMode,
    pub remote_control_policy: RemoteControlPolicy,
    pub config_resolved: bool,
    pub state_db_available: bool,
    pub ignore_user_config: bool,
    pub start_plugins: bool,
    pub default_analytics_enabled: bool,
}

impl NativeCaptureAdmission<'_> {
    fn startup_policy_sha256(&self, config: &Config) -> Option<String> {
        if !self.config_resolved {
            // Default config keeps non-strict startup repairable; it does not
            // stand in for the effective config and requirements that failed.
            return None;
        }

        let transport = match self.transport {
            AppServerTransport::Stdio => "stdio",
            AppServerTransport::UnixSocket { .. } => "unix_socket",
            AppServerTransport::WebSocket { .. } => "websocket",
            AppServerTransport::Off => "off",
        };
        let permission_kind = match config.permissions.permission_profile() {
            PermissionProfile::Managed { .. } => "managed",
            PermissionProfile::Disabled => "disabled",
            PermissionProfile::External { .. } => "external",
        };
        let mut features: Vec<_> = config
            .features
            .enabled_features()
            .into_iter()
            .map(|feature| feature.key())
            .collect();
        features.sort_unstable();

        // Only typed effective policy values and code-defined labels enter the
        // digest. Never serialize a config layer, provider, permission profile,
        // filesystem rules or other settings containing host/user-supplied text.
        // The array fixes encoding order even if JSON map features change.
        let projection = json!([
            NATIVE_STARTUP_POLICY_SCHEMA,
            transport,
            format!("{:?}", self.remote_control_mode),
            // The caller already enforced the merged allow_remote_control requirement.
            format!("{:?}", self.remote_control_policy),
            self.ignore_user_config,
            self.start_plugins,
            config.analytics_enabled.unwrap_or(self.default_analytics_enabled),
            config.validation_mode,
            config.enforce_residency.value().is_some(),
            config.permissions.approval_policy.value(),
            permission_kind,
            config.permissions.network_sandbox_policy(),
            config.permissions.allow_login_shell,
            config.web_search_mode.value(),
            features,
        ]);
        Some(hex::encode(Sha256::digest(projection.to_string())))
    }
}

/// Record the app-server's actual resolved startup admission, independently of
/// Desktop's client-triggered handshake receipt.
pub async fn record_native_capture_admission(
    config: &Config,
    admission: NativeCaptureAdmission<'_>,
    deadline: tokio::time::Instant,
) {
    if !observations::enabled() {
        return;
    }

    // Reuse the process hash (not a publication/expected-path lookup). Do not
    // perform executable I/O on an async worker or extend the startup budget.
    let executable_sha256 = tokio::time::timeout_at(
        deadline,
        tokio::task::spawn_blocking(codex_utils_build_info::executable_sha256),
    )
    .await
    .ok()
    .and_then(Result::ok)
    .flatten();
    let executable_hash_method = if cfg!(target_os = "linux") {
        "pinned_running_image"
    } else {
        "current_executable_path_snapshot"
    };

    let startup_policy_sha256 = admission.startup_policy_sha256(config);
    let monotonic_before_ns = u64::try_from(process_monotonic_ns()).ok();
    let unix_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| u64::try_from(elapsed.as_nanos()).ok());
    let monotonic_after_ns = u64::try_from(process_monotonic_ns()).ok();
    // The journal envelope supplies the producer PID and process-unique clock
    // ID (also its capture epoch). The sample bridges that clock to UTC only.
    let _ = observations::record(
        "mobile_backend_admission",
        "",
        "",
        "",
        json!({
            "admission_schema": 1,
            "executable_sha256": executable_sha256,
            "executable_hash_method": executable_hash_method,
            "startup_policy_schema": NATIVE_STARTUP_POLICY_SCHEMA,
            "startup_policy_sha256": startup_policy_sha256,
            "startup_config_resolved": admission.config_resolved,
            "state_db_available": admission.state_db_available,
            "unix_ns": unix_ns,
            "monotonic_before_ns": monotonic_before_ns,
            "monotonic_after_ns": monotonic_after_ns,
        }),
    );
}

pub(crate) fn write_desktop_runtime_receipt(
    codex_home: &Path,
    client_name: &str,
    process_rpc_transport: &str,
    connection_id: u64,
    enabled_features: Vec<String>,
) -> io::Result<Option<PathBuf>> {
    if !matches!(client_name, DESKTOP_CLIENT_NAME | "Codex Desktop") {
        return Ok(None);
    }
    let started = std::time::Instant::now();
    tracing::debug!("publishing Desktop runtime receipt");
    let executable_path = std::env::current_exe()?;
    let executable_sha256 = codex_utils_build_info::executable_sha256()
        .ok_or_else(|| io::Error::other("could not hash the running executable"))?
        .to_string();
    let build_info = BuildInfo::current();
    tracing::debug!(elapsed_ms = started.elapsed().as_millis(), "hashed running Desktop backend");
    // This owner runs after readiness. Hash bytes rather than interpreting path
    // equality as evidence that the running build is the published build.
    let published = expected_local_binary_path();
    let (published_executable_sha256, published_build_match, expected_codex_home, codex_home_match) =
        published_identity(published.as_ref(), &executable_sha256, codex_home);
    let receipt = DesktopRuntimeReceipt {
        schema_version: 3,
        pid: std::process::id(),
        executable_path,
        executable_sha256,
        codex_home: codex_home.to_path_buf(),
        client_name: DESKTOP_CLIENT_NAME.to_string(),
        // A client-supplied name does not attest the GUI process, its creation
        // identity, or ownership of this connection. Consumers must not treat
        // this backend-only receipt as successful Desktop activation proof.
        process_rpc_transport: Some(process_rpc_transport.to_string()),
        connection_id: Some(connection_id),
        enabled_features,
        stdio_peer: desktop_stdio_peer(process_rpc_transport, connection_id),
        build_version: build_info.version.to_string(),
        build_commit: build_info.commit.to_string(),
        build_dirty: build_info.dirty.to_string(),
        build_profile: build_info.profile.to_string(),
        build_built: build_info.built.to_string(),
        published_executable_sha256,
        published_build_match,
        expected_codex_home,
        codex_home_match,
    };
    let contents = serde_json::to_string_pretty(&receipt).map_err(io::Error::other)?;
    let receipt_path = codex_home.join(DESKTOP_RUNTIME_RECEIPT_RELATIVE_PATH);
    codex_file_system::write_atomically(&receipt_path, &contents)?;
    tracing::debug!(elapsed_ms = started.elapsed().as_millis(), "published Desktop runtime receipt");
    Ok(Some(receipt_path))
}

fn desktop_stdio_peer(
    transport: &str,
    connection_id: u64,
) -> Option<codex_app_server_protocol_types::DesktopStdioPeer> {
    // The stdio listener is opened before remote-control listeners and receives
    // connection zero. A process-wide stdio label alone must not attest a later
    // remote connection initialized with a Desktop client name.
    if transport != "stdio" || connection_id != 0 {
        return None;
    }
    #[cfg(windows)]
    {
        let peer = codex_utils_build_info::windows_process::stdio_peer()?;
        Some(DesktopStdioPeer {
            backend_creation_time: peer.backend_creation_time,
            input_handle: peer.input_handle,
            output_handle: peer.output_handle,
            peer_pid: peer.peer_pid,
            peer_creation_time: peer.peer_creation_time,
        })
    }
    #[cfg(not(windows))]
    {
        None
    }
}

fn published_identity(
    published: Option<&AbsolutePathBuf>,
    running_sha256: &str,
    codex_home: &Path,
) -> (Option<String>, Option<bool>, Option<PathBuf>, Option<bool>) {
    let hash = published.and_then(|path| codex_utils_build_info::file_sha256(path.as_path()).ok());
    let build_match = hash.as_ref().map(|hash| hash == running_sha256);
    let home = published
        .and_then(|path| path.as_path().parent())
        .filter(|parent| parent.file_name().is_some_and(|name| name == "bin"))
        .and_then(Path::parent)
        .map(Path::to_path_buf);
    let home_match = home
        .as_ref()
        .map(|expected| path_utils::paths_match_after_normalization(codex_home, expected));
    (hash, build_match, home, home_match)
}

pub(crate) fn current() -> ServerRuntimeInfo {
    let build_info = BuildInfo::current();
    let executable_path = current_executable_path();
    let install_context = InstallContext::current();
    let expected_local_binary_path = expected_local_binary_path();
    let local_binary_match = executable_path
        .as_ref()
        .zip(expected_local_binary_path.as_ref())
        .map(|(actual, expected)| {
            path_utils::paths_match_after_normalization(actual.as_path(), expected.as_path())
        });

    let mut warnings = local_binary_warnings(
        executable_path.as_ref(),
        expected_local_binary_path.as_ref(),
        local_binary_match,
    );
    warnings.extend(build_warnings(
        build_info,
        expected_local_binary_path.is_some(),
    ));

    ServerRuntimeInfo {
        executable_path,
        install_method: install_method_label(&install_context.method).to_string(),
        package_layout: install_context
            .package_layout
            .as_ref()
            .map(package_layout_info),
        expected_local_binary_path,
        local_binary_match,
        warnings,
    }
}

fn current_executable_path() -> Option<AbsolutePathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|path| AbsolutePathBuf::from_absolute_path(path).ok())
}

fn expected_local_binary_path() -> Option<AbsolutePathBuf> {
    let local_cli_path = std::env::var_os(LOCAL_CLI_PATH_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let local_publish_dir = std::env::var_os(LOCAL_PUBLISH_DIR_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let default_home = std::env::var_os("USERPROFILE")
        .filter(|value| !value.is_empty())
        .or_else(|| std::env::var_os("HOME").filter(|value| !value.is_empty()))
        .map(PathBuf::from);

    expected_local_binary_path_for_target(local_cli_path, local_publish_dir, default_home)
}

fn expected_local_binary_path_for_target(
    local_cli_path: Option<PathBuf>,
    local_publish_dir: Option<PathBuf>,
    default_home: Option<PathBuf>,
) -> Option<AbsolutePathBuf> {
    expected_local_binary_path_from_inputs(
        local_cli_path,
        local_publish_dir,
        default_home,
        cfg!(windows),
    )
}

fn expected_local_binary_path_from_inputs(
    local_cli_path: Option<PathBuf>,
    local_publish_dir: Option<PathBuf>,
    default_home: Option<PathBuf>,
    is_windows: bool,
) -> Option<AbsolutePathBuf> {
    if let Some(path) =
        local_cli_path.and_then(|path| AbsolutePathBuf::from_absolute_path(path).ok())
    {
        return Some(path);
    }

    let install_dir = match local_publish_dir {
        Some(path) => path,
        None if is_windows => default_home?.join("Desktop").join("LOCAL-KD").join("bin"),
        None => return None,
    };
    let executable_name = if is_windows { "codex.exe" } else { "codex" };
    AbsolutePathBuf::from_absolute_path(install_dir.join(executable_name)).ok()
}

fn package_layout_info(layout: &CodexPackageLayout) -> ServerPackageLayout {
    ServerPackageLayout {
        package_dir: layout.package_dir.clone(),
        bin_dir: layout.bin_dir.clone(),
        resources_dir: layout.resources_dir.clone(),
        path_dir: layout.path_dir.clone(),
    }
}

fn install_method_label(method: &InstallMethod) -> &'static str {
    match method {
        InstallMethod::Standalone { .. } => standalone_install_method_label(cfg!(windows)),
        InstallMethod::Npm => "npm",
        InstallMethod::Bun => "bun",
        InstallMethod::Pnpm => "pnpm",
        InstallMethod::Other => "other",
    }
}

fn standalone_install_method_label(is_windows: bool) -> &'static str {
    if is_windows {
        "standalone-windows"
    } else {
        "standalone-unix"
    }
}

fn local_binary_warnings(
    executable_path: Option<&AbsolutePathBuf>,
    expected_local_binary_path: Option<&AbsolutePathBuf>,
    local_binary_match: Option<bool>,
) -> Vec<ServerRuntimeWarning> {
    let Some(expected) = expected_local_binary_path else {
        return Vec::new();
    };

    let mut warnings = Vec::new();
    if !expected.is_file() {
        warnings.push(ServerRuntimeWarning {
            code: "expectedLocalBinaryMissing".to_string(),
            message: format!(
                "Expected local Codex binary is missing at {}.",
                expected.display()
            ),
            action: Some(ACTION_PUBLISH_LOCAL_CODEX.to_string()),
        });
    }

    match (executable_path, local_binary_match) {
        (Some(actual), Some(false)) => warnings.push(ServerRuntimeWarning {
            code: "runningBinaryMismatch".to_string(),
            message: format!(
                "Running app-server executable {} does not match expected local Codex binary {}.",
                actual.display(),
                expected.display()
            ),
            action: Some(ACTION_RESTART_CODEX_DESKTOP.to_string()),
        }),
        (None, _) => warnings.push(ServerRuntimeWarning {
            code: "runningBinaryUnknown".to_string(),
            message: format!(
                "Could not resolve the running app-server executable; expected local Codex binary is {}.",
                expected.display()
            ),
            action: Some(ACTION_RESTART_CODEX_DESKTOP.to_string()),
        }),
        _ => {}
    }

    warnings
}

fn build_warnings(
    build_info: BuildInfo,
    expected_local_binary_configured: bool,
) -> Vec<ServerRuntimeWarning> {
    if !expected_local_binary_configured || build_info.dirty != "true" {
        return Vec::new();
    }

    vec![ServerRuntimeWarning {
        code: "localBuildDirty".to_string(),
        message: "Running Codex was built from a dirty checkout; source edits are visible only after rebuilding, publishing the local Codex binary, and restarting the desktop app.".to_string(),
        action: Some(ACTION_PUBLISH_LOCAL_CODEX.to_string()),
    }]
}
