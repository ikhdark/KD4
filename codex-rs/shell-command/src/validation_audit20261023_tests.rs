use super::*;
use codex_protocol::parse_command::ParsedCommand;

fn args(values: &[&str]) -> Vec<String> { values.iter().map(|value| (*value).into()).collect() }

fn authoritative(classification: &ValidationClassification) -> bool {
    matches!(classification, ValidationClassification::Validation { exit_code_is_authoritative: true, .. })
}

#[test]
fn audit20261023_powershell_separators_and_legacy_groups_cannot_certify_success() {
    for script in [
        "cargo test --manifest-path missing.toml\\; Write-Output ok",
        "cargo test --manifest-path missing.toml\\\nWrite-Output ok",
        "(cargo test --manifest-path missing.toml)",
    ] {
        assert!(!authoritative(&classify_powershell_script(script)), "{script}");
        let command = args(&["-NoProfile", "-Command", script]);
        assert!(!authoritative(&classify_argv_with_runners("powershell", &command, &[])), "{script}");
    }
    assert!(authoritative(&classify_powershell_script("cargo test")));
}

#[test]
fn audit20261023_maven_failure_suppression_is_not_authoritative() {
    for binary in ["mvn", "mvnw"] {
        for command in [["test", "-fn"], ["--fail-never", "test"]] {
            let result = classify_argv_with_runners(binary, &args(&command), &[]);
            assert!(!authoritative(&result), "{binary} {command:?}");
            assert!(matches!(result, ValidationClassification::Validation { ref leaves, .. }
                if leaves.iter().any(|leaf| leaf.operation == ValidationOperation::Test && leaf.mode == ValidationExecutionMode::Execution)));
        }
        for command in [vec!["test"], vec!["-f", "--fail-never", "test"], vec!["-Dmessage=--fail-never", "test"]] {
            assert!(authoritative(&classify_argv_with_runners(binary, &args(&command), &[])), "{binary} {command:?}");
        }
    }
}

#[test]
fn audit20261023_listing_aliases_never_prove_test_body_execution() {
    for (binary, command) in [
        ("dotnet", vec!["test", "-t"]), ("dotnet", vec!["test", "--help"]),
        ("mvn", vec!["test", "--help"]), ("mvn", vec!["test", "--version"]),
        ("go", vec!["test", "-test.list=."]), ("go", vec!["test", "-args", "-h"]),
    ] {
        let result = classify_argv_with_runners(binary, &args(&command), &[]);
        assert!(!matches!(result, ValidationClassification::Validation { ref leaves, .. }
            if leaves.iter().any(|leaf| leaf.operation == ValidationOperation::Test && leaf.mode == ValidationExecutionMode::Execution)), "{binary} {command:?}: {result:?}");
    }
    for (binary, command) in [("dotnet", vec!["test"]), ("mvn", vec!["test", "-V"]), ("go", vec!["test", "./..."])] {
        assert!(authoritative(&classify_argv_with_runners(binary, &args(&command), &[])));
    }
}

#[test]
fn audit20261023_just_numeric_overrides_keep_child_argument_boundaries() {
    let runner: RepositoryRunner = serde_json::from_value(serde_json::json!({
        "programs": ["just"], "prefixes": [["cargo-lane"]], "allow_extra_args": true,
        "just_numeric_variables": ["jobs", "threads"], "passthrough_args": 2
    })).unwrap();
    let expected = classify_argv_with_runners("cargo", &args(&["test"]), &[]);
    for overrides in [vec![], vec!["jobs=1"], vec!["jobs=1", "threads=2"]] {
        let mut command = overrides;
        command.extend(["cargo-lane", "core", "cargo", "test"]);
        assert_eq!(classify_argv_with_runners("just", &args(&command), std::slice::from_ref(&runner)), expected);
    }
    for invalid in ["jobs=NaN", "jobs=-1", "unknown=1"] {
        assert!(!authoritative(&classify_argv_with_runners("just", &args(&[invalid, "cargo-lane", "core", "cargo", "test"]), std::slice::from_ref(&runner))));
    }
}

#[test]
fn audit20261023_mutating_exploration_forms_remain_unknown() {
    for command in [
        vec!["tree", "-o", "report"], vec!["tree", "-oreport"], vec!["tree", "-ao", "report"],
        vec!["git", "grep", "-O", "needle"], vec!["git", "grep", "-Ocat", "needle"],
        vec!["git", "grep", "--open-files-in-pager=cat", "needle"],
    ] {
        let command = args(&command);
        assert_eq!(crate::parse_command::parse_command(&command), vec![ParsedCommand::Unknown { cmd: crate::parse_command::shlex_join(&command) }]);
    }
    for command in [vec!["tree", "--", "-o"], vec!["tree", "-P", "-o"], vec!["git", "grep", "-e", "-O"], vec!["git", "grep", "needle", "--", "-O"]] {
        assert!(!crate::parse_command::parse_command(&args(&command)).iter().any(|command| matches!(command, ParsedCommand::Unknown { .. })), "{command:?}");
    }
    for script in ["tree -o report", "git grep -Ocat needle"] {
        assert!(crate::parse_command::parse_command(&args(&["bash", "-lc", script])).iter().any(|command| matches!(command, ParsedCommand::Unknown { .. })));
    }
}

#[test]
fn audit20261023_zsh_equals_expansion_is_not_a_literal_read() {
    let expanded = crate::parse_command::parse_command(&args(&["zsh", "-lc", "cat =git"]));
    assert!(expanded.iter().any(|command| matches!(command, ParsedCommand::Unknown { .. })));
    let quoted = crate::parse_command::parse_command(&args(&["zsh", "-lc", "cat '=git'"]));
    assert!(quoted.iter().any(|command| matches!(command, ParsedCommand::Read { .. })));
}
