use super::*;

fn runners() -> Vec<RepositoryRunner> {
    let config: serde_json::Value =
        serde_json::from_str(include_str!("../../../.codex/test-runners.json")).unwrap();
    serde_json::from_value(config["runners"].clone()).unwrap()
}

#[test]
fn documented_just_wait_preserves_classification_and_receipt_identity() {
    let runners = runners();
    for recipe in [
        "core-gate",
        "core-test",
        "core-test-fast",
        "core-test-small",
        "core-test-lane",
    ] {
        let plain = [recipe, "fixture"].map(str::to_owned);
        let expected = classify_argv_with_runners("just", &plain, &runners);
        for wait in ["0", "5", "0.25"] {
            let args = [
                "--set",
                "rust_validation_wait_seconds",
                wait,
                recipe,
                "fixture",
            ]
            .map(str::to_owned);
            assert_eq!(
                classify_argv_with_runners("just", &args, &runners),
                expected
            );
            let matching = runners
                .iter()
                .find(|runner| runner.matches("just", &args))
                .unwrap();
            assert_eq!(matching.receipt_runner.as_deref(), Some("rust_test_runner"));
        }
    }
}

#[test]
fn documented_nextest_filter_after_the_recipe_keeps_the_trusted_runner() {
    // Just passes words after the recipe to it; AGENTS.md documents this form.
    let runners = runners();
    for args in [
        vec!["core-test-fast", "core_lib", "-E", "test(parser)"],
        vec![
            "--set",
            "rust_validation_wait_seconds",
            "5",
            "core-test-fast",
            "core_lib",
            "-E",
            "test(parser)",
        ],
    ] {
        let args = args.into_iter().map(str::to_owned).collect::<Vec<_>>();
        let matching = runners.iter().find(|runner| runner.matches("just", &args));
        assert_eq!(
            matching.and_then(|runner| runner.receipt_runner.as_deref()),
            Some("rust_test_runner"),
            "{args:?}"
        );
    }
}

#[test]
fn just_overrides_cannot_authenticate_another_producer_or_nonexecution() {
    let runners = runners();
    for prefix in [
        vec!["--set", "shell", "evil"],
        vec!["--set", "rust_validation_wait_seconds", "NaN"],
        vec!["--set", "rust_validation_wait_seconds", "inf"],
        vec!["--set", "rust_validation_wait_seconds", "-1"],
        vec!["--set", "rust_validation_wait_seconds", "5;exit 0"],
        vec!["--set", "rust_validation_wait_seconds"],
        vec!["--justfile", "foreign.just"],
        vec!["--justfile=foreign.just"],
        vec!["-fforeign.just"],
        vec!["--working-directory", "elsewhere"],
        vec!["--dry-run"],
    ] {
        let args = prefix
            .iter()
            .copied()
            .chain(["core-gate", "fixture"])
            .map(str::to_owned)
            .collect::<Vec<_>>();
        assert!(
            !runners.iter().any(|runner| runner.matches("just", &args)),
            "{args:?}"
        );
    }
    for suffix in [
        vec!["--justfile", "foreign.just"],
        vec!["--justfile-name=foreign.just"],
        vec!["-fforeign.just"],
        vec!["-delsewhere"],
        vec!["-g"],
        vec!["--shell", "evil"],
        vec!["--dotenv-command", "evil"],
        vec!["--dry-run"],
        vec!["--show"],
        vec!["--summary"],
        vec!["--set", "shell", "evil"],
        vec!["shell=evil"],
    ] {
        let args = [
            "--set",
            "rust_validation_wait_seconds",
            "5",
            "core-gate",
            "fixture",
        ]
        .into_iter()
        .chain(suffix)
        .map(str::to_owned)
        .collect::<Vec<_>>();
        assert!(
            !runners.iter().any(|runner| runner.matches("just", &args)),
            "{args:?}"
        );
    }
}

#[test]
fn rustup_run_classifies_the_child_not_the_toolchain() {
    for args in [
        vec!["run", "stable", "cargo", "test", "one_test"],
        vec!["run", "--install", "stable", "cargo", "test", "one_test"],
        vec![
            "--quiet", "run", "stable", "--", "cargo", "test", "one_test",
        ],
    ] {
        let args = args.into_iter().map(str::to_owned).collect::<Vec<_>>();
        assert_eq!(
            classify_argv("rustup", &args),
            classify_argv("cargo", &["test".into(), "one_test".into()])
        );
    }
    for args in [vec!["run"], vec!["run", "stable"], vec!["run", "--help"]] {
        let args = args.into_iter().map(str::to_owned).collect::<Vec<_>>();
        assert_eq!(
            decode_execution_wrapper("rustup", &args),
            ExecutionWrapper::Opaque
        );
    }
}
