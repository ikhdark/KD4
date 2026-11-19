//! Applicability of already captured native source evidence. Freshness checks
//! never create acquisition records or establish semantic correctness.

use super::effective_criteria;
use crate::{AgentStatusClaim, Assignment, Attempt, CriterionAssessment, CriterionEvidenceKind,
    CriterionEvidenceRequirement, CriterionResult, CriterionStatus, EvidenceFreshness,
    SourceInspectionResult, StoreError, StoreResult, ValidationCall};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::PathBuf;

// Match the native producer's bound. Unknown, unreadable, changed and oversized
// targets fail closed; checking a hash never delivers source or acquires it.
const MAX_SOURCE_BYTES: u64 = 8 * 1024 * 1024;

async fn current_fingerprint(root: PathBuf, path: String) -> Option<(String, u64)> {
    tokio::task::spawn_blocking(move || {
        let root = std::fs::canonicalize(root).ok()?;
        let target = root.join(path);
        let canonical = std::fs::canonicalize(&target).ok()?;
        if !canonical.starts_with(&root) { return None; }
        let (mut file, before) = codex_file_system::open_confined_file_with_metadata(&root, &canonical).ok()?;
        if !before.is_file() || before.len() > MAX_SOURCE_BYTES { return None; }
        let mut digest = Sha256::new();
        let mut bytes = 0_u64;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let count = file.read(&mut buffer).ok()?;
            if count == 0 { break; }
            bytes = bytes.checked_add(count as u64)?;
            if bytes > MAX_SOURCE_BYTES { return None; }
            digest.update(&buffer[..count]);
        }
        let after = file.metadata().ok()?;
        let named = std::fs::metadata(&target).ok()?;
        if bytes != before.len() || bytes != after.len()
            || before.modified().ok()? != after.modified().ok()?
            || named.len() != after.len() || named.modified().ok()? != after.modified().ok()?
            || std::fs::canonicalize(&target).ok()? != canonical
        {
            return None;
        }
        Some((hex::encode(digest.finalize()), bytes))
    }).await.ok().flatten()
}

pub(super) async fn assess_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    assignment: &Assignment,
    attempt: &Attempt,
    results: &[CriterionResult],
    cited_call_ids: &[String],
    allow_uncited: bool,
) -> StoreResult<Vec<CriterionAssessment>> {
    assess_tx_with_freshness(transaction, assignment, attempt, results, cited_call_ids, allow_uncited)
        .await.map(|(assessments, _)| assessments)
}

pub(super) async fn assess_tx_with_freshness(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    assignment: &Assignment,
    attempt: &Attempt,
    results: &[CriterionResult],
    cited_call_ids: &[String],
    allow_uncited: bool,
) -> StoreResult<(Vec<CriterionAssessment>, bool)> {
    if !effective_criteria(assignment, attempt.amendment.as_ref()).iter()
        .any(|criterion| criterion.evidence_policy.as_ref()
            .is_some_and(|policy| policy.kind != CriterionEvidenceRequirement::ReportOnly))
    {
        return Ok((Vec::new(), false));
    }
    let calls = sqlx::query_scalar::<_, String>(
        "SELECT body_json FROM validation_calls WHERE attempt_id = ? ORDER BY julianday(json_extract(recorded_at, '$')), call_id",
    ).bind(attempt.attempt_id.to_string()).fetch_all(&mut **transaction).await?
        .into_iter().map(|body| super::decode::<ValidationCall>(&body)).collect::<StoreResult<Vec<_>>>()?;
    assess_with_calls_tx(
        transaction, assignment, attempt, results, cited_call_ids, allow_uncited, &calls,
    ).await
}

/// Reuse records loaded in the caller's transaction without caching applicability.
pub(super) async fn assess_with_calls_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    assignment: &Assignment,
    attempt: &Attempt,
    results: &[CriterionResult],
    cited_call_ids: &[String],
    allow_uncited: bool,
    calls: &[ValidationCall],
) -> StoreResult<(Vec<CriterionAssessment>, bool)> {
    let criteria = effective_criteria(assignment, attempt.amendment.as_ref()).iter()
        .filter(|criterion| criterion.evidence_policy.as_ref()
            .is_some_and(|policy| policy.kind != CriterionEvidenceRequirement::ReportOnly))
        .collect::<Vec<_>>();
    if criteria.is_empty() { return Ok((Vec::new(), false)); }
    let root = if criteria.iter().any(|criterion| criterion.evidence_policy.as_ref()
        .is_some_and(|policy| policy.kind == CriterionEvidenceRequirement::SourceInspection)) {
        sqlx::query_scalar::<_, String>(
        "SELECT canonical_root FROM assignment_repositories WHERE assignment_id = ?",
        ).bind(assignment.assignment_id.to_string()).fetch_optional(&mut **transaction).await?
    } else { None };
    let mut fingerprints = BTreeMap::new();
    let mut observes_live_source = false;
    let mut assessments = Vec::with_capacity(criteria.len());
    for criterion in criteria {
        let policy = criterion.evidence_policy.as_ref().ok_or_else(||
            StoreError::CorruptData("source criterion lost its policy".into()))?;
        let requirement = policy.required_evidence.as_deref().unwrap_or_default();
        let result = results.iter().find(|result| result.criterion_id == criterion.id);
        let reference = result.and_then(|result| result.evidence_ref.as_ref());
        if policy.kind == CriterionEvidenceRequirement::ValidationExecution {
            let expected = crate::required_command_argv(requirement);
            let owned = |call: &&ValidationCall| {
                call.attempt_id == attempt.attempt_id
                    && call.verified_evidence_kind() == Some(CriterionEvidenceKind::ValidationExecution)
                    && expected.as_ref().is_some_and(|expected| {
                        call.evidence.validation_result.as_ref()
                            .and_then(|value| value.get("argv"))
                            .and_then(|value| serde_json::from_value::<Vec<String>>(value.clone()).ok())
                            .as_ref() == Some(expected)
                    })
            };
            let call = if let Some(reference) = reference {
                calls.iter().filter(owned).find(|call| {
                    call.call_id == reference.call_id
                        && cited_call_ids.contains(&call.call_id)
                        && reference.workspace_id == assignment.workspace_id
                        && reference.kind == CriterionEvidenceKind::ValidationExecution
                        && call.evidence.end_epoch == Some(reference.evidence_epoch)
                })
            } else if allow_uncited {
                calls.iter().rfind(owned)
            } else { None };
            let satisfied = policy.freshness == EvidenceFreshness::Historical
                && (allow_uncited || result.is_some_and(|result| result.status == CriterionStatus::Passed))
                && assignment.required_evidence.iter().any(|required| required == requirement)
                && call.is_some();
            assessments.push(CriterionAssessment {
                criterion_id: criterion.id.clone(),
                satisfied,
                reason: if satisfied {
                    "successful owned exact-command execution; historical applicability only".into()
                } else {
                    let gap = if policy.freshness != EvidenceFreshness::Historical {
                        "current-freshness binding is unsupported".to_string()
                    } else {
                        reference_gap(assignment, attempt, requirement, result, reference,
                            cited_call_ids, allow_uncited, calls, CriterionEvidenceKind::ValidationExecution)
                            .unwrap_or_else(|| "cited call is not a successful owned run of the exact required command".into())
                    };
                    format!("validation_execution requires a cited successful owned exact-command receipt; current input binding is unsupported ({gap})")
                },
            });
            continue;
        }
        let call = if let Some(reference) = reference {
            calls.iter().find(|call| call.call_id == reference.call_id
                && cited_call_ids.contains(&call.call_id)
                && reference.workspace_id == assignment.workspace_id
                && reference.kind == CriterionEvidenceKind::SourceInspection
                && call.evidence.end_epoch == Some(reference.evidence_epoch))
        } else if allow_uncited {
            calls.iter().rev().find(|call| call.command_summary == requirement
                && call.verified_evidence_kind() == Some(CriterionEvidenceKind::SourceInspection))
        } else { None };
        let inspection = call.filter(|call| call.attempt_id == attempt.attempt_id
            && call.command_summary == requirement
            && call.verified_evidence_kind() == Some(CriterionEvidenceKind::SourceInspection))
            .and_then(|call| call.evidence.validation_result.as_ref())
            .and_then(|value| serde_json::from_value::<SourceInspectionResult>(value.clone()).ok())
            .filter(|inspection| requirement.strip_prefix("inspect:") == Some(inspection.path.as_str())
                && assignment.required_evidence.iter().any(|required| required == requirement));
        let mut satisfied = allow_uncited || result.is_some_and(|result| result.status == CriterionStatus::Passed);
        let reason = if let Some(inspection) = inspection.filter(|_| satisfied) {
            if policy.freshness == EvidenceFreshness::Historical {
                "complete native source acquisition; historical applicability only".to_string()
            } else {
                if !fingerprints.contains_key(&inspection.path) {
                    let fingerprint = match &root {
                        Some(root) => {
                            observes_live_source = true;
                            current_fingerprint(PathBuf::from(root), inspection.path.clone()).await
                        }
                        None => None,
                    };
                    fingerprints.insert(inspection.path.clone(), fingerprint);
                }
                satisfied = fingerprints.get(&inspection.path).and_then(Option::as_ref)
                    .is_some_and(|(hash, bytes)| hash == &inspection.source_sha256 && *bytes == inspection.bytes);
                if satisfied { "complete native source acquisition; current file hash matched at this observation".into() }
                else { "captured source is stale, unavailable or outside the bound repository".into() }
            }
        } else {
            satisfied = false;
            let gap = reference_gap(assignment, attempt, requirement, result, reference,
                cited_call_ids, allow_uncited, calls, CriterionEvidenceKind::SourceInspection)
                .unwrap_or_else(|| match call {
                    Some(call) if call.command_summary != requirement => format!(
                        "cited call {} acquired `{}`, not `{requirement}`", call.call_id, call.command_summary),
                    Some(call) if call.verified_evidence_kind() != Some(CriterionEvidenceKind::SourceInspection) => format!(
                        "cited call {} is not a successful complete native source acquisition", call.call_id),
                    _ => "the acquired path does not match the inspect:path obligation".into(),
                });
            format!("source_inspection requires a cited, successful, owned native acquisition for the exact inspect:path obligation ({gap})")
        };
        assessments.push(CriterionAssessment { criterion_id: criterion.id.clone(), satisfied, reason });
    }
    Ok((assessments, observes_live_source))
}

/// First unmet citation condition, so a rejected receipt names its correction.
/// Diagnostic only: acceptance is decided by the caller's own predicate.
#[allow(clippy::too_many_arguments)]
fn reference_gap(
    assignment: &Assignment,
    attempt: &Attempt,
    requirement: &str,
    result: Option<&CriterionResult>,
    reference: Option<&crate::CriterionEvidenceRef>,
    cited_call_ids: &[String],
    allow_uncited: bool,
    calls: &[ValidationCall],
    kind: CriterionEvidenceKind,
) -> Option<String> {
    if !allow_uncited && !result.is_some_and(|result| result.status == CriterionStatus::Passed) {
        return Some("criterion result is not passed".into());
    }
    if !assignment.required_evidence.iter().any(|required| required == requirement) {
        return Some(format!("`{requirement}` is not required evidence of the assignment"));
    }
    let Some(reference) = reference else {
        return Some(if allow_uncited {
            format!("no successful owned acquisition of `{requirement}` is recorded in attempt {}", attempt.attempt_id)
        } else {
            "the criterion result cites no evidence_ref".into()
        });
    };
    let Some(call) = calls.iter()
        .find(|call| call.call_id == reference.call_id && call.attempt_id == attempt.attempt_id)
    else {
        return Some(format!(
            "cited call {} is not a recorded call of the bound attempt {}; cite the criterion_evidence ref this attempt's call returned",
            reference.call_id, attempt.attempt_id
        ));
    };
    if !cited_call_ids.contains(&call.call_id) {
        return Some(format!("cited call {} is missing from validation_call_ids", call.call_id));
    }
    if reference.workspace_id != assignment.workspace_id {
        return Some(format!(
            "evidence_ref.workspace_id {} is not the assignment workspace {}",
            reference.workspace_id, assignment.workspace_id
        ));
    }
    if reference.kind != kind {
        return Some("evidence_ref.kind does not match the criterion's evidence policy".into());
    }
    if call.evidence.end_epoch != Some(reference.evidence_epoch) {
        return Some(format!(
            "evidence_epoch {} does not match call {}'s recorded end epoch {}",
            reference.evidence_epoch,
            call.call_id,
            call.evidence.end_epoch.map_or_else(|| "(none)".to_string(), |epoch| epoch.to_string())
        ));
    }
    None
}

pub(super) fn require_passed(
    assessments: &[CriterionAssessment],
    results: &[CriterionResult],
    status: AgentStatusClaim,
) -> StoreResult<()> {
    if let Some(assessment) = assessments.iter().find(|assessment| !assessment.satisfied
        && (status == AgentStatusClaim::Completed || results.iter().any(|result|
            result.criterion_id == assessment.criterion_id && result.status == CriterionStatus::Passed)))
    {
        return Err(StoreError::CriterionResultsInvalid(format!("{}: {}", assessment.criterion_id, assessment.reason)));
    }
    Ok(())
}
