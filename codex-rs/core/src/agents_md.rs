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
use codex_otel::MetricsClient;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use futures::StreamExt;
use serde::Deserialize;
use serde::Serialize;
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
const MAX_CONCURRENT_DIRECTORY_SEARCHES: usize = 8;
const MAX_UTF8_BOUNDARY_LOOKAHEAD_BYTES: usize = 3;
/// Project source selection and rendered prompt size are separate contracts. The configured
/// source budget may be fully used, while provenance and truncation reporting receive this
/// additional, finite allowance.
const PROJECT_DOC_RENDERED_OVERHEAD_BYTES: usize = 4 * 1024;
const PROJECT_DOC_AGGREGATE_NOTICE_RESERVE_BYTES: usize = 256;
const PROJECT_DISCOVERY_REUSE_METRIC: &str = "codex.project_discovery_reuse";

fn project_instruction_source_header(source_path: &PathUri) -> String {
    format!(
        "## AGENTS.md instructions from {}\n\n",
        source_path.inferred_native_path_string()
    )
}

enum ProjectDiscoveryReuse {
    Hit(PathUri),
    Miss(&'static str),
}

fn stable_context_identity_from_structure(
    structure: [u8; 32],
    rendered_hash: [u8; 32],
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"codex.repository-context-identity.v1");
    hasher.update(structure);
    hasher.update(rendered_hash);
    hasher.finalize().into()
}

pub(crate) struct ProjectInstructionsLoad {
    pub(crate) loaded: Option<LoadedAgentsMd>,
    pub(crate) complete: bool,
}

/// Freshness of the AGENTS.md observation attached to one sampling request.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AgentsMdFreshness {
    Refreshed,
    IncompleteRead,
    #[default]
    CachedFallback,
}

impl AgentsMdFreshness {
    pub(crate) const fn model_visible_description(self) -> &'static str {
        match self {
            Self::Refreshed => {
                "Result provenance: direct_file_read; freshness: refreshed_for_this_sampling_step."
            }
            Self::IncompleteRead => {
                "Result provenance: direct_file_read; freshness: incomplete_read_may_omit_instructions."
            }
            Self::CachedFallback => {
                "Result provenance: cached_observation; freshness: cached_may_be_stale."
            }
        }
    }
}

struct EnvironmentProjectInstructions {
    loaded: Option<LoadedAgentsMd>,
    retained_source_bytes: usize,
    rendered_bytes: usize,
    omitted_documents: Vec<ProjectDocOmission>,
}

#[derive(Clone, Debug, Serialize)]
struct ProjectDocOmission {
    environment_id: String,
    cwd: String,
    path: String,
    source_bytes: u64,
}

struct LoadedProjectDoc {
    candidate: ProjectDocCandidate,
    read: ProjectDocRead,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProjectDocCandidate {
    path: PathUri,
    size: u64,
    modified_at_ms: i64,
}

pub(crate) struct ProjectInstructionsDiscovery {
    environments: Vec<EnvironmentProjectInstructionsDiscovery>,
    #[cfg(test)]
    config_identity: usize,
}

struct EnvironmentProjectInstructionsDiscovery {
    environment_id: String,
    cwd: PathUri,
    filesystem: Arc<dyn ExecutorFileSystem>,
    result: io::Result<Vec<ProjectDocCandidate>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProjectInstructionsSourceFingerprint(
    Vec<EnvironmentProjectInstructionsFingerprint>,
);

#[derive(Clone, Debug, Eq, PartialEq)]
struct EnvironmentProjectInstructionsFingerprint {
    environment_id: String,
    cwd: PathUri,
    candidates: Vec<ProjectDocCandidate>,
}

impl ProjectInstructionsDiscovery {
    #[cfg(test)]
    pub(crate) fn config_identity(&self) -> usize {
        self.config_identity
    }

    pub(crate) fn source_fingerprint(&self) -> Option<ProjectInstructionsSourceFingerprint> {
        let mut environments = Vec::with_capacity(self.environments.len());
        for discovery in &self.environments {
            let candidates = discovery.result.as_ref().ok()?;
            // Some remote filesystems cannot provide a meaningful mtime. Do not
            // treat size alone as a trustworthy content identity in that case.
            if candidates
                .iter()
                .any(|candidate| candidate.modified_at_ms <= 0)
            {
                return None;
            }
            environments.push(EnvironmentProjectInstructionsFingerprint {
                environment_id: discovery.environment_id.clone(),
                cwd: discovery.cwd.clone(),
                candidates: candidates.clone(),
            });
        }
        Some(ProjectInstructionsSourceFingerprint(environments))
    }
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
#[cfg(test)]
pub(crate) async fn load_project_instructions(
    config: &Config,
    user_instructions: Option<UserInstructions>,
    environments: &TurnEnvironmentSnapshot,
) -> ProjectInstructionsLoad {
    let project_root_markers = effective_project_root_markers(config);
    load_project_instructions_with_markers(
        config,
        user_instructions,
        environments,
        &project_root_markers,
    )
    .await
}

#[cfg(test)]
pub(crate) async fn load_project_instructions_with_markers(
    config: &Config,
    user_instructions: Option<UserInstructions>,
    environments: &TurnEnvironmentSnapshot,
    project_root_markers: &[String],
) -> ProjectInstructionsLoad {
    let discovery = discover_project_instructions_with_markers(
        Arc::new(config.clone()),
        environments,
        project_root_markers,
    )
    .await;
    load_project_instructions_from_discovery(config, user_instructions, discovery).await
}

pub(crate) async fn discover_project_instructions_with_markers(
    config: Arc<Config>,
    environments: &TurnEnvironmentSnapshot,
    project_root_markers: &[String],
) -> ProjectInstructionsDiscovery {
    #[cfg(test)]
    let config_identity = Arc::as_ptr(&config) as usize;
    if config.project_doc_max_bytes == 0 {
        return ProjectInstructionsDiscovery {
            environments: Vec::new(),
            #[cfg(test)]
            config_identity,
        };
    }

    let project_root_markers: Arc<[String]> = Arc::from(project_root_markers.to_vec());
    let environments =
        futures::stream::iter(environments.turn_environments.clone().into_iter().map(
            move |turn_environment| {
                let config = Arc::clone(&config);
                let project_root_markers = Arc::clone(&project_root_markers);
                let environment_id = turn_environment.environment_id.clone();
                let environment = Arc::clone(&turn_environment.environment);
                let cwd = turn_environment.cwd().clone();
                async move {
                    let filesystem = environment.get_filesystem();
                    let result = agents_md_paths_with_markers(
                        config.as_ref(),
                        &cwd,
                        filesystem.as_ref(),
                        !environment.is_remote(),
                        project_root_markers.as_ref(),
                    )
                    .await;
                    EnvironmentProjectInstructionsDiscovery {
                        environment_id,
                        cwd,
                        filesystem,
                        result,
                    }
                }
            },
        ))
        .buffered(MAX_CONCURRENT_DIRECTORY_SEARCHES)
        .collect::<Vec<_>>()
        .await;

    ProjectInstructionsDiscovery {
        environments,
        #[cfg(test)]
        config_identity,
    }
}

pub(crate) async fn load_project_instructions_from_discovery(
    config: &Config,
    user_instructions: Option<UserInstructions>,
    discovery: ProjectInstructionsDiscovery,
) -> ProjectInstructionsLoad {
    let mut loaded = LoadedAgentsMd::from_user_instructions(user_instructions);
    let mut remaining_source_bytes = config.project_doc_max_bytes;
    let mut remaining_rendered_bytes = project_doc_rendered_max_bytes(config.project_doc_max_bytes);
    let aggregate_reserve =
        remaining_rendered_bytes.min(PROJECT_DOC_AGGREGATE_NOTICE_RESERVE_BYTES);
    remaining_rendered_bytes = remaining_rendered_bytes.saturating_sub(aggregate_reserve);
    let mut omitted_documents = Vec::new();
    let mut complete = true;
    if remaining_source_bytes == 0 {
        return ProjectInstructionsLoad {
            loaded: (!loaded.is_empty()).then_some(loaded),
            complete,
        };
    }

    let contributing_environments = discovery
        .environments
        .iter()
        .filter(|discovery| matches!(&discovery.result, Ok(paths) if !paths.is_empty()))
        .count();
    let mut first_project_environment = true;
    for EnvironmentProjectInstructionsDiscovery {
        environment_id,
        cwd,
        filesystem,
        result,
    } in discovery.environments
    {
        match result {
            Ok(candidates) if !candidates.is_empty() => {
                let mut generated_overhead = 0usize;
                if first_project_environment && loaded.user_instructions.is_some() {
                    generated_overhead += AGENTS_MD_SEPARATOR.len();
                }
                if contributing_environments > 1 {
                    if !first_project_environment {
                        generated_overhead += 2;
                    }
                    generated_overhead += format!(
                        "for `{}` with cwd {}\n\n",
                        environment_id,
                        cwd.inferred_native_path_string()
                    )
                    .len();
                }
                if generated_overhead >= remaining_rendered_bytes {
                    omitted_documents.extend(candidates.iter().map(|candidate| {
                        ProjectDocOmission {
                            environment_id: environment_id.clone(),
                            cwd: cwd.inferred_native_path_string(),
                            path: candidate.path.inferred_native_path_string(),
                            source_bytes: candidate.size,
                        }
                    }));
                    remaining_rendered_bytes = 0;
                    first_project_environment = false;
                    continue;
                }
                remaining_rendered_bytes -= generated_overhead;

                let project_docs = match read_discovered_project_docs(
                    filesystem.as_ref(),
                    candidates,
                    remaining_source_bytes,
                    /*prefetch_utf8_boundary_slack*/ false,
                )
                .await
                {
                    Ok(project_docs) => project_docs,
                    Err(err) => {
                        complete = false;
                        error!(
                            environment_id,
                            "error trying to read AGENTS.md docs: {err:#}"
                        );
                        continue;
                    }
                };
                let environment_load = render_project_docs(
                    &environment_id,
                    &cwd,
                    project_docs,
                    remaining_rendered_bytes,
                );
                remaining_source_bytes =
                    remaining_source_bytes.saturating_sub(environment_load.retained_source_bytes);
                remaining_rendered_bytes =
                    remaining_rendered_bytes.saturating_sub(environment_load.rendered_bytes);
                omitted_documents.extend(environment_load.omitted_documents);
                if let Some(docs) = environment_load.loaded {
                    loaded.entries.extend(docs.entries);
                }
                first_project_environment = false;
            }
            Ok(_) => {}
            Err(err) => {
                complete = false;
                error!(
                    environment_id,
                    "error trying to find AGENTS.md docs: {err:#}"
                );
            }
        }
    }

    if !omitted_documents.is_empty() {
        let notice = aggregate_project_doc_omission_notice(&omitted_documents);
        debug_assert!(notice.len().saturating_add(2) <= aggregate_reserve);
        loaded.entries.push(InstructionEntry {
            contents: notice,
            provenance: InstructionProvenance::Internal,
        });
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

    let paths = agents_md_paths(config, cwd, fs, /*reuse_project_discovery*/ false).await?;
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
    let rendered_max = project_doc_rendered_max_bytes(max_total);
    let aggregate_reserve = rendered_max.min(PROJECT_DOC_AGGREGATE_NOTICE_RESERVE_BYTES);
    let mut rendered = render_project_docs(
        environment_id,
        cwd,
        project_docs,
        rendered_max.saturating_sub(aggregate_reserve),
    );
    if !rendered.omitted_documents.is_empty() {
        let notice = aggregate_project_doc_omission_notice(&rendered.omitted_documents);
        debug_assert!(notice.len().saturating_add(2) <= aggregate_reserve);
        let mut loaded = rendered.loaded.take().unwrap_or_default();
        loaded.entries.push(InstructionEntry {
            contents: notice,
            provenance: InstructionProvenance::Internal,
        });
        rendered.loaded = Some(loaded);
        rendered.rendered_bytes = rendered.rendered_bytes.saturating_add(aggregate_reserve);
    }
    Ok(rendered)
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
        project_doc.retained_data.truncate(retained_bytes);
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
    max_rendered_bytes: usize,
) -> EnvironmentProjectInstructions {
    let mut remaining = max_rendered_bytes;
    let mut loaded = LoadedAgentsMd::default();
    let mut entries = Vec::new();
    let mut retained_source_bytes = 0usize;
    let mut omitted_documents = Vec::new();

    // Reapply the shared budget nearest-first. Each environment was prefetched with at least
    // this much local capacity, so narrowing a retained prefix never requires more I/O.
    let mut project_docs = project_docs.into_iter().rev();
    while let Some(LoadedProjectDoc {
        candidate,
        mut read,
    }) = project_docs.next()
    {
        let separator_bytes = usize::from(!entries.is_empty()) * 2;
        let source_header_bytes = project_instruction_source_header(&candidate.path).len();
        let Some((text, retained_bytes)) = render_project_doc_to_budget(
            &mut read,
            &candidate.path,
            remaining.saturating_sub(separator_bytes + source_header_bytes),
        ) else {
            omitted_documents.push(ProjectDocOmission {
                environment_id: environment_id.to_string(),
                cwd: cwd.inferred_native_path_string(),
                path: candidate.path.inferred_native_path_string(),
                source_bytes: read.original_bytes,
            });
            for LoadedProjectDoc { candidate, read } in project_docs {
                omitted_documents.push(ProjectDocOmission {
                    environment_id: environment_id.to_string(),
                    cwd: cwd.inferred_native_path_string(),
                    path: candidate.path.inferred_native_path_string(),
                    source_bytes: read.original_bytes,
                });
            }
            // Do not let a shorter broad-file notice displace this nearer document, or let a
            // later environment consume the capacity that was unavailable to this one.
            remaining = 0;
            break;
        };
        let omitted_bytes = read.original_bytes.saturating_sub(retained_bytes as u64);
        if omitted_bytes > 0 {
            tracing::warn!(
                path = %candidate.path,
                original_bytes = read.original_bytes,
                retained_bytes,
                omitted_bytes,
                "project doc exceeds remaining budget; truncation notice added"
            );
        }

        let rendered_bytes = text.len();
        entries.push(InstructionEntry {
            contents: text,
            provenance: InstructionProvenance::Project {
                source_path: candidate.path,
                environment_id: environment_id.to_string(),
                cwd: cwd.clone(),
            },
        });
        retained_source_bytes = retained_source_bytes.saturating_add(retained_bytes);
        remaining =
            remaining.saturating_sub(rendered_bytes + separator_bytes + source_header_bytes);
    }
    entries.reverse();
    loaded.entries.extend(entries);

    EnvironmentProjectInstructions {
        loaded: (!loaded.is_empty()).then_some(loaded),
        retained_source_bytes,
        rendered_bytes: max_rendered_bytes.saturating_sub(remaining),
        omitted_documents,
    }
}

const fn project_doc_rendered_max_bytes(source_bytes: usize) -> usize {
    source_bytes.saturating_add(PROJECT_DOC_RENDERED_OVERHEAD_BYTES)
}

fn aggregate_project_doc_omission_notice(documents: &[ProjectDocOmission]) -> String {
    let source_bytes = documents
        .iter()
        .map(|document| document.source_bytes)
        .sum::<u64>();
    let manifest = serde_json::to_vec(documents).unwrap_or_default();
    let manifest_sha256 = format!("{:x}", Sha256::digest(&manifest));
    let first = &documents[0];
    let mut notice = format!(
        "Project docs omitted: count={} bytes={} manifest_sha256={}; rediscover AGENTS/override files from cwd to repository root.",
        documents.len(),
        source_bytes,
        manifest_sha256,
    );
    let scope = format!(" scope={}@{}", first.environment_id, first.cwd);
    if notice.len().saturating_add(scope.len()).saturating_add(2)
        <= PROJECT_DOC_AGGREGATE_NOTICE_RESERVE_BYTES
    {
        notice.push_str(&scope);
    } else {
        let scope_sha256 = format!(
            " scope_sha256={:x}",
            Sha256::digest(format!("{}@{}", first.environment_id, first.cwd).as_bytes())
        );
        if notice
            .len()
            .saturating_add(scope_sha256.len())
            .saturating_add(2)
            <= PROJECT_DOC_AGGREGATE_NOTICE_RESERVE_BYTES
        {
            notice.push_str(&scope_sha256);
        }
    }
    let first_path = format!(" first_path={}", first.path);
    if notice
        .len()
        .saturating_add(first_path.len())
        .saturating_add(2)
        <= PROJECT_DOC_AGGREGATE_NOTICE_RESERVE_BYTES
    {
        notice.push_str(&first_path);
    }
    notice
}

fn render_project_doc_to_budget(
    read: &mut ProjectDocRead,
    path: &PathUri,
    max_bytes: usize,
) -> Option<(String, usize)> {
    truncate_project_doc_to_budget(read, max_bytes);
    loop {
        let retained_bytes = read.retained_data.len();
        let omitted_bytes = read.original_bytes.saturating_sub(retained_bytes as u64);
        let mut text = String::from_utf8_lossy(&read.retained_data).to_string();
        if omitted_bytes > 0 {
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(&project_doc_truncation_notice(
                path,
                read.original_bytes,
                retained_bytes,
            ));
        }
        if text.len() <= max_bytes {
            return Some((text, retained_bytes));
        }
        if retained_bytes == 0 {
            return None;
        }
        let excess = text.len().saturating_sub(max_bytes).max(1);
        truncate_project_doc_to_budget(read, retained_bytes.saturating_sub(excess));
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
#[cfg(test)]
async fn agents_md_paths(
    config: &Config,
    cwd: &PathUri,
    fs: &dyn ExecutorFileSystem,
    reuse_project_discovery: bool,
) -> io::Result<Vec<ProjectDocCandidate>> {
    let project_root_markers = effective_project_root_markers(config);
    agents_md_paths_with_markers(
        config,
        cwd,
        fs,
        reuse_project_discovery,
        &project_root_markers,
    )
    .await
}

async fn agents_md_paths_with_markers(
    config: &Config,
    cwd: &PathUri,
    fs: &dyn ExecutorFileSystem,
    reuse_project_discovery: bool,
    project_root_markers: &[String],
) -> io::Result<Vec<ProjectDocCandidate>> {
    let metrics = codex_otel::global();
    agents_md_paths_with_metrics_and_markers(
        config,
        cwd,
        fs,
        reuse_project_discovery,
        project_root_markers,
        metrics.as_ref(),
    )
    .await
}

#[cfg(test)]
async fn agents_md_paths_with_metrics(
    config: &Config,
    cwd: &PathUri,
    fs: &dyn ExecutorFileSystem,
    reuse_project_discovery: bool,
    metrics: Option<&MetricsClient>,
) -> io::Result<Vec<ProjectDocCandidate>> {
    let project_root_markers = effective_project_root_markers(config);
    agents_md_paths_with_metrics_and_markers(
        config,
        cwd,
        fs,
        reuse_project_discovery,
        &project_root_markers,
        metrics,
    )
    .await
}

async fn agents_md_paths_with_metrics_and_markers(
    config: &Config,
    cwd: &PathUri,
    fs: &dyn ExecutorFileSystem,
    reuse_project_discovery: bool,
    project_root_markers: &[String],
    metrics: Option<&MetricsClient>,
) -> io::Result<Vec<ProjectDocCandidate>> {
    let dir = cwd.clone();

    let reuse = if !reuse_project_discovery {
        ProjectDiscoveryReuse::Miss("reuse_disabled")
    } else if let Ok(cwd) = dir.to_abs_path() {
        match config.config_layer_stack.project_discovery() {
            None => ProjectDiscoveryReuse::Miss("context_unavailable"),
            Some(discovery) if !discovery.matches_cwd(&cwd) => {
                ProjectDiscoveryReuse::Miss("cwd_mismatch")
            }
            Some(discovery) if discovery.project_root_markers() != project_root_markers => {
                ProjectDiscoveryReuse::Miss("markers_mismatch")
            }
            Some(discovery) => {
                ProjectDiscoveryReuse::Hit(PathUri::from_abs_path(discovery.project_root()))
            }
        }
    } else {
        ProjectDiscoveryReuse::Miss("cwd_unavailable")
    };
    let project_root = match reuse {
        ProjectDiscoveryReuse::Hit(root) => {
            record_project_discovery_reuse(metrics, "agents_md", "hit", "matched");
            Some(root)
        }
        ProjectDiscoveryReuse::Miss(reason) => {
            record_project_discovery_reuse(metrics, "agents_md", "miss", reason);
            find_nearest_ancestor_with_markers(
                fs,
                &dir,
                project_root_markers.to_vec(),
                FindUpErrorPolicy::Propagate,
                /*sandbox*/ None,
            )
            .await?
        }
    };
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
    let candidate_filenames: Arc<[String]> = project_doc_candidate_filenames(config)
        .into_iter()
        .map(str::to_owned)
        .collect();
    let mut directory_searches = Vec::with_capacity(search_dirs.len());
    for directory in search_dirs {
        let candidate_filenames = Arc::clone(&candidate_filenames);
        directory_searches.push(async move {
            for name in candidate_filenames.iter() {
                let candidate = directory
                    .join(name)
                    .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
                match fs.get_metadata(&candidate, /*sandbox*/ None).await {
                    Ok(metadata) if metadata.is_file => {
                        return Ok(Some(ProjectDocCandidate {
                            path: candidate,
                            size: metadata.size,
                            modified_at_ms: metadata.modified_at_ms,
                        }));
                    }
                    Ok(_) => {}
                    Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                    Err(err) => return Err(err),
                }
            }
            Ok(None)
        });
    }
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

fn record_project_discovery_reuse(
    metrics: Option<&MetricsClient>,
    consumer: &'static str,
    result: &'static str,
    reason: &'static str,
) {
    let Some(metrics) = metrics else {
        return;
    };
    if let Err(err) = metrics.counter(
        PROJECT_DISCOVERY_REUSE_METRIC,
        /*inc*/ 1,
        &[
            ("consumer", consumer),
            ("result", result),
            ("reason", reason),
        ],
    ) {
        tracing::warn!("project discovery reuse metric failed: {err}");
    }
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

pub fn project_doc_candidate_filenames(config: &Config) -> Vec<&str> {
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

#[derive(Clone)]
pub(crate) struct RepositoryStableContextBundle {
    pub(crate) identity: [u8; 32],
    pub(crate) rendered: Arc<str>,
    pub(crate) reused: bool,
    pub(crate) semantic_replacement: bool,
}

struct RenderedStableContext {
    text: String,
    rendered_hash: [u8; 32],
    user_instructions_hash: Option<[u8; 32]>,
    entry_hashes: Vec<[u8; 32]>,
}

struct StableContextRenderer {
    text: String,
    hasher: Sha256,
    user_instructions_hash: Option<[u8; 32]>,
    entry_hashes: Vec<[u8; 32]>,
}

impl StableContextRenderer {
    fn new(entry_capacity: usize) -> Self {
        Self {
            text: String::new(),
            hasher: Sha256::new(),
            user_instructions_hash: None,
            entry_hashes: Vec::with_capacity(entry_capacity),
        }
    }

    fn push_str(&mut self, value: &str) {
        self.text.push_str(value);
        self.hasher.update(value.as_bytes());
    }

    fn push_user_instructions(&mut self, value: &str) {
        self.user_instructions_hash = Some(Sha256::digest(value.as_bytes()).into());
        self.push_str(value);
    }

    fn push_entry(&mut self, value: &str) {
        self.entry_hashes
            .push(Sha256::digest(value.as_bytes()).into());
        self.push_str(value);
    }

    fn finish(self) -> RenderedStableContext {
        RenderedStableContext {
            text: self.text,
            rendered_hash: self.hasher.finalize().into(),
            user_instructions_hash: self.user_instructions_hash,
            entry_hashes: self.entry_hashes,
        }
    }
}

impl RepositoryStableContextBundle {
    pub(crate) fn metadata(&self) -> ([u8; 32], bool, bool) {
        (self.identity, self.reused, self.semantic_replacement)
    }

    pub(crate) fn as_cached(&self) -> Self {
        Self {
            identity: self.identity,
            rendered: Arc::clone(&self.rendered),
            reused: true,
            semantic_replacement: false,
        }
    }
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
        self.render_stable_context().text
    }

    /// Stable digest of the exact model-visible instruction text.
    #[cfg(test)]
    pub(crate) fn semantic_digest(&self) -> [u8; 32] {
        Sha256::digest(self.text().as_bytes()).into()
    }

    /// Versioned identity for repository instructions used by model-input
    /// projection. This includes scope and ordered provenance so a move or
    /// environment change remains semantic even when rendered bytes match.
    #[cfg(test)]
    pub(crate) fn stable_context_identity(&self, active_cwd: &PathUri) -> [u8; 32] {
        let rendered = self.text();
        self.stable_context_identity_for_rendered(active_cwd, &rendered)
    }

    pub(crate) fn stable_context_bundle(
        &self,
        active_cwd: &PathUri,
    ) -> RepositoryStableContextBundle {
        let rendered = self.render_stable_context();
        let structure = self.stable_context_structure_key_with_body_hashes(
            active_cwd,
            rendered.user_instructions_hash.as_ref(),
            &rendered.entry_hashes,
        );
        let identity = stable_context_identity_from_structure(structure, rendered.rendered_hash);
        let rendered: Arc<str> = rendered.text.into();
        RepositoryStableContextBundle {
            identity,
            rendered,
            reused: false,
            semantic_replacement: false,
        }
    }

    #[cfg(test)]
    fn stable_context_identity_for_rendered(
        &self,
        active_cwd: &PathUri,
        rendered: &str,
    ) -> [u8; 32] {
        stable_context_identity_from_structure(
            self.stable_context_structure_key(active_cwd),
            Sha256::digest(rendered.as_bytes()).into(),
        )
    }

    #[cfg(test)]
    fn stable_context_structure_key(&self, active_cwd: &PathUri) -> [u8; 32] {
        let user_instructions_hash = self
            .user_instructions
            .as_ref()
            .map(|instructions| Sha256::digest(instructions.text.as_bytes()).into());
        let entry_hashes = self
            .entries
            .iter()
            .map(|entry| Sha256::digest(entry.contents.as_bytes()).into())
            .collect::<Vec<_>>();
        self.stable_context_structure_key_with_body_hashes(
            active_cwd,
            user_instructions_hash.as_ref(),
            &entry_hashes,
        )
    }

    fn stable_context_structure_key_with_body_hashes(
        &self,
        active_cwd: &PathUri,
        user_instructions_hash: Option<&[u8; 32]>,
        entry_hashes: &[[u8; 32]],
    ) -> [u8; 32] {
        fn update_part(hasher: &mut Sha256, value: &[u8]) {
            hasher.update((value.len() as u64).to_be_bytes());
            hasher.update(value);
        }

        let mut hasher = Sha256::new();
        hasher.update(b"codex.repository-context-structure.v1");
        update_part(
            &mut hasher,
            active_cwd.inferred_native_path_string().as_bytes(),
        );
        if let Some(instructions) = &self.user_instructions {
            update_part(&mut hasher, b"user");
            update_part(
                &mut hasher,
                PathUri::from_abs_path(&instructions.source)
                    .inferred_native_path_string()
                    .as_bytes(),
            );
            let body_hash = user_instructions_hash
                .copied()
                .unwrap_or_else(|| Sha256::digest(instructions.text.as_bytes()).into());
            update_part(&mut hasher, &body_hash);
        }
        for (entry, entry_hash) in self.entries.iter().zip(entry_hashes) {
            match &entry.provenance {
                InstructionProvenance::Project {
                    source_path,
                    environment_id,
                    cwd,
                } => {
                    update_part(&mut hasher, b"project");
                    update_part(&mut hasher, environment_id.as_bytes());
                    update_part(&mut hasher, cwd.inferred_native_path_string().as_bytes());
                    update_part(
                        &mut hasher,
                        source_path.inferred_native_path_string().as_bytes(),
                    );
                }
                InstructionProvenance::Internal => update_part(&mut hasher, b"internal"),
            }
            update_part(&mut hasher, entry_hash);
        }
        hasher.finalize().into()
    }

    fn render_stable_context(&self) -> RenderedStableContext {
        if self.has_multiple_project_environments() {
            self.render_environment_labeled_text()
        } else {
            self.render_legacy_text()
        }
    }

    #[cfg(test)]
    fn legacy_text(&self) -> String {
        self.render_legacy_text().text
    }

    fn render_legacy_text(&self) -> RenderedStableContext {
        let mut output = StableContextRenderer::new(self.entries.len());
        let mut has_previous = false;
        let mut previous_was_project = false;
        if let Some(instructions) = &self.user_instructions {
            output.push_user_instructions(&instructions.text);
            has_previous = true;
        }
        for entry in &self.entries {
            let source_path = match &entry.provenance {
                InstructionProvenance::Project { source_path, .. } => Some(source_path),
                InstructionProvenance::Internal => None,
            };
            let is_project = source_path.is_some();
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
            if let Some(source_path) = source_path {
                output.push_str(&project_instruction_source_header(source_path));
            }
            output.push_entry(&entry.contents);
            has_previous = true;
            previous_was_project = is_project;
        }
        output.finish()
    }

    #[cfg(test)]
    fn environment_labeled_text(&self) -> String {
        self.render_environment_labeled_text().text
    }

    fn render_environment_labeled_text(&self) -> RenderedStableContext {
        let mut output = StableContextRenderer::new(self.entries.len());
        let mut has_previous = false;
        let mut previous_environment: Option<(&str, &PathUri)> = None;
        if let Some(instructions) = &self.user_instructions {
            output.push_user_instructions(&instructions.text);
            has_previous = true;
        }
        for entry in &self.entries {
            match &entry.provenance {
                InstructionProvenance::Project {
                    source_path,
                    environment_id,
                    cwd,
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
                            "for `{}` with cwd {}\n\n",
                            environment_id,
                            cwd.inferred_native_path_string()
                        ));
                    }
                    output.push_str(&project_instruction_source_header(source_path));
                    output.push_entry(&entry.contents);
                    previous_environment = Some(environment);
                }
                InstructionProvenance::Internal => {
                    if has_previous {
                        output.push_str("\n\n");
                    }
                    output.push_entry(&entry.contents);
                    previous_environment = None;
                }
            }
            has_previous = true;
        }
        output.finish()
    }

    #[cfg(test)]
    pub(crate) fn contextual_user_fragment(&self) -> ContextUserInstructions {
        self.contextual_user_fragment_with_text(self.text())
    }

    pub(crate) fn contextual_user_fragment_with_text(
        &self,
        text: String,
    ) -> ContextUserInstructions {
        // One contributing project environment retains the legacy cwd wrapper. With two or more,
        // the body labels every contributing environment itself, so the outer cwd is omitted.
        let directory = if self.has_multiple_project_environments() {
            None
        } else {
            self.single_project_cwd()
                .map(PathUri::inferred_native_path_string)
        };
        ContextUserInstructions { directory, text }
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
