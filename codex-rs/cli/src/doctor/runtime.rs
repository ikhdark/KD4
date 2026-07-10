//! Captures how this Codex process was launched.
//!
//! Runtime diagnostics answer provenance questions that are hard to infer from
//! user reports: which binary is running, which install channel it resembles,
//! which platform it targets, and whether the search command comes from bundled
//! package files or from PATH.

use std::env;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use codex_install_context::InstallContext;
use codex_install_context::InstallMethod;
#[cfg(windows)]
use serde::Deserialize;
use sha2::Digest;
use sha2::Sha256;
use tokio::time::timeout;

use super::CheckStatus;
use super::DoctorCheck;
use super::DoctorIssue;
use super::describe_install_context;
use super::doctor_install_context;
use super::push_path_detail;
use crate::build_info;

/// Builds the process provenance row for the current Codex executable.
///
/// This check is informational and should not fail on its own; inconsistent
/// install state is reported by the installation and update checks instead.
pub(super) fn runtime_check() -> DoctorCheck {
    let current_exe = env::current_exe().ok();
    let install_context = doctor_install_context(current_exe.as_deref());
    let os = env::consts::OS;
    let arch = env::consts::ARCH;
    let platform = format!("{os}-{arch}");
    let install_method = install_method_name(&install_context);
    let build_info = build_info::build_info();
    let mut details = vec![
        format!("version: {}", build_info.version),
        format!("platform: {platform}"),
        format!(
            "install method: {}",
            describe_install_context(&install_context)
        ),
        format!("commit: {}", build_commit()),
        format!("dirty: {}", build_info.dirty),
        format!("profile: {}", build_info.profile),
        format!("built: {}", build_info.built),
    ];
    push_path_detail(&mut details, "current executable", current_exe.as_deref());

    DoctorCheck::new(
        "runtime.provenance",
        "runtime",
        CheckStatus::Ok,
        format!("running {install_method} on {platform}"),
    )
    .details(details)
}

/// Resolves the explicitly configured local binary, or the fork's implicit
/// Windows LOCAL-KD target. Other platforms opt in through an explicit env var.
pub(super) fn local_publish_target_path() -> Option<PathBuf> {
    let local_cli_path = env::var_os("CODEX_CLI_PATH")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let local_publish_dir = env::var_os("CODEX_LOCAL_PUBLISH_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let default_home = env::var_os("USERPROFILE")
        .filter(|value| !value.is_empty())
        .or_else(|| env::var_os("HOME").filter(|value| !value.is_empty()))
        .map(PathBuf::from);

    local_publish_target_path_from_inputs(
        local_cli_path,
        local_publish_dir,
        default_home,
        cfg!(windows),
    )
}

fn local_publish_target_path_from_inputs(
    local_cli_path: Option<PathBuf>,
    local_publish_dir: Option<PathBuf>,
    default_home: Option<PathBuf>,
    is_windows: bool,
) -> Option<PathBuf> {
    if let Some(path) = local_cli_path {
        return Some(path);
    }

    let publish_dir = match local_publish_dir {
        Some(path) => path,
        None if is_windows => default_home?.join("Desktop").join("LOCAL-KD"),
        None => return None,
    };
    Some(publish_dir.join(if is_windows { "codex.exe" } else { "codex" }))
}

/// Reports the local payload managed by the configured local publish path.
/// This is intentionally passive: it does not build, publish, restart, or
/// repair the desktop routing.
pub(super) async fn local_publish_check(target_path: PathBuf) -> DoctorCheck {
    let current_exe = env::current_exe().ok();
    let current_is_target = current_exe
        .as_deref()
        .is_some_and(|current| same_path(current, &target_path));
    let build_info = build_info::build_info();
    let mut details = vec![
        format!(
            "publish dir: {}",
            target_path.parent().unwrap_or(Path::new(".")).display()
        ),
        format!("target path: {}", target_path.display()),
        format!("target exists: {}", target_path.is_file()),
        format!("current executable is target: {current_is_target}"),
        format!("current version: {}", build_info.version),
        format!("current commit: {}", build_info.commit),
        format!("current dirty: {}", build_info.dirty),
        format!("current profile: {}", build_info.profile),
        format!("current built: {}", build_info.built),
    ];
    push_path_detail(&mut details, "current executable", current_exe.as_deref());

    match file_sha256(&target_path) {
        Ok(hash) => details.push(format!("target sha256: {hash}")),
        Err(err) => details.push(format!("target sha256: <unavailable: {err}>")),
    }

    let version_probe_error = if target_path.is_file() {
        match command_version_lines(&target_path).await {
            Ok(lines) => {
                for (index, line) in lines.into_iter().enumerate() {
                    let label = if index == 0 {
                        "target version".to_string()
                    } else {
                        format!("target version detail #{index}")
                    };
                    details.push(format!("{label}: {line}"));
                }
                None
            }
            Err(err) => {
                details.push(format!("target version: <unavailable: {err}>"));
                Some(err)
            }
        }
    } else {
        details.push("target version: <missing>".to_string());
        None
    };

    let publish_readiness = if !target_path.is_file() {
        "missing target"
    } else if current_is_target {
        "current executable matches target path"
    } else {
        "current executable differs from target path"
    };
    details.push(format!("publish readiness: {publish_readiness}"));

    if let Some(repo_root) = source_repo_root() {
        details.push(format!("source repo root: {}", repo_root.display()));
        details.push(format!(
            "source HEAD: {}",
            git_output(&repo_root, &["rev-parse", "--short", "HEAD"])
        ));
        details.push(format!(
            "source dirty files: {}",
            git_status_count(&repo_root)
                .map(|count| count.to_string())
                .unwrap_or_else(|| "<unavailable>".to_string())
        ));
    } else {
        details.push("source repo root: <not detected>".to_string());
        details.push("source HEAD: <not detected>".to_string());
    }

    if !target_path.is_file() {
        return DoctorCheck::new(
            "local_publish.readiness",
            "local-publish",
            CheckStatus::Warning,
            "local publish target is missing",
        )
        .details(details)
        .issue(
            DoctorIssue::new(CheckStatus::Warning, "LOCAL-KD codex.exe is missing")
                .measured(target_path.display().to_string())
                .expected("existing local Codex desktop payload")
                .remedy("Run just publish-local-codex-final, then restart Codex Desktop.")
                .field("target path"),
        )
        .remediation("Run just publish-local-codex-final, then restart Codex Desktop.");
    }

    if let Some(err) = version_probe_error {
        return DoctorCheck::new(
            "local_publish.readiness",
            "local-publish",
            CheckStatus::Warning,
            "local publish target version could not be verified",
        )
        .details(details)
        .issue(
            DoctorIssue::new(CheckStatus::Warning, "local target version probe failed")
                .measured(err)
                .expected("a bounded, successful codex --version response")
                .field("target version"),
        )
        .remediation("Rebuild the local target before publishing it.");
    }

    if !current_is_target {
        return DoctorCheck::new(
            "local_publish.readiness",
            "local-publish",
            CheckStatus::Warning,
            "doctor is not running from the local publish target",
        )
        .details(details)
        .issue(
            DoctorIssue::new(
                CheckStatus::Warning,
                "running Codex binary differs from LOCAL-KD target",
            )
            .measured(
                current_exe
                    .as_deref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "none".to_string()),
            )
            .expected(target_path.display().to_string())
            .remedy(
                "Run the published LOCAL-KD codex.exe or restart Codex Desktop after publishing.",
            )
            .field("current executable")
            .field("target path"),
        );
    }

    DoctorCheck::new(
        "local_publish.readiness",
        "local-publish",
        CheckStatus::Ok,
        "local publish target is present",
    )
    .details(details)
}

/// Verifies that Codex Desktop has a non-current app-server process running
/// from the selected local target, without starting or stopping Desktop.
#[cfg(windows)]
pub(super) async fn desktop_runtime_chain_check(
    target_path: PathBuf,
    show_details: bool,
) -> DoctorCheck {
    let mut details = vec![
        format!("local publish target: {}", target_path.display()),
        format!("local publish target exists: {}", target_path.is_file()),
    ];

    if !target_path.is_file() {
        return DoctorCheck::new(
            "desktop.runtime_chain",
            "desktop",
            CheckStatus::Warning,
            "local publish target is missing",
        )
        .details(details)
        .remediation("Run just publish-local-codex-final, then restart Codex Desktop.");
    }

    let processes = match desktop_process_probe().await {
        Ok(processes) => processes,
        Err(err) => {
            details.push(format!("desktop app-server probe: <unavailable: {err}>"));
            return DoctorCheck::new(
                "desktop.runtime_chain",
                "desktop",
                CheckStatus::Warning,
                "desktop app-server process could not be verified",
            )
            .details(details)
            .remediation("Restart Codex Desktop and rerun codex doctor.");
        }
    };
    let matching = matching_desktop_app_servers(&processes, &target_path, std::process::id());
    push_desktop_process_details(&mut details, &processes, matching.len(), show_details);

    if matching.is_empty() {
        return DoctorCheck::new(
            "desktop.runtime_chain",
            "desktop",
            CheckStatus::Warning,
            "Desktop is not using the selected local app-server binary",
        )
        .details(details)
        .remediation("Restart Codex Desktop after publishing the local Codex binary.");
    }

    DoctorCheck::new(
        "desktop.runtime_chain",
        "desktop",
        CheckStatus::Ok,
        "Desktop app-server is running from the selected local binary",
    )
    .details(details)
}

/// Verifies that the search command selected by the install context is usable.
///
/// Package-layout installs should point at a bundled ripgrep binary, while local
/// installs without that layout usually resolve rg from PATH. A warning here
/// means features that depend on file search may degrade even when the CLI
/// launches.
pub(super) fn search_check() -> DoctorCheck {
    let current_exe = env::current_exe().ok();
    let install_context = doctor_install_context(current_exe.as_deref());
    let rg_command = install_context.rg_command();
    let provider = search_provider(&install_context);
    let mut details = vec![
        format!("search command: {}", rg_command.display()),
        format!("search provider: {provider}"),
    ];

    let status = if rg_command.components().count() > 1 {
        match std::fs::metadata(&rg_command) {
            Ok(metadata) if metadata.is_file() => {
                details.push("search command readiness: file exists".to_string());
                CheckStatus::Ok
            }
            Ok(_) => {
                details.push("search command readiness: path is not a file".to_string());
                CheckStatus::Warning
            }
            Err(err) => {
                details.push(format!("search command readiness: {err}"));
                CheckStatus::Warning
            }
        }
    } else {
        match Command::new(&rg_command).arg("--version").output() {
            Ok(output) if output.status.success() => {
                let version = String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .next()
                    .unwrap_or("rg version unknown")
                    .to_string();
                details.push(format!("search command readiness: {version}"));
                CheckStatus::Ok
            }
            Ok(output) => {
                details.push(format!(
                    "search command readiness: exited with status {}",
                    output.status
                ));
                CheckStatus::Warning
            }
            Err(err) => {
                details.push(format!("search command readiness: {err}"));
                CheckStatus::Warning
            }
        }
    };

    let summary = match status {
        CheckStatus::Ok => format!("search is OK ({provider})"),
        CheckStatus::Warning => "search command could not be verified".to_string(),
        CheckStatus::Fail => unreachable!(),
    };
    let mut check = DoctorCheck::new("runtime.search", "search", status, summary).details(details);
    if status != CheckStatus::Ok {
        check = check.remediation("Install ripgrep or repair the bundled Codex package.");
    }
    check
}

fn install_method_name(context: &InstallContext) -> &'static str {
    match &context.method {
        InstallMethod::Standalone { .. } => "standalone",
        InstallMethod::Npm => "npm",
        InstallMethod::Bun => "bun",
        InstallMethod::Pnpm => "pnpm",
        InstallMethod::Brew => "brew",
        InstallMethod::Other => "local build",
    }
}

fn search_provider(context: &InstallContext) -> &'static str {
    let rg_command = context.rg_command();
    let from_package_layout = context
        .package_layout
        .as_ref()
        .and_then(|package_layout| package_layout.path_dir.as_ref())
        .is_some_and(|path_dir| rg_command.starts_with(path_dir));
    let from_legacy_standalone = matches!(
        &context.method,
        InstallMethod::Standalone {
            resources_dir: Some(resources_dir),
            ..
        } if rg_command.starts_with(resources_dir)
    );

    if from_package_layout || from_legacy_standalone {
        "bundled"
    } else {
        "system"
    }
}

fn build_commit() -> &'static str {
    build_info::build_info().commit
}

fn same_path(left: &Path, right: &Path) -> bool {
    let left = left.canonicalize().unwrap_or_else(|_| left.to_path_buf());
    let right = right.canonicalize().unwrap_or_else(|_| right.to_path_buf());
    if cfg!(windows) {
        left.to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy())
    } else {
        left == right
    }
}

fn file_sha256(path: &Path) -> Result<String, String> {
    let mut file = File::open(path).map_err(|err| err.to_string())?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|err| err.to_string())?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

async fn command_output_with_timeout(
    mut command: tokio::process::Command,
    duration: Duration,
) -> Result<std::process::Output, String> {
    command.kill_on_drop(true);
    match timeout(duration, command.output()).await {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(err)) => Err(err.to_string()),
        Err(_) => Err(format!("timed out after {} ms", duration.as_millis())),
    }
}

async fn command_version_lines(path: &Path) -> Result<Vec<String>, String> {
    let mut command = tokio::process::Command::new(path);
    command.arg("--version");
    let output = command_output_with_timeout(command, Duration::from_secs(5)).await?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let lines = stdout
        .lines()
        .chain(stderr.lines())
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(5)
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    if !output.status.success() {
        let detail = lines.first().map(String::as_str).unwrap_or("no output");
        return Err(format!("exit {}: {detail}", output.status));
    }
    if lines.is_empty() {
        return Err("command produced no version output".to_string());
    }
    Ok(lines)
}

fn source_repo_root() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(cwd) = env::current_dir() {
        candidates.push(cwd);
    }
    if let Ok(exe) = env::current_exe()
        && let Some(parent) = exe.parent()
    {
        candidates.push(parent.to_path_buf());
    }

    for candidate in candidates {
        for ancestor in candidate.ancestors() {
            if ancestor.join("codex-rs").join("Cargo.toml").is_file()
                && ancestor
                    .join("scripts")
                    .join("publish-local-codex.ps1")
                    .is_file()
            {
                return Some(ancestor.to_path_buf());
            }
        }
    }
    None
}

fn git_output(repo_root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(args)
        .output();
    match output {
        Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout)
            .lines()
            .next()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .unwrap_or("unknown")
            .to_string(),
        Ok(output) => format!("<unavailable: exit {}>", output.status),
        Err(err) => format!("<unavailable: {err}>"),
    }
}

fn git_status_count(repo_root: &Path) -> Option<usize> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["status", "--porcelain=v1", "-uall"])
        .output()
        .ok()?;
    output.status.success().then(|| {
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count()
    })
}

#[cfg(windows)]
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
struct DesktopProcessEvidence {
    pid: u32,
    path: Option<PathBuf>,
    is_app_server: bool,
}

#[cfg(windows)]
async fn desktop_process_probe() -> Result<Vec<self::DesktopProcessEvidence>, String> {
    let mut command = tokio::process::Command::new("powershell");
    command
        .args([
            "-NoProfile",
            "-Command",
            r#"
Get-CimInstance Win32_Process -Filter "Name='codex.exe'" -OperationTimeoutSec 2 -ErrorAction Stop |
    Where-Object { $_.ProcessId -ne [uint32]$env:CODEX_DOCTOR_CURRENT_PID } |
    Select-Object -First 20 |
    ForEach-Object {
        [pscustomobject]@{
            pid = [uint32]$_.ProcessId
            path = $_.ExecutablePath
            isAppServer = [bool]($_.CommandLine -match '(?i)(^|\s)app-server(?:\s|$)')
        } | ConvertTo-Json -Compress
    }
"#,
        ])
        .env("CODEX_DOCTOR_CURRENT_PID", std::process::id().to_string());
    let output = command_output_with_timeout(command, Duration::from_secs(5)).await?;
    if !output.status.success() {
        return Err(format!("PowerShell exited with {}", output.status));
    }

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str(line).map_err(|err| err.to_string()))
        .collect()
}

#[cfg(windows)]
fn matching_desktop_app_servers<'a>(
    processes: &'a [self::DesktopProcessEvidence],
    target_path: &Path,
    current_pid: u32,
) -> Vec<&'a self::DesktopProcessEvidence> {
    processes
        .iter()
        .filter(|process| {
            process.pid != current_pid
                && process.is_app_server
                && process
                    .path
                    .as_deref()
                    .is_some_and(|path| same_path(path, target_path))
        })
        .collect()
}

#[cfg(windows)]
fn push_desktop_process_details(
    details: &mut Vec<String>,
    processes: &[self::DesktopProcessEvidence],
    matching_count: usize,
    show_details: bool,
) {
    details.push(format!("candidate codex processes: {}", processes.len()));
    details.push(format!(
        "matching local app-server processes: {matching_count}"
    ));
    if show_details {
        details.extend(processes.iter().enumerate().map(|(index, process)| {
            format!(
                "codex process #{}: pid={} path={} app-server={}",
                index + 1,
                process.pid,
                process
                    .path
                    .as_deref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "<unavailable>".to_string()),
                process.is_app_server,
            )
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn local_publish_target_resolution_is_explicit_off_windows() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cli = temp.path().join("explicit-codex");
        let publish_dir = temp.path().join("publish");
        let home = temp.path().join("home");

        assert_eq!(
            local_publish_target_path_from_inputs(
                Some(cli.clone()),
                Some(publish_dir.clone()),
                Some(home.clone()),
                false,
            ),
            Some(cli)
        );
        assert_eq!(
            local_publish_target_path_from_inputs(
                None,
                Some(publish_dir.clone()),
                Some(home.clone()),
                false,
            ),
            Some(publish_dir.join("codex"))
        );
        assert_eq!(
            local_publish_target_path_from_inputs(None, None, Some(home.clone()), false),
            None
        );
        assert_eq!(
            local_publish_target_path_from_inputs(None, None, Some(home.clone()), true),
            Some(home.join("Desktop").join("LOCAL-KD").join("codex.exe"))
        );
    }

    #[tokio::test]
    async fn missing_local_publish_target_warns() {
        let temp = tempfile::tempdir().expect("tempdir");
        let check = local_publish_check(temp.path().join("missing-codex")).await;

        assert_eq!(check.status, CheckStatus::Warning);
        assert_eq!(check.summary, "local publish target is missing");
    }

    #[tokio::test]
    async fn command_probe_timeout_is_bounded() {
        #[cfg(windows)]
        let mut command = {
            let mut command = tokio::process::Command::new("powershell");
            command.args(["-NoProfile", "-Command", "Start-Sleep -Seconds 2"]);
            command
        };
        #[cfg(not(windows))]
        let mut command = {
            let mut command = tokio::process::Command::new("sh");
            command.args(["-c", "sleep 2"]);
            command
        };

        command.kill_on_drop(true);
        let err = command_output_with_timeout(command, Duration::from_millis(25))
            .await
            .expect_err("slow command should time out");

        assert!(err.contains("timed out"), "unexpected error: {err}");
    }

    #[cfg(windows)]
    #[test]
    fn desktop_matching_requires_noncurrent_app_server_at_target_path() {
        let temp = tempfile::tempdir().expect("tempdir");
        let target = temp.path().join("codex.exe");
        let wrong = temp.path().join("other-codex.exe");
        let current_pid = 10;
        let processes = vec![
            DesktopProcessEvidence {
                pid: current_pid,
                path: Some(target.clone()),
                is_app_server: true,
            },
            DesktopProcessEvidence {
                pid: 11,
                path: Some(wrong),
                is_app_server: true,
            },
            DesktopProcessEvidence {
                pid: 12,
                path: Some(target.clone()),
                is_app_server: false,
            },
            DesktopProcessEvidence {
                pid: 13,
                path: Some(target.clone()),
                is_app_server: true,
            },
        ];

        let matching = matching_desktop_app_servers(&processes, &target, current_pid);

        assert_eq!(matching.len(), 1);
        assert_eq!(matching[0].pid, 13);
    }
}
