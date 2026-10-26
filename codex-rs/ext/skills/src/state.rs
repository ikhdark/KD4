use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::future::Future;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::Weak;

use codex_mcp::CODEX_APPS_MCP_SERVER_NAME;
use codex_mcp::McpResourceClient;
use codex_mcp::McpResourceServerCacheKey;
use codex_protocol::capabilities::SelectedCapabilityRoot;
use futures::StreamExt;
use tokio::sync::OnceCell;

use crate::SkillsExtensionConfig;
use crate::catalog::SkillAuthority;
use crate::catalog::SkillCatalog;
use crate::catalog::SkillPackageId;
use crate::catalog::SkillProviderError;
use crate::catalog::SkillProviderErrorKind;
use crate::catalog::SkillProviderResult;
use crate::catalog::SkillReadResult;
use crate::catalog::SkillResourceId;
use crate::catalog::SkillSourceKind;
use crate::provider::SkillListQuery;
use crate::provider::SkillReadRequest;
use crate::sources::SkillProviders;

const MAX_CACHED_ORCHESTRATOR_RESOURCES: usize = 100;
const MAX_CACHED_ORCHESTRATOR_CONTENT_BYTES: usize = 8 * 1024 * 1024;
// Each root already bounds its own file reads; cap the outer fanout too.
const MAX_CONCURRENT_EXECUTOR_ROOTS: usize = 4;
// Settle an optional discovery result before the host's five-second phase
// deadline can drop the initializer. Keep the deadline across cancellation so
// another projection cannot restart a full wait. Explicit cancellation still
// leaves the cell retryable, and no background task survives its caller.
const CATALOG_DISCOVERY_BUDGET: std::time::Duration = std::time::Duration::from_secs(4);

fn discovery_timeout() -> SkillProviderError {
    SkillProviderError::new("Skill discovery timed out; retry discovery to try again.")
        .with_kind(SkillProviderErrorKind::Timeout)
}

pub(crate) struct SkillsThreadState {
    config: Mutex<SkillsExtensionConfig>,
    orchestrator_skills_available: bool,
    executor_cache: Mutex<Vec<CachedExecutorCatalog>>,
    orchestrator_cache: Mutex<Option<Arc<OrchestratorGenerationCache>>>,
}

impl SkillsThreadState {
    pub(crate) fn new(config: SkillsExtensionConfig, orchestrator_skills_available: bool) -> Self {
        Self {
            config: Mutex::new(config),
            orchestrator_skills_available,
            executor_cache: Mutex::new(Vec::new()),
            orchestrator_cache: Mutex::new(None),
        }
    }

    pub(crate) fn config(&self) -> SkillsExtensionConfig {
        self.config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn set_config(&self, config: SkillsExtensionConfig) {
        *self
            .config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = config;
    }

    pub(crate) fn orchestrator_skills_enabled(&self) -> bool {
        self.orchestrator_skills_available && self.config().orchestrator_skills_enabled
    }

    /// Returns catalogs for stable selected roots.
    ///
    /// Healthy catalogs remain cached until this thread state is dropped.
    /// Warning-bearing results retry periodically: warnings can wrap transient I/O failures.
    /// Failures are retained for the current turn, then retried on the next real observation.
    /// Environment availability only controls whether the root is projected into the current
    /// step; it never invalidates the cache. There is intentionally no filesystem watcher or
    /// content-based invalidation because selected environment roots are treated as stable.
    #[tracing::instrument(
        name = "skills.executor.catalog_snapshot",
        level = "info",
        skip_all,
        fields(root_count = query.executor_roots.len())
    )]
    pub(crate) async fn executor_catalog_snapshot(
        &self,
        providers: &SkillProviders,
        mut query: SkillListQuery,
    ) -> SkillCatalog {
        let roots = std::mem::take(&mut query.executor_roots);
        let mut results = futures::stream::iter(roots.into_iter().enumerate())
            .map(|(index, root)| {
                let mut query = query.clone();
                query.executor_roots = vec![root.clone()];
                async move { (index, self.executor_root_catalog(providers, root, query).await) }
            })
            .buffer_unordered(MAX_CONCURRENT_EXECUTOR_ROOTS)
            .collect::<Vec<_>>()
            .await;
        results.sort_unstable_by_key(|(index, _)| *index);
        let mut catalog = SkillCatalog::default();
        for (_, root_catalog) in results {
            catalog.extend_ref(&root_catalog);
        }
        catalog
    }

    /// Returns the catalog that runtime contribution would observe without populating the
    /// executor cache. Existing cached roots retain their stable snapshot; uncached roots are
    /// discovered without advancing thread state.
    pub(crate) async fn estimate_executor_catalog_snapshot(
        &self,
        providers: &SkillProviders,
        mut query: SkillListQuery,
    ) -> SkillCatalog {
        let roots = std::mem::take(&mut query.executor_roots);
        let mut results = futures::stream::iter(roots.into_iter().enumerate())
            .map(|(index, root)| {
                let mut query = query.clone();
                query.executor_roots = vec![root.clone()];
                async move {
                    let catalog = match self.cached_executor_catalog(&root, &query) {
                        Some(cached) => cached,
                        None => Arc::new(catalog_or_warning(providers.list_executor_for_turn(query).await)),
                    };
                    (index, catalog)
                }
            })
            .buffer_unordered(MAX_CONCURRENT_EXECUTOR_ROOTS)
            .collect::<Vec<_>>()
            .await;
        results.sort_unstable_by_key(|(index, _)| *index);
        let mut catalog = SkillCatalog::default();
        for (_, root_catalog) in results {
            catalog.extend_ref(&root_catalog);
        }
        catalog
    }

    pub(crate) async fn orchestrator_catalog_snapshot(
        &self,
        mcp_resources: Option<&McpResourceClient>,
        initialize: impl Future<Output = Result<SkillCatalog, SkillProviderError>> + Send,
    ) -> SkillCatalog {
        self.orchestrator_catalog_snapshot_shared(mcp_resources, initialize)
            .await.as_ref().clone()
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "Continuations wait for the base catalog instead of extending an empty default"
    )]
    pub(crate) async fn orchestrator_catalog_snapshot_shared(
        &self,
        mcp_resources: Option<&McpResourceClient>,
        initialize: impl Future<Output = Result<SkillCatalog, SkillProviderError>> + Send,
    ) -> Arc<SkillCatalog> {
        let cache = self.orchestrator_cache(mcp_resources);
        let continued = cache.continued_catalog.lock().await;
        if let Some(catalog) = continued.as_ref() {
            return catalog.clone();
        }
        let deadline = *cache
            .discovery_deadline
            .get_or_init(|| tokio::time::Instant::now() + CATALOG_DISCOVERY_BUDGET);
        shared_catalog_or_warning(
            cache
                .catalog
                .get_or_init(|| async {
                    tokio::time::timeout_at(deadline, initialize)
                        .await
                        .unwrap_or_else(|_| Err(discovery_timeout()))
                        .map(Arc::new)
                })
                .await
                .clone(),
        )
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "Serialize continuation pages so one page is never appended twice"
    )]
    pub(crate) async fn continue_orchestrator_catalog_shared(
        &self,
        providers: &SkillProviders,
        mut query: SkillListQuery,
        expected_len: usize,
    ) -> Arc<SkillCatalog> {
        let cache = self.orchestrator_cache(query.mcp_resources.as_deref());
        let mut continued = cache.continued_catalog.lock().await;
        let catalog = continued.get_or_insert_with(|| {
            shared_catalog_or_warning(
                cache
                    .catalog
                    .get()
                    .cloned()
                    .unwrap_or_else(|| Ok(Arc::new(SkillCatalog::default()))),
            )
        });
        if catalog.entries.len() != expected_len {
            return catalog.clone();
        }
        let Some(continuation) = catalog.continuation.clone() else {
            return catalog.clone();
        };
        query.continuation = Some(continuation);
        match providers.list_orchestrator_for_turn(query).await {
            Ok(next) => {
                let catalog = Arc::make_mut(catalog);
                catalog.continuation = next.continuation;
                catalog.warnings = next.warnings;
                catalog.extend_entries(next.entries);
            }
            Err(error) => {
                Arc::make_mut(catalog).warnings = vec![error.model_message().to_string()];
            }
        }
        catalog.clone()
    }

    /// Explicit discovery can retry an outage without making every projection retry it.
    pub(crate) fn retry_failed_orchestrator_catalog(&self) {
        let mut cache = self
            .orchestrator_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if cache
            .as_ref()
            .is_some_and(|cache| matches!(cache.catalog.get(), Some(Err(_))))
        {
            *cache = None;
        }
    }

    pub(crate) async fn estimate_orchestrator_catalog_snapshot(
        &self,
        mcp_resources: Option<&McpResourceClient>,
        initialize: impl Future<Output = Result<SkillCatalog, SkillProviderError>> + Send,
    ) -> SkillCatalog {
        let cache_key = mcp_resources
            .and_then(|resources| resources.server_cache_key(CODEX_APPS_MCP_SERVER_NAME));
        let cache = self
            .orchestrator_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .filter(|cache| cache.mcp_cache_key == cache_key)
            .cloned();
        if let Some(cache) = cache {
            // Explicit discovery can append to the initial snapshot. Preview
            // the same catalog contribution will render, without committing
            // a cold estimate or starting additional discovery.
            if let Some(catalog) = cache.continued_catalog.lock().await.as_ref() {
                return catalog.as_ref().clone();
            }
            if let Some(catalog) = cache.catalog.get() {
                return shared_catalog_or_warning(catalog.clone()).as_ref().clone();
            }
        }

        initialize.await.unwrap_or_else(|err| SkillCatalog {
            continuation: None,
            warnings: vec![err.message],
            ..Default::default()
        })
    }

    pub(crate) async fn read_skill(
        &self,
        providers: &SkillProviders,
        request: SkillReadRequest,
    ) -> SkillProviderResult<Arc<SkillReadResult>> {
        if request.authority.kind != SkillSourceKind::Orchestrator {
            return providers.read(request).await.map(Arc::new);
        }

        let cache = self.orchestrator_cache(request.mcp_resources.as_deref());
        let cache_key = SkillReadCacheKey::from(&request);
        let slot = {
            let mut resources = cache
                .resources
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(result) = resources.get(&cache_key) {
                return Ok(result);
            }
            resources
                .in_flight
                .retain(|_, slot| slot.strong_count() > 0);
            if let Some(slot) = resources.in_flight.get(&cache_key).and_then(Weak::upgrade) {
                slot
            } else {
                let slot = Arc::new(OnceCell::new());
                resources
                    .in_flight
                    .insert(cache_key.clone(), Arc::downgrade(&slot));
                slot
            }
        };
        // Cancelled initialization leaves the cell empty. Failures are shared
        // only with current waiters; the weak slot permits a later retry.
        let result = slot
            .get_or_init(|| async { providers.read(request).await.map(Arc::new) })
            .await
            .clone()?;
        Ok(cache
            .resources
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(cache_key, result))
    }

    fn orchestrator_cache(
        &self,
        mcp_resources: Option<&McpResourceClient>,
    ) -> Arc<OrchestratorGenerationCache> {
        let mut cache = self
            .orchestrator_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let cache_key = mcp_resources
            .and_then(|resources| resources.server_cache_key(CODEX_APPS_MCP_SERVER_NAME));
        if let Some(cache) = cache
            .as_ref()
            .filter(|cache| cache.mcp_cache_key == cache_key)
        {
            return Arc::clone(cache);
        }

        let next_cache = Arc::new(OrchestratorGenerationCache {
            mcp_cache_key: cache_key,
            catalog: OnceCell::new(),
            discovery_deadline: OnceLock::new(),
            continued_catalog: tokio::sync::Mutex::new(None),
            resources: Mutex::new(OrchestratorResourceCache::default()),
        });
        *cache = Some(Arc::clone(&next_cache));
        next_cache
    }

    #[tracing::instrument(name = "skills.executor.catalog_root", level = "info", skip_all)]
    async fn executor_root_catalog(
        &self,
        providers: &SkillProviders,
        root: SelectedCapabilityRoot,
        query: SkillListQuery,
    ) -> Arc<SkillCatalog> {
        let binding = query
            .bound_executor_roots
            .iter()
            .find(|bound| bound.selected_root() == &root)
            .cloned();
        let (slot, deadline) = {
            let mut cache = self
                .executor_cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(cached) = cache.iter().find(|cached| {
                cached.root == root
                    && cached.binding == binding
                    && (cached.reusable_success()
                        || cached.turn_id == query.turn_id)
            }) {
                (Arc::clone(&cached.catalog), cached.discovery_deadline)
            } else {
                cache.retain(|cached| cached.root != root);
                let slot = Arc::new(OnceCell::new());
                let deadline = tokio::time::Instant::now() + CATALOG_DISCOVERY_BUDGET;
                cache.push(CachedExecutorCatalog {
                    root,
                    binding,
                    turn_id: query.turn_id.clone(),
                    catalog: Arc::clone(&slot),
                    discovery_deadline: deadline,
                });
                (slot, deadline)
            }
        };
        // Publish the slot before discovery, without holding the cache mutex
        // across I/O. Cancellation leaves it empty for the next observer.
        shared_catalog_or_warning(
            slot.get_or_init(|| async {
                tokio::time::timeout_at(deadline, providers.list_executor_for_turn(query))
                    .await
                    .unwrap_or_else(|_| Err(discovery_timeout()))
                    .map(Arc::new)
            })
            .await
            .clone(),
        )
    }

    fn cached_executor_catalog(
        &self,
        root: &SelectedCapabilityRoot,
        query: &SkillListQuery,
    ) -> Option<Arc<SkillCatalog>> {
        self.executor_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .find(|cached| {
                &cached.root == root
                    && cached.binding.as_ref()
                        == query
                            .bound_executor_roots
                            .iter()
                            .find(|bound| bound.selected_root() == root)
                    && (cached.reusable_success()
                        || cached.turn_id == query.turn_id)
            })
            .and_then(|cached| cached.catalog.get().cloned())
            .map(shared_catalog_or_warning)
    }
}

struct CachedExecutorCatalog {
    root: SelectedCapabilityRoot,
    binding: Option<codex_exec_server::ResolvedSelectedCapabilityRoot>,
    turn_id: String,
    catalog: Arc<OnceCell<SkillProviderResult<Arc<SkillCatalog>>>>,
    discovery_deadline: tokio::time::Instant,
}

impl CachedExecutorCatalog {
    fn reusable_success(&self) -> bool {
        matches!(self.catalog.get(), Some(Ok(catalog)) if catalog.warnings.is_empty()
            || tokio::time::Instant::now() < self.discovery_deadline + std::time::Duration::from_secs(30))
    }
}

struct OrchestratorGenerationCache {
    mcp_cache_key: Option<McpResourceServerCacheKey>,
    catalog: OnceCell<SkillProviderResult<Arc<SkillCatalog>>>,
    discovery_deadline: OnceLock<tokio::time::Instant>,
    continued_catalog: tokio::sync::Mutex<Option<Arc<SkillCatalog>>>,
    resources: Mutex<OrchestratorResourceCache>,
}

fn catalog_or_warning(result: SkillProviderResult<SkillCatalog>) -> SkillCatalog {
    result.unwrap_or_else(|err| SkillCatalog {
        continuation: None,
        warnings: vec![err.message],
        ..Default::default()
    })
}

fn shared_catalog_or_warning(result: SkillProviderResult<Arc<SkillCatalog>>) -> Arc<SkillCatalog> {
    result.unwrap_or_else(|err| Arc::new(catalog_or_warning(Err(err))))
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct SkillReadCacheKey {
    authority: SkillAuthority,
    package: SkillPackageId,
    resource: SkillResourceId,
}

impl From<&SkillReadRequest> for SkillReadCacheKey {
    fn from(request: &SkillReadRequest) -> Self {
        Self {
            authority: request.authority.clone(),
            package: request.package.clone(),
            resource: request.resource.clone(),
        }
    }
}

type ResourceReadSlot = OnceCell<SkillProviderResult<Arc<SkillReadResult>>>;

#[derive(Default)]
struct OrchestratorResourceCache {
    entries: HashMap<SkillReadCacheKey, Arc<SkillReadResult>>,
    recent: VecDeque<SkillReadCacheKey>,
    in_flight: HashMap<SkillReadCacheKey, Weak<ResourceReadSlot>>,
    contents_bytes: usize,
}

impl OrchestratorResourceCache {
    fn get(&mut self, key: &SkillReadCacheKey) -> Option<Arc<SkillReadResult>> {
        let result = self.entries.get(key).cloned()?;
        self.recent.retain(|entry| entry != key);
        self.recent.push_back(key.clone());
        Some(result)
    }

    fn insert(
        &mut self,
        key: SkillReadCacheKey,
        result: Arc<SkillReadResult>,
    ) -> Arc<SkillReadResult> {
        if let Some(cached) = self.get(&key) {
            return cached;
        }
        let bytes = result.contents.len();
        if bytes > MAX_CACHED_ORCHESTRATOR_CONTENT_BYTES {
            return result;
        }
        while self.entries.len() >= MAX_CACHED_ORCHESTRATOR_RESOURCES
            || self.contents_bytes.saturating_add(bytes) > MAX_CACHED_ORCHESTRATOR_CONTENT_BYTES
        {
            let Some(oldest) = self.recent.pop_front() else {
                break;
            };
            if let Some(evicted) = self.entries.remove(&oldest) {
                self.contents_bytes -= evicted.contents.len();
            }
        }
        self.contents_bytes += bytes;
        self.recent.push_back(key.clone());
        self.entries.insert(key, Arc::clone(&result));
        result
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ExecutorSkillsStepState(pub(crate) Arc<SkillCatalog>);

/// Warnings already shown for one turn.
///
/// Turn input is contributed again when a pending plan is rebuilt, for example after pre-turn
/// compaction. That rebuild also sees the step's executor catalog through both the ready roots and
/// [`ExecutorSkillsStepState`], so the same diagnostic can reach one contribution twice.
#[derive(Default)]
pub(crate) struct EmittedTurnWarnings(Mutex<HashSet<String>>);

impl EmittedTurnWarnings {
    /// Records `warning` and returns whether this is its first emission in the turn.
    pub(crate) fn first_emission(&self, warning: &str) -> bool {
        let mut emitted = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        !emitted.contains(warning) && emitted.insert(warning.to_string())
    }
}
