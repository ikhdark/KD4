use std::collections::HashSet;

use codex_core_skills::model::SkillDependencies;
use codex_protocol::protocol::SkillScope;
use codex_utils_path_uri::PathUri;

/// Source authority that owns a skill package and must be used to read it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum SkillSourceKind {
    /// Codex-hosted skills, including bundled, user, repo, plugin-installed,
    /// and downloaded/materialized remote skills.
    Host,
    /// Skills owned by an execution environment.
    Executor,
    /// Skills owned by the orchestrator rather than an execution environment.
    Orchestrator,
    /// Extension-private source kind for future providers that do not fit an
    /// existing transport category.
    Custom(String),
}

impl SkillSourceKind {
    pub fn custom(kind: impl Into<String>) -> Self {
        Self::Custom(kind.into())
    }

    fn as_str(&self) -> &str {
        match self {
            Self::Host => "host",
            Self::Executor => "executor",
            Self::Orchestrator => "orchestrator",
            Self::Custom(kind) => kind,
        }
    }
}

impl std::fmt::Display for SkillSourceKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.as_str().fmt(formatter)
    }
}

/// Opaque authority identity for list/read routing.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SkillAuthority {
    pub kind: SkillSourceKind,
    pub id: String,
}

impl SkillAuthority {
    pub fn new(kind: SkillSourceKind, id: impl Into<String>) -> Self {
        Self {
            kind,
            id: id.into(),
        }
    }
}

/// Opaque package id. Callers should not parse local paths out of this value.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SkillPackageId(pub String);

/// Opaque resource id inside a skill package, optionally bound to the
/// environment path that owns its contents.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SkillResourceId {
    id: String,
    environment_path: Option<EnvironmentSkillResource>,
}

impl SkillResourceId {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            environment_path: None,
        }
    }

    pub fn environment(
        id: impl Into<String>,
        environment_id: impl Into<String>,
        path: PathUri,
    ) -> Self {
        Self {
            id: id.into(),
            environment_path: Some(EnvironmentSkillResource {
                environment_id: environment_id.into(),
                path,
            }),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.id
    }

    pub(crate) fn environment_path(&self) -> Option<(&str, &PathUri)> {
        self.environment_path
            .as_ref()
            .map(|resource| (resource.environment_id.as_str(), &resource.path))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct EnvironmentSkillResource {
    environment_id: String,
    path: PathUri,
}

/// Metadata shown in the always-visible skills catalog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillCatalogEntry {
    pub id: SkillPackageId,
    pub authority: SkillAuthority,
    pub name: String,
    pub description: String,
    pub short_description: Option<String>,
    pub main_prompt: SkillResourceId,
    pub display_path: Option<String>,
    pub dependencies: Option<SkillDependencies>,
    pub enabled: bool,
    pub prompt_visible: bool,
    pub source_scope: Option<SkillScope>,
}

impl SkillCatalogEntry {
    pub fn new(
        id: SkillPackageId,
        authority: SkillAuthority,
        name: impl Into<String>,
        description: impl Into<String>,
        main_prompt: SkillResourceId,
    ) -> Self {
        Self {
            id,
            authority,
            name: name.into(),
            description: description.into(),
            short_description: None,
            main_prompt,
            display_path: None,
            dependencies: None,
            enabled: true,
            prompt_visible: true,
            source_scope: None,
        }
    }

    pub fn with_short_description(mut self, short_description: Option<String>) -> Self {
        self.short_description = short_description;
        self
    }

    pub fn with_display_path(mut self, display_path: impl Into<String>) -> Self {
        self.display_path = Some(display_path.into());
        self
    }

    pub fn with_dependencies(mut self, dependencies: Option<SkillDependencies>) -> Self {
        self.dependencies = dependencies;
        self
    }

    pub fn with_source_scope(mut self, source_scope: SkillScope) -> Self {
        self.source_scope = Some(source_scope);
        self
    }

    pub fn disabled(mut self) -> Self {
        self.enabled = false;
        self
    }

    pub fn hidden_from_prompt(mut self) -> Self {
        self.prompt_visible = false;
        self
    }

    pub(crate) fn rendered_path(&self) -> &str {
        self.display_path
            .as_deref()
            .unwrap_or_else(|| self.main_prompt.as_str())
    }
}

/// Merged catalog for one turn.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SkillCatalog {
    /// Work remaining in bounded orchestrator discovery, distinct from warnings.
    pub continuation: Option<SkillDiscoveryContinuation>,
    pub entries: Vec<SkillCatalogEntry>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SkillDiscoveryContinuation {
    pub cursor: Option<String>,
    pub resource_offset: usize,
    pub seen_cursors: HashSet<String>,
}

impl SkillCatalog {
    pub fn extend(&mut self, other: SkillCatalog) {
        self.extend_entries(other.entries);
        self.warnings.extend(other.warnings);
        if other.continuation.is_some() {
            self.continuation = other.continuation;
        }
    }

    /// Append a batch in order, retaining the first entry for each authority/package.
    pub fn extend_entries(&mut self, entries: impl IntoIterator<Item = SkillCatalogEntry>) {
        let mut seen: HashSet<_> = self
            .entries
            .iter()
            .map(|entry| (entry.authority.clone(), entry.id.clone()))
            .collect();
        self.entries.extend(
            entries
                .into_iter()
                .filter(|entry| seen.insert((entry.authority.clone(), entry.id.clone()))),
        );
    }

    pub fn push_entry(&mut self, entry: SkillCatalogEntry) {
        if self
            .entries
            .iter()
            .any(|existing| existing.authority == entry.authority && existing.id == entry.id)
        {
            return;
        }

        self.entries.push(entry);
    }
}

/// Contents returned after resolving a skill resource through its owner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillReadResult {
    pub resource: SkillResourceId,
    pub contents: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillProviderError {
    pub message: String,
    pub kind: SkillProviderErrorKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkillProviderErrorKind {
    Unavailable,
    Timeout,
    InvalidResource,
    OversizedContent,
    InvalidResponse,
    Transport,
}

impl SkillProviderError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: SkillProviderErrorKind::Unavailable,
        }
    }

    pub fn with_kind(mut self, kind: SkillProviderErrorKind) -> Self {
        self.kind = kind;
        self
    }

    /// Fixed messages keep private provider errors out of model-visible context.
    pub(crate) fn model_message(&self) -> &'static str {
        match self.kind {
            SkillProviderErrorKind::Timeout => {
                "Skill read timed out. Retry if these instructions are still required."
            }
            SkillProviderErrorKind::Unavailable => {
                "Skill instructions are unavailable from this provider. Check availability before retrying; the instructions have not been loaded."
            }
            SkillProviderErrorKind::InvalidResource => {
                "Invalid skill resource or package. Use the exact authority, package, and resource handles from skills.list."
            }
            SkillProviderErrorKind::OversizedContent => {
                "Skill resource exceeds the provider's 1 MiB read limit. Output pagination cannot fix this; the provider must supply a smaller resource."
            }
            SkillProviderErrorKind::InvalidResponse => {
                "Skill provider returned an invalid resource response. The instructions have not been loaded; the provider must repair the response."
            }
            SkillProviderErrorKind::Transport => {
                "Skill provider could not be reached. No instructions were loaded; check availability before retrying."
            }
        }
    }
}

impl std::fmt::Display for SkillProviderError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.message.fmt(formatter)
    }
}

impl std::error::Error for SkillProviderError {}

pub type SkillProviderResult<T> = Result<T, SkillProviderError>;
