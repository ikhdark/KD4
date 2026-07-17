use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use tokio::sync::Mutex;
use tokio::sync::MutexGuard;

/// Coordinates MCP projection publication without serializing candidate construction.
///
/// A ticket is reserved before any asynchronous projection, authentication, or manager
/// initialization work begins. Only the newest ticket may acquire the short publication guard,
/// so a slow stale candidate can never replace a newer runtime.
pub(crate) struct McpProjectionCoordinator {
    latest_generation: AtomicU64,
    publication_lock: Mutex<()>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct McpProjectionTicket(u64);

pub(crate) struct McpProjectionPublicationGuard<'a> {
    _guard: MutexGuard<'a, ()>,
}

impl McpProjectionCoordinator {
    pub(crate) fn new() -> Self {
        Self {
            latest_generation: AtomicU64::new(0),
            publication_lock: Mutex::new(()),
        }
    }

    pub(crate) fn begin(&self) -> McpProjectionTicket {
        McpProjectionTicket(
            self.latest_generation
                .fetch_add(1, Ordering::AcqRel)
                .wrapping_add(1),
        )
    }

    pub(crate) async fn lock_if_current(
        &self,
        ticket: McpProjectionTicket,
    ) -> Option<McpProjectionPublicationGuard<'_>> {
        let guard = self.publication_lock.lock().await;
        (self.latest_generation.load(Ordering::Acquire) == ticket.0).then_some(
            McpProjectionPublicationGuard { _guard: guard },
        )
    }
}

#[cfg(test)]
#[path = "mcp_projection_tests.rs"]
mod tests;
