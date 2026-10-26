use std::path::PathBuf;
use std::process::Stdio;

use anyhow::Context as _;
use codex_config::LoaderOverrides;
use codex_core::config::Config;
use codex_core::config::ConfigBuilder;
use codex_core::config::ConfigOverrides;
use codex_core::exec_env::create_env;
use codex_core::spawn::CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR;
use codex_protocol::config_types::SandboxMode;
use codex_protocol::models::PermissionProfile;
use codex_protocol::models::SandboxEnforcement;
use codex_protocol::permissions::NetworkSandboxPolicy;
use codex_utils_absolute_path::AbsolutePathBuf;
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
    let sandbox_state_readable_root = config_options
        .sandbox_state
        .sandbox_state_readable_root
        .clone();
    let sandbox_state_disable_network = config_options.sandbox_state.sandbox_state_disable_network;
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
        /*strict_config*/ false,
    )
    .await?;

    // In practice, this should be `std::env::current_dir()` because this CLI
    // does not support `--cwd`, but let's use the config value for consistency.
    let cwd = config.cwd.clone();
    let workspace_roots = config.effective_workspace_roots();

    let env = create_env(
        &config.permissions.shell_environment_policy,
        /*thread_id*/ None,
    );
    let mut permission_profile = match sandbox_state.as_ref() {
        Some(state) => match &state.permission_profile {
            PermissionProfile::External { .. } => {
                // `External` only says that the producer relies on an outer sandbox; it does not
                // include filesystem permissions we can recreate here. The consumer may not share
                // that sandbox, so use a locally enforceable read-only profile instead of spawning
                // without a sandbox.
                PermissionProfile::read_only()
            }
            permission_profile => permission_profile.clone(),
        },
        None => config.permissions.effective_permission_profile(),
    };
    if matches!(permission_profile, PermissionProfile::Disabled) && sandbox_state_disable_network {
        anyhow::bail!(
            "--sandbox-state-disable-network cannot be applied to a disabled permission profile"
        );
    }
    if !matches!(permission_profile, PermissionProfile::Disabled)
        && (!sandbox_state_readable_root.is_empty() || sandbox_state_disable_network)
    {
        let file_system = permission_profile
            .file_system_sandbox_policy()
            .with_additional_readable_roots(&cwd, &sandbox_state_readable_root);
        let network = if sandbox_state_disable_network {
            NetworkSandboxPolicy::Restricted
        } else {
            permission_profile.network_sandbox_policy()
        };
        permission_profile = PermissionProfile::from_runtime_permissions(&file_system, network);
    }
    match permission_profile.enforcement() {
        SandboxEnforcement::Managed => {}
        SandboxEnforcement::Disabled | SandboxEnforcement::External => {
            let (program, args) = command
                .split_first()
                .context("sandbox command must not be empty")?;
            let mut child = spawn_debug_sandbox_child(
                PathBuf::from(program),
                args.to_vec(),
                cwd.to_path_buf(),
                permission_profile.network_sandbox_policy(),
                env,
                |_| {},
            )
            .await?;
            handle_exit_status(child.wait().await?);
        }
    }

    run_command_under_windows_session(
        &config,
        &permission_profile,
        command,
        cwd,
        workspace_roots,
        env,
    )
    .await;
}

async fn run_command_under_windows_session(
    config: &Config,
    permission_profile: &PermissionProfile,
    command: Vec<String>,
    cwd: AbsolutePathBuf,
    workspace_roots: Vec<AbsolutePathBuf>,
    env: std::collections::HashMap<String, String>,
) -> ! {
    use codex_core::windows_sandbox::WindowsSandboxLevelExt;
    use codex_protocol::config_types::WindowsSandboxLevel;
    use codex_windows_sandbox::WindowsSandboxProxySettingsMode;
    use codex_windows_sandbox::WindowsSandboxSessionRequest;
    use codex_windows_sandbox::spawn_windows_sandbox_session_for_level;

    let empty_paths: &[AbsolutePathBuf] = &[];
    let spawned = spawn_windows_sandbox_session_for_level(WindowsSandboxSessionRequest {
        permission_profile,
        workspace_roots: workspace_roots.as_slice(),
        codex_home: config.codex_home.as_path(),
        command,
        cwd: cwd.as_path(),
        env_map: env,
        windows_sandbox_level: WindowsSandboxLevel::from_config(config),
        proxy_settings_mode: WindowsSandboxProxySettingsMode::Reconcile,
        proxy_enforced: false,
        timeout_ms: None,
        read_roots_override: None,
        additional_read_roots: empty_paths,
        read_roots_include_platform_defaults: false,
        write_roots_override: None,
        deny_read_paths_override: empty_paths,
        deny_write_paths_override: empty_paths,
        tty: false,
        stdin_open: true,
        use_private_desktop: config.permissions.windows_sandbox_private_desktop,
    })
    .await;

    let spawned = match spawned {
        Ok(spawned) => spawned,
        Err(err) => {
            eprintln!("windows sandbox failed: {err}");
            std::process::exit(1);
        }
    };

    let outcome = codex_windows_sandbox::forward_sandbox_session_stdio(spawned).await;
    std::process::exit(outcome.exit_code());
}

async fn spawn_debug_sandbox_child(
    program: PathBuf,
    args: Vec<String>,
    cwd: PathBuf,
    network_sandbox_policy: NetworkSandboxPolicy,
    mut env: std::collections::HashMap<String, String>,
    apply_env: impl FnOnce(&mut std::collections::HashMap<String, String>),
) -> std::io::Result<Child> {
    let mut cmd = TokioCommand::new(&program);
    cmd.args(args);
    cmd.current_dir(cwd);
    apply_env(&mut env);
    cmd.env_clear();
    cmd.envs(env);

    if !network_sandbox_policy.is_enabled() {
        cmd.env(CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR, "1");
    }

    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
}

async fn load_debug_sandbox_config(
    cli_overrides: Vec<(String, TomlValue)>,
    options: DebugSandboxConfigOptions,
    strict_config: bool,
) -> anyhow::Result<Config> {
    load_debug_sandbox_config_with_codex_home(
        cli_overrides,
        options,
        /*codex_home*/ None,
        strict_config,
    )
    .await
}

async fn load_debug_sandbox_config_with_codex_home(
    cli_overrides: Vec<(String, TomlValue)>,
    options: DebugSandboxConfigOptions,
    codex_home: Option<PathBuf>,
    strict_config: bool,
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

    // For legacy configs, `codex sandbox` historically defaulted to read-only
    // instead of inheriting ambient `sandbox_mode` settings from user/system
    // config. Keep that behavior unless this invocation explicitly passes a
    // legacy `sandbox_mode` CLI override for compatibility with older callers.
    let uses_legacy_sandbox_mode_override = cli_overrides_use_legacy_sandbox_mode(&cli_overrides);
    let config = build_debug_sandbox_config_with_loader_overrides(
        cli_overrides.clone(),
        ConfigOverrides {
            cwd: cwd.clone(),
            ..Default::default()
        },
        codex_home.clone(),
        managed_requirements_mode,
        loader_overrides.clone(),
        strict_config,
    )
    .await?;

    if config_uses_permission_profiles(&config) || uses_legacy_sandbox_mode_override {
        return Ok(config);
    }

    build_debug_sandbox_config_with_loader_overrides(
        cli_overrides,
        ConfigOverrides {
            sandbox_mode: Some(SandboxMode::ReadOnly),
            cwd,
            ..Default::default()
        },
        codex_home,
        managed_requirements_mode,
        loader_overrides,
        strict_config,
    )
    .await
    .map_err(Into::into)
}

async fn build_debug_sandbox_config_with_loader_overrides(
    cli_overrides: Vec<(String, TomlValue)>,
    harness_overrides: ConfigOverrides,
    codex_home: Option<PathBuf>,
    managed_requirements_mode: ManagedRequirementsMode,
    mut loader_overrides: LoaderOverrides,
    strict_config: bool,
) -> std::io::Result<Config> {
    let mut builder = ConfigBuilder::default()
        .cli_overrides(cli_overrides)
        .harness_overrides(harness_overrides)
        .strict_config(strict_config);
    if matches!(managed_requirements_mode, ManagedRequirementsMode::Ignore) {
        loader_overrides.ignore_managed_requirements = true;
    }
    builder = builder.loader_overrides(loader_overrides);
    if let Some(codex_home) = codex_home {
        builder = builder
            .codex_home(codex_home.clone())
            .fallback_cwd(Some(codex_home));
    }
    builder.build().await
}

fn config_uses_permission_profiles(config: &Config) -> bool {
    config
        .config_layer_stack
        .effective_config()
        .get("default_permissions")
        .is_some()
}

fn cli_overrides_use_legacy_sandbox_mode(cli_overrides: &[(String, TomlValue)]) -> bool {
    cli_overrides.iter().any(|(key, _)| key == "sandbox_mode")
}

