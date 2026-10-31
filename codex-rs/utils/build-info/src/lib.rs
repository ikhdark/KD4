//! Shared runtime build metadata for executable surfaces.

use std::io;
use std::io::Read;
use std::path::Path;
use std::sync::OnceLock;

use sha2::Digest;
use sha2::Sha256;

#[cfg(windows)]
pub mod windows_process;

/// Version an executable reports about itself: `--version`, the TUI,
/// `codex doctor`, and app-server build info. Release packaging embeds it
/// through `CODEX_RELEASE_VERSION`; other builds use the Cargo package version.
///
/// Request headers, telemetry, and session metadata keep the Cargo package
/// version, so a packaged build talks to model servers like a source build.
pub const CODEX_VERSION: &str = match option_env!("CODEX_RELEASE_VERSION") {
    Some(version) => version,
    None => env!("CARGO_PKG_VERSION"),
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BuildInfo {
    pub version: &'static str,
    pub commit: &'static str,
    pub dirty: &'static str,
    /// Explicit build profile, or `unknown` when the producer did not embed it.
    /// Debug assertions cannot distinguish standard profiles from custom ones.
    pub profile: &'static str,
    pub built: &'static str,
}

impl BuildInfo {
    pub fn current() -> Self {
        Self::from_values(
            CODEX_VERSION,
            option_env!("CODEX_BUILD_COMMIT"),
            option_env!("GIT_COMMIT"),
            option_env!("CODEX_BUILD_DIRTY"),
            option_env!("CODEX_BUILD_PROFILE"),
            option_env!("CODEX_BUILD_TIMESTAMP"),
        )
    }

    #[doc(hidden)]
    pub const fn from_values(
        version: &'static str,
        codex_commit: Option<&'static str>,
        legacy_commit: Option<&'static str>,
        dirty: Option<&'static str>,
        profile: Option<&'static str>,
        built: Option<&'static str>,
    ) -> Self {
        Self {
            version,
            commit: match codex_commit {
                Some(commit) => commit,
                None => match legacy_commit {
                    Some(commit) => commit,
                    None => "unknown",
                },
            },
            dirty: match dirty {
                Some(dirty) => dirty,
                None => "unknown",
            },
            profile: match profile {
                Some(profile) => profile,
                None => "unknown",
            },
            built: match built {
                Some(built) => built,
                None => "unknown",
            },
        }
    }
}

/// Lowercase hex SHA-256 of a file's contents.
pub fn file_sha256(path: &Path) -> io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// SHA-256 of the running executable, hashed once per process. Commit, dirty
/// flag, and the reproducible build timestamp cannot tell two dirty builds of
/// one commit apart; this can. Hashing blocks, so call it off async workers.
/// Linux opens the pinned running image. Other platforms use a best-effort
/// current-executable pathname snapshot.
pub fn executable_sha256() -> Option<&'static str> {
    static HASH: OnceLock<Option<String>> = OnceLock::new();
    HASH.get_or_init(|| running_executable_sha256().ok())
        .as_deref()
}

/// Starts [`executable_sha256`] on a background thread, once per process.
/// The first rollout metadata write waits for this hash; starting it when a
/// session is created overlaps it with session startup instead of the first
/// turn. A later caller blocks only until the same computation finishes.
pub fn prehash_executable() {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        // Best effort: without a thread the first caller hashes inline.
        let _ = std::thread::Builder::new()
            .name("executable-sha256".to_string())
            .spawn(|| {
                executable_sha256();
            });
    });
}

#[cfg(target_os = "linux")]
fn running_executable_sha256() -> io::Result<String> {
    file_sha256(Path::new("/proc/self/exe"))
}

#[cfg(not(target_os = "linux"))]
fn running_executable_sha256() -> io::Result<String> {
    file_sha256(&std::env::current_exe()?)
}

