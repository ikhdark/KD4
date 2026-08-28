//! Windows terminal detection utilities.
//!
//! This module feeds terminal metadata into OpenTelemetry user-agent logging and into
//! terminal-specific configuration choices in the TUI.

use std::sync::OnceLock;

/// Structured terminal identification data.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalInfo {
    pub name: TerminalName,
    pub term_program: Option<String>,
    pub version: Option<String>,
    pub term: Option<String>,
}

/// Terminal categories supported by the Windows runtime.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalName {
    WarpTerminal,
    VsCode,
    WezTerm,
    Alacritty,
    WindowsTerminal,
    Dumb,
    Unknown,
}

impl TerminalInfo {
    fn new(
        name: TerminalName,
        term_program: Option<String>,
        version: Option<String>,
        term: Option<String>,
    ) -> Self {
        Self {
            name,
            term_program,
            version,
            term,
        }
    }

    fn from_term_program(
        name: TerminalName,
        term_program: String,
        version: Option<String>,
    ) -> Self {
        Self::new(name, Some(term_program), version, None)
    }

    fn from_name(name: TerminalName, version: Option<String>) -> Self {
        Self::new(name, None, version, None)
    }

    fn from_term(term: String) -> Self {
        let name = match term.as_str() {
            "dumb" => TerminalName::Dumb,
            "wezterm" | "wezterm-mux" => TerminalName::WezTerm,
            "alacritty" => TerminalName::Alacritty,
            _ => TerminalName::Unknown,
        };
        Self::new(name, None, None, Some(term))
    }

    fn unknown() -> Self {
        Self::new(TerminalName::Unknown, None, None, None)
    }

    fn user_agent_token(&self) -> String {
        let raw = if let Some(program) = self.term_program.as_ref() {
            match self.version.as_ref().filter(|value| !value.is_empty()) {
                Some(version) => format!("{program}/{version}"),
                None => program.clone(),
            }
        } else if let Some(term) = self.term.as_ref().filter(|value| !value.is_empty()) {
            term.clone()
        } else {
            match self.name {
                TerminalName::WarpTerminal => {
                    format_terminal_version("WarpTerminal", &self.version)
                }
                TerminalName::VsCode => format_terminal_version("vscode", &self.version),
                TerminalName::WezTerm => format_terminal_version("WezTerm", &self.version),
                TerminalName::Alacritty => format_terminal_version("Alacritty", &self.version),
                TerminalName::WindowsTerminal => "WindowsTerminal".to_string(),
                TerminalName::Dumb => "dumb".to_string(),
                TerminalName::Unknown => "unknown".to_string(),
            }
        };
        sanitize_header_value(raw)
    }
}

static TERMINAL_INFO: OnceLock<TerminalInfo> = OnceLock::new();

trait Environment {
    fn var(&self, name: &str) -> Option<String>;

    fn has(&self, name: &str) -> bool {
        self.var(name).is_some()
    }

    fn var_non_empty(&self, name: &str) -> Option<String> {
        self.var(name).and_then(none_if_whitespace)
    }
}

struct ProcessEnvironment;

impl Environment for ProcessEnvironment {
    fn var(&self, name: &str) -> Option<String> {
        match std::env::var(name) {
            Ok(value) => Some(value),
            Err(std::env::VarError::NotPresent) => None,
            Err(std::env::VarError::NotUnicode(_)) => {
                tracing::warn!("failed to read env var {name}: value not valid UTF-8");
                None
            }
        }
    }
}

/// Returns a sanitized terminal identifier for User-Agent strings.
pub fn user_agent() -> String {
    terminal_info().user_agent_token()
}

/// Returns structured terminal metadata for the current process.
pub fn terminal_info() -> TerminalInfo {
    TERMINAL_INFO
        .get_or_init(|| detect_terminal_info_from_env(&ProcessEnvironment))
        .clone()
}

/// Detects native Windows terminal metadata without invoking external processes.
fn detect_terminal_info_from_env(env: &dyn Environment) -> TerminalInfo {
    if let Some(term_program) = env.var_non_empty("TERM_PROGRAM") {
        let version = env.var_non_empty("TERM_PROGRAM_VERSION");
        let name = terminal_name_from_term_program(&term_program).unwrap_or(TerminalName::Unknown);
        return TerminalInfo::from_term_program(name, term_program, version);
    }

    if env.has("WT_SESSION") {
        return TerminalInfo::from_name(TerminalName::WindowsTerminal, None);
    }
    if env.has("WEZTERM_VERSION") {
        return TerminalInfo::from_name(
            TerminalName::WezTerm,
            env.var_non_empty("WEZTERM_VERSION"),
        );
    }
    if env.has("ALACRITTY_SOCKET") {
        return TerminalInfo::from_name(TerminalName::Alacritty, None);
    }
    if let Some(term) = env.var_non_empty("TERM") {
        return TerminalInfo::from_term(term);
    }
    TerminalInfo::unknown()
}

fn terminal_name_from_term_program(value: &str) -> Option<TerminalName> {
    let normalized: String = value
        .trim()
        .chars()
        .filter(|character| !matches!(character, ' ' | '-' | '_' | '.'))
        .map(|character| character.to_ascii_lowercase())
        .collect();
    match normalized.as_str() {
        "warp" | "warpterminal" => Some(TerminalName::WarpTerminal),
        "vscode" => Some(TerminalName::VsCode),
        "wezterm" => Some(TerminalName::WezTerm),
        "alacritty" => Some(TerminalName::Alacritty),
        "windowsterminal" => Some(TerminalName::WindowsTerminal),
        "dumb" => Some(TerminalName::Dumb),
        _ => None,
    }
}

fn sanitize_header_value(value: String) -> String {
    value.replace(|character| !is_valid_header_value_char(character), "_")
}

fn is_valid_header_value_char(character: char) -> bool {
    character.is_ascii_alphanumeric()
        || character == '-'
        || character == '_'
        || character == '.'
        || character == '/'
}

fn format_terminal_version(name: &str, version: &Option<String>) -> String {
    match version.as_ref().filter(|value| !value.is_empty()) {
        Some(version) => format!("{name}/{version}"),
        None => name.to_string(),
    }
}

fn none_if_whitespace(value: String) -> Option<String> {
    (!value.trim().is_empty()).then_some(value)
}

#[cfg(test)]
#[path = "terminal_tests.rs"]
mod tests;
