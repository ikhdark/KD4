use std::sync::Arc;

use serde::Serialize;
use tokio::sync::RwLock;

use crate::tools::handlers::command_shape::CommandInvocation;

pub(crate) use codex_shell_command::validation::ValidationClassification;
pub(crate) use codex_shell_command::validation::ValidationOperation;
use codex_shell_command::validation::classify_argv;
use codex_shell_command::validation::classify_powershell_script;
use codex_shell_command::validation::combine_validation_classifications;

const ALL_OPERATIONS: [ValidationOperation; 5] = [
    ValidationOperation::Test,
    ValidationOperation::Check,
    ValidationOperation::Lint,
    ValidationOperation::Bench,
    ValidationOperation::Fuzz,
];

#[derive(Debug, Default)]
pub(crate) struct ValidationAuthorization {
    // Production turn construction intentionally leaves this inactive. Only tests
    // opt into classification; its presence is not evidence of live enforcement.
    enabled: bool,
    pub(crate) revision: u64,
    denied: [bool; 5],
}

pub(crate) type SharedValidationAuthorization = Arc<RwLock<ValidationAuthorization>>;

impl ValidationAuthorization {
    #[cfg(test)]
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
    let actionable_text = actionable_directive_text(text);
    let mut normalized = actionable_text
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

fn actionable_directive_text(text: &str) -> String {
    let mut actionable = String::with_capacity(text.len());
    let mut fence: Option<&str> = None;

    for line in text.lines() {
        let trimmed = line.trim_start();
        if let Some(marker) = fence {
            if trimmed.starts_with(marker) {
                fence = None;
            }
            actionable.push('\n');
            continue;
        }
        if trimmed.starts_with("```") {
            fence = Some("```");
            actionable.push('\n');
            continue;
        }
        if trimmed.starts_with("~~~") {
            fence = Some("~~~");
            actionable.push('\n');
            continue;
        }
        if trimmed.starts_with('>') || line.starts_with("    ") || line.starts_with('\t') {
            actionable.push('\n');
            continue;
        }

        let mut closing_quote = None;
        let mut preceding_backslashes = 0_usize;
        for character in line.chars() {
            if let Some(closing) = closing_quote {
                if character == closing
                    && (closing != '"' || preceding_backslashes.is_multiple_of(2))
                {
                    closing_quote = None;
                }
                preceding_backslashes = if character == '\\' {
                    preceding_backslashes.saturating_add(1)
                } else {
                    0
                };
                continue;
            }
            closing_quote = match character {
                '"' => Some('"'),
                '\u{201c}' => Some('\u{201d}'),
                '`' => Some('`'),
                _ => None,
            };
            preceding_backslashes = 0;
            if closing_quote.is_none() {
                actionable.push(character);
            }
        }
        actionable.push('\n');
    }

    actionable
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
        let Some(body) = skip_validation_body(body) else {
            return Vec::new();
        };
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

fn skip_validation_body(body: &str) -> Option<&str> {
    let first_target = body.split_once(" and ").map_or(body, |(first, _)| first);
    let first_target = first_target
        .split_once(',')
        .map_or(first_target, |(first, _)| first);
    (!operations_from_instruction(first_target).is_empty()).then_some(body)
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
            ..
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
    // Classification also drives execution diagnostics. Only denial enforcement
    // is disabled when production validation authorization is inactive.
    let classification = classify_validation_invocations(invocations);
    let authorization = authorization.read().await;
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
        CommandInvocation::Script(script) => {
            codex_shell_command::validation::classify_script(script)
        }
        CommandInvocation::PowerShellScript(script) => classify_powershell_script(script),
    }
}

pub(crate) fn classify_validation_script(script: &str) -> ValidationClassification {
    #[cfg(test)]
    VALIDATION_CLASSIFICATION_COUNT.with(|count| count.set(count.get() + 1));
    codex_shell_command::validation::classify_script(script)
}

/// Observation policy, not evidence that tests executed. Build and discovery
/// commands can compile for minutes without proving a validation contract.
pub(crate) fn prefers_long_observation_wait(invocation: &CommandInvocation) -> bool {
    if matches!(
        classify_validation(invocation),
        ValidationClassification::Validation { .. }
    ) {
        return true;
    }
    match invocation {
        CommandInvocation::Argv { program, args } => {
            codex_shell_command::validation::is_build_or_discovery(program, args)
        }
        CommandInvocation::Script(script) | CommandInvocation::PowerShellScript(script) => {
            codex_shell_command::validation::script_has_build_or_discovery(script)
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

#[cfg(test)]
mod tests {
    use super::*;
    use codex_shell_command::validation::ValidationCommandDescriptor;

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
    fn skipping_an_unrelated_step_does_not_deny_later_validation() {
        let mut authorization = ValidationAuthorization::enabled();

        assert!(!authorization.update_from_user_input("Skip the intro and check the tests"));
        assert!(prohibited_skip_for(&authorization, &argv("cargo", &["check"]), false).is_none());
        assert!(prohibited_skip_for(&authorization, &argv("cargo", &["test"]), false).is_none());

        assert!(authorization.update_from_user_input("skip the unit tests and lint"));
        assert!(prohibited_skip_for(&authorization, &argv("cargo", &["test"]), false).is_some());
        assert!(prohibited_skip_for(&authorization, &argv("cargo", &["clippy"]), false).is_some());
    }

    #[test]
    fn quoted_or_pasted_validation_text_does_not_change_authorization() {
        for text in [
            "The phrase \u{201c}do not run tests\u{201d} is being discussed.",
            "The phrase `do not run tests` is being discussed.",
            "> do not run tests\nThat was the previous instruction.",
            "```text\ndo not run tests\n```\nThat was the previous instruction.",
            "    do not run tests\nThat was pasted output.",
            r#""\"do not run tests\"""#,
            r#"{"message":"The phrase \"do not run tests\" is data."}"#,
        ] {
            let mut authorization = ValidationAuthorization::enabled();
            assert!(!authorization.update_from_user_input(text), "{text:?}");
            assert!(
                prohibited_skip_for(&authorization, &argv("cargo", &["test"]), false).is_none(),
                "{text:?}"
            );
        }

        let mut authorization = ValidationAuthorization::enabled();
        assert!(authorization.update_from_user_input(
            "The old note said \u{201c}run tests\u{201d}. Do not run tests."
        ));
        assert!(prohibited_skip_for(&authorization, &argv("cargo", &["test"]), false).is_some());
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
    async fn production_turn_keeps_validation_classification_without_enforcement() {
        let (_session, turn) = crate::session::tests::make_session_and_context().await;
        turn.update_validation_authorization(&[codex_protocol::user_input::UserInput::Text {
            text: "do not run tests".to_string(),
            text_elements: Vec::new(),
        }])
        .await;
        reset_validation_classification_count();
        assert!(matches!(
            admit_validation(
                &turn.validation_authorization,
                &argv("cargo", &["test"]),
                true
            )
            .await,
            ValidationAdmission::Execute {
                is_validation: true,
                authorization_revision: 0,
                classification: ValidationClassification::Validation { .. },
            }
        ));
        assert_eq!(validation_classification_count(), 1);
        for (invocation, tagged, expected) in [
            (argv("cargo", &["test"]), false, true),
            (argv("custom-runner", &["verify"]), true, true),
            (argv("git", &["status"]), false, false),
        ] {
            let ValidationAdmission::Execute { is_validation, .. } =
                admit_validation(&turn.validation_authorization, &invocation, tagged).await
            else {
                panic!("inactive authorization cannot deny a command");
            };
            assert_eq!(is_validation, expected);
        }
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

    mod manifest_runner {
        use super::*;

        #[test]
        fn manifest_validation_runner_recognizes_only_execution() {
            for invocation in [
                argv("python", &["scripts/validate.py", "run", "atomic-write"]),
                argv(
                    "python",
                    &[
                        "-I",
                        "scripts/validate.py",
                        "run",
                        "--changed",
                        "src/lib.rs",
                    ],
                ),
                argv("py", &["scripts/validate.py", "run"]),
                CommandInvocation::Script(
                    concat!(
                        "python scripts/validate.py plan atomic-write; ",
                        "python scripts/validate.py run atomic-write",
                    )
                    .to_string(),
                ),
            ] {
                assert!(is_validation(&invocation), "{invocation:?}");
                let mut authorization = ValidationAuthorization::enabled();
                assert!(authorization.update_from_user_input("do not run tests"));
                assert!(prohibited_skip_for(&authorization, &invocation, false).is_some());
            }
            for args in [
                vec!["scripts/validate.py"],
                vec!["scripts/validate.py", "plan", "atomic-write"],
                vec!["scripts/validate.py", "list"],
                vec!["scripts/validate.py", "--help"],
                vec!["scripts/validate.py", "run", "--help"],
                vec!["scripts/not_validate.py", "run"],
                vec!["script.py", "scripts/validate.py", "run"],
            ] {
                assert!(!is_validation(&argv("python", &args)), "{args:?}");
            }
        }

        #[test]
        fn guarded_manifest_runs_from_rollout_are_validation() {
            // Exact `exec_command` scripts a recorded session sent.
            let guarded_runs = [
                concat!(
                    "python scripts/validate.py plan validation-tooling; ",
                    "if ($LASTEXITCODE -eq 0) { python scripts/validate.py run validation-tooling }; ",
                    "exit $LASTEXITCODE",
                ),
                concat!(
                    "python scripts/validate.py plan --changed src/atomic_write.rs; ",
                    "if ($LASTEXITCODE -eq 0) { python scripts/validate.py run --changed src/atomic_write.rs }; ",
                    "exit $LASTEXITCODE",
                ),
                concat!(
                    "python scripts/validate.py run --changed src/atomic_write.rs; ",
                    "if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }; ",
                    "rg -n '^mod tests|^    mod tests' src/config/mod.rs src/workspace.rs src/test_plan.rs ",
                    "src/validation_identity.rs src/execution_control.rs src/analysis_jobs.rs; ",
                    "git diff --check -- .cargo/config.toml .config/nextest.toml ",
                    "scripts/check_test_targets.py AGENTS.md; exit $LASTEXITCODE",
                ),
                concat!(
                    "python scripts/validate.py plan validation-tooling atomic-write; ",
                    "if ($LASTEXITCODE -eq 0) { python scripts/validate.py run validation-tooling atomic-write }; ",
                    "exit $LASTEXITCODE",
                ),
            ];
            let mut authorization = ValidationAuthorization::enabled();
            assert!(authorization.update_from_user_input("do not run tests"));
            for script in guarded_runs {
                let invocation = CommandInvocation::Script(script.to_string());
                assert!(
                    matches!(
                        classify_validation(&invocation),
                        ValidationClassification::Validation {
                            ref leaves,
                            exit_code_is_authoritative: false,
                            ..
                        } if leaves.iter().all(|leaf| leaf.operation == ValidationOperation::Test)
                    ),
                    "{script}"
                );
                assert!(
                    prohibited_skip_for(&authorization, &invocation, false).is_some(),
                    "{script}"
                );
            }
            for script in [
                concat!(
                    "python scripts/validate.py plan --changed src/validation_identity.rs ",
                    "--changed tests/config_runtime_contracts.rs --changed src/atomic_write.rs",
                ),
                "if ($LASTEXITCODE -eq 0) { python scripts/validate.py plan atomic-write }",
            ] {
                assert_eq!(
                    classify_validation(&CommandInvocation::Script(script.to_string())),
                    ValidationClassification::NonValidation,
                    "{script}"
                );
            }
        }

        #[test]
        fn powershell_if_blocks_keep_their_commands_together() {
            for script in [
                "if ($LASTEXITCODE -eq 0) { python scripts/validate.py plan a; python scripts/validate.py run a }",
                "if (Test-Path Cargo.toml) { echo skip } elseif ($env:CI) { echo ci } else { cargo test }",
                "If($ok){cargo test}",
            ] {
                assert!(
                    is_validation(&CommandInvocation::Script(script.to_string())),
                    "{script}"
                );
            }
            // An unclosed block leaves the command boundaries unknowable.
            assert_eq!(
                classify_validation(&CommandInvocation::Script(
                    "if ($ok) { cargo test".to_string()
                )),
                ValidationClassification::Opaque
            );
        }
    }

    #[test]
    fn python_launchers_and_interpreter_options_preserve_test_denials() {
        let mut authorization = ValidationAuthorization::enabled();
        assert!(authorization.update_from_user_input("do not run tests"));

        for invocation in [
            argv("py", &["-m", "pytest", "-q"]),
            argv("py.exe", &["-m", "unittest", "discover"]),
            argv("python3.12", &["-I", "-m", "pytest"]),
            argv("python", &["-X", "dev", "-m", "pytest"]),
            argv("python", &["-Xdev", "-Werror", "-m", "unittest"]),
        ] {
            assert!(is_validation(&invocation), "{invocation:?}");
            assert!(
                prohibited_skip_for(&authorization, &invocation, false).is_some(),
                "{invocation:?}"
            );
        }
    }

    #[test]
    fn python_module_markers_after_a_script_or_double_dash_are_not_tests() {
        for invocation in [
            argv("python", &["script.py", "-m", "pytest"]),
            argv("python", &["--", "-m", "pytest"]),
            argv("python", &["-c", "print('pytest')", "-m", "pytest"]),
        ] {
            assert!(!is_validation(&invocation), "{invocation:?}");
        }
    }

    #[test]
    fn wrapper_target_with_validation_word_is_not_suppressed() {
        let invocation = argv("make", &["format-report"]);
        assert_eq!(
            classify_validation(&invocation),
            ValidationClassification::Opaque
        );

        let mut authorization = ValidationAuthorization::enabled();
        assert!(authorization.update_from_user_input("do not run lint"));
        assert!(prohibited_skip_for(&authorization, &invocation, false).is_none());
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

        let powershell_pipeline = CommandInvocation::PowerShellScript(
            "cargo test -p codex-core | Select-Object -First 1".to_string(),
        );
        assert!(is_validation(&powershell_pipeline));

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
    fn incomplete_yarn_workspace_arguments_do_not_panic_or_invent_validation() {
        for args in [
            vec!["workspace"],
            vec!["--cwd", "web", "workspace"],
            vec!["workspace", "web"],
            vec!["workspace", "web", "run"],
        ] {
            assert_eq!(
                classify_validation(&argv("yarn", &args)),
                ValidationClassification::NonValidation,
            );
        }
        assert_eq!(
            classify_validation(&argv("yarn", &["workspace", "web", "run", "check"])),
            ValidationClassification::Validation {
                leaves: vec![ValidationCommandDescriptor {
                    operation: ValidationOperation::Check,
                }],
                has_unclassified_targets: false,
                exit_code_is_authoritative: true,
            },
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
