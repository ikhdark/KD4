//! Platform-specific program resolution for MCP server execution.
//!
//! This module provides a unified interface for resolving executable paths
//! using the MCP server's environment. The key challenge it addresses is that
//! Windows cannot execute script files (e.g., `.cmd`, `.bat`) directly through
//! `Command::new()` without their file extensions, while Unix systems handle
//! scripts natively through shebangs.
//!
//! The `resolve` function uses `which` to resolve full paths, including Windows
//! extensions, against the server's PATH and PATHEXT rather than the host's.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::path::Path;
use which::sys::Sys;

/// Resolves a program to its executable path on Windows systems.
///
/// Windows requires explicit file extensions for script execution. This function
/// uses the `which` crate to search the `PATH` environment variable and find
/// the full path to the executable, including necessary script extensions
/// (`.cmd`, `.bat`, etc.) defined in `PATHEXT`.
///
/// This enables tools like `npx`, `pnpm`, and `yarn` to work correctly on Windows
/// without requiring users to specify full paths or extensions in their configuration.
pub fn resolve(
    program: OsString,
    env: &HashMap<OsString, OsString>,
    cwd: &Path,
) -> std::io::Result<OsString> {
    // which_in uses the host's cached PATHEXT. Supply the child's environment
    // without mutating process-global state shared by concurrent MCP launches.
    match which::WhichConfig::new_with_sys(ServerEnvironment(env))
        .binary_name(program.clone())
        .custom_cwd(cwd.to_path_buf())
        .first_result()
    {
        Ok(resolved) => {
            tracing::debug!("Resolved {program:?} to {resolved:?}");
            Ok(resolved.into_os_string())
        }
        Err(error) => Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("failed to resolve MCP program {program:?} in the server environment: {error}"),
        )),
    }
}

struct ServerEnvironment<'a>(&'a HashMap<OsString, OsString>);

impl ServerEnvironment<'_> {
    fn get(&self, name: &str) -> Option<OsString> {
        self.0
            .iter()
            .find(|(key, _)| crate::utils::env_keys_equal(key, OsStr::new(name)))
            .map(|(_, value)| value.clone())
    }
}

// Retain which's filesystem and executable checks; only environment lookup is
// scoped to the server being launched.
impl Sys for ServerEnvironment<'_> {
    type ReadDirEntry = std::fs::DirEntry;
    type Metadata = std::fs::Metadata;

    fn is_windows(&self) -> bool {
        which::sys::RealSys.is_windows()
    }

    fn current_dir(&self) -> std::io::Result<std::path::PathBuf> {
        which::sys::RealSys.current_dir()
    }

    fn home_dir(&self) -> Option<std::path::PathBuf> {
        which::sys::RealSys.home_dir()
    }

    fn env_split_paths(&self, paths: &OsStr) -> Vec<std::path::PathBuf> {
        which::sys::RealSys.env_split_paths(paths)
    }

    fn env_path(&self) -> Option<OsString> {
        self.get("PATH")
    }

    fn env_path_ext(&self) -> Option<OsString> {
        self.get("PATHEXT")
    }

    fn metadata(&self, path: &Path) -> std::io::Result<Self::Metadata> {
        which::sys::RealSys.metadata(path)
    }

    fn symlink_metadata(&self, path: &Path) -> std::io::Result<Self::Metadata> {
        which::sys::RealSys.symlink_metadata(path)
    }

    fn read_dir(
        &self,
        path: &Path,
    ) -> std::io::Result<Box<dyn Iterator<Item = std::io::Result<Self::ReadDirEntry>>>> {
        which::sys::RealSys.read_dir(path)
    }

    fn is_valid_executable(&self, path: &Path) -> std::io::Result<bool> {
        which::sys::RealSys.is_valid_executable(path)
    }
}

