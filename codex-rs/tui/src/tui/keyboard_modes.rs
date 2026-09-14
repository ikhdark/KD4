//! Terminal keyboard enhancement setup and teardown helpers.
//!
//! The TUI uses crossterm's keyboard enhancement stack while it owns the terminal, but
//! process exit gets a stronger reset so the parent shell does not inherit enhanced key
//! reporting if a terminal misses the normal stack pop.

use std::fmt;
use std::io::Write;
use std::io::stdout;

use crossterm::Command;
#[cfg(not(windows))]
use crossterm::event::KeyboardEnhancementFlags;
#[cfg(not(windows))]
use crossterm::event::PopKeyboardEnhancementFlags;
#[cfg(not(windows))]
use crossterm::event::PushKeyboardEnhancementFlags;
use ratatui::crossterm::execute;

const DISABLE_KEYBOARD_ENHANCEMENT_ENV_VAR: &str = "CODEX_TUI_DISABLE_KEYBOARD_ENHANCEMENT";

pub(super) fn keyboard_enhancement_disabled() -> bool {
    let disable_env = std::env::var(DISABLE_KEYBOARD_ENHANCEMENT_ENV_VAR).ok();
    keyboard_enhancement_disabled_for(disable_env.as_deref())
}

fn keyboard_enhancement_disabled_for(disable_env: Option<&str>) -> bool {
    parse_bool_env(disable_env).unwrap_or(false)
}

fn parse_bool_env(value: Option<&str>) -> Option<bool> {
    match value.map(str::trim) {
        Some("1") => Some(true),
        Some(value) if value.eq_ignore_ascii_case("true") => Some(true),
        Some(value) if value.eq_ignore_ascii_case("yes") => Some(true),
        Some("0") => Some(false),
        Some(value) if value.eq_ignore_ascii_case("false") => Some(false),
        Some(value) if value.eq_ignore_ascii_case("no") => Some(false),
        _ => None,
    }
}

pub(super) fn running_in_vscode_terminal() -> bool {
    term_program_is_vscode(std::env::var("TERM_PROGRAM").ok().as_deref())
}

fn term_program_is_vscode(value: Option<&str>) -> bool {
    value.is_some_and(|value| value.eq_ignore_ascii_case("vscode"))
}

pub(super) fn enable_keyboard_enhancement() {
    if keyboard_enhancement_disabled() {
        return;
    }

    let _ = execute!(stdout(), DisableModifyOtherKeys);
    // Windows reads native input records and must not enable escape-sequence input.
    #[cfg(not(windows))]
    let _ = execute!(
        stdout(),
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
        )
    );
}

pub(super) fn restore_keyboard_enhancement_stack() {
    let _ = restore_keyboard_reporting(&mut stdout(), false);
}

pub(super) fn reset_keyboard_reporting_after_exit() {
    let _ = restore_keyboard_reporting(&mut stdout(), true);
}

fn restore_keyboard_reporting(writer: &mut impl Write, final_exit: bool) -> std::io::Result<()> {
    // No stack level is pushed on Windows; its crossterm pop also returns Unsupported.
    #[cfg(not(windows))]
    execute!(writer, PopKeyboardEnhancementFlags)?;
    if final_exit {
        execute!(writer, ResetKeyboardEnhancementFlags)?;
    }
    execute!(writer, DisableModifyOtherKeys)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ResetKeyboardEnhancementFlags;

impl Command for ResetKeyboardEnhancementFlags {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        f.write_str("\x1b[=0u")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "keyboard enhancement reset is not implemented for the legacy Windows API",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DisableModifyOtherKeys;

impl Command for DisableModifyOtherKeys {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        f.write_str("\x1b[>4;0m")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "modifyOtherKeys reset is not implemented for the legacy Windows API",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::DisableModifyOtherKeys;
    use super::ResetKeyboardEnhancementFlags;
    use super::keyboard_enhancement_disabled_for;
    use super::parse_bool_env;
    use crossterm::Command;
    use pretty_assertions::assert_eq;

    fn ansi_for(command: impl Command) -> String {
        let mut out = String::new();
        command.write_ansi(&mut out).unwrap();
        out
    }

    #[test]
    fn keyboard_enhancement_env_flag_parses_common_values() {
        assert_eq!(parse_bool_env(Some("1")), Some(true));
        assert_eq!(parse_bool_env(Some("true")), Some(true));
        assert_eq!(parse_bool_env(Some("YES")), Some(true));
        assert_eq!(parse_bool_env(Some("0")), Some(false));
        assert_eq!(parse_bool_env(Some("false")), Some(false));
        assert_eq!(parse_bool_env(Some("NO")), Some(false));
        assert_eq!(parse_bool_env(Some("unexpected")), None);
        assert_eq!(parse_bool_env(/*value*/ None), None);
    }

    #[test]
    fn keyboard_enhancement_env_flag_controls_the_feature() {
        assert!(!keyboard_enhancement_disabled_for(None));
        assert!(!keyboard_enhancement_disabled_for(Some("0")));
        assert!(keyboard_enhancement_disabled_for(Some("1")));
    }

    #[test]
    fn reset_keyboard_enhancement_flags_disables_current_reporting() {
        assert_eq!(ansi_for(ResetKeyboardEnhancementFlags), "\x1b[=0u");
    }

    #[test]
    fn cleanup_dispatch_writes_resets_only_on_final_exit() {
        for final_exit in [false, true] {
            let mut output = Vec::new();
            super::restore_keyboard_reporting(&mut output, final_exit).unwrap();
            let pop = if cfg!(windows) { "" } else { "\x1b[<1u" };
            let reset = if final_exit { "\x1b[=0u" } else { "" };
            assert_eq!(output, format!("{pop}{reset}\x1b[>4;0m").into_bytes());
        }
    }

    #[test]
    fn disable_modify_other_keys_resets_xterm_keyboard_reporting() {
        assert_eq!(ansi_for(DisableModifyOtherKeys), "\x1b[>4;0m");
    }
}
