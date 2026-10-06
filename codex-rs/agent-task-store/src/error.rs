use crate::AdmissionRejectionReason;
use crate::AssignmentId;
use crate::AttemptId;
use crate::DependencyBlocker;
use crate::MissingEvidenceObligation;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("agent task coordinator belongs to root session {expected}, not {requested}")]
    RootSessionMismatch { expected: String, requested: String },
    #[error("{kind} must be a UUIDv7 value, got {value}")]
    InvalidUuidV7 { kind: &'static str, value: String },
    #[error("invalid repository scope: {0}")]
    InvalidScope(String),
    #[error("invalid assignment: {0}")]
    InvalidAssignment(String),
    #[error("typed assignment admission rejected: {reason}")]
    AdmissionRejected {
        reason: AdmissionRejectionReason,
        /// Existing assignment whose active work or sealed result satisfies this request.
        reusable_assignment_id: Option<AssignmentId>,
    },
    #[error("assignment {0} does not exist")]
    AssignmentNotFound(AssignmentId),
    #[error("attempt {0} does not exist")]
    AttemptNotFound(AttemptId),
    #[error("attempt {0} is already sealed")]
    AttemptSealed(AttemptId),
    #[error("attempt {0} is not the active current attempt")]
    AttemptNotActive(AttemptId),
    #[error("assignment {0} already has an immutable task capsule")]
    TaskCapsuleAlreadyAttached(AssignmentId),
    #[error("invalid task capsule: {0}")]
    InvalidTaskCapsule(String),
    #[error("attempt {0} already has a sealed receipt")]
    ReceiptAlreadySealed(AttemptId),
    #[error("dependency validation failed: {blockers:?}")]
    DependencyBlocked { blockers: Vec<DependencyBlocker> },
    #[error("only one immutable correction amendment is allowed for assignment {0}")]
    AmendmentLimitReached(AssignmentId),
    #[error("only worker assignments may create a correction attempt: {0}")]
    WorkerCorrectionRequired(AssignmentId),
    #[error("operation requires root authority")]
    RootAuthorityRequired,
    #[error("actor is not authorized to set the {gate} gate")]
    GateAuthorityRequired { gate: String },
    #[error("gate {gate} may be waived only through the root-authorized waiver operation")]
    GateWaiverRequired { gate: String },
    #[error("gate {gate} cannot be waived")]
    GateNotWaivable { gate: String },
    #[error("gate {gate} is already sealed")]
    GateAlreadySealed { gate: String },
    #[error("receipt criterion results do not match the assignment: {0}")]
    CriterionResultsInvalid(String),
    #[error("receipt references validation calls not owned by the current attempt: {call_ids:?}")]
    ValidationCallOwnership { call_ids: Vec<String> },
    #[error("validation call {0} is terminal and immutable")]
    ValidationCallImmutable(String),
    #[error("receipt references validation calls with incompatible status: {call_ids:?}")]
    ValidationCallStatusInvalid { call_ids: Vec<String> },
    #[error("completed receipt is missing required evidence: {obligations:?}")]
    RequiredEvidenceMissing {
        obligations: Vec<MissingEvidenceObligation>,
    },
    #[error("observation limit must be between 0 and 100, got {0}")]
    InvalidObservationLimit(usize),
    #[error("wake watermark {0} does not belong to this root session")]
    InvalidWakeWatermark(String),
    #[error("wake watermark cannot move backward from sequence {current} to {next}")]
    WakeWatermarkRegression { current: i64, next: i64 },
    #[error("assignment {0} has no durable repository identity")]
    RepositoryBindingMissing(AssignmentId),
    #[error("repository root does not match assignment {0}")]
    RepositoryMismatch(AssignmentId),
    #[error("binding limit must be between 0 and 256, got {0}")]
    InvalidBindingLimit(usize),
    #[error("agent task store contains invalid persisted data: {0}")]
    CorruptData(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Sql(#[from] sqlx::Error),
    #[error(transparent)]
    Migration(#[from] sqlx::migrate::MigrateError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub type StoreResult<T> = Result<T, StoreError>;
