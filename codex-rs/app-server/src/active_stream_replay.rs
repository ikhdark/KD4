//! Bounded live-only prefixes for a newly attaching observer. Completed items
//! remain owned by history; these notifications seed the ordinary live consumer,
//! never its completed-item replay path.
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::item_event_to_server_notification;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::EventMsg;

const MAX_PREFIX_BYTES: usize = 1024 * 1024;
const MAX_PREFIX_EVENTS: usize = 256;

#[derive(Default)]
pub(super) struct ActiveStreamReplay {
    turn_id: Option<String>,
    thread_id: Option<String>,
    entries: Vec<Entry>,
    bytes: usize,
    incomplete: bool,
}

struct Entry {
    item_id: String,
    notification: ServerNotification,
    bytes: usize,
}

impl ActiveStreamReplay {
    pub(super) fn track(&mut self, turn_id: &str, event: &EventMsg) {
        if matches!(event, EventMsg::TurnStarted(_)) {
            *self = Self {
                turn_id: Some(turn_id.to_owned()),
                ..Self::default()
            };
            return;
        }
        if matches!(event, EventMsg::ThreadRolledBack(_))
            || (self.turn_id.as_deref() == Some(turn_id)
                && matches!(event, EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_)))
        {
            *self = Self::default();
            return;
        }
        if self.turn_id.as_deref() != Some(turn_id) {
            return;
        }
        if let EventMsg::ItemCompleted(completed) = event {
            if completed.turn_id != turn_id {
                return;
            }
            let id = completed.item.id();
            self.entries.retain(|entry| entry.item_id != id);
            self.bytes = self.entries.iter().map(|entry| entry.bytes).sum();
            return;
        }
        if self.incomplete {
            return;
        }
        let thread_id = match event {
            EventMsg::ItemStarted(started)
                if matches!(
                    started.item,
                    TurnItem::AgentMessage(_) | TurnItem::Plan(_) | TurnItem::Reasoning(_)
                ) =>
            {
                started.thread_id.to_string()
            }
            EventMsg::AgentMessageContentDelta(delta) => delta.thread_id.clone(),
            EventMsg::PlanDelta(delta) => delta.thread_id.clone(),
            EventMsg::ReasoningContentDelta(delta) => delta.thread_id.clone(),
            EventMsg::ReasoningRawContentDelta(delta) => delta.thread_id.clone(),
            EventMsg::AgentReasoningSectionBreak(_) => match &self.thread_id {
                Some(thread_id) => thread_id.clone(),
                None => {
                    self.incomplete = true;
                    return;
                }
            },
            _ => return,
        };
        self.thread_id = Some(thread_id.clone());
        let notification = item_event_to_server_notification(event.clone(), &thread_id, turn_id);
        // Adjacent deltas append in place: one-byte deltas must not cause a
        // full-prefix clone, serialization or history replay on every event.
        if let Some(last) = self.entries.last_mut()
            && let Some(additional) = merge_size(&last.notification, &notification)
        {
            if additional > MAX_PREFIX_BYTES.saturating_sub(self.bytes) {
                self.incomplete = true;
                return;
            }
            merge_delta(&mut last.notification, &notification);
            last.bytes += additional;
            self.bytes += additional;
            return;
        }
        let bytes = bounded_size(&notification);
        if self.entries.len() >= MAX_PREFIX_EVENTS
            || bytes > MAX_PREFIX_BYTES.saturating_sub(self.bytes)
        {
            self.incomplete = true;
            return;
        }
        let item_id = match &notification {
            ServerNotification::ItemStarted(started) => started.item.id(),
            ServerNotification::AgentMessageDelta(delta) => &delta.item_id,
            ServerNotification::PlanDelta(delta) => &delta.item_id,
            ServerNotification::ReasoningSummaryTextDelta(delta) => &delta.item_id,
            ServerNotification::ReasoningTextDelta(delta) => &delta.item_id,
            ServerNotification::ReasoningSummaryPartAdded(part) => &part.item_id,
            _ => unreachable!("only active textual items are retained"),
        }
        .to_owned();
        self.entries.push(Entry {
            item_id,
            notification,
            bytes,
        });
        self.bytes += bytes;
    }

    pub(super) fn snapshot(&self, active_turn_id: Option<&str>) -> Vec<ServerNotification> {
        if active_turn_id.is_none() || self.turn_id.as_deref() != active_turn_id {
            return Vec::new();
        }
        let mut notifications = self
            .entries
            .iter()
            .map(|entry| entry.notification.clone())
            .collect::<Vec<_>>();
        if self.incomplete {
            notifications.push(ServerNotification::ConfigWarning(codex_app_server_protocol::ConfigWarningNotification {
                summary: "Active stream recovery is partial: the bounded prefix retention limit was exceeded. Wait for canonical item completion; do not treat the visible prefix as the complete result.".into(),
                details: self.turn_id.as_ref().map(|id| format!("turn_id: {id}")), path: None, range: None,
            }));
        }
        notifications
    }
}

fn merge_size(left: &ServerNotification, right: &ServerNotification) -> Option<usize> {
    let text =
        match (left, right) {
            (
                ServerNotification::AgentMessageDelta(a),
                ServerNotification::AgentMessageDelta(b),
            ) if a.item_id == b.item_id => &b.delta,
            (ServerNotification::PlanDelta(a), ServerNotification::PlanDelta(b))
                if a.item_id == b.item_id =>
            {
                &b.delta
            }
            (
                ServerNotification::ReasoningSummaryTextDelta(a),
                ServerNotification::ReasoningSummaryTextDelta(b),
            ) if a.item_id == b.item_id && a.summary_index == b.summary_index => &b.delta,
            (
                ServerNotification::ReasoningTextDelta(a),
                ServerNotification::ReasoningTextDelta(b),
            ) if a.item_id == b.item_id && a.content_index == b.content_index => &b.delta,
            _ => return None,
        };
    Some(bounded_size(text).saturating_sub(2))
}

fn merge_delta(left: &mut ServerNotification, right: &ServerNotification) {
    match (left, right) {
        (ServerNotification::AgentMessageDelta(a), ServerNotification::AgentMessageDelta(b)) => {
            a.delta.push_str(&b.delta)
        }
        (ServerNotification::PlanDelta(a), ServerNotification::PlanDelta(b)) => {
            a.delta.push_str(&b.delta)
        }
        (
            ServerNotification::ReasoningSummaryTextDelta(a),
            ServerNotification::ReasoningSummaryTextDelta(b),
        ) => a.delta.push_str(&b.delta),
        (ServerNotification::ReasoningTextDelta(a), ServerNotification::ReasoningTextDelta(b)) => {
            a.delta.push_str(&b.delta)
        }
        _ => unreachable!("merge_size checked kind, item and part identity"),
    }
}

fn bounded_size(value: &(impl serde::Serialize + ?Sized)) -> usize {
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len());
            if self.0 > MAX_PREFIX_BYTES {
                return Err(std::io::Error::other("prefix budget exceeded"));
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter(0);
    if serde_json::to_writer(&mut counter, value).is_err() {
        MAX_PREFIX_BYTES + 1
    } else {
        counter.0
    }
}

