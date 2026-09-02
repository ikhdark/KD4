use codex_protocol::config_types::Personality;
use codex_protocol::models::BASE_INSTRUCTIONS_DEFAULT;
use codex_protocol::openai_models::ModelsResponse;
use pretty_assertions::assert_eq;
use std::collections::BTreeSet;

use crate::prompt_resolver::GPT_5_6_PROMPT_POLICY_SLUGS;

#[derive(Clone, Copy)]
enum PromptScope {
    FallbackAndBundled,
    Gpt56AndFallback,
    Gpt56,
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
        ],
    },
    PromptContract {
        id: "economical-tool-use",
        scope: PromptScope::Gpt56AndFallback,
        expectation: AnchorExpectation::All,
        anchors: &[
            "Match tool work to the complexity of the user's request.",
            "inspect the smallest likely source first",
            "Do not recover omitted output when a narrower reread can answer the question.",
            "Use asynchronous sessions only when a command is expected to outlive the initial tool wait or requires interaction.",
        ],
    },
    PromptContract {
        id: "general-repository-discovery",
        scope: PromptScope::Gpt56,
        expectation: AnchorExpectation::All,
        anchors: &[
            "Prefer fast, scoped search",
            "Use repository-provided discovery aids when available.",
            "Do not repeat an unchanged lookup.",
        ],
    },
    PromptContract {
        id: "general-tool-discipline",
        scope: PromptScope::Gpt56,
        expectation: AnchorExpectation::All,
        anchors: &[
            "Group independent tool work.",
            "Stop investigating when the available evidence is sufficient.",
        ],
    },
    PromptContract {
        id: "scoped-autonomy",
        scope: PromptScope::Gpt56,
        expectation: AnchorExpectation::All,
        anchors: &[
            "Read every applicable AGENTS.md",
            "Resolve conflicts by authority and scope",
            "Ask only when evidence leaves choices",
            "choose a safe, reversible option",
            "finish or persist does not expand authorization",
        ],
    },
    PromptContract {
        id: "general-change-discipline",
        scope: PromptScope::Gpt56,
        expectation: AnchorExpectation::All,
        anchors: &[
            "Before editing, identify the relevant owner or contract",
            "direct callers and consumers",
            "duplicate or generated representations",
            "Resolve each category with a source location or scoped search showing no match",
            "Test at the narrowest stable boundary",
            "actually executes at least one relevant test through the changed path",
            "Do not claim completion",
        ],
    },
    PromptContract {
        id: "workspace-ownership",
        scope: PromptScope::Gpt56,
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
        scope: PromptScope::Gpt56,
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
        scope: PromptScope::Gpt56,
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
        PromptScope::Gpt56AndFallback => {
            std::iter::once(("fallback".to_string(), BASE_INSTRUCTIONS_DEFAULT))
                .chain(
                    GPT_5_6_PROMPT_POLICY_SLUGS
                        .iter()
                        .map(|slug| ((*slug).to_string(), model(slug).base_instructions.as_str())),
                )
                .collect()
        }
        PromptScope::Gpt56 => GPT_5_6_PROMPT_POLICY_SLUGS
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
                "prompt {label} violated contract {} with anchors {:?}",
                contract.id, contract.anchors
            );
        }
    }
}

#[test]
fn gpt_5_6_family_uses_one_canonical_prompt_within_size_limit() {
    const PROMPT_CHAR_LIMIT: usize = 6_000;
    let response = crate::bundled_models_response().expect("bundled models.json should parse");
    let prompts = GPT_5_6_PROMPT_POLICY_SLUGS
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
    assert!(prompts[0].chars().count() < PROMPT_CHAR_LIMIT);
}

#[test]
fn bundled_gpt_5_6_catalog_defers_prompt_to_local_policy() {
    let catalog: serde_json::Value = serde_json::from_str(include_str!("../models.json"))
        .expect("bundled models.json should parse");
    let models = catalog["models"]
        .as_array()
        .expect("bundled models.json should contain a models array");

    for slug in GPT_5_6_PROMPT_POLICY_SLUGS {
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
fn bundled_gpt_5_6_models_match_prompt_policy_registration() {
    let response = crate::bundled_models_response().expect("bundled models.json should parse");
    let bundled_slugs = response
        .models
        .iter()
        .map(|model| model.slug.as_str())
        .filter(|slug| slug.starts_with("gpt-5.6-"))
        .collect::<BTreeSet<_>>();
    let registered_slugs = GPT_5_6_PROMPT_POLICY_SLUGS
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();

    assert_eq!(bundled_slugs, registered_slugs);
}

#[test]
fn behavior_identical_instruction_templates_are_removed() {
    let response = crate::bundled_models_response().expect("bundled models.json should parse");
    for slug in GPT_5_6_PROMPT_POLICY_SLUGS
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
