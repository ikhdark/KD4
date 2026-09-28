use codex_protocol::config_types::Personality;
use codex_protocol::models::BASE_INSTRUCTIONS_DEFAULT;
use codex_protocol::openai_models::ModelsResponse;
use codex_utils_output_truncation::model_token_count;
use pretty_assertions::assert_eq;
use std::collections::BTreeSet;

use crate::prompt_resolver::LOCAL_PROMPT_POLICY_SLUGS;

#[derive(Clone, Copy)]
enum PromptScope {
    LocalPolicyAndFallback,
}

#[derive(Clone, Copy)]
enum AnchorExpectation {
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
        id: "instruction-discovery",
        scope: PromptScope::LocalPolicyAndFallback,
        expectation: AnchorExpectation::All,
        anchors: &[
            "Supplied AGENTS.md content counts as read",
            "Check for missing nested instructions only along paths you will touch",
            "do not probe ancestors above a supplied instruction root",
            "Refresh instructions only after evidence of change",
        ],
    },
    PromptContract {
        id: "scoped-autonomy",
        scope: PromptScope::LocalPolicyAndFallback,
        expectation: AnchorExpectation::All,
        anchors: &[
            "autonomous within the requested scope",
            "Honor explicit read-only",
            "A status question does not cancel ongoing work",
            "only when authorized",
            "Do not request authorization already provided",
        ],
    },
    PromptContract {
        id: "change-discipline",
        scope: PromptScope::LocalPolicyAndFallback,
        expectation: AnchorExpectation::All,
        anchors: &[
            "concrete missing capability",
            "why existing abstractions are insufficient",
            "Read the complete enclosing function, type, or configuration unit before changing it",
            "End-to-end wiring is mandatory",
            "Existing and newly observed changes belong to the user",
            "Preserve independent changes and combine compatible behavior",
        ],
    },
    PromptContract {
        id: "bounded-discovery-and-reuse",
        scope: PromptScope::LocalPolicyAndFallback,
        expectation: AnchorExpectation::All,
        anchors: &[
            "smallest likely owner and scoped rg searches",
            "Budget combined batched output",
            "Do not repeat a full-tree status",
            "Recover missing ranges from retained artifacts before rerunning producers",
            "Explicit full-read requests require complete coverage",
            "Source or dependency changes invalidate only overlapping validation",
        ],
    },
    PromptContract {
        id: "validation-contract",
        scope: PromptScope::LocalPolicyAndFallback,
        expectation: AnchorExpectation::All,
        anchors: &[
            "least costly check that proves the changed contract",
            "Each relied-upon test must assert an observable result",
            "fail for a plausible incorrect implementation",
            "Exercise relevant real environment, filesystem, session, or timing state",
            "Run a full suite only when explicitly required",
            "Let valid, progressing validation finish",
            "rerun only affected checks",
            "Do not weaken assertions",
            "Report unrelated failures",
        ],
    },
    PromptContract {
        id: "tool-lifecycle",
        scope: PromptScope::LocalPolicyAndFallback,
        expectation: AnchorExpectation::All,
        anchors: &[
            "await all started calls and inspect every result and exit status",
            "Cargo commands sharing a target directory",
            "Finish edits before their checks",
            "resume existing operations",
            "stop for steering, cancellation, or input",
            "Continue independent work while long commands run",
        ],
    },
    PromptContract {
        id: "complete-substance-with-concise-wording",
        scope: PromptScope::LocalPolicyAndFallback,
        expectation: AnchorExpectation::All,
        anchors: &[
            "Explain each point once",
            "Use final for a self-contained handoff",
            "Include requested substance now rather than deferring it behind an offer",
            "Do not claim unperformed actions",
        ],
    },
    PromptContract {
        id: "completion-and-evidence",
        scope: PromptScope::LocalPolicyAndFallback,
        expectation: AnchorExpectation::All,
        anchors: &[
            "inspect the complete affected diff",
            "match the original request and corrections against current evidence",
            "Continue until all requested work and required validation are complete",
            "Preserve progress across context windows",
            "finish independent authorized work first",
            "Prompt tests prove guidance delivery, not model compliance",
            "Distinguish observations, inferences, stale evidence, and unavailable evidence",
        ],
    },
    PromptContract {
        id: "no-deferred-answer-expansion",
        scope: PromptScope::LocalPolicyAndFallback,
        expectation: AnchorExpectation::None,
        anchors: &[
            "expand when the user requests it",
        ],
    },
    PromptContract {
        id: "environment-neutral-prompt",
        scope: PromptScope::LocalPolicyAndFallback,
        expectation: AnchorExpectation::None,
        anchors: &[
            "C:\\Users\\",
            "/Users/",
            "/home/",
            "Read every applicable AGENTS.md from root",
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
        PromptScope::LocalPolicyAndFallback => {
            std::iter::once(("fallback".to_string(), BASE_INSTRUCTIONS_DEFAULT))
                .chain(
                    LOCAL_PROMPT_POLICY_SLUGS
                        .iter()
                        .map(|slug| ((*slug).to_string(), model(slug).base_instructions.as_str())),
                )
                .collect()
        }

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
                            AnchorExpectation::All => !matched,
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
fn resolved_prompts_keep_live_tool_mechanics_owned_and_discovery_bounded() {
    let response = crate::bundled_models_response().expect("bundled models should parse");
    for (label, prompt) in prompts_for_scope(PromptScope::LocalPolicyAndFallback, &response) {
        for anchor in [
            "Use live schemas and advertised discovery routes",
            "Follow the tool's lifecycle and polling contract",
            "stop for steering, cancellation, or input",
            "Budget combined batched output",
            "Use path-scoped git status",
            "retain a broad inventory once only when needed",
        ] {
            assert!(
                prompt.contains(anchor),
                "prompt {label} is missing {anchor}"
            );
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
                "gpt-6-astra"
                    | "gpt-6-sol"
                    | "gpt-6-luna"
                    | "gpt-5.5"
                    | "gpt-5.4"
                    | "gpt-5.4-mini"
                    | "gpt-5.2"
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
