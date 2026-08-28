use std::collections::HashMap;

use std::io::ErrorKind;

use std::path::Path;

use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use anyhow::Result;
use portable_pty::CommandBuilder;

use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::WINDOWS_PROCESS_OPERATION_TIMEOUT;
use crate::process::ChildTerminator;
use crate::process::ProcessHandle;
use crate::process::ProcessSignal;
use crate::process::PtyHandles;
use crate::process::PtyMasterHandle;
use crate::process::SpawnedProcess;
use crate::process::TerminalSize;
use crate::run_windows_process_operation;

/// Returns true when ConPTY support is available (Windows only).
pub fn conpty_supported() -> bool {
    crate::win::conpty_supported()
}

struct PtyChildTerminator {
    killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
}

impl ChildTerminator for PtyChildTerminator {
    fn signal(&mut self, signal: ProcessSignal) -> std::io::Result<()> {
        match signal {
            ProcessSignal::Interrupt => Err(crate::process::unsupported_signal(signal)),
        }
    }

    fn kill(&mut self) -> std::io::Result<()> {
        self.killer.kill()
    }
}

fn platform_native_pty_system() -> Box<dyn portable_pty::PtySystem + Send> {
    Box::new(crate::win::ConPtySystem::default())
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

    let _ = inherited_fds;

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

    let mut command_builder = CommandBuilder::new(arg0.as_ref().unwrap_or(&program.to_string()));
    command_builder.cwd(cwd);
    command_builder.env_clear();
    for arg in args {
        command_builder.arg(arg);
    }
    for (key, value) in env {
        command_builder.env(key, value);
    }

    let (slave, mut child) =
        run_windows_process_operation(WINDOWS_PROCESS_OPERATION_TIMEOUT, move || {
            let child = slave
                .spawn_command(command_builder)
                .map_err(std::io::Error::other)?;
            Ok((slave, child))
        })
        .await?;

    let killer = child.clone_killer();

    let (writer_tx, mut writer_rx) = mpsc::channel::<Vec<u8>>(128);
    let (stdout_tx, stdout_rx) = mpsc::channel::<Vec<u8>>(128);
    let (_stderr_tx, stderr_rx) = mpsc::channel::<Vec<u8>>(1);
    let mut reader = master.try_clone_reader()?;
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

    let writer = master.take_writer()?;
    let writer = Arc::new(tokio::sync::Mutex::new(writer));
    let writer_handle: JoinHandle<()> = tokio::spawn({
        let writer = Arc::clone(&writer);
        async move {
            let mut windows_input = crate::WindowsTtyInputNormalizer::default();
            while let Some(bytes) = writer_rx.recv().await {
                let bytes = windows_input.normalize(&bytes);
                let mut guard = writer.lock().await;
                use std::io::Write;
                let _ = guard.write_all(&bytes);
                let _ = guard.flush();
            }
        }
    });

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
        wait_exit_status.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Ok(mut guard) = wait_exit_code.lock() {
            *guard = Some(code);
        }
        let _ = exit_tx.send(code);
    });

    let handles = PtyHandles {
        _slave: Some(slave),
        _master: PtyMasterHandle::Resizable(master),
    };

    let handle = ProcessHandle::new(
        writer_tx,
        Box::new(PtyChildTerminator { killer }),
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

#[cfg(test)]
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

    struct TrackedDescriptor {
        drop_count: Arc<AtomicUsize>,
    }

    impl Drop for TrackedDescriptor {
        fn drop(&mut self) {
            self.drop_count.fetch_add(1, Ordering::SeqCst);
        }
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
