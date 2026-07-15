use crate::model::CommandResultV2;
use crate::model::FinalizedCommandResult;
use crate::model::FinalizedVerification;
use crate::model::LaunchErrorKind;
use crate::model::LogState;
use crate::model::PlanEnvelopeV2;
use crate::model::PlanMode;
use crate::model::Verdict;
use std::collections::HashSet;

pub fn finalize_plan(plan: PlanEnvelopeV2, results: Vec<CommandResultV2>) -> FinalizedVerification {
    if plan.mode == PlanMode::Plan {
        let verdict = plan.verdict.unwrap_or(Verdict::Planned);
        return finish(plan, Vec::new(), verdict, false, None);
    }
    if let Some(verdict) = plan.verdict {
        return finish(plan, Vec::new(), verdict, false, None);
    }
    if plan.commands.is_empty() {
        return finish(plan, Vec::new(), Verdict::VerifiedNoProof, false, None);
    }

    if results.len() != plan.commands.len() {
        return tooling_error(
            plan,
            results,
            "execution result count does not match the planned command count",
        );
    }

    let mut seen = HashSet::new();
    let mut finalized = Vec::with_capacity(results.len());
    for (ordinal, (command, result)) in plan.commands.iter().zip(results).enumerate() {
        if result.schema_version != 2
            || result.invocation_id != plan.invocation_id
            || result.command_id != command.id
            || result.command_ordinal != ordinal
            || !seen.insert(result.command_id.clone())
        {
            return tooling_error(
                plan,
                finalized
                    .into_iter()
                    .map(|entry: FinalizedCommandResult| entry.raw)
                    .chain(std::iter::once(result))
                    .collect(),
                "execution result identity or ordering does not match the plan",
            );
        }

        let status = classify_command_result(&result);
        finalized.push(FinalizedCommandResult {
            raw: result,
            status,
        });
        if status != Verdict::Verified {
            return finish(plan, finalized, status, false, None);
        }
    }

    let cache_eligible = finalized
        .iter()
        .all(|result| result.raw.log_state == LogState::Complete);
    finish(plan, finalized, Verdict::Verified, cache_eligible, None)
}

fn classify_command_result(result: &CommandResultV2) -> Verdict {
    if matches!(
        result.log_state,
        LogState::IoFailure | LogState::FramingFailure | LogState::IntegrityFailure
    ) {
        return Verdict::ToolingError;
    }
    if result.timed_out || result.cancelled {
        return Verdict::Inconclusive;
    }
    if result.log_state == LogState::IncompleteAfterTermination {
        return Verdict::ToolingError;
    }
    match result.launch_error {
        Some(LaunchErrorKind::CommandNotFound) => Verdict::Failed,
        Some(LaunchErrorKind::UnsupportedPath) => Verdict::Inconclusive,
        Some(LaunchErrorKind::PermissionDenied | LaunchErrorKind::Other) => Verdict::ToolingError,
        None if result.runner_error.is_some() => Verdict::ToolingError,
        None => match result.exit_code {
            Some(0) => Verdict::Verified,
            Some(_) => Verdict::Failed,
            None => Verdict::ToolingError,
        },
    }
}

fn tooling_error(
    plan: PlanEnvelopeV2,
    results: Vec<CommandResultV2>,
    message: &str,
) -> FinalizedVerification {
    let finalized = results
        .into_iter()
        .map(|raw| FinalizedCommandResult {
            raw,
            status: Verdict::ToolingError,
        })
        .collect();
    finish(
        plan,
        finalized,
        Verdict::ToolingError,
        false,
        Some(message.to_string()),
    )
}

fn finish(
    plan: PlanEnvelopeV2,
    results: Vec<FinalizedCommandResult>,
    verdict: Verdict,
    cache_eligible: bool,
    finalization_error: Option<String>,
) -> FinalizedVerification {
    FinalizedVerification {
        plan,
        results,
        verdict,
        exit_code: verdict.exit_code(),
        cache_eligible: cache_eligible && verdict == Verdict::Verified,
        finalization_error,
    }
}

#[cfg(test)]
#[path = "finalize_tests.rs"]
mod tests;
