use std::borrow::Cow;

use super::command_search::rg_search_path_operands;
use crate::shell::ShellType;
use crate::tools::handlers::command_shape::CommandInvocation;

#[cfg(test)]
use super::command_search::RgSearchBreadth;
#[cfg(test)]
use super::command_search::classify_rg_search_narrowing;
#[cfg(test)]
use super::command_search::classify_rg_search_narrowing_without_native_scope;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommandPreflightIssueCode {
    DirectArgvPowerShellCmdlet,
    KnownFlagTypo,
    RgLiteralGlobPath,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CommandPreflightRetry {
    Argv { program: String, args: Vec<String> },
    PowerShellScript { script_body: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CommandPreflightRejected {
    Argv(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CommandPreflightIssue {
    code: CommandPreflightIssueCode,
    rejected: CommandPreflightRejected,
    detail: String,
    guidance: Option<String>,
    retry: Option<CommandPreflightRetry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommandPreflightOutcome {
    pub(crate) invocation: CommandInvocation,
    pub(crate) validation_invocations: Vec<CommandInvocation>,
    pub(crate) repair_notice: Option<String>,
}

impl CommandPreflightOutcome {
    pub(crate) fn repaired(&self) -> bool {
        self.repair_notice.is_some()
    }
}

impl CommandPreflightIssue {
    fn reject(
        code: CommandPreflightIssueCode,
        rejected: CommandPreflightRejected,
        detail: String,
        guidance: Option<String>,
        retry: Option<CommandPreflightRetry>,
    ) -> Self {
        Self {
            code,
            rejected,
            detail,
            guidance,
            retry,
        }
    }

    pub(crate) fn render_for_model(&self) -> String {
        let mut rendered = format!(
            "Command rejected: `{}`\nReason: {}",
            self.rejected.render_for_model(),
            self.detail
        );
        match &self.retry {
            Some(retry) => {
                rendered.push_str("\nUse: ");
                rendered.push_str(&retry.render_for_model());
                rendered.push('.');
            }
            None => {
                if let Some(guidance) = &self.guidance {
                    rendered.push_str("\nUse: ");
                    rendered.push_str(guidance);
                }
            }
        }
        let metadata = serde_json::json!({
            "kind": self.code.tool_error_kind(),
            "summary": self.detail,
        });
        rendered.push_str("\nTool error metadata: ");
        rendered.push_str(&metadata.to_string());
        rendered
    }
}

impl CommandPreflightIssueCode {
    fn tool_error_kind(self) -> &'static str {
        match self {
            Self::DirectArgvPowerShellCmdlet => "direct_argv_powershell_cmdlet",
            Self::KnownFlagTypo => "known_flag_typo",
            Self::RgLiteralGlobPath => "rg_literal_glob_path",
        }
    }
}

impl CommandPreflightRejected {
    fn render_for_model(&self) -> String {
        match self {
            Self::Argv(argv) => truncate(&codex_shell_command::parse_command::shlex_join(argv)),
        }
    }
}

impl CommandPreflightRetry {
    fn render_for_model(&self) -> String {
        match self {
            Self::Argv { program, args } => format!(
                "kind: \"argv\", program: {}, args: [{}]",
                json_string(program),
                args.iter()
                    .map(|arg| json_string(arg))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::PowerShellScript { script_body } => format!(
                "kind: \"powershell_script\", script_body: {}",
                json_string(script_body)
            ),
        }
    }
}

#[cfg(test)]
pub(crate) fn preflight_command(
    command: &[String],
    shell_type: Option<ShellType>,
) -> Result<(), String> {
    preflight_command_issue(command, shell_type)
        .map(|_| ())
        .map_err(|issue| issue.render_for_model())
}

fn preflight_command_issue(
    command: &[String],
    shell_type: Option<ShellType>,
) -> Result<Vec<Vec<String>>, CommandPreflightIssue> {
    let preflight_shell_type = shell_type.or_else(|| infer_direct_shell_type(command));
    // Parsing here extracts validation metadata; the target shell owns script
    // syntax and expansion. Heuristic quote/name/path checks can reject valid scripts.
    let argv_commands = argv_commands(command, preflight_shell_type).unwrap_or_default();

    for argv in &argv_commands {
        lint_direct_argv_powershell_cmdlet(argv, preflight_shell_type)?;
        lint_known_flag_typos(argv)?;
        lint_rg_literal_glob_paths(argv, preflight_shell_type)?;
    }

    Ok(argv_commands)
}

#[cfg(test)]
pub(crate) fn preflight_invocation_with_equivalent_repair(
    invocation: &CommandInvocation,
    command: &[String],
    shell_type: Option<ShellType>,
) -> Result<CommandPreflightOutcome, String> {
    preflight_invocation_with_equivalent_repair_detailed(invocation, command, shell_type)
        .map_err(|issue| issue.render_for_model())
}

pub(crate) async fn preflight_invocation_with_equivalent_repair_async(
    invocation: &CommandInvocation,
    command: &[String],
    shell_type: Option<ShellType>,
) -> Result<CommandPreflightOutcome, String> {
    let invocation = invocation.clone();
    let command = command.to_vec();
    crate::tools::run_blocking_command_analysis(move || {
        preflight_invocation_with_equivalent_repair_detailed(&invocation, &command, shell_type)
            .map_err(|issue| issue.render_for_model())
    })
    .await
    .map_err(|error| format!("command preflight worker failed: {error}"))?
}

pub(crate) async fn preflight_invocation_for_kd4_runtime(
    kd4_runtime: bool,
    direct_runtime: bool,
    invocation: &CommandInvocation,
    command: &[String],
    shell_type: Option<ShellType>,
) -> Result<CommandPreflightOutcome, String> {
    if !kd4_runtime {
        return Ok(CommandPreflightOutcome {
            invocation: invocation.clone(),
            validation_invocations: vec![invocation.clone()],
            repair_notice: None,
        });
    }
    preflight_invocation_for_runtime(direct_runtime, invocation, command, shell_type).await
}

pub(crate) async fn preflight_invocation_for_runtime(
    direct_runtime: bool,
    invocation: &CommandInvocation,
    command: &[String],
    shell_type: Option<ShellType>,
) -> Result<CommandPreflightOutcome, String> {
    if direct_runtime {
        return Ok(CommandPreflightOutcome {
            invocation: invocation.clone(),
            validation_invocations: Vec::new(),
            repair_notice: None,
        });
    }
    preflight_invocation_with_equivalent_repair_async(invocation, command, shell_type).await
}

fn preflight_invocation_with_equivalent_repair_detailed(
    invocation: &CommandInvocation,
    command: &[String],
    shell_type: Option<ShellType>,
) -> Result<CommandPreflightOutcome, CommandPreflightIssue> {
    let issue = match preflight_command_issue(command, shell_type) {
        Ok(argv_commands) => {
            if let Some(repaired) = git_status_read_only_equivalent(invocation) {
                let Some(repaired_command) = repaired.to_direct_argv() else {
                    return Ok(CommandPreflightOutcome {
                        invocation: invocation.clone(),
                        validation_invocations: validation_invocations(argv_commands, invocation),
                        repair_notice: None,
                    });
                };
                let repaired_argv_commands =
                    preflight_command_issue(&repaired_command, /*shell_type*/ None)?;
                return Ok(CommandPreflightOutcome {
                    // Status is already valid. Disabling optional locks is
                    // normalization, so it must not bypass retry admission.
                    repair_notice: None,
                    validation_invocations: validation_invocations(
                        repaired_argv_commands,
                        &repaired,
                    ),
                    invocation: repaired,
                });
            }
            return Ok(CommandPreflightOutcome {
                invocation: invocation.clone(),
                validation_invocations: validation_invocations(argv_commands, invocation),
                repair_notice: None,
            });
        }
        Err(issue) => issue,
    };

    // A repair is execution-safe only for direct argv. Rewriting a script could
    // discard pipelines, redirection, variable expansion, or a later mutating
    // command even when the first parsed argv happens to look read-only.
    if !invocation.is_argv() {
        return Err(issue);
    }

    let Some(CommandPreflightRetry::Argv { program, args }) = issue.retry.as_ref() else {
        return Err(issue);
    };
    let program_name = program_name(program);
    let read_only_equivalent = match issue.code {
        CommandPreflightIssueCode::KnownFlagTypo => {
            matches_ignore_ascii_case(program_name, &["rg", "rga", "grep"])
        }
        _ => false,
    };
    if !read_only_equivalent {
        return Err(issue);
    }

    let repaired = CommandInvocation::Argv {
        program: program.clone(),
        args: args.clone(),
    };
    let Some(repaired_command) = repaired.to_direct_argv() else {
        return Err(issue);
    };
    // Search executables can also launch helpers (for example rg --pre).
    // Verify the complete repaired argv before automatically executing it.
    if !codex_shell_command::is_safe_command::is_known_safe_direct_argv(&repaired_command) {
        return Err(issue);
    }
    // One repair is the hard limit. If the repaired command has another issue,
    // reject it rather than chaining mechanical transformations.
    let repaired_argv_commands =
        preflight_command_issue(&repaired_command, /*shell_type*/ None)?;

    let repair_notice = read_only_repair_notice(issue.code, invocation, &repaired);
    Ok(CommandPreflightOutcome {
        validation_invocations: validation_invocations(repaired_argv_commands, &repaired),
        invocation: repaired,
        repair_notice: Some(repair_notice),
    })
}

fn validation_invocations(
    argv_commands: Vec<Vec<String>>,
    fallback: &CommandInvocation,
) -> Vec<CommandInvocation> {
    let invocations = argv_commands
        .into_iter()
        .filter_map(|argv| {
            let mut arguments = argv.into_iter();
            let program = arguments.next()?;
            Some(CommandInvocation::Argv {
                program,
                args: arguments.collect(),
            })
        })
        .collect::<Vec<_>>();
    if invocations.is_empty() {
        vec![fallback.clone()]
    } else {
        invocations
    }
}

fn git_status_read_only_equivalent(invocation: &CommandInvocation) -> Option<CommandInvocation> {
    let CommandInvocation::Argv { program, args } = invocation else {
        return None;
    };
    let command = invocation.to_direct_argv()?;
    let (subcommand_index, _) =
        codex_shell_command::is_dangerous_command::find_git_subcommand(&command, &["status"])?;
    if command[1..subcommand_index]
        .iter()
        .any(|arg| arg == "--no-optional-locks")
    {
        return None;
    }

    let mut repaired_args = Vec::with_capacity(args.len() + 1);
    repaired_args.push("--no-optional-locks".to_string());
    repaired_args.extend(args.iter().cloned());
    Some(CommandInvocation::Argv {
        program: program.clone(),
        args: repaired_args,
    })
}

fn read_only_repair_notice(
    code: CommandPreflightIssueCode,
    original: &CommandInvocation,
    repaired: &CommandInvocation,
) -> String {
    format!(
        "Command preflight applied one read-only equivalent repair ({}) before execution.\nOriginal: {}\nExecuted: {}",
        code.tool_error_kind(),
        original.display_command(),
        repaired.display_command()
    )
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| format!("{value:?}"))
}

fn extract_cmd_command(command: &[String]) -> Option<Cow<'_, str>> {
    for (index, arg) in command.iter().skip(1).enumerate() {
        let trimmed = arg.trim();
        if trimmed.eq_ignore_ascii_case("/c") || trimmed.eq_ignore_ascii_case("/k") {
            return command_tail(command, index + 2);
        }
        if let Some(script) = cmd_switch_inline_script(trimmed) {
            return Some(Cow::Borrowed(script));
        }
    }
    None
}

fn cmd_switch_inline_script(arg: &str) -> Option<&str> {
    ["/c", "/k"].iter().find_map(|switch| {
        arg.get(..switch.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(switch))
            .then(|| arg[switch.len()..].trim_start())
            .filter(|script| !script.is_empty())
    })
}

fn command_tail(command: &[String], start: usize) -> Option<Cow<'_, str>> {
    match command.get(start..) {
        Some([]) | None => None,
        Some([script]) => Some(Cow::Borrowed(script.as_str())),
        Some(args) => Some(Cow::Owned(args.join(" "))),
    }
}

pub(crate) fn infer_direct_shell_type(command: &[String]) -> Option<ShellType> {
    let program = command.first().map(|program| program_name(program))?;
    if program.eq_ignore_ascii_case("pwsh") || program.eq_ignore_ascii_case("powershell") {
        Some(ShellType::PowerShell)
    } else if program.eq_ignore_ascii_case("cmd") || program.eq_ignore_ascii_case("cmd.exe") {
        Some(ShellType::Cmd)
    } else if program.eq_ignore_ascii_case("bash") {
        Some(ShellType::Bash)
    } else if program.eq_ignore_ascii_case("zsh") {
        Some(ShellType::Zsh)
    } else if program.eq_ignore_ascii_case("sh") {
        Some(ShellType::Sh)
    } else {
        None
    }
}

fn argv_commands(command: &[String], shell_type: Option<ShellType>) -> Option<Vec<Vec<String>>> {
    match shell_type {
        Some(ShellType::Bash | ShellType::Zsh | ShellType::Sh) => {
            codex_shell_command::bash::parse_shell_lc_plain_commands(command)
        }
        Some(ShellType::PowerShell) => {
            codex_shell_command::powershell::parse_powershell_command_into_plain_commands(command)
        }
        Some(ShellType::Cmd) => parse_cmd_plain_commands(command),
        None => {
            if command.is_empty() {
                Some(Vec::new())
            } else {
                Some(vec![command.to_vec()])
            }
        }
    }
}

pub(crate) fn rg_argv_commands(
    command: &[String],
    shell_type: Option<ShellType>,
) -> Result<Vec<Vec<String>>, String> {
    match argv_commands(command, shell_type) {
        Some(commands) => Ok(commands),
        None if command_may_invoke_rg(command) => Err(
            "`rg` search rejected because the shell command could not be parsed precisely enough to verify its search scope. Use direct argv or a statically parseable shell command."
                .to_string(),
        ),
        None => Ok(Vec::new()),
    }
}

fn command_may_invoke_rg(command: &[String]) -> bool {
    command.iter().any(|argument| {
        argument
            .split(|ch: char| {
                ch.is_whitespace()
                    || matches!(
                        ch,
                        '|' | '&' | ';' | '(' | ')' | '{' | '}' | '<' | '>' | '"' | '\''
                    )
            })
            .map(|token| token.trim_matches([',', '`']))
            .any(is_rg_program)
    })
}

fn parse_cmd_plain_commands(command: &[String]) -> Option<Vec<Vec<String>>> {
    let script = extract_cmd_command(command)?;
    split_cmd_plain_commands(script.as_ref())
}

fn split_cmd_plain_commands(script: &str) -> Option<Vec<Vec<String>>> {
    let mut commands = Vec::new();
    for command in split_cmd_command_segments(script)? {
        let argv = split_cmd_words(command)?;
        if !argv.is_empty() {
            commands.push(argv);
        }
    }
    Some(commands)
}

fn split_cmd_command_segments(script: &str) -> Option<Vec<&str>> {
    let mut segments = Vec::new();
    let mut start = 0;
    let mut in_double_quote = false;
    let mut chars = script.char_indices().peekable();
    while let Some((index, ch)) = chars.next() {
        match ch {
            '"' => in_double_quote = !in_double_quote,
            '>' | '<' if !in_double_quote => return None,
            '&' | '|' if !in_double_quote => {
                let segment = script[start..index].trim();
                if !segment.is_empty() {
                    segments.push(segment);
                }
                if chars.peek().is_some_and(|(_, next)| *next == ch) {
                    chars.next();
                }
                start = chars
                    .peek()
                    .map_or(index + ch.len_utf8(), |(next_index, _)| *next_index);
            }
            _ => {}
        }
    }
    if in_double_quote {
        return None;
    }
    let segment = script[start..].trim();
    if !segment.is_empty() {
        segments.push(segment);
    }
    Some(segments)
}

fn split_cmd_words(command: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_double_quote = false;
    let mut chars = command.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => in_double_quote = !in_double_quote,
            '^' => {
                if let Some(escaped) = chars.next() {
                    word.push(escaped);
                } else {
                    word.push(ch);
                }
            }
            ch if ch.is_whitespace() && !in_double_quote => {
                if !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
            }
            _ => word.push(ch),
        }
    }
    if in_double_quote {
        return None;
    }
    if !word.is_empty() {
        words.push(word);
    }
    Some(words)
}

fn lint_known_flag_typos(argv: &[String]) -> Result<(), CommandPreflightIssue> {
    let Some(program) = argv.first().map(|program| program_name(program)) else {
        return Ok(());
    };

    let mut index = 1;
    while let Some(arg) = argv.get(index) {
        if arg == "--" {
            break;
        }
        if !arg.starts_with('-') {
            index += 1;
            continue;
        }
        if let Some((bad, good)) = known_flag_fix(program, arg) {
            let suggested = suggested_argv(argv, index, good);
            return Err(CommandPreflightIssue::reject(
                CommandPreflightIssueCode::KnownFlagTypo,
                CommandPreflightRejected::Argv(argv.to_vec()),
                format!("`{program}` has no `{bad}` flag."),
                None,
                retry_argv_from_command(&suggested),
            ));
        }
        // Only continue through options whose arity is established. In
        // particular, a dash-prefixed regexp is data after -e/--regexp.
        if matches_ignore_ascii_case(program, &["rg", "rga", "grep"]) {
            if rg_option_consumes_next(arg) {
                index += 2;
                continue;
            }
            if !arg.starts_with("--") && arg.len() > 2 {
                let mut consumes_next = false;
                for (offset, flag) in arg[1..].char_indices() {
                    if rg_option_consumes_next(&format!("-{flag}")) {
                        consumes_next = offset + flag.len_utf8() == arg.len() - 1;
                        break;
                    }
                }
                index += if consumes_next { 2 } else { 1 };
                continue;
            }
        }
        if arg.contains('=')
            || matches!(
                arg.as_str(),
                "-n" | "-i"
                    | "-l"
                    | "-L"
                    | "-F"
                    | "-w"
                    | "-x"
                    | "-v"
                    | "-q"
                    | "--files"
                    | "--hidden"
                    | "--no-ignore"
                    | "--ignore-case"
                    | "--line-number"
                    | "--fixed-strings"
                    | "--files-with-matches"
            )
        {
            index += 1;
        } else {
            // An unknown option may consume the next argument. Let the actual
            // program interpret it instead of guessing at a later repair.
            break;
        }
    }

    Ok(())
}

fn lint_rg_literal_glob_paths(
    argv: &[String],
    shell_type: Option<ShellType>,
) -> Result<(), CommandPreflightIssue> {
    if matches!(
        shell_type,
        Some(ShellType::Bash | ShellType::Zsh | ShellType::Sh)
    ) {
        return Ok(());
    }

    // Share argument roles with search scoping so explicit patterns, short
    // option clusters, and metadata modes cannot be mistaken for path operands.
    if !argv.first().is_some_and(|program| is_rg_program(program)) {
        return Ok(());
    }
    for arg in rg_search_path_operands(&[argv.to_vec()]).unwrap_or_default() {
        if looks_like_unexpanded_glob_path(&arg) {
            let detail = match shell_type {
                Some(ShellType::PowerShell) => format!(
                    "PowerShell does not POSIX-expand native-command wildcard path arguments; `rg` receives `{arg}` as a literal path."
                ),
                Some(ShellType::Cmd) => format!(
                    "cmd does not POSIX-expand native-command wildcard path arguments; `rg` receives `{arg}` as a literal path."
                ),
                _ => format!(
                    "`rg` direct argv path arguments are not shell-expanded; `{arg}` is passed as a literal path."
                ),
            };
            return Err(CommandPreflightIssue::reject(
                CommandPreflightIssueCode::RgLiteralGlobPath,
                CommandPreflightRejected::Argv(argv.to_vec()),
                detail,
                Some(
                    "search the parent directory and pass wildcards through `--glob`, for example `rg --files .codex/skills --glob '*/SKILL.md'`."
                        .to_string(),
                ),
                None,
            ));
        }
    }

    Ok(())
}

fn lint_direct_argv_powershell_cmdlet(
    argv: &[String],
    shell_type: Option<ShellType>,
) -> Result<(), CommandPreflightIssue> {
    if shell_type.is_some() {
        return Ok(());
    }

    let Some(program) = argv.first().map(|program| program_name(program)) else {
        return Ok(());
    };

    // Direct argv can legitimately target an executable whose name happens to
    // have PowerShell's Verb-Noun shape. Reject only names that are actually in
    // the cmdlet/alias allowlist; the broader shape heuristic is useful only
    // while diagnosing a script for the wrong shell.
    if is_known_powershell_cmdlet(program) || is_known_powershell_alias(program) {
        return Err(CommandPreflightIssue::reject(
            CommandPreflightIssueCode::DirectArgvPowerShellCmdlet,
            CommandPreflightRejected::Argv(argv.to_vec()),
            format!(
                "`{program}` is a PowerShell cmdlet or alias, not a standalone executable for direct argv mode."
            ),
            None,
            Some(CommandPreflightRetry::PowerShellScript {
                script_body: powershell_join_args(argv),
            }),
        ));
    }

    Ok(())
}

fn known_flag_fix<'a>(program: &str, arg: &'a str) -> Option<(&'a str, &'static str)> {
    let flag = arg.split_once('=').map_or(arg, |(flag, _)| flag);
    let program_lower = program.to_ascii_lowercase();
    let flag_lower = flag.to_ascii_lowercase();
    match (program_lower.as_str(), flag_lower.as_str()) {
        ("rg" | "rga", "--ignorecase") => Some((flag, "--ignore-case")),
        ("rg" | "rga", "--files-with-match") => Some((flag, "--files-with-matches")),
        ("grep", "--ignorecase") => Some((flag, "--ignore-case")),
        ("git", "--worktree") => Some((flag, "--work-tree")),
        ("cargo", "--pakage") => Some((flag, "--package")),
        ("npm", "--workpace") => Some((flag, "--workspace")),
        ("pytest", "--max-fail") => Some((flag, "--maxfail")),
        ("get-childitem", "-recuse") => Some((flag, "-Recurse")),
        ("select-string", "-patern") => Some((flag, "-Pattern")),
        ("select-string", "-casesensitve") => Some((flag, "-CaseSensitive")),
        _ => None,
    }
}

fn suggested_argv(argv: &[String], index: usize, good: &str) -> Vec<String> {
    let mut suggested = argv.to_vec();
    suggested[index] = argv[index]
        .split_once('=')
        .map_or_else(|| good.to_string(), |(_, value)| format!("{good}={value}"));
    suggested
}

fn retry_argv_from_command(command: &[String]) -> Option<CommandPreflightRetry> {
    let (program, args) = command.split_first()?;
    Some(CommandPreflightRetry::Argv {
        program: program.clone(),
        args: args.to_vec(),
    })
}

fn powershell_join_args(argv: &[String]) -> String {
    argv.iter()
        .map(|arg| {
            if is_bare_powershell_arg(arg) {
                arg.clone()
            } else {
                format!("'{}'", arg.replace('\'', "''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_bare_powershell_arg(arg: &str) -> bool {
    !arg.is_empty()
        && arg
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '/' | '=' | ':'))
}

pub(super) fn is_rg_program(program: &str) -> bool {
    matches_ignore_ascii_case(program_name(program), &["rg", "rga", "ripgrep"])
}

pub(super) fn rg_option_consumes_next(arg: &str) -> bool {
    matches!(
        arg,
        "-A" | "--after-context"
            | "-B"
            | "--before-context"
            | "-C"
            | "--context"
            | "--context-separator"
            | "--color"
            | "--colors"
            | "-d"
            | "--max-depth"
            | "--dfa-size-limit"
            | "-e"
            | "--regexp"
            | "-E"
            | "--encoding"
            | "--engine"
            | "-f"
            | "--file"
            | "--field-context-separator"
            | "--field-match-separator"
            | "-g"
            | "--glob"
            | "--iglob"
            | "--ignore-file"
            | "--hostname-bin"
            | "--hyperlink-format"
            | "-j"
            | "--threads"
            | "-m"
            | "--max-count"
            | "-M"
            | "--max-columns"
            | "--max-filesize"
            | "--path-separator"
            | "--pre"
            | "--pre-glob"
            | "-r"
            | "--replace"
            | "--regex-size-limit"
            | "--sort"
            | "--sortr"
            | "-t"
            | "--type"
            | "--type-add"
            | "--type-clear"
            | "-T"
            | "--type-not"
    )
}

fn looks_like_unexpanded_glob_path(arg: &str) -> bool {
    (arg.contains('*') || arg.contains('?'))
        && !arg.starts_with("http://")
        && !arg.starts_with("https://")
}

pub(super) fn program_name(program: &str) -> &str {
    let program = strip_matching_quotes(program.trim());
    let file_name = program.rsplit(['/', '\\']).next().unwrap_or(program);
    let file_name = strip_matching_quotes(file_name);
    match file_name.rsplit_once('.') {
        Some((stem, extension)) if is_windows_executable_extension(extension) => stem,
        _ => file_name,
    }
}

fn strip_matching_quotes(value: &str) -> &str {
    if value.len() < 2 {
        return value;
    }
    let bytes = value.as_bytes();
    if matches!(
        (bytes.first(), bytes.last()),
        (Some(b'"'), Some(b'"')) | (Some(b'\''), Some(b'\''))
    ) {
        &value[1..value.len() - 1]
    } else {
        value
    }
}

fn is_windows_executable_extension(extension: &str) -> bool {
    matches_ignore_ascii_case(extension, &["bat", "cmd", "com", "exe"])
}

fn is_known_powershell_cmdlet(command: &str) -> bool {
    matches_ignore_ascii_case(
        command,
        &[
            "Get-ChildItem",
            "Get-Content",
            "Set-Content",
            "Select-String",
            "Remove-Item",
            "Move-Item",
            "Copy-Item",
            "New-Item",
            "Test-Path",
            "Resolve-Path",
            "Start-Process",
            "Invoke-WebRequest",
            "Invoke-RestMethod",
        ],
    )
}

fn is_known_powershell_alias(command: &str) -> bool {
    matches_ignore_ascii_case(
        command,
        &[
            "gal", "gci", "gcm", "gc", "gl", "gp", "gps", "gu", "gv", "gwmi", "ii", "irm", "iwr",
            "mi", "ni", "ri", "rvpa", "saps", "sp",
        ],
    )
}

pub(super) fn matches_ignore_ascii_case(value: &str, candidates: &[&str]) -> bool {
    candidates
        .iter()
        .any(|candidate| value.eq_ignore_ascii_case(candidate))
}

fn truncate(value: &str) -> String {
    const MAX: usize = 240;
    if value.len() <= MAX {
        value.to_string()
    } else {
        let mut end = MAX;
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &value[..end])
    }
}

#[cfg(test)]
#[path = "command_preflight_tests.rs"]
mod command_preflight_tests;
