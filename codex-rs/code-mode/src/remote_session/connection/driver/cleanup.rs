use tokio_util::sync::CancellationToken;

use super::notify_cell_closed;
use super::session_registry::CellOwner;

#[derive(Clone)]
pub(in crate::remote_session) struct SessionCleanup {
    complete: CancellationToken,
}

impl SessionCleanup {
    pub(in crate::remote_session) fn new() -> Self {
        Self {
            complete: CancellationToken::new(),
        }
    }

    pub(super) fn fail(&self, cells: Vec<CellOwner>) {
        for owner in cells {
            notify_cell_closed(&owner.delegate, &owner.cell_id);
        }
        self.finish();
    }

    pub(super) fn finish(&self) {
        self.complete.cancel();
    }

    pub(in crate::remote_session) async fn wait(&self) {
        self.complete.cancelled().await;
    }

    pub(in crate::remote_session) fn is_complete(&self) -> bool {
        self.complete.is_cancelled()
    }
}
