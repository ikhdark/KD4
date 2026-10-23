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
    #[serde(default)]
    deliver: bool,
    #[serde(default)]
    persist: bool,
    #[serde(flatten)]
    unknown_fields: BTreeMap<String, serde::de::IgnoredAny>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ParsedExecSource<'a> {
    pub code: &'a str,
    pub max_output_tokens: Option<usize>,
    pub deliver: bool,
    pub persist: bool,
}

pub fn parse_exec_source(input: &str) -> Result<ParsedExecSource<'_>, String> {
    if input.trim().is_empty() {
        return Err(
            "exec expects raw JavaScript source text (non-empty). Provide JS only, optionally with first-line `// @exec: {\"max_output_tokens\": 40000}`.".to_string(),
        );
    }

    // Leading blank lines and a byte-order mark are not source, so the pragma
    // is still the first line. Otherwise an incidental leading newline turned
    // the directive into an ordinary comment and silently dropped its options.
    let source = input.trim_start_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
    let (first_line, rest) = match source.split_once('\n') {
        Some(parts) => parts,
        None => (source, ""),
    };
    let Some(pragma) = first_line.strip_prefix(CODE_MODE_PRAGMA_PREFIX) else {
        return Ok(ParsedExecSource {
            code: input,
            max_output_tokens: None,
            deliver: false,
            persist: false,
        });
    };

    let directive = pragma.trim();
    if directive.is_empty() {
        return Err(
            "exec pragma must be a JSON object with supported fields `max_output_tokens`, `deliver`, and `persist`"
                .to_string(),
        );
    }

    if !directive.starts_with('{') {
        return Err(
            "exec pragma must be a JSON object with supported fields `max_output_tokens`, `deliver`, and `persist`"
                .to_string(),
        );
    }
    let pragma: CodeModeExecPragma = serde_json::from_str(directive).map_err(|err| {
        if err.is_syntax() || err.is_eof() {
            format!(
                "exec pragma must be valid JSON with supported fields `max_output_tokens`, `deliver`, and `persist`: {err}"
            )
        } else {
            format!("exec pragma has an invalid field value: {err}")
        }
    })?;
    if let Some(key) = pragma.unknown_fields.keys().next() {
        return Err(format!(
            "exec pragma only supports `max_output_tokens`, `deliver`, and `persist`; got `{key}`"
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
    // An empty body is meaningful only for delivery: once a refused answer's
    // evidence is recovered, a deliver cell that prints nothing delivers it.
    if rest.trim().is_empty() && !pragma.deliver {
        return Err(
            "exec pragma must be followed by JavaScript source on subsequent lines".to_string(),
        );
    }

    Ok(ParsedExecSource {
        code: rest,
        max_output_tokens: pragma.max_output_tokens,
        deliver: pragma.deliver,
        persist: pragma.persist,
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
                    deliver: false,
                    persist: false,
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
                if value == "-1" {
                    assert!(error.contains("-1"), "{error}");
                }
                if field == "yield_time_ms" {
                    assert!(!error.contains("max_output_tokens"), "{error}");
                }
            }
        }
    }

    #[test]
    fn duplicate_fields_are_rejected_as_duplicates_even_when_values_match() {
        for (field, second) in [
            ("yield_time_ms", 1),
            ("yield_time_ms", 2),
            ("max_output_tokens", 1),
            ("max_output_tokens", 2),
        ] {
            let source =
                format!("// @exec: {{\"{field}\":1,\"{field}\":{second}}}\ntext('must not run')");
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
            "exec pragma only supports `max_output_tokens`, `deliver`, and `persist`; got `timeout_ms`"
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
                max_output_tokens: None,
                deliver: false,
                persist: false,
            })
        );
    }

    #[test]
    fn leading_blank_lines_and_byte_order_mark_do_not_hide_the_pragma() {
        for source in [
            "\n// @exec: {\"max_output_tokens\":7,\"persist\":true}\ntext('kept');",
            "\r\n  \r\n// @exec: {\"max_output_tokens\":7,\"persist\":true}\r\ntext('kept');",
            "\u{feff}// @exec: {\"max_output_tokens\":7,\"persist\":true}\ntext('kept');",
            "\u{feff}\n\t// @exec: {\"max_output_tokens\":7,\"persist\":true}\ntext('kept');",
        ] {
            assert_eq!(
                parse_exec_source(source),
                Ok(ParsedExecSource {
                    code: "text('kept');",
                    max_output_tokens: Some(7),
                    deliver: false,
                    persist: true,
                }),
                "{source:?}"
            );
        }
        // The directive is validated rather than silently ignored.
        assert_eq!(
            parse_exec_source("\n// @exec: {\"timeout_ms\":1}\ntext('must not run')").unwrap_err(),
            "exec pragma only supports `max_output_tokens`, `deliver`, and `persist`; got `timeout_ms`"
        );
        // A directive after source is still code, and source without a
        // directive is returned unchanged, leading blank lines included.
        for source in [
            "text('first');\n// @exec: {\"max_output_tokens\":7}\ntext('second');",
            "\n\ntext('plain');",
            "\u{feff}text('plain');",
        ] {
            assert_eq!(
                parse_exec_source(source),
                Ok(ParsedExecSource {
                    code: source,
                    max_output_tokens: None,
                    deliver: false,
                    persist: false,
                }),
                "{source:?}"
            );
        }
    }

    #[test]
    fn delivery_requires_an_explicit_boolean_and_preserves_the_script() {
        for deliver in [false, true] {
            let source = format!("// @exec: {{\"deliver\":{deliver}}}\ntext('answer');");
            let parsed = parse_exec_source(&source).unwrap();
            assert_eq!(parsed.deliver, deliver);
            assert_eq!(parsed.code, "text('answer');");
        }
        for value in ["null", "1", "\"true\"", "[]", "{}"] {
            assert!(
                parse_exec_source(&format!(
                    "// @exec: {{\"deliver\":{value}}}\ntext('must not execute');"
                ))
                .is_err()
            );
        }
        assert!(
            parse_exec_source(
                "// @exec: {\"deliver\":true,\"deliver\":true}\ntext('must not execute');"
            )
            .is_err()
        );
    }

    #[test]
    fn a_deliver_pragma_alone_is_the_empty_cell_that_delivers_a_retained_answer() {
        // A delivery refused for pending recovery keeps its answer, and "a
        // deliver cell that prints nothing delivers it unchanged". Sessions
        // 5596, 99f1 and cfbb sent exactly the first input below for that
        // cell, got a parse error, and spent a round trip resending `void 0;`.
        for source in [
            "// @exec: {\"deliver\": true}\n",
            "// @exec: {\"deliver\": true}",
            "// @exec: {\"deliver\":true,\"persist\":true}\r\n  \n",
        ] {
            let parsed = parse_exec_source(source).expect(source);
            assert!(parsed.deliver, "{source:?}");
            assert!(parsed.code.trim().is_empty(), "{source:?}");
        }
        // Without delivery an empty body has no use, so it stays an error.
        for source in [
            "// @exec: {}\n",
            "// @exec: {\"deliver\":false}\n",
            "// @exec: {\"persist\":true}",
        ] {
            assert_eq!(
                parse_exec_source(source).unwrap_err(),
                "exec pragma must be followed by JavaScript source on subsequent lines",
                "{source:?}"
            );
        }
        // The directive itself is still validated.
        let error = parse_exec_source("// @exec: {\"deliver\": tru}\n").unwrap_err();
        assert!(error.starts_with("exec pragma must be valid JSON"), "{error}");
        assert_eq!(
            parse_exec_source("// @exec: {\"deliver\":true,\"budget\":1}\n").unwrap_err(),
            "exec pragma only supports `max_output_tokens`, `deliver`, and `persist`; got `budget`"
        );
    }
}
