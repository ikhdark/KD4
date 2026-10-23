//! Filters durable wake notifications by subscribed root, not database commits.
use super::*;
use std::collections::HashMap;

type StreamVersion = Option<(i64, i64, Option<String>)>;

struct Interest {
    waiters: usize,
    revision: watch::Sender<u64>,
    observed: Option<StreamVersion>,
}

#[derive(Default)]
pub(super) struct WakeSubscriptions {
    roots: Mutex<HashMap<String, Interest>>,
}

impl WakeSubscriptions {
    pub(super) fn register(&self, root: &str) -> watch::Receiver<u64> {
        let mut roots = self
            .roots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let interest = roots.entry(root.into()).or_insert_with(|| Interest {
            waiters: 0,
            revision: watch::channel(0).0,
            observed: None,
        });
        interest.waiters += 1;
        interest.revision.subscribe()
    }

    pub(super) fn unregister(&self, root: &str) {
        let mut roots = self
            .roots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(interest) = roots.get_mut(root) {
            interest.waiters -= 1;
            if interest.waiters == 0 {
                roots.remove(root);
            }
        }
    }

    pub(super) async fn refresh(&self, connection: &mut sqlx::SqliteConnection) -> StoreResult<()> {
        let names = self
            .roots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        for root in names {
            // The event identity also detects delete/reinsert with reused rowids.
            let version: StreamVersion = sqlx::query_as(
                "SELECT next_sequence, retained_from_sequence, latest_event_id
                 FROM wake_streams WHERE root_session_id = ?",
            )
            .bind(&root)
            .fetch_optional(&mut *connection)
            .await?;
            let mut roots = self
                .roots
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(interest) = roots.get_mut(&root)
                && interest.observed.as_ref() != Some(&version)
            {
                interest.observed = Some(version);
                interest
                    .revision
                    .send_modify(|value| *value = value.saturating_add(1));
            }
        }
        Ok(())
    }

    pub(super) fn notify_error(&self) {
        let roots = self
            .roots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for interest in roots.values() {
            interest
                .revision
                .send_modify(|value| *value = value.saturating_add(1));
        }
    }
}
