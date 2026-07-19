use std::path::PathBuf;

use codex_utils_absolute_path::AbsolutePathBuf;

use crate::command_safety::try_parse_powershell_ast_commands;
use crate::shell_detect::ShellType;
use crate::shell_detect::detect_shell_type;

/// Prefixed command for powershell shell calls to request UTF-8 console output.
pub const UTF8_OUTPUT_PREFIX: &str =
    "try { [Console]::OutputEncoding=[System.Text.Encoding]::UTF8 } catch {}\n";

pub fn prefix_powershell_script_with_utf8(command: &[String]) -> Vec<String> {
    let Some(extracted) = extract_powershell_command_details(command) else {
        return command.to_vec();
    };

    let trimmed = extracted.script.trim_start();
    let script = if trimmed.starts_with(UTF8_OUTPUT_PREFIX) {
        extracted.script.to_string()
    } else {
        format!("{UTF8_OUTPUT_PREFIX}{}", extracted.script)
    };

    let mut command = command.to_vec();
    command[extracted.script_index] = script;
    command
}

struct ExtractedPowershellCommand<'a> {
    shell: &'a str,
    script: &'a str,
    script_index: usize,
    no_profile: bool,
}

/// Extract the PowerShell script body from an invocation such as:
///
/// - ["pwsh", "-NoProfile", "-Command", "Get-ChildItem -Recurse | Select-String foo"]
/// - ["powershell.exe", "-Command", "Write-Host hi"]
/// - ["powershell", "-NoLogo", "-NoProfile", "-Command", "...script..."]
///
/// Returns (`shell`, `script`) when the first arg is a PowerShell executable and a
/// `-Command` (or `-c`) flag is present followed by a script string.
pub fn extract_powershell_command(command: &[String]) -> Option<(&str, &str)> {
    let extracted = extract_powershell_command_details(command)?;
    Some((extracted.shell, extracted.script))
}

/// Extract a PowerShell script only when profiles are disabled and the requested wrapper resolves
/// to the trusted host.
///
/// This is intended for user-facing summaries that would otherwise hide the executable that will
/// actually run. Syntax-only consumers should use [`extract_powershell_command`] instead.
pub(crate) fn extract_trusted_noprofile_powershell_command(
    command: &[String],
) -> Option<(&str, &str)> {
    let (shell, script) = extract_noprofile_powershell_command(command)?;
    is_trusted_powershell_executable(shell).then_some((shell, script))
}

/// Extract an exact-shape PowerShell command only when profiles are explicitly disabled.
pub fn extract_noprofile_powershell_command(command: &[String]) -> Option<(&str, &str)> {
    let extracted = extract_powershell_command_details(command)?;
    extracted
        .no_profile
        .then_some((extracted.shell, extracted.script))
}

/// Return whether this executable resolves to the independently selected trusted PowerShell host.
pub fn is_trusted_powershell_executable(executable: &str) -> bool {
    crate::command_safety::is_trusted_powershell_host(executable)
}

fn extract_powershell_command_details(
    command: &[String],
) -> Option<ExtractedPowershellCommand<'_>> {
    if command.len() < 3 {
        return None;
    }

    let shell = &command[0];
    if !matches!(
        detect_shell_type(PathBuf::from(shell)),
        Some(ShellType::PowerShell)
    ) {
        return None;
    }

    let mut no_profile = false;
    let mut i = 1usize;
    while i < command.len() {
        let flag = &command[i];
        match flag.to_ascii_lowercase().as_str() {
            "-nologo" => i += 1,
            "-noprofile" => {
                no_profile = true;
                i += 1;
            }
            "-command" | "-c" => {
                let script_index = i + 1;
                if script_index + 1 != command.len() {
                    return None;
                }
                return Some(ExtractedPowershellCommand {
                    shell,
                    script: &command[script_index],
                    script_index,
                    no_profile,
                });
            }
            _ => return None,
        }
    }
    None
}

/// Parse the script body from a top-level PowerShell wrapper into argv-like commands.
///
/// This exact-shape parser is used by non-approval consumers such as command preflight. Approval
/// and execution-policy decisions must use the `-NoProfile` variant below.
pub fn parse_powershell_command_into_plain_commands(
    command: &[String],
) -> Option<Vec<Vec<String>>> {
    let extracted = extract_powershell_command_details(command)?;
    try_parse_powershell_ast_commands(extracted.shell, extracted.script)
}

/// Parse an exact-shape PowerShell command only when profiles are disabled, as required by
/// approval and execution-policy decisions that depend on the parsed command being equivalent to
/// the command that will actually run.
pub fn parse_noprofile_powershell_command_into_plain_commands(
    command: &[String],
) -> Option<Vec<Vec<String>>> {
    let (shell, script) = extract_noprofile_powershell_command(command)?;
    if !is_trusted_powershell_executable(shell) {
        return None;
    }
    try_parse_powershell_ast_commands(shell, script)
}

/// This function attempts to find a powershell.exe executable on the system.
pub fn try_find_powershell_executable_blocking() -> Option<AbsolutePathBuf> {
    try_find_powershellish_executable_in_path(&["powershell.exe"])
}

/// This function attempts to find a pwsh.exe executable on the system.
/// Note that pwsh.exe and powershell.exe are different executables:
///
/// - pwsh.exe is the cross-platform PowerShell Core (v6+) executable
/// - powershell.exe is the Windows PowerShell (v5.1 and earlier) executable
///
/// Further, while powershell.exe is included by default on Windows systems,
/// pwsh.exe must be installed separately by the user. And even when the user
/// has installed pwsh.exe, it may not be available in the system PATH, in which
/// case we attempt to locate it via other means.
pub fn try_find_pwsh_executable_blocking() -> Option<AbsolutePathBuf> {
    if let Some(ps_home) = std::process::Command::new("cmd")
        .args(["/C", "pwsh", "-NoProfile", "-Command", "$PSHOME"])
        .output()
        .ok()
        .and_then(|out| {
            if !out.status.success() {
                return None;
            }
            let stdout = String::from_utf8_lossy(&out.stdout);
            let trimmed = stdout.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        })
    {
        let candidate = AbsolutePathBuf::resolve_path_against_base("pwsh.exe", &ps_home);

        if is_powershellish_executable_available(candidate.as_path()) {
            return Some(candidate);
        }
    }

    try_find_powershellish_executable_in_path(&["pwsh.exe"])
}

fn try_find_powershellish_executable_in_path(candidates: &[&str]) -> Option<AbsolutePathBuf> {
    for candidate in candidates {
        let Ok(resolved_path) = which::which(candidate) else {
            continue;
        };

        if !is_powershellish_executable_available(&resolved_path) {
            continue;
        }

        let Ok(abs_path) = AbsolutePathBuf::from_absolute_path(resolved_path) else {
            continue;
        };

        return Some(abs_path);
    }

    None
}

fn is_powershellish_executable_available(powershell_or_pwsh_exe: &std::path::Path) -> bool {
    // This test works for both powershell.exe and pwsh.exe.
    std::process::Command::new(powershell_or_pwsh_exe)
        .args(["-NoLogo", "-NoProfile", "-Command", "Write-Output ok"])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::UTF8_OUTPUT_PREFIX;
    use super::extract_powershell_command;
    #[cfg(windows)]
    use super::parse_powershell_command_into_plain_commands;
    use super::prefix_powershell_script_with_utf8;

    #[test]
    fn extracts_basic_powershell_command() {
        let cmd = vec![
            "powershell".to_string(),
            "-Command".to_string(),
            "Write-Host hi".to_string(),
        ];
        let (_shell, script) = extract_powershell_command(&cmd).expect("extract");
        assert_eq!(script, "Write-Host hi");
    }

    #[test]
    fn extracts_lowercase_flags() {
        let cmd = vec![
            "powershell".to_string(),
            "-nologo".to_string(),
            "-command".to_string(),
            "Write-Host hi".to_string(),
        ];
        let (_shell, script) = extract_powershell_command(&cmd).expect("extract");
        assert_eq!(script, "Write-Host hi");
    }

    #[test]
    fn extracts_full_path_powershell_command() {
        let command = if cfg!(windows) {
            "C:\\windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe".to_string()
        } else {
            "/usr/local/bin/powershell.exe".to_string()
        };
        let cmd = vec![command, "-Command".to_string(), "Write-Host hi".to_string()];
        let (_shell, script) = extract_powershell_command(&cmd).expect("extract");
        assert_eq!(script, "Write-Host hi");
    }

    #[test]
    fn extracts_with_noprofile_and_alias() {
        let cmd = vec![
            "pwsh".to_string(),
            "-NoProfile".to_string(),
            "-c".to_string(),
            "Get-ChildItem | Select-String foo".to_string(),
        ];
        let (_shell, script) = extract_powershell_command(&cmd).expect("extract");
        assert_eq!(script, "Get-ChildItem | Select-String foo");
    }

    #[test]
    fn prefixes_powershell_command_with_best_effort_utf8() {
        let cmd = vec![
            "powershell".to_string(),
            "-Command".to_string(),
            "Write-Host hi".to_string(),
        ];

        let prefixed = prefix_powershell_script_with_utf8(&cmd);

        assert_eq!(
            prefixed,
            vec![
                "powershell".to_string(),
                "-Command".to_string(),
                format!("{UTF8_OUTPUT_PREFIX}Write-Host hi"),
            ]
        );
    }

    #[test]
    fn does_not_duplicate_utf8_prefix() {
        let cmd = vec![
            "powershell".to_string(),
            "-Command".to_string(),
            format!("{UTF8_OUTPUT_PREFIX}Write-Host hi"),
        ];

        assert_eq!(prefix_powershell_script_with_utf8(&cmd), cmd);
    }

    #[test]
    fn rejects_and_does_not_rewrite_trailing_powershell_arguments() {
        let cmd = vec![
            "powershell".to_string(),
            "-Command".to_string(),
            "Write-Host hi".to_string(),
            "unexpected".to_string(),
        ];

        assert_eq!(extract_powershell_command(&cmd), None);
        assert_eq!(prefix_powershell_script_with_utf8(&cmd), cmd);
    }

    #[cfg(windows)]
    #[test]
    fn parses_plain_powershell_commands() {
        let commands = parse_powershell_command_into_plain_commands(&[
            "powershell.exe".to_string(),
            "-NoProfile".to_string(),
            "-Command".to_string(),
            "echo hi".to_string(),
        ])
        .expect("parse");

        assert_eq!(commands, vec![vec!["echo".to_string(), "hi".to_string()]]);
    }

    #[cfg(windows)]
    #[test]
    fn parses_multiple_plain_powershell_commands() {
        let commands = parse_powershell_command_into_plain_commands(&[
            "powershell.exe".to_string(),
            "-NoProfile".to_string(),
            "-Command".to_string(),
            "Write-Output foo | Measure-Object".to_string(),
        ])
        .expect("parse");

        assert_eq!(
            commands,
            vec![
                vec!["Write-Output".to_string(), "foo".to_string()],
                vec!["Measure-Object".to_string()],
            ]
        );
    }
}
