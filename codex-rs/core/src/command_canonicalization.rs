use codex_shell_command::bash::extract_bash_command;
use codex_shell_command::bash::parse_shell_lc_plain_commands;
use codex_shell_command::powershell::extract_noprofile_powershell_command;
use codex_shell_command::powershell::extract_powershell_command;
use codex_shell_command::powershell::is_trusted_powershell_executable;

const CANONICAL_BASH_SCRIPT_PREFIX: &str = "__codex_shell_script__";
const CANONICAL_POWERSHELL_SCRIPT_PREFIX: &str = "__codex_powershell_script__";
const POWERSHELL_NO_PROFILE_MODE: &str = "no-profile";
const POWERSHELL_PROFILES_ENABLED_MODE: &str = "profiles-enabled";

/// Canonicalize command argv for approval-cache matching.
///
/// Plain Bash-like scripts receive stable tokenization, while the exact shell
/// executable and mode remain part of the key. Complex scripts preserve their
/// exact script text as well.
pub(crate) fn canonicalize_command_for_approval(command: &[String]) -> Vec<String> {
    if let Some((shell, script)) = extract_bash_command(command) {
        let shell_mode = command.get(1).cloned().unwrap_or_default();
        let mut canonical = vec![
            CANONICAL_BASH_SCRIPT_PREFIX.to_string(),
            shell.to_string(),
            shell_mode,
        ];
        if let Some(commands) = parse_shell_lc_plain_commands(command)
            && let [single_command] = commands.as_slice()
        {
            canonical.extend(single_command.iter().cloned());
        } else {
            canonical.push(script.to_string());
        }
        return canonical;
    }

    if let Some((shell, script)) = extract_powershell_command(command)
        && is_trusted_powershell_executable(shell)
    {
        let profile_mode = if extract_noprofile_powershell_command(command).is_some() {
            POWERSHELL_NO_PROFILE_MODE
        } else {
            POWERSHELL_PROFILES_ENABLED_MODE
        };
        let mut canonical = vec![
            CANONICAL_POWERSHELL_SCRIPT_PREFIX.to_string(),
            shell.to_string(),
            profile_mode.to_string(),
        ];
        // Extraction admits only one final script argument, either separate
        // from -Command (including its aliases) or attached with a colon.
        let script_index = command.len() - 1;
        let flags_end = if command[script_index].len() == script.len() {
            script_index - 1
        } else {
            script_index
        };
        // Keep the host and every accepted execution flag. Only case and the
        // command introducer are equivalent syntax; do not sort flags because
        // conflicting apartment flags can depend on their order.
        canonical.extend(
            command[1..flags_end]
                .iter()
                .map(|flag| flag.to_ascii_lowercase())
                .filter(|flag| flag != "-noprofile"),
        );
        canonical.push(script.to_string());
        return canonical;
    }

    command.to_vec()
}

#[cfg(test)]
#[path = "command_canonicalization_tests.rs"]
mod tests;
