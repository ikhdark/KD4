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

use crate::process::ChildTerminator;
use crate::process::ProcessHandle;
use crate::process::ProcessSignal;
use crate::process::SpawnedProcess;
use crate::process::exit_code_from_status;

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

#[cfg(windows)]
enum WindowsChildTerminator {
    Job {
        job: Arc<crate::win::JobObject>,
        process: OwnedHandle,
    },
    Process(OwnedHandle),
}

struct PipeChildTerminator {
    #[cfg(windows)]
    windows: WindowsChildTerminator,
    #[cfg(unix)]
    process_group_id: u32,
}

impl ChildTerminator for PipeChildTerminator {
    fn signal(&mut self, signal: ProcessSignal) -> io::Result<()> {
        match signal {
            ProcessSignal::Interrupt => {
                #[cfg(unix)]
                {
                    crate::process_group::interrupt_process_group(self.process_group_id)
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
            crate::process_group::kill_process_group(self.process_group_id)
        }

        #[cfg(windows)]
        {
            match &self.windows {
                WindowsChildTerminator::Job { job, process } => {
                    terminate_job_or_process(job, process)
                }
                WindowsChildTerminator::Process(process) => terminate_process(process),
            }
        }

        #[cfg(not(any(unix, windows)))]
        {
            Ok(())
        }
    }
}

#[cfg(windows)]
fn duplicate_process_handle(process: RawHandle) -> io::Result<OwnedHandle> {
    unsafe { BorrowedHandle::borrow_raw(process) }.try_clone_to_owned()
}

#[cfg(windows)]
fn terminate_process(process: &OwnedHandle) -> io::Result<()> {
    let success =
        unsafe { winapi::um::processthreadsapi::TerminateProcess(process.as_raw_handle(), 1) };
    if success == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn terminate_job_or_process(job: &crate::win::JobObject, process: &OwnedHandle) -> io::Result<()> {
    match job.terminate() {
        Ok(()) => Ok(()),
        Err(job_err) => {
            log::warn!(
                "Windows pipe failed to terminate process tree; terminating root process: \
                 {job_err}"
            );
            terminate_process(process).map_err(|process_err| {
                io::Error::other(format!(
                    "failed to terminate Windows job ({job_err}); root process fallback also \
                     failed: {process_err}"
                ))
            })
        }
    }
}

async fn read_output_stream<R>(mut reader: R, output_tx: mpsc::Sender<Vec<u8>>)
where
    R: AsyncRead + Unpin,
{
    let mut buf = vec![0u8; 8_192];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                let _ = output_tx.send(buf[..n].to_vec()).await;
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

/// On Windows, process-tree containment is best-effort because Tokio returns
/// only after the root process starts, so job assignment cannot be atomic.
async fn spawn_process_with_stdin_mode(
    program: &str,
    args: &[String],
    cwd: &Path,
    env: &HashMap<String, String>,
    arg0: &Option<String>,
    stdin_mode: PipeStdinMode,
    inherited_fds: &[i32],
) -> Result<SpawnedProcess> {
    if program.is_empty() {
        anyhow::bail!("missing program for pipe spawn");
    }

    #[cfg(not(unix))]
    let _ = inherited_fds;

    let mut command = Command::new(program);
    #[cfg(unix)]
    if let Some(arg0) = arg0 {
        command.arg0(arg0);
    }
    #[cfg(target_os = "linux")]
    let parent_pid = unsafe { libc::getpid() };
    #[cfg(unix)]
    let inherited_fds = inherited_fds.to_vec();
    #[cfg(unix)]
    unsafe {
        command.pre_exec(move || {
            crate::process_group::detach_from_tty()?;
            #[cfg(target_os = "linux")]
            crate::process_group::set_parent_death_signal(parent_pid)?;
            crate::pty::close_inherited_fds_except(&inherited_fds);
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
    for arg in args {
        command.arg(arg);
    }
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
    let job = crate::win::JobObject::create().map(Arc::new);
    let mut child = command.spawn()?;
    #[cfg(windows)]
    let windows_terminator = {
        // Accept the small race: a descendant created between spawn and
        // assignment is not guaranteed to join the job and can escape termination.
        let pid = child
            .id()
            .ok_or_else(|| io::Error::other("missing child pid"))?;
        let assigned_job = job.and_then(|job| {
            let process_handle = child
                .raw_handle()
                .ok_or_else(|| io::Error::other("missing child process handle"))?;
            job.assign_process(process_handle)?;
            Ok(job)
        });
        let process_handle = child
            .raw_handle()
            .ok_or_else(|| io::Error::other("missing child process handle"))?;
        let process = match duplicate_process_handle(process_handle) {
            Ok(process) => process,
            Err(err) => {
                if let Ok(job) = &assigned_job {
                    if job.terminate().is_err() {
                        let _ = child.start_kill();
                    }
                } else {
                    let _ = child.start_kill();
                }
                return Err(err.into());
            }
        };
        match assigned_job {
            Ok(job) => WindowsChildTerminator::Job { job, process },
            Err(err) => {
                log::warn!(
                    "Windows pipe process tree containment unavailable for pid {pid}: {err}"
                );
                WindowsChildTerminator::Process(process)
            }
        }
    };
    #[cfg(unix)]
    let process_group_id = child
        .id()
        .ok_or_else(|| io::Error::other("missing child pid"))?;

    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let (writer_tx, mut writer_rx) = mpsc::channel::<Vec<u8>>(128);
    let (stdout_tx, stdout_rx) = mpsc::channel::<Vec<u8>>(128);
    let (stderr_tx, stderr_rx) = mpsc::channel::<Vec<u8>>(128);
    let writer_handle = if let Some(stdin) = stdin {
        tokio::spawn(async move {
            let mut writer = stdin;
            while let Some(bytes) = writer_rx.recv().await {
                let _ = writer.write_all(&bytes).await;
                let _ = writer.flush().await;
            }
        })
    } else {
        drop(writer_rx);
        tokio::spawn(async {})
    };

    let stdout_handle = stdout.map(|stdout| {
        let stdout_tx = stdout_tx.clone();
        tokio::spawn(async move {
            read_output_stream(BufReader::new(stdout), stdout_tx).await;
        })
    });
    let stderr_handle = stderr.map(|stderr| {
        let stderr_tx = stderr_tx.clone();
        tokio::spawn(async move {
            read_output_stream(BufReader::new(stderr), stderr_tx).await;
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
    #[cfg(windows)]
    let wait_job = match &windows_terminator {
        WindowsChildTerminator::Job { job, .. } => Some(Arc::clone(job)),
        WindowsChildTerminator::Process(_) => None,
    };
    let wait_handle: JoinHandle<()> = tokio::spawn(async move {
        let code = match child.wait().await {
            Ok(status) => {
                #[cfg(windows)]
                if let Some(job) = wait_job
                    && let Err(err) = job.preserve_descendants()
                {
                    log::warn!(
                        "Windows pipe failed to preserve descendants after root exit: {err}"
                    );
                }
                exit_code_from_status(status)
            }
            Err(_) => -1,
        };
        wait_exit_status.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Ok(mut guard) = wait_exit_code.lock() {
            *guard = Some(code);
        }
        let _ = exit_tx.send(code);
    });

    let handle = ProcessHandle::new(
        writer_tx,
        Box::new(PipeChildTerminator {
            #[cfg(windows)]
            windows: windows_terminator,
            #[cfg(unix)]
            process_group_id,
        }),
        reader_handle,
        reader_abort_handles,
        writer_handle,
        wait_handle,
        exit_status,
        exit_code,
        /*pty_handles*/ None,
        /*resizer*/ None,
    );

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
    spawn_process_with_stdin_mode(program, args, cwd, env, arg0, PipeStdinMode::Piped, &[]).await
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
    )
    .await
}

#[cfg(all(test, windows))]
#[path = "pipe_tests.rs"]
mod tests;
