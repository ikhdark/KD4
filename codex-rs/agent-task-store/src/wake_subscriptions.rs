//! Filters durable wake notifications by subscribed root, not database commits.
use super::*;
use std::collections::HashMap;

type StreamVersion = Option<(i64, i64, Option<String>)>;

struct Interest {
    waiters: usize,
    revision: watch::Sender<u64>,
    observed: Option<StreamVersion>,
    state_revision: watch::Sender<u64>,
    observed_state: Option<Option<i64>>,
}

#[derive(Default)]
pub(super) struct WakeSubscriptions {
    roots: Mutex<HashMap<String, Interest>>,
}

impl WakeSubscriptions {
    pub(super) fn register(&self, root: &str, state: bool) -> watch::Receiver<u64> {
        let mut roots = self
            .roots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let interest = roots.entry(root.into()).or_insert_with(|| Interest {
            waiters: 0,
            revision: watch::channel(0).0,
            observed: None,
            state_revision: watch::channel(0).0,
            observed_state: None,
        });
        interest.waiters += 1;
        if state { interest.state_revision.subscribe() } else { interest.revision.subscribe() }
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
        let (names, state_needed) = {
            let roots = self
            .roots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
            (roots.keys().cloned().collect::<Vec<_>>(),
                roots.values().any(|interest| interest.state_revision.receiver_count() > 0))
        };
        if names.is_empty() {
            return Ok(());
        }
        // One consistent observation for all subscribed roots. Keep missing rows
        // distinct from an unobserved interest and retain event identity so a
        // delete/reinsert with reused sequence values still wakes subscribers.
        // Keep ordinary wake-only consumers on their existing stream query;
        // task authority lookups are needed only by readiness subscribers.
        let query = if state_needed {
            "SELECT names.value, wakes.next_sequence, wakes.retained_from_sequence,
                    wakes.latest_event_id, state.revision
             FROM json_each(?) names
             LEFT JOIN wake_streams wakes ON wakes.root_session_id = names.value
             LEFT JOIN root_task_state_revision state ON state.root_session_id = names.value"
        } else {
            "SELECT root_session_id, next_sequence, retained_from_sequence, latest_event_id, NULL
             FROM wake_streams WHERE root_session_id IN (SELECT value FROM json_each(?))"
        };
        let versions: Vec<(String, Option<i64>, Option<i64>, Option<String>, Option<i64>)> = sqlx::query_as(query)
        .bind(serde_json::to_string(&names)?)
        .fetch_all(&mut *connection)
        .await?;
        let mut versions = versions.into_iter()
            .map(|(root, next, retained, event, state)| (root, (next.zip(retained).map(|(next, retained)| (next, retained, event)), state)))
            .collect::<HashMap<_, _>>();
        let mut roots = self
            .roots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for root in names {
            let (version, state) = versions.remove(&root).unwrap_or_default();
            if let Some(interest) = roots.get_mut(&root) {
                if interest.observed.as_ref() != Some(&version) {
                    interest.observed = Some(version);
                    interest.revision.send_modify(|value| *value = value.wrapping_add(1));
                }
                if interest.observed_state != Some(state) {
                    interest.observed_state = Some(state);
                    interest.state_revision.send_modify(|value| *value = value.wrapping_add(1));
                }
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
            interest.state_revision.send_modify(|value| *value = value.wrapping_add(1));
            interest
                .revision
                .send_modify(|value| *value = value.saturating_add(1));
        }
    }
}
