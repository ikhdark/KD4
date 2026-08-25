//! Shared token usage models and the TUI's exit-summary adapter.

use codex_protocol::protocol::FinalOutput;

pub use codex_protocol::protocol::TokenUsage;
pub(crate) use codex_protocol::protocol::TokenUsageInfo;

pub(crate) fn format_token_usage(token_usage: TokenUsage) -> String {
    FinalOutput::from(token_usage).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_summary_uses_shared_token_usage_formatting() {
        let usage = TokenUsage {
            input_tokens: 1_500,
            cached_input_tokens: 500,
            output_tokens: 250,
            reasoning_output_tokens: 50,
            total_tokens: 1_750,
        };

        assert_eq!(
            format_token_usage(usage),
            "Token usage: total=1,250 input=1,000 (+ 500 cached) output=250 (reasoning 50)"
        );
    }
}
