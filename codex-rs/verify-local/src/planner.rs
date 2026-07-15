use crate::model::PlanEnvelopeV2;
use crate::model::PlanMode;
use crate::model::PlanRequest;
use crate::model::RepositorySnapshot;
use crate::model::ScopeV2;
use crate::model::SkippedDecision;
use crate::model::Verdict;
use sha2::Digest;
use sha2::Sha256;

pub fn plan_verification(
    request: PlanRequest,
    snapshot: RepositorySnapshot,
) -> PlanEnvelopeV2 {
    let mode = request.mode.unwrap_or(PlanMode::Plan);
    let invocation_id = invocation_id(&request, &snapshot);
    let mut plan = PlanEnvelopeV2::new(mode, invocation_id);
    if !snapshot.complete {
        plan.verdict = Some(Verdict::Inconclusive);
        plan.skipped.extend(snapshot.fallback_reasons.into_iter().map(|reason| {
            SkippedDecision {
                item: "repository snapshot".to_string(),
                reason,
            }
        }));
        return plan;
    }

    let active_files = if request.changed.is_empty() {
        snapshot.records.into_iter().map(|record| record.path).collect()
    } else {
        request.changed
    };
    let source = if active_files.is_empty() {
        "empty"
    } else if request.staged {
        "staged"
    } else {
        "changed"
    };
    plan.scope = Some(ScopeV2 {
        scope_id: scope_id(&active_files),
        source: source.to_string(),
        active_files,
        ..ScopeV2::default()
    });
    if plan
        .scope
        .as_ref()
        .is_none_or(|scope| scope.active_files.is_empty())
    {
        plan.verdict = Some(Verdict::VerifiedNoProof);
    }
    plan
}

fn invocation_id(request: &PlanRequest, snapshot: &RepositorySnapshot) -> String {
    let mut hasher = Sha256::new();
    hasher.update(request.mode.unwrap_or(PlanMode::Plan).as_str().as_bytes());
    for path in &request.changed {
        hasher.update(path.as_bytes());
        hasher.update([0]);
    }
    for record in &snapshot.records {
        hasher.update(record.status.as_bytes());
        hasher.update([0]);
        hasher.update(record.path.as_bytes());
        hasher.update([0xff]);
    }
    format!("{:x}", hasher.finalize())
}

fn scope_id(paths: &[crate::model::RawPath]) -> String {
    if paths.is_empty() {
        return "empty".to_string();
    }
    let mut hasher = Sha256::new();
    for path in paths {
        hasher.update(path.as_bytes());
        hasher.update([b'\n']);
    }
    let digest = format!("{:x}", hasher.finalize());
    let first = paths[0]
        .as_utf8()
        .and_then(|path| path.rsplit('/').next())
        .unwrap_or("scope");
    let stem = first.split('.').next().filter(|stem| !stem.is_empty()).unwrap_or("scope");
    format!("{stem}-{}", &digest[..10])
}
