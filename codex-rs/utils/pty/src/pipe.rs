use std::collections::HashMap;
use std::io;
use std::io::ErrorKind;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicBool;

use anyhow::Result;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::ManagedRootProcess;
#[cfg(windows)]
use crate::WINDOWS_CREATE_SUSPENDED;
#[cfg(windows)]
use crate::WINDOWS_PROCESS_OPERATION_TIMEOUT;
#[cfg(windows)]
use crate::configure_windows_command_args;
use crate::process::ChildTerminator;
use crate::process::ProcessHandle;
use crate::process::ProcessSignal;
use crate::process::SpawnedProcess;
use crate::process::exit_code_from_status;
use crate::process::publish_exit_status;
#[cfg(windows)]
use crate::run_windows_process_operation;

#[cfg(target_os = "linux")]
use libc;

#[cfg(windows)]
use std::os::windows::io::AsRawHandle;
#[cfg(windows)]
use std::os::windows::io::BorrowedHandle;
#[cfg(windows)]
use std::os::windows::io::OwnedHandle;
#[cfg(windows)]
use std::os::windows::io::RawHandle;

struct PipeChildTerminator {
    // Also retains admission while the control handle remains owned.
    #[cfg_attr(not(windows), allow(dead_code))]
    managed: Arc<ManagedRootProcess>,
    #[cfg(windows)]
    process: OwnedHandle,
    #[cfg(unix)]
    process_group: Arc<crate::process::ProcessGroupControl>,
}

impl ChildTerminator for PipeChildTerminator {
    #[cfg(windows)]
    fn tree_is_empty(&self) -> io::Result<bool> {
        self.managed.tree_is_empty()
    }

    #[cfg(windows)]
    fn replay_control(&self) -> Option<Box<dyn ChildTerminator>> {
        Some(Box::new(self.managed.replay_job()))
    }

    fn signal(&mut self, signal: ProcessSignal) -> io::Result<()> {
        match signal {
            ProcessSignal::Interrupt => {
                #[cfg(unix)]
                {
                    self.process_group.signal(signal)
                }
                #[cfg(not(unix))]
                {
                    Err(crate::process::unsupported_signal(signal))
                }
            }
        }
    }

    fn kill(&mut self) -> io::Result<()> {
        #[cfg(unix)]
        {
            self.process_group.kill()
        }
        #[cfg(windows)]
        {
            self.managed.terminate().map_err(|job_error| {
                // Killing only the root cannot establish that its descendants stopped.
                // Keep the tree error visible and the terminator available for retry.
                let fallback = match terminate_process(&self.process) {
                    Ok(()) => "succeeded".to_owned(),
                    Err(error) => format!("also failed: {error}"),
                };
                io::Error::other(format!(
                    "failed to terminate pipe job ({job_error}); root process fallback {fallback}"
                ))
            })
        }
        #[cfg(not(any(unix, windows)))]
        {
            Ok(())
        }
    }
}



#[cfg(windows)]
fn duplicate_process_handle(process: RawHandle) -> io::Result<OwnedHandle> {
    // SAFETY: The sole caller keeps its Child alive through duplication, so this borrowed
    // process handle remains valid until try_clone_to_owned returns.
    unsafe { BorrowedHandle::borrow_raw(process) }.try_clone_to_owned()
}

#[cfg(windows)]
fn terminate_process(process: &OwnedHandle) -> io::Result<()> {
    let success =
        // SAFETY: process owns the live process handle for the entire TerminateProcess call;
        // Windows validates its access rights.
        unsafe { winapi::um::processthreadsapi::TerminateProcess(process.as_raw_handle() as _, 1) };
    if success == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

async fn read_output_stream<R>(
    mut reader: R,
    output_tx: mpsc::Sender<Vec<u8>>,
    complete: Arc<AtomicBool>,
) where
    R: AsyncRead + Unpin,
{
    let mut buf = vec![0u8; 8_192];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => {
                complete.store(true, std::sync::atomic::Ordering::Release);
                break;
            }
            Ok(n) => {
                if output_tx.send(buf[..n].to_vec()).await.is_err() {
                    break;
                }
            }
            Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
}

#[derive(Clone, Copy)]
enum PipeStdinMode {
    Piped,
    Null,
}

/// Windows priority handling for a pipe root. A speculative host keeps the class
/// Windows assigns at creation (parent, job and image-execution policy included),
/// but runs below normal until it is handed real work, so its startup yields to
/// demand work; the assigned class is recorded on its handle for restoration.
#[derive(Clone, Copy)]
enum SpawnPriority {
    Normal,
    LoweredUntilHandoff,
}

/// Lowers a suspended root to below normal and returns the class Windows had
/// assigned it, or `None` (unchanged) when either query fails.
#[cfg(windows)]
fn lower_until_handoff(child: &tokio::process::Child) -> Option<u32> {
    use winapi::um::processthreadsapi::GetPriorityClass;
    use winapi::um::processthreadsapi::SetPriorityClass;
    use winapi::um::winbase::BELOW_NORMAL_PRIORITY_CLASS;
    let handle = child.raw_handle()?;
    // SAFETY: the child's process handle stays open for both calls, which only
    // read or set its priority class and report failure through their results.
    let assigned = unsafe { GetPriorityClass(handle as _) };
    if assigned == 0 {
        return None;
    }
    // SAFETY: as above.
    if unsafe { SetPriorityClass(handle as _, BELOW_NORMAL_PRIORITY_CLASS) } == 0 {
        return None;
    }
    Some(assigned)
}

/// The Windows pipe owner's actual OS process-creation call failed.
#[cfg(windows)]
#[derive(Debug)]
pub struct PipeSpawnError(pub io::Error);

#[cfg(windows)]
impl std::fmt::Display for PipeSpawnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

#[cfg(windows)]
impl std::error::Error for PipeSpawnError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

/// Windows roots are created suspended and attached to their kill-on-close Job
/// before they are allowed to run.
async fn spawn_process_with_stdin_mode(
    program: &str,
    args: &[String],
    cwd: &Path,
    env: &HashMap<String, String>,
    arg0: &Option<String>,
    stdin_mode: PipeStdinMode,
    inherited_fds: &[i32],
    priority: SpawnPriority,
) -> Result<SpawnedProcess> {
    spawn_process_with_stdin_mode_reporting_pid(
        program,
        args,
        cwd,
        env,
        arg0,
        stdin_mode,
        inherited_fds,
        /*launch_slot*/ None,
        priority,
    )
    .await
}

/// The created root's OS process id (0 when unavailable) and its start
/// observation, published for a launcher that hands the root to a later scope.
pub(crate) type LaunchObservationSlot =
    Arc<StdMutex<Option<(u32, crate::process_observation::ObservedProcess)>>>;

/// Same root launch, additionally publishing the root's id and start
/// observation in `launch_slot` before the root is resumed.
#[allow(clippy::too_many_arguments)]
async fn spawn_process_with_stdin_mode_reporting_pid(
    program: &str,
    args: &[String],
    cwd: &Path,
    env: &HashMap<String, String>,
    arg0: &Option<String>,
    stdin_mode: PipeStdinMode,
    inherited_fds: &[i32],
    launch_slot: Option<LaunchObservationSlot>,
    priority: SpawnPriority,
) -> Result<SpawnedProcess> {
    if program.is_empty() {
        anyhow::bail!("missing program for pipe spawn");
    }
    // std::process::Command implicitly invokes CMD for batch files. Native argv
    // must not acquire shell semantics; match ConPTY's explicit-shell boundary.
    #[cfg(windows)]
    if Path::new(program).extension().is_some_and(|extension| {
        extension.eq_ignore_ascii_case("cmd") || extension.eq_ignore_ascii_case("bat")
    }) {
        anyhow::bail!(
            "Pipe spawn cannot execute batch argv directly; invoke cmd.exe /d /c with an explicit shell script"
        );
    }

    #[cfg(not(unix))]
    let _ = inherited_fds;

    // Windows resolves a qualified executable before applying current_dir.
    // Anchor relative paths to the requested cwd, but leave bare PATH lookups
    // and absolute paths unchanged, matching ConPTY's qualified-path boundary.
    #[cfg(windows)]
    let program_path = {
        let path = Path::new(program);
        if path.is_relative()
            && (path.components().count() != 1 || program.contains(['/', '\\']))
        {
            std::borrow::Cow::Owned(std::path::absolute(cwd.join(path))?)
        } else {
            std::borrow::Cow::Borrowed(path)
        }
    };
    #[cfg(windows)]
    let mut command = Command::new(program_path.as_os_str());
    #[cfg(not(windows))]
    let mut command = Command::new(program);
    #[cfg(unix)]
    if let Some(arg0) = arg0 {
        command.arg0(arg0);
    }
    #[cfg(target_os = "linux")]
    // SAFETY: getpid takes no pointers and has no memory or lifetime preconditions.
    let parent_pid = unsafe { libc::getpid() };
    #[cfg(unix)]
    let mut inherited_fds = inherited_fds.to_vec();
    #[cfg(unix)]
    inherited_fds.sort_unstable();
    #[cfg(unix)]
    // SAFETY: The child callback uses only native process setup and async-signal-safe
    // descriptor marking; its descriptor vector is allocated and sorted before fork.
    unsafe {
        command.pre_exec(move || {
            crate::process_group::detach_from_tty()?;
            #[cfg(target_os = "linux")]
            crate::process_group::set_parent_death_signal(parent_pid)?;
            crate::pty::mark_inherited_fds_cloexec_except(&inherited_fds);
            Ok(())
        });
    }
    #[cfg(not(unix))]
    let _ = arg0;
    command.current_dir(cwd);
    command.env_clear();
    for (key, value) in env {
        command.env(key, value);
    }
    #[cfg(windows)]
    configure_windows_command_args(command.as_std_mut(), std::ffi::OsStr::new(program), args);
    #[cfg(not(windows))]
    command.args(args);
    match stdin_mode {
        PipeStdinMode::Piped => {
            command.stdin(Stdio::piped());
        }
        PipeStdinMode::Null => {
            command.stdin(Stdio::null());
        }
    }
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    #[cfg(windows)]
    command.creation_flags(WINDOWS_CREATE_SUSPENDED | 0x0800_0000); // CREATE_NO_WINDOW

    let managed = ManagedRootProcess::reserve_with_reclaim().await?;

    #[cfg(windows)]
    {
        command.kill_on_drop(true);
        let runtime = tokio::runtime::Handle::current();
        // The non-cancellable native owner retains admission through creation, containment,
        // handle setup, and waiter installation. An undeliverable session terminates on drop,
        // while its detached waiter retains admission until the child has been reaped.
        run_windows_process_operation(WINDOWS_PROCESS_OPERATION_TIMEOUT, move || {
            // Keep OS spawn refusal separate from waiter timeout and from
            // failures attaching or configuring a process that did spawn.
            Ok(command.spawn().map(|child| {
                runtime.block_on(finish_pipe_process_setup(child, managed, launch_slot, priority))
            }))
        })
        .await?
        .map_err(PipeSpawnError)?
    }
    #[cfg(not(windows))]
    {
        let child = command.spawn()?;
        finish_pipe_process_setup(child, managed, launch_slot, priority).await
    }
}

async fn finish_pipe_process_setup(
    mut child: tokio::process::Child,
    managed: ManagedRootProcess,
    launch_slot: Option<LaunchObservationSlot>,
    priority: SpawnPriority,
) -> Result<SpawnedProcess> {
    let observation = crate::process_observation::observe_process_start(
        "unified_pipe",
        crate::process_observation::ProcessRole::RequestedRoot,
        child.id(),
        None,
    );
    if let Some(slot) = launch_slot.as_ref()
        && let Ok(mut slot) = slot.lock()
    {
        *slot = Some((child.id().unwrap_or(0), observation.clone()));
    }
    // The root is still suspended here, so a lowered host never runs above
    // below-normal before it is handed work.
    #[cfg(windows)]
    let deferred_priority_class = match priority {
        SpawnPriority::Normal => None,
        SpawnPriority::LoweredUntilHandoff => lower_until_handoff(&child),
    };
    #[cfg(not(windows))]
    let deferred_priority_class = {
        let _ = priority;
        None
    };
    #[cfg(windows)]
    let windows_terminator = {
        let pid = child
            .id()
            .ok_or_else(|| io::Error::other("missing child pid"))?;
        if let Err(error) = managed.attach_and_resume(pid) {
            if let Err(cleanup_error) = managed.cleanup_after_failed_attach(child).await {
                observation.cleanup_observed("attach_failed_cleanup_failed");
                log::warn!("failed to finish pipe attachment cleanup: {cleanup_error}");
            } else {
                observation.cleanup_observed("attach_failed_cleanup_completed");
            }
            return Err(error.into());
        }
        managed.bind_contained_root(&observation);
        let process_handle = child
            .raw_handle()
            .ok_or_else(|| io::Error::other("missing child process handle"))?;
        let process = match duplicate_process_handle(process_handle) {
            Ok(process) => process,
            Err(err) => {
                if let Err(cleanup_error) = managed.cleanup_after_failed_attach(child).await {
                    log::warn!("failed to finish pipe handle setup cleanup: {cleanup_error}");
                }
                return Err(err.into());
            }
        };
        process
    };
    #[cfg(unix)]
    let process_group = Arc::new(crate::process::ProcessGroupControl::new(
        child
            .id()
            .ok_or_else(|| io::Error::other("missing child pid"))?,
    ));

    let managed = Arc::new(managed);
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let (writer_tx, mut writer_rx) = crate::stdin_channel(128);
    let (stdin_close_tx, mut stdin_close_rx) = tokio::sync::watch::channel(false);
    let (stdin_receipt_tx, stdin_receipt_rx) = tokio::sync::watch::channel(None);
    let (stdout_tx, stdout_rx) = mpsc::channel::<Vec<u8>>(128);
    let (stderr_tx, stderr_rx) = mpsc::channel::<Vec<u8>>(128);
    let writer_handle = if let Some(stdin) = stdin {
        tokio::spawn(async move {
            let mut writer = stdin;
            let result: io::Result<()> = async {
                loop {
                    let bytes = tokio::select! {
                        biased;
                        _ = stdin_close_rx.changed(), if !writer_rx.is_closed() => {
                            // Reject new reservations, but drain all accepted bytes.
                            // Sender clones cannot keep the pipe open after close.
                            writer_rx.close();
                            continue;
                        }
                        bytes = writer_rx.recv() => match bytes {
                            Some(bytes) => bytes,
                            None => break,
                        },
                    };
                    writer.write_all(&bytes).await?;
                    writer.flush().await?;
                }
                writer.shutdown().await
            }.await;
            // The OS write handle must be gone before EOF is acknowledged.
            drop(writer);
            stdin_receipt_tx.send_replace(Some(result.map_err(|error| (error.kind(), error.to_string()))));
        })
    } else {
        drop(writer_rx);
        stdin_receipt_tx.send_replace(Some(Ok(())));
        tokio::spawn(async {})
    };

    let capture = crate::process::OutputCapture::default();
    if stdout.is_none() {
        capture
            .stdout
            .store(true, std::sync::atomic::Ordering::Release);
    }
    if stderr.is_none() {
        capture
            .stderr
            .store(true, std::sync::atomic::Ordering::Release);
    }
    let stdout_handle = stdout.map(|stdout| {
        let stdout_tx = stdout_tx.clone();
        let complete = Arc::clone(&capture.stdout);
        tokio::spawn(async move {
            read_output_stream(BufReader::new(stdout), stdout_tx, complete).await;
        })
    });
    let stderr_handle = stderr.map(|stderr| {
        let stderr_tx = stderr_tx.clone();
        let complete = Arc::clone(&capture.stderr);
        tokio::spawn(async move {
            read_output_stream(BufReader::new(stderr), stderr_tx, complete).await;
        })
    });
    let mut reader_abort_handles = Vec::new();
    if let Some(handle) = stdout_handle.as_ref() {
        reader_abort_handles.push(handle.abort_handle());
    }
    if let Some(handle) = stderr_handle.as_ref() {
        reader_abort_handles.push(handle.abort_handle());
    }
    let reader_handle = tokio::spawn(async move {
        if let Some(handle) = stdout_handle {
            let _ = handle.await;
        }
        if let Some(handle) = stderr_handle {
            let _ = handle.await;
        }
    });

    let (exit_tx, exit_rx) = oneshot::channel::<i32>();
    let exit_status = Arc::new(AtomicBool::new(false));
    let wait_exit_status = Arc::clone(&exit_status);
    let exit_code = Arc::new(StdMutex::new(None));
    let wait_exit_code = Arc::clone(&exit_code);
    let wait_managed = Arc::clone(&managed);
    #[cfg(unix)]
    let wait_group = Arc::clone(&process_group);
    let wait_handle: JoinHandle<()> = tokio::spawn(async move {
        // Keep admission even after request_terminate removes the control handle.
        let _managed = wait_managed;
        #[cfg(unix)]
        wait_group.disarm_after_exit().await;
        let status = match child.wait().await {
            Ok(status) => status,
            Err(error) => {
                log::warn!("failed to observe pipe exit; retaining child and admission: {error}");
                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    if let Ok(Some(status)) = child.try_wait() {
                        break status;
                    }
                }
            }
        };
        #[cfg(windows)]
        if let Err(err) = _managed.preserve_descendants() {
            log::warn!("Windows pipe failed to preserve descendants after root exit: {err}");
        }
        let code = exit_code_from_status(status);
        observation.cleanup_observed("root_reaped; descendants_unknown");
        publish_exit_status(&wait_exit_status, &wait_exit_code, code);
        let _ = exit_tx.send(code);
    });

    let handle = ProcessHandle::new(
        writer_tx,
        Box::new(PipeChildTerminator {
            managed,
            #[cfg(windows)]
            process: windows_terminator,
            #[cfg(unix)]
            process_group,
        }),
        reader_handle,
        reader_abort_handles,
        writer_handle,
        wait_handle,
        exit_status,
        exit_code,
        None,
        /*resizer*/ None,
    )
    .with_output_capture(capture)
    .with_stdin_close_request(stdin_close_tx)
    .with_stdin_close_receipt(stdin_receipt_rx)
    .with_deferred_priority_class(deferred_priority_class);

    Ok(SpawnedProcess {
        session: handle,
        stdout_rx,
        stderr_rx,
        exit_rx,
    })
}

/// Spawn a process using regular pipes (no PTY), returning handles for stdin, split output, and exit.
pub async fn spawn_process(
    program: &str,
    args: &[String],
    cwd: &Path,
    env: &HashMap<String, String>,
    arg0: &Option<String>,
) -> Result<SpawnedProcess> {
    spawn_process_with_inherited_fds(program, args, cwd, env, arg0, &[]).await
}

/// Spawn with writable pipe input, preserving selected descriptors on Unix.
pub async fn spawn_process_with_inherited_fds(
    program: &str,
    args: &[String],
    cwd: &Path,
    env: &HashMap<String, String>,
    arg0: &Option<String>,
    inherited_fds: &[i32],
) -> Result<SpawnedProcess> {
    spawn_process_with_stdin_mode(
        program,
        args,
        cwd,
        env,
        arg0,
        PipeStdinMode::Piped,
        inherited_fds,
        SpawnPriority::Normal,
    )
    .await
}

/// Spawn a process using regular pipes, but close stdin immediately.
pub async fn spawn_process_no_stdin(
    program: &str,
    args: &[String],
    cwd: &Path,
    env: &HashMap<String, String>,
    arg0: &Option<String>,
) -> Result<SpawnedProcess> {
    spawn_process_no_stdin_with_inherited_fds(program, args, cwd, env, arg0, &[]).await
}

/// Spawn a process using regular pipes, close stdin immediately, and preserve
/// selected inherited file descriptors across exec on Unix.
pub async fn spawn_process_no_stdin_with_inherited_fds(
    program: &str,
    args: &[String],
    cwd: &Path,
    env: &HashMap<String, String>,
    arg0: &Option<String>,
    inherited_fds: &[i32],
) -> Result<SpawnedProcess> {
    spawn_process_with_stdin_mode(
        program,
        args,
        cwd,
        env,
        arg0,
        PipeStdinMode::Null,
        inherited_fds,
        SpawnPriority::Normal,
    )
    .await
}

/// Like [`spawn_process_no_stdin`], but a Windows root runs below normal until it is
/// handed work: the class Windows assigned at creation is available from
/// [`ProcessHandle::deferred_priority_class`] and must be restored by the child
/// before it runs caller code. Also returns the root's OS process id (0 when
/// unavailable) and its start observation, so the launcher can check who connects
/// to its hand-off channel and attribute the root to the call it is handed to
/// ([`crate::process_observation::ObservedProcess::adopt_into_current_scope`]).
pub async fn spawn_process_no_stdin_lowered_until_handoff(
    program: &str,
    args: &[String],
    cwd: &Path,
    env: &HashMap<String, String>,
    arg0: &Option<String>,
) -> Result<(
    SpawnedProcess,
    u32,
    crate::process_observation::ObservedProcess,
)> {
    let launch_slot: LaunchObservationSlot = Arc::new(StdMutex::new(None));
    let spawned = spawn_process_with_stdin_mode_reporting_pid(
        program,
        args,
        cwd,
        env,
        arg0,
        PipeStdinMode::Null,
        &[],
        Some(Arc::clone(&launch_slot)),
        SpawnPriority::LoweredUntilHandoff,
    )
    .await?;
    let (pid, observation) = launch_slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
        .unwrap_or_default();
    Ok((spawned, pid, observation))
}

