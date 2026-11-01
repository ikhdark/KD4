//! Persisted operation-attempt records and pure retry advice; capture remains in core.
use codex_protocol::tool_failure::{ToolFailure, ToolFailureCode};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct OperationAttempt {
    pub thread_id: String,
    pub turn_id: String,
    pub call_id: String,
    source: Value,
    strategy: codex_tools::ToolName,
    pub operation: String,
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
    pub recovery_deferred: bool,
}

pub struct OperationAttemptInput {
    pub thread_id: String,
    pub turn_id: String,
    pub call_id: String,
    pub source: Value,
    pub strategy: codex_tools::ToolName,
    pub operation: String,
    pub input_revision: Option<String>,
    pub effective_input_known: bool,
    pub parameters: Option<Value>,
    pub backend: Value,
    pub configuration_revision: String,
    pub observed_state_revision: String,
}

pub fn digest(value: Value) -> String {
    crate::tool_history::sha256(
        codex_config::schema::canonicalize(&value)
            .to_string()
            .as_bytes(),
    )
}

impl OperationAttempt {
    pub fn new(input: OperationAttemptInput) -> Self {
        Self {
            thread_id: input.thread_id, turn_id: input.turn_id, call_id: input.call_id,
            source: input.source, strategy: input.strategy, operation: input.operation,
            input_revision: input.input_revision, effective_input_known: input.effective_input_known,
            parameters: input.parameters, backend: input.backend,
            configuration_revision: input.configuration_revision, observed_state_revision: input.observed_state_revision,
            outcome: "unknown".into(), failure_code: None, fingerprint: None,
            original_failure: None, recovery_deferred: false,
        }
    }

    pub fn settle_outcome(&mut self, outcome: codex_tools::ToolOutputOutcome, failure: Option<ToolFailure>) {
        self.outcome = match outcome {
            codex_tools::ToolOutputOutcome::Success => "success",
            codex_tools::ToolOutputOutcome::Failure => "failure",
            codex_tools::ToolOutputOutcome::TimedOut => "timed_out",
            codex_tools::ToolOutputOutcome::Skipped => "skipped",
            codex_tools::ToolOutputOutcome::Yielded => "yielded",
        }.into();
        if let Some(failure) = failure { self.record_failure(failure); }
    }
    pub fn same_input(&self, other: &Self) -> bool {
        self.operation == other.operation
            && self.input_revision.is_some()
            && self.input_revision == other.input_revision
    }

    pub fn mark_requested_input(&mut self) {
        self.effective_input_known = false;
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

    pub fn advisory(&self, previous: &[&Self]) -> Value {
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
    pub fn recovery_preflight(&mut self, advisory: &Value) -> Option<ToolFailure> {
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
pub fn annotate_failure(failure: &mut ToolFailure, advisory: Value) {
    let details = failure.details.get_or_insert_with(|| serde_json::json!({}));
    if !details.is_object() {
        *details = serde_json::json!({"owner_details": details.take()});
    }
    details["operation_history"] = advisory;
}
