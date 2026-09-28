use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;

use codex_code_mode_protocol::CellId;
use codex_code_mode_protocol::CodeModeSessionDelegate;
use codex_code_mode_protocol::FunctionCallOutputContentItem;
use codex_code_mode_protocol::RuntimeResponse;
use codex_code_mode_protocol::WaitOutcome;
use codex_code_mode_protocol::host::SessionId;
use codex_code_mode_protocol::host::WireCellId;

use super::cell_ids::public_cell_id;
use super::cleanup::SessionCleanup;
use super::types::RemoteSession;
use crate::delivery::Delivery;

enum ReceiptState {
    InFlight,
    Ready(WaitOutcome),
    Consumed,
}

struct Receipt {
    cell_id: CellId,
    bytes: usize,
    state: Mutex<ReceiptState>,
}

#[derive(Default)]
struct ResponseReceipts {
    receipts: VecDeque<Arc<Receipt>>,
}

impl ResponseReceipts {
    fn prune(&mut self) {
        self.receipts.retain(|receipt| {
            !matches!(
                *receipt
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
                ReceiptState::Consumed
            )
        });
    }

    fn has_capacity(&mut self) -> bool {
        self.prune();
        self.receipts.len() < 256
            && self
                .receipts
                .iter()
                .map(|receipt| receipt.bytes)
                .sum::<usize>()
                < 8 * 1024 * 1024
    }

    fn reserve(&mut self, response: &RuntimeResponse) -> Arc<Receipt> {
        self.prune();
        let (cell_id, content_items, error_text) = match response {
            RuntimeResponse::Yielded {
                cell_id,
                content_items,
            }
            | RuntimeResponse::ExplicitYield {
                cell_id,
                content_items,
            }
            | RuntimeResponse::Terminated {
                cell_id,
                content_items,
            } => (cell_id, content_items, None),
            RuntimeResponse::Result {
                cell_id,
                content_items,
                error_text,
                ..
            } => (cell_id, content_items, error_text.as_deref()),
        };
        let bytes = content_items
            .iter()
            .map(|item| match item {
                FunctionCallOutputContentItem::InputText { text } => text.len(),
                FunctionCallOutputContentItem::InputImage { image_url, .. } => image_url.len(),
            })
            .fold(error_text.map_or(0, str::len), usize::saturating_add);
        let receipt = Arc::new(Receipt {
            cell_id: cell_id.clone(),
            bytes,
            state: Mutex::new(ReceiptState::InFlight),
        });
        self.receipts.push_back(Arc::clone(&receipt));
        receipt
    }

    fn deliver<T: Send + 'static>(
        receipt: Arc<Receipt>,
        value: T,
        response: fn(T) -> WaitOutcome,
    ) -> Delivery<T> {
        let claimed = Arc::clone(&receipt);
        Delivery::new(value, move |value| {
            *receipt
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                ReceiptState::Ready(response(value));
        })
        .on_claim(move || {
            *claimed
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = ReceiptState::Consumed;
        })
    }

    fn take(&mut self, cell_id: &CellId) -> Result<Option<Delivery<WaitOutcome>>, String> {
        self.prune();
        let Some(receipt) = self
            .receipts
            .iter()
            .find(|receipt| &receipt.cell_id == cell_id)
        else {
            return Ok(None);
        };
        let mut state = receipt
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let response = match std::mem::replace(&mut *state, ReceiptState::InFlight) {
            ReceiptState::Ready(response) => response,
            previous => {
                *state = previous;
                return Err("code-mode cell already has an unconsumed observation".to_string());
            }
        };
        drop(state);
        Ok(Some(Self::deliver(
            Arc::clone(receipt),
            response,
            std::convert::identity,
        )))
    }
}

pub(super) struct CellOwner {
    pub(super) session_id: SessionId,
    pub(super) cell_id: CellId,
    pub(super) delegate: Arc<dyn CodeModeSessionDelegate>,
}

pub(super) struct DelegateTarget {
    pub(super) session_id: SessionId,
    pub(super) cell_id: CellId,
    pub(super) delegate: Arc<dyn CodeModeSessionDelegate>,
}

pub(super) struct FailedSession {
    pub(super) cleanup: SessionCleanup,
    pub(super) cells: Vec<CellOwner>,
}

pub(super) enum CellAdmissionError {
    MissingSession,
    DuplicateCell,
}

struct SessionRecord {
    remote: RemoteSession,
    delegate: Arc<dyn CodeModeSessionDelegate>,
    cleanup: SessionCleanup,
    phase: SessionPhase,
    cells: HashMap<WireCellId, CellId>,
    receipts: ResponseReceipts,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum SessionPhase {
    Ready,
    Closing,
}

pub(super) struct SessionRegistry {
    records: HashMap<SessionId, SessionRecord>,
}

impl SessionRegistry {
    pub(super) fn new() -> Self {
        Self {
            records: HashMap::new(),
        }
    }

    pub(super) fn contains(&self, session_id: &SessionId) -> bool {
        self.records.contains_key(session_id)
    }

    pub(super) fn insert_ready(
        &mut self,
        session: RemoteSession,
        delegate: Arc<dyn CodeModeSessionDelegate>,
        cleanup: SessionCleanup,
    ) {
        self.records.insert(
            session.id.clone(),
            SessionRecord {
                remote: session,
                delegate,
                cleanup,
                phase: SessionPhase::Ready,
                cells: HashMap::new(),
                receipts: ResponseReceipts::default(),
            },
        );
    }

    pub(super) fn require_ready(&self, session: &RemoteSession) -> Result<(), String> {
        let record = self
            .records
            .get(&session.id)
            .ok_or_else(|| format!("unknown code-mode session {}", session.id))?;
        if record.remote != *session {
            return Err("stale code-mode session generation".to_string());
        }
        if record.phase != SessionPhase::Ready {
            return Err("code-mode session is shutting down".to_string());
        }
        Ok(())
    }

    pub(super) fn require_receipt_capacity(
        &mut self,
        session: &RemoteSession,
    ) -> Result<(), String> {
        self.require_ready(session)?;
        let record = self
            .records
            .get_mut(&session.id)
            .ok_or_else(|| format!("unknown code-mode session {}", session.id))?;
        if !record.receipts.has_capacity() {
            let ids = record
                .receipts
                .receipts
                .iter()
                .take(8)
                .map(|receipt| receipt.cell_id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(format!(
                "code mode has reached its unobserved response limit; wait for cells {ids} before starting another exec; their output is still retained"
            ));
        }
        Ok(())
    }

    pub(super) fn recover_response(
        &mut self,
        session: &RemoteSession,
        cell_id: &WireCellId,
    ) -> Result<Option<Delivery<WaitOutcome>>, String> {
        self.require_ready(session)?;
        self.records
            .get_mut(&session.id)
            .ok_or_else(|| format!("unknown code-mode session {}", session.id))?
            .receipts
            .take(&public_cell_id(session.generation, cell_id))
    }

    pub(super) fn guard_response(
        &mut self,
        session: &RemoteSession,
        response: RuntimeResponse,
    ) -> Delivery<RuntimeResponse> {
        let Some(record) = self.records.get_mut(&session.id) else {
            return Delivery::new(response, |_| {});
        };
        let receipt = record.receipts.reserve(&response);
        ResponseReceipts::deliver(receipt, response, WaitOutcome::LiveCell)
    }

    pub(super) fn guard_outcome(
        &mut self,
        session: &RemoteSession,
        outcome: WaitOutcome,
    ) -> Delivery<WaitOutcome> {
        let response = match &outcome {
            WaitOutcome::LiveCell(response) | WaitOutcome::MissingCell(response) => response,
        };
        let Some(record) = self.records.get_mut(&session.id) else {
            return Delivery::new(outcome, |_| {});
        };
        let receipt = record.receipts.reserve(response);
        ResponseReceipts::deliver(receipt, outcome, std::convert::identity)
    }

    pub(super) fn restore_before_termination(
        &mut self,
        session: &RemoteSession,
        mut outcome: WaitOutcome,
    ) -> WaitOutcome {
        let response = match &mut outcome {
            WaitOutcome::LiveCell(response) | WaitOutcome::MissingCell(response) => response,
        };
        let (cell_id, content_items) = match response {
            RuntimeResponse::Result {
                cell_id,
                content_items,
                ..
            }
            | RuntimeResponse::Terminated {
                cell_id,
                content_items,
            } => (cell_id, content_items),
            _ => return outcome,
        };
        let Some(record) = self.records.get_mut(&session.id) else {
            return outcome;
        };
        let mut restored = Vec::new();
        for receipt in &record.receipts.receipts {
            if receipt.cell_id != *cell_id {
                continue;
            }
            let mut state = receipt
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if matches!(
                &*state,
                ReceiptState::Ready(WaitOutcome::LiveCell(
                    RuntimeResponse::Yielded { .. } | RuntimeResponse::ExplicitYield { .. }
                ))
            ) && let ReceiptState::Ready(WaitOutcome::LiveCell(
                RuntimeResponse::Yielded { content_items, .. }
                | RuntimeResponse::ExplicitYield { content_items, .. },
            )) = std::mem::replace(&mut *state, ReceiptState::Consumed)
            {
                restored.extend(content_items);
            }
        }
        restored.append(content_items);
        *content_items = restored;
        outcome
    }

    pub(super) fn begin_shutdown(&mut self, session: &RemoteSession) -> Result<(), String> {
        let record = self
            .records
            .get_mut(&session.id)
            .ok_or_else(|| format!("unknown code-mode session {}", session.id))?;
        if record.remote != *session {
            return Err("stale code-mode session generation".to_string());
        }
        if record.phase == SessionPhase::Closing {
            return Err("code-mode session is already closing".to_string());
        }
        record.phase = SessionPhase::Closing;
        Ok(())
    }

    pub(super) fn begin_abandoned_shutdown(&mut self, session_id: &SessionId) -> Option<bool> {
        let record = self.records.get_mut(session_id)?;
        if record.phase == SessionPhase::Closing {
            return Some(false);
        }
        record.phase = SessionPhase::Closing;
        Some(true)
    }

    pub(super) fn is_closing(&self, session_id: &SessionId) -> Option<bool> {
        self.records
            .get(session_id)
            .map(|record| record.phase == SessionPhase::Closing)
    }

    pub(super) fn admit_cell(
        &mut self,
        session: &RemoteSession,
        cell_id: WireCellId,
    ) -> Result<CellId, CellAdmissionError> {
        let Some(record) = self.records.get_mut(&session.id) else {
            return Err(CellAdmissionError::MissingSession);
        };
        if record.cells.contains_key(&cell_id) {
            return Err(CellAdmissionError::DuplicateCell);
        }
        let public_id = public_cell_id(session.generation, &cell_id);
        record.cells.insert(cell_id, public_id.clone());
        Ok(public_id)
    }

    pub(super) fn delegate_target(
        &self,
        session_id: &SessionId,
        cell_id: &WireCellId,
    ) -> Result<DelegateTarget, String> {
        let session = self
            .records
            .get(session_id)
            .ok_or_else(|| format!("code-mode host delegated for unknown session {session_id}"))?;
        let public_id = session.cells.get(cell_id).cloned().ok_or_else(|| {
            format!(
                "code-mode host delegated for unknown cell {} in session {session_id}",
                cell_id.as_str()
            )
        })?;
        Ok(DelegateTarget {
            session_id: session_id.clone(),
            cell_id: public_id,
            delegate: Arc::clone(&session.delegate),
        })
    }

    pub(super) fn remove_cell(
        &mut self,
        session_id: &SessionId,
        cell_id: &WireCellId,
    ) -> Result<CellOwner, String> {
        let session = self.records.get_mut(session_id).ok_or_else(|| {
            format!(
                "code-mode host closed cell {} in unknown session {session_id}",
                cell_id.as_str()
            )
        })?;
        let public_id = session
            .cells
            .remove(cell_id)
            .ok_or_else(|| format!("code-mode host closed unknown cell in session {session_id}"))?;
        Ok(CellOwner {
            session_id: session_id.clone(),
            cell_id: public_id,
            delegate: Arc::clone(&session.delegate),
        })
    }

    pub(super) fn remove_session(&mut self, session_id: &SessionId) -> Vec<CellOwner> {
        let Some(session) = self.records.remove(session_id) else {
            return Vec::new();
        };
        session
            .cells
            .into_values()
            .map(|cell_id| CellOwner {
                session_id: session_id.clone(),
                cell_id,
                delegate: Arc::clone(&session.delegate),
            })
            .collect()
    }

    pub(super) fn drain(&mut self) -> Vec<FailedSession> {
        let sessions = std::mem::take(&mut self.records);
        sessions
            .into_iter()
            .map(|(session_id, session)| {
                let cells = session
                    .cells
                    .into_values()
                    .map(|cell_id| CellOwner {
                        session_id: session_id.clone(),
                        cell_id,
                        delegate: Arc::clone(&session.delegate),
                    })
                    .collect();
                FailedSession {
                    cleanup: session.cleanup,
                    cells,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod receipt_tests {
    use super::*;

    #[test]
    fn abandoned_responses_preserve_missing_outcomes_and_apply_backpressure_until_claimed() {
        let mut receipts = ResponseReceipts::default();
        let response = RuntimeResponse::Result {
            cell_id: CellId::new("retained".to_string()),
            content_items: vec![FunctionCallOutputContentItem::InputText {
                text: "x".repeat(8 * 1024 * 1024),
            }],
            error_text: None,
            output_loss: None,
        };
        let receipt = receipts.reserve(&response);
        let delivery = ResponseReceipts::deliver(
            receipt,
            WaitOutcome::MissingCell(response.clone()),
            std::convert::identity,
        );
        assert!(!receipts.has_capacity());
        assert!(receipts.take(&CellId::new("retained".to_string())).is_err());
        drop(delivery);
        let recovered = receipts
            .take(&CellId::new("retained".to_string()))
            .unwrap()
            .unwrap();
        drop(recovered);
        assert!(!receipts.has_capacity());
        assert_eq!(
            receipts
                .take(&CellId::new("retained".to_string()))
                .unwrap()
                .unwrap()
                .claim(),
            WaitOutcome::MissingCell(response)
        );
        assert!(receipts.has_capacity());
        assert!(
            receipts
                .take(&CellId::new("retained".to_string()))
                .unwrap()
                .is_none()
        );
    }
}
