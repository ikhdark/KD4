use crate::diagnostics::{DiagnosticResult, analyze};
use crate::native::{NativeAttemptEvidence, NativeAttemptRequest, ScriptedScenario, run_attempt};
use crate::prepare::provenance::{copy_tree, hash_file, hash_tree, reset_workspace, write_json};
use crate::prepare::{Prepared, unique_id};
use crate::schedule::{
    ATTEMPT_LIMIT_MS, ExecutionBudget, SCRIPTED_LIMIT_MS, ScheduledAttempt, Segment,
};
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

pub fn execute(
    manifest: &Path,
    selected: Option<&str>,
    original: Option<PathBuf>,
) -> Result<PathBuf> {
    let manifest = fs::canonicalize(manifest)?;
    let prepared = Prepared::load(&manifest)?;
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
            })
            .collect(),
        scripted_execution_ms: 0,
        real_model_execution_ms: 0,
        finished: false,
    };
    let result_path = directory.join("result.json");
    // Record every resolved attempt path before any native execution.
    write_json(&result_path, &result)?;
    let mut scripted_budget = ExecutionBudget::new(SCRIPTED_LIMIT_MS);
    let mut live_budget = ExecutionBudget::new(prepared.mode.live_limit_ms());
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
            continue;
        }
        if budget.remaining_ms() == 0 {
            attempt.reason = Some("segment_budget_exhausted before attempt started".into());
            continue;
        }
        eprintln!(
            "{} ({:?}; {} ms segment allowance left)",
            attempt.scheduled.id,
            attempt.scheduled.segment,
            budget.remaining_ms()
        );
        let timeout = if attempt.scheduled.segment == Segment::RealModel {
            budget.remaining_ms().min(ATTEMPT_LIMIT_MS)
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
        }
        if let Some(native) = &attempt.native {
            budget.charge(native.elapsed_ms);
            if native.status == "timeout" && timeout < ATTEMPT_LIMIT_MS
                || native.status == "timeout" && attempt.scheduled.segment == Segment::Scripted
            {
                attempt.reason = Some("segment_budget_exhausted during attempt".into());
            }
        }
        result.scripted_execution_ms = SCRIPTED_LIMIT_MS - scripted_budget.remaining_ms();
        result.real_model_execution_ms = prepared.mode.live_limit_ms() - live_budget.remaining_ms();
        write_json(&result_path, &result)?;
    }
    result.finished = true;
    write_json(&result_path, &result)?;
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
    ensure!(
        hash_tree(&fixture.snapshot)? == fixture.sha256,
        "fixture changed before reset"
    );
    reset_workspace(&prepared.directory, &prepared.workspace, &fixture.snapshot)
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
            attempt.status = if verified.status == VerificationStatus::Incorrect {
                "incorrect"
            } else {
                "verification_unavailable"
            }
            .into();
            attempt.reason = Some(verified.detail.clone());
        }
        attempt.verifier = Some(verified);
    }
    attempt.final_workspace_sha256 = Some(hash_tree(&prepared.workspace)?);
    preserve_final_changes(
        &fixture.snapshot,
        &prepared.workspace,
        &attempt.evidence_directory.join("final-changes"),
    )?;
    attempt.outside_execution_ms.insert(
        "verificationAndFinalState".into(),
        verification.elapsed().as_millis() as u64,
    );
    let native = attempt.native.as_ref().context("native evidence")?;
    let mut raw = serde_json::to_value(native)?;
    raw["verifier"] = serde_json::to_value(&attempt.verifier)?;
    fs::write(&native.evidence_path, serde_json::to_vec_pretty(&raw)?)?;
    // Persist verified underlying evidence before starting the fallible analyzer.
    write_json(&attempt.evidence_directory.join("attempt.json"), attempt)?;
    let analysis = Instant::now();
    attempt.diagnostics = Some(analyze(
        &prepared.environment.tools["python"].executable.path,
        &prepared.analyzer.path,
        &prepared.analyzer_files,
        native,
        &attempt.evidence_directory,
        &prepared.workspace,
        live,
    ));
    attempt
        .outside_execution_ms
        .insert("analysis".into(), analysis.elapsed().as_millis() as u64);
    write_json(&attempt.evidence_directory.join("attempt.json"), attempt)?;
    Ok(())
}

fn preserve_final_changes(snapshot: &Path, workspace: &Path, destination: &Path) -> Result<()> {
    let mut paths = std::collections::BTreeSet::new();
    for root in [snapshot, workspace] {
        for entry in walkdir::WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| {
                ![".git", "target", "node_modules", "__pycache__"]
                    .contains(&e.file_name().to_string_lossy().as_ref())
            })
        {
            let entry = entry?;
            if entry.file_type().is_file() {
                paths.insert(entry.path().strip_prefix(root)?.to_path_buf());
            }
        }
    }
    fs::create_dir_all(destination)?;
    let mut changes = vec![];
    for relative in paths {
        let before = snapshot.join(&relative);
        let after = workspace.join(&relative);
        let before_hash = before.is_file().then(|| hash_file(&before)).transpose()?;
        let after_hash = after.is_file().then(|| hash_file(&after)).transpose()?;
        if before_hash == after_hash {
            continue;
        }
        if after.is_file() {
            let target = destination.join(&relative);
            fs::create_dir_all(target.parent().context("final change parent")?)?;
            fs::copy(&after, target)?;
        }
        changes.push(json!({"path":relative,"beforeSha256":before_hash,"afterSha256":after_hash}));
    }
    write_json(&destination.join("changes.json"), &changes)
}

pub fn analysis_only(result_path: &Path) -> Result<PathBuf> {
    let mut result: RunResult = crate::prepare::provenance::read_json(result_path)?;
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
            ));
        }
    }
    let output = result.directory.join("result.json");
    write_json(&output, &result)?;
    crate::reports::write(&prepared, &result)?;
    Ok(output)
}
