use super::ContextualUserFragment;

pub(crate) const TASK_MODEL_GUIDANCE_OPEN_TAG: &str = "<task_model_guidance>";
pub(crate) const TASK_MODEL_GUIDANCE_CLOSE_TAG: &str = "</task_model_guidance>";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TaskModelGuidance;

impl ContextualUserFragment for TaskModelGuidance {
    fn role(&self) -> &'static str {
        "user"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn body(&self) -> String {
        concat!(
            "Before acting, form and maintain a working task model from the current request and ",
            "higher-priority instructions. Track the desired outcome; active requirements and ",
            "constraints; non-requirement context; authoritative, generated, legacy, fallback, ",
            "or unused evidence; repository owners and transitive runtime relationships; material ",
            "unknowns; one to three plausible hypotheses; and the smallest proof route. Attach one ",
            "explicit provenance kind to each material claim: direct_file_read, search_hit, ",
            "generated_summary, cached_observation, inferred_relationship, or test_result. Preserve ",
            "that label through summaries and durable state; storage or repetition never upgrades ",
            "its evidence strength. Treat direct file reads as observations of the exact content ",
            "read at that time, search hits as candidates rather than authority, generated summaries ",
            "as derived and potentially lossy, cached observations as potentially stale, inferred ",
            "relationships as hypotheses, and test results as proof only for the exact exercised ",
            "contract. Reuse current exact values and enumerations already returned by tools ",
            "instead of rediscovering them. Batch independent read-only checks in one tool ",
            "generation when their tool contracts allow it. After an observational or wait ",
            "result leaves the relevant state unchanged, do not repeat that observation unless ",
            "you can name a pending state transition; otherwise synthesize the evidence, take a ",
            "state-changing action, or report the blocker. Before final synthesis, compare every ",
            "version, ",
            "edition, name, count, path, subcommand, or other literal attributed to a direct file ",
            "read against the retained evidence. If that evidence is unavailable or stale, mark ",
            "the value unknown or refresh it; never substitute a remembered value while citing ",
            "the earlier read. Resolve contradictions using runtime ",
            "reachability, ownership, freshness, and generated-source contracts. Revise the model ",
            "when new evidence disagrees with it, and stay at module-level abstraction until a ",
            "specific uncertainty requires implementation detail. Never fill an unknown with an ",
            "unstated assumption."
        )
        .to_string()
    }

    fn type_markers() -> (&'static str, &'static str) {
        (TASK_MODEL_GUIDANCE_OPEN_TAG, TASK_MODEL_GUIDANCE_CLOSE_TAG)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_execution_time_task_model_contract() {
        let rendered = TaskModelGuidance.render();
        assert!(rendered.starts_with(TASK_MODEL_GUIDANCE_OPEN_TAG));
        assert!(rendered.contains("one to three plausible hypotheses"));
        for provenance in [
            "direct_file_read",
            "search_hit",
            "generated_summary",
            "cached_observation",
            "inferred_relationship",
            "test_result",
        ] {
            assert!(rendered.contains(provenance));
        }
        assert!(rendered.contains("storage or repetition never upgrades"));
        assert!(rendered.contains("generated summaries as derived and potentially lossy"));
        assert!(rendered.contains("Reuse current exact values and enumerations"));
        assert!(rendered.contains("Batch independent read-only checks"));
        assert!(rendered.contains("do not repeat that observation unless"));
        assert!(rendered.contains("name a pending state transition"));
        assert!(rendered.contains("edition, name, count, path, subcommand"));
        assert!(rendered.contains("never substitute a remembered value"));
        assert!(rendered.contains("Never fill an unknown"));
        assert!(rendered.ends_with(TASK_MODEL_GUIDANCE_CLOSE_TAG));
    }
}
