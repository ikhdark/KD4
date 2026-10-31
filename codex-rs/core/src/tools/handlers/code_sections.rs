//! On-demand source parsing shared by structure selectors and patch advisories.
//! No cache, filesystem traversal, or macro expansion belongs in the parser.
use std::path::Path;
use std::time::Instant;

use codex_tools::CanonicalByteRange;
use codex_tools::ToolProjectionInclusion;
use codex_tools::ToolProjectionSection;
use serde::Serialize;
use serde_json::json;

#[derive(Clone, Debug, Serialize)]
pub(crate) struct CodeItem {
    pub id: String,
    pub kind: &'static str,
    pub name: String,
    pub qualified_name: String,
    pub signature: String,
    pub signature_truncated: bool,
    pub start_line: usize,
    pub end_line: usize,
    pub definition_line: usize,
    pub is_test: bool,
    pub in_test_context: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub linked_path: Option<String>,
    /// Possible locations when file context is unknown; existence is not checked.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub linked_path_candidates: Vec<String>,
    #[serde(skip)]
    module_candidate_group_size: usize,
    /// The item or a Rust owner header used for its qualified identity has errors.
    pub syntax_error: bool,
    /// Diagnostic-only spelling for trait methods; never an executable alias.
    #[serde(skip)]
    pub trait_method_alias: Option<String>,
    #[serde(skip)]
    pub start: usize,
    #[serde(skip)]
    pub header_end: usize,
    #[serde(skip)]
    pub end: usize,
}

fn source_language(path: &str) -> Option<(&'static str, tree_sitter::Language)> {
    if path.ends_with(".rs") {
        Some(("Rust", tree_sitter_rust::LANGUAGE.into()))
    } else if path.ends_with(".py") {
        Some(("Python", tree_sitter_python::LANGUAGE.into()))
    } else {
        None
    }
}

fn parse_source(path: &str, source: &str, deadline: Instant) -> Option<tree_sitter::Tree> {
    if Instant::now() >= deadline {
        return None;
    }
    let (_, language) = source_language(path)?;
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&language).ok()?;
    let mut cancelled = |_: &tree_sitter::ParseState| {
        if Instant::now() >= deadline {
            std::ops::ControlFlow::Break(())
        } else {
            std::ops::ControlFlow::Continue(())
        }
    };
    parser.parse_with_options(
        &mut |offset, _| &source.as_bytes()[offset..],
        None,
        Some(tree_sitter::ParseOptions::new().progress_callback(&mut cancelled)),
    )
}

/// Count recovery nodes, not compiler diagnostics. Do not count nested ERROR
/// nodes again: parser recovery can otherwise multiply one broken construct.
fn recovery_error_count(tree: &tree_sitter::Tree, deadline: Instant) -> Option<usize> {
    let mut count = 0;
    let mut cursor = tree.walk();
    loop {
        if Instant::now() >= deadline {
            return None;
        }
        let node = cursor.node();
        if node.is_error() || node.is_missing() {
            count += 1;
        } else if node.has_error() && cursor.goto_first_child() {
            continue;
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return Some(count);
            }
        }
    }
}

/// Deliberately conservative: only an increased recovery count is advisory
/// evidence of a possible regression. Equal/decreased counts may hide new errors,
/// but must not blame moved or differently recovered pre-existing errors.
///
/// The caller compares one receipt's files against one shared deadline and
/// owns `budget_reported`: exhaustion is reported once, on the first file it
/// stops; later files under that expired deadline return None.
pub(crate) fn syntax_regression(
    before_path: &str,
    path: &str,
    before: &str,
    after: &str,
    deadline: Instant,
    budget_reported: &mut bool,
) -> Option<serde_json::Value> {
    if before == after {
        return None;
    }
    let (language, _) = source_language(path)?;
    let skipped = |reason| json!({
        "kind": "syntax_check_skipped", "path": path, "language": language,
        "advisory": true, "reason": reason,
        "message": "Syntax comparison unavailable; no compilation or correctness evidence.",
    });
    if source_language(before_path).map(|(name, _)| name) != Some(language) {
        return Some(skipped("language_changed"));
    }
    // Bound synchronous advisory work across the entire receipt, using the
    // caller's shared deadline. Never make patch success depend on parsing.
    if before.len().max(after.len()) > 1024 * 1024 {
        return Some(skipped("source_size_limit"));
    }
    let compare = || {
        let old = parse_source(path, before, deadline)?;
        let old_count = recovery_error_count(&old, deadline)?;
        let new = parse_source(path, after, deadline)?;
        let new_count = recovery_error_count(&new, deadline)?;
        Some((old_count, new_count))
    };
    let Some((before_errors, after_errors)) = compare() else {
        let mut skip = skipped("parser_unavailable_or_budget_exhausted");
        if Instant::now() >= deadline {
            if std::mem::replace(budget_reported, true) {
                return None;
            }
            skip["message"] = json!("Shared syntax-check time budget exhausted; this and later Rust/Python files in this patch were not compared. No compilation or correctness evidence.");
        }
        return Some(skip);
    };
    (after_errors > before_errors).then(|| json!({
        "kind": "syntax_regression", "path": path, "language": language,
        "parser": "tree-sitter", "advisory": true,
        "before_errors": before_errors, "after_errors": after_errors,
        "message": format!("Possible {language} syntax regression: parser recovery errors increased from {before_errors} to {after_errors}. Parsing is not compilation or correctness evidence."),
    }))
}

pub(super) fn parse_with_completeness(
    path: &str,
    source: &str,
    sha256: &str,
    deadline: Instant,
) -> Option<(Vec<CodeItem>, bool)> {
    let rust = path.ends_with(".rs");
    let tree = parse_source(path, source, deadline)?;
    let complete = !tree.root_node().has_error();
    let mut items = Vec::new();
    let mut test_contexts = std::collections::HashMap::new();
    let mut cursor = tree.walk();
    loop {
        if Instant::now() >= deadline {
            return None;
        }
        let node = cursor.node();
        let kind = match node.kind() {
            "function_item" | "function_signature_item" => Some("fn"),
            "struct_item" => Some("struct"),
            "enum_item" => Some("enum"),
            "trait_item" => Some("trait"),
            "impl_item" => Some("impl"),
            "mod_item" => Some("mod"),
            "const_item" => Some("const"),
            "static_item" => Some("static"),
            "type_item" | "associated_type" => Some("type"),
            "union_item" => Some("union"),
            "macro_definition" => Some("macro"),
            "foreign_mod_item" => Some("extern"),
            "extern_crate_declaration" => Some("extern_crate"),
            "use_declaration" => Some("use"),
            "function_definition" => Some("def"),
            "class_definition" => Some("class"),
            _ => None,
        };
        if let Some(kind) = kind {
            let body = node.child_by_field_name("body");
            let signature_end = body.map_or(node.end_byte(), |body| body.start_byte());
            let signature = source[node.start_byte()..signature_end].trim();
            let name = node
                .child_by_field_name("name")
                .or_else(|| node.child_by_field_name("type"))
                .map(|name| source[name.byte_range()].to_owned())
                .unwrap_or_else(|| kind.to_owned());
            let mut start = node.start_byte();
            let mut qualifiers = Vec::new();
            let mut trait_alias = None;
            let mut in_test_context = false;
            let mut syntax_error = node.has_error();
            let mut ancestor = node.parent();
            while let Some(parent) = ancestor {
                in_test_context |= test_contexts.get(&parent.id()).copied().unwrap_or(false);
                if matches!(
                    parent.kind(),
                    "impl_item"
                        | "trait_item"
                        | "mod_item"
                        | "function_item"
                        | "class_definition"
                        | "function_definition"
                ) {
                    if rust && parent.has_error() {
                        // Qualifiers also depend on recovered owner syntax. Inspect
                        // only the header: another method's broken body must not
                        // taint this item's otherwise sound identity.
                        let header_end = parent
                            .child_by_field_name("body")
                            .map_or(parent.end_byte(), |body| body.start_byte());
                        let mut header_cursor = parent.walk();
                        syntax_error |= parent
                            .children(&mut header_cursor)
                            .filter(|child| child.end_byte() <= header_end)
                            .any(|child| child.has_error() || child.is_missing());
                    }
                    let owner = if parent.kind() == "impl_item" {
                        parent.child_by_field_name("type")
                    } else {
                        parent.child_by_field_name("name")
                    };
                    if let Some(mut owner) = owner {
                        if parent.kind() == "impl_item"
                            && let Some(trait_name) = parent.child_by_field_name("trait")
                        {
                            qualifiers.push(format!(
                                "<{} as {}>",
                                &source[owner.byte_range()],
                                &source[trait_name.byte_range()]
                            ));
                            if kind == "fn" && trait_alias.is_none() {
                                // Both the type and trait can contain generics;
                                // recover the type from grammar nodes, not text.
                                while owner.kind() == "generic_type" {
                                    let Some(base) = owner.child_by_field_name("type") else {
                                        break;
                                    };
                                    owner = base;
                                }
                                trait_alias = Some((
                                    qualifiers.len() - 1,
                                    source[owner.byte_range()].to_owned(),
                                ));
                            }
                            ancestor = parent.parent();
                            continue;
                        }
                        // `impl<T> Type<T>` is addressed as Type::method.
                        while owner.kind() == "generic_type" {
                            let Some(base) = owner.child_by_field_name("type") else {
                                break;
                            };
                            owner = base;
                        }
                        qualifiers.push(source[owner.byte_range()].to_owned());
                    }
                }
                ancestor = parent.parent();
            }
            qualifiers.reverse();
            let trait_method_alias = trait_alias.map(|(index, owner)| {
                let mut alias = qualifiers.clone();
                let index = alias.len() - index - 1;
                alias[index] = owner;
                alias.push(name.clone());
                alias.join("::")
            });
            // Keep outline names compatible while giving impl blocks a
            // selectable identity distinct from the type definition.
            qualifiers.push(if kind == "impl" {
                match node.child_by_field_name("trait") {
                    Some(trait_name) => {
                        format!("impl {} for {name}", &source[trait_name.byte_range()])
                    }
                    None => format!("impl {name}"),
                }
            } else {
                name.clone()
            });
            let qualified_name = qualifiers.join(if rust { "::" } else { "." });
            let mut start_line = node.start_position().row + 1;
            let mut is_test = (!rust
                && (kind == "def" && name.starts_with("test_")
                    || kind == "class" && name.starts_with("Test")))
                || (rust && kind == "mod" && name == "tests");
            let mut linked_path = None;
            let mut module_path = None;
            let mut conditional_path = false;
            if rust {
                let mut sibling = node.prev_named_sibling();
                while let Some(previous) = sibling {
                    // Each step rescans earlier siblings, so a long comment or
                    // attribute run is quadratic; it must not outlive the deadline.
                    if Instant::now() >= deadline {
                        return None;
                    }
                    if !matches!(
                        previous.kind(),
                        "attribute_item" | "line_comment" | "block_comment"
                    ) {
                        break;
                    }
                    if previous.kind() == "attribute_item" {
                        start = previous.start_byte();
                        start_line = previous.start_position().row + 1;
                        if let Some(attribute) = previous.named_child(0)
                            && let Some(path_node) = attribute.named_child(0)
                        {
                            let attribute_path = &source[path_node.byte_range()];
                            // cfg_attr can supply a path directly or through another
                            // cfg_attr. Do not evaluate configuration in this parser.
                            conditional_path |= attribute_path == "cfg_attr";
                            is_test |= attribute_path.rsplit("::").next() == Some("test");
                            // Known parameterized-test syntax, not macro expansion or
                            // proof of active cfg, imported identity, or generated cases.
                            is_test |= kind == "fn"
                                && !previous.has_error()
                                && matches!(attribute_path, "test_case" | "test_case::test_case");
                            // Only an unconditional cfg(test) is proof of
                            // test-only code; cfg(any(test, feature = ...)) is not.
                            in_test_context |= attribute_path == "cfg"
                                && source[attribute.byte_range()]
                                    .chars()
                                    .filter(|ch| !ch.is_whitespace())
                                    .collect::<String>()
                                    == "cfg(test)";
                            if kind == "mod"
                                && body.is_none()
                                && attribute_path == "path"
                                && let Some(value) = attribute.child_by_field_name("value")
                                && let Some(value) = rust_string_value(value, source)
                            {
                                module_path = Some(value);
                            }
                        }
                    } else {
                        let comment = &source[previous.byte_range()];
                        if (comment.starts_with("///") && !comment.starts_with("////"))
                            || (comment.starts_with("/**") && !comment.starts_with("/***"))
                        {
                            start = previous.start_byte();
                            start_line = previous.start_position().row + 1;
                        }
                    }
                    sibling = previous.prev_named_sibling();
                }
            } else if let Some(parent) = node
                .parent()
                .filter(|parent| parent.kind() == "decorated_definition")
            {
                start = parent.start_byte();
                start_line = parent.start_position().row + 1;
            }
            // Bound outline metadata; the complete signature remains in the exact section.
            let mut linked_path_candidates = Vec::new();
            let module_candidate_group_size = if module_path.is_some() { 1 } else { 2 };
            if rust && kind == "mod" && body.is_none() && !conditional_path {
                let bases = module_directories(path, node, source, module_path.is_none(), deadline)
                    .unwrap_or_default();
                if let Some(value) = module_path {
                    for base in bases {
                        let candidate = base.join(&value).to_string_lossy().into_owned();
                        if !linked_path_candidates.contains(&candidate) {
                            linked_path_candidates.push(candidate);
                        }
                    }
                    if linked_path_candidates.len() == 1 {
                        linked_path = linked_path_candidates.pop();
                    }
                } else {
                    let name = name.trim_start_matches("r#");
                    for base in bases {
                        linked_path_candidates.push(
                            base.join(format!("{name}.rs"))
                                .to_string_lossy()
                                .into_owned(),
                        );
                        linked_path_candidates.push(
                            base.join(name)
                                .join("mod.rs")
                                .to_string_lossy()
                                .into_owned(),
                        );
                    }
                }
            }
            let signature_limit = signature.floor_char_boundary(512);
            in_test_context |= is_test;
            test_contexts.insert(node.id(), in_test_context);
            items.push(CodeItem {
                id: format!("code:{sha256}:{}", node.start_byte()),
                kind,
                name,
                qualified_name,
                signature: signature[..signature_limit].to_owned(),
                signature_truncated: signature_limit < signature.len(),
                start_line,
                end_line: node.end_position().row + 1,
                definition_line: node
                    .child_by_field_name("name")
                    .map_or(node.start_position().row + 1, |name| {
                        name.start_position().row + 1
                    }),
                is_test,
                in_test_context,
                linked_path,
                linked_path_candidates,
                module_candidate_group_size,
                syntax_error,
                trait_method_alias,
                start,
                header_end: signature_end,
                end: node.end_byte(),
            });
        }
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return Some((items, complete));
            }
        }
    }
}

/// Resolve the parser's context-dependent module candidates when a compilation
/// owner supplies crate-root context. No directory enumeration or macro
/// evaluation is performed; missing declared modules remain named inputs.
pub(crate) fn compilation_module_paths(path: &Path, source: &str, crate_root: bool) -> (Vec<std::path::PathBuf>, bool) {
    let Some((items, complete)) = parse_with_completeness(&path.to_string_lossy(), source, "compilation-input",
        Instant::now() + std::time::Duration::from_secs(2)) else { return (Vec::new(), false); };
    let use_first = crate_root || path.file_stem().is_some_and(|stem| stem == "mod");
    let paths = items.into_iter().filter(|item| item.kind == "mod" && !item.in_test_context).flat_map(|item| {
        if let Some(path) = item.linked_path { return vec![std::path::PathBuf::from(path)]; }
        let count = item.module_candidate_group_size.min(item.linked_path_candidates.len());
        let start = if use_first { 0 } else { item.linked_path_candidates.len() - count };
        item.linked_path_candidates.into_iter().skip(start).take(count)
            .map(std::path::PathBuf::from).collect::<Vec<_>>()
    }).collect();
    (paths, complete)
}

/// Whether `after` removes or rewrites a test item of `before`: an item under
/// a test attribute or `#[cfg(test)]` (every item when `whole_file_is_test`),
/// compared by qualified name, occurrence count, exact text and enclosing
/// execution-policy headers. Non-declaration test-scope statements retain
/// fixtures/oracles without treating a new sibling test as an existing edit.
/// `None` when a side cannot be parsed in time.
pub(crate) fn existing_tests_changed(
    path: &str,
    before: &str,
    after: Option<&str>,
    whole_file_is_test: bool,
    deadline: Instant,
) -> Option<bool> {
    // Test fingerprint (identity, text, enclosing headers) -> occurrence count.
    type TestOccurrences<'a> =
        std::collections::HashMap<(String, Vec<&'a str>, Vec<&'a str>), usize>;

    fn tests<'a>(
        path: &str,
        source: &'a str,
        whole_file_is_test: bool,
        deadline: Instant,
    ) -> Option<TestOccurrences<'a>> {
        struct Scope<'a> {
            end: usize,
            cursor: usize,
            name: String,
            header: &'a str,
            active: bool,
            statements: Vec<&'a str>,
        }
        impl<'a> Scope<'a> {
            fn collect(&mut self, source: &'a str, end: usize, deadline: Instant) -> Option<()> {
                if self.active {
                    for line in source[self.cursor..end].lines() {
                        if Instant::now() >= deadline {
                            return None;
                        }
                        let line = line.trim();
                        if !matches!(line, "" | "{" | "}") {
                            self.statements.push(line);
                        }
                    }
                }
                self.cursor = end;
                Some(())
            }
        }
        let mut tests = std::collections::HashMap::new();
        let mut owners = vec![Scope {
            end: source.len(),
            cursor: 0,
            name: String::new(),
            header: "",
            active: whole_file_is_test,
            statements: Vec::new(),
        }];
        let (items, complete) = parse_with_completeness(path, source, "", deadline)?;
        if !complete {
            return None;
        }
        for item in items {
            if Instant::now() >= deadline || item.syntax_error {
                return None;
            }
            while owners.len() > 1 && owners.last()?.end <= item.start {
                let mut owner = owners.pop()?;
                owner.collect(source, owner.end, deadline)?;
                if owner.active && !owner.statements.is_empty() {
                    let headers = owners
                        .iter()
                        .map(|owner| owner.header)
                        .chain(std::iter::once(owner.header))
                        .collect();
                    *tests
                        .entry((format!("scope:{}", owner.name), owner.statements, headers))
                        .or_insert(0) += 1;
                }
            }
            let owner = owners.last_mut()?;
            // A leaf is fingerprinted in full; do not separately traverse its
            // nested declarations or use their offsets as occurrence identity.
            if item.start < owner.cursor {
                continue;
            }
            owner.collect(source, item.start, deadline)?;
            owner.cursor = item.end;
            if matches!(item.kind, "mod" | "impl" | "trait" | "class" | "extern") {
                owners.push(Scope {
                    end: item.end,
                    cursor: item.header_end,
                    name: item.qualified_name,
                    header: &source[item.start..item.header_end],
                    active: whole_file_is_test || item.in_test_context,
                    statements: Vec::new(),
                });
            } else if whole_file_is_test || item.in_test_context {
                *tests
                    .entry((
                        format!("item:{}", item.qualified_name),
                        vec![&source[item.start..item.end]],
                        owners.iter().map(|owner| owner.header).collect(),
                    ))
                    .or_insert(0) += 1;
            }
        }
        while let Some(mut owner) = owners.pop() {
            owner.collect(source, owner.end, deadline)?;
            if owner.active && !owner.statements.is_empty() {
                let headers = owners
                    .iter()
                    .map(|owner| owner.header)
                    .chain(std::iter::once(owner.header))
                    .collect();
                *tests
                    .entry((format!("scope:{}", owner.name), owner.statements, headers))
                    .or_insert(0) += 1;
            }
        }
        Some(tests)
    }
    let before = tests(path, before, whole_file_is_test, deadline)?;
    let after = match after {
        Some(after) => tests(path, after, whole_file_is_test, deadline)?,
        None => std::collections::HashMap::new(),
    };
    Some(
        before
            .iter()
            .any(|(evidence, count)| after.get(evidence).copied().unwrap_or(0) < *count),
    )
}

// A standalone file does not establish its crate-root or parent #[path] context.
// Keep both directory possibilities for non-mod.rs files, including lib/main.rs.
// An inline path replaces the pending stem; ordinary inline modules consume it.
// Explicit leaf paths at file scope use the parent in either context.
fn module_directories(
    path: &str,
    node: tree_sitter::Node<'_>,
    source: &str,
    conventional: bool,
    deadline: Instant,
) -> Option<Vec<std::path::PathBuf>> {
    let path = Path::new(path);
    let base = path.parent().unwrap_or_else(|| Path::new("")).to_path_buf();
    let stem = path.file_stem()?.to_str()?;
    let mut bases = vec![(base.clone(), None)];
    if stem != "mod" {
        bases.push((base, Some(stem)));
    }
    let mut modules = Vec::new();
    let mut ancestor = node.parent();
    while let Some(parent) = ancestor {
        if parent.kind() == "mod_item" {
            modules.push(parent);
        }
        ancestor = parent.parent();
    }
    for module in modules.into_iter().rev() {
        let mut override_path = None;
        let mut sibling = module.prev_named_sibling();
        while let Some(previous) = sibling {
            if Instant::now() >= deadline {
                return None;
            }
            if !matches!(
                previous.kind(),
                "attribute_item" | "line_comment" | "block_comment"
            ) {
                break;
            }
            if previous.kind() == "attribute_item"
                && let Some(attribute) = previous.named_child(0)
                && let Some(name) = attribute.named_child(0)
            {
                match &source[name.byte_range()] {
                    "cfg_attr" => return Some(Vec::new()),
                    "path" => {
                        let value = attribute.child_by_field_name("value")?;
                        override_path = Some(rust_string_value(value, source)?);
                    }
                    _ => {}
                }
            }
            sibling = previous.prev_named_sibling();
        }
        if let Some(value) = override_path {
            for (base, pending_stem) in &mut bases {
                *pending_stem = None;
                base.push(&value);
            }
        } else {
            let name = module.child_by_field_name("name")?;
            for (base, pending_stem) in &mut bases {
                if let Some(stem) = pending_stem.take() {
                    base.push(stem);
                }
                base.push(source[name.byte_range()].trim_start_matches("r#"));
            }
        }
    }
    let mut directories = Vec::new();
    for (mut base, pending_stem) in bases {
        if conventional && let Some(stem) = pending_stem {
            base.push(stem);
        }
        if !directories.contains(&base) {
            directories.push(base);
        }
    }
    Some(directories)
}

// Decode only the string node selected by the grammar, never parse an attribute
// a second time. Unsupported or malformed literals do not advertise a path.
fn rust_string_value(node: tree_sitter::Node<'_>, source: &str) -> Option<String> {
    if node.has_error() {
        return None;
    }
    if node.kind() == "raw_string_literal" {
        return Some(source[node.named_child(0)?.byte_range()].to_owned());
    }
    if node.kind() != "string_literal" {
        return None;
    }
    let text = source[node.byte_range()]
        .strip_prefix('"')?
        .strip_suffix('"')?;
    let mut chars = text.chars().peekable();
    let mut value = String::new();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            value.push(ch);
            continue;
        }
        value.push(match chars.next()? {
            '\\' => '\\',
            '"' => '"',
            '\'' => '\'',
            'n' => '\n',
            'r' => '\r',
            't' => '\t',
            '0' => '\0',
            'x' => {
                let a = chars.next()?.to_digit(16)?;
                let b = chars.next()?.to_digit(16)?;
                let byte = a * 16 + b;
                if byte > 127 {
                    return None;
                }
                char::from_u32(byte)?
            }
            'u' => {
                if chars.next()? != '{' {
                    return None;
                }
                let mut digits = String::new();
                loop {
                    match chars.next()? {
                        '}' => break,
                        '_' => {}
                        ch if ch.is_ascii_hexdigit() => digits.push(ch),
                        _ => return None,
                    }
                }
                char::from_u32(u32::from_str_radix(&digits, 16).ok()?)?
            }
            '\n' | '\r' => {
                while chars.peek().is_some_and(char::is_ascii_whitespace) {
                    chars.next();
                }
                continue;
            }
            _ => return None,
        });
    }
    Some(value)
}

pub(crate) fn sections(items: &[CodeItem]) -> Vec<ToolProjectionSection> {
    let mut sections = items
        .iter()
        .map(|item| ToolProjectionSection {
            id: item.id.clone(),
            value: None,
            exact_bytes: (item.end - item.start) as u64,
            inclusion: ToolProjectionInclusion::Included,
            canonical_range: Some(CanonicalByteRange::new(item.start as u64, item.end as u64)),
            children: Vec::new(),
            recovery_chunk_bytes: None,
        })
        .collect::<Vec<_>>();
    // Imports are not definitions: one row each used to fill the first pages
    // of a large file before any item. Their sections above stay addressable;
    // the directory only summarizes where they are.
    let mut import_ranges = Vec::<(usize, usize, usize)>::new();
    let mut previous_was_import = false;
    for item in items {
        let import = item.kind == "use";
        if import {
            match import_ranges.last_mut().filter(|_| previous_was_import) {
                Some(run) => {
                    run.1 = item.end_line;
                    run.2 += 1;
                }
                None => import_ranges.push((item.start_line, item.end_line, 1)),
            }
        }
        previous_was_import = import;
    }
    let items = items
        .iter()
        .filter(|item| item.kind != "use")
        .collect::<Vec<_>>();
    let mut outline = outline_sections(&items);
    if let Some(value) = outline.first_mut().and_then(|section| section.value.as_mut())
        && !import_ranges.is_empty()
    {
        value["import_ranges"] = import_ranges.iter().take(16)
            .map(|(start_line, end_line, count)| json!({"start_line": start_line, "end_line": end_line, "count": count}))
            .collect();
        if import_ranges.len() > 16 {
            value["omitted_import_ranges"] = (import_ranges.len() - 16).into();
        }
    }
    sections.extend(outline);
    sections
}

/// Shared bounded directory pages; exact source ranges remain with their owner.
pub(super) fn outline_sections(items: &[impl Serialize]) -> Vec<ToolProjectionSection> {
    let mut sections = Vec::new();
    // Fit ordinary outlines in one response instead of forcing a round trip
    // per handful of items. Larger outlines retain bounded directory pages.
    let mut start = 0;
    loop {
        let mut end = start;
        let mut bytes = 0usize;
        while end < items.len() && end - start < 64 {
            let item_bytes = serde_json::to_vec(&items[end]).map_or(usize::MAX, |item| item.len());
            if end > start && bytes.saturating_add(item_bytes) > 16 * 1024 {
                break;
            }
            bytes = bytes.saturating_add(item_bytes);
            end += 1;
        }
        let next = (end < items.len()).then(|| format!("outline:{end}"));
        let mut value =
            json!({"items": &items[start..end], "total_items": items.len(), "next": next});
        value["parse_complete"] = true.into();
        sections.push(ToolProjectionSection {
            id: if start == 0 {
                "outline".into()
            } else {
                format!("outline:{start}")
            },
            value: Some(value),
            exact_bytes: 0,
            inclusion: ToolProjectionInclusion::Directory,
            canonical_range: None,
            children: next.into_iter().collect(),
            recovery_chunk_bytes: None,
        });
        if end == items.len() {
            break;
        }
        start = end;
    }
    sections
}

