use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use serde::Deserialize;
use serde::Serialize;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::tools::handlers::command_shape::CommandInvocation;

pub(crate) const VALIDATION_POLICY_VERSION: u32 = 1;
pub(crate) const VALIDATION_COST_THRESHOLD_MS: u64 = 30_000;
pub(crate) const MIN_SKIP_CONFIDENCE: f64 = 0.95;
pub(crate) const VALIDATION_MODEL_VERSION: i64 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ValidationOperation {
    Test,
    Check,
    Lint,
    Bench,
    Fuzz,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ValidationBreadth {
    Selector,
    Module,
    Package,
    Repository,
    Workspace,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ValidationEcosystem {
    Rust,
    Python,
    DotNet,
    Node,
    Go,
    Java,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ValidationAuthorizationDecision {
    Grant,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ValidationAuthorizationRule {
    pub(crate) policy_version: u32,
    pub(crate) sequence: u64,
    pub(crate) operation: ValidationOperation,
    pub(crate) ecosystem: Option<ValidationEcosystem>,
    pub(crate) minimum_breadth: Option<ValidationBreadth>,
    pub(crate) maximum_breadth: Option<ValidationBreadth>,
    pub(crate) selector: Option<String>,
    pub(crate) decision: ValidationAuthorizationDecision,
}

#[derive(Debug, Default)]
pub(crate) struct ValidationAuthorization {
    pub(crate) revision: u64,
    next_sequence: u64,
    pub(crate) rules: Vec<ValidationAuthorizationRule>,
}

pub(crate) type SharedValidationAuthorization = Arc<RwLock<ValidationAuthorization>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ValidationAuthorizationMatch {
    Authorized,
    Prohibited,
    Unspecified,
}

impl ValidationAuthorization {
    pub(crate) fn update_from_user_input(&mut self, text: &str) -> bool {
        let mut parsed = parse_directives(text);
        if parsed.is_empty() {
            return false;
        }
        self.revision = self.revision.saturating_add(1);
        for rule in &mut parsed {
            rule.sequence = self.next_sequence;
            self.next_sequence = self.next_sequence.saturating_add(1);
        }
        self.rules.extend(parsed);
        true
    }

    pub(crate) fn decision_for(
        &self,
        operation: ValidationOperation,
        ecosystem: ValidationEcosystem,
        breadth: ValidationBreadth,
        selector: Option<&str>,
    ) -> ValidationAuthorizationMatch {
        let mut latest_grant = None;
        let mut latest_deny = None;
        for rule in self.rules.iter().filter(|rule| {
            rule.operation == operation
                && rule
                    .ecosystem
                    .is_none_or(|candidate| candidate == ecosystem)
                && breadth_matches(rule, breadth)
                && selector_matches(rule, selector)
        }) {
            let candidate = (rule_specificity(rule), rule.sequence);
            match rule.decision {
                ValidationAuthorizationDecision::Grant => latest_grant = Some(candidate),
                ValidationAuthorizationDecision::Deny => latest_deny = Some(candidate),
            }
        }
        match (latest_grant, latest_deny) {
            (
                Some((grant_specificity, grant_sequence)),
                Some((deny_specificity, deny_sequence)),
            ) => {
                if deny_specificity > grant_specificity || deny_sequence > grant_sequence {
                    ValidationAuthorizationMatch::Prohibited
                } else {
                    ValidationAuthorizationMatch::Authorized
                }
            }
            (None, Some(_)) => ValidationAuthorizationMatch::Prohibited,
            (Some(_), None) => ValidationAuthorizationMatch::Authorized,
            (None, None) => ValidationAuthorizationMatch::Unspecified,
        }
    }
}

fn selector_matches(rule: &ValidationAuthorizationRule, selector: Option<&str>) -> bool {
    match (&rule.selector, selector) {
        (None, _) => true,
        (Some(expected), Some(actual)) => expected == actual,
        (Some(_), None) => false,
    }
}

fn rule_specificity(rule: &ValidationAuthorizationRule) -> u8 {
    u8::from(rule.minimum_breadth.is_some())
        + u8::from(rule.maximum_breadth.is_some())
        + u8::from(rule.ecosystem.is_some())
        + u8::from(rule.selector.is_some())
}

fn breadth_matches(rule: &ValidationAuthorizationRule, breadth: ValidationBreadth) -> bool {
    if breadth == ValidationBreadth::Unknown {
        return rule.decision == ValidationAuthorizationDecision::Deny;
    }
    rule.minimum_breadth.is_none_or(|min| breadth >= min)
        && rule.maximum_breadth.is_none_or(|max| breadth <= max)
}

fn parse_directives(text: &str) -> Vec<ValidationAuthorizationRule> {
    text.lines()
        .flat_map(|line| line.split(['.', ';', '\n']))
        .filter_map(parse_directive)
        .collect()
}

fn parse_directive(clause: &str) -> Option<ValidationAuthorizationRule> {
    let clause = clause.trim().to_ascii_lowercase();
    if clause.is_empty() || clause.contains('?') || clause.contains(['\'', '"', '`']) {
        return None;
    }
    let normalized = clause.strip_prefix("please ").unwrap_or(&clause).trim();
    let (decision, body) = if let Some(body) = normalized
        .strip_prefix("do not ")
        .or_else(|| normalized.strip_prefix("don't "))
        .or_else(|| normalized.strip_prefix("dont "))
        .or_else(|| normalized.strip_prefix("never "))
    {
        (ValidationAuthorizationDecision::Deny, body)
    } else if let Some(body) = normalized.strip_prefix("run ") {
        (ValidationAuthorizationDecision::Grant, body)
    } else {
        return None;
    };
    let body = if decision == ValidationAuthorizationDecision::Deny {
        body.strip_prefix("run ").unwrap_or(body)
    } else {
        body
    };

    let (operation, minimum_breadth, maximum_breadth) =
        if body == "tests" || body == "focused tests" || body == "tests for this change" {
            (
                ValidationOperation::Test,
                None,
                Some(ValidationBreadth::Package),
            )
        } else if body == "all tests" {
            (ValidationOperation::Test, None, None)
        } else if body == "the full suite" || body == "the workspace suite" || body == "full suite"
        {
            (
                ValidationOperation::Test,
                Some(ValidationBreadth::Repository),
                None,
            )
        } else if body == "checks" {
            (
                ValidationOperation::Check,
                None,
                Some(ValidationBreadth::Package),
            )
        } else if body == "lint" {
            (
                ValidationOperation::Lint,
                None,
                Some(ValidationBreadth::Package),
            )
        } else {
            return None;
        };
    Some(ValidationAuthorizationRule {
        policy_version: VALIDATION_POLICY_VERSION,
        sequence: 0,
        operation,
        ecosystem: None,
        minimum_breadth,
        maximum_breadth,
        selector: None,
        decision,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ValidationCostCertainty {
    Certain,
    Uncertain,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ValidationCommandDescriptor {
    pub(crate) operation: ValidationOperation,
    pub(crate) ecosystem: ValidationEcosystem,
    pub(crate) breadth: ValidationBreadth,
    pub(crate) selector: Option<String>,
    pub(crate) command_family: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ValidationClassification {
    NonValidation,
    Validation {
        leaves: Vec<ValidationCommandDescriptor>,
        cost_certainty: ValidationCostCertainty,
    },
    Opaque,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ValidationSkipReason {
    UserProhibitedValidation,
    PredictedValidationCost,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ValidationPrediction {
    pub(crate) tier: String,
    pub(crate) predicted_duration_ms: u64,
    pub(crate) predictive_lower_bound_ms: u64,
    pub(crate) confidence: f64,
    pub(crate) effective_sample_count: u64,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ValidationSkippedToolOutput {
    pub(crate) reason: ValidationSkipReason,
    pub(crate) command_was_executed: bool,
    pub(crate) predicted_duration_ms: Option<u64>,
    pub(crate) predictive_lower_bound_ms: Option<u64>,
    pub(crate) threshold_ms: u64,
    pub(crate) confidence: Option<f64>,
    pub(crate) tier: Option<String>,
    pub(crate) effective_sample_count: Option<u64>,
    pub(crate) command_family: String,
    pub(crate) breadth: ValidationBreadth,
    pub(crate) cheaper_alternatives: Vec<String>,
}

impl ValidationSkippedToolOutput {
    fn prohibited(descriptor: &ValidationCommandDescriptor) -> Self {
        Self {
            reason: ValidationSkipReason::UserProhibitedValidation,
            command_was_executed: false,
            predicted_duration_ms: None,
            predictive_lower_bound_ms: None,
            threshold_ms: VALIDATION_COST_THRESHOLD_MS,
            confidence: None,
            tier: None,
            effective_sample_count: None,
            command_family: descriptor.command_family.clone(),
            breadth: descriptor.breadth,
            cheaper_alternatives: cheaper_alternatives(descriptor),
        }
    }

    fn predicted(
        descriptor: &ValidationCommandDescriptor,
        prediction: ValidationPrediction,
    ) -> Self {
        Self {
            reason: ValidationSkipReason::PredictedValidationCost,
            command_was_executed: false,
            predicted_duration_ms: Some(prediction.predicted_duration_ms),
            predictive_lower_bound_ms: Some(prediction.predictive_lower_bound_ms),
            threshold_ms: VALIDATION_COST_THRESHOLD_MS,
            confidence: Some(prediction.confidence),
            tier: Some(prediction.tier),
            effective_sample_count: Some(prediction.effective_sample_count),
            command_family: descriptor.command_family.clone(),
            breadth: descriptor.breadth,
            cheaper_alternatives: cheaper_alternatives(descriptor),
        }
    }
}

pub(crate) fn prohibited_skip_for(
    authorization: &ValidationAuthorization,
    invocation: &CommandInvocation,
) -> Option<ValidationSkippedToolOutput> {
    let ValidationClassification::Validation { leaves, .. } = classify_validation(invocation)
    else {
        return None;
    };
    leaves
        .iter()
        .find(|leaf| {
            authorization.decision_for(
                leaf.operation,
                leaf.ecosystem,
                leaf.breadth,
                leaf.selector.as_deref(),
            ) == ValidationAuthorizationMatch::Prohibited
        })
        .map(ValidationSkippedToolOutput::prohibited)
}

#[derive(Debug, Clone)]
pub(crate) struct ReusableValidationResult {
    pub(crate) value: serde_json::Value,
}

/// Cheap identity used only to join validation that is already running.
///
/// This deliberately contains no validation-input manifest. Completed evidence
/// freshness is decided by the durable validation evidence path instead.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct InFlightValidationKey {
    repository: Vec<u8>,
    cwd: String,
    command: String,
    environment: String,
    toolchain: String,
    cargo_coverage: String,
    workspace_revision: u64,
}

impl InFlightValidationKey {
    pub(crate) fn new(
        repository: &[u8],
        cwd: impl Into<String>,
        invocation: &CommandInvocation,
        environment: impl Into<String>,
        toolchain: impl Into<String>,
        workspace_revision: u64,
    ) -> Self {
        let command = serde_json::to_string(&invocation.hook_input()).unwrap_or_default();
        let cargo_coverage = cargo_coverage_identity(invocation).unwrap_or_default();
        Self {
            repository: repository.to_vec(),
            cwd: cwd.into(),
            command,
            environment: environment.into(),
            toolchain: toolchain.into(),
            cargo_coverage,
            workspace_revision,
        }
    }
}

#[derive(Debug)]
pub(crate) struct ValidationFlight {
    leader_call_id: String,
    result: tokio::sync::Mutex<Option<ReusableValidationResult>>,
    notify: tokio::sync::Notify,
    abandoned: AtomicBool,
    waiters: AtomicUsize,
    cancellation: CancellationToken,
}

#[derive(Debug)]
pub(crate) struct ValidationLeader {
    identity: InFlightValidationKey,
    flight: Arc<ValidationFlight>,
    registry: SharedValidationSingleflight,
}

impl Clone for ValidationLeader {
    fn clone(&self) -> Self {
        self.flight.waiters.fetch_add(1, Ordering::Relaxed);
        Self {
            identity: self.identity.clone(),
            flight: Arc::clone(&self.flight),
            registry: Arc::clone(&self.registry),
        }
    }
}

impl Drop for ValidationLeader {
    fn drop(&mut self) {
        if self.flight.waiters.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.flight.abandoned.store(true, Ordering::Release);
            self.flight.cancellation.cancel();
            self.flight.notify.notify_waiters();
            let identity = self.identity.clone();
            let flight = Arc::clone(&self.flight);
            let registry = Arc::clone(&self.registry);
            tokio::spawn(async move {
                let mut registry = registry.lock().await;
                if registry
                    .get(&identity)
                    .is_some_and(|candidate| Arc::ptr_eq(candidate, &flight))
                {
                    registry.remove(&identity);
                }
            });
        }
    }
}

impl ValidationLeader {
    pub(crate) fn shared_from_call_id(&self) -> &str {
        &self.flight.leader_call_id
    }

    pub(crate) async fn join(&self) -> Option<ReusableValidationResult> {
        loop {
            let notified = self.flight.notify.notified();
            if let Some(result) = self.flight.result.lock().await.clone() {
                return Some(result);
            }
            if self.flight.abandoned.load(Ordering::Acquire) {
                return None;
            }
            notified.await;
        }
    }
}

#[derive(Debug)]
pub(crate) struct ValidationLeaderOwnership {
    identity: InFlightValidationKey,
    flight: Arc<ValidationFlight>,
    registry: SharedValidationSingleflight,
    committed: bool,
}

impl Drop for ValidationLeaderOwnership {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        self.flight.abandoned.store(true, Ordering::Release);
        self.flight.cancellation.cancel();
        self.flight.notify.notify_waiters();
        let identity = self.identity.clone();
        let flight = Arc::clone(&self.flight);
        let registry = Arc::clone(&self.registry);
        tokio::spawn(async move {
            let mut registry = registry.lock().await;
            if registry
                .get(&identity)
                .is_some_and(|candidate| Arc::ptr_eq(candidate, &flight))
            {
                registry.remove(&identity);
            }
        });
    }
}

impl ValidationLeaderOwnership {
    pub(crate) fn cancellation_token(&self) -> CancellationToken {
        self.flight.cancellation.clone()
    }

    pub(crate) async fn complete(mut self, result: ReusableValidationResult) {
        {
            let mut registry = self.registry.lock().await;
            if registry
                .get(&self.identity)
                .is_some_and(|flight| Arc::ptr_eq(flight, &self.flight))
            {
                registry.remove(&self.identity);
            }
        }
        *self.flight.result.lock().await = Some(result);
        self.flight.notify.notify_waiters();
        self.committed = true;
    }

    pub(crate) async fn abandon(mut self) {
        let mut registry = self.registry.lock().await;
        if registry
            .get(&self.identity)
            .is_some_and(|flight| Arc::ptr_eq(flight, &self.flight))
        {
            registry.remove(&self.identity);
        }
        drop(registry);
        self.flight.abandoned.store(true, Ordering::Release);
        self.flight.cancellation.cancel();
        self.flight.notify.notify_waiters();
        self.committed = true;
    }
}

pub(crate) type SharedValidationSingleflight =
    Arc<tokio::sync::Mutex<HashMap<InFlightValidationKey, Arc<ValidationFlight>>>>;

#[derive(Debug)]
pub(crate) enum ValidationRegistration {
    Leader {
        execution: ValidationLeaderOwnership,
        waiter: ValidationLeader,
    },
    Follower(ValidationLeader),
}

pub(crate) async fn register_if_absent(
    registry: &SharedValidationSingleflight,
    identity: InFlightValidationKey,
    call_id: &str,
    _caller_cancellation: &CancellationToken,
) -> ValidationRegistration {
    let mut flights = registry.lock().await;
    if let Some(flight) = flights.get(&identity).cloned() {
        flight.waiters.fetch_add(1, Ordering::Relaxed);
        return ValidationRegistration::Follower(ValidationLeader {
            identity,
            flight,
            registry: Arc::clone(registry),
        });
    }
    let flight = Arc::new(ValidationFlight {
        leader_call_id: call_id.to_string(),
        result: tokio::sync::Mutex::new(None),
        notify: tokio::sync::Notify::new(),
        abandoned: AtomicBool::new(false),
        waiters: AtomicUsize::new(1),
        // Admission transfers process ownership away from the first caller. A
        // caller cancellation detaches its waiter; only the execution owner or
        // the last disappearing waiter may cancel the shared process.
        cancellation: CancellationToken::new(),
    });
    flights.insert(identity.clone(), Arc::clone(&flight));
    ValidationRegistration::Leader {
        execution: ValidationLeaderOwnership {
            identity: identity.clone(),
            flight: Arc::clone(&flight),
            registry: Arc::clone(registry),
            committed: false,
        },
        waiter: ValidationLeader {
            identity,
            flight,
            registry: Arc::clone(registry),
        },
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ValidationObservationPlan {
    keys: Vec<ValidationObservationKey>,
}

#[derive(Debug, Clone)]
struct ValidationObservationKey {
    scope: codex_state::ValidationHistoryScope,
    repository: Option<Vec<u8>>,
    fingerprint: Vec<u8>,
    descriptor: ValidationCommandDescriptor,
}

#[derive(Debug)]
pub(crate) enum ValidationAdmission {
    Execute {
        authorization_revision: u64,
        observation: Option<ValidationObservationPlan>,
    },
    Skip(ValidationSkippedToolOutput),
}

#[derive(Debug, Clone)]
pub(crate) struct ValidationLaunchPlan {
    pub(crate) invocation: CommandInvocation,
    pub(crate) authorization_revision: u64,
    pub(crate) observation: Option<ValidationObservationPlan>,
}

pub(crate) async fn admit_validation(
    authorization: &SharedValidationAuthorization,
    state: Option<&codex_state::StateRuntime>,
    repository: &[u8],
    invocation: &CommandInvocation,
) -> ValidationAdmission {
    let classification = classify_validation(invocation);
    let ValidationClassification::Validation {
        leaves,
        cost_certainty,
    } = classification
    else {
        return ValidationAdmission::Execute {
            authorization_revision: authorization.read().await.revision,
            observation: None,
        };
    };
    let (revision, denied, authorized) = {
        let guard = authorization.read().await;
        let revision = guard.revision;
        let denied = leaves
            .iter()
            .find(|leaf| {
                guard.decision_for(
                    leaf.operation,
                    leaf.ecosystem,
                    leaf.breadth,
                    leaf.selector.as_deref(),
                ) == ValidationAuthorizationMatch::Prohibited
            })
            .cloned();
        let authorized = leaves.iter().all(|leaf| {
            guard.decision_for(
                leaf.operation,
                leaf.ecosystem,
                leaf.breadth,
                leaf.selector.as_deref(),
            ) == ValidationAuthorizationMatch::Authorized
        });
        (revision, denied, authorized)
    };
    if let Some(denied) = denied.as_ref() {
        return ValidationAdmission::Skip(ValidationSkippedToolOutput::prohibited(denied));
    }
    let observation = observation_plan(repository, &leaves);
    if authorized || cost_certainty == ValidationCostCertainty::Uncertain {
        return ValidationAdmission::Execute {
            authorization_revision: revision,
            observation: Some(observation),
        };
    }
    let Some(state) = state else {
        return ValidationAdmission::Execute {
            authorization_revision: revision,
            observation: Some(observation),
        };
    };
    for descriptor in &leaves {
        match predict(state, repository, descriptor).await {
            Ok(Some(prediction)) if prediction_requires_skip(&prediction) => {
                return ValidationAdmission::Skip(ValidationSkippedToolOutput::predicted(
                    descriptor, prediction,
                ));
            }
            Ok(_) => {}
            Err(error) => {
                tracing::debug!(%error, "validation cost lookup failed open");
                return ValidationAdmission::Execute {
                    authorization_revision: revision,
                    observation: Some(observation),
                };
            }
        }
    }
    ValidationAdmission::Execute {
        authorization_revision: revision,
        observation: Some(observation),
    }
}

fn prediction_requires_skip(prediction: &ValidationPrediction) -> bool {
    prediction.confidence >= MIN_SKIP_CONFIDENCE
        && prediction.predictive_lower_bound_ms > VALIDATION_COST_THRESHOLD_MS
}

pub(crate) fn admission_still_authorized(
    authorization: &ValidationAuthorization,
    invocation: &CommandInvocation,
) -> bool {
    prohibited_skip_for(authorization, invocation).is_none()
}

#[derive(Clone)]
pub(crate) struct ValidationObservationToken {
    plan: ValidationObservationPlan,
    state: Arc<codex_state::StateRuntime>,
    recorded: Arc<AtomicBool>,
    armed: Arc<AtomicBool>,
}

impl std::fmt::Debug for ValidationObservationToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ValidationObservationToken")
            .field("plan", &self.plan)
            .field("recorded", &self.recorded.load(Ordering::Acquire))
            .field("armed", &self.armed.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl ValidationObservationToken {
    pub(crate) fn new(
        plan: ValidationObservationPlan,
        state: Arc<codex_state::StateRuntime>,
    ) -> Self {
        Self {
            plan,
            state,
            recorded: Arc::new(AtomicBool::new(false)),
            armed: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn arm(&self) {
        self.armed.store(true, Ordering::Release);
    }

    pub(crate) async fn record_completed(&self, duration_ms: u64) {
        self.record(codex_state::ValidationHistoryObservation::Completed { duration_ms })
            .await;
    }

    pub(crate) async fn record_cancelled(&self, elapsed_ms: u64) {
        self.record(codex_state::ValidationHistoryObservation::Cancelled {
            elapsed_ms,
            threshold_ms: VALIDATION_COST_THRESHOLD_MS,
        })
        .await;
    }

    async fn record(&self, observation: codex_state::ValidationHistoryObservation) {
        if !self.armed.load(Ordering::Acquire) || self.recorded.swap(true, Ordering::AcqRel) {
            return;
        }
        for key in &self.plan.keys {
            self.state
                .validation_history()
                .record(history_key(key), observation)
                .await;
        }
    }
}

fn observation_plan(
    repository: &[u8],
    leaves: &[ValidationCommandDescriptor],
) -> ValidationObservationPlan {
    let mut keys = Vec::new();
    for descriptor in leaves {
        let exact = descriptor_fingerprint(descriptor, true);
        let family = descriptor_fingerprint(descriptor, false);
        keys.push(ValidationObservationKey {
            scope: codex_state::ValidationHistoryScope::RepositoryFingerprint,
            repository: Some(repository.to_vec()),
            fingerprint: exact,
            descriptor: descriptor.clone(),
        });
        keys.push(ValidationObservationKey {
            scope: codex_state::ValidationHistoryScope::RepositoryFamily,
            repository: Some(repository.to_vec()),
            fingerprint: family.clone(),
            descriptor: descriptor.clone(),
        });
        keys.push(ValidationObservationKey {
            scope: codex_state::ValidationHistoryScope::GlobalFamily,
            repository: None,
            fingerprint: family,
            descriptor: descriptor.clone(),
        });
    }
    ValidationObservationPlan { keys }
}

async fn predict(
    state: &codex_state::StateRuntime,
    repository: &[u8],
    descriptor: &ValidationCommandDescriptor,
) -> anyhow::Result<Option<ValidationPrediction>> {
    let candidates = observation_plan(repository, std::slice::from_ref(descriptor));
    for key in candidates.keys {
        let aggregate = state.validation_history().lookup(history_key(&key)).await?;
        let Some(aggregate) = aggregate else { continue };
        let minimum = match key.scope {
            codex_state::ValidationHistoryScope::RepositoryFingerprint => 8,
            codex_state::ValidationHistoryScope::RepositoryFamily => 16,
            codex_state::ValidationHistoryScope::GlobalFamily => 64,
        };
        if let Some(prediction) = prediction_from_aggregate(&aggregate, minimum, key.scope) {
            return Ok(Some(prediction));
        }
    }
    Ok(None)
}

fn prediction_from_aggregate(
    aggregate: &codex_state::ValidationHistoryAggregate,
    minimum: u64,
    scope: codex_state::ValidationHistoryScope,
) -> Option<ValidationPrediction> {
    let effective = aggregate.completed_count + aggregate.censored_above_count;
    if effective < minimum || aggregate.censored_below_count > 0 || aggregate.completed_count < 2 {
        return None;
    }
    let n = aggregate.completed_count as f64;
    let mean = aggregate.duration_sum_ms / n;
    let variance = ((aggregate.duration_sum_squares_ms - aggregate.duration_sum_ms * mean)
        / (n - 1.0))
        .max(0.0);
    let lower = (mean - 1.645 * variance.sqrt() * (1.0 + 1.0 / n).sqrt()).max(0.0);
    Some(ValidationPrediction {
        tier: match scope {
            codex_state::ValidationHistoryScope::RepositoryFingerprint => "repository_fingerprint",
            codex_state::ValidationHistoryScope::RepositoryFamily => "repository_family",
            codex_state::ValidationHistoryScope::GlobalFamily => "global_family",
        }
        .to_string(),
        predicted_duration_ms: mean.round() as u64,
        predictive_lower_bound_ms: lower.floor() as u64,
        confidence: MIN_SKIP_CONFIDENCE,
        effective_sample_count: effective,
    })
}

fn history_key(key: &ValidationObservationKey) -> codex_state::ValidationHistoryKey<'_> {
    codex_state::ValidationHistoryKey {
        scope: key.scope,
        repository: key.repository.as_deref(),
        fingerprint: &key.fingerprint,
        operation: key.descriptor.operation as i64,
        ecosystem: key.descriptor.ecosystem as i64,
        breadth: key.descriptor.breadth as i64,
        model_version: VALIDATION_MODEL_VERSION,
    }
}

fn descriptor_fingerprint(descriptor: &ValidationCommandDescriptor, exact: bool) -> Vec<u8> {
    format!(
        "v{VALIDATION_MODEL_VERSION}:{:?}:{:?}:{:?}:{}:{}",
        descriptor.operation,
        descriptor.ecosystem,
        descriptor.breadth,
        descriptor.command_family,
        if exact {
            descriptor.selector.as_deref().unwrap_or("")
        } else {
            ""
        }
    )
    .into_bytes()
}

fn cargo_coverage_identity(invocation: &CommandInvocation) -> Option<String> {
    let ValidationClassification::Validation { leaves, .. } = classify_validation(invocation)
    else {
        return None;
    };
    let rust: Vec<_> = leaves
        .into_iter()
        .filter(|leaf| leaf.ecosystem == ValidationEcosystem::Rust)
        .collect();
    (!rust.is_empty()).then(|| serde_json::to_string(&rust).unwrap_or_default())
}

pub(crate) fn validation_identity(
    repository: &[u8],
    cwd: impl Into<String>,
    invocation: &CommandInvocation,
    environment: impl Into<String>,
    toolchain: impl Into<String>,
    workspace_revision: u64,
) -> InFlightValidationKey {
    InFlightValidationKey::new(
        repository,
        cwd,
        invocation,
        environment,
        toolchain,
        workspace_revision,
    )
}

fn cheaper_alternatives(descriptor: &ValidationCommandDescriptor) -> Vec<String> {
    match descriptor.operation {
        ValidationOperation::Test => vec![
            "run the nearest module or package test".to_string(),
            "reuse an identical completed validation".to_string(),
        ],
        ValidationOperation::Check | ValidationOperation::Lint => {
            vec!["limit validation to the affected package".to_string()]
        }
        ValidationOperation::Bench | ValidationOperation::Fuzz => Vec::new(),
    }
}

pub(crate) fn classify_validation(invocation: &CommandInvocation) -> ValidationClassification {
    match invocation {
        CommandInvocation::Argv { program, args } => classify_argv(program, args),
        CommandInvocation::Script(script) | CommandInvocation::PowerShellScript(script) => {
            classify_script(script, 0)
        }
    }
}

fn classify_script(script: &str, depth: usize) -> ValidationClassification {
    if depth > 4 {
        return ValidationClassification::Opaque;
    }
    let mut leaves = Vec::new();
    let mut uncertain = false;
    let normalized = script
        .replace("&&", ";")
        .replace("||", ";")
        .replace('\r', ";")
        .replace('\n', ";");
    for leaf in normalized.split(';') {
        let leaf_is_dynamic = leaf.contains("$(") || leaf.contains('`') || leaf.contains("${");
        let Some(words) = shlex::split(leaf) else {
            uncertain = true;
            continue;
        };
        if words.is_empty() {
            continue;
        }
        if leaf_is_dynamic && !looks_like_known_validation(&words) {
            uncertain = true;
            continue;
        }
        let classified = classify_argv(&words[0], &words[1..]);
        match classified {
            ValidationClassification::Validation {
                leaves: mut found,
                cost_certainty,
            } => {
                leaves.append(&mut found);
                uncertain |=
                    leaf_is_dynamic || cost_certainty == ValidationCostCertainty::Uncertain;
            }
            ValidationClassification::Opaque => uncertain = true,
            ValidationClassification::NonValidation => {}
        }
    }
    if leaves.is_empty() {
        if uncertain {
            ValidationClassification::Opaque
        } else {
            ValidationClassification::NonValidation
        }
    } else {
        ValidationClassification::Validation {
            leaves,
            cost_certainty: if uncertain {
                ValidationCostCertainty::Uncertain
            } else {
                ValidationCostCertainty::Certain
            },
        }
    }
}

fn looks_like_known_validation(words: &[String]) -> bool {
    words.iter().any(|word| {
        let word = word.trim_matches(['\'', '"']);
        matches!(
            word,
            "cargo" | "pytest" | "dotnet" | "go" | "npm" | "pnpm" | "yarn"
        )
    })
}

fn classify_argv(program: &str, args: &[String]) -> ValidationClassification {
    classify_argv_at_depth(program, args, 0)
}

fn classify_argv_at_depth(
    program: &str,
    args: &[String],
    depth: usize,
) -> ValidationClassification {
    if depth > 4 {
        return ValidationClassification::Opaque;
    }
    let binary = program
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(program)
        .to_ascii_lowercase();
    if matches!(binary.as_str(), "env" | "command" | "time") {
        let Some(index) = args
            .iter()
            .position(|arg| !(arg.starts_with('-') || binary == "env" && arg.contains('=')))
        else {
            return ValidationClassification::Opaque;
        };
        return classify_argv_at_depth(&args[index], &args[index + 1..], depth + 1);
    }
    if matches!(binary.as_str(), "bash" | "sh") {
        if let Some(index) = args.iter().position(|arg| arg == "-c")
            && let Some(script) = args.get(index + 1)
        {
            return classify_script(script, depth + 1);
        }
        return ValidationClassification::Opaque;
    }
    if matches!(binary.as_str(), "pwsh" | "powershell" | "powershell.exe") {
        if let Some(index) = args
            .iter()
            .position(|arg| arg.eq_ignore_ascii_case("-command"))
            && args.get(index + 1).is_some()
        {
            return classify_script(&args[index + 1..].join(" "), depth + 1);
        }
        return ValidationClassification::Opaque;
    }
    if matches!(binary.as_str(), "cmd" | "cmd.exe") {
        if let Some(index) = args.iter().position(|arg| arg.eq_ignore_ascii_case("/c"))
            && args.get(index + 1).is_some()
        {
            return classify_script(&args[index + 1..].join(" "), depth + 1);
        }
        return ValidationClassification::Opaque;
    }

    let descriptor = match (binary.as_str(), args.first().map(String::as_str)) {
        ("cargo", Some("test")) => cargo_test_descriptor(args),
        ("cargo", Some("check")) => {
            descriptor(ValidationOperation::Check, ValidationEcosystem::Rust, args)
        }
        ("cargo", Some("clippy")) => {
            descriptor(ValidationOperation::Lint, ValidationEcosystem::Rust, args)
        }
        ("cargo", Some("bench")) => {
            descriptor(ValidationOperation::Bench, ValidationEcosystem::Rust, args)
        }
        ("pytest" | "pytest.exe" | "python" | "python3", _)
            if binary.starts_with("pytest")
                || args.iter().any(|arg| {
                    arg == "pytest" || arg == "-m" && args.iter().any(|next| next == "pytest")
                }) =>
        {
            descriptor(ValidationOperation::Test, ValidationEcosystem::Python, args)
        }
        ("dotnet", Some("test")) => {
            descriptor(ValidationOperation::Test, ValidationEcosystem::DotNet, args)
        }
        ("go", Some("test")) => {
            descriptor(ValidationOperation::Test, ValidationEcosystem::Go, args)
        }
        ("npm" | "pnpm" | "yarn", Some("test")) => {
            descriptor(ValidationOperation::Test, ValidationEcosystem::Node, args)
        }
        ("make" | "just" | "task", _) => return ValidationClassification::Opaque,
        _ => return ValidationClassification::NonValidation,
    };
    ValidationClassification::Validation {
        leaves: vec![descriptor],
        cost_certainty: if args
            .iter()
            .any(|arg| arg.contains('$') || arg.contains('*'))
        {
            ValidationCostCertainty::Uncertain
        } else {
            ValidationCostCertainty::Certain
        },
    }
}

fn cargo_test_descriptor(args: &[String]) -> ValidationCommandDescriptor {
    let mut descriptor = descriptor(ValidationOperation::Test, ValidationEcosystem::Rust, args);
    if let Some(selector) = args.get(1).filter(|selector| {
        !selector.is_empty()
            && !selector.starts_with('-')
            && !selector.contains('$')
            && !selector.contains('*')
    }) {
        descriptor.breadth = ValidationBreadth::Selector;
        descriptor.selector = Some(selector.clone());
        descriptor.command_family = format!(
            "{:?}:{:?}:{:?}",
            descriptor.ecosystem, descriptor.operation, descriptor.breadth
        );
    }
    descriptor
}

fn descriptor(
    operation: ValidationOperation,
    ecosystem: ValidationEcosystem,
    args: &[String],
) -> ValidationCommandDescriptor {
    let breadth = if args
        .iter()
        .any(|arg| arg.contains('$') || arg.contains('*'))
    {
        ValidationBreadth::Unknown
    } else if args
        .iter()
        .any(|arg| arg == "--workspace" || arg == "--all")
    {
        ValidationBreadth::Workspace
    } else if args.iter().any(|arg| arg == "-p" || arg == "--package") {
        ValidationBreadth::Package
    } else if args
        .iter()
        .any(|arg| arg.contains("::") || arg.contains("#"))
    {
        ValidationBreadth::Selector
    } else {
        match ecosystem {
            ValidationEcosystem::Rust | ValidationEcosystem::Node => ValidationBreadth::Package,
            ValidationEcosystem::Python => ValidationBreadth::Repository,
            ValidationEcosystem::Go if args.iter().any(|arg| arg == "./...") => {
                ValidationBreadth::Repository
            }
            ValidationEcosystem::Go => ValidationBreadth::Package,
            ValidationEcosystem::DotNet
            | ValidationEcosystem::Java
            | ValidationEcosystem::Other => ValidationBreadth::Unknown,
        }
    };
    ValidationCommandDescriptor {
        operation,
        ecosystem,
        breadth,
        selector: None,
        command_family: format!("{ecosystem:?}:{operation:?}:{breadth:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focused_grant_and_workspace_denial_coexist() {
        let mut authorization = ValidationAuthorization::default();
        assert!(
            authorization
                .update_from_user_input("Run focused tests; do not run the workspace suite.")
        );
        assert_eq!(
            authorization.decision_for(
                ValidationOperation::Test,
                ValidationEcosystem::Rust,
                ValidationBreadth::Package,
                None,
            ),
            ValidationAuthorizationMatch::Authorized
        );
        assert_eq!(
            authorization.decision_for(
                ValidationOperation::Test,
                ValidationEcosystem::Rust,
                ValidationBreadth::Workspace,
                None,
            ),
            ValidationAuthorizationMatch::Prohibited
        );
    }

    #[test]
    fn quoted_and_interrogative_text_do_not_authorize() {
        let mut authorization = ValidationAuthorization::default();
        assert!(!authorization.update_from_user_input(
            "Write documentation saying 'run tests'. Why did it run tests?"
        ));
    }

    #[test]
    fn broad_prohibition_matches_unknown_but_narrow_grant_does_not() {
        let mut prohibited = ValidationAuthorization::default();
        prohibited.update_from_user_input("Do not run tests.");
        assert_eq!(
            prohibited.decision_for(
                ValidationOperation::Test,
                ValidationEcosystem::Rust,
                ValidationBreadth::Unknown,
                None,
            ),
            ValidationAuthorizationMatch::Prohibited
        );
        let mut granted = ValidationAuthorization::default();
        granted.update_from_user_input("Run focused tests.");
        assert_eq!(
            granted.decision_for(
                ValidationOperation::Test,
                ValidationEcosystem::Rust,
                ValidationBreadth::Unknown,
                None,
            ),
            ValidationAuthorizationMatch::Unspecified
        );
    }

    #[test]
    fn recognized_dynamic_validation_is_not_opaque() {
        let classified = classify_validation(&CommandInvocation::Argv {
            program: "cargo".into(),
            args: vec!["test".into(), "$SELECTOR".into()],
        });
        assert!(matches!(
            classified,
            ValidationClassification::Validation {
                cost_certainty: ValidationCostCertainty::Uncertain,
                ref leaves,
            }
                if leaves[0].breadth == ValidationBreadth::Unknown
        ));
    }

    #[test]
    fn cargo_test_recognizes_only_the_immediate_positional_selector() {
        let classified = classify_validation(&CommandInvocation::Argv {
            program: "cargo".into(),
            args: vec!["test".into(), "selected_test".into()],
        });
        assert!(matches!(
            classified,
            ValidationClassification::Validation { ref leaves, .. }
                if leaves[0].breadth == ValidationBreadth::Selector
                    && leaves[0].selector.as_deref() == Some("selected_test")
        ));

        let option_before_selector = classify_validation(&CommandInvocation::Argv {
            program: "cargo".into(),
            args: vec!["test".into(), "--workspace".into(), "selected_test".into()],
        });
        assert!(matches!(
            option_before_selector,
            ValidationClassification::Validation { ref leaves, .. }
                if leaves[0].breadth == ValidationBreadth::Workspace
                    && leaves[0].selector.is_none()
        ));
    }

    #[test]
    fn directive_defaults_are_operation_isolated() {
        let mut authorization = ValidationAuthorization::default();
        assert!(authorization.update_from_user_input("Run checks; run lint; do not run tests."));
        assert_eq!(
            authorization.decision_for(
                ValidationOperation::Test,
                ValidationEcosystem::Rust,
                ValidationBreadth::Package,
                None,
            ),
            ValidationAuthorizationMatch::Prohibited
        );
        assert_eq!(
            authorization.decision_for(
                ValidationOperation::Check,
                ValidationEcosystem::Rust,
                ValidationBreadth::Package,
                None,
            ),
            ValidationAuthorizationMatch::Authorized
        );
        assert_eq!(
            authorization.decision_for(
                ValidationOperation::Lint,
                ValidationEcosystem::Rust,
                ValidationBreadth::Repository,
                None,
            ),
            ValidationAuthorizationMatch::Unspecified
        );

        let mut revoked = ValidationAuthorization::default();
        revoked.update_from_user_input("Run focused tests.");
        revoked.update_from_user_input("Do not run tests.");
        assert_eq!(
            revoked.decision_for(
                ValidationOperation::Test,
                ValidationEcosystem::Rust,
                ValidationBreadth::Package,
                None,
            ),
            ValidationAuthorizationMatch::Prohibited
        );
    }

    #[test]
    fn quoted_shell_wrapper_preserves_validation_classification() {
        let classified = classify_validation(&CommandInvocation::Script(
            "env FOO=bar bash -c 'cargo test --workspace'".into(),
        ));
        assert!(matches!(
            classified,
            ValidationClassification::Validation { ref leaves, .. }
                if leaves[0].breadth == ValidationBreadth::Workspace
        ));
    }

    #[test]
    fn threshold_is_strict_and_confidence_is_required() {
        let mut prediction = ValidationPrediction {
            tier: "repository_fingerprint".into(),
            predicted_duration_ms: 45_000,
            predictive_lower_bound_ms: VALIDATION_COST_THRESHOLD_MS,
            confidence: MIN_SKIP_CONFIDENCE,
            effective_sample_count: 8,
        };
        assert!(!prediction_requires_skip(&prediction));
        prediction.predictive_lower_bound_ms += 1;
        assert!(prediction_requires_skip(&prediction));
        prediction.confidence = MIN_SKIP_CONFIDENCE - f64::EPSILON;
        assert!(!prediction_requires_skip(&prediction));
    }

    #[test]
    fn compound_keeps_validation_leaf_and_uncertain_cost() {
        let classified = classify_validation(&CommandInvocation::Script(
            "echo preparing && cargo test --workspace".into(),
        ));
        assert!(matches!(
            classified,
            ValidationClassification::Validation { leaves, .. }
                if leaves[0].breadth == ValidationBreadth::Workspace
        ));
    }

    #[test]
    fn line_boundaries_keep_the_validation_leaf() {
        for boundary in ["\n", "\r", "\r\n"] {
            let classified = classify_validation(&CommandInvocation::Script(format!(
                "echo preparing{boundary}cargo test --workspace"
            )));
            assert!(matches!(
                classified,
                ValidationClassification::Validation { ref leaves, .. }
                    if leaves.len() == 1
                        && leaves[0].breadth == ValidationBreadth::Workspace
            ));
        }
    }

    #[test]
    fn opaque_compound_leaf_disables_cost_certainty_but_keeps_known_validation() {
        let classified = classify_validation(&CommandInvocation::Script(
            "just prepare && cargo test --workspace".into(),
        ));
        assert!(matches!(
            classified,
            ValidationClassification::Validation {
                cost_certainty: ValidationCostCertainty::Uncertain,
                ref leaves,
            } if leaves[0].breadth == ValidationBreadth::Workspace
        ));
    }

    #[test]
    fn generic_recipe_is_opaque() {
        assert_eq!(
            classify_validation(&CommandInvocation::Script("just test".into())),
            ValidationClassification::Opaque
        );
    }

    #[tokio::test]
    async fn singleflight_shares_concurrent_result_without_caching_it() {
        let registry: SharedValidationSingleflight =
            Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let task_cancellation = CancellationToken::new();
        let invocation = CommandInvocation::Script("cargo clippy -p codex-core".into());
        let key = validation_identity(b"repo", "codex-rs", &invocation, "rust-env", "stable", 7);
        let (execution, first_waiter) =
            match register_if_absent(&registry, key.clone(), "call-a", &task_cancellation).await {
                ValidationRegistration::Leader { execution, waiter } => (execution, waiter),
                ValidationRegistration::Follower(_) => panic!("first registration must lead"),
            };
        let follower =
            match register_if_absent(&registry, key.clone(), "call-b", &task_cancellation).await {
                ValidationRegistration::Follower(follower) => follower,
                ValidationRegistration::Leader { .. } => panic!("second registration must follow"),
            };
        assert_eq!(follower.shared_from_call_id(), "call-a");

        let result = ReusableValidationResult {
            value: serde_json::json!({"ok": true}),
        };
        execution.complete(result.clone()).await;
        assert_eq!(first_waiter.join().await.unwrap().value, result.value);
        assert_eq!(follower.join().await.unwrap().value, result.value);
        assert!(registry.lock().await.get(&key).is_none());

        let later = match register_if_absent(&registry, key, "call-c", &task_cancellation).await {
            ValidationRegistration::Leader { execution, .. } => execution,
            ValidationRegistration::Follower(_) => {
                panic!("a later registration must start a new flight")
            }
        };
        later.abandon().await;
    }

    #[tokio::test]
    async fn individual_waiter_cancellation_does_not_cancel_shared_execution() {
        let registry: SharedValidationSingleflight =
            Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let first_cancellation = CancellationToken::new();
        let second_cancellation = CancellationToken::new();
        let invocation = CommandInvocation::Script("cargo clippy -p codex-core".into());
        let key = validation_identity(b"repo", "codex-rs", &invocation, "env", "stable", 1);
        let (execution, first_waiter) =
            match register_if_absent(&registry, key.clone(), "call-a", &first_cancellation).await {
                ValidationRegistration::Leader { execution, waiter } => (execution, waiter),
                ValidationRegistration::Follower(_) => panic!("first registration must lead"),
            };
        let execution_cancellation = execution.cancellation_token();
        let second_waiter =
            match register_if_absent(&registry, key, "call-b", &second_cancellation).await {
                ValidationRegistration::Follower(waiter) => waiter,
                ValidationRegistration::Leader { .. } => panic!("second registration must follow"),
            };

        first_cancellation.cancel();
        drop(first_waiter);
        assert!(!execution_cancellation.is_cancelled());
        let result = ReusableValidationResult {
            value: serde_json::json!({"success": true}),
        };
        execution.complete(result.clone()).await;
        assert_eq!(second_waiter.join().await.unwrap().value, result.value);
    }

    #[tokio::test]
    async fn session_and_last_waiter_cancellation_terminate_a_flight() {
        let registry: SharedValidationSingleflight =
            Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let session_cancellation = CancellationToken::new();
        let invocation = CommandInvocation::Script("cargo clippy -p codex-core".into());
        let key = validation_identity(b"repo", "codex-rs", &invocation, "env", "stable", 1);
        let (execution, first_waiter) =
            match register_if_absent(&registry, key.clone(), "call-a", &session_cancellation).await
            {
                ValidationRegistration::Leader { execution, waiter } => (execution, waiter),
                ValidationRegistration::Follower(_) => panic!("first registration must lead"),
            };
        let second_waiter =
            match register_if_absent(&registry, key, "call-b", &session_cancellation).await {
                ValidationRegistration::Follower(waiter) => waiter,
                ValidationRegistration::Leader { .. } => panic!("second registration must follow"),
            };
        let execution_cancellation = execution.cancellation_token();
        session_cancellation.cancel();
        drop(first_waiter);
        assert!(!execution_cancellation.is_cancelled());
        drop(second_waiter);
        execution_cancellation.cancelled().await;
        execution.abandon().await;

        let second_session_cancellation = CancellationToken::new();
        let key = validation_identity(b"repo", "codex-rs", &invocation, "env", "stable", 2);
        let (execution, waiter) = match register_if_absent(
            &registry,
            key,
            "call-c",
            &second_session_cancellation,
        )
        .await
        {
            ValidationRegistration::Leader { execution, waiter } => (execution, waiter),
            ValidationRegistration::Follower(_) => panic!("new revision must lead"),
        };
        let execution_cancellation = execution.cancellation_token();
        drop(waiter);
        execution_cancellation.cancelled().await;
        assert!(execution_cancellation.is_cancelled());
        execution.abandon().await;
    }

    #[tokio::test]
    async fn abandoned_execution_is_observed_by_all_remaining_waiters() {
        let registry: SharedValidationSingleflight =
            Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let caller_cancellation = CancellationToken::new();
        let invocation = CommandInvocation::Script("cargo clippy -p codex-core".into());
        let key = validation_identity(b"repo", "codex-rs", &invocation, "env", "stable", 3);
        let (execution, first_waiter) = match register_if_absent(
            &registry,
            key.clone(),
            "call-a",
            &caller_cancellation,
        )
        .await
        {
            ValidationRegistration::Leader { execution, waiter } => (execution, waiter),
            ValidationRegistration::Follower(_) => panic!("first registration must lead"),
        };
        let second_waiter = match register_if_absent(
            &registry,
            key.clone(),
            "call-b",
            &caller_cancellation,
        )
        .await
        {
            ValidationRegistration::Follower(waiter) => waiter,
            ValidationRegistration::Leader { .. } => panic!("second registration must follow"),
        };

        execution.abandon().await;
        assert!(first_waiter.join().await.is_none());
        assert!(second_waiter.join().await.is_none());
        match register_if_absent(&registry, key, "call-c", &caller_cancellation).await {
            ValidationRegistration::Leader { execution, .. } => execution.abandon().await,
            ValidationRegistration::Follower(_) => panic!("abandoned execution must not be reused"),
        }
    }

    #[test]
    fn inflight_identity_is_revision_environment_toolchain_and_coverage_bound() {
        let package = CommandInvocation::Script("cargo clippy -p codex-core".into());
        let workspace = CommandInvocation::Script("cargo clippy --workspace".into());
        let baseline = validation_identity(b"repo", "codex-rs", &package, "env-a", "stable", 4);
        assert_eq!(
            baseline,
            validation_identity(b"repo", "codex-rs", &package, "env-a", "stable", 4)
        );
        assert_ne!(
            baseline,
            validation_identity(b"repo", "codex-rs", &workspace, "env-a", "stable", 4)
        );
        assert_ne!(
            baseline,
            validation_identity(b"repo", "codex-rs", &package, "env-b", "stable", 4)
        );
        assert_ne!(
            baseline,
            validation_identity(b"repo", "codex-rs", &package, "env-a", "nightly", 4)
        );
        assert_ne!(
            baseline,
            validation_identity(b"repo", "codex-rs", &package, "env-a", "stable", 5)
        );
    }
}
