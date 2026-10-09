mod bel;
mod osc9;

use std::io;

use bel::BelBackend;
use codex_config::types::NotificationMethod;
use codex_terminal_detection::TerminalInfo;
use codex_terminal_detection::TerminalName;
use codex_terminal_detection::terminal_info;
use osc9::Osc9Backend;

#[derive(Debug)]
pub enum DesktopNotificationBackend {
    Osc9(Osc9Backend),
    Bel(BelBackend),
}

impl DesktopNotificationBackend {
    pub fn for_method(method: NotificationMethod) -> Self {
        match method {
            NotificationMethod::Auto => {
                if supports_osc9(&terminal_info()) {
                    Self::Osc9(Osc9Backend::new())
                } else {
                    Self::Bel(BelBackend)
                }
            }
            NotificationMethod::Osc9 => Self::Osc9(Osc9Backend::new()),
            NotificationMethod::Bel => Self::Bel(BelBackend),
        }
    }

    pub fn method(&self) -> NotificationMethod {
        match self {
            DesktopNotificationBackend::Osc9(_) => NotificationMethod::Osc9,
            DesktopNotificationBackend::Bel(_) => NotificationMethod::Bel,
        }
    }

    pub fn notify(&mut self, message: &str) -> io::Result<()> {
        match self {
            DesktopNotificationBackend::Osc9(backend) => backend.notify(message),
            DesktopNotificationBackend::Bel(backend) => backend.notify(message),
        }
    }
}

pub fn detect_backend(method: NotificationMethod) -> DesktopNotificationBackend {
    DesktopNotificationBackend::for_method(method)
}

fn supports_osc9(terminal: &TerminalInfo) -> bool {
    matches!(
        terminal.name,
        TerminalName::WarpTerminal | TerminalName::WezTerm
    )
}

#[cfg(test)]
mod tests {
    use super::detect_backend;
    use super::supports_osc9;
    use codex_config::types::NotificationMethod;
    use codex_terminal_detection::TerminalInfo;
    use codex_terminal_detection::TerminalName;
    use codex_terminal_detection::terminal_info;
    use pretty_assertions::assert_eq;
    use std::io;

    fn test_terminal(name: TerminalName) -> TerminalInfo {
        TerminalInfo {
            name,
            term_program: None,
            version: None,
            term: None,
            multiplexer: None,
        }
    }

    #[test]
    fn terminal_detection_uses_process_environment() -> io::Result<()> {
        const CHILD: &str = "CODEX_TERMINAL_DETECTION_CHILD";
        if std::env::var_os(CHILD).is_some() {
            assert_eq!(terminal_info().name, TerminalName::VsCode);
            assert_eq!(codex_terminal_detection::user_agent(), "vscode/codex-test");
            assert_eq!(terminal_info().multiplexer, None);
            return Ok(());
        }

        let output = std::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "notifications::tests::terminal_detection_uses_process_environment",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("TERM_PROGRAM", "vscode")
            .env("TERM_PROGRAM_VERSION", "codex-test")
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env_remove("ZELLIJ")
            .env_remove("ZELLIJ_SESSION_NAME")
            .env_remove("ZELLIJ_VERSION")
            .output()?;
        assert!(output.status.success(), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        Ok(())
    }

    #[test]
    fn selects_explicit_notification_method() {
        for method in [NotificationMethod::Osc9, NotificationMethod::Bel] {
            let backend = detect_backend(method);
            assert!(matches!(
                (&backend, method),
                (super::DesktopNotificationBackend::Osc9(_), NotificationMethod::Osc9)
                    | (super::DesktopNotificationBackend::Bel(_), NotificationMethod::Bel)
            ));
            assert_eq!(backend.method(), method);
        }
    }

    #[test]
    fn supports_osc9_only_for_supported_terminals() {
        for (name, supported) in [
            (TerminalName::WarpTerminal, true),
            (TerminalName::WezTerm, true),
            (TerminalName::Alacritty, false),
            (TerminalName::Dumb, false),
            (TerminalName::Unknown, false),
            (TerminalName::VsCode, false),
            (TerminalName::WindowsTerminal, false),
        ] {
            assert_eq!(
                supports_osc9(&test_terminal(name)),
                supported,
                "{name:?}"
            );
        }
    }
}
