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
        // Panic is ok thanks to `load_regex` test.
        Err(err) => panic!("invalid regex pattern `{pattern}`: {err}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_regex() {
        // Exercise every lazy regex through the public sanitizer, including
        // non-secret text that an overbroad redaction must leave unchanged.
        for (input, expected) in [
            ("key sk-abcdefghijklmnopqrst end", "key [REDACTED_SECRET] end"),
            ("id AKIA0123456789ABCDEF end", "id [REDACTED_SECRET] end"),
            ("bEaReR abcdefghijklmnop", "Bearer [REDACTED_SECRET]"),
            ("api_key=abcdefgh", "api_key=[REDACTED_SECRET]"),
            ("password: \"abcdefgh\"", "password: \"[REDACTED_SECRET]\""),
            ("token='abcdefgh'", "token='[REDACTED_SECRET]'"),
            ("secret=abcdefgh", "secret=[REDACTED_SECRET]"),
            ("secret Bearer short password=short sk-short", "secret Bearer short password=short sk-short"),
            ("", ""),
        ] {
            assert_eq!(redact_secrets(input.to_string()), expected, "{input}");
        }
    }
}
