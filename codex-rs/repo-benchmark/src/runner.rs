use crate::diagnostics::{DiagnosticResult, analyze};
use crate::native::{NativeAttemptEvidence, NativeAttemptRequest, ScriptedScenario, run_attempt};
use crate::prepare::provenance::{
    FileIdentity, copy_tree, hash_file, reset_workspace, source_tree_inventory, write_atomic_json,
    write_json,
};
use crate::prepare::{Prepared, unique_id};
use crate::schedule::{ExecutionBudget, ScheduledAttempt, Segment};
use crate::workloads::{VerificationOutcome, VerificationStatus, verify_fixture_with_env};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::{Instant, SystemTime, UNIX_EPOCH},
};
#[cfg(test)]
mod tests;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Attempt {
    pub scheduled: ScheduledAttempt,
    pub status: String,
    pub reason: Option<String>,
    pub started_unix_ms: Option<u64>,
    pub native: Option<NativeAttemptEvidence>,
    pub verifier: Option<VerificationOutcome>,
    pub diagnostics: Option<DiagnosticResult>,
    pub outside_execution_ms: BTreeMap<String, u64>,
    pub final_workspace_sha256: Option<String>,
    #[serde(default)]
    pub evidence_files: Vec<FileIdentity>,
    pub evidence_directory: PathBuf,
    pub rerun_command: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunResult {
    pub schema_version: u32,
    pub id: String,
    pub prepared_manifest: PathBuf,
    pub prepared_manifest_sha256: String,
    pub directory: PathBuf,
    pub mode: crate::schedule::Mode,
    pub original_run: Option<PathBuf>,
    pub attempts: Vec<Attempt>,
    pub scripted_execution_ms: u64,
    pub real_model_execution_ms: u64,
    pub finished: bool,
}

impl RunResult {
    pub fn verify_evidence(&self) -> Result<()> {
        for attempt in &self.attempts {
            verify_attempt_evidence(attempt)?;
        }
        Ok(())
    }
    /// An unfinished result is the frozen schedule plus individual durable
    /// attempt checkpoints. Never rewrite all previous event traces per turn.
    pub fn load(path: &Path) -> Result<Self> {
        let mut result: Self = crate::prepare::provenance::read_json(path)?;
        if !result.finished {
            for attempt in &mut result.attempts {
                // Analysis-only results retain the original evidence paths;
                // never replay those checkpoints over their new diagnostics.
                if attempt.evidence_directory.parent() != Some(result.directory.as_path()) {
                    continue;
                }
                let checkpoint = attempt.evidence_directory.join("attempt.json");
                if checkpoint.exists() {
                    let saved: Attempt = crate::prepare::provenance::read_json(&checkpoint)?;
                    ensure!(
                        saved.scheduled == attempt.scheduled
                            && saved.evidence_directory == attempt.evidence_directory,
                        "attempt checkpoint differs from frozen schedule: {}",
                        checkpoint.display()
                    );
                    *attempt = saved;
                    if attempt.status == "running" {
                        attempt.status = "incomplete".into();
                        attempt.reason = Some("run stopped before final verification and diagnostics; inspect partial native evidence".into());
                    }
                } else if attempt.status == "not_started" && attempt.reason.is_none() {
                    attempt.reason = Some("run stopped before an attempt checkpoint was saved; inspect any partial native evidence".into());
                }
            }
            result.scripted_execution_ms = result.execution_ms(Segment::Scripted);
            result.real_model_execution_ms = result.execution_ms(Segment::RealModel);
        }
        Ok(result)
    }

    fn execution_ms(&self, segment: Segment) -> u64 {
        self.attempts
            .iter()
            .filter(|a| a.scheduled.segment == segment)
            .filter_map(|a| a.native.as_ref())
            .fold(0_u64, |total, native| {
                total.saturating_add(native.elapsed_ms)
            })
    }
}

fn checkpoint(attempt: &Attempt) -> Result<()> {
    fs::create_dir_all(&attempt.evidence_directory)?;
    write_atomic_json(&attempt.evidence_directory.join("attempt.json"), attempt)
}

fn native_input_paths(native: &NativeAttemptEvidence) -> Vec<&Path> {
    std::iter::once(native.evidence_path.as_path())
        .chain(native.rollout_paths.iter().map(PathBuf::as_path))
        .chain(native.stdout_paths.iter().map(PathBuf::as_path))
        .chain(native.stderr_paths.iter().map(PathBuf::as_path))
        .chain(native.provider_requests_path.iter().map(PathBuf::as_path))
        .collect()
}

fn freeze_attempt_evidence(attempt: &mut Attempt) -> Result<()> {
    let Some(native) = &attempt.native else {
        return Ok(());
    };
    attempt.evidence_files = native_input_paths(native)
        .into_iter()
        .map(FileIdentity::record)
        .collect::<Result<_>>()?;
    Ok(())
}

fn verify_attempt_evidence(attempt: &Attempt) -> Result<()> {
    let Some(native) = &attempt.native else {
        return Ok(());
    };
    for path in native_input_paths(native) {
        let resolved = fs::canonicalize(path)?;
        ensure!(
            attempt
                .evidence_files
                .iter()
                .any(|file| file.path == resolved),
            "attempt {} has no frozen identity for {}; raw evidence cannot be reanalyzed or imported",
            attempt.scheduled.id,
            path.display()
        );
    }
    for file in &attempt.evidence_files {
        file.verify()?;
    }
    let recorded: NativeAttemptEvidence =
        serde_json::from_slice(&fs::read(&native.evidence_path)?)?;
    ensure!(
        serde_json::to_value(recorded)? == serde_json::to_value(native)?,
        "native evidence differs from the embedded attempt {}",
        attempt.scheduled.id
    );
    Ok(())
}

fn lock_workspace(prepared: &Prepared) -> Result<fs::File> {
    ensure!(
        prepared.workspace_lock == prepared.directory.join("workspace.lock"),
        "prepared workspace lock path changed"
    );
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&prepared.workspace_lock)?;
    file.try_lock()
        .context("prepared workspace is already in use by another benchmark run")?;
    Ok(file)
}

pub fn execute(
    manifest: &Path,
    selected: Option<&str>,
    original: Option<PathBuf>,
) -> Result<PathBuf> {
    let manifest = fs::canonicalize(manifest)?;
    let prepared = Prepared::load(&manifest)?;
    let _workspace_guard = lock_workspace(&prepared)?;
    prepared.verify()?;
    let id = unique_id();
    let directory = prepared.runs_directory.join(&id);
    fs::create_dir_all(&directory)?;
    let scheduled: Vec<_> = prepared
        .schedule
        .iter()
        .filter(|a| selected.is_none_or(|id| a.id == id))
        .cloned()
        .collect();
    ensure!(
        !scheduled.is_empty(),
        "requested attempt is absent from prepared schedule"
    );
    let mut result = RunResult {
        schema_version: 1,
        id,
        prepared_manifest: manifest.clone(),
        prepared_manifest_sha256: hash_file(&manifest)?,
        directory: directory.clone(),
        mode: prepared.mode,
        original_run: original,
        attempts: scheduled
            .into_iter()
            .map(|scheduled| Attempt {
                evidence_directory: directory.join(&scheduled.id),
                rerun_command: format!(
                    "just repo-benchmark rerun --result \"{}\" --attempt {}",
                    directory.join("result.json").display(),
                    scheduled.id
                ),
                scheduled,
                status: "not_started".into(),
                reason: None,
                started_unix_ms: None,
                native: None,
                verifier: None,
                diagnostics: None,
                outside_execution_ms: BTreeMap::new(),
                final_workspace_sha256: None,
                evidence_files: vec![],
            })
            .collect(),
        scripted_execution_ms: 0,
        real_model_execution_ms: 0,
        finished: false,
    };
    let result_path = directory.join("result.json");
    // Record every resolved attempt path before any native execution.
    write_atomic_json(&result_path, &result)?;
    // Replay the ceilings frozen at preparation, including manifests prepared
    // before the defaults were increased.
    let budget_ms = |key: &str| -> Result<u64> {
        prepared.budgets[key]
            .as_u64()
            .filter(|value| *value > 0)
            .with_context(|| format!("prepared budget {key} must be a positive integer"))
    };
    let scripted_limit = budget_ms("scriptedMs")?;
    let live_limit = budget_ms("realModelMs")?;
    let attempt_limit = budget_ms("attemptMs")?;
    let mut scripted_budget = ExecutionBudget::new(scripted_limit);
    let mut live_budget = ExecutionBudget::new(live_limit);
    let mut unrecoverable: Option<String> = None;
    for index in 0..result.attempts.len() {
        let attempt = &mut result.attempts[index];
        let budget = if attempt.scheduled.segment == Segment::Scripted {
            &mut scripted_budget
        } else {
            &mut live_budget
        };
        if let Some(reason) = &unrecoverable {
            attempt.reason = Some(format!("dependent setup unavailable: {reason}"));
            checkpoint(attempt)?;
            continue;
        }
        if budget.remaining_ms() == 0 {
            attempt.reason = Some("segment_budget_exhausted before attempt started".into());
            checkpoint(attempt)?;
            continue;
        }
        eprintln!(
            "{} ({:?}; {} ms segment allowance left)",
            attempt.scheduled.id,
            attempt.scheduled.segment,
            budget.remaining_ms()
        );
        let timeout = if attempt.scheduled.segment == Segment::RealModel {
            budget.remaining_ms().min(attempt_limit)
        } else {
            budget.remaining_ms()
        };
        if let Err(error) = execute_one(&prepared, attempt, timeout) {
            attempt.status = "setup_failed".into();
            attempt.reason = Some(format!("{error:#}"));
            // A reset error makes subsequent workspace state unknowable. Stop dependent work.
            if error.downcast_ref::<ResetFailure>().is_some() {
                unrecoverable = attempt.reason.clone();
            }
            if attempt.evidence_files.is_empty() {
                if let Err(freeze_error) = freeze_attempt_evidence(attempt) {
                    attempt.reason = Some(format!(
                        "{}; cannot freeze partial evidence: {freeze_error:#}",
                        attempt.reason.as_deref().unwrap_or_default()
                    ));
                }
            }
        }
        if let Some(native) = &attempt.native {
            budget.charge(native.elapsed_ms);
            if native.status == "timeout" && timeout < attempt_limit
                || native.status == "timeout" && attempt.scheduled.segment == Segment::Scripted
            {
                attempt.reason = Some("segment_budget_exhausted during attempt".into());
            }
        }
        result.scripted_execution_ms = scripted_limit - scripted_budget.remaining_ms();
        result.real_model_execution_ms = live_limit - live_budget.remaining_ms();
        checkpoint(attempt)?;
    }
    result.finished = true;
    write_atomic_json(&result_path, &result)?;
    crate::reports::write(&prepared, &result)?;
    Ok(result_path)
}

#[derive(Debug)]
struct ResetFailure(String);
impl std::fmt::Display for ResetFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for ResetFailure {}

/// Authentication belongs only to the native process lifetime, never retained evidence.
struct TemporaryAuthentication {
    path: PathBuf,
    active: bool,
}
impl TemporaryAuthentication {
    fn install(source: Option<&Path>, destination: &Path) -> Result<Self> {
        // Establish cleanup before copying: even a partial failed copy is temporary.
        let temporary = Self {
            path: destination.to_path_buf(),
            active: true,
        };
        if let Some(source) = source {
            fs::copy(source, destination).context("copy temporary native authentication")?;
        }
        Ok(temporary)
    }
    fn remove(&mut self) -> Result<()> {
        if self.active {
            match fs::remove_file(&self.path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("remove temporary native authentication"),
            }
            self.active = false;
        }
        Ok(())
    }
}
impl Drop for TemporaryAuthentication {
    fn drop(&mut self) {
        if let Err(error) = self.remove() {
            eprintln!("Temporary authentication cleanup failed: {error:#}");
        }
    }
}

fn execute_one(prepared: &Prepared, attempt: &mut Attempt, timeout_ms: u64) -> Result<()> {
    let live = attempt.scheduled.segment == Segment::RealModel;
    let fixture_key = if live {
        attempt.scheduled.workload.as_str()
    } else {
        "scripted"
    };
    let fixture = &prepared.fixtures[fixture_key];
    let reset = Instant::now();
    reset_workspace(
        &prepared.directory,
        &prepared.workspace,
        &fixture.snapshot,
        &fixture.sha256,
    )
    .map_err(|e| ResetFailure(format!("cannot restore workspace: {e:#}")))?;
    attempt
        .outside_execution_ms
        .insert("workspaceReset".into(), reset.elapsed().as_millis() as u64);
    fs::create_dir_all(&attempt.evidence_directory)?;
    let home = attempt.evidence_directory.join("home");
    fs::create_dir_all(&home)?;
    fs::copy(&prepared.base_config.path, home.join("config.toml"))?;
    let mut env = prepared.environment.variables.clone();
    env.insert("CODEX_HOME".into(), home.to_string_lossy().into());
    let build = &prepared.builds[&attempt.scheduled.variant];
    for executable in build.executables.values() {
        executable.verify()?;
    }
    if let Some(host) = build.executables.get("codex-code-mode-host") {
        env.insert(
            "CODEX_CODE_MODE_HOST_PATH".into(),
            host.path.to_string_lossy().into(),
        );
    }
    let scenario = if live {
        None
    } else {
        Some(serde_json::from_value::<ScriptedScenario>(json!(
            attempt.scheduled.workload
        ))?)
    };
    let prompt = fixture
        .descriptor
        .as_ref()
        .map(|d| d.prompt.clone())
        .unwrap_or_else(|| {
            "Execute the requested deterministic repository task and finish the turn.".into()
        });
    let expected_config = prepared.expected_config(attempt.scheduled.variant)?;
    write_json(
        &attempt.evidence_directory.join("inputs.json"),
        &json!({"prompt":prompt,"fixtureSha256":fixture.sha256,"workspace":prepared.workspace,"additionalRoots":prepared.additional_roots,"configuration":&expected_config,"nativeBuild":build,"scheduled":attempt.scheduled,"timeoutMs":timeout_ms}),
    )?;
    // Supply secrets only after fallible preparation; the scope also cleans up on
    // early errors or unwinding, including a partially copied auth file.
    let mut authentication = if live {
        let source_home = std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("USERPROFILE").map(|p| PathBuf::from(p).join(".codex")));
        let source = source_home
            .map(|p| p.join("auth.json"))
            .filter(|p| p.is_file());
        if let Ok(key) = std::env::var("OPENAI_API_KEY") {
            env.insert("OPENAI_API_KEY".into(), key);
        }
        Some(TemporaryAuthentication::install(
            source.as_deref(),
            &home.join("auth.json"),
        )?)
    } else {
        None
    };
    attempt.started_unix_ms =
        Some(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64);
    attempt.status = "running".into();
    checkpoint(attempt)?;
    let native = run_attempt(&NativeAttemptRequest {
        attempt_id: attempt.scheduled.id.clone(),
        app_server: build.executables["codex-app-server"].path.clone(),
        cwd: prepared.workspace.clone(),
        codex_home: home,
        evidence_dir: attempt.evidence_directory.clone(),
        env,
        config_overrides: prepared.overrides[&attempt.scheduled.variant].clone(),
        expected_config,
        prompt,
        timeout_ms,
        scenario,
    });
    // Native code always persists evidence, including no-rollout startup failure.
    attempt.status = native.status.clone();
    attempt.reason = native
        .failure
        .as_ref()
        .map(|f| format!("{}: {}", f.kind, f.message));
    attempt
        .outside_execution_ms
        .insert("cleanup".into(), native.cleanup_ms);
    attempt.native = Some(native);
    if let Some(authentication) = &mut authentication {
        authentication.remove()?;
    }
    let verification = Instant::now();
    if let Some(descriptor) = &fixture.descriptor {
        let protected = attempt.evidence_directory.join("verification");
        copy_tree(&descriptor.protected_dir, &protected)?;
        let mut descriptor = descriptor.clone();
        descriptor.protected_dir = protected.clone();
        descriptor.verifier_path = protected.join("verify.py");
        let verified = verify_fixture_with_env(&descriptor, &prepared.environment.variables)?;
        if verified.status != VerificationStatus::Passed && attempt.status == "completed" {
            attempt.status = match verified.status {
                VerificationStatus::Incorrect => "incorrect",
                VerificationStatus::ScopeViolation => "scope_violation",
                _ => "verification_unavailable",
            }
            .into();
            attempt.reason = Some(verified.detail.clone());
        }
        attempt.verifier = Some(verified);
    }
    attempt.final_workspace_sha256 = Some(preserve_final_changes(
        &fixture.snapshot,
        &prepared.workspace,
        &attempt.evidence_directory.join("final-changes"),
    )?);
    attempt.outside_execution_ms.insert(
        "verificationAndFinalState".into(),
        verification.elapsed().as_millis() as u64,
    );
    freeze_attempt_evidence(attempt)?;
    // Persist verified underlying evidence before starting the fallible analyzer.
    checkpoint(attempt)?;
    let native = attempt.native.as_ref().context("native evidence")?;
    let analysis = Instant::now();
    attempt.diagnostics = Some(analyze(
        &prepared.environment.tools["python"].executable.path,
        &prepared.analyzer.path,
        &prepared.analyzer_files,
        native,
        &attempt.evidence_directory,
        &prepared.workspace,
        live,
        attempt.verifier.as_ref(),
    ));
    attempt
        .outside_execution_ms
        .insert("analysis".into(), analysis.elapsed().as_millis() as u64);
    checkpoint(attempt)?;
    Ok(())
}

/// The final source digest and preserved changes use the same inventory. Build
/// products and installed dependencies are outside this source-state digest.
fn preserve_final_changes(snapshot: &Path, workspace: &Path, destination: &Path) -> Result<String> {
    let before = source_tree_inventory(snapshot)?;
    let after = source_tree_inventory(workspace)?;
    let paths = before
        .files
        .keys()
        .chain(after.files.keys())
        .collect::<std::collections::BTreeSet<_>>();
    fs::create_dir_all(destination)?;
    let mut changes = vec![];
    for relative in paths {
        let before_hash = before.files.get(relative);
        let after_hash = after.files.get(relative);
        if before_hash == after_hash {
            continue;
        }
        if after_hash.is_some() {
            let target = destination.join(relative);
            fs::create_dir_all(target.parent().context("final change parent")?)?;
            fs::copy(workspace.join(relative), target)?;
        }
        changes.push(json!({"path":relative,"beforeSha256":before_hash,"afterSha256":after_hash}));
    }
    write_json(&destination.join("changes.json"), &changes)?;
    Ok(after.sha256)
}

pub fn analysis_only(result_path: &Path) -> Result<PathBuf> {
    let mut result = RunResult::load(result_path)?;
    result.verify_evidence()?;
    let prepared = Prepared::load(&result.prepared_manifest)?;
    ensure!(
        hash_file(&result.prepared_manifest)? == result.prepared_manifest_sha256,
        "prepared manifest changed since original run"
    );
    for file in &prepared.analyzer_files {
        file.verify()?;
    }
    let original = result_path.to_path_buf();
    result.id = unique_id();
    result.directory = prepared.runs_directory.join(&result.id);
    result.original_run = Some(original);
    fs::create_dir_all(&result.directory)?;
    for attempt in &mut result.attempts {
        if let Some(native) = &attempt.native {
            let output = result.directory.join(&attempt.scheduled.id);
            fs::create_dir_all(&output)?;
            attempt.diagnostics = Some(analyze(
                &prepared.environment.tools["python"].executable.path,
                &prepared.analyzer.path,
                &prepared.analyzer_files,
                native,
                &output,
                &prepared.workspace,
                attempt.scheduled.segment == Segment::RealModel,
                attempt.verifier.as_ref(),
            ));
        }
    }
    let output = result.directory.join("result.json");
    write_atomic_json(&output, &result)?;
    crate::reports::write(&prepared, &result)?;
    Ok(output)
}
