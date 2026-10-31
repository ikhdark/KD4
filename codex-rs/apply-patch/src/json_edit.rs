//! Revision-bound JSON edits. Only changed lexical spans are serialized; the
//! surrounding document never passes through a Value round trip.
use std::collections::HashSet;
use std::ops::Range;

use serde::Deserialize;
use serde::Deserializer as _;
use serde::de::Error as _;
use serde_json::value::RawValue;
use sha2::Digest;
use sha2::Sha256;

use crate::UpdateFileChunk;
use crate::UpdateLineKind;

// Match the whole-source read_file snapshot limit and bound lexical-tree memory.
const MAX_DOCUMENT_BYTES: usize = 8 * 1024 * 1024;

pub(crate) fn is_json_chunk(chunk: &UpdateFileChunk) -> bool {
    chunk
        .change_context
        .as_deref()
        .is_some_and(|context| context == "codex-json" || context.starts_with("codex-json "))
}

pub(crate) fn apply(
    source: &str,
    chunks: &[UpdateFileChunk],
    newline: &str,
) -> Result<String, String> {
    let [chunk] = chunks else {
        return Err("use exactly one codex-json chunk; do not mix JSON and textual chunks".into());
    };
    let hash = chunk
        .change_context
        .as_deref()
        .and_then(|context| context.strip_prefix("codex-json sha256:"))
        .filter(|hash| hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or("expected @@ codex-json sha256:<whole-file source_sha256>")?;
    let current = hex::encode(Sha256::digest(source.as_bytes()));
    if !hash.eq_ignore_ascii_case(&current) {
        return Err(format!(
            "codex-json source hash changed (current sha256:{current}); re-read and reconcile before editing"
        ));
    }
    if !chunk.old_lines.is_empty()
        || chunk.is_end_of_file
        || chunk.new_lines.is_empty()
        || chunk
            .line_kinds
            .iter()
            .any(|kind| *kind != UpdateLineKind::Add)
    {
        return Err(
            "codex-json requires only + lines containing a nonempty JSON operation array".into(),
        );
    }
    let payload = chunk.new_lines.join(newline);
    parse(&payload).map_err(|error| format!("invalid JSON operations: {error}"))?;
    let operations: Vec<Operation> = serde_json::from_str(&payload)
        .map_err(|error| format!("invalid JSON operations: {error}"))?;
    if operations.is_empty() {
        return Err("codex-json operation array must not be empty".into());
    }
    let mut result = source.to_owned();
    for (index, operation) in operations.iter().enumerate() {
        let root = parse(&result).map_err(|error| format!("invalid JSON document: {error}"))?;
        let (range, replacement) = edit(&result, &root, operation)
            .map_err(|error| format!("JSON operation {}: {error}", index + 1))?;
        result.replace_range(range, &replacement);
    }
    // Insertion can increase nesting or size beyond either input's own limits.
    parse(&result).map_err(|error| format!("invalid JSON candidate: {error}"))?;
    Ok(result)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Operation {
    op: OperationKind,
    path: String,
    #[serde(default, deserialize_with = "present_value")]
    value: Option<Box<RawValue>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum OperationKind {
    Add,
    Replace,
    Remove,
}

// Deserialize directly from JSON rather than an internally tagged enum's
// buffered Content, which cannot retain RawValue. Explicit null is a value,
// not a missing field.
fn present_value<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Box<RawValue>>, D::Error> {
    Box::<RawValue>::deserialize(deserializer).map(Some)
}

struct Node {
    range: Range<usize>,
    children: Option<Vec<Entry>>,
    object: bool,
}

struct Entry {
    key: Option<String>,
    start: usize,
    value: Node,
}

fn parse(source: &str) -> Result<Node, String> {
    if source.len() > MAX_DOCUMENT_BYTES {
        return Err("JSON structured editing supports documents up to 8 MiB".into());
    }
    let raw: &RawValue = serde_json::from_str(source).map_err(|error| error.to_string())?;
    node(raw, source, 0).map_err(|error| error.to_string())
}

// RawValue keeps number/string spelling and delegates JSON grammar to serde.
// Unlike the bounded read-only pointer index in core, editing must inspect all
// objects for decoded duplicate keys, including objects outside the target path.
fn node(raw: &RawValue, source: &str, depth: usize) -> serde_json::Result<Node> {
    if depth > 128 {
        return Err(serde_json::Error::custom("JSON nesting exceeds 128 levels"));
    }
    let text = raw.get();
    let start = text.as_ptr() as usize - source.as_ptr() as usize;
    let object = text.starts_with('{');
    let children = if object || text.starts_with('[') {
        Some(
            serde_json::Deserializer::from_str(text).deserialize_any(Children {
                source,
                depth,
                start,
            })?,
        )
    } else {
        None
    };
    Ok(Node {
        range: start..start + text.len(),
        children,
        object,
    })
}

struct Children<'a> {
    source: &'a str,
    depth: usize,
    start: usize,
}

impl<'de> serde::de::Visitor<'de> for Children<'_> {
    type Value = Vec<Entry>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a JSON object or array")
    }

    fn visit_seq<A: serde::de::SeqAccess<'de>>(
        self,
        mut sequence: A,
    ) -> Result<Self::Value, A::Error> {
        let mut entries = Vec::new();
        while let Some(raw) = sequence.next_element::<&'de RawValue>()? {
            let value = node(raw, self.source, self.depth + 1).map_err(A::Error::custom)?;
            entries.push(Entry {
                key: None,
                start: value.range.start,
                value,
            });
        }
        Ok(entries)
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut entries = Vec::new();
        let mut keys = HashSet::new();
        let mut previous_end = self.start + 1;
        while let Some((key, raw)) = map.next_entry::<String, &'de RawValue>()? {
            if !keys.insert(key.clone()) {
                return Err(A::Error::custom(format!(
                    "ambiguous duplicate JSON key {key:?}"
                )));
            }
            let value = node(raw, self.source, self.depth + 1).map_err(A::Error::custom)?;
            let start = self.source[previous_end..value.range.start]
                .find('"')
                .map(|offset| previous_end + offset)
                .ok_or_else(|| A::Error::custom("JSON member address unavailable"))?;
            previous_end = value.range.end;
            entries.push(Entry {
                key: Some(key),
                start,
                value,
            });
        }
        Ok(entries)
    }
}

fn pointer(path: &str) -> Result<Vec<String>, String> {
    if path.is_empty() {
        return Ok(Vec::new());
    }
    let path = path
        .strip_prefix('/')
        .ok_or("JSON pointer must be empty or start with /")?;
    path.split('/')
        .map(|token| {
            let mut decoded = String::new();
            let mut chars = token.chars();
            while let Some(ch) = chars.next() {
                decoded.push(if ch == '~' {
                    match chars.next() {
                        Some('0') => '~',
                        Some('1') => '/',
                        _ => return Err("invalid JSON pointer escape; use ~0 or ~1".into()),
                    }
                } else {
                    ch
                });
            }
            Ok(decoded)
        })
        .collect()
}

fn array_index(token: &str, len: usize, add: bool) -> Result<usize, String> {
    if add && token == "-" {
        return Ok(len);
    }
    if token.is_empty()
        || (token.len() > 1 && token.starts_with('0'))
        || !token.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err("array index must be canonical unsigned decimal (or - for add)".into());
    }
    let index: usize = token.parse().map_err(|_| "array index is too large")?;
    if index > len || (!add && index == len) {
        return Err("array index is out of bounds".into());
    }
    Ok(index)
}

fn edit(
    source: &str,
    root: &Node,
    operation: &Operation,
) -> Result<(Range<usize>, String), String> {
    let path = &operation.path;
    let (replacement, add) = match (&operation.op, operation.value.as_deref()) {
        (OperationKind::Add, Some(value)) => (Some(value.get()), true),
        (OperationKind::Replace, Some(value)) => (Some(value.get()), false),
        (OperationKind::Remove, None) => (None, false),
        (OperationKind::Remove, Some(_)) => return Err("remove must omit value".into()),
        _ => return Err("add and replace require value (which may be null)".into()),
    };
    let tokens = pointer(path)?;
    let Some((last, parents)) = tokens.split_last() else {
        return replacement
            .map(|value| (root.range.clone(), value.to_owned()))
            .ok_or_else(|| "removing the document root would not leave a JSON document".into());
    };
    let mut parent = root;
    for token in parents {
        let children = parent
            .children
            .as_ref()
            .ok_or("pointer traverses a scalar")?;
        let index = if parent.object {
            children
                .iter()
                .position(|entry| entry.key.as_ref() == Some(token))
                .ok_or("JSON pointer parent does not exist")?
        } else {
            array_index(token, children.len(), false)?
        };
        parent = &children[index].value;
    }
    let children = parent
        .children
        .as_ref()
        .ok_or("pointer parent is a scalar")?;
    let index = if parent.object {
        children
            .iter()
            .position(|entry| entry.key.as_ref() == Some(last))
    } else {
        Some(array_index(last, children.len(), add)?)
    };
    if let Some(index) = index.filter(|index| *index < children.len()) {
        if !add || parent.object {
            let entry = &children[index];
            return Ok(match replacement {
                Some(value) => (entry.value.range.clone(), value.to_owned()),
                None => {
                    // Remove this member's leading trivia, not its successor's
                    // indentation. Last members consume the preceding comma.
                    let range = if let Some(next) = children.get(index + 1) {
                        let start = if index == 0 {
                            parent.range.start + 1
                        } else {
                            separator_end(source, children[index - 1].value.range.end, entry.start)?
                        };
                        start..separator_end(source, entry.value.range.end, next.start)?
                    } else if index > 0 {
                        separator_end(source, children[index - 1].value.range.end, entry.start)? - 1
                            ..entry.value.range.end
                    } else {
                        entry.start..entry.value.range.end
                    };
                    (range, String::new())
                }
            });
        }
    } else if !add {
        return Err("JSON pointer target does not exist".into());
    }
    let value = replacement.ok_or("add requires a value")?;
    let member = if parent.object {
        let colon = children
            .first()
            .map(|entry| {
                let prefix = &source[entry.start..entry.value.range.start];
                &prefix[prefix.rfind(':').unwrap_or(prefix.len())..]
            })
            .unwrap_or(": ");
        format!(
            "{}{colon}{value}",
            serde_json::to_string(last).map_err(|error| error.to_string())?
        )
    } else {
        value.to_owned()
    };
    let index = index.unwrap_or(children.len());
    if children.is_empty() {
        let inner = &source[parent.range.start + 1..parent.range.end - 1];
        // Existing closing whitespace is untouched; use it to lay out the new
        // first member, adding one indent level only for multiline containers.
        if inner.contains('\n') {
            return Ok((
                parent.range.start + 1..parent.range.start + 1,
                format!("{inner}{}{member}", indent_unit(source)),
            ));
        }
        let end = parent.range.end - 1;
        return Ok((end..end, format!("{member}{inner}")));
    }
    let sibling = index.min(children.len() - 1);
    let separator_start = if sibling == 0 {
        parent.range.start + 1
    } else {
        separator_end(
            source,
            children[sibling - 1].value.range.end,
            children[sibling].start,
        )?
    };
    let whitespace = &source[separator_start..children[sibling].start];
    if index < children.len() {
        let start = children[index].start;
        Ok((start..start, format!("{member},{whitespace}")))
    } else {
        let end = children[children.len() - 1].value.range.end;
        Ok((end..end, format!(",{whitespace}{member}")))
    }
}

fn separator_end(source: &str, left: usize, right: usize) -> Result<usize, String> {
    source[left..right]
        .find(',')
        .map(|offset| left + offset + 1)
        .ok_or_else(|| "JSON separator unavailable".into())
}

fn indent_unit(source: &str) -> &str {
    source
        .lines()
        .find_map(|line| {
            let length = line.len() - line.trim_start_matches([' ', '\t']).len();
            (length > 0).then_some(&line[..length])
        })
        .unwrap_or("  ")
}
