//! Printed source bodies the model already sees verbatim. A cell that prints a
//! large body whose exact text appears in a tool output of the request that
//! issued it gets a labeled reference instead of a second copy. Only the
//! model-visible packet changes; the canonical artifact and the script's
//! values keep every byte.

use std::collections::HashSet;

use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::ResponseItem;
use sha2::Digest;
use sha2::Sha256;

/// Skip tiny bodies; the final reference must also be smaller than its body.
const MIN_ELIDED_BODY_BYTES: usize = 256;
const MAX_FIRST_LINE_CHARS: usize = 160;
/// Bounds bookkeeping; visible prompt bytes remain the authority for elision.
const MAX_REMEMBERED_ELISIONS: usize = 4096;
const LABEL_PREFIX: &str = "[source {";
const LABEL_SUFFIX: &str = "}]";

/// Replaces each large printed body whose exact text appears in a tool output
/// of `visible_prompt`, the input of the request that issued this cell.
/// Ordinary rereads stay referential. To explicitly repeat evidence, print the
/// selected text itself instead of the labeled result envelope.
pub(super) fn elide_bodies_already_in_context(
    body: &mut [FunctionCallOutputContentItem],
    visible_prompt: &[ResponseItem],
    elided: &mut HashSet<[u8; 32]>,
) {
    if !body.iter().any(|item| matches!(item,
        FunctionCallOutputContentItem::InputText { text } if text.contains(LABEL_PREFIX))) {
        return;
    }
    let visible = visible_prompt
        .iter()
        .flat_map(|item| match item {
            ResponseItem::FunctionCallOutput { output, .. }
            | ResponseItem::CustomToolCallOutput { output, .. } => match &output.body {
                FunctionCallOutputBody::Text(text) => vec![text.as_str()],
                FunctionCallOutputBody::ContentItems(items) => items
                    .iter()
                    .filter_map(|item| match item {
                        FunctionCallOutputContentItem::InputText { text } => Some(text.as_str()),
                        FunctionCallOutputContentItem::InputImage { .. }
                        | FunctionCallOutputContentItem::EncryptedContent { .. } => None,
                    })
                    .collect(),
            },
            _ => Vec::new(),
        })
        .filter(|text| text.len() >= MIN_ELIDED_BODY_BYTES)
        .collect::<Vec<_>>();
    if visible.is_empty() {
        return;
    }
    if elided.len() > MAX_REMEMBERED_ELISIONS {
        elided.clear();
    }
    for item in body {
        let FunctionCallOutputContentItem::InputText { text } = item else {
            continue;
        };
        if !text.contains(LABEL_PREFIX) {
            continue;
        }
        if let Some(rewritten) = elide_text(text, &visible, elided) {
            *text = rewritten;
        }
    }
}

fn elide_text(text: &str, visible: &[&str], elided: &mut HashSet<[u8; 32]>) -> Option<String> {
    let mut output: Option<String> = None;
    let mut cursor = 0;
    while cursor < text.len() {
        let line_end = text[cursor..]
            .find('\n')
            .map_or(text.len(), |offset| cursor + offset + 1);
        let label = text[cursor..line_end].trim_end_matches(['\r', '\n']);
        let Some((body_end, next)) =
            body_line_count(label).and_then(|lines| body_bounds(text, line_end, lines))
        else {
            if let Some(output) = &mut output {
                output.push_str(&text[cursor..line_end]);
            }
            cursor = line_end;
            continue;
        };
        let body_text = &text[line_end..body_end];
        if body_text.len() < MIN_ELIDED_BODY_BYTES
            || !visible.iter().any(|visible| visible.contains(body_text))
        {
            if let Some(output) = &mut output {
                output.push_str(&text[cursor..next]);
            }
            cursor = next;
            continue;
        }
        let first_line = body_text
            .lines()
            .next()
            .unwrap_or_default()
            .chars()
            .take(MAX_FIRST_LINE_CHARS)
            .collect::<String>();
        let reference = serde_json::json!({
            "first_line": first_line,
            "note": "identical text is already visible; to repeat it explicitly, print the selected text without its envelope",
        }).to_string();
        if reference.len() + ",\"already_in_context\":".len() + 1 >= body_text.len() {
            if let Some(output) = &mut output {
                output.push_str(&text[cursor..next]);
            }
            cursor = next;
            continue;
        }
        let digest = <[u8; 32]>::from(Sha256::digest(body_text.as_bytes()));
        elided.insert(digest);
        let output = output.get_or_insert_with(|| {
            let mut output = String::with_capacity(text.len());
            output.push_str(&text[..cursor]);
            output
        });
        output.push_str(&label[..label.len() - LABEL_SUFFIX.len()]);
        output.push_str(",\"already_in_context\":");
        output.push_str(&reference);
        output.push_str(LABEL_SUFFIX);
        output.push('\n');
        // Keep the newline that separates this body from the next label.
        output.push_str(&text[body_end..next]);
        cursor = next;
    }
    output
}

/// The end of a body of `lines` lines starting at `start`, and the offset after
/// its separating newline. `None` when a truncated packet ends inside the body.
fn body_bounds(text: &str, start: usize, lines: usize) -> Option<(usize, usize)> {
    let mut search = start;
    // A body of N lines holds N - 1 newlines; the next one separates it.
    for _ in 1..lines {
        search += text[search..].find('\n')? + 1;
    }
    Some(match text[search..].find('\n') {
        Some(offset) => (search + offset, search + offset + 1),
        None => (text.len(), text.len()),
    })
}

/// The body line count of a printed source label, or `None` for any other
/// line. A `same_as_body` label already prints no body.
fn body_line_count(label: &str) -> Option<usize> {
    let json = label
        .strip_prefix("[source ")?
        .strip_suffix(']')
        .filter(|_| label.starts_with(LABEL_PREFIX) && label.ends_with(LABEL_SUFFIX))?;
    let value = serde_json::from_str::<serde_json::Value>(json).ok()?;
    if value.get("same_as_body").is_some() || value.get("already_in_context").is_some() {
        return None;
    }
    usize::try_from(value.get("text_lines")?.as_u64()?)
        .ok()
        .filter(|lines| *lines > 0)
}

