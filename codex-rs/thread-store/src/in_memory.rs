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
        if let Some(history) = params.history
            && let Some(RolloutItem::SessionMeta(meta)) = history
                .iter()
                .find(|item| matches!(item, RolloutItem::SessionMeta(_)))
            && meta.meta.id != params.thread_id
        {
            return Err(ThreadStoreError::InvalidRequest {
                message: "resume history belongs to a different thread".to_string(),
            });
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
        let state = self.state.lock().await;
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
        session_id: Some(created.session_id),
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
