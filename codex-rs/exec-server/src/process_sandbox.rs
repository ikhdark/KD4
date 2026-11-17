use std::collections::HashMap;

use codex_exec_server_protocol::JSONRPCErrorError;
use codex_sandboxing::SandboxType;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use codex_utils_pty::SpawnedProcess;
use codex_utils_pty::TerminalSize;

use crate::ExecServerRuntimePaths;
use crate::protocol::ExecParams;
use crate::rpc::invalid_params;

pub(crate) struct PreparedExecRequest {
    pub(crate) command: Vec<String>,
    pub(crate) cwd: AbsolutePathBuf,
    pub(crate) env: HashMap<String, String>,
    pub(crate) arg0: Option<String>,
    pub(crate) sandbox: SandboxType,
}

impl PreparedExecRequest {
    pub(crate) async fn spawn(self, tty: bool, pipe_stdin: bool) -> Result<SpawnedProcess, String> {
        let mut request = self;
        // Canonicalize the final map before either pipe or PTY launch consumes it.
        codex_protocol::shell_environment::apply_env_overlay(&mut request.env, HashMap::new());
        request.spawn_direct(tty, pipe_stdin).await
    }

    async fn spawn_direct(self, tty: bool, pipe_stdin: bool) -> Result<SpawnedProcess, String> {
        let (program, args) = self
            .command
            .split_first()
            .ok_or_else(|| "argv must not be empty".to_string())?;
        let cwd_metadata = tokio::fs::metadata(self.cwd.as_path())
            .await
            .map_err(|error| {
                format!(
                    "process not started: stage=cwd_metadata cwd={} program={program:?}: {error}",
                    self.cwd.as_path().display()
                )
            })?;
        if !cwd_metadata.is_dir() {
            return Err(format!(
                "process not started: stage=cwd_metadata cwd={} is not a directory",
                self.cwd.as_path().display()
            ));
        }
        if std::path::Path::new(program).is_absolute() {
            let metadata = tokio::fs::metadata(program).await.map_err(|error| {
                format!(
                    "process not started: stage=executable_metadata program={program:?}: {error}"
                )
            })?;
            if !metadata.is_file() {
                return Err(format!(
                    "process not started: stage=executable_metadata program={program:?} is not a file"
                ));
            }
        }
        let spawned = if tty {
            codex_utils_pty::spawn_pty_process(
                program,
                args,
                self.cwd.as_path(),
                &self.env,
                &self.arg0,
                TerminalSize::default(),
            )
            .await
        } else if pipe_stdin {
            codex_utils_pty::spawn_pipe_process(
                program,
                args,
                self.cwd.as_path(),
                &self.env,
                &self.arg0,
            )
            .await
        } else {
            codex_utils_pty::spawn_pipe_process_no_stdin(
                program,
                args,
                self.cwd.as_path(),
                &self.env,
                &self.arg0,
            )
            .await
        };
        spawned.map_err(|err| {
            format!(
                "process spawn failed: stage=spawn program={program:?} cwd={} tty={tty}: {err}",
                self.cwd.as_path().display()
            )
        })
    }
}

pub(crate) fn prepare_exec_request_cancellable(
    params: &ExecParams,
    env: HashMap<String, String>,
    _runtime_paths: Option<&ExecServerRuntimePaths>,
    cancelled: &tokio_util::sync::CancellationToken,
) -> Result<PreparedExecRequest, JSONRPCErrorError> {
    let check_cancelled = || {
        if cancelled.is_cancelled() {
            Err(crate::rpc::invalid_request(format!("process {} start was cancelled", params.process_id)))
        } else {
            Ok(())
        }
    };
    check_cancelled()?;
    if params.enforce_managed_network || params.managed_network.is_some() {
        return Err(invalid_params(
            "managed network proxy support was removed; refusing unsupported network enforcement".to_string(),
        ));
    }
    // Legacy sandbox contexts are accepted as compatibility metadata. They
    // never require native paths, policy validation, helpers or setup.
    Ok(PreparedExecRequest {
        command: params.argv.clone(),
        cwd: native_path(&params.cwd, "cwd")?,
        env,
        arg0: params.arg0.clone(),
        sandbox: SandboxType::None,
    })
}

fn native_path(path: &PathUri, label: &str) -> Result<AbsolutePathBuf, JSONRPCErrorError> {
    path.to_abs_path().map_err(|err| {
        invalid_params(format!(
            "{label} URI `{path}` is not valid on this exec-server host: {err}"
        ))
    })
}
