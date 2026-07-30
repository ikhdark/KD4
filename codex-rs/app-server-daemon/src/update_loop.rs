#[cfg(unix)]
use std::future::Future;
#[cfg(unix)]
use std::process::Command as StdCommand;
#[cfg(unix)]
use std::process::Stdio;
#[cfg(unix)]
use std::time::Duration;

#[cfg(unix)]
use anyhow::Context;
use anyhow::Result;
#[cfg(not(unix))]
use anyhow::bail;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(unix)]
use tokio::io::AsyncWriteExt;
#[cfg(unix)]
use tokio::process::Command;
#[cfg(unix)]
use tokio::signal::unix::SignalKind;
#[cfg(unix)]
use tokio::signal::unix::signal;
#[cfg(unix)]
use tokio::sync::watch;
#[cfg(unix)]
use tokio::time::sleep;

#[cfg(unix)]
use crate::Daemon;
#[cfg(unix)]
use crate::RestartIfRunningOutcome;
#[cfg(unix)]
use crate::RestartMode;
#[cfg(unix)]
use crate::UpdaterRefreshMode;
#[cfg(unix)]
use crate::backend::force_terminate_process_group;
#[cfg(unix)]
use crate::managed_install::ExecutableIdentity;
#[cfg(unix)]
use crate::managed_install::executable_identity;
#[cfg(unix)]
use crate::managed_install::resolved_managed_codex_bin;

#[cfg(unix)]
const INITIAL_UPDATE_DELAY: Duration = Duration::from_secs(5 * 60);
#[cfg(unix)]
const RESTART_RETRY_INTERVAL: Duration = Duration::from_millis(50);
#[cfg(unix)]
const UPDATE_INTERVAL: Duration = Duration::from_secs(60 * 60);
#[cfg(unix)]
const INSTALL_SCRIPT_URL: &str = "https://chatgpt.com/codex/install.sh";

#[cfg(unix)]
pub(crate) async fn run() -> Result<()> {
    let mut terminate_signal =
        signal(SignalKind::terminate()).context("failed to install updater shutdown handler")?;
    let (terminate_tx, terminate_rx) = watch::channel(false);
    let mut update_loop = Box::pin(run_update_loop(terminate_rx));
    tokio::select! {
        result = &mut update_loop => result,
        _ = terminate_signal.recv() => {
            let _ = terminate_tx.send(true);
            update_loop.await
        }
    }
}

#[cfg(unix)]
async fn run_update_loop(mut terminate: watch::Receiver<bool>) -> Result<()> {
    let Some(running_updater_identity) =
        await_or_terminate(current_updater_identity(), &mut terminate).await
    else {
        return Ok(());
    };
    let running_updater_identity = running_updater_identity?;
    if sleep_or_terminate(INITIAL_UPDATE_DELAY, &mut terminate).await {
        return Ok(());
    }
    loop {
        match update_once(&running_updater_identity, &mut terminate).await {
            Ok(UpdateLoopControl::Continue) | Err(_) => {}
            Ok(UpdateLoopControl::Stop) => return Ok(()),
        }
        if sleep_or_terminate(UPDATE_INTERVAL, &mut terminate).await {
            return Ok(());
        }
    }
}

#[cfg(not(unix))]
pub(crate) async fn run() -> Result<()> {
    bail!("pid-managed updater loop is unsupported on this platform")
}

#[cfg(unix)]
async fn wait_for_terminate(terminate: &mut watch::Receiver<bool>) {
    if *terminate.borrow() {
        return;
    }
    let _ = terminate.changed().await;
}

#[cfg(unix)]
async fn await_or_terminate<F>(
    future: F,
    terminate: &mut watch::Receiver<bool>,
) -> Option<F::Output>
where
    F: Future,
{
    tokio::select! {
        result = future => Some(result),
        _ = wait_for_terminate(terminate) => None,
    }
}

#[cfg(unix)]
async fn sleep_or_terminate(duration: Duration, terminate: &mut watch::Receiver<bool>) -> bool {
    tokio::select! {
        _ = sleep(duration) => false,
        _ = wait_for_terminate(terminate) => true,
    }
}

#[cfg(unix)]
enum UpdateLoopControl {
    Continue,
    Stop,
}

#[cfg(unix)]
async fn update_once(
    running_updater_identity: &ExecutableIdentity,
    terminate: &mut watch::Receiver<bool>,
) -> Result<UpdateLoopControl> {
    if matches!(
        install_latest_standalone(terminate).await?,
        UpdateLoopControl::Stop
    ) {
        return Ok(UpdateLoopControl::Stop);
    }

    let daemon = Daemon::from_environment()?;
    let Some(managed_codex_bin) = await_or_terminate(
        resolved_managed_codex_bin(&daemon.managed_codex_bin),
        terminate,
    )
    .await
    else {
        return Ok(UpdateLoopControl::Stop);
    };
    let managed_codex_bin = managed_codex_bin?;
    let Some(managed_identity) =
        await_or_terminate(executable_identity(&managed_codex_bin), terminate).await
    else {
        return Ok(UpdateLoopControl::Stop);
    };
    let managed_identity = managed_identity?;
    let (restart_mode, updater_refresh_mode) =
        update_modes_for_identities(running_updater_identity, &managed_identity);

    loop {
        if *terminate.borrow() {
            return Ok(UpdateLoopControl::Stop);
        }
        let Some(restart_outcome) = await_or_terminate(
            daemon.try_restart_if_running(restart_mode, updater_refresh_mode, &managed_codex_bin),
            terminate,
        )
        .await
        else {
            return Ok(UpdateLoopControl::Stop);
        };
        match restart_outcome? {
            RestartIfRunningOutcome::Busy => {
                if sleep_or_terminate(RESTART_RETRY_INTERVAL, terminate).await {
                    return Ok(UpdateLoopControl::Stop);
                }
            }
            _ => return Ok(UpdateLoopControl::Continue),
        }
    }
}

#[cfg(unix)]
async fn current_updater_identity() -> Result<ExecutableIdentity> {
    let current_exe =
        std::env::current_exe().context("failed to resolve current updater executable")?;
    executable_identity(&current_exe).await
}

#[cfg(unix)]
fn update_modes_for_identities(
    running_updater_identity: &ExecutableIdentity,
    managed_identity: &ExecutableIdentity,
) -> (RestartMode, UpdaterRefreshMode) {
    if running_updater_identity == managed_identity {
        (RestartMode::IfVersionChanged, UpdaterRefreshMode::None)
    } else {
        (
            RestartMode::Always,
            UpdaterRefreshMode::ReexecIfManagedBinaryChanged,
        )
    }
}

#[cfg(unix)]
pub(crate) fn reexec_managed_updater(managed_codex_bin: &std::path::Path) -> Result<()> {
    let err = StdCommand::new(managed_codex_bin)
        .args(["app-server", "daemon", "pid-update-loop"])
        .exec();
    Err(err).with_context(|| {
        format!(
            "failed to replace updater with managed Codex binary {}",
            managed_codex_bin.display()
        )
    })
}

#[cfg(unix)]
async fn install_latest_standalone(
    terminate: &mut watch::Receiver<bool>,
) -> Result<UpdateLoopControl> {
    install_latest_standalone_from_url(INSTALL_SCRIPT_URL, terminate).await
}

#[cfg(unix)]
async fn install_latest_standalone_from_url(
    install_script_url: &str,
    terminate: &mut watch::Receiver<bool>,
) -> Result<UpdateLoopControl> {
    let Some(response) = await_or_terminate(reqwest::get(install_script_url), terminate).await
    else {
        return Ok(UpdateLoopControl::Stop);
    };
    let response = response
        .context("failed to fetch standalone Codex updater")?
        .error_for_status()
        .context("standalone Codex updater request failed")?;
    let Some(script) = await_or_terminate(response.bytes(), terminate).await else {
        return Ok(UpdateLoopControl::Stop);
    };
    let script = script.context("failed to read standalone Codex updater")?;

    let mut command = Command::new("/bin/sh");
    command
        .arg("-s")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command.process_group(0);
    let mut child = command
        .kill_on_drop(true)
        .spawn()
        .context("failed to invoke standalone Codex updater")?;
    let mut stdin = child
        .stdin
        .take()
        .context("standalone Codex updater stdin was unavailable")?;
    let write_result = await_or_terminate(stdin.write_all(&script), terminate).await;
    let Some(write_result) = write_result else {
        drop(stdin);
        let _ = terminate_installer(&mut child).await;
        return Ok(UpdateLoopControl::Stop);
    };
    if let Err(err) = write_result {
        drop(stdin);
        let _ = terminate_installer(&mut child).await;
        return Err(err).context("failed to pass standalone Codex updater to shell");
    }
    drop(stdin);
    let Some(status) = await_or_terminate(child.wait(), terminate).await else {
        let _ = terminate_installer(&mut child).await;
        return Ok(UpdateLoopControl::Stop);
    };
    let status = status.context("failed to wait for standalone Codex updater")?;

    if status.success() {
        Ok(UpdateLoopControl::Continue)
    } else {
        anyhow::bail!("standalone Codex updater exited with status {status}")
    }
}

#[cfg(unix)]
async fn terminate_installer(child: &mut tokio::process::Child) -> Result<()> {
    let Some(pid) = child.id() else {
        return Ok(());
    };
    force_terminate_process_group(pid)?;
    child
        .wait()
        .await
        .context("failed to reap cancelled standalone Codex updater")?;
    Ok(())
}

#[cfg(all(test, unix))]
#[path = "update_loop_tests.rs"]
mod tests;
