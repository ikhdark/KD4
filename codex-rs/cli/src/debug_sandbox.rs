use std::path::PathBuf;
use std::process::Stdio;

use anyhow::Context as _;
use codex_config::LoaderOverrides;
use codex_core::config::Config;
use codex_core::config::ConfigBuilder;
use codex_core::config::ConfigOverrides;
use codex_core::exec_env::create_env;
use codex_protocol::config_types::SandboxMode;
use codex_utils_cli::CliConfigOverrides;
use tokio::process::Child;
use tokio::process::Command as TokioCommand;
use toml::Value as TomlValue;

use crate::WindowsCommand;
use crate::exit_status::handle_exit_status;

pub async fn run_command_under_windows_sandbox(
    command: WindowsCommand,
    loader_overrides: LoaderOverrides,
) -> anyhow::Result<()> {
    let WindowsCommand {
        sandbox_state,
        permissions_profile,
        config_profile: _,
        cwd,
        include_managed_config,
        config_overrides,
        command,
    } = command;
    let managed_requirements_mode = ManagedRequirementsMode::for_profile_invocation(
        &permissions_profile,
        include_managed_config,
    );
    run_command_under_sandbox(
        DebugSandboxConfigOptions {
            sandbox_state,
            permissions_profile,
            cwd,
            managed_requirements_mode,
            loader_overrides,
        },
        command,
        config_overrides,
    )
    .await
}

#[derive(Debug)]
struct DebugSandboxConfigOptions {
    sandbox_state: crate::SandboxStateArgs,
    permissions_profile: Option<String>,
    cwd: Option<PathBuf>,
    managed_requirements_mode: ManagedRequirementsMode,
    loader_overrides: LoaderOverrides,
}

#[derive(Debug, Clone, Copy)]
enum ManagedRequirementsMode {
    Include,
    Ignore,
}

impl ManagedRequirementsMode {
    fn for_profile_invocation(
        permissions_profile: &Option<String>,
        include_managed_config: bool,
    ) -> Self {
        if permissions_profile.is_some() && !include_managed_config {
            Self::Ignore
        } else {
            Self::Include
        }
    }
}

async fn run_command_under_sandbox(
    mut config_options: DebugSandboxConfigOptions,
    command: Vec<String>,
    config_overrides: CliConfigOverrides,
) -> anyhow::Result<()> {
    let sandbox_state = config_options
        .sandbox_state
        .sandbox_state_json
        .as_deref()
        .map(serde_json::from_str::<codex_mcp::SandboxState>)
        .transpose()
        .map_err(|err| anyhow::anyhow!("invalid --sandbox-state-json value: {err}"))?;
    if let Some(state) = sandbox_state.as_ref() {
        config_options.cwd = Some(
            state
                .sandbox_cwd
                .to_abs_path()
                .context("sandbox state cwd is not native to this host")?
                .to_path_buf(),
        );
    }
    let config = load_debug_sandbox_config(
        config_overrides
            .parse_overrides()
            .map_err(anyhow::Error::msg)?,
        config_options,
    )
    .await?;

    // The resolved config carries `-C`/`--cd` or the sandbox-state directory
    // when one was given, and the current directory otherwise.
    let cwd = config.cwd.clone();

    let env = create_env(
        &config.permissions.shell_environment_policy,
        /*thread_id*/ None,
    );
    // Legacy sandbox restrictions are accepted but never enforced in KD4.
    let (program, args) = command
        .split_first()
        .context("sandbox command must not be empty")?;
    let mut child =
        spawn_debug_sandbox_child(PathBuf::from(program), args.to_vec(), cwd.to_path_buf(), env)
            .await?;
    handle_exit_status(child.wait().await?);
}

async fn spawn_debug_sandbox_child(
    program: PathBuf,
    args: Vec<String>,
    cwd: PathBuf,
    env: std::collections::HashMap<String, String>,
) -> std::io::Result<Child> {
    let mut cmd = TokioCommand::new(&program);
    cmd.args(args);
    cmd.current_dir(cwd);
    cmd.env_clear();
    cmd.envs(env);

    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
}

async fn load_debug_sandbox_config(
    cli_overrides: Vec<(String, TomlValue)>,
    options: DebugSandboxConfigOptions,
) -> anyhow::Result<Config> {
    let DebugSandboxConfigOptions {
        sandbox_state: _,
        permissions_profile,
        cwd,
        managed_requirements_mode,
        loader_overrides,
    } = options;
    let mut cli_overrides = cli_overrides;

    if let Some(permissions_profile) = permissions_profile {
        cli_overrides.push((
            "default_permissions".to_string(),
            TomlValue::String(permissions_profile),
        ));
    }

    build_debug_sandbox_config_with_loader_overrides(
        cli_overrides,
        ConfigOverrides {
            sandbox_mode: Some(SandboxMode::DangerFullAccess),
            cwd,
            ..Default::default()
        },
        managed_requirements_mode,
        loader_overrides,
    )
    .await
    .map_err(Into::into)
}

async fn build_debug_sandbox_config_with_loader_overrides(
    cli_overrides: Vec<(String, TomlValue)>,
    harness_overrides: ConfigOverrides,
    managed_requirements_mode: ManagedRequirementsMode,
    mut loader_overrides: LoaderOverrides,
) -> std::io::Result<Config> {
    let mut builder = ConfigBuilder::default()
        .cli_overrides(cli_overrides)
        .harness_overrides(harness_overrides)
        .strict_config(false);
    if matches!(managed_requirements_mode, ManagedRequirementsMode::Ignore) {
        loader_overrides.ignore_managed_requirements = true;
    }
    builder = builder.loader_overrides(loader_overrides);
    builder.build().await
}
