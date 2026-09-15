use anyhow::Context;
use std::fs;
use std::path::Path;
use std::time::Duration;

pub async fn wait_for_pid_file(path: &Path) -> anyhow::Result<String> {
    let pid = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(contents) = fs::read_to_string(path) {
                let trimmed = contents.trim();
                if !trimmed.is_empty() {
                    return trimmed.to_string();
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .context("timed out waiting for pid file")?;

    Ok(pid)
}

pub async fn process_is_alive(pid: &str) -> anyhow::Result<bool> {
    let pid = pid.parse::<u32>().context("pid file was not numeric")?;
    let filter = format!("PID eq {pid}");
    let mut command = tokio::process::Command::new("tasklist.exe");
    command.args(["/FI", &filter, "/FO", "CSV", "/NH"]);
    process_is_alive_from_command(pid, command).await
}

async fn process_is_alive_from_command(
    pid: u32,
    mut command: tokio::process::Command,
) -> anyhow::Result<bool> {
    let output = tokio::time::timeout(Duration::from_secs(2), command.kill_on_drop(true).output())
        .await
        .context("timed out probing process liveness with tasklist")?
        .context("failed to probe process liveness with tasklist")?;
    anyhow::ensure!(
        output.status.success(),
        "process liveness probe failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.contains(&format!("\"{pid}\"")))
}

async fn wait_for_process_exit_inner(pid: String) -> anyhow::Result<()> {
    loop {
        if !process_is_alive(&pid).await? {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

pub async fn wait_for_process_exit(pid: &str) -> anyhow::Result<()> {
    let pid = pid.to_string();
    tokio::time::timeout(Duration::from_secs(2), wait_for_process_exit_inner(pid))
        .await
        .context("timed out waiting for process to exit")??;

    Ok(())
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn process_liveness_probe_enforces_its_timeout() {
        let mut command = tokio::process::Command::new("powershell.exe");
        command.args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Start-Sleep -Seconds 30",
        ]);
        let started = std::time::Instant::now();
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            process_is_alive_from_command(std::process::id(), command),
        )
        .await
        .expect("the liveness probe must enforce its own shorter deadline");

        let error = result.expect_err("a timed out probe cannot establish process liveness");
        assert!(
            error
                .to_string()
                .contains("timed out probing process liveness with tasklist")
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "waiting for the child must leave the runtime able to poll its timeout"
        );
    }

    #[tokio::test]
    async fn process_liveness_probe_failure_is_not_reported_as_exit() {
        let mut command = tokio::process::Command::new("cmd.exe");
        command.args(["/D", "/C", "exit", "7"]);
        let error = process_is_alive_from_command(std::process::id(), command)
            .await
            .expect_err("a failed probe cannot establish that the process exited");
        assert!(error.to_string().contains("process liveness probe failed"));
    }
}
