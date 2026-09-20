use super::ContextualUserFragment;

pub(crate) const TASK_MODEL_GUIDANCE_OPEN_TAG: &str = "<task_model_guidance>";
pub(crate) const TASK_MODEL_GUIDANCE_CLOSE_TAG: &str = "</task_model_guidance>";
pub(crate) const TASK_MODEL_GUIDANCE_BASE_POLICY_MARKER: &str =
    "<task_model_guidance_policy version=\"1\" />";

pub(crate) fn base_instructions_own_task_model_guidance(base_instructions: &str) -> bool {
    base_instructions.contains(TASK_MODEL_GUIDANCE_BASE_POLICY_MARKER)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TaskModelGuidance;

impl ContextualUserFragment for TaskModelGuidance {
    fn role(&self) -> &'static str {
        "user"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn body(&self) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Borrowed(concat!(
            "Maintain the task state needed for the current request and higher-priority ",
            "instructions: desired outcome, active requirements and constraints, material ",
            "unknowns, and the next necessary action. Form competing hypotheses only when ",
            "uncertainty between explanations affects the next action. Track repository ",
            "ownership and runtime relationships only as needed to establish the requested ",
            "behavior. Preserve applicable provenance kinds for each material claim: direct_file_read, search_hit, ",
            "generated_summary, cached_observation, inferred_relationship, or test_result. Preserve ",
            "those labels through summaries and durable state; cached search evidence retains both its source and freshness. Storage or repetition never upgrades ",
            "its evidence strength. Treat direct file reads as observations of the exact content ",
            "read at that time. Discovery-only hits identify candidates. Complete search results establish ",
            "the exact matching facts they report for their recorded scope and snapshot, but not omitted ",
            "context or broader behavior. Treat generated summaries ",
            "as derived and potentially lossy, cached observations as potentially stale, inferred ",
            "relationships as hypotheses, and test results as proof only for the exact exercised ",
            "contract. These are internal evidence labels, not a mandatory user-facing reporting ",
            "format. Reuse current exact values and enumerations already returned by tools ",
            "instead of rediscovering them. Batch independent read-only checks in one tool ",
            "generation when their tool contracts allow it. For actionable coding tasks, begin ",
            "with the responsible owner, implementation, and direct test when available; expand ",
            "the inspection as evidence requires, and pause only when genuinely blocked. Do not repeat ",
            "an unchanged observation without a relevant input change or pending transition. Instead, ",
            "resolve a named remaining question through different evidence, synthesize the answer, ",
            "or report the blocker. Take state-changing actions only when authorized and necessary. ",
            "Before final synthesis, compare every ",
            "version, ",
            "edition, name, count, path, subcommand, or other literal attributed to a direct file ",
            "read against the retained evidence. If that evidence is unavailable or stale, mark ",
            "the value unknown or refresh it; never substitute a remembered value while citing ",
            "the earlier read. Resolve contradictions using runtime ",
            "reachability, ownership, freshness, and generated-source contracts. Revise the model ",
            "when new evidence disagrees with it. Inspect implementation detail when needed to ",
            "establish the requested behavior; expand beyond the relevant runtime path only for ",
            "a material reason. Never fill an unknown with an ",
            "unstated assumption."
        ))
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
        assert!(rendered.contains("Storage or repetition never upgrades"));
        assert!(rendered.contains("generated summaries as derived and potentially lossy"));
        assert!(rendered.contains("Reuse current exact values and enumerations"));
        assert!(rendered.contains("Batch independent read-only checks"));
        assert!(rendered.contains("relevant input change or pending transition"));
        assert!(rendered.contains("resolve a named remaining question through different evidence"));
        assert!(
            rendered.contains("Take state-changing actions only when authorized and necessary")
        );
        assert!(rendered.contains("cached search evidence retains both its source and freshness"));
        assert!(rendered.contains("Complete search results establish the exact matching facts"));
        assert!(rendered.contains("edition, name, count, path, subcommand"));
        assert!(rendered.contains("never substitute a remembered value"));
        assert!(rendered.contains("Never fill an unknown"));
        assert!(rendered.ends_with(TASK_MODEL_GUIDANCE_CLOSE_TAG));
    }

    #[test]
    fn model_message_conditions_diagnostic_work_on_the_requested_behavior() {
        use codex_protocol::models::ContentItem;
        use codex_protocol::models::ResponseItem;

        let ResponseItem::Message { role, content, .. } =
            ContextualUserFragment::into(TaskModelGuidance)
        else {
            panic!("expected a model message");
        };
        assert_eq!(role, "user");
        let [ContentItem::InputText { text }] = content.as_slice() else {
            panic!("expected one guidance fragment");
        };
        assert!(text.starts_with("<task_model_guidance>"));
        assert!(text.ends_with("</task_model_guidance>"));
        for required in [
            "Form competing hypotheses only when uncertainty between explanations affects the next action.",
            "Track repository ownership and runtime relationships only as needed to establish the requested behavior.",
            "These are internal evidence labels, not a mandatory user-facing reporting format.",
            "Inspect implementation detail when needed to establish the requested behavior; expand beyond the relevant runtime path only for a material reason.",
        ] {
            assert!(
                text.contains(required),
                "missing conditional guidance: {required}"
            );
        }
        for obsolete in [
            "Before acting",
            "one to three plausible hypotheses",
            "stay at module-level abstraction",
        ] {
            assert!(
                !text.contains(obsolete),
                "unconditional diagnostic requirement returned: {obsolete}"
            );
        }
    }

    #[test]
    fn renders_action_first_contract_for_actionable_coding_tasks() {
        let rendered = TaskModelGuidance.render();

        assert!(rendered.contains(
            "For actionable coding tasks, begin with the responsible owner, implementation, and \
             direct test when available; expand the inspection as evidence requires, and pause \
             only when genuinely blocked."
        ));
    }

    #[test]
    fn bundled_prompt_uses_the_full_runtime_guidance_fragment() {
        let bundled = include_str!("../../../protocol/src/prompts/base_instructions/default.md");

        assert!(!base_instructions_own_task_model_guidance(bundled));
        assert!(!base_instructions_own_task_model_guidance(
            "catalog supplied instructions"
        ));
        assert!(TaskModelGuidance.render().contains("direct_file_read"));
    }
}
