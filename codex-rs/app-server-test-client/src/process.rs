use std::env;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::time::Duration;
use std::time::Instant;

use std::process::Stdio;

use anyhow::Context;
use anyhow::Result;

use anyhow::bail;

pub(super) fn runtime_dir() -> PathBuf {
    env::temp_dir().join("codex-app-server-test-client")
}

pub(super) fn add_codex_parent_to_path(cmd: &mut Command, codex_bin: &Path) -> Result<()> {
    let Some(codex_bin_parent) = codex_bin
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        return Ok(());
    };

    let mut paths = vec![codex_bin_parent.to_path_buf()];
    if let Some(existing_path) = env::var_os("PATH") {
        paths.extend(env::split_paths(&existing_path));
    }
    let path = env::join_paths(paths).context("failed to build PATH for app-server child")?;
    cmd.env("PATH", path);
    Ok(())
}

pub(super) fn build_serve_command(
    codex_bin: &Path,
    config_overrides: &[String],
    listen: &str,
) -> Result<Command> {
    {
        use std::os::windows::process::CommandExt;

        let mut command = build_direct_codex_command(codex_bin, config_overrides)?;
        command.arg("--listen").arg(listen);
        command.creation_flags(0x0000_0200 | 0x0800_0000); // CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW
        Ok(command)
    }
}

fn build_direct_codex_command(codex_bin: &Path, config_overrides: &[String]) -> Result<Command> {
    let mut command = Command::new(codex_bin);
    add_codex_parent_to_path(&mut command, codex_bin)?;
    command.env("RUST_BACKTRACE", "full");
    if env::var_os("RUST_LOG").is_none() {
        command.env("RUST_LOG", "warn");
    }
    for override_kv in config_overrides {
        command.arg("--config").arg(override_kv);
    }
    command.arg("app-server");
    Ok(command)
}

fn build_windows_serve_preflight_command(
    codex_bin: &Path,
    config_overrides: &[String],
) -> Result<Command> {
    let mut command = build_direct_codex_command(codex_bin, config_overrides)?;
    command
        .arg("--help")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    Ok(command)
}

pub(super) fn preflight_serve_restart(
    codex_bin: &Path,
    config_overrides: &[String],
    deadline: Instant,
) -> Result<()> {
    {
        let status = run_helper_until(&mut build_windows_serve_preflight_command(codex_bin, config_overrides)?, deadline, false)
            .map(|output| output.status)
            .with_context(|| {
                format!(
                    "failed to launch `{}` for app-server restart preflight; existing listeners were not changed",
                    codex_bin.display()
                )
            })?;
        if !status.success() {
            bail!(
                "`{} app-server --help` failed with {status}; existing listeners were not changed",
                codex_bin.display()
            );
        }
        Ok(())
    }
}

pub(super) fn start_prepared_serve<T>(
    kill: bool,
    preflight: impl FnOnce() -> Result<()>,
    kill_existing: impl FnOnce() -> Result<()>,
    spawn: impl FnOnce() -> Result<T>,
) -> Result<T> {
    if kill {
        preflight()?;
        kill_existing()?;
    }
    spawn()
}

pub(super) fn listener_pids_on_port(port: u16, deadline: Instant) -> Result<Vec<u32>> {
    let output = run_helper_until(
        Command::new("netstat").arg("-ano").arg("-p").arg("tcp"),
        deadline,
        true,
    )
    .with_context(|| format!("failed to run netstat for port {port}"))?;
    if !output.status.success() {
        bail!(
            "netstat failed for port {port}: {}; {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(parse_windows_listener_pids(
        &String::from_utf8_lossy(&output.stdout),
        port,
    ))
}

fn parse_windows_listener_pids(output: &str, port: u16) -> Vec<u32> {
    sorted_unique_pids(output.lines().filter_map(|line| {
        let columns = line.split_whitespace().collect::<Vec<_>>();
        let [protocol, local_address, _foreign_address, state, pid] = columns.as_slice() else {
            return None;
        };
        if !protocol.eq_ignore_ascii_case("TCP")
            || !state.eq_ignore_ascii_case("LISTENING")
            || !address_has_port(local_address, port)
        {
            return None;
        }
        pid.parse::<u32>().ok()
    }))
}

fn address_has_port(address: &str, port: u16) -> bool {
    address
        .rsplit_once(':')
        .and_then(|(_, value)| value.parse::<u16>().ok())
        == Some(port)
}

fn sorted_unique_pids(pids: impl IntoIterator<Item = u32>) -> Vec<u32> {
    let mut pids = pids.into_iter().collect::<Vec<_>>();
    pids.sort_unstable();
    pids.dedup();
    pids
}

pub(super) fn terminate_process(
    pid: u32,
    force: bool,
    deadline: Instant,
) -> Result<std::process::ExitStatus> {
    let mut command = Command::new("taskkill");
    command.arg("/PID").arg(pid.to_string()).arg("/T");
    if force {
        command.arg("/F");
    }
    run_helper_until(&mut command, deadline, false)
        .map(|output| output.status)
        .with_context(|| {
            format!(
                "failed to {} pid {pid}",
                if force {
                    "force terminate"
                } else {
                    "terminate"
                }
            )
        })
}

fn run_helper_until(command: &mut Command, deadline: Instant, capture: bool) -> Result<Output> {
    use std::os::windows::process::CommandExt;
    if Instant::now() >= deadline {
        bail!("restart operation deadline expired before launching helper");
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(if capture {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stderr(if capture {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .creation_flags(0x0800_0000)
        .spawn()
        .context("launch restart helper")?;
    if !capture {
        return Ok(Output {
            status: wait_helper_until(&mut child, deadline)?,
            stdout: Vec::new(),
            stderr: Vec::new(),
        });
    }
    let read_output = |reader: Box<dyn std::io::Read + Send>| {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            use std::io::Read;
            let mut bytes = Vec::new();
            let result = reader
                .take(8 * 1024 * 1024 + 1)
                .read_to_end(&mut bytes)
                .and_then(|_| {
                    if bytes.len() > 8 * 1024 * 1024 {
                        Err(std::io::Error::other("restart helper output exceeds 8 MiB"))
                    } else {
                        Ok(bytes)
                    }
                });
            let _ = sender.send(result);
        });
        receiver
    };
    // Drain while the helper runs, not after waiting for it to exit. Detached
    // readers cannot extend the budget if a descendant retains an output pipe.
    let stdout = read_output(Box::new(
        child.stdout.take().context("helper stdout unavailable")?,
    ));
    let stderr = read_output(Box::new(
        child.stderr.take().context("helper stderr unavailable")?,
    ));
    let status = wait_helper_until(&mut child, deadline)?;
    let read = |receiver: std::sync::mpsc::Receiver<std::io::Result<Vec<u8>>>| -> Result<Vec<u8>> {
        receiver
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .context("restart operation deadline expired draining helper output")?
            .context("read restart helper output")
    };
    Ok(Output {
        status,
        stdout: read(stdout)?,
        stderr: read(stderr)?,
    })
}

fn wait_helper_until(
    child: &mut std::process::Child,
    deadline: Instant,
) -> Result<std::process::ExitStatus> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(
                Duration::from_millis(10).min(deadline.saturating_duration_since(Instant::now())),
            ),
            result => {
                if let Err(error) = child.kill()
                    && child
                        .try_wait()
                        .context("inspect helper after failed termination")?
                        .is_none()
                {
                    return Err(error).context("terminate expired restart helper");
                }
                child.wait().context("reap stopped restart helper")?;
                if let Err(error) = result {
                    return Err(error).context("inspect restart helper");
                }
                bail!(
                    "restart operation deadline expired; helper pid {} was terminated and reaped",
                    child.id()
                );
            }
        }
    }
}

pub(super) fn listeners_after_grace(
    mut inspect: impl FnMut() -> Result<Vec<u32>>,
    grace_deadline: Instant,
    operation_deadline: Instant,
    mut wait: impl FnMut(Duration),
) -> Result<Vec<u32>> {
    let remaining = inspect()?;
    if remaining.is_empty() {
        return Ok(remaining);
    }
    let until = grace_deadline.min(operation_deadline);
    wait(until.saturating_duration_since(Instant::now()));
    if Instant::now() >= operation_deadline {
        bail!("restart operation deadline expired waiting for listeners");
    }
    inspect()
}


