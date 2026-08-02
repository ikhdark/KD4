use super::*;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;

async fn ledger_fixture() -> (tempfile::TempDir, PathBuf, TaskEvidenceLedger) {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    let codex_home = temp.path().join("home");
    tokio::fs::create_dir_all(repo.join(".git"))
        .await
        .expect("git dir");
    tokio::fs::write(repo.join("kd4_features.toml"), "# fixture")
        .await
        .expect("manifest");
    let cwd = AbsolutePathBuf::from_absolute_path(&repo).expect("absolute repo");
    let ledger = TaskEvidenceLedger::load_or_new(codex_home, ThreadId::new(), cwd.as_path()).await;
    (temp, repo, ledger)
}

fn legacy_task_evidence_fixture(
    schema_version: u32,
    thread_id: &str,
    repo: &Path,
    step_status: &str,
) -> Value {
    let mut value = serde_json::json!({
        "schema_version": schema_version,
        "thread_id": thread_id,
        "started_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-01T00:00:00Z",
        "start": {
            "cwd": repo.to_string_lossy(),
            "repository_root": repo.to_string_lossy(),
            "commit_hash": null,
            "branch": null,
            "repository_url": null
        },
        "evidence_epoch": 0,
        "last_mutation_at": null,
        "plan": [{
            "id": "legacy-step",
            "step": "legacy step",
            "status": step_status,
            "depends_on": [],
            "acceptance_criteria": [],
            "runtime_paths": [],
            "generated_artifacts": [],
            "risks": [],
            "requires_desktop_activation": false,
            "edit_paths": [],
            "validation_receipt_ids": []
        }],
        "active_step_id": null,
        "edit_intents": [],
        "edit_receipts": [],
        "command_receipts": [],
        "validation_receipts": [],
        "generated_artifact_requirements": [],
        "generated_artifact_hashes": {},
        "latest_file_hashes": {},
        "risks": [],
        "validation_epoch": null,
        "wiring_receipt": null,
        "desktop_activation_receipt": null,
        "repair_turns_used": 0,
        "completion": {
            "status": "passed",
            "reasons": [],
            "evidence_path": null
        }
    });
    if schema_version >= 2 {
        let object = value.as_object_mut().expect("legacy document object");
        object.insert("revision".to_string(), serde_json::json!(7));
        object.insert(
            "latest_generated_artifact_hashes".to_string(),
            serde_json::json!({}),
        );
        object.insert(
            "next_edit_receipt_sequence".to_string(),
            serde_json::json!(3),
        );
        object.insert(
            "next_command_receipt_sequence".to_string(),
            serde_json::json!(4),
        );
        object.insert(
            "next_validation_receipt_sequence".to_string(),
            serde_json::json!(5),
        );
    }
    value
}

#[cfg(unix)]
fn create_directory_alias(target: &Path, alias: &Path) {
    std::os::unix::fs::symlink(target, alias).expect("create directory symlink");
}

#[cfg(windows)]
fn create_directory_alias(target: &Path, alias: &Path) {
    let output = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(alias)
        .arg(target)
        .output()
        .expect("create directory junction");
    assert!(
        output.status.success(),
        "mklink /J failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn existing_evidence_reuses_canonical_repository_identity() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    let repo_alias = temp.path().join("repo-alias");
    let codex_home = temp.path().join("home");
    tokio::fs::create_dir_all(repo.join(".git"))
        .await
        .expect("git dir");
    tokio::fs::write(repo.join("kd4_features.toml"), "# fixture")
        .await
        .expect("manifest");
    create_directory_alias(&repo, &repo_alias);

    let thread_id = ThreadId::new();
    let evidence_path = codex_home
        .join("task-evidence")
        .join(format!("{thread_id}.json"));
    let ledger = TaskEvidenceLedger::load_or_new(codex_home.clone(), thread_id, &repo).await;
    drop(ledger);

    let mut value = serde_json::from_slice::<Value>(
        &tokio::fs::read(&evidence_path)
            .await
            .expect("persisted evidence"),
    )
    .expect("valid evidence");
    value["start"]["repository_root"] = Value::String(repo_alias.to_string_lossy().into_owned());
    tokio::fs::write(
        &evidence_path,
        serde_json::to_vec_pretty(&value).expect("serialize evidence"),
    )
    .await
    .expect("rewrite legacy repository root");

    let reloaded = TaskEvidenceLedger::load_or_new(codex_home.clone(), thread_id, &repo).await;
    {
        let guard = reloaded.document.lock().await;
        let document = guard.as_ref().expect("document");
        assert_eq!(document.revision, 2);
        assert_eq!(
            PathBuf::from(&document.start.repository_root),
            canonical_repository_root(&repo)
        );
    }

    let mut entries = tokio::fs::read_dir(codex_home.join("task-evidence"))
        .await
        .expect("evidence directory");
    while let Some(entry) = entries.next_entry().await.expect("evidence entry") {
        assert!(
            !entry.file_name().to_string_lossy().ends_with(".preserved"),
            "matching repository evidence must not be quarantined"
        );
    }
}

#[cfg(windows)]
#[test]
fn repository_identity_canonicalizes_drive_letter_case() {
    let temp = tempfile::tempdir().expect("tempdir");
    let canonical = canonical_repository_root(temp.path());
    let mut alternate = canonical.to_string_lossy().into_owned();
    let drive = alternate.as_bytes().first().copied().expect("drive letter");
    assert_eq!(alternate.as_bytes().get(1), Some(&b':'));
    let alternate_drive = if drive.is_ascii_uppercase() {
        drive.to_ascii_lowercase()
    } else {
        drive.to_ascii_uppercase()
    };
    alternate.replace_range(0..1, &(alternate_drive as char).to_string());

    assert!(repository_root_paths_equal(
        &canonical,
        Path::new(&alternate)
    ));
    assert!(repository_roots_match(&canonical, Path::new(&alternate)));
}

#[test]
fn persisted_repository_identity_must_be_absolute() {
    assert!(!recorded_repository_root_matches(".", Path::new(".")));
}

#[tokio::test]
async fn ledger_repo_root_must_match_the_task_evidence_root() {
    let (temp, repo, ledger) = ledger_fixture().await;
    let other_repo = temp.path().join("other-repo");
    tokio::fs::create_dir_all(&other_repo)
        .await
        .expect("other repo");

    assert!(ledger.matches_repo_root(&repo));
    assert!(ledger.matches_repo_root(&repo.join(".")));
    assert!(!ledger.matches_repo_root(&other_repo));
    assert!(!TaskEvidenceLedger::disabled().matches_repo_root(&repo));
}

async fn initialize_git_repo(repo: &Path) {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["init", "--quiet"])
        .output()
        .await
        .expect("git init should run");
    assert!(
        output.status.success(),
        "git init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn evidence_result(producer: &str, text: &str, provider_fields: Value) -> CallToolResult {
    let mut structured = provider_fields.as_object().cloned().unwrap_or_default();
    structured.insert(
        "evidenceMeta".to_string(),
        serde_json::json!({
            "schemaVersion": 1,
            "producer": producer,
            "operation": "inspect",
            "evidenceBearing": true,
            "payloadCompleteness": "complete",
            "truncated": false,
            "approximate": false,
            "limitations": [],
            "snapshot": "snapshot-1"
        }),
    );
    CallToolResult {
        content: vec![serde_json::json!({"type": "text", "text": text})],
        structured_content: Some(Value::Object(structured)),
        is_error: None,
        meta: None,
    }
}

async fn record_fixture_evidence(
    result: &CallToolResult,
) -> (tempfile::TempDir, TaskEvidenceLedger) {
    let (temp, _repo, ledger) = ledger_fixture().await;
    assert_eq!(
        ledger
            .record_external_mcp_evidence("server", "raw-tool", "call-1", result)
            .await,
        ExternalEvidenceCapture::Stored
    );
    (temp, ledger)
}

#[tokio::test]
async fn external_evidence_extracts_generic_v1_envelope() {
    let (_temp, ledger) = record_fixture_evidence(&evidence_result(
        "diagnostic-provider",
        "diagnostic",
        serde_json::json!({"facts": [1]}),
    ))
    .await;
    let guard = ledger.document.lock().await;
    let receipt = guard
        .as_ref()
        .and_then(|document| document.external_evidence.last())
        .expect("receipt");
    assert_eq!(receipt.producer, "diagnostic-provider");
    assert_eq!(receipt.producer_schema_version, 1);
    assert_eq!(receipt.server_name, "server");
    assert_eq!(receipt.tool_name, "raw-tool");
    assert_eq!(receipt.provider_snapshot.as_deref(), Some("snapshot-1"));
}

#[tokio::test]
async fn external_evidence_keeps_provider_nonzero_verdict_transport_successful() {
    let result = evidence_result(
        "wiring-provider",
        "static wiring failed",
        serde_json::json!({"exit_code": 7, "verdict": "FAILED"}),
    );
    let (_temp, ledger) = record_fixture_evidence(&result).await;
    let guard = ledger.document.lock().await;
    let receipt = guard
        .as_ref()
        .and_then(|document| document.external_evidence.last())
        .expect("receipt");
    assert!(receipt.tool_success);
}

#[tokio::test]
async fn external_evidence_preserves_unregistered_producer_identifier() {
    let producer = "  vendor.example/evidence:v1  ";
    let result = evidence_result(
        producer,
        "symbol context",
        serde_json::json!({"symbols": ["alpha"]}),
    );
    let (_temp, ledger) = record_fixture_evidence(&result).await;
    let guard = ledger.document.lock().await;
    let receipt = guard
        .as_ref()
        .and_then(|document| document.external_evidence.last())
        .expect("receipt");
    assert_eq!(receipt.producer, producer);
    assert_eq!(receipt.payload_completeness, EvidenceCompleteness::Complete);
}

#[tokio::test]
async fn external_evidence_unknown_schema_is_ignored_with_warning() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    let mut result = evidence_result("diagnostic-provider", "diagnostic", serde_json::json!({}));
    result.structured_content.as_mut().expect("structured")["evidenceMeta"]["schemaVersion"] =
        serde_json::json!(2);
    assert!(matches!(
        ledger
            .record_external_mcp_evidence("server", "tool", "call", &result)
            .await,
        ExternalEvidenceCapture::Warning(_)
    ));
    assert!(
        ledger
            .document
            .lock()
            .await
            .as_ref()
            .expect("document")
            .external_evidence
            .is_empty()
    );
}

#[tokio::test]
async fn external_evidence_blank_producer_is_ignored_with_warning() {
    for producer in ["", "   ", "\t\r\n"] {
        let (_temp, _repo, ledger) = ledger_fixture().await;
        let result = evidence_result(producer, "diagnostic", serde_json::json!({}));
        assert!(
            matches!(
                ledger
                    .record_external_mcp_evidence("server", "tool", "call", &result)
                    .await,
                ExternalEvidenceCapture::Warning(_)
            ),
            "{producer:?}"
        );
        assert!(
            ledger
                .document
                .lock()
                .await
                .as_ref()
                .expect("document")
                .external_evidence
                .is_empty(),
            "{producer:?}"
        );
    }
}

async fn assert_external_evidence_rejected(
    result: &CallToolResult,
    expected_warning: &'static str,
) {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    assert_eq!(
        ledger
            .record_external_mcp_evidence("server", "tool", "call", result)
            .await,
        ExternalEvidenceCapture::Warning(expected_warning)
    );
    assert!(
        ledger
            .document
            .lock()
            .await
            .as_ref()
            .expect("document")
            .external_evidence
            .is_empty()
    );
}

#[tokio::test]
async fn external_evidence_without_snapshot_remains_ingress_compatible() {
    let mut result = evidence_result("test-provider", "diagnostic", serde_json::json!({}));
    result.structured_content.as_mut().expect("structured")["evidenceMeta"]
        .as_object_mut()
        .expect("evidenceMeta object")
        .remove("snapshot");

    let (_temp, ledger) = record_fixture_evidence(&result).await;
    let guard = ledger.document.lock().await;
    let receipt = guard
        .as_ref()
        .and_then(|document| document.external_evidence.last())
        .expect("receipt");
    assert_eq!(receipt.provider_snapshot, None);
}

#[tokio::test]
async fn external_evidence_rejects_complete_truncated_payload() {
    let mut result = evidence_result("test-provider", "diagnostic", serde_json::json!({}));
    result.structured_content.as_mut().expect("structured")["evidenceMeta"]["truncated"] =
        serde_json::json!(true);

    assert_external_evidence_rejected(
        &result,
        "MCP evidenceMeta complete payload cannot be truncated and was ignored",
    )
    .await;
}

#[tokio::test]
async fn persisted_external_evidence_from_before_strict_ingress_still_loads() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    let home = ledger.codex_home.as_ref().expect("codex home").clone();
    let thread_id = ledger.thread_id.as_ref().expect("thread id").clone();
    let evidence_path = ledger
        .evidence_path
        .as_ref()
        .expect("evidence path")
        .clone();
    let result = evidence_result("test-provider", "diagnostic", serde_json::json!({}));
    assert_eq!(
        ledger
            .record_external_mcp_evidence("server", "tool", "call", &result)
            .await,
        ExternalEvidenceCapture::Stored
    );
    drop(ledger);

    let mut persisted: Value = serde_json::from_slice(
        &tokio::fs::read(&evidence_path)
            .await
            .expect("persisted evidence"),
    )
    .expect("valid evidence");
    let receipt = persisted["external_evidence"][0]
        .as_object_mut()
        .expect("external evidence receipt");
    receipt.insert("provider_snapshot".to_string(), Value::Null);
    receipt.insert("truncated".to_string(), Value::Bool(true));
    tokio::fs::write(
        &evidence_path,
        serde_json::to_vec_pretty(&persisted).expect("serialize legacy evidence"),
    )
    .await
    .expect("write legacy evidence");

    let reloaded = TaskEvidenceLedger::load_or_new(
        home,
        ThreadId::from_string(&thread_id).expect("thread id"),
        &repo,
    )
    .await;
    let guard = reloaded.document.lock().await;
    let receipt = guard
        .as_ref()
        .and_then(|document| document.external_evidence.last())
        .expect("reloaded external evidence receipt");
    assert_eq!(receipt.provider_snapshot, None);
    assert_eq!(receipt.payload_completeness, EvidenceCompleteness::Complete);
    assert!(receipt.truncated);
}

#[tokio::test]
async fn unrelated_generic_complete_field_is_not_external_evidence() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    let result = CallToolResult {
        content: vec![],
        structured_content: Some(serde_json::json!({"complete": true, "truncated": false})),
        is_error: None,
        meta: None,
    };
    assert_eq!(
        ledger
            .record_external_mcp_evidence("server", "tool", "call", &result)
            .await,
        ExternalEvidenceCapture::Ignored
    );
}

#[test]
fn external_evidence_hash_is_stable_under_object_key_reordering() {
    let left = evidence_result(
        "test-provider",
        "same",
        serde_json::from_str(r#"{"b":2,"a":{"z":1,"y":0}}"#).expect("json"),
    );
    let right = evidence_result(
        "test-provider",
        "same",
        serde_json::from_str(r#"{"a":{"y":0,"z":1},"b":2}"#).expect("json"),
    );
    let left = serde_json::to_vec(&canonical_mcp_result_payload(&left)).expect("canonical");
    let right = serde_json::to_vec(&canonical_mcp_result_payload(&right)).expect("canonical");
    assert_eq!(Sha256::digest(left), Sha256::digest(right));
}

#[test]
fn external_evidence_hash_includes_text_content() {
    let left = evidence_result("test-provider", "first", serde_json::json!({"value": 1}));
    let right = evidence_result("test-provider", "second", serde_json::json!({"value": 1}));
    let left = serde_json::to_vec(&canonical_mcp_result_payload(&left)).expect("canonical");
    let right = serde_json::to_vec(&canonical_mcp_result_payload(&right)).expect("canonical");
    assert_ne!(Sha256::digest(left), Sha256::digest(right));
}

#[tokio::test]
async fn external_evidence_inline_payload_round_trips() {
    let result = evidence_result(
        "test-provider",
        "small",
        serde_json::json!({"value": [1, 2]}),
    );
    let expected = canonical_mcp_result_payload(&result);
    let (_temp, ledger) = record_fixture_evidence(&result).await;
    let guard = ledger.document.lock().await;
    let receipt = guard
        .as_ref()
        .and_then(|document| document.external_evidence.last())
        .expect("receipt");
    assert_eq!(receipt.payload.as_ref(), Some(&expected));
    assert_eq!(receipt.payload_artifact_id, None);
}

#[tokio::test]
async fn oversized_external_evidence_uses_opaque_artifact_id() {
    let result = evidence_result(
        "test-provider",
        &"large-payload-".repeat(3_000),
        serde_json::json!({"value": "large"}),
    );
    let expected = canonical_mcp_result_payload(&result);
    let expected_bytes = serde_json::to_vec(&expected).expect("canonical");
    let (_temp, ledger) = record_fixture_evidence(&result).await;
    let (artifact_id, payload) = {
        let guard = ledger.document.lock().await;
        let receipt = guard
            .as_ref()
            .and_then(|document| document.external_evidence.last())
            .expect("receipt");
        (
            receipt.payload_artifact_id.clone().expect("artifact id"),
            receipt.payload.clone().expect("summary"),
        )
    };
    assert!(!artifact_id.contains(['/', '\\']));
    assert_eq!(
        payload["artifact"]["encoding"],
        "KD4_EXTERNAL_EVIDENCE_CANONICAL_JSON_STRING_CHUNKS_V1"
    );
    assert!(
        !serde_json::to_string(&payload)
            .expect("summary")
            .contains("tool-output")
    );
    let artifact_path = ledger
        .codex_home
        .as_ref()
        .expect("codex home")
        .join("tool-output")
        .join(ledger.thread_id.as_ref().expect("thread"))
        .join(format!("{artifact_id}.log"));
    let artifact = tokio::fs::read_to_string(artifact_path)
        .await
        .expect("artifact");
    let mut recovered = String::new();
    let mut lines = artifact.lines();
    assert_eq!(
        lines.next(),
        Some(EXTERNAL_EVIDENCE_ARTIFACT_HEADER.trim_end())
    );
    for line in lines {
        recovered.push_str(&serde_json::from_str::<String>(line).expect("encoded chunk"));
    }
    assert_eq!(recovered.as_bytes(), expected_bytes);
}

#[tokio::test]
async fn oversized_external_evidence_summary_is_bounded_under_adversarial_metadata() {
    let mut result = evidence_result(
        "test-provider",
        "small text",
        serde_json::json!({"value": 1}),
    );
    let huge = "x".repeat(EXTERNAL_EVIDENCE_INLINE_PAYLOAD_BYTES * 4);
    let structured = result.structured_content.as_mut().expect("structured");
    structured["evidenceMeta"]["operation"] = Value::String(huge.clone());
    structured["evidenceMeta"]["snapshot"] = Value::String(huge.clone());
    structured["evidenceMeta"]["limitations"] = serde_json::json!([huge.clone()]);
    structured
        .as_object_mut()
        .expect("object")
        .insert(huge, serde_json::json!("provider value"));

    let (_temp, ledger) = record_fixture_evidence(&result).await;
    let guard = ledger.document.lock().await;
    let receipt = guard
        .as_ref()
        .and_then(|document| document.external_evidence.last())
        .expect("receipt");
    let payload = receipt.payload.as_ref().expect("bounded summary");
    assert!(
        serde_json::to_vec(payload).expect("summary").len()
            <= EXTERNAL_EVIDENCE_INLINE_PAYLOAD_BYTES
    );
    assert_eq!(payload["evidenceMetaSummary"]["producer"], "test-provider");
    assert_eq!(payload["evidenceMetaSummary"]["schemaVersion"], 1);
    assert_eq!(
        payload["evidenceMetaSummary"]["payloadCompleteness"],
        "complete"
    );
    assert_eq!(
        payload["artifact"]["id"],
        receipt.payload_artifact_id.as_deref().expect("artifact id")
    );
    assert_eq!(
        payload["artifact"]["encoding"],
        "KD4_EXTERNAL_EVIDENCE_CANONICAL_JSON_STRING_CHUNKS_V1"
    );
    assert_eq!(payload["isError"], Value::Null);
}

#[tokio::test]
async fn referenced_external_evidence_artifact_survives_restart_and_retention_pressure() {
    let result = evidence_result(
        "test-provider",
        &"restart-retained-".repeat(2_000),
        serde_json::json!({"value": 1}),
    );
    let (temp, repo, ledger) = ledger_fixture().await;
    let home = ledger.codex_home.as_ref().expect("codex home").clone();
    let thread_id = ledger.thread_id.as_ref().expect("thread id").clone();
    assert_eq!(
        ledger
            .record_external_mcp_evidence("server", "tool", "call", &result)
            .await,
        ExternalEvidenceCapture::Stored
    );
    let artifact_id = ledger
        .document
        .lock()
        .await
        .as_ref()
        .and_then(|document| document.external_evidence.last())
        .and_then(|receipt| receipt.payload_artifact_id.clone())
        .expect("artifact id");
    drop(ledger);

    let reloaded = TaskEvidenceLedger::load_or_new(
        home.clone(),
        ThreadId::from_string(&thread_id).expect("thread id"),
        &repo,
    )
    .await;
    for index in 0..(crate::tools::command_output_artifact::max_retained_artifacts_per_thread() + 5)
    {
        crate::tools::command_output_artifact::create_raw_output_artifact(
            &home,
            &thread_id,
            format!("generic-{index}").as_bytes(),
        )
        .await;
    }
    let path = home
        .join("tool-output")
        .join(&thread_id)
        .join(format!("{artifact_id}.log"));
    assert!(path.is_file());
    assert!(path.with_extension("evidence-protected").is_file());
    assert_eq!(
        reloaded
            .document
            .lock()
            .await
            .as_ref()
            .and_then(|document| document.external_evidence.last())
            .and_then(|receipt| receipt.payload_artifact_id.as_deref()),
        Some(artifact_id.as_str())
    );
    drop(temp);
}

#[tokio::test]
async fn restart_drops_external_receipt_whose_payload_artifact_is_missing() {
    let result = evidence_result(
        "test-provider",
        &"missing-on-restart-".repeat(2_000),
        serde_json::json!({"value": 1}),
    );
    let (_temp, repo, ledger) = ledger_fixture().await;
    let home = ledger.codex_home.as_ref().expect("codex home").clone();
    let thread_id = ledger.thread_id.as_ref().expect("thread id").clone();
    let evidence_path = ledger
        .evidence_path
        .as_ref()
        .expect("evidence path")
        .clone();
    assert_eq!(
        ledger
            .record_external_mcp_evidence("server", "tool", "call", &result)
            .await,
        ExternalEvidenceCapture::Stored
    );
    let artifact_id = ledger
        .document
        .lock()
        .await
        .as_ref()
        .and_then(|document| document.external_evidence.last())
        .and_then(|receipt| receipt.payload_artifact_id.clone())
        .expect("artifact id");
    let artifact_path = home
        .join("tool-output")
        .join(&thread_id)
        .join(format!("{artifact_id}.log"));
    tokio::fs::remove_file(&artifact_path)
        .await
        .expect("remove external payload");
    drop(ledger);

    let reloaded = TaskEvidenceLedger::load_or_new(
        home,
        ThreadId::from_string(&thread_id).expect("thread id"),
        &repo,
    )
    .await;
    assert!(
        reloaded
            .document
            .lock()
            .await
            .as_ref()
            .expect("document")
            .external_evidence
            .is_empty()
    );
    let persisted: TaskEvidenceDocument = serde_json::from_slice(
        &tokio::fs::read(evidence_path)
            .await
            .expect("repaired evidence"),
    )
    .expect("valid repaired evidence");
    assert!(persisted.external_evidence.is_empty());
    assert!(!artifact_path.with_extension("evidence-protected").exists());
}

#[tokio::test]
async fn trimming_and_persistence_failure_cleanup_external_evidence_artifacts() {
    let first = evidence_result(
        "test-provider",
        &"first-artifact-".repeat(2_000),
        serde_json::json!({"value": 1}),
    );
    let second = evidence_result(
        "test-provider",
        &"second-artifact-".repeat(2_000),
        serde_json::json!({"value": 2}),
    );
    let (temp, _repo, mut ledger) = ledger_fixture().await;
    for (call_id, result) in [("first", &first), ("second", &second)] {
        assert_eq!(
            ledger
                .record_external_mcp_evidence_with_limit("server", "tool", call_id, result, 1,)
                .await,
            ExternalEvidenceCapture::Stored
        );
    }
    let (retained_id, thread_directory) = {
        let guard = ledger.document.lock().await;
        let document = guard.as_ref().expect("document");
        assert_eq!(document.external_evidence.len(), 1);
        (
            document.external_evidence[0]
                .payload_artifact_id
                .clone()
                .expect("retained artifact"),
            ledger
                .codex_home
                .as_ref()
                .expect("codex home")
                .join("tool-output")
                .join(ledger.thread_id.as_ref().expect("thread")),
        )
    };
    let mut logs = Vec::new();
    let mut markers = Vec::new();
    let mut entries = tokio::fs::read_dir(&thread_directory)
        .await
        .expect("thread artifacts");
    while let Some(entry) = entries.next_entry().await.expect("artifact entry") {
        if entry.path().extension().and_then(|value| value.to_str()) == Some("log") {
            logs.push(entry.path());
        } else if entry.path().extension().and_then(|value| value.to_str())
            == Some("evidence-protected")
        {
            markers.push(entry.path());
        }
    }
    assert_eq!(
        logs,
        vec![thread_directory.join(format!("{retained_id}.log"))]
    );
    assert_eq!(
        markers,
        vec![thread_directory.join(format!("{retained_id}.evidence-protected"))]
    );

    let blocked_parent = temp.path().join("blocked-parent");
    tokio::fs::write(&blocked_parent, b"not a directory")
        .await
        .expect("blocked parent");
    ledger.evidence_path = Some(blocked_parent.join("evidence.json"));
    assert!(matches!(
        ledger
            .record_external_mcp_evidence_with_limit("server", "tool", "failed", &first, 1)
            .await,
        ExternalEvidenceCapture::Warning(_)
    ));
    {
        let guard = ledger.document.lock().await;
        let document = guard.as_ref().expect("document");
        assert_eq!(document.external_evidence.len(), 1);
        assert_eq!(
            document.external_evidence[0].payload_artifact_id.as_deref(),
            Some(retained_id.as_str())
        );
    }
    let mut entries = tokio::fs::read_dir(&thread_directory)
        .await
        .expect("thread artifacts");
    let mut retained_logs = Vec::new();
    let mut retained_markers = Vec::new();
    while let Some(entry) = entries.next_entry().await.expect("artifact entry") {
        if entry.path().extension().and_then(|value| value.to_str()) == Some("log") {
            retained_logs.push(entry.path());
        } else if entry.path().extension().and_then(|value| value.to_str())
            == Some("evidence-protected")
        {
            retained_markers.push(entry.path());
        }
    }
    assert_eq!(
        retained_logs,
        vec![thread_directory.join(format!("{retained_id}.log"))]
    );
    assert_eq!(
        retained_markers,
        vec![thread_directory.join(format!("{retained_id}.evidence-protected"))]
    );
}

#[tokio::test]
async fn documents_without_external_evidence_still_load() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    let home = temp.path().join("home");
    tokio::fs::create_dir_all(repo.join(".git"))
        .await
        .expect("git");
    tokio::fs::write(repo.join("kd4_features.toml"), "# fixture")
        .await
        .expect("marker");
    let thread_id = ThreadId::new();
    let ledger = TaskEvidenceLedger::load_or_new(home.clone(), thread_id, &repo).await;
    drop(ledger);
    let path = home.join("task-evidence").join(format!("{thread_id}.json"));
    let mut value: Value =
        serde_json::from_slice(&tokio::fs::read(&path).await.expect("evidence")).expect("json");
    value["schema_version"] = serde_json::json!(2);
    value
        .as_object_mut()
        .expect("object")
        .remove("external_evidence");
    value
        .as_object_mut()
        .expect("object")
        .remove("next_external_evidence_receipt_sequence");
    tokio::fs::write(&path, serde_json::to_vec_pretty(&value).expect("json"))
        .await
        .expect("legacy evidence");
    let reloaded = TaskEvidenceLedger::load_or_new(home, thread_id, &repo).await;
    assert!(
        reloaded
            .document
            .lock()
            .await
            .as_ref()
            .expect("document")
            .external_evidence
            .is_empty()
    );
}

#[tokio::test]
async fn duplicate_external_receipt_ids_are_repaired_by_migration() {
    let result = evidence_result("test-provider", "one", serde_json::json!({}));
    let (_temp, _repo, ledger) = ledger_fixture().await;
    for call in ["call-1", "call-2"] {
        assert_eq!(
            ledger
                .record_external_mcp_evidence("server", "tool", call, &result)
                .await,
            ExternalEvidenceCapture::Stored
        );
    }
    let mut guard = ledger.document.lock().await;
    let document = guard.as_mut().expect("document");
    document.external_evidence[1].id = document.external_evidence[0].id.clone();
    migrate_document(document);
    assert_ne!(
        document.external_evidence[0].id,
        document.external_evidence[1].id
    );
    assert!(
        document.next_external_evidence_receipt_sequence
            > next_sequence_after_ids(
                document
                    .external_evidence
                    .iter()
                    .map(|receipt| receipt.id.as_str())
            )
            .saturating_sub(1)
    );
}

#[tokio::test]
async fn non_kd4_git_repository_uses_evidence_only_mode() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    let home = temp.path().join("home");
    tokio::fs::create_dir_all(&repo).await.expect("repo");
    initialize_git_repo(&repo).await;
    let thread_id = ThreadId::new();
    let ledger = TaskEvidenceLedger::load_or_new(home.clone(), thread_id, &repo).await;
    assert_eq!(ledger.mode(), TaskEvidenceMode::EvidenceOnly);
    assert!(
        home.join("task-evidence")
            .join(format!("{thread_id}.json"))
            .is_file()
    );
}

#[tokio::test]
async fn evidence_only_mode_never_derives_completion_state() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    tokio::fs::create_dir_all(&repo).await.expect("repo");
    initialize_git_repo(&repo).await;
    let ledger =
        TaskEvidenceLedger::load_or_new(temp.path().join("home"), ThreadId::new(), &repo).await;
    let requested = plan_with(vec![plan_item("step", StepStatus::Passed)]);
    assert_eq!(ledger.record_plan_update(&requested).await, requested);
    ledger.record_edit_result("edit", "completed").await;
    assert!(ledger.completion_gate().await.is_none());
    assert!(ledger.finalization_advisory().await.is_none());
    let guard = ledger.document.lock().await;
    let document = guard.as_ref().expect("document");
    assert_eq!(document.host_mutation_revision, 1);
    assert_eq!(document.evidence_epoch, 1);
    assert!(document.plan.is_empty());
    assert!(document.edit_receipts.is_empty());
    assert!(document.command_receipts.is_empty());
}

#[tokio::test]
async fn kd4_repository_retains_completion_mode_behavior() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    assert_eq!(ledger.mode(), TaskEvidenceMode::Kd4Completion);
    ledger
        .record_plan_update(&plan_with(vec![plan_item("step", StepStatus::Completed)]))
        .await;
    assert!(ledger.completion_gate().await.is_some());
}

fn plan_item(id: &str, status: StepStatus) -> PlanItemArg {
    PlanItemArg {
        id: Some(id.to_string()),
        step: format!("Implement {id}"),
        status,
        depends_on: Vec::new(),
        acceptance_criteria: vec!["focused validation passes".to_string()],
        runtime_paths: vec![format!("src/{id}.rs")],
        generated_artifacts: Vec::new(),
        risks: Vec::new(),
        requires_desktop_activation: false,
    }
}

fn plan_with(items: Vec<PlanItemArg>) -> UpdatePlanArgs {
    UpdatePlanArgs {
        explanation: None,
        plan: items,
    }
}

fn command_receipt(id: &str) -> CommandReceipt {
    CommandReceipt {
        id: id.to_string(),
        recorded_at: timestamp(),
        epoch: 0,
        step_id: None,
        command: vec!["true".to_string()],
        cwd: ".".to_string(),
        exit_code: 0,
        timed_out: false,
        duration_ms: 1,
        possible_mutation: false,
    }
}

#[tokio::test]
async fn completion_review_summary_distinguishes_command_receipts_from_validation() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    {
        let mut guard = ledger.document.lock().await;
        let document = guard.as_mut().expect("document");
        document.evidence_epoch = 2;
        document.command_receipts = vec![
            CommandReceipt {
                id: "successful".to_string(),
                recorded_at: timestamp(),
                epoch: 2,
                step_id: None,
                command: vec!["cargo".to_string(), "test".to_string()],
                cwd: ".".to_string(),
                exit_code: 0,
                timed_out: false,
                duration_ms: 1,
                possible_mutation: false,
            },
            CommandReceipt {
                id: "timed-out".to_string(),
                recorded_at: timestamp(),
                epoch: 2,
                step_id: None,
                command: vec!["slow-check".to_string()],
                cwd: ".".to_string(),
                exit_code: 124,
                timed_out: true,
                duration_ms: 1,
                possible_mutation: true,
            },
            CommandReceipt {
                id: "stale".to_string(),
                recorded_at: timestamp(),
                epoch: 1,
                step_id: None,
                command: vec!["secret-from-prior-epoch".to_string()],
                cwd: ".".to_string(),
                exit_code: 0,
                timed_out: false,
                duration_ms: 1,
                possible_mutation: false,
            },
        ];
    }
    let gate = TaskCompletionGate {
        status: TaskCompletionStatus::Passed,
        reasons: Vec::new(),
        evidence_path: None,
    };

    let summary = ledger.completion_review_evidence_summary(&gate).await;

    assert!(summary.contains(
        "Command receipt [current epoch 2, outcome: succeeded, possible mutation: false]: cargo test"
    ));
    assert!(summary.contains(
        "Command receipt [current epoch 2, outcome: timed_out, possible mutation: true]: slow-check"
    ));
    assert!(summary.contains("Prior-epoch command receipts omitted: 1"));
    assert!(!summary.contains("secret-from-prior-epoch"));
    assert!(!summary.contains("Validation:"));
}

#[tokio::test]
async fn multiple_in_progress_steps_are_preserved_and_block_completion() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    let normalized = ledger
        .record_plan_update(&plan_with(vec![
            plan_item("one", StepStatus::InProgress),
            plan_item("two", StepStatus::InProgress),
        ]))
        .await;

    assert_eq!(normalized.plan[0].status, StepStatus::InProgress);
    assert_eq!(normalized.plan[1].status, StepStatus::InProgress);
    let gate = ledger.completion_gate().await.expect("gate");
    assert_eq!(gate.status, TaskCompletionStatus::Blocked);
    assert!(
        gate.reasons
            .iter()
            .any(|reason| reason.contains("multiple in-progress steps"))
    );
}

#[tokio::test]
async fn duplicate_explicit_step_ids_are_renamed_and_block_completion() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    let normalized = ledger
        .record_plan_update(&plan_with(vec![
            plan_item("duplicate", StepStatus::Pending),
            plan_item("duplicate", StepStatus::Pending),
        ]))
        .await;

    assert_ne!(normalized.plan[0].id, normalized.plan[1].id);
    let gate = ledger.completion_gate().await.expect("gate");
    assert_eq!(gate.status, TaskCompletionStatus::Blocked);
    assert!(
        gate.reasons
            .iter()
            .any(|reason| reason.contains("duplicate explicit step ids"))
    );
}

#[tokio::test]
async fn unresolved_dependency_prevents_completion() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    let parent = plan_item("parent", StepStatus::Pending);
    let mut child = plan_item("child", StepStatus::Passed);
    child.depends_on = vec!["parent".to_string()];
    ledger
        .record_plan_update(&plan_with(vec![parent, child]))
        .await;

    let gate = ledger.completion_gate().await.expect("gate");

    assert_eq!(gate.status, TaskCompletionStatus::Partial);
    assert!(
        gate.reasons
            .iter()
            .any(|reason| reason.contains("depends on unfinished step `parent`"))
    );
}

#[tokio::test]
async fn missing_dependency_blocks_completion() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    let mut child = plan_item("child", StepStatus::Passed);
    child.depends_on = vec!["missing".to_string()];
    ledger.record_plan_update(&plan_with(vec![child])).await;

    let gate = ledger.completion_gate().await.expect("gate");

    assert_eq!(gate.status, TaskCompletionStatus::Blocked);
    assert!(
        gate.reasons
            .iter()
            .any(|reason| reason.contains("depends on missing step `missing`"))
    );
}

#[tokio::test]
async fn passed_or_skipped_dependency_allows_completion() {
    for dependency_status in [StepStatus::Passed, StepStatus::Skipped] {
        let (_temp, _repo, ledger) = ledger_fixture().await;
        let parent = plan_item("parent", dependency_status);
        let mut child = plan_item("child", StepStatus::Passed);
        child.depends_on = vec!["parent".to_string()];
        ledger
            .record_plan_update(&plan_with(vec![parent, child]))
            .await;

        assert_eq!(
            ledger.completion_gate().await.expect("gate").status,
            TaskCompletionStatus::Passed
        );
    }
}

#[tokio::test]
async fn cyclic_dependency_blocks_completion_even_when_steps_are_passed() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    let mut first = plan_item("first", StepStatus::Passed);
    first.depends_on = vec!["second".to_string()];
    let mut second = plan_item("second", StepStatus::Passed);
    second.depends_on = vec!["first".to_string()];
    ledger
        .record_plan_update(&plan_with(vec![first, second]))
        .await;

    let gate = ledger.completion_gate().await.expect("gate");

    assert_eq!(gate.status, TaskCompletionStatus::Blocked);
    assert!(
        gate.reasons
            .iter()
            .any(|reason| reason.contains("plan dependency cycle includes: first, second"))
    );
}

#[tokio::test]
async fn dependencies_declared_by_skipped_steps_do_not_form_cycles() {
    for first_status in [StepStatus::Skipped, StepStatus::Passed] {
        let (_temp, _repo, ledger) = ledger_fixture().await;
        let mut first = plan_item("first", first_status);
        first.depends_on = vec!["second".to_string()];
        let mut second = plan_item("second", StepStatus::Skipped);
        second.depends_on = vec!["first".to_string()];
        ledger
            .record_plan_update(&plan_with(vec![first, second]))
            .await;

        assert_eq!(
            ledger.completion_gate().await.expect("gate").status,
            TaskCompletionStatus::Passed
        );
    }
}

#[tokio::test]
async fn failed_edit_does_not_promote_the_active_step() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    tokio::fs::create_dir_all(repo.join("src"))
        .await
        .expect("src");
    tokio::fs::write(repo.join("src/step.rs"), "pub fn value() -> u8 { 1 }")
        .await
        .expect("source");
    ledger
        .record_plan_update(&plan_with(vec![plan_item("step", StepStatus::InProgress)]))
        .await;
    ledger
        .record_edit_intent("failed-edit", &repo, &[PathBuf::from("src/step.rs")])
        .await;
    tokio::fs::write(repo.join("src/step.rs"), "pub fn value() -> u8 { 2 }")
        .await
        .expect("source update");
    ledger.record_edit_result("failed-edit", "failed").await;

    let guard = ledger.document.lock().await;
    let document = guard.as_ref().expect("document");
    assert_eq!(document.plan[0].status, StepStatus::InProgress);
    assert_eq!(document.edit_receipts[0].outcome, "failed");
}

#[tokio::test]
async fn failed_mutating_command_does_not_promote_the_active_step() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    ledger
        .record_plan_update(&plan_with(vec![plan_item("step", StepStatus::InProgress)]))
        .await;
    let cwd = AbsolutePathBuf::from_absolute_path(&repo).expect("repo");
    ledger
        .record_command(
            &["touch".to_string(), "src/step.rs".to_string()],
            &PathUri::from_abs_path(&cwd),
            1,
            false,
            1,
            true,
        )
        .await;

    let guard = ledger.document.lock().await;
    let document = guard.as_ref().expect("document");
    assert_eq!(document.plan[0].status, StepStatus::InProgress);
}

#[tokio::test]
async fn migration_repairs_duplicate_command_receipts_without_reopening_current_steps() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    let mut document = ledger
        .document
        .lock()
        .await
        .as_ref()
        .expect("document")
        .clone();
    document.plan = vec![EvidencePlanStep {
        id: "step".to_string(),
        step: "step".to_string(),
        status: StepStatus::Passed,
        depends_on: Vec::new(),
        acceptance_criteria: Vec::new(),
        runtime_paths: Vec::new(),
        generated_artifacts: Vec::new(),
        risks: Vec::new(),
        requires_desktop_activation: false,
        edit_paths: BTreeSet::from(["src/step.rs".to_string()]),
    }];
    document.schema_version = 3;
    document.command_receipts = vec![command_receipt("command-1"), command_receipt("command-1")];
    migrate_document(&mut document);

    assert_ne!(
        document.command_receipts[0].id,
        document.command_receipts[1].id
    );
    assert_eq!(document.plan[0].status, StepStatus::Passed);
    assert_eq!(document.schema_version, TASK_EVIDENCE_SCHEMA_VERSION);
}

#[tokio::test]
async fn migration_drops_unattributed_legacy_file_hashes() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    let mut document = ledger
        .document
        .lock()
        .await
        .as_ref()
        .expect("document")
        .clone();
    document.plan = vec![EvidencePlanStep {
        id: "step".to_string(),
        step: "step".to_string(),
        status: StepStatus::Implemented,
        depends_on: Vec::new(),
        acceptance_criteria: Vec::new(),
        runtime_paths: Vec::new(),
        generated_artifacts: Vec::new(),
        risks: Vec::new(),
        requires_desktop_activation: false,
        edit_paths: BTreeSet::from(["src/owned.rs".to_string()]),
    }];
    document.latest_file_hashes = BTreeMap::from([
        (
            "src/owned.rs".to_string(),
            FileHashSnapshot {
                path: "src/owned.rs".to_string(),
                sha1: Some("a".repeat(40)),
                exists: true,
                read_error: None,
            },
        ),
        (
            "src/unrelated.rs".to_string(),
            FileHashSnapshot {
                path: "src/unrelated.rs".to_string(),
                sha1: Some("b".repeat(40)),
                exists: true,
                read_error: None,
            },
        ),
    ]);

    migrate_document(&mut document);

    assert_eq!(
        document
            .latest_file_hashes
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        vec!["src/owned.rs".to_string()]
    );
}

#[tokio::test]
async fn explicit_passed_and_completed_are_authoritative_success_states() {
    for requested in [StepStatus::Passed, StepStatus::Completed] {
        let (_temp, _repo, ledger) = ledger_fixture().await;
        let normalized = ledger
            .record_plan_update(&plan_with(vec![plan_item("step", requested)]))
            .await;
        assert_eq!(normalized.plan[0].status, StepStatus::Passed);
        assert_eq!(
            ledger.completion_gate().await.expect("gate").status,
            TaskCompletionStatus::Passed
        );
    }
}

#[tokio::test]
async fn material_plan_update_can_pass_in_the_same_update() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    ledger
        .record_plan_update(&plan_with(vec![plan_item("step", StepStatus::Pending)]))
        .await;
    let mut changed = plan_item("step", StepStatus::Passed);
    changed.step = "Implement the materially revised step".to_string();

    let normalized = ledger.record_plan_update(&plan_with(vec![changed])).await;

    assert_eq!(normalized.plan[0].status, StepStatus::Passed);
    assert_eq!(
        ledger.completion_gate().await.expect("gate").status,
        TaskCompletionStatus::Passed
    );
}

#[tokio::test]
async fn later_file_and_command_mutations_reopen_passed_steps() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    tokio::fs::create_dir_all(repo.join("src"))
        .await
        .expect("src");
    tokio::fs::write(repo.join("src/step.rs"), "one")
        .await
        .expect("source");
    ledger
        .record_plan_update(&plan_with(vec![plan_item("step", StepStatus::Passed)]))
        .await;
    ledger
        .record_edit_intent("edit", &repo, &[PathBuf::from("src/step.rs")])
        .await;
    tokio::fs::write(repo.join("src/step.rs"), "two")
        .await
        .expect("source update");
    ledger.record_edit_result("edit", "completed").await;
    {
        let guard = ledger.document.lock().await;
        assert_eq!(
            guard.as_ref().expect("document").plan[0].status,
            StepStatus::Implemented
        );
    }

    ledger
        .record_plan_update(&plan_with(vec![plan_item("step", StepStatus::Passed)]))
        .await;
    let cwd = AbsolutePathBuf::from_absolute_path(&repo).expect("repo");
    ledger
        .record_command(
            &["generator".to_string()],
            &PathUri::from_abs_path(&cwd),
            0,
            false,
            1,
            true,
        )
        .await;
    let guard = ledger.document.lock().await;
    assert_eq!(
        guard.as_ref().expect("document").plan[0].status,
        StepStatus::Implemented
    );
}

#[tokio::test]
async fn generated_artifact_gate_checks_current_file_state() {
    let (_missing_temp, _missing_repo, missing_ledger) = ledger_fixture().await;
    let mut missing = plan_item("step", StepStatus::Passed);
    missing.generated_artifacts = vec!["generated/output.json".to_string()];
    missing_ledger
        .record_plan_update(&plan_with(vec![missing]))
        .await;
    let missing_gate = missing_ledger
        .completion_gate()
        .await
        .expect("missing gate");
    assert_eq!(missing_gate.status, TaskCompletionStatus::Blocked);
    assert!(
        missing_gate
            .reasons
            .iter()
            .any(|reason| reason.contains("missing, unreadable, or unhashable"))
    );

    let (_present_temp, repo, present_ledger) = ledger_fixture().await;
    tokio::fs::create_dir_all(repo.join("generated"))
        .await
        .expect("generated");
    tokio::fs::write(repo.join("generated/output.json"), "{}")
        .await
        .expect("artifact");
    let mut present = plan_item("step", StepStatus::Passed);
    present.generated_artifacts = vec!["generated/output.json".to_string()];
    present_ledger
        .record_plan_update(&plan_with(vec![present]))
        .await;
    assert_eq!(
        present_ledger
            .completion_gate()
            .await
            .expect("present gate")
            .status,
        TaskCompletionStatus::Passed
    );

    tokio::fs::remove_file(repo.join("generated/output.json"))
        .await
        .expect("delete artifact");
    let deleted_gate = present_ledger
        .completion_gate()
        .await
        .expect("deleted gate");
    assert_eq!(deleted_gate.status, TaskCompletionStatus::Blocked);
    let guard = present_ledger.document.lock().await;
    assert_eq!(
        guard.as_ref().expect("document").plan[0].status,
        StepStatus::Implemented
    );
}

#[tokio::test]
async fn skipped_step_artifact_and_desktop_requirements_do_not_block_completion() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    let mut skipped = plan_item("skipped", StepStatus::Skipped);
    skipped.generated_artifacts = vec!["generated/missing.json".to_string()];
    skipped.requires_desktop_activation = true;
    ledger.record_plan_update(&plan_with(vec![skipped])).await;

    let gate = ledger.completion_gate().await.expect("completion gate");

    assert_eq!(gate.status, TaskCompletionStatus::Passed);
    assert!(gate.reasons.is_empty());
    let guard = ledger.document.lock().await;
    let document = guard.as_ref().expect("document");
    assert!(document.generated_artifact_requirements.is_empty());
    assert!(document.latest_generated_artifact_hashes.is_empty());
    assert!(document.desktop_activation_receipt.is_none());
}

#[tokio::test]
async fn current_schema_reload_removes_stale_skipped_step_requirements() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    let codex_home = temp.path().join("home");
    tokio::fs::create_dir_all(repo.join(".git"))
        .await
        .expect("git directory");
    tokio::fs::write(repo.join("kd4_features.toml"), "# fixture")
        .await
        .expect("manifest");
    let thread_id = ThreadId::new();
    let evidence_path = codex_home
        .join("task-evidence")
        .join(format!("{thread_id}.json"));
    let ledger = TaskEvidenceLedger::load_or_new(codex_home.clone(), thread_id, &repo).await;
    let mut skipped = plan_item("skipped", StepStatus::Skipped);
    skipped.generated_artifacts = vec!["generated/missing.json".to_string()];
    skipped.requires_desktop_activation = true;
    ledger.record_plan_update(&plan_with(vec![skipped])).await;
    drop(ledger);

    let mut persisted: Value = serde_json::from_slice(
        &tokio::fs::read(&evidence_path)
            .await
            .expect("persisted evidence"),
    )
    .expect("valid evidence");
    assert_eq!(persisted["schema_version"], TASK_EVIDENCE_SCHEMA_VERSION);
    persisted["generated_artifact_requirements"] = serde_json::json!([{
        "id": "plan:skipped:artifact:0",
        "step_id": "skipped",
        "path": "generated/missing.json"
    }]);
    tokio::fs::write(
        &evidence_path,
        serde_json::to_vec_pretty(&persisted).expect("serialize stale evidence"),
    )
    .await
    .expect("write stale evidence");

    let reloaded = TaskEvidenceLedger::load_or_new(codex_home, thread_id, &repo).await;
    {
        let guard = reloaded.document.lock().await;
        let document = guard.as_ref().expect("reloaded document");
        assert_eq!(document.schema_version, TASK_EVIDENCE_SCHEMA_VERSION);
        assert_eq!(document.plan[0].status, StepStatus::Skipped);
        assert!(document.generated_artifact_requirements.is_empty());
    }
    let gate = reloaded.completion_gate().await.expect("completion gate");
    assert_eq!(gate.status, TaskCompletionStatus::Passed);
    assert!(gate.reasons.is_empty());
}

#[tokio::test]
async fn generated_artifact_gate_rejects_repository_escape_paths() {
    let (temp, repo, ledger) = ledger_fixture().await;
    let outside = temp.path().join("outside");
    tokio::fs::create_dir_all(&outside)
        .await
        .expect("outside directory");
    tokio::fs::write(outside.join("output.json"), "{}")
        .await
        .expect("outside artifact");
    create_directory_alias(&outside, &repo.join("artifact-link"));

    for artifact in [
        outside.join("output.json").to_string_lossy().into_owned(),
        "../outside/output.json".to_string(),
        "artifact-link/output.json".to_string(),
    ] {
        let mut step = plan_item("step", StepStatus::Passed);
        step.generated_artifacts = vec![artifact];
        ledger.record_plan_update(&plan_with(vec![step])).await;
        let gate = ledger.completion_gate().await.expect("escape gate");
        assert_eq!(gate.status, TaskCompletionStatus::Blocked);
        assert!(
            gate.reasons
                .iter()
                .any(|reason| reason.contains("missing, unreadable, or unhashable"))
        );
    }

    tokio::fs::create_dir_all(repo.join("generated"))
        .await
        .expect("generated directory");
    tokio::fs::write(repo.join("generated/output.json"), "{}")
        .await
        .expect("in-repository artifact");
    let mut valid = plan_item("step", StepStatus::Passed);
    valid.generated_artifacts = vec!["generated/output.json".to_string()];
    ledger.record_plan_update(&plan_with(vec![valid])).await;
    assert_eq!(
        ledger
            .completion_gate()
            .await
            .expect("valid artifact gate")
            .status,
        TaskCompletionStatus::Passed
    );
}

#[tokio::test]
async fn genuine_v1_and_v2_documents_load_and_migrate_without_later_fields() {
    for (schema_version, step_status, expected_revision) in
        [(1, "passed", 1_u64), (2, "completed", 8_u64)]
    {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        let codex_home = temp.path().join("home");
        tokio::fs::create_dir_all(repo.join(".git"))
            .await
            .expect("git directory");
        tokio::fs::write(repo.join("kd4_features.toml"), "# fixture")
            .await
            .expect("manifest");
        let thread_id = ThreadId::new();
        let evidence_path = codex_home
            .join("task-evidence")
            .join(format!("{thread_id}.json"));
        tokio::fs::create_dir_all(evidence_path.parent().expect("evidence parent"))
            .await
            .expect("evidence directory");
        let legacy = legacy_task_evidence_fixture(
            schema_version,
            &thread_id.to_string(),
            &repo,
            step_status,
        );
        tokio::fs::write(
            &evidence_path,
            serde_json::to_vec_pretty(&legacy).expect("serialize legacy fixture"),
        )
        .await
        .expect("write legacy fixture");

        let ledger = TaskEvidenceLedger::load_or_new(codex_home, thread_id, &repo).await;
        {
            let guard = ledger.document.lock().await;
            let migrated = guard.as_ref().expect("migrated document");
            assert_eq!(migrated.schema_version, TASK_EVIDENCE_SCHEMA_VERSION);
            assert_eq!(migrated.revision, expected_revision);
            assert_eq!(migrated.plan[0].status, StepStatus::Implemented);
            assert!(migrated.completion.is_none());
            assert!(migrated.external_evidence.is_empty());
            assert_eq!(migrated.next_external_evidence_receipt_sequence, 1);
            assert_eq!(migrated.host_mutation_revision, 0);
        }

        let persisted: TaskEvidenceDocument = serde_json::from_slice(
            &tokio::fs::read(&evidence_path)
                .await
                .expect("persisted migrated evidence"),
        )
        .expect("valid migrated evidence");
        assert_eq!(persisted.schema_version, TASK_EVIDENCE_SCHEMA_VERSION);
    }
}

#[tokio::test]
async fn v3_obsolete_validation_state_is_discarded_during_shape_migration() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    ledger
        .record_plan_update(&plan_with(vec![plan_item("step", StepStatus::Passed)]))
        .await;
    let document = ledger
        .document
        .lock()
        .await
        .as_ref()
        .expect("document")
        .clone();
    let mut legacy = serde_json::to_value(document).expect("serialize");
    legacy["schema_version"] = serde_json::json!(3);
    legacy["validation_epoch"] = serde_json::json!(0);
    legacy["next_validation_receipt_sequence"] = serde_json::json!(2);
    legacy["validation_receipts"] = serde_json::json!([{
        "id": "validation-1",
        "recorded_at": timestamp(),
        "epoch": 0,
        "step_id": "step",
        "mode": "final",
        "verdict": "VERIFIED",
        "tool_success": true,
        "proof_bearing": true,
        "active_files": [],
        "stale_reasons": [],
        "payload": null
    }]);
    legacy["plan"][0]["validation_receipt_ids"] = serde_json::json!(["validation-1"]);
    legacy["risks"] = serde_json::json!([
        {
            "id": "retired-validation-risk",
            "description": "retired validation risk",
            "source": "retired_validation",
            "blocking": true,
            "resolved": false,
            "epoch": 0
        },
        {
            "id": "artifact-freshness-risk",
            "description": "legacy freshness risk",
            "source": "retired_artifact_snapshot",
            "blocking": true,
            "resolved": false,
            "epoch": 0
        }
    ]);
    legacy["generated_artifact_requirements"] = serde_json::json!([{
        "id": "obsolete-validation-requirement",
        "step_id": "step",
        "path": null,
        "validation_command": ["cargo", "test"],
        "source": "retired_validation",
        "validation_receipt_ids": ["validation-1"]
    }]);

    let retired_shape = uses_retired_v3_completion_shape(3, &legacy);
    let mut migrated: TaskEvidenceDocument =
        serde_json::from_value(legacy).expect("legacy document");
    migrate_document_with_completion_model(&mut migrated, retired_shape);

    assert_eq!(migrated.schema_version, TASK_EVIDENCE_SCHEMA_VERSION);
    assert_eq!(migrated.plan[0].status, StepStatus::Implemented);
    assert!(migrated.completion.is_none());
    assert!(migrated.risks.is_empty());
    assert!(migrated.generated_artifact_requirements.is_empty());
    let persisted = serde_json::to_value(migrated).expect("serialize migrated");
    for obsolete in [
        "validation_epoch",
        "next_validation_receipt_sequence",
        "validation_receipts",
    ] {
        assert!(persisted.get(obsolete).is_none(), "{obsolete}");
    }
    assert!(persisted["plan"][0].get("validation_receipt_ids").is_none());
}

#[tokio::test]
async fn v3_to_v4_discards_obsolete_repair_counter_without_reopening_passed_work() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    ledger
        .record_plan_update(&plan_with(vec![plan_item("step", StepStatus::Passed)]))
        .await;
    let gate = ledger.completion_gate().await.expect("completion gate");
    assert_eq!(gate.status, TaskCompletionStatus::Passed);

    let codex_home = ledger.codex_home.as_ref().expect("codex home").clone();
    let evidence_path = ledger
        .evidence_path
        .as_ref()
        .expect("evidence path")
        .clone();
    let thread_id = ledger.thread_id.as_deref().expect("thread id").to_string();
    let document = ledger
        .document
        .lock()
        .await
        .as_ref()
        .expect("document")
        .clone();
    let mut v3 = serde_json::to_value(document).expect("serialize v3 fixture");
    v3["schema_version"] = serde_json::json!(3);
    v3["repair_turns_used"] = serde_json::json!(7);
    tokio::fs::write(
        &evidence_path,
        serde_json::to_vec_pretty(&v3).expect("serialize v3 evidence"),
    )
    .await
    .expect("write v3 evidence");
    drop(ledger);

    let reloaded = TaskEvidenceLedger::load_or_new(
        codex_home,
        ThreadId::from_string(&thread_id).expect("thread id"),
        &repo,
    )
    .await;
    {
        let guard = reloaded.document.lock().await;
        let migrated = guard.as_ref().expect("migrated document");
        assert_eq!(migrated.schema_version, TASK_EVIDENCE_SCHEMA_VERSION);
        assert_eq!(migrated.plan[0].status, StepStatus::Passed);
        assert_eq!(
            migrated.completion.as_ref().map(|gate| gate.status),
            Some(TaskCompletionStatus::Passed)
        );
        assert!(migrated.completion_review_receipts.is_empty());
    }
    let persisted: Value = serde_json::from_slice(
        &tokio::fs::read(&evidence_path)
            .await
            .expect("persisted v4 evidence"),
    )
    .expect("valid persisted v4 evidence");
    assert_eq!(persisted["schema_version"], serde_json::json!(4));
    assert!(persisted.get("repair_turns_used").is_none());
}

#[tokio::test]
async fn completion_review_receipts_are_bounded_and_never_change_completion_control_flow() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    ledger
        .record_plan_update(&plan_with(vec![plan_item("step", StepStatus::Passed)]))
        .await;
    assert_eq!(
        ledger.completion_gate().await.expect("initial gate").status,
        TaskCompletionStatus::Passed
    );
    let (initial_epoch, initial_mutation_revision) = {
        let guard = ledger.document.lock().await;
        let document = guard.as_ref().expect("document");
        (document.evidence_epoch, document.host_mutation_revision)
    };

    for index in 0..=MAX_COMPLETION_REVIEW_RECEIPTS {
        assert!(
            ledger
                .record_completion_review_audit(
                    &format!("turn-{index}"),
                    "clean",
                    None,
                    vec![format!("finding-{index}")],
                    false,
                )
                .await
        );
    }

    {
        let guard = ledger.document.lock().await;
        let document = guard.as_ref().expect("document");
        assert_eq!(
            document.completion_review_receipts.len(),
            MAX_COMPLETION_REVIEW_RECEIPTS
        );
        assert_eq!(document.completion_review_receipts[0].turn_id, "turn-1");
        assert_eq!(document.evidence_epoch, initial_epoch);
        assert_eq!(document.host_mutation_revision, initial_mutation_revision);
        assert_eq!(
            document.completion.as_ref().map(|gate| gate.status),
            Some(TaskCompletionStatus::Passed)
        );
    }
    assert_eq!(
        ledger.completion_gate().await.expect("final gate").status,
        TaskCompletionStatus::Passed
    );
}

#[tokio::test]
async fn newer_schema_payload_disables_ledger_without_modifying_the_file() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    ledger
        .record_plan_update(&plan_with(vec![plan_item("step", StepStatus::Passed)]))
        .await;
    let home = ledger.codex_home.as_ref().expect("codex home").clone();
    let evidence_path = ledger
        .evidence_path
        .as_ref()
        .expect("evidence path")
        .clone();
    let thread_id = ledger.thread_id.as_deref().expect("thread id").to_string();
    let document = ledger
        .document
        .lock()
        .await
        .as_ref()
        .expect("document")
        .clone();
    let mut legacy = serde_json::to_value(document).expect("serialize");
    legacy["schema_version"] = serde_json::json!(5);
    legacy["lifecycle"] = serde_json::json!({
        "phase": "ready",
        "outcome": "passed",
        "mutation_revision": 1,
        "accepted_evidence_revision": 1
    });
    let legacy_bytes = serde_json::to_vec_pretty(&legacy).expect("serialize v5 evidence");
    tokio::fs::write(&evidence_path, &legacy_bytes)
        .await
        .expect("write v5 evidence");
    drop(ledger);

    assert!(matches!(
        load_existing_document(&evidence_path, &thread_id, &repo).await,
        ExistingDocument::NewerSchema { schema_version: 5 }
    ));

    let reloaded = TaskEvidenceLedger::load_or_new(
        home,
        ThreadId::from_string(&thread_id).expect("thread id"),
        &repo,
    )
    .await;
    assert_eq!(reloaded.mode(), TaskEvidenceMode::Disabled);
    assert_eq!(
        tokio::fs::read(&evidence_path)
            .await
            .expect("untouched v5 evidence"),
        legacy_bytes
    );

    let mut entries = tokio::fs::read_dir(evidence_path.parent().expect("evidence parent"))
        .await
        .expect("evidence directory");
    while let Some(entry) = entries.next_entry().await.expect("evidence entry") {
        let name = entry.file_name();
        assert!(
            !name.to_string_lossy().ends_with(".preserved"),
            "newer evidence must remain at its original path"
        );
    }
}

#[tokio::test]
async fn terminal_acknowledgement_resolves_only_recoverable_nonblocking_risks() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    {
        let mut guard = ledger.document.lock().await;
        let document = guard.as_mut().expect("document");
        for (source, blocking) in [
            ("edit", false),
            ("command", false),
            ("freshness", false),
            ("task_evidence_storage", true),
            ("plan_structure", true),
        ] {
            document.risks.push(EvidenceRisk {
                id: format!("{source}-risk"),
                description: format!("{source} risk"),
                source: source.to_string(),
                blocking,
                resolved: false,
                epoch: document.evidence_epoch,
            });
        }
    }

    ledger
        .record_plan_update(&plan_with(vec![plan_item("step", StepStatus::Passed)]))
        .await;

    let guard = ledger.document.lock().await;
    let document = guard.as_ref().expect("document");
    for source in ["edit", "command", "freshness"] {
        assert!(
            document
                .risks
                .iter()
                .find(|risk| risk.source == source)
                .is_some_and(|risk| risk.resolved),
            "{source}"
        );
    }
    for source in ["task_evidence_storage", "plan_structure"] {
        assert!(
            document
                .risks
                .iter()
                .find(|risk| risk.source == source)
                .is_some_and(|risk| !risk.resolved),
            "{source}"
        );
    }
}

#[tokio::test]
async fn storage_failure_is_tracked_and_fail_closed() {
    let (_temp, _repo, mut ledger) = ledger_fixture().await;
    ledger.evidence_path = None;
    {
        let mut guard = ledger.document.lock().await;
        let document = guard.as_mut().expect("document");
        let epoch = document.evidence_epoch;
        upsert_risk(
            document,
            task_evidence_storage_risk("quarantine failed", epoch),
        );
    }

    let gate = ledger.completion_gate().await.expect("fail-closed gate");
    assert_eq!(gate.status, TaskCompletionStatus::Blocked);
    assert!(
        gate.reasons
            .iter()
            .any(|reason| reason.contains("storage is unavailable"))
    );
}

#[tokio::test]
async fn snapshot_file_hashes_across_multiple_bounded_chunks() {
    let (_temp, repo, _ledger) = ledger_fixture().await;
    let bytes = (0..FILE_HASH_CHUNK_SIZE * 2 + 17)
        .map(|index| (index % 251) as u8)
        .collect::<Vec<_>>();
    tokio::fs::write(repo.join("large.bin"), &bytes)
        .await
        .expect("large fixture");

    let snapshot = snapshot_file(&repo, "large.bin").await;

    assert_eq!(snapshot.sha1, Some(sha1_hex(&bytes)));
    assert!(snapshot.exists);
    assert_eq!(snapshot.read_error, None);
}

#[tokio::test]
async fn older_persistence_snapshot_is_reported_as_superseded() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    let document = ledger
        .document
        .lock()
        .await
        .as_ref()
        .expect("document")
        .clone();
    let mut older = document.clone();
    older.revision = document.revision.saturating_add(1);
    let mut newer = document;
    newer.revision = older.revision.saturating_add(1);

    assert_eq!(
        ledger.persist_document(&newer).await,
        PersistOutcome::Persisted
    );
    assert_eq!(
        ledger.persist_document(&older).await,
        PersistOutcome::Superseded
    );
}

fn install_persistence_test_control(
    ledger: &TaskEvidenceLedger,
    fail_writes: bool,
) -> (Arc<std::sync::Barrier>, Arc<std::sync::Barrier>) {
    let started = Arc::new(std::sync::Barrier::new(2));
    let release = Arc::new(std::sync::Barrier::new(2));
    *ledger
        .persistence_test_control
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(PersistenceTestControl {
        before_next_write: Arc::new(std::sync::Mutex::new(Some((
            Arc::clone(&started),
            Arc::clone(&release),
        )))),
        fail_writes: Arc::new(std::sync::atomic::AtomicBool::new(fail_writes)),
    });
    (started, release)
}

fn set_persistence_test_failure(ledger: &TaskEvidenceLedger, fail_writes: bool) {
    let guard = ledger
        .persistence_test_control
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard
        .as_ref()
        .expect("persistence test control")
        .fail_writes
        .store(fail_writes, std::sync::atomic::Ordering::Release);
}

async fn wait_persistence_barrier(barrier: Arc<std::sync::Barrier>) {
    tokio::task::spawn_blocking(move || barrier.wait())
        .await
        .expect("persistence barrier");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completion_persistence_failure_blocks_then_recovers_when_storage_returns() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    ledger
        .record_plan_update(&plan_with(vec![plan_item("step", StepStatus::Passed)]))
        .await;
    let ledger = Arc::new(ledger);
    let (started, release) = install_persistence_test_control(&ledger, true);
    let task_ledger = Arc::clone(&ledger);
    let completion = tokio::spawn(async move { task_ledger.completion_gate().await });

    wait_persistence_barrier(started).await;
    wait_persistence_barrier(release).await;
    let gate = completion
        .await
        .expect("completion task")
        .expect("completion gate");

    assert_eq!(gate.status, TaskCompletionStatus::Blocked);
    assert!(
        gate.reasons
            .iter()
            .any(|reason| reason.contains("persistence failed"))
    );
    {
        let guard = ledger.document.lock().await;
        let document = guard.as_ref().expect("document");
        let storage_risk = document
            .risks
            .iter()
            .find(|risk| risk.source == "task_evidence_storage")
            .expect("storage risk");
        assert!(storage_risk.blocking);
        assert!(!storage_risk.resolved);
        assert_eq!(
            document
                .completion
                .as_ref()
                .expect("cached completion")
                .status,
            TaskCompletionStatus::Blocked
        );
    }

    set_persistence_test_failure(&ledger, false);
    let recovered = ledger.completion_gate().await.expect("recovered gate");
    assert_eq!(recovered.status, TaskCompletionStatus::Passed);
    {
        let guard = ledger.document.lock().await;
        let document = guard.as_ref().expect("document");
        assert!(
            document
                .risks
                .iter()
                .find(|risk| risk.source == "task_evidence_storage")
                .is_some_and(|risk| risk.resolved)
        );
    }
    let persisted: TaskEvidenceDocument = serde_json::from_slice(
        &tokio::fs::read(ledger.evidence_path.as_ref().expect("evidence path"))
            .await
            .expect("persisted evidence"),
    )
    .expect("valid persisted evidence");
    assert_eq!(
        persisted.completion.expect("persisted completion").status,
        TaskCompletionStatus::Passed
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_external_persistence_success_keeps_receipt_and_artifact() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    let ledger = Arc::new(ledger);
    let (started, release) = install_persistence_test_control(&ledger, false);
    let result = evidence_result(
        "test-provider",
        &"large evidence ".repeat(2_000),
        serde_json::json!({"provider": "snapshot"}),
    );
    let task_ledger = Arc::clone(&ledger);
    let capture = tokio::spawn(async move {
        task_ledger
            .record_external_mcp_evidence("server", "tool", "call", &result)
            .await
    });

    wait_persistence_barrier(started).await;
    capture.abort();
    wait_persistence_barrier(release).await;
    let completion_permit = Arc::clone(&ledger.external_evidence_gate)
        .acquire_owned()
        .await
        .expect("coordinator completion");
    drop(completion_permit);

    let artifact_path = {
        let guard = ledger.document.lock().await;
        let document = guard.as_ref().expect("document");
        assert_eq!(document.external_evidence.len(), 1);
        let artifact_id = document.external_evidence[0]
            .payload_artifact_id
            .as_ref()
            .expect("artifact id");
        ledger
            .codex_home
            .as_ref()
            .expect("codex home")
            .join("tool-output")
            .join(ledger.thread_id.as_ref().expect("thread id"))
            .join(format!("{artifact_id}.log"))
    };
    assert!(artifact_path.exists());
    assert!(artifact_path.with_extension("evidence-protected").exists());

    let persisted: TaskEvidenceDocument = serde_json::from_slice(
        &tokio::fs::read(ledger.evidence_path.as_ref().expect("evidence path"))
            .await
            .expect("persisted evidence"),
    )
    .expect("valid evidence");
    assert_eq!(persisted.external_evidence.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_external_persistence_failure_rolls_back_receipt_and_artifact() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    let ledger = Arc::new(ledger);
    let (started, release) = install_persistence_test_control(&ledger, true);
    let result = evidence_result(
        "test-provider",
        &"large evidence ".repeat(2_000),
        serde_json::json!({"provider": "snapshot"}),
    );
    let task_ledger = Arc::clone(&ledger);
    let capture = tokio::spawn(async move {
        task_ledger
            .record_external_mcp_evidence("server", "tool", "call", &result)
            .await
    });

    wait_persistence_barrier(started).await;
    capture.abort();
    wait_persistence_barrier(release).await;
    let completion_permit = Arc::clone(&ledger.external_evidence_gate)
        .acquire_owned()
        .await
        .expect("coordinator completion");
    drop(completion_permit);

    {
        let guard = ledger.document.lock().await;
        assert!(
            guard
                .as_ref()
                .expect("document")
                .external_evidence
                .is_empty()
        );
    }
    let persisted: TaskEvidenceDocument = serde_json::from_slice(
        &tokio::fs::read(ledger.evidence_path.as_ref().expect("evidence path"))
            .await
            .expect("persisted evidence"),
    )
    .expect("valid evidence");
    assert!(persisted.external_evidence.is_empty());
    let tool_output = ledger
        .codex_home
        .as_ref()
        .expect("codex home")
        .join("tool-output");
    if let Ok(mut threads) = tokio::fs::read_dir(tool_output).await {
        while let Some(thread) = threads.next_entry().await.expect("thread entry") {
            let mut entries = tokio::fs::read_dir(thread.path())
                .await
                .expect("artifact directory");
            while let Some(entry) = entries.next_entry().await.expect("artifact entry") {
                let path = entry.path();
                let extension = path.extension().and_then(|value| value.to_str());
                assert_ne!(extension, Some("log"));
                assert_ne!(extension, Some("evidence-protected"));
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_older_generic_persist_cannot_overwrite_newer_snapshot() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    let ledger = Arc::new(ledger);
    let document = ledger
        .document
        .lock()
        .await
        .as_ref()
        .expect("document")
        .clone();
    let mut older = document.clone();
    older.revision = document.revision.saturating_add(1);
    older.active_step_id = Some("older".to_string());
    let mut newer = document;
    newer.revision = older.revision.saturating_add(1);
    newer.active_step_id = Some("newer".to_string());
    let (started, release) = install_persistence_test_control(&ledger, false);

    let older_ledger = Arc::clone(&ledger);
    let older_task = tokio::spawn(async move { older_ledger.persist_document(&older).await });
    wait_persistence_barrier(started).await;
    older_task.abort();
    let newer_ledger = Arc::clone(&ledger);
    let newer_task = tokio::spawn(async move { newer_ledger.persist_document(&newer).await });
    wait_persistence_barrier(release).await;
    assert_eq!(
        newer_task.await.expect("newer persistence"),
        PersistOutcome::Persisted
    );

    let persisted: TaskEvidenceDocument = serde_json::from_slice(
        &tokio::fs::read(ledger.evidence_path.as_ref().expect("evidence path"))
            .await
            .expect("persisted evidence"),
    )
    .expect("valid evidence");
    assert_eq!(persisted.active_step_id.as_deref(), Some("newer"));
}

#[test]
fn unreadable_risk_ids_are_stable() {
    assert_eq!(
        unreadable_file_risk_id("src\\step.rs"),
        unreadable_file_risk_id("src/step.rs")
    );
    assert!(edit_outcome_succeeded("completed"));
    assert!(!edit_outcome_succeeded(" completed "));
    assert!(!edit_outcome_succeeded("failed"));
}
