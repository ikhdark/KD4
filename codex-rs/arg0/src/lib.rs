use std::ffi::OsString;
use std::fs::File;
use std::future::Future;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;

use codex_apply_patch::CODEX_CORE_APPLY_PATCH_ARG1;
use codex_exec_server::CODEX_FS_HELPER_ARG1;
use codex_install_context::InstallContext;
use codex_utils_home_dir::find_codex_home;
use codex_windows_sandbox::CODEX_WINDOWS_SANDBOX_ARG1;
use tempfile::TempDir;

const APPLY_PATCH_ARG0: &str = "apply_patch";
const MISSPELLED_APPLY_PATCH_ARG0: &str = "applypatch";
const LOCK_FILENAME: &str = ".lock";
const TOKIO_WORKER_STACK_SIZE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Arg0DispatchPaths {
    /// Stable path to the current Codex executable for child re-execs.
    ///
    /// Prefer this over [`std::env::current_exe()`] in code that may run under
    /// a test harness, where `current_exe()` can point at the harness binary
    /// instead of the real Codex CLI.
    pub codex_self_exe: Option<PathBuf>,
}

/// Keeps the per-session PATH entry alive and locked for the process lifetime.
pub struct Arg0PathEntryGuard {
    _temp_dir: TempDir,
    _lock_file: File,
    paths: Arg0DispatchPaths,
}

impl Arg0PathEntryGuard {
    fn new(temp_dir: TempDir, lock_file: File, paths: Arg0DispatchPaths) -> Self {
        Self {
            _temp_dir: temp_dir,
            _lock_file: lock_file,
            paths,
        }
    }

    pub fn paths(&self) -> &Arg0DispatchPaths {
        &self.paths
    }
}

pub fn arg0_dispatch() -> Option<Arg0PathEntryGuard> {
    // Determine if we were invoked via the special alias.
    let mut args = std::env::args_os();
    let argv0 = args.next().unwrap_or_default();
    let exe_name = Path::new(&argv0)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");

    if exe_name == APPLY_PATCH_ARG0 || exe_name == MISSPELLED_APPLY_PATCH_ARG0 {
        codex_apply_patch::main();
    }

    let argv1 = args.next().unwrap_or_default();
    if argv1 == CODEX_FS_HELPER_ARG1 {
        codex_exec_server::run_fs_helper_main();
    }
    if argv1 == CODEX_WINDOWS_SANDBOX_ARG1 {
        codex_windows_sandbox::run_windows_sandbox_wrapper_main();
    }
    if argv1 == CODEX_CORE_APPLY_PATCH_ARG1 {
        let mut stdin = std::io::stdin();
        let patch_arg = match apply_patch_arg_from_args_or_stdin(args, &mut stdin) {
            Ok(patch_arg) => patch_arg,
            Err(err) => {
                print_apply_patch_input_error(&err);
                std::process::exit(apply_patch_input_error_exit_code(&err));
            }
        };

        let mut stdout = std::io::stdout();
        let mut stderr = std::io::stderr();
        let cwd = match codex_utils_absolute_path::AbsolutePathBuf::current_dir() {
            Ok(cwd) => cwd,
            Err(_) => std::process::exit(1),
        };
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(_) => std::process::exit(1),
        };
        let cwd = cwd.into();
        let exit_code = match runtime.block_on(codex_apply_patch::apply_patch(
            &patch_arg,
            &cwd,
            &mut stdout,
            &mut stderr,
            codex_exec_server::LOCAL_FS.as_ref(),
            /*sandbox*/ None,
        )) {
            Ok(_) => 0,
            Err(_) => 1,
        };
        std::process::exit(exit_code);
    }

    // This modifies the environment, which is not thread-safe, so do this
    // before creating any threads/the Tokio runtime.
    load_dotenv();

    let (path_entry_guard, updated_path_env_var) = prepare_path_env_var_with_aliases(
        InstallContext::current(),
        std::env::var_os("PATH"),
        prepare_path_entry_for_codex_aliases,
    );
    if let Some(updated_path_env_var) = updated_path_env_var {
        // It is safe to call set_var() because our process is single-threaded at
        // this point in its execution.
        unsafe {
            std::env::set_var("PATH", updated_path_env_var);
        }
    }
    path_entry_guard
}

#[derive(Debug)]
enum ApplyPatchInputError {
    EmptyStdin,
    NonUtf8Argument,
    ReadStdin(std::io::Error),
    TooManyArguments,
}

fn apply_patch_arg_from_args_or_stdin(
    mut args: impl Iterator<Item = OsString>,
    stdin: &mut impl Read,
) -> Result<String, ApplyPatchInputError> {
    let Some(arg) = args.next() else {
        let mut buf = String::new();
        stdin
            .read_to_string(&mut buf)
            .map_err(ApplyPatchInputError::ReadStdin)?;
        if buf.is_empty() {
            return Err(ApplyPatchInputError::EmptyStdin);
        }
        return Ok(buf);
    };

    let patch_arg = arg
        .into_string()
        .map_err(|_| ApplyPatchInputError::NonUtf8Argument)?;

    if args.next().is_some() {
        return Err(ApplyPatchInputError::TooManyArguments);
    }

    Ok(patch_arg)
}

fn print_apply_patch_input_error(err: &ApplyPatchInputError) {
    match err {
        ApplyPatchInputError::EmptyStdin => {
            eprintln!("Usage: apply_patch 'PATCH'\n       echo 'PATCH' | apply_patch");
        }
        ApplyPatchInputError::NonUtf8Argument => {
            eprintln!("Error: {CODEX_CORE_APPLY_PATCH_ARG1} requires a UTF-8 PATCH argument.");
        }
        ApplyPatchInputError::ReadStdin(err) => {
            eprintln!("Error: Failed to read PATCH from stdin.\n{err}");
        }
        ApplyPatchInputError::TooManyArguments => {
            eprintln!("Error: {CODEX_CORE_APPLY_PATCH_ARG1} accepts exactly one PATCH argument.");
        }
    }
}

fn apply_patch_input_error_exit_code(err: &ApplyPatchInputError) -> i32 {
    match err {
        ApplyPatchInputError::EmptyStdin | ApplyPatchInputError::TooManyArguments => 2,
        ApplyPatchInputError::NonUtf8Argument | ApplyPatchInputError::ReadStdin(_) => 1,
    }
}

fn prepare_path_env_var_with_aliases(
    install_context: &InstallContext,
    existing_path: Option<OsString>,
    prepare_aliases: impl FnOnce(Option<OsString>) -> std::io::Result<(Arg0PathEntryGuard, OsString)>,
) -> (Option<Arg0PathEntryGuard>, Option<OsString>) {
    let package_path = path_env_with_package_path_dir(install_context, existing_path.clone());
    let path_for_aliases = package_path.clone().or(existing_path);

    match prepare_aliases(path_for_aliases) {
        Ok((path_entry, updated_path_env_var)) => (Some(path_entry), Some(updated_path_env_var)),
        Err(err) => {
            // It is possible that Codex will proceed successfully even if
            // creating helper aliases fails, so warn the user and move on.
            eprintln!("WARNING: proceeding, even though we could not create PATH aliases: {err}");
            (None, package_path)
        }
    }
}

/// Prepares the Windows helper aliases and dispatches the async entry point.
///
/// 1.  Load `.env` values from `~/.codex/.env` before creating any threads.
/// 2.  Spawn a main runtime thread with a controlled stack size.
/// 3.  Construct a Tokio multi-thread runtime.
/// 4.  Execute the provided async `main_fn` inside that runtime, forwarding any
///     error. Note that `main_fn` receives [`Arg0DispatchPaths`], which
///     contains the helper executable paths needed to construct
///     [`codex_core::config::Config`].
///
/// This function should be used to wrap any `main()` function in binary crates
/// in this workspace that depends on these helper CLIs.
pub fn arg0_dispatch_or_else<F, Fut>(main_fn: F) -> anyhow::Result<()>
where
    F: FnOnce(Arg0DispatchPaths) -> Fut + Send + 'static,
    Fut: Future<Output = anyhow::Result<()>>,
{
    // Retain the TempDir so it exists for the lifetime of the invocation of
    // this executable. Admittedly, we could invoke `keep()` on it, but it
    // would be nice to avoid leaving temporary directories behind, if possible.
    let path_entry_guard = arg0_dispatch();
    let current_exe = std::env::current_exe().ok();

    // Regular invocation. Run the async entry point on a thread with the same
    // stack budget as Tokio workers; `Runtime::block_on` otherwise runs the
    // top-level future on the caller's OS stack.
    let handle = std::thread::Builder::new()
        .name("codex-main".to_string())
        .stack_size(TOKIO_WORKER_STACK_SIZE_BYTES)
        .spawn(move || {
            let runtime = build_runtime()?;
            runtime.block_on(run_main_with_arg0_guard(
                path_entry_guard,
                current_exe,
                main_fn,
            ))
        })?;
    match handle.join() {
        Ok(result) => result,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

async fn run_main_with_arg0_guard<F, Fut>(
    path_entry_guard: Option<Arg0PathEntryGuard>,
    current_exe: Option<PathBuf>,
    main_fn: F,
) -> anyhow::Result<()>
where
    F: FnOnce(Arg0DispatchPaths) -> Fut,
    Fut: Future<Output = anyhow::Result<()>>,
{
    let paths = Arg0DispatchPaths {
        codex_self_exe: current_exe.clone(),
    };

    let result = main_fn(paths).await;
    // Keep the arg0 tempdir guard alive until the async entry point finishes;
    // runtime paths above can point at aliases inside that directory.
    drop(path_entry_guard);
    result
}

fn build_runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.enable_all();
    builder.thread_stack_size(TOKIO_WORKER_STACK_SIZE_BYTES);
    Ok(builder.build()?)
}

const ILLEGAL_ENV_VAR_PREFIX: &str = "CODEX_";

/// Load env vars from ~/.codex/.env.
///
/// Security: Do not allow `.env` files to create or modify any variables
/// with names starting with `CODEX_`.
fn load_dotenv() {
    if let Ok(codex_home) = find_codex_home()
        && let Ok(iter) = dotenvy::from_path_iter(codex_home.join(".env"))
    {
        set_filtered(iter);
    }
}

/// Helper to set vars from a dotenvy iterator while filtering out `CODEX_` keys.
fn set_filtered<I>(iter: I)
where
    I: IntoIterator<Item = Result<(String, String), dotenvy::Error>>,
{
    for (key, value) in iter.into_iter().flatten() {
        if !key.to_ascii_uppercase().starts_with(ILLEGAL_ENV_VAR_PREFIX) {
            // It is safe to call set_var() because our process is
            // single-threaded at this point in its execution.
            unsafe { std::env::set_var(&key, &value) };
        }
    }
}

/// Creates a temporary directory with an `apply_patch.bat` helper.
///   with the hidden `--codex-run-as-apply-patch` flag.
///
/// Returns the temporary directory guard and the PATH value that prepends the
/// temporary directory so `apply_patch` can be on the PATH without requiring the
/// user to install a separate executable, simplifying the deployment of Codex
/// CLI.
/// Note: In debug builds the temp-dir guard is disabled to ease local testing.
///
/// IMPORTANT: Callers must update PATH before multiple threads are spawned.
fn prepare_path_entry_for_codex_aliases(
    existing_path: Option<OsString>,
) -> std::io::Result<(Arg0PathEntryGuard, OsString)> {
    let codex_home = find_codex_home()?;
    #[cfg(not(debug_assertions))]
    {
        // Guard against placing helpers in system temp directories outside debug builds.
        let temp_root = std::env::temp_dir();
        if codex_home.starts_with(&temp_root) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "Refusing to create helper binaries under temporary dir {temp_root:?} (codex_home: {codex_home:?})"
                ),
            ));
        }
    }

    std::fs::create_dir_all(&codex_home)?;
    // Use a CODEX_HOME-scoped temp root to avoid cluttering the top-level directory.
    let temp_root = codex_home.join("tmp").join("arg0");
    std::fs::create_dir_all(&temp_root)?;
    // Best-effort cleanup of stale per-session dirs. Ignore failures so startup proceeds.
    if let Err(err) = janitor_cleanup(&temp_root) {
        eprintln!("WARNING: failed to clean up stale arg0 temp dirs: {err}");
    }

    let temp_dir = tempfile::Builder::new()
        .prefix("codex-arg0")
        .tempdir_in(&temp_root)?;
    let path = temp_dir.path();

    let lock_path = path.join(LOCK_FILENAME);
    let lock_file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)?;
    lock_file.try_lock()?;

    for filename in &[APPLY_PATCH_ARG0, MISSPELLED_APPLY_PATCH_ARG0] {
        let exe = std::env::current_exe()?;
        let batch_script = path.join(format!("{filename}.bat"));
        let exe = exe.display();
        std::fs::write(
            &batch_script,
            format!(
                r#"@echo off
"{exe}" {CODEX_CORE_APPLY_PATCH_ARG1} %*
"#,
            ),
        )?;
    }

    let updated_path_env_var = path_env_with_entry(path, existing_path);

    let paths = Arg0DispatchPaths {
        codex_self_exe: std::env::current_exe().ok(),
    };

    Ok((
        Arg0PathEntryGuard::new(temp_dir, lock_file, paths),
        updated_path_env_var,
    ))
}

fn path_env_with_package_path_dir(
    install_context: &InstallContext,
    existing_path: Option<OsString>,
) -> Option<OsString> {
    let path_dir = install_context
        .package_layout
        .as_ref()
        .and_then(|package_layout| package_layout.path_dir.as_ref())?;
    Some(path_env_with_entry(path_dir.as_path(), existing_path))
}

fn path_env_with_entry(path_entry: &Path, existing_path: Option<OsString>) -> OsString {
    const PATH_SEPARATOR: &str = ";";

    let capacity = path_entry.as_os_str().len()
        + existing_path
            .as_ref()
            .map_or(0, |existing_path| 1 + existing_path.len());
    let mut path_env_var = OsString::with_capacity(capacity);
    path_env_var.push(path_entry);
    if let Some(existing_path) = existing_path {
        path_env_var.push(PATH_SEPARATOR);
        path_env_var.push(existing_path);
    }
    path_env_var
}

fn janitor_cleanup(temp_root: &Path) -> std::io::Result<()> {
    let entries = match std::fs::read_dir(temp_root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        // Skip the directory if locking fails or the lock is currently held.
        let Some(_lock_file) = try_lock_dir(&path)? else {
            continue;
        };

        match std::fs::remove_dir_all(&path) {
            Ok(()) => {}
            // Expected TOCTOU race: directory can disappear after read_dir/lock checks.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err),
        }
    }

    Ok(())
}

fn try_lock_dir(dir: &Path) -> std::io::Result<Option<File>> {
    let lock_path = dir.join(LOCK_FILENAME);
    let lock_file = match File::options().read(true).write(true).open(&lock_path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };

    match lock_file.try_lock() {
        Ok(()) => Ok(Some(lock_file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(err) => Err(err.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::ApplyPatchInputError;
    use super::LOCK_FILENAME;
    use super::janitor_cleanup;
    use codex_install_context::CodexPackageLayout;
    use codex_install_context::InstallContext;
    use codex_install_context::InstallMethod;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use pretty_assertions::assert_eq;
    use std::ffi::OsString;
    use std::fs;
    use std::fs::File;
    use std::path::Path;
    use std::path::PathBuf;
    use tempfile::TempDir;

    struct PackagePathTestFixture {
        _temp_dir: TempDir,
        arg0_dir: PathBuf,
        existing_dir: PathBuf,
        install_context: InstallContext,
        path_dir: AbsolutePathBuf,
    }

    fn create_lock(dir: &Path) -> std::io::Result<File> {
        let lock_path = dir.join(LOCK_FILENAME);
        File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)
    }

    fn invalid_unicode_os_string() -> OsString {
        use std::os::windows::ffi::OsStringExt;
        OsString::from_wide(&[0xD800])
    }

    fn package_path_test_fixture() -> anyhow::Result<PackagePathTestFixture> {
        let temp_dir = TempDir::new()?;
        let arg0_dir = temp_dir.path().join("arg0");
        let package_dir = temp_dir.path().join("package");
        let bin_dir = package_dir.join("bin");
        let path_dir = package_dir.join("codex-path");
        let existing_dir = temp_dir.path().join("existing-bin");
        fs::create_dir_all(&arg0_dir)?;
        fs::create_dir_all(&bin_dir)?;
        fs::create_dir_all(&path_dir)?;
        fs::create_dir_all(&existing_dir)?;
        let path_dir = AbsolutePathBuf::from_absolute_path(path_dir.canonicalize()?)?;
        let install_context = InstallContext {
            method: InstallMethod::Other,
            package_layout: Some(CodexPackageLayout {
                package_dir: AbsolutePathBuf::from_absolute_path(package_dir.canonicalize()?)?,
                bin_dir: AbsolutePathBuf::from_absolute_path(bin_dir.canonicalize()?)?,
                resources_dir: None,
                path_dir: Some(path_dir.clone()),
            }),
        };

        Ok(PackagePathTestFixture {
            _temp_dir: temp_dir,
            arg0_dir,
            existing_dir,
            install_context,
            path_dir,
        })
    }

    #[test]
    fn hidden_apply_patch_arg_uses_single_utf8_argument() {
        let mut stdin = "ignored".as_bytes();

        let patch = super::apply_patch_arg_from_args_or_stdin(
            [OsString::from("*** Begin Patch\n*** End Patch")].into_iter(),
            &mut stdin,
        )
        .expect("single UTF-8 patch argument should be accepted");

        assert_eq!(patch, "*** Begin Patch\n*** End Patch");
    }

    #[test]
    fn hidden_apply_patch_arg_reads_stdin_when_argument_missing() {
        let mut stdin = "*** Begin Patch\n*** Add File: file.txt\n+hello\n*** End Patch".as_bytes();

        let patch = super::apply_patch_arg_from_args_or_stdin([].into_iter(), &mut stdin)
            .expect("stdin patch should be accepted when no argument is passed");

        assert_eq!(
            patch,
            "*** Begin Patch\n*** Add File: file.txt\n+hello\n*** End Patch"
        );
    }

    #[test]
    fn hidden_apply_patch_arg_rejects_empty_stdin() {
        let mut stdin = "".as_bytes();

        let err = super::apply_patch_arg_from_args_or_stdin([].into_iter(), &mut stdin)
            .expect_err("empty stdin should be rejected");

        assert!(matches!(err, ApplyPatchInputError::EmptyStdin));
        assert_eq!(super::apply_patch_input_error_exit_code(&err), 2);
    }

    #[test]
    fn hidden_apply_patch_arg_rejects_invalid_utf8_stdin() {
        let mut stdin = std::io::Cursor::new(vec![0xff]);

        let err = super::apply_patch_arg_from_args_or_stdin([].into_iter(), &mut stdin)
            .expect_err("invalid UTF-8 stdin should be rejected");

        assert!(matches!(
            &err,
            ApplyPatchInputError::ReadStdin(source)
                if source.kind() == std::io::ErrorKind::InvalidData
        ));
        assert_eq!(super::apply_patch_input_error_exit_code(&err), 1);
    }

    #[test]
    fn hidden_apply_patch_arg_rejects_non_utf8_argument() {
        let mut stdin = "ignored".as_bytes();

        let err = super::apply_patch_arg_from_args_or_stdin(
            [invalid_unicode_os_string()].into_iter(),
            &mut stdin,
        )
        .expect_err("non-UTF-8 patch argument should be rejected");

        assert!(matches!(err, ApplyPatchInputError::NonUtf8Argument));
        assert_eq!(super::apply_patch_input_error_exit_code(&err), 1);
    }

    #[test]
    fn hidden_apply_patch_arg_rejects_extra_arguments() {
        let mut stdin = "ignored".as_bytes();

        let err = super::apply_patch_arg_from_args_or_stdin(
            [OsString::from("patch"), OsString::from("extra")].into_iter(),
            &mut stdin,
        )
        .expect_err("extra patch arguments should be rejected");

        assert!(matches!(err, ApplyPatchInputError::TooManyArguments));
        assert_eq!(super::apply_patch_input_error_exit_code(&err), 2);
    }

    #[test]
    fn path_env_can_prepend_package_path_before_arg0_alias_dir() -> anyhow::Result<()> {
        let fixture = package_path_test_fixture()?;

        let package_path = super::path_env_with_package_path_dir(
            &fixture.install_context,
            Some(fixture.existing_dir.as_os_str().to_owned()),
        )
        .expect("package path dir should update PATH");
        let updated_path = super::path_env_with_entry(&fixture.arg0_dir, Some(package_path));

        assert_eq!(
            std::env::split_paths(&updated_path).collect::<Vec<_>>(),
            vec![
                fixture.arg0_dir,
                fixture.path_dir.as_path().to_path_buf(),
                fixture.existing_dir
            ],
        );
        Ok(())
    }

    #[test]
    fn package_path_survives_arg0_alias_setup_failure() -> anyhow::Result<()> {
        let fixture = package_path_test_fixture()?;

        let (path_entry_guard, updated_path_env_var) = super::prepare_path_env_var_with_aliases(
            &fixture.install_context,
            Some(fixture.existing_dir.as_os_str().to_owned()),
            |path_for_aliases| {
                assert_eq!(
                    std::env::split_paths(
                        &path_for_aliases.expect("package PATH should be passed to alias setup")
                    )
                    .collect::<Vec<_>>(),
                    vec![
                        fixture.path_dir.as_path().to_path_buf(),
                        fixture.existing_dir.clone()
                    ],
                );
                Err(std::io::Error::other("alias setup failed"))
            },
        );

        assert!(path_entry_guard.is_none());
        let updated_path_env_var =
            updated_path_env_var.expect("package PATH should survive alias setup failure");
        assert_eq!(
            std::env::split_paths(&updated_path_env_var).collect::<Vec<_>>(),
            vec![
                fixture.path_dir.as_path().to_path_buf(),
                fixture.existing_dir
            ],
        );
        Ok(())
    }

    #[test]
    fn janitor_skips_dirs_without_lock_file() -> std::io::Result<()> {
        let root = tempfile::tempdir()?;
        let dir = root.path().join("no-lock");
        fs::create_dir(&dir)?;

        janitor_cleanup(root.path())?;

        assert!(dir.exists());
        Ok(())
    }

    #[test]
    fn janitor_skips_dirs_with_held_lock() -> std::io::Result<()> {
        let root = tempfile::tempdir()?;
        let dir = root.path().join("locked");
        fs::create_dir(&dir)?;
        let lock_file = create_lock(&dir)?;
        lock_file.try_lock()?;

        janitor_cleanup(root.path())?;

        assert!(dir.exists());
        Ok(())
    }

    #[test]
    fn janitor_removes_dirs_with_unlocked_lock() -> std::io::Result<()> {
        let root = tempfile::tempdir()?;
        let dir = root.path().join("stale");
        fs::create_dir(&dir)?;
        create_lock(&dir)?;

        janitor_cleanup(root.path())?;

        assert!(!dir.exists());
        Ok(())
    }
}
