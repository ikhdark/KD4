use crate::model_info::clear_instruction_messages;
use codex_protocol::openai_models::ModelInfo;
use tracing::debug;

/// Authoritative registration for models that use the local GPT-5.6 prompt policy.
pub(crate) const GPT_5_6_PROMPT_POLICY_SLUGS: &[&str] =
    &["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PromptId {
    Catalog,
    ProtocolDefault,
    Gpt56,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PromptSource {
    Catalog,
    ProtocolDefault,
    LocalModelPolicy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PromptNormalization {
    Preserve,
    Trim,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ResolvedPrompt<'a> {
    pub id: PromptId,
    pub source: PromptSource,
    pub normalization: PromptNormalization,
    pub content: &'a str,
    pub clears_instruction_template: bool,
}

/// Resolve model instructions after catalog acquisition and before config overrides.
///
/// Catalog acquisition, local model policy, and user configuration are deliberately
/// separate phases. This resolver makes the winning authored prompt and its
/// normalization visible in one place for bundled, remote, and fallback models.
pub(crate) fn resolve_prompt<'a>(
    slug: &str,
    catalog_prompt: Option<&'a str>,
) -> ResolvedPrompt<'a> {
    if GPT_5_6_PROMPT_POLICY_SLUGS.contains(&slug) {
        return ResolvedPrompt {
            id: PromptId::Gpt56,
            source: PromptSource::LocalModelPolicy,
            normalization: PromptNormalization::Trim,
            content: codex_protocol::models::BASE_INSTRUCTIONS_DEFAULT.trim(),
            clears_instruction_template: true,
        };
    }

    if let Some(content) = catalog_prompt {
        return ResolvedPrompt {
            id: PromptId::Catalog,
            source: PromptSource::Catalog,
            normalization: PromptNormalization::Preserve,
            content,
            clears_instruction_template: false,
        };
    }

    ResolvedPrompt {
        id: PromptId::ProtocolDefault,
        source: PromptSource::ProtocolDefault,
        normalization: PromptNormalization::Preserve,
        content: codex_protocol::models::BASE_INSTRUCTIONS_DEFAULT,
        clears_instruction_template: false,
    }
}

pub(crate) fn apply_prompt_policy(models: &mut [ModelInfo]) {
    for model in models {
        let resolved = resolve_prompt(&model.slug, Some(&model.base_instructions));
        debug!(
            model_slug = %model.slug,
            prompt_id = ?resolved.id,
            prompt_source = ?resolved.source,
            prompt_normalization = ?resolved.normalization,
            "resolved model base instructions"
        );
        let local_policy = (resolved.source == PromptSource::LocalModelPolicy).then(|| {
            (
                resolved.content.to_string(),
                resolved.clears_instruction_template,
            )
        });
        if let Some((base_instructions, clears_instruction_template)) = local_policy {
            model.base_instructions = base_instructions;
            if clears_instruction_template {
                clear_instruction_messages(model);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::openai_models::ApprovalMessages;
    use codex_protocol::openai_models::ModelInstructionsVariables;
    use codex_protocol::openai_models::ModelMessages;
    use pretty_assertions::assert_eq;

    #[test]
    fn resolver_reports_gpt_5_6_canonical_prompt_provenance() {
        let resolved = resolve_prompt("gpt-5.6-sol", Some("remote prompt"));

        assert_eq!(resolved.id, PromptId::Gpt56);
        assert_eq!(resolved.source, PromptSource::LocalModelPolicy);
        assert_eq!(resolved.normalization, PromptNormalization::Trim);
        assert_eq!(
            resolved.content,
            codex_protocol::models::BASE_INSTRUCTIONS_DEFAULT.trim()
        );
        assert!(resolved.clears_instruction_template);
    }

    #[test]
    fn resolver_preserves_non_family_catalog_prompt() {
        let resolved = resolve_prompt("catalog-model", Some("catalog prompt\n"));

        assert_eq!(resolved.id, PromptId::Catalog);
        assert_eq!(resolved.source, PromptSource::Catalog);
        assert_eq!(resolved.normalization, PromptNormalization::Preserve);
        assert_eq!(resolved.content, "catalog prompt\n");
        assert!(!resolved.clears_instruction_template);
    }

    #[test]
    fn resolver_uses_untrimmed_protocol_default_for_fallback() {
        let resolved = resolve_prompt("unknown-model", None);

        assert_eq!(resolved.id, PromptId::ProtocolDefault);
        assert_eq!(resolved.source, PromptSource::ProtocolDefault);
        assert_eq!(resolved.normalization, PromptNormalization::Preserve);
        assert_eq!(
            resolved.content,
            codex_protocol::models::BASE_INSTRUCTIONS_DEFAULT
        );
    }

    #[test]
    fn local_policy_preserves_approval_messages() {
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

        apply_prompt_policy(std::slice::from_mut(&mut model));

        assert_eq!(
            model.base_instructions,
            codex_protocol::models::BASE_INSTRUCTIONS_DEFAULT.trim()
        );
        assert_eq!(
            model.model_messages,
            Some(ModelMessages {
                instructions_template: None,
                instructions_variables: None,
                approvals: Some(approvals),
            })
        );
    }
}
