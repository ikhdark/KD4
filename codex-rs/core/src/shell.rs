use codex_exec_server::ShellInfo;
use codex_shell_command::shell_detect::DetectedShell;
use serde::Deserialize;
use serde::Serialize;
use std::path::PathBuf;

pub use codex_shell_command::shell_detect::ShellType;

type LocalShellResolution = futures::future::Shared<
    futures::future::BoxFuture<'static, Result<std::sync::Arc<Shell>, std::sync::Arc<String>>>,
>;

/// Session-owned, demand-driven host discovery. Remote metadata does not need
/// this resolver, and concurrent local consumers share the same resolution.
#[derive(Clone, Default)]
pub(crate) struct LocalShell {
    resolution: std::sync::Arc<std::sync::OnceLock<LocalShellResolution>>,
    explicit: Option<Shell>,
}

impl LocalShell {
    pub(crate) fn new(explicit: Option<Shell>) -> Self {
        Self {
            explicit,
            ..Self::default()
        }
    }

    #[cfg(test)]
    pub(crate) fn cached(&self) -> Option<std::sync::Arc<Shell>> {
        use futures::FutureExt;
        self.resolution.get()?.clone().now_or_never()?.ok()
    }

    pub(crate) async fn resolve(&self) -> anyhow::Result<std::sync::Arc<Shell>> {
        use futures::FutureExt;
        self.resolution
            .get_or_init(|| {
                let explicit = self.explicit.clone();
                // Keep the initializing future, not just its result: abandoning one
                // waiter must not restart a still-running blocking discovery job.
                async move {
                    let shell = match explicit {
                        Some(shell) => shell,
                        None => tokio::task::spawn_blocking(default_user_shell)
                            .await
                            .map_err(|error| {
                                std::sync::Arc::new(format!(
                                    "default shell discovery task failed: {error}"
                                ))
                            })?,
                    };
                    if shell.shell_type == ShellType::PowerShell {
                        let executable = shell.shell_path.to_string_lossy().into_owned();
                        drop(tokio::task::spawn_blocking(move || {
                            codex_shell_command::prewarm_powershell_parser(&executable);
                        }));
                    }
                    Ok(std::sync::Arc::new(shell))
                }
                .boxed()
                .shared()
            })
            .clone()
            .await
            .map_err(|error| anyhow::anyhow!("{error}"))
    }

    pub(crate) fn explicit(&self) -> Option<&Shell> {
        self.explicit.as_ref()
    }
}

impl From<Shell> for LocalShell {
    fn from(shell: Shell) -> Self {
        std::sync::Arc::new(shell).into()
    }
}

impl From<std::sync::Arc<Shell>> for LocalShell {
    fn from(shell: std::sync::Arc<Shell>) -> Self {
        use futures::FutureExt;
        Self {
            resolution: std::sync::Arc::new(std::sync::OnceLock::from(
                futures::future::ready(Ok(shell)).boxed().shared(),
            )),
            explicit: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Shell {
    pub(crate) shell_type: ShellType,
    pub(crate) shell_path: PathBuf,
}

impl Shell {
    pub fn name(&self) -> &'static str {
        self.shell_type.name()
    }

    /// Takes a string of shell and returns the full list of command args to
    /// use with `exec()` to run the shell command.
    pub fn derive_exec_args(
        &self,
        command: &str,
        use_login_shell: bool,
    ) -> anyhow::Result<Vec<String>> {
        match self.shell_type {
            ShellType::Zsh | ShellType::Bash | ShellType::Sh => {
                let arg = if use_login_shell { "-lc" } else { "-c" };
                Ok(vec![
                    self.shell_path.to_string_lossy().into_owned(),
                    arg.to_string(),
                    command.to_string(),
                ])
            }
            ShellType::PowerShell => {
                let mut args = vec![self.shell_path.to_string_lossy().into_owned()];
                if !use_login_shell {
                    args.push("-NoProfile".to_string());
                }

                args.push("-NonInteractive".to_string());
                args.push("-Command".to_string());
                args.push(command.to_string());
                Ok(args)
            }
            ShellType::Cmd => {
                let mut args = vec![self.shell_path.to_string_lossy().into_owned()];
                args.push("/d".to_string());
                args.push("/c".to_string());
                args.push(command.to_string());
                Ok(args)
            }
        }
    }
}

impl From<DetectedShell> for Shell {
    fn from(detected: DetectedShell) -> Self {
        Self {
            shell_type: detected.shell_type,
            shell_path: detected.shell_path,
        }
    }
}

impl Shell {
    pub(crate) fn from_environment_shell_info(shell_info: ShellInfo) -> anyhow::Result<Self> {
        let shell_type = ShellType::from_name(&shell_info.name)
            .ok_or_else(|| anyhow::anyhow!("unknown environment shell `{}`", shell_info.name))?;

        Ok(Self {
            shell_type,
            shell_path: PathBuf::from(shell_info.path),
        })
    }
}

#[cfg(test)]
fn ultimate_fallback_shell() -> Shell {
    codex_shell_command::shell_detect::ultimate_fallback_shell().into()
}

pub fn get_shell_by_model_provided_path(shell_path: &PathBuf) -> anyhow::Result<Shell> {
    // An explicit local path is not permission to silently choose another
    // shell. Remote overrides are consumed by the environment owner before
    // this local resolver is called.
    if shell_path.is_absolute() || shell_path.components().count() > 1 {
        let metadata = std::fs::metadata(shell_path).map_err(|error| {
            anyhow::anyhow!(
                "process not started: stage=shell_metadata shell={}: {error}",
                shell_path.display()
            )
        })?;
        if !metadata.is_file() {
            anyhow::bail!(
                "process not started: stage=shell_metadata shell={} is not a file",
                shell_path.display()
            );
        }
    }
    codex_shell_command::shell_detect::get_shell_by_model_provided_path(shell_path)
        .map(Into::into)
        .ok_or_else(|| anyhow::anyhow!("unsupported Windows shell `{}`", shell_path.display()))
}

pub fn get_shell(shell_type: ShellType, path: Option<&PathBuf>) -> Option<Shell> {
    codex_shell_command::shell_detect::get_shell(shell_type, path).map(Into::into)
}

pub fn default_user_shell() -> Shell {
    codex_shell_command::shell_detect::default_user_shell().into()
}

#[cfg(test)]
#[path = "shell_tests.rs"]
mod tests;

#[cfg(test)]
mod lazy_shell_tests {
    use super::*;

    #[tokio::test]
    async fn explicit_local_resolution_is_single_flight_and_preserves_identity() {
        let expected = Shell {
            shell_type: ShellType::Cmd,
            shell_path: "configured-cmd.exe".into(),
        };
        let resolver = LocalShell::new(Some(expected.clone()));
        assert!(resolver.cached().is_none());
        let (first, second) = tokio::join!(resolver.resolve(), resolver.resolve());
        let (first, second) = (first.unwrap(), second.unwrap());
        assert_eq!(*first, expected);
        assert!(std::sync::Arc::ptr_eq(&first, &second));
    }

    #[tokio::test]
    async fn cancelling_one_waiter_does_not_restart_local_resolution() {
        use futures::FutureExt;
        use std::sync::Arc;
        use std::sync::atomic::AtomicUsize;
        use std::sync::atomic::Ordering;
        let calls = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(tokio::sync::Notify::new());
        let expected = Arc::new(Shell {
            shell_type: ShellType::Cmd,
            shell_path: "fixture.exe".into(),
        });
        let resolution = {
            let calls = Arc::clone(&calls);
            let release = Arc::clone(&release);
            let expected = Arc::clone(&expected);
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                release.notified().await;
                Ok(expected)
            }
            .boxed()
            .shared()
        };
        let resolver = LocalShell {
            resolution: Arc::new(std::sync::OnceLock::from(resolution)),
            explicit: None,
        };
        let mut first = Box::pin(resolver.resolve());
        assert!(futures::poll!(&mut first).is_pending());
        drop(first);
        let mut second = Box::pin(resolver.resolve());
        assert!(futures::poll!(&mut second).is_pending());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        release.notify_one();
        assert!(Arc::ptr_eq(&second.await.unwrap(), &expected));
    }
}
