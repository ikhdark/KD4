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

use codex_install_context::InstallContext;
use codex_install_context::InstallMethod;
use sha2::Digest;
use sha2::Sha256;

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

/// Reports the local Windows desktop payload that `publish-local-codex.ps1`
/// manages. This is intentionally passive: it does not build, publish, restart,
/// or repair the desktop routing.
pub(super) fn local_publish_check(show_details: bool) -> DoctorCheck {
    let publish_dir = local_publish_dir();
    let target_path = publish_dir.join("codex.exe");
    let current_exe = env::current_exe().ok();
    let current_is_target = current_exe
        .as_deref()
        .is_some_and(|current| same_path(current, &target_path));
    let build_info = build_info::build_info();
    let mut details = vec![
        format!("publish dir: {}", publish_dir.display()),
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

    if target_path.is_file() {
        for (index, line) in command_version_lines(&target_path).into_iter().enumerate() {
            let label = if index == 0 {
                "target version".to_string()
            } else {
                format!("target version detail #{index}")
            };
            details.push(format!("{label}: {line}"));
        }
    } else {
        details.push("target version: <missing>".to_string());
    }

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

    push_desktop_process_details(&mut details, show_details);

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

/// Summarizes Desktop-visible runtime evidence without starting, stopping, or
/// repairing Desktop. Socket/version details stay in the app-server row.
pub(super) fn desktop_runtime_chain_check(show_details: bool) -> DoctorCheck {
    let publish_dir = local_publish_dir();
    let target_path = publish_dir.join("codex.exe");
    let mut details = vec![
        format!("local publish target: {}", target_path.display()),
        format!("local publish target exists: {}", target_path.is_file()),
        "app-server metadata: see app_server.status".to_string(),
    ];
    push_desktop_process_details(&mut details, show_details);

    DoctorCheck::new(
        "desktop.runtime_chain",
        "desktop",
        CheckStatus::Ok,
        "desktop runtime chain evidence collected",
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

fn local_publish_dir() -> PathBuf {
    if let Some(value) = env::var_os("CODEX_LOCAL_PUBLISH_DIR")
        && !value.is_empty()
    {
        return PathBuf::from(value);
    }

    if let Some(user_profile) = env::var_os("USERPROFILE")
        && !user_profile.is_empty()
    {
        return PathBuf::from(user_profile).join("Desktop").join("LOCAL-KD");
    }

    env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Desktop")
        .join("LOCAL-KD")
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

fn command_version_lines(path: &Path) -> Vec<String> {
    match Command::new(path).arg("--version").output() {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let mut lines = stdout
                .lines()
                .chain(stderr.lines())
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .take(5)
                .map(ToString::to_string)
                .collect::<Vec<_>>();
            if output.status.success() && !lines.is_empty() {
                return lines;
            }
            if lines.is_empty() {
                lines.push(format!("<unavailable: exit {}>", output.status));
            } else {
                lines[0] = format!("<unavailable: exit {}: {}>", output.status, lines[0]);
            }
            lines
        }
        Err(err) => vec![format!("<unavailable: {err}>")],
    }
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

fn push_desktop_process_details(details: &mut Vec<String>, show_details: bool) {
    let desktop_processes = desktop_process_details();
    if desktop_processes.is_empty() {
        details.push("desktop process: <none>".to_string());
    } else if desktop_processes.len() == 1 && desktop_processes[0].starts_with("<unavailable:") {
        details.push(format!("desktop process probe: {}", desktop_processes[0]));
    } else if show_details {
        details.extend(
            desktop_processes
                .into_iter()
                .enumerate()
                .map(|(index, detail)| format!("desktop process #{}: {detail}", index + 1)),
        );
    } else {
        details.push(format!("desktop processes: {}", desktop_processes.len()));
    }
}

#[cfg(windows)]
fn desktop_process_details() -> Vec<String> {
    let script = r#"Get-Process -Name Codex -ErrorAction SilentlyContinue | Select-Object -First 5 | ForEach-Object { if ([string]::IsNullOrWhiteSpace($_.Path)) { "pid=$($_.Id) path=<unavailable>" } else { "pid=$($_.Id) path=$($_.Path)" } }"#;
    match Command::new("powershell")
        .args(["-NoProfile", "-Command", script])
        .output()
    {
        Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToString::to_string)
            .collect(),
        Ok(output) => vec![format!("<unavailable: exit {}>", output.status)],
        Err(err) => vec![format!("<unavailable: {err}>")],
    }
}

#[cfg(not(windows))]
fn desktop_process_details() -> Vec<String> {
    Vec::new()
}
