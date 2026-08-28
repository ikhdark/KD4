//! Compatibility cleanup hook for callers that also use Windows Job Objects or
//! direct child termination. Windows has no POSIX process-group signal path, so
//! this hook is intentionally a no-op.

use std::io;

/// Windows process-tree cleanup is owned by the caller's Job Object or child
/// handle; this compatibility hook performs no additional work.
pub fn kill_process_group(_process_group_id: u32) -> io::Result<()> {
    Ok(())
}
