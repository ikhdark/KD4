use std::collections::HashMap;
#[cfg(unix)]
use std::fs::File;
use std::io::ErrorKind;
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::fd::FromRawFd;
#[cfg(unix)]
use std::os::fd::RawFd;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::Path;
#[cfg(unix)]
use std::process::Command as StdCommand;
#[cfg(unix)]
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use anyhow::Result;
#[cfg(not(unix))]
use portable_pty::CommandBuilder;
#[cfg(not(any(unix, windows)))]
use portable_pty::native_pty_system;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
#[cfg(not(unix))]
use tokio::task::JoinHandle;

use crate::process::ChildTerminator;
use crate::process::ProcessHandle;
use crate::process::ProcessSignal;
use crate::process::PtyHandles;
use crate::process::PtyMasterHandle;
use crate::process::SharedPtyHandles;
use crate::process::SpawnedProcess;
use crate::process::TerminalSize;
#[cfg(unix)]
use crate::process::exit_code_from_status;
use crate::process::publish_exit_status;


/// Returns true when ConPTY support is available (Windows only).
#[cfg(windows)]
pub fn conpty_supported() -> bool {
    crate::win::conpty_supported()
}

/// Native PTYs are available through `portable-pty` on non-Windows targets.
#[cfg(not(windows))]
pub fn conpty_supported() -> bool {
    true
}

#[cfg(not(unix))]
struct PtyChildTerminator {
    killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
    #[cfg(windows)]
    job: Option<Arc<crate::win::JobObject>>,
}

#[cfg(not(unix))]
impl ChildTerminator for PtyChildTerminator {
    #[cfg(windows)]
    fn replay_control(&self) -> Option<Box<dyn ChildTerminator>> {
        self.job.as_ref().map(|job| Box::new(Arc::clone(job)) as Box<dyn ChildTerminator>)
    }

    #[cfg(windows)]
    fn tree_is_empty(&self) -> std::io::Result<bool> {
        match &self.job {
            Some(job) => job.active_process_count().map(|count| count == 0),
            None => Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "tree accounting unavailable")),
        }
    }

    fn signal(&mut self, signal: ProcessSignal) -> std::io::Result<()> {
        Err(crate::process::unsupported_signal(signal))
    }

    fn kill(&mut self) -> std::io::Result<()> {
        self.killer.kill()
    }
}

#[cfg(unix)]
struct RawPidTerminator {
    process_group: Arc<crate::process::ProcessGroupControl>,
    master: File,
}

#[cfg(unix)]
impl ChildTerminator for RawPidTerminator {
    fn signal(&mut self, signal: ProcessSignal) -> std::io::Result<()> {
        self.process_group.signal_terminal(&self.master, signal)
    }

    fn kill(&mut self) -> std::io::Result<()> {
        self.process_group.kill()
    }
}


#[cfg(not(unix))]
fn platform_native_pty_system() -> Box<dyn portable_pty::PtySystem + Send> {
    #[cfg(windows)]
    {
        Box::new(crate::win::ConPtySystem::default())
    }
    #[cfg(not(windows))]
    {
        native_pty_system()
    }
}

// Keep the receiver asynchronous so aborting the owning ProcessHandle drops the
// queue even when external sender clones remain. At most one native write is in
// flight; terminating/releasing the process closes its PTY and releases that write.
#[cfg(not(unix))]
fn spawn_pty_writer<W>(mut writer: W, mut receiver: mpsc::Receiver<crate::StdinPayload>) -> JoinHandle<()>
where
    W: std::io::Write + Send + 'static,
{
    tokio::spawn(async move {
        #[cfg(windows)]
        let mut windows_input = crate::WindowsTtyInputNormalizer::default();
        while let Some(bytes) = receiver.recv().await {
            #[cfg(windows)]
            let normalized = windows_input.normalize(&bytes);
            let result = tokio::task::spawn_blocking(move || {
                // The blocking operation, not its abortable async waiter, owns
                // custody while native WriteFile can still retain this input.
                let custody = bytes;
                #[cfg(windows)]
                let bytes = &normalized;
                #[cfg(not(windows))]
                let bytes = &custody;
                let result = writer.write_all(bytes).and_then(|()| writer.flush());
                drop(custody);
                (writer, result)
            })
            .await;
            match result {
                Ok((returned_writer, Ok(()))) => writer = returned_writer,
                // A closed or broken input must close the queue, not retain more input.
                Ok((_, Err(_))) | Err(_) => break,
            }
        }
    })
}

// Native readers return WouldBlock when no bytes are available. Queue backpressure and
// retry delays belong to this abortable async owner, never to a lifetime-long worker.
#[cfg(not(unix))]
fn spawn_pty_reader<R>(
    mut reader: R,
    output: mpsc::Sender<Vec<u8>>,
    complete: Arc<AtomicBool>,
) -> JoinHandle<()>
where
    R: std::io::Read + Send + 'static,
{
    tokio::spawn(async move {
        let mut buf = vec![0u8; 8_192];
        loop {
            let result = tokio::task::spawn_blocking(move || {
                let result = reader.read(&mut buf);
                (reader, buf, result)
            })
            .await;
            let Ok((returned_reader, returned_buf, result)) = result else {
                break;
            };
            reader = returned_reader;
            buf = returned_buf;
            match result {
                Ok(0) => {
                    complete.store(true, std::sync::atomic::Ordering::Release);
                    break;
                }
                Ok(n) => {
                    buf.truncate(n);
                    if output.send(buf).await.is_err() {
                        break;
                    }
                    buf = vec![0u8; 8_192];
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(_) => break,
            }
        }
    })
}

#[cfg(unix)]
fn set_nonblocking(fd: RawFd) -> std::io::Result<()> {
    // SAFETY: fcntl receives an owned descriptor and scalar flags only.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Spawn a process attached to a PTY, returning handles for stdin, split output, and exit.
pub async fn spawn_process(
    program: &str,
    args: &[String],
    cwd: &Path,
    env: &HashMap<String, String>,
    arg0: &Option<String>,
    size: TerminalSize,
) -> Result<SpawnedProcess> {
    spawn_process_with_inherited_fds(program, args, cwd, env, arg0, size, &[]).await
}

/// Spawn a process attached to a PTY, preserving any inherited file
/// descriptors listed in `inherited_fds` when supported by the platform backend.
pub async fn spawn_process_with_inherited_fds(
    program: &str,
    args: &[String],
    cwd: &Path,
    env: &HashMap<String, String>,
    arg0: &Option<String>,
    size: TerminalSize,
    inherited_fds: &[i32],
) -> Result<SpawnedProcess> {
    if program.is_empty() {
        anyhow::bail!("missing program for PTY spawn");
    }

    #[cfg(not(unix))]
    let _ = inherited_fds;

    #[cfg(unix)]
    {
        spawn_process_preserving_fds(program, args, cwd, env, arg0, size, inherited_fds).await
    }
    #[cfg(not(unix))]
    {
        let (program, args, cwd, env, arg0) = (
            program.to_owned(),
            args.to_vec(),
            cwd.to_owned(),
            env.clone(),
            arg0.clone(),
        );
        // Setup owns admission even if the observing caller is cancelled while
        // the Windows loader is still running. An unclaimed result drops its
        // ProcessHandle; the detached waiter retains the charge until exit.
        let observation_context = crate::process_observation::ProcessObservationContext::current();
        tokio::spawn(async move {
            let setup = spawn_process_portable(&program, &args, &cwd, &env, &arg0, size);
            match observation_context {
                Some(context) => context.scope(setup).await,
                None => setup.await,
            }
        })
        .await?
    }
}

#[cfg(not(unix))]
async fn spawn_process_portable(
    program: &str,
    args: &[String],
    cwd: &Path,
    env: &HashMap<String, String>,
    arg0: &Option<String>,
    size: TerminalSize,
) -> Result<SpawnedProcess> {
    let admission = crate::ManagedRootProcess::reserve()?;
    let pty_system = platform_native_pty_system();
    let pair = pty_system.openpty(size.into())?;
    let portable_pty::PtyPair { master, slave } = pair;
    // Complete fallible descriptor setup before starting a child or reader task.
    let reader = master.try_clone_reader()?;
    let writer = master.take_writer()?;

    let _ = arg0; // Windows has no distinct Unix argv[0] override.
    let mut command_builder = CommandBuilder::new(program);
    command_builder.cwd(cwd);
    command_builder.env_clear();
    for arg in args {
        command_builder.arg(arg);
    }
    for (key, value) in env {
        command_builder.env(key, value);
    }

    #[cfg(windows)]
    let observation_context = crate::process_observation::ProcessObservationContext::current();
    #[cfg(windows)]
    let (slave, mut child, admission) = tokio::task::spawn_blocking(move || {
        let child = crate::process_observation::ProcessObservationContext::scope_sync(
            observation_context,
            || {
                slave
                    .spawn_command(command_builder)
                    .map_err(std::io::Error::other)
            },
        )?;
        Ok::<_, std::io::Error>((slave, child, admission))
    })
    .await??;
    #[cfg(not(windows))]
    let mut child = slave.spawn_command(command_builder)?;

    #[cfg(windows)]
    let job = (child.as_mut() as &mut dyn portable_pty::Child)
        .downcast_mut::<crate::win::WinChild>()
        .map(|child| child.job());
    let killer = child.clone_killer();

    let (writer_tx, writer_rx) = crate::stdin_channel(128);
    let (stdout_tx, stdout_rx) = mpsc::channel::<Vec<u8>>(128);
    let (_stderr_tx, stderr_rx) = mpsc::channel::<Vec<u8>>(1);
    let capture = crate::process::OutputCapture::default();
    capture
        .stderr
        .store(true, std::sync::atomic::Ordering::Release);
    let reader_handle = spawn_pty_reader(reader, stdout_tx, Arc::clone(&capture.stdout));

    let writer_handle = spawn_pty_writer(writer, writer_rx);

    let pty_handles: SharedPtyHandles = Arc::new(StdMutex::new(Some(PtyHandles {
        _slave: if cfg!(windows) { Some(slave) } else { None },
        _master: PtyMasterHandle::Resizable(master),
    })));
    #[cfg(windows)]
    let wait_pty_handles = Arc::clone(&pty_handles);
    let (exit_tx, exit_rx) = oneshot::channel::<i32>();
    let exit_status = Arc::new(AtomicBool::new(false));
    let wait_exit_status = Arc::clone(&exit_status);
    let exit_code = Arc::new(StdMutex::new(None));
    let wait_exit_code = Arc::clone(&exit_code);
    let wait_handle = tokio::spawn(async move {
        let _admission = admission;
        let code = 'wait: {
            #[cfg(windows)]
            if let Some(native_child) = (child.as_mut() as &mut dyn portable_pty::Child)
                .downcast_mut::<crate::win::WinChild>()
            {
                match native_child.await {
                    Ok(status) => break 'wait status.exit_code() as i32,
                    // Keep ownership and the conservative retry path if native
                    // waiter setup fails; never report a live child as exited.
                    Err(error) => log::warn!("native PTY exit waiter failed: {error}"),
                }
            }
            let mut warned = false;
            loop {
                // Native status checks are short; an idle child never occupies a blocking
                // worker needed to deliver the input that will let it exit.
                let result = match tokio::task::spawn_blocking(move || {
                    let result = child.try_wait();
                    (child, result)
                })
                .await
                {
                    Ok(result) => result,
                    Err(error) => {
                        log::error!("PTY status worker failed: {error}");
                        return;
                    }
                };
                child = result.0;
                match result.1 {
                    Ok(Some(status)) => break status.exit_code() as i32,
                    Ok(None) => {}
                    Err(error) => {
                        if !warned {
                            log::warn!("failed to observe PTY exit; retaining child: {error}");
                            warned = true;
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        publish_exit_status(&wait_exit_status, &wait_exit_code, code);
        let _ = exit_tx.send(code);
        // ConPTY keeps its output pipe open until the pseudoconsole is closed,
        // even after the root exits. Releasing it flushes the final frame and
        // lets the reader observe EOF without every consumer doing so itself.
        #[cfg(windows)]
        crate::process::release_pty_handles(&wait_pty_handles);
    });

    let handle = ProcessHandle::new(
        writer_tx,
        Box::new(PtyChildTerminator {
            killer,
            #[cfg(windows)]
            job,
        }),
        reader_handle,
        Vec::new(),
        writer_handle,
        wait_handle,
        exit_status,
        exit_code,
        Some(pty_handles),
        /*resizer*/ None,
    )
    .with_output_capture(capture);

    Ok(SpawnedProcess {
        session: handle,
        stdout_rx,
        stderr_rx,
        exit_rx,
    })
}

#[cfg(unix)]
async fn spawn_process_preserving_fds(
    program: &str,
    args: &[String],
    cwd: &Path,
    env: &HashMap<String, String>,
    arg0: &Option<String>,
    size: TerminalSize,
    inherited_fds: &[RawFd],
) -> Result<SpawnedProcess> {
    let admission = crate::ManagedRootProcess::reserve()?;
    let (master, slave) = open_unix_pty(size)?;
    let mut command = StdCommand::new(program);
    if let Some(arg0) = arg0 {
        command.arg0(arg0);
    }
    command.current_dir(cwd);
    command.env_clear();
    command.args(args);
    command.envs(env);

    let stdin = slave.try_clone()?;
    let stdout = slave.try_clone()?;
    let stderr = slave.try_clone()?;
    let mut inherited_fds = inherited_fds.to_vec();
    inherited_fds.sort_unstable();

    // SAFETY: The child callback uses native signal, terminal, and session operations plus
    // async-signal-safe descriptor marking. All Rust buffers are prepared before fork.
    unsafe {
        command
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .pre_exec(move || {
                for signo in &[
                    libc::SIGCHLD,
                    libc::SIGHUP,
                    libc::SIGINT,
                    libc::SIGQUIT,
                    libc::SIGTERM,
                    libc::SIGALRM,
                ] {
                    libc::signal(*signo, libc::SIG_DFL);
                }

                let empty_set: libc::sigset_t = std::mem::zeroed();
                libc::sigprocmask(libc::SIG_SETMASK, &empty_set, std::ptr::null_mut());

                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                #[allow(clippy::cast_lossless)]
                if libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }

                mark_inherited_fds_cloexec_except(&inherited_fds);
                Ok(())
            });
    }

    // Finish all fallible parent-side PTY setup before spawning so an error
    // cannot leave a live child without a ProcessHandle.
    // All duplicates share O_NONBLOCK; both read and write paths use readiness below.
    set_nonblocking(master.as_raw_fd())?;
    let reader = tokio::io::unix::AsyncFd::new(master.try_clone()?)?;
    let writer = tokio::io::unix::AsyncFd::new(master.try_clone()?)?;
    let signal_master = master.try_clone()?;

    let mut child = command.spawn()?;
    let observation = crate::process_observation::observe_process_start(
        "unified_unix_pty",
        crate::process_observation::ProcessRole::RequestedRoot,
        Some(child.id()),
        None,
    );
    drop(slave);
    let process_group = Arc::new(crate::process::ProcessGroupControl::new(child.id()));

    let (writer_tx, writer_rx) = crate::stdin_channel(128);
    let (stdout_tx, stdout_rx) = mpsc::channel::<Vec<u8>>(128);
    let (_stderr_tx, stderr_rx) = mpsc::channel::<Vec<u8>>(1);
    let capture = crate::process::OutputCapture::default();
    capture
        .stderr
        .store(true, std::sync::atomic::Ordering::Release);
    let complete = Arc::clone(&capture.stdout);
    let reader_handle = tokio::spawn(async move {
        use std::io::Read;
        let mut buf = [0u8; 8_192];
        while let Ok(mut ready) = reader.readable().await {
            match ready.try_io(|fd| fd.get_ref().read(&mut buf)) {
                Ok(Ok(0)) => {
                    complete.store(true, std::sync::atomic::Ordering::Release);
                    break;
                }
                Ok(Ok(n)) => {
                    if stdout_tx.send(buf[..n].to_vec()).await.is_err() {
                        break;
                    }
                }
                Ok(Err(error)) if error.kind() == ErrorKind::Interrupted => continue,
                // Unix PTY masters report EIO when the slave has closed.
                Ok(Err(error)) if error.raw_os_error() == Some(libc::EIO) => {
                    complete.store(true, std::sync::atomic::Ordering::Release);
                    break;
                }
                Ok(Err(_)) => break,
                Err(_) => continue,
            }
        }
    });

    let (stdin_close_tx, stdin_close_rx) = tokio::sync::watch::channel(None);
    let writer_handle = tokio::spawn(async move {
        let result = write_unix_pty_stdin(writer, writer_rx).await;
        stdin_close_tx.send_replace(Some(
            result.map_err(|error| (error.kind(), error.to_string())),
        ));
    });

    let (exit_tx, exit_rx) = oneshot::channel::<i32>();
    let exit_status = Arc::new(AtomicBool::new(false));
    let wait_exit_status = Arc::clone(&exit_status);
    let exit_code = Arc::new(StdMutex::new(None));
    let wait_exit_code = Arc::clone(&exit_code);
    let wait_group = Arc::clone(&process_group);
    let wait_handle = tokio::spawn(async move {
        let _admission = admission;
        wait_group.disarm_after_exit().await;
        let mut warned = false;
        let code = loop {
            match child.try_wait() {
                Ok(Some(status)) => break exit_code_from_status(status),
                Ok(None) => {}
                Err(error) => {
                    if !warned {
                        log::warn!("failed to reap PTY child; retaining owner: {error}");
                        warned = true;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        observation.cleanup_observed("root_reaped; descendants_unknown");
        publish_exit_status(&wait_exit_status, &wait_exit_code, code);
        let _ = exit_tx.send(code);
    });

    let pty_handles: SharedPtyHandles = Arc::new(StdMutex::new(Some(PtyHandles {
        _slave: None,
        _master: PtyMasterHandle::Owned(master),
    })));

    let handle = ProcessHandle::new(
        writer_tx,
        Box::new(RawPidTerminator {
            process_group,
            master: signal_master,
        }),
        reader_handle,
        Vec::new(),
        writer_handle,
        wait_handle,
        exit_status,
        exit_code,
        Some(pty_handles),
        /*resizer*/ None,
    )
    .with_output_capture(capture)
    .with_stdin_close_receipt(stdin_close_rx);

    Ok(SpawnedProcess {
        session: handle,
        stdout_rx,
        stderr_rx,
        exit_rx,
    })
}

#[cfg(unix)]
async fn write_unix_pty_bytes(
    writer: &tokio::io::unix::AsyncFd<File>,
    mut bytes: &[u8],
) -> std::io::Result<()> {
    use std::io::Write;
    while !bytes.is_empty() {
        let mut ready = writer.writable().await?;
        match ready.try_io(|fd| fd.get_ref().write(bytes)) {
            Ok(Ok(0)) => return Err(std::io::Error::from(ErrorKind::WriteZero)),
            Ok(Ok(n)) => bytes = &bytes[n..],
            Ok(Err(error)) if error.kind() == ErrorKind::Interrupted => continue,
            Ok(Err(error)) => return Err(error),
            Err(_) => continue,
        }
    }
    Ok(())
}

#[cfg(unix)]
async fn write_unix_pty_stdin(
    writer: tokio::io::unix::AsyncFd<File>,
    mut input: mpsc::Receiver<crate::StdinPayload>,
) -> std::io::Result<()> {
    while let Some(bytes) = input.recv().await {
        write_unix_pty_bytes(&writer, &bytes).await?;
    }
    // Canonical state can contain pending literal-next input or change in the
    // child after any termios snapshot. Injected VEOF can be data or a signal,
    // never proof of a pipe-like half-close. Drain accepted input without it.
    Err(std::io::Error::new(
        ErrorKind::Unsupported,
        "terminal input cannot be half-closed reliably; use pipe stdin for EOF",
    ))
}

#[cfg(unix)]
fn open_unix_pty(size: TerminalSize) -> Result<(File, File)> {
    let mut master: RawFd = -1;
    let mut slave: RawFd = -1;
    let mut size = libc::winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };

    // SAFETY: master, slave, and size are writable storage of the required types; the null name
    // and termios arguments are optional.
    let result = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::addr_of_mut!(size),
        )
    };
    if result != 0 {
        anyhow::bail!("failed to openpty: {:?}", std::io::Error::last_os_error());
    }

    // SAFETY: Successful openpty returned this new master descriptor, which is transferred into
    // exactly one File owner.
    let master = unsafe { File::from_raw_fd(master) };
    // SAFETY: Successful openpty returned this new slave descriptor, distinct from master,
    // which is transferred into one File owner.
    let slave = unsafe { File::from_raw_fd(slave) };
    Ok(configure_owned_pty_files(master, slave, |file| {
        set_cloexec(file.as_raw_fd())
    })?)
}

#[cfg(unix)]
fn set_cloexec(fd: RawFd) -> std::io::Result<()> {
    // SAFETY: F_GETFD consumes only the scalar descriptor and requires no variadic pointer
    // argument; invalid descriptors return an error.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: F_SETFD consumes the scalar descriptor flags, and no caller-owned memory is
    // accessed.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
fn configure_owned_pty_files<T>(
    master: T,
    slave: T,
    mut configure: impl FnMut(&T) -> std::io::Result<()>,
) -> std::io::Result<(T, T)> {
    configure(&master)?;
    configure(&slave)?;
    Ok((master, slave))
}


#[cfg(unix)]
pub(crate) fn mark_inherited_fds_cloexec_except(preserved_fds: &[RawFd]) {
    // This runs after fork: close_fds uses stack storage and native descriptor operations,
    // without allocating or taking library locks. Marking descriptors preserves Command's
    // close-on-exec error pipe until exec succeeds or the spawn error is reported.
    close_fds::set_fds_cloexec(3, preserved_fds);
}
