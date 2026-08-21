use codex_protocol::openai_models::ModelInfo;

const GPT_5_6_BASE_INSTRUCTIONS: &str = include_str!("../prompts/gpt-5.6.md");
const GPT_5_6_SLUGS: &[&str] = &["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"];

pub(crate) fn apply_gpt_5_6_prompt(models: &mut [ModelInfo]) {
    for model in models {
        if GPT_5_6_SLUGS.contains(&model.slug.as_str()) {
            model.base_instructions = GPT_5_6_BASE_INSTRUCTIONS.trim().to_string();
            clear_instruction_template(model);
        }
    }
}

fn clear_instruction_template(model: &mut ModelInfo) {
    if let Some(messages) = &mut model.model_messages {
        messages.instructions_template = None;
        messages.instructions_variables = None;
        if messages.approvals.is_none() {
            model.model_messages = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::openai_models::ApprovalMessages;
    use codex_protocol::openai_models::ModelInstructionsVariables;
    use codex_protocol::openai_models::ModelMessages;

    const GPT_5_6_PROMPT_CHAR_LIMIT: usize = 12_000;

    #[test]
    fn overrides_remote_prompt_and_preserves_approval_messages() {
        let approvals = ApprovalMessages {
            on_request: Some("request approval".to_string()),
            on_request_auto_review: Some("auto-review approval".to_string()),
        };
        let mut model = crate::model_info::model_info_from_slug("gpt-5.6-sol");
        model.base_instructions = "remote prompt".to_string();
        model.model_messages = Some(ModelMessages {
            instructions_template: Some("remote template".to_string()),
            instructions_variables: Some(ModelInstructionsVariables {
                personality_default: Some("default".to_string()),
                personality_friendly: Some("friendly".to_string()),
                personality_pragmatic: Some("pragmatic".to_string()),
            }),
            approvals: Some(approvals.clone()),
        });

        apply_gpt_5_6_prompt(std::slice::from_mut(&mut model));

        assert_eq!(model.base_instructions, GPT_5_6_BASE_INSTRUCTIONS.trim());
        assert_eq!(
            model.model_messages,
            Some(ModelMessages {
                instructions_template: None,
                instructions_variables: None,
                approvals: Some(approvals),
            })
        );
    }

    #[test]
    fn gpt_5_6_family_shares_prompt_under_12k_chars() {
        let response = crate::bundled_models_response().expect("bundled models should parse");
        let prompts = GPT_5_6_SLUGS
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

        let prompt_chars = prompts[0].chars().count();

        assert!(prompts.iter().all(|prompt| *prompt == prompts[0]));
        assert!(
            prompt_chars < GPT_5_6_PROMPT_CHAR_LIMIT,
            "the local GPT-5.6 prompt has {prompt_chars} characters; limit is {GPT_5_6_PROMPT_CHAR_LIMIT}"
        );
    }

    #[test]
    fn gpt_5_6_prompt_bounds_counted_audits() {
        let response = crate::bundled_models_response().expect("bundled models should parse");

        for slug in GPT_5_6_SLUGS {
            let prompt = &response
                .models
                .iter()
                .find(|model| model.slug == *slug)
                .unwrap_or_else(|| panic!("bundled models should contain {slug}"))
                .base_instructions;

            for rule in [
                "request a specific finding count",
                "responsible producer",
                "reachable consumer or user-visible effect",
                "stop broad searches",
                "explicitly exhaustive scope",
            ] {
                assert!(
                    prompt.contains(rule),
                    "{slug} should include bounded audit convergence rule {rule:?}"
                );
            }
        }
    }

    #[test]
    fn gpt_5_6_prompt_reuses_lookup_results_until_an_explicit_change() {
        let response = crate::bundled_models_response().expect("bundled models should parse");

        for slug in GPT_5_6_SLUGS {
            let prompt = &response
                .models
                .iter()
                .find(|model| model.slug == *slug)
                .unwrap_or_else(|| panic!("bundled models should contain {slug}"))
                .base_instructions;

            for rule in [
                "Resolve candidate paths",
                "rg --files",
                "Repo Atlas",
                "repository source map",
                "Repeat a missing-file, symbol, configuration, or test lookup only after",
                "a relevant file changes",
                "a new routing source is found",
                "supplies a new name or path",
            ] {
                assert!(
                    prompt.contains(rule),
                    "{slug} should include negative-result reuse rule {rule:?}"
                );
            }
        }
    }

    #[test]
    fn gpt_5_6_prompt_batches_evidence_and_stops_on_fixed_points() {
        let response = crate::bundled_models_response().expect("bundled models should parse");

        for slug in GPT_5_6_SLUGS {
            let prompt = &response
                .models
                .iter()
                .find(|model| model.slug == *slug)
                .unwrap_or_else(|| panic!("bundled models should contain {slug}"))
                .base_instructions;

            for rule in [
                "Before each tool call, group identified independent operations",
                "names a new path, symbol, contract, or test",
                "contradicts the current conclusion",
                "every grounding category above is resolved",
                "no inspected source contradicts the conclusion",
            ] {
                assert!(
                    prompt.contains(rule),
                    "{slug} should include evidence-loop rule {rule:?}"
                );
            }
        }
    }

    #[test]
    fn gpt_5_6_prompt_resolves_scoped_instructions_and_limits_questions() {
        let response = crate::bundled_models_response().expect("bundled models should parse");

        for slug in GPT_5_6_SLUGS {
            let prompt = &response
                .models
                .iter()
                .find(|model| model.slug == *slug)
                .unwrap_or_else(|| panic!("bundled models should contain {slug}"))
                .base_instructions;

            for rule in [
                "Repository and skill instructions retain the authority of their source",
                "Read every applicable `AGENTS.md`",
                "Resolve conflicts by authority and scope",
                "two same-authority instructions require incompatible actions",
                "Ask only when repository and tool evidence leaves choices",
                "choose the reversible option",
                "finish or persist does not expand authorization",
            ] {
                assert!(
                    prompt.contains(rule),
                    "{slug} should include scoped-autonomy rule {rule:?}"
                );
            }
        }
    }

    #[test]
    fn gpt_5_6_prompt_defines_grounding_validation_and_completion_gates() {
        let response = crate::bundled_models_response().expect("bundled models should parse");

        for slug in GPT_5_6_SLUGS {
            let prompt = &response
                .models
                .iter()
                .find(|model| model.slug == *slug)
                .unwrap_or_else(|| panic!("bundled models should contain {slug}"))
                .base_instructions;

            for rule in [
                "Resolve each category with a source location or scoped search showing no match",
                "nearest sufficient validation is the repository-named check",
                "command selects at least one test exercising that contract",
                "When editing code, add or update in the same change the test",
                "test must fail without the code change and pass with it",
                "answer to each requested question backed by inspected source locations",
                "every affected representation identified during grounding",
                "named missing permission, unresolved incompatible outcome, or external failure",
            ] {
                assert!(
                    prompt.contains(rule),
                    "{slug} should include objective completion gate {rule:?}"
                );
            }
        }
    }

    #[test]
    fn gpt_5_6_prompt_preserves_concurrent_work_and_distinguishes_codex_homes() {
        let response = crate::bundled_models_response().expect("bundled models should parse");

        for slug in GPT_5_6_SLUGS {
            let prompt = &response
                .models
                .iter()
                .find(|model| model.slug == *slug)
                .unwrap_or_else(|| panic!("bundled models should contain {slug}"))
                .base_instructions;

            for rule in [
                "Preserve concurrent work",
                "Compare overlapping versions once",
                "every affected contract and test to remain satisfied",
                r"`C:\Users\kuh\Desktop\LOCAL-KD` is the fork home",
                r"`C:\Users\kuh\.codex` is the official upstream home",
                r"Use `C:\Users\kuh\Desktop\LOCAL-KD\sessions` for fork rollouts",
                r"`C:\Users\kuh\.codex\sessions` for official upstream rollouts",
            ] {
                assert!(
                    prompt.contains(rule),
                    "{slug} should include workspace ownership rule {rule:?}"
                );
            }
        }
    }
}
