//! One criterion satisfaction decision shared by sealing, reads and dependencies.
use crate::*;
use std::collections::HashMap;

pub(crate) fn assess_criteria(
    assignment: &Assignment,
    amendment: Option<&AttemptAmendment>,
    attempt_id: AttemptId,
    results: &[CriterionResult],
    listed_calls: &[String],
    calls: &[ValidationCall],
    current_sources: &HashMap<String, String>,
) -> Vec<CriterionAssessment> {
    assignment
        .effective_criteria(amendment)
        .iter()
        .map(|criterion| {
            let decision = || -> Result<(), &'static str> {
                let result = results
                    .iter()
                    .find(|result| result.criterion_id == criterion.id)
                    .ok_or("no reported result")?;
                if result.status != CriterionStatus::Passed {
                    return Err("not reported passed");
                }
                let policy = criterion
                    .evidence_policy
                    .as_ref()
                    .ok_or("legacy or undeclared evidence policy; claim only")?;
                if policy.kind == CriterionEvidenceRequirement::ReportOnly {
                    return Ok(());
                }
                let required = policy
                    .required_evidence
                    .as_ref()
                    .filter(|required| assignment.required_evidence.contains(required))
                    .ok_or("policy does not bind an immutable required observation")?;
                let reference = result
                    .evidence_ref
                    .as_ref()
                    .ok_or("no evidence reference")?;
                let call = calls
                    .iter()
                    .find(|call| call.call_id == reference.call_id)
                    .ok_or("referenced observation unavailable")?;
                let expected_kind = match policy.kind {
                    CriterionEvidenceRequirement::SourceInspection => {
                        CriterionEvidenceKind::SourceInspection
                    }
                    CriterionEvidenceRequirement::ValidationExecution => {
                        CriterionEvidenceKind::ValidationExecution
                    }
                    CriterionEvidenceRequirement::ReportOnly => unreachable!(),
                };
                if call.attempt_id != attempt_id
                    || !listed_calls.contains(&call.call_id)
                    || reference.workspace_id != assignment.workspace_id
                    || call.command_summary != *required
                    || reference.kind != expected_kind
                    || call.evidence.start_epoch != reference.evidence_epoch
                    || call.evidence.end_epoch != Some(reference.evidence_epoch)
                    || call.verified_evidence_kind() != Some(expected_kind)
                {
                    return Err(
                        "observation is not owned, successful and applicable to this contract",
                    );
                }
                if policy.freshness == EvidenceFreshness::Historical {
                    return Ok(());
                }
                if expected_kind == CriterionEvidenceKind::SourceInspection {
                    let source = call
                        .evidence
                        .validation_result
                        .as_ref()
                        .and_then(|value| {
                            serde_json::from_value::<SourceInspectionResult>(value.clone()).ok()
                        })
                        .ok_or("source binding unavailable")?;
                    return match current_sources.get(&source.path) {
                        Some(hash) if *hash == source.source_sha256 => Ok(()),
                        Some(_) => Err("source changed since the observation"),
                        None => Err("current source or workspace identity unavailable"),
                    };
                }
                // An argv, declared paths, exit status or epoch zero does not bind a
                // complete input graph, binary/configuration and environment. Until
                // the producer supplies that authority this is an honest unknown.
                Err("current execution input binding unavailable; historical execution only")
            };
            let result = decision();
            CriterionAssessment {
                criterion_id: criterion.id.clone(),
                satisfied: result.is_ok(),
                reason: result
                    .err()
                    .unwrap_or("declared evidence policy satisfied")
                    .into(),
            }
        })
        .collect()
}
