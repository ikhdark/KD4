//! Durable client submission identities and recovery of accepted, unrecorded input.
use super::*;
use codex_protocol::protocol::TurnStartedEvent;
use codex_state::InputReceipt;
use serde::Deserialize;
use serde::Serialize;

#[derive(Serialize, Deserialize)]
struct ReceiptInput {
    input: Vec<UserInput>,
    additional_context: IndexMap<String, AdditionalContextEntry>,
    item_id_suffix: String,
}

/// The storage key and deterministic history ID of an accepted submission.
#[derive(Debug)]
pub(crate) struct AcceptedInputReceipt {
    key: InputReceipt,
    pub(super) item_id_suffix: String,
}

impl AcceptedInputReceipt {
    pub(super) fn new(mut key: InputReceipt, item_id_suffix: String) -> Arc<Self> {
        // Do not keep a second serialized copy of the queued prompt or context.
        key.payload = None;
        Arc::new(Self { key, item_id_suffix })
    }
}

impl PartialEq for AcceptedInputReceipt {
    fn eq(&self, other: &Self) -> bool {
        self.key.kind == other.key.kind
            && self.key.scope == other.key.scope
            && self.key.client_id == other.key.client_id
            && self.key.turn_id == other.key.turn_id
            && self.item_id_suffix == other.item_id_suffix
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum UserInputIdentity {
    // An internal or legacy submission can carry a UI client ID without having
    // passed durable receipt admission.
    Unreceipted(Option<String>),
    Receipted(Arc<AcceptedInputReceipt>),
}

impl UserInputIdentity {
    pub(super) fn client_id(&self) -> Option<&str> {
        match self {
            Self::Unreceipted(client_id) => client_id.as_deref(),
            Self::Receipted(receipt) => Some(receipt.key.client_id.as_str()),
        }
    }

    pub(super) fn accepted_receipt(&self) -> Option<&AcceptedInputReceipt> {
        match self {
            Self::Unreceipted(_) => None,
            Self::Receipted(receipt) => Some(receipt.as_ref()),
        }
    }
}

// Ephemeral threads deliberately have neither a rollout nor a state DB and
// cannot be resumed. Keep their existing privacy contract while retaining the
// same at-most-once semantics for the lifetime of the live thread.
pub(super) type VolatileInputReceipts = Arc<Mutex<Vec<(InputReceipt, bool)>>>;

pub(super) enum InputReceiptStore {
    Durable(state_integration::StateDbHandle),
    Volatile(VolatileInputReceipts),
}

impl InputReceiptStore {
    pub(super) async fn input_receipt(&self, thread: ThreadId, kind: &str, scope: &str, client: &str) -> anyhow::Result<Option<InputReceipt>> {
        match self {
            Self::Durable(db) => db.input_receipt(thread, kind, scope, client).await,
            Self::Volatile(receipts) => Ok(receipts.lock().await.iter().find(|(receipt, _)|
                receipt.kind == kind && receipt.scope == scope && receipt.client_id == client).map(|(receipt, _)| receipt.clone())),
        }
    }

    pub(super) async fn reserve_input_receipt(&self, thread: ThreadId, receipt: &InputReceipt) -> anyhow::Result<bool> {
        match self {
            Self::Durable(db) => db.reserve_input_receipt(thread, receipt).await,
            Self::Volatile(receipts) => {
                let mut receipts = receipts.lock().await;
                if receipts.iter().any(|(existing, _)| existing.kind == receipt.kind && existing.scope == receipt.scope && existing.client_id == receipt.client_id) { return Ok(false); }
                receipts.push((receipt.clone(), false));
                Ok(true)
            }
        }
    }

    pub(super) async fn accept_input_receipt(&self, thread: ThreadId, kind: &str, scope: &str, client: &str, turn_id: &str, payload: &str) -> anyhow::Result<()> {
        match self {
            Self::Durable(db) => db.accept_input_receipt(thread, kind, scope, client, turn_id, payload).await,
            Self::Volatile(receipts) => {
                let mut receipts = receipts.lock().await;
                let Some((receipt, _)) = receipts.iter_mut().find(|(receipt, consumed)| !*consumed && receipt.kind == kind && receipt.scope == scope && receipt.client_id == client && receipt.turn_id == turn_id && receipt.payload.is_none()) else { anyhow::bail!("input receipt is not a fresh admission"); };
                receipt.payload = Some(payload.to_owned());
                Ok(())
            }
        }
    }

    pub(super) async fn pending_input_receipts(&self, thread: ThreadId) -> anyhow::Result<Vec<InputReceipt>> {
        match self {
            Self::Durable(db) => db.pending_input_receipts(thread).await,
            Self::Volatile(receipts) => Ok(receipts.lock().await.iter().filter(|(receipt, consumed)| !*consumed && (receipt.payload.is_some() || receipt.kind == "start")).map(|(receipt, _)| receipt.clone()).collect()),
        }
    }

    pub(super) async fn consume_input_receipt(&self, thread: ThreadId, receipt: &InputReceipt) -> anyhow::Result<()> {
        match self {
            Self::Durable(db) => db.consume_input_receipt(thread, receipt).await,
            Self::Volatile(receipts) => {
                if let Some((stored, consumed)) = receipts.lock().await.iter_mut().find(|(stored, _)| stored.kind == receipt.kind && stored.scope == receipt.scope && stored.client_id == receipt.client_id) { *consumed = true; stored.payload = None; }
                Ok(())
            }
        }
    }

    pub(super) async fn cancel_turn_input_receipts(&self, thread: ThreadId, turn_id: &str) -> anyhow::Result<()> {
        match self {
            Self::Durable(db) => db.cancel_turn_input_receipts(thread, turn_id).await,
            Self::Volatile(receipts) => {
                // Mirror durable revocation: accepted starts retain their
                // source until the existing recovery/recording barrier.
                for (receipt, consumed) in receipts.lock().await.iter_mut().filter(|(receipt, _)| receipt.turn_id == turn_id && (receipt.kind != "start" || receipt.payload.is_none())) { *consumed = true; receipt.payload = None; }
                Ok(())
            }
        }
    }
}

pub(super) fn fingerprint(value: &impl Serialize) -> std::io::Result<String> {
    fn canonicalize(value: &mut Value) {
        match value {
            Value::Object(map) => {
                for value in map.values_mut() { canonicalize(value); }
                let mut values = std::mem::take(map).into_iter().collect::<Vec<_>>();
                values.sort_by(|a, b| a.0.cmp(&b.0));
                map.extend(values);
            }
            Value::Array(values) => values.iter_mut().for_each(canonicalize),
            _ => {}
        }
    }
    let mut value = serde_json::to_value(value).map_err(std::io::Error::other)?;
    canonicalize(&mut value);
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(&value).map_err(std::io::Error::other)?)))
}

impl Session {
    pub(crate) async fn reject_unaccepted_start_receipt(&self, turn_id: &str) -> CodexResult<()> {
        match self.receipt_db()? {
            InputReceiptStore::Durable(db) => db.reject_unaccepted_start_receipt(self.thread_id, turn_id)
                .await.map_err(std::io::Error::other)?,
            InputReceiptStore::Volatile(receipts) => {
                for (receipt, consumed) in receipts.lock().await.iter_mut().filter(|(receipt, _)|
                    receipt.kind == "start" && receipt.turn_id == turn_id && receipt.payload.is_none()) {
                    receipt.kind = "rejected_start".into();
                    receipt.scope = receipt.turn_id.clone();
                    *consumed = true;
                }
            }
        }
        Ok(())
    }
    pub(crate) async fn cancel_turn_receipts(&self, turn_id: &str) -> CodexResult<()> {
        // Optional-state fallback remains usable for ordinary submissions. An
        // explicit durable identity could not have been admitted in this mode.
        if self.state_db().is_none() && self.live_thread().is_some() { return Ok(()); }
        self.receipt_db()?.cancel_turn_input_receipts(self.thread_id, turn_id).await.map_err(std::io::Error::other)?;
        Ok(())
    }
    pub(super) fn receipt_db(&self) -> std::io::Result<InputReceiptStore> {
        match self.state_db() {
            Some(db) => Ok(InputReceiptStore::Durable(db)),
            None if self.live_thread().is_none() => Ok(InputReceiptStore::Volatile(Arc::clone(&self.volatile_input_receipts))),
            None => Err(std::io::Error::other("durable client submission identities require the state database")),
        }
    }

    pub(crate) async fn find_start_receipt(&self, client_id: &str, fingerprint: &str) -> CodexResult<Option<String>> {
        let receipt = self.receipt_db()?.input_receipt(self.thread_id, "start", "", client_id)
            .await.map_err(std::io::Error::other)?;
        if let Some(receipt) = receipt {
            if receipt.fingerprint != fingerprint {
                return Err(CodexErr::InvalidRequest("clientUserMessageId was already used with different turn/start contents".into()));
            }
            return Ok(Some(receipt.turn_id));
        }
        Ok(None)
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "Serialize receipt lookup and reservation for one client submission identity"
    )]
    pub(crate) async fn reserve_start_receipt(&self, client_id: &str, fingerprint: &str, turn_id: &str) -> CodexResult<String> {
        let _gate = self.input_receipt_gate.lock().await;
        if let Some(original) = self.find_start_receipt(client_id, fingerprint).await? { return Ok(original); }
        let receipt = InputReceipt { kind: "start".into(), scope: String::new(), client_id: client_id.into(),
            fingerprint: fingerprint.into(), turn_id: turn_id.into(), payload: None };
        if self.receipt_db()?.reserve_input_receipt(self.thread_id, &receipt).await.map_err(std::io::Error::other)? {
            tracing::debug!(target: "codex_input_custody", phase = "reserved", thread_id = %self.thread_id, turn_id, receipt_kind = "start", "input custody boundary");
            Ok(turn_id.into())
        } else {
            self.find_start_receipt(client_id, fingerprint).await?.ok_or_else(|| CodexErr::Fatal("input receipt disappeared".into()))
        }
    }

    pub(super) async fn accept_start_receipt(
        &self,
        turn_id: &str,
        client_id: Option<&str>,
        input: &[UserInput],
        additional_context: &IndexMap<String, AdditionalContextEntry>,
    ) -> CodexResult<Option<Arc<AcceptedInputReceipt>>> {
        let Some(client_id) = client_id else { return Ok(None); };
        let db = self.receipt_db()?;
        let Some(receipt) = db.input_receipt(self.thread_id, "start", "", client_id).await.map_err(std::io::Error::other)? else { return Ok(None); };
        if receipt.turn_id != turn_id {
            return Err(CodexErr::InvalidRequest("client submission already belongs to another turn".into()));
        }
        let (payload, item_id_suffix) = self.receipt_payload(&receipt, input, additional_context)?;
        db.accept_input_receipt(self.thread_id, "start", "", client_id, turn_id, &payload).await.map_err(std::io::Error::other)?;
        tracing::debug!(target: "codex_input_custody", phase = "accepted", thread_id = %self.thread_id, turn_id, payload_bytes = payload.len(), "input custody boundary");
        self.input_queue.remember_input_receipt(&item_id_suffix);
        Ok(Some(AcceptedInputReceipt::new(receipt, item_id_suffix)))
    }

    pub(super) fn receipt_payload(
        &self,
        receipt: &InputReceipt,
        input: &[UserInput],
        additional_context: &IndexMap<String, AdditionalContextEntry>,
    ) -> std::io::Result<(String, String)> {
        let item_id_suffix = fingerprint(&(
            self.thread_id.to_string(), &receipt.kind, &receipt.scope, &receipt.client_id,
        ))?;
        let payload = serde_json::to_string(&ReceiptInput {
            input: input.to_vec(),
            additional_context: additional_context.clone(),
            item_id_suffix: item_id_suffix.clone(),
        }).map_err(std::io::Error::other)?;
        Ok((payload, item_id_suffix))
    }

    pub(super) async fn complete_accepted_input_receipt(&self, receipt: &AcceptedInputReceipt) -> std::io::Result<()> {
        self.complete_input_receipt(&receipt.key, &receipt.item_id_suffix).await
    }

    pub(super) async fn complete_input_receipt(&self, receipt: &InputReceipt, suffix: &str) -> std::io::Result<()> {
        // Never mark consumed before the corresponding model-visible history is
        // on stable storage; recovery also recognizes its deterministic item ID.
        if let Some(live_thread) = self.live_thread() {
            live_thread.flush_durable().await.map_err(std::io::Error::other)?;
        }
        tracing::debug!(target: "codex_input_custody", phase = "rollout_barrier", thread_id = %self.thread_id, turn_id = %receipt.turn_id, receipt_kind = %receipt.kind, "input custody boundary");
        self.receipt_db()?.consume_input_receipt(self.thread_id, receipt).await.map_err(std::io::Error::other)?;
        tracing::debug!(target: "codex_input_custody", phase = "consumed", thread_id = %self.thread_id, turn_id = %receipt.turn_id, receipt_kind = %receipt.kind, "input custody boundary");
        self.input_queue.forget_input_receipt(suffix);
        Ok(())
    }

    pub(super) async fn reconcile_input_receipts(&self, history: &[RolloutItem], turn_context: &TurnContext) -> std::io::Result<()> {
        if self.state_db().is_none() && self.live_thread().is_some() { return Ok(()); }
        let db = self.receipt_db()?;
        let recorded = history.iter().filter_map(|item| match item {
            RolloutItem::ResponseItem(item) => item.id().map(ToString::to_string), _ => None,
        }).collect::<HashSet<_>>();
        let aborted = history.iter().filter_map(|item| match item {
            RolloutItem::EventMsg(EventMsg::TurnAborted(event)) => event.turn_id.clone(), _ => None,
        }).collect::<HashSet<_>>();
        let cancelled = history.iter().filter_map(|item| match item {
            RolloutItem::EventMsg(EventMsg::TurnAborted(event))
                if event.reason != TurnAbortReason::ProcessLost => event.turn_id.clone(),
            _ => None,
        }).collect::<HashSet<_>>();
        let mut recovered_input = Vec::new();
        for receipt in db.pending_input_receipts(self.thread_id).await.map_err(std::io::Error::other)? {
            let payload = receipt.payload.as_deref().map(serde_json::from_str::<ReceiptInput>)
                .transpose().map_err(std::io::Error::other)?;
            if receipt.kind == "start" {
                if let Some(payload) = payload.as_ref() {
                    let id = ResponseItemId::with_suffix(crate::event_mapping::USER_INPUT_ITEM_ID_BASE, &payload.item_id_suffix);
                    if !recorded.contains(&id.to_string()) {
                        // Preserve the accepted source without resampling it or
                        // re-reading mutable local attachments. The existing
                        // artifact owner retains the exact prompt/context and
                        // the existing item lifecycle exposes its disposition.
                        let recovery = self.retain_context_source("interrupted_input", serde_json::json!({
                            "turn_id": receipt.turn_id, "client_id": receipt.client_id,
                            "input": payload.input, "additional_context": payload.additional_context,
                        })).await.map_err(std::io::Error::other)?;
                        self.flush_tool_history_persistence().await.map_err(std::io::Error::other)?;
                        tracing::debug!(target: "codex_input_custody", phase = "recovery_artifact_checkpoint", thread_id = %self.thread_id, turn_id = %receipt.turn_id, "input custody boundary");
                        let notice = format!("Accepted submission for turn {} was interrupted before recording and has not been re-admitted or executed by recovery. Its exact prompt and context are retained at /items/input and /items/additional_context. Do not resume it without an explicit user request. Preserve each source's original authority. {recovery}", receipt.turn_id);
                        let mut user_item = UserMessageItem::new(&payload.input);
                        user_item.id = id.to_string();
                        user_item.client_id = Some(receipt.client_id.clone());
                        user_item.content.push(UserInput::Text { text: notice.clone(), text_elements: Vec::new() });
                        let response = ResponseItem::Message {
                            id: Some(id), role: "user".to_string(),
                            content: vec![codex_protocol::models::ContentItem::InputText { text: notice }],
                            phase: None, internal_chat_message_metadata_passthrough: None,
                        };
                        let mut records = Vec::new();
                        if !history.iter().any(|item| matches!(item, RolloutItem::EventMsg(EventMsg::TurnStarted(event)) if event.turn_id == receipt.turn_id)) {
                            records.push(RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
                                turn_id: receipt.turn_id.clone(), trace_id: None, started_at: None,
                                model_context_window: turn_context.model_context_window(),
                                collaboration_mode_kind: turn_context.collaboration_mode.mode,
                            })));
                        }
                        // Put the visible item before its deterministic response
                        // marker: even a prefix-only crash cannot consume the
                        // receipt before its visible recovery item is durable.
                        records.push(RolloutItem::EventMsg(EventMsg::ItemCompleted(ItemCompletedEvent {
                            thread_id: self.thread_id, turn_id: receipt.turn_id.clone(),
                            item: TurnItem::UserMessage(user_item), completed_at_ms: now_unix_timestamp_ms(),
                        })));
                        records.push(RolloutItem::ResponseItem(response.clone()));
                        self.persist_rollout_items_ordered(&records).await?;
                        tracing::debug!(target: "codex_input_custody", phase = "recovery_append", thread_id = %self.thread_id, turn_id = %receipt.turn_id, item_count = records.len(), "input custody boundary");
                        self.state.lock().await.record_items(std::slice::from_ref(&response), turn_context.model_info.truncation_policy.into());
                    }
                }
                // Process loss cannot be interpreted as a request to launch the
                // logical submission again. Close an unresolved reserved start
                // under its original ID; callers can intentionally submit new
                // work with a distinct ID after observing this interruption.
                if !aborted.contains(&receipt.turn_id) {
                    let terminal = if let Some(thread) = self.live_thread() {
                        thread.terminal_event(&receipt.turn_id, false).await.map_err(std::io::Error::other)?
                    } else { None };
                    if terminal.is_none() {
                        self.persist_rollout_items_ordered(&[RolloutItem::EventMsg(EventMsg::TurnAborted(TurnAbortedEvent {
                            turn_id: Some(receipt.turn_id.clone()), reason: TurnAbortReason::ProcessLost,
                            completed_at: None, duration_ms: None, timing: None,
                        }))]).await?;
                    }
                }
                self.complete_input_receipt(&receipt, payload.as_ref().map_or("", |payload| payload.item_id_suffix.as_str())).await?;
                continue;
            }
            let Some(payload) = payload else { continue; };
            let item_id = ResponseItemId::with_suffix(crate::event_mapping::USER_INPUT_ITEM_ID_BASE, &payload.item_id_suffix).to_string();
            if recorded.contains(&item_id) || cancelled.contains(&receipt.turn_id) {
                self.complete_input_receipt(&receipt, &payload.item_id_suffix).await?;
            } else {
                let identity = UserInputIdentity::Receipted(AcceptedInputReceipt::new(
                    receipt,
                    payload.item_id_suffix.clone(),
                ));
                recovered_input.push((payload.item_id_suffix, vec![
                    TurnInput::AdditionalContext { values: payload.additional_context, start_fence: None },
                    TurnInput::UserInput { content: payload.input, identity, start_fence: None },
                ]));
            }
        }
        // The queue's restoration API prepends one older batch. Preserve the
        // database admission order within that batch rather than reversing it.
        self.input_queue.restore_receipted_startup_input(recovered_input).await;
        Ok(())
    }
}
