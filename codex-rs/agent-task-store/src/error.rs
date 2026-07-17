use crate::AssignmentId;
use crate::AttemptId;
use crate::DependencyBlocker;
use crate::WriteClaimConflict;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("{kind} must be a UUIDv7 value, got {value}")]
    InvalidUuidV7 { kind: &'static str, value: String },
    #[error("invalid repository scope: {0}")]
    InvalidScope(String),
    #[error("invalid assignment: {0}")]
    InvalidAssignment(String),
    #[error("assignment {0} does not exist")]
    AssignmentNotFound(AssignmentId),
    #[error("attempt {0} does not exist")]
    AttemptNotFound(AttemptId),
    #[error("attempt {0} is already sealed")]
    AttemptSealed(AttemptId),
    #[error("attempt {0} already has a sealed receipt")]
    ReceiptAlreadySealed(AttemptId),
    #[error("dependency validation failed: {blockers:?}")]
    DependencyBlocked { blockers: Vec<DependencyBlocker> },
    #[error("active write claims overlap: {conflicts:?}")]
    WriteClaimConflict {
        conflicts: Vec<WriteClaimConflict>,
    },
    #[error("only one immutable correction amendment is allowed for assignment {0}")]
    AmendmentLimitReached(AssignmentId),
    #[error("operation requires root authority")]
    RootAuthorityRequired,
    #[error("gate {gate} cannot be waived")]
    GateNotWaivable { gate: String },
    #[error("gate {gate} is already sealed")]
    GateAlreadySealed { gate: String },
    #[error("receipt criterion results do not match the assignment: {0}")]
    CriterionResultsInvalid(String),
    #[error("receipt references validation calls not owned by the current attempt: {call_ids:?}")]
    ValidationCallOwnership { call_ids: Vec<String> },
    #[error("observation limit must be between 0 and 100, got {0}")]
    InvalidObservationLimit(usize),
    #[error("wake watermark {0} does not belong to this root session")]
    InvalidWakeWatermark(String),
    #[error("mutation path is not covered by the active write claim: {0}")]
    MutationOutsideClaim(String),
    #[error("mutation evidence for {path} has not been started under attempt {attempt_id}")]
    MutationNotStarted { attempt_id: AttemptId, path: String },
    #[error("private mutation snapshot cannot be garbage-collected before the task and gates are sealed")]
    SnapshotRetentionRequired,
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
