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
use futures::StreamExt;
use sha2::Digest;
use sha2::Sha256;
use std::io;
use toml::Value as TomlValue;
use tracing::error;

/// Default filename scanned for AGENTS.md instructions.
pub const DEFAULT_AGENTS_MD_FILENAME: &str = "AGENTS.md";
/// Preferred local override for AGENTS.md instructions.
pub const LOCAL_AGENTS_MD_FILENAME: &str = "AGENTS.override.md";

/// When both user and project AGENTS.md docs are present, they will be
/// concatenated with the following separator.
const AGENTS_MD_SEPARATOR: &str = "\n\n--- project-doc ---\n\n";
const MAX_CONCURRENT_ENVIRONMENT_LOADS: usize = 4;
const MAX_CONCURRENT_DIRECTORY_SEARCHES: usize = 8;
const MAX_UTF8_BOUNDARY_LOOKAHEAD_BYTES: usize = 3;

pub(crate) struct ProjectInstructionsLoad {
    pub(crate) loaded: Option<LoadedAgentsMd>,
    pub(crate) complete: bool,
}

struct EnvironmentProjectInstructions {
    loaded: Option<LoadedAgentsMd>,
    retained_bytes: usize,
}

struct LoadedProjectDoc {
    candidate: ProjectDocCandidate,
    read: ProjectDocRead,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProjectDocCandidate {
    path: PathUri,
    size: u64,
}

struct ProjectDocRead {
    retained_data: Vec<u8>,
    original_bytes: u64,
    utf8_boundary_truncation: Option<usize>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RetainedUtf8Boundary {
    CompleteOrInvalid,
    NeedsMore(usize),
    ValidSplit(usize),
}

/// Loads project AGENTS.md content and combines it with host-provided user
/// instructions.
pub(crate) async fn load_project_instructions(
    config: &Config,
    user_instructions: Option<UserInstructions>,
    environments: &TurnEnvironmentSnapshot,
) -> ProjectInstructionsLoad {
    let mut loaded = LoadedAgentsMd::from_user_instructions(user_instructions);
    let mut remaining = config.project_doc_max_bytes;
    let mut complete = true;
    if remaining == 0 {
        return ProjectInstructionsLoad {
            loaded: (!loaded.is_empty()).then_some(loaded),
            complete,
        };
    }

    let max_total = config.project_doc_max_bytes;
    let mut environment_loads = Vec::with_capacity(environments.turn_environments.len());
    for (environment_index, turn_environment) in environments.turn_environments.iter().enumerate() {
        let environment_id = turn_environment.environment_id.clone();
        let environment = turn_environment.environment.clone();
        let cwd = turn_environment.cwd().clone();
        let prefetch_utf8_boundary_slack = environment_index > 0;
        environment_loads.push(async move {
            let filesystem = environment.get_filesystem();
            let result = async {
                let candidates = agents_md_paths(config, &cwd, filesystem.as_ref()).await?;
                read_discovered_project_docs(
                    filesystem.as_ref(),
                    candidates,
                    max_total,
                    prefetch_utf8_boundary_slack,
                )
                .await
            }
            .await;
            (environment_id, cwd, result)
        });
    }
    // Independent environment discovery and file reads overlap, while `buffered` preserves
    // selection order for the aggregate-budget allocation below.
    let mut environment_loads =
        futures::stream::iter(environment_loads).buffered(MAX_CONCURRENT_ENVIRONMENT_LOADS);
    while let Some((environment_id, cwd, result)) = environment_loads.next().await {
        match result {
            Ok(project_docs) => {
                let environment_load =
                    render_project_docs(&environment_id, &cwd, project_docs, remaining);
                remaining = remaining.saturating_sub(environment_load.retained_bytes);
                if let Some(docs) = environment_load.loaded {
                    loaded.entries.extend(docs.entries);
                }
            }
            Err(err) => {
                complete = false;
                error!(
                    environment_id,
                    "error trying to find AGENTS.md docs: {err:#}"
                );
            }
        }
    }

    ProjectInstructionsLoad {
        loaded: (!loaded.is_empty()).then_some(loaded),
        complete,
    }
}

/// Attempt to locate and load AGENTS.md documentation.
///
/// On success returns `Ok(Some(loaded))` where `loaded` contains every
/// discovered doc. If no documentation file is found the function returns
/// `Ok(None)`. Unexpected I/O failures bubble up as `Err` so callers can
/// decide how to handle them.
#[cfg(test)]
async fn read_agents_md(
    config: &Config,
    fs: &dyn ExecutorFileSystem,
    environment_id: &str,
    cwd: &PathUri,
) -> io::Result<Option<LoadedAgentsMd>> {
    let max_total = config.project_doc_max_bytes;

    if max_total == 0 {
        return Ok(None);
    }

    let paths = agents_md_paths(config, cwd, fs).await?;
    Ok(
        read_discovered_agents_md(fs, environment_id, cwd, paths, max_total)
            .await?
            .loaded,
    )
}

#[cfg(test)]
async fn read_discovered_agents_md(
    fs: &dyn ExecutorFileSystem,
    environment_id: &str,
    cwd: &PathUri,
    paths: Vec<ProjectDocCandidate>,
    max_total: usize,
) -> io::Result<EnvironmentProjectInstructions> {
    let project_docs = read_discovered_project_docs(
        fs, paths, max_total, /*prefetch_utf8_boundary_slack*/ false,
    )
    .await?;
    Ok(render_project_docs(
        environment_id,
        cwd,
        project_docs,
        max_total,
    ))
}

async fn read_discovered_project_docs(
    fs: &dyn ExecutorFileSystem,
    paths: Vec<ProjectDocCandidate>,
    max_total: usize,
    prefetch_utf8_boundary_slack: bool,
) -> io::Result<Vec<LoadedProjectDoc>> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }

    let mut remaining = max_total;
    let mut project_docs = Vec::new();

    // Allocate the byte budget from the nearest scope outward, then restore the
    // root-to-cwd order used when the aggregate environment budget is applied.
    for candidate in paths.into_iter().rev() {
        // Retain up to one UTF-8 boundary's lookahead beyond this environment's current
        // allocation. A smaller aggregate allocation can trim a split code point and pass those
        // bytes to a broader document without requiring a second filesystem read.
        let prefetch_bytes = if prefetch_utf8_boundary_slack {
            remaining
                .saturating_add(MAX_UTF8_BOUNDARY_LOOKAHEAD_BYTES)
                .min(max_total)
        } else {
            remaining
        };
        let Some(mut project_doc) = read_project_doc(fs, &candidate, prefetch_bytes).await? else {
            continue;
        };

        if let Some(valid_up_to) = project_doc.utf8_boundary_truncation {
            project_doc.retained_data.truncate(valid_up_to);
        }
        let retained_bytes = retained_project_doc_bytes(&project_doc.retained_data, remaining);
        project_docs.push(LoadedProjectDoc {
            candidate,
            read: project_doc,
        });
        remaining = remaining.saturating_sub(retained_bytes);
    }
    project_docs.reverse();
    Ok(project_docs)
}

fn render_project_docs(
    environment_id: &str,
    cwd: &PathUri,
    project_docs: Vec<LoadedProjectDoc>,
    max_total: usize,
) -> EnvironmentProjectInstructions {
    let mut remaining = max_total;
    let mut loaded = LoadedAgentsMd::default();
    let mut entries = Vec::new();

    // Reapply the shared budget nearest-first. Each environment was prefetched with at least
    // this much local capacity, so narrowing a retained prefix never requires more I/O.
    for LoadedProjectDoc {
        candidate,
        mut read,
    } in project_docs.into_iter().rev()
    {
        truncate_project_doc_to_budget(&mut read, remaining);
        let retained_bytes = read.retained_data.len();
        let omitted_bytes = read.original_bytes.saturating_sub(retained_bytes as u64);
        let mut text = String::from_utf8_lossy(&read.retained_data).to_string();

        if omitted_bytes > 0 {
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(&project_doc_truncation_notice(
                &candidate.path,
                read.original_bytes,
                retained_bytes,
            ));
            tracing::warn!(
                path = %candidate.path,
                original_bytes = read.original_bytes,
                retained_bytes,
                omitted_bytes,
                "project doc exceeds remaining budget; truncation notice added"
            );
        }

        entries.push(InstructionEntry {
            contents: text,
            provenance: InstructionProvenance::Project {
                source_path: candidate.path,
                environment_id: environment_id.to_string(),
                cwd: cwd.clone(),
            },
        });
        remaining = remaining.saturating_sub(retained_bytes);
    }
    entries.reverse();
    loaded.entries.extend(entries);

    EnvironmentProjectInstructions {
        loaded: (!loaded.is_empty()).then_some(loaded),
        retained_bytes: max_total.saturating_sub(remaining),
    }
}

fn truncate_project_doc_to_budget(project_doc: &mut ProjectDocRead, max_bytes: usize) {
    let retained_bytes = retained_project_doc_bytes(&project_doc.retained_data, max_bytes);
    project_doc.retained_data.truncate(retained_bytes);
}

fn retained_project_doc_bytes(retained_data: &[u8], max_bytes: usize) -> usize {
    if retained_data.len() <= max_bytes {
        return retained_data.len();
    }
    let lookahead_end = max_bytes
        .saturating_add(MAX_UTF8_BOUNDARY_LOOKAHEAD_BYTES)
        .min(retained_data.len());
    match classify_retained_utf8_boundary(
        &retained_data[..max_bytes],
        &retained_data[max_bytes..lookahead_end],
    ) {
        RetainedUtf8Boundary::ValidSplit(valid_up_to) => valid_up_to,
        RetainedUtf8Boundary::CompleteOrInvalid | RetainedUtf8Boundary::NeedsMore(_) => max_bytes,
    }
}

async fn read_project_doc(
    fs: &dyn ExecutorFileSystem,
    candidate: &ProjectDocCandidate,
    max_bytes: usize,
) -> io::Result<Option<ProjectDocRead>> {
    let mut stream = match fs.read_file_stream(&candidate.path, /*sandbox*/ None).await {
        Ok(stream) => stream,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let max_bytes_u64 = u64::try_from(max_bytes).unwrap_or(u64::MAX);
    let mut retained_data = Vec::new();
    let mut observed_bytes = 0_u64;
    let mut pending_utf8 = Vec::with_capacity(4);
    let mut boundary_lookahead = Vec::with_capacity(MAX_UTF8_BOUNDARY_LOOKAHEAD_BYTES);
    let mut utf8_boundary_truncation = None;
    let mut has_non_whitespace = false;
    let mut reached_eof = false;

    loop {
        let Some(chunk) = stream.next().await else {
            reached_eof = true;
            break;
        };
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err),
        };
        observed_bytes = observed_bytes.saturating_add(chunk.len() as u64);

        let retain = max_bytes
            .saturating_sub(retained_data.len())
            .min(chunk.len());
        retained_data.extend_from_slice(&chunk[..retain]);
        let mut retained_utf8_boundary = RetainedUtf8Boundary::CompleteOrInvalid;
        if retained_data.len() == max_bytes {
            // A retained prefix can end up to three bytes short of a complete code point. Trim
            // only when bounded lookahead proves a valid split; invalid bytes stay lossy-visible.
            retained_utf8_boundary =
                classify_retained_utf8_boundary(&retained_data, &boundary_lookahead);
            if let RetainedUtf8Boundary::NeedsMore(needed) = retained_utf8_boundary {
                let available = &chunk[retain..];
                let take = needed.min(available.len());
                boundary_lookahead.extend_from_slice(&available[..take]);
                retained_utf8_boundary =
                    classify_retained_utf8_boundary(&retained_data, &boundary_lookahead);
            }
            utf8_boundary_truncation = match retained_utf8_boundary {
                RetainedUtf8Boundary::ValidSplit(valid_up_to) => Some(valid_up_to),
                RetainedUtf8Boundary::CompleteOrInvalid | RetainedUtf8Boundary::NeedsMore(_) => {
                    None
                }
            };
        }
        if !has_non_whitespace {
            has_non_whitespace = chunk_has_non_whitespace(&mut pending_utf8, &chunk);
        }

        let known_oversized = candidate.size > max_bytes_u64 || observed_bytes > max_bytes_u64;
        if retained_data.len() == max_bytes
            && known_oversized
            && !matches!(retained_utf8_boundary, RetainedUtf8Boundary::NeedsMore(_))
        {
            break;
        }
    }

    if reached_eof && !has_non_whitespace && !pending_utf8.is_empty() {
        // An incomplete UTF-8 sequence is rendered lossily as U+FFFD, which is
        // non-whitespace and therefore makes the document nonempty.
        has_non_whitespace = true;
    }
    if reached_eof && !has_non_whitespace {
        return Ok(None);
    }

    let original_bytes = if reached_eof {
        observed_bytes
    } else {
        candidate.size.max(observed_bytes)
    };
    Ok(Some(ProjectDocRead {
        retained_data,
        original_bytes,
        utf8_boundary_truncation,
    }))
}

fn chunk_has_non_whitespace(pending_utf8: &mut Vec<u8>, chunk: &[u8]) -> bool {
    let mut offset = 0;
    if let Some(&first_byte) = pending_utf8.first() {
        let expected_len = utf8_sequence_len(first_byte);
        let take = expected_len
            .saturating_sub(pending_utf8.len())
            .min(chunk.len());
        pending_utf8.extend_from_slice(&chunk[..take]);
        match std::str::from_utf8(pending_utf8) {
            Ok(text) => {
                if text.chars().any(|ch| !ch.is_whitespace()) {
                    return true;
                }
                pending_utf8.clear();
                offset = take;
            }
            Err(err) if err.error_len().is_some() => return true,
            Err(_) => return false,
        }
    }

    let remaining = &chunk[offset..];
    match std::str::from_utf8(remaining) {
        Ok(text) => text.chars().any(|ch| !ch.is_whitespace()),
        Err(err) => {
            let Ok(valid) = std::str::from_utf8(&remaining[..err.valid_up_to()]) else {
                return true;
            };
            if valid.chars().any(|ch| !ch.is_whitespace()) {
                return true;
            }
            if err.error_len().is_some() {
                return true;
            }
            pending_utf8.extend_from_slice(&remaining[err.valid_up_to()..]);
            false
        }
    }
}

fn utf8_sequence_len(first_byte: u8) -> usize {
    match first_byte {
        0x00..=0x7f => 1,
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => 1,
    }
}

fn classify_retained_utf8_boundary(
    retained_data: &[u8],
    boundary_lookahead: &[u8],
) -> RetainedUtf8Boundary {
    let Err(err) = std::str::from_utf8(retained_data) else {
        return RetainedUtf8Boundary::CompleteOrInvalid;
    };
    if err.error_len().is_some() {
        return RetainedUtf8Boundary::CompleteOrInvalid;
    }

    let valid_up_to = err.valid_up_to();
    let incomplete_suffix = &retained_data[valid_up_to..];
    let Some(&first_byte) = incomplete_suffix.first() else {
        return RetainedUtf8Boundary::CompleteOrInvalid;
    };
    let expected_len = utf8_sequence_len(first_byte);
    let missing = expected_len.saturating_sub(incomplete_suffix.len());
    let lookahead_len = missing.min(boundary_lookahead.len());
    let boundary_len = incomplete_suffix.len() + lookahead_len;
    let mut boundary = [0_u8; 4];
    boundary[..incomplete_suffix.len()].copy_from_slice(incomplete_suffix);
    boundary[incomplete_suffix.len()..boundary_len]
        .copy_from_slice(&boundary_lookahead[..lookahead_len]);

    match std::str::from_utf8(&boundary[..boundary_len]) {
        Ok(_) if boundary_len == expected_len => RetainedUtf8Boundary::ValidSplit(valid_up_to),
        Ok(_) => RetainedUtf8Boundary::NeedsMore(expected_len - boundary_len),
        Err(err) if err.error_len().is_some() => RetainedUtf8Boundary::CompleteOrInvalid,
        Err(_) if boundary_len < expected_len => {
            RetainedUtf8Boundary::NeedsMore(expected_len - boundary_len)
        }
        Err(_) => RetainedUtf8Boundary::CompleteOrInvalid,
    }
}

fn project_doc_truncation_notice(
    source_path: &PathUri,
    original_bytes: u64,
    retained_bytes: usize,
) -> String {
    let omitted_bytes = original_bytes.saturating_sub(retained_bytes as u64);
    format!(
        "[Project documentation truncation notice: source path: {}; original byte count: {original_bytes}; retained byte count: {retained_bytes}; omitted byte count: {omitted_bytes}.]",
        source_path.inferred_native_path_string()
    )
}

/// Discovers AGENTS.md files from the project root to the current working
/// directory, inclusive. Symlinks are allowed.
async fn agents_md_paths(
    config: &Config,
    cwd: &PathUri,
    fs: &dyn ExecutorFileSystem,
) -> io::Result<Vec<ProjectDocCandidate>> {
    let dir = cwd.clone();

    let project_root_markers = effective_project_root_markers(config);
    let project_root = find_nearest_ancestor_with_markers(
        fs,
        &dir,
        project_root_markers,
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
    let candidate_filenames = candidate_filenames(config);
    let directory_searches = search_dirs.into_iter().map(|directory| {
        let candidate_filenames = &candidate_filenames;
        async move {
            for name in candidate_filenames {
                let candidate = directory
                    .join(name)
                    .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
                match fs.get_metadata(&candidate, /*sandbox*/ None).await {
                    Ok(metadata) if metadata.is_file => {
                        return Ok(Some(ProjectDocCandidate {
                            path: candidate,
                            size: metadata.size,
                        }));
                    }
                    Ok(_) => {}
                    Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                    Err(err) => return Err(err),
                }
            }
            Ok(None)
        }
    });
    // Directories can be probed independently. `buffered` keeps results in root-to-cwd order,
    // while each directory still checks override/default/fallback filenames sequentially.
    let mut directory_searches =
        futures::stream::iter(directory_searches).buffered(MAX_CONCURRENT_DIRECTORY_SEARCHES);
    let mut found = Vec::new();
    while let Some(path) = directory_searches.next().await {
        if let Some(path) = path? {
            found.push(path);
        }
    }
    Ok(found)
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
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LoadedAgentsMd {
    /// Host-provided user instructions.
    user_instructions: Option<UserInstructions>,

    /// Ordered instructions and their provenance.
    entries: Vec<InstructionEntry>,
}

impl LoadedAgentsMd {
    /// Creates loaded instructions containing one user-level AGENTS.md entry.
    pub fn new_user(contents: String, path: AbsolutePathBuf) -> Self {
        if contents.trim().is_empty() {
            return Self::default();
        }
        Self {
            user_instructions: Some(UserInstructions {
                text: contents,
                source: path,
            }),
            entries: Vec::new(),
        }
    }

    fn from_user_instructions(user_instructions: Option<UserInstructions>) -> Self {
        Self {
            user_instructions: user_instructions
                .filter(|instructions| !instructions.text.trim().is_empty()),
            entries: Vec::new(),
        }
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
        Self {
            user_instructions: None,
            entries: vec![InstructionEntry {
                contents,
                provenance: InstructionProvenance::Internal,
            }],
        }
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
        if self.has_multiple_project_environments() {
            self.environment_labeled_text()
        } else {
            self.legacy_text()
        }
    }

    /// Stable digest of the exact model-visible instruction text.
    pub(crate) fn semantic_digest(&self) -> [u8; 32] {
        Sha256::digest(self.text().as_bytes()).into()
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
