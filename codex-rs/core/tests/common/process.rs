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

#[cfg(unix)]
pub fn process_is_alive(pid: &str) -> anyhow::Result<bool> {
    let status = std::process::Command::new("kill")
        .args(["-0", pid])
        .status()
        .context("failed to probe process liveness with kill -0")?;
    Ok(status.success())
}

#[cfg(windows)]
pub fn process_is_alive(pid: &str) -> anyhow::Result<bool> {
    let pid = pid.parse::<u32>().context("pid file was not numeric")?;
    let filter = format!("PID eq {pid}");
    let output = std::process::Command::new("tasklist.exe")
        .args(["/FI", &filter, "/FO", "CSV", "/NH"])
        .output()
        .context("failed to probe process liveness with tasklist")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(output.status.success() && stdout.contains(&format!("\"{pid}\"")))
}

async fn wait_for_process_exit_inner(pid: String) -> anyhow::Result<()> {
    loop {
        if !process_is_alive(&pid)? {
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
