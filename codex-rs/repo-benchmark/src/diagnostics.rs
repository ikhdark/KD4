//! Execute the frozen Python analyzer after native execution. This module owns
//! process and JSON validation only; all diagnostic definitions stay in Python.

use crate::native::NativeAttemptEvidence;
use crate::prepare::provenance::FileIdentity;
use anyhow::{Context, Result, bail, ensure};
use codex_app_server_test_client::terminate_owned_process;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const ANALYSIS_TIMEOUT: Duration = Duration::from_secs(120);
const AUDIT_SCHEMA_VERSION: u64 = 18;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticResult {
    pub status: String,
    pub reports: Vec<Value>,
    pub stdout_paths: Vec<PathBuf>,
    pub stderr_paths: Vec<PathBuf>,
    pub error: Option<String>,
}

pub fn analyze(
    python: &Path,
    analyzer: &Path,
    analyzer_files: &[FileIdentity],
    native: &NativeAttemptEvidence,
    evidence_dir: &Path,
    workspace: &Path,
    tokens: bool,
) -> DiagnosticResult {
    analyze_with_timeout(
        python,
        analyzer,
        analyzer_files,
        native,
        evidence_dir,
        workspace,
        tokens,
        ANALYSIS_TIMEOUT,
    )
}

#[allow(clippy::too_many_arguments)]
fn analyze_with_timeout(
    python: &Path,
    analyzer: &Path,
    analyzer_files: &[FileIdentity],
    native: &NativeAttemptEvidence,
    evidence_dir: &Path,
    workspace: &Path,
    tokens: bool,
    timeout: Duration,
) -> DiagnosticResult {
    let mut result = DiagnosticResult {
        status: "failed".into(),
        reports: vec![],
        stdout_paths: vec![],
        stderr_paths: vec![],
        error: None,
    };
    if let Err(error) = verify_analyzer(analyzer, analyzer_files) {
        result.error = Some(format!("{error:#}"));
        return result;
    }
    if let Err(error) = fs::create_dir_all(evidence_dir) {
        result.error = Some(format!("create diagnostics evidence directory: {error}"));
        return result;
    }
    let sources = if native.rollout_paths.is_empty() {
        vec![None]
    } else {
        native
            .rollout_paths
            .iter()
            .map(|path| Some(path.as_path()))
            .collect()
    };
    let deadline = Instant::now() + timeout;
    let mut errors = Vec::new();
    for (index, source) in sources.iter().enumerate() {
        let stdout = evidence_dir.join(format!("diagnostics-{index}.stdout.json"));
        let stderr = evidence_dir.join(format!("diagnostics-{index}.stderr.log"));
        result.stdout_paths.push(stdout.clone());
        result.stderr_paths.push(stderr.clone());
        let analysis = (|| -> Result<Value> {
            // Recheck immediately before every execution, including dependencies.
            verify_analyzer(analyzer, analyzer_files)?;
            let stdout_file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&stdout)?;
            let stderr_file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&stderr)?;
            let mut command = Command::new(python);
            command.arg("-B").arg(analyzer);
            if let Some(source) = source {
                command.arg(source);
            }
            // Runner evidence describes the whole attempt, so supply it once even
            // when a restart or child session produced multiple rollout files.
            if index == 0 {
                command.arg("--runner-evidence").arg(&native.evidence_path);
            }
            command
                .args(["--tokens", if tokens { "on" } else { "off" }])
                .arg("--repo-root")
                .arg(workspace)
                .arg("--json")
                .current_dir(analyzer.parent().context("analyzer parent directory")?)
                .env_remove("PYTHONPATH")
                .env_remove("PYTHONHOME")
                .env("PYTHONNOUSERSITE", "1")
                .env("PYTHONIOENCODING", "utf-8")
                .stdin(Stdio::null())
                .stdout(Stdio::from(stdout_file))
                .stderr(Stdio::from(stderr_file));
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                command.creation_flags(0x08000000);
            }
            ensure!(
                Instant::now() < deadline,
                "diagnostic analysis exceeded its shared {} second deadline",
                timeout.as_secs()
            );
            let mut child = command
                .spawn()
                .context("start frozen Python session analyzer")?;
            let status = loop {
                if let Some(status) = child.try_wait()? {
                    break status;
                }
                if Instant::now() >= deadline {
                    terminate_owned_process(&mut child)
                        .context("terminate timed-out diagnostic analyzer")?;
                    bail!(
                        "diagnostic analysis exceeded its shared {} second deadline",
                        timeout.as_secs()
                    );
                }
                thread::sleep(Duration::from_millis(10));
            };
            ensure!(
                status.success(),
                "Python session analyzer failed with {status}; see {}",
                stderr.display()
            );
            let report: Value = serde_json::from_slice(&fs::read(&stdout)?)
                .context("Python session analyzer returned invalid JSON")?;
            ensure!(
                report.get("schemaVersion").and_then(Value::as_u64) == Some(AUDIT_SCHEMA_VERSION),
                "unsupported Python audit schema: expected {AUDIT_SCHEMA_VERSION}"
            );
            ensure!(
                report.get("tokenAnalysisEnabled").and_then(Value::as_bool) == Some(tokens),
                "Python audit did not honor requested token analysis mode"
            );
            ensure!(
                report
                    .get("runnerDiagnostics")
                    .is_some_and(Value::is_object),
                "Python audit omitted runner diagnostics contract"
            );
            Ok(report)
        })();
        match analysis {
            Ok(report) => result.reports.push(report),
            Err(error) => errors.push(format!("session {index}: {error:#}")),
        }
    }
    result.status = if errors.is_empty() {
        "available"
    } else if result.reports.is_empty() {
        "failed"
    } else {
        "partial"
    }
    .into();
    if !errors.is_empty() {
        result.error = Some(errors.join("\n"));
    }
    result
}

fn verify_analyzer(analyzer: &Path, files: &[FileIdentity]) -> Result<()> {
    let analyzer = fs::canonicalize(analyzer).context("resolve frozen Python analyzer")?;
    ensure!(
        files.iter().any(|file| file.path == analyzer),
        "Python analyzer is absent from prepared file provenance"
    );
    for file in files {
        file.verify()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn evidence(root: &Path) -> NativeAttemptEvidence {
        let evidence_path = root.join("native-evidence.json");
        let native = NativeAttemptEvidence {
            schema_version: 1,
            attempt_id: "failed-startup".into(),
            status: "setup_failed".into(),
            elapsed_ms: 40,
            cleanup_ms: 0,
            thread_id: None,
            completed_turns: 0,
            tool_executions: 0,
            failure: Some(crate::native::NativeFailure {
                kind: "setup_failure".into(),
                message: "cannot access required tool".into(),
            }),
            effective_config: Value::Null,
            events: vec![],
            stdout_paths: vec![],
            stderr_paths: vec![],
            rollout_paths: vec![],
            provider_requests_path: None,
            adaptations: vec![],
            evidence_path,
        };
        fs::write(&native.evidence_path, serde_json::to_vec(&native).unwrap()).unwrap();
        native
    }

    #[test]
    fn real_python_audit_consumes_startup_failure_and_disables_token_analysis() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        let temp = tempfile::tempdir().unwrap();
        let frozen = temp.path().join("analyzer");
        fs::create_dir(&frozen).unwrap();
        let mut files = Vec::new();
        for name in [
            "kd4_turn_latency_audit.py",
            "kd4_timing_analysis.py",
            "kd4_first_useful_action_analysis.py",
            "rollout_snapshot.py",
        ] {
            let target = frozen.join(name);
            fs::copy(root.join("scripts").join(name), &target).unwrap();
            files.push(FileIdentity::record(&target).unwrap());
        }
        let native = evidence(temp.path());
        let python = which::which("python").expect("Python is a required benchmark dependency");
        let original = fs::read(&native.evidence_path).unwrap();
        let result = analyze(
            &python,
            &frozen.join("kd4_turn_latency_audit.py"),
            &files,
            &native,
            &temp.path().join("diagnostics"),
            root,
            false,
        );
        assert_eq!(result.status, "available", "{:?}", result.error);
        assert_eq!(result.reports.len(), 1);
        assert_eq!(result.reports[0]["tokenAnalysisEnabled"], false);
        assert_eq!(
            result.reports[0]["runnerDiagnostics"]["attemptId"],
            "failed-startup"
        );
        assert_eq!(
            result.reports[0]["runnerDiagnostics"]["status"],
            "setup_failed"
        );
        assert_eq!(fs::read(&native.evidence_path).unwrap(), original);
    }

    #[test]
    fn tampered_or_invalid_analyzer_cannot_replace_native_evidence() {
        let temp = tempfile::tempdir().unwrap();
        let native = evidence(temp.path());
        let original = fs::read(&native.evidence_path).unwrap();
        let analyzer = temp.path().join("analyzer.py");
        fs::write(&analyzer, "print('this is not JSON')\n").unwrap();
        let file = FileIdentity::record(&analyzer).unwrap();
        let python = which::which("python").unwrap();
        let failed = analyze(
            &python,
            &analyzer,
            std::slice::from_ref(&file),
            &native,
            &temp.path().join("invalid"),
            temp.path(),
            true,
        );
        assert_eq!(failed.status, "failed");
        assert!(failed.error.unwrap().contains("invalid JSON"));
        assert!(failed.reports.is_empty());
        fs::write(
            &analyzer,
            format!("print({:?})", json!({"schemaVersion":18}).to_string()),
        )
        .unwrap();
        let changed = analyze(
            &python,
            &analyzer,
            &[file],
            &native,
            &temp.path().join("tampered"),
            temp.path(),
            true,
        );
        assert_eq!(changed.status, "failed");
        assert!(changed.error.unwrap().contains("changed prepared artifact"));
        assert!(
            changed.stdout_paths.is_empty(),
            "a changed analyzer must not be executed"
        );
        assert_eq!(fs::read(&native.evidence_path).unwrap(), original);
    }

    #[test]
    fn hung_python_analyzer_is_terminated_without_changing_native_evidence() {
        let temp = tempfile::tempdir().unwrap();
        let native = evidence(temp.path());
        let original = fs::read(&native.evidence_path).unwrap();
        let analyzer = temp.path().join("hang.py");
        fs::write(&analyzer, "import time\ntime.sleep(30)\n").unwrap();
        let file = FileIdentity::record(&analyzer).unwrap();
        let python = which::which("python").unwrap();
        let started = Instant::now();
        let result = analyze_with_timeout(
            &python,
            &analyzer,
            &[file],
            &native,
            &temp.path().join("timeout"),
            temp.path(),
            false,
            Duration::from_millis(100),
        );
        assert_eq!(result.status, "failed");
        assert!(result.error.unwrap().contains("deadline"));
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the sleeping process must not hold up analysis"
        );
        assert_eq!(fs::read(&native.evidence_path).unwrap(), original);
    }
}
