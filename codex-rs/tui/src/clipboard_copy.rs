//! Windows clipboard copy backend for the TUI's `/copy` command and `Ctrl+O` hotkey.
//!
//! Native sessions use the Windows clipboard first. Native Windows SSH sessions use
//! OSC 52 so text reaches the client terminal's clipboard.

use base64::Engine;
use std::io::Write;

const OSC52_MAX_RAW_BYTES: usize = 100_000;

pub(crate) fn copy_to_clipboard(text: &str) -> Result<Option<ClipboardLease>, String> {
    copy_to_clipboard_with(
        text,
        CopyEnvironment {
            ssh_session: is_ssh_session(),
        },
        osc52_copy,
        arboard_copy,
    )
}

/// Marker returned by the native Windows clipboard backend.
pub(crate) struct ClipboardLease;

impl ClipboardLease {
    #[cfg(test)]
    pub(crate) fn test() -> Self {
        Self
    }
}

#[derive(Clone, Copy)]
struct CopyEnvironment {
    ssh_session: bool,
}

fn copy_to_clipboard_with(
    text: &str,
    environment: CopyEnvironment,
    osc52_copy_fn: impl Fn(&str) -> Result<(), String>,
    arboard_copy_fn: impl Fn(&str) -> Result<Option<ClipboardLease>, String>,
) -> Result<Option<ClipboardLease>, String> {
    if environment.ssh_session {
        return osc52_copy_fn(text)
            .map(|()| None)
            .map_err(|error| format!("OSC 52 clipboard copy failed over SSH: {error}"));
    }

    match arboard_copy_fn(text) {
        Ok(lease) => Ok(lease),
        Err(native_error) => {
            tracing::warn!(
                "native Windows clipboard copy failed: {native_error}, falling back to OSC 52"
            );
            osc52_copy_fn(text).map(|()| None).map_err(|osc_error| {
                format!("native clipboard: {native_error}; OSC 52 fallback: {osc_error}")
            })
        }
    }
}

fn is_ssh_session() -> bool {
    std::env::var_os("SSH_TTY").is_some() || std::env::var_os("SSH_CONNECTION").is_some()
}

fn arboard_copy(text: &str) -> Result<Option<ClipboardLease>, String> {
    let mut clipboard =
        arboard::Clipboard::new().map_err(|error| format!("clipboard unavailable: {error}"))?;
    clipboard
        .set_text(text)
        .map_err(|error| format!("failed to set clipboard text: {error}"))?;
    Ok(None)
}

fn osc52_copy(text: &str) -> Result<(), String> {
    let sequence = osc52_sequence(text)?;
    write_osc52_to_writer(std::io::stdout().lock(), &sequence)
}

fn write_osc52_to_writer(mut writer: impl Write, sequence: &str) -> Result<(), String> {
    writer
        .write_all(sequence.as_bytes())
        .map_err(|error| format!("failed to write OSC 52: {error}"))?;
    writer
        .flush()
        .map_err(|error| format!("failed to flush OSC 52: {error}"))
}

fn osc52_sequence(text: &str) -> Result<String, String> {
    let raw_bytes = text.len();
    if raw_bytes > OSC52_MAX_RAW_BYTES {
        return Err(format!(
            "OSC 52 payload too large ({raw_bytes} bytes; max {OSC52_MAX_RAW_BYTES})"
        ));
    }

    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    Ok(format!("\x1b]52;c;{encoded}\x07"))
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use std::cell::Cell;

    use super::*;

    #[test]
    fn osc52_encoding_roundtrips() {
        let text = "# Hello\n\n```rust\nfn main() {}\n```\n";
        let sequence = osc52_sequence(text).expect("OSC 52 sequence");
        let encoded = sequence
            .trim_start_matches("\u{1b}]52;c;")
            .trim_end_matches('\u{7}');
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap();
        assert_eq!(decoded, text.as_bytes());
    }

    #[test]
    fn osc52_rejects_payload_larger_than_limit() {
        let text = "x".repeat(OSC52_MAX_RAW_BYTES + 1);
        assert_eq!(
            osc52_sequence(&text),
            Err(format!(
                "OSC 52 payload too large ({} bytes; max {OSC52_MAX_RAW_BYTES})",
                OSC52_MAX_RAW_BYTES + 1
            ))
        );
    }

    #[test]
    fn ssh_uses_osc52_and_skips_native_clipboard() {
        let osc_calls = Cell::new(0_u8);
        let native_calls = Cell::new(0_u8);
        let result = copy_to_clipboard_with(
            "hello",
            CopyEnvironment { ssh_session: true },
            |_| {
                osc_calls.set(osc_calls.get() + 1);
                Ok(())
            },
            |_| {
                native_calls.set(native_calls.get() + 1);
                Ok(None)
            },
        );

        assert!(matches!(result, Ok(None)));
        assert_eq!(osc_calls.get(), 1);
        assert_eq!(native_calls.get(), 0);
    }

    #[test]
    fn local_uses_native_windows_clipboard_first() {
        let osc_calls = Cell::new(0_u8);
        let result = copy_to_clipboard_with(
            "hello",
            CopyEnvironment { ssh_session: false },
            |_| {
                osc_calls.set(osc_calls.get() + 1);
                Ok(())
            },
            |_| Ok(Some(ClipboardLease::test())),
        );

        assert!(matches!(result, Ok(Some(_))));
        assert_eq!(osc_calls.get(), 0);
    }

    #[test]
    fn local_falls_back_to_osc52_when_native_clipboard_fails() {
        let result = copy_to_clipboard_with(
            "hello",
            CopyEnvironment { ssh_session: false },
            |_| Ok(()),
            |_| Err("native unavailable".into()),
        );
        assert!(matches!(result, Ok(None)));
    }
}
