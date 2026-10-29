#![recursion_limit = "256"]

use clap::Parser;
use codex_app_server::AppServerRuntimeOptions;
use codex_app_server::AppServerTransport;
use codex_app_server::AppServerWebsocketAuthArgs;
use codex_app_server::run_main;
use codex_arg0::Arg0DispatchPaths;
use codex_arg0::arg0_dispatch_or_else;
use codex_config::LoaderOverrides;
use codex_protocol::protocol::SessionSource;
use codex_utils_cli::CliConfigOverrides;

#[derive(Debug, Parser)]
#[command(version = codex_utils_build_info::CODEX_VERSION)]
struct AppServerArgs {
    #[command(flatten)]
    config_overrides: CliConfigOverrides,

    /// Transport endpoint URL. Supported values: `stdio://` (default),
    /// `unix://`, `unix://PATH`, `ws://IP:PORT`, `off`.
    #[arg(
        long = "listen",
        value_name = "URL",
        default_value = AppServerTransport::DEFAULT_LISTEN_URL
    )]
    listen: AppServerTransport,

    /// Session source used to derive product restrictions and metadata.
    #[arg(
        long = "session-source",
        value_name = "SOURCE",
        default_value = "vscode",
        value_parser = SessionSource::from_startup_arg
    )]
    session_source: SessionSource,

    #[command(flatten)]
    auth: AppServerWebsocketAuthArgs,

    /// Fail if config.toml contains unknown configuration fields.
    #[arg(long = "strict-config", default_value_t = false)]
    strict_config: bool,

    /// Enable remote control for this app-server process without changing persistence.
    #[arg(long = "remote-control", hide = true)]
    remote_control: bool,
}

fn main() -> anyhow::Result<()> {
    let remote_control_disabled = codex_app_server::take_remote_control_disabled_env();
    arg0_dispatch_or_else(move |arg0_paths: Arg0DispatchPaths| async move {
        let AppServerArgs {
            config_overrides,
            listen,
            session_source,
            auth,
            strict_config,
            remote_control,
        } = AppServerArgs::parse();
        let loader_overrides = LoaderOverrides::default();
        let transport = listen;
        let auth = auth.try_into_settings()?;
        let runtime_options = AppServerRuntimeOptions {
            remote_control_startup_mode: match (remote_control, remote_control_disabled) {
                (true, _) => codex_app_server::RemoteControlStartupMode::EnabledEphemeral,
                (false, true) => codex_app_server::RemoteControlStartupMode::DisabledEphemeral,
                (false, false) => codex_app_server::RemoteControlStartupMode::ResolvePersisted,
            },
            ..Default::default()
        };

        run_main(
            arg0_paths,
            config_overrides,
            loader_overrides,
            strict_config,
            /*default_analytics_enabled*/ false,
            transport,
            session_source,
            auth,
            runtime_options,
        )
        .await?;
        Ok(())
    })
}


