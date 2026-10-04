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
    pub operations: Vec<ValidationOperation>,
    #[serde(default)]
    pub options: std::collections::BTreeMap<String, usize>,
    #[serde(default)]
    pub allow_extra_args: bool,
    #[serde(default)]
    pub receipt_runner: Option<String>,
}

impl RepositoryRunner {
    pub fn matches(&self, program: &str, args: &[String]) -> bool {
        if self.operations.is_empty()
            || !self.programs.iter().any(|expected| expected == &normalized_program_name(program))
            || args.iter().any(|arg| matches!(arg.as_str(), "-h" | "--help" | "--version"))
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
                    } else if arg.split_once('=').is_some_and(|(key, _)|
                        self.options.get(key) == Some(&1))
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
}

/// Only a standalone deterministic command may authenticate a receipt. In
/// particular, `runner; echo receipt`, pipelines, and shell expansion cannot.
pub fn standalone_argv(script: &str) -> Option<Vec<String>> {
    let (commands, _) = split_deterministic_script(script)?;
    if commands.len() != 1 || script.contains(['$', '`', '>', '<']) {
        return None;
    }
    shlex::split(commands[0])
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

pub fn classify_powershell_script(script: &str) -> ValidationClassification {
    classify_powershell_script_with_runners(script, &[])
}

pub fn classify_powershell_script_with_runners(script: &str, runners: &[RepositoryRunner]) -> ValidationClassification {
    // Preserve control-flow information before the PowerShell parser flattens
    // pipelines and command chains into argv leaves.
    let simple = classify_simple_script(script, 0, runners);
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
    combine_validation_classifications(commands.into_iter().filter_map(|argv| {
        let mut arguments = argv.into_iter();
        let program = arguments.next()?;
        Some(classify_argv_with_runners(&program, &arguments.collect::<Vec<_>>(), runners))
    }))
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

fn classify_simple_script(script: &str, depth: usize, runners: &[RepositoryRunner]) -> ValidationClassification {
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
            // A guard such as `if ($LASTEXITCODE -eq 0) { cargo test }` runs
            // the same commands a flat sequence would, so its condition and
            // bodies are classified instead of the `if` keyword.
            if let Some(parts) = powershell_if_statement_parts(command)
                .or_else(|| grouped_command_body(command).map(|body| vec![body]))
            {
                return Some(combine_validation_classifications(
                    parts
                        .into_iter()
                        .map(|part| classify_simple_script(part, depth + 1, runners)),
                ));
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
            b')' | b'}' if !closers.is_empty() => {
                if closers.pop() != Some(byte) {
                    return None;
                }
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

pub fn classify_argv_with_runners(program: &str, args: &[String], runners: &[RepositoryRunner]) -> ValidationClassification {
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
    if let Some(runner) = runners.iter().find(|runner| runner.matches(program, args)) {
        return classification_from_operations(runner.operations.clone(), false);
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

    if matches!(binary.as_str(), "env" | "command" | "time") {
        let Some(index) = wrapper_program_index(&binary, args) else {
            return ValidationClassification::Opaque;
        };
        return classify_argv_at_depth(&args[index], &args[index + 1..], depth + 1, runners);
    }
    if matches!(binary.as_str(), "bash" | "sh") {
        let Some(index) = args.iter().position(|arg| shell_executes_command_arg(arg)) else {
            return ValidationClassification::Opaque;
        };
        let Some(script) = args.get(index + 1) else {
            return ValidationClassification::Opaque;
        };
        return classify_simple_script(script, depth + 1, runners);
    }
    if matches!(binary.as_str(), "pwsh" | "powershell") {
        let Some(index) = args
            .iter()
            .position(|arg| arg.eq_ignore_ascii_case("-command") || arg.eq_ignore_ascii_case("-c"))
        else {
            return ValidationClassification::Opaque;
        };
        if args.get(index + 1).is_none() {
            return ValidationClassification::Opaque;
        }
        return classify_simple_script(&args[index + 1..].join(" "), depth + 1, runners);
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
        return classify_simple_script(&args[index + 1..].join(" "), depth + 1, runners);
    }

    let (operations, has_unclassified_targets) = recognize_operations(&binary, args);
    classification_from_operations(operations, has_unclassified_targets)
}

fn classification_from_operations(
    operations: Vec<ValidationOperation>,
    has_unclassified_targets: bool,
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
                .map(|operation| ValidationCommandDescriptor { operation })
                .collect(),
            has_unclassified_targets,
            exit_code_is_authoritative: !has_unclassified_targets,
        }
    }
}

fn shell_executes_command_arg(arg: &str) -> bool {
    arg == "-c"
        || arg
            .strip_prefix('-')
            .filter(|flags| !flags.starts_with('-'))
            .is_some_and(|flags| flags.contains('c'))
}

fn wrapper_program_index(binary: &str, args: &[String]) -> Option<usize> {
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        if arg == "--" {
            return (index + 1 < args.len()).then_some(index + 1);
        }
        let takes_value = match binary {
            "env" => matches!(arg.as_str(), "-u" | "--unset" | "-C" | "--chdir"),
            "time" => matches!(arg.as_str(), "-f" | "--format" | "-o" | "--output"),
            _ => false,
        };
        if takes_value {
            index += 2;
        } else if arg.starts_with('-') || binary == "env" && is_shell_assignment(arg) {
            index += 1;
        } else {
            return Some(index);
        }
    }
    None
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
        "pytest" => (vec![ValidationOperation::Test], false),
        "vitest" | "jest" => (
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
        "npm" | "pnpm" | "yarn" => (node_operations(binary, args), false),
        "just" | "make" | "task" => wrapper_operations(binary, args),
        _ => (Vec::new(), false),
    }
}

fn is_python_launcher(binary: &str) -> bool {
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
            .filter(|argument| argument.eq_ignore_ascii_case("run"))
            .map(|_| ValidationOperation::Test)
    } else {
        cargo_operation(&subcommand)
    };
    (operation.into_iter().collect(), false)
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
        if matches!(
            argument.as_str(),
            "--version" | "-V" | "--list"
        ) {
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
        "check" => Some(ValidationOperation::Check),
        "clippy" | "fmt" => Some(ValidationOperation::Lint),
        "bench" => Some(ValidationOperation::Bench),
        "fuzz" => Some(ValidationOperation::Fuzz),
        _ => None,
    }
}

fn python_operation(args: &[String]) -> Option<ValidationOperation> {
    let mut index = 0;
    while let Some(argument) = args.get(index) {
        if argument == "-m" {
            return args
                .get(index + 1)
                .is_some_and(|module| matches!(module.as_str(), "pytest" | "unittest"))
                .then_some(ValidationOperation::Test);
        }
        if argument == "--" {
            return None;
        }
        if !argument.starts_with('-') {
            return None;
        }
        if argument == "-" {
            return None;
        }

        if matches!(
            argument.as_str(),
            "-b" | "-B"
                | "-d"
                | "-E"
                | "-h"
                | "--help"
                | "-i"
                | "-I"
                | "-O"
                | "-OO"
                | "-P"
                | "-q"
                | "-s"
                | "-S"
                | "-u"
                | "-v"
                | "-V"
                | "--version"
                | "-x"
        ) {
            index += 1;
            continue;
        }

        if matches!(argument.as_str(), "-W" | "-X" | "--check-hash-based-pycs") {
            args.get(index + 1)?;
            index += 2;
            continue;
        }
        if argument
            .strip_prefix("-W")
            .or_else(|| argument.strip_prefix("-X"))
            .is_some_and(|value| !value.is_empty())
            || argument.starts_with("--check-hash-based-pycs=")
        {
            index += 1;
            continue;
        }

        return None;
    }
    None
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
        "lint" | "lints" | "clippy" | "fmt" | "format" => vec![ValidationOperation::Lint],
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
            "-b"
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

pub fn classify_script_with_runners(script: &str, runners: &[RepositoryRunner]) -> ValidationClassification {
    classify_simple_script(script, 0, runners)
}

/// Build and inventory commands may compile, but do not establish a test pass.
pub fn is_build_or_discovery(program: &str, args: &[String]) -> bool {
    let program = normalized_program_name(program);
    if program != "cargo" {
        let subcommand = args.first().map(String::as_str);
        return match program.as_str() {
            "npm" | "pnpm" | "yarn" | "bun" => {
                matches!(subcommand, Some("install" | "ci" | "add" | "build"))
                    || (subcommand == Some("run")
                        && args.get(1).is_some_and(|arg| arg == "build"))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(program: &str, args: &[&str]) -> ValidationClassification {
        classify_argv(
            program,
            &args
                .iter()
                .map(|arg| (*arg).to_string())
                .collect::<Vec<_>>(),
        )
    }

    fn is_validation(classification: &ValidationClassification) -> bool {
        matches!(classification, ValidationClassification::Validation { .. })
    }

    fn repository_argv(program: &str, args: &[&str]) -> ValidationClassification {
        let config: serde_json::Value = serde_json::from_str(
            include_str!("../../../.codex/test-runners.json"),
        ).unwrap();
        let runners: Vec<RepositoryRunner> = serde_json::from_value(config["runners"].clone()).unwrap();
        classify_argv_with_runners(program, &args.iter().map(|arg| (*arg).to_string()).collect::<Vec<_>>(), &runners)
    }

    #[test]
    fn publishing_observation_policy_does_not_claim_validation() {
        for args in [
            vec!["publish-local-codex-final"],
            vec![
                "--justfile",
                "justfile",
                "publish-local-codex-final",
                "-Verbose",
            ],
            vec![
                "--set",
                "profile",
                "local-release",
                "publish-local-codex-final",
            ],
        ] {
            let strings = args.iter().map(|arg| (*arg).to_string()).collect::<Vec<_>>();
            assert!(is_build_or_discovery("just.exe", &strings));
            // Preserve the conservative classification for an uninspected
            // recipe; observation policy must not turn it into a test pass.
            assert_eq!(argv("just", &args), ValidationClassification::Opaque);
        }
        for args in [
            vec!["--justfile", "publish-local-codex-final"],
            vec!["--set", "recipe", "publish-local-codex-final", "help"],
            vec!["help", "publish-local-codex-final"],
            vec!["not-publish-local-codex-final"],
        ] {
            let strings = args.iter().map(|arg| (*arg).to_string()).collect::<Vec<_>>();
            assert!(!is_build_or_discovery("just", &strings));
        }
        assert!(script_prefers_long_observation_wait(
            "cd repo; just --justfile justfile publish-local-codex-final -Verbose"
        ));
        assert!(!script_prefers_long_observation_wait(
            "echo 'just publish-local-codex-final'"
        ));
    }

    #[test]
    fn observation_policy_handles_compound_long_running_commands() {
        for script in [
            "cd x && cargo build",
            "cd 'space dir'; npm install",
            "go build ./...",
            "dotnet build",
            "git clone repo",
            "pnpm run build",
            "cargo +stable --locked nextest list",
        ] {
            assert!(script_prefers_long_observation_wait(script), "{script}");
        }
        for script in ["echo 'cargo build'", "cat build.rs", "git status", "echo 'x && npm install'"] {
            assert!(!script_prefers_long_observation_wait(script), "{script}");
        }
    }

    #[test]
    fn operation_recognizer_keeps_supported_runner_families() {
        for invocation in [
            argv("cargo", &["test", "--all-features"]),
            argv("pytest", &["-q"]),
            argv("python", &["-m", "unittest", "discover"]),
            argv("dotnet", &["test", "--no-restore"]),
            argv("go", &["test", "./..."]),
            argv("npm", &["run", "test:unit", "--", "--watch=false"]),
            argv("pnpm", &["run", "lint"]),
            argv("yarn", &["test"]),
            argv("mvn", &["test", "-Dgroups=unit"]),
            argv("gradlew", &[":module:test", "--continue"]),
            argv("just", &["fmt", "--unstable"]),
            argv("make", &["tests"]),
            argv("task", &["check"]),
        ] {
            assert!(is_validation(&invocation), "{invocation:?}");
        }
    }

    #[test]
    fn package_exec_classifies_the_runner_not_its_arguments() {
        for script in [
            "pnpm exec vitest run src/example.test.ts",
            "pnpm --filter app exec vitest run",
            "npm exec -- vitest run",
            "yarn exec jest --runInBand",
        ] {
            assert!(
                matches!(
                    classify_script(script),
                    ValidationClassification::Validation {
                        exit_code_is_authoritative: true,
                        ..
                    }
                ),
                "{script}"
            );
        }
        for script in [
            "pnpm exec echo vitest",
            "pnpm exec vitest --help",
            "npm exec -- vitest --version",
            "pnpm exec node test.js",
        ] {
            assert_eq!(
                classify_script(script),
                ValidationClassification::NonValidation,
                "{script}"
            );
        }
        assert_eq!(
            classify_script("npm exec --package vitest echo"),
            ValidationClassification::Opaque
        );
        assert!(matches!(
            classify_script("pnpm exec vitest run; echo done"),
            ValidationClassification::Validation {
                exit_code_is_authoritative: false,
                ..
            }
        ));
    }

    #[test]
    fn fork_validation_routes_exclude_inventory_and_unrelated_scripts() {
        let argv = repository_argv;
        for invocation in [
            argv(
                "just",
                &[
                    "core-test-fast",
                    "core_lib",
                    "-E",
                    "test(parser)",
                ],
            ),
            argv("just", &["core-gate", "tool-output-recovery"]),
            argv(
                "python",
                &["scripts/rust_test_runner.py", "run-target", "core_lib"],
            ),
            argv(
                "python",
                &[
                    "scripts/rust_test_runner.py",
                    "--manifest",
                    "tests.toml",
                    "run-gate",
                    "demo",
                ],
            ),
            argv("uv", &["run", "python", "-m", "pytest"]),
        ] {
            assert!(is_validation(&invocation), "{invocation:?}");
        }
        for invocation in [
            argv("just", &["core-test-list"]),
            argv("just", &["core-test-plan", "core_lib"]),
            argv(
                "python",
                &["scripts/rust_test_runner.py", "plan", "core_lib"],
            ),
            argv("python", &["scripts/rust_test_runner.py", "list-targets"]),
            argv(
                "python",
                &["scripts/not_rust_test_runner.py", "run-target", "core_lib"],
            ),
            argv("uv", &["run", "python", "script.py", "pytest"]),
        ] {
            assert!(!is_validation(&invocation), "{invocation:?}");
        }
    }

    #[test]
    fn cargo_help_is_neither_validation_nor_long_running_build_work() {
        for command in ["check", "build", "clippy", "fmt", "test", "bench"] {
            for help in ["--help", "-h"] {
                for args in [
                    vec![command, help],
                    vec![help, command],
                    vec!["+stable", "--locked", command, "--verbose", help],
                ] {
                    assert_eq!(argv("cargo", &args), ValidationClassification::NonValidation, "{args:?}");
                    let args = args.iter().map(|arg| (*arg).to_string()).collect::<Vec<_>>();
                    assert!(!is_build_or_discovery("cargo", &args), "{args:?}");
                }
            }
        }
        assert_eq!(
            argv("cargo", &["nextest", "run", "--help"]),
            ValidationClassification::NonValidation
        );
        assert_eq!(
            classify_script("cargo check --help"),
            ValidationClassification::NonValidation
        );
    }

    #[test]
    fn cargo_test_binary_arguments_do_not_request_cargo_help() {
        for args in [
            vec!["test", "--", "--help"],
            vec!["test", "--", "-h"],
            vec!["+stable", "test", "--", "--exact", "help"],
        ] {
            assert!(matches!(
                argv("cargo", &args),
                ValidationClassification::Validation {
                    leaves,
                    has_unclassified_targets: false,
                    exit_code_is_authoritative: true,
                } if leaves == vec![ValidationCommandDescriptor { operation: ValidationOperation::Test }]
            ), "{args:?}");
        }
    }

    #[test]
    fn build_and_discovery_waits_are_not_validation_evidence() {
        for args in [
            vec!["build"],
            vec!["+stable", "--locked", "build"],
            vec!["nextest", "list"],
        ] {
            let args = args.into_iter().map(str::to_string).collect::<Vec<_>>();
            assert!(is_build_or_discovery("cargo", &args));
            assert_eq!(
                classify_argv("cargo", &args),
                ValidationClassification::NonValidation
            );
            assert!(!is_build_or_discovery("echo", &args));
        }
        for script in [
            "echo cargo build",
            "just core-test-plan core_lib",
            "just not-test-fast",
        ] {
            assert!(!matches!(
                classify_script(script),
                ValidationClassification::Validation { .. }
            ));
        }
    }

    #[test]
    fn focused_recipes_own_arguments_without_inventing_checks() {
        let argv = repository_argv;
        for recipe in [
            "test-fast",
            "validate-crate-focused",
            "core-test-small",
            "test-slow-boundaries",
            "app-server-runtime-check",
            "tui-large-widget-check",
            "app-server-command-exec-check",
            "app-server-process-exec-check",
            "app-server-thread-status-check",
            "config-schema-protocol-check",
        ] {
            assert!(
                matches!(argv("just", &[recipe, "codex-check", "--lib"]),
                ValidationClassification::Validation { ref leaves, has_unclassified_targets: false, .. }
                if leaves == &[ValidationCommandDescriptor { operation: ValidationOperation::Test }]),
                "{recipe}"
            );
        }
        assert!(
            matches!(argv("just", &["validate-crate", "codex-check", "--lib"]),
            ValidationClassification::Validation { ref leaves, .. }
            if leaves.iter().map(|leaf| leaf.operation).collect::<Vec<_>>() == vec![ValidationOperation::Lint, ValidationOperation::Test])
        );
    }
}
