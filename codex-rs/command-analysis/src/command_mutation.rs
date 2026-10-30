//! Read-only versus mutating classification of shell commands, from argv
//! tokens alone. codex-core's turn diff tracker and tool history consume it.

use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandMutation {
    ReadOnly,
    KnownMutation { paths: Option<BTreeSet<PathBuf>> },
    Uncertain,
}

impl CommandMutation {
    pub fn may_have_mutated(&self) -> bool {
        !matches!(self, Self::ReadOnly)
    }

    pub fn paths(&self) -> Option<&BTreeSet<PathBuf>> {
        match self {
            Self::KnownMutation { paths } => paths.as_ref(),
            Self::ReadOnly | Self::Uncertain => None,
        }
    }
}

impl From<bool> for CommandMutation {
    fn from(possible_mutation: bool) -> Self {
        if possible_mutation {
            Self::Uncertain
        } else {
            Self::ReadOnly
        }
    }
}

fn unwrap_command_tokens(tokens: &[String]) -> &[String] {
    let Some(first) = tokens.first().map(|token| command_basename(token)) else {
        return tokens;
    };
    if matches!(
        first,
        "pwsh" | "pwsh.exe" | "powershell" | "powershell.exe" | "cmd" | "cmd.exe"
    ) && let Some(position) = tokens
        .iter()
        .position(|token| matches!(token.as_str(), "-command" | "-c" | "/c"))
    {
        return &tokens[position.saturating_add(1)..];
    }
    if matches!(first, "bash" | "zsh" | "sh")
        && let Some(position) = tokens
            .iter()
            .position(|token| matches!(token.as_str(), "-c" | "-lc"))
    {
        return &tokens[position.saturating_add(1)..];
    }
    tokens
}

fn is_format_only_tokens(tokens: &[String]) -> bool {
    matches!(
        tokens,
        [first, second, ..] if matches!(command_basename(first), "just" | "cargo") && second == "fmt"
    ) || matches!(
        tokens.first().map(|token| command_basename(token)),
        Some("rustfmt" | "prettier")
    )
}

fn is_read_only_format_command(command: &[String], tokens: &[String]) -> bool {
    if command
        .iter()
        .any(|token| token.contains([';', '&', '|', '>', '<', '`', '$', '\n', '\r']))
    {
        return false;
    }
    let Some(program) = tokens.first().map(|token| command_basename(token)) else {
        return false;
    };
    // Help and version queries print and exit without formatting any operand.
    // Tokens are lowercased, so `-V` is indistinguishable from verbose `-v`.
    let is_help_or_version = |arg: &String| matches!(arg.as_str(), "--help" | "-h" | "--version");
    match program {
        "prettier" => {
            !tokens
                .iter()
                .any(|arg| matches!(arg.as_str(), "--write" | "-w"))
                && tokens.iter().any(|arg| {
                    matches!(arg.as_str(), "--check" | "-c" | "--list-different" | "-l")
                        || is_help_or_version(arg)
                })
        }
        // A just recipe is user-defined; its --check argument is not proof of
        // read-only behavior. Let the ordinary workspace observation decide.
        "cargo" | "rustfmt" => {
            is_format_only_tokens(tokens)
                && tokens
                    .iter()
                    .any(|arg| arg == "--check" || is_help_or_version(arg))
        }
        _ => false,
    }
}

fn looks_like_mutating_command(command: &[String]) -> bool {
    // Preserve shell syntax and quoted arguments before considering individual
    // executables. A read-only git command must not hide a later write or redirect.
    if let Some((_, script)) = codex_shell_command::bash::extract_bash_command(command) {
        return codex_shell_command::bash::parse_shell_script_into_commands(script).is_none_or(
            |commands| {
                commands
                    .iter()
                    .any(|argv| looks_like_mutating_command(argv))
            },
        );
    }
    let normalized = normalized_command_tokens(command);
    let unwrapped = unwrap_command_tokens(&normalized);
    if codex_shell_command::is_safe_command::is_known_safe_command(command)
        || is_read_only_powershell_command(command)
        || is_direct_file_read_command(command, unwrapped)
        || is_read_only_format_command(command, unwrapped)
    {
        return false;
    }
    if command.first().is_some_and(|program| {
        matches!(
            command_basename(&program.to_ascii_lowercase()),
            "bash" | "sh" | "zsh" | "pwsh" | "powershell" | "cmd"
        )
    }) {
        // The shell-aware readers above did not establish that the full script
        // is read-only. Do not classify it from a flattened argv prefix.
        return true;
    }
    let mutating_format =
        is_format_only_tokens(unwrapped) && !unwrapped.iter().any(|token| token == "--check");
    let mutating_just_recipe = matches!(
        unwrapped,
        [first, second, ..]
            if first == "just"
                && matches!(second.as_str(), "fix" | "fix-lane" | "fix-workspace" | "fmt")
    );
    if mutating_format || mutating_just_recipe {
        return true;
    }
    if let Some(subcommand) = git_subcommand(unwrapped) {
        if matches!(
            subcommand,
            "add"
                | "apply"
                | "checkout"
                | "clean"
                | "commit"
                | "merge"
                | "mv"
                | "rebase"
                | "reset"
                | "restore"
                | "rm"
                | "switch"
        ) {
            return true;
        }
        if git_argv_is_read_only(command, subcommand, true) {
            return false;
        }
    }

    // Mutation tracking fails closed for every command that ordinary safety
    // classification has not proven read-only.
    true
}

fn is_direct_file_read_command(command: &[String], unwrapped: &[String]) -> bool {
    if !matches!(unwrapped, [program, paths @ ..] if program == "type" && !paths.is_empty()) {
        return false;
    }
    let Some(program) = command.first().map(|token| command_basename(token)) else {
        return false;
    };
    if !program.eq_ignore_ascii_case("cmd") {
        return false;
    }
    // AutoRun and environment/delayed expansion can execute commands not
    // present in the literal `type` script. Only the plain /d form is proven.
    command
        .get(1)
        .is_some_and(|arg| arg.eq_ignore_ascii_case("/d"))
        && command
            .get(2)
            .is_some_and(|arg| arg.eq_ignore_ascii_case("/c"))
        && simple_wrapped_script_argv(command).is_some()
}

fn is_read_only_powershell_command(command: &[String]) -> bool {
    // The safety check and this consumer share the cached native AST parse.
    // Never reinterpret quoted source with a second shell tokenizer.
    codex_shell_command::powershell::parse_powershell_command_into_plain_commands(command)
        .is_some_and(|commands| {
            !commands.is_empty()
                && commands.iter().all(|argv| {
                    let Some(program) = argv.first().map(|word| word.to_ascii_lowercase()) else {
                        return false;
                    };
                    if matches!(program.as_str(), "get-command" | "gcm") {
                        return false;
                    }
                    codex_shell_command::is_safe_command::is_safe_powershell_words(argv)
                || matches!(program.as_str(),
                    "compare-object" | "convertfrom-json" | "convertto-json"
                    | "format-list" | "fl" | "format-table" | "ft"
                    | "get-date" | "get-member" | "get-variable" | "get-filehash"
                    | "get-process" | "join-path" | "out-string" | "out-null"
                    | "sort-object" | "group-object" | "split-path"
                    | "write-information" | "write-verbose" | "write-warning"
                    | "where-object" | "where" | "?")
                // The AST parser admits only pure script blocks and removes
                // them from semantic argv. Member-invocation forms stay opaque.
                || (matches!(program.as_str(), "foreach-object" | "%") && argv.len() == 1)
                || !looks_like_mutating_command(argv)
                })
        })
}

pub fn script_is_read_only(script: &str) -> bool {
    let bash = ["bash".to_string(), "-lc".to_string(), script.to_string()];
    if command_mutation(&bash, None) == CommandMutation::ReadOnly {
        return true;
    }
    let powershell = [
        "pwsh".to_string(),
        "-NoProfile".to_string(),
        "-Command".to_string(),
        script.to_string(),
    ];
    command_mutation(&powershell, None) == CommandMutation::ReadOnly
}

pub fn command_mutation(command: &[String], cwd: Option<&Path>) -> CommandMutation {
    if !looks_like_mutating_command(command) {
        return CommandMutation::ReadOnly;
    }
    let paths = mutating_command_paths(command, cwd);
    if paths.is_some() || is_known_mutating_command(command) {
        CommandMutation::KnownMutation { paths }
    } else {
        CommandMutation::Uncertain
    }
}

fn is_known_mutating_command(command: &[String]) -> bool {
    let normalized = normalized_command_tokens(command);
    let unwrapped = unwrap_command_tokens(&normalized);
    if (is_format_only_tokens(unwrapped) && !unwrapped.iter().any(|token| token == "--check"))
        || matches!(
            unwrapped,
            [first, second, ..]
                if first == "just"
                    && matches!(second.as_str(), "fix" | "fix-lane" | "fix-workspace" | "fmt")
        )
    {
        return true;
    }
    if matches!(
        git_subcommand(unwrapped),
        Some(
            "add"
                | "apply"
                | "checkout"
                | "clean"
                | "commit"
                | "merge"
                | "mv"
                | "rebase"
                | "reset"
                | "restore"
                | "rm"
                | "switch"
        )
    ) {
        return true;
    }
    let tokens = shell_filter_tokens(&command.join(" ").to_ascii_lowercase());
    if tokens.iter().any(|token| {
        matches!(
            command_basename(token),
            "apply_patch"
                | "add-content"
                | "chmod"
                | "chown"
                | "chgrp"
                | "copy-item"
                | "cp"
                | "dd"
                | "del"
                | "erase"
                | "md"
                | "mkdir"
                | "move-item"
                | "mv"
                | "new-item"
                | "ni"
                | "out-file"
                | "patch"
                | "rd"
                | "reg"
                | "remove-item"
                | "ren"
                | "rename-item"
                | "rm"
                | "rmdir"
                | "rsync"
                | "set-content"
                | "set-item"
                | "set-itemproperty"
                | "tee"
                | "tee-object"
                | "touch"
                | "truncate"
        )
    }) || tokens_contain_redirect(&tokens)
    {
        return true;
    }
    // Invoking an interpreter is not proof that its script writes anything.
    // Unknown scripts still invalidate conservatively when observation fails.
    false
}

pub fn command_reads_repository_history(command: &[String]) -> bool {
    if command
        .iter()
        .any(|token| token.contains([';', '&', '|', '>', '<', '`', '$', '\n', '\r']))
    {
        return false;
    }
    let normalized = normalized_command_tokens(command);
    let unwrapped = unwrap_command_tokens(&normalized);
    matches!(git_subcommand(unwrapped), Some("log" | "show" | "shortlog"))
        && !unwrapped
            .iter()
            .any(|token| matches!(token.as_str(), "-g" | "--walk-reflogs" | "--reflog"))
}

fn git_subcommand(tokens: &[String]) -> Option<&str> {
    if command_basename(tokens.first()?) != "git" {
        return None;
    }
    let mut index = 1;
    while let Some(token) = tokens.get(index).map(String::as_str) {
        if matches!(
            token,
            "-c" | "-C" | "--git-dir" | "--work-tree" | "--namespace" | "--exec-path"
        ) {
            index = index.saturating_add(2);
            continue;
        }
        // Approval requires `--no-optional-locks` before `status`, so the
        // only certifiable status spelling must still resolve its subcommand.
        if matches!(
            token,
            "--no-pager"
                | "--no-optional-locks"
                | "--paginate"
                | "-p"
                | "--literal-pathspecs"
                | "--no-literal-pathspecs"
                | "--glob-pathspecs"
                | "--noglob-pathspecs"
                | "--icase-pathspecs"
        ) || token.starts_with("--git-dir=")
            || (token.starts_with("-c") && token.len() > 2)
            || token.starts_with("--work-tree=")
            || token.starts_with("--namespace=")
            || token.starts_with("--exec-path=")
            || token.starts_with("--config-env=")
        {
            index = index.saturating_add(1);
            continue;
        }
        return (!token.starts_with('-')).then_some(token);
    }
    None
}

/// True for a plain `git` invocation whose subcommand only reads repository state.
pub fn command_is_read_only_git(command: &[String]) -> bool {
    if command
        .iter()
        .any(|token| token.contains([';', '&', '|', '>', '<', '`', '$', '\n', '\r']))
    {
        return false;
    }
    git_subcommand(command)
        .is_some_and(|subcommand| git_argv_is_read_only(command, subcommand, false))
}

fn git_argv_is_read_only(command: &[String], subcommand: &str, allow_cwd_change: bool) -> bool {
    // Preserve known presentation-only configuration, not arbitrary executable
    // overrides. Keep exact argv case: -C changes cwd, whereas -c sets config.
    let removable = match command.get(1).map(String::as_str) {
        Some("-ccolor.ui=false" | "-ccore.pager=cat") => 1,
        Some("-c")
            if command
                .get(2)
                .is_some_and(|arg| matches!(arg.as_str(), "color.ui=false" | "core.pager=cat")) =>
        {
            2
        }
        // Changing cwd does not make a read mutate, but its output cannot be
        // attributed to the caller's cwd without resolving that change.
        Some("-C") if allow_cwd_change && command.get(2).is_some() => 2,
        _ => 0,
    };
    if removable > 0 {
        let mut stripped = command.to_vec();
        stripped.drain(1..=removable);
        return git_argv_is_read_only(&stripped, subcommand, allow_cwd_change);
    }
    if matches!(subcommand, "diff" | "log" | "show" | "status") {
        // These commands can write with --output or invoke external programs.
        // Do not override the safety owner's rejection with a subcommand label.
        codex_shell_command::is_safe_command::is_known_safe_direct_argv(command)
    } else {
        // Unknown options can run pagers, upload helpers, or content filters.
        // Preserve ordinary operand-only readers, but do not certify arbitrary
        // global configuration or unsupported subcommand options as read-only.
        is_read_only_git_subcommand(subcommand)
            && command.get(1).is_some_and(|arg| arg == subcommand)
            && (subcommand == "status" || command.iter().skip(2).all(|arg| !arg.starts_with('-')))
    }
}

fn is_read_only_git_subcommand(subcommand: &str) -> bool {
    matches!(
        subcommand,
        "blame"
            | "cat-file"
            | "describe"
            | "diff"
            | "for-each-ref"
            | "grep"
            | "log"
            | "ls-files"
            | "ls-tree"
            | "name-rev"
            | "rev-parse"
            | "rev-list"
            | "shortlog"
            | "show"
            | "status"
    )
}


fn mutating_command_paths(command: &[String], cwd: Option<&Path>) -> Option<BTreeSet<PathBuf>> {
    // Windows exec wraps every command as `pwsh -Command <script>`; without
    // unwrapping, no wrapped mutator could ever report exact paths.
    if let Some(script_argv) = simple_wrapped_script_argv(command) {
        return mutating_command_paths(&script_argv, cwd);
    }
    let program = command.first()?.to_ascii_lowercase();
    let program = command_basename(&program);
    let args = &command[1..];
    if args.iter().any(|arg| {
        arg.contains(['*', '?', '|', '>', '<', ';', '&', '$', '`']) || arg.starts_with('@')
    }) {
        return None;
    }

    let operands = match program {
        "touch" | "truncate" | "mkdir" | "md" | "rmdir" | "rd" | "rm" | "del" | "erase" => {
            simple_path_operands(args)?
        }
        "cp" | "mv" => {
            let paths = simple_path_operands(args)?;
            (paths.len() >= 2).then_some(paths)?
        }
        "chmod" | "chown" | "chgrp" => {
            let operands = simple_path_operands(args)?;
            (operands.len() >= 2).then(|| operands.into_iter().skip(1).collect())?
        }
        // rustfmt can recursively rewrite out-of-line modules, including ones
        // discovered through configuration. Its argv is not an exact write set.
        "rustfmt" => return None,
        "prettier" if !args.iter().any(|arg| arg == "--check") => simple_path_operands(args)?,
        _ => return None,
    };
    let paths = operands
        .into_iter()
        .map(PathBuf::from)
        .map(|path| {
            if path.is_absolute() {
                Some(path)
            } else {
                cwd.map(|cwd| cwd.join(path))
            }
        })
        .collect::<Option<BTreeSet<_>>>()?;
    (!paths.is_empty()).then_some(paths)
}

/// The argv of a shell wrapper whose single script is one plain command: no
/// quoting, expansion, wildcard, list, redirect or comment syntax, so splitting
/// on whitespace yields exactly the words the shell passes to the program.
fn simple_wrapped_script_argv(command: &[String]) -> Option<Vec<String>> {
    let program = command.first()?.to_ascii_lowercase();
    let (script_flags, posix): (&[&str], bool) = match command_basename(&program) {
        "pwsh" | "powershell" => (&["-command", "-c"], false),
        "cmd" => (&["/c"], false),
        "bash" | "sh" | "zsh" => (&["-c", "-lc"], true),
        _ => return None,
    };
    let position = command
        .iter()
        .position(|arg| script_flags.contains(&arg.to_ascii_lowercase().as_str()))?;
    let [script] = &command[position.saturating_add(1)..] else {
        return None;
    };
    if command_basename(&program) == "cmd"
        && (position != 2 || !command[1].eq_ignore_ascii_case("/d"))
    {
        return None;
    }
    if script.contains([
        '\'', '"', '`', '$', '*', '?', '[', ']', '{', '}', '(', ')', '|', '&', ';', '<', '>', '@',
        '#', ',', '%', '~', '!', '^', '\n', '\r',
    ]) || (posix && script.contains('\\'))
    {
        return None;
    }
    let argv = script
        .split_whitespace()
        .map(str::to_string)
        .collect::<Vec<_>>();
    // A nested wrapper is not one plain command.
    (!argv.is_empty() && simple_wrapped_script_argv(&argv).is_none()).then_some(argv)
}

fn simple_path_operands(args: &[String]) -> Option<Vec<String>> {
    let mut operands = Vec::new();
    let mut options_ended = false;
    for arg in args {
        if !options_ended && arg == "--" {
            options_ended = true;
        } else if !options_ended && arg.starts_with('-') {
            if !matches!(
                arg.as_str(),
                "-f" | "--force"
                    | "-r"
                    | "-R"
                    | "--recursive"
                    | "-p"
                    | "--parents"
                    | "-v"
                    | "--verbose"
            ) {
                return None;
            }
        } else {
            operands.push(arg.clone());
        }
    }
    Some(operands)
}

fn normalized_command_tokens(command: &[String]) -> Vec<String> {
    // Structured argv already owns its boundaries. Split only a proven plain
    // shell script, never filenames such as `a --check b.rs`.
    let script = simple_wrapped_script_argv(command);
    script
        .as_deref()
        .unwrap_or(command)
        .iter()
        .map(|part| part.to_ascii_lowercase())
        .filter(|token| !token.is_empty())
        .collect()
}

fn command_basename(token: &str) -> &str {
    let basename = token.rsplit(['/', '\\']).next().unwrap_or(token);
    basename.strip_suffix(".exe").unwrap_or(basename)
}

fn shell_filter_tokens(command: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    for ch in command.chars() {
        match quote {
            Some(q) if ch == q => {
                quote = None;
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            Some(_) => current.push(ch),
            None if matches!(ch, '\'' | '"') => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
                quote = Some(ch);
            }
            None if ch.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            None => current.push(ch),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// True when `tokens` contain a shell output-redirection operator.
///
/// A redirect is an operator that *begins* a token: `> file`, `>file`, `>>file`,
/// `2>err`, `&>both`. A `>` in the interior of a token is not a redirect — it is a
/// comparison or arrow inside argument data (`if i > 3`, `a->b`). Matching on the
/// operator's position, rather than scanning the raw joined command for any `>`
/// character, keeps real redirects (which always write) while leaving such data
/// alone. `shell_filter_tokens` splits on unquoted whitespace, so a `>` preceded
/// by non-space text lands mid-token and is correctly ignored.
fn tokens_contain_redirect(tokens: &[String]) -> bool {
    tokens.iter().any(|token| {
        // Strip a leading stream descriptor (e.g. `1`, `2`, `&`) so `2>`/`&>` match.
        let rest = token.trim_start_matches(|ch: char| ch.is_ascii_digit() || ch == '&');
        // Only a token that STARTS with the operator is a redirect; this covers both
        // the spaced (`> out.txt`) and attached (`>out.txt`) forms.
        rest.starts_with('>')
    })
}

