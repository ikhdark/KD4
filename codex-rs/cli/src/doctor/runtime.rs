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

use codex_app_server_protocol::DESKTOP_CLIENT_NAME;
use codex_app_server_protocol::DESKTOP_RUNTIME_RECEIPT_RELATIVE_PATH;
use codex_app_server_protocol::DesktopRuntimeReceipt;
use codex_install_context::InstallContext;
use codex_install_context::InstallMethod;
use codex_utils_build_info::BuildInfo;

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
    let build_info = BuildInfo::current();
    let mut details = vec![
        format!("version: {}", build_info.version),
        format!("platform: {platform}"),
        format!(
            "install method: {}",
            describe_install_context(&install_context)
        ),
        format!("commit: {}", build_info.commit),
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

/// Resolves the explicitly configured local binary, or the fork's implicit Windows LOCAL-KD
/// target.
pub(super) fn local_publish_target_path() -> Option<PathBuf> {
    let local_cli_path = env::var_os("CODEX_CLI_PATH")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let local_publish_dir = env::var_os("CODEX_LOCAL_PUBLISH_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let default_home = env::var_os("USERPROFILE")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);

    local_publish_target_path_from_inputs(local_cli_path, local_publish_dir, default_home)
}

fn local_publish_target_path_from_inputs(
    local_cli_path: Option<PathBuf>,
    local_publish_dir: Option<PathBuf>,
    default_home: Option<PathBuf>,
) -> Option<PathBuf> {
    if let Some(path) = local_cli_path {
        return Some(path);
    }

    let publish_dir = match local_publish_dir {
        Some(path) => path,
        None => default_home?.join("Desktop").join("LOCAL-KD").join("bin"),
    };
    Some(publish_dir.join("codex.exe"))
}

/// Reports the local payload managed by the configured local publish path.
/// This is intentionally passive: it does not build, publish, restart, or
/// repair the desktop routing.
pub(super) async fn local_publish_check(target_path: PathBuf) -> DoctorCheck {
    let current_exe = env::current_exe().ok();
    let current_is_target = current_exe
        .as_deref()
        .is_some_and(|current| same_path(current, &target_path));
    let build_info = BuildInfo::current();
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
pub(super) async fn desktop_runtime_chain_check(
    target_path: PathBuf,
    expected_codex_home: Option<PathBuf>,
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

    let processes = match desktop_process_probe(&target_path).await {
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

    let Some(expected_codex_home) = expected_codex_home else {
        details.push("desktop runtime receipt: <CODEX_HOME unavailable>".to_string());
        return DoctorCheck::new(
            "desktop.runtime_chain",
            "desktop",
            CheckStatus::Warning,
            "Desktop CODEX_HOME could not be verified",
        )
        .details(details)
        .remediation("Set CODEX_HOME to the fork data home and restart Codex Desktop.");
    };
    let receipt_path = expected_codex_home.join(DESKTOP_RUNTIME_RECEIPT_RELATIVE_PATH);
    details.push(format!(
        "desktop runtime receipt: {}",
        receipt_path.display()
    ));
    let receipt = match read_desktop_runtime_receipt(&receipt_path) {
        Ok(receipt) => receipt,
        Err(err) => {
            details.push(format!("desktop runtime receipt status: {err}"));
            return DoctorCheck::new(
                "desktop.runtime_chain",
                "desktop",
                CheckStatus::Warning,
                "Desktop runtime receipt is unavailable",
            )
            .details(details)
            .remediation("Restart Codex Desktop and wait for app-server initialization.");
        }
    };
    push_desktop_runtime_receipt_details(&mut details, &receipt);
    if let Err(err) = validate_desktop_runtime_receipt(
        &receipt,
        &processes,
        &target_path,
        std::process::id(),
        &expected_codex_home,
    ) {
        details.push(format!("desktop runtime receipt status: {err}"));
        return DoctorCheck::new(
            "desktop.runtime_chain",
            "desktop",
            CheckStatus::Warning,
            "Desktop runtime provenance does not match the selected local fork",
        )
        .details(details)
        .remediation("Restart Codex Desktop with the selected local binary and CODEX_HOME.");
    }
    details.push("desktop runtime receipt status: matched".to_string());

    DoctorCheck::new(
        "desktop.runtime_chain",
        "desktop",
        CheckStatus::Ok,
        "Desktop app-server matches the selected local binary, build, and CODEX_HOME",
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

fn same_path(left: &Path, right: &Path) -> bool {
    let left = left.canonicalize().unwrap_or_else(|_| left.to_path_buf());
    let right = right.canonicalize().unwrap_or_else(|_| right.to_path_buf());
    left.to_string_lossy()
        .eq_ignore_ascii_case(&right.to_string_lossy())
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
struct DesktopProcessEvidence {
    pid: u32,
    path: Option<PathBuf>,
    is_app_server: bool,
}

fn read_desktop_runtime_receipt(path: &Path) -> Result<DesktopRuntimeReceipt, String> {
    let file = File::open(path).map_err(|err| err.to_string())?;
    serde_json::from_reader(file).map_err(|err| err.to_string())
}

fn validate_desktop_runtime_receipt(
    receipt: &DesktopRuntimeReceipt,
    processes: &[DesktopProcessEvidence],
    target_path: &Path,
    current_pid: u32,
    expected_codex_home: &Path,
) -> Result<(), String> {
    if receipt.schema_version != 1 {
        return Err(format!(
            "unsupported schema version {}",
            receipt.schema_version
        ));
    }
    if receipt.client_name != DESKTOP_CLIENT_NAME {
        return Err(format!(
            "receipt client {} is not Codex Desktop",
            receipt.client_name
        ));
    }
    if receipt.pid == current_pid {
        return Err("receipt identifies the doctor process".to_string());
    }
    let Some(process) = processes.iter().find(|process| process.pid == receipt.pid) else {
        return Err(format!("receipt PID {} is not live", receipt.pid));
    };
    if !process.is_app_server {
        return Err(format!("receipt PID {} is not an app-server", receipt.pid));
    }
    if !process
        .path
        .as_deref()
        .is_some_and(|path| same_path(path, target_path))
    {
        return Err("live receipt process is not running the selected binary".to_string());
    }
    if !same_path(&receipt.executable_path, target_path) {
        return Err("receipt executable does not match the selected binary".to_string());
    }
    if !same_path(&receipt.codex_home, expected_codex_home) {
        return Err("receipt CODEX_HOME does not match the intended fork home".to_string());
    }
    let target_sha256 =
        file_sha256(target_path).map_err(|err| format!("could not hash selected binary: {err}"))?;
    if receipt.executable_sha256 != target_sha256 {
        return Err("receipt executable hash does not match the selected binary".to_string());
    }
    Ok(())
}

fn push_desktop_runtime_receipt_details(
    details: &mut Vec<String>,
    receipt: &DesktopRuntimeReceipt,
) {
    details.push(format!("receipt PID: {}", receipt.pid));
    details.push(format!(
        "receipt executable: {}",
        receipt.executable_path.display()
    ));
    details.push(format!(
        "receipt CODEX_HOME: {}",
        receipt.codex_home.display()
    ));
    details.push(format!("receipt client: {}", receipt.client_name));
    details.push(format!("receipt build commit: {}", receipt.build_commit));
    details.push(format!("receipt build built: {}", receipt.build_built));
}

const MAX_DESKTOP_PROCESS_EVIDENCE: usize = 20;

async fn desktop_process_probe(
    target_path: &Path,
) -> Result<Vec<self::DesktopProcessEvidence>, String> {
    let mut command = tokio::process::Command::new("powershell");
    command
        .args([
            "-NoProfile",
            "-Command",
            r#"
Get-CimInstance Win32_Process -Filter "Name='codex.exe'" -OperationTimeoutSec 2 -ErrorAction Stop |
    Where-Object { $_.ProcessId -ne [uint32]$env:CODEX_DOCTOR_CURRENT_PID } |
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

    let processes = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str(line).map_err(|err| err.to_string()))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(prioritize_desktop_processes(processes, target_path))
}

fn prioritize_desktop_processes(
    mut processes: Vec<DesktopProcessEvidence>,
    target_path: &Path,
) -> Vec<DesktopProcessEvidence> {
    processes.sort_by_key(|process| {
        std::cmp::Reverse((
            process.is_app_server
                && process
                    .path
                    .as_deref()
                    .is_some_and(|path| same_path(path, target_path)),
            process.is_app_server,
            process
                .path
                .as_deref()
                .is_some_and(|path| same_path(path, target_path)),
        ))
    });
    processes.truncate(MAX_DESKTOP_PROCESS_EVIDENCE);
    processes
}

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

    fn matching_receipt(
        executable_path: PathBuf,
        codex_home: PathBuf,
        pid: u32,
    ) -> DesktopRuntimeReceipt {
        std::fs::write(&executable_path, b"test codex binary").expect("write test binary");
        let build = BuildInfo::current();
        DesktopRuntimeReceipt {
            schema_version: 1,
            pid,
            executable_sha256: file_sha256(&executable_path).expect("hash test binary"),
            executable_path,
            codex_home,
            client_name: "codex_desktop".to_string(),
            build_version: build.version.to_string(),
            build_commit: build.commit.to_string(),
            build_dirty: build.dirty.to_string(),
            build_profile: build.profile.to_string(),
            build_built: build.built.to_string(),
        }
    }

    #[test]
    fn runtime_check_reports_shared_build_info() {
        let build_info = BuildInfo::current();
        let check = runtime_check();

        assert!(
            check
                .details
                .contains(&format!("commit: {}", build_info.commit))
        );
        assert!(
            check
                .details
                .contains(&format!("profile: {}", build_info.profile))
        );
    }

    #[test]
    fn local_publish_target_resolution_uses_windows_executable_paths() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cli = temp.path().join("explicit-codex");
        let publish_dir = temp.path().join("publish");
        let home = temp.path().join("home");

        assert_eq!(
            local_publish_target_path_from_inputs(
                Some(cli.clone()),
                Some(publish_dir.clone()),
                Some(home.clone()),
            ),
            Some(cli)
        );
        assert_eq!(
            local_publish_target_path_from_inputs(
                None,
                Some(publish_dir.clone()),
                Some(home.clone()),
            ),
            Some(publish_dir.join("codex.exe"))
        );
        assert_eq!(
            local_publish_target_path_from_inputs(None, None, Some(home.clone())),
            Some(
                home.join("Desktop")
                    .join("LOCAL-KD")
                    .join("bin")
                    .join("codex.exe")
            )
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
        let mut command = {
            let mut command = tokio::process::Command::new("powershell");
            command.args(["-NoProfile", "-Command", "Start-Sleep -Seconds 2"]);
            command
        };

        command.kill_on_drop(true);
        let err = command_output_with_timeout(command, Duration::from_millis(25))
            .await
            .expect_err("slow command should time out");

        assert!(err.contains("timed out"), "unexpected error: {err}");
    }

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

    #[test]
    fn desktop_process_evidence_retains_target_app_server_past_display_cap() {
        let temp = tempfile::tempdir().expect("tempdir");
        let target = temp.path().join("codex.exe");
        let mut processes = (1..=MAX_DESKTOP_PROCESS_EVIDENCE)
            .map(|pid| DesktopProcessEvidence {
                pid: pid as u32,
                path: Some(temp.path().join(format!("other-{pid}.exe"))),
                is_app_server: false,
            })
            .collect::<Vec<_>>();
        processes.push(DesktopProcessEvidence {
            pid: 42,
            path: Some(target.clone()),
            is_app_server: true,
        });

        let prioritized = prioritize_desktop_processes(processes, &target);

        assert_eq!(prioritized.len(), MAX_DESKTOP_PROCESS_EVIDENCE);
        assert_eq!(prioritized[0].pid, 42);
        assert_eq!(
            matching_desktop_app_servers(&prioritized, &target, 99)[0].pid,
            42
        );
    }

    #[test]
    fn desktop_runtime_receipt_requires_a_live_matching_pid() {
        let temp = tempfile::tempdir().expect("tempdir");
        let target = temp.path().join("codex.exe");
        let home = temp.path().join("LOCAL-KD");
        let receipt = matching_receipt(target.clone(), home.clone(), 42);

        let err = validate_desktop_runtime_receipt(&receipt, &[], &target, 10, &home)
            .expect_err("an absent receipt PID must not prove a Desktop restart");

        assert!(err.contains("not live"), "unexpected error: {err}");
    }

    #[test]
    fn desktop_runtime_receipt_rejects_non_desktop_client() {
        let temp = tempfile::tempdir().expect("tempdir");
        let target = temp.path().join("codex.exe");
        let home = temp.path().join("LOCAL-KD");
        let processes = vec![DesktopProcessEvidence {
            pid: 42,
            path: Some(target.clone()),
            is_app_server: true,
        }];
        let mut receipt = matching_receipt(target.clone(), home.clone(), 42);
        receipt.client_name = "codex_vscode".to_string();

        let err = validate_desktop_runtime_receipt(&receipt, &processes, &target, 10, &home)
            .expect_err("a non-Desktop receipt must not prove a Desktop restart");

        assert!(err.contains("not Codex Desktop"), "unexpected error: {err}");
    }

    #[test]
    fn desktop_runtime_receipt_rejects_wrong_binary_or_home() {
        let temp = tempfile::tempdir().expect("tempdir");
        let target = temp.path().join("codex.exe");
        let wrong_binary = temp.path().join("official-codex.exe");
        let home = temp.path().join("LOCAL-KD");
        let wrong_home = temp.path().join("official-home");
        let processes = vec![DesktopProcessEvidence {
            pid: 42,
            path: Some(target.clone()),
            is_app_server: true,
        }];

        let wrong_binary_receipt = matching_receipt(wrong_binary, home.clone(), 42);
        let binary_err =
            validate_desktop_runtime_receipt(&wrong_binary_receipt, &processes, &target, 10, &home)
                .expect_err("a wrong receipt executable must fail");
        assert!(binary_err.contains("executable"));

        let wrong_home_receipt = matching_receipt(target.clone(), wrong_home, 42);
        let home_err =
            validate_desktop_runtime_receipt(&wrong_home_receipt, &processes, &target, 10, &home)
                .expect_err("a wrong receipt CODEX_HOME must fail");
        assert!(home_err.contains("CODEX_HOME"));
    }

    #[test]
    fn desktop_runtime_receipt_accepts_matching_live_fork_identity() {
        let temp = tempfile::tempdir().expect("tempdir");
        let target = temp.path().join("codex.exe");
        let home = temp.path().join("LOCAL-KD");
        let processes = vec![DesktopProcessEvidence {
            pid: 42,
            path: Some(target.clone()),
            is_app_server: true,
        }];
        let receipt = matching_receipt(target.clone(), home.clone(), 42);

        validate_desktop_runtime_receipt(&receipt, &processes, &target, 10, &home)
            .expect("matching live receipt should prove the selected fork runtime");
    }

    #[test]
    fn desktop_runtime_receipt_binds_identity_to_the_selected_file_hash() {
        let temp = tempfile::tempdir().expect("tempdir");
        let target = temp.path().join("codex.exe");
        let home = temp.path().join("LOCAL-KD");
        let processes = vec![DesktopProcessEvidence {
            pid: 42,
            path: Some(target.clone()),
            is_app_server: true,
        }];
        let mut receipt = matching_receipt(target.clone(), home.clone(), 42);
        receipt.build_commit = "receipt-producer-build".to_string();
        receipt.build_built = "receipt-producer-time".to_string();

        validate_desktop_runtime_receipt(&receipt, &processes, &target, 10, &home)
            .expect("producer metadata may differ from the doctor when the file hash matches");

        receipt.executable_sha256 = "0".repeat(64);
        let err = validate_desktop_runtime_receipt(&receipt, &processes, &target, 10, &home)
            .expect_err("a receipt for different bytes must not validate");
        assert!(err.contains("hash"), "unexpected error: {err}");
    }
}
