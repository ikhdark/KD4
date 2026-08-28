//! Command parsing and safety utilities shared across Codex crates.

pub mod shell_detect;

pub mod bash;
pub(crate) mod command_safety;
pub mod parse_command;
pub mod powershell;

pub use command_safety::is_dangerous_command;
pub use command_safety::is_safe_command;

/// Escapes text that will be placed between PowerShell single quotes.
pub fn escape_powershell_single_quoted(input: &str) -> String {
    input.replace('\'', "''")
}

/// Renders one PowerShell single-quoted string literal.
pub fn quote_powershell_single_quoted(input: &str) -> String {
    format!("'{}'", escape_powershell_single_quoted(input))
}

/// Quote one Windows command-line argument using the rules followed by
/// `CommandLineToArgvW` and the Microsoft C runtime.
///
/// This helper is available on every host because callers can render commands
/// for a Windows target while running on another platform.
pub fn quote_windows_arg(arg: &str) -> String {
    let needs_quotes = arg.is_empty()
        || arg
            .chars()
            .any(|ch| matches!(ch, ' ' | '\t' | '\n' | '\r' | '"'));
    if !needs_quotes {
        return arg.to_string();
    }

    let mut quoted = String::with_capacity(arg.len() + 2);
    quoted.push('"');
    let mut backslashes = 0;
    for ch in arg.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                quoted.push_str(&"\\".repeat(backslashes * 2 + 1));
                quoted.push('"');
                backslashes = 0;
            }
            _ => {
                if backslashes > 0 {
                    quoted.push_str(&"\\".repeat(backslashes));
                    backslashes = 0;
                }
                quoted.push(ch);
            }
        }
    }
    if backslashes > 0 {
        quoted.push_str(&"\\".repeat(backslashes * 2));
    }
    quoted.push('"');
    quoted
}

#[cfg(test)]
mod tests {
    use super::escape_powershell_single_quoted;
    use super::quote_powershell_single_quoted;
    use super::quote_windows_arg;
    use pretty_assertions::assert_eq;

    #[test]
    fn windows_argument_quoting_handles_empty_quotes_and_backslashes() {
        for (argument, expected) in [
            ("", "\"\""),
            ("plain", "plain"),
            ("argument with space", "\"argument with space\""),
            ("say \"hello\"", "\"say \\\"hello\\\"\""),
            ("C:\\path with space\\", "\"C:\\path with space\\\\\""),
        ] {
            assert_eq!(quote_windows_arg(argument), expected);
        }
    }

    #[test]
    fn powershell_single_quote_helpers_share_one_escape_rule() {
        assert_eq!(escape_powershell_single_quoted("it's here"), "it''s here");
        assert_eq!(quote_powershell_single_quoted("it's here"), "'it''s here'");
    }
}
