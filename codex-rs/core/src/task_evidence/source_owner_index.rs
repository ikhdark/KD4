//! Runtime projection of the repository's source-owner manifest.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use serde::Deserialize;
use tokio::io::AsyncReadExt;
use tracing::warn;

use super::TrustedFileToken;
use super::canonical_repair_path;
use super::trusted_file_token;

const SOURCE_OWNER_MANIFEST_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Deserialize)]
struct SourceOwnerManifest {
    schema_version: u32,
    #[serde(default)]
    owners: Vec<SourceOwnerDeclaration>,
}

#[derive(Debug, Deserialize)]
struct SourceOwnerDeclaration {
    id: String,
    #[serde(default)]
    roots: Vec<String>,
}

#[derive(Debug, Clone)]
pub(super) struct SourceOwnerIndex {
    roots: Vec<SourceOwnerRoot>,
}

#[derive(Debug, Clone)]
pub(super) struct SourceOwnerIndexSnapshot {
    token: Option<TrustedFileToken>,
    index: Option<Arc<SourceOwnerIndex>>,
}

#[derive(Debug, Clone)]
struct SourceOwnerRoot {
    owner_id: String,
    root: String,
}

#[cfg(test)]
pub(super) async fn load_source_owner_index(repo_root: &Path) -> Option<SourceOwnerIndex> {
    SourceOwnerIndexSnapshot::load(repo_root)
        .await
        .index
        .as_deref()
        .cloned()
}

impl SourceOwnerIndexSnapshot {
    pub(super) async fn load(repo_root: &Path) -> Self {
        load_source_owner_index_snapshot(repo_root, None).await
    }

    #[cfg(test)]
    pub(super) async fn refresh(&mut self, repo_root: &Path) -> Option<Arc<SourceOwnerIndex>> {
        let refreshed = load_source_owner_index_snapshot(repo_root, Some(self)).await;
        *self = refreshed;
        self.index.clone()
    }

    pub(super) async fn refreshed(&self, repo_root: &Path) -> Self {
        load_source_owner_index_snapshot(repo_root, Some(self)).await
    }

    pub(super) fn install_if_unchanged(
        &mut self,
        previous: &Self,
        refreshed: Self,
    ) -> Option<Arc<SourceOwnerIndex>> {
        if self.token == previous.token {
            *self = refreshed;
        }
        self.index.clone()
    }

    pub(super) fn index(&self) -> Option<&SourceOwnerIndex> {
        self.index.as_deref()
    }

    #[cfg(test)]
    pub(super) fn shared_index(&self) -> Option<Arc<SourceOwnerIndex>> {
        self.index.clone()
    }
}

async fn load_source_owner_index_snapshot(
    repo_root: &Path,
    cached: Option<&SourceOwnerIndexSnapshot>,
) -> SourceOwnerIndexSnapshot {
    let path = repo_root.join("source_owners.toml");
    let mut file = match tokio::fs::File::open(&path).await {
        Ok(file) => file,
        Err(err) => {
            warn!(
                "source-owner derivation is unavailable because {} could not be read: {err}",
                path.display()
            );
            return SourceOwnerIndexSnapshot {
                token: None,
                index: None,
            };
        }
    };
    let token = trusted_file_token(&file).await;
    if let Some(cached) = cached
        && cached.token == token
    {
        return cached.clone();
    }
    let mut contents = String::new();
    if let Err(err) = file.read_to_string(&mut contents).await {
        warn!(
            "source-owner derivation is unavailable because {} could not be read: {err}",
            path.display()
        );
        return SourceOwnerIndexSnapshot { token, index: None };
    }
    let manifest = match toml::from_str::<SourceOwnerManifest>(&contents) {
        Ok(manifest) => manifest,
        Err(err) => {
            warn!(
                "source-owner derivation is unavailable because {} is invalid: {err}",
                path.display()
            );
            return SourceOwnerIndexSnapshot { token, index: None };
        }
    };
    if manifest.schema_version != SOURCE_OWNER_MANIFEST_SCHEMA_VERSION {
        warn!(
            "source-owner derivation is unavailable because {} uses unsupported schema version {}",
            path.display(),
            manifest.schema_version
        );
        return SourceOwnerIndexSnapshot { token, index: None };
    }

    let mut owner_ids = BTreeSet::new();
    let mut roots = Vec::new();
    for owner in manifest.owners {
        if owner.id.trim().is_empty() || !owner_ids.insert(owner.id.clone()) {
            warn!(
                "source-owner derivation is unavailable because {} contains an empty or duplicate owner id",
                path.display()
            );
            return SourceOwnerIndexSnapshot { token, index: None };
        }
        for root in owner.roots {
            let Ok(root) = canonical_repair_path(&root, false) else {
                warn!(
                    "source-owner derivation is unavailable because {} contains an unsafe owner root",
                    path.display()
                );
                return SourceOwnerIndexSnapshot { token, index: None };
            };
            roots.push(SourceOwnerRoot {
                owner_id: owner.id.clone(),
                root,
            });
        }
    }
    SourceOwnerIndexSnapshot {
        token,
        index: Some(Arc::new(SourceOwnerIndex { roots })),
    }
}

impl SourceOwnerIndex {
    pub(super) fn derive(&self, implementation_surfaces: &[String]) -> Option<String> {
        if implementation_surfaces.is_empty() {
            return None;
        }
        let mut derived_owner = None;
        for surface in implementation_surfaces {
            let surface = canonical_repair_path(surface, false).ok()?;
            let mut best_specificity = None;
            let mut best_owners = BTreeSet::new();
            for candidate in &self.roots {
                if surface == candidate.root
                    || surface
                        .strip_prefix(&candidate.root)
                        .is_some_and(|suffix| suffix.starts_with('/'))
                {
                    let specificity = candidate.root.len();
                    match best_specificity {
                        None => {
                            best_specificity = Some(specificity);
                            best_owners.insert(candidate.owner_id.as_str());
                        }
                        Some(best) if specificity > best => {
                            best_specificity = Some(specificity);
                            best_owners.clear();
                            best_owners.insert(candidate.owner_id.as_str());
                        }
                        Some(best) if specificity == best => {
                            best_owners.insert(candidate.owner_id.as_str());
                        }
                        Some(_) => {}
                    }
                }
            }
            if best_owners.len() != 1 {
                return None;
            }
            let owner = (*best_owners.first()?).to_string();
            if derived_owner
                .as_ref()
                .is_some_and(|derived: &String| derived != &owner)
            {
                return None;
            }
            derived_owner = Some(owner);
        }
        derived_owner
    }
}
