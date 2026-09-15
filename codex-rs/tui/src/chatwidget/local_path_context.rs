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
const MAX_OMISSION_BYTES: usize = 4 * 1024;
const MAX_SELECTED_PATHS: usize = 16;
const MAX_PATH_CANDIDATES: usize = 256;
const CONTEXT_OMISSION: &str = "\n<context_omission recovery=\"read the original local path; additional content or instructions omitted\">\n";
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
    let project_root = fs::canonicalize(discovery.project_root)
        .unwrap_or_else(|_| discovery.project_root.to_path_buf());
    let discovery = InstructionDiscovery {
        project_root: &project_root,
        candidate_filenames: discovery.candidate_filenames.clone(),
    };
    let mut seen = HashSet::new();
    let mut candidates = HashSet::new();
    let mut instructions_seen = HashSet::new();
    let mut contexts: Vec<(PathBuf, String)> = Vec::new();
    // Keep recovery information inside both the submission and per-path budgets.
    let mut remaining = MAX_TOTAL_CONTEXT_BYTES - MAX_OMISSION_BYTES;
    let tokens = path_tokens(text);
    for (index, token) in tokens.iter().enumerate() {
        if contexts.len() == MAX_SELECTED_PATHS || remaining < 1024 || index == MAX_PATH_CANDIDATES
        {
            if let Some((_, content)) = contexts.last_mut() {
                let omitted = serde_json::to_string(&tokens[index..]).expect("path strings");
                content.push_str(&truncate_context(
                    format!(
                        "\n<context_omission>\nLocal path candidates not collected: {omitted}\n\
                         Read these paths directly. If this list is truncated or the candidate \
                         limit was reached, inspect the original user message for remaining paths.\n\
                         </context_omission>\n"
                    ),
                    MAX_OMISSION_BYTES,
                ));
            }
            break;
        }
        let path = PathBuf::from(token);
        let path = if path.is_absolute() {
            path
        } else {
            cwd.join(path)
        };
        if !candidates.insert(path.clone()) {
            continue;
        }
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) => {
                let content = truncate_context(
                    format!("<local_path_unavailable: {error}>\n"),
                    remaining.min(MAX_OMISSION_BYTES),
                );
                remaining = remaining.saturating_sub(content.len());
                contexts.push((path, content));
                continue;
            }
        };
        if metadata.file_type().is_symlink() || !(metadata.is_file() || metadata.is_dir()) {
            let content = "<local_path_omission: symbolic links and special files are not included automatically; inspect the original path explicitly>\n".to_string();
            remaining = remaining.saturating_sub(content.len());
            contexts.push((path, content));
            continue;
        }
        let identity = fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        if !seen.insert(identity.clone()) {
            continue;
        }
        let content = if metadata.is_dir() {
            render_directory(
                &identity,
                &discovery,
                &mut instructions_seen,
                remaining.min(MAX_CONTEXT_BYTES - MAX_OMISSION_BYTES),
            )
        } else {
            render_file_selection(
                &identity,
                &discovery,
                &mut instructions_seen,
                remaining.min(MAX_CONTEXT_BYTES - MAX_OMISSION_BYTES),
            )
        };
        remaining = remaining.saturating_sub(content.len() + CONTEXT_OMISSION.len());
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
    let mut was_quoted = false;
    for ch in text.chars() {
        if let Some(active_quote) = quote {
            if ch == active_quote {
                quote = None;
            } else {
                current.push(ch);
            }
        } else if ch == '"' || ch == '\'' {
            if current.is_empty() {
                quote = Some(ch);
                was_quoted = true;
            } else {
                current.push(ch);
            }
        } else if ch.is_whitespace() {
            push_token(&mut tokens, &mut current, was_quoted);
            was_quoted = false;
        } else {
            current.push(ch);
        }
    }
    push_token(&mut tokens, &mut current, was_quoted);
    tokens
}

fn push_token(tokens: &mut Vec<String>, current: &mut String, was_quoted: bool) {
    let token =
        current.trim_matches(|ch| matches!(ch, ',' | ';' | '(' | ')' | '[' | ']' | '{' | '}'));
    // Paths must be quoted or have a separator or filename extension. Bare prose
    // and punctuation (especially ".") must not select the working directory.
    if !token.is_empty()
        && !matches!(token, "." | ".." | "/" | "\\")
        && !token.contains("://")
        && (was_quoted
            || !token.strip_prefix('/').is_some_and(|name| {
                name.parse::<crate::slash_command::SlashCommand>().is_ok()
            }))
        && (was_quoted
            || token.contains(['/', '\\'])
            || Path::new(token)
                .extension()
                .is_some_and(|extension| !extension.is_empty()))
        // One lookahead lets collection report that the candidate budget was hit.
        && tokens.len() <= MAX_PATH_CANDIDATES
    {
        tokens.push(token.to_string());
    }
    current.clear();
}

fn render_file_selection(
    path: &Path,
    discovery: &InstructionDiscovery<'_>,
    instructions_seen: &mut HashSet<PathBuf>,
    max_output: usize,
) -> String {
    let max_output = max_output.saturating_sub(CONTEXT_OMISSION.len());
    // Reserve the selected content before filling the remaining space with instructions.
    let mut selected = String::new();
    append_file(
        &mut selected,
        path,
        "selected file",
        MAX_FILE_BYTES,
        max_output,
    );
    let mut output = String::new();
    append_instruction_files(
        &mut output,
        applicable_agent_files(path, discovery),
        instructions_seen,
        max_output.saturating_sub(selected.len()),
    );
    output.push_str(&selected);
    output
}

fn render_directory(
    root: &Path,
    discovery: &InstructionDiscovery<'_>,
    instructions_seen: &mut HashSet<PathBuf>,
    max_output: usize,
) -> String {
    let max_output = max_output.saturating_sub(CONTEXT_OMISSION.len());
    let (entries, inventory_omitted) = directory_entries(root);
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
    append_instruction_files(&mut output, instructions, instructions_seen, max_output / 2);

    append_bounded(&mut output, "[directory inventory]\n", max_output);
    for path in &entries {
        let relative = path.strip_prefix(root).unwrap_or(path);
        let suffix = if path.is_dir() { "/" } else { "" };
        if !append_bounded(
            &mut output,
            &format!("{}{}\n", relative.display(), suffix),
            max_output,
        ) {
            return output;
        }
    }
    if inventory_omitted {
        append_bounded(
            &mut output,
            "<directory_inventory_omission recovery=\"list the original directory for a complete inventory\">\n",
            max_output,
        );
    }
    let mut files_added = 0;
    for path in entries.iter().filter(|path| path.is_file()) {
        if is_instruction_filename(path, discovery) {
            continue;
        }
        if files_added == MAX_DIRECTORY_FILES {
            append_bounded(&mut output, CONTEXT_OMISSION, max_output);
            break;
        }
        let relative = path.strip_prefix(root).unwrap_or(path);
        if !append_file(
            &mut output,
            path,
            &format!("file: {}", relative.display()),
            MAX_FILE_BYTES,
            max_output,
        ) {
            break;
        }
        files_added += 1;
    }
    output
}

fn directory_entries(root: &Path) -> (Vec<PathBuf>, bool) {
    let mut pending = vec![root.to_path_buf()];
    let mut entries = Vec::new();
    let mut examined = 0;
    let mut omitted = false;
    'directories: while let Some(directory) = pending.pop() {
        let Ok(read_dir) = fs::read_dir(directory) else {
            omitted = true;
            continue;
        };
        // Count examined entries, including ignored entries and errors, before
        // allocating paths or sorting. One lookahead detects a partial inventory.
        for entry in read_dir {
            if examined == MAX_DIRECTORY_ENTRIES {
                omitted = true;
                break 'directories;
            }
            examined += 1;
            let Ok(entry) = entry else {
                omitted = true;
                continue;
            };
            let path = entry.path();
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                omitted = true;
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
    (entries, omitted)
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

fn append_instruction_files(
    output: &mut String,
    files: Vec<PathBuf>,
    seen: &mut HashSet<PathBuf>,
    max_output: usize,
) {
    for path in files {
        let identity = fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        if seen.contains(&identity) {
            if !append_bounded(
                output,
                &format!(
                    "[instructions: {}; included earlier in this submission]\n",
                    path.display()
                ),
                max_output,
            ) {
                break;
            }
            continue;
        }
        if !append_file(
            output,
            &path,
            &format!("instructions: {}", path.display()),
            MAX_FILE_BYTES,
            max_output,
        ) {
            break;
        }
        seen.insert(identity);
    }
}

fn append_file(
    output: &mut String,
    path: &Path,
    label: &str,
    max_bytes: usize,
    max_output: usize,
) -> bool {
    let available = max_output.saturating_sub(output.len() + CONTEXT_OMISSION.len());
    if available < 512 {
        append_bounded(output, CONTEXT_OMISSION, max_output);
        return false;
    }
    let mut block = format!("\n[{label}]\n");
    match read_head_and_tail(path, max_bytes.min(available.saturating_sub(512))) {
        Ok((head, tail, _)) if head.contains(&0) || tail.contains(&0) => {
            block.push_str("<binary content omitted>\n");
        }
        Ok((head, tail, original_bytes)) => {
            block.push_str(&String::from_utf8_lossy(&head));
            let retained_bytes = head.len().saturating_add(tail.len());
            if original_bytes > retained_bytes as u64 {
                let omitted_bytes = original_bytes.saturating_sub(retained_bytes as u64);
                block.push_str(&format!(
                    "\n<file_content_omission original_bytes_at_least={original_bytes} omitted_bytes_at_least={omitted_bytes} recovery=\"read the original path; do not infer missing content\">\n"
                ));
                block.push_str(&String::from_utf8_lossy(&tail));
            }
            block.push('\n');
        }
        Err(error) => block.push_str(&format!("<unreadable: {error}>\n")),
    }
    let block = truncate_context(block, available);
    append_bounded(output, &block, max_output)
}

fn read_head_and_tail(path: &Path, max_bytes: usize) -> std::io::Result<(Vec<u8>, Vec<u8>, u64)> {
    let mut file = fs::File::open(path)?;
    let original_bytes = file.metadata()?.len();
    read_head_and_tail_from(&mut file, original_bytes, max_bytes)
}

fn read_head_and_tail_from(
    file: &mut (impl Read + Seek),
    original_bytes: u64,
    max_bytes: usize,
) -> std::io::Result<(Vec<u8>, Vec<u8>, u64)> {
    if original_bytes <= max_bytes as u64 {
        let mut bytes = Vec::new();
        // The file can grow after metadata is read, and virtual files may report zero.
        file.take(max_bytes as u64 + 1).read_to_end(&mut bytes)?;
        let observed_bytes = bytes.len() as u64;
        bytes.truncate(max_bytes);
        return Ok((bytes, Vec::new(), original_bytes.max(observed_bytes)));
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

fn append_bounded(output: &mut String, value: &str, max_output: usize) -> bool {
    if output.len() + value.len() + CONTEXT_OMISSION.len() <= max_output {
        output.push_str(value);
        true
    } else {
        if output.len() + CONTEXT_OMISSION.len() <= max_output {
            output.push_str(CONTEXT_OMISSION);
        }
        false
    }
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
    fn selection_limit_names_uncollected_paths() {
        let temp = tempfile::tempdir().expect("tempdir");
        let names = (0..20)
            .map(|index| format!("file{index}.rs"))
            .collect::<Vec<_>>();
        for name in &names {
            fs::write(temp.path().join(name), "selected contents").expect("write");
        }
        let contexts = collect_with_discovery(
            &names.join(" "),
            temp.path(),
            &default_discovery(temp.path()),
        );
        assert_eq!(contexts.len(), MAX_SELECTED_PATHS);
        let last = &contexts.last().expect("context").1;
        for name in &names[16..] {
            assert!(last.contains(name), "missing recovery path: {name}");
        }
        assert!(
            contexts
                .iter()
                .all(|(_, body)| body.len() <= MAX_CONTEXT_BYTES)
        );
        assert!(
            contexts.iter().map(|(_, body)| body.len()).sum::<usize>() <= MAX_TOTAL_CONTEXT_BYTES
        );
    }

    #[test]
    fn submission_byte_limit_names_remaining_paths_within_budget() {
        let temp = tempfile::tempdir().expect("tempdir");
        let names = (0..12)
            .map(|index| format!("file{index}.rs"))
            .collect::<Vec<_>>();
        for name in &names {
            fs::write(temp.path().join(name), "x".repeat(MAX_FILE_BYTES)).expect("write");
        }
        let contexts = collect_with_discovery(
            &names.join(" "),
            temp.path(),
            &default_discovery(temp.path()),
        );
        assert!(!contexts.is_empty());
        assert!(contexts.len() < names.len());
        let last = &contexts.last().expect("context").1;
        for name in &names[contexts.len()..] {
            assert!(last.contains(name), "missing recovery path: {name}");
        }
        assert!(
            contexts
                .iter()
                .all(|(_, body)| body.len() <= MAX_CONTEXT_BYTES)
        );
        assert!(
            contexts.iter().map(|(_, body)| body.len()).sum::<usize>() <= MAX_TOTAL_CONTEXT_BYTES
        );
    }

    #[test]
    fn candidate_limit_names_first_unexamined_path_and_reports_remaining_message() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::write(temp.path().join("selected.rs"), "SELECTED_CONTENT").expect("write");
        let mut names = vec!["selected.rs"; MAX_PATH_CANDIDATES];
        names.extend(["unexamined.rs", "later.rs"]);
        let contexts = collect_with_discovery(
            &names.join(" "),
            temp.path(),
            &default_discovery(temp.path()),
        );
        assert_eq!(contexts.len(), 1);
        assert!(contexts[0].1.contains("SELECTED_CONTENT"));
        assert!(contexts[0].1.contains("unexamined.rs"));
        assert!(
            contexts[0]
                .1
                .contains("inspect the original user message for remaining paths")
        );
        assert!(!contexts[0].1.contains("local_path_unavailable"));
    }

    #[test]
    fn missing_path_reports_failure_and_keeps_other_selections() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::write(temp.path().join("exists.rs"), "SELECTED_CONTENT").expect("write");
        let contexts = collect_with_discovery(
            "missing.rs exists.rs",
            temp.path(),
            &default_discovery(temp.path()),
        );
        assert_eq!(contexts.len(), 2);
        assert_eq!(contexts[0].0, temp.path().join("missing.rs"));
        assert!(contexts[0].1.contains("local_path_unavailable"));
        assert!(!contexts[0].1.contains("SELECTED_CONTENT"));
        assert!(contexts[1].1.contains("SELECTED_CONTENT"));
    }

    #[test]
    fn symlink_selection_explains_omission_without_reading_target() {
        let temp = tempfile::tempdir().expect("tempdir");
        let target = temp.path().join("target.rs");
        let link = temp.path().join("link.rs");
        fs::write(&target, "TARGET_CONTENT").expect("write");
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&target, &link).expect("symlink");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link).expect("symlink");
        let contexts =
            collect_with_discovery("link.rs", temp.path(), &default_discovery(temp.path()));
        assert_eq!(contexts.len(), 1);
        assert_eq!(contexts[0].0, link);
        assert!(contexts[0].1.contains("symbolic links"));
        assert!(!contexts[0].1.contains("TARGET_CONTENT"));
    }

    #[test]
    fn selected_file_read_is_bounded_when_metadata_understates_length() {
        let mut file = std::io::Cursor::new(b"first123unbounded tail".to_vec());

        let (head, tail, observed_bytes) =
            read_head_and_tail_from(&mut file, 0, 8).expect("read file");

        assert_eq!(head, b"first123");
        assert!(tail.is_empty());
        assert_eq!(file.position(), 9);
        assert_eq!(observed_bytes, 9);
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
        assert!(content.contains("omitted_bytes_at_least="));
        assert!(content.contains("recovery=\"read the original path"));
        assert!(content.contains("do not infer missing content"));
        assert!(content.len() <= MAX_FILE_BYTES + 1024);
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
        assert!(
            content
                .find("selected instructions")
                .expect("selected instructions")
                < content.find("[directory inventory]").expect("inventory")
        );
        assert!(
            content
                .find("nested instructions")
                .expect("nested instructions")
                < content.find("[directory inventory]").expect("inventory")
        );
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
    #[test]
    fn parent_components_do_not_load_sibling_instructions() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::create_dir(temp.path().join("sibling")).expect("mkdir");
        fs::write(temp.path().join("AGENTS.md"), "ROOT_RULE").expect("write");
        fs::write(temp.path().join("sibling/AGENTS.md"), "WRONG_SIBLING_RULE").expect("write");
        fs::write(temp.path().join("selected.rs"), "SELECTED_CONTENT").expect("write");
        let contexts = collect_with_discovery(
            "sibling/../selected.rs",
            temp.path(),
            &default_discovery(temp.path()),
        );
        assert_eq!(contexts.len(), 1);
        assert!(contexts[0].1.contains("ROOT_RULE"));
        assert!(contexts[0].1.contains("SELECTED_CONTENT"));
        assert!(!contexts[0].1.contains("WRONG_SIBLING_RULE"));
    }

    #[test]
    fn selected_files_share_instruction_bodies() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::write(temp.path().join("AGENTS.md"), "SHARED_RULE").expect("write");
        fs::write(temp.path().join("one.rs"), "FIRST_FILE").expect("write");
        fs::write(temp.path().join("two.rs"), "SECOND_FILE").expect("write");
        let contexts = collect_with_discovery(
            "one.rs two.rs",
            temp.path(),
            &default_discovery(temp.path()),
        );
        assert_eq!(contexts.len(), 2);
        let joined = contexts
            .iter()
            .map(|(_, body)| body.as_str())
            .collect::<String>();
        assert_eq!(joined.matches("SHARED_RULE").count(), 1);
        assert!(
            contexts[1]
                .1
                .contains("included earlier in this submission")
        );
        assert!(joined.contains("FIRST_FILE"));
        assert!(joined.contains("SECOND_FILE"));
    }

    #[test]
    fn explicit_paths_avoid_prose_and_keep_apostrophes() {
        assert_eq!(
            path_tokens("don't scan . or README; inspect user's.rs and ./src"),
            vec!["user's.rs", "./src"]
        );
        assert_eq!(
            path_tokens("read \"README\" and 'folder'"),
            vec!["README", "folder"]
        );
    }

    #[test]
    fn directory_scan_and_inventory_are_bounded() {
        let temp = tempfile::tempdir().expect("tempdir");
        for index in 0..MAX_DIRECTORY_ENTRIES + 20 {
            fs::write(temp.path().join(format!("{index}.txt")), "data").expect("write");
        }
        let (entries, omitted) = directory_entries(temp.path());
        assert_eq!(entries.len(), MAX_DIRECTORY_ENTRIES);
        assert!(omitted);
        let contexts = collect_with_discovery("./", temp.path(), &default_discovery(temp.path()));
        assert_eq!(contexts.len(), 1);
        assert!(contexts[0].1.contains("directory_inventory_omission"));
        assert!(contexts[0].1.len() <= MAX_CONTEXT_BYTES);
    }

    #[test]
    fn large_instructions_leave_space_for_selected_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut directory = temp.path().to_path_buf();
        for _ in 0..8 {
            fs::write(directory.join("AGENTS.md"), "RULE".repeat(MAX_FILE_BYTES)).expect("write");
            directory.push("nested");
            fs::create_dir(&directory).expect("mkdir");
        }
        let selected = directory.join("selected.rs");
        fs::write(&selected, "SELECTED_CONTENT").expect("write");
        let contexts = collect_with_discovery(
            selected.to_str().expect("path"),
            temp.path(),
            &default_discovery(temp.path()),
        );
        assert_eq!(contexts.len(), 1);
        assert!(contexts[0].1.contains("SELECTED_CONTENT"));
        assert!(contexts[0].1.contains("context_omission"));
        assert!(contexts[0].1.len() <= MAX_CONTEXT_BYTES);
    }
}
