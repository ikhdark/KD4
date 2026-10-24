//! Opt-in structural selection over the same parser and bytes as code sections.
use std::time::Duration;
use std::time::Instant;

use codex_tools::CanonicalToolResult;
use serde::Deserialize;
use serde::Serialize;

use super::code_sections;
use super::code_sections::CodeItem;
use crate::tools::command_output_artifact::ReadToolOutputError;
use crate::tools::command_output_artifact::ToolOutputSelector;

// Derived syntax only: this cache never authenticates a filesystem read. The
// caller has already acquired and hashed its canonical bytes. Path remains in
// the key because linked module identities depend on source context. The
// parser/schema revision is binary-local, so a new build starts with no cache.
struct ParsedSource {
    path: String,
    sha256: String,
    items: std::sync::Arc<[CodeItem]>,
    bytes: usize,
}

static PARSED_SOURCES: std::sync::LazyLock<std::sync::Mutex<std::collections::VecDeque<ParsedSource>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::VecDeque::new()));
const PARSED_SOURCE_CACHE_BYTES: usize = 16 * 1024 * 1024;

#[cfg(test)]
thread_local! { static SOURCE_PARSE_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

fn parsed_source(path: &str, source: &str, sha256: &str) -> Option<(std::sync::Arc<[CodeItem]>, bool)> {
    if let Some(items) = PARSED_SOURCES.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter().find(|entry| entry.path == path && entry.sha256 == sha256)
        .map(|entry| entry.items.clone())
    {
        return Some((items, true));
    }
    #[cfg(test)]
    SOURCE_PARSE_COUNT.with(|count| count.set(count.get() + 1));
    // Parsing and allocation never hold the cache lock. Concurrent cold misses
    // may parse twice rather than serialize unrelated readers behind a parser.
    let (items, complete) = code_sections::parse_with_completeness(
        path, source, sha256, Instant::now() + Duration::from_secs(2),
    )?;
    let bytes = path.len().saturating_add(sha256.len()).saturating_add(
        items.iter().map(|item| {
            std::mem::size_of::<CodeItem>() + item.id.capacity() + item.name.capacity()
                + item.qualified_name.capacity() + item.signature.capacity()
                + item.linked_path.as_ref().map_or(0, String::capacity)
                + item.trait_method_alias.as_ref().map_or(0, String::capacity)
                + item.linked_path_candidates.capacity() * std::mem::size_of::<String>()
                + item.linked_path_candidates.iter().map(String::capacity).sum::<usize>()
        }).sum::<usize>(),
    );
    let items: std::sync::Arc<[CodeItem]> = items.into();
    // Timeout/failure and recovered syntax must never become a complete hit.
    if complete && bytes <= PARSED_SOURCE_CACHE_BYTES {
        let mut cache = PARSED_SOURCES.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if !cache.iter().any(|entry| entry.path == path && entry.sha256 == sha256) {
            while cache.len() >= 32 || cache.iter().map(|entry| entry.bytes).sum::<usize>()
                .saturating_add(bytes) > PARSED_SOURCE_CACHE_BYTES
            { cache.pop_front(); }
            cache.push_back(ParsedSource { path: path.into(), sha256: sha256.into(), items: items.clone(), bytes });
        }
    }
    Some((items, complete))
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub(super) enum FileSelector {
    Structure(StructureSelector),
    Exact(ToolOutputSelector),
}

const FILE_SELECTOR_KINDS: &str =
    "bytes, lines, search, section, json_pointer, symbol or enclosing";

/// Decodes as `#[serde(untagged)]` did (structure, then exact), but a
/// rejection names the selector's own problem. Untagged decoding said only
/// "did not match any variant", so a guessed kind was retried blind.
impl<'de> Deserialize<'de> for FileSelector {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        let kind = value.get("kind").and_then(serde_json::Value::as_str);
        // The outline is advertised as a section; models also spell it as its
        // own kind. Only that bare spelling is an alias.
        if kind == Some("outline") && value.as_object().is_some_and(|fields| fields.len() == 1) {
            return Ok(Self::Exact(ToolOutputSelector::Section {
                id: "outline".to_string(),
            }));
        }
        let structure = match StructureSelector::deserialize(&value) {
            Ok(selector) => return Ok(Self::Structure(selector)),
            Err(error) => error,
        };
        let exact = match ToolOutputSelector::deserialize(&value) {
            Ok(selector) => return Ok(Self::Exact(selector)),
            Err(error) => error,
        };
        Err(serde::de::Error::custom(match kind {
            Some("symbol" | "enclosing" | "search") => structure.to_string(),
            Some("bytes" | "lines" | "section" | "json_pointer") => exact.to_string(),
            Some(other) => format!(
                "unsupported read_file selector kind `{}`; use one of {FILE_SELECTOR_KINDS} ({{\"kind\":\"section\",\"id\":\"outline\"}} lists Rust/Python items)",
                other.chars().take(64).collect::<String>()
            ),
            None => {
                format!("read_file selectors need a string `kind`: one of {FILE_SELECTOR_KINDS}")
            }
        }))
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum StructureSelector {
    Symbol {
        name: String,
    },
    Enclosing {
        line: usize,
    },
    Search {
        query: String,
        // Defaulted like every sibling field and like the identically named
        // field on `ToolOutputSelector::Search`. Without a default, a selector
        // that omitted only `enclosing` failed the untagged `oneOf` and the
        // rejection could not name the missing field, so the call was retried
        // in the same shape. `false` keeps the previous meaning of an explicit
        // `enclosing: false`.
        #[serde(default)]
        enclosing: bool,
        #[serde(default)]
        case_insensitive: bool,
        #[serde(default)]
        start_byte: u64,
        #[serde(default = "default_max_results")]
        max_results: usize,
        #[serde(default = "default_context_lines")]
        context_lines: usize,
    },
}

fn default_max_results() -> usize {
    20
}
fn default_context_lines() -> usize {
    3
}

fn enclosing(items: &[CodeItem], line: usize) -> Result<&CodeItem, ReadToolOutputError> {
    let candidates = || {
        items
            .iter()
            .filter(|item| line > 0 && item.start_line <= line && line <= item.end_line)
    };
    let item = candidates()
        .min_by_key(|item| item.end - item.start)
        .ok_or_else(|| {
            ReadToolOutputError::InvalidRange(format!("no enclosing code item at line {line}"))
        })?;
    if candidates().any(|other| other.start > item.start || other.end < item.end) {
        return Err(ReadToolOutputError::InvalidRange(format!(
            "multiple code items share line {line}; use a symbol or a different line"
        )));
    }
    Ok(item)
}

fn symbol_matches(item: &CodeItem, name: &str) -> bool {
    if item.kind == "impl" && !name.starts_with("impl ") && !name.contains("::impl ") {
        return false;
    }
    (item.kind != "impl" && item.name == name)
        || item.qualified_name == name
        || item.qualified_name.ends_with(&format!("::{name}"))
        || item.qualified_name.ends_with(&format!(".{name}"))
}

// Type::method where the parsed owner is a trait implementation. Like the
// Rust path, it resolves only when exactly one such method answers.
fn trait_method_candidate(item: &CodeItem, name: &str) -> bool {
    name.contains("::")
        && item
            .trait_method_alias
            .as_ref()
            .is_some_and(|alias| alias == name || alias.ends_with(&format!("::{name}")))
}

/// The outline names an impl by its header without generic parameters or a
/// where clause: `impl<T: Ord> Set<T> where T: Copy {` is `impl Set<T>`.
/// `None` when the request is not such a source header.
fn outline_impl_spelling(name: &str) -> Option<String> {
    let mut rest = name.trim().strip_prefix("impl")?;
    if let Some(parameters) = rest.strip_prefix('<') {
        let (mut depth, mut previous, mut end) = (1_usize, ' ', None);
        for (index, ch) in parameters.char_indices() {
            match ch {
                '<' => depth += 1,
                // `->` in a closure bound is not a closing bracket.
                '>' if previous != '-' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(index);
                        break;
                    }
                }
                _ => {}
            }
            previous = ch;
        }
        rest = &parameters[end? + 1..];
    } else if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let header = rest
        .split(" where ")
        .next()?
        .trim()
        .trim_end_matches('{')
        .trim();
    let spelled = format!("impl {header}");
    (!header.is_empty() && spelled != name).then_some(spelled)
}

fn same_impl_header(qualified_name: &str, header: &str) -> bool {
    let compact = |text: &str| text.split_whitespace().collect::<String>();
    let (qualified_name, header) = (compact(qualified_name), compact(header));
    qualified_name == header || qualified_name.ends_with(&format!("::{header}"))
}

fn ambiguous_symbol(name: &str) -> ReadToolOutputError {
    ReadToolOutputError::InvalidRange(format!(
        "symbol {name:?} is ambiguous; use a qualified name such as Type::method or <Type as Trait>::method (Python: Class.method), or enclosing with a known line"
    ))
}

/// A trait method named `Type::method`, or an impl named by its source header,
/// when exactly one item answers. Several answers are ambiguous.
fn unique_alias<'a>(
    items: &'a [CodeItem],
    name: &str,
) -> Result<Option<&'a CodeItem>, ReadToolOutputError> {
    let header = outline_impl_spelling(name);
    let mut aliases = items.iter().filter(|item| {
        trait_method_candidate(item, name)
            || header.as_deref().is_some_and(|header| {
                item.kind == "impl" && same_impl_header(&item.qualified_name, header)
            })
    });
    match (aliases.next(), aliases.next()) {
        (Some(item), None) => Ok(Some(item)),
        (Some(_), Some(_)) => Err(ambiguous_symbol(name)),
        (None, _) => Ok(None),
    }
}

// Diagnostic-only repair for impl and <Type as Trait> requests whose paths or
// generic arguments differ from the source header, which the caller cannot
// know before reading the file. Never an executable selector.
fn header_spelling_candidate(item: &CodeItem, name: &str) -> bool {
    let header = outline_impl_spelling(name);
    let name = header.as_deref().unwrap_or(name);
    let impl_request = name.starts_with("impl ") || name.contains("::impl ");
    let trait_request = name.starts_with('<') || name.contains("::<");
    if !(impl_request || trait_request) || impl_request != (item.kind == "impl") {
        return false;
    }
    let identifiers = |text: &str| {
        text.split(|ch: char| !(ch.is_alphanumeric() || ch == '_'))
            .filter(|token| !token.is_empty() && !matches!(*token, "impl" | "for" | "as"))
            .map(str::to_owned)
            .collect::<std::collections::BTreeSet<_>>()
    };
    let requested = identifiers(name);
    !requested.is_empty()
        && requested.is_subset(&identifiers(&item.qualified_name))
        && name.contains(" for ") == item.qualified_name.contains(" for ")
        && name.contains(" as ") == item.qualified_name.contains(" as ")
        && (impl_request || name.rsplit("::").next() == Some(item.name.as_str()))
}

pub(super) fn resolve_batch(
    path: &str,
    canonical: &mut CanonicalToolResult,
    selectors: Option<Vec<FileSelector>>,
) -> (
    Option<Vec<ToolOutputSelector>>,
    Vec<serde_json::Value>,
    Vec<serde_json::Value>,
) {
    let Some(selectors) = selectors else {
        return (None, Vec::new(), Vec::new());
    };
    let needs_items = selectors.iter().any(|selector| {
        matches!(
            selector,
            FileSelector::Structure(
                StructureSelector::Symbol { .. }
                    | StructureSelector::Enclosing { .. }
                    | StructureSelector::Search {
                        enclosing: true,
                        ..
                    }
            ) | FileSelector::Exact(
                ToolOutputSelector::Section { .. }
                    | ToolOutputSelector::Search {
                        enclosing: true,
                        ..
                    }
            )
        )
    });
    let parsed = needs_items
        .then(|| {
            std::str::from_utf8(&canonical.bytes)
                .ok()
                .and_then(|source| {
                    parsed_source(path, source, &canonical.sha256)
                })
        })
        .flatten();
    let parse_complete = parsed.as_ref().is_some_and(|(_, complete)| *complete);
    let items = parsed.map(|(items, _)| items);
    if let Some(items) = &items {
        canonical.sections = code_sections::sections(items);
        for section in &mut canonical.sections {
            if let Some(value) = section.value.as_mut() {
                value["parse_complete"] = parse_complete.into();
                if !parse_complete {
                    value["warning"] = "Parser recovery omitted unindexed syntax; use exact bytes or lines for full source coverage.".into();
                    value["fallback"] = serde_json::json!({"kind":"bytes", "start":0, "end":canonical.exact_bytes});
                }
            }
        }
    }
    let mut resolved = Vec::new();
    let mut failures = Vec::new();
    let mut bindings = Vec::new();
    for (index, selector) in selectors.into_iter().enumerate() {
        let requires_items = matches!(
            &selector,
            FileSelector::Structure(
                StructureSelector::Symbol { .. }
                    | StructureSelector::Enclosing { .. }
                    | StructureSelector::Search {
                        enclosing: true,
                        ..
                    }
            ) | FileSelector::Exact(
                ToolOutputSelector::Section { .. }
                    | ToolOutputSelector::Search {
                        enclosing: true,
                        ..
                    }
            )
        );
        let result = if requires_items && items.is_none() {
            Err(ReadToolOutputError::InvalidRange("structural selectors require a .rs or .py file parsed within the time limit; use lines or search otherwise".into()))
        } else {
            resolve_items(items.as_deref().unwrap_or_default(), vec![selector.clone()])
        };
        match result {
            Ok((ranges, item)) => {
                if matches!(
                    &selector,
                    FileSelector::Structure(
                        StructureSelector::Symbol { .. } | StructureSelector::Enclosing { .. }
                    )
                ) {
                    let mut binding = serde_json::json!({"selector_index":index,
                        "resolved_selectors":ranges});
                    // A bare name or line otherwise arrives without its owner.
                    // Parser-derived identity, like failure candidates; not type resolution.
                    if let Some(item) = item {
                        if item.qualified_name.len() <= 1024 {
                            binding["qualified_name"] = item.qualified_name.as_str().into();
                        }
                        binding["kind"] = item.kind.into();
                        if item.syntax_error {
                            binding["syntax_error"] = true.into();
                        }
                    }
                    bindings.push(binding);
                }
                resolved.extend(ranges);
            }
            Err(error) => {
                // Enrichment failure must not suppress an executable search.
                // Keep the error on the requested selector, alongside plain evidence.
                match &selector {
                    FileSelector::Structure(StructureSelector::Search {
                        query,
                        case_insensitive,
                        start_byte,
                        max_results,
                        context_lines,
                        ..
                    }) => {
                        resolved.push(ToolOutputSelector::Search {
                            query: query.clone(),
                            enclosing: false,
                            case_insensitive: *case_insensitive,
                            start_byte: *start_byte,
                            max_results: *max_results,
                            context_lines: *context_lines,
                        });
                    }
                    FileSelector::Exact(ToolOutputSelector::Search { .. }) => {
                        if let FileSelector::Exact(mut search) = selector.clone() {
                            if let ToolOutputSelector::Search { enclosing, .. } = &mut search {
                                *enclosing = false;
                            }
                            resolved.push(search);
                        }
                    }
                    _ => {}
                }
                let mut candidates = Vec::new();
                let mut candidate_count = 0;
                let mut imported = false;
                let mut message_suffix: Option<&str> = None;
                let mut elsewhere = None;
                if let FileSelector::Structure(StructureSelector::Symbol { name }) = &selector {
                    let mut matches = items
                        .iter()
                        .flat_map(|items| items.iter())
                        .filter(|item| {
                            symbol_matches(item, name)
                                || trait_method_candidate(item, name)
                                || header_spelling_candidate(item, name)
                        })
                        .collect::<Vec<_>>();
                    // Diagnostic-only repair for a wrong owner: the item exists in this
                    // file under another one. Like the trait alias, never executable.
                    if matches.is_empty()
                        && let Some((_, last)) =
                            name.rsplit_once("::").or_else(|| name.rsplit_once('.'))
                    {
                        matches.extend(
                            items
                                .iter()
                                .flat_map(|items| items.iter())
                                .filter(|item| item.kind != "impl" && item.name == last),
                        );
                    }
                    // Tokens may be path prefixes, renamed-away names or names in
                    // another scope. Offer source evidence, not import resolution.
                    if matches.is_empty() {
                        let leaf = name.rsplit([':', '.']).next().unwrap_or(name);
                        matches.extend(
                            items.iter().flat_map(|items| items.iter()).filter(|item| {
                                item.kind == "use"
                                    && item
                                        .signature
                                        .split(|ch: char| !(ch.is_alphanumeric() || ch == '_'))
                                        .any(|token| token == leaf)
                            }),
                        );
                        imported = !matches.is_empty();
                    }
                    for item in matches {
                        candidate_count += 1;
                        let identity = if item.kind == "use" {
                            &item.signature
                        } else {
                            &item.qualified_name
                        };
                        if candidates.len() < 8 && identity.len() <= 1024 {
                            candidates.push(serde_json::json!({"qualified_name":identity,
                                "selector":{"kind":"lines", "start":item.start_line, "end":item.end_line}}));
                        }
                    }
                    // This index covers selected declarations, not every symbol.
                    if candidate_count == 0 && items.is_some() {
                        if name.eq_ignore_ascii_case("outline") {
                            // Recorded sessions asked for the item listing as a
                            // symbol. Every parsed snapshot has the outline section.
                            candidate_count += 1;
                            candidates.push(serde_json::json!({"qualified_name":"outline",
                                "selector":{"kind":"section", "id":"outline"}}));
                            message_suffix = Some(
                                " The outline is a section, not a symbol: the candidate {\"kind\":\"section\",\"id\":\"outline\"} lists this file's items.",
                            );
                        } else {
                            let leaf = name.rsplit([':', '.']).next().unwrap_or(name);
                            elsewhere = Some(format!(
                                " No matching indexed item for {leaf:?} in this snapshot. Assignments, fields, variants and generated items may not be indexed. Use fixed-string search or lines on this retained snapshot; widen the search if needed."
                            ));
                        }
                    }
                } else if let FileSelector::Structure(StructureSelector::Enclosing { line }) =
                    &selector
                {
                    // A failed enclosing lookup otherwise dead-ends with an empty
                    // candidate list, forcing a raw-line guess. Offer the items
                    // nearest the requested line as exact line selectors. These
                    // are proximity suggestions, not resolved enclosure.
                    let mut nearby = items
                        .iter()
                        .flat_map(|items| items.iter())
                        .filter(|item| item.kind != "impl")
                        .map(|item| {
                            let distance = if *line < item.start_line {
                                item.start_line - *line
                            } else {
                                (*line).saturating_sub(item.end_line)
                            };
                            (distance, item)
                        })
                        .collect::<Vec<_>>();
                    nearby.sort_by_key(|(distance, item)| (*distance, item.start_line));
                    for (_, item) in nearby {
                        candidate_count += 1;
                        if candidates.len() < 8 && item.qualified_name.len() <= 1024 {
                            candidates.push(serde_json::json!({"qualified_name":item.qualified_name,
                                "selector":{"kind":"lines", "start":item.start_line, "end":item.end_line}}));
                        }
                    }
                    if !candidates.is_empty() {
                        message_suffix = Some(
                            " The candidates are the code items nearest the requested line, offered as exact line selectors. Proximity does not establish enclosure; confirm the owner before relying on one.",
                        );
                    }
                }
                let mut message = error.for_model();
                if imported {
                    message.push_str(" The candidates are lexical token matches in use declarations, not resolved imports or re-exports. Check aliases, scope and cfg before following a possible owner; no definition or reference relationship is established.");
                }
                if let Some(suffix) = message_suffix {
                    message.push_str(suffix);
                }
                if let Some(elsewhere) = &elsewhere {
                    message.push_str(elsewhere);
                }
                failures.push(
                    serde_json::json!({"selector_index":index, "selector":selector,
                    "status":"invalid_selector", "complete":false, "message":message,
                    "source_sha256":canonical.sha256, "candidates":candidates,
                    "omitted_candidates":candidate_count - candidates.len()}),
                );
            }
        }
    }
    (Some(resolved), failures, bindings)
}

/// Also returns the item bound by the last symbol or enclosing selector.
fn resolve_items(
    items: &[CodeItem],
    selectors: Vec<FileSelector>,
) -> Result<(Vec<ToolOutputSelector>, Option<&CodeItem>), ReadToolOutputError> {
    let error = |message: String| ReadToolOutputError::InvalidRange(message);
    let mut resolved = Vec::new();
    let mut bound = None;
    for selector in selectors {
        let structure = match selector {
            FileSelector::Exact(selector) => {
                resolved.push(selector);
                continue;
            }
            FileSelector::Structure(structure) => structure,
        };
        let item = match structure {
            StructureSelector::Symbol { name } => {
                // A nested suffix must not obscure an exact qualified name.
                // Bare names still require uniqueness across the whole file.
                let exact =
                    (name.contains("::") || name.contains('.') || name.starts_with("impl "))
                        && items
                            .iter()
                            .any(|item| item.qualified_name == name && symbol_matches(item, &name));
                let mut matches = items.iter().filter(|item| {
                    symbol_matches(item, &name) && (!exact || item.qualified_name == name)
                });
                let first = matches.next();
                if first.is_some() && matches.next().is_some() {
                    return Err(ambiguous_symbol(&name));
                }
                match first {
                    Some(item) => item,
                    None => unique_alias(items, &name)?.ok_or_else(|| {
                        error(format!(
                            "symbol {name:?} not found (macros are not expanded)"
                        ))
                    })?,
                }
            }
            StructureSelector::Enclosing { line } => enclosing(items, line)?,
            StructureSelector::Search {
                query,
                enclosing,
                case_insensitive,
                start_byte,
                max_results,
                context_lines,
            } => {
                // Matching, enclosing resolution, and fitting happen once in the
                // snapshot owner, including subsequent immutable search pages.
                resolved.push(ToolOutputSelector::Search {
                    query,
                    enclosing,
                    case_insensitive,
                    start_byte,
                    max_results,
                    context_lines,
                });
                continue;
            }
        };
        resolved.push(ToolOutputSelector::Lines {
            start: item.start_line,
            end: item.end_line,
        });
        bound = Some(item);
    }
    Ok((resolved, bound))
}

#[cfg(test)]
fn resolve(
    path: &str,
    canonical: &mut CanonicalToolResult,
    selectors: Option<Vec<FileSelector>>,
) -> Result<Option<Vec<ToolOutputSelector>>, ReadToolOutputError> {
    let (resolved, failures, _) = resolve_batch(path, canonical, selectors);
    if let Some(failure) = failures.first() {
        return Err(ReadToolOutputError::InvalidRange(
            failure["message"].to_string(),
        ));
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(
        path: &str,
        source: &str,
        selectors: Option<Vec<FileSelector>>,
    ) -> Result<Option<Vec<ToolOutputSelector>>, ReadToolOutputError> {
        super::resolve(
            path,
            &mut CanonicalToolResult::text(source.to_owned()),
            selectors,
        )
    }

    #[test]
    fn exact_units_ignore_braces_in_comments_and_raw_strings() {
        let source = "// λ\r\n#[inline]\r\nfn unit() {\r\n let s = r###\" } { \"###; // }\r\n}\r\nfn other() {}\r\n";
        for selector in [
            StructureSelector::Symbol {
                name: "unit".into(),
            },
            StructureSelector::Enclosing { line: 4 },
        ] {
            assert_eq!(
                resolve(
                    "a.rs",
                    source,
                    Some(vec![FileSelector::Structure(selector)])
                )
                .unwrap(),
                Some(vec![ToolOutputSelector::Lines { start: 2, end: 5 }])
            );
        }
    }

    #[test]
    fn failed_enclosing_lookup_offers_the_nearest_items_as_line_selectors() {
        // A line outside every code item previously returned an empty candidate
        // list, leaving a raw-line guess as the only next step.
        let source = "fn first() {\n    let a = 1;\n}\n\n\n\nfn second() {\n    let b = 2;\n}\n";
        let mut canonical = CanonicalToolResult::text(source.to_owned());
        let (_, failures, _) = resolve_batch(
            "gap.rs",
            &mut canonical,
            Some(vec![FileSelector::Structure(
                StructureSelector::Enclosing { line: 5 },
            )]),
        );
        let failure = failures.first().expect("the enclosing lookup must fail");
        assert_eq!(failure["status"], "invalid_selector");
        let candidates = failure["candidates"]
            .as_array()
            .expect("candidates must be an array");
        assert!(
            !candidates.is_empty(),
            "a failed enclosing lookup must offer nearby items: {failure}"
        );
        // Every candidate is an exact, usable line selector, not a guess.
        for candidate in candidates {
            let selector = &candidate["selector"];
            assert_eq!(selector["kind"], "lines");
            let start = selector["start"].as_u64().expect("start line");
            let end = selector["end"].as_u64().expect("end line");
            assert!(start >= 1 && start <= end, "{candidate}");
        }
        assert!(
            failure["message"]
                .as_str()
                .is_some_and(|message| message.contains("Proximity does not establish enclosure")),
            "the caveat must travel with the candidates: {failure}"
        );
    }

    #[test]
    fn unindexed_symbols_do_not_imply_absence_from_the_snapshot() {
        for (path, source, name) in [
            ("settings.py", "TIMEOUT = 30\n", "TIMEOUT"),
            ("value.rs", "struct Value { field: u32 }\n", "field"),
            ("value.rs", "enum Value { Variant }\n", "Variant"),
            ("value.rs", "fn present() {}\n", "absent"),
        ] {
            let mut canonical = CanonicalToolResult::text(source.to_owned());
            let exact = ToolOutputSelector::Lines { start: 1, end: 1 };
            let (ranges, failures, _) = resolve_batch(
                path,
                &mut canonical,
                Some(vec![
                    FileSelector::Structure(StructureSelector::Symbol { name: name.into() }),
                    FileSelector::Exact(exact.clone()),
                ]),
            );
            assert_eq!(ranges, Some(vec![exact]));
            assert_eq!(failures.len(), 1);
            let message = failures[0]["message"].as_str().unwrap();
            assert!(message.contains("No matching indexed item"));
            assert!(message.contains("retained snapshot"));
            assert!(!message.contains("defined in another file"));
            assert_eq!(failures[0]["source_sha256"], canonical.sha256);
        }
    }

    #[test]
    fn outline_requested_as_a_symbol_offers_the_outline_section() {
        // Recorded sessions sent {kind:"symbol",name:"outline"} and received an
        // empty candidate list; the item listing is the outline section.
        let symbol = |name: &str| {
            Some(vec![FileSelector::Structure(StructureSelector::Symbol {
                name: name.into(),
            })])
        };
        for name in ["outline", "Outline"] {
            let mut canonical = CanonicalToolResult::text("fn present() {}\n".to_owned());
            let (ranges, failures, bindings) = resolve_batch("lib.rs", &mut canonical, symbol(name));
            assert_eq!(ranges, Some(Vec::new()), "a candidate is never executed");
            assert!(bindings.is_empty());
            assert_eq!(
                failures[0]["candidates"],
                serde_json::json!([{"qualified_name":"outline",
                    "selector":{"kind":"section","id":"outline"}}])
            );
            assert_eq!(failures[0]["omitted_candidates"], 0);
            // A valid read_file selector for a section this snapshot has.
            let selector = failures[0]["candidates"][0]["selector"].clone();
            assert_eq!(
                serde_json::from_value::<FileSelector>(selector).unwrap(),
                FileSelector::Exact(ToolOutputSelector::Section {
                    id: "outline".into()
                })
            );
            assert!(canonical.sections.iter().any(|section| section.id == "outline"));
            let message = failures[0]["message"].as_str().unwrap();
            assert!(message.contains("not a symbol"), "{message}");
            assert!(!message.contains("fixed-string search"), "{message}");
        }
        // An item really named outline still resolves as a symbol.
        let mut canonical = CanonicalToolResult::text("fn outline() {}\n".to_owned());
        let (ranges, failures, _) = resolve_batch("lib.rs", &mut canonical, symbol("outline"));
        assert!(failures.is_empty(), "{failures:?}");
        assert_eq!(ranges, Some(vec![ToolOutputSelector::Lines { start: 1, end: 1 }]));
        // Without parsed items there is no outline section to offer.
        let mut canonical = CanonicalToolResult::text("outline\n".to_owned());
        let (_, failures, _) = resolve_batch("notes.txt", &mut canonical, symbol("outline"));
        assert_eq!(failures[0]["candidates"], serde_json::json!([]));
    }

    #[test]
    fn ambiguous_candidates_are_bounded_and_exact_selectors_survive_unsupported_structure() {
        let source = (0..12)
            .map(|i| format!("impl S{i} {{ fn run() {{}} }}\n"))
            .collect::<String>();
        let mut canonical = CanonicalToolResult::text(source);
        let (_, errors, _) = resolve_batch(
            "many.rs",
            &mut canonical,
            Some(vec![FileSelector::Structure(StructureSelector::Symbol {
                name: "run".into(),
            })]),
        );
        assert_eq!(errors[0]["candidates"].as_array().unwrap().len(), 8);
        assert_eq!(errors[0]["omitted_candidates"], 4);
        let exact = ToolOutputSelector::Lines { start: 1, end: 1 };
        let (ranges, errors, _) = resolve_batch(
            "many.txt",
            &mut canonical,
            Some(vec![
                FileSelector::Structure(StructureSelector::Symbol { name: "run".into() }),
                FileSelector::Exact(exact.clone()),
            ]),
        );
        assert_eq!(ranges, Some(vec![exact]));
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0]["source_sha256"], canonical.sha256);
    }

    #[test]
    fn smallest_method_and_explicit_failures() {
        let source = "struct S;\nimpl S {\n fn run() {\n }\n}\nmod other { fn run() {} }\n";
        assert_eq!(
            resolve(
                "a.rs",
                source,
                Some(vec![FileSelector::Structure(
                    StructureSelector::Enclosing { line: 3 }
                )])
            )
            .unwrap(),
            Some(vec![ToolOutputSelector::Lines { start: 3, end: 4 }])
        );
        for (path, source, selector) in [
            (
                "a.rs",
                source,
                StructureSelector::Symbol { name: "run".into() },
            ),
            (
                "a.rs",
                source,
                StructureSelector::Symbol {
                    name: "absent".into(),
                },
            ),
            ("a.rs", source, StructureSelector::Enclosing { line: 0 }),
            (
                "a.rs",
                "fn broken(",
                StructureSelector::Enclosing { line: 1 },
            ),
            (
                "a.rs",
                "fn a() {} fn longer() {}",
                StructureSelector::Enclosing { line: 1 },
            ),
            ("a.txt", source, StructureSelector::Enclosing { line: 1 }),
        ] {
            assert!(resolve(path, source, Some(vec![FileSelector::Structure(selector)])).is_err());
        }
        assert!(resolve("a.rs", "fn broken(", None).unwrap().is_none());
    }

    #[test]
    fn qualified_names_docs_python_and_broken_neighbors() {
        for (path, source, name, start, end) in [
            (
                "a.rs",
                "impl A {\n /// docs\n #[inline]\n fn new() {}\n}\nimpl B { fn new() {} }\nfn broken(",
                "A::new",
                2,
                4,
            ),
            (
                "a.rs",
                "impl<T> A<T> {\n fn new() {}\n}\nimpl B { fn new() {} }",
                "A::new",
                2,
                2,
            ),
            (
                "a.py",
                "class A:\n @decorator\n def new(self):\n  return 1\nclass B:\n def new(self): pass\n",
                "A.new",
                2,
                4,
            ),
            (
                "a.rs",
                "const VALUE: usize = 1;\ntype Alias = usize;\nstatic OTHER: usize = 2;",
                "Alias",
                2,
                2,
            ),
            ("a.rs", "struct A;\nimpl A { fn new() {} }", "A", 1, 1),
        ] {
            assert_eq!(
                resolve(
                    path,
                    source,
                    Some(vec![FileSelector::Structure(StructureSelector::Symbol {
                        name: name.into()
                    })])
                )
                .unwrap(),
                Some(vec![ToolOutputSelector::Lines { start, end }])
            );
        }
    }

    #[test]
    fn symbols_and_enclosing_share_outline_ranges() {
        for (path, source, name, line) in [
            ("a.rs", "/// docs\n#[inline]\nfn target() {}\n", "target", 3),
            (
                "a.py",
                "class A:\n @decorator\n def target(self):\n  return 1\n",
                "A.target",
                4,
            ),
            ("a.rs", "/// docs\nfn target() {}\nfn broken(", "target", 2),
        ] {
            let mut canonical = CanonicalToolResult::text(source.to_owned());
            let items = code_sections::parse(
                path,
                source,
                &canonical.sha256,
                Instant::now() + Duration::from_secs(2),
            )
            .unwrap();
            let item = items
                .iter()
                .find(|item| item.qualified_name == name)
                .unwrap();
            for selector in [
                StructureSelector::Symbol { name: name.into() },
                StructureSelector::Enclosing { line },
            ] {
                let resolved = super::resolve(
                    path,
                    &mut canonical,
                    Some(vec![FileSelector::Structure(selector)]),
                )
                .unwrap();
                assert_eq!(
                    resolved,
                    Some(vec![ToolOutputSelector::Lines {
                        start: item.start_line,
                        end: item.end_line
                    }])
                );
                assert!(
                    canonical
                        .sections
                        .iter()
                        .any(|section| section.id == item.id)
                );
            }
        }
    }

    #[test]
    fn verified10_trait_candidates_resolve_and_search_resolution_does_not_select_twice() {
        let source = "impl First for S { fn run() {} }\nimpl Second for S { fn run() {} }\n";
        let mut canonical = CanonicalToolResult::text(source);
        let (_, errors, _) = resolve_batch(
            "traits.rs",
            &mut canonical,
            Some(vec![FileSelector::Structure(StructureSelector::Symbol {
                name: "run".into(),
            })]),
        );
        assert_eq!(
            errors[0]["candidates"][0]["qualified_name"],
            "<S as First>::run"
        );
        assert_eq!(
            errors[0]["candidates"][1]["qualified_name"],
            "<S as Second>::run"
        );
        for (name, line) in [("<S as First>::run", 1), ("<S as Second>::run", 2)] {
            assert_eq!(
                resolve(
                    "traits.rs",
                    source,
                    Some(vec![FileSelector::Structure(StructureSelector::Symbol {
                        name: name.into()
                    })])
                )
                .unwrap(),
                Some(vec![ToolOutputSelector::Lines {
                    start: line,
                    end: line
                }])
            );
        }
        let searches = ["First", "Second"].map(|query| {
            FileSelector::Structure(StructureSelector::Search {
                query: query.into(),
                enclosing: true,
                case_insensitive: false,
                start_byte: 0,
                max_results: 1,
                context_lines: 0,
            })
        });
        let (resolved, errors, _) =
            resolve_batch("traits.rs", &mut canonical, Some(searches.to_vec()));
        assert!(errors.is_empty());
        let resolved = resolved.unwrap();
        assert_eq!(
            resolved.len(),
            2,
            "the search owner resolves items from the same single discovered page"
        );
        assert!(resolved.iter().all(|selector| matches!(
            selector,
            ToolOutputSelector::Search {
                enclosing: true,
                ..
            }
        )));
        assert!(
            canonical
                .sections
                .iter()
                .any(|section| section.id == "outline")
        );
    }

    #[test]
    fn imported_names_point_at_their_use_declaration() {
        let mut canonical = CanonicalToolResult::text(
            "pub use inner::Thing;\nuse other::{Left, Right};\nfn local() {}\n".to_owned(),
        );
        let (_, errors, _) = resolve_batch(
            "lib.rs",
            &mut canonical,
            Some(vec![
                FileSelector::Structure(StructureSelector::Symbol {
                    name: "Thing".into(),
                }),
                FileSelector::Structure(StructureSelector::Symbol {
                    name: "other::Right".into(),
                }),
                FileSelector::Structure(StructureSelector::Symbol {
                    name: "absent".into(),
                }),
            ]),
        );
        assert_eq!(
            errors[0]["candidates"],
            serde_json::json!([{"qualified_name":"pub use inner::Thing;",
            "selector":{"kind":"lines","start":1,"end":1}}])
        );
        assert!(
            errors[0]["message"]
                .as_str()
                .unwrap()
                .contains("lexical token matches")
        );
        assert_eq!(
            errors[1]["candidates"][0]["selector"],
            serde_json::json!({"kind":"lines","start":2,"end":2})
        );
        assert!(errors[2]["candidates"].as_array().unwrap().is_empty());
        assert!(!errors[2]["message"].as_str().unwrap().contains("imports"));
    }

    #[test]
    fn use_tokens_do_not_establish_imported_symbol_identity() {
        let mut canonical = CanonicalToolResult::text(
            "use original::Thing as Alias;\n#[cfg(any())]\nuse disabled::Ghost;\nmod nested { use elsewhere::Other; }\n",
        );
        let (resolved, errors, bindings) = resolve_batch(
            "lib.rs",
            &mut canonical,
            Some(
                ["original", "Thing", "Alias", "Ghost", "Other"]
                    .into_iter()
                    .map(|name| {
                        FileSelector::Structure(StructureSelector::Symbol { name: name.into() })
                    })
                    .collect(),
            ),
        );
        assert!(resolved.unwrap().is_empty());
        assert!(bindings.is_empty());
        assert_eq!(errors.len(), 5);
        for error in errors {
            assert_eq!(error["complete"], false);
            assert_eq!(error["source_sha256"], canonical.sha256);
            assert_eq!(error["candidates"].as_array().unwrap().len(), 1);
            assert!(
                error["message"]
                    .as_str()
                    .unwrap()
                    .contains("no definition or reference relationship is established")
            );
        }
    }

    #[test]
    fn actionability_trait_repairs_are_suggestions_not_fuzzy_execution() {
        let source = "impl First for S { fn run() {} }\nimpl Second for S { fn run() {} }\nimpl First for Other { fn run() {} }\n";
        let mut canonical = CanonicalToolResult::text(source.to_owned());
        let (resolved, errors, bindings) = resolve_batch(
            "traits.rs",
            &mut canonical,
            Some(vec![
                FileSelector::Structure(StructureSelector::Symbol {
                    name: "S::run".into(),
                }),
                FileSelector::Structure(StructureSelector::Symbol {
                    name: "<S as First>::run".into(),
                }),
            ]),
        );
        assert_eq!(errors[0]["candidates"].as_array().unwrap().len(), 2);
        assert_eq!(
            errors[0]["candidates"][1]["qualified_name"],
            "<S as Second>::run"
        );
        assert_eq!(
            resolved.unwrap(),
            vec![ToolOutputSelector::Lines { start: 1, end: 1 }]
        );
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings[0]["selector_index"], 1);
        assert_eq!(
            bindings[0]["resolved_selectors"],
            serde_json::json!([{"kind":"lines","start":1,"end":1}])
        );
        assert_eq!(bindings[0]["qualified_name"], "<S as First>::run");
        assert_eq!(bindings[0]["kind"], "fn");
        assert!(bindings[0].get("syntax_error").is_none());

        // An enclosing line names its owner, and a broken item says so.
        let mut canonical = CanonicalToolResult::text(
            "impl A {\n fn run() {\n }\n}\nfn broken() { let = ; }\n".to_owned(),
        );
        let (_, errors, bindings) = resolve_batch(
            "owner.rs",
            &mut canonical,
            Some(vec![
                FileSelector::Structure(StructureSelector::Enclosing { line: 3 }),
                FileSelector::Structure(StructureSelector::Symbol {
                    name: "broken".into(),
                }),
            ]),
        );
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(bindings[0]["qualified_name"], "A::run");
        assert_eq!(bindings[1]["syntax_error"], true);
    }

    /// Recorded misses that each cost a retry: a trait method requested by its
    /// Rust path and an impl requested by its source header. Each has exactly
    /// one answer; a bare name shared by several items stays ambiguous.
    #[test]
    fn unique_paths_and_source_headers_resolve_to_their_item() {
        let source = "struct RootDatabase;\nimpl Clone for RootDatabase {\n    fn clone(&self) -> Self { RootDatabase }\n}\nstruct FactSet<T>(Vec<T>);\nimpl<T: Ord + Clone> FactSet<T>\nwhere\n    T: Copy,\n{\n    fn insert(&mut self) {}\n}\nfn collect() {}\nstruct Collector;\nimpl Collector {\n    fn collect(&self) {}\n}\n";
        for (name, start, end, qualified_name) in [
            (
                "RootDatabase::clone",
                3,
                3,
                "<RootDatabase as Clone>::clone",
            ),
            ("impl<T: Ord + Clone> FactSet<T>", 6, 11, "impl FactSet<T>"),
            ("Collector::collect", 15, 15, "Collector::collect"),
        ] {
            let mut canonical = CanonicalToolResult::text(source.to_owned());
            let (resolved, errors, bindings) = resolve_batch(
                "lib.rs",
                &mut canonical,
                Some(vec![FileSelector::Structure(StructureSelector::Symbol {
                    name: name.into(),
                })]),
            );
            assert!(errors.is_empty(), "{name}: {errors:?}");
            assert_eq!(
                resolved,
                Some(vec![ToolOutputSelector::Lines { start, end }]),
                "{name}"
            );
            assert_eq!(bindings[0]["qualified_name"], qualified_name, "{name}");
        }
        assert_eq!(
            outline_impl_spelling("impl<F: Fn() -> u8> Runner<F> where F: Copy {").as_deref(),
            Some("impl Runner<F>")
        );

        // A bare name shared by a function and a method lists both.
        let mut canonical = CanonicalToolResult::text(source.to_owned());
        let (resolved, errors, _) = resolve_batch(
            "lib.rs",
            &mut canonical,
            Some(vec![FileSelector::Structure(StructureSelector::Symbol {
                name: "collect".into(),
            })]),
        );
        assert!(resolved.unwrap().is_empty());
        assert_eq!(errors[0]["candidates"].as_array().unwrap().len(), 2);

        // A miss describes index coverage, not source absence.
        let mut canonical = CanonicalToolResult::text(source.to_owned());
        let (_, errors, _) = resolve_batch(
            "lib.rs",
            &mut canonical,
            Some(vec![FileSelector::Structure(StructureSelector::Symbol {
                name: "callable_arguments".into(),
            })]),
        );
        assert!(errors[0]["candidates"].as_array().unwrap().is_empty());
        assert!(
            errors[0]["message"]
                .as_str()
                .unwrap()
                .contains("No matching indexed item"),
            "{errors:?}"
        );
    }
}
#[cfg(test)]
mod source_cache_regressions {
    use super::*;
    use sha2::Digest;

    #[test]
    fn immutable_syntax_is_reused_but_changed_bytes_and_paths_are_not() {
        let source = "mod neighboring;\nfn cached_fixture() {}\n";
        let sha = hex::encode(sha2::Sha256::digest(source));
        SOURCE_PARSE_COUNT.with(|count| count.set(0));
        let first = parsed_source("/cache-fixture/a/mod.rs", source, &sha).unwrap();
        let second = parsed_source("/cache-fixture/a/mod.rs", source, &sha).unwrap();
        assert_eq!(SOURCE_PARSE_COUNT.with(std::cell::Cell::get), 1);
        assert_eq!(serde_json::to_value(&first).unwrap(), serde_json::to_value(&second).unwrap());
        let elsewhere = parsed_source("/cache-fixture/b/mod.rs", source, &sha).unwrap();
        assert_ne!(first.0[0].linked_path_candidates, elsewhere.0[0].linked_path_candidates);
        let changed = "fn changed_cache_fixture() {}\n";
        let changed_sha = hex::encode(sha2::Sha256::digest(changed));
        let result = parsed_source("/cache-fixture/a/mod.rs", changed, &changed_sha).unwrap();
        assert_eq!(result.0[0].name, "changed_cache_fixture");
        assert_eq!(SOURCE_PARSE_COUNT.with(std::cell::Cell::get), 3);
    }

    #[test]
    fn syntax_recovery_is_never_cached_as_complete() {
        let source = "fn invalid_cache_fixture( {";
        let sha = hex::encode(sha2::Sha256::digest(source));
        SOURCE_PARSE_COUNT.with(|count| count.set(0));
        let _ = parsed_source("/cache-fixture/incomplete.rs", source, &sha);
        let _ = parsed_source("/cache-fixture/incomplete.rs", source, &sha);
        assert_eq!(SOURCE_PARSE_COUNT.with(std::cell::Cell::get), 2);
    }
}
