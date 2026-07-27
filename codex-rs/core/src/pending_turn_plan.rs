use std::collections::HashMap;
use std::collections::HashSet;

pub(crate) const MAX_FIXED_POINT_ITERATIONS: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)] // Keep impact classes explicit for fixed-point diagnostics and future effects.
pub(crate) enum EffectImpact {
    NonInvalidating,
    InvalidatesInventory,
    InvalidatesEnvironment,
    InvalidatesToolManifest,
    InvalidatesModelVisibleState,
}

impl EffectImpact {
    pub(crate) fn invalidates_snapshot(self) -> bool {
        !matches!(self, Self::NonInvalidating)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct PlanningSnapshotIdentity {
    pub(crate) generation: u64,
    pub(crate) state_digest: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CompletedEffect {
    pub(crate) impact: EffectImpact,
    pub(crate) expected_inventory_keys: HashSet<String>,
}

#[derive(Debug, Default)]
pub(crate) struct FixedPointPlanningState {
    iterations: usize,
    digest_visits: HashMap<String, usize>,
    completed_effects: HashMap<String, CompletedEffect>,
}

impl FixedPointPlanningState {
    pub(crate) fn begin_iteration(
        &mut self,
        snapshot: &PlanningSnapshotIdentity,
    ) -> Result<(), String> {
        self.iterations = self.iterations.saturating_add(1);
        if self.iterations > MAX_FIXED_POINT_ITERATIONS {
            return Err(format!(
                "pending-turn planning did not reach a fixed point after {MAX_FIXED_POINT_ITERATIONS} iterations"
            ));
        }
        let visits = self
            .digest_visits
            .entry(snapshot.state_digest.clone())
            .or_default();
        *visits = visits.saturating_add(1);
        if *visits > MAX_FIXED_POINT_ITERATIONS {
            return Err(format!(
                "pending-turn planning repeatedly invalidated the same model-visible state digest `{}`",
                snapshot.state_digest
            ));
        }
        Ok(())
    }

    pub(crate) fn completed(&self, effect_id: &str) -> Option<&CompletedEffect> {
        self.completed_effects.get(effect_id)
    }

    pub(crate) fn completed_inventory_effects(
        &self,
    ) -> impl Iterator<Item = (&str, &CompletedEffect)> {
        self.completed_effects
            .iter()
            .filter(|(_, effect)| !effect.expected_inventory_keys.is_empty())
            .map(|(id, effect)| (id.as_str(), effect))
    }

    pub(crate) fn record_completed(
        &mut self,
        effect_id: String,
        completed: CompletedEffect,
    ) -> Result<(), String> {
        if let Some(previous) = self.completed_effects.get(&effect_id) {
            if previous != &completed {
                return Err(format!(
                    "semantic effect ID `{effect_id}` was reused for a different completed request"
                ));
            }
            return Ok(());
        }
        self.completed_effects.insert(effect_id, completed);
        Ok(())
    }

    pub(crate) fn require_generation_advance(
        &self,
        before: &PlanningSnapshotIdentity,
        after_generation: u64,
        impact: EffectImpact,
    ) -> Result<(), String> {
        if !impact.invalidates_snapshot() {
            return Ok(());
        }
        if after_generation <= before.generation {
            return Err(format!(
                "{:?} effect completed at planning generation {}, but the next observable generation did not advance",
                impact, before.generation
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(generation: u64, digest: &str) -> PlanningSnapshotIdentity {
        PlanningSnapshotIdentity {
            generation,
            state_digest: digest.to_string(),
        }
    }

    #[test]
    fn invalidating_effect_requires_a_newer_generation() {
        let state = FixedPointPlanningState::default();
        let before = snapshot(4, "state-a");
        assert!(
            state
                .require_generation_advance(&before, 4, EffectImpact::InvalidatesInventory)
                .is_err()
        );
        assert!(
            state
                .require_generation_advance(&before, 5, EffectImpact::InvalidatesInventory)
                .is_ok()
        );
    }

    #[test]
    fn completed_semantic_effect_is_not_replayed_or_reclassified() {
        let mut state = FixedPointPlanningState::default();
        let completed = CompletedEffect {
            impact: EffectImpact::InvalidatesInventory,
            expected_inventory_keys: HashSet::from(["mcp__stdio__tool".to_string()]),
        };
        state
            .record_completed("install:tool@v1".to_string(), completed.clone())
            .expect("first completion");
        assert_eq!(state.completed("install:tool@v1"), Some(&completed));
        assert!(
            state
                .record_completed(
                    "install:tool@v1".to_string(),
                    CompletedEffect {
                        impact: EffectImpact::NonInvalidating,
                        expected_inventory_keys: HashSet::new(),
                    }
                )
                .is_err()
        );
    }

    #[test]
    fn fixed_point_loop_is_bounded() {
        let mut state = FixedPointPlanningState::default();
        for generation in 0..MAX_FIXED_POINT_ITERATIONS {
            state
                .begin_iteration(&snapshot(generation as u64, &format!("state-{generation}")))
                .expect("within bound");
        }
        assert!(state.begin_iteration(&snapshot(9, "state-9")).is_err());
    }
}
