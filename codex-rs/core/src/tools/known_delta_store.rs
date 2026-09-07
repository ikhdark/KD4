//! Small, fail-open evidence cache for immutable tool reads.
//!
//! Blobs and evidence are global below the existing `tool-output` root. Retrieval
//! handles remain task-scoped and are minted by `command_output_artifact`.

use crate::tools::command_output_artifact::RawOutputArtifact;
use crate::tools::command_output_artifact::create_raw_output_artifact;
use codex_file_system::FileSystemSandboxContext;
use codex_protocol::models::SandboxPermissions;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::timeout;
use uuid::Uuid;

#[cfg(test)]
pub(crate) mod test_observation {
    use std::future::Future;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    #[derive(Clone, Default)]
    struct Counters {
        immutable_git_show_identity_calls: Arc<AtomicUsize>,
        lookup_calls: Arc<AtomicUsize>,
        fingerprint_git_subprocesses: Arc<AtomicUsize>,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub(crate) struct Snapshot {
        pub immutable_git_show_identity_calls: usize,
        pub lookup_calls: usize,
        pub fingerprint_git_subprocesses: usize,
    }

    tokio::task_local! {
        static COUNTERS: Counters;
        static PROFITABILITY_COSTS: (Duration, Duration, Duration);
    }

    pub(crate) async fn observe<F: Future>(future: F) -> (F::Output, Snapshot) {
        let counters = Counters::default();
        let output = COUNTERS.scope(counters.clone(), future).await;
        let snapshot = Snapshot {
            immutable_git_show_identity_calls: counters
                .immutable_git_show_identity_calls
                .load(Ordering::Relaxed),
            lookup_calls: counters.lookup_calls.load(Ordering::Relaxed),
            fingerprint_git_subprocesses: counters
                .fingerprint_git_subprocesses
                .load(Ordering::Relaxed),
        };
        (output, snapshot)
    }

    pub(crate) async fn with_profitability_costs<F: Future>(
        future: F,
        lookup_cost: Duration,
        fingerprint_cost: Duration,
        executor_cost: Duration,
    ) -> F::Output {
        PROFITABILITY_COSTS
            .scope((lookup_cost, fingerprint_cost, executor_cost), future)
            .await
    }

    pub(super) fn profitability_costs(
        lookup_cost: Duration,
        fingerprint_cost: Duration,
        executor_cost: Duration,
    ) -> (Duration, Duration, Duration) {
        PROFITABILITY_COSTS.try_with(|costs| *costs).unwrap_or((
            lookup_cost,
            fingerprint_cost,
            executor_cost,
        ))
    }

    pub(super) fn configured_profitability_costs() -> Option<(Duration, Duration, Duration)> {
        PROFITABILITY_COSTS.try_with(|costs| *costs).ok()
    }

    pub(super) fn record_immutable_git_show_identity() {
        let _ = COUNTERS.try_with(|counters| {
            counters
                .immutable_git_show_identity_calls
                .fetch_add(1, Ordering::Relaxed);
        });
    }

    pub(super) fn record_lookup() {
        let _ = COUNTERS.try_with(|counters| {
            counters.lookup_calls.fetch_add(1, Ordering::Relaxed);
        });
    }

    pub(super) fn record_fingerprint_git_subprocess() {
        let _ = COUNTERS.try_with(|counters| {
            counters
                .fingerprint_git_subprocesses
                .fetch_add(1, Ordering::Relaxed);
        });
    }
}

pub(crate) const EVIDENCE_SCHEMA_VERSION: u32 = 2;
const GIT_TIMEOUT: Duration = Duration::from_secs(2);
const STORE_DIRECTORY: &str = "known-delta";
#[cfg(test)]
const TEST_AUTHORIZATION_SCOPE: &str = "known-delta-test-authorization-scope";

#[derive(Serialize)]
struct AuthorizationScope<'a> {
    schema_version: u32,
    file_system: &'a FileSystemSandboxContext,
    sandbox_permissions: SandboxPermissions,
}

/// Stable identity for the exact filesystem authorization used by execution.
/// A serialization failure disables Known Delta for that command instead of
/// allowing evidence to cross authorization boundaries.
pub(crate) fn authorization_scope_fingerprint(
    file_system: &FileSystemSandboxContext,
    sandbox_permissions: SandboxPermissions,
) -> Option<String> {
    serde_json::to_vec(&AuthorizationScope {
        schema_version: 1,
        file_system,
        sandbox_permissions,
    })
    .ok()
    .map(|scope| digest(&scope))
}

#[derive(Default)]
struct RuntimeQuarantineState {
    disabled_namespaces: HashSet<PathBuf>,
    unsafe_lineages: HashSet<PathBuf>,
    all_namespaces_disabled: bool,
}

const MAX_DISABLED_RUNTIME_NAMESPACES: usize = 64;

impl RuntimeQuarantineState {
    fn lookup_disabled(&self, namespace: &Path, unsafe_marker: &Path) -> bool {
        self.all_namespaces_disabled
            || self.disabled_namespaces.contains(namespace)
            || self.unsafe_lineages.contains(unsafe_marker)
    }

    fn mark_lineage_unsafe(&mut self, unsafe_marker: PathBuf) {
        self.unsafe_lineages.insert(unsafe_marker);
    }

    fn release_lineage(&mut self, unsafe_marker: &Path) {
        self.unsafe_lineages.remove(unsafe_marker);
    }

    fn disable_namespace(&mut self, namespace: PathBuf) {
        if self.all_namespaces_disabled || self.disabled_namespaces.contains(&namespace) {
            return;
        }
        if self.disabled_namespaces.len() >= MAX_DISABLED_RUNTIME_NAMESPACES {
            self.disabled_namespaces.clear();
            self.all_namespaces_disabled = true;
            return;
        }
        self.disabled_namespaces.insert(namespace);
    }
}

static RUNTIME_QUARANTINE_STATE: OnceLock<Mutex<RuntimeQuarantineState>> = OnceLock::new();

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

#[derive(Clone, Debug)]
pub(crate) struct KnownDeltaHit {
    rendered_output: String,
    raw_output_artifact: RawOutputArtifact,
}

impl KnownDeltaHit {
    pub(crate) fn rendered_output(&self) -> &str {
        &self.rendered_output
    }

    pub(crate) fn raw_output_artifact(&self) -> &RawOutputArtifact {
        &self.raw_output_artifact
    }
}

/// Immutable-read evidence prepared before command approval and execution.
///
/// The candidate is retained on a forced-fresh or shadow-validation launch so
/// the eventual exact result can either promote the evidence or quarantine a
/// contradiction. A reusable hit also carries a freshly minted task-scoped
/// artifact; cache blobs themselves never cross the task boundary directly.
#[derive(Clone, Debug)]
pub(crate) struct PreparedKnownDelta {
    identity: EvidenceIdentity,
    candidate: Option<EvidenceCandidate>,
    hit: Option<KnownDeltaHit>,
    force_fresh: bool,
    #[cfg(test)]
    profitability_costs: Option<(Duration, Duration, Duration)>,
}

impl PreparedKnownDelta {
    pub(crate) fn hit(&self) -> Option<&KnownDeltaHit> {
        self.hit.as_ref()
    }

    pub(crate) fn is_hit(&self) -> bool {
        self.hit.is_some()
    }

    #[cfg(test)]
    pub(crate) fn has_candidate(&self) -> bool {
        self.candidate.is_some()
    }
}

pub(crate) enum KnownDeltaExecutionObservation<'a> {
    CompleteSuccess {
        output: &'a [u8],
        executor_cost: Duration,
    },
    CompleteFailure,
    Incomplete,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Observation {
    Published,
    PersistenceFailed,
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
/// Local cache costs only. A Known Delta hit still returns the complete cached
/// output to the model, so these metrics must not claim provider-token savings.
struct EvidenceMetrics {
    lookup_micros: u64,
    fingerprint_micros: u64,
    executor_micros_avoided: u64,
}

#[cfg(test)]
pub(crate) async fn immutable_git_show_identity(
    cwd: &Path,
    program: &str,
    args: &[String],
) -> Option<EvidenceIdentity> {
    immutable_git_show_identity_with_project_namespace(
        cwd,
        program,
        args,
        ProjectNamespaceHint::Discover,
    )
    .await
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum ProjectNamespaceHint<'a> {
    Discover,
    Resolved(Option<&'a str>),
}

#[cfg(test)]
pub(crate) async fn immutable_git_show_identity_with_project_namespace(
    cwd: &Path,
    program: &str,
    args: &[String],
    project_namespace_hint: ProjectNamespaceHint<'_>,
) -> Option<EvidenceIdentity> {
    immutable_git_show_identity_with_authorization_scope(
        cwd,
        program,
        args,
        project_namespace_hint,
        TEST_AUTHORIZATION_SCOPE,
    )
    .await
}

async fn immutable_git_show_identity_with_authorization_scope(
    cwd: &Path,
    program: &str,
    args: &[String],
    project_namespace_hint: ProjectNamespaceHint<'_>,
    authorization_scope: &str,
) -> Option<EvidenceIdentity> {
    #[cfg(test)]
    test_observation::record_immutable_git_show_identity();
    let started = Instant::now();
    if !is_immutable_git_show_candidate(program, args) {
        return None;
    }
    let requested = &args[1];
    let (object, suffix) = requested
        .split_once(':')
        .unwrap_or((requested.as_str(), ""));
    if !is_resolved_object(object) {
        return None;
    }
    let normalized_suffix = if suffix.is_empty() {
        String::new()
    } else {
        normalize_relative_path(suffix)?
    };
    let normalized_requested = if normalized_suffix.is_empty() {
        object.to_string()
    } else {
        format!("{object}:{normalized_suffix}")
    };
    let project_namespace = match project_namespace_hint {
        ProjectNamespaceHint::Resolved(Some(namespace)) => namespace.to_owned(),
        ProjectNamespaceHint::Resolved(None) => return None,
        ProjectNamespaceHint::Discover => git_project_namespace(cwd).await?,
    };
    let cwd_position = git_stdout(cwd, &["rev-parse", "--show-prefix"])
        .await
        .unwrap_or_default();
    let resolved_blob = git_resolve_blob(cwd, &normalized_requested).await?;
    let program_identity = program.replace('\\', "/");
    let lineage_key = digest(
        format!("git_show_resolved_object\0program={program_identity}\0{normalized_suffix}")
            .as_bytes(),
    );
    let fingerprint = digest(
        format!(
            "schema={EVIDENCE_SCHEMA_VERSION}\0project={project_namespace}\0authorization={authorization_scope}\0op=git_show\0program={program_identity}\0cwd={cwd_position}\0object={normalized_requested}\0blob={resolved_blob}"
        )
        .as_bytes(),
    );
    Some(EvidenceIdentity {
        project_namespace,
        lineage_key,
        fingerprint,
        provenance: format!("git-object:{normalized_requested}"),
        fingerprint_cost: started.elapsed(),
    })
}

/// Cheap syntax-only gate for the narrow immutable command class supported by
/// Known Delta. Callers use this before computing repository identity so an
/// ordinary `rg`, status, or shell command never spawns fingerprinting Git.
pub(crate) fn is_immutable_git_show_candidate(program: &str, args: &[String]) -> bool {
    is_git_program(program) && args.len() == 2 && args[0] == "show"
}

#[allow(clippy::too_many_arguments)]
#[cfg(test)]
pub(crate) async fn prepare_immutable_git_show(
    codex_home: &Path,
    thread_id: &str,
    cwd: &Path,
    program: &str,
    args: &[String],
    project_namespace_hint: ProjectNamespaceHint<'_>,
    force_fresh: bool,
) -> Option<PreparedKnownDelta> {
    prepare_immutable_git_show_with_authorization_scope(
        codex_home,
        thread_id,
        cwd,
        program,
        args,
        project_namespace_hint,
        TEST_AUTHORIZATION_SCOPE,
        force_fresh,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn prepare_immutable_git_show_with_authorization_scope(
    codex_home: &Path,
    thread_id: &str,
    cwd: &Path,
    program: &str,
    args: &[String],
    project_namespace_hint: ProjectNamespaceHint<'_>,
    authorization_scope: &str,
    force_fresh: bool,
) -> Option<PreparedKnownDelta> {
    let identity = immutable_git_show_identity_with_authorization_scope(
        cwd,
        program,
        args,
        project_namespace_hint,
        authorization_scope,
    )
    .await?;
    let candidate = lookup(codex_home, &identity).await;
    #[cfg(test)]
    let profitability_costs = test_observation::configured_profitability_costs();
    #[cfg(test)]
    let (identity, candidate) = {
        let mut identity = identity;
        let mut candidate = candidate;
        if let Some((lookup_cost, fingerprint_cost, _)) = profitability_costs {
            identity.fingerprint_cost = fingerprint_cost;
            if let Some(candidate) = candidate.as_mut() {
                candidate.lookup_cost = lookup_cost;
            }
        }
        (identity, candidate)
    };
    let hit = if !force_fresh
        && let Some(candidate) = candidate.as_ref()
        && candidate.reusable()
    {
        let raw_output_artifact = remint_task_handle(codex_home, thread_id, candidate).await;
        if raw_output_artifact.model_projection().2.is_none() {
            Some(KnownDeltaHit {
                rendered_output: render_hit(candidate, &raw_output_artifact),
                raw_output_artifact,
            })
        } else {
            None
        }
    } else {
        None
    };
    Some(PreparedKnownDelta {
        identity,
        candidate,
        hit,
        force_fresh,
        #[cfg(test)]
        profitability_costs,
    })
}

pub(crate) async fn lookup(
    codex_home: &Path,
    identity: &EvidenceIdentity,
) -> Option<EvidenceCandidate> {
    #[cfg(test)]
    test_observation::record_lookup();
    let started = Instant::now();
    let unsafe_marker = unsafe_path(codex_home, identity);
    if lookup_disabled(codex_home, &unsafe_marker)
        || tokio::fs::try_exists(&unsafe_marker).await.ok()?
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
    // A contradiction may be reported while this lookup is reading the old
    // record. Recheck both the transient deny state and its durable successor
    // before returning it.
    if lookup_disabled(codex_home, &unsafe_marker)
        || tokio::fs::try_exists(&unsafe_marker).await.ok()?
    {
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
        let lookup_cost = candidate.lookup_cost;
        let fingerprint_cost = identity.fingerprint_cost;
        #[cfg(test)]
        let (lookup_cost, fingerprint_cost, executor_cost) =
            test_observation::profitability_costs(lookup_cost, fingerprint_cost, executor_cost);
        record.metrics = EvidenceMetrics {
            lookup_micros: micros(lookup_cost),
            fingerprint_micros: micros(fingerprint_cost),
            executor_micros_avoided: micros(executor_cost),
        };
        record.reusable = record.shadow_validations > 0
            && record.metrics.executor_micros_avoided
                > record
                    .metrics
                    .lookup_micros
                    .saturating_add(record.metrics.fingerprint_micros);
        let reuse_enabled = record.reusable;
        if write_record(codex_home, identity, &record).await.is_none() {
            return Observation::PersistenceFailed;
        }
        return Observation::Unchanged { reuse_enabled };
    }

    if write_blob(codex_home, &blob_digest, output).await.is_none() {
        return Observation::PersistenceFailed;
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
    if write_record(codex_home, identity, &record).await.is_none() {
        return Observation::PersistenceFailed;
    }
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

pub(crate) async fn record_execution(
    codex_home: &Path,
    prepared: &PreparedKnownDelta,
    observation: KnownDeltaExecutionObservation<'_>,
) {
    if prepared.is_hit() {
        return;
    }
    match observation {
        KnownDeltaExecutionObservation::CompleteSuccess {
            output,
            executor_cost,
        } => {
            #[cfg(test)]
            let executor_cost = prepared
                .profitability_costs
                .map(|(_, _, executor_cost)| executor_cost)
                .unwrap_or(executor_cost);
            let _ = record_success(
                codex_home,
                &prepared.identity,
                prepared.candidate.as_ref(),
                output,
                executor_cost,
            )
            .await;
        }
        KnownDeltaExecutionObservation::CompleteFailure if prepared.force_fresh => {
            record_contradictory_failure(
                codex_home,
                &prepared.identity,
                prepared.candidate.is_some(),
            )
            .await;
        }
        KnownDeltaExecutionObservation::CompleteFailure
        | KnownDeltaExecutionObservation::Incomplete => {}
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

fn runtime_quarantine_state() -> &'static Mutex<RuntimeQuarantineState> {
    RUNTIME_QUARANTINE_STATE.get_or_init(|| Mutex::new(RuntimeQuarantineState::default()))
}

fn lookup_disabled(codex_home: &Path, unsafe_marker: &Path) -> bool {
    let Ok(state) = runtime_quarantine_state().lock() else {
        // A poisoned deny-state lock must not restore access to suspect cache
        // entries.
        return true;
    };
    state.lookup_disabled(&store_root(codex_home), unsafe_marker)
}

fn mark_lineage_unsafe(unsafe_marker: PathBuf) {
    if let Ok(mut state) = runtime_quarantine_state().lock() {
        state.mark_lineage_unsafe(unsafe_marker);
    }
}

fn release_lineage(unsafe_marker: &Path) {
    if let Ok(mut state) = runtime_quarantine_state().lock() {
        state.release_lineage(unsafe_marker);
    }
}

fn disable_namespace(codex_home: &Path) {
    if let Ok(mut state) = runtime_quarantine_state().lock() {
        state.disable_namespace(store_root(codex_home));
    }
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
        // Windows does not replace an existing destination with `rename`. Move
        // the old record aside first, but keep it until the new record is in
        // place so an interrupted or failed replacement cannot destroy the
        // last valid cache entry.
        let backup = parent.join(format!(".{}.backup", Uuid::new_v4()));
        let had_previous = match tokio::fs::rename(path, &backup).await {
            Ok(()) => true,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
            Err(_) => {
                let _ = tokio::fs::remove_file(&temporary).await;
                return None;
            }
        };
        if tokio::fs::rename(&temporary, path).await.is_err() {
            if had_previous {
                let _ = tokio::fs::rename(&backup, path).await;
            }
            let _ = tokio::fs::remove_file(&temporary).await;
            return None;
        }
        if had_previous {
            let _ = tokio::fs::remove_file(&backup).await;
        }
    }
    Some(())
}

#[derive(Clone, Copy, Default)]
struct QuarantineFaults {
    marker_publication: bool,
    evidence_rename: bool,
}

async fn quarantine(codex_home: &Path, identity: &EvidenceIdentity) {
    quarantine_with_faults(codex_home, identity, QuarantineFaults::default()).await;
}

async fn quarantine_with_faults(
    codex_home: &Path,
    identity: &EvidenceIdentity,
    faults: QuarantineFaults,
) {
    let evidence = evidence_path(codex_home, identity);
    let unsafe_marker = unsafe_path(codex_home, identity);
    // Close the in-process race before attempting either durable mutation.
    mark_lineage_unsafe(unsafe_marker.clone());

    // Publish the lineage tombstone first. Once durable, even a failed move of
    // the old evidence cannot make it reusable by a later process.
    let marker_published =
        !faults.marker_publication && atomic_write(&unsafe_marker, b"unsafe\n").await.is_some();
    let evidence_quarantined = if tokio::fs::try_exists(&evidence).await.unwrap_or(false) {
        let quarantine = evidence.with_extension(format!("quarantine-{}", now_ms()));
        !faults.evidence_rename && tokio::fs::rename(&evidence, quarantine).await.is_ok()
    } else {
        true
    };

    if !marker_published || !evidence_quarantined {
        // If quarantine cannot be made wholly durable, fail closed for every
        // Known Delta lookup sharing this cache namespace for this process.
        disable_namespace(codex_home);
        tracing::error!(
            marker_published,
            evidence_quarantined,
            evidence = %evidence.display(),
            unsafe_marker = %unsafe_marker.display(),
            "Known Delta quarantine was not fully durable; cache namespace disabled"
        );
    }
    // A settled quarantine is protected either by its durable lineage marker
    // or by the fail-closed namespace state, so the transient race guard can go.
    release_lineage(&unsafe_marker);
}

async fn git_stdout(cwd: &Path, args: &[&str]) -> Option<String> {
    #[cfg(test)]
    test_observation::record_fingerprint_git_subprocess();
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

/// Resolve `spec` and verify that it names a blob with one bounded Git process.
///
/// `cat-file --batch-check` accepts the same revision/path expressions used by
/// `git show` without the parsing ambiguity of appending `^{blob}` to a
/// `REV:path` expression. This is intentionally a one-shot process: Known Delta
/// does not own a persistent Git worker.
async fn git_resolve_blob(cwd: &Path, spec: &str) -> Option<String> {
    if spec.contains(['\n', '\r']) {
        return None;
    }

    #[cfg(test)]
    test_observation::record_fingerprint_git_subprocess();
    let mut command = Command::new("git");
    command
        .args(["-c", "core.hooksPath=NUL", "-c", "core.fsmonitor=false"])
        .args(["cat-file", "--batch-check=%(objectname) %(objecttype)"])
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);

    let mut child = command.spawn().ok()?;
    let mut stdin = child.stdin.take()?;
    let query = format!("{spec}\n");
    let output = timeout(GIT_TIMEOUT, async move {
        stdin.write_all(query.as_bytes()).await.ok()?;
        stdin.shutdown().await.ok()?;
        drop(stdin);
        child.wait_with_output().await.ok()
    })
    .await
    .ok()??;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    let mut fields = stdout.split_whitespace();
    let object = fields.next()?;
    let object_type = fields.next()?;
    if fields.next().is_some() || object_type != "blob" || !is_resolved_object(object) {
        return None;
    }
    Some(object.to_string())
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

fn normalize_relative_path(path: &str) -> Option<String> {
    if path.starts_with('/')
        || path.starts_with('\\')
        || path
            .as_bytes()
            .get(1)
            .is_some_and(|separator| *separator == b':')
    {
        return None;
    }
    let mut normalized = Vec::new();
    for component in path.split(['/', '\\']) {
        match component {
            "" | "." => {}
            ".." => return None,
            component => normalized.push(component),
        }
    }
    Some(normalized.join("/"))
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

    #[test]
    fn authorization_scope_tracks_effective_file_system_and_override_mode() {
        let root = TempDir::new().unwrap();
        let absolute =
            codex_utils_absolute_path::AbsolutePathBuf::try_from(root.path().to_path_buf())
                .unwrap();
        let cwd = codex_utils_path_uri::PathUri::from_abs_path(&absolute);
        let read_only = FileSystemSandboxContext::from_legacy_sandbox_policy(
            codex_protocol::protocol::SandboxPolicy::ReadOnly {
                network_access: false,
            },
            cwd.clone(),
        )
        .unwrap();
        let full_access = FileSystemSandboxContext::from_legacy_sandbox_policy(
            codex_protocol::protocol::SandboxPolicy::DangerFullAccess,
            cwd,
        )
        .unwrap();

        let read_only_default =
            authorization_scope_fingerprint(&read_only, SandboxPermissions::UseDefault).unwrap();
        assert_eq!(
            read_only_default,
            authorization_scope_fingerprint(&read_only, SandboxPermissions::UseDefault).unwrap()
        );
        assert_ne!(
            read_only_default,
            authorization_scope_fingerprint(&read_only, SandboxPermissions::RequireEscalated,)
                .unwrap()
        );
        assert_ne!(
            read_only_default,
            authorization_scope_fingerprint(&full_access, SandboxPermissions::UseDefault).unwrap()
        );
    }

    #[test]
    fn immutable_git_show_gate_rejects_ordinary_commands_before_fingerprinting() {
        assert!(is_immutable_git_show_candidate(
            "git",
            &["show".to_string(), "HEAD:file.rs".to_string()]
        ));
        assert!(!is_immutable_git_show_candidate(
            "git",
            &["status".to_string(), "--short".to_string()]
        ));
        assert!(!is_immutable_git_show_candidate(
            "rg",
            &["pattern".to_string(), "src".to_string()]
        ));
    }

    #[test]
    fn runtime_quarantine_state_is_bounded_and_fails_closed() {
        let mut state = RuntimeQuarantineState::default();
        let transient_marker = PathBuf::from("transient.unsafe");
        state.mark_lineage_unsafe(transient_marker.clone());
        assert!(state.lookup_disabled(Path::new("namespace"), &transient_marker));
        state.release_lineage(&transient_marker);
        assert!(!state.lookup_disabled(Path::new("namespace"), &transient_marker));

        for index in 0..MAX_DISABLED_RUNTIME_NAMESPACES {
            state.disable_namespace(PathBuf::from(format!("namespace-{index}")));
        }
        assert_eq!(
            state.disabled_namespaces.len(),
            MAX_DISABLED_RUNTIME_NAMESPACES
        );
        state.disable_namespace(PathBuf::from("overflow"));

        assert!(state.all_namespaces_disabled);
        assert!(state.disabled_namespaces.is_empty());
        assert!(state.lookup_disabled(Path::new("any-namespace"), Path::new("any-marker")));
    }

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

    #[tokio::test]
    async fn publication_reports_persistence_failure_when_the_store_is_unwritable() {
        let root = TempDir::new().unwrap();
        let blocked_home = root.path().join("not-a-directory");
        tokio::fs::write(&blocked_home, b"blocked").await.unwrap();

        assert_eq!(
            record_success(
                &blocked_home,
                &identity("project", "lineage", "fingerprint"),
                None,
                b"output",
                Duration::from_millis(1),
            )
            .await,
            Observation::PersistenceFailed
        );
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
    async fn related_worktrees_share_git_show_identity_but_unrelated_projects_do_not() {
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

        let first_blob = run_git(&repo, &["rev-parse", "HEAD:read.txt"]);
        let related_blob = run_git(&worktree, &["rev-parse", "HEAD:read.txt"]);
        let other_blob = run_git(&unrelated, &["rev-parse", "HEAD:read.txt"]);
        let first = immutable_git_show_identity(&repo, "git", &["show".to_string(), first_blob])
            .await
            .unwrap();
        let related =
            immutable_git_show_identity(&worktree, "git", &["show".to_string(), related_blob])
                .await
                .unwrap();
        let other =
            immutable_git_show_identity(&unrelated, "git", &["show".to_string(), other_blob])
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
    }

    #[tokio::test]
    async fn immutable_git_show_still_fingerprints_and_promotes_reusable_evidence() {
        let root = TempDir::new().unwrap();
        let repo = root.path().join("repo");
        let home = root.path().join("home");
        init_repo(&repo, "immutable\n");
        let blob = run_git(&repo, &["rev-parse", "HEAD:read.txt"]);

        let (reusable, observed) = test_observation::observe(async {
            let identity = immutable_git_show_identity(&repo, "git", &["show".to_string(), blob])
                .await
                .expect("git show should have an immutable identity");
            assert!(lookup(&home, &identity).await.is_none());
            assert_eq!(
                record_success(
                    &home,
                    &identity,
                    None,
                    b"immutable output",
                    Duration::from_secs(1),
                )
                .await,
                Observation::Published
            );
            let candidate = lookup(&home, &identity)
                .await
                .expect("published evidence should be available for shadow validation");
            assert!(!candidate.reusable());
            assert_eq!(
                record_success(
                    &home,
                    &identity,
                    Some(&candidate),
                    b"immutable output",
                    Duration::from_secs(1),
                )
                .await,
                Observation::Unchanged {
                    reuse_enabled: true
                }
            );
            lookup(&home, &identity)
                .await
                .expect("validated evidence should remain available")
                .reusable()
        })
        .await;

        assert!(reusable);
        assert_eq!(observed.immutable_git_show_identity_calls, 1);
        assert_eq!(observed.lookup_calls, 3);
        assert_eq!(observed.fingerprint_git_subprocesses, 3);
    }

    #[tokio::test]
    async fn reusable_evidence_does_not_cross_authorization_scopes() {
        let root = TempDir::new().unwrap();
        let repo = root.path().join("repo");
        let home = root.path().join("home");
        init_repo(&repo, "immutable\n");
        let blob = run_git(&repo, &["rev-parse", "HEAD:read.txt"]);
        let args = ["show".to_string(), blob];
        let identity = immutable_git_show_identity_with_authorization_scope(
            &repo,
            "git",
            &args,
            ProjectNamespaceHint::Discover,
            "read-only-scope",
        )
        .await
        .expect("immutable identity");
        assert_eq!(
            record_success(
                &home,
                &identity,
                None,
                b"immutable output",
                Duration::from_secs(1),
            )
            .await,
            Observation::Published
        );
        let candidate = lookup(&home, &identity).await.expect("shadow candidate");
        assert_eq!(
            record_success(
                &home,
                &identity,
                Some(&candidate),
                b"immutable output",
                Duration::from_secs(1),
            )
            .await,
            Observation::Unchanged {
                reuse_enabled: true
            }
        );

        let different_scope = prepare_immutable_git_show_with_authorization_scope(
            &home,
            "thread-b",
            &repo,
            "git",
            &args,
            ProjectNamespaceHint::Discover,
            "workspace-write-scope",
            false,
        )
        .await
        .expect("prepared lookup");
        assert!(!different_scope.has_candidate());
        assert!(!different_scope.is_hit());

        let same_scope = prepare_immutable_git_show_with_authorization_scope(
            &home,
            "thread-a",
            &repo,
            "git",
            &args,
            ProjectNamespaceHint::Discover,
            "read-only-scope",
            false,
        )
        .await
        .expect("prepared lookup");
        assert!(same_scope.has_candidate());
        assert!(same_scope.is_hit());
    }

    #[tokio::test]
    async fn immutable_git_show_identity_uses_the_normalized_path_suffix() {
        let root = TempDir::new().unwrap();
        let repo = root.path().join("repo");
        init_repo(&repo, "immutable\n");
        let commit = run_git(&repo, &["rev-parse", "HEAD"]);

        let direct = immutable_git_show_identity(
            &repo,
            "git",
            &["show".to_string(), format!("{commit}:read.txt")],
        )
        .await
        .expect("direct path identity");
        let dotted = immutable_git_show_identity(
            &repo,
            "git",
            &["show".to_string(), format!("{commit}:./read.txt")],
        )
        .await
        .expect("normalized path identity");

        assert_eq!(direct.lineage_key, dotted.lineage_key);
        assert_eq!(direct.fingerprint, dotted.fingerprint);
        assert_eq!(direct.provenance, dotted.provenance);
        for invalid in [
            "../read.txt",
            "src/../read.txt",
            "/read.txt",
            "C:\\read.txt",
        ] {
            assert!(
                immutable_git_show_identity(
                    &repo,
                    "git",
                    &["show".to_string(), format!("{commit}:{invalid}")],
                )
                .await
                .is_none(),
                "invalid suffix must not produce an identity: {invalid}"
            );
        }
    }

    #[tokio::test]
    async fn immutable_git_show_identity_is_scoped_to_the_supplied_executable() {
        let root = TempDir::new().unwrap();
        let repo = root.path().join("repo");
        init_repo(&repo, "immutable\n");
        let blob = run_git(&repo, &["rev-parse", "HEAD:read.txt"]);
        let args = ["show".to_string(), blob];
        let alternate_program = repo.join("git.exe").to_string_lossy().into_owned();

        let system = immutable_git_show_identity(&repo, "git", &args)
            .await
            .expect("system git identity");
        let alternate = immutable_git_show_identity(&repo, &alternate_program, &args)
            .await
            .expect("alternate git identity");

        assert_ne!(system.lineage_key, alternate.lineage_key);
        assert_ne!(system.fingerprint, alternate.fingerprint);
    }

    #[tokio::test]
    async fn known_project_namespace_removes_one_fingerprint_git_process() {
        let root = TempDir::new().unwrap();
        let repo = root.path().join("repo");
        init_repo(&repo, "immutable\n");
        let blob = run_git(&repo, &["rev-parse", "HEAD:read.txt"]);
        let namespace = git_project_namespace(&repo).await.unwrap();

        let (identity, observed) = test_observation::observe(async {
            immutable_git_show_identity_with_project_namespace(
                &repo,
                "git",
                &["show".to_string(), blob],
                ProjectNamespaceHint::Resolved(Some(&namespace)),
            )
            .await
        })
        .await;

        assert!(identity.is_some());
        assert_eq!(observed.fingerprint_git_subprocesses, 2);
    }

    #[tokio::test]
    async fn resolved_missing_namespace_does_not_probe_git_again() {
        let root = TempDir::new().unwrap();
        let repo = root.path().join("repo");
        init_repo(&repo, "immutable\n");
        let blob = run_git(&repo, &["rev-parse", "HEAD:read.txt"]);

        let (identity, observed) = test_observation::observe(async {
            immutable_git_show_identity_with_project_namespace(
                &repo,
                "git",
                &["show".to_string(), blob],
                ProjectNamespaceHint::Resolved(None),
            )
            .await
        })
        .await;

        assert!(identity.is_none());
        assert_eq!(observed.fingerprint_git_subprocesses, 0);
    }

    #[tokio::test]
    async fn one_shot_blob_lookup_accepts_blobs_and_rejects_other_object_types() {
        let root = TempDir::new().unwrap();
        let repo = root.path().join("repo");
        init_repo(&repo, "immutable\n");

        assert!(git_resolve_blob(&repo, "HEAD:read.txt").await.is_some());
        assert!(git_resolve_blob(&repo, "HEAD").await.is_none());
        assert!(git_resolve_blob(&repo, "HEAD^{tree}").await.is_none());
        assert!(git_resolve_blob(&repo, "missing-object").await.is_none());
        assert!(
            git_resolve_blob(&repo, "HEAD\nHEAD:read.txt")
                .await
                .is_none()
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
    async fn empty_output_evidence_promotes_when_execution_is_profitable() {
        let home = TempDir::new().unwrap();
        let id = identity("project", "empty-lineage", "empty-fingerprint");
        assert_eq!(
            record_success(home.path(), &id, None, b"", Duration::from_secs(1)).await,
            Observation::Published
        );
        let candidate = lookup(home.path(), &id).await.unwrap();
        let observation = test_observation::with_profitability_costs(
            record_success(home.path(), &id, Some(&candidate), b"", Duration::ZERO),
            Duration::from_millis(1),
            Duration::from_millis(1),
            Duration::from_millis(10),
        )
        .await;
        assert_eq!(
            observation,
            Observation::Unchanged {
                reuse_enabled: true
            }
        );

        let candidate = lookup(home.path(), &id).await.unwrap();
        assert!(candidate.reusable());
        let artifact = remint_task_handle(home.path(), "empty-task", &candidate).await;
        let (_, bytes, error) = artifact.model_projection();
        assert_eq!(bytes, Some(0));
        assert!(error.is_none());
    }

    #[tokio::test]
    async fn legacy_projected_token_metric_is_readable_but_not_written() {
        let home = TempDir::new().unwrap();
        let id = identity("project", "legacy-lineage", "legacy-fingerprint");
        record_success(home.path(), &id, None, b"output", Duration::from_secs(1)).await;
        let candidate = lookup(home.path(), &id).await.unwrap();
        let mut serialized = serde_json::to_value(&candidate.record).unwrap();
        assert!(
            serialized["metrics"]
                .get("projected_tokens_avoided")
                .is_none()
        );

        serialized["metrics"]["projected_tokens_avoided"] = serde_json::json!(42);
        tokio::fs::write(
            evidence_path(home.path(), &id),
            serde_json::to_vec(&serialized).unwrap(),
        )
        .await
        .unwrap();

        assert!(lookup(home.path(), &id).await.is_some());
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
    async fn audit_known_delta_quarantine_rename_failure_disables_namespace() {
        let home = TempDir::new().unwrap();
        let contradicted = identity("project", "lineage", "fingerprint");
        let unrelated = identity("other-project", "other-lineage", "other-fingerprint");
        record_success(
            home.path(),
            &contradicted,
            None,
            b"known wrong",
            Duration::from_millis(1),
        )
        .await;
        record_success(
            home.path(),
            &unrelated,
            None,
            b"otherwise reusable",
            Duration::from_millis(1),
        )
        .await;

        quarantine_with_faults(
            home.path(),
            &contradicted,
            QuarantineFaults {
                evidence_rename: true,
                ..Default::default()
            },
        )
        .await;

        assert!(
            tokio::fs::try_exists(evidence_path(home.path(), &contradicted))
                .await
                .unwrap()
        );
        assert!(
            tokio::fs::try_exists(unsafe_path(home.path(), &contradicted))
                .await
                .unwrap()
        );
        assert!(lookup(home.path(), &contradicted).await.is_none());
        assert!(lookup(home.path(), &unrelated).await.is_none());
    }

    #[tokio::test]
    async fn audit_known_delta_quarantine_marker_failure_disables_namespace() {
        let home = TempDir::new().unwrap();
        let contradicted = identity("project", "lineage", "fingerprint");
        let unrelated = identity("other-project", "other-lineage", "other-fingerprint");
        record_success(
            home.path(),
            &contradicted,
            None,
            b"known wrong",
            Duration::from_millis(1),
        )
        .await;
        record_success(
            home.path(),
            &unrelated,
            None,
            b"otherwise reusable",
            Duration::from_millis(1),
        )
        .await;

        quarantine_with_faults(
            home.path(),
            &contradicted,
            QuarantineFaults {
                marker_publication: true,
                ..Default::default()
            },
        )
        .await;

        assert!(
            !tokio::fs::try_exists(evidence_path(home.path(), &contradicted))
                .await
                .unwrap()
        );
        assert!(
            !tokio::fs::try_exists(unsafe_path(home.path(), &contradicted))
                .await
                .unwrap()
        );
        assert!(lookup(home.path(), &contradicted).await.is_none());
        assert!(lookup(home.path(), &unrelated).await.is_none());
    }

    #[tokio::test]
    async fn audit_known_delta_quarantine_combined_failure_never_reuses_record() {
        let home = TempDir::new().unwrap();
        let contradicted = identity("project", "lineage", "fingerprint");
        record_success(
            home.path(),
            &contradicted,
            None,
            b"known wrong",
            Duration::from_millis(1),
        )
        .await;

        quarantine_with_faults(
            home.path(),
            &contradicted,
            QuarantineFaults {
                marker_publication: true,
                evidence_rename: true,
            },
        )
        .await;

        assert!(
            tokio::fs::try_exists(evidence_path(home.path(), &contradicted))
                .await
                .unwrap()
        );
        assert!(
            !tokio::fs::try_exists(unsafe_path(home.path(), &contradicted))
                .await
                .unwrap()
        );
        assert!(lookup(home.path(), &contradicted).await.is_none());
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
