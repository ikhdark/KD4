//! On-demand source sections shared by file structure selectors.
//! No cache, filesystem traversal, or macro expansion belongs in the parser.
use std::path::Path;
use std::time::Instant;

use codex_tools::CanonicalByteRange;
use codex_tools::ToolProjectionInclusion;
use codex_tools::ToolProjectionSection;
use serde::Serialize;
use serde_json::json;

#[derive(Debug, Serialize)]
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
    /// Conventional locations only; existence is not checked by this parser.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub linked_path_candidates: Vec<String>,
    pub syntax_error: bool,
    /// Diagnostic-only spelling for trait methods; never an executable alias.
    #[serde(skip)]
    pub trait_method_alias: Option<String>,
    #[serde(skip)]
    pub start: usize,
    #[serde(skip)]
    pub end: usize,
}

pub(crate) fn parse(
    path: &str,
    source: &str,
    sha256: &str,
    deadline: Instant,
) -> Option<Vec<CodeItem>> {
    let rust = path.ends_with(".rs");
    let language = if rust {
        tree_sitter_rust::LANGUAGE
    } else if path.ends_with(".py") {
        tree_sitter_python::LANGUAGE
    } else {
        return None;
    };
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&language.into()).ok()?;
    let mut cancelled = |_: &tree_sitter::ParseState| Instant::now() >= deadline;
    let tree = parser.parse_with_options(
        &mut |offset, _| &source.as_bytes()[offset..],
        None,
        Some(tree_sitter::ParseOptions::new().progress_callback(&mut cancelled)),
    )?;
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
            let mut ancestor = node.parent();
            while let Some(parent) = ancestor {
                in_test_context |= test_contexts.get(&parent.id()).copied().unwrap_or(false);
                if matches!(parent.kind(), "impl_item" | "trait_item" | "mod_item" | "function_item" | "class_definition" | "function_definition") {
                    let owner = if parent.kind() == "impl_item" {
                        parent.child_by_field_name("type")
                    } else {
                        parent.child_by_field_name("name")
                    };
                    if let Some(mut owner) = owner {
                        if parent.kind() == "impl_item"
                            && let Some(trait_name) = parent.child_by_field_name("trait")
                        {
                            qualifiers.push(format!("<{} as {}>",
                                &source[owner.byte_range()], &source[trait_name.byte_range()]));
                            if kind == "fn" && trait_alias.is_none() {
                                // Both the type and trait can contain generics;
                                // recover the type from grammar nodes, not text.
                                while owner.kind() == "generic_type" {
                                    let Some(base) = owner.child_by_field_name("type") else { break; };
                                    owner = base;
                                }
                                trait_alias = Some((qualifiers.len() - 1, source[owner.byte_range()].to_owned()));
                            }
                            ancestor = parent.parent();
                            continue;
                        }
                        // `impl<T> Type<T>` is addressed as Type::method.
                        while owner.kind() == "generic_type" {
                            let Some(base) = owner.child_by_field_name("type") else { break; };
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
                    Some(trait_name) => format!("impl {} for {name}", &source[trait_name.byte_range()]),
                    None => format!("impl {name}"),
                }
            } else { name.clone() });
            let qualified_name = qualifiers.join(if rust { "::" } else { "." });
            let mut start_line = node.start_position().row + 1;
            let mut is_test = (!rust
                && (kind == "def" && name.starts_with("test_")
                    || kind == "class" && name.starts_with("Test")))
                || (rust && kind == "mod" && name == "tests");
            let mut linked_path = None;
            if rust {
                let mut sibling = node.prev_named_sibling();
                while let Some(previous) = sibling {
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
                                is_test |= attribute_path.rsplit("::").next() == Some("test");
                                // Only an unconditional cfg(test) is proof of
                                // test-only code; cfg(any(test, feature = ...)) is not.
                                in_test_context |= attribute_path == "cfg"
                                    && source[attribute.byte_range()].chars()
                                        .filter(|ch| !ch.is_whitespace()).collect::<String>() == "cfg(test)";
                                if kind == "mod"
                                    && body.is_none()
                                    && attribute_path == "path"
                                    && let Some(value) = attribute.child_by_field_name("value")
                                    && let Some(value) = rust_string_value(value, source)
                                {
                                    let base = module_directory(path, node, source, false)?;
                                    linked_path = Some(
                                        base.join(value).to_string_lossy().into_owned(),
                                    );
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
            if rust && kind == "mod" && body.is_none() && linked_path.is_none() {
                let base = module_directory(path, node, source, true)?;
                let name = name.trim_start_matches("r#");
                linked_path_candidates.push(base.join(format!("{name}.rs")).to_string_lossy().into_owned());
                linked_path_candidates.push(base.join(name).join("mod.rs").to_string_lossy().into_owned());
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
                syntax_error: node.has_error(),
                trait_method_alias,
                start,
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
                return Some(items);
            }
        }
    }
}

// Resolve only syntax-known directories; never probe the filesystem. A path on
// the first inline module replaces the pending non-mod.rs stem, while ordinary
// inline modules consume it. Explicit leaf paths at file scope use the parent.
fn module_directory(
    path: &str,
    node: tree_sitter::Node<'_>,
    source: &str,
    conventional: bool,
) -> Option<std::path::PathBuf> {
    let path = Path::new(path);
    let mut base = path.parent().unwrap_or_else(|| Path::new("")).to_path_buf();
    let stem = path.file_stem()?.to_str()?;
    let mut pending_stem = (!matches!(stem, "mod" | "lib" | "main")).then_some(stem);
    let mut modules = Vec::new();
    let mut ancestor = node.parent();
    while let Some(parent) = ancestor {
        if parent.kind() == "mod_item" { modules.push(parent); }
        ancestor = parent.parent();
    }
    for module in modules.into_iter().rev() {
        let mut override_path = None;
        let mut sibling = module.prev_named_sibling();
        while let Some(previous) = sibling {
            if !matches!(previous.kind(), "attribute_item" | "line_comment" | "block_comment") { break; }
            if previous.kind() == "attribute_item"
                && let Some(attribute) = previous.named_child(0)
                && let Some(name) = attribute.named_child(0)
                && &source[name.byte_range()] == "path"
                && let Some(value) = attribute.child_by_field_name("value")
            {
                override_path = Some(rust_string_value(value, source)?);
            }
            sibling = previous.prev_named_sibling();
        }
        if let Some(value) = override_path {
            pending_stem = None;
            base.push(value);
        } else {
            if let Some(stem) = pending_stem.take() { base.push(stem); }
            let name = module.child_by_field_name("name")?;
            base.push(source[name.byte_range()].trim_start_matches("r#"));
        }
    }
    if conventional && let Some(stem) = pending_stem { base.push(stem); }
    Some(base)
}

// Decode only the string node selected by the grammar, never parse an attribute
// a second time. Unsupported or malformed literals do not advertise a path.
fn rust_string_value(node: tree_sitter::Node<'_>, source: &str) -> Option<String> {
    if node.has_error() { return None; }
    if node.kind() == "raw_string_literal" {
        return Some(source[node.named_child(0)?.byte_range()].to_owned());
    }
    if node.kind() != "string_literal" { return None; }
    let text = source[node.byte_range()].strip_prefix('"')?.strip_suffix('"')?;
    let mut chars = text.chars().peekable();
    let mut value = String::new();
    while let Some(ch) = chars.next() {
        if ch != '\\' { value.push(ch); continue; }
        value.push(match chars.next()? {
            '\\' => '\\', '"' => '"', '\'' => '\'',
            'n' => '\n', 'r' => '\r', 't' => '\t', '0' => '\0',
            'x' => {
                let a = chars.next()?.to_digit(16)?;
                let b = chars.next()?.to_digit(16)?;
                let byte = a * 16 + b;
                if byte > 127 { return None; }
                char::from_u32(byte)?
            }
            'u' => {
                if chars.next()? != '{' { return None; }
                let mut digits = String::new();
                loop {
                    match chars.next()? {
                        '}' => break,
                        '_' => {},
                        ch if ch.is_ascii_hexdigit() => digits.push(ch),
                        _ => return None,
                    }
                }
                char::from_u32(u32::from_str_radix(&digits, 16).ok()?)?
            }
            '\n' | '\r' => {
                while chars.peek().is_some_and(char::is_ascii_whitespace) { chars.next(); }
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
        sections.push(ToolProjectionSection {
            id: if start == 0 {
                "outline".into()
            } else {
                format!("outline:{start}")
            },
            value: Some(
                json!({"items": &items[start..end], "total_items": items.len(), "next": next}),
            ),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn items(path: &str, source: &str) -> Vec<CodeItem> {
        parse(
            path,
            source,
            "hash",
            Instant::now() + Duration::from_secs(2),
        )
        .unwrap()
    }

    #[test]
    fn rust_items_tests_and_sibling_paths() {
        let source = "struct S;\nimpl S {\n #[tokio::test]\n async fn works() { let s = r#\"} λ\"#; }\n}\n#[path = \"s_tests.rs\"]\nmod tests;\nenum E {}\ntrait T { fn run(); }\n";
        let parsed = items("src/s.rs", source);
        assert_eq!(
            parsed.iter().map(|item| item.kind).collect::<Vec<_>>(),
            ["struct", "impl", "fn", "mod", "enum", "trait", "fn"]
        );
        assert!(parsed[2].is_test);
        assert_eq!((parsed[2].start_line, parsed[2].end_line), (3, 4));
        assert!(source[parsed[2].start..parsed[2].end].starts_with("#[tokio::test]"));
        assert_eq!(
            Path::new(parsed[3].linked_path.as_ref().unwrap()),
            Path::new("src/s_tests.rs")
        );
        assert!(parsed[3].is_test);
    }

    #[test]
    fn attributes_use_tree_sitter_nodes_and_decode_module_paths() {
        for (attribute, expected) in [
            (r##"#[path = r#"raw/path.rs"#]"##, "raw/path.rs"),
            (r#"#[path = "escaped\x2f\u{70}ath.rs"]"#, "escaped/path.rs"),
        ] {
            let source = format!("{attribute}\nmod child;\nfn broken(");
            let parsed = items("src/lib.rs", &source);
            let child = parsed.iter().find(|item| item.name == "child").unwrap();
            assert_eq!(Path::new(child.linked_path.as_ref().unwrap()), Path::new("src").join(expected));
            assert_eq!(child.start_line, 1);
        }
    }

    #[test]
    fn trait_impl_names_and_conventional_module_candidates_are_unambiguous() {
        let parsed = items("src/lib.rs", "impl First for S { fn run() {} }\nimpl Second for S { fn run() {} }\n");
        let names = parsed.iter().filter(|item| item.kind == "fn")
            .map(|item| item.qualified_name.as_str()).collect::<Vec<_>>();
        assert_eq!(names, ["<S as First>::run", "<S as Second>::run"]);
        for (path, source, base) in [
            ("src/lib.rs", "mod child;", "src"),
            ("src/main.rs", "mod child;", "src"),
            ("src/parent/mod.rs", "mod child;", "src/parent"),
            ("src/parent.rs", "mod child;", "src/parent"),
            ("src/parent.rs", "mod inline { mod child; }", "src/parent/inline"),
            ("src/lib.rs", "mod r#type;", "src"),
        ] {
            let parsed = items(path, source);
            let item = parsed.last().unwrap();
            let name = item.name.trim_start_matches("r#");
            assert!(item.linked_path.is_none());
            assert_eq!(item.linked_path_candidates.iter().map(Path::new).collect::<Vec<_>>(),
                [Path::new(base).join(format!("{name}.rs")), Path::new(base).join(name).join("mod.rs")]);
        }
        let parsed = items("src/lib.rs", "#[path = \"elsewhere.rs\"]\nmod child;");
        assert!(parsed[0].linked_path.is_some());
        assert!(parsed[0].linked_path_candidates.is_empty());
    }

    #[test]
    fn python_decorators_nested_items_and_crlf() {
        let parsed = items(
            "a.py",
            "# λ\r\nclass TestExample:\r\n @decorator\r\n async def test_run(self):\r\n  def helper(): pass\r\n  return helper()\r\n",
        );
        assert_eq!(parsed.len(), 3);
        assert!(parsed[0].is_test && parsed[1].is_test && !parsed[2].is_test);
        assert!(parsed.iter().all(|item| item.in_test_context));
        assert_eq!((parsed[1].start_line, parsed[1].end_line), (3, 6));
        assert!(parsed[1].signature.starts_with("async def test_run"));
    }

    #[test]
    fn rust_test_context_does_not_relabel_helpers_as_tests() {
        let parsed = items("a.rs", "#[cfg(test)]\nmod fixtures { fn helper() {} }\nmod tests { fn nested() {} }\nfn production() {}\n");
        for name in ["helper", "nested"] {
            let item = parsed.iter().find(|item| item.name == name).unwrap();
            assert!(!item.is_test);
            assert!(item.in_test_context);
        }
        assert!(!parsed.iter().find(|item| item.name == "production").unwrap().in_test_context);
    }

    #[test]
    fn outlines_are_paged_and_deadlines_do_not_claim_completeness() {
        let parsed = items(
            "a.rs",
            &(0..150)
                .map(|n| format!("fn item_{n}() {{}}\n"))
                .collect::<String>(),
        );
        let sections = sections(&parsed);
        let pages = &sections[150..];
        let first = pages[0].value.as_ref().unwrap()["items"]
            .as_array()
            .unwrap()
            .len();
        assert!(first > 8 && first <= 64);
        assert_eq!(pages[0].children, [format!("outline:{first}")]);
        assert_eq!(
            pages
                .iter()
                .flat_map(|page| page.value.as_ref().unwrap()["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|item| item["name"].as_str().unwrap()))
                .collect::<Vec<_>>(),
            (0..150).map(|n| format!("item_{n}")).collect::<Vec<_>>()
        );
        for (index, page) in pages.iter().enumerate() {
            let next = pages.get(index + 1).map(|page| page.id.clone());
            assert_eq!(page.value.as_ref().unwrap()["next"], json!(next));
            assert_eq!(page.children, next.into_iter().collect::<Vec<_>>());
        }
        assert!(parse("a.rs", "fn x() {}", "hash", Instant::now()).is_none());
        assert!(parse("a.txt", "", "hash", Instant::now()).is_none());
        assert!(pages.len() > 1);
    }
}
