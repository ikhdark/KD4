//! Advisory retry memory in the existing tool-history journal, not admission policy.
use codex_protocol::tool_failure::ToolFailure;
use codex_protocol::tool_failure::ToolFailureCode;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

use crate::FunctionCallError;
use crate::tools::context::ToolCallSource;
use crate::tools::context::ToolInvocation;
use crate::tools::registry::AnyToolResult;

/// Preserve actual parameters (including computed nested arguments) and each
/// original error. Normal result bytes remain with their existing output owner.
/// Diagnostics are never grouping keys.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct OperationAttempt {
    pub(crate) thread_id: String,
    pub(crate) turn_id: String,
    pub(crate) call_id: String,
    source: Value,
    strategy: codex_tools::ToolName,
    pub(crate) operation: String,
    input_revision: Option<String>,
    #[serde(default)]
    effective_input_known: bool,
    parameters: Option<Value>,
    backend: Value,
    configuration_revision: String,
    observed_state_revision: String,
    outcome: String,
    failure_code: Option<ToolFailureCode>,
    fingerprint: Option<String>,
    original_failure: Option<ToolFailure>,
    #[serde(skip)]
    pub(crate) recovery_deferred: bool,
}

fn digest(value: Value) -> String {
    crate::tool_history::sha256(
        codex_config::schema::canonicalize(&value)
            .to_string()
            .as_bytes(),
    )
}

impl OperationAttempt {
    pub(crate) async fn capture(
        invocation: &ToolInvocation,
        contract_revision: Option<&str>,
        provider_authority: Option<&str>,
    ) -> Self {
        let turn = &invocation.step_context.turn;
        // Release the tracker before taking session/history/persistence locks.
        let mutation = invocation.tracker.lock().await.current_mutation_revision();
        let source = match &invocation.source {
            ToolCallSource::Direct => serde_json::json!({"kind": "direct"}),
            ToolCallSource::CodeMode {
                cell_id,
                parent_call_id,
                runtime_tool_call_id,
                ..
            } => serde_json::json!({"kind": "code_mode", "cell_id": cell_id,
                    "parent_call_id": parent_call_id, "runtime_tool_call_id": runtime_tool_call_id}),
        };
        let backend = serde_json::json!({
            "provider_authority": provider_authority,
            "configured_environments": invocation.step_context.environments.turn_environments
                .iter().map(|environment| serde_json::json!({
                    "id": environment.environment_id,
                    "remote": environment.environment.is_remote(),
                })).collect::<Vec<_>>(),
            // The requested route is in the exact input identity. This layer
            // cannot certify which physical server handled an external call.
            "physical_backend": "unknown",
        });
        let parameters =
            crate::tools::registry::canonical_tool_invocation_value(&invocation.payload, None);
        let input_revision = parameters.as_ref().map(|value| digest(value.clone()));
        Self {
            thread_id: invocation.session.thread_id.to_string(),
            turn_id: turn.sub_id.clone(),
            call_id: invocation.call_id.clone(),
            source,
            strategy: invocation.tool_name.clone(),
            operation: digest(
                serde_json::json!({"tool": invocation.tool_name, "backend": backend}),
            ),
            input_revision,
            effective_input_known: true,
            parameters,
            backend,
            configuration_revision: digest(serde_json::json!({
                "contract": contract_revision, "cwd": turn.cwd(),
                "permissions": turn.permission_profile(),
                "approval": turn.approval_policy.get(),
                "windows_sandbox": turn.windows_sandbox_level,
            })),
            // Counters are turn-local. Including their scope prevents false
            // equivalence after resume/fork or the next turn resets them.
            observed_state_revision: digest(serde_json::json!({
                "thread": invocation.session.thread_id.to_string(), "turn": turn.sub_id,
                "input": invocation.step_context.user_intent_epoch, "mutation": mutation,
            })),
            outcome: "unknown".into(),
            failure_code: None,
            fingerprint: None,
            original_failure: None,
            recovery_deferred: false,
        }
    }

    pub(crate) fn same_input(&self, other: &Self) -> bool {
        self.operation == other.operation
            && self.input_revision.is_some()
            && self.input_revision == other.input_revision
    }

    pub(crate) fn mark_requested_input(&mut self) {
        self.effective_input_known = false;
    }

    pub(crate) fn settle(&mut self, result: &Result<AnyToolResult, FunctionCallError>) {
        match result {
            Ok(result) => self.settle_output(result),
            Err(error) => {
                self.outcome = "failure".into();
                self.record_failure(error.failure());
            }
        }
    }

    pub(crate) fn settle_output(&mut self, result: &AnyToolResult) {
        self.outcome = match result.outcome_for_logging() {
            codex_tools::ToolOutputOutcome::Success => "success",
            codex_tools::ToolOutputOutcome::Failure => "failure",
            codex_tools::ToolOutputOutcome::TimedOut => "timed_out",
            codex_tools::ToolOutputOutcome::Skipped => "skipped",
            codex_tools::ToolOutputOutcome::Yielded => "yielded",
        }
        .into();
        if let Some(failure) = result.result.tool_failure() {
            self.record_failure(failure);
        }
    }

    fn record_failure(&mut self, failure: ToolFailure) {
        self.failure_code = Some(failure.code);
        // These coarse/legacy categories do not establish a cause. Keep
        // each instance distinct rather than hashing its diagnostic prose.
        if self.effective_input_known
            && self.input_revision.is_some()
            && !matches!(
                failure.code,
                ToolFailureCode::Failed | ToolFailureCode::UncertainEffects
            )
        {
            self.fingerprint = Some(digest(serde_json::json!({
                "version": 1, "operation": self.operation, "input": self.input_revision,
                "configuration": self.configuration_revision,
                "observed_state": self.observed_state_revision, "code": failure.code,
                "fields": failure.field_errors,
            })));
        }
        self.original_failure = Some(failure);
    }

    pub(crate) fn advisory(&self, previous: &[&Self]) -> Value {
        let unsuccessful =
            |attempt: &&&Self| matches!(attempt.outcome.as_str(), "failure" | "timed_out");
        let same_state = |attempt: &&&Self| {
            attempt.effective_input_known
                && self.effective_input_known
                && attempt.observed_state_revision == self.observed_state_revision
                && attempt.configuration_revision == self.configuration_revision
        };
        let failed_count = previous.iter().filter(unsuccessful).count();
        let unchanged_failed_count = previous.iter().filter(unsuccessful).filter(same_state).count();
        // A later success supersedes uncertainty for this exact observed state.
        // Unknown effective input cannot establish an unchanged retry.
        let recovery_required = previous.iter().rev().find(same_state)
            .filter(|attempt| unsuccessful(attempt))
            .and_then(|attempt| attempt.original_failure.as_ref())
            .is_some_and(|failure| failure.effects == codex_protocol::tool_failure::ToolEffectsState::Unknown
                || failure.execution_state == codex_protocol::tool_failure::ToolExecutionState::Unknown
                || !failure.live_operations.is_empty());
        // All these records have the same exact strategy and parameters. Surface
        // its latest relevant failure and occurrence count, not an ever-growing
        // transcript. Every original instance remains in the history journal.
        let last_failure = previous
            .iter()
            .rev()
            .filter(unsuccessful)
            .find(same_state)
            .or_else(|| previous.iter().rev().find(unsuccessful));
        serde_json::json!({
            "version": 1, "instance": {
                "thread_id": self.thread_id, "turn_id": self.turn_id, "call_id": self.call_id,
                "source": self.source, "strategy": self.strategy, "operation": self.operation,
                "input_revision": self.input_revision, "backend": self.backend,
                "effective_input_known": self.effective_input_known,
                "configuration_revision": self.configuration_revision,
                "observed_state_revision": self.observed_state_revision,
                "outcome": self.outcome, "failure_code": self.failure_code,
                "fingerprint": self.fingerprint,
            },
            "grouping": "typed_failure_context_not_proven_shared_cause",
            "state_coverage": "observed_revisions_only_external_changes_unknown",
            "parameters_and_original_result": {"thread_id": self.thread_id,
                "turn_id": self.turn_id, "call_id": self.call_id},
            "previous_attempt_count": previous.len(),
            "previous_unsuccessful_attempt_count": failed_count,
            "unchanged_unsuccessful_attempt_count": unchanged_failed_count,
            "recovery_required": recovery_required,
            "last_unsuccessful_attempt": last_failure.map(|attempt| serde_json::json!({
                "thread_id": attempt.thread_id, "turn_id": attempt.turn_id,
                "call_id": attempt.call_id, "source": attempt.source,
                "strategy": attempt.strategy, "parameters_revision": attempt.input_revision,
                "outcome": attempt.outcome, "failure_code": attempt.failure_code,
                "fingerprint": attempt.fingerprint,
                "owner_failure": attempt.original_failure,
                "same_observed_state": attempt.effective_input_known && self.effective_input_known
                    && attempt.observed_state_revision == self.observed_state_revision
                    && attempt.configuration_revision == self.configuration_revision,
            })),
            "retry_policy": "memory_is_not_authorization_or_a_permanent_retry_ban; unchanged_uncertain_attempts_require_owner_recovery_first; changed_state_or_settled_no_effects_transient_failures_allow_reconsideration",
        })
    }

    /// Reuse the original owner's recovery, not a generic retry prohibition.
    /// Transient, settled no-effects failures remain eligible for retry.
    pub(crate) fn recovery_preflight(&mut self, advisory: &Value) -> Option<ToolFailure> {
        if advisory["recovery_required"] != true {
            return None;
        }
        let previous = &advisory["last_unsuccessful_attempt"];
        let owner: ToolFailure = serde_json::from_value(previous["owner_failure"].clone()).ok()?;
        // Repeating an owner-issued continuation is recovery, not a replacement
        // launch. Compare the verified handle's arguments, not diagnostic text.
        if owner.live_operations.iter().any(|handle| {
            self.strategy.to_string() == handle.tool
                && self.parameters.as_ref().is_some_and(|parameters| {
                    parameters["kind"] == "function" && parameters["arguments"] == handle.arguments
                })
        }) {
            return None;
        }
        let mut failure = ToolFailure::no_effects(ToolFailureCode::UncertainEffects,
            "Unchanged retry was not started: recover the previous invocation's uncertain effects or live operation first.");
        failure.safe_next_actions = owner.safe_next_actions;
        failure.live_operations = owner.live_operations;
        failure.details = Some(serde_json::json!({
            "retry_path_preflight": true,
            "prior_effects": owner.effects,
            "operation_history": advisory,
        }));
        self.recovery_deferred = true;
        Some(failure)
    }
}

pub(crate) fn annotate_failure(failure: &mut ToolFailure, advisory: Value) {
    let details = failure.details.get_or_insert_with(|| serde_json::json!({}));
    if !details.is_object() {
        *details = serde_json::json!({"owner_details": details.take()});
    }
    details["operation_history"] = advisory;
}
