//! Read-only versus mutating classification of shell commands, from argv
//! tokens alone. codex-core's turn diff tracker and tool history consume it.

use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandMutation {
    ReadOnly,
    KnownMutation {
        paths: Option<BTreeSet<PathBuf>>,
    },
    /// A workspace snapshot changed, without proof that this command caused it.
    UnattributedWorkspaceChange,
    Uncertain,
}

impl CommandMutation {
    pub fn may_have_mutated(&self) -> bool {
        !matches!(self, Self::ReadOnly)
    }

    pub fn paths(&self) -> Option<&BTreeSet<PathBuf>> {
        match self {
            Self::KnownMutation { paths } => paths.as_ref(),
            Self::ReadOnly | Self::Uncertain | Self::UnattributedWorkspaceChange => None,
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
    match program {
        "prettier" => {
            !tokens
                .iter()
                .any(|arg| matches!(arg.as_str(), "--write" | "-w"))
                && tokens
                    .iter()
                    .any(|arg| matches!(arg.as_str(), "--check" | "-c" | "--list-different" | "-l"))
        }
        // A just recipe is user-defined; its --check argument is not proof of
        // read-only behavior. Let the ordinary workspace observation decide.
        "cargo" | "rustfmt" => {
            is_format_only_tokens(tokens) && tokens.iter().any(|arg| arg == "--check")
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
    let joined = command.join(" ").to_ascii_lowercase();
    let tokens = shell_filter_tokens(&joined);
    let explicit_dry_run = tokens.iter().any(|token| token == "--dry-run");
    let short_dry_run = tokens.iter().any(|token| token == "-n");
    if tokens.iter().any(|token| {
        matches!(
            command_basename(token),
            "chmod" | "chown" | "chgrp" | "touch" | "truncate"
        ) || (command_basename(token) == "dd" && tokens.iter().any(|arg| arg.starts_with("of=")))
            || (command_basename(token) == "patch" && !explicit_dry_run)
            || (command_basename(token) == "rsync" && !(explicit_dry_run || short_dry_run))
            || (command_basename(token) == "sed"
                && tokens.iter().any(|arg| {
                    arg == "-i"
                        || arg.starts_with("-i")
                        || arg == "--in-place"
                        || arg.starts_with("--in-place=")
                }))
            || (command_basename(token) == "perl"
                && tokens
                    .iter()
                    .any(|arg| arg.starts_with('-') && arg.trim_start_matches('-').contains('i')))
    }) {
        return true;
    }
    if tokens.iter().any(|token| {
        matches!(
            command_basename(token),
            "apply_patch"
                | "add-content"
                | "copy-item"
                | "cp"
                | "del"
                | "erase"
                | "md"
                | "mkdir"
                | "move-item"
                | "mv"
                | "new-item"
                | "ni"
                | "out-file"
                | "rd"
                | "reg"
                | "remove-item"
                | "ren"
                | "rename-item"
                | "rm"
                | "rmdir"
                | "set-content"
                | "set-item"
                | "set-itemproperty"
                | "tee"
                | "tee-object"
        )
    }) {
        return true;
    }

    if tokens.windows(2).any(|window| match window {
        [first, second]
            if command_basename(first) == "git"
                && matches!(
                    second.as_str(),
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
                ) =>
        {
            true
        }
        [first, second]
            if matches!(command_basename(first), "npm" | "pnpm" | "yarn")
                && matches!(
                    second.as_str(),
                    "add" | "install" | "remove" | "uninstall" | "update"
                ) =>
        {
            true
        }
        _ => false,
    }) {
        return true;
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
    !command
        .iter()
        .any(|token| token.contains(['>', '<', '&', '|', ';', '`', '$']))
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
#[cfg(any(test, feature = "test-support"))]
pub fn command_may_mutate(command: &[String]) -> bool {
    command_mutation(command, None).may_have_mutated()
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

pub fn resolve_uncertain_command_observation(workspace_changed: Option<bool>) -> CommandMutation {
    match workspace_changed {
        Some(false) => CommandMutation::ReadOnly,
        Some(true) => CommandMutation::UnattributedWorkspaceChange,
        None => CommandMutation::Uncertain,
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
            | "ls-remote"
            | "ls-tree"
            | "name-rev"
            | "rev-parse"
            | "rev-list"
            | "shortlog"
            | "show"
            | "status"
    )
}

/// Returns exact paths only for simple, direct mutators whose operands can be
/// interpreted without shell expansion. Complex scripts deliberately fall back
/// to unknown mutation invalidation.
#[cfg(any(test, feature = "test-support"))]
pub fn command_mutation_paths(command: &[String], cwd: Option<&Path>) -> Option<BTreeSet<PathBuf>> {
    if !looks_like_mutating_command(command) {
        return None;
    }
    mutating_command_paths(command, cwd)
}

fn mutating_command_paths(command: &[String], cwd: Option<&Path>) -> Option<BTreeSet<PathBuf>> {
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
        "rustfmt" | "prettier" if !args.iter().any(|arg| arg == "--check") => {
            simple_path_operands(args)?
        }
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
    command
        .iter()
        .flat_map(|token| token.split_whitespace())
        .map(|part| {
            part.trim_matches(|ch: char| {
                matches!(
                    ch,
                    '"' | '\'' | '`' | '(' | ')' | '[' | ']' | '{' | '}' | ';' | '&' | '|'
                )
            })
            .to_ascii_lowercase()
        })
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
#[cfg(test)]
mod configured_git_helper_tests {
    use super::*;

    #[test]
    fn workspace_classification_requires_the_same_helper_suppression_as_approval() {
        let argv = |args: &[&str]| {
            args.iter()
                .map(|arg| (*arg).to_string())
                .collect::<Vec<_>>()
        };
        for args in [
            vec!["git", "status"],
            vec!["git", "--no-optional-locks", "status"],
            vec!["git", "diff"],
            vec!["git", "show", "HEAD"],
        ] {
            assert!(looks_like_mutating_command(&argv(&args)), "{args:?}");
            assert!(!command_is_read_only_git(&argv(&args)), "{args:?}");
        }
        for args in [
            vec![
                "git",
                "--no-optional-locks",
                "-c",
                "core.fsmonitor=false",
                "status",
            ],
            vec![
                "git",
                "-c",
                "core.fsmonitor=false",
                "diff",
                "--no-ext-diff",
                "--no-textconv",
            ],
        ] {
            assert!(!looks_like_mutating_command(&argv(&args)), "{args:?}");
            // Dependency scoping must accept every spelling approval admits.
            assert!(command_is_read_only_git(&argv(&args)), "{args:?}");
        }
    }
}
