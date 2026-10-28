//! Execution resources that are not isolated by Cargo target lanes.
//!
//! Python test modules can own fixed cwd-relative fixtures (and pytest owns
//! `.pytest_cache`). Reserve that test working-directory resource, not the
//! checkout: unrelated commands, Cargo lanes, and other test directories do
//! not contend. The process owner, never the tool-response observer, holds it.
//! This is a cwd-relative namespace, not protection for arbitrary external
//! fixture paths or an explicitly shared pytest `--basetemp` elsewhere.

use std::fs::File;
use std::fs::OpenOptions;
use std::path::Path;
use sha2::Digest;
use sha2::Sha256;

#[derive(Debug)]
pub(crate) struct ValidationResourceLease {
    _file: File,
}

pub(crate) async fn acquire_python_test_resource(
    command: &[String],
    cwd: &Path,
) -> Result<Option<ValidationResourceLease>, String> {
    let command = command.to_vec();
    let cwd = cwd.to_path_buf();
    let acquisition = tokio::task::spawn_blocking(move || {
        let Some((program, args)) = command.split_first() else { return Ok(None); };
        // Reuse the admitted shell/wrapper traversal instead of matching prose
        // or treating every Python invocation as a test.
        let (policy, selection) = codex_shell_command::validation::analyze_validation_policy_argv_with_runners(
            program, args, &[],
        );
        if !policy.is_validation() { return Ok(None); }
        // policy_argv emits normalized_program_name in every leaf: absolute
        // interpreter paths and .exe/.cmd/.bat spellings are already canonical.
        let leaves = serde_json::from_str::<Vec<Vec<String>>>(&selection)
            .map_err(|error| format!("invalid Python test resource selection: {error}"))?;
        if !leaves.iter().any(|argv| argv.split_first().is_some_and(|(program, args)| {
            program == "pytest" || ((program == "py" || program.starts_with("python"))
                && codex_shell_command::validation::python_module(args)
                    .is_some_and(|(module, _)| matches!(module, "pytest" | "unittest")))
        })) { return Ok(None); }
        if !codex_shell_command::validation::validation_resource_cwd_is_unambiguous(program, args) {
            return Err("Python test resource cwd is ambiguous inside this shell/wrapper command. Use the tool's workdir with direct argv or a single test command; this validation did not run.".to_owned());
        }
        let root = dunce::canonicalize(cwd)
            .map_err(|error| format!("cannot identify Python test resource: {error}"))?;
        let identity = root.to_string_lossy();
        #[cfg(windows)]
        let identity = identity.to_lowercase();
        let key = hex::encode(Sha256::digest(identity.as_bytes()));
        // Shared across sessions and CODEX_HOMEs. Never unlink a live lock's
        // name: that would admit another process on a different inode.
        let directory = std::env::temp_dir().join("codex-python-test-resources-v1");
        std::fs::create_dir_all(&directory)
            .map_err(|error| format!("cannot prepare Python test resource lock: {error}"))?;
        let file = OpenOptions::new().read(true).write(true).create(true).truncate(false)
            .open(directory.join(format!("{key}.lock")))
            .map_err(|error| format!("cannot open Python test resource lock: {error}"))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(ValidationResourceLease { _file: file })),
            Err(std::fs::TryLockError::WouldBlock) => Err(format!(
                "Python test resource busy at {}. Another process owns its live test fixtures; this validation did not run. Resume the existing operation or wait for its owner to finish.", root.display(),
            )),
            Err(std::fs::TryLockError::Error(error)) => Err(format!(
                "cannot lock Python test resource: {error}; this validation did not run",
            )),
        }
    });
    // A stalled filesystem cannot hold validation admission indefinitely. A
    // timed-out worker owns no child and drops any eventual lease on completion.
    tokio::time::timeout(std::time::Duration::from_secs(5), acquisition).await
        .map_err(|_| "Python test resource admission timed out; this validation did not run".to_owned())?
        .map_err(|error| format!("Python test resource admission failed: {error}; this validation did not run"))?
}
