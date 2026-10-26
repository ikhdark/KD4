use crate::bash::extract_bash_command;
use crate::bash::try_parse_shell;
use crate::bash::try_parse_word_only_commands_with_operators;
use crate::powershell::extract_trusted_noprofile_powershell_command;
use codex_protocol::parse_command::ParsedCommand;
use shlex::split as shlex_split;
use shlex::try_join as shlex_try_join;
use std::path::PathBuf;

pub fn shlex_join(tokens: &[String]) -> String {
    shlex_try_join(tokens.iter().map(String::as_str))
        .unwrap_or_else(|_| "<command included NUL byte>".to_string())
}

/// Extracts a display-safe shell wrapper and script. PowerShell wrappers are collapsed only when
/// profiles are disabled and the requested executable resolves to the trusted host.
pub fn extract_shell_command(command: &[String]) -> Option<(&str, &str)> {
    extract_bash_command(command).or_else(|| extract_trusted_noprofile_powershell_command(command))
}

/// Parses metadata out of an arbitrary command.
/// These commands are model driven and could include just about anything.
/// The parsing is slightly lossy due to the ~infinite expressiveness of an arbitrary command.
/// The goal of the parsed metadata is to be able to provide the user with a human readable gis
/// of what it is doing.
pub fn parse_command(command: &[String]) -> Vec<ParsedCommand> {
    // Parse and then collapse consecutive duplicate commands to avoid redundant summaries.
    let mut parsed = parse_command_impl(command);
    if parsed
        .iter()
        .any(|cmd| matches!(cmd, ParsedCommand::Unknown { .. }))
    {
        vec![single_unknown_for_command(command)]
    } else {
        parsed.dedup();
        parsed
    }
}

fn single_unknown_for_command(command: &[String]) -> ParsedCommand {
    if let Some((_, shell_command)) = extract_shell_command(command) {
        ParsedCommand::Unknown {
            cmd: shell_command.to_string(),
        }
    } else {
        ParsedCommand::Unknown {
            cmd: shlex_join(command),
        }
    }
}


pub fn parse_command_impl(command: &[String]) -> Vec<ParsedCommand> {
    if let Some(commands) = parse_shell_lc_commands(command) {
        return commands;
    }

    if let Some((_, script)) = extract_trusted_noprofile_powershell_command(command) {
        return vec![ParsedCommand::Unknown {
            cmd: script.to_string(),
        }];
    }

    let normalized = normalize_tokens(command);

    if normalized.iter().any(|token| token == "cd")
        && normalized
            .iter()
            .any(|token| matches!(token.as_str(), "|" | "||" | ";"))
    {
        return vec![ParsedCommand::Unknown {
            cmd: shlex_join(command),
        }];
    }

    let operators = normalized
        .iter()
        .filter(|token| matches!(token.as_str(), "|" | "&&" | "||" | ";"))
        .cloned()
        .collect::<Vec<_>>();
    let parts = if contains_connectors(&normalized) {
        split_on_connectors(&normalized)
    } else {
        vec![normalized]
    };

    let parts = drop_small_formatting_commands(parts, &operators);

    // Preserve left-to-right execution order for all commands, including bash -c/-lc
    // so summaries reflect the order they will run.

    // Map each pipeline segment to its parsed summary, tracking `cd` to compute paths.
    let mut commands: Vec<ParsedCommand> = Vec::new();
    let mut cwd: Option<String> = None;
    for tokens in &parts {
        if let Some((head, tail)) = tokens.split_first()
            && head == "cd"
        {
            let Some(dir) = cd_target(tail) else {
                return vec![single_unknown_for_command(command)];
            };
            cwd = Some(match &cwd {
                Some(base) => join_paths(base, &dir),
                None => dir,
            });
            continue;
        }
        let parsed = summarize_main_tokens(tokens);
        let parsed = match parsed {
            ParsedCommand::Read { cmd, name, path } => {
                if let Some(base) = &cwd {
                    let full = join_paths(base, &path.to_string_lossy());
                    ParsedCommand::Read {
                        cmd,
                        name,
                        path: PathBuf::from(full),
                    }
                } else {
                    ParsedCommand::Read { cmd, name, path }
                }
            }
            other => other,
        };
        commands.push(parsed);
    }

    simplify_commands(&mut commands);

    commands
}

fn simplify_commands(commands: &mut Vec<ParsedCommand>) {
    let mut remaining = commands.len();
    let mut kept = false;
    commands.retain(|command| {
        let discard = remaining > 1
            && match command {
                ParsedCommand::Unknown { cmd } => shlex_split(cmd).is_some_and(|tokens| {
                    matches!(tokens.first().map(String::as_str), Some("true"))
                        || (!kept && tokens.first().is_some_and(|token| token == "echo"))
                }),
                _ => false,
            };
        if discard {
            remaining -= 1;
        } else {
            kept = true;
        }
        !discard
    });
}

/// Validates that this is a `sed -n 123,123p` command.
pub(crate) fn is_valid_sed_n_arg(arg: Option<&str>) -> bool {
    let s = match arg {
        Some(s) => s,
        None => return false,
    };
    let core = match s.strip_suffix('p') {
        Some(rest) => rest,
        None => return false,
    };
    let parts: Vec<&str> = core.split(',').collect();
    match parts.as_slice() {
        [num] => !num.is_empty() && num.chars().all(|c| c.is_ascii_digit()),
        [a, b] => {
            !a.is_empty()
                && !b.is_empty()
                && a.chars().all(|c| c.is_ascii_digit())
                && b.chars().all(|c| c.is_ascii_digit())
        }
        _ => false,
    }
}

fn sed_read_path(tokens: &[String]) -> Option<String> {
    let (path, formatter) = tokens.split_last()?;
    let formatter = formatter
        .strip_suffix(&["--".to_string()])
        .unwrap_or(formatter);
    (formatter.len() >= 3 && !path.starts_with('-') && is_small_sed_formatter(formatter))
        .then(|| path.clone())
}

/// Normalize a command by:
/// - Removing `yes`/`no`/`bash -c`/`bash -lc`/`zsh -c`/`zsh -lc` prefixes.
/// - Splitting on `|` and `&&`/`||`/`;
fn normalize_tokens(cmd: &[String]) -> Vec<String> {
    match cmd {
        [first, pipe, rest @ ..] if (first == "yes" || first == "y") && pipe == "|" => {
            // Do not re-shlex already-tokenized input; just drop the prefix.
            rest.to_vec()
        }
        [first, pipe, rest @ ..] if (first == "no" || first == "n") && pipe == "|" => {
            // Do not re-shlex already-tokenized input; just drop the prefix.
            rest.to_vec()
        }
        [shell, flag, script]
            if (shell == "bash" || shell == "zsh") && (flag == "-c" || flag == "-lc") =>
        {
            shlex_split(script).unwrap_or_else(|| vec![shell.clone(), flag.clone(), script.clone()])
        }
        _ => cmd.to_vec(),
    }
}

fn contains_connectors(tokens: &[String]) -> bool {
    tokens
        .iter()
        .any(|t| t == "&&" || t == "||" || t == "|" || t == ";")
}

fn split_on_connectors(tokens: &[String]) -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = Vec::new();
    let mut cur: Vec<String> = Vec::new();
    for t in tokens {
        if t == "&&" || t == "||" || t == "|" || t == ";" {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(t.clone());
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn trim_at_connector(tokens: &[String]) -> Vec<String> {
    let idx = tokens
        .iter()
        .position(|t| t == "|" || t == "&&" || t == "||" || t == ";")
        .unwrap_or(tokens.len());
    tokens[..idx].to_vec()
}

/// Shorten a path to the last component, excluding `build`/`dist`/`node_modules`/`src`.
/// It also pulls out a useful path from a directory such as:
/// - webview/src -> webview
/// - foo/src/ -> foo
/// - packages/app/node_modules/ -> app
fn short_display_path(path: &str) -> String {
    // Normalize separators and drop any trailing slash for display.
    let normalized = path.replace('\\', "/");
    let trimmed = normalized.trim_end_matches('/');
    let mut parts = trimmed.split('/').rev().filter(|p| {
        !p.is_empty() && *p != "build" && *p != "dist" && *p != "node_modules" && *p != "src"
    });
    parts
        .next()
        .map(str::to_string)
        .unwrap_or_else(|| trimmed.to_string())
}

// Skip values consumed by specific flags and ignore --flag=value style arguments.
fn skip_flag_values<'a>(args: &'a [String], flags_with_vals: &[&str]) -> Vec<&'a String> {
    let mut out: Vec<&'a String> = Vec::new();
    let mut skip_next = false;
    for (i, a) in args.iter().enumerate() {
        if skip_next {
            skip_next = false;
            continue;
        }
        if a == "--" {
            // From here on, everything is positional operands; push the rest and break.
            for rest in &args[i + 1..] {
                out.push(rest);
            }
            break;
        }
        if a.starts_with("--") && a.contains('=') {
            // --flag=value form: treat as a flag taking a value; skip entirely.
            continue;
        }
        if flags_with_vals.contains(&a.as_str()) {
            // This flag consumes the next argument as its value.
            if i + 1 < args.len() {
                skip_next = true;
            }
            continue;
        }
        out.push(a);
    }
    out
}

fn first_non_flag_operand(args: &[String], flags_with_vals: &[&str]) -> Option<String> {
    positional_operands(args, flags_with_vals)
        .into_iter()
        .next()
        .cloned()
}

fn single_non_flag_operand(args: &[String], flags_with_vals: &[&str]) -> Option<String> {
    let mut operands = positional_operands(args, flags_with_vals).into_iter();
    let first = operands.next()?;
    if operands.next().is_some() {
        return None;
    }
    Some(first.clone())
}

fn positional_operands<'a>(args: &'a [String], flags_with_vals: &[&str]) -> Vec<&'a String> {
    let mut out = Vec::new();
    let mut after_double_dash = false;
    let mut skip_next = false;
    for (i, arg) in args.iter().enumerate() {
        if skip_next {
            skip_next = false;
            continue;
        }
        if after_double_dash {
            out.push(arg);
            continue;
        }
        if arg == "--" {
            after_double_dash = true;
            continue;
        }
        if arg.starts_with("--") && arg.contains('=') {
            continue;
        }
        if flags_with_vals.contains(&arg.as_str()) {
            if i + 1 < args.len() {
                skip_next = true;
            }
            continue;
        }
        if arg.starts_with('-') {
            continue;
        }
        out.push(arg);
    }
    out
}

fn parse_grep_like(main_cmd: &[String], args: &[String]) -> ParsedCommand {
    let args_no_connector = trim_at_connector(args);
    let mut operands = Vec::new();
    let mut pattern: Option<String> = None;
    let mut after_double_dash = false;
    let mut iter = args_no_connector.iter().peekable();
    while let Some(arg) = iter.next() {
        if after_double_dash {
            operands.push(arg);
            continue;
        }
        if arg == "--" {
            after_double_dash = true;
            continue;
        }
        match arg.as_str() {
            "-e" | "--regexp" => {
                if let Some(pat) = iter.next()
                    && pattern.is_none()
                {
                    pattern = Some(pat.clone());
                }
                continue;
            }
            "-f" | "--file" => {
                if let Some(pat_file) = iter.next()
                    && pattern.is_none()
                {
                    pattern = Some(pat_file.clone());
                }
                continue;
            }
            "-m" | "--max-count" | "-C" | "--context" | "-A" | "--after-context" | "-B"
            | "--before-context" => {
                iter.next();
                continue;
            }
            _ => {}
        }
        if arg.starts_with('-') {
            continue;
        }
        operands.push(arg);
    }
    // Do not shorten the query: grep patterns may legitimately contain slashes
    // and should be preserved verbatim. Only file operands use the path formatter.
    let has_pattern = pattern.is_some();
    let query = pattern.or_else(|| operands.first().cloned().map(String::from));
    let path_index = if has_pattern { 0 } else { 1 };
    let path = operands.get(path_index).map(|s| short_display_path(s));
    ParsedCommand::Search {
        cmd: shlex_join(main_cmd),
        query,
        path,
    }
}

fn awk_data_file_operand(tokens: &[String]) -> Option<String> {
    let (path, formatter) = tokens.split_last()?;
    (!path.starts_with('-') && is_small_awk_formatter(formatter)).then(|| path.clone())
}

fn cd_target(args: &[String]) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--" {
            i += 1;
            break;
        }
        if matches!(arg.as_str(), "-L" | "-P") {
            i += 1;
            continue;
        }
        if arg.starts_with('-') {
            return None;
        }
        break;
    }
    match &args[i..] {
        [target] if target != "-" => Some(target.clone()),
        _ => None,
    }
}

fn is_pathish(s: &str) -> bool {
    s == "."
        || s == ".."
        || s.starts_with("./")
        || s.starts_with("../")
        || s.contains('/')
        || s.contains('\\')
}

fn parse_fd_query_and_path(tail: &[String]) -> (Option<String>, Option<String>) {
    let args_no_connector = trim_at_connector(tail);
    // fd has several flags that take values (e.g., -t/--type, -e/--extension).
    // Skip those values when extracting positional operands.
    let candidates = skip_flag_values(
        &args_no_connector,
        &[
            "-t",
            "--type",
            "-e",
            "--extension",
            "-E",
            "--exclude",
            "--search-path",
        ],
    );
    let non_flags: Vec<&String> = candidates
        .into_iter()
        .filter(|p| !p.starts_with('-'))
        .collect();
    match non_flags.as_slice() {
        [one] => {
            if is_pathish(one) {
                (None, Some(short_display_path(one)))
            } else {
                (Some((*one).clone()), None)
            }
        }
        [q, p, ..] => (Some((*q).clone()), Some(short_display_path(p))),
        _ => (None, None),
    }
}

fn parse_find_query_and_path(tail: &[String]) -> (Option<String>, Option<String>) {
    let args_no_connector = trim_at_connector(tail);
    // First positional argument (excluding common unary operators) is the root path
    let mut path: Option<String> = None;
    for a in &args_no_connector {
        if !a.starts_with('-') && *a != "!" && *a != "(" && *a != ")" {
            path = Some(short_display_path(a));
            break;
        }
    }
    // Extract a common name/path/regex pattern if present
    let mut query: Option<String> = None;
    let mut i = 0;
    while i < args_no_connector.len() {
        let a = &args_no_connector[i];
        if a == "-name" || a == "-iname" || a == "-path" || a == "-regex" {
            if i + 1 < args_no_connector.len() {
                query = Some(args_no_connector[i + 1].clone());
            }
            break;
        }
        i += 1;
    }
    (query, path)
}

fn parse_shell_lc_commands(original: &[String]) -> Option<Vec<ParsedCommand>> {
    // Only handle bash/zsh here; PowerShell is stripped separately without bash parsing.
    let (_, script) = extract_bash_command(original)?;
    Some(parse_shell_script(script))
}

/// Parses command metadata from a Bash-compatible shell script.
pub fn parse_shell_script(script: &str) -> Vec<ParsedCommand> {
    if let Some(tree) = try_parse_shell(script)
        && let Some((all_commands, operators)) =
            try_parse_word_only_commands_with_operators(&tree, script)
        && !all_commands.is_empty()
    {
        let single_command_text = if all_commands.len() == 1 {
            shlex_join(&all_commands[0])
        } else {
            String::new()
        };
        let has_pipe = operators.iter().any(|op| op == "|");
        let has_sed_n = all_commands.iter().any(|words| {
            words.first().is_some_and(|word| word == "sed")
                && words.get(1).is_some_and(|word| word == "-n")
        });
        // Only a successful cd followed by && establishes the next command's cwd.
        if all_commands.iter().enumerate().any(|(index, words)| {
            words.first().is_some_and(|word| word == "cd")
                && (operators.get(index).is_none_or(|op| op != "&&")
                    || index
                        .checked_sub(1)
                        .and_then(|i| operators.get(i))
                        .is_some_and(|op| op == "|")
                    || operators.iter().any(|op| matches!(op.as_str(), "||" | ";"))
                    || cd_target(&words[1..]).is_none())
        }) {
            return vec![ParsedCommand::Unknown {
                cmd: script.to_string(),
            }];
        }
        // Strip small formatting helpers (e.g., head/tail/awk/wc/etc) so we
        // bias toward the primary command when pipelines are present.
        // First, drop obvious small formatting helpers (e.g., wc/awk/etc).
        let had_multiple_commands = all_commands.len() > 1;
        // Commands arrive in source order; drop formatting helpers while preserving it.
        let filtered_commands = drop_small_formatting_commands(all_commands, &operators);
        if filtered_commands.is_empty() {
            return vec![ParsedCommand::Unknown {
                cmd: script.to_string(),
            }];
        }
        // Build parsed commands, tracking `cd` segments to compute effective file paths.
        let mut commands: Vec<ParsedCommand> = Vec::new();
        let mut cwd: Option<String> = None;
        for tokens in filtered_commands.into_iter() {
            if let Some((head, tail)) = tokens.split_first()
                && head == "cd"
            {
                if let Some(dir) = cd_target(tail) {
                    cwd = Some(match &cwd {
                        Some(base) => join_paths(base, &dir),
                        None => dir.clone(),
                    });
                }
                continue;
            }
            let parsed = summarize_main_tokens(&tokens);
            let parsed = match parsed {
                ParsedCommand::Read { cmd, name, path } => {
                    if let Some(base) = &cwd {
                        let full = join_paths(base, &path.to_string_lossy());
                        ParsedCommand::Read {
                            cmd,
                            name,
                            path: PathBuf::from(full),
                        }
                    } else {
                        ParsedCommand::Read { cmd, name, path }
                    }
                }
                other => other,
            };
            commands.push(parsed);
        }

        simplify_commands(&mut commands);
        if commands.len() == 1 {
            // If we reduced to a single command, attribute the full original script
            // for clearer UX in file-reading and listing scenarios, or when there were
            // no connectors in the original script. For pipeline commands (e.g.
            // `rg --files | sed -n`), keep only the primary command.
            let had_connectors = had_multiple_commands;
            commands = commands
                .into_iter()
                .map(|pc| match pc {
                    ParsedCommand::Read { name, cmd, path } => {
                        if had_connectors {
                            if has_pipe && has_sed_n {
                                ParsedCommand::Read {
                                    cmd: script.to_string(),
                                    name,
                                    path,
                                }
                            } else {
                                ParsedCommand::Read { cmd, name, path }
                            }
                        } else {
                            ParsedCommand::Read {
                                cmd: single_command_text.clone(),
                                name,
                                path,
                            }
                        }
                    }
                    ParsedCommand::ListFiles { path, cmd, .. } => {
                        if had_connectors {
                            ParsedCommand::ListFiles { cmd, path }
                        } else {
                            ParsedCommand::ListFiles {
                                cmd: single_command_text.clone(),
                                path,
                            }
                        }
                    }
                    ParsedCommand::Search {
                        query, path, cmd, ..
                    } => {
                        if had_connectors {
                            ParsedCommand::Search { cmd, query, path }
                        } else {
                            ParsedCommand::Search {
                                cmd: single_command_text.clone(),
                                query,
                                path,
                            }
                        }
                    }
                    other => other,
                })
                .collect();
        }
        return commands;
    }
    vec![ParsedCommand::Unknown {
        cmd: script.to_string(),
    }]
}

/// Return true if this looks like a small formatting helper in a pipeline.
/// Examples include `head -n 40`, `tail -n +10`, `wc -l`, and narrowly allowlisted
/// `sort`/`tee`/`awk`/`sed` forms.
/// We try to keep variants that clearly include a file path (e.g. `tail -n 30 file`).
fn is_small_formatting_command(tokens: &[String]) -> bool {
    if tokens.is_empty() {
        return false;
    }
    let cmd = tokens[0].as_str();
    match cmd {
        "tr" | "yes" | "printf" => true,
        // File operands on these utilities would describe another input, not
        // merely reformat the preceding stage's stream.
        "cut" | "column" => tokens.len() == 1,
        "nl" => tokens
            .iter()
            .skip(1)
            .all(|arg| matches!(arg.as_str(), "-ba" | "-bt" | "-bn")),
        "wc" => tokens
            .iter()
            .skip(1)
            .all(|arg| matches!(arg.as_str(), "-l" | "-w" | "-c" | "-m" | "-L")),
        "uniq" => tokens
            .iter()
            .skip(1)
            .all(|arg| matches!(arg.as_str(), "-c" | "-d" | "-u" | "-i" | "-z")),
        "sort" => is_small_sort_formatter(tokens),
        "tee" => is_small_tee_formatter(tokens),
        "xargs" => tokens.len() == 1,
        "awk" => is_small_awk_formatter(tokens),
        "head" => {
            // Treat as formatting when no explicit file operand is present.
            // Common forms: `head -n 40`, `head -c 100`.
            // Keep cases like `head -n 40 file`.
            match tokens {
                // `head`
                [_] => true,
                // `head <file>` or `head -n50`/`head -c100`
                [_, arg] => arg.starts_with('-'),
                // `head -n 40` / `head -c 100` (no file operand)
                [_, flag, count]
                    if (flag == "-n" || flag == "-c")
                        && count.chars().all(|c| c.is_ascii_digit()) =>
                {
                    true
                }
                _ => false,
            }
        }
        "tail" => {
            // Treat as formatting when no explicit file operand is present.
            // Common forms: `tail -n +10`, `tail -n 30`, `tail -c 100`.
            // Keep cases like `tail -n 30 file`.
            match tokens {
                // `tail`
                [_] => true,
                // `tail <file>` or `tail -n30`/`tail -n+10`
                [_, arg] => arg.starts_with('-'),
                // `tail -n 30` / `tail -n +10` (no file operand)
                [_, flag, count]
                    if flag == "-n"
                        && (count.chars().all(|c| c.is_ascii_digit())
                            || (count.starts_with('+')
                                && count[1..].chars().all(|c| c.is_ascii_digit()))) =>
                {
                    true
                }
                // `tail -c 100` / `tail -c +10` (no file operand)
                [_, flag, count]
                    if flag == "-c"
                        && (count.chars().all(|c| c.is_ascii_digit())
                            || (count.starts_with('+')
                                && count[1..].chars().all(|c| c.is_ascii_digit()))) =>
                {
                    true
                }
                _ => false,
            }
        }
        "sed" => is_small_sed_formatter(tokens),
        _ => false,
    }
}

fn is_small_sort_formatter(tokens: &[String]) -> bool {
    tokens.iter().skip(1).all(|token| match token.as_str() {
        "--dictionary-order"
        | "--fold-case"
        | "--general-numeric-sort"
        | "--human-numeric-sort"
        | "--ignore-leading-blanks"
        | "--ignore-nonprinting"
        | "--month-sort"
        | "--numeric-sort"
        | "--random-sort"
        | "--reverse"
        | "--stable"
        | "--unique"
        | "--version-sort"
        | "--zero-terminated" => true,
        short if short.starts_with('-') && !short.starts_with("--") => {
            short.chars().skip(1).all(|flag| {
                matches!(
                    flag,
                    'b' | 'd'
                        | 'f'
                        | 'g'
                        | 'h'
                        | 'i'
                        | 'M'
                        | 'n'
                        | 'R'
                        | 'r'
                        | 's'
                        | 'u'
                        | 'V'
                        | 'z'
                )
            })
        }
        _ => false,
    })
}

fn is_small_tee_formatter(tokens: &[String]) -> bool {
    tokens.iter().skip(1).all(|token| {
        matches!(
            token.as_str(),
            "-i" | "-p" | "--ignore-interrupts" | "--output-error"
        ) || token.starts_with("--output-error=")
    })
}

fn is_small_awk_formatter(tokens: &[String]) -> bool {
    let [_, program] = tokens else {
        return false;
    };
    let compact: String = program.chars().filter(|ch| !ch.is_whitespace()).collect();
    if compact == "{print}" {
        return true;
    }
    compact
        .strip_prefix("{print$")
        .and_then(|rest| rest.strip_suffix('}'))
        .is_some_and(|field| !field.is_empty() && field.chars().all(|ch| ch.is_ascii_digit()))
}

fn is_small_sed_formatter(tokens: &[String]) -> bool {
    match tokens {
        [_] => true,
        [_, flag] if flag == "-n" => true,
        [_, flag, script] if flag == "-n" => is_valid_sed_n_arg(Some(script)),
        [_, first, second, script]
            if matches!(first.as_str(), "-n" | "-e")
                && matches!(second.as_str(), "-n" | "-e")
                && first != second =>
        {
            is_valid_sed_n_arg(Some(script))
        }
        _ => false,
    }
}

fn drop_small_formatting_commands(
    commands: Vec<Vec<String>>,
    operators: &[String],
) -> Vec<Vec<String>> {
    commands
        .into_iter()
        .enumerate()
        .filter_map(|(index, tokens)| {
            let in_pipeline = operators.get(index).is_some_and(|op| op == "|")
                || index
                    .checked_sub(1)
                    .and_then(|i| operators.get(i))
                    .is_some_and(|op| op == "|");
            (!in_pipeline || !is_small_formatting_command(&tokens)).then_some(tokens)
        })
        .collect()
}

fn has_effectful_option(args: &[String], short: char, long: &[&str], value_options: &[&str]) -> bool {
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        if arg == "--" { break; }
        let flag = arg.split_once('=').map_or(arg.as_str(), |(flag, _)| flag);
        if long.contains(&flag) { return true; }
        if value_options.contains(&arg.as_str()) { index += 2; continue; }
        if arg.starts_with('-') && !arg.starts_with("--") {
            for ch in arg[1..].chars() {
                if ch == short { return true; }
                if value_options.iter().any(|value| value.len() == 2 && value.ends_with(ch)) { break; }
            }
        }
        index += 1;
    }
    false
}

fn summarize_main_tokens(main_cmd: &[String]) -> ParsedCommand {
    if let Some(head) = main_cmd.first()
        && ((head == "find"
            && !crate::command_safety::is_safe_command::is_known_safe_direct_argv(main_cmd))
            || (matches!(head.as_str(), "rg" | "rga" | "ripgrep-all")
                && !crate::command_safety::is_safe_command::is_safe_ripgrep(main_cmd)))
    {
        return ParsedCommand::Unknown {
            cmd: shlex_join(main_cmd),
        };
    }
    match main_cmd.split_first() {
        Some((head, tail)) if matches!(head.as_str(), "ls" | "eza" | "exa") => {
            let flags_with_vals: &[&str] = match head.as_str() {
                "ls" => &[
                    "-I",
                    "-w",
                    "--block-size",
                    "--format",
                    "--time-style",
                    "--color",
                    "--quoting-style",
                ],
                "eza" | "exa" => &[
                    "-I",
                    "--ignore-glob",
                    "--color",
                    "--sort",
                    "--time-style",
                    "--time",
                ],
                _ => &[],
            };
            let path =
                first_non_flag_operand(tail, flags_with_vals).map(|p| short_display_path(&p));
            ParsedCommand::ListFiles {
                cmd: shlex_join(main_cmd),
                path,
            }
        }
        Some((head, tail)) if head == "tree" => {
            if has_effectful_option(tail, 'o', &[], &["-L", "-P", "-I", "--charset", "--filelimit", "--sort"]) {
                return ParsedCommand::Unknown { cmd: shlex_join(main_cmd) };
            }
            let path = first_non_flag_operand(
                tail,
                &["-L", "-P", "-I", "--charset", "--filelimit", "--sort"],
            )
            .map(|p| short_display_path(&p));
            ParsedCommand::ListFiles {
                cmd: shlex_join(main_cmd),
                path,
            }
        }
        Some((head, tail)) if head == "du" => {
            let path = first_non_flag_operand(
                tail,
                &[
                    "-d",
                    "--max-depth",
                    "-B",
                    "--block-size",
                    "--exclude",
                    "--time-style",
                ],
            )
            .map(|p| short_display_path(&p));
            ParsedCommand::ListFiles {
                cmd: shlex_join(main_cmd),
                path,
            }
        }
        Some((head, tail)) if head == "rg" || head == "rga" || head == "ripgrep-all" => {
            let args_no_connector = trim_at_connector(tail);
            let has_files_flag = args_no_connector.iter().any(|a| a == "--files");
            let candidates = skip_flag_values(
                &args_no_connector,
                &[
                    "-g",
                    "--glob",
                    "--iglob",
                    "-t",
                    "--type",
                    "--type-add",
                    "--type-not",
                    "-m",
                    "--max-count",
                    "-A",
                    "-B",
                    "-C",
                    "--context",
                    "--max-depth",
                ],
            );
            let non_flags: Vec<&String> = candidates
                .into_iter()
                .filter(|p| !p.starts_with('-'))
                .collect();
            if has_files_flag {
                let path = non_flags.first().map(|s| short_display_path(s));
                ParsedCommand::ListFiles {
                    cmd: shlex_join(main_cmd),
                    path,
                }
            } else {
                let query = non_flags.first().cloned().map(String::from);
                let path = non_flags.get(1).map(|s| short_display_path(s));
                ParsedCommand::Search {
                    cmd: shlex_join(main_cmd),
                    query,
                    path,
                }
            }
        }
        Some((head, tail)) if head == "git" => match tail.split_first() {
            Some((subcmd, sub_tail)) if subcmd == "grep" => {
                if has_effectful_option(sub_tail, 'O', &["--open-files-in-pager"], &["-e", "--regexp", "-f", "--file", "-A", "-B", "-C", "-m", "--max-count", "--after-context", "--before-context", "--context", "--threads", "--max-depth"]) {
                    ParsedCommand::Unknown { cmd: shlex_join(main_cmd) }
                } else {
                    parse_grep_like(main_cmd, sub_tail)
                }
            },
            Some((subcmd, sub_tail)) if subcmd == "ls-files" => {
                let path = first_non_flag_operand(
                    sub_tail,
                    &["--exclude", "--exclude-from", "--pathspec-from-file"],
                )
                .map(|p| short_display_path(&p));
                ParsedCommand::ListFiles {
                    cmd: shlex_join(main_cmd),
                    path,
                }
            }
            _ => ParsedCommand::Unknown {
                cmd: shlex_join(main_cmd),
            },
        },
        Some((head, tail)) if head == "fd" => {
            if tail
                .iter()
                .take_while(|arg| arg.as_str() != "--")
                .any(|arg| {
                    matches!(arg.as_str(), "-x" | "--exec" | "-X" | "--exec-batch")
                        || arg.starts_with("--exec=")
                        || arg.starts_with("--exec-batch=")
                        || (arg.starts_with('-')
                            && !arg.starts_with("--")
                            && (arg.contains('x') || arg.contains('X')))
                })
            {
                return ParsedCommand::Unknown {
                    cmd: shlex_join(main_cmd),
                };
            }
            let (query, path) = parse_fd_query_and_path(tail);
            if query.is_some() {
                ParsedCommand::Search {
                    cmd: shlex_join(main_cmd),
                    query,
                    path,
                }
            } else {
                ParsedCommand::ListFiles {
                    cmd: shlex_join(main_cmd),
                    path,
                }
            }
        }
        Some((head, tail)) if head == "find" => {
            // Basic find support: capture path and common name filter
            let (query, path) = parse_find_query_and_path(tail);
            if query.is_some() {
                ParsedCommand::Search {
                    cmd: shlex_join(main_cmd),
                    query,
                    path,
                }
            } else {
                ParsedCommand::ListFiles {
                    cmd: shlex_join(main_cmd),
                    path,
                }
            }
        }
        Some((head, tail)) if matches!(head.as_str(), "grep" | "egrep" | "fgrep") => {
            parse_grep_like(main_cmd, tail)
        }
        Some((head, tail)) if matches!(head.as_str(), "ag" | "ack" | "pt") => {
            let args_no_connector = trim_at_connector(tail);
            let candidates = skip_flag_values(
                &args_no_connector,
                &[
                    "-G",
                    "-g",
                    "--file-search-regex",
                    "--ignore-dir",
                    "--ignore-file",
                    "--path-to-ignore",
                ],
            );
            let non_flags: Vec<&String> = candidates
                .into_iter()
                .filter(|p| !p.starts_with('-'))
                .collect();
            let query = non_flags.first().cloned().map(String::from);
            let path = non_flags.get(1).map(|s| short_display_path(s));
            ParsedCommand::Search {
                cmd: shlex_join(main_cmd),
                query,
                path,
            }
        }
        Some((head, tail)) if head == "cat" => {
            if let Some(path) = single_non_flag_operand(tail, &[]) {
                let name = short_display_path(&path);
                ParsedCommand::Read {
                    cmd: shlex_join(main_cmd),
                    name,
                    path: PathBuf::from(path),
                }
            } else {
                ParsedCommand::Unknown {
                    cmd: shlex_join(main_cmd),
                }
            }
        }
        Some((head, tail)) if matches!(head.as_str(), "bat" | "batcat") => {
            if let Some(path) = single_non_flag_operand(
                tail,
                &[
                    "--theme",
                    "--language",
                    "--style",
                    "--terminal-width",
                    "--tabs",
                    "--line-range",
                    "--map-syntax",
                ],
            ) {
                let name = short_display_path(&path);
                ParsedCommand::Read {
                    cmd: shlex_join(main_cmd),
                    name,
                    path: PathBuf::from(path),
                }
            } else {
                ParsedCommand::Unknown {
                    cmd: shlex_join(main_cmd),
                }
            }
        }
        Some((head, tail)) if head == "less" => {
            if let Some(path) = single_non_flag_operand(
                tail,
                &[
                    "-p",
                    "-P",
                    "-x",
                    "-y",
                    "-z",
                    "-j",
                    "--pattern",
                    "--prompt",
                    "--tabs",
                    "--shift",
                    "--jump-target",
                ],
            ) {
                let name = short_display_path(&path);
                ParsedCommand::Read {
                    cmd: shlex_join(main_cmd),
                    name,
                    path: PathBuf::from(path),
                }
            } else {
                ParsedCommand::Unknown {
                    cmd: shlex_join(main_cmd),
                }
            }
        }
        Some((head, tail)) if head == "more" => {
            if let Some(path) = single_non_flag_operand(tail, &[]) {
                let name = short_display_path(&path);
                ParsedCommand::Read {
                    cmd: shlex_join(main_cmd),
                    name,
                    path: PathBuf::from(path),
                }
            } else {
                ParsedCommand::Unknown {
                    cmd: shlex_join(main_cmd),
                }
            }
        }
        Some((head, tail)) if head == "head" => {
            // Support `head -n 50 file` and `head -n50 file` forms.
            let has_valid_n = match tail.split_first() {
                Some((first, rest)) if first == "-n" => rest
                    .first()
                    .is_some_and(|n| n.chars().all(|c| c.is_ascii_digit())),
                Some((first, _)) if first.starts_with("-n") => {
                    first[2..].chars().all(|c| c.is_ascii_digit())
                }
                _ => false,
            };
            if has_valid_n {
                // Build candidates skipping the numeric value consumed by `-n` when separated.
                let mut candidates: Vec<&String> = Vec::new();
                let mut i = 0;
                while i < tail.len() {
                    if i == 0 && tail[i] == "-n" && i + 1 < tail.len() {
                        let n = &tail[i + 1];
                        if n.chars().all(|c| c.is_ascii_digit()) {
                            i += 2;
                            continue;
                        }
                    }
                    candidates.push(&tail[i]);
                    i += 1;
                }
                if let Some(p) = candidates.into_iter().find(|p| !p.starts_with('-')) {
                    let path = p.clone();
                    let name = short_display_path(&path);
                    return ParsedCommand::Read {
                        cmd: shlex_join(main_cmd),
                        name,
                        path: PathBuf::from(path),
                    };
                }
            }
            if let [path] = tail
                && !path.starts_with('-')
            {
                let name = short_display_path(path);
                return ParsedCommand::Read {
                    cmd: shlex_join(main_cmd),
                    name,
                    path: PathBuf::from(path),
                };
            }
            ParsedCommand::Unknown {
                cmd: shlex_join(main_cmd),
            }
        }
        Some((head, tail)) if head == "tail" => {
            // Support `tail -n +10 file` and `tail -n+10 file` forms.
            let has_valid_n = match tail.split_first() {
                Some((first, rest)) if first == "-n" => rest.first().is_some_and(|n| {
                    let s = n.strip_prefix('+').unwrap_or(n);
                    !s.is_empty() && s.chars().all(|c| c.is_ascii_digit())
                }),
                Some((first, _)) if first.starts_with("-n") => {
                    let v = &first[2..];
                    let s = v.strip_prefix('+').unwrap_or(v);
                    !s.is_empty() && s.chars().all(|c| c.is_ascii_digit())
                }
                _ => false,
            };
            if has_valid_n {
                // Build candidates skipping the numeric value consumed by `-n` when separated.
                let mut candidates: Vec<&String> = Vec::new();
                let mut i = 0;
                while i < tail.len() {
                    if i == 0 && tail[i] == "-n" && i + 1 < tail.len() {
                        let n = &tail[i + 1];
                        let s = n.strip_prefix('+').unwrap_or(n);
                        if !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()) {
                            i += 2;
                            continue;
                        }
                    }
                    candidates.push(&tail[i]);
                    i += 1;
                }
                if let Some(p) = candidates.into_iter().find(|p| !p.starts_with('-')) {
                    let path = p.clone();
                    let name = short_display_path(&path);
                    return ParsedCommand::Read {
                        cmd: shlex_join(main_cmd),
                        name,
                        path: PathBuf::from(path),
                    };
                }
            }
            if let [path] = tail
                && !path.starts_with('-')
            {
                let name = short_display_path(path);
                return ParsedCommand::Read {
                    cmd: shlex_join(main_cmd),
                    name,
                    path: PathBuf::from(path),
                };
            }
            ParsedCommand::Unknown {
                cmd: shlex_join(main_cmd),
            }
        }
        Some((head, _)) if head == "awk" => {
            if let Some(path) = awk_data_file_operand(main_cmd) {
                let name = short_display_path(&path);
                ParsedCommand::Read {
                    cmd: shlex_join(main_cmd),
                    name,
                    path: PathBuf::from(path),
                }
            } else {
                ParsedCommand::Unknown {
                    cmd: shlex_join(main_cmd),
                }
            }
        }
        Some((head, tail)) if head == "nl" => {
            // Avoid treating option values as paths (e.g., nl -s "  ").
            let candidates = skip_flag_values(tail, &["-s", "-w", "-v", "-i", "-b"]);
            if let Some(p) = candidates.into_iter().find(|p| !p.starts_with('-')) {
                let path = p.clone();
                let name = short_display_path(&path);
                ParsedCommand::Read {
                    cmd: shlex_join(main_cmd),
                    name,
                    path: PathBuf::from(path),
                }
            } else {
                ParsedCommand::Unknown {
                    cmd: shlex_join(main_cmd),
                }
            }
        }
        Some((head, _)) if head == "sed" => {
            if let Some(path) = sed_read_path(main_cmd) {
                let name = short_display_path(&path);
                ParsedCommand::Read {
                    cmd: shlex_join(main_cmd),
                    name,
                    path: PathBuf::from(path),
                }
            } else {
                ParsedCommand::Unknown {
                    cmd: shlex_join(main_cmd),
                }
            }
        }
        // Other commands
        _ => ParsedCommand::Unknown {
            cmd: shlex_join(main_cmd),
        },
    }
}

fn is_abs_like(path: &str) -> bool {
    if std::path::Path::new(path).is_absolute() {
        return true;
    }
    let mut chars = path.chars();
    match (chars.next(), chars.next(), chars.next()) {
        // Windows drive path like C:\
        (Some(d), Some(':'), Some('\\')) if d.is_ascii_alphabetic() => return true,
        // UNC path like \\server\share
        (Some('\\'), Some('\\'), _) => return true,
        _ => {}
    }
    false
}

fn join_paths(base: &str, rel: &str) -> String {
    if is_abs_like(rel) {
        return rel.to_string();
    }
    if base.is_empty() {
        return rel.to_string();
    }
    let mut buf = PathBuf::from(base);
    buf.push(rel);
    buf.to_string_lossy().to_string()
}
