use anyhow::Context as _;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use tokio::process::Command;

const DESKTOP_OPEN_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug)]
struct DesktopOpenError {
    error: anyhow::Error,
    outcome_unknown: bool,
}

const CODEX_WINDOWS_INSTALLER_URL: &str =
    "https://get.microsoft.com/installer/download/9PLM9XGG6VKS?cid=website_cta_psi";
const CODEX_MICROSOFT_STORE_WEB_URL: &str = "https://apps.microsoft.com/detail/9plm9xgg6vks";

pub async fn run_windows_app_open_or_install(
    workspace: PathBuf,
    download_url_override: Option<String>,
) -> anyhow::Result<()> {
    let display_workspace = display_workspace_path(&workspace);
    if codex_app_is_installed().await? {
        eprintln!("Opening Codex Desktop workspace {display_workspace}...");
        open_url(&super::codex_new_thread_url(&workspace))
            .await
            .map_err(|failure| failure.error)?;
        return Ok(());
    }

    eprintln!("Codex Desktop not found; opening Windows installer...");
    let download_url = download_url_override
        .as_deref()
        .unwrap_or(CODEX_WINDOWS_INSTALLER_URL);
    if let Err(failure) = open_url(download_url).await {
        // A timed-out launcher may already have submitted the open request.
        // Only fall back when the first attempt is known to have failed.
        if download_url_override.is_some() || failure.outcome_unknown {
            return Err(failure.error);
        }
        open_url(CODEX_MICROSOFT_STORE_WEB_URL)
            .await
            .map_err(|failure| failure.error)?;
    }
    eprintln!("After installing Codex Desktop, open workspace {display_workspace}.");
    Ok(())
}

async fn codex_app_is_installed() -> anyhow::Result<bool> {
    let mut command = Command::new("powershell.exe");
    command
        .kill_on_drop(true)
        .arg("-NoProfile")
        .arg("-NonInteractive")
        .arg("-Command")
        .arg("$ErrorActionPreference = 'Stop'; Get-StartApps -Name 'Codex' | Select-Object -First 1 -ExpandProperty AppID");
    let output = tokio::time::timeout(std::time::Duration::from_secs(10), command.output())
        .await
        .context("timed out checking whether Codex Desktop is installed")?
        .context("failed to invoke `powershell.exe`")?;

    if !output.status.success() {
        anyhow::bail!(
            "failed to check Codex Desktop installation: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    Ok(!String::from_utf8_lossy(&output.stdout).trim().is_empty())
}

async fn open_url(url: &str) -> Result<(), DesktopOpenError> {
    let mut launcher = Command::new("powershell.exe")
        .kill_on_drop(true)
        .arg("-NoProfile")
        .arg("-NonInteractive")
        .arg("-Command")
        .arg("try { Start-Process -FilePath $env:CODEX_DESKTOP_OPEN_TARGET -ErrorAction Stop } catch { Write-Error $_; exit 1 }")
        .env("CODEX_DESKTOP_OPEN_TARGET", url)
        .spawn()
        .with_context(|| format!("failed to open {url}"))
        .map_err(|error| DesktopOpenError { error, outcome_unknown: false })?;

    match tokio::time::timeout(DESKTOP_OPEN_TIMEOUT, launcher.wait()).await {
        Ok(Ok(status)) if status.success() => Ok(()),
        Ok(Ok(status)) => Err(DesktopOpenError {
            error: anyhow::anyhow!("failed to open {url} with {status}"),
            outcome_unknown: false,
        }),
        Ok(Err(error)) => Err(DesktopOpenError {
            error: anyhow::Error::new(error).context(format!("could not confirm opening {url}")),
            outcome_unknown: true,
        }),
        Err(_) => Err(DesktopOpenError {
            // Dropping the launcher requests its termination. It does not kill
            // the already opened app or pretend shell delivery was cancelled.
            error: anyhow::anyhow!("timed out opening {url}; the launch outcome is unknown"),
            outcome_unknown: true,
        }),
    }
}


fn display_workspace_path(workspace: &Path) -> String {
    let path = workspace.display().to_string();
    if let Some(path) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{path}")
    } else if let Some(path) = path.strip_prefix(r"\\?\") {
        path.to_string()
    } else {
        path
    }
}

