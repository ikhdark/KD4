use std::fmt;
use std::sync::Arc;
use std::sync::Mutex;

use crate::ProcessId;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcessExecutionKnowledge {
    DefinitelyNotSubmitted,
    PossiblyExecuted,
    ObservedExit(i32),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcessCleanupStatus {
    NotRequired,
    Pending,
    Confirmed,
    Unconfirmed(String),
}

#[derive(Clone, Debug)]
pub struct ProcessCleanupRecord {
    pub process_id: ProcessId,
    pub session_id: Option<String>,
    pub execution: ProcessExecutionKnowledge,
    pub cleanup: ProcessCleanupStatus,
}

/// Bounded-client-history entry, also retained by a start error independently
/// of the client's lifetime. This is evidence, not an active retry task.
#[derive(Clone, Debug)]
pub struct ProcessCleanupTicket(pub(crate) Arc<Mutex<ProcessCleanupRecord>>);

impl ProcessCleanupTicket {
    pub(crate) fn new(process_id: ProcessId, session_id: Option<String>) -> Self {
        Self(Arc::new(Mutex::new(ProcessCleanupRecord {
            process_id,
            session_id,
            execution: ProcessExecutionKnowledge::DefinitelyNotSubmitted,
            cleanup: ProcessCleanupStatus::NotRequired,
        })))
    }

    pub fn snapshot(&self) -> ProcessCleanupRecord {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn update(
        &self,
        execution: ProcessExecutionKnowledge,
        cleanup: ProcessCleanupStatus,
    ) {
        let mut record = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Later transport failure never erases an authoritative observed exit.
        if !matches!(record.execution, ProcessExecutionKnowledge::ObservedExit(_)) {
            record.execution = execution;
        }
        // A timed-out observer can publish Pending after the detached owner
        // has already settled. Never turn a terminal receipt back into work.
        if !matches!(&cleanup, ProcessCleanupStatus::Pending)
            || matches!(
                &record.cleanup,
                ProcessCleanupStatus::NotRequired | ProcessCleanupStatus::Pending
            )
        {
            record.cleanup = cleanup;
        }
    }
}

impl fmt::Display for ProcessCleanupTicket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let record = self.snapshot();
        if matches!(
            record.execution,
            ProcessExecutionKnowledge::DefinitelyNotSubmitted
        ) {
            return write!(
                f,
                "process {}, session {:?}: this start was not submitted or was refused before launch; cleanup {:?}",
                record.process_id, record.session_id, record.cleanup
            );
        }
        write!(
            f,
            "process {}, session {:?}: {:?}; cleanup {:?}. Execution may have had side effects; do not blindly retry. Cleanup evidence is retained in this ticket and the client's bounded process_cleanup_records history",
            record.process_id, record.session_id, record.execution, record.cleanup
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn late_observer_cannot_overwrite_settlement_or_known_exit() {
        for settlement in [
            ProcessCleanupStatus::Confirmed,
            ProcessCleanupStatus::Unconfirmed("unreachable".into()),
        ] {
            let ticket = ProcessCleanupTicket::new("process".into(), Some("session".into()));
            ticket.update(
                ProcessExecutionKnowledge::ObservedExit(7),
                settlement.clone(),
            );
            ticket.update(
                ProcessExecutionKnowledge::PossiblyExecuted,
                ProcessCleanupStatus::Pending,
            );
            assert_eq!(ticket.snapshot().cleanup, settlement);
            assert_eq!(
                ticket.snapshot().execution,
                ProcessExecutionKnowledge::ObservedExit(7)
            );
        }
    }
}
