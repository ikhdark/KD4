use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_pty::ManagedRootProcess;
use codex_utils_pty::WINDOWS_CREATE_SUSPENDED;
use codex_utils_pty::WINDOWS_PROCESS_OPERATION_TIMEOUT;
use codex_utils_pty::configure_windows_command_args;
use codex_utils_pty::process_observation::ObservedProcess;
use codex_utils_pty::run_windows_process_operation;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::process::Child;
use tokio::process::Command;
use tokio::runtime::Handle;
use tracing::trace;

#[derive(Debug, Clone, Copy)]
pub enum StdioPolicy {
    RedirectForShellTool,
}

/// Spawns the child process for the exec params, ensuring the args and
/// environment variables used to create the `Command` (and `Child`) honor the
/// configuration. KD4 has no sandbox, so no sandbox environment is injected.
pub(crate) struct SpawnChildRequest<'a> {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub arg0: Option<&'a str>,
    pub cwd: AbsolutePathBuf,
    pub stdio_policy: StdioPolicy,
    pub env: HashMap<String, String>,

    pub creation_flags: u32,
}

/// Owns the suspended launch until the caller claims it for containment.
pub(crate) struct UnclaimedSpawn {
    owned: Option<(Child, ObservedProcess, ManagedRootProcess)>,
    runtime: Handle,
}

impl UnclaimedSpawn {
    pub(crate) fn into_parts(mut self) -> (Child, ObservedProcess, ManagedRootProcess) {
        self.owned.take().expect("unclaimed spawn still owned")
    }
}

impl Drop for UnclaimedSpawn {
    fn drop(&mut self) {
        if let Some((child, observation, managed_root)) = self.owned.take() {
            // The result may be discarded outside the launch runtime. Cleanup
            // takes ownership synchronously; only its bounded waiter is dropped.
            let _runtime = self.runtime.enter();
            drop(managed_root.cleanup_observed_child(child, observation, /*attached*/ false));
        }
    }
}

pub(crate) async fn spawn_child_with_observation_async(
    request: SpawnChildRequest<'_>,
    managed_root: ManagedRootProcess,
) -> std::io::Result<UnclaimedSpawn> {
    let SpawnChildRequest {
        program,
        args,
        arg0,
        cwd,
        stdio_policy,
        env,
        creation_flags,
    } = request;

    trace_spawn_child(&program, &args, arg0, &cwd, stdio_policy, &env);

    let mut cmd = Command::new(&program);
    let _ = arg0;
    configure_windows_command_args(cmd.as_std_mut(), program.as_os_str(), &args);
    cmd.current_dir(cwd);
    cmd.env_clear();
    cmd.envs(env);
    cmd.creation_flags(creation_flags | WINDOWS_CREATE_SUSPENDED);

    // If this Codex process dies (including being killed via SIGKILL), we want
    // any child processes that were spawned as part of a `"shell"` tool call
    // to also be terminated.

    match stdio_policy {
        StdioPolicy::RedirectForShellTool => {
            // Do not create a handle for stdin because otherwise some commands may hang waiting
            // for input.
            cmd.stdin(Stdio::null());

            cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        }
    }

    cmd.kill_on_drop(true);
    let runtime = Handle::current();
    // Keep admission in the blocking launch even after its caller stops waiting.
    run_windows_process_operation(WINDOWS_PROCESS_OPERATION_TIMEOUT, move || {
        let child = cmd.spawn()?;
        let observation = codex_utils_pty::process_observation::observe_process_start(
            "legacy_shell",
            codex_utils_pty::process_observation::ProcessRole::RequestedRoot,
            child.id(),
            None,
        );
        Ok(UnclaimedSpawn {
            owned: Some((child, observation, managed_root)),
            runtime,
        })
    })
    .await
}

fn trace_spawn_child(
    program: &PathBuf,
    args: &[String],
    arg0: Option<&str>,
    cwd: &AbsolutePathBuf,
    stdio_policy: StdioPolicy,
    env: &HashMap<String, String>,
) {
    // Arguments carry user and model command text; the log database keeps TRACE, so
    // record only their count.
    trace!(
        ?program,
        args_len = args.len(),
        ?arg0,
        ?cwd,
        ?stdio_policy,
        env_count = env.len(),
        "spawn_child_async"
    );
}
