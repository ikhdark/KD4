use crate::legacy_core::config::Config;
use std::collections::HashSet;
use std::fs;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::path::Path;
use std::path::PathBuf;

const MAX_CONTEXT_BYTES: usize = 64 * 1024;
const MAX_TOTAL_CONTEXT_BYTES: usize = 128 * 1024;
const MAX_SELECTED_PATHS: usize = 16;
const MAX_FILE_BYTES: usize = 16 * 1024;
const MAX_DIRECTORY_ENTRIES: usize = 256;
const MAX_DIRECTORY_FILES: usize = 48;
const LOCAL_AGENTS_MD_FILENAME: &str = "AGENTS.override.md";
const DEFAULT_AGENTS_MD_FILENAME: &str = "AGENTS.md";

fn project_doc_candidate_filenames(config: &Config) -> Vec<&str> {
    let mut names = Vec::with_capacity(2 + config.project_doc_fallback_filenames.len());
    names.push(LOCAL_AGENTS_MD_FILENAME);
    names.push(DEFAULT_AGENTS_MD_FILENAME);
    for candidate in &config.project_doc_fallback_filenames {
        let candidate = candidate.as_str();
        if !candidate.is_empty() && !names.contains(&candidate) {
            names.push(candidate);
        }
    }
    names
}

struct InstructionDiscovery<'a> {
    project_root: &'a Path,
    candidate_filenames: Vec<&'a str>,
}

impl<'a> InstructionDiscovery<'a> {
    fn from_config(config: &'a Config) -> Self {
        let project_root = config
            .config_layer_stack
            .project_discovery()
            .filter(|discovery| discovery.matches_cwd(&config.cwd))
            .map_or(config.cwd.as_path(), |discovery| {
                discovery.project_root().as_path()
            });
        let candidate_filenames = project_doc_candidate_filenames(config);
        Self {
            project_root,
            candidate_filenames,
        }
    }
}

pub(super) fn collect(text: &str, config: &Config) -> Vec<(PathBuf, String)> {
    collect_with_discovery(
        text,
        config.cwd.as_path(),
        &InstructionDiscovery::from_config(config),
    )
}

fn collect_with_discovery(
    text: &str,
    cwd: &Path,
    discovery: &InstructionDiscovery<'_>,
) -> Vec<(PathBuf, String)> {
    let mut seen = HashSet::new();
    let mut contexts = Vec::new();
    let mut remaining = MAX_TOTAL_CONTEXT_BYTES;
    for token in path_tokens(text) {
        if contexts.len() == MAX_SELECTED_PATHS || remaining == 0 {
            break;
        }
        let path = PathBuf::from(&token);
        let path = if path.is_absolute() {
            path
        } else {
            cwd.join(path)
        };
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.file_type().is_symlink() || !(metadata.is_file() || metadata.is_dir()) {
            continue;
        }
        let identity = fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        if !seen.insert(identity) {
            continue;
        }
        let content = if metadata.is_dir() {
            render_directory(&path, discovery)
        } else {
            render_file_selection(&path, discovery)
        };
        let content = truncate_context(content, remaining);
        remaining = remaining.saturating_sub(content.len());
        contexts.push((path, content));
    }
    contexts
}

fn truncate_context(content: String, max_bytes: usize) -> String {
    if content.len() <= max_bytes {
        return content;
    }
    let original_bytes = content.len();
    let mut marker = format!(
        "\n<selected_path_context_omission original_bytes={original_bytes} omitted_bytes=0 \
         recovery=\"read the original local path; do not infer missing content\">\n"
    );
    let mut bounded = String::new();
    for _ in 0..3 {
        let retained_budget = max_bytes.saturating_sub(marker.len());
        let prefix_budget = retained_budget / 2;
        let suffix_budget = retained_budget.saturating_sub(prefix_budget);
        let prefix_end = floor_char_boundary(&content, prefix_budget);
        let suffix_start = ceil_char_boundary(
            &content,
            content.len().saturating_sub(suffix_budget).max(prefix_end),
        );
        let omitted_bytes = suffix_start.saturating_sub(prefix_end);
        let next_marker = format!(
            "\n<selected_path_context_omission original_bytes={original_bytes} \
             omitted_bytes={omitted_bytes} recovery=\"read the original local path; \
             do not infer missing content\">\n"
        );
        bounded = format!(
            "{}{}{}",
            &content[..prefix_end],
            next_marker,
            &content[suffix_start..]
        );
        if next_marker.len() == marker.len() {
            break;
        }
        marker = next_marker;
    }
    bounded.truncate(floor_char_boundary(&bounded, max_bytes));
    bounded
}

fn floor_char_boundary(value: &str, target: usize) -> usize {
    let mut boundary = target.min(value.len());
    while boundary > 0 && !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    boundary
}

fn ceil_char_boundary(value: &str, target: usize) -> usize {
    let mut boundary = target.min(value.len());
    while boundary < value.len() && !value.is_char_boundary(boundary) {
        boundary += 1;
    }
    boundary
}

fn path_tokens(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    for ch in text.chars() {
        if let Some(active_quote) = quote {
            if ch == active_quote {
                quote = None;
            } else {
                current.push(ch);
            }
        } else if ch == '"' || ch == '\'' {
            quote = Some(ch);
        } else if ch.is_whitespace() {
            push_token(&mut tokens, &mut current);
        } else {
            current.push(ch);
        }
    }
    push_token(&mut tokens, &mut current);
    tokens
}

fn push_token(tokens: &mut Vec<String>, current: &mut String) {
    let token =
        current.trim_matches(|ch| matches!(ch, ',' | ';' | '(' | ')' | '[' | ']' | '{' | '}'));
    if !token.is_empty() {
        tokens.push(token.to_string());
    }
    current.clear();
}

fn render_file_selection(path: &Path, discovery: &InstructionDiscovery<'_>) -> String {
    let mut output = String::new();
    append_instruction_files(&mut output, applicable_agent_files(path, discovery));
    append_file(&mut output, path, "selected file", MAX_FILE_BYTES);
    output
}

fn render_directory(root: &Path, discovery: &InstructionDiscovery<'_>) -> String {
    let entries = directory_entries(root);
    let mut output = String::new();
    let mut instructions = applicable_agent_files(root, discovery);
    instructions.extend(
        entries
            .iter()
            .filter(|path| path.is_dir())
            .filter_map(|path| instruction_file_in(path, discovery)),
    );
    instructions.sort();
    instructions.dedup();
    append_instruction_files(&mut output, instructions);

    append_bounded(&mut output, "[directory inventory]\n");
    for path in &entries {
        let relative = path.strip_prefix(root).unwrap_or(path);
        let suffix = if path.is_dir() { "/" } else { "" };
        append_bounded(&mut output, &format!("{}{}\n", relative.display(), suffix));
    }

    let mut files_added = 0;
    for path in entries.iter().filter(|path| path.is_file()) {
        if is_instruction_filename(path, discovery) {
            continue;
        }
        if files_added == MAX_DIRECTORY_FILES || output.len() >= MAX_CONTEXT_BYTES {
            break;
        }
        let relative = path.strip_prefix(root).unwrap_or(path);
        let remaining = MAX_CONTEXT_BYTES.saturating_sub(output.len());
        append_file(
            &mut output,
            path,
            &format!("file: {}", relative.display()),
            MAX_FILE_BYTES.min(remaining),
        );
        files_added += 1;
    }
    output
}

fn directory_entries(root: &Path) -> Vec<PathBuf> {
    let mut pending = vec![root.to_path_buf()];
    let mut entries = Vec::new();
    while let Some(directory) = pending.pop() {
        let Ok(read_dir) = fs::read_dir(directory) else {
            continue;
        };
        let mut children: Vec<PathBuf> =
            read_dir.filter_map(Result::ok).map(|e| e.path()).collect();
        children.sort();
        for path in children {
            if entries.len() == MAX_DIRECTORY_ENTRIES {
                return entries;
            }
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                continue;
            };
            if metadata.file_type().is_symlink() || is_ignored_directory(&path, &metadata) {
                continue;
            }
            if metadata.is_dir() {
                pending.push(path.clone());
            }
            if metadata.is_file() || metadata.is_dir() {
                entries.push(path);
            }
        }
    }
    entries.sort();
    entries
}

fn is_ignored_directory(path: &Path, metadata: &fs::Metadata) -> bool {
    metadata.is_dir()
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| matches!(name, ".git" | "node_modules" | "target"))
}

fn applicable_agent_files(target: &Path, discovery: &InstructionDiscovery<'_>) -> Vec<PathBuf> {
    let target_dir = if target.is_dir() {
        target
    } else {
        target.parent().unwrap_or(target)
    };
    let project_root = discovery.project_root;
    let mut files = Vec::new();
    if let Ok(relative) = target_dir.strip_prefix(project_root) {
        let mut directory = project_root.to_path_buf();
        if let Some(candidate) = instruction_file_in(&directory, discovery) {
            files.push(candidate);
        }
        for component in relative.components() {
            directory.push(component);
            if let Some(candidate) = instruction_file_in(&directory, discovery) {
                files.push(candidate);
            }
        }
    } else {
        if let Some(candidate) = instruction_file_in(target_dir, discovery) {
            files.push(candidate);
        }
    }
    files
}

fn instruction_file_in(directory: &Path, discovery: &InstructionDiscovery<'_>) -> Option<PathBuf> {
    discovery
        .candidate_filenames
        .iter()
        .map(|filename| directory.join(filename))
        .find(|candidate| candidate.is_file())
}

fn is_instruction_filename(path: &Path, discovery: &InstructionDiscovery<'_>) -> bool {
    path.file_name().is_some_and(|filename| {
        discovery
            .candidate_filenames
            .iter()
            .any(|candidate| filename == *candidate)
    })
}

fn append_instruction_files(output: &mut String, files: Vec<PathBuf>) {
    for path in files {
        append_file(
            output,
            &path,
            &format!("instructions: {}", path.display()),
            MAX_FILE_BYTES,
        );
    }
}

fn append_file(output: &mut String, path: &Path, label: &str, max_bytes: usize) {
    if output.len() >= MAX_CONTEXT_BYTES || max_bytes == 0 {
        return;
    }
    append_bounded(output, &format!("\n[{label}]\n"));
    match read_head_and_tail(path, max_bytes) {
        Ok((head, tail, _)) if head.contains(&0) || tail.contains(&0) => {
            append_bounded(output, "<binary content omitted>\n");
        }
        Ok((head, tail, original_bytes)) => {
            append_bounded(output, &String::from_utf8_lossy(&head));
            if tail.is_empty() {
                append_bounded(output, "\n");
            } else {
                let retained_bytes = head.len().saturating_add(tail.len());
                let omitted_bytes = original_bytes.saturating_sub(retained_bytes as u64);
                append_bounded(
                    output,
                    &format!(
                        "\n<file_content_omission original_bytes={original_bytes} \
                         omitted_bytes={omitted_bytes} recovery=\"read the original path \
                         {path:?}; do not infer missing content\">\n"
                    ),
                );
                append_bounded(output, &String::from_utf8_lossy(&tail));
                append_bounded(output, "\n");
            }
        }
        Err(error) => append_bounded(output, &format!("<unreadable: {error}>\n")),
    }
}

fn read_head_and_tail(path: &Path, max_bytes: usize) -> std::io::Result<(Vec<u8>, Vec<u8>, u64)> {
    let mut file = fs::File::open(path)?;
    let original_bytes = file.metadata()?.len();
    if original_bytes <= max_bytes as u64 {
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        return Ok((bytes, Vec::new(), original_bytes));
    }

    let head_budget = max_bytes / 2;
    let tail_budget = max_bytes.saturating_sub(head_budget);
    let mut head = Vec::with_capacity(head_budget);
    file.by_ref()
        .take(head_budget as u64)
        .read_to_end(&mut head)?;
    file.seek(SeekFrom::End(-(tail_budget as i64)))?;
    let mut tail = Vec::with_capacity(tail_budget);
    file.take(tail_budget as u64).read_to_end(&mut tail)?;
    Ok((head, tail, original_bytes))
}

fn append_bounded(output: &mut String, value: &str) {
    let remaining = MAX_CONTEXT_BYTES.saturating_sub(output.len());
    if remaining == 0 {
        return;
    }
    let mut end = remaining.min(value.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    output.push_str(&value[..end]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_discovery(root: &Path) -> InstructionDiscovery<'_> {
        InstructionDiscovery {
            project_root: root,
            candidate_filenames: vec![LOCAL_AGENTS_MD_FILENAME, DEFAULT_AGENTS_MD_FILENAME],
        }
    }

    #[test]
    fn quoted_path_is_captured_with_content() {
        let temp = tempfile::tempdir().expect("tempdir");
        let selected = temp.path().join("with space.txt");
        fs::write(&selected, "selected contents").expect("write file");
        let contexts = collect_with_discovery(
            &format!("inspect \"{}\"", selected.display()),
            temp.path(),
            &default_discovery(temp.path()),
        );
        assert_eq!(contexts.len(), 1);
        assert!(contexts[0].1.contains("selected contents"));
    }

    #[test]
    fn over_truncation_selected_file_keeps_tail_failure_and_recovery() {
        let temp = tempfile::tempdir().expect("tempdir");
        let selected = temp.path().join("large.log");
        let file_content = format!(
            "BEGIN\n{}\nROOT_CAUSE_AT_END",
            "ordinary output\n".repeat(MAX_FILE_BYTES)
        );
        fs::write(&selected, file_content).expect("write file");

        let contexts = collect_with_discovery(
            selected.to_str().expect("utf8 path"),
            temp.path(),
            &default_discovery(temp.path()),
        );
        let content = &contexts[0].1;

        assert!(content.contains("BEGIN"));
        assert!(content.contains("ROOT_CAUSE_AT_END"));
        assert!(content.contains("file_content_omission"));
        assert!(content.contains("omitted_bytes="));
        assert!(content.contains("recovery=\"read the original path"));
        assert!(content.contains("do not infer missing content"));
    }

    #[test]
    fn over_truncation_total_context_cap_keeps_tail_and_recovery() {
        let content = format!("BEGIN{}ROOT_CAUSE_AT_END", "middle".repeat(1_000));

        let bounded = truncate_context(content, 512);

        assert!(bounded.len() <= 512);
        assert!(bounded.starts_with("BEGIN"));
        assert!(bounded.ends_with("ROOT_CAUSE_AT_END"));
        assert!(bounded.contains("selected_path_context_omission"));
        assert!(bounded.contains("original_bytes="));
        assert!(bounded.contains("omitted_bytes="));
        assert!(bounded.contains("recovery=\"read the original local path"));
    }

    #[test]
    fn directory_context_prioritizes_nested_instructions_and_is_bounded() {
        let temp = tempfile::tempdir().expect("tempdir");
        let selected = temp.path().join("selected");
        let nested = selected.join("nested");
        fs::create_dir_all(&nested).expect("create dirs");
        fs::write(selected.join("AGENTS.md"), "selected instructions").expect("write agents");
        fs::write(nested.join("AGENTS.md"), "nested instructions").expect("write agents");
        fs::write(nested.join("code.rs"), "fn example() {}").expect("write code");
        let contexts = collect_with_discovery(
            selected.to_str().expect("utf8 path"),
            temp.path(),
            &default_discovery(temp.path()),
        );
        assert_eq!(contexts.len(), 1);
        let content = &contexts[0].1;
        assert!(content.find("selected instructions") < content.find("[directory inventory]"));
        assert!(content.find("nested instructions") < content.find("[directory inventory]"));
        assert!(content.contains("nested\\code.rs") || content.contains("nested/code.rs"));
        assert!(content.len() <= MAX_CONTEXT_BYTES);
    }

    #[test]
    fn sibling_selection_loads_root_to_target_instructions() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::create_dir(temp.path().join(".git")).expect("create git marker");
        let cwd = temp.path().join("one");
        let selected = temp.path().join("two").join("nested");
        fs::create_dir_all(&cwd).expect("create cwd");
        fs::create_dir_all(&selected).expect("create selected");
        fs::write(temp.path().join("AGENTS.md"), "root instructions").expect("write root agents");
        fs::write(
            temp.path().join("two").join("AGENTS.md"),
            "sibling instructions",
        )
        .expect("write sibling agents");

        let contexts = collect_with_discovery(
            selected.to_str().expect("utf8 path"),
            &cwd,
            &default_discovery(temp.path()),
        );
        let content = &contexts[0].1;
        assert!(content.contains("root instructions"));
        assert!(content.contains("sibling instructions"));
    }

    #[test]
    fn configured_root_and_instruction_precedence_are_shared() {
        let temp = tempfile::tempdir().expect("tempdir");
        let project_root = temp.path().join("workspace");
        let cwd = project_root.join("one");
        let selected = project_root.join("two").join("file.rs");
        fs::create_dir_all(&cwd).expect("create cwd");
        fs::create_dir_all(selected.parent().expect("selected parent")).expect("create selected");
        fs::write(&selected, "fn selected() {}").expect("write selected");
        fs::write(project_root.join("AGENTS.md"), "shadowed instructions")
            .expect("write default instructions");
        fs::write(
            project_root.join("AGENTS.override.md"),
            "root override instructions",
        )
        .expect("write override instructions");
        fs::write(
            selected.parent().expect("selected parent").join("TEAM.md"),
            "fallback instructions",
        )
        .expect("write fallback instructions");
        let discovery = InstructionDiscovery {
            project_root: &project_root,
            candidate_filenames: vec!["AGENTS.override.md", "AGENTS.md", "TEAM.md"],
        };

        let contexts =
            collect_with_discovery(selected.to_str().expect("utf8 path"), &cwd, &discovery);
        let content = &contexts[0].1;
        assert!(content.contains("root override instructions"));
        assert!(content.contains("fallback instructions"));
        assert!(!content.contains("shadowed instructions"));
    }
}
