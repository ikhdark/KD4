use crate::agents_md::AgentsMdFreshness;
use crate::agents_md::LoadedAgentsMd;
use crate::agents_md::RepositoryStableContextBundle;
use crate::agents_md::discover_project_instructions_with_markers;
use crate::agents_md::effective_project_root_markers;
use crate::agents_md::load_project_instructions_with_fallback;
use crate::config::Config;
use crate::environment_selection::ThreadEnvironments;
use crate::environment_selection::TurnEnvironmentSnapshot;
use codex_config::ConfigLayerStack;
use codex_extension_api::UserInstructions;
use codex_protocol::protocol::TurnEnvironmentSelection;
use codex_utils_path_uri::PathUri;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use tokio::sync::Mutex;
use tokio::sync::Semaphore;

const REFRESH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Owns the inputs and cached result of AGENTS.md discovery for a session.
pub(crate) struct AgentsMdManager {
    user_instructions: Option<UserInstructions>,
    omission_recovery: Option<crate::agents_md::ProjectDocOmissionRecovery>,
    refresh_gate: Semaphore,
    cache: Mutex<AgentsMdCache>,
    project_root_markers_cache: StdMutex<Option<ProjectRootMarkersCacheEntry>>,
}

struct ProjectRootMarkersCacheEntry {
    config_layer_stack: Arc<ConfigLayerStack>,
    markers: Arc<[String]>,
}

#[derive(Default)]
struct AgentsMdCache {
    key: Option<AgentsMdCacheKey>,
    complete: bool,
    loaded: Option<Arc<LoadedAgentsMd>>,
    retained_sources: Vec<crate::agents_md::RetainedProjectDoc>,
    stable_context: Option<RepositoryStableContextBundle>,
    recovered_snapshot: Option<serde_json::Value>,
}

#[derive(Clone)]
pub(crate) struct AgentsMdObservation {
    pub(crate) loaded: Option<Arc<LoadedAgentsMd>>,
    pub(crate) stable_context: Option<RepositoryStableContextBundle>,
    pub(crate) freshness: AgentsMdFreshness,
}

#[derive(Clone, Debug, PartialEq)]
struct AgentsMdEnvironmentKey {
    selection: TurnEnvironmentSelection,
    environment_identity: usize,
}

#[derive(Clone, Debug, PartialEq)]
struct AgentsMdCacheKey {
    active_cwd: PathUri,
    environment_generation: u64,
    ready_environments: Vec<AgentsMdEnvironmentKey>,
    starting_environments: Vec<TurnEnvironmentSelection>,
    project_doc_max_bytes: usize,
    fallback_filenames: Vec<String>,
    project_root_markers: Vec<String>,
}

impl AgentsMdCacheKey {
    /// Persist scope evidence, not process-local Arc addresses. A resumed
    /// aggregate without matching evidence remains an opaque original-scope
    /// fallback; it must not seed the current environment's source cache.
    fn persisted_scope(&self, environments: &TurnEnvironmentSnapshot) -> serde_json::Value {
        serde_json::json!({
            "version": 1,
            "active_cwd": self.active_cwd,
            "environment_generation": self.environment_generation,
            "ready_environments": environments.turn_environments.iter()
                .map(|environment| (environment.selection(), environment.owner_id()))
                .collect::<Vec<_>>(),
            "starting_environments": self.starting_environments,
            "project_doc_max_bytes": self.project_doc_max_bytes,
            "fallback_filenames": self.fallback_filenames,
            "project_root_markers": self.project_root_markers,
        })
    }

    #[cfg(test)]
    fn capture(config: &Config, environments: &TurnEnvironmentSnapshot) -> Self {
        let project_root_markers = effective_project_root_markers(config);
        Self::capture_with_markers(config, environments, &project_root_markers)
    }

    fn capture_with_markers(
        config: &Config,
        environments: &TurnEnvironmentSnapshot,
        project_root_markers: &[String],
    ) -> Self {
        Self {
            active_cwd: PathUri::from_abs_path(&config.cwd),
            environment_generation: environments.generation,
            ready_environments: environments
                .turn_environments
                .iter()
                .map(|environment| AgentsMdEnvironmentKey {
                    selection: environment.selection(),
                    environment_identity: Arc::as_ptr(&environment.environment).cast::<()>()
                        as usize,
                })
                .collect(),
            starting_environments: environments
                .starting
                .iter()
                .map(|environment| environment.selection.clone())
                .collect(),
            project_doc_max_bytes: config.project_doc_max_bytes,
            fallback_filenames: config.project_doc_fallback_filenames.clone(),
            project_root_markers: project_root_markers.to_vec(),
        }
    }
}

impl AgentsMdCache {
    fn unverified_recovery_observation(&self, freshness: AgentsMdFreshness) -> AgentsMdObservation {
        let loaded = self
            .loaded
            .as_ref()
            .map(|loaded| Arc::new(loaded.without_unverified_omission_receipts()));
        let stable_context = loaded.as_ref().zip(self.key.as_ref()).map(|(loaded, key)| {
            let mut bundle = loaded.stable_context_bundle(&key.active_cwd);
            bundle.source_snapshot = self
                .stable_context
                .as_ref()
                .and_then(|previous| previous.source_snapshot.clone());
            bundle
        });
        AgentsMdObservation {
            loaded,
            stable_context,
            freshness,
        }
    }

    fn cached_observation(&self, freshness: AgentsMdFreshness) -> AgentsMdObservation {
        AgentsMdObservation {
            loaded: self.loaded.clone(),
            stable_context: self
                .stable_context
                .as_ref()
                .map(RepositoryStableContextBundle::as_cached),
            freshness,
        }
    }
}

impl AgentsMdManager {
    pub(crate) fn new(user_instructions: Option<UserInstructions>) -> Self {
        Self {
            omission_recovery: None,
            user_instructions: user_instructions
                .filter(|instructions| !instructions.text.trim().is_empty()),
            refresh_gate: Semaphore::new(1),
            cache: Mutex::new(AgentsMdCache::default()),
            project_root_markers_cache: StdMutex::new(None),
        }
    }

    pub(crate) fn with_thread_id(mut self, thread_id: String) -> Self {
        self.omission_recovery = Some(crate::agents_md::ProjectDocOmissionRecovery::new(thread_id));
        self
    }

    pub(crate) async fn restore_accepted_sources(
        &self,
        config: &Config,
        environments: &TurnEnvironmentSnapshot,
        value: &serde_json::Value,
    ) {
        let Some((loaded, sources)) =
            LoadedAgentsMd::restore_project_sources(value, config, self.user_instructions.clone())
        else {
            return;
        };
        let _permit = self.refresh_gate.acquire().await;
        let key = AgentsMdCacheKey::capture_with_markers(
            config,
            environments,
            &self.project_root_markers(config),
        );
        if value.get("observation_scope") != Some(&key.persisted_scope(environments)) {
            return;
        }
        let mut cache = self.cache.lock().await;
        // A resumed rollout is fallback evidence, not a replacement for the
        // instructions already loaded by this session.
        if cache.complete {
            return;
        }
        let mut bundle = loaded.stable_context_bundle(&key.active_cwd);
        bundle.source_snapshot = Some(Arc::new(value.clone()));
        cache.key = Some(key);
        cache.loaded = Some(Arc::new(loaded));
        cache.retained_sources = sources;
        cache.stable_context = Some(bundle);
    }

    pub(crate) async fn restore_repository_snapshot(
        &self,
        config: &Config,
        environments: &TurnEnvironmentSnapshot,
        snapshot: &serde_json::Value,
    ) {
        if let Some(sources) = snapshot.get("source_snapshot") {
            self.restore_accepted_sources(config, environments, sources)
                .await;
        }
        self.cache.lock().await.recovered_snapshot = Some(snapshot.clone());
    }

    pub(crate) async fn recovered_snapshot(&self) -> Option<serde_json::Value> {
        self.cache.lock().await.recovered_snapshot.clone()
    }

    fn project_root_markers(&self, config: &Config) -> Arc<[String]> {
        let mut cache = self
            .project_root_markers_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(cached) = cache.as_ref()
            && Arc::ptr_eq(&cached.config_layer_stack, &config.config_layer_stack)
        {
            return Arc::clone(&cached.markers);
        }

        let markers: Arc<[String]> = effective_project_root_markers(config).into();
        *cache = Some(ProjectRootMarkersCacheEntry {
            config_layer_stack: config.config_layer_stack.clone(),
            markers: Arc::clone(&markers),
        });
        markers
    }

    pub(crate) async fn refresh(&self, config: &Config, environments: &TurnEnvironmentSnapshot) {
        let _ = self.refresh_and_get_loaded(config, environments).await;
    }

    pub(crate) async fn refresh_and_get_loaded(
        &self,
        config: &Config,
        environments: &TurnEnvironmentSnapshot,
    ) -> Option<Arc<LoadedAgentsMd>> {
        self.refresh_and_observe(config, environments).await.loaded
    }

    pub(crate) async fn refresh_and_observe(
        &self,
        config: &Config,
        environments: &TurnEnvironmentSnapshot,
    ) -> AgentsMdObservation {
        let config = Arc::new(config.clone());
        let refresh = async {
            let _permit = self.refresh_gate.acquire().await.ok()?;
            Some(
                self.refresh_with_gate_held(Arc::clone(&config), environments)
                    .await,
            )
        };
        match tokio::time::timeout(REFRESH_TIMEOUT, refresh).await {
            Ok(Some(observation)) => observation,
            _ => self.scoped_fallback(&config, environments),
        }
    }

    pub(crate) async fn refresh_and_observe_shared(
        &self,
        config: &Arc<Config>,
        environments: &TurnEnvironmentSnapshot,
    ) -> AgentsMdObservation {
        let refresh = async {
            let _permit = self.refresh_gate.acquire().await.ok()?;
            Some(
                self.observe_with_gate_held(Arc::clone(config), environments)
                    .await,
            )
        };
        match tokio::time::timeout(REFRESH_TIMEOUT, refresh).await {
            Ok(Some(observation)) => observation,
            _ => self.scoped_fallback(config, environments),
        }
    }

    pub(crate) async fn refresh_for_step<T, F>(
        &self,
        config: &Arc<Config>,
        environments: &ThreadEnvironments,
        prepare: impl FnOnce(TurnEnvironmentSnapshot) -> F,
    ) -> (TurnEnvironmentSnapshot, AgentsMdObservation, T)
    where
        F: std::future::Future<Output = T>,
    {
        let deadline = tokio::time::Instant::now() + REFRESH_TIMEOUT;
        let capture = async {
            let permit = self.refresh_gate.acquire().await.ok()?;
            let snapshot = environments.snapshot().await;
            Some((snapshot, permit))
        };
        let (snapshot, permit) = match tokio::time::timeout_at(deadline, capture).await {
            Ok(Some((snapshot, permit))) => (snapshot, Some(permit)),
            _ => {
                // Capture ready identities without awaiting a stalled resolver.
                (environments.snapshot_now().await, None)
            }
        };
        let refresh = Box::pin(async {
            if let Some(_permit) = permit
                && let Ok(observation) = tokio::time::timeout_at(
                    deadline,
                    self.observe_with_gate_held(Arc::clone(config), &snapshot),
                )
                .await
            {
                return observation;
            }
            // Preparation already uses this snapshot; even a timeout must not switch scopes.
            self.scoped_fallback(config, &snapshot)
        });
        // Do not short-circuit instruction refresh when preparation returns an error, or keep
        // the refresh gate held while waiting for preparation after instructions are ready.
        let (observation, prepared) = tokio::join!(refresh, Box::pin(prepare(snapshot.clone())));
        (snapshot, observation, prepared)
    }

    /// Instructions are session-scoped. Reload only for a new environment or
    /// instruction configuration, or to recover an incomplete initial load.
    async fn observe_with_gate_held(
        &self,
        config: Arc<Config>,
        environments: &TurnEnvironmentSnapshot,
    ) -> AgentsMdObservation {
        let key = AgentsMdCacheKey::capture_with_markers(
            &config,
            environments,
            &self.project_root_markers(&config),
        );
        {
            let cache = self.cache.lock().await;
            if cache.complete && cache.key.as_ref() == Some(&key) {
                // Refreshed denotes a complete load for this scope, not a
                // claim that files were reread for the current model request.
                drop(cache);
                // Verify only an overflow manifest, using the existing bounded
                // recovery owner. Instruction files stay session-snapshotted.
                return self.cached_observation_with_recovery(AgentsMdFreshness::Refreshed).await;
            }
        }
        self.refresh_with_gate_held(config, environments).await
    }

    fn scoped_fallback(
        &self,
        config: &Config,
        environments: &TurnEnvironmentSnapshot,
    ) -> AgentsMdObservation {
        let key = AgentsMdCacheKey::capture_with_markers(
            config,
            environments,
            &self.project_root_markers(config),
        );
        if let Ok(cache) = self.cache.try_lock()
            && cache.key.as_ref() == Some(&key)
        {
            return cache.unverified_recovery_observation(AgentsMdFreshness::CachedFallback);
        }
        let loaded = self.user_instructions.as_ref().map(|instructions| {
            Arc::new(LoadedAgentsMd::new_user(
                instructions.text.clone(),
                instructions.source.clone(),
            ))
        });
        let stable_context = loaded
            .as_ref()
            .map(|loaded| loaded.stable_context_bundle(&key.active_cwd));
        AgentsMdObservation {
            loaded,
            stable_context,
            freshness: AgentsMdFreshness::IncompleteRead,
        }
    }

    async fn refresh_with_gate_held(
        &self,
        config: Arc<Config>,
        environments: &TurnEnvironmentSnapshot,
    ) -> AgentsMdObservation {
        let project_root_markers = self.project_root_markers(config.as_ref());
        let key = AgentsMdCacheKey::capture_with_markers(
            config.as_ref(),
            environments,
            project_root_markers.as_ref(),
        );
        let discovery = discover_project_instructions_with_markers(
            Arc::clone(&config),
            environments,
            project_root_markers.as_ref(),
        )
        .await;
        // Explicit refreshes and new scopes read the instruction bytes rather
        // than using metadata as a content identity.
        let previous = {
            let cache = self.cache.lock().await;
            (cache.key.as_ref() == Some(&key))
                .then(|| {
                    cache
                        .loaded
                        .clone()
                        .map(|loaded| (loaded, cache.retained_sources.clone()))
                })
                .flatten()
        };
        let load = load_project_instructions_with_fallback(
            config.as_ref(),
            self.user_instructions.clone(),
            discovery,
            self.omission_recovery.as_ref(),
            previous
                .as_ref()
                .map(|(loaded, sources)| (loaded.as_ref(), sources.as_slice())),
        )
        .await;
        let mut cache = self.cache.lock().await;
        cache.complete = load.complete;
        let freshness = if load.complete {
            cache.recovered_snapshot = None;
            AgentsMdFreshness::Refreshed
        } else if previous.is_some() {
            AgentsMdFreshness::CachedFallback
        } else {
            AgentsMdFreshness::IncompleteRead
        };
        let loaded = load.loaded;
        let source_snapshot = loaded.as_ref().map(|loaded| {
            let mut snapshot =
                loaded.accepted_project_sources(&key.active_cwd, &load.retained_sources);
            snapshot["observation_scope"] = key.persisted_scope(environments);
            Arc::new(snapshot)
        });
        cache.retained_sources = load.retained_sources;
        let semantically_unchanged = cache.key.as_ref() == Some(&key)
            && match (cache.loaded.as_ref(), loaded.as_ref()) {
                (Some(current), Some(candidate)) => current.as_ref() == candidate,
                (None, None) => true,
                _ => false,
            };
        if !semantically_unchanged {
            let loaded = loaded.map(Arc::new);
            let mut stable_context = loaded
                .as_deref()
                .map(|loaded| loaded.stable_context_bundle(&key.active_cwd));
            if let (Some(previous), Some(current)) =
                (cache.stable_context.as_ref(), stable_context.as_mut())
            {
                if previous.identity == current.identity {
                    *current = previous.as_cached();
                } else if previous.rendered == current.rendered {
                    current.semantic_replacement = true;
                }
            }
            cache.key = Some(key);
            cache.loaded = loaded;
            cache.stable_context = stable_context;
            if let Some(bundle) = cache.stable_context.as_mut() {
                bundle.source_snapshot = source_snapshot;
            }
            return AgentsMdObservation {
                loaded: cache.loaded.clone(),
                stable_context: cache.stable_context.clone(),
                freshness,
            };
        }
        if let Some(bundle) = cache.stable_context.as_mut() {
            bundle.source_snapshot = source_snapshot;
        }
        cache.cached_observation(freshness)
    }

    pub(crate) async fn get_loaded(&self) -> Option<Arc<LoadedAgentsMd>> {
        self.get_cached_observation().await.loaded
    }

    /// Returns the published cache without claiming that it was refreshed for this consumer.
    pub(crate) async fn get_cached_observation(&self) -> AgentsMdObservation {
        self.cached_observation_with_recovery(AgentsMdFreshness::CachedFallback).await
    }

    async fn cached_observation_with_recovery(&self, freshness: AgentsMdFreshness) -> AgentsMdObservation {
        let available = match &self.omission_recovery {
            Some(owner) => owner.cached_manifest_available().await,
            None => true,
        };
        if !available {
            return self
                .cache
                .lock()
                .await
                .unverified_recovery_observation(AgentsMdFreshness::CachedFallback);
        }
        self.cache
            .lock()
            .await
            .cached_observation(freshness)
    }

    pub(crate) fn user_instructions(&self) -> Option<UserInstructions> {
        self.user_instructions.clone()
    }
}

#[cfg(test)]
#[path = "agents_md_manager_tests.rs"]
mod tests;
