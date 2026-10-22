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

/// Smaller bodies cost about as much as the reference that would replace them.
const MIN_ELIDED_BODY_BYTES: usize = 1024;
const MAX_FIRST_LINE_CHARS: usize = 160;
/// Bounds the alternation memory; clearing it only re-prints a body once more.
const MAX_REMEMBERED_ELISIONS: usize = 4096;
const LABEL_PREFIX: &str = "[source {";
const LABEL_SUFFIX: &str = "}]";

/// Replaces each large printed body whose exact text appears in a tool output
/// of `visible_prompt`, the input of the request that issued this cell. A body
/// elided before prints in full the next time, so printing it again is the
/// way to repeat the text.
pub(super) fn elide_bodies_already_in_context(
    body: &mut [FunctionCallOutputContentItem],
    visible_prompt: &[ResponseItem],
    elided: &mut HashSet<[u8; 32]>,
) {
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
    let mut output = String::with_capacity(text.len());
    let mut changed = false;
    let mut cursor = 0;
    while cursor < text.len() {
        let line_end = text[cursor..]
            .find('\n')
            .map_or(text.len(), |offset| cursor + offset + 1);
        let label = text[cursor..line_end].trim_end_matches(['\r', '\n']);
        let Some((body_end, next)) =
            body_line_count(label).and_then(|lines| body_bounds(text, line_end, lines))
        else {
            output.push_str(&text[cursor..line_end]);
            cursor = line_end;
            continue;
        };
        let body_text = &text[line_end..body_end];
        let digest = <[u8; 32]>::from(Sha256::digest(body_text.as_bytes()));
        if body_text.len() < MIN_ELIDED_BODY_BYTES
            || !visible.iter().any(|visible| visible.contains(body_text))
            || elided.remove(&digest)
        {
            output.push_str(&text[cursor..next]);
            cursor = next;
            continue;
        }
        elided.insert(digest);
        changed = true;
        let first_line = body_text
            .lines()
            .next()
            .unwrap_or_default()
            .chars()
            .take(MAX_FIRST_LINE_CHARS)
            .collect::<String>();
        output.push_str(&label[..label.len() - LABEL_SUFFIX.len()]);
        output.push_str(",\"already_in_context\":");
        output.push_str(
            &serde_json::json!({
                "first_line": first_line,
                "note": "identical text appears in an earlier tool output in this context; print it again to repeat it",
            })
            .to_string(),
        );
        output.push_str(LABEL_SUFFIX);
        output.push('\n');
        // Keep the newline that separates this body from the next label.
        output.push_str(&text[body_end..next]);
        cursor = next;
    }
    changed.then_some(output)
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

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::models::FunctionCallOutputPayload;
    use pretty_assertions::assert_eq;

    fn printed(bodies: &[(&str, &str)]) -> String {
        let labels = bodies
            .iter()
            .enumerate()
            .map(|(index, (path, text))| {
                format!(
                    "[source {{\"body\":{},\"path\":\"{path}\",\"text_lines\":{}}}]\n{text}",
                    index + 1,
                    text.matches('\n').count() + 1
                )
            })
            .collect::<Vec<_>>();
        format!("{{\"status\":\"ok\"}}\n{}", labels.join("\n"))
    }

    fn earlier_output(text: &str) -> ResponseItem {
        ResponseItem::CustomToolCallOutput {
            id: None,
            call_id: "earlier-cell".to_string(),
            name: None,
            output: FunctionCallOutputPayload::from_text(text.to_string()),
            internal_chat_message_metadata_passthrough: None,
        }
    }

    fn elide(text: &str, prompt: &[ResponseItem], elided: &mut HashSet<[u8; 32]>) -> String {
        let mut body = vec![FunctionCallOutputContentItem::InputText {
            text: text.to_string(),
        }];
        elide_bodies_already_in_context(&mut body, prompt, elided);
        let [FunctionCallOutputContentItem::InputText { text }] = body.as_slice() else {
            panic!("one text item");
        };
        text.clone()
    }

    #[test]
    fn bodies_the_model_already_sees_verbatim_print_once_as_a_reference() {
        let unchanged = "fn unchanged() {\n    work();\n}\n".repeat(64);
        let edited = "fn edited() {\n    other();\n}\n".repeat(64);
        // The earlier output covered more than this read, around other text.
        let prompt = [earlier_output(&printed(&[(
            "src/lib.rs",
            &format!("// header\n{unchanged}// footer\n"),
        )]))];
        let reread = printed(&[("src/lib.rs", &unchanged), ("src/edit.rs", &edited)]);
        let mut elided = HashSet::new();

        let first = elide(&reread, &prompt, &mut elided);
        assert!(
            !first.contains(&unchanged),
            "the visible body is not repeated"
        );
        assert!(
            first.contains(&edited),
            "text the model has not seen still prints"
        );
        let reference = first
            .lines()
            .find(|line| line.contains("already_in_context"))
            .expect("reference label");
        let label: serde_json::Value =
            serde_json::from_str(&reference["[source ".len()..reference.len() - 1]).unwrap();
        assert_eq!(label["path"], "src/lib.rs");
        assert_eq!(
            label["already_in_context"]["first_line"],
            "fn unchanged() {"
        );
        // The following label still starts its own line, so it stays parseable.
        assert!(first.contains("\n[source {\"body\":2,"));

        // Printing it again is the opt-out: the next copy is complete.
        assert_eq!(elide(&reread, &prompt, &mut elided), reread);
        assert_ne!(elide(&reread, &prompt, &mut elided), reread);

        // A final body ending in a newline has no separator after it.
        let alone = printed(&[("src/lib.rs", &unchanged)]);
        let reference = elide(&alone, &prompt, &mut HashSet::new());
        assert!(!reference.contains(&unchanged));
        assert!(reference.ends_with("}]\n"));
    }

    #[test]
    fn bodies_absent_partial_or_small_print_unchanged() {
        let text = "line of source text\n".repeat(80);
        let reread = printed(&[("src/lib.rs", &text)]);
        let mut elided = HashSet::new();
        // Nothing earlier, or an earlier output holding only part of it.
        for prompt in [
            Vec::new(),
            vec![earlier_output(&text[..text.len() / 2])],
            vec![ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![codex_protocol::models::ContentItem::InputText {
                    text: text.clone(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            }],
        ] {
            assert_eq!(elide(&reread, &prompt, &mut elided), reread);
        }
        let small = printed(&[("src/small.rs", "fn small() {}")]);
        assert_eq!(elide(&small, &[earlier_output(&small)], &mut elided), small);
        // A packet truncated inside a body leaves that body alone.
        let cut = &reread[..reread.len() - 200];
        assert_eq!(elide(cut, &[earlier_output(&text)], &mut elided), cut);
        assert!(elided.is_empty());
    }
}
