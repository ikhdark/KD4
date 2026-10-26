use std::cell::RefCell;
use std::path::PathBuf;

use tree_sitter::Node;
use tree_sitter::Parser;
use tree_sitter::Tree;
use tree_sitter_bash::LANGUAGE as BASH;

use crate::shell_detect::ShellType;
use crate::shell_detect::detect_shell_type;

thread_local! {
    static BASH_PARSER: RefCell<Parser> = RefCell::new(new_bash_parser());
}


fn new_bash_parser() -> Parser {
    let lang = BASH.into();
    let mut parser = Parser::new();
    #[expect(clippy::expect_used)]
    parser.set_language(&lang).expect("load bash grammar");
    parser
}

/// Parse the provided bash source using tree-sitter-bash, returning a Tree on
/// success or None if parsing failed.
pub fn try_parse_shell(shell_lc_arg: &str) -> Option<Tree> {
    BASH_PARSER.with(|parser| parser.borrow_mut().parse(shell_lc_arg, /*old_tree*/ None))
}

/// Parse a script which may contain multiple simple commands joined only by
/// the safe logical/pipe/sequencing operators: `&&`, `||`, `;`, `|`.
///
/// Returns `Some(Vec<command_words>)` if every command is a plain word‑only
/// command and the parse tree does not contain disallowed constructs
/// (parentheses, redirections, substitutions, control flow, etc.). Otherwise
/// returns `None`.
pub fn try_parse_word_only_commands_sequence(tree: &Tree, src: &str) -> Option<Vec<Vec<String>>> {
    try_parse_word_only_commands_with_operators(tree, src).map(|(commands, _)| commands)
}

pub(crate) fn try_parse_word_only_commands_with_operators(
    tree: &Tree,
    src: &str,
) -> Option<(Vec<Vec<String>>, Vec<String>)> {
    if tree.root_node().has_error() {
        return None;
    }

    // List of allowed (named) node kinds for a "word only commands sequence".
    // If we encounter a named node that is not in this list we reject.
    const ALLOWED_KINDS: &[&str] = &[
        // top level containers
        "program",
        "list",
        "pipeline",
        // commands & words
        "command",
        "command_name",
        "word",
        "string",
        "string_content",
        "raw_string",
        "number",
        "concatenation",
    ];
    // Allow only safe punctuation / operator tokens; anything else causes reject.
    const ALLOWED_PUNCT_TOKENS: &[&str] = &["&&", "||", ";", "|", "\"", "'"];

    let root = tree.root_node();
    let mut cursor = root.walk();
    let mut stack = vec![root];
    let mut command_nodes = Vec::new();
    while let Some(node) = stack.pop() {
        let kind = node.kind();
        if node.is_named() {
            if !ALLOWED_KINDS.contains(&kind) {
                return None;
            }
            if kind == "command" {
                command_nodes.push(node);
            }
        } else {
            // Reject any punctuation / operator tokens that are not explicitly allowed.
            if kind.chars().any(|c| "&;|".contains(c)) && !ALLOWED_PUNCT_TOKENS.contains(&kind) {
                return None;
            }
            if !(ALLOWED_PUNCT_TOKENS.contains(&kind) || kind.trim().is_empty()) {
                // If it's a quote token or operator it's allowed above; we also allow whitespace tokens.
                // Any other punctuation like parentheses, braces, redirects, backticks, etc are rejected.
                return None;
            }
        }
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }

    // Walk uses a stack (LIFO), so re-sort by position to restore source order.
    command_nodes.sort_by_key(Node::start_byte);

    let operators = command_nodes
        .windows(2)
        .map(|nodes| {
            let gap = src[nodes[0].end_byte()..nodes[1].start_byte()].trim();
            if gap.is_empty() {
                ";".to_string()
            } else {
                gap.to_string()
            }
        })
        .collect();
    let mut commands = Vec::new();
    for node in command_nodes {
        commands.push(parse_plain_command_from_node(node, src)?);
    }
    Some((commands, operators))
}

/// Parses a shell script consisting only of plain commands joined by safe operators.
pub fn parse_shell_script_into_commands(script: &str) -> Option<Vec<Vec<String>>> {
    let tree = try_parse_shell(script)?;
    try_parse_word_only_commands_sequence(&tree, script)
}

/// Possible literal command prefixes in opaque source, for conservative
/// admission only. AST command ownership excludes comments and quoted examples,
/// but does not prove execution count, argv completeness or exit authority.
pub fn possible_command_argvs(script: &str) -> Vec<Vec<String>> {
    let Some(tree) = try_parse_shell(script) else {
        return Vec::new();
    };
    let mut stack = vec![tree.root_node()];
    let mut commands = Vec::new();
    while let Some(node) = stack.pop() {
        if node.kind() == "command" {
            if let Some(argv) = parse_plain_command_from_node(node, script) {
                commands.push(argv);
            } else if let Ok(source) = node.utf8_text(script.as_bytes())
                && let Some(mut argv) = shlex::split(source)
            {
                let assignments = argv
                    .iter()
                    .take_while(|arg| {
                        arg.split_once('=').is_some_and(|(name, _)| {
                            !name.is_empty()
                                && name
                                    .chars()
                                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
                        })
                    })
                    .count();
                argv.drain(..assignments);
                commands.push(argv);
            }
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    commands
}

pub fn extract_bash_command(command: &[String]) -> Option<(&str, &str)> {
    let [shell, flag, script] = command else {
        return None;
    };
    if !matches!(flag.as_str(), "-lc" | "-c")
        || !matches!(
            detect_shell_type(PathBuf::from(shell)),
            Some(ShellType::Zsh) | Some(ShellType::Bash) | Some(ShellType::Sh)
        )
    {
        return None;
    }
    Some((shell, script))
}

/// Returns the sequence of plain commands within a `bash -lc "..."` or
/// `zsh -lc "..."` invocation when the script only contains word-only commands
/// joined by safe operators.
pub fn parse_shell_lc_plain_commands(command: &[String]) -> Option<Vec<Vec<String>>> {
    let (_, script) = extract_bash_command(command)?;
    parse_shell_script_into_commands(script)
}

/// Returns the parsed argv for a single shell command in a here-doc style
/// script (`<<`), as long as the script contains exactly one command node.
pub fn parse_shell_lc_single_command_prefix(command: &[String]) -> Option<Vec<String>> {
    let (_, script) = extract_bash_command(command)?;
    let tree = try_parse_shell(script)?;
    let root = tree.root_node();
    if root.has_error() {
        return None;
    }
    let mut cursor = root.walk();
    let statements: Vec<_> = root
        .named_children(&mut cursor)
        .filter(|node| node.kind() != "comment")
        .collect();
    let [statement] = statements.as_slice() else {
        return None;
    };
    if !matches!(statement.kind(), "command" | "redirected_statement") {
        return None;
    }
    if statement.kind() == "redirected_statement" {
        let mut cursor = statement.walk();
        if statement
            .named_children(&mut cursor)
            .any(|node| !matches!(node.kind(), "command" | "heredoc_redirect" | "comment"))
        {
            return None;
        }
    }
    if !has_named_descendant_kind(root, "heredoc_redirect") {
        return None;
    }
    if !has_only_clearly_quoted_heredoc_delimiters(root, script) {
        return None;
    }
    if has_named_descendant_kind(root, "file_redirect")
        || has_named_descendant_kind(root, "herestring_redirect")
    {
        return None;
    }

    let command_node = find_single_command_node(root)?;
    parse_heredoc_command_words(command_node, script)
}

fn parse_plain_command_from_node(cmd: tree_sitter::Node, src: &str) -> Option<Vec<String>> {
    if cmd.kind() != "command" {
        return None;
    }
    let mut words = Vec::new();
    let mut cursor = cmd.walk();
    for child in cmd.named_children(&mut cursor) {
        match child.kind() {
            "command_name" => {
                let word_node = child.named_child(0)?;
                if word_node.kind() != "word" {
                    return None;
                }
                words.push(parse_literal_word(word_node, src)?);
            }
            "word" | "number" => {
                words.push(parse_literal_word(child, src)?);
            }
            "string" => {
                let parsed = parse_double_quoted_string(child, src)?;
                words.push(parsed);
            }
            "raw_string" => {
                let parsed = parse_raw_string(child, src)?;
                words.push(parsed);
            }
            "concatenation" => {
                // Handle concatenated arguments like -g"*.py"
                let mut concatenated = String::new();
                let mut concat_cursor = child.walk();
                for part in child.named_children(&mut concat_cursor) {
                    match part.kind() {
                        "word" | "number" => {
                            concatenated.push_str(&parse_literal_word(part, src)?);
                        }
                        "string" => {
                            let parsed = parse_double_quoted_string(part, src)?;
                            concatenated.push_str(&parsed);
                        }
                        "raw_string" => {
                            let parsed = parse_raw_string(part, src)?;
                            concatenated.push_str(&parsed);
                        }
                        _ => return None,
                    }
                }
                if concatenated.is_empty() {
                    return None;
                }
                words.push(concatenated);
            }
            _ => return None,
        }
    }
    Some(words)
}

fn parse_heredoc_command_words(cmd: Node<'_>, src: &str) -> Option<Vec<String>> {
    if cmd.kind() != "command" {
        return None;
    }

    let mut words = Vec::new();
    let mut cursor = cmd.walk();
    for child in cmd.named_children(&mut cursor) {
        match child.kind() {
            "command_name" => {
                let word_node = child.named_child(0)?;
                if !matches!(word_node.kind(), "word" | "number")
                    || !is_literal_word_or_number(word_node)
                {
                    return None;
                }
                words.push(parse_literal_word(word_node, src)?);
            }
            "word" | "number" => {
                if !is_literal_word_or_number(child) {
                    return None;
                }
                words.push(parse_literal_word(child, src)?);
            }
            // Allow heredoc constructs that attach stdin to a single command
            // without changing argv matching semantics for the executable
            // prefix. Other file redirects may write outside the sandbox and
            // must not be collapsed to the executable prefix for execpolicy.
            "comment" => {}
            kind if is_allowed_heredoc_attachment_kind(kind) => {}
            _ => return None,
        }
    }

    if words.is_empty() { None } else { Some(words) }
}

fn parse_literal_word(node: Node<'_>, src: &str) -> Option<String> {
    if !is_literal_word_or_number(node) {
        return None;
    }
    let text = node.utf8_text(src.as_bytes()).ok()?;
    // These unquoted forms need escape decoding or runtime expansion. Preserve
    // the outer shell invocation rather than claiming their source is exact argv.
    if text.starts_with('=') || text.contains(['\\', '*', '?', '[', ']', '{', '}', '~']) {
        return None;
    }
    Some(text.to_string())
}

fn is_literal_word_or_number(node: Node<'_>) -> bool {
    if !matches!(node.kind(), "word" | "number") {
        return false;
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor).next().is_none()
}

fn is_allowed_heredoc_attachment_kind(kind: &str) -> bool {
    matches!(
        kind,
        "heredoc_body"
            | "simple_heredoc_body"
            | "heredoc_redirect"
            | "herestring_redirect"
            | "redirected_statement"
    )
}

fn has_only_clearly_quoted_heredoc_delimiters(root: Node<'_>, src: &str) -> bool {
    let mut stack = vec![root];
    let mut found = false;
    while let Some(node) = stack.pop() {
        if node.kind() == "heredoc_start" {
            found = true;
            let Ok(text) = node.utf8_text(src.as_bytes()) else {
                return false;
            };
            let text = text.trim();
            let delimiter = text
                .strip_prefix("<<-")
                .or_else(|| text.strip_prefix("<<"))
                .unwrap_or(text)
                .trim();
            let quoted = (delimiter.starts_with('\'') && delimiter.ends_with('\''))
                || (delimiter.starts_with('"') && delimiter.ends_with('"'));
            if delimiter.len() < 2 || !quoted {
                return false;
            }
        }

        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
    }
    found
}

fn find_single_command_node(root: Node<'_>) -> Option<Node<'_>> {
    let mut stack = vec![root];
    let mut single_command = None;
    while let Some(node) = stack.pop() {
        if node.kind() == "command" {
            if single_command.is_some() {
                return None;
            }
            single_command = Some(node);
        }

        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
    }
    single_command
}

fn has_named_descendant_kind(node: Node<'_>, kind: &str) -> bool {
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if current.kind() == kind {
            return true;
        }
        let mut cursor = current.walk();
        for child in current.named_children(&mut cursor) {
            stack.push(child);
        }
    }
    false
}

fn parse_double_quoted_string(node: Node, src: &str) -> Option<String> {
    if node.kind() != "string" {
        return None;
    }

    let mut cursor = node.walk();
    for part in node.named_children(&mut cursor) {
        if part.kind() != "string_content" {
            return None;
        }
    }
    let raw = node.utf8_text(src.as_bytes()).ok()?;
    let stripped = raw
        .strip_prefix('"')
        .and_then(|text| text.strip_suffix('"'))?;
    // Bash gives backslashes semantic meaning inside double quotes. Until this
    // parser decodes every supported escape exactly, reject such strings so
    // authorization falls back to the exact outer shell argv.
    if stripped.contains('\\') {
        return None;
    }
    Some(stripped.to_string())
}

fn parse_raw_string(node: Node, src: &str) -> Option<String> {
    if node.kind() != "raw_string" {
        return None;
    }

    let raw_string = node.utf8_text(src.as_bytes()).ok()?;
    let stripped = raw_string
        .strip_prefix('\'')
        .and_then(|s| s.strip_suffix('\''));
    stripped.map(str::to_owned)
}

