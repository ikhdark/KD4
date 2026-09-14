use std::ffi::OsString;
use std::path::Path;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::OnceLock;
use std::sync::PoisonError;

use tempfile::tempdir;

/// Serializes tests that mutate process-wide CODEX_HOME.
///
/// Keep OAuth tests on this one guard instead of defining per-module helpers; otherwise
/// concurrently running test modules can point File/Secrets storage at different homes.
pub(super) struct TempCodexHome {
    _guard: MutexGuard<'static, ()>,
    _dir: tempfile::TempDir,
    previous_home: Option<OsString>,
}

impl TempCodexHome {
    pub(super) fn new() -> Self {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let guard = LOCK
            .get_or_init(Mutex::default)
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let dir = tempdir().expect("create CODEX_HOME temp dir");
        let previous_home = std::env::var_os("CODEX_HOME");
        unsafe {
            std::env::set_var("CODEX_HOME", dir.path());
        }
        Self {
            _guard: guard,
            _dir: dir,
            previous_home,
        }
    }

    pub(super) fn path(&self) -> &Path {
        self._dir.path()
    }
}

impl Drop for TempCodexHome {
    fn drop(&mut self) {
        unsafe {
            match &self.previous_home {
                Some(value) => std::env::set_var("CODEX_HOME", value),
                None => std::env::remove_var("CODEX_HOME"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::TempCodexHome;

    #[test]
    fn restores_previous_environment() {
        const CHILD: &str = "CODEX_HOME_RESTORE_TEST_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let previous = std::env::var_os("CODEX_HOME");
            let home = TempCodexHome::new();
            assert_eq!(
                std::env::var_os("CODEX_HOME").as_deref(),
                Some(home.path().as_os_str())
            );
            drop(home);
            assert_eq!(std::env::var_os("CODEX_HOME"), previous);
            return;
        }
        // Configure the environment before launching a single isolated test.
        for previous in [None, Some("previous-codex-home")] {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "oauth::test_support::tests::restores_previous_environment",
                    "--nocapture",
                ])
                .env(CHILD, "1");
            match previous {
                Some(value) => {
                    command.env("CODEX_HOME", value);
                }
                None => {
                    command.env_remove("CODEX_HOME");
                }
            }
            let output = command.output().unwrap();
            assert!(output.status.success(), "{output:?}");
            assert!(
                String::from_utf8_lossy(&output.stdout).contains("1 passed"),
                "{output:?}"
            );
        }
    }
}
