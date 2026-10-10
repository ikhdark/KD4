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
        find_git_subcommand(command, &["log", "diff", "show", "branch", "status"])
    else {
        return false;
    };

    let global_args = &command[1..subcommand_idx];
    if git_has_unsafe_global_option(global_args) {
        return false;
    }

    let subcommand_args = &command[subcommand_idx + 1..];

    match subcommand {
        "log" | "diff" | "show" => git_subcommand_args_are_read_only(subcommand_args),
        "branch" => {
            git_subcommand_args_are_read_only(subcommand_args)
                && git_branch_is_read_only(subcommand_args)
        }
        "status" => git_status_disables_optional_locks(global_args),
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
    global_args
        .iter()
        .map(String::as_str)
        .any(|arg| git_matches_option_pattern(arg, UNSAFE_GIT_GLOBAL_OPTIONS))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::string::ToString;

    fn vec_str(args: &[&str]) -> Vec<String> {
        args.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn review_regressions_reject_writers_and_grouped_decompression() {
        for argv in [
            vec!["uniq", "input.txt", "output.txt"],
            vec!["sed", "-n", "1p", "-ew output.txt"],
            vec!["rg", "-nz", "pattern"],
            vec!["rg", "-nze", "pattern"],
        ] {
            assert!(!is_known_safe_direct_argv(&vec_str(&argv)), "{argv:?}");
        }
        for argv in [
            vec!["rg", "-ne-z"],
            vec!["rg", "-e", "-z"],
            vec!["rg", "--regexp", "-z"],
            vec!["rg", "--", "-z"],
            vec!["rg", "-g*.zip", "pattern"],
            vec!["rg", "-nZ", "pattern"],
            vec!["sed", "-n", "1p", "-"],
        ] {
            assert!(is_known_safe_direct_argv(&vec_str(&argv)), "{argv:?}");
        }
        for source in [
            r"git diff --out\put=result.txt",
            "cat *.txt",
            r"c\at file.txt",
        ] {
            assert!(
                !is_known_safe_command(&vec_str(&["bash", "-lc", source])),
                "{source}"
            );
        }
        assert!(is_known_safe_command(&vec_str(&[
            "zsh",
            "-lc",
            "cat file.txt"
        ])));
    }

    #[test]
    fn known_safe_examples() {
        assert!(is_safe_to_call_with_exec(&vec_str(&["ls"])));
        assert!(is_safe_to_call_with_exec(&vec_str(&["git", "branch"])));
        assert!(is_safe_to_call_with_exec(&vec_str(&[
            "git",
            "branch",
            "--show-current"
        ])));
        assert!(is_safe_to_call_with_exec(&vec_str(&["base64"])));
        assert!(is_safe_to_call_with_exec(&vec_str(&[
            "sed", "-n", "1,5p", "file.txt"
        ])));
        assert!(is_safe_to_call_with_exec(&vec_str(&[
            "nl",
            "-nrz",
            "Cargo.toml"
        ])));

        // Safe `find` command (no unsafe options).
        assert!(is_safe_to_call_with_exec(&vec_str(&[
            "find", ".", "-name", "file.txt"
        ])));

        assert!(!is_safe_to_call_with_exec(&vec_str(&["numfmt", "1000"])));
        assert!(!is_safe_to_call_with_exec(&vec_str(&["tac", "Cargo.toml"])));
    }

    #[test]
    fn git_status_is_safe_only_when_optional_locks_are_disabled() {
        assert!(!is_safe_to_call_with_exec(&vec_str(&["git", "status"])));
        assert!(is_safe_to_call_with_exec(&vec_str(&[
            "git",
            "--no-optional-locks",
            "status",
            "--short",
            "--branch",
        ])));
    }

    #[test]
    fn git_branch_mutating_flags_are_not_safe() {
        assert!(!is_known_safe_command(&vec_str(&[
            "git", "branch", "-d", "feature"
        ])));
        assert!(!is_known_safe_command(&vec_str(&[
            "git",
            "branch",
            "new-branch"
        ])));
    }

    #[test]
    fn git_branch_global_options_respect_safety_rules() {
        // A harmless global option neither blocks a read-only query nor
        // excuses a mutation; an overriding one is unsafe by itself.
        assert!(is_known_safe_command(&vec_str(&[
            "git",
            "--no-pager",
            "branch",
            "--show-current",
        ])));
        assert!(!is_known_safe_command(&vec_str(&[
            "git", "--no-pager", "branch", "-d", "feature",
        ])));
        assert!(!is_known_safe_command(&vec_str(&[
            "git", "-C", ".", "branch", "--show-current",
        ])));
        assert!(!is_known_safe_command(&vec_str(&[
            "bash",
            "-lc",
            "git --no-pager branch -d feature",
        ])));
    }

    #[test]
    fn git_first_positional_is_the_subcommand() {
        // In git, the first non-option token is the subcommand. Later positional
        // args (like branch names) must not be treated as subcommands. Each later
        // name here would be approved if the scan continued past `checkout`.
        for command in [
            vec_str(&["git", "checkout", "log"]),
            vec_str(&["git", "--no-optional-locks", "checkout", "status"]),
        ] {
            assert!(!is_known_safe_command(&command), "{command:?}");
        }
    }

    #[test]
    fn git_output_flags_are_not_safe() {
        assert!(!is_known_safe_command(&vec_str(&[
            "git",
            "log",
            "--output=/tmp/git-log-out-test",
            "-n",
            "1",
        ])));
        assert!(!is_known_safe_command(&vec_str(&[
            "git",
            "diff",
            "--output",
            "/tmp/git-diff-out-test",
        ])));
        assert!(!is_known_safe_command(&vec_str(&[
            "git",
            "show",
            "--output=/tmp/git-show-out-test",
            "HEAD",
        ])));
    }

    #[test]
    fn git_external_diff_abbreviations_are_not_safe() {
        // Git accepts these as --ext-diff, overriding --no-ext-diff and
        // executing GIT_EXTERNAL_DIFF. Approval must not depend on spelling.
        for flag in ["--ext", "--ext-d", "--ext-dif", "--ext-diff"] {
            let argv = vec_str(&["git", "diff", "--no-ext-diff", flag]);
            assert!(!is_known_safe_direct_argv(&argv), "{flag}");
            assert!(!is_safe_powershell_words(&argv), "{flag}");
            assert!(!is_known_safe_command(&vec_str(&[
                "bash",
                "-lc",
                &format!("git diff --no-ext-diff {flag}"),
            ])), "{flag}");
        }
        // Disabling external programs and selecting output display characters
        // do not enable external execution or write an output file.
        for flag in ["--no-ext-diff", "--no-textconv", "--output-indicator-new=X"] {
            assert!(is_known_safe_direct_argv(&vec_str(&["git", "diff", flag])), "{flag}");
        }
    }

    #[test]
    fn git_global_pagination_flags_are_not_safe() {
        assert!(!is_known_safe_command(&vec_str(&[
            "git",
            "--paginate",
            "log",
            "-1",
        ])));
        assert!(!is_known_safe_command(&vec_str(&[
            "git", "-p", "log", "-1",
        ])));
        assert!(!is_known_safe_command(&vec_str(&[
            "bash",
            "-lc",
            "git --paginate log -1",
        ])));
        assert!(!is_known_safe_command(&vec_str(&[
            "bash",
            "-lc",
            "git -p log -1",
        ])));
    }

    #[test]
    fn git_subcommand_patch_flags_remain_safe() {
        assert!(is_known_safe_command(&vec_str(&["git", "log", "-p", "-1"])));
        assert!(is_known_safe_command(&vec_str(&["git", "diff", "-p"])));
        assert!(is_known_safe_command(&vec_str(&[
            "git", "show", "-p", "HEAD",
        ])));
        assert!(is_known_safe_command(&vec_str(&[
            "bash",
            "-lc",
            "git log -p -1",
        ])));
    }

    #[test]
    fn git_global_override_flags_are_not_safe() {
        // Plain `git status` is never approved, so every status case starts from
        // the `--no-optional-locks` form that is approved without the override.
        assert!(!is_known_safe_command(&vec_str(&[
            "git",
            "--no-optional-locks",
            "-C",
            ".",
            "status",
        ])));
        assert!(!is_known_safe_command(&vec_str(&[
            "git",
            "--no-optional-locks",
            "-C.",
            "status",
        ])));
        assert!(!is_known_safe_command(&vec_str(&[
            "git",
            "-c",
            "core.pager=cat",
            "log",
            "-n",
            "1",
        ])));
        assert!(!is_known_safe_command(&vec_str(&[
            "git",
            "--no-optional-locks",
            "-ccore.pager=cat",
            "status",
        ])));

        for args in [
            vec_str(&["git", "--config-env", "core.pager=PAGER", "show", "HEAD"]),
            vec_str(&["git", "--config-env=core.pager=PAGER", "show", "HEAD"]),
            vec_str(&["git", "--git-dir", ".evil-git", "diff", "HEAD~1..HEAD"]),
            vec_str(&["git", "--git-dir=.evil-git", "diff", "HEAD~1..HEAD"]),
            vec_str(&["git", "--no-optional-locks", "--work-tree", ".", "status"]),
            vec_str(&["git", "--no-optional-locks", "--work-tree=.", "status"]),
            vec_str(&["git", "--exec-path", ".git/helpers", "show", "HEAD"]),
            vec_str(&["git", "--exec-path=.git/helpers", "show", "HEAD"]),
            vec_str(&["git", "--namespace", "attacker", "show", "HEAD"]),
            vec_str(&["git", "--namespace=attacker", "show", "HEAD"]),
            vec_str(&["git", "--super-prefix", "attacker/", "show", "HEAD"]),
            vec_str(&["git", "--super-prefix=attacker/", "show", "HEAD"]),
        ] {
            assert!(
                !is_known_safe_command(&args),
                "expected {args:?} to require approval due to unsafe git global option",
            );
        }

        assert!(!is_known_safe_command(&vec_str(&[
            "bash",
            "-lc",
            "git --no-optional-locks -C .project-deps/test-fixtures status",
        ])));
        // No `~` revision here: the script parser rejects that word on its own,
        // which would hide a `--git-dir=` that stopped being unsafe.
        assert!(!is_known_safe_command(&vec_str(&[
            "bash",
            "-lc",
            "git --git-dir=.evil-git diff HEAD",
        ])));
    }

    #[test]
    fn zsh_lc_safe_command_sequence() {
        assert!(is_known_safe_command(&vec_str(&["zsh", "-lc", "ls"])));
    }

    #[test]
    fn authorization_identity_direct_argv_safety_is_opaque() {
        assert!(is_known_safe_direct_argv(&vec_str(&["ls"])));
        assert!(!is_known_safe_direct_argv(&vec_str(&[
            "/workspace/bash",
            "-lc",
            "ls",
        ])));
        assert!(!is_known_safe_direct_argv(&vec_str(&[
            "/workspace/pwsh.exe",
            "-NoProfile",
            "-Command",
            "Get-ChildItem",
        ])));
    }

    #[test]
    fn unknown_or_partial() {
        assert!(!is_safe_to_call_with_exec(&vec_str(&["foo"])));
        assert!(!is_safe_to_call_with_exec(&vec_str(&["git", "fetch"])));
        assert!(!is_safe_to_call_with_exec(&vec_str(&[
            "sed", "-n", "xp", "file.txt"
        ])));

        // Unsafe `find` commands.
        for args in [
            vec_str(&["find", ".", "-name", "file.txt", "-exec", "rm", "{}", ";"]),
            vec_str(&[
                "find", ".", "-name", "*.py", "-execdir", "python3", "{}", ";",
            ]),
            vec_str(&["find", ".", "-name", "file.txt", "-ok", "rm", "{}", ";"]),
            vec_str(&["find", ".", "-name", "*.py", "-okdir", "python3", "{}", ";"]),
            vec_str(&["find", ".", "-delete", "-name", "file.txt"]),
            vec_str(&["find", ".", "-fls", "/etc/passwd"]),
            vec_str(&["find", ".", "-fprint", "/etc/passwd"]),
            vec_str(&["find", ".", "-fprint0", "/etc/passwd"]),
            vec_str(&["find", ".", "-fprintf", "/root/suid.txt", "%#m %u %p\n"]),
        ] {
            assert!(
                !is_safe_to_call_with_exec(&args),
                "expected {args:?} to be unsafe"
            );
        }
    }

    #[test]
    fn base64_output_options_are_unsafe() {
        for args in [
            vec_str(&["base64", "-o", "out.bin"]),
            vec_str(&["base64", "--output", "out.bin"]),
            vec_str(&["base64", "--output=out.bin"]),
            vec_str(&["base64", "-ob64.txt"]),
        ] {
            assert!(
                !is_safe_to_call_with_exec(&args),
                "expected {args:?} to be considered unsafe due to output option"
            );
        }
    }

    #[test]
    fn ripgrep_rules() {
        // Safe ripgrep invocations – none of the unsafe flags are present.
        assert!(is_safe_to_call_with_exec(&vec_str(&[
            "rg",
            "Cargo.toml",
            "-n"
        ])));

        // Unsafe flags that do not take an argument (present verbatim).
        for args in [
            vec_str(&["rg", "--search-zip", "files"]),
            vec_str(&["rg", "-z", "files"]),
        ] {
            assert!(
                !is_safe_to_call_with_exec(&args),
                "expected {args:?} to be considered unsafe due to zip-search flag",
            );
        }

        // Unsafe flags that expect a value, provided in both split and = forms.
        for args in [
            vec_str(&["rg", "--pre", "pwned", "files"]),
            vec_str(&["rg", "--pre=pwned", "files"]),
            vec_str(&["rg", "--hostname-bin", "pwned", "files"]),
            vec_str(&["rg", "--hostname-bin=pwned", "files"]),
        ] {
            assert!(
                !is_safe_to_call_with_exec(&args),
                "expected {args:?} to be considered unsafe due to external-command flag",
            );
        }
    }

    #[test]
    fn windows_powershell_full_path_is_safe() {
        {}

        let powershell = crate::powershell::try_find_pwsh_executable_blocking()
            .or_else(crate::powershell::try_find_powershell_executable_blocking)
            .expect("a PowerShell host is required to verify this behavior");
        let powershell = powershell.as_path().to_str().unwrap();

        assert!(is_known_safe_command(&vec_str(&[
            powershell,
            "-NoProfile",
            "-Command",
            "Get-Location",
        ])));
    }

    #[test]
    fn windows_git_full_path_is_safe() {
        {}

        assert!(!is_known_safe_command(&vec_str(&[
            r"C:\Program Files\Git\cmd\git.exe",
            "status",
        ])));
        assert!(is_known_safe_command(&vec_str(&[
            r"C:\Program Files\Git\cmd\git.exe",
            "--no-optional-locks",
            "status",
        ])));
    }

    #[test]
    fn bash_lc_safe_examples() {
        assert!(is_known_safe_command(&vec_str(&["bash", "-lc", "ls"])));
        assert!(is_known_safe_command(&vec_str(&["bash", "-lc", "ls -1"])));
        assert!(!is_known_safe_command(&vec_str(&[
            "bash",
            "-lc",
            "git status"
        ])));
        assert!(is_known_safe_command(&vec_str(&[
            "bash",
            "-lc",
            "git --no-optional-locks status"
        ])));
        assert!(is_known_safe_command(&vec_str(&[
            "bash",
            "-lc",
            "grep -R \"Cargo.toml\" -n"
        ])));
        assert!(is_known_safe_command(&vec_str(&[
            "bash",
            "-lc",
            "sed -n 1,5p file.txt"
        ])));
        assert!(is_known_safe_command(&vec_str(&[
            "bash",
            "-lc",
            "sed -n '1,5p' file.txt"
        ])));

        assert!(is_known_safe_command(&vec_str(&[
            "bash",
            "-lc",
            "find . -name file.txt"
        ])));
    }

    #[test]
    fn bash_lc_safe_examples_with_operators() {
        assert!(is_known_safe_command(&vec_str(&[
            "bash",
            "-lc",
            "grep -R \"Cargo.toml\" -n || true"
        ])));
        assert!(is_known_safe_command(&vec_str(&[
            "bash",
            "-lc",
            "ls && pwd"
        ])));
        assert!(is_known_safe_command(&vec_str(&[
            "bash",
            "-lc",
            "echo 'hi' ; ls"
        ])));
        assert!(is_known_safe_command(&vec_str(&[
            "bash",
            "-lc",
            "ls | wc -l"
        ])));
    }

    #[test]
    fn bash_lc_unsafe_examples() {
        // `ls -1` is approved as a three-argument script (see bash_lc_safe_examples),
        // so only the argv shape and the quoting can make these two unsafe.
        assert!(
            !is_known_safe_command(&vec_str(&["bash", "-lc", "ls", "-1"])),
            "Four arg version is not known to be safe."
        );
        assert!(
            !is_known_safe_command(&vec_str(&["bash", "-lc", "'ls -1'"])),
            "The extra quoting around 'ls -1' makes it a program named 'ls -1' and is therefore unsafe."
        );

        assert!(
            !is_known_safe_command(&vec_str(&["bash", "-lc", "find . -name file.txt -delete"])),
            "Unsafe find option should not be auto-approved."
        );

        // Disallowed because of unsafe command in sequence.
        assert!(
            !is_known_safe_command(&vec_str(&["bash", "-lc", "ls && rm -rf /"])),
            "Sequence containing unsafe command must be rejected"
        );

        // Disallowed because of parentheses / subshell.
        assert!(
            !is_known_safe_command(&vec_str(&["bash", "-lc", "(ls)"])),
            "Parentheses (subshell) are not provably safe with the current parser"
        );
        assert!(
            !is_known_safe_command(&vec_str(&["bash", "-lc", "ls || (pwd && echo hi)"])),
            "Nested parentheses are not provably safe with the current parser"
        );

        // Disallowed redirection.
        assert!(
            !is_known_safe_command(&vec_str(&["bash", "-lc", "ls > out.txt"])),
            "> redirection should be rejected"
        );
    }

    #[test]
    fn direct_powershell_words_use_windows_safelist() {
        let command = vec_str(&["Get-Content", "Cargo.toml"]);

        assert!(is_safe_powershell_words(&command));
    }

    #[test]
    fn exact_development_tool_version_probes_are_safe() {
        for command in [
            vec_str(&["cargo", "--version"]),
            vec_str(&["rustc", "-vV"]),
            vec_str(&["node", "-v"]),
            vec_str(&["python", "-V"]),
            vec_str(&["rg", "--version"]),
            vec_str(&["just", "--version"]),
        ] {
            assert!(
                is_known_safe_command(&command),
                "expected safe: {command:?}"
            );
        }

        for command in [
            vec_str(&["cargo", "check"]),
            vec_str(&["cargo", "--version", "extra"]),
            vec_str(&["unknown-tool", "--version"]),
            vec_str(&["node", "--version", "script.js"]),
            vec_str(&["cargo", "-v"]),
        ] {
            assert!(
                !is_known_safe_command(&command),
                "expected unsafe: {command:?}"
            );
        }
    }
}
