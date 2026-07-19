use crate::bash::parse_shell_lc_plain_commands;
use std::path::Path;
#[cfg(windows)]
#[path = "windows_dangerous_commands.rs"]
mod windows_dangerous_commands;

pub fn command_might_be_dangerous(command: &[String]) -> bool {
    #[cfg(windows)]
    {
        if windows_dangerous_commands::is_dangerous_command_windows(command) {
            return true;
        }
    }

    if is_dangerous_to_call_with_exec(command) {
        return true;
    }

    // Support `bash -lc "<script>"` where the any part of the script might contain a dangerous command.
    if let Some(all_commands) = parse_shell_lc_plain_commands(command)
        && all_commands
            .iter()
            .any(|cmd| is_dangerous_to_call_with_exec(cmd))
    {
        return true;
    }

    false
}

/// Returns whether already-tokenized PowerShell words should be treated as
/// dangerous by the Windows unmatched-command heuristics.
pub fn is_dangerous_powershell_words(command: &[String]) -> bool {
    #[cfg(windows)]
    {
        windows_dangerous_commands::is_dangerous_powershell_words(command)
    }

    #[cfg(not(windows))]
    {
        let _ = command;
        false
    }
}

fn is_git_global_option_with_value(arg: &str) -> bool {
    matches!(
        arg,
        "-C" | "-c"
            | "--config-env"
            | "--exec-path"
            | "--git-dir"
            | "--namespace"
            | "--super-prefix"
            | "--work-tree"
    )
}

fn is_git_global_option_with_inline_value(arg: &str) -> bool {
    matches!(
        arg,
        s if s.starts_with("--config-env=")
            || s.starts_with("--exec-path=")
            || s.starts_with("--git-dir=")
            || s.starts_with("--namespace=")
            || s.starts_with("--super-prefix=")
            || s.starts_with("--work-tree=")
    ) || ((arg.starts_with("-C") || arg.starts_with("-c")) && arg.len() > 2)
}

pub(crate) fn executable_name_lookup_key(raw: &str) -> Option<String> {
    #[cfg(windows)]
    {
        Path::new(raw)
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| {
                let name = name.to_ascii_lowercase();
                for suffix in [".exe", ".cmd", ".bat", ".com"] {
                    if let Some(stripped) = name.strip_suffix(suffix) {
                        return stripped.to_string();
                    }
                }
                name
            })
    }

    #[cfg(not(windows))]
    {
        Path::new(raw)
            .file_name()
            .and_then(|name| name.to_str())
            .map(std::borrow::ToOwned::to_owned)
    }
}

/// Find the first matching git subcommand, skipping known global options that
/// may appear before it (e.g., `-C`, `-c`, `--git-dir`).
///
/// Shared with `is_safe_command` to avoid git-global-option bypasses.
pub(crate) fn find_git_subcommand<'a>(
    command: &'a [String],
    subcommands: &[&str],
) -> Option<(usize, &'a str)> {
    let cmd0 = command.first().map(String::as_str)?;
    if executable_name_lookup_key(cmd0).as_deref() != Some("git") {
        return None;
    }

    let mut skip_next = false;
    for (idx, arg) in command.iter().enumerate().skip(1) {
        if skip_next {
            skip_next = false;
            continue;
        }

        let arg = arg.as_str();

        if is_git_global_option_with_inline_value(arg) {
            continue;
        }

        if is_git_global_option_with_value(arg) {
            skip_next = true;
            continue;
        }

        if arg == "--" || arg.starts_with('-') {
            continue;
        }

        if subcommands.contains(&arg) {
            return Some((idx, arg));
        }

        // In git, the first non-option token is the subcommand. If it isn't
        // one of the subcommands we're looking for, we must stop scanning to
        // avoid misclassifying later positional args (e.g., branch names).
        return None;
    }

    None
}

fn is_dangerous_to_call_with_exec(mut command: &[String]) -> bool {
    loop {
        match command
            .first()
            .and_then(|executable| executable_name_lookup_key(executable))
            .as_deref()
        {
            Some("rm") => return rm_has_force_option(&command[1..]),
            Some("sudo") => {
                let Some(subcommand) = sudo_subcommand(command) else {
                    return false;
                };
                command = subcommand;
            }
            _ => return false,
        }
    }
}

fn rm_has_force_option(args: &[String]) -> bool {
    for arg in args {
        if arg == "--" {
            break;
        }
        if arg == "--force" {
            return true;
        }
        if arg.starts_with('-')
            && !arg.starts_with("--")
            && arg.chars().skip(1).any(|flag| flag == 'f')
        {
            return true;
        }
    }
    false
}

fn sudo_subcommand(command: &[String]) -> Option<&[String]> {
    let mut index = 1;
    while index < command.len() {
        let arg = command[index].as_str();
        if arg == "--" {
            return command.get(index + 1..).filter(|rest| !rest.is_empty());
        }
        if arg == "-" || !arg.starts_with('-') {
            return command.get(index..).filter(|rest| !rest.is_empty());
        }

        let takes_separate_value = sudo_short_option_consumes_next(arg)
            || matches!(
                arg,
                "--auth-type"
                    | "--chdir"
                    | "--chroot"
                    | "--close-from"
                    | "--command-timeout"
                    | "--group"
                    | "--host"
                    | "--prompt"
                    | "--role"
                    | "--type"
                    | "--user"
            );
        index += if takes_separate_value { 2 } else { 1 };
    }
    None
}

fn sudo_short_option_consumes_next(arg: &str) -> bool {
    if !arg.starts_with('-') || arg.starts_with("--") {
        return false;
    }
    let flags: Vec<char> = arg.chars().skip(1).collect();
    for (index, flag) in flags.iter().enumerate() {
        if matches!(
            flag,
            'a' | 'C' | 'D' | 'g' | 'h' | 'p' | 'R' | 'r' | 'T' | 't' | 'u'
        ) {
            return index + 1 == flags.len();
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vec_str(items: &[&str]) -> Vec<String> {
        items.iter().map(std::string::ToString::to_string).collect()
    }

    #[test]
    fn rm_rf_is_dangerous() {
        assert!(command_might_be_dangerous(&vec_str(&["rm", "-rf", "/"])));
    }

    #[test]
    fn rm_f_is_dangerous() {
        assert!(command_might_be_dangerous(&vec_str(&["rm", "-f", "/"])));
    }

    #[test]
    fn rm_force_variants_and_full_paths_are_dangerous() {
        for command in [
            vec_str(&["/bin/rm", "-fr", "target"]),
            vec_str(&["/bin/rm", "--force", "target"]),
            vec_str(&["/bin/rm", "-r", "-f", "target"]),
        ] {
            assert!(command_might_be_dangerous(&command), "{command:?}");
        }
    }

    #[test]
    fn sudo_options_do_not_hide_dangerous_rm() {
        assert!(command_might_be_dangerous(&vec_str(&[
            "sudo", "-u", "root", "/bin/rm", "--force", "target"
        ])));
        assert!(command_might_be_dangerous(&vec_str(&[
            "sudo",
            "--preserve-env",
            "sudo",
            "-n",
            "rm",
            "-rf",
            "target"
        ])));
        assert!(command_might_be_dangerous(&vec_str(&[
            "sudo", "-nu", "root", "/bin/rm", "-f", "target"
        ])));
        assert!(command_might_be_dangerous(&vec_str(&[
            "sudo", "-uroot", "/bin/rm", "-f", "target"
        ])));
    }

    #[test]
    fn direct_powershell_words_reuse_windows_dangerous_detection() {
        let command = vec_str(&["Remove-Item", "test", "-Force"]);

        if cfg!(windows) {
            assert!(is_dangerous_powershell_words(&command));
        } else {
            assert!(!is_dangerous_powershell_words(&command));
        }
    }
}
