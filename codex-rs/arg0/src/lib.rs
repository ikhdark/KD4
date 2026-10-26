use std::ffi::OsString;
use std::fs::File;
use std::future::Future;
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
}

/// Runs the helper selected by this executable's name or its hidden argv[1]
/// flag; a helper never returns. Returns only when this process is not a
/// helper invocation.
///
/// Helpers run with the environment they inherit, before `.env` values or
/// PATH aliases are applied.
pub fn arg0_dispatch_helper() {
    let mut args = std::env::args_os();
    let argv0 = args.next().unwrap_or_default();
    let exe_name = Path::new(&argv0)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");

    if is_apply_patch_executable(exe_name) {
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
        std::process::exit(codex_apply_patch::run_main_with_args(args));
    }
}

fn is_apply_patch_executable(name: &str) -> bool {
    if cfg!(windows) {
        let name = name.to_ascii_lowercase();
        let name = name.strip_suffix(".exe").unwrap_or(&name);
        name == APPLY_PATCH_ARG0 || name == MISSPELLED_APPLY_PATCH_ARG0
    } else {
        name == APPLY_PATCH_ARG0 || name == MISSPELLED_APPLY_PATCH_ARG0
    }
}

pub fn arg0_dispatch() -> Option<Arg0PathEntryGuard> {
    arg0_dispatch_helper();

    // This modifies the environment, which is not thread-safe, so do this
    // before creating any threads/the Tokio runtime.
    load_dotenv();

    prepare_path_aliases()
}

fn prepare_path_aliases() -> Option<Arg0PathEntryGuard> {
    let (path_entry_guard, updated_path_env_var) = prepare_path_env_var_with_aliases(
        InstallContext::current(),
        std::env::var_os("PATH"),
        prepare_path_entry_for_codex_aliases,
    );
    if let Some(updated_path_env_var) = updated_path_env_var {
        // SAFETY: It is safe to call set_var() because our process is single-threaded at
        // this point in its execution.
        unsafe {
            std::env::set_var("PATH", updated_path_env_var);
        }
    }
    path_entry_guard
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
///     contains the current executable path needed to construct
///     [`codex_core::config::Config`].
///
/// This function should be used to wrap any `main()` function in binary crates
/// in this workspace that depends on these helper CLIs.
pub fn arg0_dispatch_or_else<F, Fut>(main_fn: F) -> anyhow::Result<()>
where
    F: FnOnce(Arg0DispatchPaths) -> Fut + Send + 'static,
    Fut: Future<Output = anyhow::Result<()>>,
{
    arg0_dispatch_or_else_with_pre_runtime(|| main_fn)
}

/// Like [`arg0_dispatch_or_else`], but prepares the async entry point after
/// helper dispatch and dotenv loading, before creating any runtime threads.
/// The preparation callback must not start threads if it mutates the Unix
/// environment or performs other startup-only work.
pub fn arg0_dispatch_or_else_with_pre_runtime<P, F, Fut>(prepare_main: P) -> anyhow::Result<()>
where
    P: FnOnce() -> F,
    F: FnOnce(Arg0DispatchPaths) -> Fut + Send + 'static,
    Fut: Future<Output = anyhow::Result<()>>,
{
    // Retain the TempDir so it exists for the lifetime of the invocation of
    // this executable. Admittedly, we could invoke `keep()` on it, but it
    // would be nice to avoid leaving temporary directories behind, if possible.
    arg0_dispatch_helper();
    load_dotenv();
    let main_fn = prepare_main();
    // Help/version parsing can exit before creating helper files. The callback
    // still sees dotenv values and runs before runtime threads, as documented.
    let path_entry_guard = prepare_path_aliases();
    let current_exe = std::env::current_exe().ok();

    // Regular invocation. Run the async entry point on a thread with the same
    // stack budget as Tokio workers; `Runtime::block_on` otherwise runs the
    // top-level future on the caller's OS stack.
    let (main_result_tx, main_result_rx) = std::sync::mpsc::sync_channel(1);
    let (retired_tx, retired_rx) = std::sync::mpsc::sync_channel(1);
    let handle = std::thread::Builder::new()
        .name("codex-main".to_string())
        .stack_size(TOKIO_WORKER_STACK_SIZE_BYTES)
        .spawn(move || {
            let runtime = match build_runtime() {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = main_result_tx.send(Ok(Err(error)));
                    let _ = retired_tx.send(());
                    return;
                }
            };
            // Main can panic while constructing or polling its future. Publish
            // that outcome before runtime teardown, which can wait on blocking
            // work, so panic and success share the same retirement deadline.
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                runtime.block_on(run_main_with_arg0_guard(None, current_exe, main_fn))
            }));
            let _ = main_result_tx.send(result);
            // Keep real runtime custody on this thread. In particular, do not
            // claim shutdown_timeout stopped an admitted blocking filesystem effect.
            drop(runtime);
            // Runtime-owned cleanup may still execute PATH aliases.
            drop(path_entry_guard);
            let _ = retired_tx.send(());
        })?;
    let result = main_result_rx.recv().ok();
    if retired_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .is_err()
    {
        // Last-resort process boundary: returning failure lets binary main exit
        // finitely. Pending I/O may have committed; recovery, not replay, is needed.
        let message = "native runtime retirement exceeded 5 seconds; blocking I/O or committed effects may remain pending; recover the session before replaying work";
        eprintln!("{message}");
        return match result {
            Some(Ok(Err(error))) => Err(error.context(message)),
            _ => Err(anyhow::anyhow!(message)),
        };
    }
    match handle.join() {
        Ok(()) => match result {
            Some(Ok(result)) => result,
            Some(Err(payload)) => std::panic::resume_unwind(payload),
            None => anyhow::bail!("native runtime exited without a main result"),
        },
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
            // SAFETY: It is safe to call set_var() because our process is
            // single-threaded at this point in its execution.
            unsafe { std::env::set_var(&key, &value) };
        }
    }
}

/// Creates a temporary directory with `apply_patch.bat` helpers that re-invoke
/// this executable with the hidden `--codex-run-as-apply-patch` flag.
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

    let lock_file = lock_session_dir(path)?;

    let exe = std::env::current_exe()?;
    let batch_contents = patch_alias_batch_script(&exe);
    for filename in &[APPLY_PATCH_ARG0, MISSPELLED_APPLY_PATCH_ARG0] {
        let batch_script = path.join(format!("{filename}.bat"));
        std::fs::write(&batch_script, &batch_contents)?;
    }

    let updated_path_env_var = path_env_with_entry(path, existing_path)?;

    Ok((
        Arg0PathEntryGuard {
            _temp_dir: temp_dir,
            _lock_file: lock_file,
        },
        updated_path_env_var,
    ))
}

fn patch_alias_batch_script(exe: &Path) -> String {
    // Quotes do not suppress percent expansion in batch source. Delayed
    // expansion must also be disabled before cmd reads the executable line.
    // An inherited ERRORLEVEL environment variable masks cmd's status pseudo-
    // variable. Clear that alias locally before capturing the helper's exit.
    let exe = exe.display().to_string().replace('%', "%%");
    format!(
        "@echo off\nsetlocal DisableDelayedExpansion\nset \"ERRORLEVEL=\"\n\"{exe}\" {CODEX_CORE_APPLY_PATCH_ARG1} %*\nexit /b %ERRORLEVEL%\n"
    )
}

fn lock_session_dir(path: &Path) -> std::io::Result<File> {
    // Publish only an already locked handle so concurrent janitors cannot
    // acquire a newly created session's lock before its owner does.
    let pending_lock = tempfile::NamedTempFile::new_in(path)?;
    pending_lock.as_file().try_lock()?;
    pending_lock
        .persist(path.join(LOCK_FILENAME))
        .map_err(|err| err.error)
}

fn path_env_with_package_path_dir(
    install_context: &InstallContext,
    existing_path: Option<OsString>,
) -> Option<OsString> {
    let path_dir = install_context
        .package_layout
        .as_ref()
        .and_then(|package_layout| package_layout.path_dir.as_ref())?;
    match path_env_with_entry(path_dir.as_path(), existing_path) {
        Ok(path) => Some(path),
        Err(err) => {
            eprintln!("WARNING: could not prepend package PATH directory: {err}");
            None
        }
    }
}

fn path_env_with_entry(
    path_entry: &Path,
    existing_path: Option<OsString>,
) -> std::io::Result<OsString> {
    let entries = std::iter::once(path_entry.to_path_buf()).chain(
        existing_path
            .as_ref()
            .into_iter()
            .flat_map(std::env::split_paths),
    );
    std::env::join_paths(entries)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidInput, err))
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
        let _lock_file = match try_lock_dir(&path) {
            Ok(Some(file)) => file,
            Ok(None) => continue,
            Err(err) => {
                eprintln!("WARNING: could not lock stale arg0 directory {path:?}: {err}");
                continue;
            }
        };

        match std::fs::remove_dir_all(&path) {
            Ok(()) => {}
            // Expected TOCTOU race: directory can disappear after read_dir/lock checks.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => {
                eprintln!("WARNING: could not remove stale arg0 directory {path:?}: {err}");
            }
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

