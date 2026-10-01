use crate::tools::handlers::command_shape::CommandInvocation;

pub(crate) use codex_shell_command::validation::ValidationClassification;
pub(crate) use codex_shell_command::validation::ValidationOperation;
use codex_shell_command::validation::classify_argv;
use codex_shell_command::validation::classify_powershell_script;
use codex_shell_command::validation::combine_validation_classifications;

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
            codex_shell_command::validation::script_prefers_long_observation_wait(script)
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
                assert!(
                    matches!(
                        classify_validation(&invocation),
                        ValidationClassification::Validation { ref leaves, .. }
                            if leaves.iter().any(|leaf| leaf.operation == ValidationOperation::Test)
                    ),
                    "{invocation:?}"
                );
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
    fn python_launchers_and_interpreter_options_recognize_tests() {
        for invocation in [
            argv("py", &["-m", "pytest", "-q"]),
            argv("py.exe", &["-m", "unittest", "discover"]),
            argv("python3.12", &["-I", "-m", "pytest"]),
            argv("python", &["-X", "dev", "-m", "pytest"]),
            argv("python", &["-Xdev", "-Werror", "-m", "unittest"]),
        ] {
            assert!(
                matches!(
                    classify_validation(&invocation),
                    ValidationClassification::Validation { ref leaves, .. }
                        if leaves.iter().any(|leaf| leaf.operation == ValidationOperation::Test)
                ),
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
    fn wrapper_target_with_validation_word_is_opaque() {
        let invocation = argv("make", &["format-report"]);
        assert_eq!(
            classify_validation(&invocation),
            ValidationClassification::Opaque
        );
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

        assert_eq!(
            classify_validation(&argv("cargo", &["--unknown-global", "test"])),
            ValidationClassification::Opaque
        );
    }

    #[test]
    fn parsed_pipeline_stages_preserve_validation_classification() {
        let invocations = [
            argv("cargo", &["test"]),
            argv("Select-Object", &["-First", "1"]),
        ];
        assert!(matches!(
            classify_validation_invocations(&invocations),
            ValidationClassification::Validation { ref leaves, .. }
                if leaves.len() == 1 && leaves[0].operation == ValidationOperation::Test
        ));
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

        for invocation in invocations {
            assert!(
                matches!(
                    classify_validation(&invocation),
                    ValidationClassification::Validation { ref leaves, .. }
                        if leaves.iter().any(|leaf| leaf.operation == ValidationOperation::Test)
                ),
                "{invocation:?}"
            );
        }
    }

    #[test]
    fn just_option_values_are_not_recipe_targets() {
        for invocation in [
            argv("just", &["--shell", "bash", "test"]),
            argv("just", &["--color", "always", "test"]),
            argv("just", &["--set", "MODE", "test", "test"]),
        ] {
            assert!(
                matches!(
                    classify_validation(&invocation),
                    ValidationClassification::Validation { ref leaves, .. }
                        if leaves.len() == 1 && leaves[0].operation == ValidationOperation::Test
                ),
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
    fn deterministic_compound_script_recognizes_tests() {
        let invocation =
            CommandInvocation::Script("echo preparing && cargo test --workspace".to_string());
        assert!(matches!(
            classify_validation(&invocation),
            ValidationClassification::Validation { ref leaves, .. }
                if leaves.len() == 1 && leaves[0].operation == ValidationOperation::Test
        ));

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
