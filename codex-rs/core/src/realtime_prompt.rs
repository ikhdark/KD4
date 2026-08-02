use codex_prompts::BACKEND_PROMPT;

pub(crate) fn prepare_realtime_backend_prompt(
    prompt: Option<Option<String>>,
    config_prompt: Option<String>,
) -> String {
    if let Some(config_prompt) = config_prompt
        && !config_prompt.trim().is_empty()
    {
        return config_prompt;
    }

    match prompt {
        Some(Some(prompt)) => return prompt,
        Some(None) => return String::new(),
        None => {}
    }

    BACKEND_PROMPT.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::BACKEND_PROMPT;
    use super::prepare_realtime_backend_prompt;

    #[test]
    fn prepare_realtime_backend_prompt_prefers_config_override() {
        assert_eq!(
            prepare_realtime_backend_prompt(
                Some(Some("prompt from request".to_string())),
                Some("prompt from config".to_string()),
            ),
            "prompt from config"
        );
    }

    #[test]
    fn prepare_realtime_backend_prompt_uses_request_prompt() {
        assert_eq!(
            prepare_realtime_backend_prompt(
                Some(Some("prompt from request".to_string())),
                /*config_prompt*/ None,
            ),
            "prompt from request"
        );
    }

    #[test]
    fn prepare_realtime_backend_prompt_preserves_empty_request_prompt() {
        assert_eq!(
            prepare_realtime_backend_prompt(Some(Some(String::new())), /*config_prompt*/ None),
            ""
        );
        assert_eq!(
            prepare_realtime_backend_prompt(Some(None), /*config_prompt*/ None),
            ""
        );
    }

    #[test]
    fn prepare_realtime_backend_prompt_renders_default() {
        let prompt =
            prepare_realtime_backend_prompt(/*prompt*/ None, /*config_prompt*/ None);

        assert_eq!(prompt, BACKEND_PROMPT.trim_end());
    }
}
