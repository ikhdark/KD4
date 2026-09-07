/*
Module: runtimes

Concrete ToolRuntime implementations for specific tools. Each runtime stays
small and focused and reuses the orchestrator for approvals + sandbox + retry.
*/
use crate::exec_env::CODEX_PERMISSION_PROFILE_ENV_VAR;
use crate::exec_env::CODEX_THREAD_ID_ENV_VAR;
use crate::sandboxing::SandboxPermissions;
use crate::shell::Shell;
use crate::shell::ShellType;
#[cfg(test)]
use crate::shell_snapshot::CMD_SNAPSHOT_FORMAT_HEADER;
use crate::shell_snapshot::POWERSHELL_SNAPSHOT_FORMAT_HEADER;
use crate::shell_snapshot::ShellSnapshotFile;
use crate::shell_snapshot::parse_cmd_snapshot_environment;
use crate::tools::sandboxing::ToolError;

use codex_network_proxy::CUSTOM_CA_ENV_KEYS;
use codex_network_proxy::PROXY_ACTIVE_ENV_KEY;
use codex_network_proxy::PROXY_ENV_KEYS;

use codex_network_proxy::is_managed_mitm_ca_trust_bundle_path;
use codex_otel::MetricsClient;
use codex_protocol::config_types::WindowsSandboxLevel;
use codex_protocol::models::AdditionalPermissionProfile;
use codex_sandboxing::SandboxCommand;
use codex_sandboxing::SandboxType;
use codex_sandboxing::windows_sandbox_uses_elevated_backend;
use codex_shell_command::escape_powershell_single_quoted as powershell_single_quote;
use codex_shell_command::powershell::ProvenPowershellDirectArgv;
use codex_shell_command::powershell::extract_powershell_command;
use codex_shell_command::powershell::prefix_powershell_script_with_utf8;
use codex_shell_command::powershell::prove_noprofile_powershell_command_as_direct_argv;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use std::collections::HashMap;
use std::path::Path;

const SHELL_SNAPSHOT_REPLAY_METRIC: &str = "codex.shell_snapshot_replay";

pub(crate) mod apply_patch;
pub(crate) mod shell;
pub(crate) mod unified_exec;

pub(crate) async fn prove_noprofile_powershell_direct_argv_async(
    command: &[String],
    cwd: &Path,
    env: &HashMap<String, String>,
) -> Option<ProvenPowershellDirectArgv> {
    let command = command.to_vec();
    let cwd = cwd.to_path_buf();
    let env = env.clone();
    crate::tools::run_blocking_command_analysis(move || {
        prove_noprofile_powershell_command_as_direct_argv(&command, &cwd, &env)
    })
    .await
    .ok()
    .flatten()
}

pub(crate) struct ShellCommandPreparation<'a> {
    pub(crate) command: &'a [String],
    pub(crate) command_for_approval: &'a [String],
    pub(crate) shell: &'a Shell,
    pub(crate) shell_snapshot: Option<&'a ShellSnapshotFile>,
    pub(crate) explicit_env_overrides: &'a HashMap<String, String>,
    pub(crate) env: &'a mut HashMap<String, String>,
    pub(crate) shell_type: &'a ShellType,
    pub(crate) sandbox_shell_type: Option<&'a ShellType>,
    pub(crate) sandbox: SandboxType,
    pub(crate) windows_sandbox_level: WindowsSandboxLevel,
    pub(crate) enforce_managed_network: bool,
    pub(crate) approved_powershell_direct_argv: Option<&'a Vec<String>>,
    pub(crate) proof_cwd: Option<&'a Path>,
}

/// Applies the shared snapshot, sandbox-profile, and PowerShell handoff pipeline
/// used by direct shell and unified exec launches.
pub(crate) async fn prepare_shell_command(input: ShellCommandPreparation<'_>) -> Vec<String> {
    let runtime_path_prepends = RuntimePathPrepends;
    let command = maybe_wrap_shell_lc_with_snapshot_file_and_powershell_projection(
        input.command,
        Some(input.command_for_approval),
        input.shell,
        input.shell_snapshot,
        input.explicit_env_overrides,
        input.env,
        &runtime_path_prepends,
    );
    let command = disable_powershell_profile_for_elevated_windows_sandbox(
        &command,
        input.sandbox_shell_type,
        input.sandbox,
        input.windows_sandbox_level,
        input.enforce_managed_network,
    );
    if input.shell_type != &ShellType::PowerShell {
        return command;
    }

    let proof_command = if command.as_slice() == input.command {
        input.command_for_approval
    } else {
        &command
    };
    let approved_direct_command = if let (Some(approved), Some(cwd)) =
        (input.approved_powershell_direct_argv, input.proof_cwd)
    {
        prove_noprofile_powershell_direct_argv_async(proof_command, cwd, input.env)
            .await
            .and_then(|proof| proof.into_command_for_state(proof_command, cwd, input.env))
            .filter(|direct| direct == approved)
            .map(|_| command.clone())
    } else {
        None
    };
    approved_direct_command.unwrap_or_else(|| prefix_powershell_script_with_utf8(&command))
}

/// Shared helper to construct sandbox transform inputs from a tokenized command line and native
/// working directory. Validates that at least a program is present.
pub(crate) fn build_sandbox_command(
    command: &[String],
    cwd: &AbsolutePathBuf,
    env: &HashMap<String, String>,
    additional_permissions: Option<AdditionalPermissionProfile>,
) -> Result<SandboxCommand, ToolError> {
    let (program, args) = command
        .split_first()
        .ok_or_else(|| ToolError::Rejected("command args are empty".to_string()))?;
    let cwd = PathUri::from_abs_path(cwd);
    Ok(SandboxCommand {
        program: program.clone().into(),
        args: args.to_vec(),
        cwd,
        env: env.clone(),
        managed_network: None,
        additional_permissions,
    })
}

/// Returns the exact runtime-owned local snapshot file that a Windows sandbox
/// must make readable while starting the shell. This is launch plumbing, not a
/// user-granted permission, and remote snapshots are owned by their executor.
pub(crate) fn shell_snapshot_additional_read_roots(
    shell_snapshot: Option<&ShellSnapshotFile>,
    sandbox: SandboxType,
) -> Vec<AbsolutePathBuf> {
    if sandbox != SandboxType::WindowsRestrictedToken {
        return Vec::new();
    }
    shell_snapshot
        .and_then(ShellSnapshotFile::local_path)
        .cloned()
        .into_iter()
        .collect()
}

pub(crate) fn exec_env_for_sandbox_permissions(
    env: &HashMap<String, String>,
    sandbox_permissions: SandboxPermissions,
) -> HashMap<String, String> {
    let mut env = env.clone();
    if sandbox_permissions.requires_escalated_permissions()
        && env.contains_key(PROXY_ACTIVE_ENV_KEY)
    {
        strip_managed_proxy_env(&mut env);
    }
    env
}

pub(crate) fn is_managed_proxy_env_var(key: &str, value: &str) -> bool {
    if PROXY_ENV_KEYS.contains(&key) {
        return true;
    }
    if CUSTOM_CA_ENV_KEYS.contains(&key) {
        return is_managed_mitm_ca_trust_bundle_path(value);
    }

    false
}

pub(crate) fn strip_managed_proxy_env(env: &mut HashMap<String, String>) {
    env.retain(|key, value| !is_managed_proxy_env_var(key, value));
}

/// PATH entries owned by Codex runtime setup.
///
/// This is retained as a call-site boundary for runtime-owned PATH setup.
#[derive(Debug, Default, Eq, PartialEq)]
pub(crate) struct RuntimePathPrepends;

pub(crate) fn disable_powershell_profile_for_elevated_windows_sandbox(
    command: &[String],
    shell_type: Option<&ShellType>,
    sandbox: SandboxType,
    windows_sandbox_level: WindowsSandboxLevel,
    enforce_managed_network: bool,
) -> Vec<String> {
    if shell_type != Some(&ShellType::PowerShell)
        || sandbox != SandboxType::WindowsRestrictedToken
        || !windows_sandbox_uses_elevated_backend(windows_sandbox_level, enforce_managed_network)
        || command.is_empty()
    {
        return command.to_vec();
    }

    if command[1..]
        .iter()
        .any(|arg| arg.eq_ignore_ascii_case("-NoProfile"))
    {
        return command.to_vec();
    }

    // The elevated Windows sandbox runs as a dedicated sandbox account while
    // HOME/USERPROFILE may still point at the real user profile. Loading
    // PowerShell profiles in that mixed context is not a valid login shell.
    let mut command = command.to_vec();
    command.insert(1, "-NoProfile".to_string());
    command
}

/// For commands produced by `Shell::derive_exec_args` and a snapshot configured
/// on the matching session shell, rewrite the argv to a clean shell that loads
/// the snapshot before running the original script.
///
/// PowerShell uses `-NoProfile` and a native PowerShell wrapper. Cmd uses a
/// clean `/d /v:off` wrapper. Compatibility-only non-Windows shell variants
/// are left unchanged.
///
/// `explicit_env_overrides` and `env` are intentionally separate inputs.
/// `explicit_env_overrides` contains policy-driven shell env overrides that
/// should win after the snapshot is sourced, while `env` is the full live exec
/// environment. We need access to both so snapshot restore logic can preserve
/// runtime-only vars like `CODEX_THREAD_ID` without pretending they came from
/// the explicit override policy.
#[cfg(test)]
pub(crate) fn maybe_wrap_shell_lc_with_snapshot(
    command: &[String],
    session_shell: &Shell,
    shell_snapshot: Option<&AbsolutePathBuf>,
    explicit_env_overrides: &HashMap<String, String>,
    env: &HashMap<String, String>,
    runtime_path_prepends: &RuntimePathPrepends,
) -> Vec<String> {
    let metrics = codex_otel::global();
    maybe_wrap_shell_lc_with_snapshot_and_metrics(
        command,
        session_shell,
        shell_snapshot,
        explicit_env_overrides,
        env,
        runtime_path_prepends,
        metrics.as_ref(),
    )
}

pub(crate) fn maybe_wrap_shell_lc_with_snapshot_file(
    command: &[String],
    session_shell: &Shell,
    shell_snapshot: Option<&ShellSnapshotFile>,
    explicit_env_overrides: &HashMap<String, String>,
    env: &mut HashMap<String, String>,
    runtime_path_prepends: &RuntimePathPrepends,
) -> Vec<String> {
    maybe_wrap_shell_lc_with_snapshot_file_and_powershell_projection(
        command,
        /*powershell_projection*/ None,
        session_shell,
        shell_snapshot,
        explicit_env_overrides,
        env,
        runtime_path_prepends,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn maybe_wrap_shell_lc_with_snapshot_file_and_powershell_projection(
    command: &[String],
    powershell_projection: Option<&[String]>,
    session_shell: &Shell,
    shell_snapshot: Option<&ShellSnapshotFile>,
    explicit_env_overrides: &HashMap<String, String>,
    env: &mut HashMap<String, String>,
    runtime_path_prepends: &RuntimePathPrepends,
) -> Vec<String> {
    let Some(snapshot) = shell_snapshot else {
        return command.to_vec();
    };
    let snapshot_path = snapshot.native_path_string();
    if session_shell.shell_type == ShellType::Cmd {
        return maybe_wrap_cmd_with_snapshot(
            command,
            session_shell,
            &snapshot_path,
            snapshot.contents(),
            explicit_env_overrides,
            env,
        );
    }
    let metrics = codex_otel::global();
    maybe_wrap_shell_lc_with_snapshot_source_and_powershell_projection(
        command,
        powershell_projection,
        session_shell,
        &snapshot_path,
        snapshot.contents(),
        explicit_env_overrides,
        env,
        runtime_path_prepends,
        metrics.as_ref(),
    )
}

#[cfg(test)]
fn maybe_wrap_shell_lc_with_snapshot_and_metrics(
    command: &[String],
    session_shell: &Shell,
    shell_snapshot: Option<&AbsolutePathBuf>,
    explicit_env_overrides: &HashMap<String, String>,
    env: &HashMap<String, String>,
    runtime_path_prepends: &RuntimePathPrepends,
    metrics: Option<&MetricsClient>,
) -> Vec<String> {
    let record_powershell_skip = |reason| {
        if matches!(session_shell.shell_type, ShellType::PowerShell) {
            record_shell_snapshot_replay(metrics, "skipped", reason);
        }
    };
    let Some(snapshot) = shell_snapshot else {
        record_powershell_skip("snapshot_unavailable");
        return command.to_vec();
    };

    if !snapshot.exists() {
        record_powershell_skip("snapshot_missing");
        return command.to_vec();
    }

    let Ok(snapshot_contents) = std::fs::read_to_string(snapshot) else {
        record_powershell_skip("snapshot_unreadable");
        return command.to_vec();
    };

    maybe_wrap_shell_lc_with_snapshot_source(
        command,
        session_shell,
        &snapshot.to_string_lossy(),
        &snapshot_contents,
        explicit_env_overrides,
        env,
        runtime_path_prepends,
        metrics,
    )
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn maybe_wrap_shell_lc_with_snapshot_source(
    command: &[String],
    session_shell: &Shell,
    snapshot_path: &str,
    snapshot_contents: &str,
    explicit_env_overrides: &HashMap<String, String>,
    env: &HashMap<String, String>,
    runtime_path_prepends: &RuntimePathPrepends,
    metrics: Option<&MetricsClient>,
) -> Vec<String> {
    maybe_wrap_shell_lc_with_snapshot_source_and_powershell_projection(
        command,
        /*powershell_projection*/ None,
        session_shell,
        snapshot_path,
        snapshot_contents,
        explicit_env_overrides,
        env,
        runtime_path_prepends,
        metrics,
    )
}

#[allow(clippy::too_many_arguments)]
fn maybe_wrap_shell_lc_with_snapshot_source_and_powershell_projection(
    command: &[String],
    powershell_projection: Option<&[String]>,
    session_shell: &Shell,
    snapshot_path: &str,
    snapshot_contents: &str,
    explicit_env_overrides: &HashMap<String, String>,
    env: &HashMap<String, String>,
    _runtime_path_prepends: &RuntimePathPrepends,
    metrics: Option<&MetricsClient>,
) -> Vec<String> {
    match session_shell.shell_type {
        ShellType::PowerShell => maybe_wrap_powershell_with_snapshot(
            command,
            powershell_projection,
            session_shell,
            snapshot_path,
            snapshot_contents,
            explicit_env_overrides,
            env,
            metrics,
        ),
        ShellType::Cmd | ShellType::Bash | ShellType::Zsh | ShellType::Sh => command.to_vec(),
    }
}

#[allow(clippy::too_many_arguments)]
fn maybe_wrap_powershell_with_snapshot(
    command: &[String],
    powershell_projection: Option<&[String]>,
    session_shell: &Shell,
    snapshot_path: &str,
    snapshot_contents: &str,
    explicit_env_overrides: &HashMap<String, String>,
    env: &HashMap<String, String>,
    metrics: Option<&MetricsClient>,
) -> Vec<String> {
    if !snapshot_contents
        .lines()
        .any(|line| line == POWERSHELL_SNAPSHOT_FORMAT_HEADER)
    {
        record_shell_snapshot_replay(metrics, "skipped", "unsupported_format");
        return command.to_vec();
    }

    let inspectable_command = powershell_projection.unwrap_or(command);
    let Some((command_shell, original_script)) = extract_powershell_command(inspectable_command)
    else {
        record_shell_snapshot_replay(metrics, "skipped", "unsupported_command");
        return command.to_vec();
    };
    let session_shell_path = session_shell.shell_path.to_string_lossy();
    if !command_shell.eq_ignore_ascii_case(&session_shell_path) {
        record_shell_snapshot_replay(metrics, "skipped", "shell_mismatch");
        return command.to_vec();
    }

    let mut override_keys = explicit_env_overrides
        .keys()
        .map(String::as_str)
        .filter(|key| is_valid_powershell_env_name(key))
        .collect::<Vec<_>>();
    for key in [CODEX_THREAD_ID_ENV_VAR, CODEX_PERMISSION_PROFILE_ENV_VAR] {
        if key == CODEX_PERMISSION_PROFILE_ENV_VAR || powershell_env_value(env, key).is_some() {
            override_keys.push(key);
        }
    }
    override_keys.sort_unstable_by_key(|key| key.to_ascii_lowercase());
    override_keys.dedup_by(|left, right| left.eq_ignore_ascii_case(right));
    let override_restores = build_powershell_env_restores(&override_keys, env);

    let mut proxy_keys = PROXY_ENV_KEYS
        .iter()
        .copied()
        .chain(CUSTOM_CA_ENV_KEYS)
        .filter(|key| is_valid_powershell_env_name(key))
        .collect::<Vec<_>>();
    proxy_keys.sort_unstable_by_key(|key| key.to_ascii_lowercase());
    proxy_keys.dedup_by(|left, right| left.eq_ignore_ascii_case(right));
    let proxy_restores = build_powershell_env_restores(&proxy_keys, env);
    let live_proxy_active = powershell_env_value(env, PROXY_ACTIVE_ENV_KEY).is_some();
    let proxy_restore = format!(
        "if ({} -or (Microsoft.PowerShell.Management\\Test-Path -LiteralPath 'Env:{}')) {{\n{}\n}}",
        if live_proxy_active { "$true" } else { "$false" },
        powershell_single_quote(PROXY_ACTIVE_ENV_KEY),
        proxy_restores
    );

    let snapshot_path = powershell_single_quote(snapshot_path);
    let rewritten_script = format!(
        "try {{ . '{snapshot_path}' *> $null }} catch {{ [Console]::Error.WriteLine('codex: shell snapshot replay failed: ' + $_.Exception.Message) }}\n{override_restores}\n{proxy_restore}\n& {{\n{original_script}\n}}"
    );

    let rewritten = vec![
        session_shell_path.into_owned(),
        "-NoProfile".to_string(),
        "-Command".to_string(),
        rewritten_script,
    ];
    record_shell_snapshot_replay(metrics, "applied", "matched");
    rewritten
}

fn maybe_wrap_cmd_with_snapshot(
    command: &[String],
    session_shell: &Shell,
    snapshot_path: &str,
    snapshot_contents: &str,
    explicit_env_overrides: &HashMap<String, String>,
    env: &mut HashMap<String, String>,
) -> Vec<String> {
    let Some(snapshot_environment) = parse_cmd_snapshot_environment(snapshot_contents) else {
        return command.to_vec();
    };
    let session_shell_path = session_shell.shell_path.to_string_lossy();
    if command.is_empty() || !command[0].eq_ignore_ascii_case(&session_shell_path) {
        return command.to_vec();
    }
    let Some(command_flag_index) = command
        .iter()
        .position(|arg| arg.eq_ignore_ascii_case("/c"))
    else {
        return command.to_vec();
    };
    let Some(_original_script) = command.get(command_flag_index + 1) else {
        return command.to_vec();
    };

    let _ = snapshot_path;
    let mut restore_keys = explicit_env_overrides
        .keys()
        .map(String::as_str)
        .filter(|key| is_valid_cmd_environment_name(key))
        .collect::<Vec<_>>();
    restore_keys.extend([CODEX_THREAD_ID_ENV_VAR, CODEX_PERMISSION_PROFILE_ENV_VAR]);
    restore_keys.extend(PROXY_ENV_KEYS.iter().copied());
    restore_keys.extend(CUSTOM_CA_ENV_KEYS);
    restore_keys.sort_unstable_by_key(|key| key.to_ascii_lowercase());
    restore_keys.dedup_by(|left, right| left.eq_ignore_ascii_case(right));

    for (key, value) in snapshot_environment {
        if restore_keys
            .iter()
            .any(|protected| key.eq_ignore_ascii_case(protected))
        {
            continue;
        }
        env.retain(|candidate, _| !candidate.eq_ignore_ascii_case(&key));
        env.insert(key, value);
    }
    command.to_vec()
}

fn is_valid_cmd_environment_name(name: &str) -> bool {
    !name.is_empty()
        && !name.chars().any(|character| {
            matches!(
                character,
                '=' | '"'
                    | '%'
                    | '!'
                    | '^'
                    | '&'
                    | '|'
                    | '<'
                    | '>'
                    | '('
                    | ')'
                    | '\r'
                    | '\n'
                    | '\0'
            )
        })
}

fn record_shell_snapshot_replay(
    metrics: Option<&MetricsClient>,
    result: &'static str,
    reason: &'static str,
) {
    let Some(metrics) = metrics else {
        return;
    };
    if let Err(err) = metrics.counter(
        SHELL_SNAPSHOT_REPLAY_METRIC,
        /*inc*/ 1,
        &[
            ("shell", "powershell"),
            ("result", result),
            ("reason", reason),
        ],
    ) {
        tracing::warn!("shell snapshot replay metric failed: {err}");
    }
}

fn build_powershell_env_restores(keys: &[&str], env: &HashMap<String, String>) -> String {
    keys.iter()
        .map(|key| {
            let path = powershell_single_quote(&format!("Env:{key}"));
            match powershell_env_value(env, key) {
                Some(value) => {
                    let value = powershell_single_quote(value);
                    format!(
                        "Microsoft.PowerShell.Management\\Set-Item -LiteralPath '{path}' -Value '{value}'"
                    )
                }
                None => format!(
                    "Microsoft.PowerShell.Management\\Remove-Item -LiteralPath '{path}' -Force -ErrorAction SilentlyContinue"
                ),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn powershell_env_value<'a>(env: &'a HashMap<String, String>, key: &str) -> Option<&'a str> {
    env.iter().find_map(|(candidate, value)| {
        candidate
            .eq_ignore_ascii_case(key)
            .then_some(value.as_str())
    })
}

fn is_valid_powershell_env_name(name: &str) -> bool {
    !name.is_empty() && !name.contains(['\0', '='])
}

#[cfg(test)]
mod disable_powershell_profile_tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn inserts_no_profile_for_elevated_windows_sandbox() {
        let command = vec![
            "powershell.exe".to_string(),
            "-Command".to_string(),
            "Write-Output ok".to_string(),
        ];

        let rewritten = disable_powershell_profile_for_elevated_windows_sandbox(
            &command,
            Some(&ShellType::PowerShell),
            SandboxType::WindowsRestrictedToken,
            WindowsSandboxLevel::Elevated,
            false,
        );

        assert_eq!(
            rewritten,
            vec![
                "powershell.exe".to_string(),
                "-NoProfile".to_string(),
                "-Command".to_string(),
                "Write-Output ok".to_string(),
            ]
        );
    }

    #[test]
    fn inserts_no_profile_before_encoded_command() {
        let command = vec![
            "powershell.exe".to_string(),
            "-EncodedCommand".to_string(),
            "VwByAGkAdABlAC0ATwB1AHQAcAB1AHQAIABvAGsA".to_string(),
        ];

        let rewritten = disable_powershell_profile_for_elevated_windows_sandbox(
            &command,
            Some(&ShellType::PowerShell),
            SandboxType::WindowsRestrictedToken,
            WindowsSandboxLevel::Elevated,
            false,
        );

        assert_eq!(
            rewritten,
            vec![
                "powershell.exe".to_string(),
                "-NoProfile".to_string(),
                "-EncodedCommand".to_string(),
                "VwByAGkAdABlAC0ATwB1AHQAcAB1AHQAIABvAGsA".to_string(),
            ]
        );
    }

    #[test]
    fn preserves_existing_no_profile() {
        let command = vec![
            "pwsh.exe".to_string(),
            "-NoProfile".to_string(),
            "-Command".to_string(),
            "Write-Output ok".to_string(),
        ];

        let rewritten = disable_powershell_profile_for_elevated_windows_sandbox(
            &command,
            Some(&ShellType::PowerShell),
            SandboxType::WindowsRestrictedToken,
            WindowsSandboxLevel::Elevated,
            false,
        );

        assert_eq!(rewritten, command);
    }

    #[test]
    fn leaves_legacy_restricted_token_backend_alone() {
        let command = vec![
            "powershell.exe".to_string(),
            "-Command".to_string(),
            "Write-Output ok".to_string(),
        ];

        let rewritten = disable_powershell_profile_for_elevated_windows_sandbox(
            &command,
            Some(&ShellType::PowerShell),
            SandboxType::WindowsRestrictedToken,
            WindowsSandboxLevel::RestrictedToken,
            false,
        );

        assert_eq!(rewritten, command);
    }

    #[test]
    fn inserts_no_profile_when_managed_network_promotes_restricted_token_backend() {
        let command = vec![
            "powershell.exe".to_string(),
            "-Command".to_string(),
            "Write-Output ok".to_string(),
        ];

        let rewritten = disable_powershell_profile_for_elevated_windows_sandbox(
            &command,
            Some(&ShellType::PowerShell),
            SandboxType::WindowsRestrictedToken,
            WindowsSandboxLevel::RestrictedToken,
            true,
        );

        assert_eq!(
            rewritten,
            vec![
                "powershell.exe".to_string(),
                "-NoProfile".to_string(),
                "-Command".to_string(),
                "Write-Output ok".to_string(),
            ]
        );
    }

    #[test]
    fn leaves_unsandboxed_attempts_alone() {
        let command = vec![
            "powershell.exe".to_string(),
            "-Command".to_string(),
            "Write-Output ok".to_string(),
        ];

        let rewritten = disable_powershell_profile_for_elevated_windows_sandbox(
            &command,
            Some(&ShellType::PowerShell),
            SandboxType::None,
            WindowsSandboxLevel::Elevated,
            false,
        );

        assert_eq!(rewritten, command);
    }

    #[test]
    fn leaves_non_powershell_alone() {
        let command = vec![
            "/bin/bash".to_string(),
            "-lc".to_string(),
            "echo ok".to_string(),
        ];

        let rewritten = disable_powershell_profile_for_elevated_windows_sandbox(
            &command,
            Some(&ShellType::Bash),
            SandboxType::WindowsRestrictedToken,
            WindowsSandboxLevel::Elevated,
            false,
        );

        assert_eq!(rewritten, command);
    }
}

#[cfg(test)]
mod shell_snapshot_replay_tests {
    use super::*;
    use codex_otel::MetricsConfig;
    use opentelemetry_sdk::metrics::InMemoryMetricExporter;
    use opentelemetry_sdk::metrics::data::AggregatedMetrics;
    use opentelemetry_sdk::metrics::data::MetricData;
    use pretty_assertions::assert_eq;
    use std::collections::BTreeSet;

    #[test]
    fn compatibility_shell_does_not_replay_non_windows_snapshot_source() {
        let shell = Shell {
            shell_type: ShellType::Bash,
            shell_path: "bash".into(),
        };
        let command = vec![
            "bash".to_string(),
            "-lc".to_string(),
            "printf ready".to_string(),
        ];

        let rewritten = maybe_wrap_shell_lc_with_snapshot_source(
            &command,
            &shell,
            "snapshot.unsupported",
            "# Snapshot file\n# non-Windows format\n",
            &HashMap::new(),
            &HashMap::new(),
            &RuntimePathPrepends,
            None,
        );

        assert_eq!(rewritten, command);
    }

    #[test]
    fn powershell_snapshot_replay_uses_the_inspectable_encoded_command_projection() {
        let shell = Shell {
            shell_type: ShellType::PowerShell,
            shell_path: "pwsh".into(),
        };
        let encoded_command = vec![
            "pwsh".to_string(),
            "-NoLogo".to_string(),
            "-NoProfile".to_string(),
            "-EncodedCommand".to_string(),
            "opaque-encoded-payload".to_string(),
        ];
        let safety_projection = vec![
            "pwsh".to_string(),
            "-NoLogo".to_string(),
            "-NoProfile".to_string(),
            "-Command".to_string(),
            "Write-Output ready".to_string(),
        ];

        let rewritten = maybe_wrap_shell_lc_with_snapshot_source_and_powershell_projection(
            &encoded_command,
            Some(&safety_projection),
            &shell,
            "snapshot.ps1",
            &format!("# Snapshot file\n{POWERSHELL_SNAPSHOT_FORMAT_HEADER}\n"),
            &HashMap::new(),
            &HashMap::new(),
            &RuntimePathPrepends,
            None,
        );

        assert_ne!(rewritten, encoded_command);
        assert_eq!(rewritten.get(1).map(String::as_str), Some("-NoProfile"));
        assert_eq!(rewritten.get(2).map(String::as_str), Some("-Command"));
        let wrapper = rewritten.get(3).expect("PowerShell wrapper script");
        assert!(wrapper.contains("snapshot.ps1"));
        assert!(wrapper.contains("Write-Output ready"));
        assert!(!wrapper.contains("opaque-encoded-payload"));
    }

    #[test]
    fn cmd_snapshot_environment_is_applied_without_batch_reexpansion() {
        let shell = Shell {
            shell_type: ShellType::Cmd,
            shell_path: "C:\\Windows\\System32\\cmd.exe".into(),
        };
        let command = vec![
            shell.shell_path.to_string_lossy().into_owned(),
            "/c".to_string(),
            "echo ready".to_string(),
        ];
        let remote_path = "C:\\remote workspace\\snapshot.cmd";
        let mut env = HashMap::new();

        let rewritten = maybe_wrap_cmd_with_snapshot(
            &command,
            &shell,
            remote_path,
            &format!("@rem Snapshot file\r\n{CMD_SNAPSHOT_FORMAT_HEADER}\r\n"),
            &HashMap::new(),
            &mut env,
        );

        assert_eq!(rewritten, command);
        assert!(!env.contains_key("__CODEX_SNAPSHOT_FILE"));
    }

    #[test]
    fn cmd_snapshot_replays_state_and_restores_unsafe_override_value() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let snapshot_path = dir.path().join("snapshot.cmd");
        std::fs::write(
            &snapshot_path,
            format!(
                "@rem Snapshot file\r\n{CMD_SNAPSHOT_FORMAT_HEADER}\r\n@set CODEX_CMD_WRAP_TEST=snapshot\r\n"
            ),
        )
        .expect("write Cmd snapshot");
        let shell_path = std::env::var("COMSPEC")
            .unwrap_or_else(|_| "C:\\Windows\\System32\\cmd.exe".to_string());
        let shell = Shell {
            shell_type: ShellType::Cmd,
            shell_path: shell_path.clone().into(),
        };
        let command = vec![
            shell_path,
            "/c".to_string(),
            "@set CODEX_CMD_WRAP_TEST".to_string(),
        ];
        let explicit_value = "100%^&! \"quoted\" | < > (x)";
        let explicit = HashMap::from([(
            "CODEX_CMD_WRAP_TEST".to_string(),
            explicit_value.to_string(),
        )]);
        let mut env = std::env::vars().collect::<HashMap<_, _>>();
        env.insert(
            "CODEX_CMD_WRAP_TEST".to_string(),
            explicit_value.to_string(),
        );

        let rewritten = maybe_wrap_cmd_with_snapshot(
            &command,
            &shell,
            &snapshot_path.to_string_lossy(),
            &std::fs::read_to_string(&snapshot_path).expect("read snapshot"),
            &explicit,
            &mut env,
        );
        let output = std::process::Command::new(&rewritten[0])
            .args(&rewritten[1..])
            .env_clear()
            .envs(env)
            .output()
            .expect("run rewritten Cmd command");

        assert!(output.status.success(), "command failed: {output:?}");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            format!("CODEX_CMD_WRAP_TEST={explicit_value}")
        );
    }

    #[test]
    fn powershell_snapshot_replays_state_and_restores_live_overrides() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let snapshot_path = AbsolutePathBuf::from_absolute_path(dir.path().join("snapshot.ps1"))
            .expect("absolute snapshot path");
        std::fs::write(
            &snapshot_path,
            format!(
                "# Snapshot file\n{POWERSHELL_SNAPSHOT_FORMAT_HEADER}\n\
                 function Invoke-CodexSnapshotFunction {{ 'from-snapshot' }}\n\
                 Microsoft.PowerShell.Management\\Set-Item -LiteralPath 'Env:CODEX_TEST_OVERRIDE' -Value 'stale'\n\
                 Microsoft.PowerShell.Management\\Set-Item -LiteralPath 'Env:{CODEX_PERMISSION_PROFILE_ENV_VAR}' -Value 'stale-profile'\n"
            ),
        )
        .expect("write PowerShell snapshot");
        let shell = crate::shell::get_shell(ShellType::PowerShell, /*path*/ None)
            .expect("PowerShell is required on Windows");
        let original = shell.derive_exec_args(
            &format!(
                "Microsoft.PowerShell.Utility\\Write-Output ((Invoke-CodexSnapshotFunction) + '|' + $env:CODEX_TEST_OVERRIDE + '|' + (Microsoft.PowerShell.Management\\Test-Path -LiteralPath 'Env:{CODEX_PERMISSION_PROFILE_ENV_VAR}'))"
            ),
            /*use_login_shell*/ true,
        )
        .expect("PowerShell args");
        let explicit_overrides =
            HashMap::from([("CODEX_TEST_OVERRIDE".to_string(), "current".to_string())]);
        let mut env = std::env::vars().collect::<HashMap<_, _>>();
        env.insert("CODEX_TEST_OVERRIDE".to_string(), "current".to_string());
        env.retain(|key, _| !key.eq_ignore_ascii_case(CODEX_PERMISSION_PROFILE_ENV_VAR));

        let rewritten = maybe_wrap_shell_lc_with_snapshot(
            &original,
            &shell,
            Some(&snapshot_path),
            &explicit_overrides,
            &env,
            &RuntimePathPrepends,
        );

        assert_eq!(rewritten.get(1).map(String::as_str), Some("-NoProfile"));
        let output = std::process::Command::new(&rewritten[0])
            .args(&rewritten[1..])
            .env_clear()
            .envs(&env)
            .current_dir(dir.path())
            .output()
            .expect("run wrapped PowerShell command");
        assert!(output.status.success(), "command failed: {output:?}");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "from-snapshot|current|False"
        );
    }

    #[test]
    fn powershell_snapshot_source_failure_is_visible_while_the_command_continues() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let snapshot_path = AbsolutePathBuf::from_absolute_path(dir.path().join("broken.ps1"))
            .expect("absolute snapshot path");
        std::fs::write(
            &snapshot_path,
            format!("{POWERSHELL_SNAPSHOT_FORMAT_HEADER}\nthrow 'broken snapshot'\n"),
        )
        .expect("write broken PowerShell snapshot");
        let shell = crate::shell::get_shell(ShellType::PowerShell, /*path*/ None)
            .expect("PowerShell is required on Windows");
        let original = shell
            .derive_exec_args(
                "Microsoft.PowerShell.Utility\\Write-Output 'command-ran'",
                /*use_login_shell*/ true,
            )
            .expect("PowerShell args");

        let rewritten = maybe_wrap_shell_lc_with_snapshot(
            &original,
            &shell,
            Some(&snapshot_path),
            &HashMap::new(),
            &std::env::vars().collect(),
            &RuntimePathPrepends,
        );
        let output = std::process::Command::new(&rewritten[0])
            .args(&rewritten[1..])
            .output()
            .expect("run wrapped PowerShell command");

        assert!(output.status.success(), "command failed: {output:?}");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "command-ran"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("codex: shell snapshot replay failed: broken snapshot"));
    }

    #[test]
    fn activation_metric_distinguishes_powershell_snapshot_replay_applied_and_skipped() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let snapshot_path = AbsolutePathBuf::from_absolute_path(dir.path().join("snapshot.ps1"))
            .expect("absolute snapshot path");
        std::fs::write(
            &snapshot_path,
            format!("# Snapshot file\n{POWERSHELL_SNAPSHOT_FORMAT_HEADER}\n"),
        )
        .expect("write PowerShell snapshot");
        let shell = crate::shell::get_shell(ShellType::PowerShell, /*path*/ None)
            .expect("PowerShell is required on Windows");
        let command = shell
            .derive_exec_args("Write-Output ok", /*use_login_shell*/ true)
            .expect("PowerShell args");
        let env = std::env::vars().collect::<HashMap<_, _>>();
        let metrics = MetricsClient::new(
            MetricsConfig::in_memory(
                "test",
                "codex-core",
                env!("CARGO_PKG_VERSION"),
                InMemoryMetricExporter::default(),
            )
            .with_runtime_reader(),
        )
        .expect("in-memory metrics client");

        let applied = maybe_wrap_shell_lc_with_snapshot_and_metrics(
            &command,
            &shell,
            Some(&snapshot_path),
            &HashMap::new(),
            &env,
            &RuntimePathPrepends,
            Some(&metrics),
        );
        assert_ne!(applied, command);
        let skipped = maybe_wrap_shell_lc_with_snapshot_and_metrics(
            &command,
            &shell,
            None,
            &HashMap::new(),
            &env,
            &RuntimePathPrepends,
            Some(&metrics),
        );
        assert_eq!(skipped, command);

        let snapshot = metrics.snapshot().expect("metrics snapshot");
        let metric = snapshot
            .scope_metrics()
            .flat_map(opentelemetry_sdk::metrics::data::ScopeMetrics::metrics)
            .find(|metric| metric.name() == SHELL_SNAPSHOT_REPLAY_METRIC)
            .expect("shell snapshot replay metric");
        let points = match metric.data() {
            AggregatedMetrics::U64(data) => match data {
                MetricData::Sum(sum) => sum
                    .data_points()
                    .map(|point| {
                        let tags = point
                            .attributes()
                            .map(|attribute| {
                                (
                                    attribute.key.as_str().to_string(),
                                    attribute.value.as_str().to_string(),
                                )
                            })
                            .collect::<std::collections::BTreeMap<_, _>>();
                        (
                            tags.get("shell").cloned().unwrap_or_default(),
                            tags.get("result").cloned().unwrap_or_default(),
                            tags.get("reason").cloned().unwrap_or_default(),
                            point.value(),
                        )
                    })
                    .collect::<BTreeSet<_>>(),
                _ => panic!("unexpected shell snapshot metric aggregation"),
            },
            _ => panic!("unexpected shell snapshot metric type"),
        };
        assert_eq!(
            points,
            BTreeSet::from([
                (
                    "powershell".to_string(),
                    "applied".to_string(),
                    "matched".to_string(),
                    1,
                ),
                (
                    "powershell".to_string(),
                    "skipped".to_string(),
                    "snapshot_unavailable".to_string(),
                    1,
                ),
            ])
        );
    }
}
