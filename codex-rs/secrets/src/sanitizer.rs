use regex::Regex;
use std::sync::LazyLock;

static OPENAI_KEY_REGEX: LazyLock<Regex> = LazyLock::new(|| compile_regex(r"sk-[A-Za-z0-9]{20,}"));
static AWS_ACCESS_KEY_ID_REGEX: LazyLock<Regex> =
    LazyLock::new(|| compile_regex(r"\bAKIA[0-9A-Z]{16}\b"));
static BEARER_TOKEN_REGEX: LazyLock<Regex> =
    LazyLock::new(|| compile_regex(r"(?i)\bBearer\s+[A-Za-z0-9._\-]{16,}\b"));
static SECRET_ASSIGNMENT_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    compile_regex(r#"(?i)\b(api[_-]?key|token|secret|password)\b(\s*[:=]\s*)(["']?)[^\s"']{8,}"#)
});

/// Remove secret and keys from a String. This is done on best effort basis following some
/// well-known REGEX.
pub fn redact_secrets(input: String) -> String {
    let redacted = OPENAI_KEY_REGEX.replace_all(&input, "[REDACTED_SECRET]");
    let redacted = AWS_ACCESS_KEY_ID_REGEX.replace_all(&redacted, "[REDACTED_SECRET]");
    let redacted = BEARER_TOKEN_REGEX.replace_all(&redacted, "Bearer [REDACTED_SECRET]");
    let redacted = SECRET_ASSIGNMENT_REGEX.replace_all(&redacted, "$1$2$3[REDACTED_SECRET]");

    redacted.to_string()
}

fn compile_regex(pattern: &str) -> Regex {
    match Regex::new(pattern) {
        Ok(regex) => regex,
        // The redaction behavior test also compiles every pattern.
        Err(err) => panic!("invalid regex pattern `{pattern}`: {err}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_known_secrets_and_preserves_surrounding_text() {
        let input = concat!(
            "ordinary text 🦀\n",
            "sk-abcdefghijklmnopqrst\n",
            "AKIA1234567890ABCDEF\n",
            "Authorization: bEaReR abcdefghijklmnop\n",
            "api_key = \"abcdefgh\" token: '12345678' password=abcdefgh\n",
            "secret=short; end"
        );
        assert_eq!(
            redact_secrets(input.to_string()),
            concat!(
                "ordinary text 🦀\n",
                "[REDACTED_SECRET]\n",
                "[REDACTED_SECRET]\n",
                "Authorization: Bearer [REDACTED_SECRET]\n",
                "api_key = \"[REDACTED_SECRET]\" token: '[REDACTED_SECRET]' password=[REDACTED_SECRET]\n",
                "secret=short; end"
            )
        );
    }
}
