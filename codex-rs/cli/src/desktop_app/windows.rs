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
    let output = Command::new("powershell.exe")
        .arg("-NoProfile")
        .arg("-Command")
        .arg("Get-StartApps -Name 'Codex' | Select-Object -First 1 -ExpandProperty AppID")
        .output()
        .await
        .context("failed to invoke `powershell.exe`")?;

    if !output.status.success() {
        return Ok(false);
    }

    Ok(!String::from_utf8_lossy(&output.stdout).trim().is_empty())
}

async fn open_url(url: &str) -> anyhow::Result<()> {
    let status = Command::new("powershell.exe")
        .arg("-NoProfile")
        .arg("-Command")
        .arg("& { param($target) try { Start-Process -FilePath $target -ErrorAction Stop } catch { Write-Error $_; exit 1 } }")
        .arg(url)
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

#[cfg(test)]
mod tests {
    use super::codex_new_thread_url;
    use super::display_workspace_path;
    use super::open_url;
    use pretty_assertions::assert_eq;
    use std::path::Path;

    #[tokio::test]
    async fn open_url_reports_a_launch_failure() {
        let temp_dir = tempfile::tempdir().expect("create temporary directory");
        let missing_executable = temp_dir.path().join("missing-installer.exe");
        let error = open_url(&missing_executable.to_string_lossy())
            .await
            .expect_err("a nonexistent installer must not be reported as opened");
        assert!(error.to_string().contains("failed to open"));
    }

    #[test]
    fn display_workspace_path_removes_windows_extended_prefix() {
        assert_eq!(
            display_workspace_path(Path::new(r"\\?\C:\Users\fcoury\code\codex")),
            r"C:\Users\fcoury\code\codex"
        );
    }

    #[test]
    fn display_workspace_path_preserves_unc_prefix() {
        assert_eq!(
            display_workspace_path(Path::new(r"\\?\UNC\server\share\codex")),
            r"\\server\share\codex"
        );
    }

    #[test]
    fn display_workspace_path_leaves_regular_paths_unchanged() {
        assert_eq!(
            display_workspace_path(Path::new(r"C:\Users\fcoury\code\codex")),
            r"C:\Users\fcoury\code\codex"
        );
    }

    #[test]
    fn codex_new_thread_url_encodes_windows_workspace_path() {
        assert_eq!(
            codex_new_thread_url(r"C:\Users\akuma\repos\koba"),
            r"codex://threads/new?path=C%3A%5CUsers%5Cakuma%5Crepos%5Ckoba"
        );
    }

    #[test]
    fn codex_new_thread_url_preserves_verbatim_workspace_path() {
        assert_eq!(
            codex_new_thread_url(r"\\?\C:\Users\akuma\repos\koba"),
            r"codex://threads/new?path=%5C%5C%3F%5CC%3A%5CUsers%5Cakuma%5Crepos%5Ckoba"
        );
    }
}
