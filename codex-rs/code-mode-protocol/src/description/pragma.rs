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
