use crate::agents_md::AgentsMdEnvironmentDependencies;
use crate::agents_md::LoadedAgentsMd;
use crate::agents_md::effective_project_root_markers;
use crate::agents_md::load_project_instructions_snapshot;
use crate::config::Config;
use crate::environment_selection::TurnEnvironmentSnapshot;
use codex_extension_api::UserInstructions;
use codex_utils_path_uri::PathUri;
use sha2::Digest;
use sha2::Sha256;
use std::io;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Owns the inputs and cached result of AGENTS.md discovery for a session.
pub(crate) struct AgentsMdManager {
    user_instructions: Option<UserInstructions>,
    cache: Mutex<AgentsMdCache>,
    refresh_lock: Mutex<()>,
}

#[derive(Default)]
struct AgentsMdCache {
    key: Option<AgentsMdCacheKey>,
    dependencies: Option<Vec<AgentsMdEnvironmentDependencies>>,
    dependency_digest: Option<[u8; 32]>,
    loaded: Option<Arc<LoadedAgentsMd>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AgentsMdCacheKey {
    environment_generation: u64,
    environments: Vec<AgentsMdEnvironmentKey>,
    starting: Vec<(String, PathUri)>,
    project_doc_max_bytes: usize,
    project_doc_fallback_filenames: Vec<String>,
    project_root_markers: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AgentsMdEnvironmentKey {
    environment_id: String,
    cwd: PathUri,
    filesystem_identity: usize,
}

impl AgentsMdCacheKey {
    fn capture(config: &Config, environments: &TurnEnvironmentSnapshot) -> Self {
        Self {
            environment_generation: environments.generation,
            environments: environments
                .turn_environments
                .iter()
                .map(|environment| {
                    let filesystem = environment.environment.get_filesystem();
                    AgentsMdEnvironmentKey {
                        environment_id: environment.environment_id.clone(),
                        cwd: environment.cwd().clone(),
                        filesystem_identity: Arc::as_ptr(&filesystem) as *const () as usize,
                    }
                })
                .collect(),
            starting: environments
                .starting
                .iter()
                .map(|environment| {
                    (
                        environment.selection.environment_id.clone(),
                        environment.selection.cwd.clone(),
                    )
                })
                .collect(),
            project_doc_max_bytes: config.project_doc_max_bytes,
            project_doc_fallback_filenames: config.project_doc_fallback_filenames.clone(),
            project_root_markers: effective_project_root_markers(config),
        }
    }
}

impl AgentsMdManager {
    pub(crate) fn new(user_instructions: Option<UserInstructions>) -> Self {
        Self {
            user_instructions: user_instructions
                .filter(|instructions| !instructions.text.trim().is_empty()),
            cache: Mutex::new(AgentsMdCache::default()),
            refresh_lock: Mutex::new(()),
        }
    }

    pub(crate) async fn refresh(&self, config: &Config, environments: &TurnEnvironmentSnapshot) {
        let _refresh_guard = self.refresh_lock.lock().await;
        let key = AgentsMdCacheKey::capture(config, environments);
        let cached_dependencies = {
            let cache = self.cache.lock().await;
            if cache.key.as_ref() == Some(&key) {
                cache
                    .dependencies
                    .clone()
                    .zip(cache.dependency_digest)
            } else {
                None
            }
        };
        if let Some((dependencies, expected_digest)) = cached_dependencies
            && capture_dependency_digest(&dependencies).await == Some(expected_digest)
        {
            return;
        }

        let snapshot = load_project_instructions_snapshot(
            config,
            self.user_instructions.clone(),
            environments,
        )
        .await;
        let dependency_digest = match snapshot.dependencies.as_ref() {
            Some(dependencies) => capture_dependency_digest(dependencies).await,
            None => None,
        };
        let mut cache = self.cache.lock().await;
        cache.key = Some(key);
        cache.dependencies = snapshot.dependencies;
        cache.dependency_digest = dependency_digest;
        cache.loaded = snapshot.loaded.map(Arc::new);
    }

    pub(crate) async fn get_loaded(&self) -> Option<Arc<LoadedAgentsMd>> {
        self.cache.lock().await.loaded.clone()
    }

    pub(crate) fn user_instructions(&self) -> Option<UserInstructions> {
        self.user_instructions.clone()
    }
}

async fn capture_dependency_digest(
    dependencies: &[AgentsMdEnvironmentDependencies],
) -> Option<[u8; 32]> {
    let mut hasher = Sha256::new();
    for environment in dependencies {
        let filesystem_identity =
            Arc::as_ptr(&environment.filesystem) as *const () as usize;
        hasher.update(filesystem_identity.to_ne_bytes());
        for dependency in &environment.specs {
            hasher.update(dependency.path.to_string().as_bytes());
            hasher.update([u8::from(dependency.hash_contents)]);
            match environment
                .filesystem
                .get_metadata(&dependency.path, /*sandbox*/ None)
                .await
            {
                Ok(metadata) if metadata.is_file => {
                    hasher.update([1]);
                    if dependency.hash_contents {
                        let contents = environment
                            .filesystem
                            .read_file(&dependency.path, /*sandbox*/ None)
                            .await
                            .ok()?;
                        hasher.update(Sha256::digest(contents));
                    }
                }
                Ok(metadata) if metadata.is_directory => hasher.update([2]),
                Ok(metadata) if metadata.is_symlink => hasher.update([3]),
                Ok(_) => hasher.update([4]),
                Err(err) if err.kind() == io::ErrorKind::NotFound => hasher.update([0]),
                Err(_) => return None,
            }
            hasher.update([0xff]);
        }
        hasher.update([0xfe]);
    }
    Some(hasher.finalize().into())
}

#[cfg(test)]
#[path = "agents_md_manager_tests.rs"]
mod tests;
