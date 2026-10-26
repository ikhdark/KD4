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
use crate::shell_snapshot::POWERSHELL_SNAPSHOT_FORMAT_HEADER;
use crate::shell_snapshot::ShellSnapshotFile;
use crate::shell_snapshot::parse_cmd_snapshot_environment;
use crate::tools::sandboxing::ToolError;


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
use codex_shell_command::powershell::preserve_powershell_native_exit_code;
use codex_shell_command::powershell::prove_noprofile_powershell_command_as_direct_argv;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use std::collections::HashMap;
use std::path::Path;

const SHELL_SNAPSHOT_REPLAY_METRIC: &str = "codex.shell_snapshot_replay";

// These are ordinary user transport settings, not the removed managed proxy.
// A shell snapshot must not overwrite the current proxy or TLS trust configuration.
const SHELL_TRANSPORT_ENV_KEYS: &[&str] = &[
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "http_proxy",
    "https_proxy",
    "YARN_HTTP_PROXY",
    "YARN_HTTPS_PROXY",
    "npm_config_http_proxy",
    "npm_config_https_proxy",
    "npm_config_proxy",
    "NPM_CONFIG_HTTP_PROXY",
    "NPM_CONFIG_HTTPS_PROXY",
    "NPM_CONFIG_PROXY",
    "BUNDLE_HTTP_PROXY",
    "BUNDLE_HTTPS_PROXY",
    "PIP_PROXY",
    "WS_PROXY",
    "WSS_PROXY",
    "ws_proxy",
    "wss_proxy",
    "NO_PROXY",
    "no_proxy",
    "npm_config_noproxy",
    "NPM_CONFIG_NOPROXY",
    "YARN_NO_PROXY",
    "BUNDLE_NO_PROXY",
    "ALL_PROXY",
    "all_proxy",
    "FTP_PROXY",
    "ftp_proxy",
    "CODEX_CA_CERTIFICATE",
    "SSL_CERT_FILE",
    "REQUESTS_CA_BUNDLE",
    "CURL_CA_BUNDLE",
    "NODE_EXTRA_CA_CERTS",
    "GIT_SSL_CAINFO",
    "CARGO_HTTP_CAINFO",
    "PIP_CERT",
    "BUNDLE_SSL_CA_CERT",
    "npm_config_cafile",
    "NPM_CONFIG_CAFILE",
    "ELECTRON_GET_USE_PROXY",
    "NODE_USE_ENV_PROXY",
];

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
    let command = maybe_wrap_shell_lc_with_snapshot_file_and_powershell_projection(
        input.command,
        Some(input.command_for_approval),
        input.shell,
        input.shell_snapshot,
        input.explicit_env_overrides,
        input.env,
    );
    let command = disable_powershell_profile_for_elevated_windows_sandbox(
        &command,
        input.sandbox_shell_type,
        input.sandbox,
        input.windows_sandbox_level,
        input.enforce_managed_network,
    );
    if input.shell_type != &ShellType::PowerShell || extract_powershell_command(&command).is_none()
    {
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
            .is_some_and(|direct| &direct == approved)
    } else {
        false
    };
    if approved_direct_command {
        return command;
    }
    let command = prefix_powershell_script_with_utf8(&command);
    // A snapshot wrapper ends with the approval script; an unwrapped launch
    // ends with its own script.
    [input.command_for_approval, input.command]
        .into_iter()
        .filter_map(|candidate| extract_powershell_command(candidate).map(|(_, script)| script))
        .find_map(|script| preserve_powershell_native_exit_code(&command, script))
        .unwrap_or(command)
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
pub(crate) fn maybe_wrap_shell_lc_with_snapshot_file(
    command: &[String],
    session_shell: &Shell,
    shell_snapshot: Option<&ShellSnapshotFile>,
    explicit_env_overrides: &HashMap<String, String>,
    env: &mut HashMap<String, String>,
) -> Vec<String> {
    maybe_wrap_shell_lc_with_snapshot_file_and_powershell_projection(
        command,
        /*powershell_projection*/ None,
        session_shell,
        shell_snapshot,
        explicit_env_overrides,
        env,
    )
}

pub(crate) fn maybe_wrap_shell_lc_with_snapshot_file_and_powershell_projection(
    command: &[String],
    powershell_projection: Option<&[String]>,
    session_shell: &Shell,
    shell_snapshot: Option<&ShellSnapshotFile>,
    explicit_env_overrides: &HashMap<String, String>,
    env: &mut HashMap<String, String>,
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
        metrics.as_ref(),
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
    override_keys.extend(SHELL_TRANSPORT_ENV_KEYS.iter().copied());
    override_keys.sort_unstable_by_key(|key| key.to_ascii_lowercase());
    override_keys.dedup_by(|left, right| left.eq_ignore_ascii_case(right));
    let override_restores = build_powershell_env_restores(&override_keys, env);

    let snapshot_path = powershell_single_quote(snapshot_path);
    // Keep the command at script scope so PowerShell propagates its final native
    // exit status; invoking a script block can turn a failed pipeline into success.
    let rewritten_script = format!(
        "try {{ . '{snapshot_path}' *> $null }} catch {{ [Console]::Error.WriteLine('codex: shell snapshot replay failed: ' + $_.Exception.Message) }}\n{override_restores}\n{original_script}"
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
    restore_keys.extend(SHELL_TRANSPORT_ENV_KEYS.iter().copied());
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


