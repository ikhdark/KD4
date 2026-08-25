use super::*;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;

// Frozen schema-admission boundary from `task_evidence.rs` at
// 7930b330a54c86adbdaea37ecbda77977df2a74e. This deliberately does not call
// the current loader: changing v5 cannot change both sides of the downgrade
// refusal proof. The original v4 loader performed no write before this check.
mod frozen_v4 {
    use serde_json::Value;
    use std::io;
    use std::path::Path;

    const TASK_EVIDENCE_SCHEMA_VERSION: u32 = 4;

    #[derive(Debug, PartialEq, Eq)]
    pub(super) enum Admission {
        Missing,
        Accepted,
        NewerSchema { schema_version: u64 },
        Rejected { kind: &'static str, reason: String },
    }

    #[derive(Debug, PartialEq, Eq)]
    pub(super) enum LoadOutcome {
        Active,
        Disabled,
    }

    pub(super) async fn admit(path: &Path) -> Admission {
        let bytes = match tokio::fs::read(path).await {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Admission::Missing,
            Err(err) => {
                return Admission::Rejected {
                    kind: "unreadable",
                    reason: format!("could not read evidence: {err}"),
                };
            }
        };
        let value = match serde_json::from_slice::<Value>(&bytes) {
            Ok(value) => value,
            Err(err) => {
                return Admission::Rejected {
                    kind: "corrupt",
                    reason: format!("invalid JSON: {err}"),
                };
            }
        };
        let schema_version = match value.get("schema_version").and_then(Value::as_u64) {
            Some(schema_version) => schema_version,
            None => {
                return Admission::Rejected {
                    kind: "incompatible",
                    reason: "missing numeric schema_version".to_string(),
                };
            }
        };
        if schema_version > u64::from(TASK_EVIDENCE_SCHEMA_VERSION) {
            return Admission::NewerSchema { schema_version };
        }
        if schema_version == 0 {
            return Admission::Rejected {
                kind: "incompatible",
                reason: format!("unsupported schema version {schema_version}"),
            };
        }
        Admission::Accepted
    }

    pub(super) async fn load(path: &Path) -> LoadOutcome {
        match admit(path).await {
            Admission::NewerSchema { .. } => LoadOutcome::Disabled,
            Admission::Missing | Admission::Accepted | Admission::Rejected { .. } => {
                LoadOutcome::Active
            }
        }
    }
}

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

async fn ledger_fixture_with_source_owners(
    source_owners: &str,
) -> (tempfile::TempDir, PathBuf, TaskEvidenceLedger) {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    let codex_home = temp.path().join("home");
    tokio::fs::create_dir_all(repo.join(".git"))
        .await
        .expect("git dir");
    tokio::fs::write(repo.join("kd4_features.toml"), "# fixture")
        .await
        .expect("manifest");
    tokio::fs::write(repo.join("source_owners.toml"), source_owners)
        .await
        .expect("source owners");
    let cwd = AbsolutePathBuf::from_absolute_path(&repo).expect("absolute repo");
    let ledger = TaskEvidenceLedger::load_or_new(codex_home, ThreadId::new(), cwd.as_path()).await;
    (temp, repo, ledger)
}

fn text_input(text: &str) -> UserInput {
    UserInput::Text {
        text: text.to_string(),
        text_elements: Vec::new(),
    }
}

fn local_requirement_fixture(requirement_spans: Vec<SourceSpan>) -> SourceLocalClassification {
    SourceLocalClassification {
        local_kind: SourceLocalClassificationKind::RequirementBearing,
        local_semantic_cues: requirement_spans
            .iter()
            .cloned()
            .map(|source_span| LocalSemanticCue {
                kind: LocalSemanticCueKind::Assertion,
                source_span: Some(source_span),
            })
            .collect(),
        requirement_spans,
        reason: "test fixture contains requirements".to_string(),
    }
}

fn local_non_requirement_fixture(reason: &str) -> SourceLocalClassification {
    SourceLocalClassification {
        local_kind: SourceLocalClassificationKind::NonRequirement,
        requirement_spans: Vec::new(),
        local_semantic_cues: Vec::new(),
        reason: reason.to_string(),
    }
}

fn local_unavailable_fixture() -> SourceLocalClassification {
    SourceLocalClassification {
        local_kind: SourceLocalClassificationKind::UnavailableOrTruncated,
        requirement_spans: Vec::new(),
        local_semantic_cues: Vec::new(),
        reason: "test fixture source is unavailable".to_string(),
    }
}

fn source_materialization_fixture(
    dossier: &CompletionReviewDossier,
    explicit_locals: Vec<(String, SourceLocalClassification)>,
    resolved_sources: Vec<ClassifiedSource>,
) -> SourceMaterialization {
    let mut local_classifications = dossier
        .source_classification_cache
        .iter()
        .map(|(key, classification)| (key.clone(), classification.clone()))
        .collect::<BTreeMap<_, _>>();
    for (source_id, classification) in explicit_locals {
        let source = dossier
            .sources
            .iter()
            .find(|source| source.source_id == source_id)
            .expect("explicit local source belongs to dossier");
        local_classifications.insert(source_classification_cache_key(source), classification);
    }
    SourceMaterialization {
        local_classifications,
        resolved_sources,
    }
}

fn active_gap_materialization_fixture(
    dossier: &CompletionReviewDossier,
    gaps: &[ManifestGapInput],
) -> SourceMaterialization {
    let local_classifications = source_local_classifications_with_manifest_gaps(dossier, gaps)
        .expect("manifest gaps produce corrected local facts");
    let resolved_sources = dossier
        .sources
        .iter()
        .map(|source| {
            let local = local_classifications
                .get(&source_classification_cache_key(source))
                .expect("corrected local facts cover every source");
            let kind = match local.local_kind {
                SourceLocalClassificationKind::RequirementBearing => {
                    ClassifiedSourceKind::RequirementBearing
                }
                SourceLocalClassificationKind::NonRequirement => {
                    ClassifiedSourceKind::NonRequirement
                }
                SourceLocalClassificationKind::RelationshipOnlyContext => {
                    ClassifiedSourceKind::SupersededContext
                }
                SourceLocalClassificationKind::UnavailableOrTruncated => {
                    ClassifiedSourceKind::UnavailableOrTruncated
                }
            };
            ClassifiedSource {
                source_id: source.source_id.clone(),
                kind,
                requirements: local
                    .requirement_spans
                    .iter()
                    .cloned()
                    .map(|source_span| ClassifiedRequirement {
                        source_span,
                        status: RequirementStatus::Active,
                        superseded_by: None,
                    })
                    .collect(),
                reason: matches!(
                    local.local_kind,
                    SourceLocalClassificationKind::NonRequirement
                        | SourceLocalClassificationKind::RelationshipOnlyContext
                )
                .then(|| local.reason.clone()),
            }
        })
        .collect();
    SourceMaterialization {
        local_classifications,
        resolved_sources,
    }
}

fn repair_instruction_fixture(
    dossier: &CompletionReviewDossier,
    findings: &[CompletionReviewFindingInput],
) -> String {
    let preview_findings = findings
        .iter()
        .map(|finding| CompletionReviewFindingReceipt {
            finding_id: format!("preview/F{}", finding.local_ordinal),
            requirement_ids: finding.requirement_ids.clone(),
            lens: finding.lens.clone(),
            contract_surface: finding.contract_surface.clone(),
            severity: finding.severity.clone(),
            evidence: finding.evidence.clone(),
            smallest_correction: finding.smallest_correction.clone(),
            proof_route: finding.proof_route.clone(),
        })
        .collect::<Vec<_>>();
    let baseline = build_repair_baseline(dossier, &preview_findings).expect("repair baseline");
    serde_json::json!({
        "repair_baseline_hash": repair_baseline_hash(&baseline),
        "declared_repair_scope": baseline.repair_scope,
    })
    .to_string()
}

async fn classified_requirement_fixture() -> (
    tempfile::TempDir,
    PathBuf,
    TaskEvidenceLedger,
    CompletionReviewDossier,
) {
    let (temp, repo, ledger) = ledger_fixture().await;
    ledger
        .record_planning_update(PlanningUpdateInput {
            plan: vec![proof_free_plan_item("step", StepStatus::Passed)],
            step_evidence: vec![PlanStepEvidenceInput {
                step_id: "step".to_string(),
                validation_disposition: Some(ValidationDisposition::NotRequired),
                source_owner: None,
                implementation_surfaces: Vec::new(),
                surface_roles: Vec::new(),
                validation_asset_paths: Vec::new(),
                mutation_obligations: Vec::new(),
                external_validation_route: None,
            }],
            ..PlanningUpdateInput::default()
        })
        .await;
    assert!(
        ledger
            .record_user_sources("message-1", &[text_input("implement alpha")])
            .await
    );
    let dossier = ledger
        .completion_review_dossier(
            Some("candidate complete"),
            &[],
            &[],
            &ReviewLensSelectionFacts::default(),
            &[],
            true,
            true,
        )
        .await
        .expect("unclassified dossier");
    let source = dossier.sources.first().expect("captured source");
    assert!(matches!(
        ledger
            .apply_source_classification(
                &dossier,
                source_materialization_fixture(
                    &dossier,
                    vec![(
                        source.source_id.clone(),
                        local_requirement_fixture(vec![SourceSpan::Text { start: 0, end: 15 }]),
                    )],
                    vec![ClassifiedSource {
                        source_id: source.source_id.clone(),
                        kind: ClassifiedSourceKind::RequirementBearing,
                        requirements: vec![ClassifiedRequirement {
                            source_span: SourceSpan::Text { start: 0, end: 15 },
                            status: RequirementStatus::Active,
                            superseded_by: None,
                        }],
                        reason: None,
                    }],
                ),
            )
            .await,
        AtomicReviewTransition::Persisted(())
    ));
    let dossier = ledger
        .completion_review_dossier(
            Some("candidate complete"),
            &[],
            &[],
            &ReviewLensSelectionFacts::default(),
            &[],
            true,
            true,
        )
        .await
        .expect("classified dossier");
    (temp, repo, ledger, dossier)
}

#[tokio::test]
async fn completion_review_dossier_collects_step_review_metadata() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    ledger
        .record_planning_update(PlanningUpdateInput {
            plan: vec![proof_free_plan_item("reviewed", StepStatus::Passed)],
            step_evidence: vec![PlanStepEvidenceInput {
                step_id: "reviewed".to_string(),
                source_owner: None,
                implementation_surfaces: Vec::new(),
                surface_roles: vec!["packaging".to_string()],
                validation_asset_paths: vec!["quality/plain.data".to_string()],
                mutation_obligations: Vec::new(),
                validation_disposition: Some(ValidationDisposition::NotRequired),
                external_validation_route: None,
            }],
            ..PlanningUpdateInput::default()
        })
        .await;

    let dossier = ledger
        .completion_review_dossier(
            Some("candidate complete"),
            &[],
            &[],
            &ReviewLensSelectionFacts::default(),
            &[],
            true,
            true,
        )
        .await
        .expect("completion review dossier");

    assert_eq!(
        dossier.review_lens_selection_facts.surface_roles,
        vec!["packaging".to_string()]
    );
    assert_eq!(
        dossier.review_lens_selection_facts.validation_asset_paths,
        vec!["quality/plain.data".to_string()]
    );
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

#[tokio::test]
async fn external_evidence_blank_operation_is_ignored_with_warning() {
    for operation in ["", "   ", "\t\r\n"] {
        let mut result = evidence_result("test-provider", "diagnostic", serde_json::json!({}));
        result.structured_content.as_mut().expect("structured")["evidenceMeta"]["operation"] =
            serde_json::json!(operation);
        assert_external_evidence_rejected(
            &result,
            "MCP evidenceMeta operation is malformed and was ignored",
        )
        .await;
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
    let evidence_path = ledger.evidence_path().expect("evidence path");
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
    let evidence_path = ledger.evidence_path().expect("evidence path");
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
    let (temp, _repo, ledger) = ledger_fixture().await;
    for (call_id, result) in [("first", &first), ("second", &second)] {
        assert_eq!(
            ledger
                .record_external_mcp_evidence_with_limit(
                    "server", "tool", call_id, result, None, None, 1,
                )
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
    *ledger
        .evidence_path
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        Some(blocked_parent.join("evidence.json"));
    assert!(matches!(
        ledger
            .record_external_mcp_evidence_with_limit(
                "server", "tool", "failed", &first, None, None, 1,
            )
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
async fn non_kd4_git_repository_disables_task_evidence() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    let home = temp.path().join("home");
    tokio::fs::create_dir_all(&repo).await.expect("repo");
    initialize_git_repo(&repo).await;
    let thread_id = ThreadId::new();
    let ledger = TaskEvidenceLedger::load_or_new(home.clone(), thread_id, &repo).await;
    assert_eq!(ledger.mode(), TaskEvidenceMode::Disabled);
    assert!(
        !home
            .join("task-evidence")
            .join(format!("{thread_id}.json"))
            .is_file()
    );
}

#[tokio::test]
async fn disabled_non_kd4_mode_never_records_task_state() {
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
    assert!(guard.is_none());
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

#[tokio::test]
async fn rollout_workflow_guardrails_require_durable_reconciliation_before_final() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    ledger
        .record_plan_update(&plan_with(vec![plan_item(
            "review-audit",
            StepStatus::InProgress,
        )]))
        .await;
    assert!(
        ledger
            .record_completion_review_audit(
                "turn-review-audit",
                "failed",
                Some("correctness_gate_failed"),
                vec!["mutation evidence is incomplete".to_string()],
                false,
            )
            .await
    );

    let advisory = ledger.finalization_advisory().await.expect("advisory");
    assert!(advisory.contains("correctness_gate_failed"));
    assert!(advisory.contains("mutation evidence is incomplete"));
    assert!(advisory.contains("reconcile durable task state"));
    assert!(advisory.contains("explicitly state that durable task state remains unresolved"));
    assert!(advisory.contains("Do not claim completion while active or pending"));
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
        validation_route: None,
    }
}

fn proof_free_plan_item(id: &str, status: StepStatus) -> PlanItemArg {
    let mut item = plan_item(id, status);
    item.runtime_paths.clear();
    item
}

fn plan_with(items: Vec<PlanItemArg>) -> UpdatePlanArgs {
    UpdatePlanArgs {
        explanation: None,
        plan: items,
    }
}

fn focused_validation_route(covered_paths: Vec<String>) -> ValidationRoute {
    ValidationRoute {
        leaves: vec![codex_protocol::plan_tool::ValidationRouteLeaf {
            argv: vec![
                "cargo".to_string(),
                "test".to_string(),
                "-p".to_string(),
                "codex-core".to_string(),
                "focused_validation_case".to_string(),
            ],
            uncertainty: "the focused validation contract remains satisfied".to_string(),
            covered_paths,
            covered_contracts: vec!["focused-validation-v1".to_string()],
            timeout_ms: 30_000,
            semantic_timeout: false,
        }],
        ordering: ValidationRouteOrdering::StopOnFailure,
    }
}

#[tokio::test]
async fn focused_planning_uses_stable_work_unit_without_plan_dependencies() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    tokio::fs::create_dir_all(repo.join("src"))
        .await
        .expect("source directory");
    tokio::fs::write(repo.join("src/focused.rs"), "one")
        .await
        .expect("focused source");
    let update = PlanningUpdateInput {
        tier: Some(PlanningTier::Focused),
        source_owner: Some("core".to_string()),
        implementation_surfaces: vec!["src/focused.rs".to_string()],
        acceptance_criteria: vec!["focused edit is present".to_string()],
        mutation_obligations: vec![MutationObligationInput {
            id: "mutation".to_string(),
            description: "edit the focused owner".to_string(),
            paths: vec!["src/focused.rs".to_string()],
        }],
        validation_disposition: Some(ValidationDisposition::NotRequired),
        ..PlanningUpdateInput::default()
    };

    let first = ledger.record_planning_update(update.clone()).await;
    assert_eq!(first.effect, PlanUpdateEffect::Initial);
    assert!(first.public_update.plan.is_empty());
    assert_eq!(first.unfinished_mutation_obligation, Some(true));
    let work_unit_id = {
        let guard = ledger.document.lock().await;
        let document = guard.as_ref().expect("document");
        assert!(document.plan.is_empty());
        document
            .planning
            .work_unit
            .as_ref()
            .expect("focused work unit")
            .id
            .clone()
    };

    assert_eq!(
        ledger.record_planning_update(update).await.effect,
        PlanUpdateEffect::NoOp
    );
    ledger
        .record_edit_intent("focused-edit", &repo, &[PathBuf::from("src/focused.rs")])
        .await;
    tokio::fs::write(repo.join("src/focused.rs"), "two")
        .await
        .expect("focused edit");
    ledger.record_edit_result("focused-edit", "completed").await;

    let guard = ledger.document.lock().await;
    let document = guard.as_ref().expect("document");
    assert!(document.plan.is_empty());
    let work_unit = document
        .planning
        .work_unit
        .as_ref()
        .expect("focused work unit remains");
    assert_eq!(work_unit.id, work_unit_id);
    assert!(work_unit.mutation_obligations[0].satisfied);
    assert_eq!(
        document.edit_receipts[0].work_unit_id.as_deref(),
        Some(work_unit_id.as_str())
    );
    assert!(document.edit_receipts[0].step_id.is_none());
}

#[tokio::test]
async fn structured_plan_actions_without_an_active_step_are_outside_plan() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    ledger
        .record_planning_update(PlanningUpdateInput {
            tier: Some(PlanningTier::Medium),
            plan: vec![plan_item("implement", StepStatus::Pending)],
            ..PlanningUpdateInput::default()
        })
        .await;
    let cwd = AbsolutePathBuf::from_absolute_path(&repo).expect("repo");
    ledger
        .record_command(
            &["rg".to_string(), "owner".to_string()],
            &PathUri::from_abs_path(&cwd),
            0,
            false,
            1,
            false,
        )
        .await;

    let guard = ledger.document.lock().await;
    let document = guard.as_ref().expect("document");
    assert_eq!(document.planning.outside_plan_actions.len(), 1);
    let persisted = serde_json::to_value(document).expect("serialize task evidence");
    assert!(persisted["planning"].get("counters").is_none());
    assert!(document.command_receipts[0].step_id.is_none());
    assert!(document.command_receipts[0].step_revision.is_none());
    assert!(document.command_receipts[0].work_unit_id.is_none());
    assert_eq!(
        document.command_receipts[0].attribution,
        Some(ActionAttributionKind::OutsidePlan)
    );
}

#[tokio::test]
async fn read_only_command_alone_does_not_create_compaction_recovery_state() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    let cwd = AbsolutePathBuf::from_absolute_path(&repo).expect("repo");
    ledger
        .record_command(
            &["rg".to_string(), "recovery-owner".to_string()],
            &PathUri::from_abs_path(&cwd),
            0,
            false,
            1,
            false,
        )
        .await;

    let state = ledger.compaction_task_state().await;
    assert!(state.is_none(), "unexpected compaction state: {state:#?}");
}

#[tokio::test]
async fn compaction_task_state_retains_source_context_for_unresolved_work() {
    let manifest = r#"
schema_version = 2

[[owners]]
id = "core-agent-runtime"
roots = ["codex-rs/core/src/task_evidence.rs"]

[[owners]]
id = "planning-architecture-runtime"
roots = ["codex-rs/core/src/task_evidence_tests.rs"]
"#;
    let (_temp, _repo, ledger) = ledger_fixture_with_source_owners(manifest).await;
    let mut item = plan_item("implement", StepStatus::InProgress);
    item.runtime_paths = vec!["codex-rs/core/src/task_evidence.rs".to_string()];
    ledger
        .record_planning_update(PlanningUpdateInput {
            tier: Some(PlanningTier::Medium),
            step_evidence: vec![PlanStepEvidenceInput {
                step_id: "implement".to_string(),
                source_owner: Some("core-agent-runtime".to_string()),
                implementation_surfaces: vec!["codex-rs/core/src/task_evidence.rs".to_string()],
                surface_roles: Vec::new(),
                validation_asset_paths: Vec::new(),
                mutation_obligations: Vec::new(),
                validation_disposition: None,
                external_validation_route: None,
            }],
            plan: vec![item],
            ..PlanningUpdateInput::default()
        })
        .await;

    let compacted_plan = ledger
        .compaction_task_state()
        .await
        .expect("compaction task state for unresolved plan");
    assert!(
        compacted_plan.contains("- implement source owner: core-agent-runtime"),
        "unexpected compaction state: {compacted_plan}"
    );
    assert!(
        compacted_plan
            .contains("- implement implementation surfaces: codex-rs/core/src/task_evidence.rs")
    );

    let (_temp, _repo, focused_ledger) = ledger_fixture_with_source_owners(manifest).await;
    focused_ledger
        .record_planning_update(PlanningUpdateInput {
            tier: Some(PlanningTier::Focused),
            source_owner: Some("planning-architecture-runtime".to_string()),
            implementation_surfaces: vec!["codex-rs/core/src/task_evidence_tests.rs".to_string()],
            ..PlanningUpdateInput::default()
        })
        .await;

    let compacted_work_unit = focused_ledger
        .compaction_task_state()
        .await
        .expect("compaction task state for unresolved focused work unit");
    assert!(compacted_work_unit.contains("source owner: planning-architecture-runtime"));
    assert!(
        compacted_work_unit
            .contains("implementation surfaces: codex-rs/core/src/task_evidence_tests.rs")
    );
}

#[tokio::test]
async fn compaction_recovery_marks_command_evidence_stale_after_mutation() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    let cwd = AbsolutePathBuf::from_absolute_path(&repo).expect("repo");
    ledger
        .record_command(
            &["git".to_string(), "status".to_string()],
            &PathUri::from_abs_path(&cwd),
            0,
            false,
            1,
            false,
        )
        .await;
    ledger
        .record_command(
            &["write-fixture".to_string()],
            &PathUri::from_abs_path(&cwd),
            0,
            false,
            1,
            true,
        )
        .await;

    let compacted = ledger
        .compaction_task_state()
        .await
        .expect("compaction task state");
    let stale_receipt = compacted
        .lines()
        .find(|line| line.contains("command command-1"))
        .expect("first command receipt");
    assert!(stale_receipt.contains("freshness=stale"));
}

#[tokio::test]
async fn legacy_planning_fact_is_rendered_as_unverified_evidence() {
    let legacy_fact: PlanningFactInput = serde_json::from_value(serde_json::json!({
        "id": "legacy-owner",
        "value": "codex-core"
    }))
    .expect("legacy planning fact");
    assert_eq!(legacy_fact.provenance, ResultProvenance::Unverified);
    assert_eq!(legacy_fact.source, None);
    assert!(legacy_fact.depends_on_paths.is_empty());
    assert!(!legacy_fact.dependencies_current);

    let (_temp, _repo, ledger) = ledger_fixture().await;
    let update = PlanningUpdateInput {
        tier: Some(PlanningTier::Medium),
        facts: vec![legacy_fact],
        plan: vec![plan_item("verify-legacy", StepStatus::InProgress)],
        ..PlanningUpdateInput::default()
    };
    assert_eq!(
        ledger.record_planning_update(update).await.effect,
        PlanUpdateEffect::Initial
    );

    let compacted = ledger
        .compaction_task_state()
        .await
        .expect("compaction task state");
    assert!(compacted.contains("Recorded facts remain evidence claims"));
    assert!(compacted.contains("provenance: unverified; source: not recorded"));
}

#[tokio::test]
async fn stable_planning_patches_preserve_omissions_and_audit_reasoned_removals() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    let initial = PlanningUpdateInput {
        tier: Some(PlanningTier::Medium),
        facts: vec![PlanningFactInput {
            id: "owner".to_string(),
            value: "codex-core".to_string(),
            provenance: ResultProvenance::DirectFileRead,
            source: Some("SOURCEMAP.md".to_string()),
            depends_on_paths: vec!["SOURCEMAP.md".to_string()],
            dependencies_current: true,
        }],
        plan: vec![plan_item("implement", StepStatus::InProgress)],
        ..PlanningUpdateInput::default()
    };
    assert_eq!(
        ledger.record_planning_update(initial).await.effect,
        PlanUpdateEffect::Initial
    );
    let compacted = ledger
        .compaction_task_state()
        .await
        .expect("compaction task state");
    assert!(compacted.contains("do not treat durable storage as proof"));
    assert!(compacted.contains("Recorded evidence claims"));
    assert!(compacted.contains("provenance: direct_file_read; source: SOURCEMAP.md"));
    assert!(compacted.contains("depends on: SOURCEMAP.md"));

    let status_only = PlanningUpdateInput {
        plan: vec![plan_item("implement", StepStatus::Pending)],
        ..PlanningUpdateInput::default()
    };
    assert_eq!(
        ledger
            .record_planning_update(status_only.clone())
            .await
            .effect,
        PlanUpdateEffect::StatusOnly
    );
    assert_eq!(
        ledger.record_planning_update(status_only).await.effect,
        PlanUpdateEffect::NoOp
    );
    {
        let guard = ledger.document.lock().await;
        let document = guard.as_ref().expect("document");
        assert_eq!(document.planning.facts["owner"].value, "codex-core");
        assert_eq!(document.plan.len(), 1);
        assert_eq!(document.plan[0].revision, 1);
    }

    let removal = PlanningUpdateInput {
        removed_facts: vec![ReasonedPlanningRemoval {
            id: "owner".to_string(),
            reason: "owner was superseded by generated evidence".to_string(),
        }],
        removed_steps: vec![ReasonedPlanningRemoval {
            id: "implement".to_string(),
            reason: "work is no longer required".to_string(),
        }],
        ..PlanningUpdateInput::default()
    };
    assert_eq!(
        ledger.record_planning_update(removal).await.effect,
        PlanUpdateEffect::StructuralRevision
    );
    let guard = ledger.document.lock().await;
    let document = guard.as_ref().expect("document");
    assert!(document.planning.facts.is_empty());
    assert!(document.plan.is_empty());
    assert_eq!(document.planning.audit_history.len(), 2);
}

#[tokio::test]
async fn planning_facts_invalidate_only_when_dependency_paths_overlap() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    let facts = [("foo", "src/foo.rs"), ("bar", "src/bar.rs")]
        .into_iter()
        .map(|(id, path)| PlanningFactInput {
            id: id.to_string(),
            value: format!("fact from {path}"),
            provenance: ResultProvenance::DirectFileRead,
            source: Some(path.to_string()),
            depends_on_paths: vec![path.to_string()],
            dependencies_current: true,
        })
        .collect();
    ledger
        .record_planning_update(PlanningUpdateInput {
            tier: Some(PlanningTier::Medium),
            facts,
            plan: vec![plan_item("implement", StepStatus::InProgress)],
            ..PlanningUpdateInput::default()
        })
        .await;

    {
        let mut guard = ledger.document.lock().await;
        let document = guard.as_mut().expect("document");
        invalidate_for_mutation(document, Some(&BTreeSet::from(["src/foo.rs".to_string()])));
        assert!(!document.planning.facts["foo"].dependencies_current);
        assert!(document.planning.facts["bar"].dependencies_current);
    }

    let compacted = ledger
        .compaction_task_state()
        .await
        .expect("compaction task state");
    assert!(compacted.contains("bar: fact from src/bar.rs"));
    assert!(compacted.contains("foo: stale after a dependency changed"));
    assert!(compacted.contains("Invalidated evidence claims (not authoritative)"));
}

#[tokio::test]
async fn multi_obligation_step_requires_all_matching_edits() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    tokio::fs::create_dir_all(repo.join("src"))
        .await
        .expect("source directory");
    for path in ["src/one.rs", "src/two.rs"] {
        tokio::fs::write(repo.join(path), "one")
            .await
            .expect("source fixture");
    }
    ledger
        .record_planning_update(PlanningUpdateInput {
            tier: Some(PlanningTier::Medium),
            step_evidence: vec![PlanStepEvidenceInput {
                step_id: "implement".to_string(),
                source_owner: Some("core".to_string()),
                implementation_surfaces: vec!["src".to_string()],
                surface_roles: Vec::new(),
                validation_asset_paths: Vec::new(),
                mutation_obligations: vec![
                    MutationObligationInput {
                        id: "one".to_string(),
                        description: "edit one".to_string(),
                        paths: vec!["src/one.rs".to_string()],
                    },
                    MutationObligationInput {
                        id: "two".to_string(),
                        description: "edit two".to_string(),
                        paths: vec!["src/two.rs".to_string()],
                    },
                ],
                validation_disposition: Some(ValidationDisposition::NotRequired),
                external_validation_route: None,
            }],
            plan: vec![plan_item("implement", StepStatus::InProgress)],
            ..PlanningUpdateInput::default()
        })
        .await;

    for (call_id, path) in [("edit-one", "src/one.rs"), ("edit-two", "src/two.rs")] {
        ledger
            .record_edit_intent(call_id, &repo, &[PathBuf::from(path)])
            .await;
        tokio::fs::write(repo.join(path), "two")
            .await
            .expect("source edit");
        ledger.record_edit_result(call_id, "completed").await;
        let status = ledger
            .document
            .lock()
            .await
            .as_ref()
            .expect("document")
            .plan[0]
            .status
            .clone();
        if call_id == "edit-one" {
            assert_eq!(status, StepStatus::InProgress);
        } else {
            assert_eq!(status, StepStatus::Implemented);
        }
    }
}

#[tokio::test]
async fn explicit_batch_ack_reuses_bound_route_and_survives_only_disjoint_edits() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    tokio::fs::create_dir_all(repo.join("src"))
        .await
        .expect("source directory");
    tokio::fs::create_dir_all(repo.join("docs"))
        .await
        .expect("docs directory");
    tokio::fs::create_dir_all(repo.join("tests"))
        .await
        .expect("tests directory");
    tokio::fs::write(repo.join("src/step.rs"), "one")
        .await
        .expect("source fixture");
    tokio::fs::write(repo.join("docs/note.md"), "one")
        .await
        .expect("docs fixture");
    tokio::fs::write(repo.join("tests/stable.rs"), "one")
        .await
        .expect("stable test fixture");

    let mut route = focused_validation_route(vec!["src/step.rs".to_string()]);
    let mut stable_leaf = route.leaves[0].clone();
    stable_leaf.argv.pop();
    stable_leaf.argv.push("stable_validation_case".to_string());
    stable_leaf.uncertainty = "the stable validation contract remains satisfied".to_string();
    stable_leaf.covered_paths = vec!["tests/stable.rs".to_string()];
    route.leaves.push(stable_leaf);
    let mut initial = plan_item("step", StepStatus::InProgress);
    initial.validation_route = Some(route.clone());
    ledger.record_plan_update(&plan_with(vec![initial])).await;

    ledger
        .record_edit_intent(
            "implementation-edit",
            &repo,
            &[PathBuf::from("src/step.rs")],
        )
        .await;
    tokio::fs::write(repo.join("src/step.rs"), "two")
        .await
        .expect("implementation edit");
    ledger
        .record_edit_result("implementation-edit", "completed")
        .await;
    assert!(
        ledger.auto_validation_candidate().await.is_none(),
        "automatic edit promotion must not acknowledge the batch"
    );

    // The explicit status transition may omit a previously admitted route.
    // The host retains and rechecks that exact route instead of requiring the
    // model to redundantly resend it.
    ledger
        .record_plan_update(&plan_with(vec![plan_item("step", StepStatus::Implemented)]))
        .await;
    let acknowledged = ledger
        .auto_validation_candidate()
        .await
        .expect("explicit acknowledgement should expose the bound route");
    assert_eq!(acknowledged.route, route);

    ledger
        .record_edit_intent("docs-edit", &repo, &[PathBuf::from("docs/note.md")])
        .await;
    tokio::fs::write(repo.join("docs/note.md"), "two")
        .await
        .expect("disjoint edit");
    ledger.record_edit_result("docs-edit", "completed").await;
    let after_disjoint = ledger
        .auto_validation_candidate()
        .await
        .expect("disjoint edit should preserve acknowledgement");
    assert_eq!(
        after_disjoint.implementation_identity,
        acknowledged.implementation_identity
    );
    assert!(
        after_disjoint.implementation_revision > acknowledged.implementation_revision,
        "orchestration revision should still advance"
    );

    ledger
        .record_edit_intent("relevant-edit", &repo, &[PathBuf::from("src/step.rs")])
        .await;
    tokio::fs::write(repo.join("src/step.rs"), "three")
        .await
        .expect("relevant edit");
    ledger
        .record_edit_result("relevant-edit", "completed")
        .await;
    assert!(
        ledger.auto_validation_candidate().await.is_none(),
        "covered mutation must invalidate the acknowledgement"
    );

    ledger
        .record_plan_update(&plan_with(vec![plan_item("step", StepStatus::InProgress)]))
        .await;
    ledger
        .record_plan_update(&plan_with(vec![plan_item("step", StepStatus::Implemented)]))
        .await;
    let after_relevant = ledger
        .auto_validation_candidate()
        .await
        .expect("explicit acknowledgement should rebind the route");
    assert_ne!(
        after_relevant.leaf_implementation_identities[0],
        acknowledged.leaf_implementation_identities[0],
        "the leaf covering the edited path must receive a fresh identity"
    );
    assert_eq!(
        after_relevant.leaf_implementation_identities[1],
        acknowledged.leaf_implementation_identities[1],
        "a leaf covering only an unchanged path must remain reusable"
    );
}

#[tokio::test]
async fn covered_directory_snapshot_detects_descendant_changes_and_rejects_escape() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    tokio::fs::create_dir_all(repo.join("src/nested"))
        .await
        .expect("covered directory");
    tokio::fs::write(repo.join("src/nested/step.rs"), "one")
        .await
        .expect("covered source fixture");

    let mut step = plan_item("step", StepStatus::InProgress);
    step.validation_route = Some(focused_validation_route(vec!["src".to_string()]));
    ledger.record_plan_update(&plan_with(vec![step])).await;
    ledger
        .record_plan_update(&plan_with(vec![plan_item("step", StepStatus::Implemented)]))
        .await;
    assert!(
        ledger.auto_validation_candidate().await.is_some(),
        "unchanged covered directory should retain its acknowledgement"
    );

    tokio::fs::write(repo.join("src/nested/step.rs"), "two")
        .await
        .expect("out-of-band descendant edit");
    assert!(
        ledger.auto_validation_candidate().await.is_none(),
        "descendant changes must invalidate a covered directory snapshot"
    );

    let escaped = snapshot_file(&repo, "../outside.rs").await;
    assert_eq!(escaped.sha1, None);
    assert_eq!(escaped.exists, false);
    assert!(escaped.read_error.is_some());
}

#[tokio::test]
async fn direct_validation_identity_ignores_unrelated_changes() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    tokio::fs::create_dir_all(repo.join("src"))
        .await
        .expect("source directory");
    tokio::fs::write(repo.join("src/covered.rs"), "one")
        .await
        .expect("covered fixture");
    tokio::fs::write(repo.join("unrelated.md"), "one")
        .await
        .expect("unrelated fixture");

    let covered_paths = vec!["src/covered.rs".to_string()];
    let before = ledger
        .direct_validation_implementation_identity(&covered_paths)
        .await
        .expect("initial direct identity");
    tokio::fs::write(repo.join("unrelated.md"), "two")
        .await
        .expect("unrelated mutation");
    let after_unrelated = ledger
        .direct_validation_implementation_identity(&covered_paths)
        .await
        .expect("identity after unrelated mutation");
    assert_eq!(before, after_unrelated);

    tokio::fs::write(repo.join("src/covered.rs"), "two")
        .await
        .expect("covered mutation");
    let after_covered = ledger
        .direct_validation_implementation_identity(&covered_paths)
        .await
        .expect("identity after covered mutation");
    assert_ne!(before, after_covered);
}

#[tokio::test]
async fn successful_validation_infers_its_current_plan_binding_atomically() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    tokio::fs::create_dir_all(repo.join("src"))
        .await
        .expect("source directory");
    tokio::fs::write(repo.join("src/step.rs"), "implemented")
        .await
        .expect("implementation fixture");

    let route = focused_validation_route(vec!["src/step.rs".to_string()]);
    let mut initial = plan_item("step", StepStatus::InProgress);
    initial.validation_route = Some(route.clone());
    ledger.record_plan_update(&plan_with(vec![initial])).await;
    ledger
        .record_plan_update(&plan_with(vec![plan_item("step", StepStatus::Implemented)]))
        .await;
    let candidate = ledger
        .auto_validation_candidate()
        .await
        .expect("validation candidate");
    let implementation_identity = candidate.leaf_implementation_identities[0].clone();
    let repository = repo.to_string_lossy().into_owned();
    let cwd = AbsolutePathBuf::from_absolute_path(&repo).expect("repo");
    let cwd_uri = PathUri::from_abs_path(&cwd);
    let validation_result = codex_protocol::validation::ValidationResult {
        proof_key: codex_protocol::validation::ValidationProofKey {
            repository: repository.clone(),
            cwd: repository,
            canonical_route_hash: "route".to_string(),
            implementation_identity: implementation_identity.clone(),
            coverage_identity: "coverage".to_string(),
            environment_identity: "test-environment".to_string(),
            toolchain_identity: "test-toolchain".to_string(),
            configuration_identity: "test-configuration".to_string(),
            validation_contract_version: codex_protocol::validation::VALIDATION_CONTRACT_VERSION,
        },
        route: route.clone(),
        call_id: "validation".to_string(),
        process_id: None,
        status: codex_protocol::validation::ValidationTerminalStatus::Succeeded,
        duration_ms: 1,
        summary: Some("focused validation succeeded".to_string()),
        failure_excerpt: None,
        raw_artifact_ref: None,
        raw_artifact_sha256: None,
        freshness: codex_protocol::validation::ValidationFreshness::Executed,
    };

    ledger
        .record_command_bound_with_validation_result(
            &route.leaves[0].argv,
            &cwd_uri,
            0,
            false,
            1,
            false,
            None,
            None,
            Some(&implementation_identity),
            Some(validation_result),
            None,
        )
        .await;

    let guard = ledger.document.lock().await;
    let document = guard.as_ref().expect("document");
    let step = &document.plan[0];
    assert_eq!(step.status, StepStatus::Passed);
    assert!(step.validation_receipt_id.is_some());
    let receipt = document.command_receipts.last().expect("command receipt");
    assert_eq!(receipt.step_id.as_deref(), Some("step"));
    assert_eq!(receipt.step_revision, Some(candidate.step_revision));
}

#[tokio::test]
async fn multi_leaf_validation_passes_only_after_every_leaf_has_current_proof() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    tokio::fs::create_dir_all(repo.join("src"))
        .await
        .expect("source directory");
    tokio::fs::write(repo.join("src/first.rs"), "one")
        .await
        .expect("first fixture");
    tokio::fs::write(repo.join("src/second.rs"), "two")
        .await
        .expect("second fixture");
    tokio::fs::write(repo.join("unrelated.md"), "one")
        .await
        .expect("unrelated fixture");

    let mut route = focused_validation_route(vec!["src/first.rs".to_string()]);
    let mut second_leaf = route.leaves[0].clone();
    second_leaf.argv.pop();
    second_leaf.argv.push("second_validation_case".to_string());
    second_leaf.uncertainty = "the second focused contract remains satisfied".to_string();
    second_leaf.covered_paths = vec!["src/second.rs".to_string()];
    route.leaves.push(second_leaf);

    let mut initial = plan_item("step", StepStatus::InProgress);
    initial.validation_route = Some(route.clone());
    ledger.record_plan_update(&plan_with(vec![initial])).await;
    ledger
        .record_plan_update(&plan_with(vec![plan_item("step", StepStatus::Implemented)]))
        .await;
    let step_revision = {
        let guard = ledger.document.lock().await;
        guard.as_ref().expect("document").plan[0].revision
    };
    let initial_candidate = ledger
        .auto_validation_candidate()
        .await
        .expect("initial validation candidate");
    assert_eq!(initial_candidate.leaf_implementation_identities.len(), 2);
    let cwd = AbsolutePathBuf::from_absolute_path(&repo).expect("repo");
    let cwd_uri = PathUri::from_abs_path(&cwd);
    let repository = repo.to_string_lossy().into_owned();
    let validation_result = |index: usize, identity: &str| {
        let leaf_route = ValidationRoute {
            leaves: vec![route.leaves[index].clone()],
            ordering: route.ordering,
        };
        codex_protocol::validation::ValidationResult {
            proof_key: codex_protocol::validation::ValidationProofKey {
                repository: repository.clone(),
                cwd: repository.clone(),
                canonical_route_hash: format!("route-{index}"),
                implementation_identity: identity.to_string(),
                coverage_identity: format!("coverage-{index}"),
                environment_identity: "test-environment".to_string(),
                toolchain_identity: "test-toolchain".to_string(),
                configuration_identity: "test-configuration".to_string(),
                validation_contract_version:
                    codex_protocol::validation::VALIDATION_CONTRACT_VERSION,
            },
            route: leaf_route,
            call_id: format!("validation-{index}"),
            process_id: None,
            status: codex_protocol::validation::ValidationTerminalStatus::Succeeded,
            duration_ms: 1,
            summary: Some("focused validation succeeded".to_string()),
            failure_excerpt: None,
            raw_artifact_ref: None,
            raw_artifact_sha256: None,
            freshness: if index == 0 {
                codex_protocol::validation::ValidationFreshness::Executed
            } else {
                codex_protocol::validation::ValidationFreshness::Reused
            },
        }
    };

    ledger
        .record_command_bound_with_validation_result(
            &route.leaves[0].argv,
            &cwd_uri,
            0,
            false,
            1,
            false,
            None,
            None,
            Some(&initial_candidate.leaf_implementation_identities[0]),
            Some(validation_result(
                0,
                &initial_candidate.leaf_implementation_identities[0],
            )),
            Some(("step", step_revision)),
        )
        .await;
    {
        let guard = ledger.document.lock().await;
        let step = &guard.as_ref().expect("document").plan[0];
        assert_eq!(step.status, StepStatus::Implemented);
        assert!(step.validation_receipt_id.is_none());
    }

    ledger
        .record_command_bound_with_validation_result(
            &route.leaves[1].argv,
            &cwd_uri,
            0,
            false,
            1,
            false,
            None,
            None,
            Some(&initial_candidate.leaf_implementation_identities[1]),
            Some(validation_result(
                1,
                &initial_candidate.leaf_implementation_identities[1],
            )),
            Some(("step", step_revision)),
        )
        .await;
    {
        let guard = ledger.document.lock().await;
        let step = &guard.as_ref().expect("document").plan[0];
        assert_eq!(step.status, StepStatus::Passed);
        assert!(step.validation_receipt_id.is_some());
    }

    ledger
        .record_command_bound_with_validation_result(
            &["touch".to_string(), "unrelated.md".to_string()],
            &cwd_uri,
            0,
            false,
            1,
            true,
            Some(&BTreeSet::from([repo.join("unrelated.md")])),
            None,
            None,
            None,
            None,
        )
        .await;
    {
        let guard = ledger.document.lock().await;
        let document = guard.as_ref().expect("document");
        assert_eq!(document.plan[0].status, StepStatus::Passed);
        assert!(
            document
                .command_receipts
                .iter()
                .filter(|receipt| receipt.step_id.as_deref() == Some("step"))
                .all(|receipt| command_receipt_has_current_proof_identity(document, receipt)),
            "disjoint edits must retain every leaf receipt for the passed step"
        );
    }

    tokio::fs::write(repo.join("src/first.rs"), "changed")
        .await
        .expect("first implementation mutation");
    ledger
        .record_command_bound_with_validation_result(
            &["touch".to_string(), "src/first.rs".to_string()],
            &cwd_uri,
            0,
            false,
            1,
            true,
            Some(&BTreeSet::from([repo.join("src/first.rs")])),
            None,
            None,
            None,
            None,
        )
        .await;
    {
        let guard = ledger.document.lock().await;
        let document = guard.as_ref().expect("document");
        assert_eq!(document.plan[0].status, StepStatus::Implemented);
        assert!(
            document
                .command_receipts
                .iter()
                .filter(|receipt| receipt.step_id.as_deref() == Some("step"))
                .all(|receipt| !command_receipt_has_current_proof_identity(document, receipt)),
            "the mutation invalidates the old batch acknowledgement"
        );
    }

    ledger
        .record_plan_update(&plan_with(vec![plan_item("step", StepStatus::InProgress)]))
        .await;
    ledger
        .record_plan_update(&plan_with(vec![plan_item("step", StepStatus::Implemented)]))
        .await;
    let candidate = ledger
        .auto_validation_candidate()
        .await
        .expect("changed leaf should remain a validation candidate");
    assert_eq!(candidate.route.leaves, vec![route.leaves[0].clone()]);
    assert_eq!(candidate.leaf_implementation_identities.len(), 1);

    ledger
        .record_command_bound_with_validation_result(
            &route.leaves[0].argv,
            &cwd_uri,
            0,
            false,
            1,
            false,
            None,
            None,
            Some(&candidate.leaf_implementation_identities[0]),
            Some(validation_result(
                0,
                &candidate.leaf_implementation_identities[0],
            )),
            Some(("step", candidate.step_revision)),
        )
        .await;
    let guard = ledger.document.lock().await;
    let document = guard.as_ref().expect("document");
    assert_eq!(document.plan[0].status, StepStatus::Passed);
    assert!(
        document
            .command_receipts
            .iter()
            .filter(|receipt| receipt.step_id.as_deref() == Some("step"))
            .filter(|receipt| command_receipt_has_current_proof_identity(document, receipt))
            .count()
            >= 2,
        "the changed and unchanged leaves must jointly close the step"
    );
}

#[tokio::test]
async fn unknown_coverage_acknowledgement_requires_repository_wide_quiescence() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    tokio::fs::write(repo.join("unrelated.txt"), "one")
        .await
        .expect("repository fixture");
    let mut initial = plan_item("step", StepStatus::InProgress);
    initial.validation_route = Some(focused_validation_route(Vec::new()));
    ledger.record_plan_update(&plan_with(vec![initial])).await;
    ledger
        .record_plan_update(&plan_with(vec![plan_item("step", StepStatus::Implemented)]))
        .await;
    assert!(
        ledger
            .auto_validation_candidate()
            .await
            .is_some_and(|candidate| candidate.repository_wide)
    );

    ledger
        .record_edit_intent("repository-edit", &repo, &[PathBuf::from("unrelated.txt")])
        .await;
    tokio::fs::write(repo.join("unrelated.txt"), "two")
        .await
        .expect("repository edit");
    ledger
        .record_edit_result("repository-edit", "completed")
        .await;
    assert!(ledger.auto_validation_candidate().await.is_none());
}

#[test]
fn repository_wide_validation_coverage_invalidates_for_disjoint_mutations() {
    let repository_wide = EvidencePlanStep {
        id: "step".to_string(),
        revision: 1,
        step: "step".to_string(),
        status: StepStatus::Passed,
        depends_on: Vec::new(),
        acceptance_criteria: Vec::new(),
        runtime_paths: Vec::new(),
        generated_artifacts: Vec::new(),
        risks: Vec::new(),
        validation_route: Some(focused_validation_route(Vec::new())),
        external_validation_route: None,
        validation_disposition: ValidationDisposition::Executable,
        source_owner: None,
        implementation_surfaces: vec!["src/owned.rs".to_string()],
        surface_roles: Vec::new(),
        validation_asset_paths: Vec::new(),
        mutation_obligations: Vec::new(),
        validation_receipt_id: Some("receipt".to_string()),
        edit_paths: BTreeSet::new(),
    };
    let disjoint = BTreeSet::from(["docs/unrelated.md".to_string()]);

    assert!(mutation_can_affect_step(&repository_wide, Some(&disjoint)));

    let mut scoped = repository_wide;
    scoped.validation_route = Some(focused_validation_route(vec!["src/owned.rs".to_string()]));
    assert!(!mutation_can_affect_step(&scoped, Some(&disjoint)));
}

fn command_receipt(id: &str) -> CommandReceipt {
    CommandReceipt {
        id: id.to_string(),
        recorded_at: timestamp(),
        epoch: 0,
        step_id: None,
        step_revision: None,
        work_unit_id: None,
        attribution: None,
        command: vec!["true".to_string()],
        cwd: ".".to_string(),
        exit_code: 0,
        timed_out: false,
        duration_ms: 1,
        possible_mutation: false,
        observed_mutation: false,
        host_mutation_revision: None,
        manifest_revision: None,
        user_source_ledger_hash: None,
        requirement_manifest_hash: None,
        implementation_identity_hash: None,
        validation_result: None,
        source_thread_id: None,
        source_agent_path: None,
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
                step_revision: None,
                work_unit_id: None,
                attribution: None,
                command: vec!["cargo".to_string(), "test".to_string()],
                cwd: ".".to_string(),
                exit_code: 0,
                timed_out: false,
                duration_ms: 1,
                possible_mutation: false,
                observed_mutation: false,
                host_mutation_revision: None,
                manifest_revision: None,
                user_source_ledger_hash: None,
                requirement_manifest_hash: None,
                implementation_identity_hash: None,
                validation_result: None,
                source_thread_id: None,
                source_agent_path: None,
            },
            CommandReceipt {
                id: "timed-out".to_string(),
                recorded_at: timestamp(),
                epoch: 2,
                step_id: None,
                step_revision: None,
                work_unit_id: None,
                attribution: None,
                command: vec!["slow-check".to_string()],
                cwd: ".".to_string(),
                exit_code: 124,
                timed_out: true,
                duration_ms: 1,
                possible_mutation: true,
                observed_mutation: false,
                host_mutation_revision: None,
                manifest_revision: None,
                user_source_ledger_hash: None,
                requirement_manifest_hash: None,
                implementation_identity_hash: None,
                validation_result: None,
                source_thread_id: None,
                source_agent_path: None,
            },
            CommandReceipt {
                id: "stale".to_string(),
                recorded_at: timestamp(),
                epoch: 1,
                step_id: None,
                step_revision: None,
                work_unit_id: None,
                attribution: None,
                command: vec!["secret-from-prior-epoch".to_string()],
                cwd: ".".to_string(),
                exit_code: 0,
                timed_out: false,
                duration_ms: 1,
                possible_mutation: false,
                observed_mutation: false,
                host_mutation_revision: None,
                manifest_revision: None,
                user_source_ledger_hash: None,
                requirement_manifest_hash: None,
                implementation_identity_hash: None,
                validation_result: None,
                source_thread_id: None,
                source_agent_path: None,
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
        let parent = proof_free_plan_item("parent", dependency_status);
        let mut child = proof_free_plan_item("child", StepStatus::Passed);
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
        let mut first = proof_free_plan_item("first", first_status);
        first.depends_on = vec!["second".to_string()];
        let mut second = proof_free_plan_item("second", StepStatus::Skipped);
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
        revision: 1,
        step: "step".to_string(),
        status: StepStatus::Passed,
        depends_on: Vec::new(),
        acceptance_criteria: Vec::new(),
        runtime_paths: Vec::new(),
        generated_artifacts: Vec::new(),
        risks: Vec::new(),
        validation_route: None,
        external_validation_route: None,
        validation_disposition: ValidationDisposition::NotRequired,
        source_owner: None,
        implementation_surfaces: Vec::new(),
        surface_roles: Vec::new(),
        validation_asset_paths: Vec::new(),
        mutation_obligations: Vec::new(),
        validation_receipt_id: None,
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
        revision: 1,
        step: "step".to_string(),
        status: StepStatus::Implemented,
        depends_on: Vec::new(),
        acceptance_criteria: Vec::new(),
        runtime_paths: Vec::new(),
        generated_artifacts: Vec::new(),
        risks: Vec::new(),
        validation_route: None,
        external_validation_route: None,
        validation_disposition: ValidationDisposition::NotRequired,
        source_owner: None,
        implementation_surfaces: Vec::new(),
        surface_roles: Vec::new(),
        validation_asset_paths: Vec::new(),
        mutation_obligations: Vec::new(),
        validation_receipt_id: None,
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
async fn completed_plan_step_requires_validation_proof() {
    let (_temp, _repo, proof_free_ledger) = ledger_fixture().await;
    let mut proof_free = plan_item("non-code", StepStatus::Completed);
    proof_free.runtime_paths.clear();
    let proof_free_update = proof_free_ledger
        .record_planning_update(PlanningUpdateInput {
            plan: vec![proof_free],
            ..PlanningUpdateInput::default()
        })
        .await;
    assert_eq!(
        proof_free_update.public_update.plan[0].status,
        StepStatus::Passed
    );
    {
        let guard = proof_free_ledger.document.lock().await;
        assert_eq!(
            guard.as_ref().expect("document").plan[0].validation_disposition,
            ValidationDisposition::NotRequired
        );
    }
    assert_eq!(
        proof_free_ledger
            .completion_gate()
            .await
            .expect("gate")
            .status,
        TaskCompletionStatus::Passed
    );

    let (_temp, _repo, ledger) = ledger_fixture().await;
    let unproved = ledger
        .record_planning_update(PlanningUpdateInput {
            plan: vec![plan_item("step", StepStatus::Completed)],
            ..PlanningUpdateInput::default()
        })
        .await;

    assert_eq!(
        unproved.public_update.plan[0].status,
        StepStatus::InProgress
    );
    {
        let guard = ledger.document.lock().await;
        let step = &guard.as_ref().expect("document").plan[0];
        assert_eq!(
            step.validation_disposition,
            ValidationDisposition::UnresolvedDiscoverable
        );
    }
    assert_eq!(
        ledger.completion_gate().await.expect("gate").status,
        TaskCompletionStatus::Partial
    );

    let explicitly_unvalidated = ledger
        .record_planning_update(PlanningUpdateInput {
            step_evidence: vec![PlanStepEvidenceInput {
                step_id: "step".to_string(),
                source_owner: None,
                implementation_surfaces: vec!["src/step.rs".to_string()],
                surface_roles: Vec::new(),
                validation_asset_paths: Vec::new(),
                mutation_obligations: Vec::new(),
                validation_disposition: Some(ValidationDisposition::NotRequired),
                external_validation_route: None,
            }],
            plan: vec![plan_item("step", StepStatus::Completed)],
            ..PlanningUpdateInput::default()
        })
        .await;

    assert_eq!(
        explicitly_unvalidated.public_update.plan[0].status,
        StepStatus::InProgress
    );
    {
        let guard = ledger.document.lock().await;
        assert_eq!(
            guard.as_ref().expect("document").plan[0].validation_disposition,
            ValidationDisposition::UnresolvedDiscoverable
        );
    }
    assert_eq!(
        ledger.completion_gate().await.expect("gate").status,
        TaskCompletionStatus::Partial
    );

    let (_temp, _repo, focused_ledger) = ledger_fixture().await;
    focused_ledger
        .record_planning_update(PlanningUpdateInput {
            tier: Some(PlanningTier::Focused),
            implementation_surfaces: vec!["src/focused.rs".to_string()],
            ..PlanningUpdateInput::default()
        })
        .await;
    {
        let guard = focused_ledger.document.lock().await;
        assert_eq!(
            guard
                .as_ref()
                .expect("document")
                .planning
                .work_unit
                .as_ref()
                .expect("focused work unit")
                .validation_disposition,
            ValidationDisposition::UnresolvedDiscoverable
        );
    }
    let gate = focused_ledger.completion_gate().await.expect("gate");
    assert_eq!(gate.status, TaskCompletionStatus::Partial);
    assert!(
        gate.reasons
            .iter()
            .any(|reason| reason.contains("lacks current validation proof"))
    );
}

#[tokio::test]
async fn source_owner_is_derived_from_implementation_surfaces() {
    let manifest = r#"
schema_version = 2

[[owners]]
id = "broad"
roots = ["codex-rs/core/src"]

[[owners]]
id = "narrow"
roots = ["codex-rs/core/src/tools/handlers"]

[[owners]]
id = "other"
roots = ["codex-rs/app-server"]
"#;
    let (_temp, repo, ledger) = ledger_fixture_with_source_owners(manifest).await;
    ledger
        .record_planning_update(PlanningUpdateInput {
            step_evidence: vec![PlanStepEvidenceInput {
                step_id: "owned".to_string(),
                source_owner: Some("caller-authored-owner".to_string()),
                implementation_surfaces: vec![
                    "codex-rs/core/src/tools/handlers/plan.rs".to_string(),
                ],
                surface_roles: Vec::new(),
                validation_asset_paths: Vec::new(),
                mutation_obligations: Vec::new(),
                validation_disposition: None,
                external_validation_route: None,
            }],
            plan: vec![plan_item("owned", StepStatus::InProgress)],
            ..PlanningUpdateInput::default()
        })
        .await;
    {
        let guard = ledger.document.lock().await;
        let step = &guard.as_ref().expect("document").plan[0];
        assert_eq!(step.source_owner.as_deref(), Some("narrow"));
        assert_eq!(
            step.implementation_surfaces,
            vec!["codex-rs/core/src/tools/handlers/plan.rs".to_string()]
        );
    }

    ledger
        .record_planning_update(PlanningUpdateInput {
            step_evidence: vec![PlanStepEvidenceInput {
                step_id: "owned".to_string(),
                source_owner: Some("narrow".to_string()),
                implementation_surfaces: vec![
                    "codex-rs/core/src/task_evidence.rs".to_string(),
                    "codex-rs/app-server/src/lib.rs".to_string(),
                ],
                surface_roles: Vec::new(),
                validation_asset_paths: Vec::new(),
                mutation_obligations: Vec::new(),
                validation_disposition: None,
                external_validation_route: None,
            }],
            plan: vec![plan_item("owned", StepStatus::InProgress)],
            ..PlanningUpdateInput::default()
        })
        .await;
    {
        let guard = ledger.document.lock().await;
        assert!(
            guard.as_ref().expect("document").plan[0]
                .source_owner
                .is_none()
        );
    }

    let codex_home = ledger.codex_home.clone().expect("codex home");
    let thread_id = ledger.thread_id.clone().expect("thread id");
    let snapshot = {
        let mut guard = ledger.document.lock().await;
        let document = guard.as_mut().expect("document");
        document.plan[0].source_owner = Some("legacy-fixture-owner".to_string());
        document.plan[0].implementation_surfaces = vec!["unowned/file.rs".to_string()];
        document.revision = document.revision.saturating_add(1);
        document.clone()
    };
    assert_eq!(
        ledger.persist_document(&snapshot).await,
        PersistOutcome::Persisted
    );
    drop(ledger);
    let reloaded = TaskEvidenceLedger::load_or_new(
        codex_home,
        ThreadId::from_string(&thread_id).expect("thread id"),
        &repo,
    )
    .await;
    {
        let guard = reloaded.document.lock().await;
        assert!(
            guard.as_ref().expect("document").plan[0]
                .source_owner
                .is_none()
        );
    }

    let ambiguous_manifest = r#"
schema_version = 2

[[owners]]
id = "first"
roots = ["src"]

[[owners]]
id = "second"
roots = ["src"]
"#;
    let (_temp, _repo, ambiguous) = ledger_fixture_with_source_owners(ambiguous_manifest).await;
    ambiguous
        .record_planning_update(PlanningUpdateInput {
            source_owner: Some("first".to_string()),
            implementation_surfaces: vec!["src/lib.rs".to_string()],
            tier: Some(PlanningTier::Focused),
            ..PlanningUpdateInput::default()
        })
        .await;
    {
        let guard = ambiguous.document.lock().await;
        assert!(
            guard
                .as_ref()
                .expect("document")
                .planning
                .work_unit
                .as_ref()
                .expect("work unit")
                .source_owner
                .is_none()
        );
    }

    let (_temp, _repo, missing_manifest) = ledger_fixture().await;
    missing_manifest
        .record_planning_update(PlanningUpdateInput {
            source_owner: Some("caller-authored-owner".to_string()),
            implementation_surfaces: vec!["src/lib.rs".to_string()],
            tier: Some(PlanningTier::Focused),
            ..PlanningUpdateInput::default()
        })
        .await;
    let guard = missing_manifest.document.lock().await;
    assert!(
        guard
            .as_ref()
            .expect("document")
            .planning
            .work_unit
            .as_ref()
            .expect("work unit")
            .source_owner
            .is_none()
    );
}

#[tokio::test]
async fn repository_source_owner_manifest_keeps_planning_boundaries_exact() {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("repository root");
    let index = load_source_owner_index(repo_root)
        .await
        .expect("repository source owner index");

    for planning_surface in [
        "codex-rs/core/src/plan_store.rs",
        "codex-rs/core/src/tools/handlers/plan.rs",
        "codex-rs/core/src/session/reasoning_governor.rs",
    ] {
        assert_eq!(
            index.derive(&[planning_surface.to_string()]).as_deref(),
            Some("planning-architecture-runtime"),
            "{planning_surface} must remain planning-owned"
        );
    }

    for core_surface in [
        "codex-rs/core/src/tools/handlers/request_plugin_install.rs",
        "codex-rs/core/src/session/mcp_runtime.rs",
    ] {
        assert_eq!(
            index.derive(&[core_surface.to_string()]).as_deref(),
            Some("core-agent-runtime"),
            "{core_surface} must not be swallowed by planning ownership"
        );
    }

    assert_eq!(
        index
            .derive(&["codex-rs/core/src/task_evidence.rs".to_string()])
            .as_deref(),
        Some("task-evidence-runtime")
    );
    assert_eq!(
        index
            .derive(&["codex-rs/utils/build-info/src/lib.rs".to_string()])
            .as_deref(),
        Some("shared-utility-crates")
    );
}

#[tokio::test]
async fn source_owner_snapshot_reuses_unchanged_index_and_rejects_stale_refreshes() {
    let temp = tempfile::tempdir().expect("tempdir");
    let manifest = temp.path().join("source_owners.toml");
    tokio::fs::write(
        &manifest,
        "schema_version = 2\n[[owners]]\nid = \"first\"\nroots = [\"src\"]\n",
    )
    .await
    .expect("write first manifest");
    let mut snapshot = SourceOwnerIndexSnapshot::load(temp.path()).await;
    let first = snapshot.shared_index().expect("first index");

    let unchanged = snapshot
        .refresh(temp.path())
        .await
        .expect("unchanged index");
    assert!(Arc::ptr_eq(&first, &unchanged));

    tokio::fs::write(
        &manifest,
        "schema_version = 2\n[[owners]]\nid = \"second-owner\"\nroots = [\"src\"]\n",
    )
    .await
    .expect("write changed manifest");
    let changed = snapshot.refresh(temp.path()).await.expect("changed index");
    assert!(!Arc::ptr_eq(&first, &changed));
    assert_eq!(
        changed.derive(&["src/lib.rs".to_string()]).as_deref(),
        Some("second-owner")
    );

    let stale_base = snapshot.clone();
    tokio::fs::write(
        &manifest,
        "schema_version = 2\n[[owners]]\nid = \"stale-third-owner\"\nroots = [\"src\"]\n",
    )
    .await
    .expect("write stale candidate manifest");
    let stale_refresh = stale_base.refreshed(temp.path()).await;

    tokio::fs::write(
        &manifest,
        "schema_version = 2\n[[owners]]\nid = \"current-fourth-owner-longer\"\nroots = [\"src\"]\n",
    )
    .await
    .expect("write current manifest");
    let current_base = snapshot.clone();
    let current_refresh = current_base.refreshed(temp.path()).await;
    snapshot.install_if_unchanged(&current_base, current_refresh);
    snapshot.install_if_unchanged(&stale_base, stale_refresh);

    assert_eq!(
        snapshot
            .shared_index()
            .expect("current index")
            .derive(&["src/lib.rs".to_string()])
            .as_deref(),
        Some("current-fourth-owner-longer")
    );
}

#[tokio::test]
async fn explicit_passed_and_completed_are_authoritative_success_states() {
    for requested in [StepStatus::Passed, StepStatus::Completed] {
        let (_temp, _repo, ledger) = ledger_fixture().await;
        let normalized = ledger
            .record_plan_update(&plan_with(vec![proof_free_plan_item("step", requested)]))
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
        .record_plan_update(&plan_with(vec![proof_free_plan_item(
            "step",
            StepStatus::Pending,
        )]))
        .await;
    let mut changed = proof_free_plan_item("step", StepStatus::Passed);
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
        .record_plan_update(&plan_with(vec![proof_free_plan_item(
            "step",
            StepStatus::Passed,
        )]))
        .await;
    ledger
        .document
        .lock()
        .await
        .as_mut()
        .expect("document")
        .plan[0]
        .runtime_paths = vec!["src/step.rs".to_string()];
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
        .record_plan_update(&plan_with(vec![proof_free_plan_item(
            "step",
            StepStatus::Passed,
        )]))
        .await;
    ledger
        .document
        .lock()
        .await
        .as_mut()
        .expect("document")
        .plan[0]
        .runtime_paths = vec!["src/step.rs".to_string()];
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
    let mut missing = proof_free_plan_item("step", StepStatus::Passed);
    missing.generated_artifacts = vec!["generated/output.json".to_string()];
    missing_ledger
        .record_plan_update(&plan_with(vec![missing]))
        .await;
    let missing_gate = missing_ledger
        .completion_gate()
        .await
        .expect("missing gate");
    assert_eq!(missing_gate.status, TaskCompletionStatus::Partial);
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
    let mut present = proof_free_plan_item("step", StepStatus::Passed);
    present.generated_artifacts = vec!["generated/output.json".to_string()];
    present_ledger
        .record_plan_update(&plan_with(vec![present]))
        .await;
    let present_gate = present_ledger
        .completion_gate()
        .await
        .expect("present gate");
    assert_eq!(present_gate.status, TaskCompletionStatus::Partial);
    assert!(
        present_gate
            .reasons
            .iter()
            .all(|reason| !reason.contains("missing, unreadable, or unhashable"))
    );

    tokio::fs::remove_file(repo.join("generated/output.json"))
        .await
        .expect("delete artifact");
    let deleted_gate = present_ledger
        .completion_gate()
        .await
        .expect("deleted gate");
    assert_eq!(deleted_gate.status, TaskCompletionStatus::Partial);
    assert!(
        deleted_gate
            .reasons
            .iter()
            .any(|reason| reason.contains("missing, unreadable, or unhashable"))
    );
    let guard = present_ledger.document.lock().await;
    assert_eq!(
        guard.as_ref().expect("document").plan[0].status,
        StepStatus::InProgress
    );
}

#[tokio::test]
async fn skipped_step_artifact_requirements_do_not_block_completion() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    let mut skipped = plan_item("skipped", StepStatus::Skipped);
    skipped.generated_artifacts = vec!["generated/missing.json".to_string()];
    ledger.record_plan_update(&plan_with(vec![skipped])).await;

    let gate = ledger.completion_gate().await.expect("completion gate");

    assert_eq!(gate.status, TaskCompletionStatus::Passed);
    assert!(gate.reasons.is_empty());
    let guard = ledger.document.lock().await;
    let document = guard.as_ref().expect("document");
    assert!(declared_generated_artifact_requirements(document).is_empty());
    assert!(document.latest_generated_artifact_hashes.is_empty());
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
    ledger.record_plan_update(&plan_with(vec![skipped])).await;
    drop(ledger);

    let mut persisted: Value = serde_json::from_slice(
        &tokio::fs::read(&evidence_path)
            .await
            .expect("persisted evidence"),
    )
    .expect("valid evidence");
    assert_eq!(persisted["schema_version"], TASK_EVIDENCE_SCHEMA_VERSION);
    persisted["schema_version"] = serde_json::json!(FROZEN_TASK_EVIDENCE_V12_SCHEMA_VERSION);
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
        assert!(declared_generated_artifact_requirements(document).is_empty());
        let migrated = serde_json::to_value(document).expect("serialize migrated evidence");
        assert!(migrated.get("generated_artifact_requirements").is_none());
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
        let mut step = proof_free_plan_item("step", StepStatus::Passed);
        step.generated_artifacts = vec![artifact];
        ledger.record_plan_update(&plan_with(vec![step])).await;
        let gate = ledger.completion_gate().await.expect("escape gate");
        assert_eq!(gate.status, TaskCompletionStatus::Partial);
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
    let mut valid = proof_free_plan_item("step", StepStatus::Passed);
    valid.generated_artifacts = vec!["generated/output.json".to_string()];
    ledger.record_plan_update(&plan_with(vec![valid])).await;
    let valid_gate = ledger.completion_gate().await.expect("valid artifact gate");
    assert_eq!(valid_gate.status, TaskCompletionStatus::Partial);
    assert!(
        valid_gate
            .reasons
            .iter()
            .all(|reason| !reason.contains("missing, unreadable, or unhashable"))
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
    legacy["plan"][0]["status"] = serde_json::json!("passed");
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
    assert!(declared_generated_artifact_requirements(&migrated).is_empty());
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
async fn v3_to_current_discards_obsolete_repair_counter_without_reopening_passed_work() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    ledger
        .record_plan_update(&plan_with(vec![proof_free_plan_item(
            "step",
            StepStatus::Passed,
        )]))
        .await;
    let gate = ledger.completion_gate().await.expect("completion gate");
    assert_eq!(gate.status, TaskCompletionStatus::Passed);

    let codex_home = ledger.codex_home.as_ref().expect("codex home").clone();
    let evidence_path = ledger.evidence_path().expect("evidence path");
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
    assert_eq!(
        persisted["schema_version"],
        serde_json::json!(TASK_EVIDENCE_SCHEMA_VERSION)
    );
    assert!(persisted.get("repair_turns_used").is_none());
}

#[tokio::test]
async fn completion_review_receipts_are_bounded_and_never_change_completion_control_flow() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    ledger
        .record_plan_update(&plan_with(vec![proof_free_plan_item(
            "step",
            StepStatus::Passed,
        )]))
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
async fn frozen_v4_loader_refuses_v5_without_modifying_the_file() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    ledger
        .record_plan_update(&plan_with(vec![plan_item("step", StepStatus::Passed)]))
        .await;
    let evidence_path = ledger.evidence_path().expect("evidence path");
    let document = ledger
        .document
        .lock()
        .await
        .as_ref()
        .expect("document")
        .clone();
    let mut future = serde_json::to_value(document).expect("serialize");
    future["schema_version"] = serde_json::json!(5);
    let future_bytes = serde_json::to_vec_pretty(&future).expect("serialize v5 evidence");
    tokio::fs::write(&evidence_path, &future_bytes)
        .await
        .expect("write v5 evidence");

    assert_eq!(
        frozen_v4::admit(&evidence_path).await,
        frozen_v4::Admission::NewerSchema { schema_version: 5 }
    );
    assert_eq!(
        frozen_v4::load(&evidence_path).await,
        frozen_v4::LoadOutcome::Disabled
    );
    assert_eq!(
        tokio::fs::read(&evidence_path)
            .await
            .expect("untouched v5 evidence"),
        future_bytes
    );
}

#[tokio::test]
async fn rich_v4_to_v5_migration_preserves_evidence_and_seeds_terminal_lineage() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    let home = temp.path().join("home");
    tokio::fs::create_dir_all(repo.join(".git"))
        .await
        .expect("git directory");
    tokio::fs::create_dir_all(repo.join("src"))
        .await
        .expect("source directory");
    tokio::fs::create_dir_all(repo.join("generated"))
        .await
        .expect("generated directory");
    tokio::fs::write(repo.join("kd4_features.toml"), "# fixture")
        .await
        .expect("manifest");
    let owned_bytes = b"pub fn migrated() {}\n";
    let generated_bytes = br#"{"schema":4}"#;
    tokio::fs::write(repo.join("src/owned.rs"), owned_bytes)
        .await
        .expect("owned file");
    tokio::fs::write(repo.join("generated/out.json"), generated_bytes)
        .await
        .expect("generated artifact");

    let thread_id = ThreadId::new();
    let evidence_path = home.join("task-evidence").join(format!("{thread_id}.json"));
    tokio::fs::create_dir_all(evidence_path.parent().expect("evidence parent"))
        .await
        .expect("evidence directory");
    let mut v4 = legacy_task_evidence_fixture(
        FROZEN_TASK_EVIDENCE_V4_SCHEMA_VERSION,
        &thread_id.to_string(),
        &repo,
        "passed",
    );
    v4["revision"] = serde_json::json!(41);
    v4["evidence_epoch"] = serde_json::json!(7);
    v4["host_mutation_revision"] = serde_json::json!(3);
    v4["last_mutation_at"] = serde_json::json!("2026-07-31T23:59:00Z");
    v4["plan"] = serde_json::json!([
        {
            "id": "prepare",
            "step": "prepare the owned contract",
            "status": "passed",
            "depends_on": [],
            "acceptance_criteria": ["owned behavior is present"],
            "runtime_paths": ["core/runtime/prepare"],
            "generated_artifacts": ["generated/out.json"],
            "risks": ["preserve the v4 contract"],
            "edit_paths": ["src/owned.rs"]
        },
        {
            "id": "verify",
            "step": "verify the migrated contract",
            "status": "passed",
            "depends_on": ["prepare"],
            "acceptance_criteria": ["proof remains attributable"],
            "runtime_paths": ["core/runtime/verify"],
            "generated_artifacts": [],
            "risks": [],
            "edit_paths": []
        }
    ]);
    v4["active_step_id"] = Value::Null;
    v4["edit_intents"] = serde_json::json!([{
        "call_id": "edit-call",
        "step_id": "prepare",
        "started_at": "2026-07-31T23:58:00Z",
        "completed_at": "2026-07-31T23:58:01Z",
        "outcome": "success",
        "files": [{
            "path": "src/owned.rs",
            "sha1": null,
            "exists": false,
            "read_error": null
        }]
    }]);
    v4["edit_receipts"] = serde_json::json!([{
        "id": "edit-1",
        "call_id": "edit-call",
        "step_id": "prepare",
        "recorded_at": "2026-07-31T23:58:01Z",
        "epoch": 7,
        "outcome": "success",
        "files": [{
            "path": "src/owned.rs",
            "before_sha1": null,
            "after_sha1": sha1_hex(owned_bytes),
            "before_exists": false,
            "after_exists": true,
            "before_read_error": null,
            "after_read_error": null
        }]
    }]);
    v4["command_receipts"] = serde_json::json!([{
        "id": "command-1",
        "recorded_at": "2026-07-31T23:58:02Z",
        "epoch": 7,
        "step_id": "verify",
        "command": ["cargo", "test", "focused"],
        "cwd": repo.to_string_lossy(),
        "exit_code": 0,
        "timed_out": false,
        "duration_ms": 12,
        "possible_mutation": false
    }]);
    v4["completion_review_receipts"] = serde_json::json!([{
        "turn_id": "legacy-review-turn",
        "recorded_at": "2026-07-31T23:58:03Z",
        "evidence_epoch": 7,
        "outcome": "clean",
        "failure_category": null,
        "finding_summary": [],
        "repair_injected": false
    }]);
    v4["generated_artifact_requirements"] = serde_json::json!([{
        "id": "prepare:generated:0",
        "step_id": "prepare",
        "path": "generated/out.json"
    }]);
    v4["latest_generated_artifact_hashes"] = serde_json::json!({
        "generated/out.json": {
            "path": "generated/out.json",
            "sha1": sha1_hex(generated_bytes),
            "exists": true,
            "read_error": null
        }
    });
    v4["latest_file_hashes"] = serde_json::json!({
        "src/owned.rs": {
            "path": "src/owned.rs",
            "sha1": sha1_hex(owned_bytes),
            "exists": true,
            "read_error": null
        }
    });
    v4["risks"] = serde_json::json!([{
        "id": "legacy-observed-risk",
        "description": "legacy risk was explicitly resolved",
        "source": "command",
        "blocking": false,
        "resolved": true,
        "epoch": 7
    }]);
    v4["next_edit_receipt_sequence"] = serde_json::json!(2);
    v4["next_command_receipt_sequence"] = serde_json::json!(2);
    v4["next_external_evidence_receipt_sequence"] = serde_json::json!(1);
    v4["completion"] = serde_json::json!({
        "status": "passed",
        "reasons": [],
        "evidence_path": evidence_path.to_string_lossy()
    });
    tokio::fs::write(
        &evidence_path,
        serde_json::to_vec_pretty(&v4).expect("serialize rich v4 fixture"),
    )
    .await
    .expect("write rich v4 fixture");

    let migrated = TaskEvidenceLedger::load_or_new(home.clone(), thread_id, &repo).await;
    {
        let guard = migrated.document.lock().await;
        let document = guard.as_ref().expect("migrated document");
        assert_eq!(document.schema_version, TASK_EVIDENCE_SCHEMA_VERSION);
        assert_eq!(document.evidence_epoch, 7);
        assert_eq!(document.host_mutation_revision, 3);
        assert_eq!(
            document.last_mutation_at.as_deref(),
            Some("2026-07-31T23:59:00Z")
        );
        assert_eq!(document.plan.len(), 2);
        assert_eq!(document.plan[1].depends_on, ["prepare"]);
        assert_eq!(
            document.plan[0].acceptance_criteria,
            ["owned behavior is present"]
        );
        assert_eq!(document.plan[0].runtime_paths, ["core/runtime/prepare"]);
        assert_eq!(document.plan[0].generated_artifacts, ["generated/out.json"]);
        assert_eq!(document.plan[0].risks, ["preserve the v4 contract"]);
        assert!(document.risks.iter().all(|risk| risk.source != "plan"));
        assert!(effective_risks(document).any(|risk| {
            risk.source == "plan" && risk.description == "preserve the v4 contract"
        }));
        let persisted = serde_json::to_value(document).expect("serialize migrated evidence");
        assert!(persisted.get("generated_artifact_requirements").is_none());
        assert!(
            persisted["risks"]
                .as_array()
                .expect("persisted risks")
                .iter()
                .all(|risk| risk["source"] != "plan")
        );
        assert_eq!(document.edit_intents.len(), 1);
        assert_eq!(document.edit_receipts.len(), 1);
        assert_eq!(document.command_receipts.len(), 1);
        assert!(
            document.command_receipts[0]
                .implementation_identity_hash
                .is_none()
        );
        assert_eq!(document.completion_review_receipts.len(), 1);
        let owned_hash = sha1_hex(owned_bytes);
        assert_eq!(
            document.latest_file_hashes["src/owned.rs"].sha1.as_deref(),
            Some(owned_hash.as_str())
        );
        let generated_hash = sha1_hex(generated_bytes);
        assert_eq!(
            document.latest_generated_artifact_hashes["generated/out.json"]
                .sha1
                .as_deref(),
            Some(generated_hash.as_str())
        );
        assert!(
            document
                .risks
                .iter()
                .any(|risk| { risk.id == "legacy-observed-risk" && risk.resolved })
        );
        assert_eq!(
            document.completion.as_ref().map(|gate| gate.status),
            Some(TaskCompletionStatus::Passed)
        );
        let review = document.completion_review_v2.as_ref().expect("V2 ledger");
        assert_eq!(review.receipts.len(), 2);
        assert_eq!(next_review_sequence(review), 3);
        assert_eq!(
            review.receipts[0].attempt_kind,
            CompletionReviewAttemptKind::InitialReview
        );
        assert_eq!(
            review.receipts[1].attempt_kind,
            CompletionReviewAttemptKind::TerminalClosure
        );
        assert_eq!(
            review.receipts[1].parent_review_id.as_deref(),
            Some(review.receipts[0].review_id.as_str())
        );
        assert_eq!(
            review.active_review_cycle.as_ref().map(|cycle| cycle.phase),
            Some(CompletionReviewCyclePhase::Closed)
        );
        assert!(!review.review_risk.unresolved);
        assert_eq!(
            latest_terminal_closure(review).map(|receipt| receipt.review_id.as_str()),
            Some(review.receipts[1].review_id.as_str())
        );
    }
    drop(migrated);

    match load_existing_document(&evidence_path, &thread_id.to_string(), &repo).await {
        ExistingDocument::Loaded { .. } => {}
        ExistingDocument::Rejected { kind, reason } => {
            panic!("migrated v5 evidence was rejected as {kind}: {reason}")
        }
        ExistingDocument::NewerSchema { schema_version } => {
            panic!("migrated v5 evidence was treated as schema {schema_version}")
        }
        ExistingDocument::Missing => panic!("migrated v5 evidence disappeared"),
    }
    let reloaded = TaskEvidenceLedger::load_or_new(home, thread_id, &repo).await;
    let guard = reloaded.document.lock().await;
    let document = guard.as_ref().expect("reloaded migrated document");
    assert_eq!(document.schema_version, TASK_EVIDENCE_SCHEMA_VERSION);
    assert_eq!(document.plan.len(), 2);
    assert_eq!(
        document
            .completion_review_v2
            .as_ref()
            .expect("reloaded V2 ledger")
            .receipts
            .len(),
        2
    );
    assert_eq!(
        document.completion.as_ref().map(|gate| gate.status),
        Some(TaskCompletionStatus::Passed)
    );
}

#[tokio::test]
async fn newer_schema_payload_disables_ledger_without_modifying_the_file() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    ledger
        .record_plan_update(&plan_with(vec![plan_item("step", StepStatus::Passed)]))
        .await;
    let home = ledger.codex_home.as_ref().expect("codex home").clone();
    let evidence_path = ledger.evidence_path().expect("evidence path");
    let thread_id = ledger.thread_id.as_deref().expect("thread id").to_string();
    let document = ledger
        .document
        .lock()
        .await
        .as_ref()
        .expect("document")
        .clone();
    let mut legacy = serde_json::to_value(document).expect("serialize");
    let newer_schema_version = TASK_EVIDENCE_SCHEMA_VERSION + 1;
    legacy["schema_version"] = serde_json::json!(newer_schema_version);
    legacy["lifecycle"] = serde_json::json!({
        "phase": "ready",
        "outcome": "passed",
        "mutation_revision": 1,
        "accepted_evidence_revision": 1
    });
    let legacy_bytes = serde_json::to_vec_pretty(&legacy).expect("serialize newer evidence");
    tokio::fs::write(&evidence_path, &legacy_bytes)
        .await
        .expect("write v7 evidence");
    drop(ledger);

    assert!(matches!(
        load_existing_document(&evidence_path, &thread_id, &repo).await,
        ExistingDocument::NewerSchema { schema_version }
            if schema_version == u64::from(newer_schema_version)
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
            .expect("untouched newer evidence"),
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
        .record_plan_update(&plan_with(vec![proof_free_plan_item(
            "step",
            StepStatus::Passed,
        )]))
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
    let (_temp, _repo, ledger) = ledger_fixture().await;
    *ledger
        .evidence_path
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
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
    assert_eq!(gate.status, TaskCompletionStatus::Partial);
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

async fn install_tracked_freshness_file(
    ledger: &TaskEvidenceLedger,
    repo: &Path,
    path: &str,
    bytes: &[u8],
) -> TrustedFileToken {
    let absolute = repo.join(path);
    if let Some(parent) = absolute.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .expect("tracked file parent");
    }
    tokio::fs::write(&absolute, bytes)
        .await
        .expect("tracked freshness file");
    let snapshot = snapshot_file(repo, path).await;
    ledger
        .document
        .lock()
        .await
        .as_mut()
        .expect("document")
        .latest_file_hashes
        .insert(path.to_string(), snapshot);
    let file = tokio::fs::File::open(absolute)
        .await
        .expect("open tracked freshness file");
    trusted_file_token(&file)
        .await
        .expect("test filesystem exposes a trusted freshness token")
}

#[tokio::test]
async fn ordinary_freshness_refresh_reuses_unchanged_strong_hashes() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    let bytes = b"unchanged freshness\n";
    install_tracked_freshness_file(&ledger, &repo, "src/fresh.txt", bytes).await;

    for _ in 0..3 {
        ledger.refresh_external_file_freshness().await;
    }

    assert_eq!(
        ledger.freshness_diagnostics(),
        FreshnessDiagnostics {
            scan_invocations: 3,
            files_strongly_hashed: 1,
            bytes_strongly_hashed: bytes.len() as u64,
            strong_hashes_reused: 2,
            conservative_reruns: 0,
        }
    );
}

#[tokio::test]
async fn trusted_token_change_forces_ordinary_strong_rehash() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    let before = install_tracked_freshness_file(&ledger, &repo, "src/fresh.txt", b"one").await;
    ledger.refresh_external_file_freshness().await;

    tokio::fs::write(repo.join("src/fresh.txt"), b"two-with-a-different-length")
        .await
        .expect("mutate tracked file");
    let file = tokio::fs::File::open(repo.join("src/fresh.txt"))
        .await
        .expect("open mutated file");
    let after = trusted_file_token(&file)
        .await
        .expect("trusted token after mutation");
    assert_ne!(
        before, after,
        "the content mutation must alter the trusted token"
    );

    ledger.refresh_external_file_freshness().await;

    let diagnostics = ledger.freshness_diagnostics();
    assert_eq!(diagnostics.scan_invocations, 2);
    assert_eq!(diagnostics.files_strongly_hashed, 2);
    assert_eq!(diagnostics.strong_hashes_reused, 0);
}

#[tokio::test]
async fn unavailable_trusted_tokens_remain_strong_hash_only() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    let bytes = b"fail closed\n";
    install_tracked_freshness_file(&ledger, &repo, "src/fresh.txt", bytes).await;
    ledger.set_force_untrusted_freshness_tokens(true);

    for _ in 0..3 {
        ledger.refresh_external_file_freshness().await;
    }

    assert_eq!(
        ledger.freshness_diagnostics(),
        FreshnessDiagnostics {
            scan_invocations: 3,
            files_strongly_hashed: 3,
            bytes_strongly_hashed: 3 * bytes.len() as u64,
            strong_hashes_reused: 0,
            conservative_reruns: 0,
        }
    );
}

#[tokio::test]
async fn ambiguous_trusted_tokens_remain_strong_hash_only() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    let bytes = b"unstable identity\n";
    install_tracked_freshness_file(&ledger, &repo, "src/fresh.txt", bytes).await;
    ledger.set_force_ambiguous_freshness_tokens(true);

    for _ in 0..3 {
        ledger.refresh_external_file_freshness().await;
    }

    assert_eq!(
        ledger.freshness_diagnostics(),
        FreshnessDiagnostics {
            scan_invocations: 3,
            files_strongly_hashed: 3,
            bytes_strongly_hashed: 3 * bytes.len() as u64,
            strong_hashes_reused: 0,
            conservative_reruns: 0,
        }
    );
}

#[tokio::test]
async fn tracked_and_artifact_lexical_aliases_hash_once_and_keep_associations() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    let tracked_path = "target/out.txt";
    let artifact_path = "target/./out.txt";
    install_tracked_freshness_file(&ledger, &repo, tracked_path, b"shared output\n").await;
    let mut artifact = proof_free_plan_item("artifact", StepStatus::Pending);
    artifact.generated_artifacts = vec![artifact_path.to_string()];
    ledger.record_plan_update(&plan_with(vec![artifact])).await;
    let artifact_snapshot = snapshot_file(&repo, artifact_path).await;
    {
        let mut guard = ledger.document.lock().await;
        let document = guard.as_mut().expect("document");
        document
            .latest_generated_artifact_hashes
            .insert(artifact_path.to_string(), artifact_snapshot);
    }

    ledger.refresh_external_file_freshness().await;

    let diagnostics = ledger.freshness_diagnostics();
    assert_eq!(diagnostics.scan_invocations, 1);
    assert_eq!(diagnostics.files_strongly_hashed, 1);
    let guard = ledger.document.lock().await;
    let document = guard.as_ref().expect("document");
    assert!(document.latest_file_hashes.contains_key(tracked_path));
    assert!(
        document
            .latest_generated_artifact_hashes
            .contains_key(artifact_path)
    );
}

#[tokio::test]
async fn completion_proof_cycle_starts_with_fresh_hash_then_reuses_retry_proof() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    install_tracked_freshness_file(&ledger, &repo, "src/fresh.txt", b"proof\n").await;

    ledger.refresh_external_file_freshness().await;
    ledger
        .refresh_external_file_freshness_for(FreshnessPurpose::CompletionFresh)
        .await;
    ledger
        .refresh_external_file_freshness_for(FreshnessPurpose::CompletionRetry)
        .await;
    ledger
        .refresh_external_file_freshness_for(FreshnessPurpose::CompletionRetry)
        .await;

    let diagnostics = ledger.freshness_diagnostics();
    assert_eq!(diagnostics.scan_invocations, 2);
    assert_eq!(diagnostics.files_strongly_hashed, 2);
    assert_eq!(diagnostics.strong_hashes_reused, 2);
}

#[tokio::test]
async fn requirement_manifest_change_starts_a_new_completion_proof_scan() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    install_tracked_freshness_file(&ledger, &repo, "src/fresh.txt", b"tracked\n").await;
    tokio::fs::write(repo.join("artifact.txt"), b"artifact\n")
        .await
        .expect("artifact");
    ledger
        .refresh_external_file_freshness_for(FreshnessPurpose::CompletionFresh)
        .await;
    let mut artifact = proof_free_plan_item("artifact", StepStatus::Pending);
    artifact.generated_artifacts = vec!["artifact.txt".to_string()];
    ledger.record_plan_update(&plan_with(vec![artifact])).await;
    let artifact_snapshot = snapshot_file(&repo, "artifact.txt").await;
    {
        let mut guard = ledger.document.lock().await;
        let document = guard.as_mut().expect("document");
        document
            .latest_generated_artifact_hashes
            .insert("artifact.txt".to_string(), artifact_snapshot);
    }

    ledger
        .refresh_external_file_freshness_for(FreshnessPurpose::CompletionRetry)
        .await;

    let diagnostics = ledger.freshness_diagnostics();
    assert_eq!(diagnostics.scan_invocations, 2);
    assert_eq!(diagnostics.files_strongly_hashed, 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_evidence_change_discards_scan_candidate_and_retries() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    install_tracked_freshness_file(&ledger, &repo, "src/fresh.txt", b"candidate\n").await;
    let ledger = Arc::new(ledger);
    let (started, release) = ledger.install_freshness_scan_barrier();
    let refresh_ledger = Arc::clone(&ledger);
    let refresh = tokio::spawn(async move {
        refresh_ledger.refresh_external_file_freshness().await;
    });

    started.wait().await;
    {
        let mut guard = ledger.document.lock().await;
        let document = guard.as_mut().expect("document");
        document.host_mutation_revision = document.host_mutation_revision.saturating_add(1);
        document.revision = document.revision.saturating_add(1);
    }
    release.wait().await;
    refresh.await.expect("freshness refresh");

    let diagnostics = ledger.freshness_diagnostics();
    assert_eq!(diagnostics.scan_invocations, 2);
    assert_eq!(diagnostics.files_strongly_hashed, 2);
    assert_eq!(diagnostics.strong_hashes_reused, 0);
}

#[tokio::test]
async fn exact_patch_lexical_aliases_are_deduplicated_before_snapshots() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    tokio::fs::create_dir_all(repo.join("src/nested"))
        .await
        .expect("source tree");
    tokio::fs::write(repo.join("src/file.rs"), "fn main() {}\n")
        .await
        .expect("source file");

    ledger
        .record_edit_intent(
            "patch",
            &repo,
            &[
                PathBuf::from("src/file.rs"),
                PathBuf::from("src/./file.rs"),
                PathBuf::from("src/nested/../file.rs"),
            ],
        )
        .await;

    let guard = ledger.document.lock().await;
    let intent = guard
        .as_ref()
        .expect("document")
        .edit_intents
        .last()
        .expect("edit intent");
    assert_eq!(intent.files.len(), 1);
    assert_eq!(intent.files[0].path, "src/file.rs");
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
        supersede_writes: Arc::new(AtomicU64::new(0)),
    });
    (started, release)
}

fn set_persistence_test_superseded_writes(ledger: &TaskEvidenceLedger, count: u64) {
    let guard = ledger
        .persistence_test_control
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard
        .as_ref()
        .expect("persistence test control")
        .supersede_writes
        .store(count, Ordering::Release);
}

fn install_persistence_supersede_control(ledger: &TaskEvidenceLedger, count: u64) {
    *ledger
        .persistence_test_control
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(PersistenceTestControl {
        before_next_write: Arc::new(std::sync::Mutex::new(None)),
        fail_writes: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        supersede_writes: Arc::new(AtomicU64::new(count)),
    });
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

fn terminal_decision_claim(terminal_identity: &str) -> TerminalDecisionClaim {
    let turn_id = terminal_identity
        .rsplit_once(':')
        .map(|(_, turn_id)| turn_id)
        .unwrap_or(terminal_identity)
        .to_string();
    let event = EventMsg::TurnComplete(TurnCompleteEvent {
        turn_id: turn_id.clone(),
        last_agent_message: Some("done".to_string()),
        surfaced_result: None,
        error: None,
        completion: None,
        completed_at: None,
        duration_ms: None,
        time_to_first_token_ms: None,
        timing: None,
    });
    TerminalDecisionClaim {
        authoritative_event: AuthoritativeTerminalEventV1 {
            version: 1,
            terminal_identity: terminal_identity.to_string(),
            turn_id,
            fingerprint: crate::terminal_event_fingerprint(&event).expect("terminal event"),
            event,
            semantic_outcome: "passed".to_string(),
            final_proof_identity: None,
            rollout_repair: TerminalRolloutRepairV1::default(),
        },
        deadline_exhausted_phase: None,
        mutation_quiescent: true,
        durable_success_established: true,
        retained_ownership: Vec::new(),
        phase_timings_ns: BTreeMap::from([("gate".to_string(), 17)]),
    }
}

#[test]
fn completion_review_dimensions_keep_skips_outcomes_and_findings_distinct() {
    assert_eq!(
        CompletionReviewRequirement::from_obligation_mode("disabled"),
        CompletionReviewRequirement::Disabled
    );
    assert_eq!(
        CompletionReviewRequirement::from_obligation_mode("supplemental"),
        CompletionReviewRequirement::Supplemental
    );
    assert_eq!(
        CompletionReviewRequirement::from_obligation_mode("mandatory"),
        CompletionReviewRequirement::Mandatory
    );

    for infrastructure in [
        "capacity",
        "spawn_model",
        "oversized_request",
        "persistence",
        "input_unavailable_or_truncated",
        "user_source_drift",
        "repeated_or_invalid_manifest_gap",
        "invalid_or_incomplete_dossier",
        "unsupported_reviewer_configuration",
        "self_review_prohibited",
        "candidate_changed",
    ] {
        assert_eq!(
            completion_review_attempt_dimensions(
                CompletionReviewAttemptKind::InitialReview,
                infrastructure,
                false,
                false,
            ),
            (CompletionReviewDisposition::PreflightSkipped, None)
        );
    }
    assert_eq!(
        completion_review_attempt_dimensions(
            CompletionReviewAttemptKind::InitialReview,
            "timeout",
            false,
            false,
        ),
        (
            CompletionReviewDisposition::Attempted,
            Some(CompletionReviewAttemptedOutcome::InfrastructureFailure),
        )
    );
    assert_eq!(
        completion_review_attempt_dimensions(
            CompletionReviewAttemptKind::InitialReview,
            "ok",
            true,
            false,
        ),
        (
            CompletionReviewDisposition::Attempted,
            Some(CompletionReviewAttemptedOutcome::Clean),
        )
    );
    assert_eq!(
        completion_review_attempt_dimensions(
            CompletionReviewAttemptKind::InitialReview,
            "ok",
            false,
            true,
        ),
        (
            CompletionReviewDisposition::Attempted,
            Some(CompletionReviewAttemptedOutcome::ActionableFindings),
        )
    );
    for attempt_kind in [
        CompletionReviewAttemptKind::CorrectionEvidence,
        CompletionReviewAttemptKind::TerminalClosure,
    ] {
        assert_eq!(
            completion_review_attempt_dimensions(attempt_kind, "ok", true, false),
            (CompletionReviewDisposition::NotApplicable, None)
        );
    }
}

#[tokio::test]
async fn missing_mandatory_completion_review_proof_overlays_but_supplemental_review_does_not() {
    let (_temp, _repo, ledger, dossier) = classified_requirement_fixture().await;
    let requirement_id = dossier.requirements[0].requirement_id.clone();

    assert!(matches!(
        ledger
            .synchronize_completion_review_obligation(CompletionReviewObligationInput {
                mode: "supplemental".to_string(),
                requirement_ids: Vec::new(),
                obligation_hash: "supplemental-obligation".to_string(),
                required_attempt_identity: None,
            })
            .await,
        AtomicReviewTransition::Persisted(())
    ));
    assert_eq!(
        ledger
            .completion_gate()
            .await
            .expect("supplemental gate")
            .status,
        TaskCompletionStatus::Passed,
        "missing optional review proof must not worsen ordinary completion"
    );

    assert!(matches!(
        ledger
            .synchronize_completion_review_obligation(CompletionReviewObligationInput {
                mode: "mandatory".to_string(),
                requirement_ids: vec![requirement_id],
                obligation_hash: "mandatory-obligation".to_string(),
                required_attempt_identity: Some("required-attempt".to_string()),
            })
            .await,
        AtomicReviewTransition::Persisted(())
    ));
    let mandatory_gate = ledger.completion_gate().await.expect("mandatory gate");
    assert_eq!(mandatory_gate.status, TaskCompletionStatus::Partial);
    assert!(
        mandatory_gate
            .reasons
            .iter()
            .any(|reason| { reason.contains("mandatory completion-review proof is missing") })
    );
}

#[tokio::test]
async fn terminal_decision_and_delivery_claim_are_atomic_and_one_shot() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    let identity = "thread:turn";
    let terminalization = codex_protocol::protocol::TurnTimingTerminalization {
        post_cleanup_ns: 17,
        ..Default::default()
    };

    assert!(matches!(
        ledger
            .commit_terminal_decision_and_claim(terminal_decision_claim(identity))
            .await,
        TerminalClaimResult::Claimed(_)
    ));
    assert!(matches!(
        ledger
            .commit_terminal_decision_and_claim(terminal_decision_claim(identity))
            .await,
        TerminalClaimResult::AlreadyClaimed(_)
    ));
    assert_eq!(
        ledger.terminalization_receipts_for_test().await,
        vec![(
            identity.to_string(),
            TerminalDeliveryState::Claimed,
            false,
            false,
            TerminalRecoveryState::Pending,
        )]
    );

    assert!(
        ledger
            .update_terminal_interaction(TerminalInteractionUpdate {
                terminal_identity: identity.to_string(),
                delivery_state: TerminalDeliveryState::Delivered,
                app_server_acknowledged: true,
                runtime_status_converged: true,
                rollout_mirrored: true,
                parent_notification_completed: true,
                post_terminal_cleanup_completed: true,
                active_turn_detached: true,
                terminal_interaction_released: true,
                recovery_state: TerminalRecoveryState::None,
                phase_timings_ns: BTreeMap::from([("delivery_attempt".to_string(), 3)]),
                terminalization: Some(terminalization.clone()),
            })
            .await
    );
    assert_eq!(
        ledger.terminalization_receipts_for_test().await,
        vec![(
            identity.to_string(),
            TerminalDeliveryState::Delivered,
            true,
            true,
            TerminalRecoveryState::None,
        )]
    );
    assert_eq!(
        ledger.terminal_timing_receipt_for_test(identity).await,
        Some(terminalization)
    );
}

#[tokio::test]
async fn terminalization_recovery_is_monotonic_in_task_evidence() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    let identity = "thread:recovered-turn";
    let terminalization = codex_protocol::protocol::TurnTimingTerminalization {
        post_cleanup_ns: 17,
        ..Default::default()
    };

    assert!(matches!(
        ledger
            .commit_terminal_decision_and_claim(terminal_decision_claim(identity))
            .await,
        TerminalClaimResult::Claimed(_)
    ));
    assert!(
        ledger
            .update_terminal_interaction(TerminalInteractionUpdate {
                terminal_identity: identity.to_string(),
                delivery_state: TerminalDeliveryState::Delivered,
                app_server_acknowledged: false,
                runtime_status_converged: true,
                rollout_mirrored: true,
                parent_notification_completed: true,
                post_terminal_cleanup_completed: true,
                active_turn_detached: true,
                terminal_interaction_released: true,
                recovery_state: TerminalRecoveryState::Recovered,
                phase_timings_ns: BTreeMap::new(),
                terminalization: Some(terminalization.clone()),
            })
            .await
    );

    // A late cleanup update may add evidence, but must not erase completed recovery.
    assert!(
        ledger
            .update_terminal_interaction(TerminalInteractionUpdate {
                terminal_identity: identity.to_string(),
                delivery_state: TerminalDeliveryState::Claimed,
                app_server_acknowledged: true,
                runtime_status_converged: true,
                rollout_mirrored: true,
                parent_notification_completed: true,
                post_terminal_cleanup_completed: true,
                active_turn_detached: true,
                terminal_interaction_released: true,
                recovery_state: TerminalRecoveryState::None,
                phase_timings_ns: BTreeMap::new(),
                terminalization: None,
            })
            .await
    );

    let snapshot = ledger
        .terminalization_receipt_snapshot(identity)
        .await
        .expect("terminalization receipt snapshot");
    assert_eq!(snapshot.delivery_state, TerminalDeliveryState::Delivered);
    assert_eq!(snapshot.recovery_state, TerminalRecoveryState::Recovered);
    assert_eq!(snapshot.terminalization, terminalization);
    assert!(snapshot.active_turn_detached);
    assert!(snapshot.terminal_interaction_released);
}

#[tokio::test]
async fn conflicting_terminal_candidate_cannot_replace_authoritative_event() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    let identity = "thread:turn";
    let first = terminal_decision_claim(identity);
    let first_fingerprint = first.authoritative_event.fingerprint.clone();
    assert!(matches!(
        ledger.commit_terminal_decision_and_claim(first).await,
        TerminalClaimResult::Claimed(_)
    ));

    let mut conflicting = terminal_decision_claim(identity);
    let EventMsg::TurnComplete(event) = &mut conflicting.authoritative_event.event else {
        unreachable!("test claim is terminal completion");
    };
    event.last_agent_message = Some("conflicting outcome".to_string());
    conflicting.authoritative_event.fingerprint =
        crate::terminal_event_fingerprint(&conflicting.authoritative_event.event)
            .expect("terminal fingerprint");
    let result = ledger.commit_terminal_decision_and_claim(conflicting).await;
    let TerminalClaimResult::Conflict {
        authoritative: Some(authoritative),
        ..
    } = result
    else {
        panic!("conflicting terminal event must be rejected");
    };
    assert_eq!(authoritative.fingerprint, first_fingerprint);
    assert_eq!(
        ledger
            .authoritative_terminal_event(identity)
            .await
            .expect("authoritative event")
            .fingerprint,
        first_fingerprint
    );
}

#[tokio::test]
async fn terminal_receipt_rejects_mismatched_durable_outcome() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    let identity = "thread:mismatched-outcome";
    let claim = terminal_decision_claim(identity);
    let fingerprint = claim.authoritative_event.fingerprint.clone();
    assert!(matches!(
        ledger.commit_terminal_decision_and_claim(claim).await,
        TerminalClaimResult::Claimed(_)
    ));
    {
        let mut guard = ledger.document.lock().await;
        let receipt = guard
            .as_mut()
            .expect("document")
            .terminalization_receipts
            .iter_mut()
            .find(|receipt| receipt.terminal_identity == identity)
            .expect("terminal receipt");
        receipt.durable_outcome = "contradictory".to_string();
    }

    assert!(
        ledger
            .authoritative_terminal_event(identity)
            .await
            .is_none()
    );
    assert!(
        ledger
            .pending_authoritative_terminal_events()
            .await
            .is_empty()
    );
    assert!(
        !ledger
            .acknowledge_terminal_event(identity, &fingerprint)
            .await
    );
}

#[tokio::test]
async fn terminal_claim_persistence_failure_establishes_no_durable_success() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    install_persistence_supersede_control(&ledger, 0);
    set_persistence_test_failure(&ledger, true);

    assert!(matches!(
        ledger
            .commit_terminal_decision_and_claim(terminal_decision_claim("failed:turn"))
            .await,
        TerminalClaimResult::Failed
    ));
    assert!(ledger.terminalization_receipts_for_test().await.is_empty());
}

#[tokio::test]
async fn late_terminalization_persistence_failure_does_not_change_committed_claim() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    install_persistence_supersede_control(&ledger, 0);
    let identity = "committed:turn";
    assert!(matches!(
        ledger
            .commit_terminal_decision_and_claim(terminal_decision_claim(identity))
            .await,
        TerminalClaimResult::Claimed(_)
    ));

    set_persistence_test_failure(&ledger, true);
    assert!(
        !ledger
            .update_terminal_interaction(TerminalInteractionUpdate {
                terminal_identity: identity.to_string(),
                delivery_state: TerminalDeliveryState::Delivered,
                app_server_acknowledged: false,
                runtime_status_converged: true,
                rollout_mirrored: false,
                parent_notification_completed: false,
                post_terminal_cleanup_completed: false,
                active_turn_detached: true,
                terminal_interaction_released: true,
                recovery_state: TerminalRecoveryState::None,
                phase_timings_ns: BTreeMap::new(),
                terminalization: Some(Default::default()),
            })
            .await
    );
    assert_eq!(
        ledger.terminalization_receipts_for_test().await,
        vec![(
            identity.to_string(),
            TerminalDeliveryState::Claimed,
            false,
            false,
            TerminalRecoveryState::Pending,
        )]
    );
    assert_eq!(
        ledger.terminal_timing_receipt_for_test(identity).await,
        None
    );
}

#[tokio::test]
async fn claimed_terminal_receipt_remains_pending_for_exact_replay() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    let codex_home = temp.path().join("home");
    tokio::fs::create_dir_all(repo.join(".git"))
        .await
        .expect("git dir");
    tokio::fs::write(repo.join("kd4_features.toml"), "# fixture")
        .await
        .expect("manifest");
    let thread_id = ThreadId::new();
    let identity = format!("{thread_id}:turn");
    let ledger = TaskEvidenceLedger::load_or_new(codex_home.clone(), thread_id, &repo).await;
    assert!(matches!(
        ledger
            .commit_terminal_decision_and_claim(terminal_decision_claim(&identity))
            .await,
        TerminalClaimResult::Claimed(_)
    ));
    drop(ledger);

    let recovered = TaskEvidenceLedger::load_or_new(codex_home, thread_id, &repo).await;
    assert_eq!(
        recovered.terminalization_receipts_for_test().await,
        vec![(
            identity,
            TerminalDeliveryState::Claimed,
            false,
            false,
            TerminalRecoveryState::Pending,
        )]
    );
}

#[test]
fn workspace_scope_expansion_reclassifies_a_retained_unrelated_event() {
    let event = TaskAttributedWorkspaceEvent {
        workspace_id: "workspace".to_string(),
        epoch: 7,
        actor_id: "root:session".to_string(),
        paths: vec!["docs/contract.md".to_string()],
        contracts: Vec::new(),
        actor_kind: Some(codex_agent_task_store::WorkspaceActorKind::Root),
        attribution_confidence: Some(codex_agent_task_store::AttributionConfidence::Definitive),
        relevance: WorkspaceEventRelevance::Unknown,
        classified_scope_identity: String::new(),
    };
    let original_scope = WorkspaceProofScope {
        identity: "scope-a".to_string(),
        paths: BTreeSet::from(["src/lib.rs".to_string()]),
        contracts: BTreeSet::new(),
    };
    assert_eq!(
        classify_workspace_event(&event, &original_scope),
        WorkspaceEventRelevance::Unrelated
    );

    let expanded_scope = WorkspaceProofScope {
        identity: "scope-b".to_string(),
        paths: BTreeSet::from(["src/lib.rs".to_string(), "docs/contract.md".to_string()]),
        contracts: BTreeSet::new(),
    };
    assert_eq!(
        classify_workspace_event(&event, &expanded_scope),
        WorkspaceEventRelevance::Relevant
    );
}

#[tokio::test]
async fn initial_workspace_event_baseline_seeds_zero_completion_epoch() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    {
        let mut guard = ledger.document.lock().await;
        let workspace = guard
            .as_mut()
            .and_then(|document| document.completion_review_v2.as_mut())
            .expect("completion review workspace ledger");
        workspace.completion_epoch = 0;
        workspace.workspace_event_baseline_epoch = 0;
        workspace.workspace_event_history_complete = false;
    }

    assert!(
        ledger
            .seed_workspace_event_baseline(7, BTreeSet::new())
            .await
    );

    let guard = ledger.document.lock().await;
    let workspace = guard
        .as_ref()
        .and_then(|document| document.completion_review_v2.as_ref())
        .expect("completion review workspace ledger");
    assert_eq!(workspace.completion_epoch, 0);
    assert_eq!(workspace.workspace_event_baseline_epoch, 0);
    assert_eq!(workspace.last_workspace_event_epoch, 7);
    assert!(workspace.workspace_event_history_complete);
}

#[test]
fn workspace_scope_change_with_incomplete_history_invalidates_conservatively() {
    assert!(workspace_scope_history_is_unknown(true, false));
    assert!(!workspace_scope_history_is_unknown(false, false));
    assert!(!workspace_scope_history_is_unknown(true, true));

    let repository_wide = TaskAttributedWorkspaceEvent {
        workspace_id: "workspace".to_string(),
        epoch: 9,
        actor_id: "root:session".to_string(),
        paths: vec![codex_agent_task_store::REPOSITORY_WIDE_PATH.to_string()],
        contracts: Vec::new(),
        actor_kind: Some(codex_agent_task_store::WorkspaceActorKind::Root),
        attribution_confidence: Some(codex_agent_task_store::AttributionConfidence::Definitive),
        relevance: WorkspaceEventRelevance::Unknown,
        classified_scope_identity: String::new(),
    };
    let disjoint_scope = WorkspaceProofScope {
        identity: "scope".to_string(),
        paths: BTreeSet::from(["src/lib.rs".to_string()]),
        contracts: BTreeSet::new(),
    };
    assert_eq!(
        classify_workspace_event(&repository_wide, &disjoint_scope),
        WorkspaceEventRelevance::Unknown,
        "repository-wide facts stay unknown without proof of the actor's complete disjoint scope"
    );
}

#[test]
fn typed_workspace_actor_requires_exact_same_root_definitive_identity() {
    use codex_agent_task_store::AttributionConfidence;
    use codex_agent_task_store::WorkspaceActorKind;

    let proven_attempt_id = "attempt:same-root".to_string();
    let admitted = BTreeSet::from([proven_attempt_id.clone()]);
    let event = codex_agent_task_store::WorkspaceEvent {
        workspace_id: "workspace".to_string(),
        epoch: 8,
        actor_id: Some(proven_attempt_id),
        actor_kind: WorkspaceActorKind::Typed,
        attribution_confidence: AttributionConfidence::Definitive,
        paths: vec!["src/lib.rs".to_string()],
        contracts: Vec::new(),
        created_at: chrono::Utc::now(),
    };

    assert!(workspace_event_actor_is_admitted(
        &event,
        "root:session",
        "legacy:session:",
        &admitted,
    ));

    let unmatched = codex_agent_task_store::WorkspaceEvent {
        actor_id: Some("attempt:other-root".to_string()),
        ..event.clone()
    };
    assert!(!workspace_event_actor_is_admitted(
        &unmatched,
        "root:session",
        "legacy:session:",
        &admitted,
    ));

    let detection_only = codex_agent_task_store::WorkspaceEvent {
        attribution_confidence: AttributionConfidence::DetectionOnly,
        ..event.clone()
    };
    assert!(!workspace_event_actor_is_admitted(
        &detection_only,
        "root:session",
        "legacy:session:",
        &admitted,
    ));

    let missing_identity = codex_agent_task_store::WorkspaceEvent {
        actor_id: None,
        ..event.clone()
    };
    assert!(!workspace_event_actor_is_admitted(
        &missing_identity,
        "root:session",
        "legacy:session:",
        &admitted,
    ));

    let external = codex_agent_task_store::WorkspaceEvent {
        actor_kind: WorkspaceActorKind::External,
        ..event
    };
    assert!(!workspace_event_actor_is_admitted(
        &external,
        "root:session",
        "legacy:session:",
        &admitted,
    ));
}

async fn wait_persistence_barrier(barrier: Arc<std::sync::Barrier>) {
    tokio::task::spawn_blocking(move || barrier.wait())
        .await
        .expect("persistence barrier");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completion_persistence_retries_reuse_one_completion_proof_scan() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    ledger
        .record_plan_update(&plan_with(vec![proof_free_plan_item(
            "step",
            StepStatus::Passed,
        )]))
        .await;
    install_tracked_freshness_file(&ledger, &repo, "tracked.txt", b"unchanged\n").await;
    install_persistence_supersede_control(&ledger, 2);

    let gate = ledger.completion_gate().await.expect("completion gate");

    assert_eq!(gate.status, TaskCompletionStatus::Passed);
    let diagnostics = ledger.freshness_diagnostics();
    assert_eq!(diagnostics.scan_invocations, 1);
    assert_eq!(diagnostics.files_strongly_hashed, 1);
    assert_eq!(diagnostics.strong_hashes_reused, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trusted_token_change_between_persistence_retries_starts_new_proof() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    ledger
        .record_plan_update(&plan_with(vec![plan_item("step", StepStatus::Passed)]))
        .await;
    let token_before =
        install_tracked_freshness_file(&ledger, &repo, "tracked.txt", b"before\n").await;
    let ledger = Arc::new(ledger);
    let (started, release) = install_persistence_test_control(&ledger, false);
    set_persistence_test_superseded_writes(&ledger, 1);

    let completion_ledger = Arc::clone(&ledger);
    let completion = tokio::spawn(async move { completion_ledger.completion_gate().await });
    wait_persistence_barrier(started).await;
    tokio::fs::write(repo.join("tracked.txt"), b"after with a different length\n")
        .await
        .expect("mutate tracked file");
    let file = tokio::fs::File::open(repo.join("tracked.txt"))
        .await
        .expect("reopen tracked file");
    let token_after = trusted_file_token(&file)
        .await
        .expect("trusted token after mutation");
    assert_ne!(token_before, token_after);
    wait_persistence_barrier(release).await;

    let gate = completion
        .await
        .expect("completion task")
        .expect("completion gate");
    assert_eq!(gate.status, TaskCompletionStatus::Partial);
    let diagnostics = ledger.freshness_diagnostics();
    assert_eq!(diagnostics.scan_invocations, 2);
    assert_eq!(diagnostics.files_strongly_hashed, 2);
    assert_eq!(diagnostics.strong_hashes_reused, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn evidence_change_between_persistence_retries_starts_new_proof() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    ledger
        .record_plan_update(&plan_with(vec![proof_free_plan_item(
            "step",
            StepStatus::Passed,
        )]))
        .await;
    install_tracked_freshness_file(&ledger, &repo, "tracked.txt", b"unchanged\n").await;
    let ledger = Arc::new(ledger);
    let (started, release) = install_persistence_test_control(&ledger, false);
    set_persistence_test_superseded_writes(&ledger, 1);

    let completion_ledger = Arc::clone(&ledger);
    let completion = tokio::spawn(async move { completion_ledger.completion_gate().await });
    wait_persistence_barrier(started).await;
    {
        let mut guard = ledger.document.lock().await;
        let document = guard.as_mut().expect("document");
        document.host_mutation_revision += 1;
        document.revision += 1;
    }
    wait_persistence_barrier(release).await;

    let gate = completion
        .await
        .expect("completion task")
        .expect("completion gate");
    assert_eq!(gate.status, TaskCompletionStatus::Passed);
    let diagnostics = ledger.freshness_diagnostics();
    assert_eq!(diagnostics.scan_invocations, 2);
    assert_eq!(diagnostics.files_strongly_hashed, 2);
    assert_eq!(diagnostics.strong_hashes_reused, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completion_persistence_failure_is_partial_then_recovers_when_storage_returns() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    ledger
        .record_plan_update(&plan_with(vec![proof_free_plan_item(
            "step",
            StepStatus::Passed,
        )]))
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

    assert_eq!(gate.status, TaskCompletionStatus::Partial);
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
        assert!(!storage_risk.blocking);
        assert!(!storage_risk.resolved);
        assert_eq!(
            document
                .completion
                .as_ref()
                .expect("cached completion")
                .status,
            TaskCompletionStatus::Partial
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
        &tokio::fs::read(ledger.evidence_path().expect("evidence path"))
            .await
            .expect("persisted evidence"),
    )
    .expect("valid persisted evidence");
    assert_eq!(
        persisted.completion.expect("persisted completion").status,
        TaskCompletionStatus::Passed
    );
}

#[tokio::test]
async fn atomic_review_commit_publishes_runtime_state_only_after_durable_write() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    install_persistence_supersede_control(&ledger, 0);
    let committed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let revision = ledger.document_revision().await.expect("document revision");
    set_persistence_test_failure(&ledger, true);

    let commit_flag = Arc::clone(&committed);
    let failed = ledger
        .atomic_review_update_with_commit(
            revision,
            None,
            None,
            |document| {
                document.active_step_id = Some("durable-before-runtime".to_string());
            },
            move || commit_flag.store(true, std::sync::atomic::Ordering::Release),
        )
        .await;

    assert_eq!(failed, AtomicReviewTransition::Failed);
    assert!(!committed.load(std::sync::atomic::Ordering::Acquire));
    assert_eq!(
        ledger
            .document
            .lock()
            .await
            .as_ref()
            .expect("document")
            .active_step_id,
        None
    );

    set_persistence_test_failure(&ledger, false);
    let commit_flag = Arc::clone(&committed);
    let persisted = ledger
        .atomic_review_update_with_commit(
            revision,
            None,
            None,
            |document| {
                document.active_step_id = Some("durable-before-runtime".to_string());
            },
            move || commit_flag.store(true, std::sync::atomic::Ordering::Release),
        )
        .await;

    assert_eq!(persisted, AtomicReviewTransition::Persisted(()));
    assert!(committed.load(std::sync::atomic::Ordering::Acquire));
    assert_eq!(
        ledger
            .document
            .lock()
            .await
            .as_ref()
            .expect("document")
            .active_step_id
            .as_deref(),
        Some("durable-before-runtime")
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
        &tokio::fs::read(ledger.evidence_path().expect("evidence path"))
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
        &tokio::fs::read(ledger.evidence_path().expect("evidence path"))
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
        &tokio::fs::read(ledger.evidence_path().expect("evidence path"))
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

#[tokio::test]
async fn local_user_source_capture_hashes_across_multiple_bounded_chunks() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    let image = repo.join("multi-chunk-review-image.png");
    let bytes = (0..FILE_HASH_CHUNK_SIZE * 2 + 17)
        .map(|index| (index % 251) as u8)
        .collect::<Vec<_>>();
    tokio::fs::write(&image, &bytes)
        .await
        .expect("multi-chunk image fixture");

    assert!(
        ledger
            .record_user_sources(
                "message-with-multi-chunk-image",
                &[UserInput::LocalImage {
                    path: image,
                    detail: None,
                }],
            )
            .await
    );
    let dossier = ledger
        .completion_review_dossier(
            None,
            &[],
            &[],
            &ReviewLensSelectionFacts::default(),
            &[],
            true,
            true,
        )
        .await
        .expect("completion review dossier");
    let source = dossier.sources.first().expect("captured image source");
    assert_eq!(source.availability, UserSourceAvailability::Available);
    assert!(
        source
            .exact_material
            .ends_with(&format!("#sha256={:x}", Sha256::digest(&bytes)))
    );
}

#[tokio::test]
async fn unavailable_local_image_is_preserved_as_an_unavailable_user_source() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    let missing_image = repo.join("missing-review-image.png");
    assert!(
        ledger
            .record_user_sources(
                "message-with-missing-image",
                &[UserInput::LocalImage {
                    path: missing_image.clone(),
                    detail: None,
                }],
            )
            .await
    );

    let dossier = ledger
        .completion_review_dossier(
            None,
            &[],
            &[],
            &ReviewLensSelectionFacts::default(),
            &[],
            true,
            true,
        )
        .await
        .expect("completion review dossier");
    let source = dossier.sources.first().expect("captured image source");
    assert_eq!(source.source_kind, UserSourceKind::Image);
    assert_eq!(source.availability, UserSourceAvailability::Unavailable);
    assert!(source.exact_material.contains("missing-review-image.png"));

    assert!(matches!(
        ledger
            .apply_source_classification(
                &dossier,
                source_materialization_fixture(
                    &dossier,
                    vec![(source.source_id.clone(), local_unavailable_fixture())],
                    vec![ClassifiedSource {
                        source_id: source.source_id.clone(),
                        kind: ClassifiedSourceKind::UnavailableOrTruncated,
                        requirements: Vec::new(),
                        reason: None,
                    }],
                ),
            )
            .await,
        AtomicReviewTransition::Persisted(())
    ));
    let classified = ledger
        .completion_review_dossier(
            None,
            &[],
            &[],
            &ReviewLensSelectionFacts::default(),
            &[],
            true,
            true,
        )
        .await
        .expect("classified completion review dossier");
    assert!(matches!(
        classified.source_mappings.get(&source.source_id),
        Some(SourceMapping::UnavailableOrTruncated)
    ));
}

#[tokio::test]
async fn source_classification_rejects_supersession_cycles_before_persistence() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    assert!(
        ledger
            .record_user_sources("message-1", &[text_input("alpha"), text_input("beta")])
            .await
    );
    let dossier = ledger
        .completion_review_dossier(
            None,
            &[],
            &[],
            &ReviewLensSelectionFacts::default(),
            &[],
            true,
            true,
        )
        .await
        .expect("completion review dossier");
    let alpha = dossier
        .sources
        .iter()
        .find(|source| source.exact_material == "alpha")
        .expect("alpha source");
    let beta = dossier
        .sources
        .iter()
        .find(|source| source.exact_material == "beta")
        .expect("beta source");
    let alpha_ref = ClassifiedRequirementRef {
        source_id: alpha.source_id.clone(),
        source_span: SourceSpan::Text { start: 0, end: 5 },
    };
    let beta_ref = ClassifiedRequirementRef {
        source_id: beta.source_id.clone(),
        source_span: SourceSpan::Text { start: 0, end: 4 },
    };
    let classifications = |beta_status, beta_superseded_by| {
        source_materialization_fixture(
            &dossier,
            vec![
                (
                    alpha.source_id.clone(),
                    local_requirement_fixture(vec![alpha_ref.source_span.clone()]),
                ),
                (
                    beta.source_id.clone(),
                    local_requirement_fixture(vec![beta_ref.source_span.clone()]),
                ),
            ],
            vec![
                ClassifiedSource {
                    source_id: alpha.source_id.clone(),
                    kind: ClassifiedSourceKind::RequirementBearing,
                    requirements: vec![ClassifiedRequirement {
                        source_span: alpha_ref.source_span.clone(),
                        status: RequirementStatus::Superseded,
                        superseded_by: Some(beta_ref.clone()),
                    }],
                    reason: None,
                },
                ClassifiedSource {
                    source_id: beta.source_id.clone(),
                    kind: ClassifiedSourceKind::RequirementBearing,
                    requirements: vec![ClassifiedRequirement {
                        source_span: beta_ref.source_span.clone(),
                        status: beta_status,
                        superseded_by: beta_superseded_by,
                    }],
                    reason: None,
                },
            ],
        )
    };

    assert_eq!(
        ledger
            .apply_source_classification(
                &dossier,
                classifications(RequirementStatus::Superseded, Some(alpha_ref.clone())),
            )
            .await,
        AtomicReviewTransition::Failed
    );
    assert!(matches!(
        ledger
            .apply_source_classification(
                &dossier,
                classifications(RequirementStatus::Active, None),
            )
            .await,
        AtomicReviewTransition::Persisted(())
    ));
}

#[test]
fn supersession_graph_validation_rejects_self_and_multi_node_cycles() {
    let requirement = |id: &str, superseded_by: Option<&str>| RequirementRecord {
        requirement_id: id.to_string(),
        source_id: format!("source-{id}"),
        source_content_hash: "hash".to_string(),
        source_span: SourceSpan::Text { start: 0, end: 1 },
        exact_material: id.to_string(),
        status: if superseded_by.is_some() {
            RequirementStatus::Superseded
        } else {
            RequirementStatus::Active
        },
        superseded_by: superseded_by.map(str::to_string),
    };
    assert!(!requirement_supersession_is_acyclic(&[requirement(
        "A",
        Some("A")
    )]));
    assert!(!requirement_supersession_is_acyclic(&[
        requirement("A", Some("B")),
        requirement("B", Some("A")),
    ]));
    assert!(requirement_supersession_is_acyclic(&[
        requirement("A", Some("B")),
        requirement("B", None),
    ]));
}

#[tokio::test]
async fn proof_accumulation_changes_dossier_but_not_implementation_identity() {
    let (_temp, repo, ledger, before) = classified_requirement_fixture().await;
    let cwd = AbsolutePathBuf::from_absolute_path(&repo).expect("repo path");
    ledger
        .record_command_bound_with_provenance(
            &["proof-a".to_string()],
            &PathUri::from_abs_path(&cwd),
            0,
            false,
            1,
            false,
            None,
            None,
            Some(&before.implementation_identity_hash),
        )
        .await;
    let after_a = ledger
        .completion_review_dossier(
            Some("candidate complete"),
            &[],
            &[],
            &ReviewLensSelectionFacts::default(),
            &[],
            true,
            true,
        )
        .await
        .expect("dossier after proof A");
    ledger
        .record_command_bound_with_provenance(
            &["proof-b".to_string()],
            &PathUri::from_abs_path(&cwd),
            0,
            false,
            1,
            false,
            None,
            None,
            Some(&before.implementation_identity_hash),
        )
        .await;
    let after_b = ledger
        .completion_review_dossier(
            Some("candidate complete"),
            &[],
            &[],
            &ReviewLensSelectionFacts::default(),
            &[],
            true,
            true,
        )
        .await
        .expect("dossier after proof B");

    assert_eq!(
        before.implementation_identity_hash,
        after_a.implementation_identity_hash
    );
    assert_eq!(
        after_a.implementation_identity_hash,
        after_b.implementation_identity_hash
    );
    assert_ne!(before.dossier_snapshot_id, after_a.dossier_snapshot_id);
    assert_ne!(after_a.dossier_snapshot_id, after_b.dossier_snapshot_id);
    assert_eq!(
        before.reviewer_visible_evidence["proofReceipts"]
            .as_array()
            .expect("proof list")
            .len(),
        0
    );
    assert_eq!(
        after_a.reviewer_visible_evidence["proofReceipts"]
            .as_array()
            .expect("proof list")
            .len(),
        1
    );
    let proofs = after_b.reviewer_visible_evidence["proofReceipts"]
        .as_array()
        .expect("proof list");
    assert_eq!(proofs.len(), 2);
    assert!(proofs.iter().all(|proof| {
        proof["boundImplementationIdentity"].as_str()
            == Some(after_b.implementation_identity_hash.as_str())
    }));
}

#[tokio::test]
async fn terminal_closure_is_atomic_and_reload_preserves_v2_lineage() {
    let (temp, repo, ledger, initial_dossier) = classified_requirement_fixture().await;
    let ledger = Arc::new(ledger);
    assert!(matches!(
        ledger.begin_completion_review_cycle(&initial_dossier).await,
        AtomicReviewTransition::Persisted(_)
    ));
    let review_dossier = ledger
        .completion_review_dossier(
            Some("candidate complete"),
            &[],
            &[],
            &ReviewLensSelectionFacts::default(),
            &[],
            true,
            true,
        )
        .await
        .expect("review dossier");
    assert_eq!(
        initial_dossier.implementation_identity_hash,
        review_dossier.implementation_identity_hash
    );
    assert_eq!(
        initial_dossier.dossier_snapshot_id,
        review_dossier.dossier_snapshot_id
    );
    let recorded = match ledger
        .record_completion_review_attempt_v2(
            &review_dossier,
            CompletionReviewAttemptInput {
                attempt_kind: CompletionReviewAttemptKind::InitialReview,
                parent_review_id: review_dossier.cycle_parent_review_id.clone(),
                superseded_review_id: None,
                findings: Vec::new(),
                dispositions: Vec::new(),
                manifest_gaps: Vec::new(),
                repair_instruction: None,
                repair_instruction_hash: None,
                infrastructure_outcome: "ok".to_string(),
                review_clean: true,
                terminal_outcome: None,
                attempt_identity: "test-attempt-identity".to_string(),
                reviewer_contract_hash: "test-reviewer-contract".to_string(),
            },
        )
        .await
    {
        AtomicReviewTransition::Persisted(recorded) => recorded,
        other => panic!("clean initial review did not persist: {other:?}"),
    };
    let closure_dossier = ledger
        .completion_review_dossier(
            Some("candidate complete"),
            &[],
            &[],
            &ReviewLensSelectionFacts::default(),
            &[],
            true,
            true,
        )
        .await
        .expect("terminal dossier");
    assert_eq!(
        review_dossier.implementation_identity_hash,
        closure_dossier.implementation_identity_hash
    );
    assert_eq!(
        review_dossier.dossier_snapshot_id,
        closure_dossier.dossier_snapshot_id
    );
    let evidence_path = ledger.evidence_path().expect("evidence path");
    let bytes_before_failed_closure = tokio::fs::read(&evidence_path)
        .await
        .expect("evidence before failed closure");
    let receipts_before_failed_closure = {
        let guard = ledger.document.lock().await;
        guard
            .as_ref()
            .expect("task evidence")
            .completion_review_v2
            .as_ref()
            .expect("V2 ledger")
            .receipts
            .len()
    };
    let (started, release) = install_persistence_test_control(&ledger, true);
    let failed_ledger = Arc::clone(&ledger);
    let failed_dossier = closure_dossier.clone();
    let failed_closure = tokio::spawn(async move {
        failed_ledger
            .finalize_completion_review(&failed_dossier)
            .await
    });
    wait_persistence_barrier(started).await;
    wait_persistence_barrier(release).await;
    assert!(matches!(
        failed_closure.await.expect("failed closure task"),
        AtomicReviewTransition::Failed
    ));
    assert_eq!(
        tokio::fs::read(&evidence_path)
            .await
            .expect("evidence after failed closure"),
        bytes_before_failed_closure
    );
    {
        let guard = ledger.document.lock().await;
        let document = guard.as_ref().expect("task evidence");
        let review = document.completion_review_v2.as_ref().expect("V2 ledger");
        assert_eq!(review.receipts.len(), receipts_before_failed_closure);
        assert!(review.review_risk.unresolved);
        assert_eq!(
            review.active_review_cycle.as_ref().map(|cycle| cycle.phase),
            Some(CompletionReviewCyclePhase::ProvisionalClean)
        );
        assert_ne!(
            document.completion.as_ref().map(|gate| gate.status),
            Some(TaskCompletionStatus::Passed)
        );
    }
    set_persistence_test_failure(&ledger, false);
    let gate = match ledger.finalize_completion_review(&closure_dossier).await {
        AtomicReviewTransition::Persisted(gate) => gate,
        other => panic!("terminal closure did not persist: {other:?}"),
    };
    assert_eq!(
        gate.status,
        TaskCompletionStatus::Passed,
        "unexpected completion gate reasons: {:?}",
        gate.reasons
    );

    let thread_id = ledger.thread_id.clone().expect("thread ID");
    {
        let guard = ledger.document.lock().await;
        let document = guard.as_ref().expect("task evidence");
        let review = document.completion_review_v2.as_ref().expect("V2 ledger");
        let terminal = review.receipts.last().expect("terminal receipt");
        assert_eq!(
            terminal.attempt_kind,
            CompletionReviewAttemptKind::TerminalClosure
        );
        assert_eq!(
            terminal.parent_review_id.as_deref(),
            Some(recorded.review_id.as_str())
        );
        assert_eq!(terminal.terminal_outcome.as_deref(), Some("passed"));
        assert!(!review.review_risk.unresolved);
        assert_eq!(
            latest_terminal_closure(review).map(|receipt| receipt.review_id.as_str()),
            Some(terminal.review_id.as_str())
        );
        let persisted = serde_json::to_value(document).expect("serialize task evidence");
        assert!(persisted.get("generated_artifact_requirements").is_none());
        assert!(persisted["planning"].get("counters").is_none());
        let persisted_review = &persisted["completion_review_v2"];
        for field in [
            "next_source_ordinal",
            "next_review_sequence",
            "last_terminal_closure",
        ] {
            assert!(persisted_review.get(field).is_none(), "{field}");
        }
        assert!(
            persisted_review["active_review_cycle"]
                .get("accepted_dossier_snapshot_id")
                .is_none()
        );
        for receipt in persisted_review["receipts"]
            .as_array()
            .expect("review receipts")
        {
            assert!(receipt.get("candidate_hash").is_none());
        }
        assert_eq!(
            review.active_review_cycle.as_ref().map(|cycle| cycle.phase),
            Some(CompletionReviewCyclePhase::Closed)
        );
    }
    drop(ledger);

    let reloaded = TaskEvidenceLedger::load_or_new(
        temp.path().join("home"),
        ThreadId::from_string(&thread_id).expect("thread ID parses"),
        &repo,
    )
    .await;
    let dossier = reloaded
        .completion_review_dossier(
            Some("candidate complete"),
            &[],
            &[],
            &ReviewLensSelectionFacts::default(),
            &[],
            true,
            true,
        )
        .await
        .expect("reloaded dossier");
    assert!(reloaded.passed_completion_matches_dossier(&dossier).await);
}

#[tokio::test]
async fn invalid_repair_lineage_reaches_the_production_rereview_dossier() {
    let (temp, repo, ledger, initial_dossier) = classified_requirement_fixture().await;
    assert!(matches!(
        ledger.begin_completion_review_cycle(&initial_dossier).await,
        AtomicReviewTransition::Persisted(_)
    ));
    let review_dossier = ledger
        .completion_review_dossier(
            Some("candidate complete"),
            &[],
            &[],
            &ReviewLensSelectionFacts::default(),
            &[],
            true,
            true,
        )
        .await
        .expect("review dossier");
    let findings = vec![CompletionReviewFindingInput {
        local_ordinal: 1,
        requirement_ids: vec![review_dossier.requirements[0].requirement_id.clone()],
        lens: COMPLETION_REVIEW_LENSES[0].to_string(),
        contract_surface: "bounded owner".to_string(),
        severity: "high".to_string(),
        evidence: "the active requirement is not met".to_string(),
        smallest_correction: "implement the missing behavior".to_string(),
        proof_route: "cargo test focused_case".to_string(),
    }];
    let initial_review = match ledger
        .record_completion_review_attempt_v2(
            &review_dossier,
            CompletionReviewAttemptInput {
                attempt_kind: CompletionReviewAttemptKind::InitialReview,
                parent_review_id: review_dossier.cycle_parent_review_id.clone(),
                superseded_review_id: None,
                findings,
                dispositions: Vec::new(),
                manifest_gaps: Vec::new(),
                repair_instruction: Some("{}".to_string()),
                repair_instruction_hash: None,
                infrastructure_outcome: "ok".to_string(),
                review_clean: false,
                terminal_outcome: None,
                attempt_identity: "test-attempt-identity".to_string(),
                reviewer_contract_hash: "test-reviewer-contract".to_string(),
            },
        )
        .await
    {
        AtomicReviewTransition::Persisted(recorded) => recorded,
        other => panic!("initial finding review did not persist: {other:?}"),
    };
    let thread_id = ledger.thread_id.clone().expect("thread ID");
    drop(ledger);
    let ledger = TaskEvidenceLedger::load_or_new(
        temp.path().join("home"),
        ThreadId::from_string(&thread_id).expect("thread ID parses"),
        &repo,
    )
    .await;
    let correction_dossier = ledger
        .completion_review_dossier(
            Some("candidate complete"),
            &[],
            &[],
            &ReviewLensSelectionFacts::default(),
            &[],
            true,
            true,
        )
        .await
        .expect("correction dossier");
    assert!(matches!(
        ledger
            .record_completion_review_attempt_v2(
                &correction_dossier,
                CompletionReviewAttemptInput {
                    attempt_kind: CompletionReviewAttemptKind::CorrectionEvidence,
                    parent_review_id: Some(initial_review.review_id),
                    superseded_review_id: None,
                    findings: Vec::new(),
                    dispositions: Vec::new(),
                    manifest_gaps: Vec::new(),
                    repair_instruction: None,
                    repair_instruction_hash: correction_dossier
                        .initial_repair_instruction_hash
                        .clone(),
                    infrastructure_outcome: "ok".to_string(),
                    review_clean: false,
                    terminal_outcome: None,
                    attempt_identity: "test-attempt-identity".to_string(),
                    reviewer_contract_hash: "test-reviewer-contract".to_string(),
                },
            )
            .await,
        AtomicReviewTransition::Persisted(_)
    ));

    let rereview_dossier = ledger
        .completion_review_dossier(
            Some("candidate complete"),
            &[],
            &[],
            &ReviewLensSelectionFacts::default(),
            &[],
            true,
            true,
        )
        .await
        .expect("rereview dossier");
    let rereview_input = rereview_dossier
        .rereview_input
        .expect("rereview input after correction evidence");
    assert_eq!(rereview_input.input_mode, RereviewInputMode::FullFallback);
    assert_eq!(
        rereview_input.fallback_reasons,
        vec![RereviewFallbackReason::InvalidRepairLineage]
    );
}

#[tokio::test]
async fn rereview_infrastructure_failure_survives_v5_reload() {
    let (temp, repo, ledger, initial_dossier) = classified_requirement_fixture().await;
    assert!(matches!(
        ledger.begin_completion_review_cycle(&initial_dossier).await,
        AtomicReviewTransition::Persisted(_)
    ));
    let review_dossier = ledger
        .completion_review_dossier(
            Some("candidate complete"),
            &[],
            &[],
            &ReviewLensSelectionFacts::default(),
            &[],
            true,
            true,
        )
        .await
        .expect("review dossier");
    let requirement_id = review_dossier.requirements[0].requirement_id.clone();
    let findings = vec![CompletionReviewFindingInput {
        local_ordinal: 1,
        requirement_ids: vec![requirement_id],
        lens: COMPLETION_REVIEW_LENSES[0].to_string(),
        contract_surface: "bounded owner".to_string(),
        severity: "high".to_string(),
        evidence: "the active requirement is not met".to_string(),
        smallest_correction: "implement the missing behavior".to_string(),
        proof_route: "cargo test focused_case".to_string(),
    }];
    let repair_instruction = repair_instruction_fixture(&review_dossier, &findings);
    let initial_review = match ledger
        .record_completion_review_attempt_v2(
            &review_dossier,
            CompletionReviewAttemptInput {
                attempt_kind: CompletionReviewAttemptKind::InitialReview,
                parent_review_id: review_dossier.cycle_parent_review_id.clone(),
                superseded_review_id: None,
                findings,
                dispositions: Vec::new(),
                manifest_gaps: Vec::new(),
                repair_instruction: Some(repair_instruction),
                repair_instruction_hash: None,
                infrastructure_outcome: "ok".to_string(),
                review_clean: false,
                terminal_outcome: None,
                attempt_identity: "test-attempt-identity".to_string(),
                reviewer_contract_hash: "test-reviewer-contract".to_string(),
            },
        )
        .await
    {
        AtomicReviewTransition::Persisted(recorded) => recorded,
        other => panic!("initial finding review did not persist: {other:?}"),
    };
    let correction_dossier = ledger
        .completion_review_dossier(
            Some("candidate complete"),
            &[],
            &[],
            &ReviewLensSelectionFacts::default(),
            &[],
            true,
            true,
        )
        .await
        .expect("correction dossier");
    assert!(matches!(
        ledger
            .record_completion_review_attempt_v2(
                &correction_dossier,
                CompletionReviewAttemptInput {
                    attempt_kind: CompletionReviewAttemptKind::CorrectionEvidence,
                    parent_review_id: Some(initial_review.review_id.clone()),
                    superseded_review_id: None,
                    findings: Vec::new(),
                    dispositions: Vec::new(),
                    manifest_gaps: Vec::new(),
                    repair_instruction: None,
                    repair_instruction_hash: correction_dossier
                        .initial_repair_instruction_hash
                        .clone(),
                    infrastructure_outcome: "ok".to_string(),
                    review_clean: false,
                    terminal_outcome: None,
                    attempt_identity: "test-attempt-identity".to_string(),
                    reviewer_contract_hash: "test-reviewer-contract".to_string(),
                },
            )
            .await,
        AtomicReviewTransition::Persisted(_)
    ));
    let rereview_dossier = ledger
        .completion_review_dossier(
            Some("candidate complete"),
            &[],
            &[],
            &ReviewLensSelectionFacts::default(),
            &[],
            true,
            true,
        )
        .await
        .expect("rereview dossier");
    let failed_rereview = match ledger
        .record_completion_review_attempt_v2(
            &rereview_dossier,
            CompletionReviewAttemptInput {
                attempt_kind: CompletionReviewAttemptKind::Rereview,
                parent_review_id: Some(initial_review.review_id),
                superseded_review_id: None,
                findings: Vec::new(),
                dispositions: Vec::new(),
                manifest_gaps: Vec::new(),
                repair_instruction: None,
                repair_instruction_hash: rereview_dossier.initial_repair_instruction_hash.clone(),
                infrastructure_outcome: "timeout".to_string(),
                review_clean: false,
                terminal_outcome: Some("partial".to_string()),
                attempt_identity: "test-attempt-identity".to_string(),
                reviewer_contract_hash: "test-reviewer-contract".to_string(),
            },
        )
        .await
    {
        AtomicReviewTransition::Persisted(recorded) => recorded,
        other => panic!("failed rereview did not persist: {other:?}"),
    };

    let thread_id = ledger.thread_id.clone().expect("thread ID");
    drop(ledger);
    let reloaded = TaskEvidenceLedger::load_or_new(
        temp.path().join("home"),
        ThreadId::from_string(&thread_id).expect("thread ID parses"),
        &repo,
    )
    .await;
    let guard = reloaded.document.lock().await;
    let document = guard.as_ref().expect("reloaded v5 evidence");
    assert_eq!(
        document.completion.as_ref().map(|gate| gate.status),
        Some(TaskCompletionStatus::Partial),
        "reload risks: {:?}",
        document.risks
    );
    let review = document
        .completion_review_v2
        .as_ref()
        .expect("reloaded V2 ledger");
    let receipt = review
        .receipts
        .iter()
        .find(|receipt| receipt.review_id == failed_rereview.review_id)
        .expect("failed rereview receipt");
    assert_eq!(receipt.infrastructure_outcome, "timeout");
    assert!(receipt.findings.is_empty());
    assert!(receipt.dispositions.is_empty());
    let terminal = review.receipts.last().expect("partial terminal closure");
    assert_eq!(
        terminal.parent_review_id.as_deref(),
        Some(failed_rereview.review_id.as_str())
    );
    assert_eq!(terminal.terminal_outcome.as_deref(), Some("partial"));
}

#[tokio::test]
async fn last_second_mutation_invalidates_a_provisional_clean_review() {
    let (_temp, repo, ledger, initial_dossier) = classified_requirement_fixture().await;
    assert!(matches!(
        ledger.begin_completion_review_cycle(&initial_dossier).await,
        AtomicReviewTransition::Persisted(_)
    ));
    let review_dossier = ledger
        .completion_review_dossier(
            Some("candidate complete"),
            &[],
            &[],
            &ReviewLensSelectionFacts::default(),
            &[],
            true,
            true,
        )
        .await
        .expect("review dossier");
    assert!(matches!(
        ledger
            .record_completion_review_attempt_v2(
                &review_dossier,
                CompletionReviewAttemptInput {
                    attempt_kind: CompletionReviewAttemptKind::InitialReview,
                    parent_review_id: review_dossier.cycle_parent_review_id.clone(),
                    superseded_review_id: None,
                    findings: Vec::new(),
                    dispositions: Vec::new(),
                    manifest_gaps: Vec::new(),
                    repair_instruction: None,
                    repair_instruction_hash: None,
                    infrastructure_outcome: "ok".to_string(),
                    review_clean: true,
                    terminal_outcome: None,
                    attempt_identity: "test-attempt-identity".to_string(),
                    reviewer_contract_hash: "test-reviewer-contract".to_string(),
                },
            )
            .await,
        AtomicReviewTransition::Persisted(_)
    ));
    let stale_terminal_dossier = ledger
        .completion_review_dossier(
            Some("candidate complete"),
            &[],
            &[],
            &ReviewLensSelectionFacts::default(),
            &[],
            true,
            true,
        )
        .await
        .expect("provisional terminal dossier");
    let cwd = AbsolutePathBuf::from_absolute_path(&repo).expect("repo path");
    ledger
        .record_command(
            &["late-mutating-command".to_string()],
            &PathUri::from_abs_path(&cwd),
            0,
            false,
            1,
            true,
        )
        .await;

    assert_eq!(
        ledger
            .finalize_completion_review(&stale_terminal_dossier)
            .await,
        AtomicReviewTransition::Superseded
    );
    let gate = ledger.completion_gate().await.expect("completion gate");
    assert_eq!(gate.status, TaskCompletionStatus::Partial);
    let guard = ledger.document.lock().await;
    let review = guard
        .as_ref()
        .expect("task evidence")
        .completion_review_v2
        .as_ref()
        .expect("V2 ledger");
    assert!(review.review_risk.unresolved);
    assert!(latest_terminal_closure(review).is_none());
}

#[tokio::test]
async fn implemented_below_ignored_above_stale_supersession_retries_current_revision() {
    let (_temp, repo, ledger, initial_dossier) = classified_requirement_fixture().await;
    assert!(matches!(
        ledger.begin_completion_review_cycle(&initial_dossier).await,
        AtomicReviewTransition::Persisted(_)
    ));
    let review_dossier = ledger
        .completion_review_dossier(
            Some("candidate complete"),
            &[],
            &[],
            &ReviewLensSelectionFacts::default(),
            &[],
            true,
            true,
        )
        .await
        .expect("review dossier");
    assert!(matches!(
        ledger
            .record_completion_review_attempt_v2(
                &review_dossier,
                CompletionReviewAttemptInput {
                    attempt_kind: CompletionReviewAttemptKind::InitialReview,
                    parent_review_id: review_dossier.cycle_parent_review_id.clone(),
                    superseded_review_id: None,
                    findings: Vec::new(),
                    dispositions: Vec::new(),
                    manifest_gaps: Vec::new(),
                    repair_instruction: None,
                    repair_instruction_hash: None,
                    infrastructure_outcome: "ok".to_string(),
                    review_clean: true,
                    terminal_outcome: None,
                    attempt_identity: "test-attempt-identity".to_string(),
                    reviewer_contract_hash: "test-reviewer-contract".to_string(),
                },
            )
            .await,
        AtomicReviewTransition::Persisted(_)
    ));
    let stale_dossier = ledger
        .completion_review_dossier(
            Some("candidate complete"),
            &[],
            &[],
            &ReviewLensSelectionFacts::default(),
            &[],
            true,
            true,
        )
        .await
        .expect("provisional dossier");
    let cwd = AbsolutePathBuf::from_absolute_path(&repo).expect("repo path");
    ledger
        .record_command(
            &["late-mutating-command".to_string()],
            &PathUri::from_abs_path(&cwd),
            0,
            false,
            1,
            true,
        )
        .await;

    assert_eq!(
        ledger
            .supersede_provisional_completion_review(&stale_dossier)
            .await,
        AtomicReviewTransition::Superseded
    );
    install_persistence_supersede_control(&ledger, 0);
    set_persistence_test_failure(&ledger, true);
    assert_eq!(
        ledger
            .supersede_current_provisional_completion_review()
            .await,
        AtomicReviewTransition::Failed
    );
    {
        let guard = ledger.document.lock().await;
        let review = guard
            .as_ref()
            .expect("task evidence")
            .completion_review_v2
            .as_ref()
            .expect("V2 ledger");
        assert_eq!(
            review.active_review_cycle.as_ref().map(|cycle| cycle.phase),
            Some(CompletionReviewCyclePhase::InitialReviewPending)
        );
    }
    set_persistence_test_failure(&ledger, false);
    assert_eq!(
        ledger
            .supersede_current_provisional_completion_review()
            .await,
        AtomicReviewTransition::Persisted(())
    );
    let guard = ledger.document.lock().await;
    let document = guard.as_ref().expect("task evidence");
    let review = document.completion_review_v2.as_ref().expect("V2 ledger");
    let cycle = review.active_review_cycle.as_ref().expect("review cycle");
    assert_eq!(
        cycle.phase,
        CompletionReviewCyclePhase::InitialReviewPending
    );
    assert!(cycle.accepted_review_id.is_none());
    assert!(review.review_risk.unresolved);
    assert!(document.completion.is_none());
}

#[tokio::test]
async fn after_agent_reentry_requires_fresh_review_and_preserves_correction_use() {
    let (_temp, _repo, ledger, initial_dossier) = classified_requirement_fixture().await;
    assert!(matches!(
        ledger.begin_completion_review_cycle(&initial_dossier).await,
        AtomicReviewTransition::Persisted(_)
    ));
    let review_dossier = ledger
        .completion_review_dossier(
            Some("candidate complete"),
            &[],
            &[],
            &ReviewLensSelectionFacts::default(),
            &[],
            true,
            true,
        )
        .await
        .expect("review dossier");
    assert!(matches!(
        ledger
            .record_completion_review_attempt_v2(
                &review_dossier,
                CompletionReviewAttemptInput {
                    attempt_kind: CompletionReviewAttemptKind::InitialReview,
                    parent_review_id: review_dossier.cycle_parent_review_id.clone(),
                    superseded_review_id: None,
                    findings: Vec::new(),
                    dispositions: Vec::new(),
                    manifest_gaps: Vec::new(),
                    repair_instruction: None,
                    repair_instruction_hash: None,
                    infrastructure_outcome: "ok".to_string(),
                    review_clean: true,
                    terminal_outcome: None,
                    attempt_identity: "test-attempt-identity".to_string(),
                    reviewer_contract_hash: "test-reviewer-contract".to_string(),
                },
            )
            .await,
        AtomicReviewTransition::Persisted(_)
    ));
    assert!(matches!(
        ledger
            .prepare_after_agent_completion_review_reentry(true)
            .await,
        AtomicReviewTransition::Persisted(_)
    ));

    let guard = ledger.document.lock().await;
    let document = guard.as_ref().expect("task evidence");
    let review = document.completion_review_v2.as_ref().expect("V2 ledger");
    let cycle = review.active_review_cycle.as_ref().expect("active cycle");
    assert_eq!(
        cycle.phase,
        CompletionReviewCyclePhase::InitialReviewPending
    );
    assert!(cycle.correction_consumed);
    assert!(cycle.accepted_review_id.is_none());
    assert!(review.review_risk.unresolved);
    assert_ne!(
        document.completion.as_ref().map(|gate| gate.status),
        Some(TaskCompletionStatus::Passed)
    );
}

#[tokio::test]
async fn manifest_gap_replacement_review_links_to_superseded_receipt_across_reload() {
    let (temp, repo, ledger) = ledger_fixture().await;
    ledger
        .record_plan_update(&plan_with(vec![plan_item("step", StepStatus::Passed)]))
        .await;
    assert!(
        ledger
            .record_user_sources("message-1", &[text_input("implement alpha and beta")])
            .await
    );
    let unclassified = ledger
        .completion_review_dossier(
            Some("candidate complete"),
            &[],
            &[],
            &ReviewLensSelectionFacts::default(),
            &[],
            true,
            true,
        )
        .await
        .expect("unclassified dossier");
    let source = unclassified.sources.first().expect("captured source");
    assert!(matches!(
        ledger
            .apply_source_classification(
                &unclassified,
                source_materialization_fixture(
                    &unclassified,
                    vec![(
                        source.source_id.clone(),
                        local_requirement_fixture(vec![SourceSpan::Text { start: 0, end: 15 }]),
                    )],
                    vec![ClassifiedSource {
                        source_id: source.source_id.clone(),
                        kind: ClassifiedSourceKind::RequirementBearing,
                        requirements: vec![ClassifiedRequirement {
                            source_span: SourceSpan::Text { start: 0, end: 15 },
                            status: RequirementStatus::Active,
                            superseded_by: None,
                        }],
                        reason: None,
                    }],
                ),
            )
            .await,
        AtomicReviewTransition::Persisted(())
    ));
    let initial = ledger
        .completion_review_dossier(
            Some("candidate complete"),
            &[],
            &[],
            &ReviewLensSelectionFacts::default(),
            &[],
            true,
            true,
        )
        .await
        .expect("initial dossier");
    assert!(matches!(
        ledger.begin_completion_review_cycle(&initial).await,
        AtomicReviewTransition::Persisted(_)
    ));
    let gap_dossier = ledger
        .completion_review_dossier(
            Some("candidate complete"),
            &[],
            &[],
            &ReviewLensSelectionFacts::default(),
            &[],
            true,
            true,
        )
        .await
        .expect("gap review dossier");
    let manifest_gaps = vec![ManifestGapInput {
        source_id: source.source_id.clone(),
        omitted_spans: vec![SourceSpan::Text { start: 15, end: 24 }],
    }];
    let gap_materialization = active_gap_materialization_fixture(&gap_dossier, &manifest_gaps);
    let gap_review = match ledger
        .record_completion_review_attempt_v2_with_materialization(
            &gap_dossier,
            CompletionReviewAttemptInput {
                attempt_kind: CompletionReviewAttemptKind::InitialReview,
                parent_review_id: gap_dossier.cycle_parent_review_id.clone(),
                superseded_review_id: None,
                findings: Vec::new(),
                dispositions: Vec::new(),
                manifest_gaps,
                repair_instruction: None,
                repair_instruction_hash: None,
                infrastructure_outcome: "ok".to_string(),
                review_clean: false,
                terminal_outcome: None,
                attempt_identity: "test-attempt-identity".to_string(),
                reviewer_contract_hash: "test-reviewer-contract".to_string(),
            },
            gap_materialization,
        )
        .await
    {
        AtomicReviewTransition::Persisted(recorded) => recorded,
        other => panic!("manifest gap review did not persist: {other:?}"),
    };

    let replacement_dossier = ledger
        .completion_review_dossier(
            Some("candidate complete"),
            &[],
            &[],
            &ReviewLensSelectionFacts::default(),
            &[],
            true,
            true,
        )
        .await
        .expect("replacement initial dossier");
    assert_eq!(
        replacement_dossier.manifest_revision,
        initial.manifest_revision.saturating_add(1)
    );
    assert_eq!(replacement_dossier.requirements.len(), 2);
    assert_eq!(
        replacement_dossier.cycle_superseded_review_id.as_deref(),
        Some(gap_review.review_id.as_str())
    );
    let replacement = CompletionReviewAttemptInput {
        attempt_kind: CompletionReviewAttemptKind::InitialReview,
        parent_review_id: replacement_dossier.cycle_parent_review_id.clone(),
        superseded_review_id: replacement_dossier.cycle_superseded_review_id.clone(),
        findings: Vec::new(),
        dispositions: Vec::new(),
        manifest_gaps: Vec::new(),
        repair_instruction: None,
        repair_instruction_hash: None,
        infrastructure_outcome: "ok".to_string(),
        review_clean: true,
        terminal_outcome: None,
        attempt_identity: "test-attempt-identity".to_string(),
        reviewer_contract_hash: "test-reviewer-contract".to_string(),
    };
    let mut missing_link = replacement.clone();
    missing_link.superseded_review_id = None;
    assert!(matches!(
        ledger
            .record_completion_review_attempt_v2(&replacement_dossier, missing_link)
            .await,
        AtomicReviewTransition::Failed
    ));
    let replacement_review = match ledger
        .record_completion_review_attempt_v2(&replacement_dossier, replacement)
        .await
    {
        AtomicReviewTransition::Persisted(recorded) => recorded,
        other => panic!("replacement initial review did not persist: {other:?}"),
    };

    {
        let guard = ledger.document.lock().await;
        assert_eq!(
            validate_v5_completion_review(guard.as_ref().expect("task evidence")),
            Ok(()),
            "replacement lineage must be reload-valid"
        );
    }

    let thread_id = ledger.thread_id.clone().expect("thread ID");
    drop(ledger);
    let reloaded = TaskEvidenceLedger::load_or_new(
        temp.path().join("home"),
        ThreadId::from_string(&thread_id).expect("thread ID parses"),
        &repo,
    )
    .await;
    let reloaded_dossier = reloaded
        .completion_review_dossier(
            Some("candidate complete"),
            &[],
            &[],
            &ReviewLensSelectionFacts::default(),
            &[],
            true,
            true,
        )
        .await
        .expect("reloaded replacement dossier");
    assert_eq!(
        reloaded_dossier.cycle_superseded_review_id.as_deref(),
        Some(gap_review.review_id.as_str())
    );
    let guard = reloaded.document.lock().await;
    let review = guard
        .as_ref()
        .expect("task evidence")
        .completion_review_v2
        .as_ref()
        .expect("V2 ledger");
    let receipt = review
        .receipts
        .iter()
        .find(|receipt| receipt.review_id == replacement_review.review_id)
        .expect("replacement review receipt");
    assert_eq!(
        receipt.superseded_review_id.as_deref(),
        Some(gap_review.review_id.as_str())
    );
}

#[tokio::test]
async fn rereview_manifest_gap_starts_a_linked_initial_review_and_survives_reload() {
    let (temp, repo, ledger) = ledger_fixture().await;
    ledger
        .record_plan_update(&plan_with(vec![plan_item("step", StepStatus::Passed)]))
        .await;
    assert!(
        ledger
            .record_user_sources("message-1", &[text_input("implement alpha and beta")])
            .await
    );
    let unclassified = ledger
        .completion_review_dossier(
            Some("candidate complete"),
            &[],
            &[],
            &ReviewLensSelectionFacts::default(),
            &[],
            true,
            true,
        )
        .await
        .expect("unclassified dossier");
    let source_id = unclassified.sources[0].source_id.clone();
    assert!(matches!(
        ledger
            .apply_source_classification(
                &unclassified,
                source_materialization_fixture(
                    &unclassified,
                    vec![(
                        source_id.clone(),
                        local_requirement_fixture(vec![SourceSpan::Text { start: 0, end: 15 }]),
                    )],
                    vec![ClassifiedSource {
                        source_id: source_id.clone(),
                        kind: ClassifiedSourceKind::RequirementBearing,
                        requirements: vec![ClassifiedRequirement {
                            source_span: SourceSpan::Text { start: 0, end: 15 },
                            status: RequirementStatus::Active,
                            superseded_by: None,
                        }],
                        reason: None,
                    }],
                ),
            )
            .await,
        AtomicReviewTransition::Persisted(())
    ));
    let initial = ledger
        .completion_review_dossier(
            Some("candidate complete"),
            &[],
            &[],
            &ReviewLensSelectionFacts::default(),
            &[],
            true,
            true,
        )
        .await
        .expect("initial dossier");
    let requirement_id = initial.requirements[0].requirement_id.clone();
    assert!(matches!(
        ledger.begin_completion_review_cycle(&initial).await,
        AtomicReviewTransition::Persisted(_)
    ));
    let review_dossier = ledger
        .completion_review_dossier(
            Some("candidate complete"),
            &[],
            &[],
            &ReviewLensSelectionFacts::default(),
            &[],
            true,
            true,
        )
        .await
        .expect("review dossier");
    let findings = vec![CompletionReviewFindingInput {
        local_ordinal: 1,
        requirement_ids: vec![requirement_id],
        lens: COMPLETION_REVIEW_LENSES[0].to_string(),
        contract_surface: "bounded owner".to_string(),
        severity: "high".to_string(),
        evidence: "alpha needs a focused correction".to_string(),
        smallest_correction: "correct alpha".to_string(),
        proof_route: "cargo test alpha".to_string(),
    }];
    let repair_instruction = repair_instruction_fixture(&review_dossier, &findings);
    let initial_review = match ledger
        .record_completion_review_attempt_v2(
            &review_dossier,
            CompletionReviewAttemptInput {
                attempt_kind: CompletionReviewAttemptKind::InitialReview,
                parent_review_id: review_dossier.cycle_parent_review_id.clone(),
                superseded_review_id: None,
                findings,
                dispositions: Vec::new(),
                manifest_gaps: Vec::new(),
                repair_instruction: Some(repair_instruction),
                repair_instruction_hash: None,
                infrastructure_outcome: "ok".to_string(),
                review_clean: false,
                terminal_outcome: None,
                attempt_identity: "test-attempt-identity".to_string(),
                reviewer_contract_hash: "test-reviewer-contract".to_string(),
            },
        )
        .await
    {
        AtomicReviewTransition::Persisted(recorded) => recorded,
        other => panic!("initial finding review did not persist: {other:?}"),
    };
    let correction_dossier = ledger
        .completion_review_dossier(
            Some("candidate complete"),
            &[],
            &[],
            &ReviewLensSelectionFacts::default(),
            &[],
            true,
            true,
        )
        .await
        .expect("correction dossier");
    assert!(matches!(
        ledger
            .record_completion_review_attempt_v2(
                &correction_dossier,
                CompletionReviewAttemptInput {
                    attempt_kind: CompletionReviewAttemptKind::CorrectionEvidence,
                    parent_review_id: Some(initial_review.review_id.clone()),
                    superseded_review_id: None,
                    findings: Vec::new(),
                    dispositions: Vec::new(),
                    manifest_gaps: Vec::new(),
                    repair_instruction: None,
                    repair_instruction_hash: correction_dossier
                        .initial_repair_instruction_hash
                        .clone(),
                    infrastructure_outcome: "ok".to_string(),
                    review_clean: false,
                    terminal_outcome: None,
                    attempt_identity: "test-attempt-identity".to_string(),
                    reviewer_contract_hash: "test-reviewer-contract".to_string(),
                },
            )
            .await,
        AtomicReviewTransition::Persisted(_)
    ));
    let rereview_dossier = ledger
        .completion_review_dossier(
            Some("candidate complete"),
            &[],
            &[],
            &ReviewLensSelectionFacts::default(),
            &[],
            true,
            true,
        )
        .await
        .expect("rereview dossier");
    let manifest_gaps = vec![ManifestGapInput {
        source_id,
        omitted_spans: vec![SourceSpan::Text { start: 15, end: 24 }],
    }];
    let gap_materialization = active_gap_materialization_fixture(&rereview_dossier, &manifest_gaps);
    let gap_rereview = match ledger
        .record_completion_review_attempt_v2_with_materialization(
            &rereview_dossier,
            CompletionReviewAttemptInput {
                attempt_kind: CompletionReviewAttemptKind::Rereview,
                parent_review_id: Some(initial_review.review_id.clone()),
                superseded_review_id: None,
                findings: Vec::new(),
                dispositions: vec![CompletionReviewDispositionReceipt {
                    finding_id: initial_review.findings[0].finding_id.clone(),
                    disposition: "resolved".to_string(),
                    evidence: "fresh proof resolves the original finding".to_string(),
                }],
                manifest_gaps,
                repair_instruction: None,
                repair_instruction_hash: rereview_dossier.initial_repair_instruction_hash.clone(),
                infrastructure_outcome: "ok".to_string(),
                review_clean: false,
                terminal_outcome: None,
                attempt_identity: "test-attempt-identity".to_string(),
                reviewer_contract_hash: "test-reviewer-contract".to_string(),
            },
            gap_materialization,
        )
        .await
    {
        AtomicReviewTransition::Persisted(recorded) => recorded,
        other => panic!("rereview manifest gap did not persist: {other:?}"),
    };

    let replacement_dossier = ledger
        .completion_review_dossier(
            Some("candidate complete"),
            &[],
            &[],
            &ReviewLensSelectionFacts::default(),
            &[],
            true,
            true,
        )
        .await
        .expect("replacement dossier");
    assert!(replacement_dossier.correction_consumed);
    assert_eq!(replacement_dossier.requirements.len(), 2);
    assert_eq!(
        replacement_dossier.cycle_superseded_review_id.as_deref(),
        Some(gap_rereview.review_id.as_str())
    );
    let replacement_review = match ledger
        .record_completion_review_attempt_v2(
            &replacement_dossier,
            CompletionReviewAttemptInput {
                attempt_kind: CompletionReviewAttemptKind::InitialReview,
                parent_review_id: replacement_dossier.cycle_parent_review_id.clone(),
                superseded_review_id: replacement_dossier.cycle_superseded_review_id.clone(),
                findings: Vec::new(),
                dispositions: Vec::new(),
                manifest_gaps: Vec::new(),
                repair_instruction: None,
                repair_instruction_hash: None,
                infrastructure_outcome: "ok".to_string(),
                review_clean: true,
                terminal_outcome: None,
                attempt_identity: "test-attempt-identity".to_string(),
                reviewer_contract_hash: "test-reviewer-contract".to_string(),
            },
        )
        .await
    {
        AtomicReviewTransition::Persisted(recorded) => recorded,
        other => panic!("replacement initial review did not persist: {other:?}"),
    };

    let thread_id = ledger.thread_id.clone().expect("thread ID");
    drop(ledger);
    let reloaded = TaskEvidenceLedger::load_or_new(
        temp.path().join("home"),
        ThreadId::from_string(&thread_id).expect("thread ID parses"),
        &repo,
    )
    .await;
    let guard = reloaded.document.lock().await;
    let review = guard
        .as_ref()
        .expect("task evidence")
        .completion_review_v2
        .as_ref()
        .expect("V2 ledger");
    let receipt = review
        .receipts
        .iter()
        .find(|receipt| receipt.review_id == replacement_review.review_id)
        .expect("replacement review receipt");
    assert_eq!(
        receipt.superseded_review_id.as_deref(),
        Some(gap_rereview.review_id.as_str())
    );
}

#[tokio::test]
async fn reclassification_cannot_erase_a_previously_mapped_requirement() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    ledger
        .record_plan_update(&plan_with(vec![plan_item("step", StepStatus::Implemented)]))
        .await;
    assert!(
        ledger
            .record_user_sources("message-1", &[text_input("implement alpha")])
            .await
    );
    let unclassified = ledger
        .completion_review_dossier(
            Some("candidate complete"),
            &[],
            &[],
            &ReviewLensSelectionFacts::default(),
            &[],
            true,
            true,
        )
        .await
        .expect("unclassified dossier");
    let source_id = unclassified.sources[0].source_id.clone();
    assert!(matches!(
        ledger
            .apply_source_classification(
                &unclassified,
                source_materialization_fixture(
                    &unclassified,
                    vec![(
                        source_id.clone(),
                        local_requirement_fixture(vec![SourceSpan::Text { start: 0, end: 15 }]),
                    )],
                    vec![ClassifiedSource {
                        source_id,
                        kind: ClassifiedSourceKind::RequirementBearing,
                        requirements: vec![ClassifiedRequirement {
                            source_span: SourceSpan::Text { start: 0, end: 15 },
                            status: RequirementStatus::Active,
                            superseded_by: None,
                        }],
                        reason: None,
                    }],
                ),
            )
            .await,
        AtomicReviewTransition::Persisted(())
    ));
    let classified = ledger
        .completion_review_dossier(
            Some("candidate complete"),
            &[],
            &[],
            &ReviewLensSelectionFacts::default(),
            &[],
            true,
            true,
        )
        .await
        .expect("classified dossier");
    let prior_source = classified.sources[0].clone();
    let prior_requirement = classified.requirements[0].clone();
    assert!(
        ledger
            .record_user_sources("message-2", &[text_input("background context")])
            .await
    );
    let dossier = ledger
        .completion_review_dossier(
            Some("candidate complete"),
            &[],
            &[],
            &ReviewLensSelectionFacts::default(),
            &[],
            true,
            true,
        )
        .await
        .expect("reclassification dossier");
    let new_source = dossier
        .sources
        .iter()
        .find(|source| source.message_id == "message-2")
        .expect("new source")
        .clone();
    let context = ClassifiedSource {
        source_id: new_source.source_id.clone(),
        kind: ClassifiedSourceKind::NonRequirement,
        requirements: Vec::new(),
        reason: Some("background context only".to_string()),
    };
    assert!(matches!(
        ledger
            .apply_source_classification(
                &dossier,
                source_materialization_fixture(
                    &dossier,
                    vec![(
                        new_source.source_id.clone(),
                        local_non_requirement_fixture("background context only"),
                    )],
                    vec![
                        ClassifiedSource {
                            source_id: prior_source.source_id.clone(),
                            kind: ClassifiedSourceKind::SupersededContext,
                            requirements: Vec::new(),
                            reason: Some("incorrectly treated as context".to_string()),
                        },
                        context.clone(),
                    ],
                ),
            )
            .await,
        AtomicReviewTransition::Failed
    ));

    assert!(matches!(
        ledger
            .apply_source_classification(
                &dossier,
                source_materialization_fixture(
                    &dossier,
                    vec![(
                        new_source.source_id,
                        local_non_requirement_fixture("background context only"),
                    )],
                    vec![
                        ClassifiedSource {
                            source_id: prior_source.source_id,
                            kind: ClassifiedSourceKind::RequirementBearing,
                            requirements: vec![ClassifiedRequirement {
                                source_span: prior_requirement.source_span.clone(),
                                status: RequirementStatus::Active,
                                superseded_by: None,
                            }],
                            reason: None,
                        },
                        context,
                    ],
                ),
            )
            .await,
        AtomicReviewTransition::Persisted(())
    ));
    let refreshed = ledger
        .completion_review_dossier(
            Some("candidate complete"),
            &[],
            &[],
            &ReviewLensSelectionFacts::default(),
            &[],
            true,
            true,
        )
        .await
        .expect("refreshed dossier");
    assert_eq!(refreshed.requirements, vec![prior_requirement]);
}

#[tokio::test]
async fn v6_migration_seeds_final_proof_field() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    let home = ledger.codex_home.as_ref().expect("home").clone();
    let evidence_path = ledger.evidence_path().expect("path");
    let thread_id = ledger.thread_id.as_deref().expect("thread").to_string();
    let mut value = serde_json::to_value(
        ledger
            .document
            .lock()
            .await
            .as_ref()
            .expect("document")
            .clone(),
    )
    .expect("serialize current document");
    value["schema_version"] = serde_json::json!(FROZEN_TASK_EVIDENCE_V6_SCHEMA_VERSION);
    let object = value.as_object_mut().expect("document object");
    object.remove("final_proof");
    tokio::fs::write(
        &evidence_path,
        serde_json::to_vec_pretty(&value).expect("serialize v6"),
    )
    .await
    .expect("write v6");
    drop(ledger);

    let migrated = TaskEvidenceLedger::load_or_new(
        home,
        ThreadId::from_string(&thread_id).expect("thread id"),
        &repo,
    )
    .await;
    let guard = migrated.document.lock().await;
    let document = guard.as_ref().expect("migrated document");
    assert_eq!(document.schema_version, TASK_EVIDENCE_SCHEMA_VERSION);
    assert_eq!(document.final_proof, FinalProofStateV1::default());
}

fn final_proof_input() -> FinalProofSealInputV1 {
    FinalProofSealInputV1 {
        implementation_identity: "implementation-a".to_string(),
        source_identity: "sources-a".to_string(),
        requirement_identity: "requirements-a".to_string(),
        workspace_epoch: 11,
        workspace_manifest_identity: "workspace-a".to_string(),
        environment_identity: "environment-a".to_string(),
        toolchain_identity: "toolchain-a".to_string(),
        features_identity: "features-a".to_string(),
        configuration_identity: "configuration-a".to_string(),
        child_gate_state: Vec::new(),
        reviewer_configuration_identity: "reviewer-a".to_string(),
        typed_validation_proofs: Vec::new(),
        diff_snapshot: CandidateDiffSnapshotV1 {
            candidate_id: String::new(),
            diff_identity: "diff-a".to_string(),
            head_identity: Some("head-a".to_string()),
            index_identity: Some("index-a".to_string()),
            worktree_identity: Some("worktree-a".to_string()),
            changed_paths: vec!["src/lib.rs".to_string()],
            bounded_hunks: "@@ focused diff @@".to_string(),
            raw_artifact_digest: "artifact-digest-a".to_string(),
            raw_artifact_ref: Some("artifact://diff-a".to_string()),
            workspace_epoch: 11,
        },
        checkpoint_token_budget: 10_000,
    }
}

#[tokio::test]
async fn completion_candidate_basis_ignores_orchestration_epochs() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    let document = ledger
        .document
        .lock()
        .await
        .as_ref()
        .expect("document")
        .clone();
    let input = final_proof_input();
    let basis = completion_candidate_basis(&document, &input);

    let mut observational = document.clone();
    observational.revision = observational.revision.saturating_add(9);
    observational.updated_at = timestamp();
    assert_eq!(
        basis.basis_id,
        completion_candidate_basis(&observational, &input).basis_id
    );

    let mut evidence_changed = document.clone();
    evidence_changed.evidence_epoch = evidence_changed.evidence_epoch.saturating_add(1);
    assert_eq!(
        basis.basis_id,
        completion_candidate_basis(&evidence_changed, &input).basis_id
    );
    let mut host_changed = document.clone();
    host_changed.host_mutation_revision = host_changed.host_mutation_revision.saturating_add(1);
    assert_eq!(
        basis.basis_id,
        completion_candidate_basis(&host_changed, &input).basis_id
    );
    let mut workspace_epoch_changed = input.clone();
    workspace_epoch_changed.workspace_epoch =
        workspace_epoch_changed.workspace_epoch.saturating_add(1);
    assert_eq!(
        basis.basis_id,
        completion_candidate_basis(&document, &workspace_epoch_changed).basis_id
    );

    for changed in [
        ("implementation", "implementation-b"),
        ("source", "sources-b"),
        ("requirement", "requirements-b"),
        ("workspace", "workspace-b"),
        ("environment", "environment-b"),
        ("toolchain", "toolchain-b"),
        ("features", "features-b"),
        ("configuration", "configuration-b"),
        ("reviewer", "reviewer-b"),
        ("diff", "diff-b"),
    ] {
        let mut changed_input = input.clone();
        match changed.0 {
            "implementation" => changed_input.implementation_identity = changed.1.to_string(),
            "source" => changed_input.source_identity = changed.1.to_string(),
            "requirement" => changed_input.requirement_identity = changed.1.to_string(),
            "workspace" => changed_input.workspace_manifest_identity = changed.1.to_string(),
            "environment" => changed_input.environment_identity = changed.1.to_string(),
            "toolchain" => changed_input.toolchain_identity = changed.1.to_string(),
            "features" => changed_input.features_identity = changed.1.to_string(),
            "configuration" => changed_input.configuration_identity = changed.1.to_string(),
            "reviewer" => changed_input.reviewer_configuration_identity = changed.1.to_string(),
            "diff" => changed_input.diff_snapshot.diff_identity = changed.1.to_string(),
            _ => unreachable!(),
        }
        assert_ne!(
            basis.basis_id,
            completion_candidate_basis(&document, &changed_input).basis_id,
            "{} must affect the immutable candidate basis",
            changed.0
        );
    }
}

#[tokio::test]
async fn final_proof_observation_survives_exact_candidate_revert() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    let mut document = ledger
        .document
        .lock()
        .await
        .as_ref()
        .expect("document")
        .clone();
    let basis = completion_candidate_basis(&document, &final_proof_input());
    let plan = ValidationPlanV1 {
        plan_id: "plan-a".to_string(),
        basis_id: basis.basis_id.clone(),
        steps: vec![ValidationPlanStepV1 {
            step_id: "step-a".to_string(),
            obligation_id: "obligation-a".to_string(),
            ..ValidationPlanStepV1::default()
        }],
        ..ValidationPlanV1::default()
    };
    let candidate = completion_candidate_for(&basis, &plan);
    document.final_proof.candidate = Some(candidate.clone());
    document.final_proof.checkpoint = Some(CompletionCheckpointV1::default());
    document.final_proof.proof_observations = vec![FinalProofObservationV1 {
        candidate_id: candidate.candidate_id.clone(),
        plan_step_id: "step-a".to_string(),
        obligation_id: "obligation-a".to_string(),
        successful: true,
        complete_identity: true,
        evidence_revision: document.evidence_epoch,
        ..FinalProofObservationV1::default()
    }];
    let prior_epoch = document.evidence_epoch;

    invalidate_for_mutation(
        &mut document,
        Some(&BTreeSet::from(["src/lib.rs".to_string()])),
    );
    assert!(document.final_proof.checkpoint.is_none());
    assert_eq!(document.final_proof.candidate.as_ref(), Some(&candidate));
    let (observations, _, _) =
        current_final_proof_observations(&document, &basis, &candidate, &plan, &[]);

    assert_eq!(document.evidence_epoch, prior_epoch.saturating_add(1));
    assert_eq!(observations.len(), 1);
    assert_eq!(observations[0].evidence_revision, document.evidence_epoch);
}

fn typed_validation_proof_fixture(
    repository_root: &Path,
    workspace_epoch: u64,
    step: &ValidationPlanStepV1,
) -> TypedValidationProofInputV1 {
    let repository_root = repository_root.to_string_lossy().into_owned();
    let implementation_identity = "child-implementation".to_string();
    let coverage_identity = "child-coverage".to_string();
    let call_id = "child-validation-call".to_string();
    TypedValidationProofInputV1 {
        assignment_id: "child-assignment".to_string(),
        attempt_id: "child-attempt".to_string(),
        call_id: call_id.clone(),
        receipt_evidence_epoch: workspace_epoch,
        workspace_epoch,
        validation_end_epoch: workspace_epoch,
        implementation_identity: implementation_identity.clone(),
        coverage_identity: coverage_identity.clone(),
        recorded_cwd: repository_root.clone(),
        retained_output_digest: "child-output-digest".to_string(),
        retained_output_ref: "artifact://child-output".to_string(),
        covered_manifest: Vec::new(),
        current_workspace_manifest_identity: None,
        validation_result: codex_protocol::validation::ValidationResult {
            proof_key: codex_protocol::validation::ValidationProofKey {
                repository: repository_root.clone(),
                cwd: repository_root,
                canonical_route_hash: "child-route".to_string(),
                implementation_identity,
                coverage_identity,
                environment_identity: "child-environment".to_string(),
                toolchain_identity: "child-toolchain".to_string(),
                configuration_identity: "child-configuration".to_string(),
                validation_contract_version:
                    codex_protocol::validation::VALIDATION_CONTRACT_VERSION,
            },
            route: ValidationRoute {
                leaves: vec![codex_protocol::plan_tool::ValidationRouteLeaf {
                    argv: step.argv.clone(),
                    uncertainty: "root final-proof obligation".to_string(),
                    covered_paths: step.covered_paths.clone(),
                    covered_contracts: step.covered_contracts.clone(),
                    timeout_ms: step.timeout_ms,
                    semantic_timeout: step.semantic_timeout,
                }],
                ordering: ValidationRouteOrdering::RunAll,
            },
            call_id,
            process_id: None,
            status: codex_protocol::validation::ValidationTerminalStatus::Succeeded,
            duration_ms: 17,
            summary: Some("child validation passed".to_string()),
            failure_excerpt: None,
            raw_artifact_ref: Some("artifact://child-raw-output".to_string()),
            raw_artifact_sha256: Some("child-raw-output-sha256".to_string()),
            freshness: codex_protocol::validation::ValidationFreshness::Executed,
        },
    }
}

#[tokio::test]
async fn typed_validation_manifest_observations_are_reused_across_proofs() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    tokio::fs::create_dir_all(&repo).await.expect("create repo");
    initialize_git_repo(&repo).await;
    let covered_path = repo.join("covered.rs");
    tokio::fs::write(&covered_path, b"validated contents")
        .await
        .expect("write covered path");
    let add = tokio::process::Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["add", "covered.rs"])
        .output()
        .await
        .expect("git add should run");
    assert!(add.status.success(), "git add failed");
    let commit = tokio::process::Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args([
            "-c",
            "user.name=Codex Test",
            "-c",
            "user.email=codex@example.com",
            "commit",
            "--quiet",
            "-m",
            "fixture",
        ])
        .output()
        .await
        .expect("git commit should run");
    assert!(
        commit.status.success(),
        "git commit failed: {}",
        String::from_utf8_lossy(&commit.stderr)
    );
    let head = codex_git_utils::get_head_commit_hash(&repo)
        .await
        .expect("fixture HEAD")
        .0;
    let step = ValidationPlanStepV1 {
        step_id: "typed-step".to_string(),
        covered_paths: vec!["covered.rs".to_string()],
        ..ValidationPlanStepV1::default()
    };
    let mut proof = typed_validation_proof_fixture(&repo, 7, &step);
    proof.covered_manifest = vec![
        codex_agent_task_store::WorkspaceManifestEntry {
            path: codex_agent_task_store::REPOSITORY_WIDE_PATH.to_string(),
            content_hash: Some(head),
            existed: true,
        },
        codex_agent_task_store::WorkspaceManifestEntry {
            path: "covered.rs".to_string(),
            content_hash: Some(sha256_file(&covered_path).await.expect("hash covered path")),
            existed: true,
        },
    ];

    let (current, file_observations, head_observations) =
        current_typed_validation_proofs_with_observation_counts(
            Some(&repo),
            "workspace-manifest",
            vec![proof.clone(), proof],
        )
        .await;

    assert_eq!(current.len(), 2);
    assert_eq!(file_observations, 1);
    assert_eq!(head_observations, 1);
}

#[tokio::test]
async fn typed_child_validation_receipt_satisfies_final_proof_without_root_command_receipt() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    let covered_path = repo.join("codex-rs/core/src/task_evidence.rs");
    tokio::fs::create_dir_all(covered_path.parent().expect("covered path parent"))
        .await
        .expect("create covered path parent");
    tokio::fs::write(&covered_path, b"validated child contents")
        .await
        .expect("write covered path");
    let document = ledger
        .document
        .lock()
        .await
        .as_ref()
        .expect("document")
        .clone();
    assert!(document.command_receipts.is_empty());
    let basis = completion_candidate_basis(&document, &final_proof_input());
    let plan = ValidationPlanV1 {
        plan_id: "typed-plan".to_string(),
        basis_id: basis.basis_id.clone(),
        steps: vec![ValidationPlanStepV1 {
            step_id: "typed-step".to_string(),
            obligation_id: "typed-obligation".to_string(),
            argv: vec![
                "cargo".to_string(),
                "test".to_string(),
                "typed-proof".to_string(),
            ],
            covered_paths: vec!["codex-rs/core/src/task_evidence.rs".to_string()],
            covered_contracts: vec!["typed-child-final-proof".to_string()],
            timeout_ms: 120_000,
            semantic_timeout: false,
            batch_group: 0,
        }],
        ..ValidationPlanV1::default()
    };
    let candidate = completion_candidate_for(&basis, &plan);
    let mut proof = typed_validation_proof_fixture(&repo, 7, &plan.steps[0]);
    proof.covered_manifest = vec![codex_agent_task_store::WorkspaceManifestEntry {
        path: plan.steps[0].covered_paths[0].clone(),
        content_hash: Some(sha256_file(&covered_path).await.expect("hash covered path")),
        existed: true,
    }];
    let child_cwd = repo.join("codex-rs").to_string_lossy().into_owned();
    proof.recorded_cwd.clone_from(&child_cwd);
    proof.validation_result.proof_key.cwd = child_cwd;
    let proofs = current_typed_validation_proofs(
        Some(&repo),
        &basis.workspace_manifest_identity,
        vec![proof],
    )
    .await;

    let (observations, launch_count, process_ns) =
        current_final_proof_observations(&document, &basis, &candidate, &plan, &proofs);

    assert_eq!(observations.len(), 1);
    assert_eq!(observations[0].plan_step_id, "typed-step");
    assert!(observations[0].successful);
    assert!(observations[0].complete_identity);
    assert_eq!(
        observations[0].retained_output_ref.as_deref(),
        Some("artifact://child-output")
    );
    assert_eq!(launch_count, 0, "child proof is not a root process launch");
    assert_eq!(process_ns, 0, "child proof is not root process time");

    tokio::fs::write(&covered_path, b"mutated after child validation")
        .await
        .expect("mutate covered path");
    assert!(
        current_typed_validation_proofs(Some(&repo), &basis.workspace_manifest_identity, proofs,)
            .await
            .is_empty(),
        "a child receipt cannot cross a covered-manifest mutation"
    );
}

#[tokio::test]
async fn typed_child_validation_receipt_rejects_stale_or_scope_mismatched_proof() {
    let (temp, repo, ledger) = ledger_fixture().await;
    let document = ledger
        .document
        .lock()
        .await
        .as_ref()
        .expect("document")
        .clone();
    let basis = completion_candidate_basis(&document, &final_proof_input());
    let plan = ValidationPlanV1 {
        plan_id: "typed-plan".to_string(),
        basis_id: basis.basis_id.clone(),
        steps: vec![ValidationPlanStepV1 {
            step_id: "typed-step".to_string(),
            obligation_id: "typed-obligation".to_string(),
            argv: vec![
                "cargo".to_string(),
                "test".to_string(),
                "typed-proof".to_string(),
            ],
            covered_paths: vec!["codex-rs/core/src/task_evidence.rs".to_string()],
            covered_contracts: vec!["typed-child-final-proof".to_string()],
            timeout_ms: 120_000,
            semantic_timeout: false,
            batch_group: 0,
        }],
        ..ValidationPlanV1::default()
    };
    let candidate = completion_candidate_for(&basis, &plan);
    let mut stale = typed_validation_proof_fixture(&repo, 7, &plan.steps[0]);
    stale.validation_end_epoch = 6;
    assert!(
        current_final_proof_observations(&document, &basis, &candidate, &plan, &[stale])
            .0
            .is_empty()
    );

    let mut scope_mismatched = typed_validation_proof_fixture(&repo, 7, &plan.steps[0]);
    scope_mismatched.validation_result.route.leaves[0]
        .covered_paths
        .clear();
    assert!(
        current_final_proof_observations(
            &document,
            &basis,
            &candidate,
            &plan,
            &[scope_mismatched],
        )
        .0
        .is_empty()
    );

    let mut wrong_repository = typed_validation_proof_fixture(&repo, 7, &plan.steps[0]);
    wrong_repository.validation_result.proof_key.repository =
        temp.path().to_string_lossy().into_owned();
    assert!(
        current_final_proof_observations(
            &document,
            &basis,
            &candidate,
            &plan,
            &[wrong_repository],
        )
        .0
        .is_empty()
    );
}

#[tokio::test]
async fn deterministic_validation_plan_batches_run_all_without_generation() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    let mut item = plan_item("focused", StepStatus::Completed);
    let mut route = focused_validation_route(vec!["src/focused.rs".to_string()]);
    route.ordering = ValidationRouteOrdering::RunAll;
    let mut second = route.leaves[0].clone();
    second.argv.push("second".to_string());
    route.leaves.push(second);
    item.validation_route = Some(route);
    ledger.record_plan_update(&plan_with(vec![item])).await;
    let guard = ledger.document.lock().await;
    let document = guard.as_ref().expect("document");
    let basis = completion_candidate_basis(document, &final_proof_input());
    let plan = validation_plan_for_basis(document, &basis);
    assert!(!plan.ambiguous_or_unmappable);
    assert!(!plan.resolution_generation_used);
    assert_eq!(plan.steps.len(), 2);
    assert!(plan.steps.iter().all(|step| step.batch_group == 0));
}

#[test]
fn incomplete_legacy_proof_is_not_reusable_and_failure_fingerprint_invalidates() {
    let basis = CompletionCandidateBasisV1 {
        basis_id: "basis-a".to_string(),
        ..CompletionCandidateBasisV1::default()
    };
    let plan = ValidationPlanV1 {
        plan_id: "plan-a".to_string(),
        basis_id: basis.basis_id.clone(),
        steps: vec![ValidationPlanStepV1 {
            step_id: "step-a".to_string(),
            obligation_id: "obligation-a".to_string(),
            ..ValidationPlanStepV1::default()
        }],
        ..ValidationPlanV1::default()
    };
    let candidate = completion_candidate_for(&basis, &plan);
    let legacy = FinalProofObservationV1 {
        candidate_id: candidate.candidate_id.clone(),
        plan_step_id: "step-a".to_string(),
        obligation_id: "obligation-a".to_string(),
        successful: true,
        complete_identity: false,
        ..FinalProofObservationV1::default()
    };
    let missing = missing_or_failed_obligations(&candidate, &plan, &[legacy], 0);
    assert_eq!(missing, vec!["obligation-a".to_string()]);
    let first = completion_failure_fingerprint(3, &candidate, &missing, &[], None);
    let evidence_changed = completion_failure_fingerprint(4, &candidate, &missing, &[], None);
    let obligation_changed =
        completion_failure_fingerprint(3, &candidate, &["obligation-b".to_string()], &[], None);
    assert_ne!(first.fingerprint, evidence_changed.fingerprint);
    assert_ne!(first.fingerprint, obligation_changed.fingerprint);
}

#[test]
fn stale_evidence_revision_proof_is_not_reusable() {
    let basis = CompletionCandidateBasisV1 {
        basis_id: "basis-current".to_string(),
        ..CompletionCandidateBasisV1::default()
    };
    let plan = ValidationPlanV1 {
        plan_id: "plan-current".to_string(),
        basis_id: basis.basis_id.clone(),
        steps: vec![ValidationPlanStepV1 {
            step_id: "step-current".to_string(),
            obligation_id: "obligation-current".to_string(),
            ..ValidationPlanStepV1::default()
        }],
        ..ValidationPlanV1::default()
    };
    let candidate = completion_candidate_for(&basis, &plan);
    let stale = FinalProofObservationV1 {
        candidate_id: candidate.candidate_id.clone(),
        plan_step_id: "step-current".to_string(),
        obligation_id: "obligation-current".to_string(),
        successful: true,
        complete_identity: true,
        evidence_revision: 3,
        ..FinalProofObservationV1::default()
    };

    assert_eq!(
        missing_or_failed_obligations(&candidate, &plan, &[stale], 4),
        vec!["obligation-current".to_string()]
    );
}

#[tokio::test]
async fn sealed_checkpoint_is_complete_and_finalization_is_exactly_memoized() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    let sealed = ledger
        .seal_final_proof_candidate(final_proof_input())
        .await
        .expect("KD4 final proof enabled");
    let (candidate, checkpoint, gate) = match sealed {
        FinalProofSealResultV1::Sealed {
            candidate,
            checkpoint,
            gate,
            ..
        } => (candidate, checkpoint, gate),
        other => panic!("expected sealed candidate, got {other:?}"),
    };
    assert_eq!(gate.status, TaskCompletionStatus::Passed);
    assert_eq!(checkpoint.candidate_id, candidate.candidate_id);
    assert!(!checkpoint.checkpoint_id.is_empty());
    assert!(!checkpoint.basis_id.is_empty());
    assert!(!checkpoint.validation_plan_id.is_empty());
    assert!(!checkpoint.diff_identity.is_empty());
    assert!(checkpoint.estimated_tokens <= 10_000);
    assert!(
        ledger
            .memoized_finalization_result("turn-1")
            .await
            .is_none()
    );
    assert!(
        ledger
            .record_finalization_result(
                "turn-1".to_string(),
                "final answer".to_string(),
                true,
                true,
            )
            .await
    );
    assert_eq!(
        ledger
            .memoized_finalization_result("turn-1")
            .await
            .as_deref(),
        Some("final answer")
    );
    assert!(ledger.completion_recovery_intent("turn-2").await.is_none());
    {
        let mut document = ledger.document.lock().await;
        document
            .as_mut()
            .expect("document")
            .final_proof
            .basis
            .as_mut()
            .expect("basis")
            .implementation_identity = "stale-implementation".to_string();
    }
    assert!(ledger.completion_recovery_intent("turn-1").await.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn implemented_below_ignored_above_failed_finalization_memo_write_is_not_visible() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    let sealed = ledger
        .seal_final_proof_candidate(final_proof_input())
        .await
        .expect("KD4 final proof enabled");
    assert!(matches!(sealed, FinalProofSealResultV1::Sealed { .. }));
    let ledger = Arc::new(ledger);
    let (started, release) = install_persistence_test_control(&ledger, true);

    let memo_ledger = Arc::clone(&ledger);
    let memo = tokio::spawn(async move {
        memo_ledger
            .record_finalization_result(
                "turn-1".to_string(),
                "final answer".to_string(),
                true,
                true,
            )
            .await
    });
    wait_persistence_barrier(started).await;
    wait_persistence_barrier(release).await;

    assert!(!memo.await.expect("memo task"));
    assert!(
        ledger
            .memoized_finalization_result("turn-1")
            .await
            .is_none()
    );
    assert!(ledger.current_finalization_memo_identity().await.is_none());
    assert!(ledger.completion_recovery_intent("turn-1").await.is_none());
}

#[tokio::test]
async fn reviewer_infrastructure_memo_is_exact_to_candidate_config_and_condition() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    assert!(
        ledger
            .record_reviewer_infrastructure_memo(
                "candidate-a".to_string(),
                "dossier-a".to_string(),
                "reviewer-a".to_string(),
                "capacity-unavailable".to_string(),
                "capacity".to_string(),
            )
            .await
    );
    assert!(
        ledger
            .reviewer_infrastructure_memo_matches(
                "candidate-a",
                "dossier-a",
                "reviewer-a",
                "capacity-unavailable",
            )
            .await
    );
    assert!(
        !ledger
            .reviewer_infrastructure_memo_matches(
                "candidate-b",
                "dossier-a",
                "reviewer-a",
                "capacity-unavailable",
            )
            .await
    );
    assert!(
        !ledger
            .reviewer_infrastructure_memo_matches(
                "candidate-a",
                "dossier-a",
                "reviewer-b",
                "capacity-unavailable",
            )
            .await
    );
    assert!(
        !ledger
            .reviewer_infrastructure_memo_matches(
                "candidate-a",
                "dossier-a",
                "reviewer-a",
                "capacity-available",
            )
            .await
    );
}

fn user_history_message(message_id: &str, text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: Some(codex_protocol::ResponseItemId::from_server(
            message_id.to_string(),
        )),
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

#[tokio::test]
async fn cwd_rebinds_task_evidence_when_entering_and_leaving_kd4_repositories() {
    let temp = tempfile::tempdir().expect("tempdir");
    let outside = temp.path().join("outside");
    let repo = temp.path().join("repo");
    let codex_home = temp.path().join("home");
    tokio::fs::create_dir_all(&outside)
        .await
        .expect("outside dir");
    tokio::fs::create_dir_all(repo.join(".git"))
        .await
        .expect("git dir");
    tokio::fs::write(repo.join("kd4_features.toml"), "# fixture")
        .await
        .expect("manifest");

    let ledger = TaskEvidenceLedger::load_or_new(codex_home, ThreadId::new(), &outside).await;
    assert!(!ledger.allows_kd4_completion());
    assert_eq!(ledger.repository_root(), None);

    ledger.rebind_to_cwd(&repo).await;
    assert!(ledger.allows_kd4_completion());
    assert_eq!(ledger.repository_root().as_deref(), Some(repo.as_path()));
    assert!(
        ledger
            .record_user_sources("msg_entered", &[text_input("fix the integration")])
            .await
    );

    ledger.rebind_to_cwd(&outside).await;
    assert!(!ledger.allows_kd4_completion());
    assert_eq!(ledger.repository_root(), None);
    assert_eq!(ledger.bound_repo_root(), None);
    assert_eq!(ledger.evidence_path(), None);
    assert!(ledger.finalization_advisory().await.is_none());

    ledger.rebind_to_cwd(&repo).await;
    assert!(ledger.allows_kd4_completion());
    let guard = ledger.document.lock().await;
    let evidence = guard
        .as_ref()
        .and_then(|document| document.completion_review_v2.as_ref())
        .expect("reloaded repository evidence");
    assert!(evidence.source_records.values().any(|source| {
        source.message_id == "msg_entered" && source.exact_material == "fix the integration"
    }));
}

#[tokio::test]
async fn rollback_history_prunes_removed_sources_and_invalidates_derived_state() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    assert!(
        ledger
            .record_user_sources("msg_keep", &[text_input("keep this requirement")])
            .await
    );
    assert!(
        ledger
            .record_user_sources("msg_remove", &[text_input("remove this requirement")])
            .await
    );
    ledger
        .record_plan_update(&plan_with(vec![plan_item(
            "derived-work",
            StepStatus::InProgress,
        )]))
        .await;

    assert!(
        ledger
            .reconcile_rollback_history(&[user_history_message(
                "msg_keep",
                "keep this requirement",
            )])
            .await
    );

    let guard = ledger.document.lock().await;
    let document = guard.as_ref().expect("task evidence document");
    let evidence = document
        .completion_review_v2
        .as_ref()
        .expect("completion review ledger");
    let active_sources = evidence
        .source_records
        .values()
        .filter(|source| source.completion_epoch == evidence.completion_epoch)
        .collect::<Vec<_>>();
    assert_eq!(active_sources.len(), 1);
    assert_eq!(active_sources[0].message_id, "msg_keep");
    assert_eq!(active_sources[0].exact_material, "keep this requirement");
    assert!(document.plan.is_empty());
    assert!(document.edit_receipts.is_empty());
    assert!(document.command_receipts.is_empty());
    assert!(document.completion_review_receipts.is_empty());
    assert!(document.completion.is_none());
    assert_eq!(
        evidence
            .active_review_cycle
            .as_ref()
            .map(|cycle| cycle.phase),
        Some(CompletionReviewCyclePhase::ClassificationPending)
    );
}

#[tokio::test]
async fn fork_inherits_only_retained_parent_sources_under_child_identity() {
    let temp = tempfile::tempdir().expect("tempdir");
    let first_repo = temp.path().join("first-repo");
    let repo = temp.path().join("current-repo");
    let codex_home = temp.path().join("home");
    tokio::fs::create_dir_all(first_repo.join(".git"))
        .await
        .expect("first git dir");
    tokio::fs::write(first_repo.join("kd4_features.toml"), "# fixture")
        .await
        .expect("first manifest");
    tokio::fs::create_dir_all(repo.join(".git"))
        .await
        .expect("git dir");
    tokio::fs::write(repo.join("kd4_features.toml"), "# fixture")
        .await
        .expect("manifest");
    let parent_thread_id = ThreadId::new();
    let child_thread_id = ThreadId::new();
    let parent =
        TaskEvidenceLedger::load_or_new(codex_home.clone(), parent_thread_id, &first_repo).await;
    parent.rebind_to_cwd(&repo).await;
    assert!(
        parent
            .record_user_sources(
                "msg_keep",
                &[
                    text_input("inherited requirement"),
                    UserInput::Mention {
                        name: "inherited attachment".to_string(),
                        path: "plugin://inherited@example".to_string(),
                    },
                ],
            )
            .await
    );
    assert!(
        parent
            .record_user_sources("msg_remove", &[text_input("excluded requirement")])
            .await
    );
    let parent_source_id = {
        let guard = parent.document.lock().await;
        guard
            .as_ref()
            .and_then(|document| document.completion_review_v2.as_ref())
            .and_then(|evidence| {
                evidence
                    .source_records
                    .values()
                    .find(|source| source.message_id == "msg_keep")
            })
            .map(|source| source.source_id.clone())
            .expect("parent source")
    };

    let child = TaskEvidenceLedger::load_or_new(codex_home, child_thread_id, &repo).await;
    assert!(
        child
            .inherit_forked_history(
                parent_thread_id,
                &[user_history_message("msg_keep", "inherited requirement")],
            )
            .await
    );

    let guard = child.document.lock().await;
    let document = guard.as_ref().expect("child evidence document");
    let evidence = document
        .completion_review_v2
        .as_ref()
        .expect("child completion review ledger");
    let active_sources = evidence
        .source_records
        .values()
        .filter(|source| source.completion_epoch == evidence.completion_epoch)
        .collect::<Vec<_>>();
    assert_eq!(evidence.root_task_id, child_thread_id.to_string());
    assert_eq!(active_sources.len(), 2);
    assert!(
        active_sources
            .iter()
            .all(|source| source.message_id == "msg_keep")
    );
    let inherited_text = active_sources
        .iter()
        .find(|source| source.source_kind == UserSourceKind::Text)
        .expect("inherited text source");
    assert_eq!(inherited_text.exact_material, "inherited requirement");
    assert_ne!(inherited_text.source_id, parent_source_id);
    assert!(active_sources.iter().any(|source| {
        source.source_kind == UserSourceKind::Attachment
            && source.exact_material == "mention:inherited attachment:plugin://inherited@example"
    }));
    assert!(document.plan.is_empty());
}
