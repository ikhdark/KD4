//! Durable client submission identities and recovery of accepted, unrecorded input.
use super::*;
use codex_state::InputReceipt;
use serde::Deserialize;
use serde::Serialize;

#[derive(Serialize, Deserialize)]
struct ReceiptInput {
    input: Vec<UserInput>,
    additional_context: IndexMap<String, AdditionalContextEntry>,
    item_id_suffix: String,
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

    pub(super) async fn cancel_start_input_receipts(&self, thread: ThreadId, turn_id: &str) -> anyhow::Result<()> {
        match self {
            Self::Durable(db) => db.cancel_start_input_receipts(thread, turn_id).await,
            Self::Volatile(receipts) => {
                for (receipt, consumed) in receipts.lock().await.iter_mut().filter(|(receipt, _)| receipt.kind == "start" && receipt.turn_id == turn_id) { *consumed = true; receipt.payload = None; }
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
    pub(crate) async fn cancel_start_receipts(&self, turn_id: &str) -> CodexResult<()> {
        // Optional-state fallback remains usable for ordinary submissions. An
        // explicit durable identity could not have been admitted in this mode.
        if self.state_db().is_none() && self.live_thread().is_some() { return Ok(()); }
        self.receipt_db()?.cancel_start_input_receipts(self.thread_id, turn_id).await.map_err(std::io::Error::other)?;
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

    pub(crate) async fn reserve_start_receipt(&self, client_id: &str, fingerprint: &str, turn_id: &str) -> CodexResult<String> {
        let _gate = self.input_receipt_gate.lock().await;
        if let Some(original) = self.find_start_receipt(client_id, fingerprint).await? { return Ok(original); }
        let receipt = InputReceipt { kind: "start".into(), scope: String::new(), client_id: client_id.into(),
            fingerprint: fingerprint.into(), turn_id: turn_id.into(), payload: None };
        if self.receipt_db()?.reserve_input_receipt(self.thread_id, &receipt).await.map_err(std::io::Error::other)? {
            Ok(turn_id.into())
        } else {
            self.find_start_receipt(client_id, fingerprint).await?.ok_or_else(|| CodexErr::Fatal("input receipt disappeared".into()))
        }
    }

    pub(super) async fn accept_start_receipt(&self, turn_id: &str, client_id: Option<&str>, input: &[UserInput], additional_context: &IndexMap<String, AdditionalContextEntry>) -> CodexResult<()> {
        let Some(client_id) = client_id else { return Ok(()); };
        let db = self.receipt_db()?;
        let Some(receipt) = db.input_receipt(self.thread_id, "start", "", client_id).await.map_err(std::io::Error::other)? else { return Ok(()); };
        if receipt.turn_id != turn_id {
            return Err(CodexErr::InvalidRequest("client submission already belongs to another turn".into()));
        }
        let payload = self.receipt_payload(&receipt, input, additional_context)?;
        db.accept_input_receipt(self.thread_id, "start", "", client_id, turn_id, &payload).await.map_err(std::io::Error::other)?;
        self.input_queue.remember_input_receipt(&serde_json::from_str::<ReceiptInput>(&payload).map_err(std::io::Error::other)?.item_id_suffix);
        Ok(())
    }

    pub(super) fn receipt_payload(&self, receipt: &InputReceipt, input: &[UserInput], additional_context: &IndexMap<String, AdditionalContextEntry>) -> std::io::Result<String> {
        serde_json::to_string(&ReceiptInput { input: input.to_vec(), additional_context: additional_context.clone(),
            item_id_suffix: fingerprint(&(self.thread_id.to_string(), &receipt.kind, &receipt.scope, &receipt.client_id))?,
        }).map_err(std::io::Error::other)
    }

    pub(super) async fn pending_receipt_for_input(&self, input: &[UserInput], client_id: Option<&str>) -> std::io::Result<Option<(InputReceipt, String)>> {
        let Some(client_id) = client_id else { return Ok(None); };
        let db = self.receipt_db()?;
        for receipt in db.pending_input_receipts(self.thread_id).await.map_err(std::io::Error::other)? {
            if receipt.client_id != client_id { continue; }
            let Some(payload) = receipt.payload.as_deref() else { continue; };
            let payload: ReceiptInput = serde_json::from_str(payload).map_err(std::io::Error::other)?;
            if payload.input == input { return Ok(Some((receipt, payload.item_id_suffix))); }
        }
        Ok(None)
    }

    pub(super) async fn complete_input_receipt(&self, receipt: &InputReceipt, suffix: &str) -> std::io::Result<()> {
        // Never mark consumed before the corresponding model-visible history is
        // on stable storage; recovery also recognizes its deterministic item ID.
        if let Some(live_thread) = self.live_thread() {
            live_thread.flush_durable().await.map_err(std::io::Error::other)?;
        }
        self.receipt_db()?.consume_input_receipt(self.thread_id, receipt).await.map_err(std::io::Error::other)?;
        self.input_queue.forget_input_receipt(suffix);
        Ok(())
    }

    pub(super) async fn reconcile_input_receipts(&self, history: &[RolloutItem]) -> std::io::Result<()> {
        if self.state_db().is_none() && self.live_thread().is_some() { return Ok(()); }
        let db = self.receipt_db()?;
        let recorded = history.iter().filter_map(|item| match item {
            RolloutItem::ResponseItem(item) => item.id().map(ToString::to_string), _ => None,
        }).collect::<HashSet<_>>();
        let aborted = history.iter().filter_map(|item| match item {
            RolloutItem::EventMsg(EventMsg::TurnAborted(event)) => event.turn_id.clone(), _ => None,
        }).collect::<HashSet<_>>();
        let mut recovered_input = Vec::new();
        for receipt in db.pending_input_receipts(self.thread_id).await.map_err(std::io::Error::other)? {
            if receipt.kind == "start" {
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
                            turn_id: Some(receipt.turn_id.clone()), reason: TurnAbortReason::Interrupted,
                            completed_at: None, duration_ms: None, timing: None,
                        }))]).await?;
                    }
                }
                self.complete_input_receipt(&receipt, "").await?;
                continue;
            }
            let payload: ReceiptInput = serde_json::from_str(receipt.payload.as_deref().unwrap_or_default()).map_err(std::io::Error::other)?;
            let item_id = ResponseItemId::with_suffix(crate::event_mapping::USER_INPUT_ITEM_ID_BASE, &payload.item_id_suffix).to_string();
            if recorded.contains(&item_id) {
                self.complete_input_receipt(&receipt, &payload.item_id_suffix).await?;
            } else {
                recovered_input.push((payload.item_id_suffix, vec![
                    TurnInput::AdditionalContext { values: payload.additional_context, start_fence: None },
                    TurnInput::UserInput { content: payload.input, client_id: Some(receipt.client_id), start_fence: None },
                ]));
            }
        }
        // The queue's restoration API prepends one older batch. Preserve the
        // database admission order within that batch rather than reversing it.
        self.input_queue.restore_receipted_startup_input(recovered_input).await;
        Ok(())
    }
}
