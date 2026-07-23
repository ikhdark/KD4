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
        // Serialize key capture, filesystem loading, and publication so an older refresh cannot
        // finish after and overwrite a newer request. Clone the request's published value before
        // releasing the gate so a later refresh cannot replace it between refresh and capture.
        let Ok(_refresh_permit) = self.refresh_gate.acquire().await else {
            return self.get_loaded().await;
        };
        self.refresh_with_gate_held(config, environments).await
    }

    pub(crate) async fn refresh_for_step(
        &self,
        config: &Config,
        environments: &ThreadEnvironments,
    ) -> (TurnEnvironmentSnapshot, Option<Arc<LoadedAgentsMd>>) {
        // Enter serialization before capturing live environments so an older snapshot cannot be
        // delayed until after a newer one publishes and then overwrite the newer cache entry.
        let Ok(_refresh_permit) = self.refresh_gate.acquire().await else {
            let environments = environments.snapshot().await;
            let loaded = self.get_loaded().await;
            return (environments, loaded);
        };
        let environments = environments.snapshot().await;
        let loaded = self.refresh_with_gate_held(config, &environments).await;
        (environments, loaded)
    }

    async fn refresh_with_gate_held(
        &self,
        config: &Config,
        environments: &TurnEnvironmentSnapshot,
    ) -> Option<Arc<LoadedAgentsMd>> {
        let key = AgentsMdCacheKey::capture(config, environments);
        let load =
            load_project_instructions(config, self.user_instructions.clone(), environments).await;
        let mut cache = self.cache.lock().await;
        if !load.complete && cache.key.as_ref() == Some(&key) {
            return cache.loaded.clone();
        }
        let loaded = load.loaded;
        let semantically_unchanged = cache.key.as_ref() == Some(&key)
            && match (cache.loaded.as_ref(), loaded.as_ref()) {
                (Some(current), Some(candidate)) => {
                    current.semantic_digest() == candidate.semantic_digest()
                }
                (None, None) => true,
                _ => false,
            };
        if !semantically_unchanged {
            cache.key = Some(key);
            cache.loaded = loaded.map(Arc::new);
        }
        cache.loaded.clone()
    }

    pub(crate) async fn get_loaded(&self) -> Option<Arc<LoadedAgentsMd>> {
        self.cache.lock().await.loaded.clone()
    }

    pub(crate) fn user_instructions(&self) -> Option<UserInstructions> {
        self.user_instructions.clone()
    }
}

#[cfg(test)]
#[path = "agents_md_manager_tests.rs"]
mod tests;
