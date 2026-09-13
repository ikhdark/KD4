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
use portable_pty::CommandBuilder;
#[cfg(not(windows))]
use portable_pty::native_pty_system;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

#[cfg(windows)]
use crate::WINDOWS_PROCESS_OPERATION_TIMEOUT;
use crate::process::ChildTerminator;
use crate::process::ProcessHandle;
use crate::process::ProcessSignal;
use crate::process::PtyHandles;
use crate::process::PtyMasterHandle;
use crate::process::SpawnedProcess;
use crate::process::TerminalSize;
#[cfg(unix)]
use crate::process::exit_code_from_status;
use crate::process::publish_exit_status;
#[cfg(windows)]
use crate::run_windows_process_operation;

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

struct PtyChildTerminator {
    killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
    #[cfg(unix)]
    process_group_id: Option<u32>,
}

impl ChildTerminator for PtyChildTerminator {
    fn signal(&mut self, signal: ProcessSignal) -> std::io::Result<()> {
        match signal {
            ProcessSignal::Interrupt => {
                #[cfg(unix)]
                if let Some(process_group_id) = self.process_group_id {
                    return crate::process_group::interrupt_process_group(process_group_id);
                }
                Err(crate::process::unsupported_signal(signal))
            }
        }
    }

    fn kill(&mut self) -> std::io::Result<()> {
        #[cfg(unix)]
        if let Some(process_group_id) = self.process_group_id {
            let process_group_kill_result =
                crate::process_group::kill_process_group(process_group_id);
            let child_kill_result = self.killer.kill();
            return match child_kill_result {
                Ok(()) => Ok(()),
                Err(err) if err.kind() == ErrorKind::NotFound => process_group_kill_result,
                Err(err) => process_group_kill_result.or(Err(err)),
            };
        }
        self.killer.kill()
    }
}

#[cfg(unix)]
struct RawPidTerminator {
    process_group_id: u32,
}

#[cfg(unix)]
impl ChildTerminator for RawPidTerminator {
    fn signal(&mut self, signal: ProcessSignal) -> std::io::Result<()> {
        match signal {
            ProcessSignal::Interrupt => {
                crate::process_group::interrupt_process_group(self.process_group_id)
            }
        }
    }

    fn kill(&mut self) -> std::io::Result<()> {
        crate::process_group::kill_process_group(self.process_group_id)
    }
}

#[cfg(all(test, windows))]
thread_local! {
    static TEST_PTY_SYSTEM: std::cell::RefCell<Option<Box<dyn portable_pty::PtySystem + Send>>> =
        std::cell::RefCell::new(None);
}

fn platform_native_pty_system() -> Box<dyn portable_pty::PtySystem + Send> {
    #[cfg(all(test, windows))]
    if let Some(system) = TEST_PTY_SYSTEM.with(|system| system.borrow_mut().take()) {
        return system;
    }
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
fn spawn_pty_writer<W>(mut writer: W, mut receiver: mpsc::Receiver<Vec<u8>>) -> JoinHandle<()>
where
    W: std::io::Write + Send + 'static,
{
    tokio::spawn(async move {
        #[cfg(windows)]
        let mut windows_input = crate::WindowsTtyInputNormalizer::default();
        while let Some(bytes) = receiver.recv().await {
            #[cfg(windows)]
            let bytes = windows_input.normalize(&bytes);
            let result = tokio::task::spawn_blocking(move || {
                let result = writer.write_all(&bytes).and_then(|()| writer.flush());
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
    if !inherited_fds.is_empty() {
        return spawn_process_preserving_fds(program, args, cwd, env, arg0, size, inherited_fds)
            .await;
    }

    spawn_process_portable(program, args, cwd, env, arg0, size).await
}

async fn spawn_process_portable(
    program: &str,
    args: &[String],
    cwd: &Path,
    env: &HashMap<String, String>,
    arg0: &Option<String>,
    size: TerminalSize,
) -> Result<SpawnedProcess> {
    let pty_system = platform_native_pty_system();
    let pair = pty_system.openpty(size.into())?;
    let portable_pty::PtyPair { master, slave } = pair;
    // Complete fallible descriptor setup before starting a child or reader task.
    let mut reader = master.try_clone_reader()?;
    let writer = master.take_writer()?;

    let mut command_builder = CommandBuilder::new(arg0.as_ref().unwrap_or(&program.to_string()));
    command_builder.cwd(cwd);
    command_builder.env_clear();
    for arg in args {
        command_builder.arg(arg);
    }
    for (key, value) in env {
        command_builder.env(key, value);
    }

    #[cfg(windows)]
    let (slave, mut child) =
        run_windows_process_operation(WINDOWS_PROCESS_OPERATION_TIMEOUT, move || {
            let child = slave
                .spawn_command(command_builder)
                .map_err(std::io::Error::other)?;
            Ok((slave, child))
        })
        .await?;
    #[cfg(not(windows))]
    let mut child = slave.spawn_command(command_builder)?;

    #[cfg(unix)]
    let process_group_id = child.process_id();

    let killer = child.clone_killer();

    let (writer_tx, writer_rx) = mpsc::channel::<Vec<u8>>(128);
    let (stdout_tx, stdout_rx) = mpsc::channel::<Vec<u8>>(128);
    let (_stderr_tx, stderr_rx) = mpsc::channel::<Vec<u8>>(1);
    let reader_handle: JoinHandle<()> = tokio::task::spawn_blocking(move || {
        let mut buf = [0u8; 8_192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let _ = stdout_tx.blocking_send(buf[..n].to_vec());
                }
                Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(_) => break,
            }
        }
    });

    let writer_handle = spawn_pty_writer(writer, writer_rx);

    let (exit_tx, exit_rx) = oneshot::channel::<i32>();
    let exit_status = Arc::new(AtomicBool::new(false));
    let wait_exit_status = Arc::clone(&exit_status);
    let exit_code = Arc::new(StdMutex::new(None));
    let wait_exit_code = Arc::clone(&exit_code);
    let wait_handle: JoinHandle<()> = tokio::task::spawn_blocking(move || {
        let code = match child.wait() {
            Ok(status) => status.exit_code() as i32,
            Err(_) => -1,
        };
        publish_exit_status(&wait_exit_status, &wait_exit_code, code);
        let _ = exit_tx.send(code);
    });

    let handles = PtyHandles {
        _slave: if cfg!(windows) { Some(slave) } else { None },
        _master: PtyMasterHandle::Resizable(master),
    };

    let handle = ProcessHandle::new(
        writer_tx,
        Box::new(PtyChildTerminator {
            killer,
            #[cfg(unix)]
            process_group_id,
        }),
        reader_handle,
        Vec::new(),
        writer_handle,
        wait_handle,
        exit_status,
        exit_code,
        Some(handles),
        /*resizer*/ None,
    );

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
    let mut reader = master.try_clone()?;
    let writer = master.try_clone()?;

    let mut child = command.spawn()?;
    drop(slave);
    let process_group_id = child.id();

    let (writer_tx, writer_rx) = mpsc::channel::<Vec<u8>>(128);
    let (stdout_tx, stdout_rx) = mpsc::channel::<Vec<u8>>(128);
    let (_stderr_tx, stderr_rx) = mpsc::channel::<Vec<u8>>(1);
    let reader_handle: JoinHandle<()> = tokio::task::spawn_blocking(move || {
        let mut buf = [0u8; 8_192];
        loop {
            match std::io::Read::read(&mut reader, &mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let _ = stdout_tx.blocking_send(buf[..n].to_vec());
                }
                Err(ref error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(ref error) if error.kind() == ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(_) => break,
            }
        }
    });

    let writer_handle = spawn_pty_writer(writer, writer_rx);

    let (exit_tx, exit_rx) = oneshot::channel::<i32>();
    let exit_status = Arc::new(AtomicBool::new(false));
    let wait_exit_status = Arc::clone(&exit_status);
    let exit_code = Arc::new(StdMutex::new(None));
    let wait_exit_code = Arc::clone(&exit_code);
    let wait_handle: JoinHandle<()> = tokio::task::spawn_blocking(move || {
        let code = match child.wait() {
            Ok(status) => exit_code_from_status(status),
            Err(_) => -1,
        };
        publish_exit_status(&wait_exit_status, &wait_exit_code, code);
        let _ = exit_tx.send(code);
    });

    let handles = PtyHandles {
        _slave: None,
        _master: PtyMasterHandle::Opaque {
            raw_fd: master.as_raw_fd(),
            _handle: Box::new(master),
        },
    };

    let handle = ProcessHandle::new(
        writer_tx,
        Box::new(RawPidTerminator { process_group_id }),
        reader_handle,
        Vec::new(),
        writer_handle,
        wait_handle,
        exit_status,
        exit_code,
        Some(handles),
        /*resizer*/ None,
    );

    Ok(SpawnedProcess {
        session: handle,
        stdout_rx,
        stderr_rx,
        exit_rx,
    })
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

#[cfg(any(unix, test))]
fn configure_owned_pty_files<T>(
    master: T,
    slave: T,
    mut configure: impl FnMut(&T) -> std::io::Result<()>,
) -> std::io::Result<(T, T)> {
    configure(&master)?;
    configure(&slave)?;
    Ok((master, slave))
}

#[cfg(test)]
mod pty_fd_tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use super::configure_owned_pty_files;

    #[cfg(windows)]
    #[tokio::test(flavor = "current_thread")]
    async fn public_pty_descriptor_failure_does_not_spawn_child() -> anyhow::Result<()> {
        struct PreparedSystem(std::sync::Mutex<Option<portable_pty::PtyPair>>);
        impl portable_pty::PtySystem for PreparedSystem {
            fn openpty(&self, _: portable_pty::PtySize) -> anyhow::Result<portable_pty::PtyPair> {
                Ok(self.0.lock().unwrap().take().expect("one native pair"))
            }
        }
        struct ObservedSlave {
            native: Box<dyn portable_pty::SlavePty + Send>,
            spawn_count: Arc<AtomicUsize>,
        }
        impl portable_pty::SlavePty for ObservedSlave {
            fn spawn_command(
                &self,
                command: portable_pty::CommandBuilder,
            ) -> anyhow::Result<Box<dyn portable_pty::Child + Send + Sync>> {
                self.spawn_count.fetch_add(1, Ordering::SeqCst);
                self.native.spawn_command(command)
            }
        }

        let size = super::TerminalSize::default();
        let pair = super::platform_native_pty_system().openpty(size.into())?;
        // Create a real native descriptor-acquisition failure, not an injected result.
        drop(pair.master.take_writer()?);
        let spawn_count = Arc::new(AtomicUsize::new(0));
        let pair = portable_pty::PtyPair {
            master: pair.master,
            slave: Box::new(ObservedSlave {
                native: pair.slave,
                spawn_count: Arc::clone(&spawn_count),
            }),
        };
        super::TEST_PTY_SYSTEM.with(|system| {
            *system.borrow_mut() =
                Some(Box::new(PreparedSystem(std::sync::Mutex::new(Some(pair)))));
        });
        let result = super::spawn_process(
            "cmd.exe",
            &["/D".to_string(), "/C".to_string(), "exit 0".to_string()],
            &std::env::current_dir()?,
            &std::env::vars().collect(),
            &None,
            size,
        )
        .await;
        let error = match result {
            Ok(_) => anyhow::bail!("consumed native writer unexpectedly allowed process setup"),
            Err(error) => error,
        };
        assert_eq!(error.to_string(), "writer already taken");
        assert_eq!(
            spawn_count.load(Ordering::SeqCst),
            0,
            "descriptor failure must precede any native child creation"
        );
        assert!(super::TEST_PTY_SYSTEM.with(|system| system.borrow().is_none()));
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn public_pipe_and_pty_spawns_preserve_only_requested_descriptors() -> anyhow::Result<()>
    {
        use std::os::fd::AsRawFd;
        use std::os::fd::FromRawFd;
        use std::os::fd::OwnedFd;
        use std::time::Duration;

        let source = std::fs::File::open("/dev/null")?;
        let duplicate = |minimum| -> std::io::Result<OwnedFd> {
            // SAFETY: source owns a live descriptor; F_DUPFD returns a separate inheritable
            // descriptor, with failure checked before ownership is transferred.
            let fd = unsafe { libc::fcntl(source.as_raw_fd(), libc::F_DUPFD, minimum) };
            if fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // SAFETY: successful F_DUPFD created a new descriptor with no other owner.
            Ok(unsafe { OwnedFd::from_raw_fd(fd) })
        };
        let preserved = duplicate(128)?;
        let excluded = duplicate(192)?;
        let script = format!(
            "test -e /dev/fd/{} && test ! -e /dev/fd/{} || exit 79; printf descriptor-contract-ok",
            preserved.as_raw_fd(),
            excluded.as_raw_fd()
        );
        let args = ["-c".to_string(), script];
        let cwd = std::env::current_dir()?;
        let env = std::env::vars().collect();
        let preserved_fds = [preserved.as_raw_fd()];
        for use_pty in [false, true] {
            let spawned = if use_pty {
                crate::pty::spawn_process_with_inherited_fds(
                    "/bin/sh",
                    &args,
                    &cwd,
                    &env,
                    &None,
                    crate::TerminalSize::default(),
                    &preserved_fds,
                )
                .await?
            } else {
                crate::pipe::spawn_process_no_stdin_with_inherited_fds(
                    "/bin/sh",
                    &args,
                    &cwd,
                    &env,
                    &None,
                    &preserved_fds,
                )
                .await?
            };
            let crate::SpawnedProcess {
                session,
                mut stdout_rx,
                exit_rx,
                ..
            } = spawned;
            let mut output = Vec::new();
            tokio::time::timeout(Duration::from_secs(5), async {
                while output.len() < b"descriptor-contract-ok".len() {
                    let Some(chunk) = stdout_rx.recv().await else {
                        break;
                    };
                    output.extend(chunk);
                }
            })
            .await?;
            assert_eq!(output, b"descriptor-contract-ok");
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(5), exit_rx).await??,
                0
            );
            drop(session);

            for fd in [preserved.as_raw_fd(), excluded.as_raw_fd()] {
                // SAFETY: the parent still owns both descriptors; F_GETFD only reads flags.
                let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
                assert_eq!(
                    flags, 0,
                    "child filtering must not change parent descriptors"
                );
            }

            let missing = "/codex-missing-executable-for-fd-filter-test";
            let result = if use_pty {
                crate::pty::spawn_process_with_inherited_fds(
                    missing,
                    &[],
                    &cwd,
                    &env,
                    &None,
                    crate::TerminalSize::default(),
                    &preserved_fds,
                )
                .await
            } else {
                crate::pipe::spawn_process_no_stdin_with_inherited_fds(
                    missing,
                    &[],
                    &cwd,
                    &env,
                    &None,
                    &preserved_fds,
                )
                .await
            };
            assert!(
                result.is_err(),
                "the spawn error pipe must survive until exec fails"
            );
        }
        Ok(())
    }

    struct TrackedDescriptor {
        drop_count: Arc<AtomicUsize>,
    }

    impl Drop for TrackedDescriptor {
        fn drop(&mut self) {
            self.drop_count.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct ControlledWriter {
        entered: Option<tokio::sync::oneshot::Sender<()>>,
        release: std::sync::mpsc::Receiver<()>,
        output: Arc<std::sync::Mutex<Vec<u8>>>,
        dropped: Option<tokio::sync::oneshot::Sender<()>>,
    }

    impl std::io::Write for ControlledWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if let Some(entered) = self.entered.take() {
                let _ = entered.send(());
                self.release
                    .recv_timeout(std::time::Duration::from_secs(3))
                    .map_err(std::io::Error::other)?;
            }
            self.output.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.output.lock().unwrap().push(b'|');
            Ok(())
        }
    }

    impl Drop for ControlledWriter {
        fn drop(&mut self) {
            if let Some(dropped) = self.dropped.take() {
                let _ = dropped.send(());
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn pty_writer_keeps_executor_responsive_and_preserves_order() {
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
        let output = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        let handle = super::spawn_pty_writer(
            ControlledWriter {
                entered: Some(entered_tx),
                release: release_rx,
                output: Arc::clone(&output),
                dropped: Some(dropped_tx),
            },
            receiver,
        );
        let started = std::time::Instant::now();
        sender.send(b"first".to_vec()).await.unwrap();
        entered_rx.await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        assert!(output.lock().unwrap().is_empty());
        release_tx.send(()).unwrap();
        sender.send(b"second".to_vec()).await.unwrap();
        drop(sender);
        tokio::time::timeout(std::time::Duration::from_secs(2), handle)
            .await
            .unwrap()
            .unwrap();
        dropped_rx.await.unwrap();
        assert_eq!(output.lock().unwrap().as_slice(), b"first|second|");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn pty_writer_abort_closes_queue_and_releases_inflight_writer() {
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
        let output = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        let handle = super::spawn_pty_writer(
            ControlledWriter {
                entered: Some(entered_tx),
                release: release_rx,
                output: Arc::clone(&output),
                dropped: Some(dropped_tx),
            },
            receiver,
        );
        sender.send(b"inflight".to_vec()).await.unwrap();
        entered_rx.await.unwrap();
        sender.send(b"must not write".to_vec()).await.unwrap();
        handle.abort();
        assert!(handle.await.unwrap_err().is_cancelled());
        assert!(sender.send(b"late".to_vec()).await.is_err());
        release_tx.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), dropped_rx)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(output.lock().unwrap().as_slice(), b"inflight|");
    }

    #[test]
    fn cloexec_failure_drops_both_owned_pty_descriptors() {
        for failing_call in [1, 2] {
            let drop_count = Arc::new(AtomicUsize::new(0));
            let mut call_count = 0;
            let result = configure_owned_pty_files(
                TrackedDescriptor {
                    drop_count: Arc::clone(&drop_count),
                },
                TrackedDescriptor {
                    drop_count: Arc::clone(&drop_count),
                },
                |_| {
                    call_count += 1;
                    if call_count == failing_call {
                        Err(std::io::Error::other("injected CLOEXEC failure"))
                    } else {
                        Ok(())
                    }
                },
            );

            assert!(result.is_err());
            assert_eq!(call_count, failing_call);
            assert_eq!(drop_count.load(Ordering::SeqCst), 2);
        }
    }
}

#[cfg(unix)]
pub(crate) fn mark_inherited_fds_cloexec_except(preserved_fds: &[RawFd]) {
    // This runs after fork: close_fds uses stack storage and native descriptor operations,
    // without allocating or taking library locks. Marking descriptors preserves Command's
    // close-on-exec error pipe until exec succeeds or the spawn error is reported.
    close_fds::set_fds_cloexec(3, preserved_fds);
}
