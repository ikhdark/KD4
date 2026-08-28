use std::fmt;
use std::io;
use std::io::stdout;

use crossterm::Command;
use ratatui::crossterm::execute;

#[derive(Debug)]
pub struct Osc9Backend;

impl Default for Osc9Backend {
    fn default() -> Self {
        Self::new()
    }
}

impl Osc9Backend {
    pub fn new() -> Self {
        Self
    }

    pub fn notify(&mut self, message: &str) -> io::Result<()> {
        execute!(
            stdout(),
            PostNotification {
                message: message.to_string(),
            }
        )
    }
}

/// Command that emits an OSC 9 desktop notification with a message.
#[derive(Debug, Clone)]
pub struct PostNotification {
    pub message: String,
}

impl Command for PostNotification {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        let message = sanitize_osc9_message(&self.message);
        write!(f, "\x1b]9;{message}\x07")
    }

    fn execute_winapi(&self) -> io::Result<()> {
        Err(std::io::Error::other(
            "tried to execute PostNotification using WinAPI; use ANSI instead",
        ))
    }

    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

fn sanitize_osc9_message(message: &str) -> String {
    message
        .chars()
        .filter(|character| !matches!(character, '\u{0000}'..='\u{001f}' | '\u{007f}'..='\u{009f}'))
        .collect()
}

#[cfg(test)]
mod tests {
    use crossterm::Command;
    use pretty_assertions::assert_eq;

    use super::PostNotification;

    fn control_characters() -> String {
        (0_u32..=0x1f)
            .chain(0x7f..=0x9f)
            .filter_map(char::from_u32)
            .collect()
    }

    #[test]
    fn post_notification_writes_plain_osc9_sequence() {
        let mut ansi = String::new();
        let command = PostNotification {
            message: "hello".to_string(),
        };

        command
            .write_ansi(&mut ansi)
            .expect("OSC 9 command should format");

        assert_eq!(ansi, "\u{1b}]9;hello\u{7}");
    }

    #[test]
    fn post_notification_sanitizes_controls_before_plain_framing() {
        let mut ansi = String::new();
        let command = PostNotification {
            message: format!("safe λ🙂{}終", control_characters()),
        };

        command
            .write_ansi(&mut ansi)
            .expect("OSC 9 command should format");

        assert_eq!(ansi, "\u{1b}]9;safe λ🙂終\u{7}");
    }
}
