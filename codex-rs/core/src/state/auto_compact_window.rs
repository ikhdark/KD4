use codex_protocol::protocol::TokenUsage;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AutoCompactWindowIds {
    pub(crate) first_window_id: Uuid,
    pub(crate) previous_window_id: Option<Uuid>,
    pub(crate) window_id: Uuid,
}

impl AutoCompactWindowIds {
    pub(crate) fn new_initial() -> Self {
        let window_id = Uuid::now_v7();
        Self {
            first_window_id: window_id,
            previous_window_id: None,
            window_id,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AutoCompactWindowSnapshot {
    pub(crate) prefill_input_tokens: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutoCompactWindowPrefill {
    ServerObserved(i64),
    Estimated(i64),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AutoCompactWindow {
    window_number: u64,
    ids: AutoCompactWindowIds,
    /// Absolute input-token baseline for the current compaction window.
    ///
    /// `body_after_prefix` subtracts this from later active-context usage. It is
    /// not the growth itself; server-observed usage replaces estimated
    /// resume/recompute baselines when available.
    prefill_input_tokens: Option<AutoCompactWindowPrefill>,
}

impl AutoCompactWindow {
    pub(super) fn new_with_ids(ids: AutoCompactWindowIds) -> Self {
        Self {
            window_number: 0,
            ids,
            prefill_input_tokens: None,
        }
    }

    pub(super) fn clear_prefill(&mut self) {
        self.prefill_input_tokens = None;
    }

    pub(super) fn window_number(&self) -> u64 {
        self.window_number
    }

    pub(super) fn ids(&self) -> AutoCompactWindowIds {
        self.ids
    }

    pub(super) fn restore(&mut self, window_number: u64, ids: AutoCompactWindowIds) {
        self.window_number = window_number;
        self.ids = ids;
    }

    pub(super) fn advance(&mut self) -> (u64, AutoCompactWindowIds) {
        self.window_number = self.window_number.saturating_add(1);
        self.ids.previous_window_id = Some(self.ids.window_id);
        self.ids.window_id = Uuid::now_v7();
        self.prefill_input_tokens = None;
        (self.window_number, self.ids)
    }

    /// Records the request-input side of the first server usage sample. The
    /// sampled output from that response is body growth and should remain
    /// counted against the scoped auto-compact budget.
    pub(super) fn ensure_server_observed_prefill_from_usage(&mut self, usage: &TokenUsage) {
        if matches!(
            self.prefill_input_tokens,
            Some(AutoCompactWindowPrefill::ServerObserved(_))
        ) {
            return;
        }

        self.prefill_input_tokens = Some(AutoCompactWindowPrefill::ServerObserved(
            usage.input_tokens.max(0),
        ));
    }

    pub(super) fn set_estimated_prefill(&mut self, tokens: i64) {
        if self.prefill_input_tokens.is_some() {
            return;
        }

        self.prefill_input_tokens = Some(AutoCompactWindowPrefill::Estimated(tokens.max(0)));
    }

    pub(super) fn snapshot(&self) -> AutoCompactWindowSnapshot {
        let prefill_input_tokens = match self.prefill_input_tokens {
            Some(AutoCompactWindowPrefill::ServerObserved(tokens))
            | Some(AutoCompactWindowPrefill::Estimated(tokens)) => Some(tokens),
            None => None,
        };
        AutoCompactWindowSnapshot {
            prefill_input_tokens,
        }
    }

    /// The request input of the window's first server usage sample. An estimated
    /// resume/recompute baseline covers history only, not a full prepared request.
    pub(super) fn server_observed_prefill_input_tokens(&self) -> Option<i64> {
        match self.prefill_input_tokens {
            Some(AutoCompactWindowPrefill::ServerObserved(tokens)) => Some(tokens),
            Some(AutoCompactWindowPrefill::Estimated(_)) | None => None,
        }
    }
}

