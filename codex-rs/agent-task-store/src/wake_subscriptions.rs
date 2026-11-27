//! Filters durable wake notifications by subscribed root, not database commits.
use super::*;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::ops::Bound::{Excluded, Included, Unbounded};

const INTEREST_SNAPSHOT_PAGE_SIZE: usize = 32;

type StreamVersion = Option<(i64, i64, Option<String>)>;

struct Interest {
    waiters: usize,
    wake: Arc<WakeInterest>,
}

struct WakeInterest {
    revision: watch::Sender<u64>,
    state_revision: watch::Sender<u64>,
    observed: Mutex<Observed>,
}

#[derive(Default)]
struct Observed {
    stream: Option<StreamVersion>,
    state: Option<Option<i64>>,
}

#[derive(Default)]
pub(super) struct WakeSubscriptions {
    roots: Mutex<BTreeMap<String, Interest>>,
}

impl WakeSubscriptions {
    pub(super) fn register(&self, root: &str, state: bool) -> watch::Receiver<u64> {
        let mut roots = self
            .roots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let interest = roots.entry(root.into()).or_insert_with(|| Interest {
            waiters: 0,
            wake: Arc::new(WakeInterest {
                revision: watch::channel(0).0,
                state_revision: watch::channel(0).0,
                observed: Mutex::new(Observed::default()),
            }),
        });
        interest.waiters += 1;
        if state {
            interest.wake.state_revision.subscribe()
        } else {
            interest.wake.revision.subscribe()
        }
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
        let mut names = Vec::new();
        let mut state_needed = false;
        self.visit_interests(|root, wake| {
            names.push(root);
            state_needed |= wake.state_revision.receiver_count() > 0;
        });
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
        for root in names {
            // Re-resolve the queried name after the read so a root whose
            // last waiter dropped can transfer wakes to its new registration.
            let wake = self
                .roots
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&root)
                .map(|interest| Arc::clone(&interest.wake));
            let Some(wake) = wake else { continue };
            let (version, state) = versions.remove(&root).unwrap_or_default();
            let mut observed = wake
                .observed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if observed.stream.as_ref() != Some(&version) {
                observed.stream = Some(version);
                wake.revision.send_modify(|value| *value = value.wrapping_add(1));
            }
            if observed.state != Some(state) {
                observed.state = Some(state);
                wake.state_revision.send_modify(|value| *value = value.wrapping_add(1));
            }
        }
        Ok(())
    }

    pub(super) fn notify_error(&self) {
        self.visit_interests(|_, wake| {
            wake.state_revision.send_modify(|value| *value = value.wrapping_add(1));
            wake.revision.send_modify(|value| *value = value.saturating_add(1));
        });
    }

    // Copy a bounded ordered window while holding the registration lock.
    // New keys beyond the initial horizon or behind the cursor are picked
    // up on the next refresh. The visitor itself never holds the lock.
    fn visit_interests(&self, mut visit: impl FnMut(String, Arc<WakeInterest>)) {
        let horizon = self
            .roots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last_key_value()
            .map(|(root, _)| root.clone());
        let Some(horizon) = horizon else { return };
        let mut after: Option<String> = None;
        loop {
            let page = self
                .roots
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .range::<String, _>((after.as_ref().map_or(Unbounded, Excluded), Included(&horizon)))
                .take(INTEREST_SNAPSHOT_PAGE_SIZE)
                .map(|(root, interest)| (root.clone(), Arc::clone(&interest.wake)))
                .collect::<Vec<_>>();
            let Some((last, _)) = page.last() else { break };
            let done = page.len() < INTEREST_SNAPSHOT_PAGE_SIZE || last == &horizon;
            if !done {
                after = Some(last.clone());
            }
            for (root, wake) in page {
                visit(root, wake);
            }
            if done {
                break;
            }
        }
    }
}
