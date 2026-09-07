use codex_network_proxy::NetworkProxy;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_pty::WINDOWS_CREATE_SUSPENDED;
use codex_utils_pty::WINDOWS_PROCESS_OPERATION_TIMEOUT;
use codex_utils_pty::configure_windows_command_args;
use codex_utils_pty::run_windows_process_operation;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::process::Child;
use tokio::process::Command;
use tracing::trace;

use codex_protocol::permissions::NetworkSandboxPolicy;

/// Experimental environment variable that will be set to some non-empty value
/// if both of the following are true:
///
/// 1. The process was spawned by Codex as part of a shell tool call.
/// 2. NetworkSandboxPolicy is restricted for the tool call.
///
/// We may try to have just one environment variable for all sandboxing
/// attributes, so this may change in the future.
pub const CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR: &str = "CODEX_SANDBOX_NETWORK_DISABLED";

/// Set when the process is spawned under the Windows restricted-token sandbox.
pub const CODEX_SANDBOX_ENV_VAR: &str = "CODEX_SANDBOX";

#[derive(Debug, Clone, Copy)]
pub enum StdioPolicy {
    RedirectForShellTool,
    Inherit,
}

/// Spawns the appropriate child process for the exec params and sandbox settings,
/// ensuring the args and environment variables used to create the `Command`
/// (and `Child`) honor the configuration.
///
/// For now, we take `NetworkSandboxPolicy` as a parameter to spawn_child()
/// because we need to determine whether to set the
/// `CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR` environment variable.
pub(crate) struct SpawnChildRequest<'a> {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub arg0: Option<&'a str>,
    pub cwd: AbsolutePathBuf,
    pub network_sandbox_policy: NetworkSandboxPolicy,
    pub network: Option<&'a NetworkProxy>,
    pub stdio_policy: StdioPolicy,
    pub env: HashMap<String, String>,

    pub creation_flags: u32,
}

pub(crate) async fn spawn_child_async(request: SpawnChildRequest<'_>) -> std::io::Result<Child> {
    let SpawnChildRequest {
        program,
        args,
        arg0,
        cwd,
        network_sandbox_policy,
        network,
        stdio_policy,
        mut env,
        creation_flags,
    } = request;

    if let Some(network) = network {
        network.apply_to_env(&mut env);
    }

    apply_network_sandbox_policy_to_env(&mut env, network_sandbox_policy);
    trace_spawn_child(
        &program,
        &args,
        arg0,
        &cwd,
        network_sandbox_policy,
        stdio_policy,
        &env,
    );

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
        StdioPolicy::Inherit => {
            // Inherit stdin, stdout, and stderr from the parent process.
            cmd.stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit());
        }
    }

    cmd.kill_on_drop(true);
    run_windows_process_operation(WINDOWS_PROCESS_OPERATION_TIMEOUT, move || cmd.spawn()).await
}

fn apply_network_sandbox_policy_to_env(
    env: &mut HashMap<String, String>,
    network_sandbox_policy: NetworkSandboxPolicy,
) {
    env.retain(|key, _| !key.eq_ignore_ascii_case(CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR));
    if !network_sandbox_policy.is_enabled() {
        env.insert(
            CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR.to_string(),
            "1".to_string(),
        );
    }
}

fn trace_spawn_child(
    program: &PathBuf,
    args: &[String],
    arg0: Option<&str>,
    cwd: &AbsolutePathBuf,
    network_sandbox_policy: NetworkSandboxPolicy,
    stdio_policy: StdioPolicy,
    env: &HashMap<String, String>,
) {
    trace!(
        ?program,
        ?args,
        ?arg0,
        ?cwd,
        ?network_sandbox_policy,
        ?stdio_policy,
        env_count = env.len(),
        "spawn_child_async"
    );
}

#[cfg(test)]
#[path = "spawn_tests.rs"]
mod tests;
