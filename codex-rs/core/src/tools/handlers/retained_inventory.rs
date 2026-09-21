//! Task-neutral inventories over immutable producer evidence.
//!
//! The model chooses the scope, vocabulary, queries, and classifications. This
//! tool only retains exact producer identifiers and mechanically reconciles
//! snapshots. Nothing here infers what a candidate means.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use codex_tools::CanonicalToolResult;
use codex_tools::JsonSchema;
use codex_tools::JsonToolOutput;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use futures::StreamExt;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;

use crate::FunctionCallError;
use crate::tools::command_output_artifact::ToolOutputSelector;
use crate::tools::command_output_artifact::create_canonical_output_artifact;
use crate::tools::command_output_artifact::read_complete_canonical_snapshot;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::handlers::read_tool_output::execute_recovery_transaction_with_continuations;
use crate::tools::handlers::resolve_tool_environment;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;

const FORMAT: &str = "retained_inventory_v1";
const MAX_SNAPSHOT_BYTES: usize = 4 * 1024 * 1024;
const MAX_CANDIDATES: usize = 10_000;
const PAGE_TOKENS: usize = 2_000;
const OBSERVATION_CONCURRENCY: usize = 16;
const _: () = assert!(
    MAX_SNAPSHOT_BYTES <= crate::tools::command_output_artifact::MAX_RAW_OUTPUT_ARTIFACT_BYTES
);

pub(crate) struct RetainedInventoryHandler;

/// The receipt is explanatory; history recovery must retain the exact export,
/// including when the small receipt itself would fit without an artifact.
struct RenderedInventoryOutput {
    receipt: JsonToolOutput,
    canonical: CanonicalToolResult,
    artifact_id: String,
    delivery: Value,
}

impl codex_tools::ToolOutput for RenderedInventoryOutput {
    fn log_preview(&self) -> String {
        self.receipt.log_preview()
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn requires_canonical_artifact(&self) -> bool {
        true
    }

    fn canonical_result(&self, _: &ToolPayload) -> Option<CanonicalToolResult> {
        Some(self.canonical.clone())
    }

    fn projection_metadata(&self) -> Option<codex_tools::ToolOutputProjectionMetadata> {
        let mut metadata = self.receipt.projection_metadata()?;
        metadata.essential_inline["raw_output_artifact_id"] = json!(self.artifact_id);
        metadata.essential_inline["delivery"] = self.delivery.clone();
        Some(metadata)
    }

    fn to_response_item(
        &self,
        call_id: &str,
        payload: &ToolPayload,
    ) -> codex_protocol::models::ResponseInputItem {
        self.receipt.to_response_item(call_id, payload)
    }

    fn code_mode_result(&self, payload: &ToolPayload) -> Value {
        self.receipt.code_mode_result(payload)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Profile {
    categories: BTreeSet<String>,
    classifications: BTreeSet<String>,
    required_categories: BTreeSet<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Source {
    artifact_id: String,
    #[serde(default)]
    pointer: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    lines: Option<[usize; 2]>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
struct Evidence {
    source: Source,
    sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Candidate {
    id: String,
    /// Producer-observed state; null explicitly means it was not established.
    #[serde(deserialize_with = "required_nullable")]
    exists: Option<bool>,
    tracking: String,
    /// Producer-observed identity of the relevant source, if available.
    #[serde(deserialize_with = "required_nullable")]
    revision: Option<String>,
}

fn required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
struct Record {
    scope: String,
    category: String,
    candidate: Candidate,
    classification: Option<String>,
    evidence: Vec<Evidence>,
    status: String,
    unresolved_reason: Option<String>,
    provenance: Evidence,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
struct Category {
    complete: bool,
    unresolved_reason: Option<String>,
    provenance: Option<Evidence>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
struct Snapshot {
    format: String,
    scope_id: String,
    scope: Value,
    profile: Profile,
    environment_id: Option<String>,
    cwd: Option<String>,
    categories: BTreeMap<String, Category>,
    records: BTreeMap<String, BTreeMap<String, Record>>,
}

/// This envelope is emitted by the model-selected enumeration command. Import
/// reads the original artifact, never a model's transcription of its output.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Enumeration {
    scope_id: String,
    category: String,
    complete: bool,
    unresolved_reason: Option<String>,
    candidates: Vec<Candidate>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Decision {
    category: String,
    candidate_id: String,
    classification: Option<String>,
    evidence: Vec<Source>,
    unresolved_reason: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
enum Args {
    Create {
        scope: Value,
        profile: Profile,
        environment_id: Option<String>,
    },
    Import {
        inventory_id: String,
        source: Source,
    },
    Observe {
        inventory_id: String,
        category: String,
        paths: Vec<String>,
        #[serde(default = "default_fingerprint_contents")]
        fingerprint_contents: bool,
        complete: bool,
        unresolved_reason: Option<String>,
    },
    Classify {
        inventory_id: String,
        decisions: Vec<Decision>,
    },
    Read {
        inventory_id: String,
        #[serde(default)]
        unresolved_only: bool,
        #[serde(default)]
        offset: usize,
        #[serde(default = "default_limit")]
        limit: usize,
    },
    Render {
        inventory_id: String,
        #[serde(default)]
        classifications: BTreeSet<String>,
    },
}

fn default_fingerprint_contents() -> bool {
    true
}

fn default_limit() -> usize {
    20
}

fn invalid(message: impl Into<String>) -> FunctionCallError {
    FunctionCallError::RespondToModel(message.into())
}

fn digest(value: &impl Serialize) -> Result<String, FunctionCallError> {
    fn ordered(value: Value) -> Value {
        match value {
            Value::Object(fields) => Value::Object(
                fields
                    .into_iter()
                    .collect::<BTreeMap<_, _>>()
                    .into_iter()
                    .map(|(key, value)| (key, ordered(value)))
                    .collect(),
            ),
            Value::Array(values) => Value::Array(values.into_iter().map(ordered).collect()),
            value => value,
        }
    }
    let value = ordered(serde_json::to_value(value).map_err(|err| invalid(err.to_string()))?);
    let bytes = serde_json::to_vec(&value).map_err(|err| invalid(err.to_string()))?;
    Ok(crate::tool_history::sha256(&bytes))
}

fn nonempty(value: &str) -> bool {
    !value.trim().is_empty()
}

impl Snapshot {
    fn new(scope: Value, profile: Profile) -> Result<Self, FunctionCallError> {
        if !scope.is_object()
            || scope.as_object().is_none_or(serde_json::Map::is_empty)
            || scope.to_string().len() > 4_096
            || profile.categories.is_empty()
            || profile.categories.len() > 32
            || profile.classifications.is_empty()
            || profile.classifications.len() > 64
            || !profile.required_categories.is_subset(&profile.categories)
            || profile
                .categories
                .iter()
                .chain(&profile.classifications)
                .any(|label| !nonempty(label) || label.len() > 64)
        {
            return Err(invalid(
                "scope must be a nonempty object of at most 4096 bytes; profile must declare 1-32 categories and 1-64 classifications (labels at most 64 bytes), with required_categories drawn from categories",
            ));
        }
        let scope_id = digest(&(&scope, &profile))?;
        let categories = profile
            .categories
            .iter()
            .map(|name| {
                (
                    name.clone(),
                    Category {
                        complete: false,
                        unresolved_reason: Some("not enumerated".to_string()),
                        provenance: None,
                    },
                )
            })
            .collect();
        Ok(Self {
            format: FORMAT.to_string(),
            scope_id,
            scope,
            profile,
            environment_id: None,
            cwd: None,
            categories,
            records: BTreeMap::new(),
        })
    }

    fn import(
        &mut self,
        enumeration: Enumeration,
        provenance: Evidence,
    ) -> Result<Value, FunctionCallError> {
        if enumeration.scope_id != self.scope_id
            || !self.categories.contains_key(&enumeration.category)
        {
            return Err(invalid(
                "enumeration scope_id/category does not match this inventory; create a separate inventory for a different scope/profile",
            ));
        }
        if enumeration.candidates.len() > MAX_CANDIDATES
            || (!enumeration.complete
                && !enumeration
                    .unresolved_reason
                    .as_deref()
                    .is_some_and(nonempty))
            || (enumeration.complete && enumeration.unresolved_reason.is_some())
            || enumeration
                .unresolved_reason
                .as_ref()
                .is_some_and(|reason| reason.len() > 512)
        {
            return Err(invalid(
                "enumeration requires at most 10000 candidates, no reason for complete coverage, and a nonempty reason of at most 512 bytes for incomplete coverage",
            ));
        }
        let mut incoming = BTreeMap::new();
        for (index, candidate) in enumeration.candidates.into_iter().enumerate() {
            if !nonempty(&candidate.id)
                || candidate.id.len() > 1_024
                || !matches!(
                    candidate.tracking.as_str(),
                    "tracked" | "untracked" | "unknown"
                )
                || candidate
                    .revision
                    .as_ref()
                    .is_some_and(|revision| revision.len() > 256)
            {
                return Err(invalid(
                    "candidate requires an exact nonempty id (max 1024 bytes), tracking tracked/untracked/unknown, and an optional revision of at most 256 bytes",
                ));
            }
            if let Some((previous, _)) = incoming.get(&candidate.id) {
                if previous != &candidate {
                    return Err(invalid(format!(
                        "conflicting producer records for candidate {}",
                        candidate.id
                    )));
                }
            } else {
                incoming.insert(candidate.id.clone(), (candidate, index));
            }
        }
        let records = self
            .records
            .entry(enumeration.category.clone())
            .or_default();
        let mut added = Vec::new();
        let mut changed = Vec::new();
        let mut removed = Vec::new();
        let mut unverified = Vec::new();
        let mut unchanged = 0usize;
        // Absence is only established by a complete enumeration. An incomplete
        // page must never erase candidates retained from earlier pages.
        if enumeration.complete {
            for (id, record) in records.iter_mut() {
                if !incoming.contains_key(id) && record.status != "removed" {
                    record.status = "removed".to_string();
                    record.unresolved_reason = Some("absent from the latest complete enumeration; this does not prove file deletion".to_string());
                    removed.push(id.clone());
                }
            }
        }
        for (id, (candidate, index)) in incoming {
            let identical = records
                .get(&id)
                .is_some_and(|record| record.candidate == candidate && record.status != "removed");
            let revision_unknown = identical
                && candidate.revision.is_none()
                && candidate.exists != Some(false)
                && records.get(&id).is_some_and(|record| {
                    record.status == "classified"
                        && record.provenance.source.artifact_id != provenance.source.artifact_id
                });
            if identical && !revision_unknown {
                unchanged += 1;
                continue;
            }
            let status = if records.contains_key(&id) {
                if revision_unknown {
                    unverified.push(id.clone());
                } else {
                    changed.push(id.clone());
                }
                "stale"
            } else {
                added.push(id.clone());
                "unclassified"
            };
            let candidate_source = Source {
                artifact_id: provenance.source.artifact_id.clone(),
                pointer: format!("{}/candidates/{index}", provenance.source.pointer),
                lines: None,
            };
            let prior = records.remove(&id);
            let record = Record {
                scope: self.scope_id.clone(),
                category: enumeration.category.clone(),
                provenance: Evidence {
                    source: candidate_source,
                    sha256: digest(&candidate)?,
                },
                candidate,
                // Preserve the old decision as evidence, but never count it as
                // resolved after its producer-observed identity changed.
                classification: prior
                    .as_ref()
                    .and_then(|record| record.classification.clone()),
                evidence: prior.map(|record| record.evidence).unwrap_or_default(),
                status: status.to_string(),
                unresolved_reason: Some(
                    if revision_unknown {
                        "source revision is unknown; prior classification needs review"
                    } else if status == "stale" {
                        "candidate observation changed; classification needs review"
                    } else {
                        "classification pending"
                    }
                    .to_string(),
                ),
            };
            records.insert(id, record);
        }
        let category = self
            .categories
            .get_mut(&enumeration.category)
            .ok_or_else(|| invalid("unknown category"))?;
        if !added.is_empty()
            || !changed.is_empty()
            || !removed.is_empty()
            || !unverified.is_empty()
            || category.complete != enumeration.complete
            || category.unresolved_reason != enumeration.unresolved_reason
            || category.provenance.is_none()
        {
            *category = Category {
                complete: enumeration.complete,
                unresolved_reason: enumeration.unresolved_reason,
                provenance: Some(provenance),
            };
        }
        Ok(
            json!({"added": added, "changed": changed, "removed": removed, "unverified": unverified, "unchanged": unchanged}),
        )
    }

    fn coverage(&self) -> Value {
        self.categories
            .iter()
            .map(|(name, category)| {
                let candidate_count = self
                    .records
                    .get(name)
                    .into_iter()
                    .flat_map(|records| records.values())
                    .filter(|record| record.status != "removed")
                    .count();
                let outcome = if category.provenance.is_none() {
                    "not_enumerated"
                } else if !category.complete {
                    "incomplete"
                } else if candidate_count == 0 {
                    "complete_empty"
                } else {
                    "complete_with_candidates"
                };
                (
                    name.clone(),
                    json!({
                        "required": self.profile.required_categories.contains(name),
                        "outcome": outcome,
                        "candidate_count": candidate_count,
                        "unresolved_reason": category.unresolved_reason,
                        "provenance": category.provenance,
                    }),
                )
            })
            .collect()
    }

    fn progress_since(&self, before: &Self) -> Value {
        let mut categories_completed = 0;
        let mut categories_reopened = 0;
        for name in &self.profile.required_categories {
            match (
                before
                    .categories
                    .get(name)
                    .is_some_and(|category| category.complete),
                self.categories
                    .get(name)
                    .is_some_and(|category| category.complete),
            ) {
                (false, true) => categories_completed += 1,
                (true, false) => categories_reopened += 1,
                _ => {}
            }
        }
        let mut records_resolved = 0;
        let mut records_reopened = 0;
        for (category, records) in &self.records {
            for (id, record) in records {
                let prior = before
                    .records
                    .get(category)
                    .and_then(|records| records.get(id));
                let was_resolved = prior.is_some_and(|record| record.status == "classified");
                if record.status == "classified" && !was_resolved {
                    records_resolved += 1;
                } else if was_resolved
                    && !matches!(record.status.as_str(), "classified" | "removed")
                {
                    records_reopened += 1;
                }
            }
        }
        json!({
            "required_categories_completed": categories_completed,
            "required_categories_reopened": categories_reopened,
            "records_resolved": records_resolved,
            "records_reopened": records_reopened,
        })
    }

    fn summary(&self) -> Value {
        let mut statuses = BTreeMap::<String, usize>::new();
        let mut unique = BTreeSet::new();
        for record in self.records.values().flat_map(|records| records.values()) {
            *statuses.entry(record.status.clone()).or_default() += 1;
            if record.status != "removed" {
                unique.insert(&record.candidate.id);
            }
        }
        let unresolved_categories = self
            .profile
            .required_categories
            .iter()
            .filter(|category| !self.categories[*category].complete)
            .cloned()
            .collect::<Vec<_>>();
        let unresolved_records = statuses
            .iter()
            .filter(|(status, _)| !matches!(status.as_str(), "classified" | "removed"))
            .map(|(_, count)| count)
            .sum::<usize>();
        let unresolved_required_records = self
            .profile
            .required_categories
            .iter()
            .filter_map(|category| self.records.get(category))
            .flat_map(|records| records.values())
            .filter(|record| !matches!(record.status.as_str(), "classified" | "removed"))
            .count();
        json!({
            "scope_id": self.scope_id,
            "unique_candidates": unique.len(),
            "record_count": statuses.values().sum::<usize>(),
            "statuses": statuses,
            "unresolved_records": unresolved_records,
            "unresolved_required_categories": unresolved_categories,
            "unresolved_required_records": unresolved_required_records,
            "all_candidates_classified": unresolved_records == 0,
            "complete": unresolved_categories.is_empty() && unresolved_required_records == 0,
            "freshness": "retained producer snapshot; import fresh enumeration and reclassify changed evidence before claiming current workspace state"
        })
    }
}

fn native_key(path: &std::path::Path) -> String {
    let path = path.to_string_lossy().into_owned();
    if cfg!(windows) {
        path.replace('\\', "/").to_lowercase()
    } else {
        path
    }
}

/// Observe caller-selected paths through the same filesystem/permission
/// boundary as read_file. Search strategy and coverage are caller-owned; path
/// identities, existence, tracked state, and content revisions are not.
async fn observe(
    invocation: &ToolInvocation,
    snapshot: &Snapshot,
    category: String,
    paths: Vec<String>,
    fingerprint_contents: bool,
    complete: bool,
    unresolved_reason: Option<String>,
) -> Result<(Enumeration, Evidence), FunctionCallError> {
    if paths.len() > MAX_CANDIDATES || !snapshot.categories.contains_key(&category) {
        return Err(invalid(
            "observe requires a declared category and at most 10000 paths per enumeration",
        ));
    }
    let environment = resolve_tool_environment(
        &invocation.step_context.environments,
        snapshot.environment_id.as_deref(),
    )?
    .ok_or_else(|| invalid("observe requires an execution environment"))?;
    if snapshot.environment_id.as_ref() != Some(&environment.environment_id)
        || snapshot.cwd.as_deref() != Some(environment.cwd().to_string().as_str())
    {
        return Err(invalid(
            "inventory environment/cwd changed; create a separate scope rather than mixing observations",
        ));
    }
    let sandbox = invocation
        .step_context
        .turn
        .file_system_sandbox_context(None, environment.cwd());
    let fs = environment.environment.get_filesystem();
    let mut resolved = BTreeMap::new();
    for path in paths {
        let path = environment
            .cwd()
            .join(&path)
            .map_err(|err| invalid(err.to_string()))?;
        // Keep the requested logical path, including tracked symlinks, rather
        // than silently changing the candidate to a target outside the scope.
        let key = if !environment.environment.is_remote() {
            path.to_abs_path()
                .ok()
                .map(|path| native_key(path.as_path()))
                .unwrap_or_else(|| path.to_string())
        } else {
            path.to_string()
        };
        resolved.entry(key).or_insert(path);
    }
    let native_cwd = if environment.environment.is_remote() {
        None
    } else {
        environment.cwd().to_abs_path().ok()
    };
    let repo = native_cwd
        .as_ref()
        .and_then(|cwd| codex_git_utils::get_git_repo_root(cwd.as_path()));
    let mut tracked = None;
    if let Some(repo) = &repo {
        let paths = resolved
            .values()
            .filter_map(|path| path.to_abs_path().ok())
            .filter(|path| path.as_path().starts_with(repo))
            .map(|path| path.as_path().to_path_buf())
            .collect::<Vec<_>>();
        if let Some(index) = codex_git_utils::git_index_entries(repo, &paths).await {
            let mut ids = BTreeSet::new();
            let mut valid = true;
            for entry in index
                .split(|byte| *byte == 0)
                .filter(|entry| !entry.is_empty())
            {
                if let Some(tab) = memchr::memchr(b'\t', entry)
                    && let Ok(path) = std::str::from_utf8(&entry[tab + 1..])
                {
                    ids.insert(native_key(&repo.join(path)));
                } else {
                    valid = false;
                }
            }
            if valid {
                tracked = Some(ids);
            }
        }
    }
    // Each path has dependent metadata/content reads, but paths are independent.
    // Ordered buffering preserves deterministic evidence without serial remote I/O.
    let fs = &fs;
    let sandbox = &sandbox;
    let repo = &repo;
    let tracked = &tracked;
    let observations = futures::stream::iter(resolved.into_values().map(|path| async move {
        let id = path.inferred_native_path_string();
        let result = async {
            if invocation.cancellation_token.is_cancelled() {
                return Err(invalid("inventory observation cancelled"));
            }
            let tracking = match (path.to_abs_path().ok(), repo, tracked) {
                (Some(path), Some(repo), Some(tracked)) if path.as_path().starts_with(repo) => {
                    if tracked.contains(&native_key(path.as_path())) {
                        "tracked"
                    } else {
                        "untracked"
                    }
                }
                _ => "unknown",
            };
            let (exists, revision) = match fs.get_metadata(&path, Some(sandbox)).await {
                Ok(metadata) if metadata.is_file && fingerprint_contents => {
                    let contents = fs
                        .read_file_bounded(&path, 8 * 1024 * 1024, Some(sandbox))
                        .await
                        .map_err(|err| {
                            invalid(format!(
                                "unable to fingerprint {}: {err}",
                                path.inferred_native_path_string()
                            ))
                        })?;
                    (
                        Some(true),
                        contents.map(|bytes| crate::tool_history::sha256(&bytes)),
                    )
                }
                Ok(_) => (Some(true), None),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => (Some(false), None),
                Err(error) => {
                    return Err(invalid(format!(
                        "unable to observe {}: {error}",
                        path.inferred_native_path_string()
                    )));
                }
            };
            Ok(Candidate {
                id: path.inferred_native_path_string(),
                exists,
                tracking: tracking.to_string(),
                revision,
            })
        }
        .await;
        (id, result)
    }))
    .buffered(OBSERVATION_CONCURRENCY)
    .collect::<Vec<_>>();
    let observations = invocation
        .cancellation_token
        .run_until_cancelled(observations)
        .await
        .ok_or_else(|| invalid("inventory observation cancelled"))?;
    let mut candidates = Vec::new();
    let mut failures = Vec::new();
    for (id, observation) in observations {
        match observation {
            Ok(candidate) => candidates.push(candidate),
            Err(error) => {
                failures.push(error.to_string());
                candidates.push(Candidate {
                    id,
                    exists: None,
                    tracking: "unknown".to_string(),
                    revision: None,
                });
            }
        }
    }
    // A failed observation cannot establish absence. Preserve successful work
    // as an incomplete enumeration and retain the failures for targeted retry.
    let complete = complete && failures.is_empty();
    let unresolved_reason = if failures.is_empty() {
        unresolved_reason
    } else {
        let artifact = persist(invocation, json!({"observation_errors": failures})).await?;
        Some(format!(
            "observation incomplete; retry failed paths in artifact {artifact}"
        ))
    };
    let value = json!({"scope_id": snapshot.scope_id, "category": category, "complete": complete,
        "unresolved_reason": unresolved_reason, "candidates": candidates});
    let enumeration =
        serde_json::from_value(value.clone()).map_err(|err| invalid(err.to_string()))?;
    let artifact_id = persist(invocation, value.clone()).await?;
    Ok((
        enumeration,
        Evidence {
            source: Source {
                artifact_id,
                pointer: String::new(),
                lines: None,
            },
            sha256: digest(&value)?,
        },
    ))
}

async fn read_source(
    invocation: &ToolInvocation,
    source: &Source,
) -> Result<(Value, Evidence), FunctionCallError> {
    validate_source(source)?;
    let bytes = read_complete_canonical_snapshot(
        &invocation.step_context.turn.config.codex_home,
        &invocation.session.thread_id.to_string(),
        &source.artifact_id,
        MAX_SNAPSHOT_BYTES,
    )
    .await
    .map_err(|err| invalid(err.for_model()))?;
    let selected = if let Some([start, end]) = source.lines {
        if !source.pointer.is_empty() || start == 0 || end < start {
            return Err(invalid(
                "evidence lines require an inclusive 1-based range and no JSON pointer",
            ));
        }
        let text = std::str::from_utf8(&bytes).map_err(|err| invalid(err.to_string()))?;
        let lines = text.split_inclusive('\n').collect::<Vec<_>>();
        let range = lines
            .get(start - 1..end)
            .ok_or_else(|| invalid("evidence line range is outside the retained source"))?;
        Value::String(range.concat())
    } else {
        let document: Value = serde_json::from_slice(&bytes).map_err(|err| {
            invalid(format!(
                "source must be exact JSON producer output, or use lines for text evidence: {err}"
            ))
        })?;
        document
            .pointer(&source.pointer)
            .ok_or_else(|| invalid("source JSON pointer does not exist"))?
            .clone()
    };
    let evidence = Evidence {
        source: source.clone(),
        sha256: digest(&selected)?,
    };
    Ok((selected, evidence))
}

fn validate_source(source: &Source) -> Result<(), FunctionCallError> {
    if source.pointer.len() > 2_048
        || (!source.pointer.is_empty() && !source.pointer.starts_with('/'))
        || source
            .pointer
            .split('~')
            .skip(1)
            .any(|escape| !escape.starts_with(['0', '1']))
        || (source.lines.is_some() && !source.pointer.is_empty())
    {
        return Err(invalid(
            "source requires a valid RFC 6901 pointer (at most 2048 bytes) or a line range, not both",
        ));
    }
    Ok(())
}

enum EvidenceLink {
    Record(Record),
    Source(Source),
    Evidence(Evidence),
}

async fn read_evidence(
    invocation: &ToolInvocation,
    source: Source,
    require_supporting_source: bool,
    inspected: &mut BTreeMap<String, bool>,
) -> Result<(Evidence, Option<EvidenceLink>), FunctionCallError> {
    validate_source(&source)?;
    let selector = match source.lines {
        Some([start, end]) => ToolOutputSelector::Lines { start, end },
        None => ToolOutputSelector::JsonPointer {
            pointer: source.pointer.clone(),
        },
    };
    let transaction = execute_recovery_transaction_with_continuations(
        &invocation.step_context.turn.config.codex_home,
        &invocation.session.thread_id.to_string(),
        &source.artifact_id,
        vec![selector],
        false,
        &invocation.cancellation_token,
    )
    .await
    .map_err(|err| invalid(err.for_model()))?;
    let result = transaction.output;
    if !result.complete || result.results.len() != 1 || !result.results[0].complete {
        return Err(invalid(
            "evidence is unavailable, invalid, or exceeds exact recovery bounds; select a smaller exact range with read_tool_output",
        ));
    }
    let selected = &result.results[0];
    let value = selected
        .value
        .clone()
        .or_else(|| selected.text.clone().map(Value::String))
        .ok_or_else(|| invalid("evidence has no exact text/JSON value"))?;
    let mut record = None;
    if require_supporting_source {
        // Inspect the whole retained object: selecting a scalar or lines must
        // not turn bookkeeping into supporting source evidence.
        let source_is_bookkeeping = if let Some(bookkeeping) = inspected.get(&source.artifact_id) {
            *bookkeeping
        } else {
            let bytes = read_complete_canonical_snapshot(
                &invocation.step_context.turn.config.codex_home,
                &invocation.session.thread_id.to_string(),
                &source.artifact_id,
                crate::tools::command_output_artifact::MAX_RAW_OUTPUT_ARTIFACT_BYTES,
            )
            .await
            .map_err(|err| invalid(err.for_model()))?;
            let document = serde_json::from_slice::<Value>(&bytes).ok();
            let bookkeeping = document.as_ref().is_some_and(|doc| {
                doc["format"] == FORMAT
                    || doc.get("inventory_id").is_some()
                    || (doc.get("scope_id").is_some() && doc.get("candidates").is_some())
                    || serde_json::from_value::<Record>(doc.clone()).is_ok()
            });
            if inspected.len() < 128 {
                inspected.insert(source.artifact_id.clone(), bookkeeping);
            }
            bookkeeping
        };
        let selected_link = serde_json::from_value::<Record>(value.clone())
            .ok()
            .map(EvidenceLink::Record)
            .or_else(|| {
                serde_json::from_value::<Evidence>(value.clone())
                    .ok()
                    .map(EvidenceLink::Evidence)
            })
            .or_else(|| {
                serde_json::from_value::<Source>(value.clone())
                    .ok()
                    .map(EvidenceLink::Source)
            })
            .or_else(|| {
                value
                    .get("record")
                    .and_then(|link| serde_json::from_value::<Source>(link.clone()).ok())
                    .map(EvidenceLink::Source)
            });
        let bookkeeping = selected_link.is_some() || source_is_bookkeeping;
        if bookkeeping {
            let selected_link = selected_link.ok_or_else(|| invalid(
                "inventory bookkeeping is not supporting evidence; select a classified record with a source-backed evidence chain",
            ))?;
            if source.lines.is_some() {
                return Err(invalid(
                    "inventory evidence must reference a classified record with supporting source evidence",
                ));
            }
            if let EvidenceLink::Record(selected_record) = &selected_link {
                if selected_record.status != "classified" || selected_record.evidence.is_empty() {
                    return Err(invalid(
                        "inventory evidence must reference a classified record with supporting source evidence",
                    ));
                }
            }
            record = Some(selected_link);
        }
    }
    Ok((
        Evidence {
            source,
            sha256: digest(&value)?,
        },
        record,
    ))
}

async fn resolve_evidence(
    invocation: &ToolInvocation,
    snapshot: &Snapshot,
    decision: &Decision,
    inspected: &mut BTreeMap<String, bool>,
) -> Result<Vec<Evidence>, FunctionCallError> {
    let candidate = snapshot
        .records
        .get(&decision.category)
        .and_then(|records| records.get(&decision.candidate_id))
        .ok_or_else(|| {
            invalid("decision names a candidate absent from the retained enumeration")
        })?;
    let mut pending = decision
        .evidence
        .iter()
        .rev()
        .cloned()
        .map(|source| (source, None::<String>, Vec::<Source>::new()))
        .collect::<Vec<_>>();
    let mut evidence = Vec::new();
    let mut visited = 0;
    while let Some((source, expected_digest, mut ancestors)) = pending.pop() {
        if invocation.cancellation_token.is_cancelled() {
            return Err(invalid("inventory evidence resolution cancelled"));
        }
        visited += 1;
        if visited > 64 || ancestors.contains(&source) {
            return Err(invalid(
                "inventory evidence chain is cyclic or exceeds 64 references",
            ));
        }
        ancestors.push(source.clone());
        // Unresolved records may cite bookkeeping to explain missing evidence;
        // they cannot subsequently serve as proof of a resolved classification.
        let require_supporting_source =
            decision.classification.is_some() && decision.unresolved_reason.is_none();
        let (reference, record) =
            read_evidence(invocation, source, require_supporting_source, inspected).await?;
        if expected_digest
            .as_ref()
            .is_some_and(|expected| expected != &reference.sha256)
        {
            return Err(invalid(
                "inventory evidence chain does not match its retained source digest",
            ));
        }
        if let Some(EvidenceLink::Record(record)) = record {
            if record.scope != snapshot.scope_id
                || record.category != decision.category
                || record.candidate != candidate.candidate
                || record.classification != decision.classification
            {
                return Err(invalid(
                    "inventory evidence record supports a different candidate or classification",
                ));
            }
            pending.extend(
                record
                    .evidence
                    .into_iter()
                    .rev()
                    .map(|edge| (edge.source, Some(edge.sha256), ancestors.clone())),
            );
        } else if let Some(EvidenceLink::Source(source)) = record {
            pending.push((source, None, ancestors));
        } else if let Some(EvidenceLink::Evidence(edge)) = record {
            pending.push((edge.source, Some(edge.sha256), ancestors));
        } else if !evidence.contains(&reference) {
            evidence.push(reference);
        }
    }
    Ok(evidence)
}

async fn persist(invocation: &ToolInvocation, value: Value) -> Result<String, FunctionCallError> {
    if invocation.cancellation_token.is_cancelled() {
        return Err(invalid("inventory operation cancelled"));
    }
    if serde_json::to_vec(&value)
        .map_err(|err| invalid(err.to_string()))?
        .len()
        > MAX_SNAPSHOT_BYTES
    {
        return Err(invalid(
            "inventory exceeds the 4 MiB snapshot limit; divide the selected scope explicitly",
        ));
    }
    let canonical = CanonicalToolResult::json(value);
    let artifact = create_canonical_output_artifact(
        &invocation.step_context.turn.config.codex_home,
        &invocation.session.thread_id.to_string(),
        &canonical,
    )
    .await;
    if !artifact.complete {
        return Err(invalid(artifact.error.unwrap_or_else(|| {
            "inventory snapshot could not be fully retained".to_string()
        })));
    }
    let artifact_id = artifact
        .artifact_id()
        .ok_or_else(|| invalid("inventory snapshot has no artifact identity"))?;
    invocation
        .session
        .register_tool_artifact_origin(
            artifact_id.clone(),
            invocation.call_id.clone(),
            canonical.exact_bytes,
            canonical.sha256,
        )
        .await;
    Ok(artifact_id)
}

async fn execute(
    invocation: &ToolInvocation,
    args: Args,
    canonical_render: &mut Option<CanonicalToolResult>,
) -> Result<Value, FunctionCallError> {
    if let Args::Create {
        scope,
        profile,
        environment_id,
    } = args
    {
        let mut snapshot = Snapshot::new(scope, profile)?;
        if let Some(environment) = resolve_tool_environment(
            &invocation.step_context.environments,
            environment_id.as_deref(),
        )? {
            snapshot.environment_id = Some(environment.environment_id.clone());
            snapshot.cwd = Some(environment.cwd().to_string());
        }
        snapshot.scope_id = digest(&(
            &snapshot.scope,
            &snapshot.profile,
            &snapshot.environment_id,
            &snapshot.cwd,
        ))?;
        let id = persist(
            invocation,
            serde_json::to_value(&snapshot).map_err(|err| invalid(err.to_string()))?,
        )
        .await?;
        return Ok(json!({"inventory_id": id, "reused": false, "summary": snapshot.summary()}));
    }
    let id = match &args {
        Args::Import { inventory_id, .. }
        | Args::Observe { inventory_id, .. }
        | Args::Classify { inventory_id, .. }
        | Args::Read { inventory_id, .. }
        | Args::Render { inventory_id, .. } => inventory_id.clone(),
        Args::Create { .. } => unreachable!(),
    };
    let (value, _) = read_source(
        invocation,
        &Source {
            artifact_id: id.clone(),
            pointer: String::new(),
            lines: None,
        },
    )
    .await?;
    let mut snapshot: Snapshot = serde_json::from_value(value)
        .map_err(|err| invalid(format!("invalid inventory snapshot: {err}")))?;
    if snapshot.format != FORMAT {
        return Err(invalid("unsupported inventory snapshot format"));
    }
    let expected = Snapshot::new(snapshot.scope.clone(), snapshot.profile.clone())?;
    if digest(&(
        &snapshot.scope,
        &snapshot.profile,
        &snapshot.environment_id,
        &snapshot.cwd,
    ))? != snapshot.scope_id
        || snapshot.categories.keys().ne(expected.categories.keys())
        || snapshot.records.iter().any(|(category, records)| {
            !snapshot.categories.contains_key(category)
                || records.iter().any(|(id, record)| {
                    record.scope != snapshot.scope_id
                        || &record.category != category
                        || &record.candidate.id != id
                        || !matches!(
                            record.status.as_str(),
                            "classified" | "unclassified" | "unresolved" | "stale" | "removed"
                        )
                        || (record.status == "classified"
                            && (record.evidence.is_empty()
                                || record.classification.is_none()
                                || record.unresolved_reason.is_some()))
                        || record
                            .classification
                            .as_ref()
                            .is_some_and(|label| !snapshot.profile.classifications.contains(label))
                })
        })
    {
        return Err(invalid(
            "inventory snapshot violates its scope/profile/record contract",
        ));
    }
    let before =
        (!matches!(&args, Args::Read { .. } | Args::Render { .. })).then(|| snapshot.clone());
    let mut changes = None;
    match args {
        Args::Import { source, .. } => {
            let (value, provenance) = read_source(invocation, &source).await?;
            let enumeration = serde_json::from_value(value)
                .map_err(|err| invalid(format!("invalid enumeration: {err}")))?;
            changes = Some(snapshot.import(enumeration, provenance)?);
        }
        Args::Observe {
            category,
            paths,
            fingerprint_contents,
            complete,
            unresolved_reason,
            ..
        } => {
            let (enumeration, provenance) = observe(
                invocation,
                &snapshot,
                category,
                paths,
                fingerprint_contents,
                complete,
                unresolved_reason,
            )
            .await?;
            changes = Some(snapshot.import(enumeration, provenance)?);
        }
        Args::Classify { decisions, .. } => {
            if decisions.is_empty() || decisions.len() > 100 {
                return Err(invalid("classify accepts 1-100 decisions"));
            }
            let mut inspected = BTreeMap::new();
            let mut seen = BTreeSet::new();
            for decision in decisions {
                if !seen.insert((decision.category.clone(), decision.candidate_id.clone())) {
                    return Err(invalid(
                        "duplicate decision for the same category and candidate",
                    ));
                }
                if decision
                    .classification
                    .as_ref()
                    .is_some_and(|label| !snapshot.profile.classifications.contains(label))
                {
                    return Err(invalid(
                        "classification is not declared by this inventory's profile",
                    ));
                }
                let resolved =
                    decision.classification.is_some() && decision.unresolved_reason.is_none();
                if decision.evidence.is_empty()
                    || decision.evidence.len() > 8
                    || (!resolved && !decision.unresolved_reason.as_deref().is_some_and(nonempty))
                    || decision
                        .unresolved_reason
                        .as_ref()
                        .is_some_and(|reason| reason.len() > 512)
                {
                    return Err(invalid(
                        "each decision needs 1-8 exact evidence references and either a classification or an unresolved_reason",
                    ));
                }
                let evidence =
                    resolve_evidence(invocation, &snapshot, &decision, &mut inspected).await?;
                let record = snapshot
                    .records
                    .get_mut(&decision.category)
                    .and_then(|records| records.get_mut(&decision.candidate_id))
                    .ok_or_else(|| {
                        invalid("decision names a candidate absent from the retained enumeration")
                    })?;
                if record.status == "removed" {
                    return Err(invalid(
                        "cannot classify a removed candidate; import fresh enumeration first",
                    ));
                }
                record.classification = decision.classification;
                record.evidence = evidence;
                record.status = if resolved { "classified" } else { "unresolved" }.to_string();
                record.unresolved_reason = decision.unresolved_reason;
            }
        }
        Args::Read {
            offset,
            limit,
            unresolved_only,
            ..
        } => {
            if !(1..=50).contains(&limit) {
                return Err(invalid("read limit must be 1-50"));
            }
            let all = snapshot
                .records
                .values()
                .flat_map(|records| records.values())
                .filter(|record| {
                    !unresolved_only || !matches!(record.status.as_str(), "classified" | "removed")
                })
                .collect::<Vec<_>>();
            if offset > all.len() {
                return Err(invalid("offset is past the record count"));
            }
            let mut page = Vec::new();
            let summary = snapshot.summary();
            let mut page_tokens = codex_utils_string::approx_token_count(
                &json!({"summary": summary, "records": []}).to_string(),
            );
            for record in all.iter().skip(offset).take(limit) {
                let row = json!({"category": record.category, "candidate": record.candidate,
                    "classification": record.classification, "status": record.status,
                    "unresolved_reason": record.unresolved_reason,
                    "record": {"artifact_id": id, "pointer": format!("/records/{}/{}", escape_pointer(&record.category), escape_pointer(&record.candidate.id))}});
                // JSON row boundaries are punctuation, so the sum of row
                // estimates plus commas conservatively bounds the full page.
                let row_tokens = codex_utils_string::approx_token_count(&row.to_string())
                    + usize::from(!page.is_empty());
                if page_tokens.saturating_add(row_tokens) > PAGE_TOKENS {
                    break;
                }
                page_tokens += row_tokens;
                page.push(row);
            }
            if page.is_empty() && offset < all.len() {
                return Err(invalid(
                    "record metadata exceeds the page budget; use read_tool_output on the inventory artifact",
                ));
            }
            let end = offset + page.len();
            return Ok(
                json!({"inventory_id": id, "reused": true, "summary": summary, "records": page,
                "unresolved_only": unresolved_only, "matching_records": all.len(),
                "page_complete": end == all.len(), "next_offset": if end < all.len() { Some(end) } else { None }}),
            );
        }
        Args::Render {
            classifications, ..
        } => {
            if !classifications.is_subset(&snapshot.profile.classifications) {
                return Err(invalid("render filter contains undeclared classifications"));
            }
            let identifiers = snapshot
                .records
                .values()
                .flat_map(|records| records.values())
                .filter(|record| {
                    record.status == "classified"
                        && record.classification.as_ref().is_some_and(|label| {
                            classifications.is_empty() || classifications.contains(label)
                        })
                })
                .map(|record| record.candidate.id.clone())
                .collect::<BTreeSet<_>>();
            let rendered = json!({"inventory_id": id, "scope": snapshot.scope, "profile": snapshot.profile,
                "classifications": classifications, "summary": snapshot.summary(),
                "coverage": snapshot.coverage(),
                "count": identifiers.len(), "identifiers": identifiers});
            let count = rendered["count"].clone();
            let inline_identifiers =
                (codex_utils_string::approx_token_count(&rendered.to_string()) <= PAGE_TOKENS)
                    .then(|| rendered["identifiers"].clone());
            *canonical_render = Some(CanonicalToolResult::json(rendered.clone()));
            let artifact_id = persist(invocation, rendered).await?;
            // Snapshots are capped below the artifact segment size: this file
            // contains the complete rendered document, not a head fragment.
            let rendered_path = invocation
                .step_context
                .turn
                .config
                .codex_home
                .join("tool-output")
                .join(invocation.session.thread_id.to_string())
                .join(format!("{artifact_id}.log"));
            let mut receipt = json!({"inventory_id": id, "rendered_artifact_id": artifact_id, "rendered_path": rendered_path, "count": count,
                "summary": snapshot.summary(), "classifications": classifications,
                "coverage_source": {"artifact_id": artifact_id, "pointer": "/coverage"},
                "identifiers_source": "exact sorted deduplicated retained identifiers; recover /identifiers from rendered_artifact_id"});
            if let Some(identifiers) = inline_identifiers {
                receipt["identifiers"] = identifiers;
            }
            return Ok(receipt);
        }
        Args::Create { .. } => unreachable!(),
    }
    let before = before.expect("mutating inventory operations retain their baseline");
    let reused = before == snapshot;
    let id = if reused {
        id
    } else {
        persist(
            invocation,
            serde_json::to_value(&snapshot).map_err(|err| invalid(err.to_string()))?,
        )
        .await?
    };
    // Detailed changes are retained once, rather than repeated in the model
    // context. The small receipt reports counts and an exact recovery handle.
    let change_receipt = if let Some(changes) = changes {
        let counts = json!({"added": changes["added"].as_array().map(Vec::len), "changed": changes["changed"].as_array().map(Vec::len),
            "unverified": changes["unverified"].as_array().map(Vec::len),
            "removed": changes["removed"].as_array().map(Vec::len), "unchanged": changes["unchanged"]});
        if reused {
            Some(json!({"counts": counts}))
        } else {
            let artifact_id = persist(invocation, changes).await?;
            Some(json!({"counts": counts, "artifact_id": artifact_id}))
        }
    } else {
        None
    };
    Ok(
        json!({"inventory_id": id, "reused": reused, "summary": snapshot.summary(), "changes": change_receipt,
            "progress": snapshot.progress_since(&before)}),
    )
}

fn escape_pointer(value: &str) -> impl std::fmt::Display + '_ {
    codex_utils_string::json_pointer_segment(value)
}

impl ToolExecutor<ToolInvocation> for RetainedInventoryHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("inventory")
    }

    fn exposure(&self) -> codex_tools::ToolExposure {
        codex_tools::ToolExposure::Deferred
    }

    fn spec(&self) -> ToolSpec {
        let source = json!({"type":"object","properties":{"artifact_id":{"type":"string"},"pointer":{"type":"string"},
            "lines":{"type":"array","items":{"type":"integer","minimum":1},"minItems":2,"maxItems":2}},
            "required":["artifact_id"],"additionalProperties":false});
        let strings = json!({"type":"array","items":{"type":"string"}});
        let profile = json!({"type":"object","properties":{"categories":strings,"classifications":strings,"required_categories":strings},
            "required":["categories","classifications","required_categories"],"additionalProperties":false});
        let decision = json!({"type":"object","properties":{"category":{"type":"string"},"candidate_id":{"type":"string"},
            "classification":{"type":["string","null"]},"evidence":{"type":"array","items":source,"minItems":1,"maxItems":8},
            "unresolved_reason":{"type":["string","null"]}},"required":["category","candidate_id","evidence"],"additionalProperties":false});
        let variants = [
            ("create", json!({"scope":{"type":"object","additionalProperties":true},"profile":profile,"environment_id":{"type":"string"}}), vec!["scope","profile"]),
            ("import", json!({"inventory_id":{"type":"string"},"source":source}), vec!["inventory_id","source"]),
            ("observe", json!({"inventory_id":{"type":"string"},"category":{"type":"string"},"paths":{"type":"array","items":{"type":"string"},"maxItems":10000},"fingerprint_contents":{"type":"boolean","default":true},"complete":{"type":"boolean"},"unresolved_reason":{"type":["string","null"]}}), vec!["inventory_id","category","paths","complete"]),
            ("classify", json!({"inventory_id":{"type":"string"},"decisions":{"type":"array","items":decision,"minItems":1,"maxItems":100}}), vec!["inventory_id","decisions"]),
            ("read", json!({"inventory_id":{"type":"string"},"unresolved_only":{"type":"boolean"},"offset":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1,"maximum":50}}), vec!["inventory_id"]),
            ("render", json!({"inventory_id":{"type":"string"},"classifications":strings}), vec!["inventory_id"]),
        ].into_iter().map(|(operation, mut properties, mut required)| {
            properties["operation"] = json!({"type":"string","enum":[operation]});
            required.push("operation");
            json!({"type":"object","properties":properties,"required":required,"additionalProperties":false})
        }).collect::<Vec<_>>();
        // Constructed only from fixed schemas, never from producer input.
        #[expect(
            clippy::expect_used,
            reason = "fixed schema authored beside the handler"
        )]
        let parameters: JsonSchema =
            serde_json::from_value(json!({"oneOf":variants})).expect("inventory tool schema");
        ToolSpec::Function(ResponsesApiTool {
            name: "inventory".to_string(),
            description: "Retain a generic repository inventory without retranscribing identifiers. create freezes a scope object and a task-supplied profile (categories, classifications, required_categories); returns inventory_id and summary.scope_id. observe checks caller-selected paths in the bound environment, records exact identifiers, existence, Git tracking (unknown when unavailable), and content revisions without copying file bodies. Set fingerprint_contents=false for path-only inventories (revision is null; file contents are not read). observe accepts a full enumeration of up to 10000 paths, matching the inventory capacity. Pass complete=true only for a full category enumeration; incomplete batches require unresolved_reason. Alternatively, use a chosen enumeration command to emit exact JSON {scope_id,category,complete,unresolved_reason,candidates:[{id,exists,tracking,revision}]}, where exists is boolean/null, tracking is tracked/untracked/unknown, and revision is the relevant source hash or null. Import its original raw-output artifact via source {artifact_id,pointer}; pointer defaults to the JSON root. Import deduplicates exact records, rejects conflicting IDs/scope, merges incomplete batches without deletion, and reconciles complete enumerations with explicit added/changed/removed evidence. Classify existing candidates using profile labels and supporting source evidence for that particular claim; an enumeration or file existence does not prove runtime use. Inventory record references resolve to source evidence for the same scope, candidate revision, and classification; bookkeeping-only, cyclic, and mismatched chains are rejected. Mechanical chain validation does not establish semantic support: inspect actual source and consumers before classifying. Evidence references accept a JSON pointer or inclusive 1-based lines [start,end] from a text artifact, including read_file snapshots. read pages bounded records; unresolved_only=true skips classified and removed records, with offsets relative to that filtered snapshot. render returns inline identifiers for small results and produces an exact identifier/count artifact with explicit classification filters and per-category coverage outcomes, reasons, and provenance at /coverage. Update receipts report progress as resolved/reopened records and required categories, not tool-call counts. Always use the returned inventory_id: updates create immutable snapshots, so concurrent updates branch and never overwrite each other. Unchanged imports reuse the prior snapshot. Evidence describes producer snapshots, not automatically fresh filesystem state. Scope, vocabulary, enumeration completeness, and semantic classification remain the caller's decisions; summary.complete covers required categories and their records; all_candidates_classified separately reports whether optional records are also resolved, not an independent check of the user's request.".to_string(),
            strict: false, defer_loading: None, parameters, output_schema: None,
        })
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let ToolPayload::Function { arguments } = &invocation.payload else {
                return Err(invalid("inventory requires function arguments"));
            };
            let args = parse_arguments(arguments)?;
            if invocation.cancellation_token.is_cancelled() {
                return Err(invalid("inventory operation cancelled"));
            }
            let mut canonical_render = None;
            let result = execute(&invocation, args, &mut canonical_render).await?;
            if let Some(canonical) = canonical_render {
                let artifact_id = result["rendered_artifact_id"]
                    .as_str()
                    .ok_or_else(|| invalid("rendered inventory has no artifact identity"))?
                    .to_string();
                return Ok(boxed_tool_output(RenderedInventoryOutput {
                    delivery: json!({"rendered_path": result["rendered_path"], "count": result["count"],
                        "scope_id": result["summary"]["scope_id"], "complete": result["summary"]["complete"]}),
                    receipt: JsonToolOutput::new(result),
                    canonical,
                    artifact_id,
                }));
            }
            Ok(boxed_tool_output(JsonToolOutput::new(result)))
        })
    }
}

impl CoreToolRuntime for RetainedInventoryHandler {}

#[cfg(test)]
#[path = "retained_inventory_tests.rs"]
mod tests;
