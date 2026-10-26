use crate::model_info::clear_instruction_messages;
use crate::model_info::matches_model_slug;
use crate::model_info::namespaced_model_suffix;
use codex_protocol::openai_models::ModelInfo;
use tracing::debug;

/// Authoritative registration for models that use the fork's local prompt policy.
pub(crate) const LOCAL_PROMPT_POLICY_SLUGS: &[&str] = &[
    "gpt-6-astra",
    "gpt-6.1-sol",
    "gpt-6-sol",
    "gpt-6-luna",
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-5.6-luna",
    "gpt-5.5",
    "gpt-5.4",
    "gpt-5.4-mini",
    "gpt-5.2",
    "gpt-daybreak-blue-latest",
    "gpt-daybreak-red-latest",
    "codex-auto-review",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PromptId {
    Catalog,
    ProtocolDefault,
    LocalPolicy,
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
    let policy_slug = namespaced_model_suffix(slug).unwrap_or(slug);
    if LOCAL_PROMPT_POLICY_SLUGS
        .iter()
        .any(|registered| matches_model_slug(policy_slug, registered))
    {
        return ResolvedPrompt {
            id: PromptId::LocalPolicy,
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
        normalization: PromptNormalization::Trim,
        content: codex_protocol::models::BASE_INSTRUCTIONS_DEFAULT.trim(),
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

