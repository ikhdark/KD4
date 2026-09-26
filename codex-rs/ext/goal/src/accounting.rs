use codex_protocol::config_types::ModeKind;
use codex_protocol::protocol::TokenUsage;
use codex_state::ThreadGoalStatus;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::Semaphore;
use tokio::sync::SemaphorePermit;

#[derive(Debug)]
pub(crate) struct GoalAccountingState {
    inner: Mutex<GoalAccountingInner>,
    progress_accounting_lock: Semaphore,
}

#[derive(Debug)]
struct GoalAccountingInner {
    current_turn_id: Option<String>,
    turns: HashMap<String, GoalTurnAccounting>,
    wall_clock: GoalWallClockAccounting,
    budget_limit_reported_goal_id: Option<String>,
}

#[derive(Debug)]
struct GoalTurnAccounting {
    current_token_usage: TokenUsage,
    last_accounted_token_usage: TokenUsage,
    active_goal_id: Option<String>,
    account_tokens: bool,
    pending_time_seconds: i64,
}

#[derive(Debug)]
struct GoalWallClockAccounting {
    last_accounted_at: Instant,
    stopped_at: Option<Instant>,
    active_goal_id: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct GoalProgressSnapshot {
    pub(crate) current_token_usage: TokenUsage,
    pub(crate) expected_goal_id: String,
    pub(crate) time_delta_seconds: i64,
    pub(crate) token_delta: i64,
}

#[derive(Debug, Clone)]
pub(crate) struct IdleGoalProgressSnapshot {
    pub(crate) expected_goal_id: String,
    pub(crate) time_delta_seconds: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BudgetLimitedGoalDisposition {
    KeepActive,
    ClearActive,
}

impl GoalAccountingState {
    pub(crate) fn has_active_goal(&self) -> bool {
        self.inner().wall_clock.active_goal_id.is_some()
    }

    pub(crate) fn start_turn(
        &self,
        turn_id: impl Into<String>,
        collaboration_mode: ModeKind,
        token_usage_at_turn_start: &TokenUsage,
    ) {
        let turn_id = turn_id.into();
        let mut inner = self.inner();
        inner.current_turn_id = Some(turn_id.clone());
        inner.turns.insert(
            turn_id,
            GoalTurnAccounting::new(
                token_usage_at_turn_start.clone(),
                !matches!(collaboration_mode, ModeKind::Plan),
            ),
        );
    }

    pub(crate) fn current_turn_id(&self) -> Option<String> {
        self.inner().current_turn_id.clone()
    }

    /// Acquires the per-thread progress-accounting permit.
    ///
    /// Hold the returned permit from before taking a progress snapshot until after the persistent
    /// usage write has succeeded and the snapshot has been marked accounted. This serializes
    /// concurrent tool-completion hooks so only one hook can charge a given token or time delta.
    pub(crate) async fn progress_accounting_permit(
        &self,
    ) -> Result<SemaphorePermit<'_>, tokio::sync::AcquireError> {
        self.progress_accounting_lock.acquire().await
    }

    pub(crate) fn turn_is_current_active_goal(&self, turn_id: &str) -> bool {
        let inner = self.inner();
        if inner.current_turn_id.as_deref() != Some(turn_id) {
            return false;
        }
        let Some(turn) = inner.turns.get(turn_id) else {
            return false;
        };
        turn.account_tokens && turn.active_goal_id.is_some()
    }

    pub(crate) fn record_token_usage(
        &self,
        turn_id: &str,
        total_usage: &TokenUsage,
        last_usage: &TokenUsage,
    ) {
        let mut inner = self.inner();
        if inner.current_turn_id.as_deref() == Some(turn_id)
            && let Some(turn) = inner.turns.get_mut(turn_id)
        {
            turn.record_token_usage(total_usage, last_usage);
        }
    }

    pub(crate) fn mark_turn_goal_active(&self, turn_id: &str, goal_id: impl Into<String>) {
        let mut inner = self.inner();
        let goal_id = goal_id.into();
        if inner.budget_limit_reported_goal_id.as_deref() != Some(goal_id.as_str()) {
            inner.budget_limit_reported_goal_id = None;
        }
        if let Some(turn) = inner.turns.get_mut(turn_id) {
            turn.active_goal_id = Some(goal_id.clone());
            if inner.current_turn_id.as_deref() == Some(turn_id) {
                inner.wall_clock.mark_active_goal(goal_id);
            }
        }
    }

    pub(crate) fn mark_current_turn_goal_active(
        &self,
        goal_id: impl Into<String>,
    ) -> Option<String> {
        let mut inner = self.inner();
        let turn_id = inner.current_turn_id.clone()?;
        let goal_id = goal_id.into();
        if inner.budget_limit_reported_goal_id.as_deref() != Some(goal_id.as_str()) {
            inner.budget_limit_reported_goal_id = None;
        }
        let turn = inner.turns.get_mut(turn_id.as_str())?;
        if turn.active_goal_id.as_deref() != Some(goal_id.as_str()) {
            turn.reset_baseline_to_current();
            turn.active_goal_id = Some(goal_id.clone());
        }
        inner.wall_clock.mark_active_goal(goal_id);
        Some(turn_id)
    }

    pub(crate) fn mark_idle_goal_active(&self, goal_id: impl Into<String>) {
        let mut inner = self.inner();
        let goal_id = goal_id.into();
        if inner.budget_limit_reported_goal_id.as_deref() != Some(goal_id.as_str()) {
            inner.budget_limit_reported_goal_id = None;
        }
        inner.wall_clock.mark_active_goal(goal_id);
    }

    pub(crate) fn clear_current_turn_goal(&self) -> Option<String> {
        let mut inner = self.inner();
        let turn_id = inner.current_turn_id.clone()?;
        if let Some(turn) = inner.turns.get_mut(turn_id.as_str()) {
            turn.active_goal_id = None;
        }
        inner.wall_clock.clear_active_goal();
        inner.budget_limit_reported_goal_id = None;
        Some(turn_id)
    }

    pub(crate) fn clear_active_goal(&self) {
        let mut inner = self.inner();
        if let Some(turn_id) = inner.current_turn_id.clone()
            && let Some(turn) = inner.turns.get_mut(turn_id.as_str())
        {
            turn.active_goal_id = None;
        }
        inner.wall_clock.clear_active_goal();
        inner.budget_limit_reported_goal_id = None;
    }

    /// Freeze usage already incurred so stopping work never depends on a successful flush.
    pub(crate) fn suspend_accounting(&self) {
        let mut inner = self.inner();
        let elapsed = inner.wall_clock.time_delta_since_last_accounting();
        let goal_id = inner.wall_clock.active_goal_id.clone();
        if inner.current_turn_id.is_none() {
            inner.wall_clock.stopped_at.get_or_insert_with(Instant::now);
            return;
        }
        if let Some(turn_id) = inner.current_turn_id.take()
            && let Some(turn) = inner.turns.get_mut(&turn_id)
            && turn.active_goal_id == goal_id
            && goal_id.is_some()
        {
            turn.pending_time_seconds += elapsed;
        }
        inner.wall_clock.clear_active_goal();
        inner.budget_limit_reported_goal_id = None;
    }

    pub(crate) fn progress_snapshot(&self, turn_id: &str) -> Option<GoalProgressSnapshot> {
        let inner = self.inner();
        let turn = inner.turns.get(turn_id)?;
        if !turn.account_tokens {
            return None;
        }
        let expected_goal_id = turn.active_goal_id()?;
        let token_delta = turn.token_delta_since_last_accounting();
        let time_delta_seconds = turn.pending_time_seconds
            + if inner.wall_clock.active_goal_id.as_deref() == Some(expected_goal_id.as_str()) {
                inner.wall_clock.time_delta_since_last_accounting()
            } else {
                0
            };
        if time_delta_seconds == 0 && token_delta <= 0 {
            return None;
        }
        Some(GoalProgressSnapshot {
            current_token_usage: turn.current_token_usage.clone(),
            expected_goal_id,
            time_delta_seconds,
            token_delta,
        })
    }

    pub(crate) fn idle_progress_snapshot(&self) -> Option<IdleGoalProgressSnapshot> {
        let inner = self.inner();
        let expected_goal_id = inner.wall_clock.active_goal_id.clone()?;
        let time_delta_seconds = inner.wall_clock.time_delta_since_last_accounting();
        if time_delta_seconds == 0 {
            return None;
        }
        Some(IdleGoalProgressSnapshot {
            expected_goal_id,
            time_delta_seconds,
        })
    }

    pub(crate) fn mark_progress_accounted_for_status(
        &self,
        turn_id: &str,
        snapshot: &GoalProgressSnapshot,
        status: ThreadGoalStatus,
        budget_limited_goal_disposition: BudgetLimitedGoalDisposition,
    ) {
        let clear_active_goal = should_clear_active_goal(status, budget_limited_goal_disposition);
        let mut inner = self.inner();
        let mut pending_accounted = 0;
        if let Some(turn) = inner.turns.get_mut(turn_id)
            && turn.active_goal_id.as_deref() == Some(snapshot.expected_goal_id.as_str())
        {
            turn.last_accounted_token_usage = snapshot.current_token_usage.clone();
            pending_accounted = turn.pending_time_seconds.min(snapshot.time_delta_seconds);
            turn.pending_time_seconds -= pending_accounted;
            if clear_active_goal {
                turn.active_goal_id = None;
            }
        }
        if inner.wall_clock.active_goal_id.as_deref() == Some(snapshot.expected_goal_id.as_str()) {
            inner
                .wall_clock
                .mark_accounted(snapshot.time_delta_seconds - pending_accounted);
            if clear_active_goal {
                inner.wall_clock.clear_active_goal();
            }
        }
        if status != ThreadGoalStatus::BudgetLimited {
            inner.budget_limit_reported_goal_id = None;
        }
    }

    pub(crate) fn pending_turn_ids(&self) -> Vec<String> {
        let inner = self.inner();
        inner
            .turns
            .keys()
            .filter(|turn_id| inner.current_turn_id.as_ref() != Some(*turn_id))
            .cloned()
            .collect()
    }

    pub(crate) fn defer_turn_finish(&self, turn_id: &str) {
        let mut inner = self.inner();
        if inner.current_turn_id.as_deref() == Some(turn_id) {
            inner.current_turn_id = None;
        }
    }

    pub(crate) fn finish_turn(&self, turn_id: &str) {
        let mut inner = self.inner();
        inner.turns.remove(turn_id);
        if inner.current_turn_id.as_deref() == Some(turn_id) {
            inner.current_turn_id = None;
        }
    }

    pub(crate) fn mark_idle_progress_accounted_for_status(
        &self,
        snapshot: &IdleGoalProgressSnapshot,
        status: ThreadGoalStatus,
        budget_limited_goal_disposition: BudgetLimitedGoalDisposition,
    ) {
        let clear_active_goal = should_clear_active_goal(status, budget_limited_goal_disposition);
        let mut inner = self.inner();
        if inner.wall_clock.active_goal_id.as_deref() != Some(snapshot.expected_goal_id.as_str()) {
            return;
        }
        inner.wall_clock.mark_accounted(snapshot.time_delta_seconds);
        if clear_active_goal {
            inner.wall_clock.clear_active_goal();
        }
        if status != ThreadGoalStatus::BudgetLimited {
            inner.budget_limit_reported_goal_id = None;
        }
    }

    pub(crate) fn reset_idle_progress_baseline_and_clear_active_goal(&self) {
        let mut inner = self.inner();
        inner.wall_clock.reset_baseline();
        inner.wall_clock.clear_active_goal();
        inner.budget_limit_reported_goal_id = None;
    }

    pub(crate) fn mark_budget_limit_reported_if_new(&self, goal_id: &str) -> bool {
        let mut inner = self.inner();
        if inner.budget_limit_reported_goal_id.as_deref() == Some(goal_id) {
            return false;
        }
        inner.budget_limit_reported_goal_id = Some(goal_id.to_string());
        true
    }

    pub(crate) fn rearm_budget_limit_report(&self, goal_id: &str) {
        let mut inner = self.inner();
        if inner.budget_limit_reported_goal_id.as_deref() == Some(goal_id) {
            inner.budget_limit_reported_goal_id = None;
        }
    }

    fn inner(&self) -> std::sync::MutexGuard<'_, GoalAccountingInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Default for GoalAccountingState {
    fn default() -> Self {
        Self {
            inner: Mutex::new(GoalAccountingInner::default()),
            progress_accounting_lock: Semaphore::new(/*permits*/ 1),
        }
    }
}

fn usage_difference(current: &TokenUsage, previous: &TokenUsage) -> TokenUsage {
    TokenUsage {
        input_tokens: current.input_tokens.saturating_sub(previous.input_tokens),
        cached_input_tokens: current
            .cached_input_tokens
            .saturating_sub(previous.cached_input_tokens),
        output_tokens: current.output_tokens.saturating_sub(previous.output_tokens),
        reasoning_output_tokens: current
            .reasoning_output_tokens
            .saturating_sub(previous.reasoning_output_tokens),
        total_tokens: current.total_tokens.saturating_sub(previous.total_tokens),
    }
}

fn token_delta_since_last_accounting(last: &TokenUsage, current: &TokenUsage) -> i64 {
    goal_token_delta_for_usage(&usage_difference(current, last))
}

pub(crate) fn goal_token_delta_for_usage(usage: &TokenUsage) -> i64 {
    usage
        .input_tokens
        .saturating_sub(usage.cached_input_tokens)
        .saturating_add(usage.output_tokens.max(0))
}

impl Default for GoalAccountingInner {
    fn default() -> Self {
        Self {
            current_turn_id: None,
            turns: HashMap::new(),
            wall_clock: GoalWallClockAccounting::new(),
            budget_limit_reported_goal_id: None,
        }
    }
}

impl GoalTurnAccounting {
    fn new(current_token_usage: TokenUsage, account_tokens: bool) -> Self {
        Self {
            last_accounted_token_usage: current_token_usage.clone(),
            current_token_usage,
            active_goal_id: None,
            account_tokens,
            pending_time_seconds: 0,
        }
    }

    fn active_goal_id(&self) -> Option<String> {
        self.active_goal_id.clone()
    }

    fn reset_baseline_to_current(&mut self) {
        self.last_accounted_token_usage = self.current_token_usage.clone();
    }

    fn record_token_usage(&mut self, total_usage: &TokenUsage, last_usage: &TokenUsage) {
        // Hosts can rebase cumulative usage between reports, e.g. after a context-window
        // overflow. Move the baseline to the rebased origin so unflushed usage and this report
        // are both charged instead of being hidden by a negative cumulative delta.
        let origin = usage_difference(total_usage, last_usage);
        let observed = &self.current_token_usage;
        if origin.input_tokens < observed.input_tokens
            || origin.cached_input_tokens < observed.cached_input_tokens
            || origin.output_tokens < observed.output_tokens
        {
            let unaccounted = usage_difference(observed, &self.last_accounted_token_usage);
            self.last_accounted_token_usage = usage_difference(&origin, &unaccounted);
        }
        self.current_token_usage = total_usage.clone();
    }

    fn token_delta_since_last_accounting(&self) -> i64 {
        token_delta_since_last_accounting(
            &self.last_accounted_token_usage,
            &self.current_token_usage,
        )
    }
}

impl GoalWallClockAccounting {
    fn new() -> Self {
        Self {
            last_accounted_at: Instant::now(),
            stopped_at: None,
            active_goal_id: None,
        }
    }

    fn time_delta_since_last_accounting(&self) -> i64 {
        let end = self.stopped_at.unwrap_or_else(Instant::now);
        i64::try_from(
            end.saturating_duration_since(self.last_accounted_at)
                .as_secs(),
        )
        .unwrap_or(i64::MAX)
    }

    fn mark_accounted(&mut self, accounted_seconds: i64) {
        if accounted_seconds <= 0 {
            return;
        }
        let advance = Duration::from_secs(u64::try_from(accounted_seconds).unwrap_or(u64::MAX));
        self.last_accounted_at = self
            .last_accounted_at
            .checked_add(advance)
            .unwrap_or_else(Instant::now);
    }

    fn reset_baseline(&mut self) {
        self.last_accounted_at = Instant::now();
        self.stopped_at = None;
    }

    fn mark_active_goal(&mut self, goal_id: impl Into<String>) {
        let goal_id = goal_id.into();
        if self.active_goal_id.as_deref() != Some(goal_id.as_str()) || self.stopped_at.is_some() {
            self.reset_baseline();
            self.active_goal_id = Some(goal_id);
        }
    }

    fn clear_active_goal(&mut self) {
        self.active_goal_id = None;
        self.reset_baseline();
    }
}

fn should_clear_active_goal(
    status: ThreadGoalStatus,
    budget_limited_goal_disposition: BudgetLimitedGoalDisposition,
) -> bool {
    match status {
        ThreadGoalStatus::Active => false,
        ThreadGoalStatus::BudgetLimited => matches!(
            budget_limited_goal_disposition,
            BudgetLimitedGoalDisposition::ClearActive
        ),
        ThreadGoalStatus::Paused
        | ThreadGoalStatus::Blocked
        | ThreadGoalStatus::UsageLimited
        | ThreadGoalStatus::Complete => true,
    }
}
