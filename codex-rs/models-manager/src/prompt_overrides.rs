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
    fn compact_prompt_is_shared_by_the_gpt_5_6_family() {
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

        assert!(prompts.iter().all(|prompt| *prompt == prompts[0]));
        assert!(
            prompts[0].chars().count() < 10_000,
            "the local GPT-5.6 prompt should remain compact"
        );
    }
}
