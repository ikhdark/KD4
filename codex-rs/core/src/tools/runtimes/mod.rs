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
use crate::shell_snapshot::POSIX_SNAPSHOT_FORMAT_HEADER;
use crate::shell_snapshot::POWERSHELL_SNAPSHOT_FORMAT_HEADER;
use crate::tools::sandboxing::ToolError;
#[cfg(unix)]
use codex_install_context::InstallContext;
#[cfg(target_os = "macos")]
use codex_network_proxy::CODEX_PROXY_GIT_SSH_COMMAND_MARKER;
use codex_network_proxy::CUSTOM_CA_ENV_KEYS;
use codex_network_proxy::PROXY_ACTIVE_ENV_KEY;
use codex_network_proxy::PROXY_ENV_KEYS;
#[cfg(target_os = "macos")]
use codex_network_proxy::PROXY_GIT_SSH_COMMAND_ENV_KEY;
use codex_network_proxy::is_managed_mitm_ca_trust_bundle_path;
use codex_otel::MetricsClient;
use codex_protocol::config_types::WindowsSandboxLevel;
use codex_protocol::models::AdditionalPermissionProfile;
use codex_sandboxing::SandboxCommand;
use codex_sandboxing::SandboxType;
use codex_shell_command::powershell::extract_powershell_command;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use std::collections::HashMap;
#[cfg(unix)]
use std::path::Path;

const SHELL_SNAPSHOT_REPLAY_METRIC: &str = "codex.shell_snapshot_replay";

pub(crate) mod apply_patch;
pub(crate) mod shell;
pub(crate) mod unified_exec;

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
    #[cfg(target_os = "macos")]
    {
        key == PROXY_GIT_SSH_COMMAND_ENV_KEY
            && value.starts_with(CODEX_PROXY_GIT_SSH_COMMAND_MARKER)
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

pub(crate) fn strip_managed_proxy_env(env: &mut HashMap<String, String>) {
    env.retain(|key, value| !is_managed_proxy_env_var(key, value));
}

/// Prepends `path_entry` to `PATH`, removing duplicate and empty existing
/// entries.
///
/// Returns the updated `PATH` value when `env` was changed. Returns `None` when
/// `path_entry` is empty, leaving `env` untouched so an empty entry does not add
/// the current working directory to command lookup.
#[cfg(unix)]
fn prepend_path_entry(env: &mut HashMap<String, String>, path_entry: &str) -> Option<String> {
    if path_entry.is_empty() {
        None
    } else {
        let updated_path = match env.get("PATH") {
            Some(path) if !path.is_empty() => std::iter::once(path_entry)
                .chain(
                    path.split(':')
                        .filter(|entry| !entry.is_empty() && *entry != path_entry),
                )
                .collect::<Vec<_>>()
                .join(":"),
            _ => path_entry.to_string(),
        };
        env.insert("PATH".to_string(), updated_path.clone());
        Some(updated_path)
    }
}

/// PATH entries owned by Codex runtime setup.
///
/// These are applied to the live exec environment immediately and replayed after
/// restoring a shell snapshot, unless the user explicitly overrides `PATH`.
#[derive(Debug, Default, Eq, PartialEq)]
pub(crate) struct RuntimePathPrepends {
    entries: Vec<String>,
}

impl RuntimePathPrepends {
    #[cfg(unix)]
    pub(crate) fn prepend(&mut self, env: &mut HashMap<String, String>, path_entry: &Path) {
        let path_entry = path_entry.to_string_lossy().to_string();
        if prepend_path_entry(env, &path_entry).is_some() {
            self.entries.retain(|entry| entry != &path_entry);
            self.entries.push(path_entry);
        }
    }

    fn shell_exports_after_snapshot(
        &self,
        explicit_env_overrides: &HashMap<String, String>,
    ) -> String {
        if explicit_env_overrides.contains_key("PATH") {
            return String::new();
        }

        self.entries
            .iter()
            .filter(|entry| !entry.is_empty())
            .map(|entry| {
                let entry = shell_single_quote(entry);
                format!(
                    "if \\command [ -n \"${{PATH:-}}\" ]; then \\command export PATH='{entry}':\"$PATH\"; else \\command export PATH='{entry}'; fi"
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[cfg(unix)]
pub(crate) fn apply_package_path_prepend(
    env: &mut HashMap<String, String>,
    runtime_path_prepends: &mut RuntimePathPrepends,
) {
    let Some(path_dir) = InstallContext::current()
        .package_layout
        .as_ref()
        .and_then(|package_layout| package_layout.path_dir.as_ref())
    else {
        return;
    };

    runtime_path_prepends.prepend(env, path_dir.as_path());
}

#[cfg(unix)]
pub(crate) fn prepend_zsh_fork_bin_to_path(
    env: &mut HashMap<String, String>,
    shell_zsh_path: &Path,
) -> Option<String> {
    let zsh_bin_dir = shell_zsh_path
        .parent()
        .map(|path| path.to_string_lossy().to_string())?;
    prepend_path_entry(env, &zsh_bin_dir)
}

#[cfg(unix)]
pub(crate) fn apply_zsh_fork_path_prepend(
    env: &mut HashMap<String, String>,
    runtime_path_prepends: &mut RuntimePathPrepends,
    shell_zsh_path: &Path,
) {
    let Some(zsh_bin_dir) = shell_zsh_path.parent() else {
        return;
    };
    runtime_path_prepends.prepend(env, zsh_bin_dir);
}

pub(crate) fn disable_powershell_profile_for_elevated_windows_sandbox(
    command: &[String],
    shell_type: Option<&ShellType>,
    sandbox: SandboxType,
    windows_sandbox_level: WindowsSandboxLevel,
) -> Vec<String> {
    if shell_type != Some(&ShellType::PowerShell)
        || sandbox != SandboxType::WindowsRestrictedToken
        || windows_sandbox_level != WindowsSandboxLevel::Elevated
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
///   shell -lc "<script>"
///   => user_shell <clean-startup flags> -lc ". SNAPSHOT (best effort); eval <script>"
///
/// Bash/Zsh/sh use a POSIX wrapper. PowerShell uses `-NoProfile` and a native
/// PowerShell wrapper. Cmd remains unsupported. A non-matching command is left
/// unchanged because shell-local snapshot state can only be reused safely by
/// the matching executable. Cwd mismatch is filtered by the caller before a
/// snapshot path reaches this helper.
///
/// `explicit_env_overrides` and `env` are intentionally separate inputs.
/// `explicit_env_overrides` contains policy-driven shell env overrides that
/// should win after the snapshot is sourced, while `env` is the full live exec
/// environment. We need access to both so snapshot restore logic can preserve
/// runtime-only vars like `CODEX_THREAD_ID` without pretending they came from
/// the explicit override policy.
///
/// `runtime_path_prepends` contains Codex-owned PATH entries already applied to
/// the live `env`; snapshot wrapping replays them after restoring the snapshot
/// PATH unless the user explicitly overrides `PATH`.
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

    match session_shell.shell_type {
        ShellType::PowerShell => {
            return maybe_wrap_powershell_with_snapshot(
                command,
                session_shell,
                snapshot,
                &snapshot_contents,
                explicit_env_overrides,
                env,
                metrics,
            );
        }
        ShellType::Cmd => return command.to_vec(),
        ShellType::Bash | ShellType::Zsh | ShellType::Sh => {}
    }

    if command.len() < 3 || command[1] != "-lc" {
        return command.to_vec();
    }

    let shell_path = session_shell.shell_path.to_string_lossy();
    let reuse_initialized_shell = command[0].as_str() == shell_path.as_ref();
    if !reuse_initialized_shell {
        return command.to_vec();
    }
    let has_functions_section = snapshot_contents
        .lines()
        .any(|line| line.starts_with("# Functions"));
    let has_supported_snapshot_format = snapshot_contents
        .lines()
        .any(|line| line == POSIX_SNAPSHOT_FORMAT_HEADER);
    if has_functions_section && !has_supported_snapshot_format {
        // Older formats can execute functions inline or replay Bash declaration
        // attributes that conflict with live policy restoration. Let the original
        // login command rebuild state instead.
        return command.to_vec();
    }
    let original_shell = shell_single_quote(&command[0]);
    let original_script = shell_single_quote(&command[2]);
    let snapshot_path = snapshot.to_string_lossy();
    let snapshot_path = shell_single_quote(snapshot_path.as_ref());
    let trailing_args = command[3..]
        .iter()
        .map(|arg| format!(" '{}'", shell_single_quote(arg)))
        .collect::<String>();
    let mut override_env = explicit_env_overrides.clone();
    for key in [CODEX_THREAD_ID_ENV_VAR, CODEX_PERMISSION_PROFILE_ENV_VAR] {
        if let Some(value) = env.get(key) {
            override_env.insert(key.to_string(), value.clone());
        }
    }
    // Do not let a snapshot resurrect a stale profile when no named profile is active.
    let (override_captures, override_exports) =
        build_override_exports(&override_env, &[CODEX_PERMISSION_PROFILE_ENV_VAR]);
    let (proxy_captures, proxy_exports) = build_proxy_env_exports();
    let runtime_path_prepend_exports =
        runtime_path_prepends.shell_exports_after_snapshot(explicit_env_overrides);
    let override_captures = join_shell_blocks([override_captures, proxy_captures]);
    let override_exports = join_shell_blocks([
        override_exports,
        proxy_exports,
        runtime_path_prepend_exports,
    ]);
    let activate_snapshot_state = "\\command eval '\\command unset __CODEX_SNAPSHOT_FUNCTIONS __CODEX_SNAPSHOT_ALIASES __CODEX_SNAPSHOT_BASH_ENV_PRESENT\n'\"${__CODEX_SNAPSHOT_FUNCTIONS-}\"'\n'\"${__CODEX_SNAPSHOT_ALIASES-}\"";
    let command_invocation =
        format!("{activate_snapshot_state} &&\n\\command eval '{original_script}'");
    let post_snapshot_restore = if session_shell.shell_type == ShellType::Bash {
        "case \"${__CODEX_SNAPSHOT_BASH_ENV_PRESENT-0}\" in\n  1) ;;\n  *) \\command unset BASH_ENV ;;\nesac"
    } else {
        ""
    };
    let rewritten_body = if override_exports.is_empty() {
        format!(
            "\\command . '{snapshot_path}' >/dev/null 2>&1 || \\command true\n\n{post_snapshot_restore}\n\n{command_invocation}"
        )
    } else {
        format!(
            "{override_captures}\n\n\\command . '{snapshot_path}' >/dev/null 2>&1 || \\command true\n\n{post_snapshot_restore}\n\n{override_exports}\n\n{command_invocation}"
        )
    };
    // Parse Codex's control block with aliases disabled. Functions and aliases
    // stay encoded as data until the activation eval, whose first command removes
    // the transport variables before defining the captured functions and aliases.
    // The trusted second eval then parses the user script after aliases are live;
    // `command`/`builtin` functions are rejected during capture, so captured
    // dispatcher functions cannot intercept either boundary.
    // Privileged startup makes Bash ignore BASH_ENV and imported functions while
    // preserving the caller's BASH_ENV value for the override capture below.
    // Leave privileged mode before replaying any snapshot state or user code.
    let shell_bootstrap = if session_shell.shell_type == ShellType::Bash {
        "\\command set +p\n"
    } else {
        ""
    };
    let zsh_option_baseline = if session_shell.shell_type == ShellType::Zsh {
        "\\command setopt RCS\n"
    } else {
        ""
    };
    let control_script = format!(
        "{shell_bootstrap}\\command unalias -a 2>/dev/null || \\command true\n{{\n{zsh_option_baseline}{rewritten_body}\n}}"
    );
    let rewritten_script = if session_shell.shell_type == ShellType::Bash {
        control_script
    } else if session_shell.shell_type == ShellType::Zsh {
        let fallback_invocation =
            format!("'{original_shell}' -lc '{original_script}'{trailing_args}");
        format!(
            "if [[ \"${{(t)functions}}\" == association* && -z \"${{functions[builtin]-}}\" && -z \"${{functions[command]-}}\" ]]; then\n{control_script}\nelse\n  {fallback_invocation}\nfi"
        )
    } else {
        let fallback_invocation =
            format!("'{original_shell}' -lc '{original_script}'{trailing_args}");
        format!(
            "\\unset -f command 2>/dev/null\ncase \"$(\\command printf '%s' __CODEX_SNAPSHOT_COMMAND_OK)\" in\n  __CODEX_SNAPSHOT_COMMAND_OK)\n{control_script}\n    ;;\n  *)\n    {fallback_invocation}\n    ;;\nesac"
        )
    };

    let mut rewritten = match session_shell.shell_type {
        ShellType::Bash => vec![
            "/usr/bin/env".to_string(),
            "-u".to_string(),
            "BASH_FUNC_command%%".to_string(),
            "-u".to_string(),
            "BASH_FUNC_builtin%%".to_string(),
            shell_path.to_string(),
            "--noprofile".to_string(),
            "--norc".to_string(),
            "-p".to_string(),
            "-lc".to_string(),
            rewritten_script,
        ],
        ShellType::Zsh => vec![
            shell_path.to_string(),
            "-f".to_string(),
            "-lc".to_string(),
            rewritten_script,
        ],
        ShellType::Sh => vec![shell_path.to_string(), "-lc".to_string(), rewritten_script],
        ShellType::PowerShell | ShellType::Cmd => {
            unreachable!("non-POSIX shells return before POSIX snapshot wrapping")
        }
    };
    rewritten.extend(command[3..].iter().cloned());
    rewritten
}

fn maybe_wrap_powershell_with_snapshot(
    command: &[String],
    session_shell: &Shell,
    snapshot: &AbsolutePathBuf,
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

    let Some((command_shell, original_script)) = extract_powershell_command(command) else {
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

    let snapshot_path = powershell_single_quote(&snapshot.to_string_lossy());
    let rewritten_script = format!(
        "try {{ . '{snapshot_path}' *> $null }} catch {{}}\n{override_restores}\n{proxy_restore}\n& {{\n{original_script}\n}}"
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

fn powershell_single_quote(input: &str) -> String {
    input.replace('\'', "''")
}

fn build_override_exports(
    explicit_env_overrides: &HashMap<String, String>,
    restore_even_when_absent: &[&str],
) -> (String, String) {
    let mut keys = explicit_env_overrides
        .keys()
        .map(String::as_str)
        .chain(restore_even_when_absent.iter().copied())
        .filter(|key| is_valid_shell_variable_name(key))
        .collect::<Vec<_>>();
    keys.sort_unstable();
    keys.dedup();

    build_override_exports_for_keys("__CODEX_SNAPSHOT_OVERRIDE", &keys)
}

fn build_proxy_env_exports() -> (String, String) {
    let mut keys = PROXY_ENV_KEYS
        .iter()
        .copied()
        .chain(CUSTOM_CA_ENV_KEYS)
        .filter(|key| is_valid_shell_variable_name(key))
        .collect::<Vec<_>>();
    keys.sort_unstable();
    keys.dedup();

    let (captures, restores) =
        build_override_exports_for_keys("__CODEX_SNAPSHOT_PROXY_OVERRIDE", &keys);
    let key = PROXY_ACTIVE_ENV_KEY;
    let proxy_blocks = (
        format!("{captures}\n__CODEX_SNAPSHOT_PROXY_ENV_SET=\"${{{key}+x}}\""),
        format!(
            "if \\command [ -n \"$__CODEX_SNAPSHOT_PROXY_ENV_SET\" ] || \\command [ -n \"${{{key}+x}}\" ]; then\n{restores}\nfi"
        ),
    );
    let git_blocks = build_codex_proxy_git_ssh_command_exports();
    (
        join_shell_blocks([proxy_blocks.0, git_blocks.0]),
        join_shell_blocks([proxy_blocks.1, git_blocks.1]),
    )
}

#[cfg(target_os = "macos")]
fn build_codex_proxy_git_ssh_command_exports() -> (String, String) {
    let key = PROXY_GIT_SSH_COMMAND_ENV_KEY;
    let marker_pattern = format!("{}\\ *", CODEX_PROXY_GIT_SSH_COMMAND_MARKER.trim_end());
    (
        format!(
            "__CODEX_SNAPSHOT_PROXY_GIT_SSH_COMMAND_SET=\"${{{key}+x}}\"\n__CODEX_SNAPSHOT_PROXY_GIT_SSH_COMMAND=\"${{{key}-}}\"\ncase \"$__CODEX_SNAPSHOT_PROXY_GIT_SSH_COMMAND\" in\n  {marker_pattern}) __CODEX_SNAPSHOT_PROXY_GIT_SSH_COMMAND_LIVE_MARKED=1 ;;\n  *) __CODEX_SNAPSHOT_PROXY_GIT_SSH_COMMAND_LIVE_MARKED= ;;\nesac"
        ),
        format!(
            "case \"${{{key}-}}\" in\n  {marker_pattern}) __CODEX_SNAPSHOT_PROXY_GIT_SSH_COMMAND_AFTER_MARKED=1 ;;\n  *) __CODEX_SNAPSHOT_PROXY_GIT_SSH_COMMAND_AFTER_MARKED= ;;\nesac\nif \\command [ -n \"$__CODEX_SNAPSHOT_PROXY_GIT_SSH_COMMAND_LIVE_MARKED\" ]; then\n  if \\command [ -z \"${{{key}+x}}\" ] || \\command [ -n \"$__CODEX_SNAPSHOT_PROXY_GIT_SSH_COMMAND_AFTER_MARKED\" ]; then\n    \\command export {key}=\"$__CODEX_SNAPSHOT_PROXY_GIT_SSH_COMMAND\"\n  fi\nelif \\command [ -n \"$__CODEX_SNAPSHOT_PROXY_GIT_SSH_COMMAND_AFTER_MARKED\" ]; then\n  if \\command [ -n \"$__CODEX_SNAPSHOT_PROXY_GIT_SSH_COMMAND_SET\" ]; then\n    \\command export {key}=\"$__CODEX_SNAPSHOT_PROXY_GIT_SSH_COMMAND\"\n  else\n    \\command unset {key}\n  fi\nfi"
        ),
    )
}

#[cfg(not(target_os = "macos"))]
fn build_codex_proxy_git_ssh_command_exports() -> (String, String) {
    (String::new(), String::new())
}

fn build_override_exports_for_keys(variable_prefix: &str, keys: &[&str]) -> (String, String) {
    if keys.is_empty() {
        return (String::new(), String::new());
    }

    let captures = keys
        .iter()
        .enumerate()
        .map(|(idx, key)| {
            let set_var = format!("{variable_prefix}_SET_{idx}");
            let value_var = format!("{variable_prefix}_{idx}");
            format!("{set_var}=\"${{{key}+x}}\"\n{value_var}=\"${{{key}-}}\"")
        })
        .collect::<Vec<_>>()
        .join("\n");
    let restores = keys
        .iter()
        .enumerate()
        .map(|(idx, key)| {
            let set_var = format!("{variable_prefix}_SET_{idx}");
            let value_var = format!("{variable_prefix}_{idx}");
            format!(
                "if \\command [ -n \"${{{set_var}}}\" ]; then \\command export {key}=\"${{{value_var}}}\"; else \\command unset {key}; fi"
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    (captures, restores)
}

fn join_shell_blocks(blocks: impl IntoIterator<Item = String>) -> String {
    blocks
        .into_iter()
        .filter(|block| !block.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_valid_shell_variable_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first == '_' || first.is_ascii_alphabetic()) {
        return false;
    }
    chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

fn shell_single_quote(input: &str) -> String {
    input.replace('\'', r#"'"'"'"#)
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
        );

        assert_eq!(rewritten, command);
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
        );

        assert_eq!(rewritten, command);
    }
}

#[cfg(all(test, windows))]
mod powershell_snapshot_tests {
    use super::*;
    use codex_otel::MetricsConfig;
    use opentelemetry_sdk::metrics::InMemoryMetricExporter;
    use opentelemetry_sdk::metrics::data::AggregatedMetrics;
    use opentelemetry_sdk::metrics::data::MetricData;
    use pretty_assertions::assert_eq;
    use std::collections::BTreeSet;

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
        );
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
            &RuntimePathPrepends::default(),
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
        let command = shell.derive_exec_args("Write-Output ok", /*use_login_shell*/ true);
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
            &RuntimePathPrepends::default(),
            Some(&metrics),
        );
        assert_ne!(applied, command);
        let skipped = maybe_wrap_shell_lc_with_snapshot_and_metrics(
            &command,
            &shell,
            None,
            &HashMap::new(),
            &env,
            &RuntimePathPrepends::default(),
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

#[cfg(all(test, unix))]
#[path = "mod_tests.rs"]
mod tests;
