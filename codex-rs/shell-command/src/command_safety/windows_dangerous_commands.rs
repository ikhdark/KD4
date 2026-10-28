use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;
use shlex::split as shlex_split;
use url::Url;

use crate::command_safety::powershell_parser::PowershellInvocation;
use crate::command_safety::powershell_parser::parse_powershell_invocation;
use crate::command_safety::powershell_parser::try_parse_powershell_ast_commands;

pub fn is_dangerous_command_windows(command: &[String]) -> bool {
    // Prefer structured parsing for PowerShell/CMD so we can spot URL-bearing
    // invocations of ShellExecute-style entry points before falling back to
    // simple argv heuristics.
    if is_dangerous_powershell(command) {
        return true;
    }

    if is_dangerous_cmd(command) {
        return true;
    }

    is_direct_gui_launch(command)
}

/// A structured shell launch is opaque, not an owned wrapper. Require normal
/// hazardous-command approval without lowering its body or granting it authority.
pub(super) fn is_dangerous_direct_argv_windows(command: &[String]) -> bool {
    command.first().is_some_and(|exe| {
        is_powershell_executable(exe)
            || matches!(executable_basename(exe).as_deref(), Some("cmd" | "cmd.exe"))
    }) || is_direct_gui_launch(command)
}

fn is_dangerous_powershell(command: &[String]) -> bool {
    let Some((exe, rest)) = command.split_first() else {
        return false;
    };
    if !is_powershell_executable(exe) {
        return false;
    }
    // These forms have PowerShell semantics that POSIX shlex cannot preserve.
    // An unavailable bounded parse cannot establish harmlessness.
    match parse_powershell_invocation(rest) {
        PowershellInvocation::InlineCommand { script, .. }
            if script.contains(['`', '\\']) || script.contains("<#") => {
            try_parse_powershell_ast_commands(exe, script)
                .map(|commands| {
                    commands
                        .iter()
                        .any(|words| is_dangerous_powershell_words(words))
                })
                .unwrap_or(true)
        }
        // Preserve the established best-effort scan for unescaped scripts,
        // including nested commands that the restricted AST parser does not lower.
        PowershellInvocation::InlineCommand { script, .. } => shlex_split(script)
            .map(|tokens| is_dangerous_powershell_script_tokens(&tokens))
            .unwrap_or(true),
        PowershellInvocation::Opaque
        | PowershellInvocation::Bare
        | PowershellInvocation::Empty
        | PowershellInvocation::Invalid => true,
    }
}

/// Inspect one command already lowered by the restricted PowerShell AST parser.
pub(crate) fn is_dangerous_powershell_words(words: &[String]) -> bool {
    let Some((command, args)) = words.split_first() else {
        return false;
    };
    let command = command.to_ascii_lowercase();
    // Explicit executable spellings retain native option semantics. Bare `rm`
    // can be a PowerShell alias, so do not reinterpret its cmdlet parameters.
    if is_explicit_executable_spelling(&command) && super::is_dangerous_to_call_with_exec(words) {
        return true;
    }
    if matches!(
        command.as_str(),
        "start-process" | "start" | "saps" | "invoke-item" | "ii"
    ) && args_have_url(args)
    {
        return true;
    }
    if matches!(
        command.as_str(),
        "remove-item" | "ri" | "rm" | "del" | "erase" | "rd" | "rmdir"
    ) && args.iter().any(|arg| is_powershell_force_parameter(arg))
    {
        return true;
    }
    is_direct_gui_launch(words)
}

fn is_explicit_executable_spelling(command: &str) -> bool {
    command.contains(['/', '\\'])
        || [".exe", ".com", ".cmd", ".bat"].iter().any(|suffix| command.ends_with(suffix))
}

/// PowerShell binds any unambiguous parameter prefix (`-Fo`, `-forc`) and treats the en dash,
/// em dash, and horizontal bar as parameter dashes, so `-Force` cannot be matched literally.
fn is_powershell_force_parameter(token: &str) -> bool {
    let Some(name) = token.strip_prefix(['-', '\u{2013}', '\u{2014}', '\u{2015}']) else {
        return false;
    };
    let name = name.split_once(':').map_or(name, |(name, _)| name);
    // A bare `-F` is ambiguous with Remove-Item's `-Filter`, so PowerShell rejects it.
    name.len() >= 2 && "force".starts_with(name.to_ascii_lowercase().as_str())
}

fn is_dangerous_powershell_script_tokens(words: &[String]) -> bool {
    let tokens_lc: Vec<String> = words
        .iter()
        .map(|t| t.trim_matches('\'').trim_matches('"').to_ascii_lowercase())
        .collect();
    let has_url = args_have_url(words);

    if has_url
        && tokens_lc.iter().any(|t| {
            matches!(
                t.as_str(),
                "start-process" | "start" | "saps" | "invoke-item" | "ii"
            ) || t.contains("start-process")
                || t.contains("invoke-item")
        })
    {
        return true;
    }

    if has_url
        && tokens_lc
            .iter()
            .any(|t| t.contains("shellexecute") || t.contains("shell.application"))
    {
        return true;
    }

    if is_direct_gui_launch(&tokens_lc) {
        return true;
    }

    // Check for force delete operations (e.g., Remove-Item -Force)
    has_force_delete_cmdlet(&tokens_lc) || has_force_remove_executable(&tokens_lc)
}

/// Scripts the restricted AST parser cannot lower (try/catch, loops) reach this
/// scan, so apply the explicit-executable `rm` check per command segment too.
fn has_force_remove_executable(tokens: &[String]) -> bool {
    // Only `rm`, directly or through `sudo`, can match below; skip segmenting otherwise.
    if !tokens.iter().any(|token| token.contains("rm") || token.contains("sudo")) {
        return false;
    }
    fn force_removes(segment: &[String]) -> bool {
        segment.iter().enumerate().any(|(index, word)| {
            is_explicit_executable_spelling(word)
                && super::is_dangerous_to_call_with_exec(&segment[index..])
        })
    }
    let mut segment = Vec::new();
    for token in tokens {
        for (index, part) in token.split([';', '|', '&', '\n', '\r']).enumerate() {
            if index > 0 {
                if force_removes(&segment) {
                    return true;
                }
                segment.clear();
            }
            segment.extend(
                part.split(['{', '}', '(', ')', '[', ']', ','])
                    .map(str::trim)
                    .filter(|atom| !atom.is_empty())
                    .map(str::to_owned),
            );
        }
    }
    force_removes(&segment)
}

fn is_dangerous_cmd(command: &[String]) -> bool {
    let Some((exe, rest)) = command.split_first() else {
        return false;
    };
    let Some(base) = executable_basename(exe) else {
        return false;
    };
    if base != "cmd" && base != "cmd.exe" {
        return false;
    }

    let mut iter = rest.iter();
    for arg in iter.by_ref() {
        let lower = arg.to_ascii_lowercase();
        match lower.as_str() {
            // `/k` runs the command body like `/c` and then keeps the shell open.
            "/c" | "/r" | "/k" | "-c" => break,
            _ if lower.starts_with('/') => continue,
            // Unknown tokens before the command body => bail.
            _ => return false,
        }
    }

    let remaining: Vec<String> = iter.cloned().collect();
    if remaining.is_empty() {
        return false;
    }

    let Some(commands) = split_cmd_script(&remaining.join(" ")) else {
        return true;
    };
    commands.iter().any(|segment| {
        let Some(cmd) = segment.first() else {
            return false;
        };

        // Classic `cmd /c ... start https://...` ShellExecute path.
        if cmd.eq_ignore_ascii_case("start") && args_have_url(segment) {
            return true;
        }
        // Force delete: del /f, erase /f
        if (cmd.eq_ignore_ascii_case("del") || cmd.eq_ignore_ascii_case("erase"))
            && has_cmd_switch(segment, 'f')
        {
            return true;
        }
        // Recursive directory removal: rd /s /q, rmdir /s /q
        if (cmd.eq_ignore_ascii_case("rd") || cmd.eq_ignore_ascii_case("rmdir"))
            && has_cmd_switch(segment, 's')
            && has_cmd_switch(segment, 'q')
        {
            return true;
        }
        false
    })
}

fn is_direct_gui_launch(command: &[String]) -> bool {
    let Some((exe, rest)) = command.split_first() else {
        return false;
    };
    let Some(base) = executable_basename(exe) else {
        return false;
    };

    // Explorer/rundll32/mshta or direct browser exe with a URL anywhere in args.
    if matches!(base.as_str(), "explorer" | "explorer.exe") && args_have_url(rest) {
        return true;
    }
    if matches!(base.as_str(), "mshta" | "mshta.exe") && args_have_url(rest) {
        return true;
    }
    if (base == "rundll32" || base == "rundll32.exe")
        && rest.iter().any(|t| {
            t.to_ascii_lowercase()
                .contains("url.dll,fileprotocolhandler")
        })
        && args_have_url(rest)
    {
        return true;
    }
    if is_browser_executable(&base) && args_have_url(rest) {
        return true;
    }

    false
}

fn split_cmd_script(script: &str) -> Option<Vec<Vec<String>>> {
    // CMD groups with double quotes. Apostrophes and backslashes are literal;
    // only an unquoted caret escapes the next character.
    let mut commands = Vec::new();
    let mut words = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut quoted = false;
    let mut chars = script.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            '^' if !quoted => {
                word.push(chars.next()?);
                started = true;
            }
            '&' | '|' | '\r' | '\n' if !quoted => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
                if !words.is_empty() {
                    commands.push(std::mem::take(&mut words));
                }
                if chars.peek() == Some(&ch) {
                    chars.next();
                }
            }
            ch if ch.is_whitespace() && !quoted => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            _ => {
                word.push(ch);
                started = true;
            }
        }
    }
    if quoted {
        return None;
    }
    if started {
        words.push(word);
    }
    if !words.is_empty() {
        commands.push(words);
    }
    Some(commands)
}

fn has_force_delete_cmdlet(tokens: &[String]) -> bool {
    const DELETE_CMDLETS: &[&str] = &["remove-item", "ri", "rm", "del", "erase", "rd", "rmdir"];

    // Keep -Force in the same command segment as a delete cmdlet. Token
    // boundaries preserve that state; shell operators reset it.
    let mut has_delete = false;
    let mut has_force = false;
    for token in tokens {
        for (index, segment) in token.split([';', '|', '&', '\n', '\r']).enumerate() {
            if index > 0 {
                has_delete = false;
                has_force = false;
            }
            for atom in segment
                .split(['{', '}', '(', ')', '[', ']', ',', ';'])
                .map(str::trim)
                .filter(|atom| !atom.is_empty())
            {
                has_delete |= DELETE_CMDLETS
                    .iter()
                    .any(|cmd| atom.eq_ignore_ascii_case(cmd));
                has_force |= is_powershell_force_parameter(atom);
                if has_delete && has_force {
                    return true;
                }
            }
        }
    }
    false
}

/// Check a CMD builtin's switches case-insensitively. CMD accepts switches joined in one
/// token (`rd /s/q`, `del /f/q`) and switch values after a colon (`del /a:h`).
fn has_cmd_switch(args: &[impl AsRef<str>], switch: char) -> bool {
    args.iter()
        .map(|arg| arg.as_ref())
        .filter(|arg| arg.starts_with('/'))
        .flat_map(|arg| arg.split('/'))
        .filter_map(|group| group.chars().next())
        .any(|ch| ch.eq_ignore_ascii_case(&switch))
}

fn args_have_url(args: &[impl AsRef<str>]) -> bool {
    args.iter().any(|arg| looks_like_url(arg.as_ref()))
}

fn looks_like_url(token: &str) -> bool {
    // Strip common PowerShell punctuation around inline URLs (quotes, parens, trailing semicolons).
    // Capture the middle token after trimming leading quotes/parens/whitespace and trailing semicolons/closing parens.
    static RE: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r#"^[ "'\(\s]*([^\s"'\);]+)[\s;\)]*$"#).ok());
    // If the token embeds a URL alongside other text (e.g., Start-Process('https://...'))
    // as a single shlex token, grab the substring starting at the first URL prefix.
    let lowercase_token = token.to_ascii_lowercase();
    let urlish = lowercase_token
        .find("https://")
        .or_else(|| lowercase_token.find("http://"))
        .map(|idx| &token[idx..])
        .unwrap_or(token);

    let candidate = RE
        .as_ref()
        .and_then(|re| re.captures(urlish))
        .and_then(|caps| caps.get(1))
        .map(|m| m.as_str())
        .unwrap_or(urlish);
    let Ok(url) = Url::parse(candidate) else {
        return false;
    };
    matches!(url.scheme(), "http" | "https")
}

fn executable_basename(exe: &str) -> Option<String> {
    Path::new(exe)
        .file_name()
        .and_then(|osstr| osstr.to_str())
        .map(str::to_ascii_lowercase)
}

fn is_powershell_executable(exe: &str) -> bool {
    matches!(
        executable_basename(exe).as_deref(),
        Some("powershell") | Some("powershell.exe") | Some("pwsh") | Some("pwsh.exe")
    )
}

fn is_browser_executable(name: &str) -> bool {
    matches!(
        name,
        "chrome"
            | "chrome.exe"
            | "msedge"
            | "msedge.exe"
            | "firefox"
            | "firefox.exe"
            | "iexplore"
            | "iexplore.exe"
    )
}

