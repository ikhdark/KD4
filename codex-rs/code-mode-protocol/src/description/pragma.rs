use serde::Deserialize;
use std::collections::BTreeMap;

const MAX_JS_SAFE_INTEGER: u64 = (1_u64 << 53) - 1;
pub const CODE_MODE_PRAGMA_PREFIX: &str = "// @exec:";

#[derive(Debug, Default, Deserialize)]
struct CodeModeExecPragma {
    #[serde(default, rename = "yield_time_ms")]
    compatibility_yield_time_ms: Option<u64>,
    #[serde(default)]
    max_output_tokens: Option<usize>,
    #[serde(flatten)]
    unknown_fields: BTreeMap<String, serde::de::IgnoredAny>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ParsedExecSource<'a> {
    pub code: &'a str,
    pub max_output_tokens: Option<usize>,
}

pub fn parse_exec_source(input: &str) -> Result<ParsedExecSource<'_>, String> {
    if input.trim().is_empty() {
        return Err(
            "exec expects raw JavaScript source text (non-empty). Provide JS only, optionally with first-line `// @exec: {\"max_output_tokens\": 10000}`.".to_string(),
        );
    }

    let (first_line, rest) = match input.split_once('\n') {
        Some(parts) => parts,
        None => (input, ""),
    };
    let trimmed = first_line.trim_start();
    let Some(pragma) = trimmed.strip_prefix(CODE_MODE_PRAGMA_PREFIX) else {
        return Ok(ParsedExecSource {
            code: input,
            max_output_tokens: None,
        });
    };

    if rest.trim().is_empty() {
        return Err(
            "exec pragma must be followed by JavaScript source on subsequent lines".to_string(),
        );
    }

    let directive = pragma.trim();
    if directive.is_empty() {
        return Err(
            "exec pragma must be a JSON object with supported field `max_output_tokens`"
                .to_string(),
        );
    }

    if !directive.starts_with('{') {
        return Err(
            "exec pragma must be a JSON object with supported field `max_output_tokens`"
                .to_string(),
        );
    }
    let pragma: CodeModeExecPragma = serde_json::from_str(directive).map_err(|err| {
        if err.is_syntax() || err.is_eof() {
            format!(
                "exec pragma must be valid JSON with supported field `max_output_tokens`: {err}"
            )
        } else {
            format!("exec pragma has an invalid field value: {err}")
        }
    })?;
    if let Some(key) = pragma.unknown_fields.keys().next() {
        return Err(format!(
            "exec pragma only supports `max_output_tokens`; got `{key}`"
        ));
    }
    if pragma
        .compatibility_yield_time_ms
        .is_some_and(|yield_time_ms| yield_time_ms > MAX_JS_SAFE_INTEGER)
    {
        return Err(
            "exec pragma field `yield_time_ms` must be a non-negative safe integer".to_string(),
        );
    }
    if pragma.max_output_tokens.is_some_and(|max_output_tokens| {
        u64::try_from(max_output_tokens)
            .map(|max_output_tokens| max_output_tokens > MAX_JS_SAFE_INTEGER)
            .unwrap_or(true)
    }) {
        return Err(
            "exec pragma field `max_output_tokens` must be a non-negative safe integer".to_string(),
        );
    }

    Ok(ParsedExecSource {
        code: rest,
        max_output_tokens: pragma.max_output_tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::ParsedExecSource;
    use super::parse_exec_source;
    use pretty_assertions::assert_eq;

    #[test]
    fn valid_pragma_boundaries_preserve_source_and_output_budget() {
        for (directive, max_output_tokens) in [
            ("{}", None),
            (r#"{"max_output_tokens":0}"#, Some(0)),
            (
                r#"{"max_output_tokens":9007199254740991}"#,
                Some(9_007_199_254_740_991),
            ),
            (r#"{"yield_time_ms":0,"max_output_tokens":7}"#, Some(7)),
            (r#"{"yield_time_ms":9007199254740991}"#, None),
        ] {
            let source = format!("  // @exec: {directive}\r\ntext('first');\r\ntext('second');");
            assert_eq!(
                parse_exec_source(&source),
                Ok(ParsedExecSource {
                    code: "text('first');\r\ntext('second');",
                    max_output_tokens,
                }),
                "{directive}"
            );
        }
    }

    #[test]
    fn invalid_field_types_report_data_errors_without_blame_on_another_field() {
        for field in ["yield_time_ms", "max_output_tokens"] {
            for value in ["-1", "1.5", r#""many""#, "true", "[]", "{}"] {
                let source = format!("// @exec: {{\"{field}\":{value}}}\ntext('must not run')");
                let error = parse_exec_source(&source).expect_err(&source);
                assert!(
                    error.starts_with("exec pragma has an invalid field value:"),
                    "{error}"
                );
                assert!(!error.contains("must be valid JSON"), "{error}");
                if field == "yield_time_ms" {
                    assert!(!error.contains("max_output_tokens"), "{error}");
                }
            }
        }
    }

    #[test]
    fn duplicate_fields_are_rejected_as_duplicates_even_when_values_match() {
        for field in ["yield_time_ms", "max_output_tokens"] {
            let source = format!("// @exec: {{\"{field}\":1,\"{field}\":1}}\ntext('must not run')");
            let error = parse_exec_source(&source).expect_err(&source);
            assert!(
                error.starts_with("exec pragma has an invalid field value:"),
                "{error}"
            );
            assert!(
                error.contains(&format!("duplicate field `{field}`")),
                "{error}"
            );
            assert!(!error.contains("safe integer"), "{error}");
        }
    }

    #[test]
    fn unsafe_integers_identify_the_field_that_exceeded_the_limit() {
        for field in ["yield_time_ms", "max_output_tokens"] {
            let source =
                format!("// @exec: {{\"{field}\":9007199254740992}}\ntext('must not run')");
            assert_eq!(
                parse_exec_source(&source).unwrap_err(),
                format!("exec pragma field `{field}` must be a non-negative safe integer")
            );
        }
    }

    #[test]
    fn malformed_json_unknown_fields_and_missing_code_remain_distinct() {
        let error = parse_exec_source("// @exec: {\"max_output_tokens\":\ntext('hi')").unwrap_err();
        assert!(
            error.starts_with("exec pragma must be valid JSON"),
            "{error}"
        );
        assert_eq!(
            parse_exec_source("// @exec: {\"timeout_ms\":1}\ntext('hi')").unwrap_err(),
            "exec pragma only supports `max_output_tokens`; got `timeout_ms`"
        );
        assert_eq!(
            parse_exec_source("// @exec: {\"max_output_tokens\":1}\n  ").unwrap_err(),
            "exec pragma must be followed by JavaScript source on subsequent lines"
        );
        let code = "text('hi');\n// @exec: {\"max_output_tokens\":1}";
        assert_eq!(
            parse_exec_source(code),
            Ok(ParsedExecSource {
                code,
                max_output_tokens: None
            })
        );
    }
}
