//! Shared runtime build metadata for executable surfaces.

use std::io;
use std::io::Read;
use std::path::Path;
use std::sync::OnceLock;

use sha2::Digest;
use sha2::Sha256;

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
    Ok(format!("{:x}", hasher.finalize()))
}

/// SHA-256 of the running executable, hashed once per process. Commit, dirty
/// flag, and the reproducible build timestamp cannot tell two dirty builds of
/// one commit apart; this can. Hashing blocks, so call it off async workers.
pub fn executable_sha256() -> Option<&'static str> {
    static HASH: OnceLock<Option<String>> = OnceLock::new();
    HASH.get_or_init(|| {
        std::env::current_exe()
            .and_then(|path| file_sha256(&path))
            .ok()
    })
    .as_deref()
}

#[cfg(test)]
mod tests {
    use super::BuildInfo;
    use super::CODEX_VERSION;

    #[test]
    fn current_reports_embedded_metadata() {
        assert_eq!(
            BuildInfo::current(),
            BuildInfo {
                version: CODEX_VERSION,
                commit: option_env!("CODEX_BUILD_COMMIT")
                    .or(option_env!("GIT_COMMIT"))
                    .unwrap_or("unknown"),
                dirty: option_env!("CODEX_BUILD_DIRTY").unwrap_or("unknown"),
                profile: option_env!("CODEX_BUILD_PROFILE").unwrap_or("unknown"),
                built: option_env!("CODEX_BUILD_TIMESTAMP").unwrap_or("unknown"),
            }
        );
    }

    #[test]
    fn commit_precedence_and_fallbacks_are_shared() {
        assert_eq!(
            BuildInfo::from_values(
                "1.2.3",
                Some("codex"),
                Some("legacy"),
                Some("true"),
                Some("custom"),
                Some("now"),
            ),
            BuildInfo {
                version: "1.2.3",
                commit: "codex",
                dirty: "true",
                profile: "custom",
                built: "now",
            }
        );
        assert_eq!(
            BuildInfo::from_values("1.2.3", None, Some("legacy"), None, None, None),
            BuildInfo {
                version: "1.2.3",
                commit: "legacy",
                dirty: "unknown",
                profile: "unknown",
                built: "unknown",
            }
        );
        assert_eq!(
            BuildInfo::from_values("1.2.3", None, None, None, None, None),
            BuildInfo {
                version: "1.2.3",
                commit: "unknown",
                dirty: "unknown",
                profile: "unknown",
                built: "unknown",
            }
        );
    }
}
