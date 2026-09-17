use codex_protocol::config_types::Personality;
use codex_protocol::models::BASE_INSTRUCTIONS_DEFAULT;
use codex_protocol::openai_models::ModelsResponse;
use codex_utils_output_truncation::approx_token_count;
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
        id: "requirement-fidelity-and-runtime-grounding",
        scope: PromptScope::LocalPolicyAndFallback,
        expectation: AnchorExpectation::All,
        anchors: &[
            "explicit constraints, prohibitions, and out-of-scope work until superseded",
            "Delegated objectives and write scopes bound a worker's task",
            "Read the complete enclosing function, type, or configuration unit before changing it",
            "Current file content overrides summaries, plans, and stale reads",
            "entrypoint through registration, dispatch, feature flags or config defaults to consumers",
            "Partial wiring is forbidden.",
            "Resolve contradictions by runtime reachability, ownership, and freshness.",
            "Cargo commands sharing a target directory",
            "do not evade denials",
            "combine compatible behavior and verify it as one runtime path",
            "Cancellation need not roll back effects.",
            "without weakening required invariants or assertions",
            "match every explicit requirement, prohibition, and preserved invariant to current evidence",
            "Ending a turn or exhausting a budget does not prove completion.",
        ],
    },
    PromptContract {
        id: "concise-progress-updates",
        scope: PromptScope::LocalPolicyAndFallback,
        expectation: AnchorExpectation::All,
        anchors: &[
            "Give one brief initial update before tools.",
            "keep commentary to one or two sentences about new findings, blockers, decisions, or results",
            "Avoid repeating plans, restating the task contract, or narrating routine tool calls.",
            "Preserve required updates and disclosures.",
            "Use final for a self-contained handoff.",
            "Do not claim actions or tests that did not occur.",
        ],
    },
    PromptContract {
        id: "avoid-overengineering",
        scope: PromptScope::LocalPolicyAndFallback,
        expectation: AnchorExpectation::All,
        anchors: &[
            "Implement the smallest coherent change that fully satisfies the requested behavior.",
            "avoid unrelated refactors, renames, file moves, dependencies, and redesigns.",
        ],
    },
    PromptContract {
        id: "nearest-sufficient-completion",
        scope: PromptScope::FallbackAndBundled,
        expectation: AnchorExpectation::Any,
        anchors: &["nearest sufficient completion point"],
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
            "Do not run additional validation solely for extra confidence.",
        ],
    },
    PromptContract {
        id: "validation-reuse-and-authorized-publishing",
        scope: PromptScope::LocalPolicyAndFallback,
        expectation: AnchorExpectation::All,
        anchors: &[
            "A required broader check may replace a redundant narrower check unless that narrower check is independently required.",
            "Run an earlier focused check when its result can change the next action or prevent expensive rework.",
            "When clippy is required, omit a preceding cargo check only if both cover the same packages, targets, features, toolchain, environment, and source revision, and cargo check is not independently required.",
            "Current reads, searches, agent results, and successful checks do not expire merely because of a new turn, handoff, or unrelated edit.",
            "Honor explicit freshness and repetition requirements.",
            "Repeat a failed operation only when relevant inputs changed, new evidence changes the approach, or a documented retry policy or explicit task requirement justifies repetition.",
            "When publishing is authorized, publish only after the source state is fixed and required validation is complete.",
        ],
    },
    PromptContract {
        id: "economical-tool-use",
        scope: PromptScope::LocalPolicyAndFallback,
        expectation: AnchorExpectation::All,
        anchors: &[
            "Match tool work to the complexity of the user's request",
            "inspect the smallest likely source first",
            "Do not recover omitted output when a narrower reread can answer the question.",
            "Use asynchronous sessions only when a command is expected to outlive the initial tool wait or requires interaction.",
        ],
    },
    PromptContract {
        id: "general-repository-discovery",
        scope: PromptScope::LocalPolicy,
        expectation: AnchorExpectation::All,
        anchors: &[
            "Inspect named implementation and contract paths directly.",
            "Use discovery only for missing information",
            "prefer scoped rg searches or repository discovery aids",
            "Do not repeat an unchanged lookup.",
        ],
    },
    PromptContract {
        id: "general-tool-discipline",
        scope: PromptScope::LocalPolicyAndFallback,
        expectation: AnchorExpectation::All,
        anchors: &[
            "Batch independent calls using the available tool-native concurrency mechanism when their contracts and execution resources permit it",
            "wait for every started call and inspect every result and exit status",
            "finish edits before checks that validate them",
            "Stop investigating when the available evidence is sufficient.",
        ],
    },
    PromptContract {
        id: "scoped-autonomy",
        scope: PromptScope::LocalPolicyAndFallback,
        expectation: AnchorExpectation::All,
        anchors: &[
            "Read every applicable AGENTS.md",
            "fresh content in context counts as read",
            "Retrieve missing or potentially changed instructions.",
            "Resolve conflicts by authority, scope, and explicit supersession.",
            "Ask when conflicting requirements or an essential missing fact cannot be resolved from available evidence.",
            "When proceeding under a material assumption, state it",
            "incorporate corrections before the next dependent action",
            "a status question does not cancel ongoing work",
            "Stage, commit, push, publish, deploy, install, restart, contact third parties, delete data, change external state, or rebuild or activate the installed application only when authorized.",
            "Do not request authorization already provided.",
            "autonomous within the requested scope",
            "Ask only about material requirements that remain unresolved after examining available evidence.",
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
            "Before editing, identify the behavior's owner, intended observable change, preserved invariants, likely files and why each must change, affected contracts, and focused validation.",
            "Revise that prediction as evidence changes.",
            "retain it in the final handoff",
            "Inspect affected callers, schemas",
            "duplicate or generated representations",
            "persistence/migrations, compatibility paths, and tests encoding old behavior",
            "Reuse evidence; resolve material uncertainty",
            "avoid checklist-only absence searches",
            "Run all validation explicitly required by the user or applicable repository instructions",
            "including a full suite only when either explicitly requires it.",
            "For every changed behavior, identify and run the existing test or tests that exercise that behavior.",
            "A test counts as validation only if at least one of its assertions would fail when the changed behavior is absent, produces the wrong result, or is not reached through the path the test is intended to exercise.",
            "If the existing tests would still pass under any of those failures, add or strengthen the smallest test necessary to make that failure observable.",
            "Every added or modified test must assert the intended result.",
            "Repair weak tests covering the changed behavior or blocking its validation.",
            "Report unrelated weaknesses encountered without starting a broader test audit.",
            "Validate every affected behavior after the final relevant implementation change.",
            "Changes to dependency manifests, lockfiles, build configuration, or feature flags invalidate prior dependency setup and validation evidence for the affected scope.",
            "Prefer the least costly check that proves the affected behavior and covers affected consumers.",
            "A result produced before a later change to that behavior or its exercised path does not validate the final state.",
            "Do not substitute compilation, formatting, linting, static analysis, code inspection, or unrelated passing tests for behavior validation.",
            "Run those only when required by the user, repository instructions, or the changed code's normal required validation.",
            "For documentation changes, verify factual claims against the implementation or referenced source and run documentation validation required by the repository.",
            "Preserve any diagnosis the user requested.",
            "the validation run for each changed behavior;",
            "what each validation proved;",
            "every failure;",
            "any changed behavior that remains unvalidated and why.",
            "Do not run additional validation solely for extra confidence.",
            "Reuse successful checks of the final source state",
            "Rerun only when relevant inputs changed, evidence is incomplete, or the user requires it.",
            "Once these conditions and user-required checks are satisfied, deliver the result",
            "Do not claim completion",
        ],
    },
    PromptContract {
        id: "workspace-ownership",
        scope: PromptScope::LocalPolicy,
        expectation: AnchorExpectation::All,
        anchors: &[
            "Existing and newly observed changes belong to the user",
            "Preserve concurrent work",
            "Compare overlapping versions once",
            "do not discard unrelated changes",
            "Do not hard-code machine-specific paths.",
        ],
    },
    PromptContract {
        id: "general-global-prompt",
        scope: PromptScope::LocalPolicy,
        expectation: AnchorExpectation::None,
        anchors: &[
            "KD4",
            "Repo Atlas",
            "repository source map",
            "official session roots",
        ],
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
fn local_policy_models_use_one_canonical_prompt_within_size_limit() {
    const PROMPT_TOKEN_LIMIT: usize = 10_000;
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
    let tokens = approx_token_count(prompts[0]);
    assert!(
        tokens <= PROMPT_TOKEN_LIMIT,
        "default.md uses approximately {tokens} tokens; limit is {PROMPT_TOKEN_LIMIT}"
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
        .filter(|slug| *slug == "gpt-6-astra" || slug.starts_with("gpt-5.6-"))
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
    for slug in LOCAL_PROMPT_POLICY_SLUGS
        .iter()
        .copied()
        .chain(std::iter::once("gpt-5.2"))
    {
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
