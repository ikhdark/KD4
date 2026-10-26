use codex_core_skills::SkillLoadOutcome;
use codex_core_skills::SkillMetadata;
use codex_core_skills::HostSkillsSnapshot;
use std::sync::{Arc, Mutex, Weak};

use crate::catalog::SkillAuthority;
use crate::catalog::SkillCatalog;
use crate::catalog::SkillCatalogEntry;
use crate::catalog::SkillPackageId;
use crate::catalog::SkillProviderError;
use crate::catalog::SkillReadResult;
use crate::catalog::SkillResourceId;
use crate::catalog::SkillSourceKind;
use crate::provider::SkillListQuery;
use crate::provider::SkillProvider;
use crate::provider::SkillProviderFuture;
use crate::provider::SkillReadRequest;

const HOST_AUTHORITY_ID: &str = "host";

/// Host-owned skill provider backed by an immutable service snapshot.
///
/// Discovery and caching belong to `SkillsService`; this provider only maps a
/// snapshot into the authority-aware catalog/read contract.
#[derive(Clone, Default)]
pub struct HostSkillProvider {
    catalogs: Arc<Mutex<Vec<(Weak<HostSkillsSnapshot>, Arc<SkillCatalog>)>>>,
}

impl HostSkillProvider {
    pub fn new() -> Self {
        Self::default()
    }
}

impl SkillProvider for HostSkillProvider {
    fn list(&self, query: SkillListQuery) -> SkillProviderFuture<'_, SkillCatalog> {
        Box::pin(async move { self.list_shared(query).await.map(|catalog| catalog.as_ref().clone()) })
    }

    fn list_shared(&self, query: SkillListQuery) -> SkillProviderFuture<'_, Arc<SkillCatalog>> {
        Box::pin(async move {
            let Some(host_snapshot) = query.host_snapshot else {
                return Err(SkillProviderError::new(
                    "host skill provider requires a host skills snapshot",
                ));
            };

            let mut catalogs = self.catalogs.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            catalogs.retain(|(snapshot, _)| snapshot.strong_count() != 0);
            if let Some((_, catalog)) = catalogs.iter().find(|(snapshot, _)|
                snapshot.upgrade().is_some_and(|snapshot| Arc::ptr_eq(&snapshot, &host_snapshot)))
            {
                return Ok(Arc::clone(catalog));
            }
            let catalog = Arc::new(catalog_from_outcome(host_snapshot.outcome()));
            catalogs.push((Arc::downgrade(&host_snapshot), Arc::clone(&catalog)));
            Ok(catalog)
        })
    }

    fn read(&self, request: SkillReadRequest) -> SkillProviderFuture<'_, SkillReadResult> {
        Box::pin(async move {
            if request.authority != SkillAuthority::new(SkillSourceKind::Host, HOST_AUTHORITY_ID) {
                return Err(SkillProviderError::new(
                    "host skill provider cannot read this authority",
                ));
            }
            if request.package.0.replace('\\', "/") != request.resource.as_str().replace('\\', "/")
            {
                return Err(SkillProviderError::new(
                    "host skill resource does not match its package",
                ));
            }
            let Some(host_snapshot) = request.host_snapshot else {
                return Err(SkillProviderError::new(
                    "host skill provider requires a host skills snapshot",
                ));
            };
            let Some(skill) = host_snapshot.outcome().skills.iter().find(|skill| {
                let skill_path = skill.path_to_skills_md.to_string_lossy();
                skill_path == request.resource.as_str()
                    || skill_path.replace('\\', "/") == request.resource.as_str()
            }) else {
                return Err(SkillProviderError::new(format!(
                    "host skill resource is not loaded: {}",
                    request.resource.as_str()
                )));
            };

            let contents = host_snapshot.read_skill_text(skill).await.map_err(|err| {
                SkillProviderError::new(format!(
                    "failed to read host skill resource {}: {err}",
                    request.resource.as_str()
                ))
            })?;

            Ok(SkillReadResult {
                resource: request.resource,
                contents,
            })
        })
    }
}

fn catalog_from_outcome(outcome: &SkillLoadOutcome) -> SkillCatalog {
    let mut catalog = SkillCatalog {
        continuation: None,
        entries: Vec::new(),
        warnings: outcome
            .errors
            .iter()
            .map(|err| {
                format!(
                    "Failed to load skill at {}: {}",
                    err.path.display(),
                    err.message
                )
            })
            .collect(),
    };

    catalog.extend_entries(
        outcome
            .skills_with_enabled()
            .map(|(skill, enabled)| catalog_entry_from_skill(skill, enabled)),
    );

    catalog
}

fn catalog_entry_from_skill(skill: &SkillMetadata, enabled: bool) -> SkillCatalogEntry {
    let skill_path = skill.path_to_skills_md.to_string_lossy().into_owned();
    let display_path = skill_path.replace('\\', "/");
    let mut entry = SkillCatalogEntry::new(
        SkillPackageId(skill_path.clone()),
        SkillAuthority::new(SkillSourceKind::Host, HOST_AUTHORITY_ID),
        skill.name.clone(),
        skill.description.clone(),
        SkillResourceId::new(skill_path),
    )
    .with_short_description(skill.short_description.clone())
    .with_display_path(display_path)
    .with_dependencies(skill.dependencies.clone())
    .with_source_scope(skill.scope);

    entry.selection_paths = skill
        .discovery_paths
        .iter()
        .flat_map(|path| {
            let path = path.to_string_lossy().into_owned();
            // Accept both native paths and the slash form used by skill links.
            let normalized = path.replace('\\', "/");
            let normalized = (normalized != path).then_some(normalized);
            std::iter::once(path).chain(normalized)
        })
        .collect();
    if !enabled {
        entry = entry.disabled();
    }
    if !skill.allows_implicit_invocation() {
        entry = entry.hidden_from_prompt();
    }

    entry
}
