//! Small, fail-open evidence cache for immutable tool reads.
//!
//! Blobs and evidence are global below the existing `tool-output` root. Retrieval
//! handles remain task-scoped and are minted by `command_output_artifact`.

use crate::tools::command_output_artifact::RawOutputArtifact;
use crate::tools::command_output_artifact::create_raw_output_artifact;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::timeout;
use uuid::Uuid;

pub(crate) const EVIDENCE_SCHEMA_VERSION: u32 = 1;
const GIT_TIMEOUT: Duration = Duration::from_secs(2);
const STORE_DIRECTORY: &str = "known-delta";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EvidenceIdentity {
    pub project_namespace: String,
    pub lineage_key: String,
    pub fingerprint: String,
    pub provenance: String,
    pub fingerprint_cost: Duration,
}

#[derive(Clone, Debug)]
pub(crate) struct EvidenceCandidate {
    record: EvidenceRecord,
    pub output: Vec<u8>,
    pub lookup_cost: Duration,
}

impl EvidenceCandidate {
    pub(crate) fn reusable(&self) -> bool {
        self.record.reusable
    }

    pub(crate) fn age(&self) -> Duration {
        Duration::from_millis(now_ms().saturating_sub(self.record.created_at_ms))
    }

    pub(crate) fn provenance(&self) -> &str {
        &self.record.provenance
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Observation {
    Published,
    Unchanged { reuse_enabled: bool },
    Contradiction,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct EvidenceRecord {
    schema_version: u32,
    project_namespace: String,
    lineage_key: String,
    fingerprint: String,
    outcome: EvidenceOutcome,
    provenance: String,
    blob_digest: String,
    created_at_ms: u64,
    shadow_validations: u32,
    reusable: bool,
    metrics: EvidenceMetrics,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum EvidenceOutcome {
    Success,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct EvidenceMetrics {
    lookup_micros: u64,
    fingerprint_micros: u64,
    projected_tokens_avoided: u64,
    executor_micros_avoided: u64,
}

pub(crate) async fn immutable_file_identity(
    repo_root: &Path,
    relative_path: &str,
    start_line: usize,
    line_count: usize,
) -> Option<EvidenceIdentity> {
    let started = Instant::now();
    let relative_path = normalize_relative_path(relative_path)?;
    let metadata = tokio::fs::symlink_metadata(repo_root.join(&relative_path))
        .await
        .ok()?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return None;
    }
    let project_namespace = git_project_namespace(repo_root).await?;
    let object_spec = format!("HEAD:{relative_path}");
    let blob = git_stdout(repo_root, &["rev-parse", &object_spec]).await?;
    if !is_resolved_object(&blob) {
        return None;
    }
    if !git_success(
        repo_root,
        &[
            "diff",
            "--no-ext-diff",
            "--quiet",
            "HEAD",
            "--",
            &relative_path,
        ],
    )
    .await
    {
        return None;
    }

    let lineage_key = digest(format!("read_file_span\0{relative_path}").as_bytes());
    let fingerprint = digest(
        format!(
            "schema={EVIDENCE_SCHEMA_VERSION}\0project={project_namespace}\0op=read_file_span\0cwd=.\0path={relative_path}\0start={start_line}\0count={line_count}\0blob={blob}"
        )
        .as_bytes(),
    );
    Some(EvidenceIdentity {
        project_namespace,
        lineage_key,
        fingerprint,
        provenance: format!("git-clean:{relative_path}@{blob}"),
        fingerprint_cost: started.elapsed(),
    })
}

pub(crate) async fn immutable_git_show_identity(
    cwd: &Path,
    program: &str,
    args: &[String],
) -> Option<EvidenceIdentity> {
    let started = Instant::now();
    if !is_git_program(program) || args.len() != 2 || args[0] != "show" {
        return None;
    }
    let requested = &args[1];
    let (object, suffix) = requested
        .split_once(':')
        .unwrap_or((requested.as_str(), ""));
    if !is_resolved_object(object)
        || (!suffix.is_empty() && normalize_relative_path(suffix).is_none())
    {
        return None;
    }
    let project_namespace = git_project_namespace(cwd).await?;
    let cwd_position = git_stdout(cwd, &["rev-parse", "--show-prefix"])
        .await
        .unwrap_or_default();
    let resolved_blob = git_stdout(cwd, &["rev-parse", "--verify", requested]).await?;
    if !is_resolved_object(&resolved_blob)
        || git_stdout(cwd, &["cat-file", "-t", &resolved_blob]).await? != "blob"
    {
        return None;
    }
    let lineage_key = digest(format!("git_show_resolved_object\0{suffix}").as_bytes());
    let fingerprint = digest(
        format!(
            "schema={EVIDENCE_SCHEMA_VERSION}\0project={project_namespace}\0op=git_show\0cwd={cwd_position}\0object={requested}\0blob={resolved_blob}"
        )
        .as_bytes(),
    );
    Some(EvidenceIdentity {
        project_namespace,
        lineage_key,
        fingerprint,
        provenance: format!("git-object:{requested}"),
        fingerprint_cost: started.elapsed(),
    })
}

pub(crate) async fn lookup(
    codex_home: &Path,
    identity: &EvidenceIdentity,
) -> Option<EvidenceCandidate> {
    let started = Instant::now();
    if tokio::fs::try_exists(unsafe_path(codex_home, identity))
        .await
        .ok()?
    {
        return None;
    }
    let bytes = tokio::fs::read(evidence_path(codex_home, identity))
        .await
        .ok()?;
    let record: EvidenceRecord = serde_json::from_slice(&bytes).ok()?;
    if record.schema_version != EVIDENCE_SCHEMA_VERSION
        || record.project_namespace != identity.project_namespace
        || record.lineage_key != identity.lineage_key
        || record.fingerprint != identity.fingerprint
        || record.outcome != EvidenceOutcome::Success
    {
        return None;
    }
    let output = tokio::fs::read(blob_path(codex_home, &record.blob_digest))
        .await
        .ok()?;
    if digest(&output) != record.blob_digest {
        return None;
    }
    Some(EvidenceCandidate {
        record,
        output,
        lookup_cost: started.elapsed(),
    })
}

pub(crate) async fn record_success(
    codex_home: &Path,
    identity: &EvidenceIdentity,
    candidate: Option<&EvidenceCandidate>,
    output: &[u8],
    executor_cost: Duration,
) -> Observation {
    let blob_digest = digest(output);
    if let Some(candidate) = candidate {
        if candidate.record.blob_digest != blob_digest || candidate.output != output {
            quarantine(codex_home, identity).await;
            return Observation::Contradiction;
        }
        let mut record = candidate.record.clone();
        record.shadow_validations = record.shadow_validations.saturating_add(1);
        record.metrics = EvidenceMetrics {
            lookup_micros: micros(candidate.lookup_cost),
            fingerprint_micros: micros(identity.fingerprint_cost),
            projected_tokens_avoided: u64::try_from(output.len().div_ceil(4)).unwrap_or(u64::MAX),
            executor_micros_avoided: micros(executor_cost),
        };
        record.reusable = record.shadow_validations > 0
            && record.metrics.projected_tokens_avoided > 0
            && record.metrics.executor_micros_avoided
                > record
                    .metrics
                    .lookup_micros
                    .saturating_add(record.metrics.fingerprint_micros);
        let reuse_enabled = record.reusable;
        if write_record(codex_home, identity, &record).await.is_none() {
            return Observation::Published;
        }
        return Observation::Unchanged { reuse_enabled };
    }

    if write_blob(codex_home, &blob_digest, output).await.is_none() {
        return Observation::Published;
    }
    let record = EvidenceRecord {
        schema_version: EVIDENCE_SCHEMA_VERSION,
        project_namespace: identity.project_namespace.clone(),
        lineage_key: identity.lineage_key.clone(),
        fingerprint: identity.fingerprint.clone(),
        outcome: EvidenceOutcome::Success,
        provenance: identity.provenance.clone(),
        blob_digest,
        created_at_ms: now_ms(),
        shadow_validations: 0,
        reusable: false,
        metrics: EvidenceMetrics::default(),
    };
    let _ = write_record(codex_home, identity, &record).await;
    Observation::Published
}

pub(crate) async fn record_contradictory_failure(
    codex_home: &Path,
    identity: &EvidenceIdentity,
    had_candidate: bool,
) {
    if had_candidate {
        quarantine(codex_home, identity).await;
    }
}

pub(crate) async fn remint_task_handle(
    codex_home: &Path,
    thread_id: &str,
    candidate: &EvidenceCandidate,
) -> RawOutputArtifact {
    create_raw_output_artifact(codex_home, thread_id, &candidate.output).await
}

pub(crate) fn render_hit(candidate: &EvidenceCandidate, artifact: &RawOutputArtifact) -> String {
    let content = String::from_utf8_lossy(&candidate.output);
    format!(
        "{content}\n\n[known-delta cache hit; age={}ms; provenance={}; {}]",
        candidate.age().as_millis(),
        candidate.provenance(),
        artifact.render_for_model()
    )
}

fn store_root(codex_home: &Path) -> PathBuf {
    codex_home.join("tool-output").join(STORE_DIRECTORY)
}

fn evidence_path(codex_home: &Path, identity: &EvidenceIdentity) -> PathBuf {
    store_root(codex_home)
        .join("evidence")
        .join(&identity.project_namespace)
        .join(format!("{}.json", identity.fingerprint))
}

fn blob_path(codex_home: &Path, blob_digest: &str) -> PathBuf {
    // Keep cache blobs in the shape consumed by the existing global
    // tool-output retention sweep instead of introducing another budget.
    store_root(codex_home).join(format!("{blob_digest}.log"))
}

fn unsafe_path(codex_home: &Path, identity: &EvidenceIdentity) -> PathBuf {
    store_root(codex_home)
        .join("unsafe")
        .join(&identity.project_namespace)
        .join(format!("{}.unsafe", identity.lineage_key))
}

async fn write_blob(codex_home: &Path, blob_digest: &str, output: &[u8]) -> Option<()> {
    let path = blob_path(codex_home, blob_digest);
    if let Ok(existing) = tokio::fs::read(&path).await
        && digest(&existing) == blob_digest
    {
        return Some(());
    }
    atomic_write(&path, output).await?;
    let verified = tokio::fs::read(&path).await.ok()?;
    (digest(&verified) == blob_digest).then_some(())
}

async fn write_record(
    codex_home: &Path,
    identity: &EvidenceIdentity,
    record: &EvidenceRecord,
) -> Option<()> {
    let bytes = serde_json::to_vec(record).ok()?;
    atomic_write(&evidence_path(codex_home, identity), &bytes).await
}

async fn atomic_write(path: &Path, bytes: &[u8]) -> Option<()> {
    let parent = path.parent()?;
    tokio::fs::create_dir_all(parent).await.ok()?;
    let temporary = parent.join(format!(".{}.tmp", Uuid::new_v4()));
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .await
        .ok()?;
    if file.write_all(bytes).await.is_err() || file.sync_all().await.is_err() {
        let _ = tokio::fs::remove_file(&temporary).await;
        return None;
    }
    drop(file);
    if tokio::fs::rename(&temporary, path).await.is_err() {
        let _ = tokio::fs::remove_file(path).await;
        if tokio::fs::rename(&temporary, path).await.is_err() {
            let _ = tokio::fs::remove_file(&temporary).await;
            return None;
        }
    }
    Some(())
}

async fn quarantine(codex_home: &Path, identity: &EvidenceIdentity) {
    let evidence = evidence_path(codex_home, identity);
    if tokio::fs::try_exists(&evidence).await.unwrap_or(false) {
        let quarantine = evidence.with_extension(format!("quarantine-{}", now_ms()));
        let _ = tokio::fs::rename(&evidence, quarantine).await;
    }
    let _ = atomic_write(&unsafe_path(codex_home, identity), b"unsafe\n").await;
}

async fn git_stdout(cwd: &Path, args: &[&str]) -> Option<String> {
    let mut command = Command::new("git");
    command
        .args(["-c", "core.hooksPath=NUL", "-c", "core.fsmonitor=false"])
        .args(args)
        .current_dir(cwd)
        .kill_on_drop(true);
    let output = timeout(GIT_TIMEOUT, command.output()).await.ok()?.ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()
        .map(|value| value.trim().replace('\\', "/"))
}

async fn git_project_namespace(cwd: &Path) -> Option<String> {
    let mut roots = git_stdout(cwd, &["rev-list", "--max-parents=0", "HEAD"])
        .await?
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if roots.is_empty() || roots.iter().any(|root| !is_resolved_object(root)) {
        return None;
    }
    roots.sort_unstable();
    Some(digest(
        format!("git-project-roots-v1\0{}", roots.join("\0")).as_bytes(),
    ))
}

async fn git_success(cwd: &Path, args: &[&str]) -> bool {
    let mut command = Command::new("git");
    command
        .args(["-c", "core.hooksPath=NUL", "-c", "core.fsmonitor=false"])
        .args(args)
        .current_dir(cwd)
        .kill_on_drop(true);
    timeout(GIT_TIMEOUT, command.status())
        .await
        .ok()
        .and_then(Result::ok)
        .is_some_and(|status| status.success())
}

fn normalize_relative_path(path: &str) -> Option<String> {
    let path = Path::new(path);
    if path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return None;
    }
    Some(path.to_string_lossy().replace('\\', "/"))
}

fn is_git_program(program: &str) -> bool {
    Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name.eq_ignore_ascii_case("git") || name.eq_ignore_ascii_case("git.exe")
        })
}

fn is_resolved_object(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use tempfile::TempDir;

    fn identity(project: &str, lineage: &str, fingerprint: &str) -> EvidenceIdentity {
        EvidenceIdentity {
            project_namespace: project.to_string(),
            lineage_key: lineage.to_string(),
            fingerprint: fingerprint.to_string(),
            provenance: "git-clean:src/lib.rs@0123456789012345678901234567890123456789".to_string(),
            fingerprint_cost: Duration::ZERO,
        }
    }

    fn run_git(cwd: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_AUTHOR_NAME", "KD4 Test")
            .env("GIT_AUTHOR_EMAIL", "kd4@example.invalid")
            .env("GIT_COMMITTER_NAME", "KD4 Test")
            .env("GIT_COMMITTER_EMAIL", "kd4@example.invalid")
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("git output is UTF-8")
            .trim()
            .to_string()
    }

    fn init_repo(path: &Path, content: &str) {
        std::fs::create_dir_all(path).unwrap();
        run_git(path, &["init"]);
        std::fs::write(path.join("read.txt"), content).unwrap();
        run_git(path, &["add", "read.txt"]);
        run_git(path, &["commit", "-m", "initial"]);
    }

    #[tokio::test]
    async fn related_worktrees_share_identity_but_unrelated_projects_do_not() {
        let root = TempDir::new().unwrap();
        let repo = root.path().join("repo");
        let worktree = root.path().join("worktree");
        let unrelated = root.path().join("unrelated");
        init_repo(&repo, "shared\n");
        run_git(
            &repo,
            &[
                "worktree",
                "add",
                "--detach",
                worktree.to_str().unwrap(),
                "HEAD",
            ],
        );
        init_repo(&unrelated, "different\n");

        let first = immutable_file_identity(&repo, "read.txt", 1, 20)
            .await
            .unwrap();
        let related = immutable_file_identity(&worktree, "read.txt", 1, 20)
            .await
            .unwrap();
        let other = immutable_file_identity(&unrelated, "read.txt", 1, 20)
            .await
            .unwrap();
        assert_eq!(first.project_namespace, related.project_namespace);
        assert_eq!(first.fingerprint, related.fingerprint);
        assert_ne!(first.project_namespace, other.project_namespace);
        assert!(
            !first
                .provenance
                .contains(root.path().to_string_lossy().as_ref())
        );

        let blob = run_git(&repo, &["rev-parse", "HEAD:read.txt"]);
        assert!(
            immutable_git_show_identity(&repo, "git", &["show".to_string(), blob])
                .await
                .is_some()
        );
    }

    #[tokio::test]
    async fn shadow_match_promotes_and_cross_task_handle_is_readable() {
        let home = TempDir::new().unwrap();
        let id = identity("project", "lineage", "fingerprint");
        assert_eq!(
            record_success(
                home.path(),
                &id,
                None,
                b"complete output",
                Duration::from_secs(1)
            )
            .await,
            Observation::Published
        );
        let candidate = lookup(home.path(), &id).await.unwrap();
        assert!(!candidate.reusable());
        assert_eq!(
            record_success(
                home.path(),
                &id,
                Some(&candidate),
                b"complete output",
                Duration::from_secs(1),
            )
            .await,
            Observation::Unchanged {
                reuse_enabled: true
            }
        );
        let candidate = lookup(home.path(), &id).await.unwrap();
        assert!(candidate.reusable());
        let artifact = remint_task_handle(home.path(), "new-task", &candidate).await;
        let (artifact_id, _, error) = artifact.model_projection();
        assert!(error.is_none());
        let artifact_id = artifact_id.unwrap();
        let retained = crate::tools::command_output_artifact::read_tool_output_artifact(
            home.path(),
            "new-task",
            &artifact_id.to_string(),
            1,
            1,
            16_384,
        )
        .await
        .expect("reminted artifact should be readable by the new task");
        assert!(retained.contains("complete output"));
        assert!(
            crate::tools::command_output_artifact::read_tool_output_artifact(
                home.path(),
                "other-task",
                &artifact_id.to_string(),
                1,
                1,
                16_384,
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn projects_and_exact_fingerprints_are_isolated_from_lineage() {
        let home = TempDir::new().unwrap();
        let first = identity("project-a", "same-lineage", "fingerprint-a");
        record_success(home.path(), &first, None, b"a", Duration::from_millis(1)).await;
        assert!(
            lookup(
                home.path(),
                &identity("project-b", "same-lineage", "fingerprint-a")
            )
            .await
            .is_none()
        );
        assert!(
            lookup(
                home.path(),
                &identity("project-a", "same-lineage", "fingerprint-b")
            )
            .await
            .is_none()
        );
        assert!(
            lookup(
                home.path(),
                &identity("project-a", "other-lineage", "fingerprint-a")
            )
            .await
            .is_none()
        );
        assert!(lookup(home.path(), &first).await.is_some());
    }

    #[tokio::test]
    async fn contradictory_force_fresh_failure_quarantines_cached_success() {
        let home = TempDir::new().unwrap();
        let id = identity("project", "lineage", "fingerprint");
        record_success(home.path(), &id, None, b"success", Duration::from_millis(1)).await;
        assert!(lookup(home.path(), &id).await.is_some());

        record_contradictory_failure(home.path(), &id, true).await;

        assert!(lookup(home.path(), &id).await.is_none());
    }

    #[tokio::test]
    async fn contradiction_quarantines_the_entire_lineage() {
        let home = TempDir::new().unwrap();
        let first = identity("project", "lineage", "fingerprint-a");
        record_success(home.path(), &first, None, b"a", Duration::from_millis(1)).await;
        let candidate = lookup(home.path(), &first).await.unwrap();
        assert_eq!(
            record_success(
                home.path(),
                &first,
                Some(&candidate),
                b"b",
                Duration::from_millis(1)
            )
            .await,
            Observation::Contradiction
        );
        assert!(lookup(home.path(), &first).await.is_none());
        assert!(
            lookup(
                home.path(),
                &identity("project", "lineage", "fingerprint-b")
            )
            .await
            .is_none()
        );
    }

    #[tokio::test]
    async fn corrupt_or_evicted_blob_is_a_miss_and_provenance_is_checkout_free() {
        let home = TempDir::new().unwrap();
        let id = identity("project", "lineage", "fingerprint");
        record_success(
            home.path(),
            &id,
            None,
            b"secret-free",
            Duration::from_millis(1),
        )
        .await;
        let candidate = lookup(home.path(), &id).await.unwrap();
        assert!(
            !candidate
                .provenance()
                .contains(home.path().to_string_lossy().as_ref())
        );
        let mut wrong_schema = candidate.record.clone();
        wrong_schema.schema_version = wrong_schema.schema_version.saturating_add(1);
        tokio::fs::write(
            evidence_path(home.path(), &id),
            serde_json::to_vec(&wrong_schema).unwrap(),
        )
        .await
        .unwrap();
        assert!(lookup(home.path(), &id).await.is_none());
        tokio::fs::write(
            evidence_path(home.path(), &id),
            serde_json::to_vec(&candidate.record).unwrap(),
        )
        .await
        .unwrap();
        tokio::fs::write(
            blob_path(home.path(), &candidate.record.blob_digest),
            b"corrupt",
        )
        .await
        .unwrap();
        assert!(lookup(home.path(), &id).await.is_none());
        tokio::fs::remove_file(blob_path(home.path(), &candidate.record.blob_digest))
            .await
            .unwrap();
        assert!(lookup(home.path(), &id).await.is_none());
    }

    #[test]
    fn git_show_requires_a_resolved_object_and_safe_relative_suffix() {
        assert!(is_resolved_object(
            "0123456789012345678901234567890123456789"
        ));
        assert!(!is_resolved_object("HEAD"));
        assert_eq!(
            normalize_relative_path("src/lib.rs"),
            Some("src/lib.rs".to_string())
        );
        assert_eq!(normalize_relative_path("../secret"), None);
    }
}
