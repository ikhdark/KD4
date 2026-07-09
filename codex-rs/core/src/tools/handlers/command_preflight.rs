use std::borrow::Cow;
use std::path::Path;

use crate::shell::ShellType;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommandPreflightIssueCode {
    UnbalancedQuotes,
    ShellMismatch,
    WindowsLiteralPathRequired,
    DirectArgvPowerShellCmdlet,
    KnownFlagTypo,
    RgGlobPathSeparator,
    RgLiteralGlobPath,
    PowerShellMeasureObjectScriptBlockProperty,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CommandPreflightRetry {
    Argv { program: String, args: Vec<String> },
    PowerShellScript { script_body: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CommandPreflightRejected {
    Script(String),
    Argv(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommandPreflightIssue {
    pub(crate) code: CommandPreflightIssueCode,
    pub(crate) rejected: CommandPreflightRejected,
    pub(crate) detail: String,
    pub(crate) guidance: Option<String>,
    pub(crate) retry: Option<CommandPreflightRetry>,
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
            Self::UnbalancedQuotes => "command_preflight_unbalanced_quotes",
            Self::ShellMismatch => "command_preflight_shell_mismatch",
            Self::WindowsLiteralPathRequired => "windows_literal_path_required",
            Self::DirectArgvPowerShellCmdlet => "direct_argv_powershell_cmdlet",
            Self::KnownFlagTypo => "known_flag_typo",
            Self::RgGlobPathSeparator => "rg_glob_path_separator",
            Self::RgLiteralGlobPath => "rg_literal_glob_path",
            Self::PowerShellMeasureObjectScriptBlockProperty => {
                "powershell_measure_object_scriptblock_property"
            }
        }
    }
}

impl CommandPreflightRejected {
    fn render_for_model(&self) -> String {
        match self {
            Self::Script(script) => truncate(script),
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

pub(crate) fn preflight_command(
    command: &[String],
    shell_type: Option<ShellType>,
) -> Result<(), String> {
    preflight_command_issue(command, shell_type).map_err(|issue| issue.render_for_model())
}

pub(crate) fn preflight_command_issue(
    command: &[String],
    shell_type: Option<ShellType>,
) -> Result<(), CommandPreflightIssue> {
    let preflight_shell_type = shell_type.or_else(|| infer_direct_shell_type(command));
    let argv_commands = argv_commands(command, preflight_shell_type);

    if let Some(script) = shell_script(command, preflight_shell_type) {
        let script = script.as_ref();
        lint_balanced_quotes(script, preflight_shell_type)?;
        lint_shell_mismatch(script, preflight_shell_type, &argv_commands)?;
        lint_powershell_measure_object_scriptblock_property(script, preflight_shell_type)?;
        lint_windows_path_shape(script, preflight_shell_type, &argv_commands)?;
    }

    for argv in argv_commands {
        lint_direct_argv_powershell_cmdlet(&argv, preflight_shell_type)?;
        lint_known_flag_typos(&argv)?;
        lint_rg_glob_path_separators(&argv)?;
        lint_rg_literal_glob_paths(&argv, preflight_shell_type)?;
    }

    Ok(())
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| format!("{value:?}"))
}

pub(crate) fn powershell_single_quoted_literal(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "''"))
}

pub(crate) fn powershell_literal_path_arg(path: &Path) -> Vec<String> {
    vec![
        "-LiteralPath".to_string(),
        powershell_single_quoted_literal(path),
    ]
}

pub(crate) fn cmd_quoted_path(path: &Path) -> String {
    format!("\"{}\"", path.to_string_lossy().replace('"', "\"\""))
}

pub(crate) fn posix_single_quoted(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\"'\"'"))
}

fn shell_script(command: &[String], shell_type: Option<ShellType>) -> Option<Cow<'_, str>> {
    match shell_type {
        Some(ShellType::Bash | ShellType::Zsh | ShellType::Sh) => {
            codex_shell_command::bash::extract_bash_command(command)
                .map(|(_, script)| Cow::Borrowed(script))
        }
        Some(ShellType::PowerShell) => {
            codex_shell_command::powershell::extract_powershell_command(command)
                .map(|(_, script)| Cow::Borrowed(script))
        }
        Some(ShellType::Cmd) => extract_cmd_command(command),
        None => None,
    }
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

fn infer_direct_shell_type(command: &[String]) -> Option<ShellType> {
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

fn argv_commands(command: &[String], shell_type: Option<ShellType>) -> Vec<Vec<String>> {
    match shell_type {
        Some(ShellType::Bash | ShellType::Zsh | ShellType::Sh) => {
            codex_shell_command::bash::parse_shell_lc_plain_commands(command).unwrap_or_default()
        }
        Some(ShellType::PowerShell) => {
            codex_shell_command::powershell::parse_powershell_command_into_plain_commands(command)
                .unwrap_or_default()
        }
        Some(ShellType::Cmd) => parse_cmd_plain_commands(command).unwrap_or_default(),
        None => {
            if command.is_empty() {
                Vec::new()
            } else {
                vec![command.to_vec()]
            }
        }
    }
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

fn lint_balanced_quotes(
    script: &str,
    shell_type: Option<ShellType>,
) -> Result<(), CommandPreflightIssue> {
    let normalized_script = script_without_multiline_literal_bodies(script, shell_type);
    let mut single = false;
    let mut double = false;
    let mut escaped = false;

    for ch in normalized_script.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' && !single {
            escaped = true;
            continue;
        }
        match ch {
            '\'' if !double => single = !single,
            '"' if !single => double = !double,
            _ => {}
        }
    }

    if single || double {
        let quote = if single { "single" } else { "double" };
        return Err(CommandPreflightIssue::reject(
            CommandPreflightIssueCode::UnbalancedQuotes,
            CommandPreflightRejected::Script(script.to_string()),
            format!("missing closing {quote} quote."),
            Some(
                "regenerate the command with balanced quotes, or use structured argv for simple commands."
                    .to_string(),
            ),
            None,
        ));
    }

    Ok(())
}

fn lint_shell_mismatch(
    script: &str,
    shell_type: Option<ShellType>,
    argv_commands: &[Vec<String>],
) -> Result<(), CommandPreflightIssue> {
    match shell_type {
        Some(ShellType::PowerShell)
            if contains_any(
                script,
                &["2>/dev/null", "export ", "source ", " && \\", " || \\"],
            ) =>
        {
            Err(CommandPreflightIssue::reject(
                CommandPreflightIssueCode::ShellMismatch,
                CommandPreflightRejected::Script(script.to_string()),
                "this looks like POSIX shell syntax, but the target shell is PowerShell."
                    .to_string(),
                Some("rewrite the command for PowerShell or select a POSIX shell.".to_string()),
                None,
            ))
        }
        Some(ShellType::Cmd) if starts_with_powershell_cmdlet(script) => {
            Err(CommandPreflightIssue::reject(
                CommandPreflightIssueCode::ShellMismatch,
                CommandPreflightRejected::Script(script.to_string()),
                "this looks like a PowerShell cmdlet or alias, but the target shell is cmd."
                    .to_string(),
                Some("run it with PowerShell or rewrite it for cmd.".to_string()),
                None,
            ))
        }
        Some(ShellType::Bash | ShellType::Zsh | ShellType::Sh)
            if argv_commands.iter().any(|argv| {
                argv.first()
                    .is_some_and(|program| is_powershell_cmdlet_or_alias(program_name(program)))
            }) || (argv_commands.is_empty() && starts_with_powershell_cmdlet(script))
                || contains_ignore_ascii_case(script, "$env:") =>
        {
            Err(CommandPreflightIssue::reject(
                CommandPreflightIssueCode::ShellMismatch,
                CommandPreflightRejected::Script(script.to_string()),
                "this looks like PowerShell syntax, but the target shell is POSIX.".to_string(),
                Some("rewrite the command for POSIX shell or select PowerShell.".to_string()),
                None,
            ))
        }
        _ => Ok(()),
    }
}

fn lint_windows_path_shape(
    script: &str,
    shell_type: Option<ShellType>,
    argv_commands: &[Vec<String>],
) -> Result<(), CommandPreflightIssue> {
    if shell_type != Some(ShellType::PowerShell) || !contains_windows_drive_path(script) {
        return Ok(());
    }

    let has_filesystem_cmdlet = argv_commands.iter().any(|argv| {
        argv.first()
            .is_some_and(|program| is_powershell_filesystem_cmdlet(program_name(program)))
    });
    let has_literal_path_parameter = argv_commands
        .iter()
        .any(|argv| has_powershell_parameter(argv, "LiteralPath"));
    let has_path_parameter = argv_commands
        .iter()
        .any(|argv| has_powershell_parameter(argv, "Path"));
    if has_filesystem_cmdlet
        && !has_literal_path_parameter
        && (has_path_parameter || script.contains('[') || script.contains(']'))
    {
        let example_path = Path::new("C:\\path with spaces\\[name]");
        let powershell_example = powershell_literal_path_arg(example_path).join(" ");
        return Err(CommandPreflightIssue::reject(
            CommandPreflightIssueCode::WindowsLiteralPathRequired,
            CommandPreflightRejected::Script(script.to_string()),
            "PowerShell filesystem paths with spaces or wildcard characters should use `-LiteralPath`."
                .to_string(),
            Some(format!(
                "pass the path as `{powershell_example}`.\nPath quoting examples: cmd {}, POSIX {}.",
                cmd_quoted_path(example_path),
                posix_single_quoted(Path::new("/path with spaces/[name]")),
            )),
            None,
        ));
    }

    Ok(())
}

fn lint_powershell_measure_object_scriptblock_property(
    script: &str,
    shell_type: Option<ShellType>,
) -> Result<(), CommandPreflightIssue> {
    if shell_type != Some(ShellType::PowerShell) || !has_measure_object_scriptblock_property(script)
    {
        return Ok(());
    }

    Err(CommandPreflightIssue::reject(
        CommandPreflightIssueCode::PowerShellMeasureObjectScriptBlockProperty,
        CommandPreflightRejected::Script(script.to_string()),
        "PowerShell `Measure-Object -Property` expects property names, not a script block."
            .to_string(),
        Some(
            "pipe computed numeric values first, for example `... | ForEach-Object { <number> } | Measure-Object -Sum`; for real properties, use `Measure-Object -Property Count -Sum`."
                .to_string(),
        ),
        None,
    ))
}

fn has_measure_object_scriptblock_property(script: &str) -> bool {
    let lower = script.to_ascii_lowercase();
    let mut offset = 0;
    while let Some(index) = lower[offset..].find("measure-object") {
        let start = offset + index;
        let after_command = start + "measure-object".len();
        if !is_word_boundary(lower[..start].chars().next_back())
            || !is_word_boundary(lower[after_command..].chars().next())
        {
            offset = after_command;
            continue;
        }

        let command_segment = lower[after_command..]
            .split(['|', '\n', '\r', ';'])
            .next()
            .unwrap_or_default();
        if measure_object_segment_has_scriptblock_property(command_segment) {
            return true;
        }
        offset = after_command;
    }
    false
}

fn is_word_boundary(ch: Option<char>) -> bool {
    ch.is_none_or(|ch| !ch.is_ascii_alphanumeric() && ch != '_')
}

fn measure_object_segment_has_scriptblock_property(segment: &str) -> bool {
    let mut rest = segment;
    while let Some(index) = rest.find("-property") {
        let after_name = &rest[index + "-property".len()..];
        let Some(next) = after_name.chars().next() else {
            return false;
        };
        if next.is_ascii_alphanumeric() || next == '_' || next == '-' {
            rest = after_name;
            continue;
        }

        let value = after_name
            .trim_start_matches(|ch: char| ch.is_ascii_whitespace() || ch == ':' || ch == '=');
        if value.starts_with('{') {
            return true;
        }
        rest = after_name;
    }
    false
}

fn lint_known_flag_typos(argv: &[String]) -> Result<(), CommandPreflightIssue> {
    let Some(program) = argv.first().map(|program| program_name(program)) else {
        return Ok(());
    };

    for arg in argv.iter().skip(1) {
        if !arg.starts_with('-') {
            continue;
        }
        if let Some((bad, good)) = known_flag_fix(program, arg) {
            let suggested = suggested_argv(argv, bad, good);
            return Err(CommandPreflightIssue::reject(
                CommandPreflightIssueCode::KnownFlagTypo,
                CommandPreflightRejected::Argv(argv.to_vec()),
                format!("`{program}` has no `{bad}` flag."),
                None,
                retry_argv_from_command(&suggested),
            ));
        }
    }

    Ok(())
}

fn lint_rg_glob_path_separators(argv: &[String]) -> Result<(), CommandPreflightIssue> {
    let Some(program) = argv.first().map(|program| program_name(program)) else {
        return Ok(());
    };
    if !matches_ignore_ascii_case(program, &["rg", "rga"]) {
        return Ok(());
    }

    let mut index = 1;
    while let Some(arg) = argv.get(index) {
        let glob = if arg == "--glob" || arg == "-g" {
            argv.get(index + 1).map(|next| (index + 1, next.as_str()))
        } else if let Some((flag, value)) = arg.split_once('=') {
            (flag == "--glob").then_some((index, value))
        } else {
            arg.strip_prefix("-g")
                .filter(|value| !value.is_empty())
                .map(|value| (index, value))
        };

        let Some((glob_index, glob)) = glob else {
            index += 1;
            continue;
        };
        if glob.contains('\\') {
            let mut suggested = argv.to_vec();
            suggested[glob_index] = suggested[glob_index].replace('\\', "/");
            return Err(CommandPreflightIssue::reject(
                CommandPreflightIssueCode::RgGlobPathSeparator,
                CommandPreflightRejected::Argv(argv.to_vec()),
                "`rg` glob patterns use gitignore-style `/` separators, even on Windows."
                    .to_string(),
                None,
                retry_argv_from_command(&suggested),
            ));
        }
        index += 1;
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

    let Some(program) = argv.first().map(|program| program_name(program)) else {
        return Ok(());
    };
    if !matches_ignore_ascii_case(program, &["rg", "rga"]) {
        return Ok(());
    }

    let files_mode = argv.iter().skip(1).any(|arg| {
        matches!(
            arg.as_str(),
            "--files" | "--files-with-matches" | "--files-without-match"
        )
    });
    let mut positional = 0usize;
    let mut index = 1usize;
    while let Some(arg) = argv.get(index) {
        if rg_option_consumes_next(arg) {
            index += 2;
            continue;
        }
        if arg.starts_with('-') {
            index += 1;
            continue;
        }

        let is_path_position = files_mode || positional > 0;
        if is_path_position && looks_like_unexpanded_glob_path(arg) {
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
                    "search the parent directory and pass wildcards through `--glob`, for example `rg --files .codex/skills --glob */SKILL.md`."
                        .to_string(),
                ),
                None,
            ));
        }

        positional += 1;
        index += 1;
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

    if is_powershell_cmdlet_or_alias(program) {
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
        ("get-childitem" | "get-content" | "select-string", "-recuse") => Some((flag, "-Recurse")),
        ("select-string", "-patern") => Some((flag, "-Pattern")),
        ("select-string", "-casesensitve") => Some((flag, "-CaseSensitive")),
        _ => None,
    }
}

fn suggested_argv(argv: &[String], bad: &str, good: &str) -> Vec<String> {
    let mut suggested = argv.to_vec();
    for arg in suggested.iter_mut().skip(1) {
        if arg == bad {
            *arg = good.to_string();
            break;
        }
        if let Some((flag, value)) = arg.split_once('=')
            && flag == bad
        {
            *arg = format!("{good}={value}");
            break;
        }
    }
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

fn rg_option_consumes_next(arg: &str) -> bool {
    matches!(
        arg,
        "-A" | "--after-context"
            | "-B"
            | "--before-context"
            | "-C"
            | "--context"
            | "-e"
            | "--regexp"
            | "-g"
            | "--glob"
            | "-m"
            | "--max-count"
            | "-t"
            | "--type"
            | "-T"
            | "--type-not"
    )
}

fn looks_like_unexpanded_glob_path(arg: &str) -> bool {
    (arg.contains('*') || arg.contains('?'))
        && (arg.contains('/') || arg.contains('\\'))
        && !arg.starts_with("http://")
        && !arg.starts_with("https://")
}

fn program_name(program: &str) -> &str {
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
    matches_ignore_ascii_case(extension, &["bat", "cmd", "com", "exe", "ps1", "psm1"])
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
}

fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    haystack
        .as_bytes()
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

fn starts_with_powershell_cmdlet(script: &str) -> bool {
    let trimmed =
        script.trim_start_matches(|ch: char| ch.is_whitespace() || matches!(ch, ';' | '|' | '&'));
    let first = trimmed
        .split_once(|ch: char| ch.is_whitespace() || matches!(ch, ';' | '|' | '&'))
        .map_or(trimmed, |(first, _)| first);
    is_powershell_cmdlet_or_alias(program_name(first))
}

fn is_powershell_cmdlet_or_alias(command: &str) -> bool {
    is_known_powershell_cmdlet(command)
        || is_known_powershell_alias(command)
        || has_powershell_cmdlet_shape(command)
}

fn is_powershell_filesystem_cmdlet(command: &str) -> bool {
    is_known_powershell_filesystem_cmdlet(command)
        || is_known_powershell_filesystem_alias(command)
        || has_powershell_filesystem_cmdlet_shape(command)
}

fn is_known_powershell_filesystem_cmdlet(command: &str) -> bool {
    matches_ignore_ascii_case(
        command,
        &[
            "Get-ChildItem",
            "Get-Content",
            "Set-Content",
            "Remove-Item",
            "Move-Item",
            "Copy-Item",
            "Test-Path",
            "Resolve-Path",
        ],
    )
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

fn is_known_powershell_filesystem_alias(command: &str) -> bool {
    matches_ignore_ascii_case(
        command,
        &[
            "cat", "copy", "cp", "cpi", "dir", "gc", "gci", "ls", "mi", "move", "mv", "ri", "rm",
            "rmdir", "rvpa", "sc", "type",
        ],
    )
}

fn has_powershell_cmdlet_shape(command: &str) -> bool {
    let Some((verb, noun)) = command.split_once('-') else {
        return false;
    };
    !noun.is_empty()
        && is_common_powershell_verb(verb)
        && noun
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
}

fn has_powershell_filesystem_cmdlet_shape(command: &str) -> bool {
    let Some((verb, noun)) = command.split_once('-') else {
        return false;
    };
    matches_ignore_ascii_case(
        verb,
        &["Copy", "Get", "Move", "Remove", "Resolve", "Set", "Test"],
    ) && noun
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
        && matches_ignore_ascii_case(
            noun,
            &["ChildItem", "Content", "Item", "ItemProperty", "Path"],
        )
}

fn is_common_powershell_verb(verb: &str) -> bool {
    matches_ignore_ascii_case(
        verb,
        &[
            "Add",
            "Clear",
            "Compare",
            "ConvertFrom",
            "ConvertTo",
            "Copy",
            "Disable",
            "Enable",
            "Enter",
            "Exit",
            "Export",
            "Find",
            "ForEach",
            "Format",
            "Get",
            "Group",
            "Import",
            "Install",
            "Invoke",
            "Join",
            "Measure",
            "Move",
            "New",
            "Out",
            "Pop",
            "Push",
            "Read",
            "Receive",
            "Register",
            "Remove",
            "Rename",
            "Resolve",
            "Restart",
            "Resume",
            "Save",
            "Search",
            "Select",
            "Send",
            "Set",
            "Sort",
            "Split",
            "Start",
            "Stop",
            "Tee",
            "Test",
            "Uninstall",
            "Unregister",
            "Update",
            "Wait",
            "Where",
            "Write",
        ],
    )
}

fn has_powershell_parameter(argv: &[String], parameter: &str) -> bool {
    argv.iter().skip(1).any(|arg| {
        powershell_parameter_name(arg).is_some_and(|name| name.eq_ignore_ascii_case(parameter))
    })
}

fn powershell_parameter_name(arg: &str) -> Option<&str> {
    let rest = arg.trim_start().strip_prefix('-')?;
    let name = rest.split_once(':').map_or(rest, |(name, _)| name);
    let name = name.split_once('=').map_or(name, |(name, _)| name);
    (!name.is_empty()).then_some(name)
}

fn matches_ignore_ascii_case(value: &str, candidates: &[&str]) -> bool {
    candidates
        .iter()
        .any(|candidate| value.eq_ignore_ascii_case(candidate))
}

fn contains_windows_drive_path(script: &str) -> bool {
    let bytes = script.as_bytes();
    bytes
        .windows(3)
        .any(|window| window[0].is_ascii_alphabetic() && window[1] == b':' && window[2] == b'\\')
}

fn script_without_multiline_literal_bodies(script: &str, shell_type: Option<ShellType>) -> String {
    match shell_type {
        Some(ShellType::Bash | ShellType::Zsh | ShellType::Sh) => {
            strip_posix_heredoc_bodies(script)
        }
        Some(ShellType::PowerShell) => strip_powershell_here_string_bodies(script),
        _ => script.to_string(),
    }
}

fn strip_posix_heredoc_bodies(script: &str) -> String {
    let mut rendered = String::new();
    let mut pending_delimiters = Vec::<String>::new();
    let mut lines = script.lines();

    while let Some(line) = lines.next() {
        rendered.push_str(line);
        rendered.push('\n');
        pending_delimiters.extend(posix_heredoc_delimiters(line));

        while let Some(delimiter) = pending_delimiters.first() {
            let Some(body_line) = lines.next() else {
                return rendered;
            };
            if body_line == delimiter {
                rendered.push_str(body_line);
                rendered.push('\n');
                pending_delimiters.remove(0);
            }
        }
    }

    rendered
}

fn posix_heredoc_delimiters(line: &str) -> Vec<String> {
    let mut delimiters = Vec::new();
    let mut rest = line;
    while let Some(index) = rest.find("<<") {
        rest = &rest[index + 2..];
        if rest.starts_with('-') {
            rest = &rest[1..];
        }
        rest = rest.trim_start();
        let Some((delimiter, tail)) = read_shell_word(rest) else {
            break;
        };
        if !delimiter.is_empty() {
            delimiters.push(delimiter);
        }
        rest = tail;
    }
    delimiters
}

fn read_shell_word(value: &str) -> Option<(String, &str)> {
    let mut word = String::new();
    let mut chars = value.char_indices().peekable();
    let mut end = 0;
    while let Some((index, ch)) = chars.next() {
        end = index + ch.len_utf8();
        match ch {
            '\'' | '"' => {
                let quote = ch;
                for (_, quoted_ch) in chars.by_ref() {
                    end += quoted_ch.len_utf8();
                    if quoted_ch == quote {
                        break;
                    }
                    word.push(quoted_ch);
                }
            }
            ch if ch.is_whitespace() || matches!(ch, ';' | '|' | '&' | '(' | ')') => {
                end = index;
                break;
            }
            _ => word.push(ch),
        }
    }
    (!word.is_empty()).then(|| (word, &value[end..]))
}

fn strip_powershell_here_string_bodies(script: &str) -> String {
    let mut rendered = String::new();
    let mut in_here_string: Option<&str> = None;
    for line in script.lines() {
        if let Some(terminator) = in_here_string {
            if line.trim_start().starts_with(terminator) {
                rendered.push_str(line);
                rendered.push('\n');
                in_here_string = None;
            }
            continue;
        }

        rendered.push_str(line);
        rendered.push('\n');
        let trimmed = line.trim_start();
        if trimmed.starts_with("@'") {
            in_here_string = Some("'@");
        } else if trimmed.starts_with("@\"") {
            in_here_string = Some("\"@");
        }
    }
    rendered
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
