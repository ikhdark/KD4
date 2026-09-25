use std::collections::HashMap;
use std::collections::VecDeque;
use std::future::Future;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;

use codex_mcp::CODEX_APPS_MCP_SERVER_NAME;
use codex_mcp::McpResourceClient;
use codex_mcp::McpResourceServerCacheKey;
use codex_protocol::capabilities::SelectedCapabilityRoot;
use tokio::sync::OnceCell;

use crate::SkillsExtensionConfig;
use crate::catalog::SkillAuthority;
use crate::catalog::SkillCatalog;
use crate::catalog::SkillCatalogEntry;
use crate::catalog::SkillPackageId;
use crate::catalog::SkillProviderError;
use crate::catalog::SkillProviderResult;
use crate::catalog::SkillReadResult;
use crate::catalog::SkillResourceId;
use crate::catalog::SkillSourceKind;
use crate::provider::SkillListQuery;
use crate::provider::SkillReadRequest;
use crate::sources::SkillProviders;

const MAX_CACHED_ORCHESTRATOR_RESOURCES: usize = 100;
const MAX_CACHED_ORCHESTRATOR_CONTENT_BYTES: usize = 8 * 1024 * 1024;

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
    /// The first successful catalog for a root remains cached until this thread state is dropped.
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
        let mut catalog = SkillCatalog::default();
        for root in roots {
            query.executor_roots = vec![root.clone()];
            catalog.extend(
                self.executor_root_catalog(providers, root, query.clone())
                    .await,
            );
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
        let mut catalog = SkillCatalog::default();
        for root in roots {
            query.executor_roots = vec![root.clone()];
            if let Some(cached) = self.cached_executor_catalog(&root, &query.turn_id) {
                catalog.extend(cached);
            } else {
                catalog.extend(catalog_or_warning(
                    providers.list_executor_for_turn(query.clone()).await,
                ));
            }
        }
        catalog
    }

    pub(crate) async fn orchestrator_catalog_snapshot(
        &self,
        mcp_resources: Option<&McpResourceClient>,
        initialize: impl Future<Output = Result<SkillCatalog, SkillProviderError>> + Send,
    ) -> SkillCatalog {
        let cache = self.orchestrator_cache(mcp_resources);
        let continued = cache.continued_catalog.lock().await;
        if let Some(catalog) = continued.as_ref() {
            return catalog.clone();
        }
        catalog_or_warning(cache.catalog.get_or_init(|| initialize).await.clone())
    }

    pub(crate) async fn continue_orchestrator_catalog(
        &self,
        providers: &SkillProviders,
        mut query: SkillListQuery,
        expected_len: usize,
    ) -> SkillCatalog {
        let cache = self.orchestrator_cache(query.mcp_resources.as_deref());
        let mut continued = cache.continued_catalog.lock().await;
        let catalog = continued.get_or_insert_with(|| {
            catalog_or_warning(
                cache
                    .catalog
                    .get()
                    .cloned()
                    .unwrap_or_else(|| Ok(SkillCatalog::default())),
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
                catalog.continuation = next.continuation;
                catalog.warnings = next.warnings;
                catalog.extend_entries(next.entries);
            }
            Err(error) => {
                catalog.warnings = vec![error.model_message().to_string()];
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
                return catalog.clone();
            }
            if let Some(catalog) = cache.catalog.get() {
                return catalog_or_warning(catalog.clone());
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
    ) -> SkillCatalog {
        if let Some(cached) = self.cached_executor_catalog(&root, &query.turn_id) {
            return cached;
        }

        let turn_id = query.turn_id.clone();
        let discovered = providers.list_executor_for_turn(query).await;
        let mut cache = self
            .executor_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(cached) = cache.iter().find(|cached| {
            cached.root == root && (cached.catalog.is_ok() || cached.turn_id == turn_id)
        }) {
            return catalog_or_warning(cached.catalog.clone());
        }
        cache.retain(|cached| cached.root != root);
        cache.push(CachedExecutorCatalog {
            root,
            turn_id,
            catalog: discovered.clone(),
        });
        catalog_or_warning(discovered)
    }

    fn cached_executor_catalog(
        &self,
        root: &SelectedCapabilityRoot,
        turn_id: &str,
    ) -> Option<SkillCatalog> {
        self.executor_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .find(|cached| {
                &cached.root == root && (cached.catalog.is_ok() || cached.turn_id == turn_id)
            })
            .map(|cached| catalog_or_warning(cached.catalog.clone()))
    }
}

struct CachedExecutorCatalog {
    root: SelectedCapabilityRoot,
    turn_id: String,
    catalog: SkillProviderResult<SkillCatalog>,
}

struct OrchestratorGenerationCache {
    mcp_cache_key: Option<McpResourceServerCacheKey>,
    catalog: OnceCell<SkillProviderResult<SkillCatalog>>,
    continued_catalog: tokio::sync::Mutex<Option<SkillCatalog>>,
    resources: Mutex<OrchestratorResourceCache>,
}

fn catalog_or_warning(result: SkillProviderResult<SkillCatalog>) -> SkillCatalog {
    result.unwrap_or_else(|err| SkillCatalog {
        continuation: None,
        warnings: vec![err.message],
        ..Default::default()
    })
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

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SkillsTurnState {
    pub(crate) catalog: SkillCatalog,
    pub(crate) selected_entries: Vec<SkillCatalogEntry>,
    pub(crate) warnings: Vec<String>,
    pub(crate) main_prompts_injected: bool,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ExecutorSkillsStepState(pub(crate) SkillCatalog);

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn estimate_includes_explicitly_continued_catalog_without_discovery() {
        let state = SkillsThreadState::new(
            SkillsExtensionConfig {
                include_instructions: true,
                bundled_skills_enabled: true,
                orchestrator_skills_enabled: true,
            },
            true,
        );
        let initial = SkillCatalog {
            continuation: Some(Default::default()),
            ..Default::default()
        };
        state
            .orchestrator_catalog_snapshot(None, async { Ok(initial.clone()) })
            .await;
        let continued = SkillCatalog {
            entries: vec![SkillCatalogEntry::new(
                SkillPackageId("later".to_string()),
                SkillAuthority::new(SkillSourceKind::Orchestrator, "codex_apps"),
                "later",
                "Discovered through continuation",
                SkillResourceId::new("skill://later/SKILL.md"),
            )],
            ..Default::default()
        };
        let cache = state.orchestrator_cache(None);
        *cache.continued_catalog.lock().await = Some(continued.clone());

        for _ in 0..2 {
            let preview = state
                .estimate_orchestrator_catalog_snapshot(None, async {
                    panic!("a warm preview must not rediscover skills")
                })
                .await;
            assert_eq!(preview, continued);
        }
        assert_eq!(cache.catalog.get(), Some(&Ok(initial)));
        assert_eq!(
            state
                .orchestrator_catalog_snapshot(None, async {
                    panic!("contribution must reuse explicit discovery")
                })
                .await,
            continued
        );
    }
}

#[cfg(test)]
mod resource_cache_tests {
    use super::*;
    use crate::provider::SkillProvider;
    use crate::provider::SkillProviderFuture;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    fn request(name: &str) -> SkillReadRequest {
        SkillReadRequest {
            authority: SkillAuthority::new(SkillSourceKind::Orchestrator, "codex_apps"),
            package: SkillPackageId("skill://test/package".to_string()),
            resource: SkillResourceId::new(name),
            host_snapshot: None,
            mcp_resources: None,
        }
    }

    #[test]
    fn saturated_cache_admits_the_new_working_set_without_copying_contents() {
        let mut cache = OrchestratorResourceCache::default();
        for index in 0..9 {
            let req = request(&index.to_string());
            let result = Arc::new(SkillReadResult {
                resource: req.resource.clone(),
                contents: "x".repeat(1024 * 1024),
            });
            let key = SkillReadCacheKey::from(&req);
            cache.insert(key.clone(), Arc::clone(&result));
            assert!(Arc::ptr_eq(
                &cache.get(&key).expect("new resource admitted"),
                &result
            ));
            assert!(cache.contents_bytes <= MAX_CACHED_ORCHESTRATOR_CONTENT_BYTES);
        }
        assert_eq!(cache.entries.len(), 8);
        assert!(cache.get(&SkillReadCacheKey::from(&request("0"))).is_none());
        assert!(cache.get(&SkillReadCacheKey::from(&request("8"))).is_some());
    }

    struct CountingReader {
        calls: AtomicUsize,
        fail_first: bool,
    }
    impl SkillProvider for CountingReader {
        fn list(&self, _: SkillListQuery) -> SkillProviderFuture<'_, SkillCatalog> {
            Box::pin(async { Ok(SkillCatalog::default()) })
        }
        fn read(&self, request: SkillReadRequest) -> SkillProviderFuture<'_, SkillReadResult> {
            Box::pin(async move {
                let call = self.calls.fetch_add(1, Ordering::SeqCst);
                tokio::task::yield_now().await;
                if self.fail_first && call == 0 {
                    return Err(SkillProviderError::new("temporary"));
                }
                Ok(SkillReadResult {
                    resource: request.resource,
                    contents: "instructions".to_string(),
                })
            })
        }
    }

    #[tokio::test]
    async fn concurrent_reads_share_success_and_failed_reads_can_be_retried() {
        for fail_first in [false, true] {
            let state = SkillsThreadState::new(
                SkillsExtensionConfig {
                    include_instructions: true,
                    bundled_skills_enabled: true,
                    orchestrator_skills_enabled: true,
                },
                true,
            );
            let reader = Arc::new(CountingReader {
                calls: AtomicUsize::new(0),
                fail_first,
            });
            let providers = SkillProviders::new().with_orchestrator_provider(reader.clone());
            let (first, second) = tokio::join!(
                state.read_skill(&providers, request("resource")),
                state.read_skill(&providers, request("resource"))
            );
            assert_eq!(reader.calls.load(Ordering::SeqCst), 1);
            if fail_first {
                assert!(first.is_err() && second.is_err());
                assert_eq!(
                    state
                        .read_skill(&providers, request("resource"))
                        .await
                        .expect("retry")
                        .contents,
                    "instructions"
                );
                assert_eq!(reader.calls.load(Ordering::SeqCst), 2);
            } else {
                assert!(Arc::ptr_eq(
                    &first.expect("first"),
                    &second.expect("second")
                ));
            }
        }
    }
}
