use super::*;
use pretty_assertions::assert_eq;
use std::collections::HashMap;

#[derive(Default)]
struct FakeEnvironment {
    vars: HashMap<String, String>,
}

impl FakeEnvironment {
    fn with_var(mut self, key: &str, value: &str) -> Self {
        self.vars.insert(key.to_string(), value.to_string());
        self
    }
}

impl Environment for FakeEnvironment {
    fn var(&self, name: &str) -> Option<String> {
        self.vars.get(name).cloned()
    }
}

fn expected(
    name: TerminalName,
    term_program: Option<&str>,
    version: Option<&str>,
    term: Option<&str>,
) -> TerminalInfo {
    TerminalInfo {
        name,
        term_program: term_program.map(ToString::to_string),
        version: version.map(ToString::to_string),
        term: term.map(ToString::to_string),
    }
}

#[test]
fn detects_supported_windows_term_programs() {
    for (program, name) in [
        ("WindowsTerminal", TerminalName::WindowsTerminal),
        ("vscode", TerminalName::VsCode),
        ("WarpTerminal", TerminalName::WarpTerminal),
        ("WezTerm", TerminalName::WezTerm),
        ("Alacritty", TerminalName::Alacritty),
    ] {
        let terminal = detect_terminal_info_from_env(
            &FakeEnvironment::default()
                .with_var("TERM_PROGRAM", program)
                .with_var("TERM_PROGRAM_VERSION", "1.2.3"),
        );
        assert_eq!(terminal, expected(name, Some(program), Some("1.2.3"), None));
        assert_eq!(terminal.user_agent_token(), format!("{program}/1.2.3"));
    }
}

#[test]
fn detects_windows_terminal_and_windows_compatible_markers() {
    assert_eq!(
        detect_terminal_info_from_env(
            &FakeEnvironment::default().with_var("WT_SESSION", "session")
        ),
        expected(TerminalName::WindowsTerminal, None, None, None)
    );
    assert_eq!(
        detect_terminal_info_from_env(
            &FakeEnvironment::default().with_var("WEZTERM_VERSION", "2024.2")
        ),
        expected(TerminalName::WezTerm, None, Some("2024.2"), None)
    );
    assert_eq!(
        detect_terminal_info_from_env(
            &FakeEnvironment::default().with_var("ALACRITTY_SOCKET", r"\\.\pipe\alacritty")
        ),
        expected(TerminalName::Alacritty, None, None, None)
    );
}

#[test]
fn retired_terminal_markers_do_not_create_non_windows_categories() {
    let terminal = detect_terminal_info_from_env(
        &FakeEnvironment::default()
            .with_var("KITTY_WINDOW_ID", "1")
            .with_var("ITERM_SESSION_ID", "1")
            .with_var("KONSOLE_VERSION", "1")
            .with_var("GNOME_TERMINAL_SCREEN", "1")
            .with_var("VTE_VERSION", "1"),
    );
    assert_eq!(terminal, expected(TerminalName::Unknown, None, None, None));
}

#[test]
fn term_fallbacks_are_preserved_without_non_windows_categories() {
    assert_eq!(
        detect_terminal_info_from_env(&FakeEnvironment::default().with_var("TERM", "dumb")),
        expected(TerminalName::Dumb, None, None, Some("dumb"))
    );
    assert_eq!(
        detect_terminal_info_from_env(
            &FakeEnvironment::default().with_var("TERM", "xterm-256color")
        ),
        expected(TerminalName::Unknown, None, None, Some("xterm-256color"))
    );
}
