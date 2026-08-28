use crate::agents_md::AgentsMdFreshness;
use crate::agents_md::LoadedAgentsMd;
use crate::agents_md::ProjectInstructionsSourceFingerprint;
use crate::agents_md::RepositoryStableContextBundle;
use crate::agents_md::discover_project_instructions_with_markers;
use crate::agents_md::effective_project_root_markers;
use crate::agents_md::load_project_instructions_from_discovery;
use crate::config::Config;
use crate::environment_selection::ThreadEnvironments;
use crate::environment_selection::TurnEnvironmentSnapshot;
use codex_extension_api::UserInstructions;
use codex_protocol::protocol::TurnEnvironmentSelection;
use codex_utils_path_uri::PathUri;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::Semaphore;

/// Owns the inputs and cached result of AGENTS.md discovery for a session.
pub(crate) struct AgentsMdManager {
    user_instructions: Option<UserInstructions>,
    refresh_gate: Semaphore,
    cache: Mutex<AgentsMdCache>,
}

#[derive(Default)]
struct AgentsMdCache {
    key: Option<AgentsMdCacheKey>,
    source_fingerprint: Option<ProjectInstructionsSourceFingerprint>,
    loaded: Option<Arc<LoadedAgentsMd>>,
    stable_context: Option<RepositoryStableContextBundle>,
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
            user_instructions: user_instructions
                .filter(|instructions| !instructions.text.trim().is_empty()),
            refresh_gate: Semaphore::new(1),
            cache: Mutex::new(AgentsMdCache::default()),
        }
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
        self.refresh_and_observe_shared(&config, environments).await
    }

    pub(crate) async fn refresh_and_observe_shared(
        &self,
        config: &Arc<Config>,
        environments: &TurnEnvironmentSnapshot,
    ) -> AgentsMdObservation {
        // Serialize key capture, filesystem loading, and publication so an older refresh cannot
        // finish after and overwrite a newer request. Clone the request's published value before
        // releasing the gate so a later refresh cannot replace it between refresh and capture.
        let Ok(_refresh_permit) = self.refresh_gate.acquire().await else {
            return self.get_cached_observation().await;
        };
        self.refresh_with_gate_held(Arc::clone(config), environments)
            .await
    }

    pub(crate) async fn refresh_for_step(
        &self,
        config: &Arc<Config>,
        environments: &ThreadEnvironments,
    ) -> (TurnEnvironmentSnapshot, AgentsMdObservation) {
        // Enter serialization before capturing live environments so an older snapshot cannot be
        // delayed until after a newer one publishes and then overwrite the newer cache entry.
        let Ok(_refresh_permit) = self.refresh_gate.acquire().await else {
            let environments = environments.snapshot().await;
            let observation = self.get_cached_observation().await;
            return (environments, observation);
        };
        let environments = environments.snapshot().await;
        let observation = self
            .refresh_with_gate_held(Arc::clone(config), &environments)
            .await;
        (environments, observation)
    }

    async fn refresh_with_gate_held(
        &self,
        config: Arc<Config>,
        environments: &TurnEnvironmentSnapshot,
    ) -> AgentsMdObservation {
        let project_root_markers = effective_project_root_markers(config.as_ref());
        let key = AgentsMdCacheKey::capture_with_markers(
            config.as_ref(),
            environments,
            &project_root_markers,
        );
        let discovery = discover_project_instructions_with_markers(
            Arc::clone(&config),
            environments,
            &project_root_markers,
        )
        .await;
        let source_fingerprint = discovery.source_fingerprint();
        if source_fingerprint.is_some() {
            let cache = self.cache.lock().await;
            if cache.key.as_ref() == Some(&key)
                && cache.source_fingerprint.as_ref() == source_fingerprint.as_ref()
            {
                return cache.cached_observation(AgentsMdFreshness::Refreshed);
            }
        }
        let load = load_project_instructions_from_discovery(
            config.as_ref(),
            self.user_instructions.clone(),
            discovery,
        )
        .await;
        let mut cache = self.cache.lock().await;
        if !load.complete && cache.key.as_ref() == Some(&key) {
            return cache.cached_observation(AgentsMdFreshness::CachedFallback);
        }
        let freshness = if load.complete {
            AgentsMdFreshness::Refreshed
        } else {
            AgentsMdFreshness::IncompleteRead
        };
        let complete = load.complete;
        let loaded = load.loaded;
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
            cache.source_fingerprint = complete.then_some(source_fingerprint).flatten();
            cache.loaded = loaded;
            cache.stable_context = stable_context;
            return AgentsMdObservation {
                loaded: cache.loaded.clone(),
                stable_context: cache.stable_context.clone(),
                freshness,
            };
        }
        if complete {
            cache.source_fingerprint = source_fingerprint;
        }
        cache.cached_observation(freshness)
    }

    pub(crate) async fn get_loaded(&self) -> Option<Arc<LoadedAgentsMd>> {
        self.cache.lock().await.loaded.clone()
    }

    /// Returns the published cache without claiming that it was refreshed for this consumer.
    pub(crate) async fn get_cached_observation(&self) -> AgentsMdObservation {
        self.cache
            .lock()
            .await
            .cached_observation(AgentsMdFreshness::CachedFallback)
    }

    pub(crate) fn user_instructions(&self) -> Option<UserInstructions> {
        self.user_instructions.clone()
    }
}

#[cfg(test)]
#[path = "agents_md_manager_tests.rs"]
mod tests;
