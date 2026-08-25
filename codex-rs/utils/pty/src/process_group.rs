//! Process-group helpers shared by pipe/pty and shell command execution.
//!
//! This module centralizes the OS-specific pieces that ensure a spawned
//! command can be cleaned up reliably:
//! - `set_process_group` is called in `pre_exec` so the child starts its own
//!   process group.
//! - `detach_from_tty` starts a new session so non-interactive children do not
//!   inherit the controlling TTY.
//! - `kill_process_group_by_pid` targets the whole group (children/grandchildren)
//! - `kill_process_group` targets a known process group ID directly
//!   instead of a single PID.
//! - `set_parent_death_signal` (Linux only) arranges for the child to receive a
//!   `SIGTERM` when the parent exits, and re-checks the parent PID to avoid
//!   races during fork/exec.
//!
//! On non-Unix platforms these helpers are no-ops.

use std::io;

use tokio::process::Child;

/// No-op on non-Linux platforms.
pub fn set_parent_death_signal(_parent_pid: i32) -> io::Result<()> {
    Ok(())
}

/// No-op on non-Unix platforms.
pub fn detach_from_tty() -> io::Result<()> {
    Ok(())
}

/// No-op on non-Unix platforms.
pub fn set_process_group() -> io::Result<()> {
    Ok(())
}

/// No-op on non-Unix platforms.
pub fn kill_process_group_by_pid(_pid: u32) -> io::Result<()> {
    Ok(())
}

/// No-op on non-Unix platforms.
pub fn terminate_process_group(_process_group_id: u32) -> io::Result<bool> {
    Ok(false)
}

/// No-op on non-Unix platforms.
pub fn interrupt_process_group(_process_group_id: u32) -> io::Result<()> {
    Ok(())
}

/// No-op on non-Unix platforms.
pub fn kill_process_group(_process_group_id: u32) -> io::Result<()> {
    Ok(())
}

/// No-op on non-Unix platforms.
pub fn kill_child_process_group(_child: &mut Child) -> io::Result<()> {
    Ok(())
}
