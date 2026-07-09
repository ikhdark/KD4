use std::path::Path;
use std::path::PathBuf;

use codex_app_server_protocol::ServerPackageLayout;
use codex_app_server_protocol::ServerRuntimeInfo;
use codex_app_server_protocol::ServerRuntimeWarning;
use codex_install_context::CodexPackageLayout;
use codex_install_context::InstallContext;
use codex_install_context::InstallMethod;
use codex_install_context::StandalonePlatform;
use codex_utils_absolute_path::AbsolutePathBuf;

use crate::build_info::BuildInfo;

const LOCAL_PUBLISH_DIR_ENV: &str = "CODEX_LOCAL_PUBLISH_DIR";
const LOCAL_CLI_PATH_ENV: &str = "CODEX_CLI_PATH";
const ACTION_PUBLISH_LOCAL_CODEX: &str = "publishLocalCodex";
const ACTION_RESTART_CODEX_DESKTOP: &str = "restartCodexDesktop";

pub(crate) fn current() -> ServerRuntimeInfo {
    let build_info = BuildInfo::current();
    let executable_path = current_executable_path();
    let install_context = InstallContext::current();
    let expected_local_binary_path = expected_local_binary_path();
    let local_binary_match = executable_path
        .as_ref()
        .zip(expected_local_binary_path.as_ref())
        .map(|(actual, expected)| paths_match(actual.as_path(), expected.as_path()));

    let mut warnings = local_binary_warnings(
        executable_path.as_ref(),
        expected_local_binary_path.as_ref(),
        local_binary_match,
    );
    warnings.extend(build_warnings(build_info));

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
    if let Some(path) = std::env::var_os(LOCAL_CLI_PATH_ENV)
        .filter(|value| !value.is_empty())
        .and_then(|path| AbsolutePathBuf::from_absolute_path(PathBuf::from(path)).ok())
    {
        return Some(path);
    }

    let install_dir = if let Some(path) =
        std::env::var_os(LOCAL_PUBLISH_DIR_ENV).filter(|value| !value.is_empty())
    {
        PathBuf::from(path)
    } else if let Some(user_profile) =
        std::env::var_os("USERPROFILE").filter(|value| !value.is_empty())
    {
        PathBuf::from(user_profile).join("Desktop").join("LOCAL-KD")
    } else if let Some(home) = std::env::var_os("HOME").filter(|value| !value.is_empty()) {
        PathBuf::from(home).join("Desktop").join("LOCAL-KD")
    } else {
        return None;
    };

    AbsolutePathBuf::from_absolute_path(install_dir.join("codex.exe")).ok()
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
        InstallMethod::Standalone {
            platform: StandalonePlatform::Windows,
            ..
        } => "standalone-windows",
        InstallMethod::Standalone {
            platform: StandalonePlatform::Unix,
            ..
        } => "standalone-unix",
        InstallMethod::Npm => "npm",
        InstallMethod::Bun => "bun",
        InstallMethod::Pnpm => "pnpm",
        InstallMethod::Brew => "brew",
        InstallMethod::Other => "other",
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

fn build_warnings(build_info: BuildInfo) -> Vec<ServerRuntimeWarning> {
    if build_info.dirty != "true" {
        return Vec::new();
    }

    vec![ServerRuntimeWarning {
        code: "localBuildDirty".to_string(),
        message: "Running Codex was built from a dirty checkout; source edits are visible only after rebuilding, publishing the local Codex binary, and restarting the desktop app.".to_string(),
        action: Some(ACTION_PUBLISH_LOCAL_CODEX.to_string()),
    }]
}

fn paths_match(left: &Path, right: &Path) -> bool {
    if cfg!(windows) {
        left.to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy())
    } else {
        left == right
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn absolute_temp_path(temp_dir: &TempDir, leaf: &str) -> AbsolutePathBuf {
        AbsolutePathBuf::from_absolute_path(temp_dir.path().join(leaf)).expect("absolute temp path")
    }

    #[test]
    fn install_method_labels_are_stable() {
        assert_eq!(install_method_label(&InstallMethod::Other), "other");
        assert_eq!(install_method_label(&InstallMethod::Npm), "npm");
        assert_eq!(install_method_label(&InstallMethod::Bun), "bun");
        assert_eq!(install_method_label(&InstallMethod::Pnpm), "pnpm");
        assert_eq!(install_method_label(&InstallMethod::Brew), "brew");
    }

    #[test]
    fn local_binary_warnings_are_structured_for_missing_and_mismatch() {
        let temp_dir = TempDir::new().expect("temp dir");
        let actual = absolute_temp_path(&temp_dir, "running-codex.exe");
        let expected = absolute_temp_path(&temp_dir, "codex.exe");

        let warnings = local_binary_warnings(Some(&actual), Some(&expected), Some(false));

        assert_eq!(warnings.len(), 2);
        assert_eq!(warnings[0].code, "expectedLocalBinaryMissing");
        assert_eq!(warnings[0].action.as_deref(), Some("publishLocalCodex"));
        assert!(
            warnings[0]
                .message
                .contains(expected.as_path().to_string_lossy().as_ref())
        );
        assert_eq!(warnings[1].code, "runningBinaryMismatch");
        assert_eq!(warnings[1].action.as_deref(), Some("restartCodexDesktop"));
        assert!(
            warnings[1]
                .message
                .contains(actual.as_path().to_string_lossy().as_ref())
        );
        assert!(
            warnings[1]
                .message
                .contains(expected.as_path().to_string_lossy().as_ref())
        );
    }

    #[test]
    fn local_binary_warnings_are_empty_without_expected_local_binary() {
        assert!(local_binary_warnings(None, None, None).is_empty());
    }

    #[test]
    fn build_warnings_are_structured_for_dirty_local_build() {
        let warnings = build_warnings(BuildInfo {
            version: "0.1.0",
            commit: "abc123",
            dirty: "true",
            profile: "release",
            built: "now",
        });

        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].code, "localBuildDirty");
        assert_eq!(warnings[0].action.as_deref(), Some("publishLocalCodex"));
        assert!(
            warnings[0]
                .message
                .contains("rebuilding, publishing the local Codex binary, and restarting")
        );
    }

    #[test]
    fn build_warnings_ignore_unknown_dirty_state() {
        assert!(
            build_warnings(BuildInfo {
                version: "0.1.0",
                commit: "unknown",
                dirty: "unknown",
                profile: "release",
                built: "unknown",
            })
            .is_empty()
        );
    }

    #[test]
    fn package_layout_info_maps_install_context_layout() {
        let temp_dir = TempDir::new().expect("temp dir");
        let layout = CodexPackageLayout {
            package_dir: absolute_temp_path(&temp_dir, "package"),
            bin_dir: absolute_temp_path(&temp_dir, "package/bin"),
            resources_dir: Some(absolute_temp_path(&temp_dir, "package/codex-resources")),
            path_dir: Some(absolute_temp_path(&temp_dir, "package/codex-path")),
        };

        let info = package_layout_info(&layout);

        assert_eq!(info.package_dir, layout.package_dir);
        assert_eq!(info.bin_dir, layout.bin_dir);
        assert_eq!(info.resources_dir, layout.resources_dir);
        assert_eq!(info.path_dir, layout.path_dir);
    }
}
