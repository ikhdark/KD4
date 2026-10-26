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


fn parsed_source(path: &str, source: &str, sha256: &str) -> Option<(std::sync::Arc<[CodeItem]>, bool)> {
    if let Some(items) = PARSED_SOURCES.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter().find(|entry| entry.path == path && entry.sha256 == sha256)
        .map(|entry| entry.items.clone())
    {
        return Some((items, true));
    }
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


