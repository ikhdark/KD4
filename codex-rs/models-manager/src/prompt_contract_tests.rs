use codex_protocol::config_types::Personality;
use codex_protocol::models::BASE_INSTRUCTIONS_DEFAULT;
use codex_protocol::openai_models::ModelsResponse;
use codex_utils_output_truncation::model_token_count;
use pretty_assertions::assert_eq;
use std::collections::BTreeSet;

use crate::prompt_resolver::LOCAL_PROMPT_POLICY_SLUGS;

#[derive(Clone, Copy)]
enum PromptScope {
    FallbackAndBundled,
    LocalPolicyAndFallback,
    LocalPolicy,
    FallbackAndGpt52,
}

#[derive(Clone, Copy)]
enum AnchorExpectation {
    Any,
    All,
    None,
}

struct PromptContract {
    id: &'static str,
    scope: PromptScope,
    expectation: AnchorExpectation,
    anchors: &'static [&'static str],
}

const PROMPT_CONTRACTS: &[PromptContract] = &[
    PromptContract {
        id: "omit-redundant-tool-arguments",
        scope: PromptScope::LocalPolicyAndFallback,
        expectation: AnchorExpectation::All,
        anchors: &[
            "Supply required arguments and optional values that affect behavior",
            "omit equivalent defaults, empty collections, and nulls",
        ],
    },
    PromptContract {
        id: "requirement-fidelity-and-runtime-grounding",
        scope: PromptScope::LocalPolicyAndFallback,
        expectation: AnchorExpectation::All,
        anchors: &[
            "explicit constraints, prohibitions, and out-of-scope work until superseded",
            "Delegated objectives and write scopes bound a worker's task",
            "Read the complete enclosing function, type, or configuration unit before changing it",
            "Current file content overrides summaries, plans, and stale reads",
            "entrypoint through registration, dispatch, feature flags or config defaults to consumers",
            "Partial wiring of implemented code is forbidden. End-to-end wiring is mandatory.",
            "Resolve contradictions by runtime reachability, ownership, and freshness.",
            "Cargo commands sharing a target directory",
            "do not evade denials",
            "combine compatible behavior against the requested contract. Verify the combined runtime path",
            "Cancellation need not roll back effects",
            "without weakening required invariants or assertions",
            "match every explicit requirement, prohibition, and preserved invariant to current evidence",
            "Ending a turn or exhausting a budget does not prove completion",
            "Distinguish missing capability, failure to follow existing guidance, and interface friction",
            "Check the supported reuse path",
            "Incomplete discovery or an output receipt never proves absence or omitted content",
            "Measure progress by resolved requirements and validated outcomes",
            "finish missing coverage without restarting discovery",
            "using the original request and corrections, not just a checklist",
            "Respect explicit user limits",
            "repair omissions with targeted follow-up",
            "without an automatic review loop",
            "report any partial, blocked, or unverified result",
        ],
    },
    PromptContract {
        id: "concise-progress-updates",
        scope: PromptScope::LocalPolicyAndFallback,
        expectation: AnchorExpectation::All,
        anchors: &[
            "Give one brief initial update before tools.",
            "Later updates should report material results, decisions, or blockers, usually in one sentence.",
            "Skip routine edit/test narration and repeated plans",
            "preserve required updates and disclosures",
            "without recapping",
            "Use final for a self-contained handoff:",
            "Do not claim actions or tests that did not occur.",
        ],
    },
    PromptContract {
        id: "complete-substance-with-concise-wording",
        scope: PromptScope::LocalPolicyAndFallback,
        expectation: AnchorExpectation::All,
        anchors: &[
            "Conciseness limits wording, not substance.",
            "Include all requested material content now; do not defer it behind follow-up offers or length targets.",
            "Treat \"what else?\" as a request for remaining material points within scope.",
            "Respect explicit user limits",
            "Respect explicit user limits without omitting necessary explanations and caveats.",
            "Explain each material point once.",
        ],
    },
    PromptContract {
        id: "no-deferred-answer-expansion",
        scope: PromptScope::LocalPolicyAndFallback,
        expectation: AnchorExpectation::None,
        anchors: &["expand when the user requests it"],
    },
    PromptContract {
        id: "avoid-overengineering",
        scope: PromptScope::LocalPolicyAndFallback,
        expectation: AnchorExpectation::All,
        anchors: &[
            "Implement the smallest coherent change that fully satisfies the requested behavior.",
            "Before adding a mechanism, establish from relevant source evidence the concrete missing capability and why existing abstractions are insufficient.",
            "avoid unrelated refactors, renames, file moves, and dependencies.",
        ],
    },
    PromptContract {
        id: "nearest-sufficient-completion",
        scope: PromptScope::FallbackAndBundled,
        expectation: AnchorExpectation::Any,
        anchors: &[
            "Once all requested changes and affected validation pass, inspect the diff and deliver the result without repeating passing checks.",
        ],
    },
    PromptContract {
        id: "user-work-protection",
        scope: PromptScope::FallbackAndBundled,
        expectation: AnchorExpectation::Any,
        anchors: &[
            "first protect user work",
            "Existing and newly observed changes belong to the user",
        ],
    },
    PromptContract {
        id: "patch-is-not-validation",
        scope: PromptScope::FallbackAndBundled,
        expectation: AnchorExpectation::Any,
        anchors: &[
            "Patch success means the patch applied",
            "Patch success proves only that the patch applied",
        ],
    },
    PromptContract {
        id: "concurrent-edit-convergence",
        scope: PromptScope::FallbackAndBundled,
        expectation: AnchorExpectation::Any,
        anchors: &["Concurrent Edit Convergence", "concurrent changes"],
    },
    PromptContract {
        id: "implementation-self-repair",
        scope: PromptScope::FallbackAndBundled,
        expectation: AnchorExpectation::Any,
        anchors: &[
            "implementation self-repair is mandatory",
            "Implementation self-repair is required",
        ],
    },
    PromptContract {
        id: "scoped-nearest-sufficient-validation",
        scope: PromptScope::FallbackAndBundled,
        expectation: AnchorExpectation::Any,
        anchors: &[
            "nearest sufficient tests or checks",
            "nearest sufficient validation",
        ],
    },
    PromptContract {
        id: "risk-based-validation-and-tool-fallbacks",
        scope: PromptScope::LocalPolicyAndFallback,
        expectation: AnchorExpectation::All,
        anchors: &[
            "Choose validation by the changed contract and plausible failure modes, not the number or type of files touched.",
            "Use inspection or a direct assertion when it establishes the relevant property; run targeted tests when correctness depends on execution.",
            "Add consumer or integration tests only for a distinct affected risk not already covered.",
            "Before launching validation, account for compilation, helper binaries, setup, and execution cost; a narrow test filter does not imply a cheap check.",
            "Do not run a check for every touched file or layer.",
            "Broaden validation only when required or when a concrete unresolved risk makes existing evidence insufficient; state the risk and how the added check addresses it.",
            "If the cost is disproportionate, use a cheaper valid proof or report the remaining uncertainty, not a stronger claim.",
            "Respect explicit scope limits and stop at the nearest sufficient validation.",
            "Continue independent work while builds or other long commands run. Host-owned waits may hold for up to ten seconds and must return immediately on interruption, explicit yield, or completion.",
            "use `context_checkpoint` when it would remove substantial consumed output or preserve state needed for continuation",
            "using a concise evidence summary if the tool is unavailable",
            "Do not checkpoint merely because a phase ended.",
            "Preserve unresolved failures, active edit context, constraints, and essential validation evidence.",
            "Triage all reported issues together before repair edits.",
            "Fully diagnose issues caused by the requested changes or necessary to complete required validation; report unrelated issues and their validation impact without expanding the investigation.",
            "Render identifiers and counts from retained records, using an inventory/report tool when available",
        ],
    },
    PromptContract {
        id: "no-unconditional-validation-limits-or-tool-requirements",
        scope: PromptScope::LocalPolicyAndFallback,
        expectation: AnchorExpectation::None,
        anchors: &[
            "full suite only when explicitly required",
            "Do not run additional validation solely for extra confidence.",
            "Run those checks only when required",
            "At a completed phase, use `context_checkpoint` for",
            "use `context_checkpoint` when available",
            "diagnose all reported issues before making repair edits",
            "with the available inventory/report tool and link its report",
            "Every validation test must assert",
            "Repair weak tests covering the changed behavior or blocking its validation.",
            "identify the concrete missing capability and explain",
            "Prefer existing code mode",
            "Host-owned waits may hold for up to five minutes",
            "For changed behavior and mechanical refactors, run the tests that exercise the new or preserved contract and its affected consumers.",
            "Do not substitute compilation, formatting, linting, static analysis, inspection, or unrelated tests for behavior validation.",
        ],
    },
    PromptContract {
        id: "validation-reuse-and-authorized-publishing",
        scope: PromptScope::LocalPolicyAndFallback,
        expectation: AnchorExpectation::All,
        anchors: &[
            "A required broader check may replace a redundant narrower one unless independently required",
            "clippy is required, it can replace cargo check only for identical packages, targets, features, toolchain, environment, and revision",
            "only if cargo check is not independently required",
            "They do not expire merely because of a new turn, handoff, or unrelated edit.",
            "Refresh only evidence affected by changed inputs, contradictions, incompleteness, or explicit freshness requirements.",
            "Retry a failed operation only when changed inputs, new evidence, a documented retry policy, or an explicit task requirement justify it",
            "When publishing is authorized, publish only after the source state is fixed and required validation is complete.",
        ],
    },
    PromptContract {
        id: "economical-tool-use",
        scope: PromptScope::LocalPolicyAndFallback,
        expectation: AnchorExpectation::All,
        anchors: &[
            "Match discovery to the request",
            "start with the smallest likely owner",
            "Before optional discovery, planning, or validation, identify the material uncertainty and how the result could change the next action",
            "otherwise skip it",
            "This is an internal decision, not a narrated checklist or extra tool call.",
            "Never skip required validation to save time.",
            "Read complete useful regions",
            "retain a recovery route for oversized output",
            "do not mistake recovery for fresh evidence",
            "Disclose unresolved staleness.",
            "Use asynchronous sessions for long or interactive commands",
        ],
    },
    PromptContract {
        id: "general-repository-discovery",
        scope: PromptScope::LocalPolicy,
        expectation: AnchorExpectation::All,
        anchors: &[
            "inspect named paths directly",
            "scoped rg searches",
            "identify the material uncertainty and how the result could change the next action; otherwise skip it",
            "Reuse current reads, exact values, enumerations, agent results, and passing checks.",
        ],
    },
    PromptContract {
        id: "general-tool-discipline",
        scope: PromptScope::LocalPolicyAndFallback,
        expectation: AnchorExpectation::All,
        anchors: &[
            "Batch independent calls with bounded tool-native concurrency",
            "await every started call, and inspect every result and exit status",
            "Batch independent calls with bounded tool-native concurrency",
            "Return to the model for decisions or dependencies, not between already-planned calls.",
            "Return to the model for decisions or dependencies",
            "Finish edits before their checks.",
            "Serialize actual shared-resource conflicts",
            "not whole categories of independent work",
            "Change locking only with evidence of conflict or unnecessary exclusion.",
            "Prefer extending the existing owning execution mechanism over parallel orchestration unless it is demonstrably insufficient.",
            "Once all requested changes and affected validation pass, inspect the diff and deliver the result without repeating passing checks.",
        ],
    },
    PromptContract {
        id: "scoped-autonomy",
        scope: PromptScope::LocalPolicyAndFallback,
        expectation: AnchorExpectation::All,
        anchors: &[
            "Read every applicable AGENTS.md",
            "fresh content in context counts as read",
            "Retrieve missing or potentially changed instructions",
            "Resolve conflicts by authority, scope, and explicit supersession.",
            "Ask only when conflicting requirements or an essential user-only fact remain unresolved after inspecting available evidence.",
            "State material assumptions, keep affected conclusions conditional",
            "Incorporate new user corrections before the next dependent action",
            "a status question does not cancel ongoing work.",
            "Stage, commit, push, publish, deploy, install, restart, contact third parties, delete data, change external state, or rebuild or activate the installed application only when authorized.",
            "Do not request authorization already provided.",
            "autonomous within the requested scope",
        ],
    },
    PromptContract {
        id: "superseded-autonomy-and-batching-rules",
        scope: PromptScope::LocalPolicyAndFallback,
        expectation: AnchorExpectation::None,
        anchors: &[
            "Get permission to publish",
            "stop on incompatible equal-authority requirements",
            "Batch known independent reads and final checks in one tool round",
        ],
    },
    PromptContract {
        id: "general-change-discipline",
        scope: PromptScope::LocalPolicy,
        expectation: AnchorExpectation::All,
        anchors: &[
            "Before editing, identify the behavior's owner, intended observable change, preserved invariants, affected files and contracts, and focused validation.",
            "Revise this prediction as evidence changes.",
            "retain them in the final handoff",
            "Update affected callers, schemas, generated representations, persistence/migrations, compatibility paths, and tests.",
            "identify the material uncertainty and how the result could change the next action; otherwise skip it",
            "Run validation required by the user or repository.",
            "Every test relied upon as evidence for the changed behavior must assert an expected observable result and fail for at least one plausible incorrect implementation of that behavior.",
            "Exercise the intended path, including required rejection and absent-side-effect cases.",
            "Strengthen weak tests relied upon to validate the changed behavior when necessary",
            "including required rejection and absent-side-effect cases",
            "Strengthen weak tests relied upon to validate the changed behavior when necessary to make that validation meaningful.",
            "Report unrelated weaknesses encountered without starting a broader test audit.",
            "Validate the final relevant source state",
            "applying the evidence lifecycle rules to source, dependency, lockfile, configuration, and feature changes",
            "Do not substitute compilation, formatting, linting, static analysis, inspection, or unrelated tests for behavior validation when the claim requires execution.",
            "Verify documentation claims against implementation or referenced sources.",
            "Prefer the least costly check that proves the relevant contract.",
            "distinguish mechanism proof from model-driven outcomes",
            "Replaying valid evidence without its producer saves execution",
            "does not save that model request",
            "Measure complete-turn time, model handoffs, output/recovery cost, cancellation responsiveness, and task success",
            "Prompt contract tests establish guidance delivery, not model compliance.",
            "validation and what it proved, failures, unvalidated behavior",
            "rerun only affected checks that failed or whose prior results were invalidated by relevant changes",
            "report any partial, blocked, or unverified result",
        ],
    },
    PromptContract {
        id: "workspace-ownership",
        scope: PromptScope::LocalPolicy,
        expectation: AnchorExpectation::All,
        anchors: &[
            "Existing and newly observed changes belong to the user",
            "When edits overlap, preserve independent changes",
            "When edits overlap, preserve independent changes and combine compatible behavior against the requested contract.",
            "do not discard unrelated changes",
            "Do not hard-code machine-specific paths.",
        ],
    },
    PromptContract {
        id: "general-global-prompt",
        scope: PromptScope::LocalPolicy,
        expectation: AnchorExpectation::None,
        anchors: &["KD4", "repository source map", "official session roots"],
    },
    PromptContract {
        id: "environment-neutral-global-prompt",
        scope: PromptScope::LocalPolicy,
        expectation: AnchorExpectation::None,
        anchors: &[r"C:\Users\", "/Users/", "/home/"],
    },
    PromptContract {
        id: "live-tool-contract-ownership",
        scope: PromptScope::FallbackAndGpt52,
        expectation: AnchorExpectation::None,
        anchors: &[
            r#"{"command":["apply_patch""#,
            "## apply_patch",
            "This is a FREEFORM tool",
            "## `update_plan`",
            "(`pending`, `in_progress`, or `completed`)",
            "Do not jump an item from pending to completed",
        ],
    },
];

fn prompts_for_scope(scope: PromptScope, response: &ModelsResponse) -> Vec<(String, &str)> {
    let model = |slug: &str| {
        response
            .models
            .iter()
            .find(|model| model.slug == slug)
            .unwrap_or_else(|| panic!("bundled models.json should contain {slug}"))
    };
    match scope {
        PromptScope::FallbackAndBundled => {
            let mut prompts = vec![("fallback".to_string(), BASE_INSTRUCTIONS_DEFAULT)];
            for model in &response.models {
                prompts.push((
                    format!("{}.base_instructions", model.slug),
                    &model.base_instructions,
                ));
                if let Some(template) = model
                    .model_messages
                    .as_ref()
                    .and_then(|messages| messages.instructions_template.as_deref())
                {
                    prompts.push((format!("{}.instructions_template", model.slug), template));
                }
            }
            prompts
        }
        PromptScope::LocalPolicyAndFallback => {
            std::iter::once(("fallback".to_string(), BASE_INSTRUCTIONS_DEFAULT))
                .chain(
                    LOCAL_PROMPT_POLICY_SLUGS
                        .iter()
                        .map(|slug| ((*slug).to_string(), model(slug).base_instructions.as_str())),
                )
                .collect()
        }
        PromptScope::LocalPolicy => LOCAL_PROMPT_POLICY_SLUGS
            .iter()
            .map(|slug| ((*slug).to_string(), model(slug).base_instructions.as_str()))
            .collect(),
        PromptScope::FallbackAndGpt52 => vec![
            ("fallback".to_string(), BASE_INSTRUCTIONS_DEFAULT),
            (
                "gpt-5.2".to_string(),
                model("gpt-5.2").base_instructions.as_str(),
            ),
        ],
    }
}

#[test]
fn resolved_prompts_satisfy_named_contract_registry() {
    // This checks authored wording after resolution, not whether an agent obeys it.
    let response = crate::bundled_models_response().expect("bundled models.json should parse");
    assert!(!response.models.is_empty());

    for contract in PROMPT_CONTRACTS {
        for (label, prompt) in prompts_for_scope(contract.scope, &response) {
            let matches = contract
                .anchors
                .iter()
                .map(|anchor| prompt.contains(anchor))
                .collect::<Vec<_>>();
            let passed = match contract.expectation {
                AnchorExpectation::Any => matches.iter().any(|matched| *matched),
                AnchorExpectation::All => matches.iter().all(|matched| *matched),
                AnchorExpectation::None => matches.iter().all(|matched| !*matched),
            };
            assert!(
                passed,
                "prompt {label} violated contract {}: relevant anchors {:?}",
                contract.id,
                contract
                    .anchors
                    .iter()
                    .zip(&matches)
                    .filter_map(|(anchor, matched)| {
                        let relevant = match contract.expectation {
                            AnchorExpectation::Any | AnchorExpectation::All => !matched,
                            AnchorExpectation::None => *matched,
                        };
                        relevant.then_some(anchor)
                    })
                    .collect::<Vec<_>>()
            );
        }
    }
}

#[test]
fn resolved_prompts_prioritize_complete_answers() {
    let response = crate::bundled_models_response().expect("bundled models.json should parse");
    for id in [
        "complete-substance-with-concise-wording",
        "no-deferred-answer-expansion",
    ] {
        let contract = PROMPT_CONTRACTS
            .iter()
            .find(|contract| contract.id == id)
            .expect("answer completeness contract should be registered");
        let prompts = prompts_for_scope(contract.scope, &response);
        assert!(!prompts.is_empty());
        for (label, prompt) in prompts {
            for anchor in contract.anchors {
                let expected = match contract.expectation {
                    AnchorExpectation::All => true,
                    AnchorExpectation::None => false,
                    AnchorExpectation::Any => panic!("completeness requires every anchor"),
                };
                assert_eq!(
                    prompt.contains(anchor),
                    expected,
                    "prompt {label} violated contract {id}: {anchor}"
                );
            }
        }
    }
}

#[test]
fn local_policy_models_use_one_canonical_prompt_within_size_limit() {
    const PROMPT_TOKEN_LIMIT: usize = 4_000;
    let response = crate::bundled_models_response().expect("bundled models.json should parse");
    let prompts = LOCAL_PROMPT_POLICY_SLUGS
        .iter()
        .map(|slug| {
            response
                .models
                .iter()
                .find(|model| model.slug == *slug)
                .unwrap_or_else(|| panic!("bundled models should contain {slug}"))
                .base_instructions
                .as_str()
        })
        .collect::<Vec<_>>();

    assert!(prompts.iter().all(|prompt| *prompt == prompts[0]));
    assert_eq!(prompts[0], BASE_INSTRUCTIONS_DEFAULT.trim());
    // Enforce the budget with the existing model tokenizer, not the conservative
    // lexical estimate used to bound arbitrary tool output.
    let tokens = model_token_count(prompts[0]);
    assert!(
        tokens <= PROMPT_TOKEN_LIMIT,
        "default.md uses {tokens} o200k tokens; limit is {PROMPT_TOKEN_LIMIT}"
    );
}

#[test]
fn bundled_local_policy_catalog_defers_prompt_to_local_policy() {
    let catalog: serde_json::Value = serde_json::from_str(include_str!("../models.json"))
        .expect("bundled models.json should parse");
    let models = catalog["models"]
        .as_array()
        .expect("bundled models.json should contain a models array");

    for slug in LOCAL_PROMPT_POLICY_SLUGS {
        let model = models
            .iter()
            .find(|model| model["slug"].as_str() == Some(slug))
            .unwrap_or_else(|| panic!("bundled models.json should contain {slug}"));

        assert_eq!(
            model["base_instructions"].as_str(),
            Some(""),
            "{slug} should defer prompt content to its registered local policy"
        );
    }
}

#[test]
fn bundled_local_policy_models_match_prompt_policy_registration() {
    let response = crate::bundled_models_response().expect("bundled models.json should parse");
    let bundled_slugs = response
        .models
        .iter()
        .map(|model| model.slug.as_str())
        .filter(|slug| {
            matches!(
                *slug,
                "gpt-6-astra" | "gpt-6-sol" | "gpt-5.5" | "gpt-5.4" | "gpt-5.4-mini" | "gpt-5.2"
            ) || slug.starts_with("gpt-5.6-")
        })
        .collect::<BTreeSet<_>>();
    let registered_slugs = LOCAL_PROMPT_POLICY_SLUGS
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();

    assert_eq!(bundled_slugs, registered_slugs);
}

#[test]
fn behavior_identical_instruction_templates_are_removed() {
    let response = crate::bundled_models_response().expect("bundled models.json should parse");
    for slug in LOCAL_PROMPT_POLICY_SLUGS.iter().copied() {
        let model = response
            .models
            .iter()
            .find(|model| model.slug == slug)
            .unwrap_or_else(|| panic!("bundled models.json should contain {slug}"));
        assert!(
            model
                .model_messages
                .as_ref()
                .is_none_or(|messages| messages.instructions_template.is_none()),
            "{slug} should not duplicate base_instructions in instructions_template"
        );
        assert_eq!(model.get_model_instructions(None), model.base_instructions);
        for personality in [
            Personality::None,
            Personality::Friendly,
            Personality::Pragmatic,
        ] {
            assert_eq!(
                model.get_model_instructions(Some(personality)),
                model.base_instructions,
                "{slug} should preserve base rendering for {personality}"
            );
        }
        assert!(!model.supports_personality());
    }
}
