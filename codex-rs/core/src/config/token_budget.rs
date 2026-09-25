use super::ManagedFeatures;
use codex_config::config_toml::ConfigToml;
use codex_features::Feature;
use codex_features::FeatureToml;
use codex_features::FeaturesToml;
use codex_features::TokenBudgetConfigToml;
use serde::Serialize;

const DEFAULT_REMINDER: &str = "Your context window is nearly exhausted (only {n_remaining} tokens remaining) and will be automatically reset for you soon. Once reset, message items in current context window will be cleared in the new window, but notes and history items will be persistent across windows.";

const TOKEN_BUDGET_REMINDER_MESSAGE_TEMPLATE_MAX_BYTES: usize = 2000;
const TOKEN_BUDGET_GUIDANCE_MESSAGE_MAX_BYTES: usize = 2000;
const AUTO_COMPACT_FALLBACK_PROMPT_MAX_BYTES: usize = 2000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TokenBudgetConfig {
    pub use_history_notes_extension: bool,
    pub reminder_threshold_tokens: Option<i64>,
    pub reminder_message_template: String,
    pub guidance_message: Option<String>,
    pub auto_compact_fallback_prompt: Option<String>,
    pub auto_compact_fallback_buffer_tokens: Option<i64>,
}

impl TokenBudgetConfig {
    pub(crate) fn validate(&self) -> std::io::Result<()> {
        if self
            .reminder_threshold_tokens
            .is_some_and(|tokens| tokens <= 0)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "features.token_budget.reminder_threshold_tokens must be positive",
            ));
        }

        if self.reminder_message_template.trim().is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "features.token_budget.reminder_message_template must not be empty",
            ));
        }
        if self.reminder_message_template.len() > TOKEN_BUDGET_REMINDER_MESSAGE_TEMPLATE_MAX_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "features.token_budget.reminder_message_template must not exceed {TOKEN_BUDGET_REMINDER_MESSAGE_TEMPLATE_MAX_BYTES} bytes"
                ),
            ));
        }

        if self
            .guidance_message
            .as_ref()
            .is_some_and(|message| message.len() > TOKEN_BUDGET_GUIDANCE_MESSAGE_MAX_BYTES)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "features.token_budget.guidance_message must not exceed {TOKEN_BUDGET_GUIDANCE_MESSAGE_MAX_BYTES} bytes"
                ),
            ));
        }

        if self
            .auto_compact_fallback_prompt
            .as_ref()
            .is_some_and(|prompt| prompt.len() > AUTO_COMPACT_FALLBACK_PROMPT_MAX_BYTES)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "features.token_budget.auto_compact_fallback_prompt must not exceed {AUTO_COMPACT_FALLBACK_PROMPT_MAX_BYTES} bytes"
                ),
            ));
        }
        if self.auto_compact_fallback_prompt.is_some()
            && self.auto_compact_fallback_buffer_tokens.is_none()
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "features.token_budget.auto_compact_fallback_buffer_tokens is required when auto_compact_fallback_prompt is set",
            ));
        }
        if self
            .auto_compact_fallback_buffer_tokens
            .is_some_and(|tokens| tokens <= 0)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "features.token_budget.auto_compact_fallback_buffer_tokens must be positive",
            ));
        }

        Ok(())
    }

    pub(crate) fn fallback_buffer_tokens(&self) -> i64 {
        if self.auto_compact_fallback_prompt.is_some() {
            self.auto_compact_fallback_buffer_tokens.unwrap_or(0)
        } else {
            0
        }
    }
}

impl Default for TokenBudgetConfig {
    fn default() -> Self {
        Self {
            use_history_notes_extension: false,
            reminder_threshold_tokens: None,
            reminder_message_template: DEFAULT_REMINDER.to_owned(),
            guidance_message: None,
            auto_compact_fallback_prompt: None,
            auto_compact_fallback_buffer_tokens: None,
        }
    }
}

pub(crate) fn resolve_token_budget_config(
    config_toml: &ConfigToml,
    features: &ManagedFeatures,
) -> std::io::Result<Option<TokenBudgetConfig>> {
    if !features.enabled(Feature::TokenBudget) {
        return Ok(None);
    }

    let token_budget_config = token_budget_toml_config(config_toml.features.as_ref());
    let use_history_notes_extension = token_budget_config
        .and_then(|config| config.use_history_notes_extension)
        .unwrap_or_default();
    let reminder_threshold_tokens =
        token_budget_config.and_then(|config| config.reminder_threshold_tokens);
    let reminder_message_template = token_budget_config
        .and_then(|config| config.reminder_message_template.clone())
        .unwrap_or_else(|| DEFAULT_REMINDER.to_owned());
    let guidance_message = token_budget_config
        .and_then(|config| config.guidance_message.clone())
        .filter(|message| !message.trim().is_empty());
    let auto_compact_fallback_prompt = token_budget_config
        .and_then(|config| config.auto_compact_fallback_prompt.as_deref())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let auto_compact_fallback_buffer_tokens =
        token_budget_config.and_then(|config| config.auto_compact_fallback_buffer_tokens);

    let token_budget = TokenBudgetConfig {
        use_history_notes_extension,
        reminder_threshold_tokens,
        reminder_message_template,
        guidance_message,
        auto_compact_fallback_prompt,
        auto_compact_fallback_buffer_tokens,
    };
    token_budget.validate()?;
    Ok(Some(token_budget))
}

fn token_budget_toml_config(features: Option<&FeaturesToml>) -> Option<&TokenBudgetConfigToml> {
    match features?.token_budget.as_ref()? {
        FeatureToml::Enabled(_) => None,
        FeatureToml::Config(config) => Some(config),
    }
}
