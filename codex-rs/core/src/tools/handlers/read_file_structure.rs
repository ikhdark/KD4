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

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(untagged)]
pub(super) enum FileSelector {
    Structure(StructureSelector),
    Exact(ToolOutputSelector),
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum StructureSelector {
    Symbol { name: String },
    Enclosing { line: usize },
    Search {
        query: String,
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

fn default_max_results() -> usize { 20 }
fn default_context_lines() -> usize { 3 }

fn enclosing(items: &[CodeItem], line: usize) -> Result<&CodeItem, ReadToolOutputError> {
    let candidates = || items.iter().filter(|item|
        line > 0 && item.start_line <= line && line <= item.end_line);
    let item = candidates().min_by_key(|item| item.end - item.start)
        .ok_or_else(|| ReadToolOutputError::InvalidRange(format!("no enclosing code item at line {line}")))?;
    if candidates().any(|other| other.start > item.start || other.end < item.end) {
        return Err(ReadToolOutputError::InvalidRange(format!(
            "multiple code items share line {line}; use a symbol or a different line"
        )));
    }
    Ok(item)
}

fn symbol_matches(item: &CodeItem, name: &str) -> bool {
    item.kind != "impl" && (item.name == name || item.qualified_name == name
        || item.qualified_name.ends_with(&format!("::{name}"))
        || item.qualified_name.ends_with(&format!(".{name}")))
}

// Diagnostic-only repair for Type::method when the parsed owner is a trait
// implementation. Never make this looser spelling an executable selector.
fn trait_method_candidate(item: &CodeItem, name: &str) -> bool {
    let Some((owner, method)) = name.rsplit_once("::") else { return false; };
    if item.name != method { return false; }
    let Some(qualified_owner) = item.qualified_name.strip_suffix(&format!("::{method}")) else { return false; };
    let Some((prefix, implementation)) = qualified_owner.rsplit_once('<') else { return false; };
    let Some((ty, trait_name)) = implementation.split_once(" as ") else { return false; };
    trait_name.ends_with('>') && (owner == ty || owner == format!("{prefix}{ty}"))
}

pub(super) fn resolve_batch(
    path: &str,
    canonical: &mut CanonicalToolResult,
    selectors: Option<Vec<FileSelector>>,
) -> (Option<Vec<ToolOutputSelector>>, Vec<serde_json::Value>, Vec<serde_json::Value>) {
    let Some(selectors) = selectors else { return (None, Vec::new(), Vec::new()); };
    let needs_items = selectors.iter().any(|selector| matches!(selector,
        FileSelector::Structure(StructureSelector::Symbol { .. } | StructureSelector::Enclosing { .. }
            | StructureSelector::Search { enclosing: true, .. })
            | FileSelector::Exact(ToolOutputSelector::Section { .. } | ToolOutputSelector::Search { enclosing: true, .. })));
    let items = needs_items.then(|| std::str::from_utf8(&canonical.bytes).ok().and_then(|source|
        code_sections::parse(path, source, &canonical.sha256, Instant::now() + Duration::from_secs(2)))).flatten();
    if let Some(items) = &items {
        canonical.sections = code_sections::sections(items);
    }
    let mut resolved = Vec::new();
    let mut failures = Vec::new();
    let mut bindings = Vec::new();
    for (index, selector) in selectors.into_iter().enumerate() {
        let requires_items = matches!(&selector,
            FileSelector::Structure(StructureSelector::Symbol { .. } | StructureSelector::Enclosing { .. }
                | StructureSelector::Search { enclosing: true, .. })
                | FileSelector::Exact(ToolOutputSelector::Section { .. } | ToolOutputSelector::Search { enclosing: true, .. }));
        let result = if requires_items && items.is_none() {
            Err(ReadToolOutputError::InvalidRange("structural selectors require a .rs or .py file parsed within the time limit; use lines or search otherwise".into()))
        } else {
            resolve_items(items.as_deref().unwrap_or_default(), vec![selector.clone()])
        };
        match result {
            Ok(ranges) => {
                if matches!(&selector, FileSelector::Structure(
                    StructureSelector::Symbol { .. } | StructureSelector::Enclosing { .. }))
                {
                    bindings.push(serde_json::json!({"selector_index":index,
                        "resolved_selectors":ranges}));
                }
                resolved.extend(ranges);
            }
            Err(error) => {
                // Enrichment failure must not suppress an executable search.
                // Keep the error on the requested selector, alongside plain evidence.
                match &selector {
                    FileSelector::Structure(StructureSelector::Search { query, case_insensitive, start_byte, max_results, context_lines, .. }) => {
                        resolved.push(ToolOutputSelector::Search { query: query.clone(), enclosing: false,
                            case_insensitive: *case_insensitive, start_byte: *start_byte,
                            max_results: *max_results, context_lines: *context_lines });
                    }
                    FileSelector::Exact(ToolOutputSelector::Search { .. }) => {
                        if let FileSelector::Exact(mut search) = selector.clone() {
                            if let ToolOutputSelector::Search { enclosing, .. } = &mut search { *enclosing = false; }
                            resolved.push(search);
                        }
                    }
                    _ => {}
                }
                let mut candidates = Vec::new();
                let mut candidate_count = 0;
                if let FileSelector::Structure(StructureSelector::Symbol { name }) = &selector {
                    for item in items.iter().flatten().filter(|item|
                        symbol_matches(item, name) || trait_method_candidate(item, name)) {
                        candidate_count += 1;
                        if candidates.len() < 8 && item.qualified_name.len() <= 1024 {
                            candidates.push(serde_json::json!({"qualified_name":item.qualified_name,
                                "selector":{"kind":"lines", "start":item.start_line, "end":item.end_line}}));
                        }
                    }
                }
                failures.push(serde_json::json!({"selector_index":index, "selector":selector,
                    "status":"invalid_selector", "complete":false, "message":error.for_model(),
                    "source_sha256":canonical.sha256, "candidates":candidates,
                    "omitted_candidates":candidate_count - candidates.len()}));
            }
        }
    }
    (Some(resolved), failures, bindings)
}

fn resolve_items(
    items: &[CodeItem],
    selectors: Vec<FileSelector>,
) -> Result<Vec<ToolOutputSelector>, ReadToolOutputError> {
    let error = |message: String| ReadToolOutputError::InvalidRange(message);
    let mut resolved = Vec::new();
    for selector in selectors {
        let structure = match selector {
            FileSelector::Exact(selector) => { resolved.push(selector); continue; }
            FileSelector::Structure(structure) => structure,
        };
        let item = match structure {
            StructureSelector::Symbol { name } => {
                let mut matches = items.iter().filter(|item| symbol_matches(item, &name));
                let item = matches.next().ok_or_else(|| error(format!("symbol {name:?} not found (macros are not expanded)")))?;
                if matches.next().is_some() {
                    return Err(error(format!("symbol {name:?} is ambiguous; use a qualified name such as Type::method or <Type as Trait>::method (Python: Class.method), or enclosing with a known line")));
                }
                item
            }
            StructureSelector::Enclosing { line } => enclosing(&items, line)?,
            StructureSelector::Search { query, enclosing, case_insensitive, start_byte, max_results, context_lines } => {
                // Matching, enclosing resolution, and fitting happen once in the
                // snapshot owner, including subsequent immutable search pages.
                resolved.push(ToolOutputSelector::Search { query, enclosing, case_insensitive, start_byte, max_results, context_lines });
                continue;
            }
        };
        resolved.push(ToolOutputSelector::Lines { start: item.start_line, end: item.end_line });
    }
    Ok(resolved)
}

#[cfg(test)]
fn resolve(path: &str, canonical: &mut CanonicalToolResult, selectors: Option<Vec<FileSelector>>)
    -> Result<Option<Vec<ToolOutputSelector>>, ReadToolOutputError>
{
    let (resolved, failures, _) = resolve_batch(path, canonical, selectors);
    if let Some(failure) = failures.first() {
        return Err(ReadToolOutputError::InvalidRange(failure["message"].to_string()));
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(path: &str, source: &str, selectors: Option<Vec<FileSelector>>) -> Result<Option<Vec<ToolOutputSelector>>, ReadToolOutputError> {
        super::resolve(path, &mut CanonicalToolResult::text(source.to_owned()), selectors)
    }

    #[test]
    fn exact_units_ignore_braces_in_comments_and_raw_strings() {
        let source = "// λ\r\n#[inline]\r\nfn unit() {\r\n let s = r###\" } { \"###; // }\r\n}\r\nfn other() {}\r\n";
        for selector in [StructureSelector::Symbol { name: "unit".into() }, StructureSelector::Enclosing { line: 4 }] {
            assert_eq!(resolve("a.rs", source, Some(vec![FileSelector::Structure(selector)])).unwrap(),
                Some(vec![ToolOutputSelector::Lines { start: 2, end: 5 }]));
        }
    }

    #[test]
    fn ambiguous_candidates_are_bounded_and_exact_selectors_survive_unsupported_structure() {
        let source = (0..12).map(|i| format!("impl S{i} {{ fn run() {{}} }}\n")).collect::<String>();
        let mut canonical = CanonicalToolResult::text(source);
        let (_, errors, _) = resolve_batch("many.rs", &mut canonical, Some(vec![
            FileSelector::Structure(StructureSelector::Symbol { name:"run".into() })]));
        assert_eq!(errors[0]["candidates"].as_array().unwrap().len(), 8);
        assert_eq!(errors[0]["omitted_candidates"], 4);
        let exact = ToolOutputSelector::Lines { start:1, end:1 };
        let (ranges, errors, _) = resolve_batch("many.txt", &mut canonical, Some(vec![
            FileSelector::Structure(StructureSelector::Symbol { name:"run".into() }),
            FileSelector::Exact(exact.clone())]));
        assert_eq!(ranges, Some(vec![exact]));
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0]["source_sha256"], canonical.sha256);
    }

    #[test]
    fn smallest_method_and_explicit_failures() {
        let source = "struct S;\nimpl S {\n fn run() {\n }\n}\nmod other { fn run() {} }\n";
        assert_eq!(resolve("a.rs", source, Some(vec![FileSelector::Structure(StructureSelector::Enclosing { line: 3 })])).unwrap(),
            Some(vec![ToolOutputSelector::Lines { start: 3, end: 4 }]));
        for (path, source, selector) in [
            ("a.rs", source, StructureSelector::Symbol { name: "run".into() }),
            ("a.rs", source, StructureSelector::Symbol { name: "absent".into() }),
            ("a.rs", source, StructureSelector::Enclosing { line: 0 }),
            ("a.rs", "fn broken(", StructureSelector::Enclosing { line: 1 }),
            ("a.rs", "fn a() {} fn longer() {}", StructureSelector::Enclosing { line: 1 }),
            ("a.txt", source, StructureSelector::Enclosing { line: 1 }),
        ] {
            assert!(resolve(path, source, Some(vec![FileSelector::Structure(selector)])).is_err());
        }
        assert!(resolve("a.rs", "fn broken(", None).unwrap().is_none());
    }

    #[test]
    fn qualified_names_docs_python_and_broken_neighbors() {
        for (path, source, name, start, end) in [
            ("a.rs", "impl A {\n /// docs\n #[inline]\n fn new() {}\n}\nimpl B { fn new() {} }\nfn broken(", "A::new", 2, 4),
            ("a.rs", "impl<T> A<T> {\n fn new() {}\n}\nimpl B { fn new() {} }", "A::new", 2, 2),
            ("a.py", "class A:\n @decorator\n def new(self):\n  return 1\nclass B:\n def new(self): pass\n", "A.new", 2, 4),
            ("a.rs", "const VALUE: usize = 1;\ntype Alias = usize;\nstatic OTHER: usize = 2;", "Alias", 2, 2),
            ("a.rs", "struct A;\nimpl A { fn new() {} }", "A", 1, 1),
        ] {
            assert_eq!(resolve(path, source, Some(vec![FileSelector::Structure(StructureSelector::Symbol { name: name.into() })])).unwrap(),
                Some(vec![ToolOutputSelector::Lines { start, end }]));
        }
    }

    #[test]
    fn symbols_and_enclosing_share_outline_ranges() {
        for (path, source, name, line) in [
            ("a.rs", "/// docs\n#[inline]\nfn target() {}\n", "target", 3),
            ("a.py", "class A:\n @decorator\n def target(self):\n  return 1\n", "A.target", 4),
            ("a.rs", "/// docs\nfn target() {}\nfn broken(", "target", 2),
        ] {
            let mut canonical = CanonicalToolResult::text(source.to_owned());
            let items = code_sections::parse(path, source, &canonical.sha256,
                Instant::now() + Duration::from_secs(2)).unwrap();
            let item = items.iter().find(|item| item.qualified_name == name).unwrap();
            for selector in [StructureSelector::Symbol { name: name.into() }, StructureSelector::Enclosing { line }] {
                let resolved = super::resolve(path, &mut canonical, Some(vec![FileSelector::Structure(selector)])).unwrap();
                assert_eq!(resolved, Some(vec![ToolOutputSelector::Lines { start: item.start_line, end: item.end_line }]));
                assert!(canonical.sections.iter().any(|section| section.id == item.id));
            }
        }
    }

    #[test]
    fn verified10_trait_candidates_resolve_and_search_resolution_does_not_select_twice() {
        let source = "impl First for S { fn run() {} }\nimpl Second for S { fn run() {} }\n";
        let mut canonical = CanonicalToolResult::text(source);
        let (_, errors, _) = resolve_batch("traits.rs", &mut canonical, Some(vec![
            FileSelector::Structure(StructureSelector::Symbol { name:"run".into() })]));
        assert_eq!(errors[0]["candidates"][0]["qualified_name"], "<S as First>::run");
        assert_eq!(errors[0]["candidates"][1]["qualified_name"], "<S as Second>::run");
        for (name, line) in [("<S as First>::run", 1), ("<S as Second>::run", 2)] {
            assert_eq!(resolve("traits.rs", source, Some(vec![FileSelector::Structure(
                StructureSelector::Symbol { name:name.into() })])).unwrap(),
                Some(vec![ToolOutputSelector::Lines { start:line, end:line }]));
        }
        let searches = ["First", "Second"].map(|query| FileSelector::Structure(StructureSelector::Search {
            query:query.into(), enclosing:true, case_insensitive:false, start_byte:0, max_results:1, context_lines:0 }));
        let (resolved, errors, _) = resolve_batch("traits.rs", &mut canonical, Some(searches.to_vec()));
        assert!(errors.is_empty());
        let resolved = resolved.unwrap();
        assert_eq!(resolved.len(), 2, "the search owner resolves items from the same single discovered page");
        assert!(resolved.iter().all(|selector| matches!(selector, ToolOutputSelector::Search { enclosing:true, .. })));
        assert!(canonical.sections.iter().any(|section| section.id == "outline"));
    }

    #[test]
    fn actionability_trait_repairs_are_suggestions_not_fuzzy_execution() {
        let source = "impl First for S { fn run() {} }\nimpl Second for S { fn run() {} }\nimpl First for Other { fn run() {} }\n";
        let mut canonical = CanonicalToolResult::text(source.to_owned());
        let (resolved, errors, bindings) = resolve_batch("traits.rs", &mut canonical, Some(vec![
            FileSelector::Structure(StructureSelector::Symbol {name:"S::run".into()}),
            FileSelector::Structure(StructureSelector::Symbol {name:"<S as First>::run".into()}),
        ]));
        assert_eq!(errors[0]["candidates"].as_array().unwrap().len(), 2);
        assert_eq!(errors[0]["candidates"][1]["qualified_name"], "<S as Second>::run");
        assert_eq!(resolved.unwrap(), vec![ToolOutputSelector::Lines {start:1, end:1}]);
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings[0]["selector_index"], 1);
        assert_eq!(bindings[0]["resolved_selectors"], serde_json::json!([{"kind":"lines","start":1,"end":1}]));
    }
}
