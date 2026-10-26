use anyhow::Context as _;
use std::path::Path;
use std::path::PathBuf;
use tokio::process::Command;

const CODEX_WINDOWS_INSTALLER_URL: &str =
    "https://get.microsoft.com/installer/download/9PLM9XGG6VKS?cid=website_cta_psi";
const CODEX_MICROSOFT_STORE_WEB_URL: &str = "https://apps.microsoft.com/detail/9plm9xgg6vks";

pub async fn run_windows_app_open_or_install(
    workspace: PathBuf,
    download_url_override: Option<String>,
) -> anyhow::Result<()> {
    let workspace_path = workspace.display().to_string();
    let display_workspace = display_workspace_path(&workspace);
    if codex_app_is_installed().await? {
        eprintln!("Opening Codex Desktop workspace {display_workspace}...");
        open_url(&codex_new_thread_url(&workspace_path)).await?;
        return Ok(());
    }

    eprintln!("Codex Desktop not found; opening Windows installer...");
    let download_url = download_url_override
        .as_deref()
        .unwrap_or(CODEX_WINDOWS_INSTALLER_URL);
    if let Err(error) = open_url(download_url).await {
        if download_url_override.is_some() {
            return Err(error);
        }
        open_url(CODEX_MICROSOFT_STORE_WEB_URL).await?;
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

async fn open_url(url: &str) -> anyhow::Result<()> {
    let status = Command::new("powershell.exe")
        .arg("-NoProfile")
        .arg("-NonInteractive")
        .arg("-Command")
        .arg("try { Start-Process -FilePath $env:CODEX_DESKTOP_OPEN_TARGET -ErrorAction Stop } catch { Write-Error $_; exit 1 }")
        .env("CODEX_DESKTOP_OPEN_TARGET", url)
        .status()
        .await
        .with_context(|| format!("failed to open {url}"))?;

    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("failed to open {url} with {status}");
    }
}

fn codex_new_thread_url(workspace: &str) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    serializer.append_pair("path", workspace);
    let query = serializer.finish();
    format!("codex://threads/new?{query}")
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

