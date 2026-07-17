use codex_analytics::GuardianApprovalRequestSource;
use codex_analytics::GuardianReviewAnalyticsResult;
use codex_analytics::GuardianReviewDecision;
use codex_analytics::GuardianReviewFailureReason;
use codex_analytics::GuardianReviewTerminalStatus;
use codex_analytics::GuardianReviewTrackContext;
use codex_analytics::GuardianReviewedAction;
use codex_protocol::config_types::ApprovalsReviewer;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::GuardianAssessmentDecisionSource;
use codex_protocol::protocol::GuardianAssessmentEvent;
use codex_protocol::protocol::GuardianAssessmentStatus;
use codex_protocol::protocol::GuardianRiskLevel;
use codex_protocol::protocol::GuardianUserAuthorization;
use codex_protocol::protocol::ReviewDecision;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::WarningEvent;
use std::sync::Arc;
use std::sync::LazyLock;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::time::Instant;
use tokio::time::sleep_until;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::turn_timing::now_unix_timestamp_ms;
use crate::util::backoff;

use super::AUTO_REVIEW_DENIAL_WINDOW_SIZE;
use super::GUARDIAN_REVIEW_TIMEOUT;
use super::GUARDIAN_REVIEWER_NAME;
use super::GuardianApprovalRequest;
use super::GuardianAssessment;
use super::GuardianAssessmentOutcome;
use super::GuardianRejection;
use super::GuardianRejectionCircuitBreakerAction;
use super::approval_request::guardian_assessment_action;
use super::approval_request::guardian_request_target_item_id;
use super::approval_request::guardian_request_turn_id;
use super::approval_request::guardian_reviewed_action;
use super::metrics::emit_guardian_review_metrics;
use super::prompt::guardian_output_schema;
use super::prompt::parse_guardian_assessment;
use super::review_session::GuardianReviewSessionOutcome;
use super::review_session::GuardianReviewSessionParams;
use super::review_session::build_guardian_review_session_config;

const GUARDIAN_REJECTION_INSTRUCTIONS: &str = concat!(
    "The agent must not attempt to achieve the same outcome via workaround, ",
    "indirect execution, or policy circumvention. ",
    "Proceed only with a materially safer alternative, ",
    "or if the user explicitly approves the action after being informed of the risk. ",
    "Otherwise, stop and request user input.",
);

const GUARDIAN_TIMEOUT_INSTRUCTIONS: &str = concat!(
    "The automatic permission approval review did not finish before its deadline. ",
    "Do not assume the action is unsafe based on the timeout alone. ",
    "You may retry once, or ask the user for guidance or explicit approval.",
);

const GUARDIAN_REVIEW_MAX_ATTEMPTS: i64 = 3;
const GUARDIAN_REVIEW_THREAD_STACK_SIZE: usize = 8 * 1024 * 1024;
const GUARDIAN_REVIEW_MAX_CONCURRENCY: usize = 4;
const GUARDIAN_REVIEW_QUEUE_CAPACITY: usize = 4;

static GUARDIAN_REVIEW_EXECUTOR: LazyLock<Result<GuardianReviewExecutor, String>> =
    LazyLock::new(GuardianReviewExecutor::new);

struct GuardianReviewExecutor {
    sender: mpsc::Sender<GuardianReviewJob>,
}

struct GuardianReviewJob {
    session: Arc<Session>,
    turn: Arc<TurnContext>,
    review_id: String,
    request: GuardianApprovalRequest,
    retry_reason: Option<String>,
    approval_request_source: GuardianApprovalRequestSource,
    cancel_token: CancellationToken,
    deadline: Instant,
    response: oneshot::Sender<ReviewDecision>,
}

enum GuardianReviewAdmission {
    Acquired(OwnedSemaphorePermit),
    TimedOut,
    Cancelled,
    Closed,
}

impl GuardianReviewExecutor {
    fn new() -> Result<Self, String> {
        Self::new_with_capacity(
            Arc::new(Semaphore::new(GUARDIAN_REVIEW_MAX_CONCURRENCY)),
            GUARDIAN_REVIEW_QUEUE_CAPACITY,
        )
    }

    fn new_with_capacity(
        capacity: Arc<Semaphore>,
        queue_capacity: usize,
    ) -> Result<Self, String> {
        let (sender, receiver) = mpsc::channel(queue_capacity);
        drop(
            std::thread::Builder::new()
                .name("codex-guardian-review".to_string())
                .stack_size(GUARDIAN_REVIEW_THREAD_STACK_SIZE)
                .spawn(move || {
                    let runtime = match tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    {
                        Ok(runtime) => runtime,
                        Err(err) => {
                            warn!(%err, "failed to create shared guardian review runtime");
                            return;
                        }
                    };
                    let local = tokio::task::LocalSet::new();
                    local.block_on(
                        &runtime,
                        run_guardian_review_executor(receiver, capacity),
                    );
                })
                .map_err(|err| err.to_string())?,
        );
        Ok(Self { sender })
    }
}

async fn run_guardian_review_job(job: GuardianReviewJob) {
    let GuardianReviewJob {
        session,
        turn,
        review_id,
        request,
        retry_reason,
        approval_request_source,
        cancel_token,
        deadline,
        response,
    } = job;
    let decision = run_guardian_review(
        session,
        turn,
        review_id,
        request,
        retry_reason,
        approval_request_source,
        Some(cancel_token),
        deadline,
    )
    .await;
    let _ = response.send(decision);
}

async fn terminalize_guardian_review_job(
    job: GuardianReviewJob,
    error: GuardianReviewError,
) {
    let GuardianReviewJob {
        session,
        turn,
        review_id,
        request,
        retry_reason: _,
        approval_request_source,
        cancel_token: _,
        deadline: _,
        response,
    } = job;
    let decision = terminalize_guardian_review_before_execution(
        session,
        turn,
        review_id,
        &request,
        approval_request_source,
        error,
    )
    .await;
    let _ = response.send(decision);
}

async fn acquire_guardian_review_capacity(
    capacity: Arc<Semaphore>,
    deadline: Instant,
    cancel_token: &CancellationToken,
) -> GuardianReviewAdmission {
    tokio::select! {
        biased;
        _ = cancel_token.cancelled() => GuardianReviewAdmission::Cancelled,
        _ = sleep_until(deadline) => GuardianReviewAdmission::TimedOut,
        permit = capacity.acquire_owned() => match permit {
            Ok(permit) => GuardianReviewAdmission::Acquired(permit),
            Err(_) => GuardianReviewAdmission::Closed,
        },
    }
}

async fn run_guardian_review_executor(
    mut receiver: mpsc::Receiver<GuardianReviewJob>,
    capacity: Arc<Semaphore>,
) {
    while let Some(job) = receiver.recv().await {
        match acquire_guardian_review_capacity(
            Arc::clone(&capacity),
            job.deadline,
            &job.cancel_token,
        )
        .await
        {
            GuardianReviewAdmission::Acquired(permit) => {
                drop(tokio::task::spawn_local(async move {
                    run_guardian_review_job(job).await;
                    drop(permit);
                }));
            }
            GuardianReviewAdmission::Cancelled => {
                terminalize_guardian_review_job(job, GuardianReviewError::Cancelled).await;
            }
            GuardianReviewAdmission::TimedOut => {
                terminalize_guardian_review_job(job, GuardianReviewError::Timeout).await;
            }
            GuardianReviewAdmission::Closed => {
                terminalize_guardian_review_job(
                    job,
                    GuardianReviewError::session(anyhow::anyhow!(
                        "shared guardian review capacity closed before execution"
                    )),
                )
                .await;
            }
        }
    }
}

pub(crate) fn new_guardian_review_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub(crate) async fn guardian_rejection_message(session: &Session, review_id: &str) -> String {
    let rejection = session
        .services
        .guardian_rejections
        .lock()
        .await
        .remove(review_id)
        .filter(|rejection| !rejection.rationale.trim().is_empty())
        .unwrap_or_else(|| GuardianRejection {
            rationale: "Auto-reviewer denied the action without a specific rationale.".to_string(),
            source: GuardianAssessmentDecisionSource::Agent,
        });
    match rejection.source {
        GuardianAssessmentDecisionSource::Agent => format!(
            "This action was rejected due to unacceptable risk.\nReason: {}\n{}",
            rejection.rationale.trim(),
            GUARDIAN_REJECTION_INSTRUCTIONS
        ),
    }
}

pub(crate) fn guardian_timeout_message() -> String {
    GUARDIAN_TIMEOUT_INSTRUCTIONS.to_string()
}

#[derive(Debug)]
pub(super) enum GuardianReviewOutcome {
    Completed(GuardianAssessment),
    Error(GuardianReviewError),
}

#[derive(Debug)]
pub(super) enum GuardianReviewError {
    PromptBuild {
        message: String,
    },
    Session {
        message: String,
        error_info: Option<CodexErrorInfo>,
    },
    Parse {
        message: String,
    },
    Timeout,
    Cancelled,
}

impl GuardianReviewError {
    fn prompt_build(err: anyhow::Error) -> Self {
        Self::PromptBuild {
            message: err.to_string(),
        }
    }

    fn session(err: anyhow::Error) -> Self {
        Self::Session {
            message: err.to_string(),
            error_info: None,
        }
    }

    fn session_with_error_info(err: anyhow::Error, error_info: CodexErrorInfo) -> Self {
        Self::Session {
            message: err.to_string(),
            error_info: Some(error_info),
        }
    }

    fn parse(err: anyhow::Error) -> Self {
        Self::Parse {
            message: err.to_string(),
        }
    }

    fn failure_reason(&self) -> GuardianReviewFailureReason {
        match self {
            Self::PromptBuild { .. } => GuardianReviewFailureReason::PromptBuildError,
            Self::Session { .. } => GuardianReviewFailureReason::SessionError,
            Self::Parse { .. } => GuardianReviewFailureReason::ParseError,
            Self::Timeout => GuardianReviewFailureReason::Timeout,
            Self::Cancelled => GuardianReviewFailureReason::Cancelled,
        }
    }
}

fn guardian_risk_level_str(level: GuardianRiskLevel) -> &'static str {
    match level {
        GuardianRiskLevel::Low => "low",
        GuardianRiskLevel::Medium => "medium",
        GuardianRiskLevel::High => "high",
        GuardianRiskLevel::Critical => "critical",
    }
}

/// Whether this turn should route allowed approval prompts through the guardian
/// reviewer instead of surfacing them to the user. ARC may still block actions
/// earlier in the flow.
pub(crate) fn routes_approval_to_guardian(turn: &TurnContext) -> bool {
    routes_approval_to_guardian_with_reviewer(turn, turn.config.approvals_reviewer)
}

/// Whether an approval with its own reviewer selection should be routed through guardian.
pub(crate) fn routes_approval_to_guardian_with_reviewer(
    turn: &TurnContext,
    approvals_reviewer: ApprovalsReviewer,
) -> bool {
    matches!(
        turn.approval_policy.value(),
        AskForApproval::OnRequest | AskForApproval::Granular(_)
    ) && approvals_reviewer == ApprovalsReviewer::AutoReview
}

pub(crate) fn is_guardian_reviewer_source(
    session_source: &codex_protocol::protocol::SessionSource,
) -> bool {
    matches!(
        session_source,
        codex_protocol::protocol::SessionSource::SubAgent(SubAgentSource::Other(label))
            if label == GUARDIAN_REVIEWER_NAME
    )
}

fn track_guardian_review(
    session: &Session,
    tracking: &GuardianReviewTrackContext,
    approval_request_source: GuardianApprovalRequestSource,
    reviewed_action: &GuardianReviewedAction,
    result: GuardianReviewAnalyticsResult,
    completed_at_ms: u64,
) {
    emit_guardian_review_metrics(
        &session.services.session_telemetry,
        &result,
        approval_request_source,
        reviewed_action,
        completed_at_ms.saturating_sub(tracking.started_at_ms),
    );
    session
        .services
        .analytics_events_client
        .track_guardian_review(tracking, result, completed_at_ms);
}

async fn record_guardian_non_denial(session: &Arc<Session>, turn_id: &str) {
    session
        .services
        .guardian_rejection_circuit_breaker
        .lock()
        .await
        .record_non_denial(turn_id);
}

async fn record_guardian_denial(session: &Arc<Session>, turn: &Arc<TurnContext>, turn_id: &str) {
    let action = session
        .services
        .guardian_rejection_circuit_breaker
        .lock()
        .await
        .record_denial(turn_id);
    let GuardianRejectionCircuitBreakerAction::InterruptTurn {
        consecutive_denials,
        recent_denials,
    } = action
    else {
        return;
    };

    if session.turn_context_for_sub_id(turn_id).await.is_none() {
        return;
    }

    session
        .send_event(
            turn.as_ref(),
            EventMsg::GuardianWarning(WarningEvent {
                message: format!(
                    "Automatic approval review rejected too many approval requests for this turn ({consecutive_denials} consecutive, {recent_denials} in the last {AUTO_REVIEW_DENIAL_WINDOW_SIZE} reviews); interrupting the turn."
                ),
            }),
        )
        .await;

    let runtime_handle = session.services.runtime_handle.clone();
    let session = Arc::clone(session);
    let turn_id = turn_id.to_string();
    let _abort_task = runtime_handle.spawn(async move {
        session
            .abort_turn_if_active(&turn_id, TurnAbortReason::Interrupted)
            .await;
    });
}

#[cfg(test)]
pub(crate) async fn record_guardian_denial_for_test(
    session: &Arc<Session>,
    turn: &Arc<TurnContext>,
    turn_id: &str,
) {
    record_guardian_denial(session, turn, turn_id).await;
}

async fn terminalize_guardian_review_before_execution(
    session: Arc<Session>,
    turn: Arc<TurnContext>,
    review_id: String,
    request: &GuardianApprovalRequest,
    approval_request_source: GuardianApprovalRequestSource,
    error: GuardianReviewError,
) -> ReviewDecision {
    let target_item_id = guardian_request_target_item_id(request).map(str::to_string);
    let assessment_turn_id = guardian_request_turn_id(request, &turn.sub_id).to_string();
    let action_summary = guardian_assessment_action(request);
    let reviewed_action = guardian_reviewed_action(request);
    let review_tracking = GuardianReviewTrackContext::new(
        session.thread_id.to_string(),
        assessment_turn_id.clone(),
        review_id.clone(),
        target_item_id.clone(),
        approval_request_source,
        reviewed_action.clone(),
        GUARDIAN_REVIEW_TIMEOUT.as_millis() as u64,
    );
    let started_at_ms = review_tracking.started_at_ms.try_into().unwrap_or_default();
    session
        .send_event(
            turn.as_ref(),
            EventMsg::GuardianAssessment(GuardianAssessmentEvent {
                id: review_id.clone(),
                target_item_id: target_item_id.clone(),
                turn_id: assessment_turn_id.clone(),
                started_at_ms,
                completed_at_ms: None,
                status: GuardianAssessmentStatus::InProgress,
                risk_level: None,
                user_authorization: None,
                rationale: None,
                decision_source: None,
                action: action_summary.clone(),
            }),
        )
        .await;

    let completed_at_ms = now_unix_timestamp_ms();
    match error {
        GuardianReviewError::Timeout => {
            let rationale =
                "Automatic approval review timed out while evaluating the requested approval."
                    .to_string();
            track_guardian_review(
                session.as_ref(),
                &review_tracking,
                approval_request_source,
                &reviewed_action,
                GuardianReviewAnalyticsResult {
                    decision: GuardianReviewDecision::Denied,
                    terminal_status: GuardianReviewTerminalStatus::TimedOut,
                    failure_reason: Some(GuardianReviewFailureReason::Timeout),
                    ..GuardianReviewAnalyticsResult::without_session()
                },
                completed_at_ms.try_into().unwrap_or_default(),
            );
            session
                .send_event(
                    turn.as_ref(),
                    EventMsg::GuardianWarning(WarningEvent {
                        message: rationale.clone(),
                    }),
                )
                .await;
            session
                .send_event(
                    turn.as_ref(),
                    EventMsg::GuardianAssessment(GuardianAssessmentEvent {
                        id: review_id,
                        target_item_id,
                        turn_id: assessment_turn_id.clone(),
                        started_at_ms,
                        completed_at_ms: Some(completed_at_ms),
                        status: GuardianAssessmentStatus::TimedOut,
                        risk_level: None,
                        user_authorization: None,
                        rationale: Some(rationale),
                        decision_source: Some(GuardianAssessmentDecisionSource::Agent),
                        action: action_summary,
                    }),
                )
                .await;
            record_guardian_non_denial(&session, &assessment_turn_id).await;
            ReviewDecision::TimedOut
        }
        GuardianReviewError::Cancelled => {
            track_guardian_review(
                session.as_ref(),
                &review_tracking,
                approval_request_source,
                &reviewed_action,
                GuardianReviewAnalyticsResult {
                    decision: GuardianReviewDecision::Aborted,
                    terminal_status: GuardianReviewTerminalStatus::Aborted,
                    failure_reason: Some(GuardianReviewFailureReason::Cancelled),
                    ..GuardianReviewAnalyticsResult::without_session()
                },
                completed_at_ms.try_into().unwrap_or_default(),
            );
            session
                .send_event(
                    turn.as_ref(),
                    EventMsg::GuardianAssessment(GuardianAssessmentEvent {
                        id: review_id,
                        target_item_id,
                        turn_id: assessment_turn_id.clone(),
                        started_at_ms,
                        completed_at_ms: Some(completed_at_ms),
                        status: GuardianAssessmentStatus::Aborted,
                        risk_level: None,
                        user_authorization: None,
                        rationale: None,
                        decision_source: Some(GuardianAssessmentDecisionSource::Agent),
                        action: action_summary,
                    }),
                )
                .await;
            record_guardian_non_denial(&session, &assessment_turn_id).await;
            ReviewDecision::Abort
        }
        error @ (GuardianReviewError::PromptBuild { .. }
        | GuardianReviewError::Session { .. }
        | GuardianReviewError::Parse { .. }) => {
            let message = match &error {
                GuardianReviewError::PromptBuild { message }
                | GuardianReviewError::Session { message, .. }
                | GuardianReviewError::Parse { message } => message,
                GuardianReviewError::Timeout | GuardianReviewError::Cancelled => {
                    "guardian review failed"
                }
            };
            let rationale = format!("Automatic approval review failed: {message}");
            track_guardian_review(
                session.as_ref(),
                &review_tracking,
                approval_request_source,
                &reviewed_action,
                GuardianReviewAnalyticsResult {
                    decision: GuardianReviewDecision::Denied,
                    terminal_status: GuardianReviewTerminalStatus::FailedClosed,
                    failure_reason: Some(error.failure_reason()),
                    ..GuardianReviewAnalyticsResult::without_session()
                },
                completed_at_ms.try_into().unwrap_or_default(),
            );
            session
                .send_event(
                    turn.as_ref(),
                    EventMsg::GuardianWarning(WarningEvent {
                        message: format!(
                            "Automatic approval review denied (risk: high, authorization: unknown): {rationale}"
                        ),
                    }),
                )
                .await;
            session.services.guardian_rejections.lock().await.insert(
                review_id.clone(),
                GuardianRejection {
                    rationale: rationale.clone(),
                    source: GuardianAssessmentDecisionSource::Agent,
                },
            );
            session
                .send_event(
                    turn.as_ref(),
                    EventMsg::GuardianAssessment(GuardianAssessmentEvent {
                        id: review_id,
                        target_item_id,
                        turn_id: assessment_turn_id.clone(),
                        started_at_ms,
                        completed_at_ms: Some(completed_at_ms),
                        status: GuardianAssessmentStatus::Denied,
                        risk_level: Some(GuardianRiskLevel::High),
                        user_authorization: Some(GuardianUserAuthorization::Unknown),
                        rationale: Some(rationale),
                        decision_source: Some(GuardianAssessmentDecisionSource::Agent),
                        action: action_summary,
                    }),
                )
                .await;
            record_guardian_non_denial(&session, &assessment_turn_id).await;
            ReviewDecision::Denied
        }
    }
}

/// This function always fails closed: timeouts, review-session failures, and
/// parse failures all block execution, but timeouts are still surfaced to the
/// caller as distinct from explicit guardian denials.
async fn run_guardian_review(
    session: Arc<Session>,
    turn: Arc<TurnContext>,
    review_id: String,
    request: GuardianApprovalRequest,
    retry_reason: Option<String>,
    approval_request_source: GuardianApprovalRequestSource,
    external_cancel: Option<CancellationToken>,
    deadline: Instant,
) -> ReviewDecision {
    let target_item_id = guardian_request_target_item_id(&request).map(str::to_string);
    let assessment_turn_id = guardian_request_turn_id(&request, &turn.sub_id).to_string();
    let action_summary = guardian_assessment_action(&request);
    let reviewed_action = guardian_reviewed_action(&request);
    let review_tracking = GuardianReviewTrackContext::new(
        session.thread_id.to_string(),
        assessment_turn_id.clone(),
        review_id.clone(),
        target_item_id.clone(),
        approval_request_source,
        reviewed_action.clone(),
        GUARDIAN_REVIEW_TIMEOUT.as_millis() as u64,
    );
    let started_at_ms = review_tracking.started_at_ms.try_into().unwrap_or_default();
    session
        .send_event(
            turn.as_ref(),
            EventMsg::GuardianAssessment(GuardianAssessmentEvent {
                id: review_id.clone(),
                target_item_id: target_item_id.clone(),
                turn_id: assessment_turn_id.clone(),
                started_at_ms,
                completed_at_ms: None,
                status: GuardianAssessmentStatus::InProgress,
                risk_level: None,
                user_authorization: None,
                rationale: None,
                decision_source: None,
                action: action_summary.clone(),
            }),
        )
        .await;

    if external_cancel
        .as_ref()
        .is_some_and(CancellationToken::is_cancelled)
    {
        let completed_at_ms = now_unix_timestamp_ms();
        track_guardian_review(
            session.as_ref(),
            &review_tracking,
            approval_request_source,
            &reviewed_action,
            GuardianReviewAnalyticsResult {
                decision: GuardianReviewDecision::Aborted,
                terminal_status: GuardianReviewTerminalStatus::Aborted,
                failure_reason: Some(GuardianReviewFailureReason::Cancelled),
                ..GuardianReviewAnalyticsResult::without_session()
            },
            completed_at_ms.try_into().unwrap_or_default(),
        );
        session
            .send_event(
                turn.as_ref(),
                EventMsg::GuardianAssessment(GuardianAssessmentEvent {
                    id: review_id,
                    target_item_id,
                    turn_id: assessment_turn_id.clone(),
                    started_at_ms,
                    completed_at_ms: Some(completed_at_ms),
                    status: GuardianAssessmentStatus::Aborted,
                    risk_level: None,
                    user_authorization: None,
                    rationale: None,
                    decision_source: Some(GuardianAssessmentDecisionSource::Agent),
                    action: action_summary,
                }),
            )
            .await;
        record_guardian_non_denial(&session, &assessment_turn_id).await;
        return ReviewDecision::Abort;
    }

    let terminal_action = action_summary.clone();
    let schema = guardian_output_schema();
    let (outcome, analytics_result) = Box::pin(run_guardian_review_session_with_retry(
        session.clone(),
        turn.clone(),
        request,
        retry_reason.clone(),
        schema,
        external_cancel,
        deadline,
        GUARDIAN_REVIEW_MAX_ATTEMPTS,
    ))
    .await;

    let completed_at_ms = now_unix_timestamp_ms();
    let (assessment, count_denial_for_circuit_breaker) = match outcome {
        GuardianReviewOutcome::Completed(assessment) => {
            let approved = matches!(assessment.outcome, GuardianAssessmentOutcome::Allow);
            track_guardian_review(
                session.as_ref(),
                &review_tracking,
                approval_request_source,
                &reviewed_action,
                GuardianReviewAnalyticsResult {
                    decision: if approved {
                        GuardianReviewDecision::Approved
                    } else {
                        GuardianReviewDecision::Denied
                    },
                    terminal_status: if approved {
                        GuardianReviewTerminalStatus::Approved
                    } else {
                        GuardianReviewTerminalStatus::Denied
                    },
                    failure_reason: None,
                    risk_level: Some(assessment.risk_level),
                    user_authorization: Some(assessment.user_authorization),
                    outcome: Some(assessment.outcome),
                    ..analytics_result
                },
                completed_at_ms.try_into().unwrap_or_default(),
            );
            let count_denial_for_circuit_breaker =
                matches!(assessment.outcome, GuardianAssessmentOutcome::Deny);
            (assessment, count_denial_for_circuit_breaker)
        }
        GuardianReviewOutcome::Error(error) => match error {
            GuardianReviewError::Timeout => {
                let rationale =
                    "Automatic approval review timed out while evaluating the requested approval."
                        .to_string();
                track_guardian_review(
                    session.as_ref(),
                    &review_tracking,
                    approval_request_source,
                    &reviewed_action,
                    GuardianReviewAnalyticsResult {
                        decision: GuardianReviewDecision::Denied,
                        terminal_status: GuardianReviewTerminalStatus::TimedOut,
                        failure_reason: Some(error.failure_reason()),
                        ..analytics_result
                    },
                    completed_at_ms.try_into().unwrap_or_default(),
                );
                session
                    .send_event(
                        turn.as_ref(),
                        EventMsg::GuardianWarning(WarningEvent {
                            message: rationale.clone(),
                        }),
                    )
                    .await;
                session
                    .send_event(
                        turn.as_ref(),
                        EventMsg::GuardianAssessment(GuardianAssessmentEvent {
                            id: review_id,
                            target_item_id,
                            turn_id: assessment_turn_id.clone(),
                            started_at_ms,
                            completed_at_ms: Some(completed_at_ms),
                            status: GuardianAssessmentStatus::TimedOut,
                            risk_level: None,
                            user_authorization: None,
                            rationale: Some(rationale),
                            decision_source: Some(GuardianAssessmentDecisionSource::Agent),
                            action: terminal_action,
                        }),
                    )
                    .await;
                record_guardian_non_denial(&session, &assessment_turn_id).await;
                return ReviewDecision::TimedOut;
            }
            GuardianReviewError::Cancelled => {
                track_guardian_review(
                    session.as_ref(),
                    &review_tracking,
                    approval_request_source,
                    &reviewed_action,
                    GuardianReviewAnalyticsResult {
                        decision: GuardianReviewDecision::Aborted,
                        terminal_status: GuardianReviewTerminalStatus::Aborted,
                        failure_reason: Some(error.failure_reason()),
                        ..analytics_result
                    },
                    completed_at_ms.try_into().unwrap_or_default(),
                );
                session
                    .send_event(
                        turn.as_ref(),
                        EventMsg::GuardianAssessment(GuardianAssessmentEvent {
                            id: review_id,
                            target_item_id,
                            turn_id: assessment_turn_id.clone(),
                            started_at_ms,
                            completed_at_ms: Some(completed_at_ms),
                            status: GuardianAssessmentStatus::Aborted,
                            risk_level: None,
                            user_authorization: None,
                            rationale: None,
                            decision_source: Some(GuardianAssessmentDecisionSource::Agent),
                            action: action_summary,
                        }),
                    )
                    .await;
                record_guardian_non_denial(&session, &assessment_turn_id).await;
                return ReviewDecision::Abort;
            }
            GuardianReviewError::PromptBuild { .. }
            | GuardianReviewError::Session { .. }
            | GuardianReviewError::Parse { .. } => {
                let message = match &error {
                    GuardianReviewError::PromptBuild { message }
                    | GuardianReviewError::Session { message, .. }
                    | GuardianReviewError::Parse { message } => message,
                    GuardianReviewError::Timeout | GuardianReviewError::Cancelled => {
                        "guardian review failed"
                    }
                };
                let rationale = format!("Automatic approval review failed: {message}");
                track_guardian_review(
                    session.as_ref(),
                    &review_tracking,
                    approval_request_source,
                    &reviewed_action,
                    GuardianReviewAnalyticsResult {
                        decision: GuardianReviewDecision::Denied,
                        terminal_status: GuardianReviewTerminalStatus::FailedClosed,
                        failure_reason: Some(error.failure_reason()),
                        ..analytics_result
                    },
                    completed_at_ms.try_into().unwrap_or_default(),
                );
                (
                    GuardianAssessment {
                        risk_level: GuardianRiskLevel::High,
                        user_authorization: GuardianUserAuthorization::Unknown,
                        outcome: GuardianAssessmentOutcome::Deny,
                        rationale,
                    },
                    false,
                )
            }
        },
    };

    let approved = match assessment.outcome {
        GuardianAssessmentOutcome::Allow => true,
        GuardianAssessmentOutcome::Deny => false,
    };
    let verdict = if approved { "approved" } else { "denied" };
    let user_authorization = match assessment.user_authorization {
        GuardianUserAuthorization::Unknown => "unknown",
        GuardianUserAuthorization::Low => "low",
        GuardianUserAuthorization::Medium => "medium",
        GuardianUserAuthorization::High => "high",
    };
    let warning = format!(
        "Automatic approval review {verdict} (risk: {}, authorization: {user_authorization}): {}",
        guardian_risk_level_str(assessment.risk_level),
        assessment.rationale
    );
    session
        .send_event(
            turn.as_ref(),
            EventMsg::GuardianWarning(WarningEvent { message: warning }),
        )
        .await;
    let status = if approved {
        GuardianAssessmentStatus::Approved
    } else {
        GuardianAssessmentStatus::Denied
    };
    {
        let mut rationales = session.services.guardian_rejections.lock().await;
        if approved {
            rationales.remove(&review_id);
        } else {
            let rejection = GuardianRejection {
                rationale: assessment.rationale.clone(),
                source: GuardianAssessmentDecisionSource::Agent,
            };
            rationales.insert(review_id.clone(), rejection);
        }
    }
    session
        .send_event(
            turn.as_ref(),
            EventMsg::GuardianAssessment(GuardianAssessmentEvent {
                id: review_id,
                target_item_id,
                turn_id: assessment_turn_id.clone(),
                started_at_ms,
                completed_at_ms: Some(completed_at_ms),
                status,
                risk_level: Some(assessment.risk_level),
                user_authorization: Some(assessment.user_authorization),
                rationale: Some(assessment.rationale.clone()),
                decision_source: Some(GuardianAssessmentDecisionSource::Agent),
                action: terminal_action,
            }),
        )
        .await;

    if count_denial_for_circuit_breaker {
        record_guardian_denial(&session, &turn, &assessment_turn_id).await;
    } else {
        record_guardian_non_denial(&session, &assessment_turn_id).await;
    }

    if approved {
        ReviewDecision::Approved
    } else {
        ReviewDecision::Denied
    }
}

/// Public entrypoint for approval requests that should be reviewed by guardian.
pub(crate) async fn review_approval_request(
    session: &Arc<Session>,
    turn: &Arc<TurnContext>,
    review_id: String,
    request: GuardianApprovalRequest,
    retry_reason: Option<String>,
) -> ReviewDecision {
    let cancel_token = CancellationToken::new();
    let cancel_guard = cancel_token.clone().drop_guard();
    let review_rx = spawn_approval_request_review(
        Arc::clone(session),
        Arc::clone(turn),
        review_id,
        request,
        retry_reason,
        GuardianApprovalRequestSource::MainTurn,
        cancel_token,
    )
    .await;
    let decision = review_rx.await.unwrap_or_default();
    drop(cancel_guard);
    decision
}

pub(crate) async fn review_approval_request_with_cancel(
    session: &Arc<Session>,
    turn: &Arc<TurnContext>,
    review_id: String,
    request: GuardianApprovalRequest,
    retry_reason: Option<String>,
    approval_request_source: GuardianApprovalRequestSource,
    cancel_token: CancellationToken,
) -> ReviewDecision {
    run_guardian_review(
        Arc::clone(session),
        Arc::clone(turn),
        review_id,
        request,
        retry_reason,
        approval_request_source,
        Some(cancel_token),
        Instant::now() + GUARDIAN_REVIEW_TIMEOUT,
    )
    .await
}

async fn submit_guardian_review_job(
    executor: &GuardianReviewExecutor,
    job: GuardianReviewJob,
) {
    let admission = tokio::select! {
        biased;
        _ = job.cancel_token.cancelled() => Err(GuardianReviewError::Cancelled),
        _ = sleep_until(job.deadline) => Err(GuardianReviewError::Timeout),
        permit = executor.sender.reserve() => permit.map_err(|_| {
            GuardianReviewError::session(anyhow::anyhow!(
                "shared guardian review executor closed before queue admission"
            ))
        }),
    };
    match admission {
        Ok(admission) => {
            admission.send(job);
        }
        Err(error) => {
            terminalize_guardian_review_job(job, error).await;
        }
    }
}

pub(crate) async fn spawn_approval_request_review(
    session: Arc<Session>,
    turn: Arc<TurnContext>,
    review_id: String,
    request: GuardianApprovalRequest,
    retry_reason: Option<String>,
    approval_request_source: GuardianApprovalRequestSource,
    cancel_token: CancellationToken,
) -> oneshot::Receiver<ReviewDecision> {
    let (tx, rx) = oneshot::channel();
    let deadline = Instant::now() + GUARDIAN_REVIEW_TIMEOUT;
    let job = GuardianReviewJob {
        session,
        turn,
        review_id,
        request,
        retry_reason,
        approval_request_source,
        cancel_token,
        deadline,
        response: tx,
    };
    let executor = match &*GUARDIAN_REVIEW_EXECUTOR {
        Ok(executor) => executor,
        Err(err) => {
            warn!(%err, "failed to start shared guardian review executor");
            let terminal_error = if job.cancel_token.is_cancelled() {
                GuardianReviewError::Cancelled
            } else {
                GuardianReviewError::session(anyhow::anyhow!(
                    "failed to start shared guardian review executor: {err}"
                ))
            };
            terminalize_guardian_review_job(
                job,
                terminal_error,
            )
            .await;
            return rx;
        }
    };
    submit_guardian_review_job(executor, job).await;
    rx
}

pub(super) struct GuardianReviewSessionConfig {
    pub(super) spawn_config: crate::config::Config,
    model: String,
    reasoning_effort: Option<codex_protocol::openai_models::ReasoningEffort>,
    default_review_model_id: String,
    catalog_contains_auto_review: bool,
    model_overridden: bool,
    model_override: Option<String>,
}

pub(super) async fn guardian_review_session_config(
    session: &Session,
    turn: &TurnContext,
) -> anyhow::Result<GuardianReviewSessionConfig> {
    let network_proxy = session.services.network_proxy.load_full();
    let live_network_config = match network_proxy.as_ref() {
        Some(network_proxy) => Some(network_proxy.proxy().current_cfg().await?),
        None => None,
    };
    let available_models = session
        .services
        .models_manager
        .list_models_snapshot(
            codex_models_manager::manager::RefreshStrategy::Offline,
            turn.config.http_client_factory(),
        )
        .await;
    let default_review_model_id = turn.provider.approval_review_preferred_model();
    let preferred_reasoning_effort = |supports_low: bool, fallback| {
        if supports_low {
            Some(codex_protocol::openai_models::ReasoningEffort::Low)
        } else {
            fallback
        }
    };
    let model_override = turn.model_info.auto_review_model_override.as_deref();
    let review_model_id = model_override.unwrap_or(default_review_model_id);
    let review_model = available_models
        .iter()
        .find(|preset| preset.model == review_model_id);
    let guardian_catalog_contains_auto_review = available_models
        .iter()
        .any(|preset| preset.model == default_review_model_id);
    let guardian_review_model_overridden = model_override.is_some();
    let guardian_review_model_override = model_override.map(str::to_string);
    let (guardian_model, guardian_reasoning_effort) = if let Some(preset) = review_model {
        let reasoning_effort = preferred_reasoning_effort(
            preset
                .supported_reasoning_efforts
                .iter()
                .any(|effort| effort.effort == codex_protocol::openai_models::ReasoningEffort::Low),
            Some(preset.default_reasoning_effort.clone()),
        );
        (review_model_id.to_string(), reasoning_effort)
    } else {
        let reasoning_effort = preferred_reasoning_effort(
            turn.model_info
                .supported_reasoning_levels
                .iter()
                .any(|preset| preset.effort == codex_protocol::openai_models::ReasoningEffort::Low),
            turn.reasoning_effort
                .clone()
                .or_else(|| turn.model_info.default_reasoning_level.clone()),
        );
        (
            model_override
                .unwrap_or(turn.model_info.slug.as_str())
                .to_string(),
            reasoning_effort,
        )
    };

    let spawn_config = build_guardian_review_session_config(
        turn.config.as_ref(),
        live_network_config,
        guardian_model.as_str(),
        guardian_reasoning_effort.clone(),
    )?;
    Ok(GuardianReviewSessionConfig {
        spawn_config,
        model: guardian_model,
        reasoning_effort: guardian_reasoning_effort,
        default_review_model_id: default_review_model_id.to_string(),
        catalog_contains_auto_review: guardian_catalog_contains_auto_review,
        model_overridden: guardian_review_model_overridden,
        model_override: guardian_review_model_override,
    })
}

/// Runs the guardian in a locked-down reusable review session.
///
/// The guardian itself should not mutate state or trigger further approvals, so
/// it is pinned to a read-only sandbox with `approval_policy = never` and
/// nonessential agent features disabled. When the cached trunk session is idle,
/// later approvals append onto that same guardian conversation to preserve a
/// stable prompt-cache key. If the trunk is already busy, the review runs in an
/// ephemeral fork from the last committed trunk rollout so parallel approvals
/// do not block each other or mutate the cached thread. The trunk is recreated
/// when the effective review-session config changes, and any future compaction
/// must continue to preserve the guardian policy as exact top-level developer
/// context. It may still reuse the parent's managed-network allowlist for
/// read-only checks, but it intentionally runs without inherited exec-policy
/// rules.
async fn run_guardian_review_session_before_deadline(
    session: Arc<Session>,
    turn: Arc<TurnContext>,
    request: GuardianApprovalRequest,
    retry_reason: Option<String>,
    schema: serde_json::Value,
    external_cancel: Option<CancellationToken>,
    deadline: Instant,
) -> (GuardianReviewOutcome, GuardianReviewAnalyticsResult) {
    let session_config = match guardian_review_session_config(session.as_ref(), turn.as_ref()).await
    {
        Ok(session_config) => session_config,
        Err(err) => {
            return (
                GuardianReviewOutcome::Error(GuardianReviewError::prompt_build(err)),
                GuardianReviewAnalyticsResult::without_session(),
            );
        }
    };
    let (session_outcome, session_analytics_result) = Box::pin(
        session
            .guardian_review_session
            .run_review(GuardianReviewSessionParams {
                parent_session: Arc::clone(&session),
                parent_turn: turn.clone(),
                spawn_config: session_config.spawn_config,
                request,
                retry_reason,
                schema,
                model: session_config.model,
                reasoning_effort: session_config.reasoning_effort,
                guardian_default_review_model_id: session_config.default_review_model_id,
                guardian_catalog_contains_auto_review: session_config.catalog_contains_auto_review,
                guardian_review_model_overridden: session_config.model_overridden,
                guardian_review_model_override: session_config.model_override,
                reasoning_summary: turn.reasoning_summary,
                personality: turn.personality,
                external_cancel,
                deadline,
            }),
    )
    .await;

    match session_outcome {
        GuardianReviewSessionOutcome::Completed(Ok(last_agent_message)) => match last_agent_message
        {
            Some(last_agent_message) => {
                match parse_guardian_assessment(Some(&last_agent_message)) {
                    Ok(assessment) => (
                        GuardianReviewOutcome::Completed(assessment),
                        session_analytics_result,
                    ),
                    Err(err) => (
                        GuardianReviewOutcome::Error(GuardianReviewError::parse(err)),
                        session_analytics_result,
                    ),
                }
            }
            None => (
                GuardianReviewOutcome::Error(GuardianReviewError::session(anyhow::anyhow!(
                    "guardian review completed without an assessment payload"
                ))),
                session_analytics_result,
            ),
        },
        GuardianReviewSessionOutcome::Completed(Err(err)) => (
            GuardianReviewOutcome::Error(GuardianReviewError::session(err)),
            session_analytics_result,
        ),
        GuardianReviewSessionOutcome::PromptBuildFailed(err) => (
            GuardianReviewOutcome::Error(GuardianReviewError::prompt_build(err)),
            session_analytics_result,
        ),
        GuardianReviewSessionOutcome::SessionFailed { error, error_info } => {
            let error = match error_info {
                Some(error_info) => GuardianReviewError::session_with_error_info(error, error_info),
                None => GuardianReviewError::session(error),
            };
            (
                GuardianReviewOutcome::Error(error),
                session_analytics_result,
            )
        }
        GuardianReviewSessionOutcome::TimedOut => (
            GuardianReviewOutcome::Error(GuardianReviewError::Timeout),
            session_analytics_result,
        ),
        GuardianReviewSessionOutcome::Aborted => (
            GuardianReviewOutcome::Error(GuardianReviewError::Cancelled),
            session_analytics_result,
        ),
    }
}

pub(super) async fn run_guardian_review_session_with_retry(
    session: Arc<Session>,
    turn: Arc<TurnContext>,
    request: GuardianApprovalRequest,
    retry_reason: Option<String>,
    schema: serde_json::Value,
    external_cancel: Option<CancellationToken>,
    deadline: Instant,
    max_attempts: i64,
) -> (GuardianReviewOutcome, GuardianReviewAnalyticsResult) {
    assert!(max_attempts > 0, "guardian review must run at least once");
    let mut attempt_count = 1;
    loop {
        let (outcome, mut analytics_result) = run_guardian_review_session_before_deadline(
            Arc::clone(&session),
            Arc::clone(&turn),
            request.clone(),
            retry_reason.clone(),
            schema.clone(),
            external_cancel.clone(),
            deadline,
        )
        .await;
        analytics_result.attempt_count = attempt_count;
        if attempt_count >= max_attempts || !should_retry_guardian_review(&outcome) {
            return (outcome, analytics_result);
        }
        if let Some(error) =
            wait_before_guardian_retry(attempt_count, deadline, external_cancel.as_ref()).await
        {
            return (GuardianReviewOutcome::Error(error), analytics_result);
        }
        attempt_count += 1;
    }
}

async fn wait_before_guardian_retry(
    attempt_count: i64,
    deadline: Instant,
    external_cancel: Option<&CancellationToken>,
) -> Option<GuardianReviewError> {
    let retry_delay = backoff(attempt_count as u64);
    let retry_at = (Instant::now() + retry_delay).min(deadline);
    tokio::select! {
        _ = sleep_until(retry_at) => {
            (Instant::now() >= deadline).then_some(GuardianReviewError::Timeout)
        }
        _ = async {
            if let Some(cancel_token) = external_cancel {
                cancel_token.cancelled().await;
            } else {
                std::future::pending::<()>().await;
            }
        } => Some(GuardianReviewError::Cancelled),
    }
}

fn should_retry_guardian_review(outcome: &GuardianReviewOutcome) -> bool {
    matches!(
        outcome,
        GuardianReviewOutcome::Error(
            GuardianReviewError::Session {
                error_info: Some(
                    CodexErrorInfo::ServerOverloaded
                        | CodexErrorInfo::HttpConnectionFailed { .. }
                        | CodexErrorInfo::ResponseStreamConnectionFailed { .. }
                        | CodexErrorInfo::InternalServerError
                        | CodexErrorInfo::ResponseStreamDisconnected { .. }
                ),
                ..
            } | GuardianReviewError::Parse { .. }
        )
    )
}

#[cfg(test)]
mod review_tests {
    use super::*;
    use core_test_support::PathBufExt;
    use core_test_support::test_path_buf;
    use std::time::Duration;

    fn guardian_review_job_for_test(
        session: Arc<Session>,
        turn: Arc<TurnContext>,
        review_id: &str,
        cancel_token: CancellationToken,
        deadline: Instant,
    ) -> (GuardianReviewJob, oneshot::Receiver<ReviewDecision>) {
        let (response, receiver) = oneshot::channel();
        (
            GuardianReviewJob {
                session,
                turn,
                review_id: review_id.to_string(),
                request: GuardianApprovalRequest::Shell {
                    id: format!("shell-{review_id}"),
                    command: vec!["git".to_string(), "push".to_string()],
                    cwd: test_path_buf("/repo").abs(),
                    sandbox_permissions: crate::sandboxing::SandboxPermissions::UseDefault,
                    additional_permissions: None,
                    justification: Some("exercise guardian executor admission".to_string()),
                },
                retry_reason: None,
                approval_request_source: GuardianApprovalRequestSource::MainTurn,
                cancel_token,
                deadline,
                response,
            },
            receiver,
        )
    }

    fn collect_guardian_lifecycle(
        events: &async_channel::Receiver<codex_protocol::protocol::Event>,
    ) -> (Vec<GuardianAssessmentStatus>, Vec<String>) {
        let mut statuses = Vec::new();
        let mut warnings = Vec::new();
        while let Ok(event) = events.try_recv() {
            match event.msg {
                EventMsg::GuardianAssessment(event) => statuses.push(event.status),
                EventMsg::GuardianWarning(event) => warnings.push(event.message),
                _ => {}
            }
        }
        (statuses, warnings)
    }

    fn assert_send<T: Send>(_: T) {}

    #[test]
    fn guardian_review_error_reason_distinguishes_error_kinds() {
        let parse_error = GuardianReviewError::parse(anyhow::anyhow!("bad guardian JSON"));
        let prompt_error = GuardianReviewError::prompt_build(anyhow::anyhow!("bad prompt/config"));
        let session_error =
            GuardianReviewError::session(anyhow::anyhow!("guardian runtime failed"));
        let structured_session_error = GuardianReviewError::session_with_error_info(
            anyhow::anyhow!("temporary guardian failure"),
            CodexErrorInfo::ServerOverloaded,
        );

        assert!(matches!(
            parse_error.failure_reason(),
            GuardianReviewFailureReason::ParseError
        ));
        assert!(matches!(
            prompt_error.failure_reason(),
            GuardianReviewFailureReason::PromptBuildError
        ));
        assert!(matches!(
            session_error.failure_reason(),
            GuardianReviewFailureReason::SessionError
        ));
        assert!(matches!(
            structured_session_error.failure_reason(),
            GuardianReviewFailureReason::SessionError
        ));
    }

    #[test]
    fn guardian_review_retry_only_retries_transient_session_and_parse_errors() {
        let assessment = GuardianAssessment {
            risk_level: GuardianRiskLevel::High,
            user_authorization: GuardianUserAuthorization::Unknown,
            outcome: GuardianAssessmentOutcome::Deny,
            rationale: "deny".to_string(),
        };
        let transient_error_info = [
            CodexErrorInfo::ServerOverloaded,
            CodexErrorInfo::HttpConnectionFailed {
                http_status_code: Some(502),
            },
            CodexErrorInfo::ResponseStreamConnectionFailed {
                http_status_code: Some(503),
            },
            CodexErrorInfo::InternalServerError,
            CodexErrorInfo::ResponseStreamDisconnected {
                http_status_code: None,
            },
        ];
        let mut outcomes = transient_error_info
            .into_iter()
            .map(|error_info| {
                (
                    GuardianReviewOutcome::Error(GuardianReviewError::session_with_error_info(
                        anyhow::anyhow!("transient session"),
                        error_info,
                    )),
                    true,
                )
            })
            .collect::<Vec<_>>();
        outcomes.extend([
            (GuardianReviewOutcome::Completed(assessment), false),
            (
                GuardianReviewOutcome::Error(GuardianReviewError::prompt_build(anyhow::anyhow!(
                    "prompt"
                ))),
                false,
            ),
            (
                GuardianReviewOutcome::Error(GuardianReviewError::session(anyhow::anyhow!(
                    "session"
                ))),
                false,
            ),
            (
                GuardianReviewOutcome::Error(GuardianReviewError::session_with_error_info(
                    anyhow::anyhow!("bad request"),
                    CodexErrorInfo::BadRequest,
                )),
                false,
            ),
            (
                GuardianReviewOutcome::Error(GuardianReviewError::parse(anyhow::anyhow!("parse"))),
                true,
            ),
            (
                GuardianReviewOutcome::Error(GuardianReviewError::Timeout),
                false,
            ),
            (
                GuardianReviewOutcome::Error(GuardianReviewError::Cancelled),
                false,
            ),
        ]);

        for (outcome, expected) in outcomes {
            assert_eq!(should_retry_guardian_review(&outcome), expected);
        }
    }

    #[tokio::test]
    async fn guardian_review_retry_wait_honors_cancellation() {
        let cancel_token = CancellationToken::new();
        cancel_token.cancel();

        let error = wait_before_guardian_retry(
            /*attempt_count*/ 1,
            Instant::now() + Duration::from_secs(/*secs*/ 1),
            Some(&cancel_token),
        )
        .await;

        assert!(matches!(error, Some(GuardianReviewError::Cancelled)));
    }

    #[tokio::test]
    async fn guardian_review_retry_wait_honors_deadline() {
        let error = wait_before_guardian_retry(
            /*attempt_count*/ 1,
            Instant::now(),
            /*external_cancel*/ None,
        )
        .await;

        assert!(matches!(error, Some(GuardianReviewError::Timeout)));
    }

    #[tokio::test]
    async fn shared_executor_submission_future_is_send() {
        let (session, turn) = crate::session::tests::make_session_and_context().await;
        assert_send(spawn_approval_request_review(
            Arc::new(session),
            Arc::new(turn),
            "send-safe-submission".to_string(),
            GuardianApprovalRequest::Shell {
                id: "shell-send-safe-submission".to_string(),
                command: vec!["git".to_string(), "push".to_string()],
                cwd: test_path_buf("/repo").abs(),
                sandbox_permissions: crate::sandboxing::SandboxPermissions::UseDefault,
                additional_permissions: None,
                justification: Some("compile-check guardian submission future".to_string()),
            },
            /*retry_reason*/ None,
            GuardianApprovalRequestSource::MainTurn,
            CancellationToken::new(),
        ));
    }

    #[tokio::test]
    async fn shared_executor_capacity_timeout_runs_terminal_lifecycle() {
        let executor = GuardianReviewExecutor::new_with_capacity(
            Arc::new(Semaphore::new(/*permits*/ 0)),
            /*queue_capacity*/ 1,
        )
        .expect("start test guardian executor");
        let (session, turn, events) =
            crate::session::tests::make_session_and_context_with_rx().await;
        {
            let mut circuit_breaker = session
                .services
                .guardian_rejection_circuit_breaker
                .lock()
                .await;
            assert_eq!(
                circuit_breaker.record_denial(&turn.sub_id),
                GuardianRejectionCircuitBreakerAction::Continue
            );
            assert_eq!(
                circuit_breaker.record_denial(&turn.sub_id),
                GuardianRejectionCircuitBreakerAction::Continue
            );
        }
        let (job, decision) = guardian_review_job_for_test(
            Arc::clone(&session),
            Arc::clone(&turn),
            "capacity-timeout",
            CancellationToken::new(),
            Instant::now(),
        );

        assert!(executor.sender.send(job).await.is_ok());
        assert_eq!(
            decision.await.expect("guardian capacity timeout decision"),
            ReviewDecision::TimedOut
        );

        let (statuses, warnings) = collect_guardian_lifecycle(&events);
        assert_eq!(
            statuses,
            vec![
                GuardianAssessmentStatus::InProgress,
                GuardianAssessmentStatus::TimedOut,
            ]
        );
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("timed out"));
        assert!(
            !session
                .services
                .guardian_rejections
                .lock()
                .await
                .contains_key("capacity-timeout")
        );
        assert_eq!(
            session
                .services
                .guardian_rejection_circuit_breaker
                .lock()
                .await
                .record_denial(&turn.sub_id),
            GuardianRejectionCircuitBreakerAction::Continue,
            "capacity timeout must reset consecutive denial accounting"
        );
    }

    #[tokio::test]
    async fn shared_executor_queue_saturation_preserves_timeout_and_abort_lifecycle() {
        let executor = GuardianReviewExecutor::new_with_capacity(
            Arc::new(Semaphore::new(/*permits*/ 0)),
            /*queue_capacity*/ 1,
        )
        .expect("start test guardian executor");
        let blocker_deadline = Instant::now() + Duration::from_secs(30);

        let (first_session, first_turn, _first_events) =
            crate::session::tests::make_session_and_context_with_rx().await;
        let first_cancel = CancellationToken::new();
        let (first_job, first_decision) = guardian_review_job_for_test(
            first_session,
            first_turn,
            "queue-blocker-one",
            first_cancel.clone(),
            blocker_deadline,
        );
        submit_guardian_review_job(&executor, first_job).await;

        let (second_session, second_turn, _second_events) =
            crate::session::tests::make_session_and_context_with_rx().await;
        let second_cancel = CancellationToken::new();
        let (second_job, second_decision) = guardian_review_job_for_test(
            second_session,
            second_turn,
            "queue-blocker-two",
            second_cancel.clone(),
            blocker_deadline,
        );
        submit_guardian_review_job(&executor, second_job).await;

        let (timeout_session, timeout_turn, timeout_events) =
            crate::session::tests::make_session_and_context_with_rx().await;
        let (timeout_job, timeout_decision) = guardian_review_job_for_test(
            timeout_session,
            timeout_turn,
            "queue-timeout",
            CancellationToken::new(),
            Instant::now(),
        );
        submit_guardian_review_job(&executor, timeout_job).await;
        assert_eq!(
            timeout_decision.await.expect("guardian queue timeout decision"),
            ReviewDecision::TimedOut
        );
        let (timeout_statuses, timeout_warnings) =
            collect_guardian_lifecycle(&timeout_events);
        assert_eq!(
            timeout_statuses,
            vec![
                GuardianAssessmentStatus::InProgress,
                GuardianAssessmentStatus::TimedOut,
            ]
        );
        assert_eq!(timeout_warnings.len(), 1);
        assert!(timeout_warnings[0].contains("timed out"));

        let (abort_session, abort_turn, abort_events) =
            crate::session::tests::make_session_and_context_with_rx().await;
        let abort_cancel = CancellationToken::new();
        abort_cancel.cancel();
        let (abort_job, abort_decision) = guardian_review_job_for_test(
            abort_session,
            abort_turn,
            "queue-abort",
            abort_cancel,
            blocker_deadline,
        );
        submit_guardian_review_job(&executor, abort_job).await;
        assert_eq!(
            abort_decision.await.expect("guardian queue abort decision"),
            ReviewDecision::Abort
        );
        let (abort_statuses, abort_warnings) = collect_guardian_lifecycle(&abort_events);
        assert_eq!(
            abort_statuses,
            vec![
                GuardianAssessmentStatus::InProgress,
                GuardianAssessmentStatus::Aborted,
            ]
        );
        assert!(abort_warnings.is_empty());

        first_cancel.cancel();
        second_cancel.cancel();
        assert_eq!(
            first_decision.await.expect("first queue blocker decision"),
            ReviewDecision::Abort
        );
        assert_eq!(
            second_decision.await.expect("second queue blocker decision"),
            ReviewDecision::Abort
        );
    }
}
