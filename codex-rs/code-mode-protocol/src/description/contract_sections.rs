//! Bounded delivery for nested tool contracts.
//!
//! `resolve_tool(name).description` and `ALL_TOOLS` hand a contract to a script, and scripts
//! print it. Complete contracts reach about 28 KB, so printing two or three overflowed the
//! cell's display budget and cost a recovery generation. A large contract therefore renders as
//! a bounded core plus named sections, each small enough to print. Together they keep every
//! word of the complete contract; only a section-free contract is the same text as before.

use std::collections::BTreeSet;

use serde_json::Value as JsonValue;

use super::metadata::render_code_mode_tool_declaration;

/// Divides the core from its sections inside the one `description` string V1 hosts transport.
const SECTION_SEPARATOR: char = '\u{1e}';
/// Contracts up to this size stay whole: splitting them would only add a lookup.
const WHOLE_CONTRACT_BYTES: usize = 6_000;
const CORE_PROSE_BYTES: usize = 1_500;
/// A result type this short stays in the core's declaration.
const CORE_RESULT_BYTES: usize = 1_200;
/// Keeps a few sections together under a cell's default display budget.
const SECTION_BYTES: usize = 12_000;

pub(super) fn exceeds_whole_contract(rendered: &str) -> bool {
    rendered.len() > WHOLE_CONTRACT_BYTES
}

/// Authored or provider text must not be able to forge a section boundary.
pub(super) fn sanitize_contract_text(text: &str) -> std::borrow::Cow<'_, str> {
    if text.contains(SECTION_SEPARATOR) {
        text.replace(SECTION_SEPARATOR, " ").into()
    } else {
        text.into()
    }
}

/// What `.description` returns: the whole contract, or the core of a sectioned one.
pub fn contract_core(description: &str) -> &str {
    description
        .split_once(SECTION_SEPARATOR)
        .map_or(description, |(core, _)| core)
}

pub fn contract_section<'a>(description: &'a str, name: &str) -> Option<&'a str> {
    description
        .split(SECTION_SEPARATOR)
        .skip(1)
        .find_map(|section| {
            let (section_name, text) = section.split_once('\n')?;
            (section_name == name).then_some(text)
        })
}

pub fn contract_section_names(description: &str) -> impl Iterator<Item = &str> {
    description
        .split(SECTION_SEPARATOR)
        .skip(1)
        .filter_map(|section| section.split_once('\n').map(|(name, _)| name))
}

/// Append to the core, leaving the sections that follow it in place.
pub fn append_to_contract_core(description: &str, note: &str) -> String {
    let core = contract_core(description);
    format!("{core}\n{note}{}", &description[core.len()..])
}

pub(super) struct ContractParts<'a> {
    pub tool_name: &'a str,
    pub input_name: &'a str,
    pub guidance: Option<&'a str>,
    pub prose: &'a str,
    pub result_envelope: Option<&'a str>,
    pub aliases: &'a [String],
    pub input_type: &'a str,
    pub output_type: &'a str,
    pub output_schema: Option<&'a JsonValue>,
    /// Authoritative JSON, retained only for projections that lost structure.
    pub schemas: &'a [(&'static str, String)],
}

pub(super) fn render_sectioned_contract(parts: ContractParts<'_>) -> String {
    let (core_prose, mut sections) = split_prose(parts.prose);

    let compact_input = keep_first_comment_sentences(parts.input_type);
    let compact_output = keep_first_comment_sentences(parts.output_type);
    let inline_result = compact_output.len() <= CORE_RESULT_BYTES;
    let core_result = if inline_result {
        compact_output.as_str()
    } else {
        "ToolResult"
    };

    let compact_arguments =
        with_aliases(parts.aliases, keep_first_comment_sentences, &compact_input);
    let full_arguments = with_aliases(parts.aliases, str::to_owned, parts.input_type);
    if compact_arguments != full_arguments {
        push_section(&mut sections, "arguments", full_arguments);
    }
    let full_result = with_aliases(parts.aliases, str::to_owned, parts.output_type);
    if !inline_result
        || with_aliases(parts.aliases, keep_first_comment_sentences, &compact_output) != full_result
    {
        let envelope = parts.result_envelope.unwrap_or("");
        push_section(&mut sections, "result", format!("{envelope}{full_result}"));
    }
    for (label, schema) in parts.schemas {
        push_section(&mut sections, label, schema.clone());
    }

    let mut core = String::new();
    if let Some(guidance) = parts.guidance {
        core.push_str(guidance);
        core.push_str("\n\n");
    }
    core.push_str(&core_prose);
    core.push_str("\n\nexec tool declaration:\n```ts\n");
    if inline_result && let Some(envelope) = parts.result_envelope {
        core.push_str(envelope);
    }
    for alias in aliases_used_by(parts.aliases, &[&compact_input, core_result]) {
        core.push_str(&keep_first_comment_sentences(alias));
        core.push('\n');
    }
    core.push_str(&format!(
        "declare const tools: {{ {} }};",
        render_code_mode_tool_declaration(
            parts.tool_name,
            parts.input_name,
            compact_input.clone(),
            core_result.to_owned(),
            false,
        )
    ));
    if !inline_result {
        let fields = parts
            .output_schema
            .map(result_field_names)
            .unwrap_or_default();
        if !fields.is_empty() {
            core.push_str(&format!("\n// ToolResult fields: {}", fields.join(", ")));
        }
    }
    core.push_str("\n```");
    if !sections.is_empty() {
        let index = sections
            .iter()
            .map(|(name, text)| format!("{name} ({}K chars)", text.len().div_ceil(1000)))
            .collect::<Vec<_>>()
            .join(", ");
        core.push_str(&format!(
            "\n\nDetail sections, printed with `text(resolve_tool(name).section(\"<section>\"))`: {index}."
        ));
    }

    // Namespace guidance and result-field labels also come from providers.
    // Sanitize the assembled core before adding our own section boundaries.
    if let std::borrow::Cow::Owned(sanitized) = sanitize_contract_text(&core) {
        core = sanitized;
    }
    for (name, text) in sections {
        core.push(SECTION_SEPARATOR);
        core.push_str(&name);
        core.push('\n');
        core.push_str(&text);
    }
    core
}

/// A sentence ends at a period before a space, past common abbreviations.
pub fn first_sentence(text: &str) -> &str {
    let mut search_from = 0;
    while let Some(offset) = text[search_from..].find(". ") {
        let end = search_from + offset + 1;
        let word = text[..end - 1]
            .rsplit(char::is_whitespace)
            .next()
            .unwrap_or("");
        if !matches!(word, "e.g" | "i.e" | "vs" | "etc") {
            return &text[..end];
        }
        search_from = end;
    }
    text
}

fn keep_first_comment_sentences(rendered: &str) -> String {
    let mut kept = String::with_capacity(rendered.len());
    for (index, line) in rendered.split('\n').enumerate() {
        if index > 0 {
            kept.push('\n');
        }
        let comment = line.trim_start();
        if comment.starts_with("// ") {
            kept.push_str(&line[..line.len() - comment.len()]);
            kept.push_str(first_sentence(comment));
        } else {
            kept.push_str(line);
        }
    }
    kept
}

fn with_aliases(aliases: &[String], project: impl Fn(&str) -> String, rendered: &str) -> String {
    let mut text = String::new();
    for alias in aliases_used_by(aliases, &[rendered]) {
        text.push_str(&project(alias));
        text.push('\n');
    }
    text.push_str(rendered);
    text
}

fn is_identifier_char(character: char) -> bool {
    character == '_' || character == '$' || character.is_ascii_alphanumeric()
}

fn mentions_identifier(text: &str, name: &str) -> bool {
    text.match_indices(name).any(|(at, _)| {
        !text[..at].chars().next_back().is_some_and(is_identifier_char)
            && !text[at + name.len()..]
                .chars()
                .next()
                .is_some_and(is_identifier_char)
    })
}

/// Aliases reference one another, so follow them from the text that stays.
fn aliases_used_by<'a>(aliases: &'a [String], roots: &[&str]) -> Vec<&'a str> {
    let names = aliases
        .iter()
        .map(|alias| {
            alias
                .strip_prefix("type ")
                .and_then(|rest| rest.split_once(" ="))
                .map(|(name, _)| name)
        })
        .collect::<Vec<_>>();
    let mut used = vec![false; aliases.len()];
    let mut frontier = roots.to_vec();
    while let Some(text) = frontier.pop() {
        for (index, name) in names.iter().enumerate() {
            if !used[index] && name.is_some_and(|name| mentions_identifier(text, name)) {
                used[index] = true;
                frontier.push(&aliases[index]);
            }
        }
    }
    aliases
        .iter()
        .zip(used)
        .filter_map(|(alias, used)| used.then_some(alias.as_str()))
        .collect()
}

/// Top-level names across union branches, so a script knows what it can read without
/// fetching the whole type.
fn result_field_names(schema: &JsonValue) -> Vec<String> {
    fn visit(schema: &JsonValue, root: &JsonValue, depth: usize, names: &mut BTreeSet<String>) {
        if depth > 4 {
            return;
        }
        if let Some(target) = schema
            .get("$ref")
            .and_then(JsonValue::as_str)
            .and_then(|reference| reference.strip_prefix('#'))
            .and_then(|pointer| root.pointer(pointer))
        {
            visit(target, root, depth + 1, names);
        }
        if let Some(properties) = schema.get("properties").and_then(JsonValue::as_object) {
            names.extend(properties.keys().cloned());
        }
        for key in ["anyOf", "oneOf", "allOf"] {
            for branch in schema
                .get(key)
                .and_then(JsonValue::as_array)
                .map_or(&[][..], Vec::as_slice)
            {
                visit(branch, root, depth + 1, names);
            }
        }
    }
    let structured = super::metadata::mcp_structured_content_schema(Some(schema)).unwrap_or(schema);
    let mut names = BTreeSet::new();
    visit(structured, structured, 0, &mut names);
    names.into_iter().collect()
}

fn paragraphs(prose: &str) -> Vec<String> {
    let mut paragraphs = Vec::new();
    let mut current = String::new();
    let mut fenced = false;
    for line in prose.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
        }
        if line.trim().is_empty() && !fenced {
            if !current.is_empty() {
                paragraphs.push(std::mem::take(&mut current));
            }
            continue;
        }
        if !current.is_empty() {
            current.push('\n');
        }
        current.push_str(line);
    }
    if !current.is_empty() {
        paragraphs.push(current);
    }
    paragraphs
}

fn append_paragraph(text: &mut String, paragraph: &str) {
    if !text.is_empty() {
        text.push_str("\n\n");
    }
    text.push_str(paragraph);
}

/// The head names a section only when it reads as a label, such as `Search guidance:`.
fn section_title(paragraph: &str) -> Option<String> {
    let line = paragraph.lines().next()?;
    let end = line.find([':', '('])?;
    let head = line[..end].trim();
    if head.is_empty() || head.len() > 48 || head.contains(['.', ',']) {
        return None;
    }
    if head.split_whitespace().count() > 5 {
        return None;
    }
    let slug = head
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>();
    Some(slug)
}

/// Split at the last sentence end within `limit`, or at the limit in sentence-free text.
fn split_at_sentence(text: &str, limit: usize) -> (&str, &str) {
    let mut end = 0;
    loop {
        let sentence = first_sentence(&text[end..]);
        let next = end + sentence.len();
        if next > limit || next == text.len() {
            break;
        }
        end = next;
    }
    if end == 0 {
        end = text.floor_char_boundary(limit);
    }
    text.split_at(end)
}

fn split_prose(prose: &str) -> (String, Vec<(String, String)>) {
    let mut core = String::new();
    let mut details = String::new();
    let mut titled = Vec::new();
    for paragraph in paragraphs(prose) {
        if core.len() + paragraph.len() + 2 <= CORE_PROSE_BYTES {
            append_paragraph(&mut core, &paragraph);
        } else if core.is_empty() {
            let (head, tail) = split_at_sentence(&paragraph, CORE_PROSE_BYTES);
            core.push_str(head.trim_end());
            append_paragraph(&mut details, tail.trim_start());
        } else if let Some(title) = section_title(&paragraph) {
            titled.push((title, paragraph));
        } else {
            append_paragraph(&mut details, &paragraph);
        }
    }
    let mut sections = Vec::new();
    if !details.is_empty() {
        push_section(&mut sections, "details", details);
    }
    for (title, paragraph) in titled {
        push_section(&mut sections, &title, paragraph);
    }
    (core, sections)
}

fn push_section(sections: &mut Vec<(String, String)>, name: &str, text: String) {
    let mut rest = text.as_str();
    let mut part = 1;
    while !rest.is_empty() {
        let (head, tail) = if rest.len() <= SECTION_BYTES {
            (rest, "")
        } else {
            let window = &rest[..rest.floor_char_boundary(SECTION_BYTES)];
            // Prefer whole lines; minified JSON has none, so split it between members.
            let cut = window
                .rfind('\n')
                .map(|at| at + 1)
                .filter(|cut| *cut > SECTION_BYTES / 2)
                .or_else(|| window.rfind(',').map(|at| at + 1))
                .unwrap_or(window.len());
            rest.split_at(cut)
        };
        let mut label = if part == 1 {
            name.to_owned()
        } else {
            format!("{name}_{part}")
        };
        while sections.iter().any(|(existing, _)| *existing == label) {
            part += 1;
            label = format!("{name}_{part}");
        }
        sections.push((label, head.to_owned()));
        rest = tail;
        part += 1;
    }
}
