use std::sync::Arc;

use serde::Serialize;
use tokio::sync::RwLock;

use crate::tools::handlers::command_shape::CommandInvocation;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ValidationOperation {
    Test,
    Check,
    Lint,
    Bench,
    Fuzz,
}

const ALL_OPERATIONS: [ValidationOperation; 5] = [
    ValidationOperation::Test,
    ValidationOperation::Check,
    ValidationOperation::Lint,
    ValidationOperation::Bench,
    ValidationOperation::Fuzz,
];

#[derive(Debug, Default)]
pub(crate) struct ValidationAuthorization {
    enabled: bool,
    pub(crate) revision: u64,
    denied: [bool; 5],
}

pub(crate) type SharedValidationAuthorization = Arc<RwLock<ValidationAuthorization>>;

impl ValidationAuthorization {
    pub(crate) fn enabled() -> Self {
        Self {
            enabled: true,
            ..Self::default()
        }
    }

    pub(crate) fn update_from_user_input(&mut self, text: &str) -> bool {
        if !self.enabled {
            return false;
        }
        let directives = parse_directives(text);
        if directives.is_empty() {
            return false;
        }
        self.revision = self.revision.saturating_add(1);
        for (operation, denied) in directives {
            self.denied[operation_index(operation)] = denied;
        }
        true
    }

    fn is_enabled(&self) -> bool {
        self.enabled
    }

    fn is_denied(&self, operation: ValidationOperation) -> bool {
        self.denied[operation_index(operation)]
    }

    fn has_any_denial(&self) -> bool {
        self.denied.iter().copied().any(std::convert::identity)
    }
}

const fn operation_index(operation: ValidationOperation) -> usize {
    match operation {
        ValidationOperation::Test => 0,
        ValidationOperation::Check => 1,
        ValidationOperation::Lint => 2,
        ValidationOperation::Bench => 3,
        ValidationOperation::Fuzz => 4,
    }
}

fn parse_directives(text: &str) -> Vec<(ValidationOperation, bool)> {
    let mut normalized = text
        .replace(['\u{2018}', '\u{2019}'], "'")
        .to_ascii_lowercase();
    for starter in [
        "do not ",
        "don't ",
        "dont ",
        "never ",
        "must not ",
        "may not ",
        "cannot ",
        "can't ",
        "you must not ",
        "you may not ",
        "you cannot ",
        "you can't ",
        "no ",
        "skip ",
        "without running ",
        "without executing ",
        "run ",
        "rerun ",
        "re-run ",
        "execute ",
        "perform ",
        "start ",
        "allow ",
        "permit ",
        "you may ",
        "you can ",
        "go ahead and ",
        "feel free to ",
    ] {
        normalized = normalized.replace(&format!(", {starter}"), &format!(";{starter}"));
        normalized = normalized.replace(&format!(" and {starter}"), &format!(";{starter}"));
        normalized = normalized.replace(
            &format!(", please {starter}"),
            &format!(";please {starter}"),
        );
        normalized = normalized.replace(
            &format!(" and please {starter}"),
            &format!(";please {starter}"),
        );
    }
    normalized = normalized
        .replace(",;", ";")
        .replace(" but ", ";")
        .replace(" then ", ";");

    let mut directives = Vec::new();
    for clause in normalized.lines().flat_map(|line| line.split(['.', ';'])) {
        directives.extend(parse_directive(clause));
        if let Some(denial) = imperative_suffix_denial(clause) {
            directives.extend(parse_directive(denial));
        }
    }
    directives
}

fn imperative_suffix_denial(clause: &str) -> Option<&str> {
    let clause = clause
        .trim()
        .strip_prefix("please, ")
        .or_else(|| clause.trim().strip_prefix("please "))
        .unwrap_or_else(|| clause.trim());
    let imperative = clause.split_whitespace().next().is_some_and(|verb| {
        matches!(
            verb,
            "add"
                | "build"
                | "change"
                | "complete"
                | "continue"
                | "create"
                | "do"
                | "finish"
                | "fix"
                | "implement"
                | "keep"
                | "proceed"
                | "remove"
                | "update"
        )
    });
    if !imperative {
        return None;
    }
    clause
        .find(" without running ")
        .map(|index| &clause[index + 1..])
        .or_else(|| {
            clause
                .find(" without executing ")
                .map(|index| &clause[index + 1..])
        })
}

fn parse_directive(clause: &str) -> Vec<(ValidationOperation, bool)> {
    let clause = clause.trim();
    if clause.is_empty() || clause.contains('?') {
        return Vec::new();
    }
    let clause = clause
        .strip_prefix("please, ")
        .or_else(|| clause.strip_prefix("please "))
        .unwrap_or(clause)
        .trim();

    let (denied, body) = if let Some(body) = [
        "do not ",
        "don't ",
        "dont ",
        "never ",
        "must not ",
        "may not ",
        "cannot ",
        "can't ",
        "you must not ",
        "you may not ",
        "you cannot ",
        "you can't ",
    ]
    .iter()
    .find_map(|prefix| clause.strip_prefix(prefix))
    {
        let Some(body) = validation_action_body(body) else {
            return Vec::new();
        };
        (true, body)
    } else if let Some(body) = clause.strip_prefix("no ") {
        let Some(body) = bare_no_validation_body(body) else {
            return Vec::new();
        };
        (true, body)
    } else if let Some(body) = clause.strip_prefix("skip ") {
        (true, body)
    } else if let Some(body) = clause
        .strip_prefix("without running ")
        .or_else(|| clause.strip_prefix("without executing "))
    {
        (true, body)
    } else {
        let clause = ["you may ", "you can ", "go ahead and ", "feel free to "]
            .iter()
            .find_map(|prefix| clause.strip_prefix(prefix))
            .unwrap_or(clause);
        let Some(body) = validation_action_body(clause) else {
            return Vec::new();
        };
        (false, body)
    };

    operations_from_instruction(body)
        .into_iter()
        .map(|operation| (operation, denied))
        .collect()
}

fn bare_no_validation_body(body: &str) -> Option<&str> {
    let mut saw_operation = false;
    for component in body
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|component| !component.is_empty())
    {
        match component {
            "test" | "tests" | "testing" | "suite" | "suites" | "check" | "checks" | "checking"
            | "lint" | "lints" | "linting" | "clippy" | "bench" | "benches" | "benchmark"
            | "benchmarks" | "benchmarking" | "fuzz" | "fuzzing" | "fuzzer" | "fuzzers"
            | "validation" | "validations" => saw_operation = true,
            "and" | "or" | "any" | "all" | "more" | "further" => {}
            _ => return None,
        }
    }
    saw_operation.then_some(body)
}

fn validation_action_body(text: &str) -> Option<&str> {
    let text = text.trim();
    for prefix in [
        "run ", "rerun ", "re-run ", "execute ", "perform ", "start ", "allow ", "permit ",
    ] {
        if let Some(body) = text.strip_prefix(prefix) {
            return Some(body);
        }
    }

    let first = text
        .split(|character: char| !character.is_ascii_alphanumeric())
        .next()
        .unwrap_or_default();
    matches!(
        first,
        "test" | "check" | "lint" | "bench" | "benchmark" | "fuzz"
    )
    .then_some(text)
}

fn operations_from_instruction(body: &str) -> Vec<ValidationOperation> {
    let components = body
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|component| !component.is_empty())
        .collect::<Vec<_>>();
    if components
        .iter()
        .any(|component| matches!(*component, "validation" | "validations"))
    {
        return ALL_OPERATIONS.to_vec();
    }

    let mut operations = Vec::new();
    for component in components {
        let operation = match component {
            "test" | "tests" | "testing" | "suite" | "suites" => Some(ValidationOperation::Test),
            "check" | "checks" | "checking" => Some(ValidationOperation::Check),
            "lint" | "lints" | "linting" | "clippy" => Some(ValidationOperation::Lint),
            "bench" | "benches" | "benchmark" | "benchmarks" | "benchmarking" => {
                Some(ValidationOperation::Bench)
            }
            "fuzz" | "fuzzing" | "fuzzer" | "fuzzers" => Some(ValidationOperation::Fuzz),
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ValidationCommandDescriptor {
    pub(crate) operation: ValidationOperation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ValidationClassification {
    NonValidation,
    Validation {
        leaves: Vec<ValidationCommandDescriptor>,
        has_unclassified_targets: bool,
    },
    Opaque,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ValidationSkipReason {
    UserProhibitedValidation,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ValidationSkippedToolOutput {
    pub(crate) reason: ValidationSkipReason,
    pub(crate) skip_disposition: codex_tools::ToolOutputSkipDisposition,
    pub(crate) command_was_executed: bool,
    pub(crate) operation: Option<ValidationOperation>,
}

impl ValidationSkippedToolOutput {
    fn prohibited(operation: Option<ValidationOperation>) -> Self {
        Self {
            reason: ValidationSkipReason::UserProhibitedValidation,
            skip_disposition: codex_tools::ToolOutputSkipDisposition::Suppressed,
            command_was_executed: false,
            operation,
        }
    }
}

#[cfg(test)]
pub(crate) fn prohibited_skip_for(
    authorization: &ValidationAuthorization,
    invocation: &CommandInvocation,
    explicitly_tagged: bool,
) -> Option<ValidationSkippedToolOutput> {
    let classification = classify_validation(invocation);
    prohibited_skip_for_classification(authorization, &classification, explicitly_tagged)
}

pub(crate) fn prohibited_skip_for_classification(
    authorization: &ValidationAuthorization,
    classification: &ValidationClassification,
    explicitly_tagged: bool,
) -> Option<ValidationSkippedToolOutput> {
    if !authorization.is_enabled() {
        return None;
    }
    match classification {
        ValidationClassification::Validation {
            leaves,
            has_unclassified_targets,
        } => {
            if explicitly_tagged && *has_unclassified_targets && authorization.has_any_denial() {
                Some(ValidationSkippedToolOutput::prohibited(None))
            } else {
                leaves
                    .iter()
                    .find(|leaf| authorization.is_denied(leaf.operation))
                    .map(|leaf| ValidationSkippedToolOutput::prohibited(Some(leaf.operation)))
            }
        }
        ValidationClassification::NonValidation | ValidationClassification::Opaque
            if explicitly_tagged && authorization.has_any_denial() =>
        {
            Some(ValidationSkippedToolOutput::prohibited(None))
        }
        ValidationClassification::NonValidation | ValidationClassification::Opaque => None,
    }
}

#[derive(Debug)]
pub(crate) enum ValidationAdmission {
    Execute {
        authorization_revision: u64,
        is_validation: bool,
        classification: ValidationClassification,
    },
    Skip(ValidationSkippedToolOutput),
}

#[derive(Debug, Clone)]
pub(crate) struct ValidationLaunchPlan {
    pub(crate) classification: ValidationClassification,
    pub(crate) authorization_revision: u64,
    pub(crate) explicitly_tagged: bool,
    pub(crate) structured_route: Option<codex_protocol::plan_tool::ValidationRoute>,
    pub(crate) bound_plan_step: Option<(String, u64)>,
    pub(crate) bound_work_unit: Option<(String, u64)>,
    pub(crate) validation_call_id: Option<String>,
    pub(crate) turn_timing_state: Option<Arc<crate::turn_timing::TurnTimingState>>,
    pub(crate) focused_validation_token:
        Option<crate::agent::task_coordinator::FocusedValidationToken>,
}

pub(crate) fn recheck_validation_launch(
    authorization: &ValidationAuthorization,
    launch: &ValidationLaunchPlan,
) -> Option<ValidationSkippedToolOutput> {
    (authorization.revision != launch.authorization_revision)
        .then(|| {
            prohibited_skip_for_classification(
                authorization,
                &launch.classification,
                launch.explicitly_tagged,
            )
        })
        .flatten()
}

#[cfg(test)]
pub(crate) async fn admit_validation(
    authorization: &SharedValidationAuthorization,
    invocation: &CommandInvocation,
    explicitly_tagged: bool,
) -> ValidationAdmission {
    admit_validation_invocations(
        authorization,
        std::slice::from_ref(invocation),
        explicitly_tagged,
    )
    .await
}

pub(crate) async fn admit_validation_invocations(
    authorization: &SharedValidationAuthorization,
    invocations: &[CommandInvocation],
    explicitly_tagged: bool,
) -> ValidationAdmission {
    let authorization = authorization.read().await;
    if !authorization.is_enabled() {
        return ValidationAdmission::Execute {
            authorization_revision: authorization.revision,
            is_validation: false,
            classification: ValidationClassification::NonValidation,
        };
    }
    let classification = classify_validation_invocations(invocations);
    if let Some(skipped) =
        prohibited_skip_for_classification(&authorization, &classification, explicitly_tagged)
    {
        return ValidationAdmission::Skip(skipped);
    }
    let is_validation =
        explicitly_tagged || matches!(&classification, ValidationClassification::Validation { .. });
    ValidationAdmission::Execute {
        authorization_revision: authorization.revision,
        is_validation,
        classification,
    }
}

pub(crate) fn classify_validation(invocation: &CommandInvocation) -> ValidationClassification {
    #[cfg(test)]
    VALIDATION_CLASSIFICATION_COUNT.with(|count| count.set(count.get() + 1));
    match invocation {
        CommandInvocation::Argv { program, args } => classify_argv(program, args),
        CommandInvocation::Script(script) | CommandInvocation::PowerShellScript(script) => {
            classify_simple_script(script, 0)
        }
    }
}

#[cfg(test)]
thread_local! {
    static VALIDATION_CLASSIFICATION_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn reset_validation_classification_count() {
    VALIDATION_CLASSIFICATION_COUNT.with(|count| count.set(0));
}

#[cfg(test)]
pub(crate) fn validation_classification_count() -> usize {
    VALIDATION_CLASSIFICATION_COUNT.with(std::cell::Cell::get)
}

pub(crate) fn classify_validation_invocations(
    invocations: &[CommandInvocation],
) -> ValidationClassification {
    combine_validation_classifications(invocations.iter().map(classify_validation))
}

fn combine_validation_classifications(
    classifications: impl IntoIterator<Item = ValidationClassification>,
) -> ValidationClassification {
    let mut leaves = Vec::new();
    let mut has_unclassified_targets = false;
    let mut saw_opaque = false;
    for classification in classifications {
        match classification {
            ValidationClassification::Validation {
                leaves: mut found,
                has_unclassified_targets: found_unclassified,
            } => {
                leaves.append(&mut found);
                has_unclassified_targets |= found_unclassified;
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
        }
    }
}

const MAX_WRAPPER_DEPTH: usize = 4;

fn classify_simple_script(script: &str, depth: usize) -> ValidationClassification {
    if depth > MAX_WRAPPER_DEPTH {
        return ValidationClassification::Opaque;
    }
    let Some(commands) = split_deterministic_script(script) else {
        return ValidationClassification::Opaque;
    };
    let classifications = commands.into_iter().filter_map(|command| {
        let Some(words) = shlex::split(command.trim()) else {
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
        ))
    });
    combine_validation_classifications(classifications)
}

fn split_deterministic_script(script: &str) -> Option<Vec<&str>> {
    if script.contains("$(") || script.contains("${") || script.contains('`') {
        return None;
    }
    let bytes = script.as_bytes();
    let mut commands = Vec::new();
    let mut start = 0;
    let mut index = 0;
    let mut quote = None;
    let mut escaped = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if escaped {
            escaped = false;
            index += 1;
            continue;
        }
        if byte == b'\\' && quote != Some(b'\'') {
            escaped = true;
            index += 1;
            continue;
        }
        if matches!(byte, b'\'' | b'"') {
            match quote {
                Some(active) if active == byte => quote = None,
                None => quote = Some(byte),
                Some(_) => {}
            }
            index += 1;
            continue;
        }
        if quote.is_some() {
            index += 1;
            continue;
        }
        let separator_length = match byte {
            b';' | b'\r' | b'\n' => 1,
            b'&' if bytes.get(index + 1) == Some(&b'&') => 2,
            b'|' if bytes.get(index + 1) == Some(&b'|') => 2,
            b'&' | b'|' => return None,
            _ => 0,
        };
        if separator_length == 0 {
            index += 1;
            continue;
        }
        commands.push(&script[start..index]);
        index += separator_length;
        if byte == b'\r' && bytes.get(index) == Some(&b'\n') {
            index += 1;
        }
        start = index;
    }
    if quote.is_some() || escaped {
        return None;
    }
    commands.push(&script[start..]);
    Some(commands)
}

fn classify_argv(program: &str, args: &[String]) -> ValidationClassification {
    classify_argv_at_depth(program, args, 0)
}

fn classify_argv_at_depth(
    program: &str,
    args: &[String],
    depth: usize,
) -> ValidationClassification {
    if depth > MAX_WRAPPER_DEPTH {
        return ValidationClassification::Opaque;
    }
    let binary = normalized_program_name(program);

    if matches!(binary.as_str(), "env" | "command" | "time") {
        let Some(index) = wrapper_program_index(&binary, args) else {
            return ValidationClassification::Opaque;
        };
        return classify_argv_at_depth(&args[index], &args[index + 1..], depth + 1);
    }
    if matches!(binary.as_str(), "bash" | "sh") {
        let Some(index) = args.iter().position(|arg| shell_executes_command_arg(arg)) else {
            return ValidationClassification::Opaque;
        };
        let Some(script) = args.get(index + 1) else {
            return ValidationClassification::Opaque;
        };
        return classify_simple_script(script, depth + 1);
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
        return classify_simple_script(&args[index + 1..].join(" "), depth + 1);
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
        return classify_simple_script(&args[index + 1..].join(" "), depth + 1);
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
        "python" | "python3" => (python_operation(args).into_iter().collect(), false),
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
    let mut index = 0;
    while let Some(argument) = args.get(index) {
        if matches!(
            argument.as_str(),
            "--version" | "-V" | "--list" | "--help" | "-h"
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
    (args.first().is_some_and(|argument| argument == "-m")
        && args
            .get(1)
            .is_some_and(|module| matches!(module.as_str(), "pytest" | "unittest")))
    .then_some(ValidationOperation::Test)
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
        return args[command_index + 2..]
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
        let found = selector_operations(selector);
        if found.is_empty() {
            has_unclassified_targets = true;
        } else {
            extend_unique(&mut operations, found);
        }
        index += 1;
    }
    (operations, has_unclassified_targets)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(program: &str, args: &[&str]) -> CommandInvocation {
        CommandInvocation::Argv {
            program: program.to_string(),
            args: args.iter().map(|arg| (*arg).to_string()).collect(),
        }
    }

    fn is_validation(invocation: &CommandInvocation) -> bool {
        matches!(
            classify_validation(invocation),
            ValidationClassification::Validation { .. }
        )
    }

    #[test]
    fn latest_explicit_instruction_replaces_operation_denial() {
        let mut authorization = ValidationAuthorization::enabled();
        assert!(authorization.update_from_user_input("do not run tests; run tests"));
        assert!(prohibited_skip_for(&authorization, &argv("cargo", &["test"]), false).is_none());

        assert!(authorization.update_from_user_input("run lint; never run lint"));
        assert!(prohibited_skip_for(&authorization, &argv("cargo", &["clippy"]), false).is_some());

        let mut authorization = ValidationAuthorization::enabled();
        assert!(authorization.update_from_user_input("do not run lint, run tests"));
        assert!(prohibited_skip_for(&authorization, &argv("cargo", &["test"]), false).is_none());
        assert!(prohibited_skip_for(&authorization, &argv("cargo", &["clippy"]), false).is_some());

        for prefix in [
            "must not",
            "may not",
            "cannot",
            "can't",
            "you must not",
            "you may not",
            "you cannot",
            "you can't",
        ] {
            let mut authorization = ValidationAuthorization::enabled();
            assert!(
                authorization.update_from_user_input(&format!("run tests, {prefix} run tests"))
            );
            assert!(
                prohibited_skip_for(&authorization, &argv("cargo", &["test"]), false).is_some(),
                "{prefix}"
            );
        }

        let mut authorization = ValidationAuthorization::enabled();
        assert!(authorization.update_from_user_input("do not run tests, you may run tests"));
        assert!(prohibited_skip_for(&authorization, &argv("cargo", &["test"]), false).is_none());

        let mut authorization = ValidationAuthorization::enabled();
        assert!(authorization.update_from_user_input("do not run tests, please run tests"));
        assert!(prohibited_skip_for(&authorization, &argv("cargo", &["test"]), false).is_none());

        assert!(authorization.update_from_user_input("do not run tests and lint"));
        assert!(authorization.update_from_user_input("run tests"));
        assert!(prohibited_skip_for(&authorization, &argv("cargo", &["test"]), false).is_none());
        assert!(prohibited_skip_for(&authorization, &argv("cargo", &["clippy"]), false).is_some());

        let mut authorization = ValidationAuthorization::enabled();
        assert!(authorization.update_from_user_input("do not run tests, lint, and checks"));
        for invocation in [
            argv("cargo", &["test"]),
            argv("cargo", &["clippy"]),
            argv("cargo", &["check"]),
        ] {
            assert!(
                prohibited_skip_for(&authorization, &invocation, false).is_some(),
                "{invocation:?}"
            );
        }
    }

    #[test]
    fn only_straightforward_explicit_directives_change_denial_state() {
        let mut authorization = ValidationAuthorization::enabled();
        assert!(!authorization.update_from_user_input("do not modify tests"));
        assert!(!authorization.update_from_user_input("tests were not run"));
        assert!(!authorization.update_from_user_input("No tests were run."));
        assert!(
            !authorization
                .update_from_user_input("The previous agent completed this without running tests.")
        );
        assert!(!authorization.update_from_user_input("that should not happen again"));
        assert!(!authorization.update_from_user_input("should we run tests?"));

        assert!(authorization.update_from_user_input("do not check"));
        assert!(prohibited_skip_for(&authorization, &argv("cargo", &["check"]), false).is_some());
        assert!(authorization.update_from_user_input("don't test this change"));
        assert!(
            prohibited_skip_for(
                &authorization,
                &argv("cargo", &["test", "selected_case"]),
                false,
            )
            .is_some()
        );
        assert!(authorization.update_from_user_input("you may run tests"));
        assert!(prohibited_skip_for(&authorization, &argv("pytest", &["-q"]), false).is_none());

        assert!(authorization.update_from_user_input("implement this without running tests"));
        assert!(prohibited_skip_for(&authorization, &argv("pytest", &["-q"]), false).is_some());

        assert!(authorization.update_from_user_input("no tests"));
        assert!(prohibited_skip_for(&authorization, &argv("pytest", &["-q"]), false).is_some());
        assert!(authorization.update_from_user_input("run tests"));
        assert!(prohibited_skip_for(&authorization, &argv("pytest", &["-q"]), false).is_none());
    }

    #[test]
    fn scope_words_do_not_override_the_latest_operation_stance() {
        let focused = argv("cargo", &["test", "selected_case"]);
        let workspace = argv("cargo", &["test", "--workspace"]);

        let mut authorization = ValidationAuthorization::enabled();
        assert!(
            authorization
                .update_from_user_input("Run focused tests; do not run the workspace suite.")
        );
        assert!(prohibited_skip_for(&authorization, &focused, false).is_some());
        assert!(prohibited_skip_for(&authorization, &workspace, false).is_some());

        let mut authorization = ValidationAuthorization::enabled();
        assert!(
            authorization
                .update_from_user_input("Do not run the workspace suite; run focused tests.")
        );
        assert!(prohibited_skip_for(&authorization, &focused, false).is_none());
        assert!(prohibited_skip_for(&authorization, &workspace, false).is_none());
    }

    #[tokio::test]
    async fn positive_instruction_is_not_required_and_feature_boundary_is_preserved() {
        let enabled = Arc::new(RwLock::new(ValidationAuthorization::enabled()));
        reset_validation_classification_count();
        assert!(matches!(
            admit_validation(&enabled, &argv("cargo", &["test"]), false).await,
            ValidationAdmission::Execute {
                is_validation: true,
                ..
            }
        ));
        assert_eq!(validation_classification_count(), 1);

        let disabled = Arc::new(RwLock::new(ValidationAuthorization::default()));
        assert!(matches!(
            admit_validation(&disabled, &argv("cargo", &["test"]), true).await,
            ValidationAdmission::Execute {
                is_validation: false,
                ..
            }
        ));
    }

    #[test]
    fn tagged_unknown_target_is_blocked_by_any_active_denial() {
        let mut authorization = ValidationAuthorization::enabled();
        assert!(authorization.update_from_user_input("do not run checks"));
        assert!(
            prohibited_skip_for(&authorization, &argv("custom-runner", &["verify"]), true)
                .is_some()
        );
        assert!(
            prohibited_skip_for(&authorization, &argv("custom-runner", &["verify"]), false)
                .is_none()
        );

        let mixed = argv("make", &["test", "deploy"]);
        assert!(prohibited_skip_for(&authorization, &mixed, true).is_some());
        assert!(prohibited_skip_for(&authorization, &mixed, false).is_none());
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
            argv("just", &["fmt-check", "--unstable"]),
            argv("make", &["integration-tests"]),
            argv("task", &["check"]),
        ] {
            assert!(is_validation(&invocation), "{invocation:?}");
        }
    }

    #[test]
    fn cargo_global_options_nextest_and_fmt_preserve_validation_operations() {
        for invocation in [
            argv("cargo", &["nextest", "run"]),
            argv("cargo", &["--locked", "test"]),
            argv("cargo", &["--color", "always", "test"]),
        ] {
            assert!(
                matches!(
                    classify_validation(&invocation),
                    ValidationClassification::Validation { ref leaves, .. }
                        if leaves.len() == 1
                            && leaves[0].operation == ValidationOperation::Test
                ),
                "{invocation:?}"
            );
        }

        let offline_check = argv("cargo", &["--offline", "check"]);
        assert!(matches!(
            classify_validation(&offline_check),
            ValidationClassification::Validation { ref leaves, .. }
                if leaves.len() == 1 && leaves[0].operation == ValidationOperation::Check
        ));

        for invocation in [
            argv("cargo", &["fmt", "--check"]),
            argv("cargo", &["+nightly", "fmt", "--", "--check"]),
        ] {
            assert!(
                matches!(
                    classify_validation(&invocation),
                    ValidationClassification::Validation { ref leaves, .. }
                        if leaves.len() == 1
                            && leaves[0].operation == ValidationOperation::Lint
                ),
                "{invocation:?}"
            );
        }

        let mut authorization = ValidationAuthorization::enabled();
        assert!(authorization.update_from_user_input("do not run tests or lint"));
        for invocation in [
            argv("cargo", &["nextest", "run"]),
            argv("cargo", &["--locked", "test"]),
            argv("cargo", &["fmt", "--check"]),
            argv("cargo", &["+nightly", "fmt", "--", "--check"]),
        ] {
            assert!(
                prohibited_skip_for(&authorization, &invocation, false).is_some(),
                "{invocation:?}"
            );
        }

        assert_eq!(
            classify_validation(&argv("cargo", &["--unknown-global", "test"])),
            ValidationClassification::Opaque
        );
    }

    #[tokio::test]
    async fn parsed_pipeline_stages_cannot_bypass_validation_denial() {
        let mut authorization = ValidationAuthorization::enabled();
        assert!(authorization.update_from_user_input("do not run tests"));
        let authorization = Arc::new(RwLock::new(authorization));
        let invocations = [
            argv("cargo", &["test"]),
            argv("Select-Object", &["-First", "1"]),
        ];

        assert!(matches!(
            admit_validation_invocations(&authorization, &invocations, false).await,
            ValidationAdmission::Skip(ValidationSkippedToolOutput {
                operation: Some(ValidationOperation::Test),
                ..
            })
        ));
    }

    #[test]
    fn late_authorization_recheck_reuses_the_admitted_classification() {
        let mut authorization = ValidationAuthorization::enabled();
        let launch = ValidationLaunchPlan {
            classification: classify_validation(&argv("cargo", &["test"])),
            authorization_revision: authorization.revision,
            explicitly_tagged: false,
            structured_route: None,
            bound_plan_step: None,
            bound_work_unit: None,
            validation_call_id: None,
            turn_timing_state: None,
            focused_validation_token: None,
        };
        assert!(authorization.update_from_user_input("do not run tests"));
        reset_validation_classification_count();

        assert!(matches!(
            recheck_validation_launch(&authorization, &launch),
            Some(ValidationSkippedToolOutput {
                operation: Some(ValidationOperation::Test),
                ..
            })
        ));
        assert_eq!(validation_classification_count(), 0);
    }

    #[test]
    fn operation_recognition_does_not_parse_runner_modes() {
        for invocation in [
            argv("task", &["--summary", "test"]),
            argv("go", &["test", "-c"]),
            argv("dotnet", &["test", "--list-tests"]),
        ] {
            assert!(is_validation(&invocation), "{invocation:?}");
        }

        for invocation in [
            argv("npm", &["--help"]),
            argv("just", &["--list"]),
            argv("cargo", &["--version"]),
        ] {
            assert!(!is_validation(&invocation), "{invocation:?}");
        }
    }

    #[test]
    fn value_taking_node_flags_do_not_hide_the_script_selector() {
        for invocation in [
            argv("npm", &["--prefix", "web", "run", "lint"]),
            argv("pnpm", &["--filter", "api", "test"]),
            argv("yarn", &["--cwd", "web", "test"]),
            argv("yarn", &["workspace", "web", "run", "check"]),
        ] {
            assert!(is_validation(&invocation), "{invocation:?}");
        }
    }

    #[test]
    fn forwarded_runner_arguments_are_not_validation_operations() {
        for invocation in [
            argv("cargo", &["run", "--", "test"]),
            argv("dotnet", &["run", "test"]),
            argv("go", &["run", "test"]),
            argv("npm", &["install", "test"]),
            argv("python", &["script.py", "-m", "pytest"]),
            argv("gradle", &["-p", "test", "build"]),
            argv("mvn", &["-f", "test", "package"]),
            argv("make", &["-f", "test", "all"]),
            argv("task", &["--dir", "test", "build"]),
        ] {
            assert!(!is_validation(&invocation), "{invocation:?}");
        }
    }

    #[test]
    fn just_scans_past_options_and_non_validation_recipes() {
        let invocations = [
            argv("just", &["--justfile", "Justfile", "test"]),
            argv("just", &["prepare", "test"]),
        ];
        let mut authorization = ValidationAuthorization::enabled();
        assert!(authorization.update_from_user_input("no tests"));

        for invocation in invocations {
            assert!(is_validation(&invocation), "{invocation:?}");
            assert!(
                prohibited_skip_for(&authorization, &invocation, false).is_some(),
                "{invocation:?}"
            );
        }
    }

    #[test]
    fn just_option_values_are_not_recipe_targets() {
        let mut authorization = ValidationAuthorization::enabled();
        assert!(authorization.update_from_user_input("do not run lint"));
        for invocation in [
            argv("just", &["--shell", "bash", "test"]),
            argv("just", &["--color", "always", "test"]),
            argv("just", &["--set", "MODE", "test", "test"]),
        ] {
            assert!(is_validation(&invocation), "{invocation:?}");
            assert!(
                prohibited_skip_for(&authorization, &invocation, true).is_none(),
                "{invocation:?}"
            );
        }
    }

    #[test]
    fn simple_commands_are_recognized_without_shell_emulation() {
        let script = CommandInvocation::Script("cargo test".to_string());
        assert!(is_validation(&script));

        let wrapped = argv("env", &["MODE=test", "bash", "-lc", "cargo test"]);
        assert!(is_validation(&wrapped));

        let powershell = CommandInvocation::PowerShellScript(
            "Invoke-Expression -Verbose -Command 'cargo test'".to_string(),
        );
        assert!(!is_validation(&powershell));

        let cmd_for = argv("cmd", &["/c", "for %i in (do) do cargo test"]);
        assert!(!is_validation(&cmd_for));

        let nested = CommandInvocation::Script("echo $(cargo test)".to_string());
        assert_eq!(
            classify_validation(&nested),
            ValidationClassification::Opaque
        );
    }

    #[test]
    fn deterministic_compound_script_cannot_bypass_test_denial() {
        let invocation =
            CommandInvocation::Script("echo preparing && cargo test --workspace".to_string());
        assert!(matches!(
            classify_validation(&invocation),
            ValidationClassification::Validation { ref leaves, .. }
                if leaves.len() == 1 && leaves[0].operation == ValidationOperation::Test
        ));

        let mut authorization = ValidationAuthorization::enabled();
        assert!(authorization.update_from_user_input("do not run tests"));
        assert!(prohibited_skip_for(&authorization, &invocation, false).is_some());

        assert_eq!(
            classify_validation(&CommandInvocation::Script(
                "echo 'cargo test && still text'".to_string(),
            )),
            ValidationClassification::NonValidation
        );
    }

    #[test]
    fn runner_flags_flow_through_operation_recognition() {
        for invocation in [
            argv("cargo", &["test", "--", "--ignored"]),
            argv("pytest", &["--maxfail=1", "tests/unit"]),
            argv("dotnet", &["test", "--blame-hang"]),
            argv("go", &["test", "-run", "Case", "./pkg"]),
        ] {
            assert!(is_validation(&invocation), "{invocation:?}");
        }
    }
}
