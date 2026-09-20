use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::OnceLock;

use chrono::Utc;
use codex_protocol::ThreadId;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::SessionContextWindow;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadMemoryMode;
use codex_rollout::persisted_rollout_items;

use crate::AppendThreadItemsParams;
use crate::ArchiveThreadParams;
use crate::CreateThreadParams;
use crate::DeleteThreadParams;
use crate::ListThreadsParams;
use crate::LoadThreadHistoryParams;
use crate::ReadThreadByRolloutPathParams;
use crate::ReadThreadParams;
use crate::ResumeThreadParams;
use crate::StoredThread;
use crate::StoredThreadHistory;
use crate::ThreadMetadataPatch;
use crate::ThreadPage;
use crate::ThreadRelationFilter;
use crate::ThreadStore;
use crate::ThreadStoreError;
use crate::ThreadStoreFuture;
use crate::ThreadStoreResult;
use crate::UpdateThreadMetadataParams;
use crate::error::reject_paginated_history_mode;
use crate::types::canonical_history_mode_from_rollout_items;

static IN_MEMORY_THREAD_STORES: OnceLock<Mutex<HashMap<String, Arc<InMemoryThreadStore>>>> =
    OnceLock::new();

fn stores() -> &'static Mutex<HashMap<String, Arc<InMemoryThreadStore>>> {
    IN_MEMORY_THREAD_STORES.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ListItemsParams;
    use crate::ListTurnsParams;
    use crate::LiveThread;
    use crate::SortDirection;
    use crate::StoredTurnItemsView;
    use crate::ThreadPersistenceMetadata;
    use crate::ThreadSortKey;
    use codex_protocol::models::BaseInstructions;
    use codex_protocol::models::FunctionCallOutputBody;
    use codex_protocol::models::FunctionCallOutputPayload;
    use codex_protocol::models::ResponseItem;
    use codex_protocol::protocol::EventMsg;
    use codex_protocol::protocol::SessionSource;
    use codex_protocol::protocol::TurnCompleteEvent;

    #[tokio::test]
    async fn default_turn_pagination_methods_return_unsupported() {
        let store = InMemoryThreadStore::default();
        let thread_id = ThreadId::default();

        let turns_err = store
            .list_turns(ListTurnsParams {
                thread_id,
                include_archived: true,
                cursor: None,
                page_size: 10,
                sort_direction: SortDirection::Asc,
                items_view: StoredTurnItemsView::Summary,
            })
            .await
            .expect_err("default list_turns should be unsupported");
        assert!(matches!(
            turns_err,
            ThreadStoreError::Unsupported {
                operation: "list_turns"
            }
        ));

        let items_err = store
            .list_items(ListItemsParams {
                thread_id,
                turn_id: None,
                include_archived: true,
                cursor: None,
                page_size: 10,
                sort_direction: SortDirection::Asc,
            })
            .await
            .expect_err("default list_items should be unsupported");
        assert!(matches!(
            items_err,
            ThreadStoreError::Unsupported {
                operation: "list_items"
            }
        ));
    }

    #[tokio::test]
    async fn live_terminal_lookup_uses_append_index_without_loading_history() {
        let store = Arc::new(InMemoryThreadStore::default());
        let thread_id = ThreadId::default();
        let live_thread = LiveThread::create(
            store.clone(),
            create_thread_params(thread_id, ThreadHistoryMode::Legacy),
        )
        .await
        .expect("create live thread");
        let terminal = RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
            surfaced_result: None,
            turn_id: "turn-1".to_string(),
            last_agent_message: None,
            error: None,
            completed_at: None,
            duration_ms: None,
            time_to_first_token_ms: None,
            timing: None,
        }));

        live_thread
            .append_items(std::slice::from_ref(&terminal))
            .await
            .expect("append terminal event");
        let found = live_thread
            .terminal_event("turn-1", /*include_archived*/ true)
            .await
            .expect("lookup terminal event")
            .expect("terminal event should be indexed");
        let missing = live_thread
            .terminal_event("turn-2", /*include_archived*/ true)
            .await
            .expect("lookup absent terminal event");

        assert!(matches!(
            found,
            EventMsg::TurnComplete(TurnCompleteEvent { turn_id, .. }) if turn_id == "turn-1"
        ));
        assert!(missing.is_none());
        assert_eq!(store.calls().await.load_history, 0);
    }

    #[tokio::test]
    async fn live_thread_rejects_transient_items_before_calling_the_store() {
        let store = Arc::new(InMemoryThreadStore::default());
        let thread_id = ThreadId::default();
        let live_thread = LiveThread::create(
            store.clone(),
            create_thread_params(thread_id, ThreadHistoryMode::Legacy),
        )
        .await
        .expect("create live thread");

        live_thread
            .append_items(&[RolloutItem::EventMsg(EventMsg::ShutdownComplete)])
            .await
            .expect("ignore transient event");

        assert_eq!(store.calls().await.append_items_requests, 0);
    }

    /// N16 regression: a durable append must reach the store through the borrowed
    /// already-filtered API. The owned `AppendThreadItemsParams` path copies every raw item
    /// before the store can apply the persistence policy, so taking it means the batch was
    /// deep-cloned an extra time on the way to disk.
    #[tokio::test]
    async fn durable_items_reach_the_store_without_an_extra_owned_copy() {
        let store = Arc::new(InMemoryThreadStore::default());
        let thread_id = ThreadId::default();
        let live_thread = LiveThread::create(
            store.clone(),
            create_thread_params(thread_id, ThreadHistoryMode::Legacy),
        )
        .await
        .expect("create live thread");

        let durable = RolloutItem::ResponseItem(ResponseItem::FunctionCallOutput {
            id: None,
            call_id: "call-1".to_string(),
            output: FunctionCallOutputPayload {
                body: FunctionCallOutputBody::Text("x".repeat(4096)),
                success: Some(true),
            },
            internal_chat_message_metadata_passthrough: None,
        });

        live_thread
            .append_items_ordered(std::slice::from_ref(&durable))
            .await
            .expect("append durable item");

        let calls = store.calls().await;
        assert_eq!(
            calls.append_borrowed_item_batches, 1,
            "durable appends must use the borrowed already-filtered store API"
        );
        assert_eq!(
            calls.append_owned_item_batches, 0,
            "durable appends must not copy the raw batch into an owned request"
        );
        assert_eq!(
            calls.owned_items_copied, 0,
            "no rollout item may be copied into an owned request batch"
        );
        assert_eq!(calls.append_items_ordered, 1);
    }

    #[tokio::test]
    async fn list_threads_filters_by_spawn_relationship() {
        let store = InMemoryThreadStore::default();
        let parent_thread_id = ThreadId::default();
        let child_thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000001").expect("valid thread id");
        let unrelated_thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000002").expect("valid thread id");
        let grandchild_thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000003").expect("valid thread id");

        for (thread_id, parent_thread_id) in [
            (child_thread_id, Some(parent_thread_id)),
            (unrelated_thread_id, None),
            (grandchild_thread_id, Some(child_thread_id)),
        ] {
            store
                .create_thread(CreateThreadParams {
                    session_id: thread_id.into(),
                    thread_id,
                    extra_config: None,
                    forked_from_id: None,
                    parent_thread_id,
                    source: SessionSource::Exec,
                    thread_source: None,
                    originator: "test_originator".to_string(),
                    base_instructions: BaseInstructions::default(),
                    dynamic_tools: Vec::new(),
                    selected_capability_roots: Vec::new(),
                    multi_agent_version: None,
                    history_mode: ThreadHistoryMode::Legacy,
                    initial_window_id: uuid::Uuid::now_v7().to_string(),
                    metadata: ThreadPersistenceMetadata {
                        cwd: None,
                        model_provider: "test-provider".to_string(),
                        memory_mode: ThreadMemoryMode::Enabled,
                    },
                })
                .await
                .expect("create thread");
        }

        let page = ThreadStore::list_threads(
            &store,
            ListThreadsParams {
                project_id: None,
                page_size: 10,
                cursor: None,
                sort_key: ThreadSortKey::CreatedAt,
                sort_direction: SortDirection::Desc,
                allowed_sources: Vec::new(),
                model_providers: None,
                cwd_filters: None,
                archived: false,
                search_term: None,
                relation_filter: Some(ThreadRelationFilter::DirectChildrenOf(parent_thread_id)),
                storage_mode: crate::ThreadListStorageMode::PreferStateDb,
            },
        )
        .await
        .expect("list child threads");

        assert_eq!(
            page.items
                .into_iter()
                .map(|item| item.thread_id)
                .collect::<Vec<_>>(),
            vec![child_thread_id]
        );

        let page = ThreadStore::list_threads(
            &store,
            ListThreadsParams {
                project_id: None,
                page_size: 10,
                cursor: None,
                sort_key: ThreadSortKey::CreatedAt,
                sort_direction: SortDirection::Desc,
                allowed_sources: Vec::new(),
                model_providers: None,
                cwd_filters: None,
                archived: false,
                search_term: None,
                relation_filter: Some(ThreadRelationFilter::DescendantsOf(parent_thread_id)),
                storage_mode: crate::ThreadListStorageMode::PreferStateDb,
            },
        )
        .await
        .expect("list descendant threads");

        assert_eq!(
            page.items
                .into_iter()
                .map(|item| item.thread_id)
                .collect::<HashSet<_>>(),
            HashSet::from([child_thread_id, grandchild_thread_id])
        );
    }

    #[tokio::test]
    async fn paginated_threads_allow_metadata_reads_and_reject_legacy_history_paths() {
        let store = InMemoryThreadStore::default();
        let thread_id = ThreadId::default();
        let rollout_path = PathBuf::from("/tmp/paginated-thread.jsonl");

        store
            .create_thread(create_thread_params(thread_id, ThreadHistoryMode::Legacy))
            .await
            .expect("create legacy thread");
        store
            .resume_thread(ResumeThreadParams {
                thread_id,
                rollout_path: Some(rollout_path.clone()),
                history: None,
                include_archived: false,
                metadata: thread_metadata(),
            })
            .await
            .expect("register rollout path");
        {
            let mut state = store.state.lock().await;
            state
                .created_threads
                .get_mut(&thread_id)
                .expect("created thread")
                .history_mode = ThreadHistoryMode::Paginated;
            let Some(RolloutItem::SessionMeta(meta_line)) = state
                .histories
                .get_mut(&thread_id)
                .and_then(|history| history.first_mut())
            else {
                panic!("canonical session meta");
            };
            meta_line.meta.history_mode = ThreadHistoryMode::Paginated;
        }

        let thread = store
            .read_thread(ReadThreadParams {
                thread_id,
                include_archived: false,
                include_history: false,
            })
            .await
            .expect("metadata read");
        assert_eq!(thread.history_mode, ThreadHistoryMode::Paginated);
        assert!(thread.history.is_none());

        let thread = store
            .read_thread_by_rollout_path(ReadThreadByRolloutPathParams {
                rollout_path,
                include_archived: false,
                include_history: false,
            })
            .await
            .expect("metadata path read");
        assert_eq!(thread.history_mode, ThreadHistoryMode::Paginated);
        assert!(thread.history.is_none());

        assert_paginated_threads_unsupported(
            store
                .read_thread(ReadThreadParams {
                    thread_id,
                    include_archived: false,
                    include_history: true,
                })
                .await
                .expect_err("full history read should fail"),
        );
        assert_paginated_threads_unsupported(
            store
                .read_thread_by_rollout_path(ReadThreadByRolloutPathParams {
                    rollout_path: PathBuf::from("/tmp/paginated-thread.jsonl"),
                    include_archived: false,
                    include_history: true,
                })
                .await
                .expect_err("full history path read should fail"),
        );
        assert_paginated_threads_unsupported(
            store
                .load_history(LoadThreadHistoryParams {
                    thread_id,
                    include_archived: false,
                })
                .await
                .expect_err("history load should fail"),
        );
        assert_paginated_threads_unsupported(
            store
                .resume_thread(ResumeThreadParams {
                    thread_id,
                    rollout_path: None,
                    history: None,
                    include_archived: false,
                    metadata: thread_metadata(),
                })
                .await
                .expect_err("resume should fail"),
        );
        assert_paginated_threads_unsupported(
            store
                .create_thread(create_thread_params(
                    ThreadId::default(),
                    ThreadHistoryMode::Paginated,
                ))
                .await
                .expect_err("paginated create should fail"),
        );
    }

    #[tokio::test]
    async fn rejected_lifecycle_operations_leave_no_thread_data() {
        let store = InMemoryThreadStore::default();
        let missing = ThreadId::new();
        assert!(matches!(
            store
                .update_thread_metadata(UpdateThreadMetadataParams {
                    thread_id: missing,
                    patch: ThreadMetadataPatch {
                        name: Some(Some("orphan".to_string())),
                        ..Default::default()
                    },
                    include_archived: true,
                })
                .await,
            Err(ThreadStoreError::ThreadNotFound { .. })
        ));
        assert!(matches!(
            store
                .resume_thread(ResumeThreadParams {
                    thread_id: missing,
                    rollout_path: Some(PathBuf::from("orphan.jsonl")),
                    history: Some(Arc::new(Vec::new())),
                    include_archived: true,
                    metadata: thread_metadata(),
                })
                .await,
            Err(ThreadStoreError::ThreadNotFound { .. })
        ));
        {
            let state = store.state.lock().await;
            assert!(state.histories.is_empty());
            assert!(state.metadata_updates.is_empty());
            assert!(state.names.is_empty());
            assert!(state.rollout_paths.is_empty());
        }
        store
            .create_thread(create_thread_params(missing, ThreadHistoryMode::Legacy))
            .await
            .expect("create");
        assert!(matches!(
            store
                .create_thread(create_thread_params(missing, ThreadHistoryMode::Legacy))
                .await,
            Err(ThreadStoreError::Conflict { .. })
        ));
        let read = ReadThreadParams {
            thread_id: missing,
            include_archived: false,
            include_history: true,
        };
        let first = store.read_thread(read.clone()).await.expect("first read");
        let second = store.read_thread(read).await.expect("second read");
        assert_eq!(first.history.expect("history").items.len(), 1);
        assert_eq!(first.created_at, second.created_at);
        assert_eq!(first.updated_at, second.updated_at);
        assert_eq!(first.recency_at, second.recency_at);
        assert_eq!(first.model_provider, thread_metadata().model_provider);
        assert_eq!(first.cwd, thread_metadata().cwd.unwrap_or_default());
    }

    #[tokio::test]
    async fn cancelled_resume_discards_the_open_writer() {
        let store = Arc::new(InMemoryThreadStore::default());
        let thread_id = ThreadId::new();
        store
            .create_thread(create_thread_params(thread_id, ThreadHistoryMode::Legacy))
            .await
            .expect("create");
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        store.state.lock().await.history_load_gate = Some((entered.clone(), release));
        let task = tokio::spawn(LiveThread::resume(
            store.clone(),
            ThreadHistoryMode::Legacy,
            ResumeThreadParams {
                thread_id,
                rollout_path: None,
                history: None,
                include_archived: false,
                metadata: thread_metadata(),
            },
        ));
        tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified())
            .await
            .expect("history load started");
        task.abort();
        let Err(error) = task.await else {
            panic!("resume should have been cancelled");
        };
        assert!(error.is_cancelled());
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while store.calls().await.discard_thread == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("guard cleanup");
        assert_eq!(store.calls().await.discard_thread, 1);
    }

    #[tokio::test]
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "Blocks discard deliberately to verify cancellation retains cleanup ownership"
    )]
    async fn cancelled_explicit_discard_keeps_initialization_guard_armed() {
        let store = Arc::new(InMemoryThreadStore::default());
        let live = LiveThread::create(
            store.clone(),
            create_thread_params(ThreadId::new(), ThreadHistoryMode::Legacy),
        )
        .await
        .expect("create live thread");
        let mut guard = crate::LiveThreadInitGuard::new(Some(live));
        let state = store.state.lock().await;
        {
            let discard = guard.discard();
            tokio::pin!(discard);
            assert!(matches!(
                std::future::poll_fn(|cx| std::task::Poll::Ready(std::future::Future::poll(
                    discard.as_mut(),
                    cx
                )))
                .await,
                std::task::Poll::Pending
            ));
        }
        assert!(
            guard.as_ref().is_some(),
            "cancelled cleanup must retain ownership"
        );
        drop(state);
        guard.discard().await;
        assert!(guard.as_ref().is_none());
        assert_eq!(store.calls().await.discard_thread, 1);
    }

    #[tokio::test]
    async fn later_user_messages_defer_touch_only_metadata_until_flush() {
        let store = Arc::new(InMemoryThreadStore::default());
        let live = LiveThread::create(
            store.clone(),
            create_thread_params(ThreadId::new(), ThreadHistoryMode::Legacy),
        )
        .await
        .expect("create live thread");
        let message = |text: &str| {
            RolloutItem::EventMsg(EventMsg::UserMessage(
                codex_protocol::protocol::UserMessageEvent {
                    message: text.to_string(),
                    images: None,
                    local_images: Vec::new(),
                    text_elements: Vec::new(),
                    ..Default::default()
                },
            ))
        };
        live.append_items(&[message("first")])
            .await
            .expect("first append");
        let writes = store.calls().await.update_thread_metadata;
        live.append_items(&[message("second")])
            .await
            .expect("second append");
        assert_eq!(store.calls().await.update_thread_metadata, writes);
        live.flush().await.expect("flush touch");
        assert_eq!(store.calls().await.update_thread_metadata, writes + 1);
        assert_eq!(
            live.read_thread(false, false)
                .await
                .expect("summary")
                .preview,
            "first"
        );
    }

    #[tokio::test]
    async fn listing_honors_filters_and_rejects_unsupported_operations() {
        let store = InMemoryThreadStore::default();
        let first = ThreadId::new();
        let second = ThreadId::new();
        for (id, provider, seconds) in [(first, "one", 100), (second, "two", 200)] {
            let mut create = create_thread_params(id, ThreadHistoryMode::Legacy);
            create.metadata.model_provider = provider.to_string();
            create.metadata.cwd = Some(PathBuf::from(provider));
            store.create_thread(create).await.expect("create");
            store
                .update_thread_metadata(UpdateThreadMetadataParams {
                    thread_id: id,
                    patch: ThreadMetadataPatch {
                        updated_at: chrono::DateTime::from_timestamp(seconds, 0),
                        preview: Some(provider.to_string()),
                        title: Some(format!("title-{provider}")),
                        ..Default::default()
                    },
                    include_archived: false,
                })
                .await
                .expect("metadata");
        }
        let base = ListThreadsParams {
            project_id: None,
            page_size: 10,
            cursor: None,
            sort_key: ThreadSortKey::UpdatedAt,
            sort_direction: SortDirection::Desc,
            allowed_sources: Vec::new(),
            model_providers: None,
            cwd_filters: None,
            archived: false,
            search_term: None,
            relation_filter: None,
            storage_mode: crate::ThreadListStorageMode::PreferStateDb,
        };
        let list = |params| ThreadStore::list_threads(&store, params);
        assert_eq!(
            list(base.clone())
                .await
                .expect("sorted")
                .items
                .into_iter()
                .map(|t| t.thread_id)
                .collect::<Vec<_>>(),
            vec![second, first]
        );
        for filter in [
            ListThreadsParams {
                model_providers: Some(vec!["one".to_string()]),
                ..base.clone()
            },
            ListThreadsParams {
                cwd_filters: Some(vec![PathBuf::from("one")]),
                ..base.clone()
            },
            ListThreadsParams {
                search_term: Some("one".to_string()),
                ..base.clone()
            },
            ListThreadsParams {
                search_term: Some("title-one".to_string()),
                ..base.clone()
            },
        ] {
            assert_eq!(
                list(filter)
                    .await
                    .expect("filtered")
                    .items
                    .into_iter()
                    .map(|t| t.thread_id)
                    .collect::<Vec<_>>(),
                vec![first]
            );
        }
        assert!(
            list(ListThreadsParams {
                archived: true,
                ..base.clone()
            })
            .await
            .expect("archived")
            .items
            .is_empty()
        );
        assert!(
            list(ListThreadsParams {
                cwd_filters: Some(Vec::new()),
                ..base.clone()
            })
            .await
            .expect("empty cwd filter")
            .items
            .is_empty()
        );
        for params in [
            ListThreadsParams {
                page_size: 1,
                ..base.clone()
            },
            ListThreadsParams {
                cursor: Some("cursor".to_string()),
                ..base
            },
        ] {
            assert!(matches!(
                list(params).await,
                Err(ThreadStoreError::Unsupported {
                    operation: "in_memory_thread_list_pagination"
                })
            ));
        }
        assert!(matches!(
            store
                .archive_thread(ArchiveThreadParams { thread_id: first })
                .await,
            Err(ThreadStoreError::Unsupported {
                operation: "in_memory_archive_thread"
            })
        ));
    }

    fn create_thread_params(
        thread_id: ThreadId,
        history_mode: ThreadHistoryMode,
    ) -> CreateThreadParams {
        CreateThreadParams {
            session_id: thread_id.into(),
            thread_id,
            extra_config: None,
            forked_from_id: None,
            parent_thread_id: None,
            source: SessionSource::Exec,
            thread_source: None,
            originator: "test_originator".to_string(),
            base_instructions: BaseInstructions::default(),
            dynamic_tools: Vec::new(),
            selected_capability_roots: Vec::new(),
            multi_agent_version: None,
            history_mode,
            initial_window_id: uuid::Uuid::now_v7().to_string(),
            metadata: thread_metadata(),
        }
    }

    fn thread_metadata() -> ThreadPersistenceMetadata {
        ThreadPersistenceMetadata {
            cwd: None,
            model_provider: "test-provider".to_string(),
            memory_mode: ThreadMemoryMode::Enabled,
        }
    }

    fn assert_paginated_threads_unsupported(err: ThreadStoreError) {
        assert!(matches!(
            err,
            ThreadStoreError::Unsupported {
                operation: "paginated_threads"
            }
        ));
    }
}

fn stores_guard() -> MutexGuard<'static, HashMap<String, Arc<InMemoryThreadStore>>> {
    match stores().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Recorded call counts for [`InMemoryThreadStore`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InMemoryThreadStoreCalls {
    pub create_thread: usize,
    pub resume_thread: usize,
    pub append_items_requests: usize,
    pub append_items: usize,
    pub append_items_ordered: usize,
    /// Appends that arrived through the owned [`AppendThreadItemsParams`] API, which copies
    /// every raw item before the store can apply the persistence policy.
    pub append_owned_item_batches: usize,
    /// Appends that arrived through the borrowed already-filtered API.
    pub append_borrowed_item_batches: usize,
    /// Rollout items the store had to copy out of an owned request batch.
    pub owned_items_copied: usize,
    pub persist_thread: usize,
    pub flush_thread: usize,
    pub shutdown_thread: usize,
    pub discard_thread: usize,
    pub load_history: usize,
    pub read_thread: usize,
    pub read_thread_with_history: usize,
    pub read_thread_by_rollout_path: usize,
    pub list_threads: usize,
    pub update_thread_metadata: usize,
    pub archive_thread: usize,
    pub unarchive_thread: usize,
    pub delete_thread: usize,
}

/// In-memory [`ThreadStore`] implementation for tests and debug configs.
///
/// Test and debug configs can select this store by id, letting tests exercise
/// config-driven non-local persistence without requiring the real remote gRPC
/// service.
#[derive(Default)]
pub struct InMemoryThreadStore {
    state: tokio::sync::Mutex<InMemoryThreadStoreState>,
}

#[derive(Default)]
struct InMemoryThreadStoreState {
    calls: InMemoryThreadStoreCalls,
    created_threads: HashMap<ThreadId, CreateThreadParams>,
    histories: HashMap<ThreadId, Vec<RolloutItem>>,
    metadata_updates: HashMap<ThreadId, ThreadMetadataPatch>,
    names: HashMap<ThreadId, Option<String>>,
    rollout_paths: HashMap<PathBuf, ThreadId>,
    #[cfg(test)]
    fail_next_shutdown: bool,
    #[cfg(test)]
    history_load_gate: Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>,
}

impl InMemoryThreadStore {
    /// Returns the store associated with `id`, creating it if needed.
    pub fn for_id(id: impl Into<String>) -> Arc<Self> {
        let id = id.into();
        let mut stores = stores_guard();
        stores
            .entry(id)
            .or_insert_with(|| Arc::new(Self::default()))
            .clone()
    }

    /// Removes a shared in-memory store for `id`.
    pub fn remove_id(id: &str) -> Option<Arc<Self>> {
        stores_guard().remove(id)
    }

    /// Returns the calls observed by this store.
    pub async fn calls(&self) -> InMemoryThreadStoreCalls {
        self.state.lock().await.calls.clone()
    }

    #[cfg(test)]
    pub async fn fail_next_shutdown(&self) {
        self.state.lock().await.fail_next_shutdown = true;
    }

    async fn create_thread(&self, params: CreateThreadParams) -> ThreadStoreResult<()> {
        reject_paginated_history_mode(params.history_mode)?;
        let mut state = self.state.lock().await;
        state.calls.create_thread += 1;
        if state.created_threads.contains_key(&params.thread_id) {
            return Err(ThreadStoreError::Conflict {
                message: format!("thread {} already exists", params.thread_id),
            });
        }
        let created_at = Utc::now();
        let session_meta = SessionMeta {
            session_id: params.session_id,
            id: params.thread_id,
            timestamp: created_at.to_rfc3339(),
            forked_from_id: params.forked_from_id,
            parent_thread_id: params.parent_thread_id,
            cwd: params.metadata.cwd.clone().unwrap_or_default(),
            agent_nickname: params.source.get_nickname(),
            agent_role: params.source.get_agent_role(),
            agent_path: params.source.get_agent_path().map(Into::into),
            originator: params.originator.clone(),
            source: params.source.clone(),
            thread_source: params.thread_source.clone(),
            model_provider: Some(params.metadata.model_provider.clone()),
            base_instructions: Some(params.base_instructions.clone()),
            dynamic_tools: (!params.dynamic_tools.is_empty()).then(|| params.dynamic_tools.clone()),
            selected_capability_roots: params.selected_capability_roots.clone(),
            memory_mode: matches!(params.metadata.memory_mode, ThreadMemoryMode::Disabled)
                .then_some("disabled".to_string()),
            history_mode: params.history_mode,
            multi_agent_version: params.multi_agent_version,
            context_window: Some(SessionContextWindow::new(params.initial_window_id.clone())),
            ..SessionMeta::default()
        };
        state
            .histories
            .entry(params.thread_id)
            .or_default()
            .push(RolloutItem::SessionMeta(SessionMetaLine {
                meta: session_meta,
                git: None,
            }));
        state.metadata_updates.insert(
            params.thread_id,
            ThreadMetadataPatch {
                created_at: Some(created_at),
                updated_at: Some(created_at),
                advance_recency_at: Some(created_at),
                ..Default::default()
            },
        );
        state.created_threads.insert(params.thread_id, params);
        Ok(())
    }

    async fn resume_thread(&self, params: ResumeThreadParams) -> ThreadStoreResult<()> {
        let mut state = self.state.lock().await;
        state.calls.resume_thread += 1;
        require_thread(&state, params.thread_id)?;
        let history_mode = params
            .history
            .as_deref()
            .map(Vec::as_slice)
            .map(canonical_history_mode_from_rollout_items)
            .unwrap_or_else(|| history_mode_from_state(&state, params.thread_id));
        reject_paginated_history_mode(history_mode)?;
        if let Some(history) = params.history {
            state
                .histories
                .insert(params.thread_id, Arc::unwrap_or_clone(history));
        } else {
            state.histories.entry(params.thread_id).or_default();
        }
        if let Some(rollout_path) = params.rollout_path {
            state
                .metadata_updates
                .entry(params.thread_id)
                .or_default()
                .rollout_path = Some(rollout_path.clone());
            state.rollout_paths.insert(rollout_path, params.thread_id);
        }
        Ok(())
    }

    async fn append_items_with_ordering(
        &self,
        params: AppendThreadItemsParams,
        ordered: bool,
    ) -> ThreadStoreResult<()> {
        if params.items.is_empty() {
            return Ok(());
        }
        let mut state = self.state.lock().await;
        state.calls.append_items_requests += 1;
        state.calls.append_owned_item_batches += 1;
        state.calls.owned_items_copied += params.items.len();
        require_thread(&state, params.thread_id)?;
        let history_mode = history_mode_from_state(&state, params.thread_id);
        let persisted_items = persisted_rollout_items(params.items.as_slice(), history_mode);
        if persisted_items.is_empty() {
            return Ok(());
        }
        state.calls.append_items += 1;
        if ordered {
            state.calls.append_items_ordered += 1;
        }
        state
            .histories
            .entry(params.thread_id)
            .or_default()
            .extend(persisted_items);
        Ok(())
    }

    async fn append_persisted_items_with_ordering(
        &self,
        thread_id: ThreadId,
        items: &[RolloutItem],
        ordered: bool,
    ) -> ThreadStoreResult<()> {
        if items.is_empty() {
            return Ok(());
        }
        let mut state = self.state.lock().await;
        state.calls.append_items_requests += 1;
        state.calls.append_borrowed_item_batches += 1;
        require_thread(&state, thread_id)?;
        state.calls.append_items += 1;
        if ordered {
            state.calls.append_items_ordered += 1;
        }
        state
            .histories
            .entry(thread_id)
            .or_default()
            .extend_from_slice(items);
        Ok(())
    }

    async fn load_history(
        &self,
        params: LoadThreadHistoryParams,
    ) -> ThreadStoreResult<StoredThreadHistory> {
        #[cfg(test)]
        {
            let gate = {
                let mut state = self.state.lock().await;
                state.calls.load_history += 1;
                state.history_load_gate.take()
            };
            if let Some((entered, release)) = gate {
                entered.notify_one();
                release.notified().await;
            }
        }
        let state = self.state.lock().await;
        #[cfg(not(test))]
        let state = {
            let mut state = state;
            state.calls.load_history += 1;
            state
        };
        let items =
            state
                .histories
                .get(&params.thread_id)
                .ok_or(ThreadStoreError::ThreadNotFound {
                    thread_id: params.thread_id,
                })?;
        let history_mode = history_mode_from_state(&state, params.thread_id);
        reject_paginated_history_mode(history_mode)?;
        Ok(StoredThreadHistory {
            thread_id: params.thread_id,
            items: items.clone(),
        })
    }

    async fn read_thread(&self, params: ReadThreadParams) -> ThreadStoreResult<StoredThread> {
        let mut state = self.state.lock().await;
        state.calls.read_thread += 1;
        if params.include_history {
            state.calls.read_thread_with_history += 1;
            reject_paginated_history_mode(history_mode_from_state(&state, params.thread_id))?;
        }
        let thread = stored_thread_from_state(&state, params.thread_id, params.include_history)?;
        Ok(thread)
    }

    async fn read_thread_by_rollout_path(
        &self,
        params: ReadThreadByRolloutPathParams,
    ) -> ThreadStoreResult<StoredThread> {
        let mut state = self.state.lock().await;
        state.calls.read_thread_by_rollout_path += 1;
        let Some(thread_id) = state.rollout_paths.get(&params.rollout_path).copied() else {
            return Err(ThreadStoreError::InvalidRequest {
                message: format!(
                    "in-memory thread store does not know rollout path {}",
                    params.rollout_path.display()
                ),
            });
        };
        if params.include_history {
            reject_paginated_history_mode(history_mode_from_state(&state, thread_id))?;
        }
        let thread = stored_thread_from_state(&state, thread_id, params.include_history)?;
        Ok(thread)
    }

    async fn list_threads(&self, params: ListThreadsParams) -> ThreadStoreResult<ThreadPage> {
        if params.project_id.is_some() {
            return Err(ThreadStoreError::Unsupported {
                operation: "projects",
            });
        }
        if params.cursor.is_some() {
            return Err(ThreadStoreError::Unsupported {
                operation: "in_memory_thread_list_pagination",
            });
        }
        if params.storage_mode != crate::ThreadListStorageMode::PreferStateDb {
            return Err(ThreadStoreError::Unsupported {
                operation: "in_memory_thread_list_storage_mode",
            });
        }
        let mut state = self.state.lock().await;
        state.calls.list_threads += 1;
        let items = state
            .created_threads
            .keys()
            .map(|thread_id| stored_thread_from_state(&state, *thread_id, false))
            .collect::<ThreadStoreResult<Vec<_>>>()?;
        let mut page = ThreadPage {
            items,
            next_cursor: None,
            backwards_cursor: None,
        };
        match params.relation_filter {
            Some(ThreadRelationFilter::DirectChildrenOf(parent_thread_id)) => {
                page.items
                    .retain(|thread| thread.parent_thread_id == Some(parent_thread_id));
            }
            Some(ThreadRelationFilter::DescendantsOf(ancestor_thread_id)) => {
                let mut subtree = HashSet::from([ancestor_thread_id]);
                let mut children: HashMap<ThreadId, Vec<ThreadId>> = HashMap::new();
                for thread in &page.items {
                    if let Some(parent) = thread.parent_thread_id {
                        children.entry(parent).or_default().push(thread.thread_id);
                    }
                }
                let mut pending = vec![ancestor_thread_id];
                while let Some(parent) = pending.pop() {
                    for child in children.get(&parent).into_iter().flatten() {
                        if subtree.insert(*child) {
                            pending.push(*child);
                        }
                    }
                }
                page.items.retain(|thread| {
                    thread.thread_id != ancestor_thread_id && subtree.contains(&thread.thread_id)
                });
            }
            None => {}
        }
        page.items.retain(|thread| {
            !params.archived
                && (params.allowed_sources.is_empty()
                    || params.allowed_sources.contains(&thread.source))
                && params.model_providers.as_ref().is_none_or(|providers| {
                    providers.is_empty() || providers.contains(&thread.model_provider)
                })
                && params
                    .cwd_filters
                    .as_ref()
                    .is_none_or(|cwds| cwds.contains(&thread.cwd))
                && params.search_term.as_ref().is_none_or(|term| {
                    thread.preview.contains(term)
                        || state
                            .metadata_updates
                            .get(&thread.thread_id)
                            .and_then(|metadata| metadata.title.as_ref())
                            .is_some_and(|title| title.contains(term))
                        || thread.name.as_ref().is_some_and(|name| name.contains(term))
                })
        });
        drop(state);
        page.items.sort_by(|left, right| {
            let timestamp = |thread: &StoredThread| match params.sort_key {
                crate::ThreadSortKey::CreatedAt => thread.created_at,
                crate::ThreadSortKey::UpdatedAt => thread.updated_at,
                crate::ThreadSortKey::RecencyAt => thread.recency_at,
            };
            let order = timestamp(left)
                .cmp(&timestamp(right))
                .then_with(|| left.thread_id.to_string().cmp(&right.thread_id.to_string()));
            match params.sort_direction {
                crate::SortDirection::Asc => order,
                crate::SortDirection::Desc => order.reverse(),
            }
        });
        if page.items.len() > params.page_size {
            return Err(ThreadStoreError::Unsupported {
                operation: "in_memory_thread_list_pagination",
            });
        }
        Ok(page)
    }

    async fn update_thread_metadata(
        &self,
        params: UpdateThreadMetadataParams,
    ) -> ThreadStoreResult<StoredThread> {
        if params.patch.project_id.is_some() {
            return Err(ThreadStoreError::Unsupported {
                operation: "projects",
            });
        }
        let mut state = self.state.lock().await;
        state.calls.update_thread_metadata += 1;
        require_thread(&state, params.thread_id)?;
        if let Some(name) = params.patch.name.clone() {
            state.names.insert(params.thread_id, name);
        }
        state
            .metadata_updates
            .entry(params.thread_id)
            .or_default()
            .merge(params.patch);
        stored_thread_from_state(&state, params.thread_id, /*include_history*/ false)
    }

    async fn delete_thread(&self, params: DeleteThreadParams) -> ThreadStoreResult<()> {
        let mut state = self.state.lock().await;
        state.calls.delete_thread += 1;
        let existed = state.histories.remove(&params.thread_id).is_some();
        state.created_threads.remove(&params.thread_id);
        state.names.remove(&params.thread_id);
        state.metadata_updates.remove(&params.thread_id);
        state
            .rollout_paths
            .retain(|_, thread_id| *thread_id != params.thread_id);
        if existed {
            Ok(())
        } else {
            Err(ThreadStoreError::ThreadNotFound {
                thread_id: params.thread_id,
            })
        }
    }
}

impl ThreadStore for InMemoryThreadStore {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn create_thread(&self, params: CreateThreadParams) -> ThreadStoreFuture<'_, ()> {
        Box::pin(InMemoryThreadStore::create_thread(self, params))
    }

    fn resume_thread(&self, params: ResumeThreadParams) -> ThreadStoreFuture<'_, ()> {
        Box::pin(InMemoryThreadStore::resume_thread(self, params))
    }

    fn append_items(&self, params: AppendThreadItemsParams) -> ThreadStoreFuture<'_, ()> {
        Box::pin(InMemoryThreadStore::append_items_with_ordering(
            self, params, false,
        ))
    }

    fn append_items_ordered(&self, params: AppendThreadItemsParams) -> ThreadStoreFuture<'_, ()> {
        Box::pin(InMemoryThreadStore::append_items_with_ordering(
            self, params, true,
        ))
    }

    fn append_persisted_items<'a>(
        &'a self,
        thread_id: ThreadId,
        items: &'a [RolloutItem],
    ) -> ThreadStoreFuture<'a, ()> {
        Box::pin(InMemoryThreadStore::append_persisted_items_with_ordering(
            self, thread_id, items, false,
        ))
    }

    fn append_persisted_items_ordered<'a>(
        &'a self,
        thread_id: ThreadId,
        items: &'a [RolloutItem],
    ) -> ThreadStoreFuture<'a, ()> {
        Box::pin(InMemoryThreadStore::append_persisted_items_with_ordering(
            self, thread_id, items, true,
        ))
    }

    fn persist_thread(&self, _thread_id: ThreadId) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            self.state.lock().await.calls.persist_thread += 1;
            Ok(())
        })
    }

    fn flush_thread(&self, _thread_id: ThreadId) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            self.state.lock().await.calls.flush_thread += 1;
            Ok(())
        })
    }

    fn shutdown_thread(&self, _thread_id: ThreadId) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            let mut state = self.state.lock().await;
            state.calls.shutdown_thread += 1;
            #[cfg(test)]
            if std::mem::take(&mut state.fail_next_shutdown) {
                return Err(ThreadStoreError::Internal {
                    message: "injected shutdown failure".to_string(),
                });
            }
            Ok(())
        })
    }

    fn discard_thread(&self, _thread_id: ThreadId) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            self.state.lock().await.calls.discard_thread += 1;
            Ok(())
        })
    }

    fn load_history(
        &self,
        params: LoadThreadHistoryParams,
    ) -> ThreadStoreFuture<'_, StoredThreadHistory> {
        Box::pin(InMemoryThreadStore::load_history(self, params))
    }

    fn read_thread(&self, params: ReadThreadParams) -> ThreadStoreFuture<'_, StoredThread> {
        Box::pin(InMemoryThreadStore::read_thread(self, params))
    }

    fn read_thread_by_rollout_path(
        &self,
        params: ReadThreadByRolloutPathParams,
    ) -> ThreadStoreFuture<'_, StoredThread> {
        Box::pin(InMemoryThreadStore::read_thread_by_rollout_path(
            self, params,
        ))
    }

    fn list_threads(&self, params: ListThreadsParams) -> ThreadStoreFuture<'_, ThreadPage> {
        Box::pin(InMemoryThreadStore::list_threads(self, params))
    }

    fn update_thread_metadata(
        &self,
        params: UpdateThreadMetadataParams,
    ) -> ThreadStoreFuture<'_, StoredThread> {
        Box::pin(InMemoryThreadStore::update_thread_metadata(self, params))
    }

    fn archive_thread(&self, _params: ArchiveThreadParams) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            self.state.lock().await.calls.archive_thread += 1;
            Err(ThreadStoreError::Unsupported {
                operation: "in_memory_archive_thread",
            })
        })
    }

    fn unarchive_thread(&self, params: ArchiveThreadParams) -> ThreadStoreFuture<'_, StoredThread> {
        Box::pin(async move {
            let mut state = self.state.lock().await;
            state.calls.unarchive_thread += 1;
            // In-memory threads are always active, so unarchive is idempotent.
            stored_thread_from_state(&state, params.thread_id, false)
        })
    }

    fn delete_thread(&self, params: DeleteThreadParams) -> ThreadStoreFuture<'_, ()> {
        Box::pin(InMemoryThreadStore::delete_thread(self, params))
    }
}

fn require_thread(state: &InMemoryThreadStoreState, thread_id: ThreadId) -> ThreadStoreResult<()> {
    if state.created_threads.contains_key(&thread_id) {
        Ok(())
    } else {
        Err(ThreadStoreError::ThreadNotFound { thread_id })
    }
}

fn stored_thread_from_state(
    state: &InMemoryThreadStoreState,
    thread_id: ThreadId,
    include_history: bool,
) -> ThreadStoreResult<StoredThread> {
    let created = state
        .created_threads
        .get(&thread_id)
        .ok_or(ThreadStoreError::ThreadNotFound { thread_id })?;
    let history = include_history.then(|| StoredThreadHistory {
        thread_id,
        items: state.histories.get(&thread_id).cloned().unwrap_or_default(),
    });
    let name = state.names.get(&thread_id).cloned().flatten();
    let metadata = state.metadata_updates.get(&thread_id);

    let missing_timestamp = || ThreadStoreError::Internal {
        message: format!("thread {thread_id} is missing its creation timestamps"),
    };
    Ok(StoredThread {
        project_id: None,
        thread_id,
        extra_config: created.extra_config.clone(),
        rollout_path: metadata.and_then(|metadata| metadata.rollout_path.clone()),
        forked_from_id: created.forked_from_id,
        parent_thread_id: created.parent_thread_id,
        preview: metadata
            .and_then(|metadata| metadata.preview.clone())
            .unwrap_or_default(),
        name,
        model_provider: metadata
            .and_then(|metadata| metadata.model_provider.clone())
            .unwrap_or_else(|| created.metadata.model_provider.clone()),
        model: metadata.and_then(|metadata| metadata.model.clone()),
        reasoning_effort: metadata.and_then(|metadata| metadata.reasoning_effort.clone().flatten()),
        created_at: metadata
            .and_then(|metadata| metadata.created_at)
            .ok_or_else(missing_timestamp)?,
        updated_at: metadata
            .and_then(|metadata| metadata.updated_at)
            .ok_or_else(missing_timestamp)?,
        recency_at: metadata
            .and_then(|metadata| metadata.advance_recency_at)
            .ok_or_else(missing_timestamp)?,
        archived_at: None,
        cwd: metadata
            .and_then(|metadata| metadata.cwd.clone())
            .unwrap_or_else(|| created.metadata.cwd.clone().unwrap_or_default()),
        cli_version: metadata
            .and_then(|metadata| metadata.cli_version.clone())
            .unwrap_or_else(|| "test".to_string()),
        source: metadata
            .and_then(|metadata| metadata.source.clone())
            .unwrap_or_else(|| created.source.clone()),
        history_mode: created.history_mode,
        thread_source: metadata
            .and_then(|metadata| metadata.thread_source.clone())
            .unwrap_or_else(|| created.thread_source.clone()),
        agent_nickname: metadata.and_then(|metadata| metadata.agent_nickname.clone().flatten()),
        agent_role: metadata.and_then(|metadata| metadata.agent_role.clone().flatten()),
        agent_path: metadata.and_then(|metadata| metadata.agent_path.clone().flatten()),
        git_info: metadata.and_then(git_info_from_patch),
        approval_mode: metadata
            .and_then(|metadata| metadata.approval_mode)
            .unwrap_or(AskForApproval::Never),
        permission_profile: metadata
            .and_then(|metadata| metadata.permission_profile.clone())
            .unwrap_or_else(PermissionProfile::read_only),
        token_usage: metadata.and_then(|metadata| metadata.token_usage.clone()),
        first_user_message: metadata.and_then(|metadata| metadata.first_user_message.clone()),
        history,
    })
}

fn history_mode_from_state(
    state: &InMemoryThreadStoreState,
    thread_id: ThreadId,
) -> ThreadHistoryMode {
    state
        .created_threads
        .get(&thread_id)
        .map(|thread| thread.history_mode)
        .unwrap_or_default()
}

fn git_info_from_patch(patch: &ThreadMetadataPatch) -> Option<codex_protocol::protocol::GitInfo> {
    let git_info = patch.git_info.as_ref()?;
    let sha = git_info.sha.clone().flatten();
    let branch = git_info.branch.clone().flatten();
    let origin_url = git_info.origin_url.clone().flatten();
    if sha.is_none() && branch.is_none() && origin_url.is_none() {
        return None;
    }
    Some(codex_protocol::protocol::GitInfo {
        commit_hash: sha.as_deref().map(codex_git_utils::GitSha::new),
        branch,
        repository_url: origin_url,
    })
}
