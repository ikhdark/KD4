use super::*;

fn classify(program: &str, args: &[&str]) -> ValidationClassification {
    classify_argv(program, &args.iter().map(|arg| (*arg).into()).collect::<Vec<_>>())
}

fn leaves(classification: ValidationClassification) -> Vec<ValidationCommandDescriptor> {
    let ValidationClassification::Validation { leaves, .. } = classification else {
        panic!("expected validation")
    };
    leaves
}

fn recipe(args: &[&str]) -> ValidationClassification {
    let catalog: serde_json::Value = serde_json::from_str(include_str!("../../../.codex/test-runners.json")).unwrap();
    let runners: Vec<RepositoryRunner> = serde_json::from_value(catalog["runners"].clone()).unwrap();
    classify_argv_with_runners("just", &args.iter().map(|arg| (*arg).into()).collect::<Vec<_>>(), &runners)
}

#[test]
fn argument_values_cannot_disable_test_capabilities() {
    for value in ["--no-run", "--list", "--dry-run", "--help"] {
        let descriptors = leaves(classify("cargo", &["test", "--", "--skip", value]));
        assert_eq!(descriptors[0].mode, ValidationExecutionMode::Execution, "{value}");
    }
    assert_eq!(leaves(classify("cargo", &["test", "--no-run"]))[0].mode, ValidationExecutionMode::CompilationOnly);
    assert_eq!(leaves(classify("cargo", &["test", "--", "--list"]))[0].mode, ValidationExecutionMode::Listing);
    assert_eq!(leaves(classify("npm", &["test", "--dry-run"]))[0].mode, ValidationExecutionMode::Execution);
    assert_eq!(leaves(classify("npm", &["run", "check", "--if-present"]))[0].mode, ValidationExecutionMode::Unknown);
}

#[test]
fn bench_harness_and_check_alias_preserve_capabilities() {
    let descriptors = leaves(classify("cargo", &["bench", "--", "--test"]));
    assert!(descriptors.iter().any(|leaf| leaf.operation == ValidationOperation::Test && leaf.mode == ValidationExecutionMode::Execution));
    assert!(leaves(classify("cargo", &["bench", "--", "--skip", "--test"])).iter().all(|leaf| leaf.operation != ValidationOperation::Test));
    assert_eq!(classify("cargo", &["c", "--tests"]), classify("cargo", &["check", "--tests"]));
}

#[test]
fn pytest_inspection_and_setup_are_not_body_execution_proof() {
    for program in ["pytest", "python"] {
        for flag in ["--collectonly", "--collect-only", "--setup-plan", "--fixtures"] {
            let args = if program == "python" { vec!["-I", "-m", "pytest", flag] } else { vec![flag] };
            assert_eq!(leaves(classify(program, &args))[0].mode, ValidationExecutionMode::Listing);
        }
    }
    assert_eq!(leaves(classify("pytest", &["--setup-only"]))[0].mode, ValidationExecutionMode::Unknown);
    assert_eq!(leaves(classify("pytest", &["-k", "--setup-only"]))[0].mode, ValidationExecutionMode::Execution);
    assert_eq!(leaves(classify("pytest", &["-q"]))[0].mode, ValidationExecutionMode::Execution);
}

#[test]
fn first_party_recipes_describe_children_and_keep_recipe_boundaries() {
    for name in ["config-schema-check", "app-server-schema-check", "sdk-python-check", "sdk-ts-check", "test-release-tooling"] {
        assert!(leaves(recipe(&[name])).iter().any(|leaf| leaf.operation == ValidationOperation::Test && leaf.mode == ValidationExecutionMode::Execution), "{name}");
    }
    let mixed = leaves(recipe(&["config-schema-protocol-check", "core-test-fast", "core_lib", "--no-run"]));
    assert!(mixed.iter().any(|leaf| leaf.operation == ValidationOperation::Test && leaf.mode == ValidationExecutionMode::Execution));
    assert!(mixed.iter().any(|leaf| leaf.mode == ValidationExecutionMode::CompilationOnly));
    assert_eq!(leaves(recipe(&["fmt-check-fast", "core-test-fast", "core_lib"])).len(), 1);
    assert!(leaves(recipe(&["check-lane", "codex-core", "--tests"])).iter().all(|leaf| leaf.mode == ValidationExecutionMode::Checking));
    assert!(leaves(recipe(&["test-compile"])).iter().all(|leaf| leaf.mode == ValidationExecutionMode::CompilationOnly));
    assert_eq!(leaves(recipe(&["cargo-lane", "core-tests", "cargo", "nextest", "run"]))[0].mode, ValidationExecutionMode::Execution);
    assert_eq!(leaves(recipe(&["cargo-lane", "core-tests", "cargo", "test", "--no-run"]))[0].mode, ValidationExecutionMode::CompilationOnly);
}

#[test]
fn audit148_lane_and_gate_recipes_authenticate_their_admission_producer() {
    let catalog: serde_json::Value = serde_json::from_str(include_str!("../../../.codex/test-runners.json")).unwrap();
    let runners: Vec<RepositoryRunner> = serde_json::from_value(catalog["runners"].clone()).unwrap();
    // Command analysis consults the first matching declaration.
    let admission = |args: &[&str]| {
        let args = args.iter().map(|arg| (*arg).to_string()).collect::<Vec<_>>();
        runners.iter().find(|runner| runner.matches("just", &args)).map(|runner| runner.admission_runner.clone())
    };
    // Whole recipe bodies are one run-lane or core-gate admission; busy exit 75 means nothing ran.
    for (args, producer) in [
        (&["clippy-lane", "codex-core"][..], "rust_build_status"),
        (&["test-fast", "-p", "codex-core"][..], "rust_build_status"),
        (&["test-lane", "audit148", "-p", "codex-core"][..], "rust_build_status"),
        (&["test-lane-main", "-p", "codex-core"][..], "rust_build_status"),
        (&["test-lane-fast", "audit148", "-p", "codex-core"][..], "rust_build_status"),
        (&["test-lane-package", "codex-core"][..], "rust_build_status"),
        (&["hooks-schema-check"][..], "rust_build_status"),
        (&["test-slow-boundaries"][..], "rust_test_runner"),
        (&["config-schema-protocol-check"][..], "rust_test_runner"),
        (&["app-server-runtime-check"][..], "rust_test_runner"),
    ] {
        assert_eq!(admission(args), Some(Some(producer.to_string())), "{args:?}");
    }
    // Formatter recipes and unverified wrappers cannot authenticate a busy report.
    for args in [
        &["fmt-check"][..],
        &["fmt-check-fast"][..],
        &["test-fast-nosccache", "-p", "codex-core"][..],
        &["validate-crate-focused", "codex-core"][..],
    ] {
        assert_eq!(admission(args), Some(None), "{args:?}");
    }
    assert!(leaves(recipe(&["clippy-lane", "codex-core"])).iter().all(|leaf| leaf.operation == ValidationOperation::Lint));
}
