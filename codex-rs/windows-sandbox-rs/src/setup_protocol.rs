use codex_otel::StatsigMetricsSettings;
use serde::Deserialize;
use serde::Serialize;
use std::path::PathBuf;
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::os::windows::fs::OpenOptionsExt;

/// A small argv receipt for an immutable, content-bound setup request. The
/// parent pins the file against write/delete until the foreground helper exits.
pub struct PreparedSetupRequest {
    pin: std::fs::File,
    path: tempfile::TempPath,
    digest: String,
    pub payload_len: usize,
}

impl PreparedSetupRequest {
    pub fn new(payload: &SetupPayload) -> Result<Self> {
        let directory = payload.codex_home.join(".sandbox").join("setup-requests");
        std::fs::create_dir_all(&directory)?;
        let bytes = serde_json::to_vec(payload)?;
        let digest = request_digest(&bytes);
        let mut staged = tempfile::Builder::new().prefix("setup-").tempfile_in(&directory)?;
        staged.write_all(&bytes)?;
        staged.as_file().sync_all()?;
        let path = staged.into_temp_path();
        let pin = std::fs::OpenOptions::new().read(true)
            .share_mode(windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ).open(&path)?;
        Ok(Self { pin, path, digest, payload_len: bytes.len() })
    }

    pub fn arguments(&self, consume: bool) -> Result<Vec<String>> {
        let mut args = vec!["--request-file".into(), self.path.to_str().context("setup request path is not Unicode")?.to_owned(), self.digest.clone(), self.payload_len.to_string()];
        if consume { args.push("--consume-request".into()); }
        Ok(args)
    }

    /// Background read-ACL work owns a distinct request. Transfer cleanup only
    /// after staging; a failed spawn must remove the returned exact path.
    pub fn into_background(self) -> Result<(PathBuf, Vec<String>)> {
        let args = self.arguments(true)?;
        let Self { pin, path, .. } = self;
        drop(pin);
        Ok((path.keep()?, args))
    }
}

pub fn read_setup_request(args: &[String]) -> Result<SetupPayload> {
    anyhow::ensure!(matches!(args.len(), 4 | 5) && args[0] == "--request-file", "expected setup request-file receipt");
    let path = PathBuf::from(&args[1]);
    anyhow::ensure!(path.is_absolute(), "setup request path must be absolute");
    anyhow::ensure!(args[2].len() == 64 && args[2].bytes().all(|byte| byte.is_ascii_hexdigit()), "invalid setup request digest");
    let expected: u64 = args[3].parse().context("invalid setup request length")?;
    let mut file = std::fs::OpenOptions::new().read(true)
        .share_mode(windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ).open(&path)?;
    anyhow::ensure!(file.metadata()?.len() == expected, "setup request length mismatch");
    let mut bytes = Vec::new();
    (&mut file).take(expected.checked_add(1).context("setup request too large")?).read_to_end(&mut bytes)?;
    anyhow::ensure!(bytes.len() as u64 == expected && request_digest(&bytes) == args[2], "setup request integrity mismatch");
    let payload: SetupPayload = serde_json::from_slice(&bytes)?;
    if args.len() == 5 {
        anyhow::ensure!(args[4] == "--consume-request", "unexpected setup request argument");
        let directory = payload.codex_home.join(".sandbox").join("setup-requests");
        anyhow::ensure!(path.parent().map(std::fs::canonicalize).transpose()? == Some(std::fs::canonicalize(directory)?)
            && path.file_name().and_then(|name| name.to_str()).is_some_and(|name| name.starts_with("setup-")), "refusing cleanup outside the owned setup request directory");
        drop(file);
        std::fs::remove_file(&path)?;
    }
    Ok(payload)
}

fn request_digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Versioned payload exchanged between the sandbox setup orchestrator and helper.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SetupPayload {
    pub version: u32,
    pub offline_username: String,
    pub online_username: String,
    pub codex_home: PathBuf,
    pub command_cwd: PathBuf,
    pub read_roots: Vec<PathBuf>,
    pub write_roots: Vec<PathBuf>,
    #[serde(default)]
    pub deny_read_paths: Vec<PathBuf>,
    #[serde(default)]
    pub deny_write_paths: Vec<PathBuf>,
    pub proxy_ports: Vec<u16>,
    #[serde(default)]
    pub allow_local_binding: bool,
    #[serde(default)]
    pub otel: Option<StatsigMetricsSettings>,
    pub real_user: String,
    #[serde(default)]
    pub mode: SetupMode,
    #[serde(default)]
    pub refresh_only: bool,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SetupMode {
    #[default]
    Full,
    ProvisionOnly,
    ReadAclsOnly,
    ReadAclsOnlyStrict,
}
