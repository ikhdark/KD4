//! Content-free retry decisions. A plan is not evidence of transport entry.
use serde::Serialize;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

static NEXT_DECISION_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RetryTrigger {
    Connection,
    Http,
    Timeout,
    Stream,
    Capacity,
    WebsocketRejected,
    UsageLimit,
    MissingCredentials,
    Authentication,
    Other,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RetryAction {
    Replay,
    Reconnect,
    FallbackHttp,
    NetworkRecovery,
    WebsocketRepair,
    AccountRecovery,
    TransportRetry,
    AuthRecovery,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RetryRoute {
    Http,
    Websocket,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct RetryDecision {
    schema_version: u16,
    clock_id: &'static str,
    decision_id: u64,
    phase: &'static str,
    trigger: RetryTrigger,
    http_status: Option<u16>,
    action: RetryAction,
    generation_index: Option<u32>,
    source_sampling_request_id: Option<String>,
    source_physical_attempt_id: Option<String>,
    executed_sampling_request_id: Option<String>,
    executed_physical_attempt_id: Option<String>,
    executed_route: Option<RetryRoute>,
    started_ns: u128,
    ended_ns: Option<u128>,
    planned_backoff_ns: Option<u128>,
    actual_backoff_ns: u128,
    /// Includes backoff. Never add these two durations together.
    elapsed_ns: u128,
    #[serde(skip)]
    wait_started_ns: Option<u128>,
}

impl RetryDecision {
    fn emit(&self) {
        // Only enums, local identities and numeric timing enter this record.
        // Error messages, URLs, headers, account IDs and credentials never do.
        if let Ok(record) = serde_json::to_string(self) {
            tracing::info!(
                event.name = "codex.retry_decision",
                retry_decision_json = %record,
                "retry decision observation"
            );
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct RetryDecisionLedger {
    pending: Option<RetryDecision>,
}

pub(crate) struct RetryPlan<'a> {
    pub(crate) generation_index: Option<u32>,
    pub(crate) clock_id: &'static str,
    pub(crate) trigger: RetryTrigger,
    pub(crate) http_status: Option<u16>,
    pub(crate) action: RetryAction,
    pub(crate) source: Option<(&'a str, &'a str)>,
    /// None means the account/network recovery deadline is not a scheduled sleep.
    pub(crate) backoff_ns: Option<u128>,
}

pub(crate) struct RetryExecution<'a> {
    pub(crate) generation_index: Option<u32>,
    pub(crate) sampling_request_id: &'a str,
    pub(crate) physical_attempt_id: &'a str,
    pub(crate) route: RetryRoute,
}

impl RetryDecisionLedger {
    pub(crate) fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    pub(crate) fn is_fallback_for(&self, generation_index: Option<u32>) -> bool {
        self.pending.as_ref().is_some_and(|decision| {
            matches!(decision.action, RetryAction::FallbackHttp)
                && (decision.generation_index.is_none()
                    || decision.generation_index == generation_index)
        })
    }

    pub(crate) fn plan(&mut self, now_ns: u128, plan: RetryPlan<'_>) {
        self.finish(now_ns, None);
        let decision = RetryDecision {
            schema_version: 1,
            clock_id: plan.clock_id,
            decision_id: NEXT_DECISION_ID.fetch_add(1, Ordering::Relaxed),
            phase: "planned",
            trigger: plan.trigger,
            http_status: plan.http_status,
            action: plan.action,
            generation_index: plan.generation_index,
            source_sampling_request_id: plan.source.map(|source| source.0.to_owned()),
            source_physical_attempt_id: plan.source.map(|source| source.1.to_owned()),
            executed_sampling_request_id: None,
            executed_physical_attempt_id: None,
            executed_route: None,
            started_ns: now_ns,
            ended_ns: None,
            planned_backoff_ns: plan.backoff_ns,
            actual_backoff_ns: 0,
            elapsed_ns: 0,
            wait_started_ns: None,
        };
        decision.emit();
        self.pending = Some(decision);
    }

    pub(crate) fn begin_wait(&mut self, now_ns: u128) {
        if let Some(decision) = self.pending.as_mut() {
            decision.wait_started_ns.get_or_insert(now_ns);
        }
    }

    pub(crate) fn end_wait(&mut self, now_ns: u128) {
        if let Some(decision) = self.pending.as_mut()
            && let Some(start) = decision.wait_started_ns.take()
        {
            decision.actual_backoff_ns = decision
                .actual_backoff_ns
                .saturating_add(now_ns.saturating_sub(start));
        }
    }

    pub(crate) fn finish(&mut self, now_ns: u128, execution: Option<RetryExecution<'_>>) {
        self.end_wait(now_ns);
        let Some(mut decision) = self.pending.take() else {
            return;
        };
        decision.ended_ns = Some(now_ns);
        decision.elapsed_ns = now_ns.saturating_sub(decision.started_ns);
        decision.phase = if let Some(execution) = execution.filter(|execution| {
            decision.generation_index.is_none()
                || execution.generation_index == decision.generation_index
        }) {
            decision.executed_sampling_request_id = Some(execution.sampling_request_id.to_owned());
            decision.executed_physical_attempt_id = Some(execution.physical_attempt_id.to_owned());
            decision.executed_route = Some(execution.route);
            "executed"
        } else {
            "not_executed"
        };
        decision.emit();
    }
}
