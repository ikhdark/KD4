use crate::config::RolloutBudgetConfig;
use codex_features::RolloutBudgetAction;
use codex_protocol::ThreadId;
use codex_protocol::protocol::TokenUsage;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::OnceLock;

pub(crate) const ROLLOUT_BUDGET_APPROVAL_PHRASE: &str = "approve additional budget";

pub(crate) struct RolloutBudgetReminder {
    pub(crate) remaining_tokens: i64,
    reminder_index: i64,
}

#[must_use = "a subagent budget reservation must be committed after a successful spawn"]
pub(crate) struct SubagentBudgetReservation<'a> {
    budget: &'a RolloutBudget,
    charged: bool,
}

impl SubagentBudgetReservation<'_> {
    pub(crate) fn commit(mut self) {
        self.charged = false;
    }
}

impl Drop for SubagentBudgetReservation<'_> {
    fn drop(&mut self) {
        if self.charged {
            self.budget.refund_subagent_spawn();
        }
    }
}

/// Shared accounting and reminder state for one root-thread session tree.
#[derive(Default)]
pub(crate) struct RolloutBudget {
    state: OnceLock<Mutex<RolloutBudgetState>>,
}

struct RolloutBudgetState {
    config: RolloutBudgetConfig,
    weighted_tokens_used: f64,
    effective_limit_tokens: f64,
    budget_epoch: i64,
    ask_approval_pending: bool,
    model_calls: u64,
    tool_output_bytes: u64,
    subagent_count: u64,
    /// Last reminder delivered to each thread, so every thread observes crossed thresholds.
    deliveries: HashMap<ThreadId, ThreadBudgetDelivery>,
}

struct ThreadBudgetDelivery {
    window_id: String,
    reminder_index: i64,
}

impl RolloutBudget {
    pub(crate) fn configure(&self, config: RolloutBudgetConfig) {
        self.state.get_or_init(|| {
            Mutex::new(RolloutBudgetState {
                effective_limit_tokens: config.limit_tokens as f64,
                config,
                weighted_tokens_used: 0.0,
                budget_epoch: 0,
                ask_approval_pending: false,
                model_calls: 0,
                tool_output_bytes: 0,
                subagent_count: 0,
                deliveries: HashMap::new(),
            })
        });
    }

    /// Records API-reported usage and returns whether the current turn must stop.
    pub(crate) fn record_usage(&self, usage: &TokenUsage, turn_id: &str) -> bool {
        let Some(mut state) = self.lock() else {
            return false;
        };
        state.weighted_tokens_used += usage.output_tokens.max(0) as f64
            * state.config.sampling_token_weight
            + usage.non_cached_input() as f64 * state.config.prefill_token_weight
            + usage.cached_input_tokens.max(0) as f64 * state.config.cached_input_token_weight;
        state.should_block(turn_id)
    }

    /// Atomically checks and charges a model request before it is sent.
    pub(crate) fn try_reserve_model_call(&self, _turn_id: &str) -> bool {
        let Some(mut state) = self.lock() else {
            return true;
        };
        let cost = state.config.model_call_token_cost;
        if !state.try_reserve_cost(cost) {
            return false;
        }
        state.model_calls = state.model_calls.saturating_add(1);
        state.weighted_tokens_used += cost;
        true
    }

    pub(crate) fn record_tool_output_bytes(&self, bytes: usize) {
        let Some(mut state) = self.lock() else {
            return;
        };
        let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
        state.tool_output_bytes = state.tool_output_bytes.saturating_add(bytes);
        state.weighted_tokens_used += bytes as f64 * state.config.tool_output_byte_weight;
    }

    pub(crate) fn try_reserve_subagent_spawn(&self) -> Result<SubagentBudgetReservation<'_>, ()> {
        let Some(mut state) = self.lock() else {
            return Ok(SubagentBudgetReservation {
                budget: self,
                charged: false,
            });
        };
        let cost = state.config.subagent_token_cost;
        if !state.try_reserve_cost(cost) {
            return Err(());
        }
        state.subagent_count = state.subagent_count.saturating_add(1);
        state.weighted_tokens_used += cost;
        Ok(SubagentBudgetReservation {
            budget: self,
            charged: true,
        })
    }

    #[cfg(test)]
    pub(crate) fn should_block_sampling(&self, turn_id: &str) -> bool {
        self.lock()
            .is_some_and(|mut state| state.should_block(turn_id))
    }

    pub(crate) fn approve_additional_tranche(&self) -> bool {
        let Some(mut state) = self.lock() else {
            return false;
        };
        if state.config.action != RolloutBudgetAction::Ask || !state.ask_approval_pending {
            return false;
        }
        state.effective_limit_tokens += state.config.limit_tokens as f64;
        state.budget_epoch = state.budget_epoch.saturating_add(1);
        state.ask_approval_pending = false;
        true
    }

    pub(crate) fn pending_reminder(
        &self,
        thread_id: ThreadId,
        window_id: &str,
    ) -> Option<RolloutBudgetReminder> {
        let state = self.lock()?;
        let remaining_tokens = (state.effective_limit_tokens - state.weighted_tokens_used)
            .max(0.0)
            .floor() as i64;
        let crossed_thresholds = state
            .config
            .reminder_at_remaining_tokens
            .iter()
            .filter(|&&threshold| remaining_tokens <= threshold)
            .count() as i64;
        let reminder_span = state.config.reminder_at_remaining_tokens.len() as i64 + 2;
        let reminder_index = state
            .budget_epoch
            .saturating_mul(reminder_span)
            .saturating_add(crossed_thresholds)
            .saturating_add(i64::from(remaining_tokens == 0));
        if state.deliveries.get(&thread_id).is_some_and(|delivery| {
            delivery.window_id.as_str() == window_id && delivery.reminder_index >= reminder_index
        }) {
            return None;
        }
        Some(RolloutBudgetReminder {
            remaining_tokens,
            reminder_index,
        })
    }

    pub(crate) fn mark_reminder_delivered(
        &self,
        thread_id: ThreadId,
        window_id: &str,
        reminder: RolloutBudgetReminder,
    ) {
        // Mark delivery only after history insertion; cancellation before then should retry it.
        let Some(mut state) = self.lock() else {
            return;
        };
        state.deliveries.insert(
            thread_id,
            ThreadBudgetDelivery {
                window_id: window_id.to_string(),
                reminder_index: reminder.reminder_index,
            },
        );
    }

    /// Forces the next sampling request for `thread_id` to restate the current remainder.
    pub(crate) fn rearm_reminder(&self, thread_id: ThreadId) {
        let Some(mut state) = self.lock() else {
            return;
        };
        state.deliveries.remove(&thread_id);
    }

    fn lock(&self) -> Option<MutexGuard<'_, RolloutBudgetState>> {
        self.state.get().map(|state| {
            state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        })
    }

    fn refund_subagent_spawn(&self) {
        let Some(mut state) = self.lock() else {
            return;
        };
        if state.subagent_count == 0 {
            return;
        }
        state.subagent_count -= 1;
        state.weighted_tokens_used =
            (state.weighted_tokens_used - state.config.subagent_token_cost).max(0.0);
    }
}

impl RolloutBudgetState {
    fn try_reserve_cost(&mut self, cost: f64) -> bool {
        if self.config.action == RolloutBudgetAction::Remind {
            return true;
        }
        if self.weighted_tokens_used + cost >= self.effective_limit_tokens {
            if self.config.action == RolloutBudgetAction::Ask {
                self.ask_approval_pending = true;
            }
            return false;
        }
        true
    }

    fn should_block(&mut self, _turn_id: &str) -> bool {
        if self.weighted_tokens_used < self.effective_limit_tokens {
            return false;
        }
        match self.config.action {
            RolloutBudgetAction::Remind => false,
            RolloutBudgetAction::Stop => true,
            RolloutBudgetAction::Ask => {
                self.ask_approval_pending = true;
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(action: RolloutBudgetAction) -> RolloutBudgetConfig {
        RolloutBudgetConfig {
            limit_tokens: 60,
            reminder_at_remaining_tokens: vec![30, 10],
            sampling_token_weight: 1.0,
            prefill_token_weight: 1.0,
            cached_input_token_weight: 0.5,
            model_call_token_cost: 10.0,
            tool_output_byte_weight: 0.25,
            subagent_token_cost: 20.0,
            action,
        }
    }

    #[test]
    fn shared_budget_charges_calls_cached_input_tool_bytes_and_subagents() {
        let budget = RolloutBudget::default();
        budget.configure(config(RolloutBudgetAction::Stop));

        assert!(budget.try_reserve_model_call("turn-1"));
        budget.record_tool_output_bytes(40);
        budget
            .try_reserve_subagent_spawn()
            .expect("subagent reservation should fit")
            .commit();
        assert!(!budget.record_usage(
            &TokenUsage {
                input_tokens: 10,
                cached_input_tokens: 4,
                output_tokens: 10,
                reasoning_output_tokens: 0,
                total_tokens: 20,
            },
            "turn-1",
        ));
        budget.record_tool_output_bytes(8);

        assert!(budget.should_block_sampling("turn-1"));
    }

    #[test]
    fn remind_action_never_blocks_sampling() {
        let budget = RolloutBudget::default();
        let mut config = config(RolloutBudgetAction::Remind);
        config.limit_tokens = 1;
        budget.configure(config);

        assert!(budget.try_reserve_model_call("turn-1"));
        assert!(!budget.should_block_sampling("turn-1"));
    }

    #[test]
    fn ask_action_requires_explicit_approval_before_granting_a_new_tranche() {
        let budget = RolloutBudget::default();
        let mut config = config(RolloutBudgetAction::Ask);
        config.limit_tokens = 10;
        config.model_call_token_cost = 10.0;
        budget.configure(config);

        assert!(!budget.try_reserve_model_call("turn-1"));
        assert!(!budget.try_reserve_model_call("turn-2"));
        assert!(budget.approve_additional_tranche());
        assert!(budget.try_reserve_model_call("turn-2"));
        assert!(!budget.try_reserve_model_call("turn-2"));
        assert!(budget.approve_additional_tranche());
        assert!(!budget.approve_additional_tranche());
    }

    #[test]
    fn projected_model_call_cost_blocks_before_the_request() {
        let budget = RolloutBudget::default();
        let mut config = config(RolloutBudgetAction::Stop);
        config.limit_tokens = 10;
        config.model_call_token_cost = 10.0;
        budget.configure(config);

        assert!(!budget.try_reserve_model_call("turn-1"));
    }

    #[test]
    fn concurrent_model_call_reservations_cannot_overshoot() {
        use std::sync::Arc;
        use std::sync::Barrier;
        use std::thread;

        let budget = Arc::new(RolloutBudget::default());
        let mut config = config(RolloutBudgetAction::Stop);
        config.limit_tokens = 11;
        config.model_call_token_cost = 6.0;
        budget.configure(config);
        let barrier = Arc::new(Barrier::new(3));
        let handles = (0..2)
            .map(|index| {
                let budget = Arc::clone(&budget);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    budget.try_reserve_model_call(&format!("turn-{index}"))
                })
            })
            .collect::<Vec<_>>();

        barrier.wait();
        let admitted = handles
            .into_iter()
            .map(|handle| handle.join().expect("reservation thread panicked"))
            .filter(|admitted| *admitted)
            .count();
        assert_eq!(admitted, 1);
    }

    #[test]
    fn dropped_subagent_reservation_refunds_the_charge() {
        let budget = RolloutBudget::default();
        let mut config = config(RolloutBudgetAction::Stop);
        config.limit_tokens = 31;
        config.subagent_token_cost = 20.0;
        budget.configure(config);

        drop(
            budget
                .try_reserve_subagent_spawn()
                .expect("first reservation should fit"),
        );
        budget
            .try_reserve_subagent_spawn()
            .expect("dropped reservation must be refunded")
            .commit();
        assert!(budget.try_reserve_model_call("turn-1"));
    }
}
