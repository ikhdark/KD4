use crate::bash::parse_shell_lc_plain_commands;
use crate::command_safety::is_dangerous_command::executable_name_lookup_key;
// Find the first matching git subcommand, skipping known global options that
// may appear before it (e.g., `-C`, `-c`, `--git-dir`).
// Implemented in `is_dangerous_command` and shared here.
use crate::command_safety::is_dangerous_command::find_git_subcommand;
use crate::parse_command::is_valid_sed_n_arg;

use crate::command_safety::windows_safe_commands::is_safe_command_windows;

use crate::command_safety::windows_safe_commands::is_safe_powershell_words as is_safe_powershell_words_windows;

pub fn is_known_safe_command(command: &[String]) -> bool {
    {
        if is_safe_command_windows(command) {
            return true;
        }
    }

    if is_safe_to_call_with_exec(command) {
        return true;
    }

    // Support `bash -lc "..."` where the script consists solely of one or
    // more "plain" commands (only bare words / quoted strings) combined with
    // a conservative allow‑list of shell operators that themselves do not
    // introduce side effects ( "&&", "||", ";", and "|" ). If every
    // individual command in the script is itself a known‑safe command, then
    // the composite expression is considered safe.
    if let Some(all_commands) = parse_shell_lc_plain_commands(command)
        && !all_commands.is_empty()
        && all_commands
            .iter()
            .all(|cmd| is_safe_to_call_with_exec(cmd))
    {
        return true;
    }
    false
}

/// Returns whether an exact, already-tokenized argv is known safe without
/// interpreting a shell-looking executable as an owned shell wrapper.
pub fn is_known_safe_direct_argv(command: &[String]) -> bool {
    is_safe_to_call_with_exec(command)
}

/// Returns whether words from the restricted PowerShell AST parser are read-only
/// enough to be auto-approved. Raw script tokens do not satisfy this precondition.
pub fn is_safe_powershell_words(command: &[String]) -> bool {
    is_safe_powershell_words_windows(command)
}

fn is_safe_to_call_with_exec(command: &[String]) -> bool {
    let Some(cmd0) = command.first().map(String::as_str) else {
        return false;
    };

    if is_exact_version_probe(command) {
        return true;
    }

    match executable_name_lookup_key(cmd0).as_deref() {
        #[rustfmt::skip]
        Some(
            "cat" |
            "cd" |
            "cut" |
            "echo" |
            "expr" |
            "false" |
            "grep" |
            "head" |
            "id" |
            "ls" |
            "nl" |
            "paste" |
            "pwd" |
            "rev" |
            "seq" |
            "stat" |
            "tail" |
            "tr" |
            "true" |
            "uname" |
            "wc" |
            "which" |
            "whoami") => {
            true
        },

        Some("base64") => {
            const UNSAFE_BASE64_OPTIONS: &[&str] = &["-o", "--output"];

            !command.iter().skip(1).any(|arg| {
                UNSAFE_BASE64_OPTIONS.contains(&arg.as_str())
                    || arg.starts_with("--output=")
                    || (arg.starts_with("-o") && arg != "-o")
            })
        }

        Some("find") => {
            // Certain options to `find` can delete files, write to files, or
            // execute arbitrary commands, so we cannot auto-approve the
            // invocation of `find` in such cases.
            #[rustfmt::skip]
            const UNSAFE_FIND_OPTIONS: &[&str] = &[
                // Options that can execute arbitrary commands.
                "-exec", "-execdir", "-ok", "-okdir",
                // Option that deletes matching files.
                "-delete",
                // Options that write pathnames to a file.
                "-fls", "-fprint", "-fprint0", "-fprintf",
            ];

            !command
                .iter()
                .any(|arg| UNSAFE_FIND_OPTIONS.contains(&arg.as_str()))
        }

        // Ripgrep
        Some("rg") => is_safe_ripgrep(command),

        // Git
        Some("git") => is_safe_git_command(command),

        // Special-case `sed -n {N|M,N}p`
        Some("sed")
            if {
                command.len() <= 4
                    && command.get(1).map(String::as_str) == Some("-n")
                    && is_valid_sed_n_arg(command.get(2).map(String::as_str))
                    && command
                        .get(3)
                        .is_none_or(|arg| !arg.starts_with('-') || arg == "-")
            } =>
        {
            true
        }

        // ── anything else ─────────────────────────────────────────────────
        _ => false,
    }
}

/// Inspect native options with their case-sensitive spelling and short-option values.
pub(crate) fn is_safe_ripgrep(words: &[String]) -> bool {
    let mut args = words.iter().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--" {
            break;
        }
        if let Some(long) = arg.strip_prefix("--") {
            let (name, inline) = long
                .split_once('=')
                .map_or((long, false), |(name, _)| (name, true));
            if matches!(name, "pre" | "hostname-bin" | "search-zip") {
                return false;
            }
            if !inline
                && matches!(
                    name,
                    "after-context"
                        | "before-context"
                        | "context"
                        | "context-separator"
                        | "encoding"
                        | "engine"
                        | "field-context-separator"
                        | "field-match-separator"
                        | "file"
                        | "glob"
                        | "iglob"
                        | "ignore-file"
                        | "max-columns"
                        | "max-count"
                        | "max-depth"
                        | "max-filesize"
                        | "path-separator"
                        | "pre-glob"
                        | "regexp"
                        | "replace"
                        | "sort"
                        | "sortr"
                        | "threads"
                        | "type"
                        | "type-not"
                        | "type-add"
                        | "type-clear"
                        | "colors"
                        | "color"
                        | "hyperlink-format"
                        | "dfa-size-limit"
                        | "regex-size-limit"
                )
            {
                args.next();
            }
        } else if let Some(short) = arg.strip_prefix('-') {
            let mut flags = short.chars().peekable();
            while let Some(flag) = flags.next() {
                if flag == 'z' {
                    return false;
                }
                if matches!(
                    flag,
                    'A' | 'B' | 'C' | 'E' | 'M' | 'e' | 'f' | 'g' | 'j' | 'm' | 'r' | 't' | 'T'
                ) {
                    if flags.peek().is_none() {
                        args.next();
                    }
                    break;
                }
            }
        }
    }
    true
}

pub(crate) fn is_safe_git_command(command: &[String]) -> bool {
    let Some((subcommand_idx, subcommand)) =
        find_git_subcommand(command, &["diff", "branch", "status"])
    else {
        return false;
    };

    let global_args = &command[1..subcommand_idx];
    if git_has_unsafe_global_option(global_args) {
        return false;
    }

    let subcommand_args = &command[subcommand_idx + 1..];

    match subcommand {
        // log/show can invoke configured signature verifiers through defaults,
        // pretty formats, and abbreviated flags. They require normal approval.
        "diff" => {
            let options: Vec<_> = subcommand_args
                .iter()
                .take_while(|arg| arg.as_str() != "--")
                .collect();
            git_subcommand_args_are_read_only(subcommand_args)
                && git_fsmonitor_disabled(global_args)
                && options.iter().any(|arg| arg.as_str() == "--no-ext-diff")
                && options.iter().any(|arg| arg.as_str() == "--no-textconv")
        }
        "branch" => {
            git_subcommand_args_are_read_only(subcommand_args)
                && git_branch_is_read_only(subcommand_args)
        }
        "status" => {
            git_status_disables_optional_locks(global_args) && git_fsmonitor_disabled(global_args)
        }
        other => {
            debug_assert!(false, "unexpected git subcommand from matcher: {other}");
            false
        }
    }
}

/// Recognizes version-only probes for development tools whose documented
/// version switch cannot execute a project task or consume another operand.
/// Keeping this exact (two argv values, one admitted switch) is intentional:
/// options such as Cargo's `--list` and package-manager subcommands can execute
/// project-controlled code even when they look informational.
pub(crate) fn is_exact_version_probe(command: &[String]) -> bool {
    let [executable, flag] = command else {
        return false;
    };
    let Some(executable) = executable_name_lookup_key(executable) else {
        return false;
    };

    match executable.as_str() {
        "cargo" => matches!(flag.as_str(), "--version" | "-V"),
        "rustc" => matches!(flag.as_str(), "--version" | "-V" | "-vV"),
        "node" | "npm" | "pnpm" | "yarn" => matches!(flag.as_str(), "--version" | "-v"),
        "python" | "python3" | "py" => matches!(flag.as_str(), "--version" | "-V"),
        "rg" | "fd" | "jq" | "yq" | "delta" | "ast-grep" | "sg" | "just" | "rustfmt"
        | "cargo-clippy" | "taplo" | "dprint" | "hyperfine" | "tokei" => flag == "--version",
        _ => false,
    }
}

// `git status` ordinarily refreshes the index and therefore needs mutation
// authority. Git's global `--no-optional-locks` flag disables that refresh,
// making status safe to run concurrently with another workspace writer.
fn git_status_disables_optional_locks(global_args: &[String]) -> bool {
    global_args.iter().any(|arg| arg == "--no-optional-locks")
}

fn git_fsmonitor_disabled(global_args: &[String]) -> bool {
    global_args
        .iter()
        .any(|arg| arg == "-ccore.fsmonitor=false")
        || global_args
            .windows(2)
            .any(|args| args[0] == "-c" && args[1] == "core.fsmonitor=false")
}

// Treat `git branch` as safe only when the arguments clearly indicate
// a read-only query, not a branch mutation (create/rename/delete).
fn git_branch_is_read_only(branch_args: &[String]) -> bool {
    if branch_args.is_empty() {
        // `git branch` with no additional args lists branches.
        return true;
    }

    let mut saw_read_only_flag = false;
    for arg in branch_args.iter().map(String::as_str) {
        match arg {
            "--list" | "-l" | "--show-current" | "-a" | "--all" | "-r" | "--remotes" | "-v"
            | "-vv" | "--verbose" => {
                saw_read_only_flag = true;
            }
            _ if arg.starts_with("--format=") => {
                saw_read_only_flag = true;
            }
            _ => {
                // Any other flag or positional argument may create, rename, or delete branches.
                return false;
            }
        }
    }

    saw_read_only_flag
}

#[derive(Clone, Copy)]
enum GitOptionPattern {
    Exact(&'static str),
    ShortWithInlineValue(&'static str),
    Prefix(&'static str),
    LongWithAbbreviation(&'static str),
}

const UNSAFE_GIT_GLOBAL_OPTIONS: &[GitOptionPattern] = &[
    GitOptionPattern::Exact("-C"),
    GitOptionPattern::ShortWithInlineValue("-C"),
    GitOptionPattern::Exact("-c"),
    GitOptionPattern::ShortWithInlineValue("-c"),
    GitOptionPattern::Exact("-p"),
    GitOptionPattern::Exact("--config-env"),
    GitOptionPattern::Prefix("--config-env="),
    GitOptionPattern::Exact("--exec-path"),
    GitOptionPattern::Prefix("--exec-path="),
    GitOptionPattern::Exact("--git-dir"),
    GitOptionPattern::Prefix("--git-dir="),
    GitOptionPattern::Exact("--namespace"),
    GitOptionPattern::Prefix("--namespace="),
    GitOptionPattern::Exact("--paginate"),
    GitOptionPattern::Exact("--super-prefix"),
    GitOptionPattern::Prefix("--super-prefix="),
    GitOptionPattern::Exact("--work-tree"),
    GitOptionPattern::Prefix("--work-tree="),
];

const UNSAFE_GIT_SUBCOMMAND_OPTIONS: &[GitOptionPattern] = &[
    GitOptionPattern::LongWithAbbreviation("--output"),
    GitOptionPattern::LongWithAbbreviation("--ext-diff"),
    GitOptionPattern::LongWithAbbreviation("--textconv"),
    GitOptionPattern::LongWithAbbreviation("--exec"),
];

impl GitOptionPattern {
    fn matches(self, arg: &str) -> bool {
        match self {
            GitOptionPattern::Exact(option) => arg == option,
            GitOptionPattern::ShortWithInlineValue(option) => {
                arg.starts_with(option) && arg.len() > option.len()
            }
            GitOptionPattern::Prefix(prefix) => arg.starts_with(prefix),
            GitOptionPattern::LongWithAbbreviation(option) => {
                // Git's subcommand parser accepts unique long-option prefixes.
                // Conservatively reject ambiguous prefixes too, rather than
                // depending on the installed Git version's other option names.
                let name = arg.split_once('=').map_or(arg, |(name, _)| name);
                name.starts_with("--") && name.len() > 2 && option.starts_with(name)
            }
        }
    }
}

fn git_matches_option_pattern(arg: &str, patterns: &[GitOptionPattern]) -> bool {
    patterns.iter().any(|pattern| pattern.matches(arg))
}

fn git_has_unsafe_global_option(global_args: &[String]) -> bool {
    let mut args = global_args.iter().map(String::as_str);
    while let Some(arg) = args.next() {
        if arg == "-ccore.fsmonitor=false" {
            continue;
        }
        if arg == "-c" {
            if args.next() != Some("core.fsmonitor=false") {
                return true;
            }
        } else if git_matches_option_pattern(arg, UNSAFE_GIT_GLOBAL_OPTIONS) {
            return true;
        }
    }
    false
}

fn git_subcommand_args_are_read_only(args: &[String]) -> bool {
    !args
        .iter()
        .map(String::as_str)
        .any(|arg| git_matches_option_pattern(arg, UNSAFE_GIT_SUBCOMMAND_OPTIONS))
}

// (bash parsing helpers implemented in crate::bash)

/* ----------------------------------------------------------
Example
---------------------------------------------------------- */

