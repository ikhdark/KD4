//! Advisory detection for explicit executable stubs in committed source changes.
use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

use tree_sitter::Node;
use tree_sitter::Parser;

use crate::AppliedPatchDelta;
use crate::AppliedPatchFileChange;

/// Inspect the net committed change, not patch text. Failed and rolled-back
/// hunks are not candidates. This advisory never affects the receipt.
pub(crate) fn advise(delta: &AppliedPatchDelta, out: &mut impl Write) {
    if !delta.is_exact() {
        return;
    }
    let mut endpoints = BTreeMap::new();
    for change in delta.changes() {
        let (before, after) = match &change.change {
            AppliedPatchFileChange::Update {
                old_content,
                new_content,
                move_path: None,
                ..
            } => (old_content.as_str(), Some(new_content.as_str())),
            AppliedPatchFileChange::Add {
                content,
                overwritten_content,
            } => (
                overwritten_content.as_deref().unwrap_or(""),
                Some(content.as_str()),
            ),
            // A removed or renamed source no longer has eligible postimage bytes.
            _ => ("", None),
        };
        if let Some(after) = after {
            endpoints
                .entry(change.path.as_path())
                .and_modify(|entry: &mut (&str, &str)| entry.1 = after)
                .or_insert((before, after));
        } else {
            endpoints.remove(change.path.as_path());
        }
    }

    let (mut remaining_bytes, mut remaining_notices) = (256 * 1024usize, 8);
    for (path, (before, after)) in endpoints {
        if remaining_notices == 0 {
            break;
        }
        let rust = match Path::extension(path).and_then(|ext| ext.to_str()) {
            Some("rs") if after.contains("todo") || after.contains("unimplemented") => true,
            Some("py") if after.contains("NotImplementedError") => false,
            _ => continue,
        };
        let size = before.len().saturating_add(after.len());
        if size > 128 * 1024 || size > remaining_bytes {
            continue;
        }
        remaining_bytes -= size;
        let language = if rust {
            tree_sitter_rust::LANGUAGE.into()
        } else {
            tree_sitter_python::LANGUAGE.into()
        };
        let mut parser = Parser::new();
        if parser.set_language(&language).is_err() {
            continue;
        }
        let Some(old) = stubs(&mut parser, before, rust) else {
            continue;
        };
        let Some(new) = stubs(&mut parser, after, rust) else {
            continue;
        };
        for ((owner, marker), lines) in new {
            let previous = old.get(&(owner, marker)).map_or(0, Vec::len);
            if previous > 0 {
                let added = lines.len().saturating_sub(previous);
                if added > 0 {
                    // Identical markers in one scope may have moved. Report the
                    // increase without guessing which occurrence is new.
                    let _ = writeln!(
                        out,
                        "Patch advisory: introduced placeholder {marker} ({added} additional occurrence(s)) in {}; review if this change requires a complete implementation.",
                        path.display()
                    );
                    remaining_notices -= 1;
                }
            } else {
                for line in lines.into_iter().take(remaining_notices) {
                    let _ = writeln!(
                        out,
                        "Patch advisory: introduced placeholder {marker} at {}:{line}; review if this change requires a complete implementation.",
                        path.display()
                    );
                    remaining_notices -= 1;
                }
            }
            if remaining_notices == 0 {
                return;
            }
        }
    }
}

fn stubs<'a>(
    parser: &mut Parser,
    source: &'a str,
    rust: bool,
) -> Option<BTreeMap<(Vec<&'a str>, &'static str), Vec<usize>>> {
    let tree = parser.parse(source, None)?;
    let root = tree.root_node();
    if root.has_error() {
        return None;
    }
    let mut result = BTreeMap::<_, Vec<_>>::new();
    let text = |node: Node<'_>| node.utf8_text(source.as_bytes()).ok();
    let mut cursor = root.walk();
    loop {
        let node = cursor.node();
        let marker = if rust && node.kind() == "macro_invocation" {
            match node.child_by_field_name("macro").and_then(text) {
                Some("todo") => Some("todo!"),
                Some("unimplemented") => Some("unimplemented!"),
                _ => None,
            }
        } else if !rust && node.kind() == "raise_statement" {
            let expression = node.named_child(0);
            let target = expression.and_then(|child| {
                if child.kind() == "call" {
                    child.child_by_field_name("function")
                } else {
                    Some(child)
                }
            });
            (target.and_then(text) == Some("NotImplementedError"))
                .then_some("raise NotImplementedError")
        } else {
            None
        };
        if let Some(marker) = marker {
            let mut owner = Vec::new();
            let mut parent = node.parent();
            while let Some(item) = parent {
                if matches!(
                    item.kind(),
                    "function_item"
                        | "mod_item"
                        | "function_definition"
                        | "class_definition"
                        | "impl_item"
                ) && let Some(name) = item
                    .child_by_field_name("name")
                    .or_else(|| item.child_by_field_name("type"))
                    .and_then(text)
                {
                    owner.push(name);
                }
                parent = item.parent();
            }
            result
                .entry((owner, marker))
                .or_default()
                .push(node.start_position().row + 1);
        }
        if cursor.goto_first_child() {
            continue;
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return Some(result);
            }
        }
    }
}
