//! AGENTS.md discovery and user instruction assembly.
//!
//! Project-level documentation is primarily stored in files named `AGENTS.md`.
//! Additional fallback filenames can be configured via `project_doc_fallback_filenames`.
//! We include the concatenation of all files found along the path from the
//! project root to the current working directory as follows:
//!
//! 1.  Determine the project root by walking upwards from the current working
//!     directory until a configured `project_root_markers` entry is found.
//!     When `project_root_markers` is unset, the default marker list is used
//!     (`.git`). If no marker is found, only the current working directory is
//!     considered. An empty marker list disables parent traversal.
//! 2.  Collect every `AGENTS.md` found from the project root down to the
//!     current working directory (inclusive) and concatenate their contents in
//!     that order.
//! 3.  We do **not** walk past the project root.

use crate::config::Config;
use crate::context::UserInstructions as ContextUserInstructions;
use crate::environment_selection::TurnEnvironmentSnapshot;
use codex_config::ConfigLayerSource;
use codex_config::ConfigLayerStackOrdering;
use codex_config::default_project_root_markers;
use codex_config::merge_toml_values;
use codex_config::project_root_markers_from_config;
use codex_exec_server::ExecutorFileSystem;
use codex_extension_api::UserInstructions;
use codex_file_system::FindUpErrorPolicy;
use codex_file_system::find_nearest_ancestor_with_markers;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use sha2::Digest;
use sha2::Sha256;
use std::io;
use std::sync::Arc;
use toml::Value as TomlValue;
use tracing::error;

/// Default filename scanned for AGENTS.md instructions.
pub const DEFAULT_AGENTS_MD_FILENAME: &str = "AGENTS.md";
/// Preferred local override for AGENTS.md instructions.
pub const LOCAL_AGENTS_MD_FILENAME: &str = "AGENTS.override.md";

/// When both user and project AGENTS.md docs are present, they will be
/// concatenated with the following separator.
const AGENTS_MD_SEPARATOR: &str = "\n\n--- project-doc ---\n\n";
const MAX_AGENTS_MD_DEPENDENCIES: usize = 4_096;

pub(crate) struct ProjectInstructionsSnapshot {
    pub(crate) loaded: Option<LoadedAgentsMd>,
    pub(crate) dependencies: Option<Vec<AgentsMdEnvironmentDependencies>>,
}

pub(crate) struct AgentsMdEnvironmentDependencies {
    pub(crate) filesystem: Arc<dyn ExecutorFileSystem>,
    pub(crate) specs: Vec<AgentsMdDependencySpec>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AgentsMdDependencySpec {
    pub(crate) path: PathUri,
    pub(crate) hash_contents: bool,
}

/// Loads project AGENTS.md content and combines it with host-provided user
/// instructions.
pub(crate) async fn load_project_instructions(
    config: &Config,
    user_instructions: Option<UserInstructions>,
    environments: &TurnEnvironmentSnapshot,
) -> Option<LoadedAgentsMd> {
    load_project_instructions_snapshot(config, user_instructions, environments)
        .await
        .loaded
}

pub(crate) async fn load_project_instructions_snapshot(
    config: &Config,
    user_instructions: Option<UserInstructions>,
    environments: &TurnEnvironmentSnapshot,
) -> ProjectInstructionsSnapshot {
    let mut loaded = LoadedAgentsMd::from_user_instructions(user_instructions);
    let mut dependencies = Some(Vec::with_capacity(environments.turn_environments.len()));
    for turn_environment in &environments.turn_environments {
        let filesystem = turn_environment.environment.get_filesystem();
        match read_agents_md_snapshot(
            config,
            filesystem.as_ref(),
            &turn_environment.environment_id,
            turn_environment.cwd(),
        )
        .await
        {
            Ok(snapshot) => {
                if let Some(docs) = snapshot.loaded {
                    loaded.entries.extend(docs.entries);
                }
                match (&mut dependencies, snapshot.dependencies) {
                    (Some(all), Some(specs)) => all.push(AgentsMdEnvironmentDependencies {
                        filesystem,
                        specs,
                    }),
                    (dependencies, None) => *dependencies = None,
                    (None, _) => {}
                }
            }
            Err(e) => {
                dependencies = None;
                error!(
                    environment_id = turn_environment.environment_id,
                    "error trying to find AGENTS.md docs: {e:#}"
                );
            }
        }
    }

    loaded.recompose();
    ProjectInstructionsSnapshot {
        loaded: (!loaded.is_empty()).then_some(loaded),
        dependencies,
    }
}

struct AgentsMdReadSnapshot {
    loaded: Option<LoadedAgentsMd>,
    dependencies: Option<Vec<AgentsMdDependencySpec>>,
}

/// Attempt to locate and load AGENTS.md documentation.
///
/// On success returns `Ok(Some(loaded))` where `loaded` contains every
/// discovered doc. If no documentation file is found the function returns
/// `Ok(None)`. Unexpected I/O failures bubble up as `Err` so callers can
/// decide how to handle them.
async fn read_agents_md(
    config: &Config,
    fs: &dyn ExecutorFileSystem,
    environment_id: &str,
    cwd: &PathUri,
) -> io::Result<Option<LoadedAgentsMd>> {
    Ok(read_agents_md_snapshot(config, fs, environment_id, cwd)
        .await?
        .loaded)
}

async fn read_agents_md_snapshot(
    config: &Config,
    fs: &dyn ExecutorFileSystem,
    environment_id: &str,
    cwd: &PathUri,
) -> io::Result<AgentsMdReadSnapshot> {
    let max_total = config.project_doc_max_bytes;

    if max_total == 0 {
        return Ok(AgentsMdReadSnapshot {
            loaded: None,
            dependencies: Some(Vec::new()),
        });
    }

    let discovery = discover_agents_md_paths(config, cwd, fs).await?;

    let mut remaining: u64 = max_total as u64;
    let mut loaded = LoadedAgentsMd::default();

    for p in discovery.paths {
        if remaining == 0 {
            break;
        }

        let mut data = match fs.read_file(&p, /*sandbox*/ None).await {
            Ok(data) => data,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err),
        };
        let size = data.len() as u64;
        if size > remaining {
            data.truncate(remaining as usize);
        }

        if size > remaining {
            tracing::warn!(
                path = %p,
                remaining_bytes = remaining,
                "project doc exceeds remaining budget; truncating"
            );
        }

        let text = String::from_utf8_lossy(&data).to_string();
        if !text.trim().is_empty() {
            loaded.entries.push(InstructionEntry {
                contents: text,
                provenance: InstructionProvenance::Project {
                    source_path: p,
                    environment_id: environment_id.to_string(),
                    cwd: cwd.clone(),
                },
            });
            remaining = remaining.saturating_sub(data.len() as u64);
        }
    }

    loaded.recompose();
    Ok(AgentsMdReadSnapshot {
        loaded: (!loaded.is_empty()).then_some(loaded),
        dependencies: discovery.dependencies,
    })
}

struct AgentsMdPathDiscovery {
    paths: Vec<PathUri>,
    dependencies: Option<Vec<AgentsMdDependencySpec>>,
}

/// Discovers AGENTS.md files from the project root to the current working
/// directory, inclusive. Symlinks are allowed.
async fn agents_md_paths(
    config: &Config,
    cwd: &PathUri,
    fs: &dyn ExecutorFileSystem,
) -> io::Result<Vec<PathUri>> {
    Ok(discover_agents_md_paths(config, cwd, fs).await?.paths)
}

async fn discover_agents_md_paths(
    config: &Config,
    cwd: &PathUri,
    fs: &dyn ExecutorFileSystem,
) -> io::Result<AgentsMdPathDiscovery> {
    let dir = cwd.clone();
    let project_root_markers = effective_project_root_markers(config);
    let project_root = find_nearest_ancestor_with_markers(
        fs,
        &dir,
        project_root_markers.clone(),
        FindUpErrorPolicy::Propagate,
        /*sandbox*/ None,
    )
    .await?;
    let search_dirs = if let Some(root) = project_root {
        let mut dirs = Vec::new();
        let mut cursor = dir.clone();
        loop {
            dirs.push(cursor.clone());
            if cursor == root {
                break;
            }
            let Some(parent) = cursor.parent() else {
                break;
            };
            cursor = parent;
        }
        dirs.reverse();
        dirs
    } else {
        vec![dir]
    };

    let mut found = Vec::new();
    let mut dependencies = Vec::new();
    let marker_search_end = project_root.as_ref();
    let mut marker_cursor = Some(dir.clone());
    while let Some(directory) = marker_cursor {
        for marker in &project_root_markers {
            let path = directory
                .join(marker)
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
            dependencies.push(AgentsMdDependencySpec {
                path,
                hash_contents: false,
            });
        }
        if marker_search_end == Some(&directory) {
            break;
        }
        marker_cursor = directory.parent();
    }
    let candidate_filenames = candidate_filenames(config);
    for directory in search_dirs {
        for name in &candidate_filenames {
            let candidate = directory
                .join(name)
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
            dependencies.push(AgentsMdDependencySpec {
                path: candidate.clone(),
                hash_contents: false,
            });
            match fs.get_metadata(&candidate, /*sandbox*/ None).await {
                Ok(metadata) if metadata.is_file => {
                    if let Some(dependency) = dependencies.last_mut() {
                        dependency.hash_contents = true;
                    }
                    found.push(candidate);
                    break;
                }
                Ok(_) => {}
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(err),
            }
        }
    }
    let dependencies = normalize_dependencies(dependencies);
    Ok(AgentsMdPathDiscovery {
        paths: found,
        dependencies,
    })
}

pub(crate) fn effective_project_root_markers(config: &Config) -> Vec<String> {
    let mut merged = TomlValue::Table(toml::map::Map::new());
    for layer in config.config_layer_stack.get_layers(
        ConfigLayerStackOrdering::LowestPrecedenceFirst,
        /*include_disabled*/ false,
    ) {
        if matches!(layer.name, ConfigLayerSource::Project { .. }) {
            continue;
        }
        merge_toml_values(&mut merged, &layer.config);
    }
    match project_root_markers_from_config(&merged) {
        Ok(Some(markers)) => markers,
        Ok(None) => default_project_root_markers(),
        Err(err) => {
            tracing::warn!("invalid project_root_markers: {err}");
            default_project_root_markers()
        }
    }
}

fn normalize_dependencies(
    mut dependencies: Vec<AgentsMdDependencySpec>,
) -> Option<Vec<AgentsMdDependencySpec>> {
    dependencies.sort_by(|left, right| left.path.cmp(&right.path));
    dependencies.dedup_by(|left, right| {
        if left.path != right.path {
            return false;
        }
        right.hash_contents |= left.hash_contents;
        true
    });
    (dependencies.len() <= MAX_AGENTS_MD_DEPENDENCIES).then_some(dependencies)
}

fn candidate_filenames(config: &Config) -> Vec<&str> {
    let mut names: Vec<&str> = Vec::with_capacity(2 + config.project_doc_fallback_filenames.len());
    names.push(LOCAL_AGENTS_MD_FILENAME);
    names.push(DEFAULT_AGENTS_MD_FILENAME);
    for candidate in &config.project_doc_fallback_filenames {
        let candidate = candidate.as_str();
        if candidate.is_empty() {
            continue;
        }
        if !names.contains(&candidate) {
            names.push(candidate);
        }
    }
    names
}

/// Model-visible instructions loaded from AGENTS.md files and internal
/// guidance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadedAgentsMd {
    /// Host-provided user instructions.
    user_instructions: Option<UserInstructions>,

    /// Ordered instructions and their provenance.
    entries: Vec<InstructionEntry>,

    /// Model-visible composed bytes, retained once for the lifetime of this snapshot.
    composed_text: Arc<str>,

    /// SHA-256 of `composed_text`, used by fixed-point planning without serializing it again.
    semantic_digest: [u8; 32],
}

impl Default for LoadedAgentsMd {
    fn default() -> Self {
        Self {
            user_instructions: None,
            entries: Vec::new(),
            composed_text: Arc::from(""),
            semantic_digest: Sha256::digest([]).into(),
        }
    }
}

impl LoadedAgentsMd {
    /// Creates loaded instructions containing one user-level AGENTS.md entry.
    pub fn new_user(contents: String, path: AbsolutePathBuf) -> Self {
        if contents.trim().is_empty() {
            return Self::default();
        }
        let mut loaded = Self {
            user_instructions: Some(UserInstructions {
                text: contents,
                source: path,
            }),
            entries: Vec::new(),
            ..Self::default()
        };
        loaded.recompose();
        loaded
    }

    fn from_user_instructions(user_instructions: Option<UserInstructions>) -> Self {
        let mut loaded = Self {
            user_instructions: user_instructions
                .filter(|instructions| !instructions.text.trim().is_empty()),
            entries: Vec::new(),
            ..Self::default()
        };
        loaded.recompose();
        loaded
    }

    /// Creates source-less user instructions for tests.
    ///
    /// This cannot be gated with `#[cfg(test)]` because integration tests
    /// compile `codex-core` as a normal dependency without that configuration.
    pub fn from_text_for_testing(contents: impl Into<String>) -> Self {
        let contents = contents.into();
        if contents.trim().is_empty() {
            return Self::default();
        }
        let mut loaded = Self {
            user_instructions: None,
            entries: vec![InstructionEntry {
                contents,
                provenance: InstructionProvenance::Internal,
            }],
            ..Self::default()
        };
        loaded.recompose();
        loaded
    }

    fn is_empty(&self) -> bool {
        self.user_instructions.is_none()
            && self
                .entries
                .iter()
                .all(|entry| entry.contents.trim().is_empty())
    }

    /// Returns the concatenated model-visible instruction text.
    pub fn text(&self) -> String {
        self.composed_text.to_string()
    }

    pub(crate) fn semantic_digest(&self) -> &[u8; 32] {
        &self.semantic_digest
    }

    fn recompose(&mut self) {
        let text = self.compose_text();
        self.semantic_digest = Sha256::digest(text.as_bytes()).into();
        self.composed_text = Arc::from(text);
    }

    fn compose_text(&self) -> String {
        if self.has_multiple_project_environments() {
            self.environment_labeled_text()
        } else {
            self.legacy_text()
        }
    }

    fn legacy_text(&self) -> String {
        let mut output = String::new();
        let mut has_previous = false;
        let mut previous_was_project = false;
        if let Some(instructions) = &self.user_instructions {
            output.push_str(&instructions.text);
            has_previous = true;
        }
        for entry in &self.entries {
            let is_project = matches!(&entry.provenance, InstructionProvenance::Project { .. });
            if has_previous {
                // The project-doc marker tells the model where workspace-scoped
                // instructions begin, so it is only needed on the transition
                // from user or internal instructions to project instructions.
                let separator = if is_project && !previous_was_project {
                    AGENTS_MD_SEPARATOR
                } else {
                    "\n\n"
                };
                output.push_str(separator);
            }
            output.push_str(&entry.contents);
            has_previous = true;
            previous_was_project = is_project;
        }
        output
    }

    fn environment_labeled_text(&self) -> String {
        let mut output = String::new();
        let mut has_previous = false;
        let mut previous_environment: Option<(&str, &PathUri)> = None;
        if let Some(instructions) = &self.user_instructions {
            output.push_str(&instructions.text);
            has_previous = true;
        }
        for entry in &self.entries {
            match &entry.provenance {
                InstructionProvenance::Project {
                    environment_id,
                    cwd,
                    ..
                } => {
                    if has_previous {
                        output.push_str("\n\n");
                    }
                    // One environment can contribute several hierarchical AGENTS.md files from
                    // its project root through its cwd. Label that environment once for the
                    // complete group rather than repeating the label before every file.
                    let environment = (environment_id.as_str(), cwd);
                    if previous_environment != Some(environment) {
                        output.push_str(&format!(
                            "for `{}` with root {}\n\n",
                            environment_id,
                            cwd.inferred_native_path_string()
                        ));
                    }
                    output.push_str(&entry.contents);
                    previous_environment = Some(environment);
                }
                InstructionProvenance::Internal => {
                    if has_previous {
                        output.push_str("\n\n");
                    }
                    output.push_str(&entry.contents);
                    previous_environment = None;
                }
            }
            has_previous = true;
        }
        output
    }

    pub(crate) fn contextual_user_fragment(&self) -> ContextUserInstructions {
        // One contributing project environment retains the legacy cwd wrapper. With two or more,
        // the body labels every contributing environment itself, so the outer cwd is omitted.
        let directory = if self.has_multiple_project_environments() {
            None
        } else {
            self.single_project_cwd()
                .map(PathUri::inferred_native_path_string)
        };
        ContextUserInstructions {
            directory,
            text: self.text(),
        }
    }

    /// Returns the AGENTS.md files that supplied instruction entries.
    pub fn sources(&self) -> impl Iterator<Item = PathUri> + '_ {
        self.user_instructions
            .iter()
            .map(|instructions| PathUri::from_abs_path(&instructions.source))
            .chain(
                self.entries
                    .iter()
                    .filter_map(|entry| entry.provenance.path()),
            )
    }

    fn has_multiple_project_environments(&self) -> bool {
        let mut first_environment_id = None;
        self.entries.iter().any(|entry| {
            let InstructionProvenance::Project { environment_id, .. } = &entry.provenance else {
                return false;
            };
            match first_environment_id {
                Some(first_environment_id) => first_environment_id != environment_id,
                None => {
                    first_environment_id = Some(environment_id);
                    false
                }
            }
        })
    }

    fn single_project_cwd(&self) -> Option<&PathUri> {
        self.entries
            .iter()
            .find_map(|entry| match &entry.provenance {
                InstructionProvenance::Project { cwd, .. } => Some(cwd),
                InstructionProvenance::Internal => None,
            })
    }
}

/// One model-visible instruction and its provenance.
#[derive(Clone, Debug, PartialEq, Eq)]
struct InstructionEntry {
    /// Model-visible instruction text.
    contents: String,

    /// Origin of the instruction.
    provenance: InstructionProvenance,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum InstructionProvenance {
    /// Workspace instructions discovered from project AGENTS.md files.
    Project {
        /// Exact AGENTS.md file, distinct from the environment's selected cwd.
        source_path: PathUri,
        environment_id: String,
        cwd: PathUri,
    },

    /// Instructions without a file source, including internally defined guidance.
    Internal,
}

impl InstructionProvenance {
    fn path(&self) -> Option<PathUri> {
        match self {
            Self::Project { source_path, .. } => Some(source_path.clone()),
            Self::Internal => None,
        }
    }
}

#[cfg(test)]
#[path = "agents_md_tests.rs"]
mod tests;
