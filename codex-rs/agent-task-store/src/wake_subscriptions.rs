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
        if names.is_empty() {
            return Ok(());
        }
        // One consistent observation for all subscribed roots. Keep missing rows
        // distinct from an unobserved interest and retain event identity so a
        // delete/reinsert with reused sequence values still wakes subscribers.
        let versions: Vec<(String, i64, i64, Option<String>)> = sqlx::query_as(
            "SELECT root_session_id, next_sequence, retained_from_sequence, latest_event_id
             FROM wake_streams WHERE root_session_id IN (SELECT value FROM json_each(?))",
        )
        .bind(serde_json::to_string(&names)?)
        .fetch_all(&mut *connection)
        .await?;
        let mut versions = versions.into_iter()
            .map(|(root, next, retained, event)| (root, (next, retained, event)))
            .collect::<HashMap<_, _>>();
        let mut roots = self
            .roots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for root in names {
            let version: StreamVersion = versions.remove(&root);
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
