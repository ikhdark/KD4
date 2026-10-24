//! One task-store owner per live Codex home. Roots retain their own task authority.
use crate::LocalAgentTaskStore;
use crate::StoreResult;
use codex_state::StateRuntime;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::Weak;

struct Initialization {
    cached: Mutex<Weak<LocalAgentTaskStore>>,
    gate: tokio::sync::Semaphore,
}

impl Default for Initialization {
    fn default() -> Self {
        Self {
            cached: Mutex::default(),
            gate: tokio::sync::Semaphore::new(1),
        }
    }
}

static STORES: OnceLock<Mutex<HashMap<PathBuf, Arc<Initialization>>>> = OnceLock::new();

impl LocalAgentTaskStore {
    /// Shares the pool, startup repair, and wake poller, never root/task authorization.
    /// Failed or cancelled initialization is not cached. The final owner releases
    /// the service; explicit `close` shuts down that entire shared service.
    pub async fn initialize_shared(state: &StateRuntime) -> StoreResult<Arc<Self>> {
        let home = tokio::fs::canonicalize(state.codex_home()).await?;
        let initialization = {
            let mut stores = STORES
                .get_or_init(Mutex::default)
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            stores.retain(|_, entry| {
                Arc::strong_count(entry) > 1
                    || entry
                        .cached
                        .try_lock()
                        .map_or(true, |store| store.strong_count() > 0)
            });
            Arc::clone(
                stores
                    .entry(home)
                    .or_insert_with(|| Arc::new(Initialization::default())),
            )
        };
        // Serialize same-home I/O with a cancellable permit, not a data lock.
        // Dropping a failed/cancelled initializer releases the permit for retry.
        let _permit = initialization
            .gate
            .acquire()
            .await
            .map_err(std::io::Error::other)?;
        let cached = initialization
            .cached
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .upgrade();
        if let Some(store) = cached
            && !store.is_closed()
        {
            return Ok(store);
        }
        let store = Arc::new(Self::initialize(state).await?);
        *initialization
            .cached
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Arc::downgrade(&store);
        Ok(store)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn audit151_concurrent_roots_share_one_store_and_reopen_after_close() {
        let home = tempfile::tempdir().unwrap();
        let state = StateRuntime::init(home.path().into(), "test".into())
            .await
            .unwrap();
        let stores = futures::future::try_join_all(
            (0..8).map(|_| LocalAgentTaskStore::initialize_shared(&state)),
        )
        .await
        .unwrap();
        assert!(stores.iter().all(|store| Arc::ptr_eq(store, &stores[0])));
        stores[0].close().await;
        let reopened = LocalAgentTaskStore::initialize_shared(&state)
            .await
            .unwrap();
        assert!(!Arc::ptr_eq(&reopened, &stores[0]));
        assert!(!reopened.is_closed());
        reopened.close().await;
        state.close().await;
    }

    #[tokio::test]
    async fn audit151_different_homes_do_not_share_store_authority() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let a = StateRuntime::init(first.path().into(), "test".into())
            .await
            .unwrap();
        let b = StateRuntime::init(second.path().into(), "test".into())
            .await
            .unwrap();
        let (x, y) = tokio::join!(
            LocalAgentTaskStore::initialize_shared(&a),
            LocalAgentTaskStore::initialize_shared(&b),
        );
        let (x, y) = (x.unwrap(), y.unwrap());
        assert!(!Arc::ptr_eq(&x, &y));
        x.close().await;
        y.close().await;
        a.close().await;
        b.close().await;
    }
}
