use crate::context::PlannerContext;
use crate::context::CargoGraph;
use crate::model::RepositorySnapshot;
use crate::model::SnapshotRecord;
use crate::model::SnapshotSource;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use tempfile::NamedTempFile;
use thiserror::Error;

const CI_SCHEMA_VERSION: u64 = 1;
const ARTIFACT_NAME: &str = "verify-local-ci-decision";
const WORKFLOWS: [&str; 7] = [
    "blob-size-policy",
    "cargo-deny",
    "cargo-full",
    "codespell",
    "repo-checks",
    "rust-ci",
    "sdk",
];

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowDecision {
    pub id: String,
    pub run: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MatrixPlan {
    pub rust_packages: Vec<String>,
    pub rust_shards: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CiDecisionBody {
    pub schema_version: u64,
    pub event: String,
    pub comparison_mode: String,
    pub base: Option<String>,
    pub merge_base: Option<String>,
    pub head: Option<String>,
    pub changes: Vec<SnapshotRecord>,
    pub full_fallback: bool,
    pub fallback_reasons: Vec<String>,
    pub workflows: Vec<WorkflowDecision>,
    pub affected_packages: Vec<String>,
    pub reverse_closure: Vec<String>,
    pub matrix: MatrixPlan,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CiDecisionOutputs {
    pub decision_id: String,
    pub artifact_name: String,
    pub full_fallback: bool,
    pub workflows: Vec<WorkflowDecision>,
    pub rust_matrix_json: String,
    pub rust_shards_json: String,
}

impl CiDecisionOutputs {
    pub fn workflow(&self, id: &str) -> Option<bool> {
        self.workflows
            .iter()
            .find(|decision| decision.id == id)
            .map(|decision| decision.run)
    }

    pub fn utf16_budget_bytes(&self) -> usize {
        let mut records = vec![
            ("decision_id".to_string(), self.decision_id.clone()),
            ("artifact_name".to_string(), self.artifact_name.clone()),
            ("full_fallback".to_string(), self.full_fallback.to_string()),
            ("rust_matrix".to_string(), self.rust_matrix_json.clone()),
            ("rust_shards".to_string(), self.rust_shards_json.clone()),
        ];
        records.extend(self.workflows.iter().map(|workflow| {
            (
                format!("run_{}", workflow.id.replace('-', "_")),
                workflow.run.to_string(),
            )
        }));
        records
            .into_iter()
            .map(|(name, value)| {
                format!("{name}={value}\n").encode_utf16().count() * 2
            })
            .sum()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CiDecisionArtifact {
    pub body: CiDecisionBody,
    pub bytes: Vec<u8>,
    pub outputs: CiDecisionOutputs,
}

#[derive(Debug, Error)]
pub enum CiDecisionError {
    #[error("failed to load verifier repository context: {0}")]
    Context(String),
    #[error("failed to serialize CI decision: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("decision artifact hash mismatch: expected {expected}, actual {actual}")]
    HashMismatch { expected: String, actual: String },
    #[error("invalid CI decision contract: {0}")]
    InvalidContract(String),
    #[error("failed to write decision artifact {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

pub fn build_ci_decision(
    repository_root: &Path,
    snapshot: RepositorySnapshot,
    event: impl Into<String>,
) -> Result<CiDecisionArtifact, CiDecisionError> {
    let context = PlannerContext::load(repository_root)
        .map_err(|error| CiDecisionError::Context(error.to_string()))?;
    build_ci_decision_with_context(&context, snapshot, event.into())
}

pub fn build_ci_decision_from_metadata(
    repository_root: &Path,
    mut snapshot: RepositorySnapshot,
    event: impl Into<String>,
    metadata: &serde_json::Value,
) -> Result<CiDecisionArtifact, CiDecisionError> {
    let context = match PlannerContext::from_cargo_metadata(repository_root, metadata) {
        Ok(context) => context,
        Err(error) => {
            snapshot.complete = false;
            snapshot
                .fallback_reasons
                .push(format!("Cargo metadata graph is unusable: {error}"));
            let repository_root = fs::canonicalize(repository_root)
                .map_err(|error| CiDecisionError::Context(error.to_string()))?;
            PlannerContext {
                workspace_root: repository_root.join("codex-rs"),
                repository_root,
                graph: CargoGraph::default(),
                rules: Vec::new(),
            }
        }
    };
    build_ci_decision_with_context(&context, snapshot, event.into())
}

fn build_ci_decision_with_context(
    context: &PlannerContext,
    mut snapshot: RepositorySnapshot,
    event: String,
) -> Result<CiDecisionArtifact, CiDecisionError> {
    snapshot.records.sort_by(|left, right| {
        left.status
            .as_bytes()
            .cmp(right.status.as_bytes())
            .then_with(|| left.path.as_bytes().cmp(right.path.as_bytes()))
            .then_with(|| {
                left.original_path
                    .as_ref()
                    .map(crate::model::RawPath::as_bytes)
                    .unwrap_or_default()
                    .cmp(
                        right
                            .original_path
                            .as_ref()
                            .map(crate::model::RawPath::as_bytes)
                            .unwrap_or_default(),
                    )
            })
    });
    let mut body = classify(context, snapshot, event);
    if body
        .fallback_reasons
        .iter()
        .any(|reason| reason == "Rust package matrix exceeded its bounded output budget")
    {
        body = full_suite_replacement(body, "GitHub output budget exceeded");
    }
    let mut bytes = canonical_body_bytes(&body)?;
    let mut outputs = outputs_for(&body, &bytes)?;
    if matrix_exceeds_budget(&body.matrix) || outputs.utf16_budget_bytes() > 64 * 1024 {
        body = full_suite_replacement(body, "GitHub output budget exceeded");
        bytes = canonical_body_bytes(&body)?;
        outputs = outputs_for(&body, &bytes)?;
    }
    Ok(CiDecisionArtifact {
        body,
        bytes,
        outputs,
    })
}

pub fn verify_ci_decision_artifact(
    bytes: &[u8],
    expected_decision_id: &str,
) -> Result<CiDecisionBody, CiDecisionError> {
    let actual = decision_id(bytes);
    if actual != expected_decision_id {
        return Err(CiDecisionError::HashMismatch {
            expected: expected_decision_id.to_string(),
            actual,
        });
    }
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let body = CiDecisionBody::deserialize(&mut deserializer)?;
    deserializer.end()?;
    validate_decision_body(&body)?;
    Ok(body)
}

pub fn write_ci_decision_artifact(
    artifact: &CiDecisionArtifact,
    destination: &Path,
) -> Result<(), CiDecisionError> {
    if decision_id(&artifact.bytes) != artifact.outputs.decision_id {
        return Err(CiDecisionError::InvalidContract(
            "artifact bytes do not match the advertised decision_id".to_string(),
        ));
    }
    let parsed = verify_ci_decision_artifact(&artifact.bytes, &artifact.outputs.decision_id)?;
    if parsed != artifact.body {
        return Err(CiDecisionError::InvalidContract(
            "artifact bytes do not match the in-memory body".to_string(),
        ));
    }
    let parent = destination.parent().ok_or_else(|| CiDecisionError::Write {
        path: destination.to_path_buf(),
        source: std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "artifact destination has no parent",
        ),
    })?;
    fs::create_dir_all(parent).map_err(|source| CiDecisionError::Write {
        path: parent.to_path_buf(),
        source,
    })?;
    let mut temporary = NamedTempFile::new_in(parent).map_err(|source| CiDecisionError::Write {
        path: parent.to_path_buf(),
        source,
    })?;
    temporary
        .write_all(&artifact.bytes)
        .and_then(|_| temporary.as_file().sync_all())
        .map_err(|source| CiDecisionError::Write {
            path: destination.to_path_buf(),
            source,
        })?;
    temporary
        .persist(destination)
        .map_err(|error| CiDecisionError::Write {
            path: destination.to_path_buf(),
            source: error.error,
        })?;
    Ok(())
}

fn classify(
    context: &PlannerContext,
    snapshot: RepositorySnapshot,
    event: String,
) -> CiDecisionBody {
    let (comparison_mode, base, merge_base, head) = match &snapshot.source {
        SnapshotSource::CommitDiff {
            base,
            head,
            merge_base,
            pull_request,
        } => (
            if *pull_request {
                "pull_request_merge_base".to_string()
            } else {
                "direct_commit_diff".to_string()
            },
            base.clone(),
            merge_base.clone(),
            head.clone(),
        ),
        SnapshotSource::Worktree => ("worktree".to_string(), None, None, None),
        SnapshotSource::ExplicitPaths => ("explicit_paths".to_string(), None, None, None),
    };
    let mut workflow = WORKFLOWS
        .into_iter()
        .map(|id| (id.to_string(), false))
        .collect::<BTreeMap<_, _>>();
    let mut affected = BTreeSet::new();
    let mut fallback_reasons = snapshot.fallback_reasons.clone();
    let mut unknown = false;

    for record in &snapshot.records {
        for path in std::iter::once(&record.path).chain(record.original_path.iter()) {
            let Some(text) = path.as_utf8() else {
                fallback_reasons.push("non-UTF-8 change cannot be classified safely".to_string());
                unknown = true;
                continue;
            };
            *workflow
                .get_mut("blob-size-policy")
                .expect("known workflow") = true;
            *workflow.get_mut("repo-checks").expect("known workflow") = true;
            if is_manifest_or_shared_rust(text) {
                fallback_reasons.push(format!("shared Rust or manifest change: {text}"));
                continue;
            }
            if text.starts_with(".github/workflows/") {
                fallback_reasons.push(format!("workflow definition changed: {text}"));
                continue;
            }
            if text.starts_with("sdk/") {
                *workflow.get_mut("sdk").expect("known workflow") = true;
                *workflow.get_mut("codespell").expect("known workflow") = true;
                continue;
            }
            if text.starts_with("docs/") || text.ends_with(".md") {
                *workflow.get_mut("codespell").expect("known workflow") = true;
                continue;
            }
            if text.starts_with("codex-rs/") {
                if let Some(package) = context
                    .graph
                    .package_for_path(&context.repository_root, path)
                {
                    affected.insert(package.name.clone());
                    *workflow.get_mut("rust-ci").expect("known workflow") = true;
                    continue;
                }
                fallback_reasons.push(format!("Rust path has unknown ownership: {text}"));
                unknown = true;
                continue;
            }
            if text.starts_with("scripts/") {
                *workflow.get_mut("codespell").expect("known workflow") = true;
                if text.starts_with("scripts/verify_local")
                    || text == "scripts/verify_local_rules.toml"
                {
                    fallback_reasons.push(format!("verifier infrastructure changed: {text}"));
                }
                continue;
            }
            if matches!(
                text,
                "README.md"
                    | "LICENSE"
                    | "NOTICE"
                    | "package.json"
                    | "pnpm-lock.yaml"
                    | "pnpm-workspace.yaml"
                    | "justfile"
            ) {
                *workflow.get_mut("codespell").expect("known workflow") = true;
                continue;
            }
            fallback_reasons.push(format!("unknown changed path: {text}"));
            unknown = true;
        }
    }

    if context.graph.has_cycle() && !affected.is_empty() {
        fallback_reasons.push("workspace dependency graph contains a cycle".to_string());
    }
    let mut full_fallback = !snapshot.complete
        || unknown
        || !fallback_reasons.is_empty()
        || snapshot.records.is_empty();
    let reverse_closure = context
        .graph
        .reverse_closure(&affected.iter().cloned().collect::<Vec<_>>());
    let affected_packages = affected.into_iter().collect::<Vec<_>>();
    let reverse_closure = reverse_closure.into_iter().collect::<Vec<_>>();
    let mut matrix = MatrixPlan {
        rust_packages: reverse_closure.clone(),
        rust_shards: reverse_closure
            .iter()
            .enumerate()
            .map(|(index, _)| format!("rust-{index:03}"))
            .collect(),
    };
    if matrix_exceeds_budget(&matrix) {
        full_fallback = true;
        fallback_reasons.push("Rust package matrix exceeded its bounded output budget".to_string());
    }
    if full_fallback {
        for run in workflow.values_mut() {
            *run = true;
        }
        matrix = MatrixPlan {
            rust_packages: Vec::new(),
            rust_shards: vec!["workspace".to_string()],
        };
    } else if !affected_packages.is_empty() {
        *workflow.get_mut("cargo-deny").expect("known workflow") = true;
    }
    let workflows = WORKFLOWS
        .into_iter()
        .map(|id| WorkflowDecision {
            id: id.to_string(),
            run: workflow[id],
        })
        .collect();
    fallback_reasons.sort();
    fallback_reasons.dedup();
    CiDecisionBody {
        schema_version: CI_SCHEMA_VERSION,
        event,
        comparison_mode,
        base,
        merge_base,
        head,
        changes: snapshot.records,
        full_fallback,
        fallback_reasons,
        workflows,
        affected_packages,
        reverse_closure,
        matrix,
    }
}

fn is_manifest_or_shared_rust(path: &str) -> bool {
    path.ends_with("/Cargo.toml")
        || path == "codex-rs/Cargo.toml"
        || path == "codex-rs/Cargo.lock"
        || path.ends_with("rust-toolchain")
        || path.ends_with("rust-toolchain.toml")
        || path.contains("/.cargo/config")
}

fn full_suite_replacement(mut body: CiDecisionBody, reason: &str) -> CiDecisionBody {
    body.full_fallback = true;
    body.changes.clear();
    body.affected_packages.clear();
    body.reverse_closure.clear();
    body.fallback_reasons = vec![reason.to_string()];
    for workflow in &mut body.workflows {
        workflow.run = true;
    }
    body.matrix = MatrixPlan {
        rust_packages: Vec::new(),
        rust_shards: vec!["workspace".to_string()],
    };
    body
}

fn validate_decision_body(body: &CiDecisionBody) -> Result<(), CiDecisionError> {
    if body.schema_version != CI_SCHEMA_VERSION {
        return Err(CiDecisionError::InvalidContract(format!(
            "unsupported schema version {}",
            body.schema_version
        )));
    }
    let mut workflow_ids = BTreeSet::new();
    for workflow in &body.workflows {
        if !WORKFLOWS.contains(&workflow.id.as_str()) || !workflow_ids.insert(&workflow.id) {
            return Err(CiDecisionError::InvalidContract(format!(
                "unknown or duplicate workflow {}",
                workflow.id
            )));
        }
    }
    if workflow_ids.len() != WORKFLOWS.len() {
        return Err(CiDecisionError::InvalidContract(
            "workflow decision set is incomplete".to_string(),
        ));
    }
    if matrix_exceeds_budget(&body.matrix) {
        return Err(CiDecisionError::InvalidContract(
            "matrix exceeds the transport budget".to_string(),
        ));
    }
    for record in &body.changes {
        record
            .path
            .validate_repository_relative()
            .map_err(CiDecisionError::InvalidContract)?;
        if let Some(original) = &record.original_path {
            original
                .validate_repository_relative()
                .map_err(CiDecisionError::InvalidContract)?;
        }
    }
    for oid in [body.base.as_deref(), body.merge_base.as_deref(), body.head.as_deref()]
        .into_iter()
        .flatten()
    {
        if !matches!(oid.len(), 40 | 64)
            || !oid.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(CiDecisionError::InvalidContract(
                "comparison object ID is not canonical lowercase hex".to_string(),
            ));
        }
    }
    if body.full_fallback
        && (!body.workflows.iter().all(|workflow| workflow.run)
            || !body.matrix.rust_packages.is_empty()
            || body.matrix.rust_shards != ["workspace"])
    {
        return Err(CiDecisionError::InvalidContract(
            "full fallback does not select the complete suite".to_string(),
        ));
    }
    Ok(())
}

fn matrix_exceeds_budget(matrix: &MatrixPlan) -> bool {
    matrix.rust_packages.len() > 128
        || matrix.rust_shards.len() > 32
        || serde_json::to_vec(&matrix.rust_packages)
            .map(|bytes| bytes.len() > 32 * 1024)
            .unwrap_or(true)
}

fn outputs_for(body: &CiDecisionBody, bytes: &[u8]) -> Result<CiDecisionOutputs, CiDecisionError> {
    Ok(CiDecisionOutputs {
        decision_id: decision_id(bytes),
        artifact_name: ARTIFACT_NAME.to_string(),
        full_fallback: body.full_fallback,
        workflows: body.workflows.clone(),
        rust_matrix_json: serde_json::to_string(&body.matrix.rust_packages)?,
        rust_shards_json: serde_json::to_string(&body.matrix.rust_shards)?,
    })
}

fn canonical_body_bytes(body: &CiDecisionBody) -> Result<Vec<u8>, serde_json::Error> {
    let mut bytes = serde_json::to_vec_pretty(body)?;
    bytes.push(b'\n');
    Ok(bytes)
}

pub fn decision_id(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
#[path = "ci_tests.rs"]
mod tests;
