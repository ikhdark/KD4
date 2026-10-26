use crate::command_safety::is_safe_command::is_exact_version_probe;
use crate::command_safety::is_safe_command::is_safe_git_command;
use crate::command_safety::is_safe_command::is_safe_ripgrep;
use crate::command_safety::is_trusted_powershell_host;
use crate::command_safety::powershell_parser::PowershellInvocation;
use crate::command_safety::powershell_parser::PowershellParseAnalysis;
use crate::command_safety::powershell_parser::PowershellParseOutcome;
use crate::command_safety::powershell_parser::parse_powershell_invocation;
use crate::command_safety::powershell_parser::parse_with_powershell_ast;

/// On Windows, we conservatively allow only clearly read-only PowerShell invocations
/// that match a small safelist. Anything else (including direct CMD commands) is unsafe.
pub fn is_safe_command_windows(command: &[String]) -> bool {
    if let Some(commands) = try_parse_powershell_command_sequence(command) {
        commands
            .iter()
            .all(|cmd| is_safe_powershell_words(cmd.as_slice()))
    } else {
        // Only PowerShell invocations are allowed on Windows for now; anything else is unsafe.
        false
    }
}

/// Returns each command sequence if the invocation starts with the trusted PowerShell host and
/// explicitly disables profiles. For example,
/// `pwsh -NoProfile -Command "Get-ChildItem | Measure-Object"` becomes two command sequences.
fn try_parse_powershell_command_sequence(command: &[String]) -> Option<Vec<Vec<String>>> {
    let (exe, rest) = command.split_first()?;
    let PowershellInvocation::InlineCommand {
        script,
        no_profile: true,
        ..
    } = parse_powershell_invocation(rest)
    else {
        return None;
    };
    if !is_trusted_powershell_host(exe) {
        return None;
    }
    parse_powershell_script(exe, script)
}

/// Tokenizes an inline PowerShell script and delegates to the command splitter.
/// Examples of when this is called: pwsh.exe -Command '<script>' or pwsh.exe -Command:<script>
fn parse_powershell_script(executable: &str, script: &str) -> Option<Vec<Vec<String>>> {
    if let PowershellParseOutcome::Analysis(PowershellParseAnalysis { commands, .. }) =
        parse_with_powershell_ast(executable, script)
    {
        Some(commands)
    } else {
        None
    }
}

/// Validates that a parsed PowerShell command stays within our read-only safelist.
/// The words must come from the restricted AST parser; argument text is literal data.
pub(crate) fn is_safe_powershell_words(words: &[String]) -> bool {
    if words.is_empty() {
        // Examples rejected here: "pwsh -Command ''" and "pwsh -Command \"\"".
        return false;
    }

    if is_exact_version_probe(words) {
        return true;
    }

    let command = words[0]
        .trim_matches(|c| c == '(' || c == ')')
        .trim_start_matches('-')
        .to_ascii_lowercase();
    match command.as_str() {
        "echo" | "write-output" | "write-host" => true, // (no redirection allowed)
        "dir" | "ls" | "get-childitem" | "gci" => true,
        "cat" | "type" | "gc" | "get-content" => true,
        "select-string" | "sls" | "findstr" => true,
        "measure-object" | "measure" => true,
        "get-location" | "gl" | "pwd" => true,
        "test-path" | "tp" => true,
        "resolve-path" | "rvpa" => true,
        "select-object" | "select" => true,
        "get-item" => true,
        // Exact-name discovery can import modules. These semantic words do not
        // preserve enough parameter-binding syntax to prove -ListImported is enabled.
        "get-command" | "gcm" => false,
        "get-itemproperty" | "gp" => true,

        "git" => is_safe_git_command(words),

        "rg" => is_safe_ripgrep(words),

        // Extra safety: explicitly prohibit common side-effecting cmdlets regardless of args.
        "set-content" | "add-content" | "out-file" | "new-item" | "remove-item" | "move-item"
        | "copy-item" | "rename-item" | "start-process" | "stop-process" => {
            // Examples rejected here: "pwsh -Command 'Set-Content notes.txt data'" and "pwsh -Command 'Remove-Item temp.log'".
            false
        }

        _ => {
            // Examples rejected here: "pwsh -Command 'Invoke-WebRequest https://example.com'" and "pwsh -Command 'Start-Service Spooler'".
            false
        }
    }
}

