use regex::Regex;
use std::borrow::Cow;
use std::sync::LazyLock;

static OPENAI_KEY_REGEX: LazyLock<Regex> = LazyLock::new(|| compile_regex(r"sk-[A-Za-z0-9]{20,}"));
static AWS_ACCESS_KEY_ID_REGEX: LazyLock<Regex> =
    LazyLock::new(|| compile_regex(r"\bAKIA[0-9A-Z]{16}\b"));
static BEARER_TOKEN_REGEX: LazyLock<Regex> =
    LazyLock::new(|| compile_regex(r"(?i)\bBearer\s+[A-Za-z0-9._~+/\-]+=*"));
static SECRET_ASSIGNMENT_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    compile_regex(
        r#"(?i)(\b(?:api[_-]?key|token|secret|password)\b["']?\s*[:=]\s*)(?:"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|[^\s"';,}\]]{8,})"#,
    )
});

/// Remove secret and keys from a String. This is done on best effort basis following some
/// well-known REGEX.
pub fn redact_secrets(mut input: String) -> String {
    if let Cow::Owned(redacted) =
        SECRET_ASSIGNMENT_REGEX.replace_all(&input, |captures: &regex::Captures<'_>| {
            let prefix = &captures[1];
            let value = &captures[0][prefix.len()..];
            match value.as_bytes().first() {
                Some(b'"') => format!("{prefix}\"[REDACTED_SECRET]\""),
                Some(b'\'') => format!("{prefix}'[REDACTED_SECRET]'"),
                _ => format!("{prefix}[REDACTED_SECRET]"),
            }
        })
    {
        input = redacted;
    }
    for (pattern, replacement) in [
        (&*OPENAI_KEY_REGEX, "[REDACTED_SECRET]"),
        (&*AWS_ACCESS_KEY_ID_REGEX, "[REDACTED_SECRET]"),
        (&*BEARER_TOKEN_REGEX, "Bearer [REDACTED_SECRET]"),
    ] {
        if let Cow::Owned(redacted) = pattern.replace_all(&input, replacement) {
            input = redacted;
        }
    }
    input
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
    fn redacts_complete_quoted_and_bearer_values_without_consuming_delimiters() {
        for (input, expected) in [
            (
                r#"{"api_key":"abcdefgh","mode":"safe"}"#,
                r#"{"api_key":"[REDACTED_SECRET]","mode":"safe"}"#,
            ),
            (
                r#"password="abc def ghi""#,
                r#"password="[REDACTED_SECRET]""#,
            ),
            (
                r#"password="abc\" def ghi""#,
                r#"password="[REDACTED_SECRET]""#,
            ),
            ("Bearer abcdefghijklmnop+/=", "Bearer [REDACTED_SECRET]"),
            ("Bearer abcdefghijklmnop-", "Bearer [REDACTED_SECRET]"),
            ("Bearer abc~+/==; next", "Bearer [REDACTED_SECRET]; next"),
            (
                "token=abcdefgh; mode=safe",
                "token=[REDACTED_SECRET]; mode=safe",
            ),
            (
                "token=abcdefgh, mode=safe",
                "token=[REDACTED_SECRET], mode=safe",
            ),
            ("ordinary text 🦀", "ordinary text 🦀"),
        ] {
            assert_eq!(redact_secrets(input.to_string()), expected);
        }
    }

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
