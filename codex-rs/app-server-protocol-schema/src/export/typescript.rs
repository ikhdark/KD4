//! TypeScript filtering, declaration scanning, and generated-file post-processing.

use super::GENERATED_TS_HEADER;
use super::experimental_method_types;
use super::remove_generated_type_files;
use super::trim_trailing_line_whitespace;
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use codex_app_server_protocol_types as types;
use std::collections::HashMap;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::fs;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use types::experimental_fields;
use types::schema_support::EXPERIMENTAL_CLIENT_METHODS;
use types::schema_support::EXPERIMENTAL_SERVER_METHODS;

// Header processing also visits existing user files in the output directory.
// Support up to 64 MiB per TypeScript file without an unbounded allocation.
const MAX_TYPESCRIPT_HEADER_INPUT_BYTES: u64 = 64 * 1024 * 1024;

pub(super) fn prettier_file_arguments(files: &[PathBuf], current_dir: &Path) -> Vec<PathBuf> {
    files
        .iter()
        .map(|file| {
            // Keep the process cwd unchanged: executable, config, and ignore-file
            // resolution must not change merely to shorten repeated prefixes.
            let Ok(relative) = file.strip_prefix(current_dir) else {
                return file.clone();
            };
            let relative = Path::new(".").join(relative);
            if relative.as_os_str().as_encoded_bytes().len()
                < file.as_os_str().as_encoded_bytes().len()
            {
                relative
            } else {
                file.clone()
            }
        })
        .collect()
}

// Conservative allowance for Windows quoting/escaping, below the process limit.
pub(super) fn prettier_file_batches<'a>(prettier: &Path, files: &'a [PathBuf]) -> Result<Vec<&'a [PathBuf]>> {
    // Batch wrappers are admitted through cmd.exe, whose ceiling is 8191 characters.
    let limit = if prettier.extension().and_then(OsStr::to_str)
        .is_some_and(|extension| extension.eq_ignore_ascii_case("cmd") || extension.eq_ignore_ascii_case("bat"))
    { 7_000 } else { 30_000 };
    let argument_size = |path: &Path| path.as_os_str().as_encoded_bytes().len() * 2 + 3;
    let base = argument_size(prettier) + 64;
    let mut batches = Vec::new();
    let mut start = 0;
    let mut size = base;
    for (index, file) in files.iter().enumerate() {
        let next = argument_size(file);
        anyhow::ensure!(
            base + next <= limit,
            "Prettier file argument is too long: {}",
            file.display()
        );
        if size + next > limit {
            batches.push(&files[start..index]);
            start = index;
            size = base;
        }
        size += next;
    }
    if start < files.len() {
        batches.push(&files[start..]);
    }
    Ok(batches)
}

pub(super) fn filter_experimental_ts(out_dir: &Path) -> Result<()> {
    let registered_fields = experimental_fields();
    let experimental_method_types = experimental_method_types();
    // Most generated TS files are filtered by schema processing, but
    // Request unions and types with `#[experimental(...)]` fields need direct
    // post-processing because they encode method/field information locally.
    filter_request_ts(out_dir, "ClientRequest.ts", EXPERIMENTAL_CLIENT_METHODS)?;
    filter_request_ts(out_dir, "ServerRequest.ts", EXPERIMENTAL_SERVER_METHODS)?;
    filter_experimental_type_fields_ts(out_dir, &registered_fields)?;
    remove_generated_type_files(out_dir, &experimental_method_types, "ts")?;
    Ok(())
}


/// Removes union arms from a generated request type for methods marked experimental.
fn filter_request_ts(out_dir: &Path, file_name: &str, experimental_methods: &[&str]) -> Result<()> {
    let path = out_dir.join(file_name);
    if !path.exists() {
        return Ok(());
    }
    let mut content =
        fs::read_to_string(&path).with_context(|| format!("Failed to read {}", path.display()))?;
    content = filter_request_ts_contents(content, experimental_methods)?;

    fs::write(&path, content).with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(())
}

fn filter_request_ts_contents(
    mut content: String,
    experimental_methods: &[&str],
) -> Result<String> {
    let (prefix, body, suffix) =
        split_type_alias(&content).context("cannot parse generated request type alias")?;
    let experimental_methods: HashSet<&str> = experimental_methods
        .iter()
        .copied()
        .filter(|method| !method.is_empty())
        .collect();
    let arms = split_top_level(&body, '|');
    let filtered_arms: Vec<String> = arms
        .into_iter()
        .filter(|arm| {
            extract_method_from_arm(arm)
                .is_none_or(|method| !experimental_methods.contains(method.as_str()))
        })
        .collect();
    let new_body = if filtered_arms.is_empty() { "never".to_string() } else { filtered_arms.join(" | ") };
    content = format!("{prefix}{new_body}{suffix}");
    Ok(prune_unused_type_imports(content))
}

/// Removes experimental properties from generated TypeScript type files.
fn filter_experimental_type_fields_ts(
    out_dir: &Path,
    experimental_fields: &[&'static types::ExperimentalField],
) -> Result<()> {
    let mut fields_by_type_name: HashMap<String, HashSet<String>> = HashMap::new();
    for field in experimental_fields {
        fields_by_type_name
            .entry(field.type_name.to_string())
            .or_default()
            .insert(field.field_name.to_string());
    }
    if fields_by_type_name.is_empty() {
        return Ok(());
    }

    for path in ts_files_in_recursive(out_dir)? {
        let Some(type_name) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let Some(experimental_field_names) = fields_by_type_name.get(type_name) else {
            continue;
        };
        filter_experimental_fields_in_ts_file(&path, experimental_field_names)?;
    }

    Ok(())
}

fn filter_experimental_fields_in_ts_file(
    path: &Path,
    experimental_field_names: &HashSet<String>,
) -> Result<()> {
    let mut content =
        fs::read_to_string(path).with_context(|| format!("Failed to read {}", path.display()))?;
    content = filter_experimental_type_fields_ts_contents(content, experimental_field_names)?;
    fs::write(path, content).with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(())
}

fn filter_experimental_type_fields_ts_contents(
    mut content: String,
    experimental_field_names: &HashSet<String>,
) -> Result<String> {
    let mut spans = Vec::new();
    let (open, close) = type_body_brace_span(&content)
        .context("cannot parse generated type with experimental fields")?;
    spans.push((open, close));
    let mut offset = close + 1;
    while let Some((open, close)) = find_top_level_brace_span(&content[offset..]) {
        spans.push((offset + open, offset + close));
        offset += close + 1;
    }
    // Work backwards so removing a field cannot invalidate subsequent offsets.
    for (open_brace, close_brace) in spans.into_iter().rev() {
        let inner = &content[open_brace + 1..close_brace];
        let filtered_fields: Vec<String> = split_top_level_multi(inner, &[',', ';'])
            .into_iter()
            .filter(|field| {
                let field = strip_leading_block_comments(field);
                parse_property_name(field)
                    .is_none_or(|name| !experimental_field_names.contains(name.as_str()))
            })
            .collect();
        content.replace_range(open_brace + 1..close_brace, &filtered_fields.join(", "));
    }
    Ok(prune_unused_type_imports(content))
}

fn prune_unused_type_imports(content: String) -> String {
    let Some((declaration, _)) = type_declaration_start(&content) else {
        return content;
    };
    let mut identifiers = HashSet::new();
    let mut state = ScanState::default();
    let mut start = None;
    let scope = &content[declaration..];
    for (index, ch) in scope
        .char_indices()
        .chain(std::iter::once((scope.len(), ' ')))
    {
        if !state.in_ignored_syntax() && is_ident_char(ch) {
            start.get_or_insert(index);
        } else if let Some(start) = start.take() {
            identifiers.insert(&scope[start..index]);
        }
        state.observe(ch);
    }
    let trailing_newline = content.ends_with('\n');
    let mut lines = Vec::new();
    for line in content.lines() {
        if let Some(type_name) = parse_imported_type_name(line)
            && !identifiers.contains(type_name)
        {
            continue;
        }
        lines.push(line);
    }

    let mut rewritten = lines.join("\n");
    if trailing_newline {
        rewritten.push('\n');
    }
    rewritten
}

fn parse_imported_type_name(line: &str) -> Option<&str> {
    let line = line.trim();
    let rest = line.strip_prefix("import type {")?;
    let (type_name, _) = rest.split_once("} from ")?;
    let type_name = type_name.trim();
    if type_name.is_empty() || type_name.contains(',') || type_name.contains(" as ") {
        return None;
    }
    Some(type_name)
}

// Restrict parsing to a real generated declaration, ignoring examples in comments.
fn type_declaration_start(content: &str) -> Option<(usize, bool)> {
    let mut state = ScanState::default();
    for (index, ch) in content.char_indices() {
        if !state.in_ignored_syntax()
            && state.depth.is_top_level()
            && (index == 0
                || !content[..index]
                    .chars()
                    .next_back()
                    .is_some_and(is_ident_char))
        {
            if content[index..].starts_with("export type ") {
                return Some((index, false));
            }
            if content[index..].starts_with("export interface ") {
                return Some((index, true));
            }
        }
        state.observe(ch);
    }
    None
}

fn top_level_char(input: &str, target: char) -> Option<usize> {
    let mut state = ScanState::default();
    for (index, ch) in input.char_indices() {
        if !state.in_ignored_syntax() && state.depth.is_top_level() && ch == target {
            return Some(index);
        }
        state.observe(ch);
    }
    None
}

fn split_type_alias(content: &str) -> Option<(String, String, String)> {
    let (start, interface) = type_declaration_start(content)?;
    if interface {
        return None;
    }
    let eq_index = start + top_level_char(&content[start..], '=')?;
    let semi_index = eq_index + 1 + top_level_char(&content[eq_index + 1..], ';')?;
    Some((
        content[..eq_index + 1].to_string(),
        content[eq_index + 1..semi_index].to_string(),
        content[semi_index..].to_string(),
    ))
}

fn type_body_brace_span(content: &str) -> Option<(usize, usize)> {
    let (start, interface) = type_declaration_start(content)?;
    let body_start = if interface {
        start + "export interface ".len()
    } else {
        start + top_level_char(&content[start..], '=')? + 1
    };
    let (open, close) = find_top_level_brace_span(&content[body_start..])?;
    Some((body_start + open, body_start + close))
}

fn find_top_level_brace_span(input: &str) -> Option<(usize, usize)> {
    let mut state = ScanState::default();
    let mut open_index = None;
    for (index, ch) in input.char_indices() {
        if !state.in_ignored_syntax() && ch == '{' && state.depth.is_top_level() {
            open_index = Some(index);
        }
        state.observe(ch);
        if !state.in_ignored_syntax()
            && ch == '}'
            && state.depth.is_top_level()
            && let Some(open) = open_index
        {
            return Some((open, index));
        }
    }
    None
}

fn split_top_level(input: &str, delimiter: char) -> Vec<String> {
    split_top_level_multi(input, &[delimiter])
}

fn split_top_level_multi(input: &str, delimiters: &[char]) -> Vec<String> {
    let mut state = ScanState::default();
    let mut start = 0usize;
    let mut parts = Vec::new();
    for (index, ch) in input.char_indices() {
        if !state.in_ignored_syntax() && state.depth.is_top_level() && delimiters.contains(&ch) {
            let part = input[start..index].trim();
            if !part.is_empty() {
                parts.push(part.to_string());
            }
            start = index + ch.len_utf8();
        }
        state.observe(ch);
    }
    let tail = input[start..].trim();
    if !tail.is_empty() {
        parts.push(tail.to_string());
    }
    parts
}

fn extract_method_from_arm(arm: &str) -> Option<String> {
    let (open, close) = find_top_level_brace_span(arm)?;
    let inner = &arm[open + 1..close];
    for field in split_top_level(inner, ',') {
        let Some((name, value)) = parse_property(field.as_str()) else {
            continue;
        };
        if name != "method" {
            continue;
        }
        let value = value.trim_start();
        let (literal, _) = parse_string_literal(value)?;
        return Some(literal);
    }
    None
}

fn parse_property(input: &str) -> Option<(String, &str)> {
    let name = parse_property_name(input)?;
    let colon_index = input.find(':')?;
    Some((name, input[colon_index + 1..].trim_start()))
}

fn strip_leading_block_comments(input: &str) -> &str {
    let mut rest = input.trim_start();
    loop {
        let Some(after_prefix) = rest.strip_prefix("/*") else {
            return rest;
        };
        let Some(end_rel) = after_prefix.find("*/") else {
            return rest;
        };
        rest = after_prefix[end_rel + 2..].trim_start();
    }
}

fn parse_property_name(input: &str) -> Option<String> {
    let trimmed = input.trim_start();
    if trimmed.is_empty() {
        return None;
    }
    if let Some((literal, consumed)) = parse_string_literal(trimmed) {
        let rest = trimmed[consumed..].trim_start();
        let rest = rest.strip_prefix('?').unwrap_or(rest).trim_start();
        if rest.starts_with(':') {
            return Some(literal);
        }
        return None;
    }

    let mut end = 0usize;
    for (index, ch) in trimmed.char_indices() {
        if !is_ident_char(ch) {
            break;
        }
        end = index + ch.len_utf8();
    }
    if end == 0 {
        return None;
    }
    let name = &trimmed[..end];
    let rest = trimmed[end..].trim_start();
    let rest = if let Some(stripped) = rest.strip_prefix('?') {
        stripped.trim_start()
    } else {
        rest
    };
    if rest.starts_with(':') {
        return Some(name.to_string());
    }
    None
}

fn parse_string_literal(input: &str) -> Option<(String, usize)> {
    let mut chars = input.char_indices();
    let (start_index, quote) = chars.next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let mut escape = false;
    for (index, ch) in chars {
        if escape {
            escape = false;
            continue;
        }
        if ch == '\\' {
            escape = true;
            continue;
        }
        if ch == quote {
            let literal = input[start_index + 1..index].to_string();
            let consumed = index + ch.len_utf8();
            return Some((literal, consumed));
        }
    }
    None
}

fn is_ident_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_' || ch == '$'
}

#[derive(Default)]
struct ScanState {
    depth: Depth,
    string_delim: Option<char>,
    escape: bool,
    block_comment: bool,
    line_comment: bool,
    previous_char: Option<char>,
}

impl ScanState {
    fn observe(&mut self, ch: char) {
        if self.line_comment {
            if ch == '\n' {
                self.line_comment = false;
            }
            self.previous_char = Some(ch);
            return;
        }

        if self.block_comment {
            if self.previous_char == Some('*') && ch == '/' {
                self.block_comment = false;
                self.previous_char = None;
            } else {
                self.previous_char = Some(ch);
            }
            return;
        }

        if let Some(delim) = self.string_delim {
            if self.escape {
                self.escape = false;
                self.previous_char = Some(ch);
                return;
            }
            if ch == '\\' {
                self.escape = true;
                self.previous_char = Some(ch);
                return;
            }
            if ch == delim {
                self.string_delim = None;
            }
            self.previous_char = Some(ch);
            return;
        }

        if self.previous_char == Some('/') && ch == '/' {
            self.line_comment = true;
            self.previous_char = Some(ch);
            return;
        }

        if self.previous_char == Some('/') && ch == '*' {
            self.block_comment = true;
            self.previous_char = Some(ch);
            return;
        }

        match ch {
            '"' | '\'' => {
                self.string_delim = Some(ch);
            }
            '{' => self.depth.brace += 1,
            '}' => self.depth.brace = (self.depth.brace - 1).max(0),
            '[' => self.depth.bracket += 1,
            ']' => self.depth.bracket = (self.depth.bracket - 1).max(0),
            '(' => self.depth.paren += 1,
            ')' => self.depth.paren = (self.depth.paren - 1).max(0),
            '<' => self.depth.angle += 1,
            '>' if self.depth.angle > 0 => {
                self.depth.angle -= 1;
            }
            _ => {}
        }
        self.previous_char = Some(ch);
    }

    fn in_ignored_syntax(&self) -> bool {
        self.string_delim.is_some() || self.block_comment || self.line_comment
    }
}

#[derive(Default)]
struct Depth {
    brace: i32,
    bracket: i32,
    paren: i32,
    angle: i32,
}

impl Depth {
    fn is_top_level(&self) -> bool {
        self.brace == 0 && self.bracket == 0 && self.paren == 0 && self.angle == 0
    }
}

pub(super) fn prepend_header_if_missing(path: &Path) -> Result<()> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .with_context(|| format!("Failed to open {} for reading", path.display()))?
        .take(MAX_TYPESCRIPT_HEADER_INPUT_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    if bytes.len() as u64 > MAX_TYPESCRIPT_HEADER_INPUT_BYTES {
        return Err(anyhow!(
            "TypeScript file {} exceeds the {MAX_TYPESCRIPT_HEADER_INPUT_BYTES}-byte header processing limit",
            path.display()
        ));
    }
    let content = String::from_utf8(bytes)
        .with_context(|| format!("Failed to read {} as UTF-8", path.display()))?;

    if content.starts_with(GENERATED_TS_HEADER) {
        return Ok(());
    }

    let mut f = fs::File::create(path)
        .with_context(|| format!("Failed to open {} for writing", path.display()))?;
    f.write_all(GENERATED_TS_HEADER.as_bytes())
        .with_context(|| format!("Failed to write header to {}", path.display()))?;
    f.write_all(content.as_bytes())
        .with_context(|| format!("Failed to write content to {}", path.display()))?;
    Ok(())
}

fn ts_files_in(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in
        fs::read_dir(dir).with_context(|| format!("Failed to read dir {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.is_file() && path.extension() == Some(OsStr::new("ts")) {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

pub(super) fn ts_files_in_recursive(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in
            fs::read_dir(&d).with_context(|| format!("Failed to read dir {}", d.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            let kind = entry.file_type()?;
            anyhow::ensure!(!kind.is_symlink(), "linked TypeScript output is not supported: {}", path.display());
            if kind.is_dir() {
                stack.push(path);
            } else if kind.is_file() && path.extension() == Some(OsStr::new("ts")) {
                files.push(path);
            }
        }
    }
    files.sort();
    Ok(files)
}

pub(super) fn trim_trailing_whitespace_in_ts_files(paths: &[PathBuf]) -> Result<()> {
    for path in paths {
        let content = fs::read_to_string(path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        let trimmed = trim_trailing_line_whitespace(&content);
        if trimmed != content {
            fs::write(path, trimmed)
                .with_context(|| format!("Failed to write {}", path.display()))?;
        }
    }
    Ok(())
}

/// Generate an index.ts file that re-exports all generated types.
/// This allows consumers to import all types from a single file.
pub(super) fn generate_index_ts(out_dir: &Path) -> Result<PathBuf> {
    let content = generated_index_ts_with_header(index_ts_entries(
        &ts_files_in(out_dir)?
            .iter()
            .map(PathBuf::as_path)
            .collect::<Vec<_>>(),
        ts_files_in(&out_dir.join("v2"))
            .map(|v| !v.is_empty())
            .unwrap_or(false),
    ));

    let index_path = out_dir.join("index.ts");
    let mut f = fs::File::create(&index_path)
        .with_context(|| format!("Failed to create {}", index_path.display()))?;
    f.write_all(content.as_bytes())
        .with_context(|| format!("Failed to write {}", index_path.display()))?;
    Ok(index_path)
}


fn generated_index_ts_with_header(content: String) -> String {
    let mut with_header = String::with_capacity(GENERATED_TS_HEADER.len() + content.len());
    with_header.push_str(GENERATED_TS_HEADER);
    with_header.push_str(&content);
    with_header
}

fn index_ts_entries(paths: &[&Path], has_v2_ts: bool) -> String {
    let mut stems: Vec<String> = paths
        .iter()
        .filter(|path| path.extension() == Some(OsStr::new("ts")))
        .filter_map(|path| {
            let stem = path.file_stem()?.to_string_lossy().into_owned();
            if stem == "index" { None } else { Some(stem) }
        })
        .filter(|stem| stem != "EventMsg")
        .collect();
    stems.sort();
    stems.dedup();

    let mut entries = String::new();
    for name in stems {
        let _ = writeln!(entries, "export type {{ {name} }} from \"./{name}\";");
    }
    if has_v2_ts {
        entries.push_str("export * as v2 from \"./v2\";\n");
    }
    entries
}

