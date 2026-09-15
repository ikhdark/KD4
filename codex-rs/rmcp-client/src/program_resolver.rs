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
        Err(e) => {
            tracing::debug!("Failed to resolve {program:?}: {e}. Using original path");
            // Fallback to original program - let Command::new() handle the error
            Ok(program)
        }
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

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use crate::utils::create_env_for_mcp_server;
    use anyhow::Result;
    use pretty_assertions::assert_eq;
    use std::fs;
    use std::path::Path;
    use std::path::PathBuf;
    use tempfile::TempDir;
    use tokio::process::Command;

    /// Windows: Verifies scripts fail to execute without the proper extension.

    #[tokio::test]
    async fn test_windows_fails_without_extension() -> Result<()> {
        let env = TestExecutableEnv::new()?;
        let mut cmd = Command::new(&env.program_name);
        cmd.envs(&env.mcp_env);

        let output = cmd.output().await;
        assert!(
            output.is_err(),
            "Windows requires .cmd/.bat extension for direct execution"
        );
        Ok(())
    }

    /// Windows: Verifies scripts with an explicit extension execute correctly.

    #[tokio::test]
    async fn test_windows_succeeds_with_extension() -> Result<()> {
        let env = TestExecutableEnv::new()?;
        // Append the `.cmd` extension to the program name
        let program_with_ext = format!("{}.cmd", env.program_name);
        let mut cmd = Command::new(&program_with_ext);
        cmd.envs(&env.mcp_env);

        let output = cmd.output().await?;
        assert_eq!(output.status.code(), Some(0));
        assert_eq!(
            String::from_utf8(output.stdout)?.trim(),
            "mcp-resolver-fixture"
        );
        Ok(())
    }

    /// Verifies program resolution enables successful execution on all platforms.
    #[tokio::test]
    async fn test_resolved_program_executes_successfully() -> Result<()> {
        let env = TestExecutableEnv::new()?;
        let program = OsString::from(&env.program_name);

        // Apply platform-specific resolution
        let resolved = resolve(program, &env.mcp_env, std::env::current_dir()?.as_path())?;
        assert_eq!(
            PathBuf::from(&resolved),
            env._temp_dir.path().join("test_mcp_server.cmd")
        );

        // Verify resolved path executes successfully
        let mut cmd = Command::new(resolved);
        cmd.envs(&env.mcp_env);
        let output = cmd.output().await?;
        assert_eq!(output.status.code(), Some(0));
        assert_eq!(
            String::from_utf8(output.stdout)?.trim(),
            "mcp-resolver-fixture"
        );
        Ok(())
    }

    #[tokio::test]
    async fn server_pathext_controls_the_executed_script() -> Result<()> {
        let fixture = TestExecutableEnv::new()?;
        let cwd = fixture._temp_dir.path();
        fs::write(
            cwd.join("test_mcp_server.bat"),
            "@echo off\necho bat-fixture\n",
        )?;
        let mut env = fixture.mcp_env.clone();
        env.retain(|key, _| !crate::utils::env_keys_equal(key, OsStr::new("PATHEXT")));

        // Alternate orders in one process to catch both host-environment lookup
        // and a cached extension list leaking between server configurations.
        for (extensions, extension, expected_output) in [
            (".CMD;.BAT", "cmd", "mcp-resolver-fixture"),
            (".BAT;.CMD", "bat", "bat-fixture"),
        ] {
            env.insert(OsString::from("pAtHeXt"), OsString::from(extensions));
            let resolved = resolve(OsString::from(&fixture.program_name), &env, cwd)?;
            assert_eq!(
                Path::new(&resolved),
                cwd.join(format!("test_mcp_server.{extension}"))
            );
            let output = Command::new(resolved)
                .env_clear()
                .envs(&env)
                .current_dir(cwd)
                .output()
                .await?;
            assert_eq!(output.status.code(), Some(0));
            assert_eq!(String::from_utf8(output.stdout)?.trim(), expected_output);
        }

        env.insert(OsString::from("pAtHeXt"), OsString::from(".EXE"));
        assert_eq!(
            resolve(OsString::from(&fixture.program_name), &env, cwd)?,
            OsString::from(&fixture.program_name),
            "a script excluded by the server's PATHEXT must not be selected"
        );
        Ok(())
    }

    // Test fixture for creating temporary executables in a controlled environment.
    struct TestExecutableEnv {
        // Held to prevent the temporary directory from being deleted.
        _temp_dir: TempDir,
        program_name: String,
        mcp_env: HashMap<OsString, OsString>,
    }

    impl TestExecutableEnv {
        const TEST_PROGRAM: &'static str = "test_mcp_server";

        fn new() -> Result<Self> {
            let temp_dir = TempDir::new()?;
            let dir_path = temp_dir.path();

            Self::create_executable(dir_path)?;

            // Build a clean environment with the temp dir in the PATH.
            let mut extra_env = HashMap::new();
            extra_env.insert(OsString::from("pAtH"), Self::build_path_env_var(dir_path));

            extra_env.insert(OsString::from("PATHEXT"), Self::ensure_cmd_extension());

            let mcp_env = create_env_for_mcp_server(Some(extra_env), &[])?;

            Ok(Self {
                _temp_dir: temp_dir,
                program_name: Self::TEST_PROGRAM.to_string(),
                mcp_env,
            })
        }

        /// Creates a simple, platform-specific executable script.
        fn create_executable(dir: &Path) -> Result<()> {
            {
                let file = dir.join(format!("{}.cmd", Self::TEST_PROGRAM));
                fs::write(&file, "@echo off\necho mcp-resolver-fixture\nexit 0")?;
            }

            Ok(())
        }

        /// Prepends the given directory to the system's PATH variable.
        fn build_path_env_var(dir: &Path) -> OsString {
            let mut path = OsString::from(dir.as_os_str());
            if let Some(current) = std::env::var_os("PATH") {
                let sep = ";";
                path.push(sep);
                path.push(current);
            }
            path
        }

        /// Ensures `.CMD` is in the `PATHEXT` variable on Windows for script discovery.
        fn ensure_cmd_extension() -> OsString {
            let current = std::env::var_os("PATHEXT").unwrap_or_default();
            if current
                .to_string_lossy()
                .to_ascii_uppercase()
                .contains(".CMD")
            {
                current
            } else {
                let mut path_ext = OsString::from(".CMD;");
                path_ext.push(current);
                path_ext
            }
        }
    }
}
