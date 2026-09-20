use regex::Regex;
use regex::RegexSet;
use std::borrow::Cow;
use std::sync::LazyLock;

static OPENAI_KEY_REGEX: LazyLock<Regex> =
    LazyLock::new(|| compile_regex(r"sk-(?:(?:proj|svcacct)-)?[A-Za-z0-9_-]{20,}"));
static AWS_ACCESS_KEY_ID_REGEX: LazyLock<Regex> =
    LazyLock::new(|| compile_regex(r"\bAKIA[0-9A-Z]{16}\b"));
static GITHUB_TOKEN_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    compile_regex(r"\b(?:gh[pousr]_[A-Za-z0-9]{36,}|github_pat_[A-Za-z0-9_]{29,})\b")
});
static BEARER_TOKEN_REGEX: LazyLock<Regex> =
    LazyLock::new(|| compile_regex(r"(?i)\bBearer\s+[A-Za-z0-9._~+/\-]+=*"));
static SECRET_ASSIGNMENT_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    compile_regex(
        r#"(?i)(\b(?:api[_-]?key|token|secret|password|(?:aws[_-])?secret[_-]access[_-]key|aws[_-]session[_-]token)\b["']?\s*[:=]\s*)(?:\[REDACTED_SECRET\]|"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|[^\s"';,}\]]+)"#,
    )
});

/// Remove secret and keys from a String. This is done on best effort basis following some
/// well-known REGEX.
pub fn redact_secrets(mut input: String) -> String {
    static ANY_SECRET: LazyLock<RegexSet> = LazyLock::new(|| {
        match RegexSet::new([
            SECRET_ASSIGNMENT_REGEX.as_str(),
            OPENAI_KEY_REGEX.as_str(),
            AWS_ACCESS_KEY_ID_REGEX.as_str(),
            GITHUB_TOKEN_REGEX.as_str(),
            BEARER_TOKEN_REGEX.as_str(),
        ]) {
            Ok(patterns) => patterns,
            Err(err) => panic!("invalid secret pattern set: {err}"),
        }
    });
    if !ANY_SECRET.is_match(&input) {
        return input;
    }
    if let Cow::Owned(redacted) =
        SECRET_ASSIGNMENT_REGEX.replace_all(&input, |captures: &regex::Captures<'_>| {
            let prefix = &captures[1];
            let value = &captures[0][prefix.len()..];
            if value == "[REDACTED_SECRET]" {
                return captures[0].to_string();
            }
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
        (&*GITHUB_TOKEN_REGEX, "[REDACTED_SECRET]"),
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
    fn redacts_github_tokens_in_free_text() {
        for prefix in ["ghp_", "gho_", "ghu_", "ghs_", "ghr_", "github_pat_"] {
            let token = format!("{prefix}{}", "a".repeat(36));
            assert_eq!(
                redact_secrets(format!("copied ({token}), then continued")),
                "copied ([REDACTED_SECRET]), then continued"
            );
        }
        assert_eq!(
            redact_secrets("github_pat_short and ghp_short".to_string()),
            "github_pat_short and ghp_short"
        );
    }

    #[test]
    fn redacts_short_assignments_and_named_aws_credentials() {
        for (input, expected) in [
            ("secret=short; end", "secret=[REDACTED_SECRET]; end"),
            ("password=x, next", "password=[REDACTED_SECRET], next"),
            ("token=123", "token=[REDACTED_SECRET]"),
            (
                "password=[abcdefgh]; next",
                "password=[REDACTED_SECRET]]; next",
            ),
            (
                "AWS_SECRET_ACCESS_KEY=abcdefghijklmnopqrstuvwxyz0123456789+/AB",
                "AWS_SECRET_ACCESS_KEY=[REDACTED_SECRET]",
            ),
            (
                "aws_session_token=temporary+/=; end",
                "aws_session_token=[REDACTED_SECRET]; end",
            ),
            (
                r#"{"secret_access_key":"abc+/=","region":"us-east-1"}"#,
                r#"{"secret_access_key":"[REDACTED_SECRET]","region":"us-east-1"}"#,
            ),
        ] {
            let redacted = redact_secrets(input.to_string());
            assert_eq!(redacted, expected);
            assert_eq!(redact_secrets(redacted), expected);
        }
    }

    #[test]
    fn redacts_project_and_service_account_keys_in_free_text() {
        for key in [
            "sk-proj-abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-_",
            "sk-svcacct-abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGH",
            "sk-abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGH",
        ] {
            assert_eq!(
                redact_secrets(format!("copied ({key}), then continued")),
                "copied ([REDACTED_SECRET]), then continued"
            );
        }
        assert_eq!(
            redact_secrets("sk-proj-short and ordinary text".to_string()),
            "sk-proj-short and ordinary text"
        );
    }

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
                "secret=[REDACTED_SECRET]; end"
            )
        );
    }
}
