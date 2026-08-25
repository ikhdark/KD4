//! Clipboard copy backend for the TUI's `/copy` command and `Ctrl+O` hotkey.
//!
//! This module decides *how* to get text onto the user's clipboard based on the
//! current environment. The selection order is:
//!
//! 1. **SSH session** (`SSH_TTY` / `SSH_CONNECTION` set): use tmux clipboard
//!    integration when available, otherwise OSC 52, because the native clipboard
//!    belongs to the remote machine.
//! 2. **Local session**: try `arboard` (native Windows clipboard) first, then fall
//!    back to terminal-mediated copy if the native clipboard is unavailable.
//!
//! The module is intentionally narrow: text copy only, user-facing error strings,
//! no reusable clipboard abstraction. Image paste lives in `clipboard_paste`.

use base64::Engine;
use std::io::Write;

/// Maximum raw bytes we will base64-encode into an OSC 52 sequence.
/// Large payloads are rejected before encoding to avoid overwhelming the terminal.
const OSC52_MAX_RAW_BYTES: usize = 100_000;

/// Copy text to the system clipboard.
///
/// Over SSH, uses terminal-mediated copy so the text reaches the *local*
/// terminal emulator's clipboard rather than a remote X11/Wayland clipboard
/// that the user cannot access. On a local session, tries `arboard` (native
/// clipboard) first and falls back to terminal-mediated copy if needed.
///
/// OSC 52 is supported by kitty, WezTerm, iTerm2, Ghostty, and others.
pub(crate) fn copy_to_clipboard(text: &str) -> Result<Option<ClipboardLease>, String> {
    copy_to_clipboard_with(
        text,
        CopyEnvironment {
            ssh_session: is_ssh_session(),
            tmux_session: is_tmux_session(),
        },
        tmux_clipboard_copy,
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

/// Core copy logic with injected backends, enabling deterministic unit tests
/// without touching real clipboards or terminal I/O.
#[derive(Clone, Copy)]
struct CopyEnvironment {
    ssh_session: bool,
    tmux_session: bool,
}

fn copy_to_clipboard_with(
    text: &str,
    environment: CopyEnvironment,
    tmux_copy_fn: impl Fn(&str) -> Result<(), String>,
    osc52_copy_fn: impl Fn(&str) -> Result<(), String>,
    arboard_copy_fn: impl Fn(&str) -> Result<Option<ClipboardLease>, String>,
) -> Result<Option<ClipboardLease>, String> {
    if environment.ssh_session {
        // Over SSH the native clipboard writes to the remote machine which is
        // useless. Terminal-mediated copy reaches the local terminal emulator.
        return terminal_clipboard_copy_with(
            text,
            environment.tmux_session,
            &tmux_copy_fn,
            &osc52_copy_fn,
        )
        .map(|()| None)
        .map_err(|terminal_err| {
            tracing::warn!("terminal clipboard copy failed over SSH: {terminal_err}");
            if environment.tmux_session {
                format!("terminal clipboard copy failed over SSH: {terminal_err}")
            } else {
                format!("OSC 52 clipboard copy failed over SSH: {terminal_err}")
            }
        });
    }

    match arboard_copy_fn(text) {
        Ok(lease) => Ok(lease),
        Err(native_err) => {
            tracing::warn!(
                "native clipboard copy failed: {native_err}, falling back to terminal clipboard"
            );
            terminal_clipboard_copy_with(
                text,
                environment.tmux_session,
                &tmux_copy_fn,
                &osc52_copy_fn,
            )
            .map(|()| None)
            .map_err(|terminal_err| {
                if environment.tmux_session {
                    format!("native clipboard: {native_err}; terminal fallback: {terminal_err}")
                } else {
                    format!("native clipboard: {native_err}; OSC 52 fallback: {terminal_err}")
                }
            })
        }
    }
}

/// Copy through the active terminal, preferring tmux's native clipboard path.
fn terminal_clipboard_copy_with(
    text: &str,
    tmux_session: bool,
    tmux_copy_fn: &impl Fn(&str) -> Result<(), String>,
    osc52_copy_fn: &impl Fn(&str) -> Result<(), String>,
) -> Result<(), String> {
    if tmux_session {
        match tmux_copy_fn(text) {
            Ok(()) => return Ok(()),
            Err(tmux_err) => {
                tracing::warn!("tmux clipboard copy failed: {tmux_err}, falling back to OSC 52");
                return osc52_copy_fn(text).map_err(|osc_err| {
                    format!("tmux clipboard: {tmux_err}; OSC 52 fallback: {osc_err}")
                });
            }
        }
    }

    osc52_copy_fn(text)
}

/// Detect whether the current process is running inside an SSH session.
fn is_ssh_session() -> bool {
    std::env::var_os("SSH_TTY").is_some() || std::env::var_os("SSH_CONNECTION").is_some()
}

/// Detect whether the current process is running inside tmux.
fn is_tmux_session() -> bool {
    std::env::var_os("TMUX").is_some() || std::env::var_os("TMUX_PANE").is_some()
}

fn arboard_copy(text: &str) -> Result<Option<ClipboardLease>, String> {
    let mut clipboard =
        arboard::Clipboard::new().map_err(|e| format!("clipboard unavailable: {e}"))?;
    clipboard
        .set_text(text)
        .map_err(|e| format!("failed to set clipboard text: {e}"))?;
    Ok(None)
}

/// Copy text through tmux's native clipboard integration.
///
/// `load-buffer -w -` lets tmux read the text from stdin, keep a matching tmux
/// paste buffer, and forward the contents to the outer terminal clipboard when
/// possible without relying on DCS passthrough.
fn tmux_clipboard_copy(text: &str) -> Result<(), String> {
    tmux_clipboard_copy_ready(
        || tmux_command_output(["show-options", "-gv", "set-clipboard"]),
        || tmux_command_output(["info"]),
    )?;

    let mut child = std::process::Command::new("tmux")
        .args(["load-buffer", "-w", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to spawn tmux: {e}"))?;

    let Some(mut stdin) = child.stdin.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err("failed to open tmux stdin".to_string());
    };

    if let Err(err) = stdin.write_all(text.as_bytes()) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("failed to write to tmux: {err}"));
    }

    drop(stdin);

    let output = child
        .wait_with_output()
        .map_err(|e| format!("failed to wait for tmux: {e}"))?;

    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if stderr.is_empty() {
            let status = output.status;
            Err(format!("tmux exited with status {status}"))
        } else {
            Err(format!("tmux failed: {stderr}"))
        }
    }
}

/// Verify that tmux is configured to forward clipboard writes to the outer terminal.
fn tmux_clipboard_copy_ready(
    set_clipboard_fn: impl FnOnce() -> Result<String, String>,
    tmux_info_fn: impl FnOnce() -> Result<String, String>,
) -> Result<(), String> {
    let set_clipboard = set_clipboard_fn()?;
    if set_clipboard.trim() == "off" {
        return Err("tmux clipboard forwarding is disabled".to_string());
    }

    let tmux_info = tmux_info_fn()?;
    if tmux_info.lines().any(|line| line.contains("Ms: [missing]")) {
        return Err("tmux clipboard forwarding is unavailable: missing Ms capability".to_string());
    }

    Ok(())
}

fn tmux_command_output<const N: usize>(args: [&str; N]) -> Result<String, String> {
    let output = std::process::Command::new("tmux")
        .args(args)
        .output()
        .map_err(|e| format!("failed to spawn tmux: {e}"))?;

    if output.status.success() {
        String::from_utf8(output.stdout).map_err(|e| format!("tmux output was not UTF-8: {e}"))
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if stderr.is_empty() {
            let status = output.status;
            Err(format!("tmux exited with status {status}"))
        } else {
            Err(format!("tmux failed: {stderr}"))
        }
    }
}

/// Write text to the clipboard via the OSC 52 terminal escape sequence.
fn osc52_copy(text: &str) -> Result<(), String> {
    let sequence = osc52_sequence(text, std::env::var_os("TMUX").is_some())?;

    write_osc52_to_writer(std::io::stdout().lock(), &sequence)
}

fn write_osc52_to_writer(mut writer: impl Write, sequence: &str) -> Result<(), String> {
    writer
        .write_all(sequence.as_bytes())
        .map_err(|e| format!("failed to write OSC 52: {e}"))?;
    writer
        .flush()
        .map_err(|e| format!("failed to flush OSC 52: {e}"))
}

fn osc52_sequence(text: &str, tmux: bool) -> Result<String, String> {
    let raw_bytes = text.len();
    if raw_bytes > OSC52_MAX_RAW_BYTES {
        return Err(format!(
            "OSC 52 payload too large ({raw_bytes} bytes; max {OSC52_MAX_RAW_BYTES})"
        ));
    }

    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    if tmux {
        Ok(format!("\x1bPtmux;\x1b\x1b]52;c;{encoded}\x07\x1b\\"))
    } else {
        Ok(format!("\x1b]52;c;{encoded}\x07"))
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use std::cell::Cell;

    use super::CopyEnvironment;
    use super::OSC52_MAX_RAW_BYTES;
    use super::copy_to_clipboard_with;
    use super::osc52_sequence;
    use super::tmux_clipboard_copy_ready;
    use super::write_osc52_to_writer;

    fn remote_environment() -> CopyEnvironment {
        CopyEnvironment {
            ssh_session: true,
            tmux_session: false,
        }
    }

    fn remote_tmux_environment() -> CopyEnvironment {
        CopyEnvironment {
            tmux_session: true,
            ..remote_environment()
        }
    }

    fn local_environment() -> CopyEnvironment {
        CopyEnvironment {
            ssh_session: false,
            tmux_session: false,
        }
    }

    fn local_tmux_environment() -> CopyEnvironment {
        CopyEnvironment {
            tmux_session: true,
            ..local_environment()
        }
    }

    #[test]
    fn osc52_encoding_roundtrips() {
        use base64::Engine;
        let text = "# Hello\n\n```rust\nfn main() {}\n```\n";
        let sequence = osc52_sequence(text, /*tmux*/ false).expect("OSC 52 sequence");
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
            osc52_sequence(&text, /*tmux*/ false),
            Err(format!(
                "OSC 52 payload too large ({} bytes; max {OSC52_MAX_RAW_BYTES})",
                OSC52_MAX_RAW_BYTES + 1
            ))
        );
    }

    #[test]
    fn osc52_wraps_tmux_passthrough() {
        assert_eq!(
            osc52_sequence("hello", /*tmux*/ true),
            Ok("\u{1b}Ptmux;\u{1b}\u{1b}]52;c;aGVsbG8=\u{7}\u{1b}\\".to_string())
        );
    }

    #[test]
    fn write_osc52_to_writer_emits_sequence_verbatim() {
        let sequence = "\u{1b}]52;c;aGVsbG8=\u{7}";
        let mut output = Vec::new();
        assert_eq!(write_osc52_to_writer(&mut output, sequence), Ok(()));
        assert_eq!(output, sequence.as_bytes());
    }

    #[test]
    fn ssh_uses_osc52_and_skips_native_on_success() {
        let tmux_calls = Cell::new(0_u8);
        let osc_calls = Cell::new(0_u8);
        let native_calls = Cell::new(0_u8);
        let result = copy_to_clipboard_with(
            "hello",
            remote_environment(),
            |_| {
                tmux_calls.set(tmux_calls.get() + 1);
                Ok(())
            },
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
        assert_eq!(tmux_calls.get(), 0);
        assert_eq!(osc_calls.get(), 1);
        assert_eq!(native_calls.get(), 0);
    }

    #[test]
    fn ssh_returns_osc52_error_and_skips_native() {
        let tmux_calls = Cell::new(0_u8);
        let osc_calls = Cell::new(0_u8);
        let native_calls = Cell::new(0_u8);
        let result = copy_to_clipboard_with(
            "hello",
            remote_environment(),
            |_| {
                tmux_calls.set(tmux_calls.get() + 1);
                Ok(())
            },
            |_| {
                osc_calls.set(osc_calls.get() + 1);
                Err("blocked".into())
            },
            |_| {
                native_calls.set(native_calls.get() + 1);
                Ok(None)
            },
        );

        let Err(error) = result else {
            panic!("expected OSC 52 error");
        };
        assert_eq!(error, "OSC 52 clipboard copy failed over SSH: blocked");
        assert_eq!(tmux_calls.get(), 0);
        assert_eq!(osc_calls.get(), 1);
        assert_eq!(native_calls.get(), 0);
    }

    #[test]
    fn ssh_inside_tmux_prefers_tmux_clipboard() {
        let tmux_calls = Cell::new(0_u8);
        let osc_calls = Cell::new(0_u8);
        let native_calls = Cell::new(0_u8);
        let result = copy_to_clipboard_with(
            "hello",
            remote_tmux_environment(),
            |_| {
                tmux_calls.set(tmux_calls.get() + 1);
                Ok(())
            },
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
        assert_eq!(tmux_calls.get(), 1);
        assert_eq!(osc_calls.get(), 0);
        assert_eq!(native_calls.get(), 0);
    }

    #[test]
    fn ssh_inside_tmux_falls_back_to_osc52_when_tmux_copy_fails() {
        let tmux_calls = Cell::new(0_u8);
        let osc_calls = Cell::new(0_u8);
        let native_calls = Cell::new(0_u8);
        let result = copy_to_clipboard_with(
            "hello",
            remote_tmux_environment(),
            |_| {
                tmux_calls.set(tmux_calls.get() + 1);
                Err("tmux unavailable".into())
            },
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
        assert_eq!(tmux_calls.get(), 1);
        assert_eq!(osc_calls.get(), 1);
        assert_eq!(native_calls.get(), 0);
    }

    #[test]
    fn ssh_inside_tmux_reports_tmux_and_osc52_errors_when_both_fail() {
        let result = copy_to_clipboard_with(
            "hello",
            remote_tmux_environment(),
            |_| Err("tmux unavailable".into()),
            |_| Err("osc blocked".into()),
            |_| Ok(None),
        );

        let Err(error) = result else {
            panic!("expected tmux and OSC 52 errors");
        };
        assert_eq!(
            error,
            "terminal clipboard copy failed over SSH: tmux clipboard: tmux unavailable; OSC 52 fallback: osc blocked"
        );
    }

    #[test]
    fn tmux_clipboard_copy_ready_accepts_forwarding_configuration() {
        let result = tmux_clipboard_copy_ready(
            || Ok("external\n".to_string()),
            || Ok("193: Ms: (string) \\033]52;%p1%s;%p2%s\\a\n".to_string()),
        );

        assert_eq!(result, Ok(()));
    }

    #[test]
    fn tmux_clipboard_copy_ready_rejects_disabled_forwarding() {
        let result = tmux_clipboard_copy_ready(
            || Ok("off\n".to_string()),
            || panic!("tmux info should not be queried when forwarding is disabled"),
        );

        assert_eq!(
            result,
            Err("tmux clipboard forwarding is disabled".to_string())
        );
    }

    #[test]
    fn tmux_clipboard_copy_ready_rejects_missing_ms_capability() {
        let result = tmux_clipboard_copy_ready(
            || Ok("external\n".to_string()),
            || Ok("193: Ms: [missing]\n".to_string()),
        );

        assert_eq!(
            result,
            Err("tmux clipboard forwarding is unavailable: missing Ms capability".to_string())
        );
    }

    #[test]
    fn local_uses_native_clipboard_first() {
        let osc_calls = Cell::new(0_u8);
        let native_calls = Cell::new(0_u8);
        let result = copy_to_clipboard_with(
            "hello",
            local_environment(),
            |_| Ok(()),
            |_| {
                osc_calls.set(osc_calls.get() + 1);
                Ok(())
            },
            |_| {
                native_calls.set(native_calls.get() + 1);
                Ok(Some(super::ClipboardLease::test()))
            },
        );

        assert!(matches!(result, Ok(Some(_))));
        assert_eq!(osc_calls.get(), 0);
        assert_eq!(native_calls.get(), 1);
    }

    #[test]
    fn local_falls_back_to_osc52_when_native_fails() {
        let osc_calls = Cell::new(0_u8);
        let native_calls = Cell::new(0_u8);
        let result = copy_to_clipboard_with(
            "hello",
            local_environment(),
            |_| Ok(()),
            |_| {
                osc_calls.set(osc_calls.get() + 1);
                Ok(())
            },
            |_| {
                native_calls.set(native_calls.get() + 1);
                Err("native unavailable".into())
            },
        );

        assert!(matches!(result, Ok(None)));
        assert_eq!(osc_calls.get(), 1);
        assert_eq!(native_calls.get(), 1);
    }

    #[test]
    fn local_tmux_fallback_prefers_tmux_when_native_fails() {
        let tmux_calls = Cell::new(0_u8);
        let osc_calls = Cell::new(0_u8);
        let native_calls = Cell::new(0_u8);
        let result = copy_to_clipboard_with(
            "hello",
            local_tmux_environment(),
            |_| {
                tmux_calls.set(tmux_calls.get() + 1);
                Ok(())
            },
            |_| {
                osc_calls.set(osc_calls.get() + 1);
                Ok(())
            },
            |_| {
                native_calls.set(native_calls.get() + 1);
                Err("native unavailable".into())
            },
        );

        assert!(matches!(result, Ok(None)));
        assert_eq!(tmux_calls.get(), 1);
        assert_eq!(osc_calls.get(), 0);
        assert_eq!(native_calls.get(), 1);
    }

    #[test]
    fn local_reports_both_errors_when_native_and_osc52_fail() {
        let osc_calls = Cell::new(0_u8);
        let native_calls = Cell::new(0_u8);
        let result = copy_to_clipboard_with(
            "hello",
            local_environment(),
            |_| Ok(()),
            |_| {
                osc_calls.set(osc_calls.get() + 1);
                Err("osc blocked".into())
            },
            |_| {
                native_calls.set(native_calls.get() + 1);
                Err("native unavailable".into())
            },
        );

        let Err(error) = result else {
            panic!("expected native and OSC 52 errors");
        };
        assert_eq!(
            error,
            "native clipboard: native unavailable; OSC 52 fallback: osc blocked"
        );
        assert_eq!(osc_calls.get(), 1);
        assert_eq!(native_calls.get(), 1);
    }
}
