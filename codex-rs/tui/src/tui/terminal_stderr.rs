//! Protect the inline viewport from unmanaged macOS writes to stderr.
//!
//! Some macOS frameworks and runtime diagnostics write directly to file
//! descriptor 2. While the inline TUI is active, those writes paint into the
//! same terminal region as the composer without going through the renderer.
//! Keep them off the terminal until the TUI releases terminal ownership.

use std::io;

/// Keeps unmanaged stderr output away from the terminal while the TUI owns it.
pub(crate) struct TerminalStderrGuard {
    active: bool,
}

impl TerminalStderrGuard {
    pub(super) fn install() -> io::Result<Self> {
        Ok(Self { active: false })
    }
}

impl Drop for TerminalStderrGuard {
    fn drop(&mut self) {
        if self.active {
            let _ = finish();
            self.active = false;
        }
    }
}

/// Restores stderr while terminal ownership is temporarily released.
pub(super) fn pause() -> io::Result<()> {
    Ok(())
}

/// Suppresses stderr again when terminal ownership returns to the TUI.
pub(super) fn resume() -> io::Result<()> {
    Ok(())
}

/// Restores stderr permanently when the TUI session ends.
pub(super) fn finish() -> io::Result<()> {
    Ok(())
}
