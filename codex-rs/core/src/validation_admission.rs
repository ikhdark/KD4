use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
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
            rule.policy_version == VALIDATION_POLICY_VERSION
                && rule.operation == operation
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
    pub(crate) skip_disposition: codex_tools::ToolOutputSkipDisposition,
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
            skip_disposition: codex_tools::ToolOutputSkipDisposition::Suppressed,
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
            skip_disposition: codex_tools::ToolOutputSkipDisposition::Deferred,
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
    if let Some(descriptor) = validation_like_wrapper_descriptor(invocation)
        && authorization.decision_for(
            descriptor.operation,
            descriptor.ecosystem,
            descriptor.breadth,
            descriptor.selector.as_deref(),
        ) == ValidationAuthorizationMatch::Prohibited
    {
        return Some(ValidationSkippedToolOutput::prohibited(&descriptor));
    }
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

fn validation_like_wrapper_descriptor(
    invocation: &CommandInvocation,
) -> Option<ValidationCommandDescriptor> {
    match invocation {
        CommandInvocation::Argv { program, args } => wrapper_descriptor(program, args),
        CommandInvocation::Script(script) | CommandInvocation::PowerShellScript(script) => script
            .replace("&&", ";")
            .replace("||", ";")
            .replace(['\r', '\n'], ";")
            .split(';')
            .filter_map(shlex::split)
            .find_map(|words| wrapper_descriptor(words.first()?, &words[1..])),
    }
}

fn wrapper_descriptor(program: &str, args: &[String]) -> Option<ValidationCommandDescriptor> {
    let binary = program
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(program)
        .trim_end_matches(".exe")
        .to_ascii_lowercase();
    let selector = match binary.as_str() {
        "make" | "just" | "task" => wrapper_recipe_argument(args),
        "npm" | "pnpm" => args
            .first()
            .filter(|arg| arg.as_str() == "run")
            .and_then(|_| args.get(1))
            .map(String::as_str),
        "yarn" => match args.first().map(String::as_str) {
            Some("run") => args.get(1).map(String::as_str),
            Some(script) if !script.starts_with('-') => Some(script),
            _ => None,
        },
        _ => None,
    }?;
    let operation = wrapper_recipe_operation(selector)?;
    let ecosystem = ValidationEcosystem::Other;
    let breadth = ValidationBreadth::Unknown;
    Some(ValidationCommandDescriptor {
        operation,
        ecosystem,
        breadth,
        selector: Some(selector.to_string()),
        command_family: format!("{ecosystem:?}:{operation:?}:{breadth:?}"),
    })
}

fn wrapper_recipe_argument(args: &[String]) -> Option<&str> {
    let mut skip_next = false;
    for arg in args {
        if skip_next {
            skip_next = false;
            continue;
        }
        if matches!(
            arg.as_str(),
            "-C" | "-f" | "--directory" | "--file" | "--justfile" | "--working-directory"
        ) {
            skip_next = true;
            continue;
        }
        if arg.starts_with('-') || arg.contains('=') {
            continue;
        }
        if wrapper_recipe_operation(arg).is_some() {
            return Some(arg);
        }
    }
    None
}

fn wrapper_recipe_operation(selector: &str) -> Option<ValidationOperation> {
    selector
        .to_ascii_lowercase()
        .split(|character: char| !character.is_ascii_alphanumeric())
        .find_map(|component| match component {
            "test" | "tests" | "testing" => Some(ValidationOperation::Test),
            "check" | "checks" => Some(ValidationOperation::Check),
            "lint" | "lints" | "clippy" => Some(ValidationOperation::Lint),
            "bench" | "benchmark" | "benchmarks" => Some(ValidationOperation::Bench),
            "fuzz" | "fuzzing" => Some(ValidationOperation::Fuzz),
            _ => None,
        })
}

#[derive(Debug, Clone)]
pub(crate) struct ReusableValidationResult {
    pub(crate) value: serde_json::Value,
}

/// Cheap identity used only to join validation that is already running.
///
/// This deliberately contains no validation-input manifest. Completed evidence
/// freshness is decided by the durable validation evidence path instead.
pub(crate) type InFlightValidationKey = codex_protocol::validation::ValidationProofKey;

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
        // Publish before unlinking the flight. Otherwise a new caller can
        // become leader in the gap while existing followers still have no
        // result to observe.
        *self.flight.result.lock().await = Some(result);
        {
            let mut registry = self.registry.lock().await;
            if registry
                .get(&self.identity)
                .is_some_and(|flight| Arc::ptr_eq(flight, &self.flight))
            {
                registry.remove(&self.identity);
            }
        }
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

/// Process-wide validation admission, partitioned by the complete proof key.
///
/// Keeping the registry process-scoped lets concurrent sessions in the same
/// workspace share an in-flight Cargo validation. Repository, source,
/// toolchain, environment, configuration, and contract changes remain isolated
/// by [`InFlightValidationKey`]. Completed results are still never cached.
pub(crate) fn process_validation_singleflight() -> SharedValidationSingleflight {
    static REGISTRY: std::sync::OnceLock<SharedValidationSingleflight> = std::sync::OnceLock::new();
    Arc::clone(REGISTRY.get_or_init(|| Arc::new(tokio::sync::Mutex::new(HashMap::new()))))
}

#[derive(Debug)]
pub(crate) enum ValidationRegistration {
    Leader {
        execution: Box<ValidationLeaderOwnership>,
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
        execution: Box::new(ValidationLeaderOwnership {
            identity: identity.clone(),
            flight: Arc::clone(&flight),
            registry: Arc::clone(registry),
            committed: false,
        }),
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
    /// Filled only after the launch-boundary admission has rechecked the
    /// current implementation, environment, configuration, and coverage.
    pub(crate) proof_key: Option<codex_protocol::validation::ValidationProofKey>,
    /// The exact predeclared leaf route, when this is an automatic launch.
    pub(crate) structured_route: Option<codex_protocol::plan_tool::ValidationRoute>,
    pub(crate) validation_call_id: Option<String>,
    pub(crate) turn_timing_state: Option<Arc<crate::turn_timing::TurnTimingState>>,
    pub(crate) force_fresh: bool,
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
        let keys = self.plan.keys.iter().map(history_key).collect::<Vec<_>>();
        self.state
            .validation_history()
            .record_batch(&keys, observation)
            .await;
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
    // Censored observations do not carry enough information to estimate the
    // duration variance. Do not advertise a confidence level unless the sample
    // consists entirely of completed observations.
    if aggregate.completed_count < minimum
        || aggregate.censored_below_count > 0
        || aggregate.censored_above_count > 0
    {
        return None;
    }
    let n = aggregate.completed_count as f64;
    let mean = aggregate.duration_sum_ms / n;
    let variance = ((aggregate.duration_sum_squares_ms - aggregate.duration_sum_ms * mean)
        / (n - 1.0))
        .max(0.0);
    let lower = (mean
        - one_sided_student_t_95(aggregate.completed_count - 1)
            * variance.sqrt()
            * (1.0 + 1.0 / n).sqrt())
    .max(0.0);
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
        effective_sample_count: aggregate.completed_count,
    })
}

/// Conservative one-sided 95% Student-t critical values.
///
/// The admission thresholds require at least eight observations, so a compact
/// upper-envelope table is enough and avoids treating estimated variance as if
/// it were known. Each bucket uses the critical value at its smallest degree of
/// freedom and is therefore conservative for the rest of the bucket.
fn one_sided_student_t_95(degrees_of_freedom: u64) -> f64 {
    match degrees_of_freedom {
        0..=7 => 1.895,
        8..=15 => 1.860,
        16..=31 => 1.746,
        32..=63 => 1.694,
        64..=127 => 1.669,
        _ => 1.658,
    }
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

pub(crate) fn validation_identity(
    repository: &[u8],
    cwd: impl Into<String>,
    invocation: &CommandInvocation,
    environment: impl Into<String>,
    toolchain: impl Into<String>,
    workspace_revision: u64,
) -> InFlightValidationKey {
    validation_identity_with_scope(
        repository,
        cwd,
        invocation,
        environment,
        toolchain,
        "",
        workspace_revision.to_string(),
        "",
        &[],
        &[],
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn validation_identity_with_scope(
    repository: &[u8],
    cwd: impl Into<String>,
    invocation: &CommandInvocation,
    environment: impl Into<String>,
    toolchain: impl Into<String>,
    configuration: impl Into<String>,
    implementation_identity: impl Into<String>,
    uncertainty: &str,
    covered_paths: &[String],
    covered_contracts: &[String],
) -> InFlightValidationKey {
    let canonical_route = canonical_test_proof_route(invocation)
        .unwrap_or_else(|| serde_json::to_vec(&invocation.hook_input()).unwrap_or_default());
    let canonical_route_hash = format!("{:x}", Sha256::digest(canonical_route));
    let mut paths = covered_paths.to_vec();
    paths.sort();
    paths.dedup();
    let mut contracts = covered_contracts.to_vec();
    contracts.sort();
    contracts.dedup();
    let coverage_identity = if paths.is_empty() && contracts.is_empty() {
        // Unknown coverage is intentionally repository-wide; never invent a
        // narrower identity from command text.
        "repository-wide".to_string()
    } else {
        let encoded =
            serde_json::to_vec(&(uncertainty.trim(), paths, contracts)).unwrap_or_default();
        format!("{:x}", Sha256::digest(encoded))
    };
    InFlightValidationKey {
        repository: String::from_utf8_lossy(repository).into_owned(),
        cwd: cwd.into(),
        canonical_route_hash,
        implementation_identity: implementation_identity.into(),
        coverage_identity,
        environment_identity: environment.into(),
        toolchain_identity: toolchain.into(),
        configuration_identity: configuration.into(),
        validation_contract_version: codex_protocol::validation::VALIDATION_CONTRACT_VERSION,
    }
}

/// Canonicalizes runner spelling away from a focused test proof. Cargo and
/// nextest receipts are equivalent only when package, features, and exact test
/// IDs are identical; environment, toolchain, configuration, and repository
/// epoch remain separate `ValidationProofKey` dimensions.
fn canonical_test_proof_route(invocation: &CommandInvocation) -> Option<Vec<u8>> {
    let CommandInvocation::Argv { program, args } = invocation else {
        return None;
    };
    let (forwarded, package_fallback) = match (program.as_str(), args.first()?.as_str()) {
        ("cargo", "test") => (&args[1..], None),
        ("just", "test-fast") => (&args[1..], None),
        ("just", "test-lane" | "test-lane-fast") => (&args[2..], None),
        ("just", "test-lane-package") => (&args[2..], args.get(1).map(String::as_str)),
        _ => return None,
    };
    let mut package = package_fallback.map(str::to_string);
    let mut features = Vec::new();
    let mut test_ids = Vec::new();
    let mut target_selectors = Vec::new();
    let mut target_flags = Vec::new();
    let mut harness_args = Vec::new();
    let mut all_features = false;
    let mut no_default_features = false;
    let mut cargo_exact = program != "cargo";
    let mut index = 0;
    while index < forwarded.len() {
        let argument = &forwarded[index];
        if program == "cargo" && argument == "--" {
            let harness = &forwarded[index + 1..];
            cargo_exact = harness.iter().any(|argument| argument == "--exact");
            harness_args.extend(
                harness
                    .iter()
                    .filter(|argument| argument.as_str() != "--exact")
                    .cloned(),
            );
            break;
        }
        let (option, inline) = argument
            .split_once('=')
            .map_or((argument.as_str(), None), |(option, value)| {
                (option, Some(value))
            });
        match option {
            "-p" | "--package" => {
                package = inline
                    .or_else(|| forwarded.get(index + 1).map(String::as_str))
                    .map(str::to_string);
                index += usize::from(inline.is_none());
            }
            "--features" => {
                let value = inline.or_else(|| forwarded.get(index + 1).map(String::as_str))?;
                features.extend(
                    value
                        .split(',')
                        .filter(|value| !value.is_empty())
                        .map(str::to_string),
                );
                index += usize::from(inline.is_none());
            }
            "--all-features" => all_features = true,
            "--no-default-features" => no_default_features = true,
            "-E" | "--filterset" | "--filter-expr" => {
                let expression = inline.or_else(|| forwarded.get(index + 1).map(String::as_str))?;
                let test_id = expression.strip_prefix("test(=")?.strip_suffix(')')?;
                if !exact_test_id(test_id) {
                    return None;
                }
                test_ids.push(test_id.to_string());
                index += usize::from(inline.is_none());
            }
            "--test" | "--bin" | "--example" | "--bench" | "--manifest-path" | "--target"
                if program == "cargo" =>
            {
                let value = inline.or_else(|| forwarded.get(index + 1).map(String::as_str))?;
                target_selectors.push(format!("{option}={value}"));
                index += usize::from(inline.is_none());
            }
            "--target-dir" | "-j" | "--jobs" if program == "cargo" => {
                let _ = inline.or_else(|| forwarded.get(index + 1).map(String::as_str))?;
                index += usize::from(inline.is_none());
            }
            "--lib" | "--bins" | "--tests" | "--benches" | "--all-targets" | "--doc" => {
                target_flags.push(option.to_string());
            }
            _ if program == "cargo" && !argument.starts_with('-') && exact_test_id(argument) => {
                test_ids.push(argument.clone());
            }
            _ => {}
        }
        index += 1;
    }
    if package.as_deref().is_none_or(str::is_empty) || test_ids.is_empty() || !cargo_exact {
        return None;
    }
    features.sort();
    features.dedup();
    test_ids.sort();
    test_ids.dedup();
    target_selectors.sort();
    target_selectors.dedup();
    target_flags.sort();
    target_flags.dedup();
    serde_json::to_vec(&serde_json::json!({
        "operation": "test",
        "package": package,
        "features": features,
        "all_features": all_features,
        "no_default_features": no_default_features,
        "selected_test_ids": test_ids,
        "target_selectors": target_selectors,
        "target_flags": target_flags,
        "harness_args": harness_args,
    }))
    .ok()
}

fn exact_test_id(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':'))
}

pub(crate) fn validation_argv_semantically_covers(
    executed: &[String],
    required: &[String],
) -> bool {
    let invocation = |argv: &[String]| {
        let (program, args) = argv.split_first()?;
        Some(CommandInvocation::Argv {
            program: program.clone(),
            args: args.to_vec(),
        })
    };
    let Some(executed_invocation) = invocation(executed) else {
        return false;
    };
    let Some(required_invocation) = invocation(required) else {
        return false;
    };
    let Some(executed_route) = canonical_test_proof_route(&executed_invocation) else {
        return executed == required;
    };
    let Some(required_route) = canonical_test_proof_route(&required_invocation) else {
        return false;
    };
    let Ok(executed): Result<serde_json::Value, _> = serde_json::from_slice(&executed_route) else {
        return false;
    };
    let Ok(required): Result<serde_json::Value, _> = serde_json::from_slice(&required_route) else {
        return false;
    };
    executed["package"] == required["package"]
        && executed["features"] == required["features"]
        && executed["all_features"] == required["all_features"]
        && executed["no_default_features"] == required["no_default_features"]
        && executed["target_selectors"] == required["target_selectors"]
        && executed["target_flags"] == required["target_flags"]
        && executed["harness_args"] == required["harness_args"]
        && required["selected_test_ids"]
            .as_array()
            .is_some_and(|required_ids| {
                executed["selected_test_ids"]
                    .as_array()
                    .is_some_and(|executed_ids| {
                        required_ids
                            .iter()
                            .all(|test_id| executed_ids.contains(test_id))
                    })
            })
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
        .replace(['\r', '\n'], ";");
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
        ("cargo" | "cargo.exe", Some("test")) => cargo_test_descriptor(args),
        ("cargo" | "cargo.exe", Some("check")) => {
            descriptor(ValidationOperation::Check, ValidationEcosystem::Rust, args)
        }
        ("cargo" | "cargo.exe", Some("clippy")) => {
            descriptor(ValidationOperation::Lint, ValidationEcosystem::Rust, args)
        }
        ("cargo" | "cargo.exe", Some("bench")) => {
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
    fn prohibited_validation_wrappers_fail_closed() {
        let mut authorization = ValidationAuthorization::default();
        assert!(authorization.update_from_user_input("Do not run tests."));

        for invocation in [
            CommandInvocation::Argv {
                program: "just".into(),
                args: vec!["test".into()],
            },
            CommandInvocation::Argv {
                program: "make".into(),
                args: vec!["-C".into(), "codex-rs".into(), "integration-tests".into()],
            },
            CommandInvocation::Script("echo preparing; task test:focused".into()),
            CommandInvocation::PowerShellScript("Write-Host preparing; yarn run test:unit".into()),
        ] {
            assert!(
                prohibited_skip_for(&authorization, &invocation).is_some(),
                "validation wrapper should be blocked: {invocation:?}"
            );
        }
    }

    #[test]
    fn validation_wrapper_guard_does_not_block_unprohibited_recipes() {
        let mut authorization = ValidationAuthorization::default();
        assert!(authorization.update_from_user_input("Do not run tests."));

        for invocation in [
            CommandInvocation::Argv {
                program: "just".into(),
                args: vec!["build".into()],
            },
            CommandInvocation::Argv {
                program: "task".into(),
                args: vec!["check".into()],
            },
        ] {
            assert!(prohibited_skip_for(&authorization, &invocation).is_none());
        }
    }

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
    fn validation_authorization_ignores_foreign_policy_versions() {
        let mut authorization = ValidationAuthorization::default();
        assert!(authorization.update_from_user_input("Run focused tests."));
        assert_eq!(
            authorization.decision_for(
                ValidationOperation::Test,
                ValidationEcosystem::Rust,
                ValidationBreadth::Selector,
                Some("selected_test"),
            ),
            ValidationAuthorizationMatch::Authorized
        );
        authorization.rules[0].policy_version = VALIDATION_POLICY_VERSION + 1;

        assert_eq!(
            authorization.decision_for(
                ValidationOperation::Test,
                ValidationEcosystem::Rust,
                ValidationBreadth::Selector,
                Some("selected_test"),
            ),
            ValidationAuthorizationMatch::Unspecified
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
    fn windows_cargo_executable_is_classified_as_validation() {
        let classified = classify_validation(&CommandInvocation::Argv {
            program: r"C:\Users\tester\.cargo\bin\cargo.exe".into(),
            args: vec!["test".into(), "--quiet".into()],
        });
        assert!(matches!(
            classified,
            ValidationClassification::Validation { ref leaves, .. }
                if leaves[0].operation == ValidationOperation::Test
                    && leaves[0].ecosystem == ValidationEcosystem::Rust
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
    async fn process_singleflight_registry_is_shared_across_turns() {
        let first_turn = process_validation_singleflight();
        let second_turn = process_validation_singleflight();
        assert!(Arc::ptr_eq(&first_turn, &second_turn));

        let task_cancellation = CancellationToken::new();
        let invocation = CommandInvocation::Script(
            "cargo test -p codex-core process_singleflight_registry_is_shared_across_turns".into(),
        );
        let key = validation_identity(
            b"process-registry-test-repo",
            "codex-rs",
            &invocation,
            "rust-env",
            "stable",
            7,
        );
        let execution = match register_if_absent(
            &first_turn,
            key.clone(),
            "first-turn-call",
            &task_cancellation,
        )
        .await
        {
            ValidationRegistration::Leader { execution, .. } => execution,
            ValidationRegistration::Follower(_) => panic!("first turn must lead"),
        };
        match register_if_absent(&second_turn, key, "second-turn-call", &task_cancellation).await {
            ValidationRegistration::Follower(follower) => {
                assert_eq!(follower.shared_from_call_id(), "first-turn-call");
            }
            ValidationRegistration::Leader { .. } => {
                panic!("second turn must join the process-scoped flight")
            }
        }
        execution.abandon().await;
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

        let scoped = validation_identity_with_scope(
            b"repo",
            "codex-rs",
            &package,
            "env-a",
            "stable",
            "features-a",
            "implementation-a",
            "core behavior remains correct",
            &["core/src/lib.rs".to_string()],
            &["core-contract".to_string()],
        );
        for mismatch in [
            validation_identity_with_scope(
                b"repo",
                "other-cwd",
                &package,
                "env-a",
                "stable",
                "features-a",
                "implementation-a",
                "core behavior remains correct",
                &["core/src/lib.rs".to_string()],
                &["core-contract".to_string()],
            ),
            validation_identity_with_scope(
                b"repo",
                "codex-rs",
                &package,
                "env-a",
                "stable",
                "features-b",
                "implementation-a",
                "core behavior remains correct",
                &["core/src/lib.rs".to_string()],
                &["core-contract".to_string()],
            ),
            validation_identity_with_scope(
                b"repo",
                "codex-rs",
                &package,
                "env-a",
                "stable",
                "features-a",
                "implementation-b",
                "core behavior remains correct",
                &["core/src/lib.rs".to_string()],
                &["core-contract".to_string()],
            ),
            validation_identity_with_scope(
                b"repo",
                "codex-rs",
                &package,
                "env-a",
                "stable",
                "features-a",
                "implementation-a",
                "core behavior remains correct",
                &["core/src/other.rs".to_string()],
                &["core-contract".to_string()],
            ),
        ] {
            assert_ne!(scoped, mismatch);
        }
    }

    #[test]
    fn recommended_fixes_combined_nextest_covers_equivalent_cargo_obligations() {
        let combined = vec![
            "just".to_string(),
            "test-fast".to_string(),
            "-p".to_string(),
            "codex-core".to_string(),
            "-E".to_string(),
            "test(=alpha)".to_string(),
            "-E".to_string(),
            "test(=beta)".to_string(),
        ];
        let alpha = vec![
            "cargo".to_string(),
            "test".to_string(),
            "-p".to_string(),
            "codex-core".to_string(),
            "alpha".to_string(),
            "--".to_string(),
            "--exact".to_string(),
        ];
        let unrelated = vec![
            "cargo".to_string(),
            "test".to_string(),
            "-p".to_string(),
            "codex-core".to_string(),
            "gamma".to_string(),
            "--".to_string(),
            "--exact".to_string(),
        ];

        assert!(validation_argv_semantically_covers(&combined, &alpha));
        assert!(!validation_argv_semantically_covers(&alpha, &combined));
        assert!(!validation_argv_semantically_covers(&combined, &unrelated));
    }

    #[test]
    fn focused_validation_identity_preserves_cargo_target_selectors() {
        let integration_a = vec![
            "cargo".to_string(),
            "test".to_string(),
            "-p".to_string(),
            "codex-core".to_string(),
            "--test".to_string(),
            "integration_a".to_string(),
            "focused_validation".to_string(),
            "--".to_string(),
            "--exact".to_string(),
        ];
        let integration_b = vec![
            "cargo".to_string(),
            "test".to_string(),
            "-p".to_string(),
            "codex-core".to_string(),
            "--test".to_string(),
            "integration_b".to_string(),
            "focused_validation".to_string(),
            "--".to_string(),
            "--exact".to_string(),
        ];

        assert!(validation_argv_semantically_covers(
            &integration_a,
            &integration_a
        ));
        assert!(!validation_argv_semantically_covers(
            &integration_a,
            &integration_b
        ));
    }
}
