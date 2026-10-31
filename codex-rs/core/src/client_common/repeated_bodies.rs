//! Request-local sharing. Only content still included in this projection may
//! own a reference; canonical history and native tool values are never edited.

use std::collections::HashMap;
use std::sync::Arc;

use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::ResponseItem;
use serde_json::Value;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;

const MIN_BODY_BYTES: usize = 256;

#[derive(Clone)]
struct IncludedBody {
    text: String,
    location: Value,
}

#[derive(Default)]
struct IncludedContent {
    exact: HashMap<[u8; 32], Vec<IncludedBody>>,
    sources: HashMap<String, Vec<(usize, IncludedBody)>>,
    artifact_sources: HashMap<(String, String), String>,
}

#[derive(Clone)]
struct FrameSource {
    source: String,
    selector: Value,
    range: Value,
}

pub(super) fn deduplicate(source: &Arc<[ResponseItem]>) -> Arc<[ResponseItem]> {
    let mut included = IncludedContent::default();
    let mut result = Arc::clone(source);
    for (item_index, item) in source.iter().enumerate() {
        let (call_id, output) = match item {
            ResponseItem::FunctionCallOutput {
                call_id, output, ..
            }
            | ResponseItem::CustomToolCallOutput {
                call_id, output, ..
            } => (call_id, output),
            _ => continue,
        };
        // Preserve text-item boundaries, images, encrypted content, success,
        // call identity and all other item metadata.
        let texts: Vec<_> = match &output.body {
            FunctionCallOutputBody::Text(text) => vec![(0, text)],
            FunctionCallOutputBody::ContentItems(items) => items
                .iter()
                .enumerate()
                .filter_map(|(index, item)| {
                    if let FunctionCallOutputContentItem::InputText { text } = item {
                        Some((index, text))
                    } else {
                        None
                    }
                })
                .collect(),
        };
        for (content_index, text) in texts {
            let location = json!({"call_id":call_id,"content_index":content_index});
            let Some(replacement) = included.project_text(text, &location) else {
                continue;
            };
            let output = match &mut Arc::make_mut(&mut result)[item_index] {
                ResponseItem::FunctionCallOutput { output, .. }
                | ResponseItem::CustomToolCallOutput { output, .. } => output,
                _ => unreachable!(),
            };
            match &mut output.body {
                FunctionCallOutputBody::Text(text) => *text = replacement,
                FunctionCallOutputBody::ContentItems(items) => {
                    if let FunctionCallOutputContentItem::InputText { text } =
                        &mut items[content_index]
                    {
                        *text = replacement;
                    }
                }
            }
        }
    }
    result
}

impl IncludedContent {
    /// Share a complete nonsource payload without hiding its surrounding
    /// status, timing, recovery or other changed result metadata.
    fn project_body(&mut self, text: &str, location: &Value) -> Option<Value> {
        if text.len() < MIN_BODY_BYTES {
            return None;
        }
        let digest: [u8; 32] = Sha256::digest(text.as_bytes()).into();
        if let Some(owner) = self
            .exact
            .get(&digest)
            .into_iter()
            .flatten()
            .find(|owner| owner.text == text)
        {
            let reference = reference(&owner.location, text);
            if reference.to_string().len() + 64 < text.len() {
                return Some(reference);
            }
        }
        self.exact.entry(digest).or_default().push(IncludedBody {
            text: text.to_string(),
            location: location.clone(),
        });
        None
    }

    fn source_key(&mut self, value: &Value) -> Option<String> {
        let key = source_key(value)?;
        let hash = value
            .get("canonical_sha256")
            .or_else(|| value.get("source_sha256"))?
            .as_str()?;
        if let Some(artifact) = value.get("artifact_id").and_then(Value::as_str) {
            let alias = (artifact.to_string(), hash.to_string());
            if value.get("path").is_some() || value.get("canonical_uri").is_some() {
                self.artifact_sources
                    .entry(alias)
                    .or_insert_with(|| key.clone());
            } else if let Some(source) = self.artifact_sources.get(&alias) {
                return Some(source.clone());
            }
        }
        Some(key)
    }

    fn project_text(&mut self, text: &str, location: &Value) -> Option<String> {
        if text.len() < MIN_BODY_BYTES {
            return None;
        }
        let digest: [u8; 32] = Sha256::digest(text.as_bytes()).into();
        if let Some(owner) = self
            .exact
            .get(&digest)
            .into_iter()
            .flatten()
            .find(|owner| owner.text == text)
        {
            let reference =
                json!({"already_in_context":reference(&owner.location, text)}).to_string();
            if reference.len() < text.len() {
                return Some(reference);
            }
        }
        let mut result = String::new();
        let mut changed = false;
        let mut cursor = 0;
        let mut frames = Vec::new();
        while cursor < text.len() {
            let line_end = text[cursor..]
                .find('\n')
                .map_or(text.len(), |n| cursor + n + 1);
            let line = text[cursor..line_end].trim_end_matches(['\r', '\n']);
            if let Some(mut label) = source_label(line) {
                let bounds = label
                    .get("text_lines")
                    .and_then(Value::as_u64)
                    .and_then(|n| usize::try_from(n).ok())
                    .filter(|n| *n > 0)
                    .and_then(|n| body_bounds(text, line_end, n));
                if let Some((end, next)) = bounds {
                    let body = &text[line_end..end];
                    let key = self.source_key(&label).or_else(|| {
                        let mut candidates = frames.iter().filter(|frame: &&FrameSource| {
                            frame.selector == label["selector"]
                                && frame.range == label["canonical_range"]
                        });
                        let first = candidates.next()?;
                        candidates
                            .all(|other| other.source == first.source)
                            .then(|| first.source.clone())
                    });
                    let projected = if label["complete"] != false {
                        if let Some(key) = key.as_deref() {
                            self.project_source(body, &label, key, location)
                        } else {
                            self.project_body(body, location)
                                .map(|reference| json!([{"already_in_context":reference}]))
                        }
                    } else {
                        None
                    };
                    if let Some(parts) = projected {
                        label["included_content"] = parts;
                        result.push_str("[source ");
                        result.push_str(&label.to_string());
                        result.push_str("]\n");
                        result.push_str(&text[end..next]);
                        changed = true;
                    } else {
                        result.push_str(&text[cursor..next]);
                    }
                    cursor = next;
                    continue;
                }
            } else if let Ok(mut value) = serde_json::from_str::<Value>(line) {
                frames.clear();
                if self.project_json(&mut value, None, location, &mut frames) {
                    result.push_str(&value.to_string());
                    result.push_str(&text[cursor + line.len()..line_end]);
                    changed = true;
                    cursor = line_end;
                    continue;
                }
            }
            result.push_str(&text[cursor..line_end]);
            cursor = line_end;
        }
        // Never register the original bytes of a rewritten result: subsequent
        // references must not form chains to content that was removed above.
        if !changed {
            self.exact.entry(digest).or_default().push(IncludedBody {
                text: text.to_string(),
                location: location.clone(),
            });
        }
        changed.then_some(result)
    }

    fn project_json(
        &mut self,
        value: &mut Value,
        inherited: Option<&str>,
        location: &Value,
        frames: &mut Vec<FrameSource>,
    ) -> bool {
        let own_key = self.source_key(value);
        let key = own_key.as_deref().or(inherited);
        let mut changed = false;
        match value {
            Value::Object(object) => {
                // Only exact acquired source rows qualify for range sharing.
                if object.contains_key("canonical_range")
                    && object.get("complete") != Some(&Value::Bool(false))
                    && object.get("status").is_none_or(|status| status == "ok")
                    && let Some(key) = key
                {
                    if let Some(text) = object.get("text").and_then(Value::as_str) {
                        let row = Value::Object(object.clone());
                        if let Some(parts) = self.project_source(text, &row, key, location) {
                            object.remove("text");
                            object.insert("included_content".to_string(), parts);
                            changed = true;
                        }
                    } else if object.contains_key("text_lines") {
                        frames.push(FrameSource {
                            source: key.to_string(),
                            selector: object.get("selector").cloned().unwrap_or(Value::Null),
                            range: object["canonical_range"].clone(),
                        });
                    }
                }
                if !object.contains_key("canonical_range") {
                    for field in ["output", "stdout", "stderr", "content", "text"] {
                        if let Some(text) = object.get(field).and_then(Value::as_str)
                            && let Some(reference) = self.project_body(text, location)
                        {
                            object
                                .insert(field.to_string(), json!({"already_in_context":reference}));
                            changed = true;
                        }
                    }
                }
                for (name, child) in object.iter_mut() {
                    if name != "included_content" {
                        changed |= self.project_json(child, key, location, frames);
                    }
                }
            }
            Value::Array(array) => {
                for child in array {
                    changed |= self.project_json(child, key, location, frames);
                }
            }
            _ => {}
        }
        changed
    }

    fn project_source(
        &mut self,
        text: &str,
        row: &Value,
        key: &str,
        location: &Value,
    ) -> Option<Value> {
        let start = usize::try_from(row["canonical_range"]["start"].as_u64()?).ok()?;
        let end = usize::try_from(row["canonical_range"]["end"].as_u64()?).ok()?;
        if end.checked_sub(start)? != text.len() || text.len() < MIN_BODY_BYTES {
            return None;
        }
        let owners = self.sources.entry(key.to_string()).or_default();
        let mut overlaps = owners
            .iter()
            .filter_map(|(owner_start, owner)| {
                let left = start.max(*owner_start);
                let right = end.min(owner_start.checked_add(owner.text.len())?);
                if right.saturating_sub(left) < MIN_BODY_BYTES {
                    return None;
                }
                let wanted = text.get(left - start..right - start)?;
                let present = owner.text.get(left - owner_start..right - owner_start)?;
                // Identity alone never proves equality (malformed or changed
                // ranges fail open). Compare the overlapping bytes as well.
                (Sha256::digest(wanted.as_bytes()) == Sha256::digest(present.as_bytes())
                    && wanted == present)
                    .then_some((left, right, &owner.location))
            })
            .collect::<Vec<_>>();
        overlaps.sort_by_key(|(left, right, _)| (*left, std::cmp::Reverse(*right)));
        let mut parts = Vec::new();
        let mut unique = Vec::new();
        let mut cursor = start;
        for (left, right, owner) in overlaps {
            let left = left.max(cursor);
            if right <= left {
                continue;
            }
            if cursor < left {
                let part = &text[cursor - start..left - start];
                parts.push(json!({"canonical_range":{"start":cursor,"end":left},"text":part}));
                unique.push((cursor, part.to_string()));
            }
            parts.push(json!({"canonical_range":{"start":left,"end":right},
                "already_in_context":reference(owner, &text[left - start..right - start])}));
            cursor = right;
        }
        if cursor < end {
            let part = &text[cursor - start..];
            parts.push(json!({"canonical_range":{"start":cursor,"end":end},"text":part}));
            unique.push((cursor, part.to_string()));
        }
        // A compact reference must earn its overhead. Otherwise retain and
        // index the complete original, including any small overlaps.
        let replacement = Value::Array(parts);
        let saved = cursor > start && replacement.to_string().len() + 32 < text.len();
        if saved {
            for (offset, text) in unique {
                owners.push((
                    offset,
                    IncludedBody {
                        text,
                        location: location.clone(),
                    },
                ));
            }
            Some(replacement)
        } else {
            owners.push((
                start,
                IncludedBody {
                    text: text.to_string(),
                    location: location.clone(),
                },
            ));
            None
        }
    }
}

fn source_key(value: &Value) -> Option<String> {
    let hash = value
        .get("canonical_sha256")
        .or_else(|| value.get("source_sha256"))?
        .as_str()?;
    if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let locator = value
        .get("canonical_uri")
        .or_else(|| value.get("path"))
        .or_else(|| value.get("artifact_id"))?
        .as_str()?;
    Some(
        json!([
            value.get("environment_id"),
            locator,
            hash,
            value.get("source_encoding"),
            value.get("selector_coordinate_space")
        ])
        .to_string(),
    )
}

fn reference(location: &Value, text: &str) -> Value {
    let mut value = location.clone();
    value["sha256"] = Value::String(hex::encode(Sha256::digest(text.as_bytes())));
    value["bytes"] = json!(text.len());
    value
}

fn source_label(line: &str) -> Option<Value> {
    let value: Value =
        serde_json::from_str(line.strip_prefix("[source ")?.strip_suffix(']')?).ok()?;
    (value.is_object()
        && value.get("same_as_body").is_none()
        && value.get("already_in_context").is_none()
        && value.get("included_content").is_none())
    .then_some(value)
}

/// Counted frames, including their original trailing newline, are indivisible
/// unless the complete body is present. Truncated/legacy frames stay verbatim.
fn body_bounds(text: &str, start: usize, lines: usize) -> Option<(usize, usize)> {
    let mut search = start;
    for _ in 1..lines {
        search += text[search..].find('\n')? + 1;
    }
    Some(match text[search..].find('\n') {
        Some(offset) => (search + offset, search + offset + 1),
        None => (text.len(), text.len()),
    })
}
