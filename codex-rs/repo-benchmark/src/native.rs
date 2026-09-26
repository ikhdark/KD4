//! Native app-server execution. Raw evidence is retained even when a turn fails.
//! Diagnostic interpretation belongs to the Python session audit.

mod client;
mod scripted;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

pub use scripted::ScriptedScenario;

#[derive(Debug, Clone)]
pub struct NativeAttemptRequest {
    pub attempt_id: String,
    pub app_server: PathBuf,
    pub cwd: PathBuf,
    pub codex_home: PathBuf,
    pub evidence_dir: PathBuf,
    pub env: BTreeMap<String, String>,
    pub config_overrides: Vec<String>,
    pub expected_config: Value,
    pub prompt: String,
    pub timeout_ms: u64,
    pub scenario: Option<ScriptedScenario>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeAttemptEvidence {
    pub schema_version: u32,
    pub attempt_id: String,
    pub status: String,
    pub elapsed_ms: u64,
    /// Each turn from its `turn/start` request to its terminal notification, summed and
    /// recorded only when every turn finished. `elapsed_ms` additionally includes launch,
    /// the handshake, thread start or resume, and harness checks between turns.
    #[serde(default)]
    pub turn_elapsed_ms: Option<u64>,
    pub cleanup_ms: u64,
    pub thread_id: Option<String>,
    pub completed_turns: usize,
    pub tool_executions: usize,
    pub failure: Option<NativeFailure>,
    pub effective_config: Value,
    pub events: Vec<Value>,
    pub stdout_paths: Vec<PathBuf>,
    pub stderr_paths: Vec<PathBuf>,
    pub rollout_paths: Vec<PathBuf>,
    pub provider_requests_path: Option<PathBuf>,
    pub adaptations: Vec<String>,
    pub evidence_path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NativeFailure {
    pub kind: String,
    pub message: String,
}

/// Never discards a failed native attempt. Errors creating the evidence directory
/// are the only failures that cannot also be durably recorded by this function.
pub fn run_attempt(request: &NativeAttemptRequest) -> NativeAttemptEvidence {
    let mut evidence = NativeAttemptEvidence {
        schema_version: 1,
        attempt_id: request.attempt_id.clone(),
        status: "setup_failed".into(),
        elapsed_ms: 0,
        turn_elapsed_ms: None,
        cleanup_ms: 0,
        thread_id: None,
        completed_turns: 0,
        tool_executions: 0,
        failure: None,
        effective_config: Value::Null,
        events: vec![],
        stdout_paths: vec![],
        stderr_paths: vec![],
        rollout_paths: vec![],
        provider_requests_path: None,
        adaptations: vec![],
        evidence_path: request.evidence_dir.join("native-evidence.json"),
    };
    if let Err(error) = execute(request, &mut evidence) {
        let message = format!("{error:#}");
        let kind = if error.downcast_ref::<client::DeadlineExpired>().is_some() {
            evidence.status = "timeout".into();
            "attempt_timeout"
        } else if evidence.thread_id.is_none() {
            "setup_failure"
        } else {
            evidence.status = "failed".into();
            "native_turn_failure"
        };
        evidence.failure = Some(NativeFailure {
            kind: kind.into(),
            message,
        });
    }
    if let Err(error) = save_evidence(&evidence) {
        evidence.status = "failed".into();
        let previous = evidence
            .failure
            .as_ref()
            .map(|failure| format!("; original failure: {}", failure.message))
            .unwrap_or_default();
        evidence.failure = Some(NativeFailure {
            kind: "evidence_write_failure".into(),
            message: format!("{error:#}{previous}"),
        });
    }
    evidence
}

fn save_evidence(evidence: &NativeAttemptEvidence) -> Result<()> {
    fs::write(
        &evidence.evidence_path,
        serde_json::to_vec_pretty(evidence)?,
    )
    .context("persist native attempt evidence")
}

fn execute(request: &NativeAttemptRequest, evidence: &mut NativeAttemptEvidence) -> Result<()> {
    fs::create_dir_all(&request.evidence_dir)?;
    if !request.cwd.is_absolute()
        || !request.codex_home.is_absolute()
        || !request.app_server.is_absolute()
    {
        bail!(
            "native executable, working directory and isolated home must be absolute prepared paths"
        );
    }
    let scripted = request
        .scenario
        .map(|scenario| scripted::ScriptedProvider::start(scenario, &request.evidence_dir))
        .transpose()?;
    let mut overrides = request.config_overrides.clone();
    if let Some(provider) = &scripted {
        overrides.extend(provider.config_overrides());
        evidence.provider_requests_path = Some(provider.requests_path.clone());
    }
    let started = Instant::now();
    let deadline = started + Duration::from_millis(request.timeout_ms);
    let mut process = spawn_recorded(request, &overrides, started, deadline, None, evidence)?;
    let result = (|| -> Result<()> {
        initialize(&mut process, request, &overrides, evidence)?;
        let prepared_config = fs::read(request.codex_home.join("config.toml"))?;
        let response = process.rpc(
            "thread/start",
            json!({
                "cwd": request.cwd, "ephemeral": false,
                "experimentalRawEvents": true,
            }),
        )?;
        let thread_id = response
            .pointer("/thread/id")
            .and_then(Value::as_str)
            .context("thread/start did not return thread.id")?
            .to_owned();
        evidence.thread_id = Some(thread_id.clone());
        let turns = request
            .scenario
            .map(ScriptedScenario::turn_count)
            .unwrap_or(1);
        let mut turn_elapsed = Duration::ZERO;
        for turn_index in 0..turns {
            if turn_index == 1 && request.scenario == Some(ScriptedScenario::RestartResume) {
                process.stop()?;
                evidence.events.append(&mut process.events);
                process = spawn_recorded(
                    request,
                    &overrides,
                    started,
                    deadline,
                    Some(&prepared_config),
                    evidence,
                )?;
                initialize(&mut process, request, &overrides, evidence)?;
                let resumed = process.rpc(
                    "thread/resume",
                    json!({"threadId": thread_id, "cwd": request.cwd}),
                )?;
                if resumed.pointer("/thread/id").and_then(Value::as_str) != Some(&thread_id) {
                    bail!("thread/resume returned a different thread");
                }
            }
            if let Some(provider) = &scripted {
                provider.begin_turn(turn_index)?;
            }
            let prompt = request
                .scenario
                .map(|scenario| scenario.prompt(&request.prompt, turn_index))
                .unwrap_or_else(|| request.prompt.clone());
            let turn_started = Instant::now();
            let response = process.rpc(
                "turn/start",
                json!({
                    "threadId": thread_id,
                    "input": [{"type": "text", "text": prompt, "text_elements": []}],
                }),
            )?;
            let turn_id = response
                .pointer("/turn/id")
                .and_then(Value::as_str)
                .context("turn/start did not return turn.id")?
                .to_owned();
            let cancel = request
                .scenario
                .is_some_and(scripted::ScriptedScenario::cancel_first)
                && turn_index == 0;
            let mut checkpoint = || {
                scripted
                    .as_ref()
                    .context("cancellation requires a scripted provider")?
                    .cancellation_checkpoint(request, deadline)
            };
            let terminal = process.finish_turn(
                &thread_id,
                &turn_id,
                if cancel { Some(&mut checkpoint) } else { None },
            )?;
            turn_elapsed += turn_started.elapsed();
            evidence.tool_executions += terminal.tool_executions;
            if cancel {
                if terminal.status != "interrupted" {
                    bail!(
                        "cancelled turn ended with {}, expected interrupted",
                        terminal.status
                    );
                }
                if let Some(provider) = &scripted {
                    // The interrupted terminal releases the artificial provider
                    // hold. Cleanup must not wait on a response that the harness
                    // itself keeps blocked while checking child termination.
                    provider.confirm_interrupted();
                    let stopped = provider.verify_cancelled(request, deadline)?;
                    process.events.push(json!({"elapsedMs":started.elapsed().as_millis() as u64,"message":{"method":"repoBenchmark/cancellationStopped","params":stopped}}));
                }
            } else {
                if terminal.status != "completed" {
                    bail!("turn ended with {}: {}", terminal.status, terminal.error);
                }
                if let Some(provider) = &scripted {
                    provider.verify_turn(&request.cwd, turn_index)?;
                }
                evidence.completed_turns += 1;
            }
        }
        evidence.turn_elapsed_ms = Some(turn_elapsed.as_millis() as u64);
        evidence.status = "completed".into();
        Ok(())
    })();
    evidence.elapsed_ms = started.elapsed().as_millis() as u64;
    let cleanup = Instant::now();
    let stopped = process.stop();
    evidence.events.append(&mut process.events);
    evidence.cleanup_ms = cleanup.elapsed().as_millis() as u64;
    collect_rollouts(
        &request.codex_home.join("sessions"),
        &mut evidence.rollout_paths,
    )?;
    collect_rollouts(
        &request.codex_home.join("archived_sessions"),
        &mut evidence.rollout_paths,
    )?;
    if let Some(provider) = &scripted {
        evidence.adaptations = provider.adaptations()?;
    }
    result?;
    stopped?;
    Ok(())
}

fn spawn_recorded(
    request: &NativeAttemptRequest,
    overrides: &[String],
    started: Instant,
    deadline: Instant,
    restart_config: Option<&[u8]>,
    evidence: &mut NativeAttemptEvidence,
) -> Result<client::NativeClient> {
    let launch = usize::from(restart_config.is_some());
    if let Some(config) = restart_config {
        // thread/start can persist workspace trust. Reapply the frozen base on
        // restart while retaining the isolated home's session data.
        fs::write(request.codex_home.join("config.toml"), config)
            .context("restore prepared configuration before native restart")?;
    }
    let process = client::NativeClient::spawn(request, overrides, started, deadline, launch);
    // These logs are opened before process launch. Keep their identities even
    // when an executable is missing or Windows rejects process creation.
    for (suffix, paths) in [
        ("stdout.jsonl", &mut evidence.stdout_paths),
        ("stderr.log", &mut evidence.stderr_paths),
    ] {
        let path = request
            .evidence_dir
            .join(format!("app-server-{launch}.{suffix}"));
        if path.is_file() {
            paths.push(path);
        }
    }
    if process.is_err() {
        evidence.elapsed_ms = started.elapsed().as_millis() as u64;
    }
    process
}

fn initialize(
    client: &mut client::NativeClient,
    request: &NativeAttemptRequest,
    overrides: &[String],
    evidence: &mut NativeAttemptEvidence,
) -> Result<()> {
    client.rpc(
        "initialize",
        json!({
            "clientInfo": {"name": "repo-benchmark", "title": "Repo Benchmark", "version": "1"},
            "capabilities": {"experimentalApi": true}
        }),
    )?;
    client.notify("initialized", json!({}))?;
    let response = client.rpc(
        "config/read",
        json!({"includeLayers": true, "cwd": request.cwd}),
    )?;
    evidence.effective_config = response.clone();
    verify_effective_config(
        &response,
        &request.expected_config,
        &request.codex_home,
        overrides,
    )
}

fn verify_effective_config(
    response: &Value,
    expected: &Value,
    home: &Path,
    overrides: &[String],
) -> Result<()> {
    let effective = response
        .get("config")
        .filter(|value| value.is_object())
        .context("config/read returned no config")?;
    if !expected.is_object() {
        bail!("prepared expected configuration is not an object");
    }
    let config_path = home.join("config.toml");
    let config_path =
        fs::canonicalize(&config_path).context("resolve exact isolated config.toml")?;
    let base: toml::Value =
        toml::from_str(&fs::read_to_string(&config_path)?).context("parse isolated config.toml")?;
    let base = serde_json::to_value(base)?;
    let mut allowed_flags = json!({});
    for setting in overrides {
        let parsed: toml::Value =
            toml::from_str(setting).context("parse prepared native override")?;
        merge_config(&mut allowed_flags, serde_json::to_value(parsed)?)?;
    }
    let mut expected_with_flags = expected.clone();
    merge_config(&mut expected_with_flags, allowed_flags.clone())?;
    let mut actual_with_flags = base.clone();
    merge_config(&mut actual_with_flags, allowed_flags.clone())?;
    if actual_with_flags != expected_with_flags {
        bail!("isolated config.toml contains settings outside the prepared configuration");
    }
    verify_subset(effective, &expected_with_flags, "config")?;
    if effective
        .get("service_tier")
        .is_some_and(|value| !value.is_null())
    {
        bail!("unexpected service_tier in effective benchmark configuration");
    }
    // Effective values alone cannot establish isolation: inspect the sources too.
    let mut user_layers = 0;
    let mut flags_layers = 0;
    for layer in response
        .get("layers")
        .and_then(Value::as_array)
        .context("config/read omitted requested layers")?
    {
        let config = layer
            .get("config")
            .filter(|value| value.is_object())
            .context("config/read returned a malformed or missing layer config")?;
        let name = layer
            .get("name")
            .filter(|value| value.is_object())
            .context("config/read returned a malformed layer name")?;
        let source = name
            .get("type")
            .and_then(Value::as_str)
            .context("config/read layer has no source type")?;
        if layer
            .get("disabledReason")
            .is_some_and(|reason| !reason.is_null())
        {
            continue;
        }
        match source {
            "user" => {
                user_layers += 1;
                let path = name
                    .get("file")
                    .and_then(Value::as_str)
                    .context("user layer omitted config file")?;
                if name
                    .get("profile")
                    .is_some_and(|profile| !profile.is_null())
                {
                    bail!("benchmark user configuration must not select a profile");
                }
                let reported = Path::new(path);
                if !reported.is_absolute()
                    || fs::canonicalize(path).context("resolve reported user config file")?
                        != config_path
                {
                    bail!("user layer must use the exact isolated home/config.toml");
                }
                // Native config migration adds schema metadata in memory, even
                // when the isolated file has no explicit version.
                let mut reported_base = config.clone();
                if base.get("config_version").is_none()
                    && reported_base.get("config_version") == Some(&json!(1))
                    && let Some(reported_map) = reported_base.as_object_mut()
                {
                    reported_map.remove("config_version");
                }
                if reported_base != base {
                    bail!("reported user config differs from isolated config.toml");
                }
            }
            "sessionFlags" => {
                flags_layers += 1;
                if config != &allowed_flags {
                    bail!("sessionFlags contains settings other than the prepared overrides");
                }
            }
            "system"
            | "project"
            | "mdm"
            | "enterpriseManaged"
            | "legacyManagedConfigTomlFromFile"
            | "legacyManagedConfigTomlFromMdm"
                // An otherwise empty layer can acquire the same schema marker.
                if config.as_object().is_some_and(serde_json::Map::is_empty)
                    || config == &json!({"config_version": 1}) => {}
            _ => bail!("unexpected nonempty configuration layer: {name}"),
        }
    }
    if user_layers != 1 {
        bail!("config/read must report exactly one active isolated user configuration layer");
    }
    if flags_layers > 1
        || (flags_layers == 0 && allowed_flags.as_object().is_some_and(|map| !map.is_empty()))
    {
        bail!("config/read must report the prepared sessionFlags exactly once");
    }
    Ok(())
}

fn merge_config(target: &mut Value, source: Value) -> Result<()> {
    let target = target
        .as_object_mut()
        .context("configuration must be an object")?;
    for (key, value) in source.as_object().context("override must be an object")? {
        if value.is_object()
            && let Some(nested) = target.get_mut(key).filter(|slot| slot.is_object())
        {
            merge_config(nested, value.clone())?;
        } else {
            target.insert(key.clone(), value.clone());
        }
    }
    Ok(())
}

fn verify_subset(actual: &Value, expected: &Value, path: &str) -> Result<()> {
    if let Some(fields) = expected.as_object() {
        for (key, value) in fields {
            verify_subset(
                actual.get(key).unwrap_or(&Value::Null),
                value,
                &format!("{path}.{key}"),
            )?;
        }
    } else if actual != expected {
        bail!("effective setting mismatch at {path}: expected {expected}, got {actual}");
    }
    Ok(())
}

fn collect_rollouts(path: &Path, result: &mut Vec<PathBuf>) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            collect_rollouts(&entry.path(), result)?;
        } else if kind.is_file() && entry.path().extension().is_some_and(|ext| ext == "jsonl") {
            result.push(entry.path());
        }
    }
    result.sort();
    Ok(())
}

#[cfg(test)]
mod tests;
