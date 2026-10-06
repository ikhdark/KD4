use crate::tools::handlers::command_shape::CommandInvocation;

pub(crate) use codex_shell_command::validation::ValidationClassification;
pub(crate) use codex_shell_command::validation::ValidationOperation;
use codex_shell_command::validation::classify_argv;
use codex_shell_command::validation::classify_powershell_script;
use codex_shell_command::validation::combine_validation_classifications;

/// Launch-time metadata, retained with the process through polling. Repository
/// trust is never deserialized from model arguments or inferred from stdout.
#[derive(Debug, Clone, PartialEq)]
pub struct CommandValidation {
    pub(crate) declared: Option<codex_protocol::validation::ValidationCommandContext>,
    pub(crate) classification: ValidationClassification,
    pub(crate) receipt_runner: Option<String>,
}

impl CommandValidation {
    pub(crate) fn is_validation(&self) -> bool {
        matches!(self.classification, ValidationClassification::Validation { .. })
    }

    pub(crate) fn is_test(&self) -> bool {
        matches!(&self.classification, ValidationClassification::Validation { leaves, .. }
            if leaves.iter().any(|leaf| leaf.operation == ValidationOperation::Test))
    }

    pub(crate) fn signal(&self) -> serde_json::Value {
        serde_json::json!({
            "validation": self.is_validation(),
            "proof": matches!(self.classification, ValidationClassification::Validation {
                exit_code_is_authoritative: true, has_unclassified_targets: false, ..
            }),
            "tests": self.is_test(),
        })
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RepositoryRunners {
    version: u32,
    runners: Vec<codex_shell_command::validation::RepositoryRunner>,
}

fn repository_runners(cwd: &std::path::Path) -> Vec<codex_shell_command::validation::RepositoryRunner> {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::{Mutex, OnceLock};
    type Runners = Vec<codex_shell_command::validation::RepositoryRunner>;
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, (String, Runners)>>> = OnceLock::new();

    let Some(root) = codex_git_utils::get_git_repo_root(cwd) else { return Vec::new() };
    let Some(oid) = repository_head_oid(&root) else { return Vec::new() };
    // This function runs on the blocking analysis pool. Coalesce concurrent
    // misses; never retain cwd-specific path matching in the shared cache.
    let mut cache = CACHE.get_or_init(Default::default).lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((cached_oid, runners)) = cache.get(&root)
        && cached_oid == &oid
    {
        let mut runners = runners.clone();
        for runner in &mut runners {
            runner.path_context = Some((root.clone(), cwd.to_path_buf()));
        }
        return runners;
    }
    // HEAD, not the index or working tree: writing a declaration during a turn
    // must not let an arbitrary echo authenticate a fabricated execution ledger.
    // Read the exact observed commit so a concurrent HEAD move cannot poison
    // this entry. Replacement refs must not change content at a cached oid.
    let Ok(output) = std::process::Command::new(codex_git_utils::git_executable())
        .arg("--no-replace-objects").arg("-C").arg(&root)
        .arg("show").arg(format!("{oid}:.codex/test-runners.json")).output()
    else { return Vec::new() };
    if !output.status.success() || output.stdout.len() > 64 * 1024 {
        return Vec::new();
    }
    let Ok(mut config) = serde_json::from_slice::<RepositoryRunners>(&output.stdout)
    else { return Vec::new() };
    if config.version != 1 || config.runners.len() > 256
        || config.runners.iter().any(|runner| runner.options.values().any(|count| *count > 2))
        || config.runners.iter().any(|runner| runner.passthrough_after.as_ref()
            .is_some_and(|separator| separator.is_empty() || runner.receipt_runner.is_some()))
    {
        return Vec::new();
    }
    if cache.len() >= 64 {
        cache.clear();
    }
    cache.insert(root.clone(), (oid, config.runners.clone()));
    for runner in &mut config.runners {
        runner.path_context = Some((root.clone(), cwd.to_path_buf()));
    }
    config.runners
}

fn repository_head_oid(root: &std::path::Path) -> Option<String> {
    let root = codex_utils_absolute_path::AbsolutePathBuf::try_from(root).ok()?;
    let (git_dir, common_dir, _) = crate::git_workspace::resolve_git_dirs(&root)?;
    let mut value = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    // HEAD often contains a stable symbolic ref across commits. Read its target,
    // including packed refs and linked-worktree common dirs, on every lookup.
    for _ in 0..8 {
        let Some(reference) = value.trim().strip_prefix("ref:").map(str::trim) else {
            let oid = value.trim();
            return ((oid.len() == 40 || oid.len() == 64)
                && oid.bytes().all(|byte| byte.is_ascii_hexdigit())).then(|| oid.to_string());
        };
        if let Ok(next) = std::fs::read_to_string(common_dir.join(reference)) {
            value = next;
            continue;
        }
        if let Ok(packed) = std::fs::read_to_string(common_dir.join("packed-refs"))
            && let Some(oid) = packed.lines().find_map(|line| {
                let (oid, name) = line.split_once(' ')?;
                (name == reference).then(|| oid.to_string())
            })
        {
            value = oid;
            continue;
        }
        // Unborn branches and alternate ref storage (e.g. reftable) remain
        // Git-owned. Ordinary loose/packed refs need no subprocess here.
        let output = std::process::Command::new(codex_git_utils::git_executable()).arg("-C").arg(root.as_path())
            .args(["rev-parse", "--verify", "HEAD"]).output().ok()?;
        if !output.status.success() { return None; }
        value = String::from_utf8(output.stdout).ok()?;
    }
    None
}

fn classify_with_runners(
    invocation: &CommandInvocation,
    runners: &[codex_shell_command::validation::RepositoryRunner],
) -> ValidationClassification {
    use codex_shell_command::validation as commands;
    match invocation {
        CommandInvocation::Argv { program, args } => commands::classify_argv_with_runners(program, args, runners),
        CommandInvocation::Script(script) => commands::classify_script_with_runners(script, runners),
        CommandInvocation::PowerShellScript(script) => commands::classify_powershell_script_with_runners(script, runners),
    }
}

pub(crate) async fn resolve_command_validation(
    invocation: &CommandInvocation,
    cwd: Option<&std::path::Path>,
    declared: Option<codex_protocol::validation::ValidationCommandContext>,
) -> Option<CommandValidation> {
    let invocation = invocation.clone();
    let cwd = cwd.map(std::path::Path::to_path_buf);
    crate::tools::run_blocking_command_analysis(move || {
        let runners = cwd.as_deref().map(repository_runners).unwrap_or_default();
        let classification = classify_with_runners(&invocation, &runners);
        let argv = match &invocation {
            CommandInvocation::Argv { program, args } => Some(
                std::iter::once(program.clone()).chain(args.iter().cloned()).collect::<Vec<_>>()
            ),
            CommandInvocation::Script(script) | CommandInvocation::PowerShellScript(script) =>
                codex_shell_command::validation::standalone_argv(script),
        };
        let receipt_runner = argv.as_ref().and_then(|argv| argv.split_first())
            .and_then(|(program, args)| runners.iter().find(|runner| runner.matches(program, args)))
            .and_then(|runner| runner.receipt_runner.clone());
        (declared.is_some() || matches!(classification, ValidationClassification::Validation { .. }))
            .then_some(CommandValidation { declared, classification, receipt_runner })
    }).await.ok().flatten()
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
        fn head_oid_tracks_loose_packed_detached_and_linked_refs() {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path();
            let git = root.join(".git");
            std::fs::create_dir_all(git.join("refs/heads")).unwrap();
            let first = "a".repeat(40);
            let second = "b".repeat(40);
            std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
            std::fs::write(git.join("refs/heads/main"), &first).unwrap();
            assert_eq!(repository_head_oid(root), Some(first.clone()));
            std::fs::write(git.join("refs/heads/main"), &second).unwrap();
            assert_eq!(repository_head_oid(root), Some(second.clone()));
            std::fs::remove_file(git.join("refs/heads/main")).unwrap();
            std::fs::write(git.join("packed-refs"), format!("{first} refs/heads/main\n")).unwrap();
            assert_eq!(repository_head_oid(root), Some(first.clone()));
            std::fs::write(git.join("HEAD"), &second).unwrap();
            assert_eq!(repository_head_oid(root), Some(second));
            let linked = root.join("linked");
            let worktree_git = git.join("worktrees/linked");
            std::fs::create_dir_all(&linked).unwrap();
            std::fs::create_dir_all(&worktree_git).unwrap();
            std::fs::write(linked.join(".git"), "gitdir: ../.git/worktrees/linked\n").unwrap();
            std::fs::write(worktree_git.join("commondir"), "../..\n").unwrap();
            std::fs::write(worktree_git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
            assert_eq!(repository_head_oid(&linked), Some(first));
        }

        #[test]
        fn runner_cache_uses_committed_revision_and_rebinds_cwd() {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path();
            let git = |args: &[&str]| {
                let output = std::process::Command::new("git").arg("-C").arg(root)
                    .args(["-c", "user.name=Test", "-c", "user.email=test@example.com", "-c", "commit.gpgsign=false"])
                    .args(args).output().unwrap();
                assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
            };
            git(&["init"]);
            std::fs::create_dir(root.join(".codex")).unwrap();
            std::fs::create_dir(root.join("nested")).unwrap();
            let manifest = root.join(".codex/test-runners.json");
            let config = |operation| serde_json::json!({"version":1,"runners":[{
                "programs":["python"],"prefixes":[["scripts/check.py"]],"operations":[operation]
            }]}).to_string();
            std::fs::write(&manifest, config("test")).unwrap();
            git(&["add", ".codex/test-runners.json"]);
            git(&["commit", "-m", "first"]);
            assert_eq!(repository_runners(root)[0].operations, vec![ValidationOperation::Test]);
            std::fs::write(&manifest, config("lint")).unwrap();
            let nested = root.join("nested");
            let cached = repository_runners(&nested);
            assert_eq!(cached[0].operations, vec![ValidationOperation::Test]);
            assert_eq!(cached[0].path_context, Some((root.to_path_buf(), nested)));
            git(&["add", ".codex/test-runners.json"]);
            git(&["commit", "-m", "second"]);
            assert_eq!(repository_runners(root)[0].operations, vec![ValidationOperation::Lint]);
        }

        fn classify_validation(invocation: &CommandInvocation) -> ValidationClassification {
            let runner = serde_json::from_value(serde_json::json!({
                "programs": ["python", "py"],
                "prefixes": [["scripts/validate.py", "run"]],
                "options": {"-I": 0},
                "allow_extra_args": true,
                "operations": ["test"]
            })).unwrap();
            classify_with_runners(invocation, &[runner])
        }

        fn is_validation(invocation: &CommandInvocation) -> bool {
            matches!(classify_validation(invocation), ValidationClassification::Validation { .. })
        }

        #[test]
        fn passthrough_runner_classifies_only_the_child_and_cannot_claim_receipts() {
            let mut runner: codex_shell_command::validation::RepositoryRunner =
                serde_json::from_value(serde_json::json!({
                    "programs": ["python"],
                    "prefixes": [["scripts/rust_build_status.py", "run-lane"]],
                    "allow_extra_args": true,
                    "passthrough_after": "--"
                })).unwrap();
            for (child, expected) in [
                (vec!["cargo", "test", "-p", "example"], true),
                (vec!["cargo", "check"], true),
                (vec!["cargo", "build"], false),
                (vec!["cargo", "test", "--help"], false),
                (vec!["echo", "cargo", "test"], false),
                (vec![], false),
            ] {
                let mut args = vec!["scripts/rust_build_status.py", "run-lane", "--lane", "core-tests", "--"];
                args.extend(child);
                let invocation = argv("python", &args);
                assert_eq!(matches!(classify_with_runners(&invocation, &[runner.clone()]),
                    ValidationClassification::Validation { .. }), expected, "{args:?}");
            }
            runner.receipt_runner = Some("rust_test_runner".into());
            assert!(!runner.matches("python", &[
                "scripts/rust_build_status.py", "run-lane", "--", "cargo", "test",
            ].map(str::to_string)));
        }

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
