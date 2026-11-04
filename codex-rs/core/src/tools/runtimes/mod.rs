/*
Module: runtimes

Concrete ToolRuntime implementations for specific tools. Each runtime stays
small and focused and reuses the orchestrator for approvals and sandbox selection.
*/
use crate::exec_env::CODEX_PERMISSION_PROFILE_ENV_VAR;
use crate::exec_env::CODEX_THREAD_ID_ENV_VAR;
use crate::sandboxing::SandboxPermissions;
use crate::shell::Shell;
use crate::shell::ShellType;
use crate::shell_snapshot::ShellSnapshotFile;
use crate::shell_snapshot::parse_cmd_snapshot_environment;
use crate::tools::sandboxing::ExecApprovalRequirement;
use crate::tools::sandboxing::ToolCtx;
use crate::tools::sandboxing::ToolError;


use codex_protocol::config_types::WindowsSandboxLevel;
use codex_protocol::models::AdditionalPermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_sandboxing::SandboxCommand;
use codex_sandboxing::SandboxType;
use codex_sandboxing::windows_sandbox_uses_elevated_backend;
use codex_shell_command::powershell::ProvenPowershellDirectArgv;
use codex_shell_command::powershell::extract_powershell_command;
use codex_shell_command::powershell::prefix_powershell_script_with_utf8;
use codex_shell_command::powershell::preserve_powershell_native_exit_code;
use codex_shell_command::powershell::prove_noprofile_powershell_command_as_direct_argv;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use std::collections::HashMap;
use std::path::Path;


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

/// Readonly admission is not a reusable executable-resolution receipt. Re-prove
/// after snapshot environment projection for every sandbox attempt, and pin a
/// direct utility's path without changing its policy/approval identity.
pub(crate) async fn recheck_readonly_command_before_launch(
    command: &[String],
    shell_wrapper_is_owned: bool,
    cwd: Option<&Path>,
    env: &HashMap<String, String>,
    sandbox_permissions: SandboxPermissions,
    requirement: &ExecApprovalRequirement,
    ctx: &ToolCtx,
) -> Result<Option<Vec<String>>, ToolError> {
    let profile = ctx.turn.permission_profile();
    let managed_without_backend = ctx.turn.windows_sandbox_level == WindowsSandboxLevel::Disabled
        && crate::exec_policy::profile_has_managed_filesystem_restrictions(&profile);
    let heuristic_readonly = matches!(
        requirement,
        ExecApprovalRequirement::Skip { bypass_sandbox: false, .. }
    ) && (ctx.turn.approval_policy.value() == AskForApproval::UnlessTrusted
        || managed_without_backend);
    if !heuristic_readonly {
        return Ok(None);
    }
    let cwd = cwd.ok_or_else(|| {
        ToolError::Rejected(
            "Readonly executable provenance is unavailable for this execution environment.".into(),
        )
    })?.to_path_buf();
    // Reuse the policy owner so explicitly allowed segments remain allowed;
    // only unmatched readonly segments need provenance. Never replace an
    // explicit authorization with a blanket whole-script safelist check.
    let request = crate::exec_policy::ExecApprovalRequest {
        command,
        command_for_safety: None,
        execution_context: Some((&cwd, env)),
        approval_policy: ctx.turn.approval_policy.value(),
        permission_profile: profile,
        windows_sandbox_level: ctx.turn.windows_sandbox_level,
        sandbox_permissions,
        prefix_rule: None,
    };
    let policy = &ctx.session.services.exec_policy;
    let refreshed = if shell_wrapper_is_owned {
        policy.create_exec_approval_requirement_for_command(request).await
    } else {
        policy.create_exec_approval_requirement_for_direct_argv(request).await
    };
    match refreshed {
        ExecApprovalRequirement::Skip { bypass_sandbox, .. } => {
            if shell_wrapper_is_owned || bypass_sandbox {
                return Ok(None);
            }
        }
        _ => return Err(ToolError::Rejected(
            "Readonly executable provenance changed before launch; the requested command was not started.".into(),
        )),
    }
    let command = command.to_vec();
    let env = env.clone();
    let (safe, pinned) = crate::tools::run_blocking_command_analysis(move || {
        if shell_wrapper_is_owned {
            (
                codex_shell_command::is_safe_command::is_known_safe_command_in_state(
                    &command, &cwd, &env,
                ),
                None,
            )
        } else {
            let pinned = codex_shell_command::is_safe_command::proven_safe_direct_argv_in_state(
                &command, &cwd, &env,
            );
            (pinned.is_some(), pinned)
        }
    })
    .await
    .map_err(|error| {
        ToolError::Rejected(format!("Readonly executable provenance recheck failed: {error}"))
    })?;
    if !safe {
        return Err(ToolError::Rejected(
            "Readonly executable provenance changed or could not be proven before launch; the requested command was not started.".into(),
        ));
    }
    Ok(pinned)
}

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
    let command = maybe_wrap_shell_lc_with_snapshot_file(
        input.command,
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
    // Preserve native status when either inspectable source matches the
    // script suffix prepared for launch.
    [input.command_for_approval, input.command]
        .into_iter()
        .filter_map(|candidate| extract_powershell_command(candidate).map(|(_, script)| script))
        .find_map(|script| preserve_powershell_native_exit_code(&command, script))
        .unwrap_or(command)
}

/// Shared helper to construct sandbox transform inputs from a tokenized command line and selected
/// working directory. Validates that at least a program is present.
pub(crate) fn build_sandbox_command(
    command: &[String],
    cwd: PathUri,
    env: &HashMap<String, String>,
    additional_permissions: Option<AdditionalPermissionProfile>,
) -> Result<SandboxCommand, ToolError> {
    let (program, args) = command
        .split_first()
        .ok_or_else(|| ToolError::Rejected("command args are empty".to_string()))?;
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

/// Apply the matching Cmd snapshot's environment while preserving explicit,
/// runtime-owned and transport overrides. Snapshot construction is disabled for
/// PowerShell; compatibility-only non-Windows shell variants stay unchanged.
pub(crate) fn maybe_wrap_shell_lc_with_snapshot_file(
    command: &[String],
    session_shell: &Shell,
    shell_snapshot: Option<&ShellSnapshotFile>,
    explicit_env_overrides: &HashMap<String, String>,
    env: &mut HashMap<String, String>,
) -> Vec<String> {
    let Some(snapshot) = shell_snapshot else {
        return command.to_vec();
    };
    if session_shell.shell_type != ShellType::Cmd {
        return command.to_vec();
    }
    maybe_wrap_cmd_with_snapshot(
        command,
        session_shell,
        snapshot.contents(),
        explicit_env_overrides,
        env,
    )
}

fn maybe_wrap_cmd_with_snapshot(
    command: &[String],
    session_shell: &Shell,
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



