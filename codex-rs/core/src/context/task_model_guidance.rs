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
            "Maintain the current outcome, requirements, constraints, unknowns, and next action. ",
            "Form competing hypotheses only when uncertainty between explanations affects the ",
            "next action. Track repository ownership and runtime relationships only as needed to ",
            "establish the requested behavior. Distinguish evidence sources and freshness. Use ",
            "provenance labels such as direct_file_read, search_hit, generated_summary, ",
            "cached_observation, inferred_relationship, and test_result only when they help ",
            "preserve a material distinction. Preserve material evidence distinctions through ",
            "summaries and durable state; cached search evidence retains both its source and ",
            "freshness. Storage or repetition never upgrades evidence strength. Direct reads ",
            "observe exact content at that time; discovery-only hits identify candidates. ",
            "Complete search results establish the exact matching facts for their recorded scope ",
            "and snapshot, not omitted context or broader behavior. Treat generated summaries as ",
            "derived and potentially lossy, cached observations as potentially stale, inferred ",
            "relationships as hypotheses, and tests as proof only of the exercised contract. ",
            "These are internal evidence labels, not a mandatory user-facing reporting format. ",
            "Before synthesis, check every version, edition, name, count, path, subcommand, or ",
            "other literal attributed to a direct read against retained evidence. If unavailable ",
            "or stale, refresh it or mark it unknown; never substitute a remembered value while ",
            "citing the earlier read. Resolve contradictions by runtime reachability, ownership, ",
            "freshness, and generated-source contracts. Revise conclusions when evidence ",
            "disagrees; never fill an unknown with an unstated assumption. A no-change result is ",
            "valid and preferred when the requested capability already exists adequately. Before ",
            "adding a mechanism, identify the concrete missing capability and explain, using ",
            "relevant source evidence, why existing abstractions are insufficient. Prefer reuse, ",
            "consolidation, or deletion over adding parallel machinery. When edits overlap, ",
            "preserve independent changes and combine compatible behavior against the requested ",
            "contract. Verify the combined runtime path; ask only when conflicting intended ",
            "behavior cannot be resolved from current evidence. Partial wiring of implemented ",
            "code is forbidden. End-to-end wiring is mandatory. Every validation test must assert ",
            "an expected result and fail for a plausible incorrect implementation of the behavior ",
            "or logic under test. Repair weak tests covering the changed behavior or blocking its ",
            "validation. Report unrelated weaknesses encountered without starting a broader test ",
            "audit. When validation or tests report errors, warnings, or failures, let a valid, ",
            "progressing run finish and diagnose all reported issues before making repair edits. ",
            "Stop a run when evidence shows it is stalled or cannot validate the intended inputs; ",
            "retain its output and diagnose the cause before restarting. Apply related fixes in ",
            "consolidated batches and rerun only affected checks that failed or whose prior ",
            "results were invalidated by relevant changes. Repeat only if failures remain or new ",
            "evidence requires it. Reuse passing results while relevant inputs remain unchanged; ",
            "rerun affected checks when those inputs change or new evidence invalidates the ",
            "result."
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
        assert!(rendered.contains("cached search evidence retains both its source and freshness"));
        assert!(rendered.contains("Complete search results establish the exact matching facts"));
        assert!(rendered.contains("edition, name, count, path, subcommand"));
        assert!(rendered.contains("never substitute a remembered value"));
        assert!(rendered.contains("never fill an unknown"));
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
            "test_result only when they help preserve a material distinction.",
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
    fn guidance_keeps_tool_and_authorization_policy_with_the_base() {
        let rendered = TaskModelGuidance.render();
        let base = include_str!("../../../protocol/src/prompts/base_instructions/default.md");
        assert!(base.contains("Batch independent calls"));
        assert!(base.contains("Do not request authorization already provided."));
        assert!(!rendered.contains("Batch independent"));
        assert!(!rendered.contains("Take state-changing actions"));
        for required in [
            "A no-change result is valid and preferred when the requested capability already exists adequately.",
            "Before adding a mechanism, identify the concrete missing capability and explain, using relevant source evidence, why existing abstractions are insufficient.",
            "Prefer reuse, consolidation, or deletion over adding parallel machinery.",
            "When edits overlap, preserve independent changes and combine compatible behavior against the requested contract.",
            "Verify the combined runtime path; ask only when conflicting intended behavior cannot be resolved from current evidence.",
            "Partial wiring of implemented code is forbidden. End-to-end wiring is mandatory.",
            "Every validation test must assert an expected result and fail for a plausible incorrect implementation of the behavior or logic under test.",
            "Repair weak tests covering the changed behavior or blocking its validation.",
            "Report unrelated weaknesses encountered without starting a broader test audit.",
            "When validation or tests report errors, warnings, or failures, let a valid, progressing run finish and diagnose all reported issues before making repair edits. Stop a run when evidence shows it is stalled or cannot validate the intended inputs; retain its output and diagnose the cause before restarting.",
            "Apply related fixes in consolidated batches and rerun only affected checks that failed or whose prior results were invalidated by relevant changes.",
            "Repeat only if failures remain or new evidence requires it.",
            "Reuse passing results while relevant inputs remain unchanged; rerun affected checks when those inputs change or new evidence invalidates the result.",
        ] {
            assert!(
                rendered.contains(required),
                "missing model guidance: {required}"
            );
            assert!(base.contains(required), "missing base guidance: {required}");
        }
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
