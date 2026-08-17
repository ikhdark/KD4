use crate::agents_md::AgentsMdFreshness;
use crate::agents_md::LoadedAgentsMd;
use crate::agents_md::effective_project_root_markers;
use crate::agents_md::load_project_instructions;
use crate::config::Config;
use crate::environment_selection::ThreadEnvironments;
use crate::environment_selection::TurnEnvironmentSnapshot;
use codex_extension_api::UserInstructions;
use codex_protocol::protocol::TurnEnvironmentSelection;
#[cfg(test)]
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
    loaded: Option<Arc<LoadedAgentsMd>>,
}

#[derive(Clone)]
pub(crate) struct AgentsMdObservation {
    pub(crate) loaded: Option<Arc<LoadedAgentsMd>>,
    pub(crate) freshness: AgentsMdFreshness,
}

#[derive(Clone, Debug, PartialEq)]
struct AgentsMdEnvironmentKey {
    selection: TurnEnvironmentSelection,
    environment_identity: usize,
}

#[derive(Clone, Debug, PartialEq)]
struct AgentsMdCacheKey {
    environment_generation: u64,
    ready_environments: Vec<AgentsMdEnvironmentKey>,
    starting_environments: Vec<TurnEnvironmentSelection>,
    project_doc_max_bytes: usize,
    fallback_filenames: Vec<String>,
    project_root_markers: Vec<String>,
}

impl AgentsMdCacheKey {
    fn capture(config: &Config, environments: &TurnEnvironmentSnapshot) -> Self {
        Self {
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
            project_root_markers: effective_project_root_markers(config),
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
        // Serialize key capture, filesystem loading, and publication so an older refresh cannot
        // finish after and overwrite a newer request. Clone the request's published value before
        // releasing the gate so a later refresh cannot replace it between refresh and capture.
        let Ok(_refresh_permit) = self.refresh_gate.acquire().await else {
            return AgentsMdObservation {
                loaded: self.get_loaded().await,
                freshness: AgentsMdFreshness::CachedFallback,
            };
        };
        self.refresh_with_gate_held(config, environments).await
    }

    pub(crate) async fn refresh_for_step(
        &self,
        config: &Config,
        environments: &ThreadEnvironments,
    ) -> (TurnEnvironmentSnapshot, AgentsMdObservation) {
        // Enter serialization before capturing live environments so an older snapshot cannot be
        // delayed until after a newer one publishes and then overwrite the newer cache entry.
        let Ok(_refresh_permit) = self.refresh_gate.acquire().await else {
            let environments = environments.snapshot().await;
            let observation = AgentsMdObservation {
                loaded: self.get_loaded().await,
                freshness: AgentsMdFreshness::CachedFallback,
            };
            return (environments, observation);
        };
        let environments = environments.snapshot().await;
        let observation = self.refresh_with_gate_held(config, &environments).await;
        (environments, observation)
    }

    async fn refresh_with_gate_held(
        &self,
        config: &Config,
        environments: &TurnEnvironmentSnapshot,
    ) -> AgentsMdObservation {
        let key = AgentsMdCacheKey::capture(config, environments);
        let load =
            load_project_instructions(config, self.user_instructions.clone(), environments).await;
        let mut cache = self.cache.lock().await;
        if !load.complete && cache.key.as_ref() == Some(&key) {
            return AgentsMdObservation {
                loaded: cache.loaded.clone(),
                freshness: AgentsMdFreshness::CachedFallback,
            };
        }
        let freshness = if load.complete {
            AgentsMdFreshness::Refreshed
        } else {
            AgentsMdFreshness::IncompleteRead
        };
        let loaded = load.loaded;
        let semantically_unchanged = cache.key.as_ref() == Some(&key)
            && match (cache.loaded.as_ref(), loaded.as_ref()) {
                (Some(current), Some(candidate)) => current.as_ref() == candidate,
                (None, None) => true,
                _ => false,
            };
        if !semantically_unchanged {
            cache.key = Some(key);
            cache.loaded = loaded.map(Arc::new);
        }
        AgentsMdObservation {
            loaded: cache.loaded.clone(),
            freshness,
        }
    }

    pub(crate) async fn get_loaded(&self) -> Option<Arc<LoadedAgentsMd>> {
        self.cache.lock().await.loaded.clone()
    }

    /// Returns the published cache without claiming that it was refreshed for this consumer.
    pub(crate) async fn get_cached_observation(&self) -> AgentsMdObservation {
        AgentsMdObservation {
            loaded: self.get_loaded().await,
            freshness: AgentsMdFreshness::CachedFallback,
        }
    }

    pub(crate) fn user_instructions(&self) -> Option<UserInstructions> {
        self.user_instructions.clone()
    }
}

#[cfg(test)]
#[path = "agents_md_manager_tests.rs"]
mod tests;
