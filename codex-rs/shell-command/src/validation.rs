//! Pure command recognition. Classification is not proof that a command ran.



use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationOperation {
    Test,
    Check,
    Lint,
    Bench,
    Fuzz,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationCommandDescriptor {
    pub operation: ValidationOperation,
    pub mode: ValidationExecutionMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationExecutionMode {
    Execution,
    Listing,
    CompilationOnly,
    Mutation,
    Checking,
    DryRun,
    Unknown,
}

impl ValidationExecutionMode {
    pub fn can_prove_validation(self) -> bool {
        // Compilation proves a static check, never test-body execution.
        matches!(
            self,
            Self::Execution | Self::Checking | Self::CompilationOnly
        )
    }
}

/// Repository-owned entrypoints. These declarations must be loaded from trusted
/// repository state by the caller, never from command output or tool arguments.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryRunner {
    #[serde(skip)]
    pub path_context: Option<(std::path::PathBuf, std::path::PathBuf)>,
    pub programs: Vec<String>,
    pub prefixes: Vec<Vec<String>>,
    #[serde(default)]
    pub operations: Vec<ValidationOperation>,
    #[serde(default)]
    pub options: std::collections::BTreeMap<String, usize>,
    /// Just variables whose overrides are limited to finite, nonnegative
    /// decimal literals. Unlike an unrestricted `--set: 2` option, this cannot
    /// change the recipe's executable, working directory, or shell.
    #[serde(default)]
    pub just_numeric_variables: Vec<String>,
    #[serde(default)]
    pub allow_extra_args: bool,
    #[serde(default)]
    pub receipt_runner: Option<String>,
    /// Authenticates non-execution admission reports, not test receipts.
    #[serde(default)]
    pub admission_runner: Option<String>,
    /// Classify child argv; passthrough wrappers cannot authenticate receipts.
    #[serde(default)]
    pub passthrough_after: Option<String>,
    /// Positional transport prefix, including recipe and lane arguments.
    #[serde(default)]
    pub passthrough_args: Option<usize>,
    #[serde(default)]
    pub execution_mode: Option<ValidationExecutionMode>,
}

impl RepositoryRunner {
    fn normalized_args<'a>(&self, program: &str, args: &'a [String]) -> Option<&'a [String]> {
        if normalized_program_name(program) == "just" { self.just_runner_args(args) } else { Some(args) }
    }

    pub fn matches(&self, program: &str, args: &[String]) -> bool {
        let Some(args) = self.normalized_args(program, args) else { return false; };
        if self.passthrough_args.is_some()
            && (self.receipt_runner.is_some() || self.passthrough_after.is_some())
        {
            return false;
        }
        let args = if let Some(separator) = &self.passthrough_after {
            if separator.is_empty() || self.receipt_runner.is_some() {
                return false;
            }
            let Some(index) = args.iter().position(|arg| arg == separator) else {
                return false;
            };
            &args[..index]
        } else {
            args
        };
        if (self.operations.is_empty() && self.passthrough_after.is_none() && self.passthrough_args.is_none())
            || !self
                .programs
                .iter()
                .any(|expected| expected == &normalized_program_name(program))
            || args
                .iter()
                .any(|arg| matches!(arg.as_str(), "-h" | "--help" | "--version"))
        {
            return false;
        }
        self.prefixes.iter().any(|prefix| {
            if prefix.is_empty() {
                return false;
            }
            let mut index = 0;
            for expected in prefix {
                while let Some(arg) = args.get(index) {
                    if let Some(count) = self.options.get(arg) {
                        index += count + 1;
                    } else if arg
                        .split_once('=')
                        .is_some_and(|(key, _)| self.options.get(key) == Some(&1))
                    {
                        index += 1;
                    } else {
                        break;
                    }
                }
                let matches = args.get(index).is_some_and(|actual| {
                    if expected.contains('/')
                        && let Some((root, cwd)) = &self.path_context
                    {
                        use codex_utils_absolute_path::AbsolutePathBuf;
                        AbsolutePathBuf::resolve_path_against_base(expected, root)
                            == AbsolutePathBuf::resolve_path_against_base(actual, cwd)
                    } else {
                        actual == expected
                    }
                });
                if !matches {
                    return false;
                }
                index += 1;
            }
            index == args.len() || self.allow_extra_args
        })
    }

    fn just_runner_args<'a>(&self, mut args: &'a [String]) -> Option<&'a [String]> {
        while args.first().is_some_and(|arg| arg == "--set") {
            let name = args.get(1)?;
            let value = args.get(2)?;
            if !self.just_numeric_variables.contains(name)
                || !value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || byte == b'.')
                || !value
                    .parse::<f64>()
                    .is_ok_and(|number| number.is_finite() && number >= 0.0)
            {
                return None;
            }
            args = &args[3..];
        }
        // Never authenticate a different producer or a nonexecuting invocation
        // through extra args. Just passes words after the recipe to the recipe,
        // so a recipe-forwarded `-E <filterset>` is a nextest filter, not
        // Just's `--dotenv-path`; a leading global cannot match a runner prefix.
        if args.iter().any(|arg| {
            let flag = arg.split_once('=').map_or(arg.as_str(), |(flag, _)| flag);
            matches!(
                flag,
                "--set"
                    | "--justfile"
                    | "--file"
                    | "--justfile-name"
                    | "--ceiling"
                    | "--global-justfile"
                    | "-g"
                    | "--working-directory"
                    | "--directory"
                    | "--shell"
                    | "--shell-arg"
                    | "--clear-shell-args"
                    | "--shell-command"
                    | "--command"
                    | "--dry-run"
                    | "-n"
                    | "--list"
                    | "-l"
                    | "--show"
                    | "-s"
                    | "--summary"
                    | "--dump"
                    | "--evaluate"
                    | "--choose"
                    | "--usage"
                    | "--changelog"
                    | "--clean"
                    | "--completions"
                    | "--edit"
                    | "-e"
                    | "--fmt"
                    | "--init"
                    | "--json"
                    | "--groups"
                    | "--man"
                    | "--variables"
                    | "--dotenv-path"
                    | "--dotenv-filename"
                    | "--dotenv-command"
                    | "--no-deps"
            ) || (!arg.starts_with("--")
                && ["-f", "-d", "-c", "-F"]
                    .iter()
                    .any(|flag| arg.starts_with(*flag)))
                || arg.split_once('=').is_some_and(|(name, _)| {
                    !name.is_empty()
                        && name
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                })
        }) {
            return None;
        }
        Some(args)
    }
}

/// Only a standalone deterministic command may authenticate a receipt. In
/// particular, `runner; echo receipt`, pipelines, and shell expansion cannot.
pub fn standalone_argv(script: &str) -> Option<Vec<String>> {
    let tree = crate::bash::try_parse_shell(script)?;
    let (mut commands, operators) =
        crate::bash::try_parse_word_only_commands_with_operators(&tree, script)?;
    (commands.len() == 1 && operators.is_empty()).then(|| commands.remove(0))
}

/// Preserve PowerShell literal arguments instead of applying POSIX escaping.
/// Reuse the syntax owner's single-command proof: pipelines, redirections,
/// expressions and compound scripts cannot authenticate a runner receipt.
pub fn standalone_powershell_argv(script: &str) -> Option<Vec<String>> {
    crate::command_safety::try_parse_powershell_ast_analysis("pwsh", script)?
        .direct_argv
        .map(|candidate| candidate.argv)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationClassification {
    NonValidation,
    Validation {
        leaves: Vec<ValidationCommandDescriptor>,
        has_unclassified_targets: bool,
        // A compound command's exit code does not establish which validation
        // stages executed or whether an earlier failure was masked.
        exit_code_is_authoritative: bool,
    },
    Opaque,
}

/// Launch admission, not receipt evidence. Opaque work is intentionally distinct
/// from recognized validation so callers need not ban unrelated scripts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationPolicyClassification {
    Unrelated,
    ReadOnly,
    TargetedCompilation,
    Benchmark,
    /// Other tooling operations; this is admission classification, not receipt proof.
    Tooling,
    /// Test execution and full-suite runners, in any language.
    Tests,
    /// Non-compilation checks, including diagnostic-only lint/format/schema checks.
    Checks,
    Restricted,
    Opaque,
}

impl ValidationPolicyClassification {
    pub fn is_validation(self) -> bool {
        matches!(self, Self::TargetedCompilation | Self::Benchmark | Self::Tooling | Self::Tests | Self::Checks | Self::Restricted)
    }

    pub fn is_compile_eligible(self) -> bool {
        matches!(self, Self::TargetedCompilation)
    }
}

fn combine_policy_classifications(
    classifications: impl IntoIterator<Item = ValidationPolicyClassification>,
) -> ValidationPolicyClassification {
    use ValidationPolicyClassification as Policy;
    classifications.into_iter().reduce(|left, right| {
        // Preserve prohibited work even beside opaque, benchmark or tooling leaves.
        for prohibited in [Policy::Tests, Policy::Checks] {
            if left == prohibited || right == prohibited { return prohibited; }
        }
        // Benchmark/tooling permission must not cover an unclassified sibling workload.
        if (matches!(left, Policy::Benchmark | Policy::Tooling) && matches!(right, Policy::Opaque | Policy::Unrelated))
            || (matches!(right, Policy::Benchmark | Policy::Tooling) && matches!(left, Policy::Opaque | Policy::Unrelated))
        {
            return Policy::Restricted;
        }
        // Never lose a recognized prohibited command behind an opaque sibling.
        for candidate in [Policy::Restricted, Policy::Benchmark, Policy::Tooling, Policy::TargetedCompilation, Policy::Opaque, Policy::ReadOnly] {
            if left == candidate || right == candidate {
                return candidate;
            }
        }
        Policy::Unrelated
    }).unwrap_or(Policy::Unrelated)
}

pub fn classify_powershell_script(script: &str) -> ValidationClassification {
    classify_powershell_script_with_runners(script, &[])
}

pub fn classify_powershell_script_with_runners(
    script: &str,
    runners: &[RepositoryRunner],
) -> ValidationClassification {
    // Preserve control-flow information before the PowerShell parser flattens
    // pipelines and command chains into argv leaves.
    let simple = classify_simple_script(script, 0, runners, true);
    if matches!(
        simple,
        ValidationClassification::Validation {
            exit_code_is_authoritative: true,
            ..
        }
    ) {
        return simple;
    }
    let command = vec![
        "pwsh".to_string(),
        "-Command".to_string(),
        script.to_string(),
    ];
    let Some(commands) = crate::powershell::parse_powershell_command_into_plain_commands(&command)
    else {
        return simple;
    };
    if commands.is_empty() {
        return simple;
    }
    let mut combined =
        combine_validation_classifications(commands.into_iter().filter_map(|argv| {
            let mut arguments = argv.into_iter();
            let program = arguments.next()?;
            Some(classify_argv_with_runners(
                &program,
                &arguments.collect::<Vec<_>>(),
                runners,
            ))
        }));
    // The word-only fallback discards exit guards and control-flow lineage.
    // It can recover intent, never upgrade the fast path's exit authority.
    if let ValidationClassification::Validation {
        exit_code_is_authoritative,
        ..
    } = &mut combined
    {
        *exit_code_is_authoritative = false;
    }
    combined
}

pub fn combine_validation_classifications(
    classifications: impl IntoIterator<Item = ValidationClassification>,
) -> ValidationClassification {
    let mut leaves = Vec::new();
    let mut has_unclassified_targets = false;
    let mut saw_opaque = false;
    let mut command_count = 0;
    let mut exit_code_is_authoritative = true;
    for classification in classifications {
        command_count += 1;
        match classification {
            ValidationClassification::Validation {
                leaves: mut found,
                has_unclassified_targets: found_unclassified,
                exit_code_is_authoritative: found_authoritative,
            } => {
                leaves.append(&mut found);
                has_unclassified_targets |= found_unclassified;
                exit_code_is_authoritative &= found_authoritative;
            }
            ValidationClassification::Opaque => saw_opaque = true,
            ValidationClassification::NonValidation => {}
        }
    }
    if leaves.is_empty() {
        if saw_opaque {
            ValidationClassification::Opaque
        } else {
            ValidationClassification::NonValidation
        }
    } else {
        ValidationClassification::Validation {
            leaves,
            has_unclassified_targets: has_unclassified_targets || saw_opaque,
            exit_code_is_authoritative: command_count == 1
                && exit_code_is_authoritative
                && !saw_opaque,
        }
    }
}

const MAX_WRAPPER_DEPTH: usize = 4;

fn classify_simple_script(
    script: &str,
    depth: usize,
    runners: &[RepositoryRunner],
    powershell: bool,
) -> ValidationClassification {
    if depth > MAX_WRAPPER_DEPTH {
        return ValidationClassification::Opaque;
    }
    if !powershell {
        let Some(tree) = crate::bash::try_parse_shell(script) else {
            return ValidationClassification::Opaque;
        };
        let Some((commands, operators)) =
            crate::bash::try_parse_word_only_commands_with_operators(&tree, script)
        else {
            let root = tree.root_node();
            if root.has_error() {
                // Not POSIX at all: session scripts keep that shell's syntax
                // (PowerShell guards on Windows). Recover intent literally,
                // never exit authority.
                let mut recovered = classify_literal_script(script, depth, runners, false);
                if let ValidationClassification::Validation {
                    exit_code_is_authoritative,
                    ..
                } = &mut recovered
                {
                    *exit_code_is_authoritative = false;
                }
                return recovered;
            }
            // A whole-script brace group or subshell runs its body and returns
            // its status. Other valid POSIX outside the word-only subset
            // (comments, expansions, redirections, heredocs) stays opaque.
            if root.named_child_count() == 1
                && root
                    .named_child(0)
                    .is_some_and(|node| matches!(node.kind(), "compound_statement" | "subshell"))
                && let Some(body) = grouped_command_body(script.trim())
            {
                return classify_simple_script(body, depth + 1, runners, false);
            }
            return ValidationClassification::Opaque;
        };
        let classifications = commands
            .iter()
            .filter_map(|argv| {
                let (program, args) = argv.split_first()?;
                Some(classify_argv_at_depth(program, args, depth + 1, runners))
            })
            .collect::<Vec<_>>();
        let authoritative = operators.iter().all(|operator| operator == "&&")
            && classifications.iter().all(|classification| {
                matches!(
                    classification,
                    ValidationClassification::Validation {
                        exit_code_is_authoritative: true,
                        has_unclassified_targets: false,
                        ..
                    }
                )
            });
        let mut combined = combine_validation_classifications(classifications);
        if let ValidationClassification::Validation {
            exit_code_is_authoritative,
            ..
        } = &mut combined
        {
            *exit_code_is_authoritative = authoritative;
        }
        return combined;
    }
    // The fast path is deliberately literal. Native AST fallback may recover
    // intent for expansion/comments but cannot supply the missing exit proof.
    if script.contains(['$', '#', '`', '\\']) {
        return ValidationClassification::Opaque;
    }
    classify_literal_script(script, depth, runners, powershell)
}

/// Literal top-level splitting; callers decide whether its exit authority applies.
fn classify_literal_script(
    script: &str,
    depth: usize,
    runners: &[RepositoryRunner],
    powershell: bool,
) -> ValidationClassification {
    if depth > MAX_WRAPPER_DEPTH {
        return ValidationClassification::Opaque;
    }
    let Some((commands, success_chain)) = split_deterministic_script(script) else {
        return ValidationClassification::Opaque;
    };
    let classifications = commands
        .into_iter()
        .filter_map(|command| {
            let command = command.trim();
            // Unlike a POSIX command group, a bare PowerShell script block is
            // a value. Its body has not run and cannot establish validation.
            if powershell && command.starts_with('{') && grouped_command_body(command).is_some() {
                return Some(ValidationClassification::NonValidation);
            }
            // A guard such as `if ($LASTEXITCODE -eq 0) { cargo test }` runs
            // the same commands a flat sequence would, so its condition and
            // bodies are classified instead of the `if` keyword.
            if let Some(parts) = powershell_if_statement_parts(command)
                .or_else(|| grouped_command_body(command).map(|body| vec![body]))
            {
                // Non-POSIX session syntax (the only non-PowerShell caller)
                // stays literal; its parts are not bash either.
                let mut classification = combine_validation_classifications(parts.into_iter().map(
                    |part| {
                        if powershell {
                            classify_simple_script(part, depth + 1, runners, powershell)
                        } else {
                            classify_literal_script(part, depth + 1, runners, powershell)
                        }
                    },
                ));
                if powershell && let ValidationClassification::Validation { exit_code_is_authoritative, .. } = &mut classification {
                    *exit_code_is_authoritative = false;
                }
                return Some(classification);
            }
            let Some(words) = shlex::split(command) else {
                return Some(ValidationClassification::Opaque);
            };
            let first_command = words
                .iter()
                .position(|word| !is_shell_assignment(word))
                .unwrap_or(words.len());
            let program = words.get(first_command)?;
            Some(classify_argv_at_depth(
                program,
                &words[first_command + 1..],
                depth + 1,
                runners,
            ))
        })
        .collect::<Vec<_>>();
    // Only an all-validation && chain proves every suite ran successfully.
    // Semicolons, ||, opaque setup, and trailing commands can mask failures or
    // change the workspace after validation. Keep those conservative.
    let authoritative_chain = success_chain
        && classifications.iter().all(|classification| {
            matches!(
                classification,
                ValidationClassification::Validation {
                    exit_code_is_authoritative: true,
                    has_unclassified_targets: false,
                    ..
                }
            )
        });
    let mut combined = combine_validation_classifications(classifications);
    if let ValidationClassification::Validation {
        exit_code_is_authoritative,
        ..
    } = &mut combined
    {
        *exit_code_is_authoritative |= authoritative_chain;
    }
    combined
}

fn split_deterministic_script(script: &str) -> Option<(Vec<&str>, bool)> {
    split_script(script, false)
}

fn split_script(script: &str, allow_pipelines: bool) -> Option<(Vec<&str>, bool)> {
    if script.contains("$(") || script.contains("${") || script.contains('`') {
        return None;
    }
    let bytes = script.as_bytes();
    let mut commands = Vec::new();
    let mut start = 0;
    let mut index = 0;
    let mut quoting = QuoteState::default();
    // Separators inside a statement block or condition belong to that
    // statement; only top-level separators delimit commands.
    let mut closers = Vec::new();
    let mut success_chain = true;
    while index < bytes.len() {
        let byte = bytes[index];
        if !quoting.is_syntax(byte) {
            index += 1;
            continue;
        }
        match byte {
            b'(' => closers.push(b')'),
            b'{' => closers.push(b'}'),
            // A stray closer stays literal; a mismatched one leaves the
            // command boundaries unknowable.
            b')' | b'}' if !closers.is_empty() && closers.pop() != Some(byte) => {
                return None;
            }
            _ => {}
        }
        let separator_length = match byte {
            b';' | b'\r' | b'\n' => 1,
            b'&' if bytes.get(index + 1) == Some(&b'&') => 2,
            b'|' if bytes.get(index + 1) == Some(&b'|') => 2,
            b'|' if allow_pipelines => 1,
            b'&' | b'|' => return None,
            _ => 0,
        };
        if separator_length == 0 || !closers.is_empty() {
            index += separator_length.max(1);
            continue;
        }
        success_chain &= byte == b'&';
        commands.push(&script[start..index]);
        index += separator_length;
        if byte == b'\r' && bytes.get(index) == Some(&b'\n') {
            index += 1;
        }
        start = index;
    }
    if !quoting.is_closed() || !closers.is_empty() {
        return None;
    }
    commands.push(&script[start..]);
    Some((commands, success_chain))
}

/// Tracks shell quoting so separator and bracket scanning only sees syntax.
#[derive(Default)]
struct QuoteState {
    quote: Option<u8>,
    escaped: bool,
}

impl QuoteState {
    /// Consumes `byte` and returns whether it is unquoted, unescaped syntax.
    fn is_syntax(&mut self, byte: u8) -> bool {
        if self.escaped {
            self.escaped = false;
            return false;
        }
        if byte == b'\\' && self.quote != Some(b'\'') {
            self.escaped = true;
            return false;
        }
        if matches!(byte, b'\'' | b'"') {
            match self.quote {
                Some(active) if active == byte => self.quote = None,
                None => self.quote = Some(byte),
                Some(_) => {}
            }
            return false;
        }
        self.quote.is_none()
    }

    fn is_closed(&self) -> bool {
        self.quote.is_none() && !self.escaped
    }
}

/// Returns the conditions and block bodies of a complete PowerShell
/// `if (...) { ... }` statement, including `elseif` and `else` clauses.
fn powershell_if_statement_parts(command: &str) -> Option<Vec<&str>> {
    let mut parts = Vec::new();
    let mut clause = strip_statement_keyword(command, "if")?;
    loop {
        let (condition, after_condition) = bracketed_prefix(clause.trim_start(), b'(')?;
        let (body, after_body) = bracketed_prefix(after_condition.trim_start(), b'{')?;
        parts.extend([condition, body]);
        let after_body = after_body.trim_start();
        if after_body.is_empty() {
            return Some(parts);
        }
        if let Some(next) = strip_statement_keyword(after_body, "elseif") {
            clause = next;
            continue;
        }
        let else_clause = strip_statement_keyword(after_body, "else")?;
        let (body, rest) = bracketed_prefix(else_clause.trim_start(), b'{')?;
        parts.push(body);
        return rest.trim().is_empty().then_some(parts);
    }
}

/// Returns the body of a segment that is entirely one `( ... )` or
/// `{ ... }` group, such as a subshell.
fn grouped_command_body(command: &str) -> Option<&str> {
    let open = *command.as_bytes().first()?;
    if !matches!(open, b'(' | b'{') {
        return None;
    }
    let (body, rest) = bracketed_prefix(command, open)?;
    rest.trim().is_empty().then_some(body)
}

fn strip_statement_keyword<'a>(text: &'a str, keyword: &str) -> Option<&'a str> {
    let rest = text
        .get(..keyword.len())
        .filter(|head| head.eq_ignore_ascii_case(keyword))
        .map(|_| &text[keyword.len()..])?;
    // `ifconfig` or `else-branch` is a command name, not a keyword.
    rest.starts_with(|character: char| character.is_whitespace() || matches!(character, '(' | '{'))
        .then_some(rest)
}

/// Splits `text` after the bracket pair it opens with, honoring quotes and
/// nested brackets, into the enclosed text and the remainder.
fn bracketed_prefix(text: &str, open: u8) -> Option<(&str, &str)> {
    if text.as_bytes().first() != Some(&open) {
        return None;
    }
    let mut quoting = QuoteState::default();
    let mut closers = Vec::new();
    for (index, byte) in text.bytes().enumerate() {
        if !quoting.is_syntax(byte) {
            continue;
        }
        match byte {
            b'(' => closers.push(b')'),
            b'{' => closers.push(b'}'),
            b')' | b'}' => {
                if closers.pop() != Some(byte) {
                    return None;
                }
                if closers.is_empty() {
                    return Some((&text[1..index], &text[index + 1..]));
                }
            }
            _ => {}
        }
    }
    None
}

pub fn classify_argv(program: &str, args: &[String]) -> ValidationClassification {
    classify_argv_with_runners(program, args, &[])
}

pub fn classify_argv_with_runners(
    program: &str,
    args: &[String],
    runners: &[RepositoryRunner],
) -> ValidationClassification {
    classify_argv_at_depth(program, args, 0, runners)
}

fn classify_argv_at_depth(
    program: &str,
    args: &[String],
    depth: usize,
    runners: &[RepositoryRunner],
) -> ValidationClassification {
    if depth > MAX_WRAPPER_DEPTH {
        return ValidationClassification::Opaque;
    }
    let binary = normalized_program_name(program);
    if binary == "just" {
        // A nonvariadic recipe owns only its declared prefix. Later recipes
        // cannot change the earlier recipe's execution mode or receipt owner.
        for runner in runners.iter().filter(|runner| !runner.allow_extra_args) {
            let Some(normalized) = runner.normalized_args(program, args) else { continue; };
            let offset = args.len() - normalized.len();
            if let Some(prefix) = runner.prefixes.iter().find(|prefix| {
                normalized.len() > prefix.len() && runner.matches(program, &args[..offset + prefix.len()])
            }) {
                let end = offset + prefix.len();
                return combine_validation_classifications([
                    classify_argv_at_depth(program, &args[..end], depth + 1, runners),
                    classify_argv_at_depth(program, &args[end..], depth + 1, runners),
                ]);
            }
        }
    }
    if let Some(runner) = runners.iter().find(|runner| runner.matches(program, args)) {
        let Some(args) = runner.normalized_args(program, args) else { return ValidationClassification::Opaque; };
        if let Some(skip) = runner.passthrough_args {
            let Some((program, arguments)) = args.get(skip..).and_then(|args| args.split_first()) else {
                return ValidationClassification::Opaque;
            };
            return classify_argv_at_depth(program, arguments, depth + 1, runners);
        }
        if let Some(separator) = &runner.passthrough_after {
            let nested = args
                .iter()
                .position(|arg| arg == separator)
                .and_then(|index| args[index + 1..].split_first());
            let Some((program, arguments)) = nested else {
                return ValidationClassification::Opaque;
            };
            return classify_argv_at_depth(program, arguments, depth + 1, runners);
        }
        let mut classification = classification_from_operations(runner.operations.clone(), false, &binary, args);
        if let Some(mode) = runner.execution_mode
            && let ValidationClassification::Validation { leaves, .. } = &mut classification
        {
            for leaf in leaves {
                leaf.mode = mode;
            }
        }
        return classification;
    }

    if matches!(binary.as_str(), "npm" | "pnpm" | "yarn")
        && let Some(index) = node_command_index(&binary, args)
        && args[index] == "exec"
    {
        let mut nested = &args[index + 1..];
        if nested.first().is_some_and(|arg| arg == "--") {
            nested = &nested[1..];
        }
        let Some((program, arguments)) = nested.split_first() else {
            return ValidationClassification::Opaque;
        };
        // Do not mistake package/launcher option values for executables.
        if program.starts_with('-') {
            return ValidationClassification::Opaque;
        }
        return classify_argv_at_depth(program, arguments, depth + 1, runners);
    }

    if binary == "uv" && args.first().is_some_and(|arg| arg == "run") {
        let Some(program) = args.get(1).filter(|arg| !arg.starts_with('-')) else {
            return ValidationClassification::Opaque;
        };
        return classify_argv_at_depth(program, &args[2..], depth + 1, runners);
    }

    if matches!(binary.as_str(), "env" | "command" | "time" | "exec" | "rustup") {
        let index = match decode_execution_wrapper(&binary, args) {
            ExecutionWrapper::Child(index) => index,
            ExecutionWrapper::NonExecution => return ValidationClassification::NonValidation,
            ExecutionWrapper::Opaque => return ValidationClassification::Opaque,
        };
        return classify_argv_at_depth(&args[index], &args[index + 1..], depth + 1, runners);
    }
    if matches!(binary.as_str(), "bash" | "sh") {
        let Some(script) = executing_posix_shell_script(args) else {
            return ValidationClassification::Opaque;
        };
        return classify_simple_script(script, depth + 1, runners, false);
    }
    if matches!(binary.as_str(), "pwsh" | "powershell") {
        let command = std::iter::once(program.to_owned())
            .chain(args.iter().cloned())
            .collect::<Vec<_>>();
        let Some((_, script)) = crate::powershell::extract_powershell_command(&command) else {
            return ValidationClassification::Opaque;
        };
        return classify_simple_script(script, depth + 1, runners, true);
    }
    if binary == "cmd" {
        let Some(index) = args
            .iter()
            .position(|arg| arg.eq_ignore_ascii_case("/c") || arg.eq_ignore_ascii_case("/k"))
        else {
            return ValidationClassification::Opaque;
        };
        if args.get(index + 1).is_none() {
            return ValidationClassification::Opaque;
        }
        // CMD has different expansion and exit semantics from POSIX shells.
        return ValidationClassification::Opaque;
    }

    let (operations, has_unclassified_targets) = recognize_operations(&binary, args);
    classification_from_operations(operations, has_unclassified_targets, &binary, args)
}

fn classification_from_operations(
    operations: Vec<ValidationOperation>,
    has_unclassified_targets: bool,
    binary: &str,
    args: &[String],
) -> ValidationClassification {
    if operations.is_empty() {
        if has_unclassified_targets {
            ValidationClassification::Opaque
        } else {
            ValidationClassification::NonValidation
        }
    } else {
        ValidationClassification::Validation {
            leaves: operations
                .into_iter()
                .map(|operation| ValidationCommandDescriptor {
                    operation,
                    mode: execution_mode(binary, args, operation),
                })
                .collect(),
            has_unclassified_targets,
            exit_code_is_authoritative: !has_unclassified_targets
                && !(is_python_launcher(binary) && python_interactive(args))
                && !(matches!(binary, "mvn" | "mvnw") && runner_flags(binary, args).iter().any(|flag| matches!(*flag, "-fn" | "--fail-never"))),
        }
    }
}

const LIBTEST_VALUE_OPTIONS: &[&str] = &["--skip", "--logfile", "--format", "--color", "--test-threads", "--shuffle-seed"];
const CARGO_VALUE_OPTIONS: &[&str] = &["-p", "--package", "--exclude", "--test", "--bin", "--bench", "--example", "--target", "--target-dir", "--manifest-path", "--features", "-F", "--profile", "--config", "--color", "--message-format", "-j", "--jobs", "-E", "--filterset", "--skip", "--run-ignored", "--retries", "--test-threads", "--partition", "--status-level", "--final-status-level", "--success-output", "--failure-output", "--no-tests", "--binaries-metadata", "--cargo-metadata"];

fn runner_flags<'a>(binary: &str, args: &'a [String]) -> Vec<&'a str> {
    let mut flags = Vec::new();
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        if arg == "--" { break; }
        let (flag, inline) = arg.split_once('=').map_or((arg.as_str(), false), |(flag, _)| (flag, true));
        flags.push(flag);
        index += 1 + if inline { 0 } else { runner_option_value_count(binary, flag) };
    }
    flags
}

fn option_flags<'a>(args: &'a [String], value_options: &[&str]) -> Vec<&'a str> {
    let mut flags = Vec::new();
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        let (flag, inline) = arg.split_once('=').map_or((arg.as_str(), false), |(flag, _)| (flag, true));
        flags.push(flag);
        index += if !inline && value_options.contains(&flag) { 2 } else { 1 };
    }
    flags
}

/// Resolve only an interpreter's actual script operand, never `-c` program data.
pub fn python_script_index(args: &[String]) -> Option<usize> {
    match decode_python_invocation(args)?.operand? {
        PythonOperand::Script(index) => Some(index),
        _ => None,
    }
}

/// Decode the module and its own arguments using the same interpreter grammar
/// as script, inline-code, interaction and compiler-input classification.
pub fn python_module(args: &[String]) -> Option<(&str, &[String])> {
    match decode_python_invocation(args)?.operand? {
        PythonOperand::Module(name, tail) => Some((name, tail)),
        _ => None,
    }
}

enum PythonOperand<'a> {
    Module(&'a str, &'a [String]),
    Code(&'a str),
    Script(usize),
    Stdin,
}

struct PythonInvocation<'a> {
    operand: Option<PythonOperand<'a>>,
    interactive: bool,
}

fn decode_python_invocation(args: &[String]) -> Option<PythonInvocation<'_>> {
    let mut index = 0;
    let mut interactive = false;
    while let Some(arg) = args.get(index) {
        let operand = if arg == "--" {
            args.get(index + 1).map(|value| if value == "-" {
                PythonOperand::Stdin
            } else {
                PythonOperand::Script(index + 1)
            })
        } else if arg == "-" {
            Some(PythonOperand::Stdin)
        } else if !arg.starts_with('-') {
            Some(PythonOperand::Script(index))
        } else {
            None
        };
        if operand.is_some() || arg == "--" {
            return Some(PythonInvocation { operand, interactive });
        }
        // Windows py selectors precede the interpreter's options. Recognizing
        // these conservatively cannot grant an opaque child compilation proof.
        if arg.strip_prefix('-').is_some_and(|value|
            value.starts_with(|ch: char| ch.is_ascii_digit())
                && value.chars().all(|ch| ch.is_ascii_digit() || matches!(ch, '.' | '-')))
            || arg.strip_prefix("-V:").is_some_and(|value| !value.is_empty())
        {
            index += 1;
            continue;
        }
        if arg == "--check-hash-based-pycs" {
            args.get(index + 1)?;
            index += 2;
            continue;
        }
        if arg.starts_with("--check-hash-based-pycs=") {
            index += 1;
            continue;
        }
        let flags = arg.strip_prefix('-').filter(|value| !value.is_empty())?;
        let mut consumed_value = false;
        for (offset, flag) in flags.char_indices() {
            if matches!(flag, 'c' | 'm' | 'W' | 'X') {
                let inline = &flags[offset + flag.len_utf8()..];
                let (value, tail) = if inline.is_empty() {
                    (args.get(index + 1)?.as_str(), index + 2)
                } else {
                    (inline, index + 1)
                };
                if matches!(flag, 'c' | 'm') {
                    let operand = if flag == 'c' { PythonOperand::Code(value) }
                        else { PythonOperand::Module(value, &args[tail..]) };
                    return Some(PythonInvocation { operand: Some(operand), interactive });
                }
                index = tail;
                consumed_value = true;
                break;
            }
            if !"bBdEiIOPqsSuvx".contains(flag) { return None; }
            interactive |= flag == 'i';
        }
        if !consumed_value { index += 1; }
    }
    Some(PythonInvocation { operand: None, interactive })
}

/// Public KD4 lane recipes own exactly a recipe and lane before the child argv.
pub fn just_lane_child(args: &[String]) -> Option<&[String]> {
    matches!(args.first()?.as_str(), "cargo-lane" | "cargo-lane-isolated-home")
        .then(|| args.get(2..)).flatten().filter(|args| !args.is_empty())
}

fn execution_mode(
    binary: &str,
    args: &[String],
    operation: ValidationOperation,
) -> ValidationExecutionMode {
    use ValidationExecutionMode as Mode;
    if matches!(binary, "mvn" | "mvnw") {
        let flags = runner_flags(binary, args);
        if flags.iter().any(|flag| matches!(*flag, "--help" | "-h" | "--version" | "-v")) {
            return Mode::Listing;
        }
    }
    if binary == "dotnet" {
        let flags = option_flags(args, &["--settings", "-s", "--filter", "--logger", "-l", "--results-directory", "--test-adapter-path", "--framework", "-f", "--configuration", "--runtime", "-r", "--collect", "--diag"]);
        if flags.iter().any(|flag| matches!(*flag, "-t" | "--list-tests" | "--help" | "-h" | "--version")) {
            return Mode::Listing;
        }
    }
    if binary == "cargo"
        && let Ok(Some(index)) = cargo_subcommand_index(args)
        && matches!(args[index].as_str(), "test" | "t" | "bench" | "nextest")
    {
        let tail = &args[index + 1..];
        if args[index] == "nextest" && tail.first().is_some_and(|arg| arg == "list") {
            return Mode::Listing;
        }
        let separator = tail.iter().position(|arg| arg == "--").unwrap_or(tail.len());
        let flags = option_flags(&tail[..separator], CARGO_VALUE_OPTIONS);
        let harness = tail.get(separator + 1..).unwrap_or_default();
        let harness_flags = option_flags(harness, LIBTEST_VALUE_OPTIONS);
        if flags.iter().any(|flag| matches!(*flag, "--help" | "-h" | "--version"))
            || harness_flags.iter().any(|flag| matches!(*flag, "--list" | "--help" | "-h"))
        {
            return Mode::Listing;
        }
        if flags.contains(&"--no-run") {
            return Mode::CompilationOnly;
        }
        return Mode::Execution;
    }
    if binary == "pytest" || is_python_launcher(binary)
        && args.windows(2).any(|pair| pair[0] == "-m" && pair[1] == "pytest")
    {
        let flags = option_flags(args, &["-k", "-m", "--override-ini", "-o", "--confcutdir", "--rootdir", "--basetemp", "--ignore", "--ignore-glob", "--deselect", "--import-mode", "--tb", "--capture"]);
        if flags.contains(&"--setup-only") {
            // Fixture setup is side-effectful runtime work, not compilation.
            return Mode::Unknown;
        }
        if flags.iter().any(|flag| matches!(*flag,
            "--collect-only" | "--collectonly" | "--co" | "--setup-plan"
            | "--fixtures" | "--funcargs" | "--fixtures-per-test" | "--help" | "-h" | "--version"))
        {
            return Mode::Listing;
        }
        return Mode::Execution;
    }
    if binary == "npm" {
        let options = args.iter().take_while(|arg| arg.as_str() != "--").map(String::as_str).collect::<Vec<_>>();
        if options.iter().any(|arg| matches!(*arg, "--help" | "-h" | "--version" | "-v")) {
            return Mode::Listing;
        }
        if options.iter().any(|arg| *arg == "--if-present" || *arg == "--if-present=true") {
            return Mode::Unknown;
        }
        // npm's --dry-run does not prevent a test script from executing.
        return if matches!(operation, ValidationOperation::Check | ValidationOperation::Lint) {
            Mode::Checking
        } else {
            Mode::Execution
        };
    }
    let has = |flags: &[&str]| args.iter().any(|arg| flags.contains(&arg.as_str()));
    if is_python_launcher(binary) && python_interactive(args) {
        return Mode::Unknown;
    }
    if binary == "just"
        && has(&[
            "--show",
            "-s",
            "--dump",
            "--evaluate",
            "--variables",
            "--groups",
            "--usage",
        ])
    {
        return Mode::Listing;
    }
    if binary == "make" && has(&["--question", "-q"]) || binary == "task" && has(&["--status"]) {
        return Mode::DryRun;
    }
    // Forwarding wrappers retain the lint operation, so mutation flags must
    // not depend on the outer executable being Cargo.
    if operation == ValidationOperation::Lint && has(&["--fix", "--write"]) {
        return Mode::Mutation;
    }
    if binary == "npm"
        && args
            .iter()
            .take_while(|arg| arg.as_str() != "--")
            .any(|arg| matches!(arg.as_str(), "--help" | "-h" | "--version" | "-v"))
    {
        return Mode::Listing;
    }
    if binary == "go" {
        let flags = option_flags(args, &["-run", "-test.run", "-bench", "-test.bench", "-count", "-test.count", "-timeout", "-test.timeout", "-cpu", "-test.cpu", "-o", "-coverprofile", "-benchtime", "-test.benchtime"]);
        if flags.iter().any(|flag| matches!(*flag, "-list" | "-test.list" | "-h" | "--help" | "-help" | "-test.help"))
        {
            return Mode::Listing;
        }
        if has(&["-n"]) {
            return Mode::DryRun;
        }
    }
    if matches!(binary, "gradle" | "gradlew") {
        if has(&["--version", "-v", "--help", "-h"]) {
            return Mode::Listing;
        }
        if args.iter().any(|arg| {
            matches!(arg.as_str(), "-x" | "--exclude-task")
                || arg.starts_with("--exclude-task=")
                || (arg.starts_with("-x") && arg.len() > 2)
        }) {
            return Mode::Unknown;
        }
    }
    if has(&["--dry-run", "--dry", "--dryrun"])
        || matches!(binary, "just" | "make" | "task") && has(&["-n"])
        || matches!(binary, "gradle" | "gradlew") && has(&["-m"])
        || matches!(binary, "mvn" | "mvnw")
            && has(&["-DskipTests", "-DskipTests=true", "-Dmaven.test.skip=true"])
    {
        return Mode::DryRun;
    }
    if has(&[
        "--list",
        "--list-tests",
        "--listTests",
        "--collect-only",
        "--co",
        "--summary",
    ]) {
        return Mode::Listing;
    }
    if has(&["--no-run"]) || binary == "go" && has(&["-c"]) {
        return Mode::CompilationOnly;
    }
    if binary == "cargo"
        && let Ok(Some(index)) = cargo_subcommand_index(args)
    {
        // Cargo forwards arguments after `--` to libtest. Its help flags
        // display usage and exit without running any tests or benchmarks.
        if matches!(args[index].as_str(), "test" | "t" | "bench")
            && let Some(separator) = args[index + 1..].iter().position(|arg| arg == "--")
            && args[index + separator + 2..]
                .iter()
                .any(|arg| matches!(arg.as_str(), "-h" | "--help"))
        {
            return Mode::Listing;
        }
        match args[index].as_str() {
            "fmt" => {
                return if has(&["--check"]) {
                    Mode::Checking
                } else {
                    Mode::Mutation
                };
            }
            "clippy" if has(&["--fix"]) => return Mode::Mutation,
            "nextest" if args.get(index + 1).is_some_and(|arg| arg == "list") => {
                return Mode::Listing;
            }
            "fuzz" => {
                return match args.get(index + 1).map(String::as_str) {
                    Some("run") => Mode::Execution,
                    Some("list") => Mode::Listing,
                    Some("build") => Mode::CompilationOnly,
                    Some("init" | "add" | "cmin" | "tmin" | "fmt") => Mode::Mutation,
                    _ => Mode::Unknown,
                };
            }
            _ => {}
        }
    }
    if matches!(
        operation,
        ValidationOperation::Check | ValidationOperation::Lint
    ) {
        Mode::Checking
    } else {
        Mode::Execution
    }
}


/// Resolve only supported shell options before the command string. `-c` does
/// not stop option parsing: `sh -c -n SCRIPT` is still syntax-only, while a
/// later `+n` enables execution again. Words after SCRIPT are positional args.
fn executing_posix_shell_script(args: &[String]) -> Option<&str> {
    let mut command_string = false;
    let mut noexec = false;
    let mut args = args.iter();
    while let Some(argument) = args.next() {
        if argument == "--" {
            return (command_string && !noexec)
                .then(|| args.next().map(String::as_str))
                .flatten();
        }
        if matches!(
            argument.as_str(),
            "--norc" | "--noprofile" | "--posix" | "--login"
        ) {
            continue;
        }
        let (enabled, flags) = if let Some(flags) = argument.strip_prefix('-') {
            (true, flags)
        } else if let Some(flags) = argument.strip_prefix('+') {
            (false, flags)
        } else {
            return (command_string && !noexec).then_some(argument.as_str());
        };
        if flags.is_empty() || flags.starts_with('-') {
            return None;
        }
        let mut flags = flags.chars().peekable();
        while let Some(flag) = flags.next() {
            match flag {
                'c' if enabled => command_string = true,
                'n' => noexec = enabled,
                'o' if flags.peek().is_none() => match args.next()?.as_str() {
                    "noexec" => noexec = enabled,
                    "errexit" | "nounset" | "pipefail" | "xtrace" | "noglob" | "noclobber"
                    | "verbose" => {}
                    _ => return None,
                },
                'a' | 'b' | 'e' | 'f' | 'h' | 'k' | 'l' | 'm' | 'p' | 'r' | 'u' | 'v' | 'x'
                | 'B' | 'C' | 'E' | 'H' | 'P' | 'T' => {}
                // Interactive, dump/inspection, stdin and unknown modes are
                // not a proof that this command string ran to completion.
                _ => return None,
            }
        }
    }
    None
}

/// Static execution-envelope decoding. Unsupported target-changing modes stay opaque.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionWrapper {
    Child(usize),
    NonExecution,
    Opaque,
}

pub fn decode_execution_wrapper(binary: &str, args: &[String]) -> ExecutionWrapper {
    if binary == "rustup" {
        let mut index = 0;
        while args
            .get(index)
            .is_some_and(|arg| matches!(arg.as_str(), "-q" | "--quiet" | "-v" | "--verbose"))
        {
            index += 1;
        }
        if args.get(index).is_none_or(|arg| arg != "run") {
            return ExecutionWrapper::Opaque;
        }
        index += 1;
        if args.get(index).is_some_and(|arg| arg == "--install") {
            index += 1;
        }
        if args
            .get(index)
            .is_none_or(|arg| arg.is_empty() || arg.starts_with('-'))
        {
            return ExecutionWrapper::Opaque;
        }
        index += 1; // toolchain, not a command or a test selector
        if args.get(index).is_some_and(|arg| arg == "--") {
            index += 1;
        }
        return if args.get(index).is_some_and(|arg| !arg.starts_with('-')) {
            ExecutionWrapper::Child(index)
        } else {
            ExecutionWrapper::Opaque
        };
    }
    let mut index = 0;
    let mut options = true;
    while let Some(arg) = args.get(index) {
        if options && arg == "--" {
            options = false;
            index += 1;
            continue;
        }
        if binary == "env" && is_shell_assignment(arg) {
            options = false;
            index += 1;
            continue;
        }
        if !options || !arg.starts_with('-') {
            return ExecutionWrapper::Child(index);
        }
        match (binary, arg.as_str()) {
            ("command", "-v" | "-V") | ("env" | "time", "--help" | "--version") => {
                return ExecutionWrapper::NonExecution;
            }
            ("command", "-p")
            | ("exec", "-c" | "-l")
            | ("env", "-i" | "--ignore-environment" | "-")
            | (
                "time",
                "-p" | "--portability" | "-v" | "--verbose" | "-a" | "--append" | "-q" | "--quiet",
            ) => {
                index += 1;
            }
            ("env", "-u" | "--unset" | "-C" | "--chdir")
            | ("exec", "-a")
            | ("time", "-o" | "--output" | "-f" | "--format") => {
                if args.get(index + 1).is_none() {
                    return ExecutionWrapper::Opaque;
                }
                index += 2;
            }
            ("env", _)
                if arg.starts_with("--unset=")
                    || arg.starts_with("--chdir=")
                    || (arg.starts_with("-u") && arg.len() > 2)
                    || (arg.starts_with("-C") && arg.len() > 2) =>
            {
                index += 1;
            }
            ("time", _) if arg.starts_with("--output=") || arg.starts_with("--format=") => {
                index += 1;
            }
            _ => return ExecutionWrapper::Opaque,
        }
    }
    ExecutionWrapper::Opaque
}

fn python_interactive(args: &[String]) -> bool {
    decode_python_invocation(args).is_some_and(|invocation| invocation.interactive)
}

fn is_shell_assignment(argument: &str) -> bool {
    let Some((name, _)) = argument.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && name.chars().enumerate().all(|(index, character)| {
            character == '_'
                || character.is_ascii_alphanumeric() && (index > 0 || !character.is_ascii_digit())
        })
}

fn recognize_operations(binary: &str, args: &[String]) -> (Vec<ValidationOperation>, bool) {
    match binary {
        "cargo" => cargo_operations(args),
        "pytest" | "vitest" | "jest" => (
            if args
                .iter()
                .any(|arg| matches!(arg.as_str(), "--help" | "-h" | "--version"))
            {
                Vec::new()
            } else {
                vec![ValidationOperation::Test]
            },
            false,
        ),
        binary if is_python_launcher(binary) => {
            (python_operation(args).into_iter().collect(), false)
        }
        "dotnet" | "go" => (
            args.first()
                .is_some_and(|argument| argument.eq_ignore_ascii_case("test"))
                .then_some(ValidationOperation::Test)
                .into_iter()
                .collect(),
            false,
        ),
        "mvn" | "mvnw" | "gradle" | "gradlew" => (
            jvm_test_operation(binary, args).into_iter().collect(),
            false,
        ),
        "npm" | "pnpm" | "yarn" => {
            let operations = node_operations(binary, args);
            // Script names describe intent, not their repository-owned bodies.
            let opaque = !operations.is_empty();
            (operations, opaque)
        }
        "just" | "make" | "task" => {
            let (operations, unclassified) = wrapper_operations(binary, args);
            let opaque = unclassified || !operations.is_empty();
            (operations, opaque)
        }
        _ => (Vec::new(), false),
    }
}

pub fn is_python_launcher(binary: &str) -> bool {
    if matches!(binary, "py" | "python") {
        return true;
    }

    binary.strip_prefix("python").is_some_and(|version| {
        !version.is_empty()
            && version
                .chars()
                .all(|character| character.is_ascii_digit() || character == '.')
            && version.chars().any(|character| character.is_ascii_digit())
    })
}

fn cargo_operations(args: &[String]) -> (Vec<ValidationOperation>, bool) {
    let subcommand_index = match cargo_subcommand_index(args) {
        Ok(Some(index)) => index,
        Ok(None) => return (Vec::new(), false),
        Err(()) => return (Vec::new(), true),
    };
    let subcommand = args[subcommand_index].to_ascii_lowercase();
    let operation = if subcommand == "nextest" {
        args.get(subcommand_index + 1)
            .filter(|argument| {
                argument.eq_ignore_ascii_case("run") || argument.eq_ignore_ascii_case("list")
            })
            .map(|_| ValidationOperation::Test)
    } else {
        cargo_operation(&subcommand)
    };
    let mut operations = operation.into_iter().collect::<Vec<_>>();
    if subcommand == "bench"
        && let Some(separator) = args.iter().position(|arg| arg == "--")
        && option_flags(&args[separator + 1..], LIBTEST_VALUE_OPTIONS).contains(&"--test")
    {
        operations.push(ValidationOperation::Test);
    }
    (operations, false)
}

fn cargo_subcommand_index(args: &[String]) -> Result<Option<usize>, ()> {
    // Cargo accepts help after the subcommand too. Arguments after `--`
    // belong to the test binary or another delegated tool, not Cargo itself.
    if args
        .iter()
        .take_while(|argument| argument.as_str() != "--")
        .any(|argument| matches!(argument.as_str(), "--help" | "-h"))
    {
        return Ok(None);
    }
    let mut index = 0;
    while let Some(argument) = args.get(index) {
        if matches!(argument.as_str(), "--version" | "-V" | "--list") {
            return Ok(None);
        }
        if matches!(
            argument.as_str(),
            "--locked" | "--offline" | "--frozen" | "--quiet" | "-q" | "--verbose" | "-v"
        ) || argument.starts_with('+') && argument.len() > 1
            || argument.starts_with('-')
                && argument.len() > 2
                && argument[1..].chars().all(|flag| flag == 'v')
        {
            index += 1;
            continue;
        }
        if matches!(argument.as_str(), "--color" | "--config" | "-Z" | "-C") {
            if args.get(index + 1).is_none() {
                return Err(());
            }
            index += 2;
            continue;
        }
        if argument.starts_with("--color=")
            || argument.starts_with("--config=")
            || argument.starts_with("-Z") && argument.len() > 2
            || argument.starts_with("-C") && argument.len() > 2
        {
            index += 1;
            continue;
        }
        if argument.starts_with('-') {
            return Err(());
        }
        return Ok(Some(index));
    }
    Ok(None)
}

fn cargo_operation(argument: &str) -> Option<ValidationOperation> {
    match argument.to_ascii_lowercase().as_str() {
        "test" | "t" => Some(ValidationOperation::Test),
        "check" | "c" => Some(ValidationOperation::Check),
        "clippy" | "fmt" => Some(ValidationOperation::Lint),
        "bench" => Some(ValidationOperation::Bench),
        "fuzz" => Some(ValidationOperation::Fuzz),
        _ => None,
    }
}

fn python_operation(args: &[String]) -> Option<ValidationOperation> {
    let (module, tail) = python_module(args)?;
    if tail.iter().take_while(|arg| arg.as_str() != "--")
        .any(|arg| matches!(arg.as_str(), "--help" | "-h" | "--version")) {
        return None;
    }
    matches!(module, "pytest" | "unittest").then_some(ValidationOperation::Test)
}

fn jvm_test_operation(binary: &str, args: &[String]) -> Option<ValidationOperation> {
    let mut index = 0;
    while let Some(argument) = args.get(index) {
        let value_count = runner_option_value_count(binary, argument);
        if value_count > 0 {
            index += value_count + 1;
            continue;
        }
        if !argument.starts_with('-')
            && argument
                .rsplit(':')
                .next()
                .is_some_and(|component| component.eq_ignore_ascii_case("test"))
        {
            return Some(ValidationOperation::Test);
        }
        index += 1;
    }
    None
}

fn node_operations(binary: &str, args: &[String]) -> Vec<ValidationOperation> {
    let Some(command_index) = node_command_index(binary, args) else {
        return Vec::new();
    };
    let command = args[command_index].to_ascii_lowercase();
    if command == "test" {
        return vec![ValidationOperation::Test];
    }
    if matches!(command.as_str(), "run" | "run-script") {
        return args[command_index + 1..]
            .iter()
            .find(|selector| selector.as_str() != "--" && !selector.starts_with('-'))
            .map_or_else(Vec::new, |selector| selector_operations(selector));
    }
    if binary == "yarn" && command == "workspace" {
        return args
            .get(command_index + 2..)
            .unwrap_or_default()
            .iter()
            .position(|argument| argument.eq_ignore_ascii_case("run"))
            .and_then(|run_offset| args.get(command_index + 3 + run_offset))
            .map_or_else(Vec::new, |selector| selector_operations(selector));
    }
    if matches!(binary, "pnpm" | "yarn") {
        return selector_operations(&command);
    }
    Vec::new()
}

fn node_command_index(binary: &str, args: &[String]) -> Option<usize> {
    let mut index = 0;
    while let Some(argument) = args.get(index) {
        if argument == "--" {
            return args.get(index + 1).map(|_| index + 1);
        }
        let takes_value = matches!(
            (binary, argument.as_str()),
            ("npm", "--prefix") | ("pnpm", "--filter") | ("yarn", "--cwd")
        );
        if takes_value {
            index += 2;
        } else if argument.starts_with('-') {
            index += 1;
        } else {
            return Some(index);
        }
    }
    None
}

fn wrapper_operations(binary: &str, args: &[String]) -> (Vec<ValidationOperation>, bool) {
    let mut operations = Vec::new();
    let mut has_unclassified_targets = false;
    let mut index = 0;
    while let Some(selector) = args.get(index) {
        let value_count = runner_option_value_count(binary, selector);
        if value_count > 0 {
            index += value_count + 1;
            continue;
        }
        if selector == "--" || selector.starts_with('-') || selector.contains('=') {
            index += 1;
            continue;
        }
        let found = exact_selector_operations(selector);
        if found.is_empty() {
            has_unclassified_targets = true;
        } else {
            extend_unique(&mut operations, found);
        }
        index += 1;
    }
    (operations, has_unclassified_targets)
}

fn exact_selector_operations(selector: &str) -> Vec<ValidationOperation> {
    match selector.to_ascii_lowercase().as_str() {
        "test" | "tests" | "testing" => vec![ValidationOperation::Test],
        "check" | "checks" => vec![ValidationOperation::Check],
        "lint" | "lints" | "clippy" => vec![ValidationOperation::Lint],
        "bench" | "benchmark" | "benchmarks" => vec![ValidationOperation::Bench],
        "fuzz" | "fuzzing" => vec![ValidationOperation::Fuzz],
        _ => Vec::new(),
    }
}

fn runner_option_value_count(binary: &str, option: &str) -> usize {
    match binary {
        "just" => just_option_value_count(option),
        "make" => match option {
            "-C" | "-f" | "-I" | "-o" | "-W" | "--assume-new" | "--assume-old" | "--directory"
            | "--eval" | "--file" | "--include-dir" | "--makefile" | "--new-file"
            | "--old-file" | "--what-if" => 1,
            _ => 0,
        },
        "task" => match option {
            "-C"
            | "-d"
            | "-I"
            | "-o"
            | "-t"
            | "--completion"
            | "--concurrency"
            | "--dir"
            | "--interval"
            | "--output"
            | "--output-group-begin"
            | "--output-group-end"
            | "--sort"
            | "--taskfile" => 1,
            _ => 0,
        },
        "mvn" | "mvnw" => match option {
            "-b"
            | "-D"
            | "-emp"
            | "-ep"
            | "-f"
            | "-gs"
            | "-l"
            | "-P"
            | "-pl"
            | "-rf"
            | "-s"
            | "-t"
            | "-T"
            | "--activate-profiles"
            | "--builder"
            | "--define"
            | "--encrypt-master-password"
            | "--encrypt-password"
            | "--file"
            | "--global-settings"
            | "--log-file"
            | "--projects"
            | "--resume-from"
            | "--settings"
            | "--threads"
            | "--toolchains" => 1,
            _ => 0,
        },
        "gradle" | "gradlew" => match option {
            "-x"
            | "--exclude-task"
            | "-b"
            | "-c"
            | "-g"
            | "-I"
            | "-p"
            | "--build-file"
            | "--configuration-cache-problems"
            | "--console"
            | "--dependency-verification"
            | "--gradle-user-home"
            | "--init-script"
            | "--priority"
            | "--project-cache-dir"
            | "--project-dir"
            | "--settings-file"
            | "--warning-mode"
            | "--write-verification-metadata" => 1,
            _ => 0,
        },
        _ => 0,
    }
}

fn just_option_value_count(option: &str) -> usize {
    match option {
        "--set" => 2,
        "-C"
        | "-E"
        | "-F"
        | "-d"
        | "-f"
        | "--alias-style"
        | "--ceiling"
        | "--chooser"
        | "--color"
        | "--command-color"
        | "--cygpath"
        | "--directory"
        | "--dotenv-command"
        | "--dotenv-filename"
        | "--dotenv-path"
        | "--dump-format"
        | "--evaluate-format"
        | "--file"
        | "--group"
        | "--indentation"
        | "--jobs"
        | "--justfile"
        | "--justfile-name"
        | "--list-heading"
        | "--list-prefix"
        | "--shell"
        | "--shell-arg"
        | "--tempdir"
        | "--timestamp-format"
        | "--working-directory" => 1,
        _ => 0,
    }
}

fn selector_operations(selector: &str) -> Vec<ValidationOperation> {
    let mut operations = Vec::new();
    for component in selector
        .to_ascii_lowercase()
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|component| !component.is_empty())
    {
        let operation = match component {
            "test" | "tests" | "testing" => Some(ValidationOperation::Test),
            "check" | "checks" => Some(ValidationOperation::Check),
            "lint" | "lints" | "clippy" | "fmt" | "format" => Some(ValidationOperation::Lint),
            "bench" | "benchmark" | "benchmarks" => Some(ValidationOperation::Bench),
            "fuzz" | "fuzzing" => Some(ValidationOperation::Fuzz),
            _ => None,
        };
        if let Some(operation) = operation
            && !operations.contains(&operation)
        {
            operations.push(operation);
        }
    }
    operations
}

fn extend_unique(operations: &mut Vec<ValidationOperation>, additional: Vec<ValidationOperation>) {
    for operation in additional {
        if !operations.contains(&operation) {
            operations.push(operation);
        }
    }
}

fn normalized_program_name(program: &str) -> String {
    let mut binary = program
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(program)
        .to_ascii_lowercase();
    for suffix in [".exe", ".cmd", ".bat"] {
        if binary.ends_with(suffix) {
            binary.truncate(binary.len() - suffix.len());
            break;
        }
    }
    binary
}

pub fn classify_script(script: &str) -> ValidationClassification {
    classify_script_with_runners(script, &[])
}

pub fn classify_script_with_runners(
    script: &str,
    runners: &[RepositoryRunner],
) -> ValidationClassification {
    classify_simple_script(script, 0, runners, false)
}

/// A check-only `cargo fmt`, bare or as the single command of a shell wrapper.
/// It reads sources and never takes Cargo's build lock.
pub fn is_standalone_cargo_fmt_check(program: &str, args: &[String]) -> bool {
    let binary = normalized_program_name(program);
    let script = match binary.as_str() {
        "cargo" => {
            return matches!(cargo_subcommand_index(args), Ok(Some(index)) if args[index].eq_ignore_ascii_case("fmt"))
                && execution_mode(&binary, args, ValidationOperation::Lint)
                    == ValidationExecutionMode::Checking;
        }
        "bash" | "sh" => executing_posix_shell_script(args).map(str::to_string),
        "pwsh" | "powershell" => {
            let command = std::iter::once(program.to_owned())
                .chain(args.iter().cloned())
                .collect::<Vec<_>>();
            crate::powershell::extract_powershell_command(&command)
                .map(|(_, script)| script.to_owned())
        }
        _ => None,
    };
    script
        .as_deref()
        .and_then(standalone_argv)
        .is_some_and(|argv| {
            argv.split_first().is_some_and(|(program, args)| {
                normalized_program_name(program) == "cargo"
                    && is_standalone_cargo_fmt_check(program, args)
            })
        })
}

/// Build and inventory commands may compile, but do not establish a test pass.
pub fn is_build_or_discovery(program: &str, args: &[String]) -> bool {
    let program = normalized_program_name(program);
    if program != "cargo" {
        let subcommand = args.first().map(String::as_str);
        return match program.as_str() {
            "npm" | "pnpm" | "yarn" | "bun" => {
                matches!(subcommand, Some("install" | "ci" | "add" | "build"))
                    || (subcommand == Some("run") && args.get(1).is_some_and(|arg| arg == "build"))
            }
            "go" => matches!(subcommand, Some("build" | "install" | "generate")),
            "dotnet" => matches!(subcommand, Some("build" | "restore" | "publish")),
            "git" => matches!(subcommand, Some("clone" | "fetch")),
            "just" => {
                // Recipe arguments and option values are not recipe names.
                // Publishing is long-running work, never validation evidence.
                let mut index = 0;
                while let Some(argument) = args.get(index) {
                    let value_count = just_option_value_count(argument);
                    if value_count > 0 {
                        index += value_count + 1;
                    } else if argument.starts_with('-') || argument.contains('=') {
                        index += 1;
                    } else {
                        return argument == "publish-local-codex-final";
                    }
                }
                false
            }
            "make" | "ninja" | "msbuild" => true,
            "cmake" => matches!(subcommand, Some("--build" | "--install")),
            _ => false,
        };
    }
    let Ok(Some(index)) = cargo_subcommand_index(args) else {
        return false;
    };
    matches!(args[index].as_str(), "build" | "b")
        || (args[index] == "nextest" && args.get(index + 1).is_some_and(|arg| arg == "list"))
}

/// Reuse the shell classifier's quote-aware command splitting for observation
/// policy. This does not authorize execution or establish validation coverage.
pub fn script_prefers_long_observation_wait(script: &str) -> bool {
    // Pipelines can hide a native command's exit status, but not its need for
    // a long observation window. Keep this separate from evidence classification.
    let Some((commands, _)) = split_script(script, true) else {
        return false;
    };
    commands.into_iter().any(|command| {
        let Some(words) = shlex::split(command) else {
            return false;
        };
        let Some(index) = words.iter().position(|word| !is_shell_assignment(word)) else {
            return false;
        };
        is_build_or_discovery(&words[index], &words[index + 1..])
            || matches!(
                classify_argv(&words[index], &words[index + 1..]),
                ValidationClassification::Validation { .. }
            )
    })
}

/// Policy and duplicate-selection analysis deliberately share one traversal.
pub fn analyze_validation_policy_argv_with_runners(
    program: &str,
    args: &[String],
    runners: &[RepositoryRunner],
) -> (ValidationPolicyClassification, String) {
    let mut leaves = Vec::new();
    let runners = if validation_resource_cwd_is_unambiguous(program, args) { runners } else { &[] };
    let policy = policy_argv(program, args, runners, 0, &mut leaves);
    (policy, policy_selection(leaves, program, args))
}

pub fn classify_validation_policy_argv_with_runners(
    program: &str,
    args: &[String],
    runners: &[RepositoryRunner],
) -> ValidationPolicyClassification {
    analyze_validation_policy_argv_with_runners(program, args, runners).0
}

/// Whether the caller's cwd also names a selected validation child's cwd.
/// Unlike policy leaves, this must not discard sibling shell commands: `cd`
/// and Set-Location are not validation, but change a later test's resources.
pub fn validation_resource_cwd_is_unambiguous(program: &str, args: &[String]) -> bool {
    fn script(body: &str, powershell: bool, depth: usize) -> bool {
        if depth > MAX_WRAPPER_DEPTH { return false; }
        let commands = if powershell {
            let wrapper = ["pwsh".to_owned(), "-Command".to_owned(), body.to_owned()];
            crate::powershell::parse_powershell_command_into_plain_commands(&wrapper)
        } else {
            crate::bash::try_parse_shell(body).and_then(|tree|
                crate::bash::try_parse_word_only_commands_with_operators(&tree, body)
                    .map(|(commands, _)| commands))
        };
        let Some(commands) = commands else {
            return !powershell && grouped_command_body(body.trim())
                .is_some_and(|body| script(body, false, depth + 1));
        };
        let [command] = commands.as_slice() else { return false; };
        command.split_first().is_some_and(|(program, args)| argv(program, args, depth + 1))
    }

    fn argv(program: &str, args: &[String], depth: usize) -> bool {
        if depth > MAX_WRAPPER_DEPTH { return false; }
        let binary = normalized_program_name(program);
        if matches!(binary.as_str(), "env" | "command" | "time" | "exec" | "rustup") {
            return match decode_execution_wrapper(&binary, args) {
                ExecutionWrapper::Child(index) => {
                    let changes_cwd = binary == "env" && args[..index].iter().any(|arg|
                        matches!(arg.as_str(), "-C" | "--chdir")
                            || arg.starts_with("--chdir=")
                            || (arg.starts_with("-C") && arg.len() > 2));
                    !changes_cwd && argv(&args[index], &args[index + 1..], depth + 1)
                }
                ExecutionWrapper::NonExecution => true,
                ExecutionWrapper::Opaque => false,
            };
        }
        if matches!(binary.as_str(), "bash" | "sh") {
            return executing_posix_shell_script(args)
                .is_some_and(|body| script(body, false, depth + 1));
        }
        if matches!(binary.as_str(), "pwsh" | "powershell") {
            let wrapper = std::iter::once(program.to_owned()).chain(args.iter().cloned()).collect::<Vec<_>>();
            return crate::powershell::extract_powershell_command(&wrapper)
                .is_some_and(|(_, body)| script(body, true, depth + 1));
        }
        if binary == "cmd" {
            let Some(index) = args.iter().position(|arg| arg.eq_ignore_ascii_case("/c")
                || arg.eq_ignore_ascii_case("/k")) else { return false; };
            let body = args[index + 1..].join(" ");
            // Match policy_cmd_script's CMD outer-quote handling before
            // deciding whether an operator belongs to a sibling command.
            let body = body.trim();
            let body = if body.starts_with('"') && body.ends_with('"')
                && body.matches('"').count() == 2 {
                &body[1..body.len() - 1]
            } else { body };
            let mut quoted = false;
            for character in body.chars() {
                if character == '"' { quoted = !quoted; }
                if !quoted && matches!(character, '&' | '|') { return false; }
            }
            let mut leaves = Vec::new();
            let policy = policy_cmd_script(body, &[], depth + 1, &mut leaves);
            return policy.is_validation() && leaves.len() == 1
                && leaves[0].split_first().is_some_and(|(program, args)| argv(program, args, depth + 1));
        }
        if binary == "uv" && args.first().is_some_and(|arg| arg == "run") {
            return args.get(1).filter(|arg| !arg.starts_with('-'))
                .is_some_and(|program| argv(program, &args[2..], depth + 1));
        }
        // Inline code can call chdir before launching its recognized child.
        !(is_python_launcher(&binary) && python_inline_code(args).is_some())
    }

    argv(program, args, 0)
}

pub fn analyze_validation_policy_script_with_runners(
    script: &str,
    runners: &[RepositoryRunner],
    powershell: bool,
) -> (ValidationPolicyClassification, String) {
    let mut leaves = Vec::new();
    // A selected cwd authenticates no owner after an inline cwd change or
    // opaque sibling. Reuse the resource owner's exact cwd boundary.
    let wrapper = ["-Command".to_owned(), script.to_owned()];
    let posix_wrapper = ["-c".to_owned(), script.to_owned()];
    let runners = if validation_resource_cwd_is_unambiguous(
        if powershell { "pwsh" } else { "sh" },
        if powershell { &wrapper } else { &posix_wrapper },
    ) { runners } else { &[] };
    let policy = policy_script(script, runners, powershell, 0, &mut leaves);
    (policy, policy_selection(leaves, if powershell { "powershell-script" } else { "script" }, &[script.to_owned()]))
}

pub fn classify_validation_policy_script_with_runners(
    script: &str,
    runners: &[RepositoryRunner],
) -> ValidationPolicyClassification {
    analyze_validation_policy_script_with_runners(script, runners, false).0
}

pub fn classify_validation_policy_powershell_script_with_runners(
    script: &str,
    runners: &[RepositoryRunner],
) -> ValidationPolicyClassification {
    analyze_validation_policy_script_with_runners(script, runners, true).0
}

fn policy_selection(mut leaves: Vec<Vec<String>>, program: &str, args: &[String]) -> String {
    if leaves.is_empty() {
        leaves.push(std::iter::once(program.to_owned()).chain(args.iter().cloned()).collect());
    }
    serde_json::to_string(&leaves).unwrap_or_default()
}

fn policy_script(
    script: &str,
    runners: &[RepositoryRunner],
    powershell: bool,
    depth: usize,
    leaves: &mut Vec<Vec<String>>,
) -> ValidationPolicyClassification {
    use ValidationPolicyClassification as Policy;
    if depth > MAX_WRAPPER_DEPTH {
        return Policy::Opaque;
    }
    let commands = if powershell {
        if let Some(argv) = crate::powershell::standalone_powershell_inspection_argv(script) {
            vec![argv]
        } else {
            let command = ["pwsh".to_owned(), "-Command".to_owned(), script.to_owned()];
            let Some(commands) = crate::powershell::parse_powershell_command_into_plain_commands(&command) else {
                return Policy::Opaque;
            };
            commands
        }
    } else {
        let Some(tree) = crate::bash::try_parse_shell(script) else {
            return Policy::Opaque;
        };
        let Some((commands, _)) = crate::bash::try_parse_word_only_commands_with_operators(&tree, script) else {
            let root = tree.root_node();
            if root.named_child_count() == 1
                && root.named_child(0).is_some_and(|node| matches!(node.kind(), "compound_statement" | "subshell"))
                && let Some(body) = grouped_command_body(script.trim())
            {
                return policy_script(body, runners, false, depth + 1, leaves);
            }
            return Policy::Opaque;
        };
        commands
    };
    combine_policy_classifications(commands.iter().filter_map(|argv| {
        let (program, args) = argv.split_first()?;
        Some(policy_argv(program, args, runners, depth + 1, leaves))
    }))
}

fn policy_argv(
    program: &str,
    args: &[String],
    runners: &[RepositoryRunner],
    depth: usize,
    leaves: &mut Vec<Vec<String>>,
) -> ValidationPolicyClassification {
    let start = leaves.len();
    let policy = policy_argv_inner(program, args, runners, depth, leaves);
    if !policy.is_validation() {
        leaves.truncate(start);
    }
    policy
}

fn policy_argv_inner(
    program: &str,
    args: &[String],
    runners: &[RepositoryRunner],
    depth: usize,
    leaves: &mut Vec<Vec<String>>,
) -> ValidationPolicyClassification {
    use ValidationPolicyClassification as Policy;
    if depth > MAX_WRAPPER_DEPTH {
        return Policy::Opaque;
    }
    let binary = normalized_program_name(program);
    if matches!(binary.as_str(), "env" | "command" | "time" | "exec" | "rustup") {
        return match decode_execution_wrapper(&binary, args) {
            ExecutionWrapper::Child(index) => policy_argv(&args[index], &args[index + 1..], runners, depth + 1, leaves),
            ExecutionWrapper::NonExecution => Policy::ReadOnly,
            ExecutionWrapper::Opaque => Policy::Opaque,
        };
    }
    if matches!(binary.as_str(), "bash" | "sh") {
        return executing_posix_shell_script(args).map_or(Policy::Opaque, |script| {
            policy_script(script, runners, false, depth + 1, leaves)
        });
    }
    if matches!(binary.as_str(), "pwsh" | "powershell") {
        let command = std::iter::once(program.to_owned()).chain(args.iter().cloned()).collect::<Vec<_>>();
        return crate::powershell::extract_powershell_command(&command).map_or(Policy::Opaque, |(_, script)| {
            policy_script(script, runners, true, depth + 1, leaves)
        });
    }
    if binary == "cmd" {
        let mut index = 0;
        while let Some(arg) = args.get(index) {
            if arg.eq_ignore_ascii_case("/c") || arg.eq_ignore_ascii_case("/k") {
                return policy_cmd_script(&args[index + 1..].join(" "), runners, depth + 1, leaves);
            }
            if !["/d", "/s", "/q", "/a", "/u", "/e:on", "/e:off", "/v:on", "/v:off"].iter().any(|flag| arg.eq_ignore_ascii_case(flag)) {
                return Policy::Opaque;
            }
            index += 1;
        }
        return Policy::Opaque;
    }
    if binary == "just" {
        return policy_just(args, runners, depth, leaves);
    }
    if matches!(binary.as_str(), "npx" | "bunx") {
        let mut index = 0;
        while let Some(arg) = args.get(index) {
            if matches!(arg.as_str(), "--package" | "-p") { index += 2; }
            else if matches!(arg.as_str(), "--call" | "-c") {
                return args.get(index + 1).map_or(Policy::Opaque, |script| policy_script(script, runners, false, depth + 1, leaves));
            } else if arg.starts_with('-') { index += 1; }
            else {
                let program = arg.split_once('@').filter(|(name, _)| !name.is_empty()).map_or(arg.as_str(), |(name, _)| name);
                return policy_argv(program, &args[index + 1..], runners, depth + 1, leaves);
            }
        }
        return Policy::Opaque;
    }
    if matches!(binary.as_str(), "npm" | "pnpm" | "yarn")
        && let Some(index) = node_command_index(&binary, args)
        && args[index] == "exec"
    {
        let mut child = &args[index + 1..];
        if child.first().is_some_and(|arg| arg == "--") { child = &child[1..]; }
        return child.split_first().map_or(Policy::Opaque, |(program, args)| {
            policy_argv(program, args, runners, depth + 1, leaves)
        });
    }
    if binary == "uv" && args.first().is_some_and(|arg| arg == "run") {
        return args.get(1).filter(|arg| !arg.starts_with('-')).map_or(Policy::Opaque, |program| {
            policy_argv(program, &args[2..], runners, depth + 1, leaves)
        });
    }
    if is_python_launcher(&binary) {
        let interactive_policy = |policy| {
            if python_interactive(args) && !matches!(policy, Policy::Tests | Policy::Checks) {
                Policy::Restricted
            } else {
                policy
            }
        };
        if let Some(code) = python_inline_code(args) {
            return interactive_policy(policy_python_code(code, runners, depth + 1, leaves));
        }
        if let Some(index) = python_script_index(args)
            && let Some(policy) = policy_repository_script(&args[index], &args[index + 1..], runners, depth, leaves)
        {
            return interactive_policy(policy);
        }
    }
    if let Some(runner) = runners.iter().find(|runner| runner.matches(program, args)) {
        if let Some(separator) = &runner.passthrough_after
            && let Some((program, args)) = args.iter().position(|arg| arg == separator).and_then(|index| args[index + 1..].split_first())
        {
            return policy_argv(program, args, runners, depth + 1, leaves);
        }
        if let Some(skip) = runner.passthrough_args
            && let Some((program, args)) = args.get(skip..).and_then(|args| args.split_first())
        {
            return policy_argv(program, args, runners, depth + 1, leaves);
        }
    }
    let mut canonical_args = args.to_vec();
    if binary == "cargo" && let Ok(Some(index)) = cargo_subcommand_index(args) {
        canonical_args[index] = match args[index].as_str() {
            "c" => "check", "t" => "test", "b" => "build", "r" => "run", other => other,
        }.to_owned();
    }
    leaves.push(std::iter::once(binary.clone()).chain(canonical_args).collect());
    if binary == "cargo" {
        return policy_cargo(args);
    }
    if let Some(subcommand) = binary.strip_prefix("cargo-")
        .filter(|subcommand| matches!(*subcommand, "nextest" | "llvm-cov" | "miri"))
    {
        let tail = if args.first().is_some_and(|arg| arg == subcommand) { &args[1..] } else { args };
        return policy_cargo(&std::iter::once(subcommand.to_owned()).chain(tail.iter().cloned()).collect::<Vec<_>>());
    }
    if matches!(binary.as_str(), "cargo-kda" | "kda") {
        return policy_kda(args);
    }
    if matches!(binary.as_str(), "kdr" | "rolloutdetect") {
        return policy_kdr(args);
    }
    if is_python_launcher(&binary) && !python_interactive(args)
        && let Some(policy) = policy_kdr_python(args)
    {
        return policy;
    }
    if matches!(binary.as_str(), "cargo-clippy" | "cargo-shear") {
        return policy_lint(args);
    }
    if let Some(policy) = policy_language_tool(&binary, args) {
        return policy;
    }
    let help_options = if is_python_launcher(&binary) || binary == "pytest" {
        option_flags(args, &["-m", "-W", "-X", "-k", "--override-ini", "-o", "--rootdir", "--basetemp", "--ignore", "--deselect"])
    } else {
        runner_flags(&binary, args)
    };
    let known_python_module = is_python_launcher(&binary)
        && python_module(args).is_some_and(|(module, _)| matches!(module, "pytest" | "unittest"));
    if (known_python_module || matches!(binary.as_str(), "pytest" | "vitest" | "jest" | "make" | "task" | "go" | "dotnet" | "mvn" | "mvnw" | "gradle" | "gradlew" | "cmake" | "ninja" | "msbuild" | "npm" | "pnpm" | "yarn"))
        && help_options.iter().any(|flag| matches!(*flag, "--help" | "-h" | "--version"))
    {
        return Policy::ReadOnly;
    }
    let classification = classify_argv_with_runners(program, args, runners);
    if let Some(policy) = policy_test_or_check(&classification) {
        return policy;
    }
    if matches!(&classification, ValidationClassification::Validation { .. }) {
        return if benchmark_only(&classification) { Policy::Benchmark } else { Policy::Restricted };
    }
    if matches!(binary.as_str(), "npm" | "pnpm" | "yarn" | "bun" | "go" | "dotnet" | "make" | "ninja" | "msbuild" | "cmake")
        && is_build_or_discovery(program, args)
    {
        return Policy::Restricted;
    }
    if matches!(binary.as_str(), "python" | "py" | "pytest" | "vitest" | "jest" | "go" | "dotnet" | "npm" | "pnpm" | "yarn")
        && args.first().is_some_and(|arg| matches!(arg.as_str(), "--help" | "-h" | "--version" | "-V"))
    {
        return Policy::ReadOnly;
    }
    if is_python_launcher(&binary) { Policy::Opaque } else { Policy::Unrelated }
}

fn policy_test_or_check(classification: &ValidationClassification) -> Option<ValidationPolicyClassification> {
    let ValidationClassification::Validation { leaves, .. } = classification else { return None; };
    if leaves.iter().any(|leaf| matches!(leaf.operation, ValidationOperation::Test | ValidationOperation::Fuzz)
        && matches!(leaf.mode, ValidationExecutionMode::Execution | ValidationExecutionMode::Unknown))
    {
        Some(ValidationPolicyClassification::Tests)
    } else if leaves.iter().any(|leaf| matches!(leaf.operation, ValidationOperation::Check | ValidationOperation::Lint)
        && matches!(leaf.mode, ValidationExecutionMode::Checking | ValidationExecutionMode::Unknown))
    {
        Some(ValidationPolicyClassification::Checks)
    } else { None }
}

fn policy_language_tool(binary: &str, args: &[String]) -> Option<ValidationPolicyClassification> {
    use ValidationPolicyClassification as Policy;
    let has = |flags: &[&str]| args.iter().any(|arg| flags.contains(&arg.as_str()));
    let named = matches!(binary, "tsc" | "vue-tsc" | "javac" | "eslint" | "jsonlint" | "ajv" | "deno" | "bun"
        | "shellcheck" | "pylint" | "mypy" | "pyright" | "prettier" | "ruff" | "ctest" | "mocha" | "ava" | "tap");
    if named && has(&["--help", "-h", "--version"]) { return Some(Policy::ReadOnly); }
    if matches!(binary, "tsc" | "vue-tsc" | "javac")
        || binary == "node" && has(&["--check", "-c"])
        || binary == "deno" && args.first().is_some_and(|arg| arg == "check")
    { return Some(Policy::TargetedCompilation); }
    if matches!(binary, "deno" | "bun") && args.first().is_some_and(|arg| arg == "test") {
        return Some(Policy::Tests);
    }
    if matches!(binary, "deno" | "vitest") && args.first().is_some_and(|arg| arg == "bench") {
        return Some(Policy::Benchmark);
    }
    if binary == "go" && args.first().is_some_and(|arg| arg == "test") {
        let value = |flag: &str| args.iter().enumerate().filter_map(|(index, arg)| {
            if arg == flag { args.get(index + 1).map(String::as_str) }
            else { arg.split_once('=').filter(|(key, _)| *key == flag).map(|(_, value)| value) }
        }).next_back();
        if value("-run") == Some("^$") && value("-bench").is_some_and(|value| !value.is_empty()) && value("-fuzz").is_none()
            && !args.iter().any(|arg| matches!(arg.as_str(), "--" | "-args" | "-test.run") || arg.starts_with("-test.run="))
        {
            return Some(Policy::Benchmark);
        }
    }
    if is_python_launcher(binary) && !python_interactive(args)
        && let Some((module, tail)) = python_module(args)
    {
        return match module {
            "py_compile" | "compileall" => Some(Policy::TargetedCompilation),
            "json.tool" => Some(if tail.iter().any(|arg| matches!(arg.as_str(), "--help" | "-h")) { Policy::ReadOnly } else { Policy::Checks }),
            _ => None,
        };
    }
    if matches!(binary, "ctest" | "mocha" | "ava" | "tap")
        || binary == "node" && has(&["--test"])
        || binary == "dotnet" && args.first().is_some_and(|arg| arg == "vstest")
        || binary == "java" && args.iter().any(|arg| matches!(arg.as_str(), "org.junit.runner.JUnitCore" | "org.junit.platform.console.ConsoleLauncher" | "org.testng.TestNG" | "junit.textui.TestRunner"))
    { return Some(Policy::Tests); }
    if matches!(binary, "mvn" | "mvnw" | "gradle" | "gradlew")
        && args.iter().any(|arg| {
            let task = arg.rsplit(':').next().unwrap_or(arg);
            if matches!(binary, "mvn" | "mvnw") {
                matches!(task, "verify" | "integration-test" | "package" | "install" | "deploy")
            } else { matches!(task, "build" | "check") || (task.starts_with("test") || task.ends_with("Test")) && !task.ends_with("Classes") }
        })
        && matches!(execution_mode(binary, args, ValidationOperation::Test), ValidationExecutionMode::Execution | ValidationExecutionMode::Unknown)
    { return Some(Policy::Tests); }
    if matches!(binary, "eslint" | "jsonlint" | "ajv" | "shellcheck" | "pylint" | "mypy" | "pyright") {
        return Some(if has(&["--fix", "--write"]) { Policy::Tooling } else { Policy::Checks });
    }
    if binary == "prettier" {
        return Some(if has(&["--check", "-c", "--list-different", "-l"]) { Policy::Checks } else { Policy::Tooling });
    }
    if binary == "ruff" {
        return Some(if has(&["--check"]) || args.first().is_some_and(|arg| arg == "check") && !has(&["--fix", "--fix-only"]) {
            Policy::Checks
        } else { Policy::Tooling });
    }
    if binary == "go" && args.first().is_some_and(|arg| arg == "vet") { return Some(Policy::Checks); }
    None
}

fn benchmark_only(classification: &ValidationClassification) -> bool {
    matches!(classification, ValidationClassification::Validation {
        leaves, has_unclassified_targets: false, exit_code_is_authoritative: true,
    } if !leaves.is_empty() && leaves.iter().all(|leaf| {
        leaf.operation == ValidationOperation::Bench && leaf.mode.can_prove_validation()
    }))
}

fn policy_cargo(args: &[String]) -> ValidationPolicyClassification {
    use ValidationPolicyClassification as Policy;
    let index = match cargo_subcommand_index(args) {
        Ok(Some(index)) => index,
        Ok(None) => return Policy::ReadOnly,
        Err(()) => return Policy::Opaque,
    };
    let classification = classify_argv("cargo", args);
    if matches!(&classification, ValidationClassification::Validation { leaves, .. }
        if leaves.iter().any(|leaf| matches!(leaf.operation, ValidationOperation::Test | ValidationOperation::Fuzz)
            && !matches!(leaf.mode, ValidationExecutionMode::Listing | ValidationExecutionMode::DryRun | ValidationExecutionMode::CompilationOnly)))
    {
        return Policy::Tests;
    }
    // Coverage and Miri can execute Rust tests without a `cargo test` leaf.
    if (args[index] == "miri" && args.get(index + 1).is_some_and(|arg| arg == "test")
        || args[index] == "llvm-cov" && !args.get(index + 1).is_some_and(|arg|
            matches!(arg.as_str(), "show-env" | "report" | "clean" | "export" | "demangle")))
        && matches!(execution_mode("cargo", args, ValidationOperation::Test), ValidationExecutionMode::Execution | ValidationExecutionMode::Unknown)
        && !args.iter().any(|arg| matches!(arg.as_str(), "--help" | "-h" | "--version"))
    {
        return Policy::Tests;
    }
    if matches!(&classification, ValidationClassification::Validation { leaves, .. }
        if !leaves.is_empty() && leaves.iter().all(|leaf| leaf.mode == ValidationExecutionMode::Listing))
    {
        return Policy::ReadOnly;
    }
    match args[index].as_str() {
        "check" | "c" | "bench" => {
            let benchmark = args[index] == "bench";
            if benchmark && !benchmark_only(&classify_argv("cargo", args)) {
                return Policy::Restricted;
            }
            let mut targeted = false;
            let mut named_benchmark = false;
            let mut other_targets = false;
            let mut cursor = index + 1;
            while let Some(arg) = args.get(cursor) {
                let (flag, inline) = arg.split_once('=').map_or((arg.as_str(), None), |(flag, value)| (flag, Some(value)));
                if benchmark && flag == "--" { break; }
                if matches!(flag, "--workspace" | "--all" | "-a" | "--exclude" | "--") {
                    return Policy::Restricted;
                }
                if flag == "--bench" {
                    let literal = inline.or_else(|| args.get(cursor + 1).map(String::as_str))
                        .is_some_and(literal_package_selector);
                    named_benchmark |= literal;
                    // A wildcard or missing selector must not piggyback on a named one.
                    other_targets |= !literal;
                }
                other_targets |= matches!(flag, "--lib" | "--bins" | "--bin" | "--examples" | "--example" | "--tests" | "--test" | "--benches" | "--all-targets");
                if matches!(flag, "-p" | "--package") || arg.starts_with("-p") && arg.len() > 2 {
                    let value = if matches!(flag, "-p" | "--package") {
                        inline.or_else(|| args.get(cursor + 1).map(String::as_str))
                    } else {
                        Some(&arg[2..])
                    };
                    if !value.is_some_and(literal_package_selector) { return Policy::Restricted; }
                    targeted = true;
                }
                cursor += if inline.is_none() && CARGO_VALUE_OPTIONS.contains(&flag) { 2 } else { 1 };
            }
            // A named bench also scopes `cargo check --manifest-path ... --bench ...`;
            // it compiles that target without executing or budgeting a benchmark.
            if !targeted && !(named_benchmark && !other_targets) { Policy::Restricted }
            else if benchmark { Policy::Benchmark }
            else { Policy::TargetedCompilation }
        }
        "clippy" | "shear" => policy_lint(&args[index + 1..]),
        "fmt" if args[index + 1..].iter().any(|arg| arg == "--check") => Policy::Checks,
        "kda" => policy_kda(&args[index + 1..]),
        "help" | "version" | "metadata" | "tree" | "locate-project" | "pkgid" | "search" => Policy::ReadOnly,
        "test" | "t" | "build" | "b" | "run" | "r" | "fmt" | "doc" | "fix" | "nextest" | "fuzz" | "llvm-cov" | "miri" => Policy::Restricted,
        _ => Policy::Opaque,
    }
}

fn literal_package_selector(value: &str) -> bool {
    !value.is_empty() && !value.starts_with('-') && !value.contains(['*', '?', '[', ']', '$', '`'])
}

fn policy_lint(args: &[String]) -> ValidationPolicyClassification {
    if args.iter().any(|arg| arg == "--fix" || arg.starts_with("--fix=")) {
        ValidationPolicyClassification::Restricted
    } else {
        ValidationPolicyClassification::Checks
    }
}

fn policy_kdr(args: &[String]) -> ValidationPolicyClassification {
    if args.iter().take_while(|arg| arg.as_str() != "--")
        .any(|arg| matches!(arg.as_str(), "--help" | "-h" | "--version"))
    { ValidationPolicyClassification::ReadOnly } else { ValidationPolicyClassification::Tooling }
}

fn policy_kdr_python(args: &[String]) -> Option<ValidationPolicyClassification> {
    // Use the actual interpreter operand, not a later argument or -c payload.
    let tail = if let Some((module, tail)) = python_module(args) {
        matches!(module, "kdr" | "rolloutdetect" | "tools.kdr_session_suite"
            | "tools.benchmark_frozen_analysis" | "tools.rollout_analysis.benchmark_scan")
            .then_some(tail)?
    } else {
        let index = python_script_index(args)?;
        let script = args[index].replace('\\', "/");
        ["tools/kdr_session_suite.py", "tools/benchmark_frozen_analysis.py",
            "tools/rollout_analysis/benchmark_scan.py"].iter().any(|path| {
                script == *path || script.strip_suffix(*path).is_some_and(|prefix| prefix.ends_with('/'))
            }).then_some(&args[index + 1..])?
    };
    Some(policy_kdr(tail))
}

fn policy_kda(mut args: &[String]) -> ValidationPolicyClassification {
    use ValidationPolicyClassification as Policy;
    // Cargo's external-subcommand transport repeats the subcommand name.
    if args.first().is_some_and(|arg| arg == "kda") { args = &args[1..]; }
    // Keep documented discovery out of the validation-attempt budget. Other
    // operations are classified by their own workload, not an unrestricted alias.
    if args.first().is_some_and(|arg| matches!(arg.as_str(), "lints" | "explain" | "config-schema")) {
        return Policy::ReadOnly;
    }
    if args.first().is_some_and(|arg| arg == "op") {
        let json = args.windows(2).find(|pair| pair[0] == "--json").map(|pair| pair[1].as_str())
            .or_else(|| args.iter().find_map(|arg| arg.strip_prefix("--json=")));
        if let Some(policy) = json.and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok())
            .and_then(|request| policy_kda_request(&request, 0))
        {
            return policy;
        }
    }
    if args.first().is_some_and(|arg| matches!(arg.as_str(), "test" | "tests" | "suite")) { return Policy::Tests; }
    if args.first().is_some_and(|arg| matches!(arg.as_str(), "check" | "lint" | "gate" | "validate")) { return Policy::Checks; }
    Policy::Tooling
}

/// Composite KDA requests execute their nested operations, so classify those
/// too; a plan-only batch lists its steps without running them.
fn policy_kda_request(request: &serde_json::Value, depth: usize) -> Option<ValidationPolicyClassification> {
    use ValidationPolicyClassification as Policy;
    let op = request.get("op")?.as_str()?;
    let own = match op {
        "status" | "catalog" => Policy::ReadOnly,
        "test" | "tests" | "suite" => Policy::Tests,
        "gate" | "check" | "lint" | "validate" => Policy::Checks,
        _ => Policy::Tooling,
    };
    let nested: Vec<&serde_json::Value> = match op {
        "batch" if request.get("plan_only") != Some(&serde_json::Value::Bool(true)) => request
            .get("steps")
            .and_then(serde_json::Value::as_array)
            .map(|steps| steps.iter().collect())
            .unwrap_or_default(),
        "run" => request.get("request").into_iter().collect(),
        _ => Vec::new(),
    };
    Some(combine_policy_classifications(std::iter::once(own).chain(nested.into_iter().map(|step| {
        if depth >= MAX_WRAPPER_DEPTH {
            return Policy::Restricted;
        }
        policy_kda_request(step, depth + 1).unwrap_or(Policy::Restricted)
    }))))
}

fn policy_just(args: &[String], runners: &[RepositoryRunner], depth: usize, leaves: &mut Vec<Vec<String>>) -> ValidationPolicyClassification {
    use ValidationPolicyClassification as Policy;
    if depth > MAX_WRAPPER_DEPTH { return Policy::Opaque; }
    // Reuse the manifest's constrained numeric --set grammar; arbitrary
    // overrides must not gain compile eligibility through a known recipe.
    let owner = runners.iter().find(|runner| runner.matches("just", args));
    let args = owner.and_then(|runner| runner.normalized_args("just", args)).unwrap_or(args);
    let mut index = 0;
    let mut changed_owner = false;
    while let Some(arg) = args.get(index) {
        if arg == "--command" || arg == "-c" {
            return args[index + 1..].split_first().map_or(Policy::Opaque, |(program, args)| policy_argv(program, args, runners, depth + 1, leaves));
        }
        if arg == "--shell-command" {
            return policy_script(&args[index + 1..].join(" "), runners, false, depth + 1, leaves);
        }
        if matches!(arg.as_str(), "--help" | "-h" | "--version" | "-V" | "--list" | "-l" | "--summary" | "--show" | "-s" | "--dump" | "--usage" | "--groups" | "--variables" | "--dry-run" | "-n") {
            return Policy::ReadOnly;
        }
        if !arg.starts_with('-') && !is_shell_assignment(arg) { break; }
        let flag = arg.split_once('=').map_or(arg.as_str(), |(flag, _)| flag);
        changed_owner |= is_shell_assignment(arg) || matches!(flag, "--set" | "--justfile" | "--file" | "-f" | "--working-directory" | "--directory" | "-d" | "--shell" | "--shell-arg" | "--dotenv-path" | "--dotenv-command");
        let count = if arg.contains('=') { 0 } else { just_option_value_count(arg) };
        index += 1 + count;
    }
    let tail = args.get(index..).unwrap_or_default();
    let Some(recipe) = tail.first() else { return Policy::Opaque; };
    // An owner-changing option forfeits lane privileges, not inspection of the
    // visible child: its tests and checks still propagate.
    if let Some(child) = just_lane_child(tail) && let Some((program, args)) = child.split_first() {
        let policy = policy_argv(program, args, runners, depth + 1, leaves);
        let owned_lane = !changed_owner && owner.is_some_and(|runner|
            runner.admission_runner.as_deref() == Some("rust_build_status")
                && runner.passthrough_args == Some(2));
        if policy == Policy::Checks && owned_lane {
            return Policy::Tooling;
        }
        // The visible child may restrict a foreign recipe, but cannot prove
        // that this recipe delegates only to that compiler/tooling workload.
        return if !owned_lane && matches!(policy, Policy::TargetedCompilation | Policy::Tooling | Policy::Benchmark | Policy::ReadOnly) {
            Policy::Restricted
        } else { policy };
    }
    if !changed_owner && recipe == "check-lane"
        && owner.is_some_and(|runner| runner.admission_runner.as_deref() == Some("rust_build_status")
            && runner.operations.contains(&ValidationOperation::Check))
        && tail.get(1).is_some_and(|package| literal_package_selector(package)) {
        let child = [vec!["check".to_owned(), "-p".to_owned(), tail[1].clone()], tail[2..].to_vec()].concat();
        return policy_argv("cargo", &child, runners, depth + 1, leaves);
    }
    if !changed_owner && matches!(recipe.as_str(), "clippy" | "clippy-lane" | "cargo-shear" | "lint-rust") {
        leaves.push(std::iter::once("just".to_owned()).chain(tail.iter().cloned()).collect());
        let policy = policy_lint(&tail[1..]);
        // Only these recipes delegate the whole check to the permitted lane
        // script; cargo-shear and the mixed lint-rust recipe are not exempt.
        return if policy == Policy::Checks && matches!(recipe.as_str(), "clippy" | "clippy-lane")
            && owner.is_some_and(|runner| runner.admission_runner.as_deref() == Some("rust_build_status")
                && runner.operations.contains(&ValidationOperation::Lint)) {
            Policy::Tooling
        } else { policy };
    }
    if !changed_owner && matches!(recipe.as_str(), "fmt-check" | "fmt-check-fast") {
        // These repository recipes invoke scripts/maintenance/format.py.
        leaves.push(std::iter::once("just".to_owned()).chain(tail.iter().cloned()).collect());
        return if owner.is_some_and(|runner| runner.operations.contains(&ValidationOperation::Lint)) {
            Policy::Tooling
        } else { Policy::Checks };
    }
    if !changed_owner && matches!(recipe.as_str(), "core-test-plan" | "core-test-list") {
        let arity = if recipe == "core-test-plan" { 2 } else { 1 };
        if tail.len() > arity {
            return combine_policy_classifications([
                policy_just(&tail[..arity], runners, depth + 1, leaves),
                policy_just(&tail[arity..], runners, depth + 1, leaves),
            ]);
        }
    }
    if !changed_owner && matches!(recipe.as_str(), "core-test-plan" | "core-test-list" | "core-test-gates-for") {
        return Policy::ReadOnly;
    }
    let classification = classify_argv_with_runners("just", args, runners);
    if let Some(policy) = policy_test_or_check(&classification) {
        leaves.push(std::iter::once("just".to_owned()).chain(tail.iter().cloned()).collect());
        return policy;
    }
    if !changed_owner && matches!(recipe.as_str(),
        "test" | "test-fast" | "test-fast-nosccache" | "test-timings"
        | "test-lane" | "test-lane-main" | "test-lane-fast" | "test-lane-package"
        | "_test-lane-local-reserved" | "_test-lane-fast-reserved" | "_test-lane-package-reserved"
        | "core-test" | "core-test-fast" | "core-gate" | "hooks-schema-check")
        && matches!(execution_mode("just", &tail[1..], ValidationOperation::Test), ValidationExecutionMode::Execution | ValidationExecutionMode::Unknown)
    {
        leaves.push(std::iter::once("just".to_owned()).chain(tail.iter().cloned()).collect());
        return Policy::Tests;
    }
    if matches!(recipe.as_str(), "core-gate" | "check-lane" | "release-lane" | "build" | "test-compile")
        || matches!(&classification, ValidationClassification::Validation { .. })
    {
        leaves.push(std::iter::once("just".to_owned()).chain(tail.iter().cloned()).collect());
        return if !changed_owner && benchmark_only(&classification) { Policy::Benchmark } else { Policy::Restricted };
    }
    Policy::Opaque
}

fn policy_repository_script(script: &str, args: &[String], runners: &[RepositoryRunner], depth: usize, leaves: &mut Vec<Vec<String>>) -> Option<ValidationPolicyClassification> {
    use ValidationPolicyClassification as Policy;
    let script = script.replace('\\', "/");
    let kind = if script.ends_with("scripts/build/rust_test_runner.py") { 0 }
        else if script.ends_with("scripts/build/rust_build_status.py") { 1 }
        else if script.ends_with("scripts/maintenance/root_maintenance.py") { 2 }
        else { return None; };
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        if matches!(arg.as_str(), "--help" | "-h") { return Some(Policy::ReadOnly); }
        if !arg.starts_with('-') { break; }
        let flag = arg.split_once('=').map_or(arg.as_str(), |(flag, _)| flag);
        if !matches!(flag, "--manifest" | "--target-dir" | "--admission-timeout-seconds" | "--cargo-profile") { return Some(Policy::Opaque); }
        index += if arg.contains('=') { 1 } else { 2 };
    }
    let command = args.get(index)?.as_str();
    if kind == 1 && command == "run-lane" {
        let policy = args.iter().position(|arg| arg == "--")
            .and_then(|index| args[index + 1..].split_first())
            .map_or(Policy::Opaque, |(program, args)| policy_argv(program, args, runners, depth + 1, leaves));
        let invocation = std::iter::once(script).chain(args.iter().cloned()).collect::<Vec<_>>();
        let owned_lane = runners.iter().any(|runner| {
                runner.admission_runner.as_deref() == Some("rust_build_status")
                    && runner.passthrough_after.as_deref() == Some("--")
                    && runner.matches("python", &invocation)
            });
        if policy == Policy::Checks && owned_lane {
            return Some(Policy::Tooling);
        }
        // A script wrapper must never erase a test or full-test-suite child.
        return Some(if !owned_lane && matches!(policy, Policy::TargetedCompilation | Policy::Tooling | Policy::Benchmark | Policy::ReadOnly) {
            Policy::Restricted
        } else { policy });
    }
    let script = match kind {
        0 => "scripts/build/rust_test_runner.py",
        1 => "scripts/build/rust_build_status.py",
        _ => "scripts/maintenance/root_maintenance.py",
    };
    if args.get(index + 1).is_some_and(|arg| matches!(arg.as_str(), "--help" | "-h")) {
        return Some(Policy::ReadOnly);
    }
    leaves.push(std::iter::once(script.to_owned()).chain(args.iter().cloned()).collect());
    Some(match (kind, command) {
        (0, "plan" | "plan-batch" | "list-targets" | "gates-for" | "required-for") | (1, "lanes" | "doctor" | "disk") => Policy::ReadOnly,
        (0, "run-target" | "run-gate" | "rerun-inventory" | "check-gates") | (2, "test-python") => Policy::Tests,
        (0, "check-manifest") | (2, "lint-python" | "audit-scripts") => Policy::Checks,
        _ => Policy::Opaque,
    })
}

/// CMD's literal subset uses double quotes, not POSIX single-quote escaping.
/// Expansion, FOR/IF blocks and redirection intentionally remain opaque.
fn policy_cmd_script(script: &str, runners: &[RepositoryRunner], depth: usize, leaves: &mut Vec<Vec<String>>) -> ValidationPolicyClassification {
    use ValidationPolicyClassification as Policy;
    if script.contains(['%', '!', '^', '(', ')', '<', '>']) { return Policy::Opaque; }
    let script = script.trim();
    let script = if script.starts_with('"') && script.ends_with('"') && script.matches('"').count() == 2 { &script[1..script.len() - 1] } else { script };
    let mut quoted = false;
    let mut word = String::new();
    let mut argv = Vec::new();
    let mut commands = Vec::new();
    for character in script.chars() {
        if character == '"' { quoted = !quoted; continue; }
        if !quoted && (character.is_whitespace() || matches!(character, '&' | '|')) {
            if !word.is_empty() { argv.push(std::mem::take(&mut word)); }
            if matches!(character, '&' | '|') && !argv.is_empty() { commands.push(std::mem::take(&mut argv)); }
        } else { word.push(character); }
    }
    if quoted { return Policy::Opaque; }
    if !word.is_empty() { argv.push(word); }
    if !argv.is_empty() { commands.push(argv); }
    combine_policy_classifications(commands.iter().filter_map(|argv| {
        let (program, args) = argv.split_first()?;
        let program = program.trim_start_matches('@');
        if program.eq_ignore_ascii_case("if") || program.eq_ignore_ascii_case("for") {
            return Some(Policy::Opaque);
        }
        if program.eq_ignore_ascii_case("call") {
            return Some(args.split_first().map_or(Policy::Opaque, |(program, args)| {
                policy_argv(program, args, runners, depth + 1, leaves)
            }));
        }
        Some(policy_argv(program, args, runners, depth + 1, leaves))
    }))
}

fn python_inline_code(args: &[String]) -> Option<&str> {
    match decode_python_invocation(args)?.operand? {
        PythonOperand::Code(code) => Some(code),
        _ => None,
    }
}

/// This is not a Python evaluator. Only literal subprocess calls and their
/// literal import aliases are decoded; computed arguments remain opaque.
fn policy_python_code(
    code: &str,
    runners: &[RepositoryRunner],
    depth: usize,
    leaves: &mut Vec<Vec<String>>,
) -> ValidationPolicyClassification {
    use ValidationPolicyClassification as Policy;
    let Some((statements, _)) = split_deterministic_script(code) else {
        return Policy::Opaque;
    };
    let identifier = |name: &str| {
        !name.is_empty()
            && name.chars().enumerate().all(|(index, character)| {
                character == '_'
                    || character.is_ascii_alphanumeric()
                        && (index > 0 || !character.is_ascii_digit())
            })
    };
    let functions = ["run", "call", "check_call", "check_output", "Popen"];
    let mut module_aliases = vec!["subprocess"];
    let mut function_aliases = Vec::new();
    let mut policies = Vec::new();
    for statement in statements {
        let statement = statement.trim();
        if statement.is_empty()
            || matches!(statement, "import subprocess" | "import sys" | "import subprocess, sys" | "import sys, subprocess")
        {
            continue;
        }
        if let Some(alias) = statement.strip_prefix("import subprocess as ").filter(|alias| identifier(alias)) {
            module_aliases.push(alias);
            continue;
        }
        if let Some(imports) = statement.strip_prefix("from subprocess import ") {
            for import in imports.split(',').map(str::trim) {
                let (function, alias) = import.split_once(" as ").unwrap_or((import, import));
                if functions.contains(&function) && identifier(alias) {
                    function_aliases.push(alias);
                } else {
                    policies.push(Policy::Opaque);
                }
            }
            continue;
        }
        let statement = statement
            .strip_prefix("sys.exit(")
            .or_else(|| statement.strip_prefix("raise SystemExit("))
            .and_then(|body| body.strip_suffix(')'))
            .unwrap_or(statement);
        let statement = statement
            .split_once('=')
            .filter(|(name, _)| identifier(name.trim()))
            .map_or(statement, |(_, body)| body.trim());
        let Some((callee, body)) = statement.split_once('(') else {
            policies.push(Policy::Opaque);
            continue;
        };
        let supported = function_aliases.contains(&callee)
            || callee.split_once('.').is_some_and(|(module, function)| {
                module_aliases.contains(&module) && functions.contains(&function)
            });
        let Some(body) = body.strip_suffix(')').filter(|_| supported) else {
            policies.push(Policy::Opaque);
            continue;
        };
        let parsed_argv = python_literal_argv(body);
        let parsed_script = if parsed_argv.is_none() {
            python_literal_string(body)
        } else {
            None
        };
        let rest = match (&parsed_argv, &parsed_script) {
            (Some((_, rest)), _) | (_, Some((_, rest))) => *rest,
            _ => {
                policies.push(Policy::Opaque);
                continue;
            }
        };
        // Unknown keyword expressions may execute arbitrary additional work.
        let safe_options = rest.split(',').all(|option| {
            let option = option.trim();
            option.is_empty()
                || matches!(
                    option.replace(' ', "").as_str(),
                    "check=True" | "check=False" | "shell=True" | "shell=False"
                        | "capture_output=True" | "capture_output=False"
                        | "text=True" | "text=False" | "stdout=subprocess.PIPE"
                        | "stderr=subprocess.PIPE" | "stderr=subprocess.STDOUT"
                )
                || option.split_once('=').is_some_and(|(name, value)| {
                    name.trim() == "cwd"
                        && python_literal_string(value)
                            .is_some_and(|(_, rest)| rest.trim().is_empty())
                })
        });
        let shell_list = parsed_argv.is_some() && rest.split(',')
            .any(|option| option.trim().replace(' ', "") == "shell=True");
        let policy = if let Some((argv, _)) = parsed_argv {
            argv.split_first().map_or(Policy::Opaque, |(program, args)| {
                policy_argv(program, args, runners, depth + 1, leaves)
            })
        } else if let Some((script, _)) = parsed_script {
            // Inspect both literal grammars, recording the child only once.
            let mut posix_leaves = Vec::new();
            let mut cmd_leaves = Vec::new();
            let posix = policy_script(&script, runners, false, depth + 1, &mut posix_leaves);
            let cmd = policy_cmd_script(&script, runners, depth + 1, &mut cmd_leaves);
            let policy = combine_policy_classifications([posix, cmd]);
            if policy == posix {
                leaves.extend(posix_leaves);
            } else {
                leaves.extend(cmd_leaves);
            }
            policy
        } else {
            Policy::Opaque
        };
        policies.push(if shell_list && !matches!(policy, Policy::Tests | Policy::Checks) {
            // Windows serializes lists using CRT quoting before COMSPEC /c;
            // shell metacharacters are not inert argv. Do not grant compile-
            // only or readonly authority from that list. Keep visible bans.
            Policy::Restricted
        } else if safe_options || matches!(policy, Policy::Restricted | Policy::Tests | Policy::Checks) {
            policy
        } else if matches!(policy, Policy::TargetedCompilation | Policy::Benchmark | Policy::Tooling) {
            // Effectful/unknown keyword expressions are not compile/benchmark/tooling-only.
            Policy::Restricted
        } else {
            Policy::Opaque
        });
    }
    combine_policy_classifications(policies)
}

fn python_literal_argv(input: &str) -> Option<(Vec<String>, &str)> {
    let mut input = input.trim_start().strip_prefix('[')?.trim_start();
    let mut argv = Vec::new();
    loop {
        if let Some(rest) = input.strip_prefix(']') { return Some((argv, rest)); }
        let (value, rest) = python_literal_string(input)?;
        argv.push(value);
        input = rest.trim_start();
        if let Some(rest) = input.strip_prefix(',') { input = rest.trim_start(); }
        else { return input.strip_prefix(']').map(|rest| (argv, rest)); }
    }
}

fn python_literal_string(input: &str) -> Option<(String, &str)> {
    let input = input.trim_start();
    let quote = input.chars().next().filter(|quote| matches!(*quote, '\'' | '"'))?;
    let mut value = String::new();
    let mut chars = input.char_indices().skip(1);
    while let Some((index, character)) = chars.next() {
        if character == quote { return Some((value, &input[index + character.len_utf8()..])); }
        if character == '\\' {
            let (_, escaped) = chars.next()?;
            value.push(match escaped { '\\' | '\'' | '"' => escaped, 'n' => '\n', 'r' => '\r', 't' => '\t', _ => return None });
        } else { value.push(character); }
    }
    None
}


