use clap::Args;
use clap::CommandFactory;
use clap::FromArgMatches;
use clap::Parser;
use clap_complete::Shell;
use clap_complete::generate;
use codex_arg0::Arg0DispatchPaths;
use codex_arg0::arg0_dispatch_or_else_with_pre_runtime;
use codex_chatgpt::apply_command::ApplyCommand;
use codex_chatgpt::apply_command::run_apply_command;
use codex_cli::read_access_token_from_stdin;
use codex_cli::read_api_key_from_stdin;
use codex_cli::run_login_status;
use codex_cli::run_login_with_access_token;
use codex_cli::run_login_with_api_key;
use codex_cli::run_login_with_chatgpt;
use codex_cli::run_login_with_device_code;
use codex_cli::run_logout;
use codex_exec::Cli as ExecCli;
use codex_exec::Command as ExecCommand;
use codex_exec::ReviewArgs;
use codex_execpolicy::ExecPolicyCheckCommand;

use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_cli::ApprovalModeCliArg;
use codex_utils_cli::CliConfigOverrides;
use codex_utils_cli::ProfileV2Name;
use codex_utils_cli::SharedCliOptions;
use std::path::PathBuf;
use std::sync::Arc;

mod app_cmd;
mod desktop_app;
mod doctor;
mod exec_server_telemetry;
mod marketplace_cmd;
mod mcp_cmd;
mod plugin_cmd;
mod sandbox_setup;

use crate::mcp_cmd::McpCli;
use crate::plugin_cmd::PluginCli;
use crate::plugin_cmd::PluginSubcommand;
use doctor::DoctorCommand;

use codex_config::LoaderOverrides;
use codex_core::build_models_manager;
use codex_core::config::ConfigBuilder;
use codex_core::config::ConfigOverrides;
use codex_core::config::edit::ConfigEditsBuilder;
use codex_core::config::find_codex_home;
use codex_core::config::load_config_toml_with_layer_stack;
use codex_core::config::resolve_oss_provider;
use codex_core::config::resolve_profile_v2_config_path;
use codex_features::Stage;
use codex_features::feature_for_key;
use codex_features::user_settable_feature_for_key;
use codex_features::user_settable_features;
use codex_home::CodexHomeUserInstructionsProvider;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_login::read_codex_access_token_from_env;

use codex_models_manager::bundled_models_response;
use codex_models_manager::manager::RefreshStrategy;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::user_input::UserInput;
use codex_utils_oss::get_default_model_for_oss_provider;

/// Codex CLI
#[derive(Debug, Parser)]
#[clap(
    author,
    // A bare `version` would embed this crate's Cargo version instead of the
    // shared release-aware version.
    version = codex_utils_build_info::CODEX_VERSION,
    // Root option requirements are validated after subcommand routing.
    subcommand_negates_reqs = true,
    // The executable is sometimes invoked via a platform-specific name like
    // `codex-x86_64-pc-windows-msvc.exe`, but the help output should always use
    // the generic `codex` command name that users run.
    bin_name = "codex",
    subcommand_required = true,
    arg_required_else_help = true,
    override_usage = "codex [OPTIONS] <COMMAND> [ARGS]"
)]
struct MultitoolCli {
    #[clap(flatten)]
    pub config_overrides: CliConfigOverrides,

    #[clap(flatten)]
    pub feature_toggles: FeatureToggles,

    #[clap(flatten)]
    root: RootOptions,

    #[clap(subcommand)]
    subcommand: Option<Subcommand>,
}

/// Runtime options accepted before a subcommand. Each subcommand's routing
/// policy decides which of them it inherits.
#[derive(Debug, Parser, Clone, Default)]
struct RootOptions {
    /// Error out when config.toml contains fields that are not recognized by this version of Codex.
    #[arg(long = "strict-config", default_value_t = false)]
    strict_config: bool,

    #[clap(flatten)]
    shared: SharedCliOptions,

    /// Configure when the model requires human approval before executing a command.
    #[arg(
        long = "ask-for-approval",
        short = 'a',
        conflicts_with = "dangerously_bypass_approvals_and_sandbox"
    )]
    approval_policy: Option<ApprovalModeCliArg>,

    /// Enable live web search. When enabled, the native Responses `web_search` tool is available to the model (no per-call approval).
    #[arg(long = "search", default_value_t = false)]
    web_search: bool,
}

impl std::ops::Deref for RootOptions {
    type Target = SharedCliOptions;

    fn deref(&self) -> &Self::Target {
        &self.shared
    }
}

#[derive(Debug, clap::Subcommand)]
enum Subcommand {
    /// Run Codex non-interactively.
    #[clap(visible_alias = "e")]
    Exec(ExecCli),

    /// Unavailable: review is not supported by the exec app-server protocol.
    #[command(hide = true)]
    Review(ReviewCommand),

    /// Manage login.
    Login(LoginCommand),

    /// Remove stored authentication credentials.
    Logout(LogoutCommand),

    /// Manage external MCP servers for Codex.
    Mcp(McpCli),

    /// Manage Codex plugins.
    Plugin(PluginCli),

    /// [experimental] Run the app server or related tooling.
    AppServer(AppServerCommand),

    /// Launch the Codex desktop app (opens the app installer if missing).
    App(app_cmd::AppCommand),

    /// Generate shell completion scripts.
    Completion(CompletionCommand),

    /// Diagnose local Codex installation, config, auth, and runtime health.
    Doctor(DoctorCommand),

    /// Run commands within a Codex-provided sandbox.
    Sandbox(HostSandboxArgs),

    /// Debugging tools.
    Debug(DebugCommand),

    /// Execpolicy tooling.
    #[clap(hide = true)]
    Execpolicy(ExecpolicyCommand),

    /// Apply the latest diff produced by Codex agent as a `git apply` to your local working tree.
    #[clap(visible_alias = "a")]
    Apply(ApplyCommand),

    /// Internal: relay stdio to a Unix domain socket.
    #[clap(hide = true, name = "stdio-to-uds")]
    StdioToUds(StdioToUdsCommand),

    /// [EXPERIMENTAL] Run the standalone exec-server service.
    ExecServer(ExecServerCommand),

    /// Inspect feature flags.
    Features(FeaturesCli),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SubcommandRoutingPolicy {
    strict_config_name: &'static str,
    supports_profile: bool,
    supports_strict_config: bool,
}

impl SubcommandRoutingPolicy {
    const fn new(
        command_name: &'static str,
        supports_profile: bool,
        supports_strict_config: bool,
    ) -> Self {
        Self {
            strict_config_name: command_name,
            supports_profile,
            supports_strict_config,
        }
    }

    const fn with_strict_config_name(mut self, strict_config_name: &'static str) -> Self {
        self.strict_config_name = strict_config_name;
        self
    }
}

impl Subcommand {
    /// Authoritative policy for root-option routing into a selected command.
    fn routing_policy(&self) -> SubcommandRoutingPolicy {
        match self {
            Self::Exec(_) => SubcommandRoutingPolicy::new("exec", true, true),
            Self::Review(_) => SubcommandRoutingPolicy::new("review", true, true),
            Self::Login(_) => SubcommandRoutingPolicy::new("login", false, false),
            Self::Logout(_) => SubcommandRoutingPolicy::new("logout", false, false),
            Self::Mcp(_) => SubcommandRoutingPolicy::new("mcp", true, false),
            Self::Plugin(_) => SubcommandRoutingPolicy::new("plugin", false, false),
            Self::AppServer(app_server) => {
                let name = app_server_subcommand_name(app_server.subcommand.as_ref());
                SubcommandRoutingPolicy::new(name, false, app_server.subcommand.is_none())
            }
            Self::App(_) => SubcommandRoutingPolicy::new("app", false, false),
            Self::Completion(_) => SubcommandRoutingPolicy::new("completion", false, false),
            Self::Doctor(_) => SubcommandRoutingPolicy::new("doctor", false, true),
            Self::Sandbox(_) => SubcommandRoutingPolicy::new("sandbox", true, false),
            Self::Debug(DebugCommand { subcommand }) => {
                let (name, supports_profile) = match subcommand {
                    DebugSubcommand::Models(_) => ("debug models", false),
                    DebugSubcommand::PromptInput(_) => ("debug prompt-input", true),
                };
                SubcommandRoutingPolicy::new(name, supports_profile, false)
                    .with_strict_config_name("debug")
            }
            Self::Execpolicy(ExecpolicyCommand { sub }) => {
                let name = match sub {
                    ExecpolicySubcommand::Check(_) => "execpolicy check",
                };
                SubcommandRoutingPolicy::new(name, false, false)
                    .with_strict_config_name("execpolicy")
            }
            Self::Apply(_) => SubcommandRoutingPolicy::new("apply", false, false),
            Self::StdioToUds(_) => SubcommandRoutingPolicy::new("stdio-to-uds", false, false),
            Self::ExecServer(_) => SubcommandRoutingPolicy::new("exec-server", false, true),
            Self::Features(FeaturesCli { sub }) => {
                let name = match sub {
                    FeaturesSubcommand::List => "features list",
                    FeaturesSubcommand::Enable(_) => "features enable",
                    FeaturesSubcommand::Disable(_) => "features disable",
                };
                SubcommandRoutingPolicy::new(name, false, false).with_strict_config_name("features")
            }
        }
    }
}

#[derive(Debug, Parser)]
struct CompletionCommand {
    /// Shell to generate completions for
    #[clap(value_enum, default_value_t = Shell::Bash)]
    shell: Shell,
}

#[derive(Debug, Parser)]
struct DebugCommand {
    #[command(subcommand)]
    subcommand: DebugSubcommand,
}

#[derive(Debug, clap::Subcommand)]
enum DebugSubcommand {
    /// Render the raw model catalog as JSON.
    Models(DebugModelsCommand),

    /// Render the model-visible prompt input list as JSON.
    PromptInput(DebugPromptInputCommand),

}

#[derive(Debug, Parser)]
struct DebugPromptInputCommand {
    /// Optional user prompt to append after session context.
    #[arg(value_name = "PROMPT")]
    prompt: Option<String>,

    /// Image(s) to attach to the user prompt. Repeat -i or use comma-separated paths.
    #[arg(
        long = "image",
        short = 'i',
        value_name = "FILE",
        value_delimiter = ',',
        num_args = 1
    )]
    images: Vec<PathBuf>,
}

#[derive(Debug, Parser)]
struct DebugModelsCommand {
    /// Skip refresh and dump only the bundled catalog shipped with this binary.
    #[arg(long = "bundled", default_value_t = false)]
    bundled: bool,
}

#[derive(Debug, Parser)]
struct ReviewCommand {
    /// Error out when config.toml contains fields that are not recognized by this version of Codex.
    #[arg(long = "strict-config", default_value_t = false)]
    strict_config: bool,

    #[clap(flatten)]
    args: ReviewArgs,
}

#[derive(Debug, Parser)]
#[command(args_conflicts_with_subcommands = true)]
struct HostSandboxArgs {
    #[command(flatten)]
    execution: codex_cli::WindowsCommand,

    #[command(subcommand)]
    action: Option<SandboxAction>,
}

#[derive(Debug, clap::Subcommand)]
enum SandboxAction {
    /// Provision the Windows sandbox for a Codex user (requires elevation).
    Setup(sandbox_setup::SandboxSetupCommand),
}

#[derive(Debug, Parser)]
struct ExecpolicyCommand {
    #[command(subcommand)]
    sub: ExecpolicySubcommand,
}

#[derive(Debug, clap::Subcommand)]
enum ExecpolicySubcommand {
    /// Check execpolicy files against a command.
    #[clap(name = "check")]
    Check(ExecPolicyCheckCommand),
}

#[derive(Debug, Parser)]
#[command(args_conflicts_with_subcommands = true)]
struct LoginCommand {
    #[clap(skip)]
    config_overrides: CliConfigOverrides,

    #[arg(
        long = "with-api-key",
        help = "Read the API key from stdin (e.g. `printenv OPENAI_API_KEY | codex login --with-api-key`)",
        conflicts_with_all = ["with_access_token", "use_device_code"]
    )]
    with_api_key: bool,

    #[arg(
        long = "with-access-token",
        help = "Read the access token from stdin (e.g. `printenv CODEX_ACCESS_TOKEN | codex login --with-access-token`)",
        conflicts_with = "use_device_code"
    )]
    with_access_token: bool,

    #[arg(long = "device-auth")]
    use_device_code: bool,

    /// EXPERIMENTAL: Use custom OAuth issuer base URL (advanced)
    /// Override the OAuth issuer base URL (advanced)
    #[arg(
        long = "experimental_issuer",
        value_name = "URL",
        hide = true,
        requires = "use_device_code"
    )]
    issuer_base_url: Option<String>,

    /// EXPERIMENTAL: Use custom OAuth client ID (advanced)
    #[arg(
        long = "experimental_client-id",
        value_name = "CLIENT_ID",
        hide = true,
        requires = "use_device_code"
    )]
    client_id: Option<String>,

    #[command(subcommand)]
    action: Option<LoginSubcommand>,
}

#[derive(Debug, clap::Subcommand)]
enum LoginSubcommand {
    /// Show login status.
    Status,
}

#[derive(Debug, Parser)]
struct LogoutCommand {
    #[clap(skip)]
    config_overrides: CliConfigOverrides,
}

#[derive(Debug, Parser)]
struct AppServerCommand {
    /// Omit to run the app server; specify a subcommand for tooling.
    #[command(subcommand)]
    subcommand: Option<AppServerSubcommand>,

    /// Error out when config.toml contains fields that are not recognized by this version of Codex.
    #[arg(long = "strict-config", default_value_t = false)]
    strict_config: bool,

    /// Transport endpoint URL. Supported values: `stdio://` (default),
    /// `unix://`, `unix://PATH`, `ws://IP:PORT`, `off`.
    #[arg(long = "listen", value_name = "URL")]
    listen: Option<codex_app_server::AppServerTransport>,

    /// Use stdio as the transport (equivalent to `--listen stdio://`).
    #[arg(long = "stdio", conflicts_with = "listen")]
    stdio: bool,

    /// Enable remote control for this app-server process without changing persistence.
    #[arg(long = "remote-control", hide = true)]
    remote_control: bool,

    /// Controls whether analytics are enabled by default.
    ///
    /// Analytics are disabled by default for app-server. Users have to explicitly opt in
    /// via the `analytics` section in the config.toml file.
    ///
    /// However, for first-party use cases like the VSCode IDE extension, we default analytics
    /// to be enabled by default by setting this flag. Users can still opt out by setting this
    /// in their config.toml:
    ///
    /// ```toml
    /// [analytics]
    /// enabled = false
    /// ```
    ///
    /// See https://developers.openai.com/codex/config-advanced/#metrics for more details.
    #[arg(long = "analytics-default-enabled")]
    analytics_default_enabled: bool,

    #[command(flatten)]
    auth: codex_app_server::AppServerWebsocketAuthArgs,
}

#[derive(Debug, Parser)]
struct ExecServerCommand {
    /// Error out when config.toml contains fields that are not recognized by this version of Codex.
    #[arg(long = "strict-config", default_value_t = false)]
    strict_config: bool,

    /// Transport endpoint URL. `ws://IP:PORT` (default) requires a private per-launch
    /// CODEX_EXEC_SERVER_AUTH_TOKEN (at least 32 URL-safe ASCII characters).
    /// `stdio` and `stdio://` use the private parent-child channel without a token.
    #[arg(long = "listen", value_name = "URL", conflicts_with = "remote")]
    listen: Option<String>,

    /// Register this exec-server as a remote environment using the given base URL.
    #[arg(long = "remote", value_name = "URL", requires = "environment_id")]
    remote: Option<String>,

    /// Environment id to attach to when registering remotely.
    #[arg(long = "environment-id", value_name = "ID", requires = "remote")]
    environment_id: Option<String>,

    /// Human-readable environment name.
    #[arg(long = "name", value_name = "NAME", requires = "remote")]
    name: Option<String>,

    /// Use Agent Identity auth from CODEX_ACCESS_TOKEN for remote registration.
    #[arg(long = "use-agent-identity-auth", requires = "remote")]
    use_agent_identity_auth: bool,
}

#[derive(Debug, clap::Subcommand)]
#[allow(clippy::enum_variant_names)]
enum AppServerSubcommand {
    /// Proxy stdio bytes to the running app-server control socket.
    Proxy(AppServerProxyCommand),

    /// [experimental] Generate TypeScript bindings for the app server protocol.
    GenerateTs(GenerateTsCommand),

    /// [experimental] Generate JSON Schema for the app server protocol.
    GenerateJsonSchema(GenerateJsonSchemaCommand),

    /// [internal] Generate internal JSON Schema artifacts for Codex tooling.
    #[clap(hide = true)]
    GenerateInternalJsonSchema(GenerateInternalJsonSchemaCommand),
}

#[derive(Debug, Args)]
struct AppServerProxyCommand {
    /// Path to the app-server Unix domain socket to connect to.
    #[arg(long = "sock", value_name = "SOCKET_PATH", value_parser = parse_socket_path)]
    socket_path: Option<AbsolutePathBuf>,
}

#[derive(Debug, Args)]
struct GenerateTsCommand {
    /// Output directory where .ts files will be written
    #[arg(short = 'o', long = "out", value_name = "DIR")]
    out_dir: PathBuf,

    /// Optional path to the Prettier executable to format generated files
    #[arg(short = 'p', long = "prettier", value_name = "PRETTIER_BIN")]
    prettier: Option<PathBuf>,

    /// Include experimental methods and fields in the generated output
    #[arg(long = "experimental", default_value_t = false)]
    experimental: bool,
}

#[derive(Debug, Args)]
struct GenerateJsonSchemaCommand {
    /// Output directory where the schema bundle will be written
    #[arg(short = 'o', long = "out", value_name = "DIR")]
    out_dir: PathBuf,

    /// Include experimental methods and fields in the generated output
    #[arg(long = "experimental", default_value_t = false)]
    experimental: bool,
}

#[derive(Debug, Args)]
struct GenerateInternalJsonSchemaCommand {
    /// Output directory where internal JSON Schema artifacts will be written
    #[arg(short = 'o', long = "out", value_name = "DIR")]
    out_dir: PathBuf,
}

#[derive(Debug, Parser)]
struct StdioToUdsCommand {
    /// Path to the Unix domain socket to connect to.
    #[arg(value_name = "SOCKET_PATH", value_parser = parse_socket_path)]
    socket_path: AbsolutePathBuf,
}

fn parse_socket_path(raw: &str) -> Result<AbsolutePathBuf, String> {
    AbsolutePathBuf::relative_to_current_dir(raw)
        .map_err(|err| format!("failed to resolve socket path `{raw}`: {err}"))
}

#[derive(Debug, Default, Parser, Clone)]
struct FeatureToggles {
    /// Enable a feature (repeatable). Equivalent to `-c features.<name>=true`.
    #[arg(long = "enable", value_name = "FEATURE", action = clap::ArgAction::Append, global = true)]
    enable: Vec<String>,

    /// Disable a feature (repeatable). Equivalent to `-c features.<name>=false`.
    #[arg(long = "disable", value_name = "FEATURE", action = clap::ArgAction::Append, global = true)]
    disable: Vec<String>,
}

impl FeatureToggles {
    fn scope_to_each_command_level(command: clap::Command) -> clap::Command {
        let mut command = if command.get_arguments().any(|arg| arg.get_id() == "enable") {
            command
        } else {
            Self::augment_args(command)
        }
        .mut_arg("enable", |arg| arg.global(false))
        .mut_arg("disable", |arg| arg.global(false));
        for subcommand in command.get_subcommands_mut() {
            *subcommand = Self::scope_to_each_command_level(std::mem::take(subcommand));
        }
        command
    }

    fn ordered_config_overrides(
        matches: &clap::ArgMatches,
    ) -> Result<CliConfigOverrides, clap::Error> {
        let mut raw_overrides = Vec::new();
        let mut level = Some(matches);
        while let Some(matches) = level {
            let mut occurrences = Vec::new();
            for (id, enabled) in [
                ("raw_overrides", None),
                ("enable", Some(true)),
                ("disable", Some(false)),
            ] {
                if let (Some(indices), Some(values)) =
                    (matches.indices_of(id), matches.get_many::<String>(id))
                {
                    for (index, value) in indices.zip(values) {
                        let value = if let Some(enabled) = enabled {
                            Self::validate_feature(value).map_err(|error| {
                                clap::Error::raw(
                                    clap::error::ErrorKind::ValueValidation,
                                    error.to_string(),
                                )
                            })?;
                            format!("features.{value}={enabled}")
                        } else {
                            value.clone()
                        };
                        occurrences.push((index, value));
                    }
                }
            }
            occurrences.sort_by_key(|(index, _)| *index);
            raw_overrides.extend(occurrences.into_iter().map(|(_, value)| value));
            level = matches.subcommand().map(|(_, matches)| matches);
        }
        Ok(CliConfigOverrides { raw_overrides })
    }

    fn to_overrides(&self) -> anyhow::Result<Vec<String>> {
        let mut v = Vec::new();
        for feature in &self.enable {
            Self::validate_feature(feature)?;
            v.push(format!("features.{feature}=true"));
        }
        for feature in &self.disable {
            Self::validate_feature(feature)?;
            v.push(format!("features.{feature}=false"));
        }
        Ok(v)
    }

    fn validate_feature(feature: &str) -> anyhow::Result<()> {
        if user_settable_feature_for_key(feature).is_some() {
            return Ok(());
        }
        anyhow::bail!("Unknown feature flag: {feature}")
    }
}

#[derive(Debug, Parser)]
struct FeaturesCli {
    #[command(subcommand)]
    sub: FeaturesSubcommand,
}

#[derive(Debug, Parser)]
enum FeaturesSubcommand {
    /// List known features with their stage and effective state.
    List,
    /// Enable a feature in config.toml.
    Enable(FeatureSetArgs),
    /// Disable a feature in config.toml.
    Disable(FeatureSetArgs),
}

#[derive(Debug, Parser)]
struct FeatureSetArgs {
    /// Feature key to update (for example: unified_exec).
    feature: String,
}

fn stage_str(stage: Stage) -> &'static str {
    match stage {
        Stage::UnderDevelopment => "under development",
        Stage::Experimental { .. } => "experimental",
        Stage::Stable => "stable",
        Stage::Deprecated => "deprecated",
        Stage::Internal => "internal",
    }
}

fn validate_root_shared_options(shared: &SharedCliOptions) -> anyhow::Result<()> {
    if !shared.oss && shared.oss_provider.is_some() {
        anyhow::bail!("--local-provider requires --oss");
    }
    Ok(())
}

/// Parses the command line, keeping config overrides and sandbox selections
/// at their original command levels before applying precedence.
///
/// Raw config and feature spellings share last-occurrence precedence, and are
/// handed to consumers exactly once instead of replaying propagated child copies.
fn parse_multitool_cli(args: impl IntoIterator<Item = std::ffi::OsString>) -> MultitoolCli {
    try_parse_multitool_cli(args).unwrap_or_else(|error| error.exit())
}

fn try_parse_multitool_cli(
    args: impl IntoIterator<Item = std::ffi::OsString>,
) -> Result<MultitoolCli, clap::Error> {
    let args: Vec<std::ffi::OsString> = args.into_iter().collect();
    let setup_child_delimited = args
        .iter()
        .position(|arg| arg == "--")
        .is_some_and(|index| args.get(index + 1).is_some_and(|arg| arg == "setup"));
    let command = SharedCliOptions::scope_sandbox_selection(MultitoolCli::command());
    let matches = command.clone().try_get_matches_from(&args)?;
    let has_overrides = matches.get_many::<String>("raw_overrides").is_some()
        || matches.get_many::<String>("enable").is_some()
        || matches.get_many::<String>("disable").is_some();
    let config_overrides = if has_overrides {
        let scoped_command = FeatureToggles::scope_to_each_command_level(
            CliConfigOverrides::scope_to_each_command_level(
                SharedCliOptions::scope_sandbox_selection(MultitoolCli::command()),
            ),
        );
        let scoped_matches = scoped_command.try_get_matches_from(&args)?;
        FeatureToggles::ordered_config_overrides(&scoped_matches)?
    } else {
        CliConfigOverrides::default()
    };
    let matches = if has_overrides {
        let trailing_child_values = trailing_child_value_count(&command, &matches);
        command.try_get_matches_from(without_config_flags(args, trailing_child_values))?
    } else {
        matches
    };
    let mut cli = MultitoolCli::from_arg_matches(&matches)?;
    cli.config_overrides = config_overrides;
    if let Some(Subcommand::Exec(exec)) = &mut cli.subcommand {
        exec.shared.apply_sandbox_selection(&matches);
    }
    // Clap intentionally permits inherited global args on subcommands, so
    // args_conflicts_with_subcommands does not enforce this setup-only boundary.
    if let Some(Subcommand::Sandbox(sandbox)) = &cli.subcommand
        && ((!setup_child_delimited
            && sandbox
                .execution
                .command
                .first()
                .is_some_and(|command| command == "setup"))
            || (matches!(&sandbox.action, Some(SandboxAction::Setup(_)))
                && (cli.root.config_profile_v2.is_some()
                    || cli.root.cwd.is_some()
                    || sandbox.execution.config_profile.is_some()
                    || sandbox.execution.cwd.is_some())))
    {
        return Err(MultitoolCli::command().error(
            clap::error::ErrorKind::ArgumentConflict,
            "sandbox setup does not support execution flags; use `sandbox [OPTIONS] -- setup ...` to run a child named setup",
        ));
    }
    Ok(cli)
}

/// Counts the argv suffix captured by a trailing var-arg positional, such as the
/// child command of `sandbox`, `mcp add`, or `execpolicy check`. Clap treats
/// every token after its first value as a value, never as a Codex flag.
fn trailing_child_value_count(command: &clap::Command, matches: &clap::ArgMatches) -> usize {
    let mut command = command;
    let mut matches = matches;
    loop {
        if let Some(count) = command
            .get_positionals()
            .filter(|arg| arg.is_trailing_var_arg_set())
            .find_map(|arg| matches.get_raw(arg.get_id().as_str()))
            .map(Iterator::count)
        {
            return count;
        }
        let Some((name, sub_matches)) = matches.subcommand() else {
            return 0;
        };
        let Some(sub_command) = command.find_subcommand(name) else {
            return 0;
        };
        command = sub_command;
        matches = sub_matches;
    }
}

fn without_config_flags(
    mut args: Vec<std::ffi::OsString>,
    trailing_child_values: usize,
) -> Vec<std::ffi::OsString> {
    // Clap exposes no mutable subcommand matches. Reparse the already-validated
    // argv without globals after preserving their canonical, ordered values.
    // Child-command values (for example `sandbox bash -c ...`) stay verbatim.
    let child_args = args.split_off(args.len().saturating_sub(trailing_child_values));
    let mut args = args.into_iter();
    let mut filtered: Vec<_> = args.next().into_iter().collect();
    while let Some(arg) = args.next() {
        let text = arg.to_str().unwrap_or_default();
        if text == "--" {
            filtered.push(arg);
            filtered.extend(args);
            break;
        }
        if matches!(text, "-c" | "--config" | "--enable" | "--disable") {
            let _ = args.next();
        } else if !(text.starts_with("-c")
            || text.starts_with("--config=")
            || text.starts_with("--enable=")
            || text.starts_with("--disable="))
        {
            filtered.push(arg);
        }
    }
    filtered.extend(child_args);
    filtered
}

fn main() -> anyhow::Result<()> {
    let remote_control_disabled = codex_app_server::take_remote_control_disabled_env();
    arg0_dispatch_or_else_with_pre_runtime(move || {
        // SAFETY: dotenv loading has finished and no runtime threads exist yet.
        unsafe { codex_process_hardening::pre_main_hardening() };
        let cli = parse_multitool_cli(std::env::args_os());
        move |arg0_paths: Arg0DispatchPaths| async move {
            cli_main(cli, arg0_paths, remote_control_disabled).await
        }
    })
}

async fn cli_main(
    cli: MultitoolCli,
    arg0_paths: Arg0DispatchPaths,
    remote_control_disabled: bool,
) -> anyhow::Result<()> {
    let MultitoolCli {
        config_overrides: mut root_config_overrides,
        feature_toggles,
        root,
        subcommand,
    } = cli;
    validate_root_shared_options(&root.shared)?;

    // Fold --enable/--disable into config overrides so they flow to all subcommands.
    let toggle_overrides = feature_toggles.to_overrides()?;
    root_config_overrides.raw_overrides.extend(toggle_overrides);
    let root_strict_config = root.strict_config;
    reject_root_strict_config_for_subcommand(root_strict_config, &subcommand)?;
    if let Some(subcommand) = subcommand.as_ref() {
        profile_v2_for_subcommand(&root, subcommand)?;
    }

    match subcommand {
        None => anyhow::bail!("a command is required; run `codex --help` to list commands"),
        Some(Subcommand::Exec(mut exec_cli)) => {
            inherit_exec_root_options(&mut exec_cli, &root)?;
            exec_cli.strict_config |= root_strict_config;
            prepend_config_flags(
                &mut exec_cli.config_overrides,
                root_config_overrides.clone(),
            );
            codex_exec::run_main(exec_cli, arg0_paths.clone()).await?;
        }
        Some(Subcommand::Review(ReviewCommand {
            strict_config,
            args: review_args,
        })) => {
            let mut exec_cli = ExecCli::try_parse_from(["codex", "exec"])?;
            exec_cli.shared.inherit_exec_root_options(&root.shared);
            exec_cli.command = Some(ExecCommand::Review(review_args));
            exec_cli.strict_config = strict_config || root_strict_config;
            prepend_config_flags(
                &mut exec_cli.config_overrides,
                root_config_overrides.clone(),
            );
            codex_exec::run_main(exec_cli, arg0_paths.clone()).await?;
        }
        Some(Subcommand::Mcp(mut mcp_cli)) => {
            // Propagate any root-level config overrides (e.g. `-c key=value`).
            prepend_config_flags(&mut mcp_cli.config_overrides, root_config_overrides.clone());
            let loader_overrides = loader_overrides_for_profile(root.config_profile_v2.as_ref())?;
            mcp_cli.run(loader_overrides).await?;
        }
        Some(Subcommand::Plugin(plugin_cli)) => {
            let PluginCli {
                mut config_overrides,
                subcommand,
            } = plugin_cli;
            prepend_config_flags(&mut config_overrides, root_config_overrides.clone());
            match subcommand {
                PluginSubcommand::Add(args) => {
                    let overrides = config_overrides
                        .parse_overrides()
                        .map_err(anyhow::Error::msg)?;
                    plugin_cmd::run_plugin_add(overrides, args).await?;
                }
                PluginSubcommand::List(args) => {
                    let overrides = config_overrides
                        .parse_overrides()
                        .map_err(anyhow::Error::msg)?;
                    plugin_cmd::run_plugin_list(overrides, args).await?;
                }
                PluginSubcommand::Marketplace(mut marketplace_cli) => {
                    prepend_config_flags(&mut marketplace_cli.config_overrides, config_overrides);
                    marketplace_cli.run().await?;
                }
                PluginSubcommand::Remove(args) => {
                    let overrides = config_overrides
                        .parse_overrides()
                        .map_err(anyhow::Error::msg)?;
                    plugin_cmd::run_plugin_remove(overrides, args).await?;
                }
            }
        }
        Some(Subcommand::AppServer(app_server_cli)) => {
            reject_runtime_options_for_app_server_subcommand(&app_server_cli)?;
            let AppServerCommand {
                subcommand,
                strict_config: app_server_strict_config,
                listen,
                stdio,
                remote_control,
                analytics_default_enabled,
                auth,
            } = app_server_cli;
            let strict_config = app_server_strict_config || root_strict_config;
            reject_strict_config_for_app_server_subcommand(strict_config, subcommand.as_ref())?;
            match subcommand {
                None => {
                    let transport = app_server_transport(listen, stdio);
                    let auth = auth.try_into_settings()?;
                    let runtime_options = codex_app_server::AppServerRuntimeOptions {
                        remote_control_startup_mode: match (remote_control, remote_control_disabled)
                        {
                            (true, _) => {
                                codex_app_server::RemoteControlStartupMode::EnabledEphemeral
                            }
                            (false, true) => {
                                codex_app_server::RemoteControlStartupMode::DisabledEphemeral
                            }
                            (false, false) => {
                                codex_app_server::RemoteControlStartupMode::ResolvePersisted
                            }
                        },
                        ..Default::default()
                    };
                    codex_app_server::run_main(
                        arg0_paths.clone(),
                        root_config_overrides,
                        LoaderOverrides::default(),
                        strict_config,
                        analytics_default_enabled,
                        transport,
                        codex_protocol::protocol::SessionSource::VSCode,
                        auth,
                        runtime_options,
                    )
                    .await?;
                }
                Some(AppServerSubcommand::Proxy(proxy_cli)) => {
                    let socket_path = match proxy_cli.socket_path {
                        Some(socket_path) => socket_path,
                        None => {
                            let codex_home = find_codex_home()?;
                            codex_app_server::app_server_control_socket_path(&codex_home)?
                        }
                    };
                    codex_stdio_to_uds::run(socket_path.as_path()).await?;
                }
                Some(AppServerSubcommand::GenerateTs(gen_cli)) => {
                    let options = codex_app_server_protocol::GenerateTsOptions {
                        experimental_api: gen_cli.experimental,
                        ..Default::default()
                    };
                    codex_app_server_protocol::generate_ts_with_options(
                        &gen_cli.out_dir,
                        gen_cli.prettier.as_deref(),
                        options,
                    )?;
                }
                Some(AppServerSubcommand::GenerateJsonSchema(gen_cli)) => {
                    codex_app_server_protocol::generate_json_with_experimental(
                        &gen_cli.out_dir,
                        gen_cli.experimental,
                    )?;
                }
                Some(AppServerSubcommand::GenerateInternalJsonSchema(gen_cli)) => {
                    codex_app_server_protocol::generate_internal_json_schema(&gen_cli.out_dir)?;
                }
            }
        }
        Some(Subcommand::App(app_cli)) => {
            app_cmd::run_app(app_cli).await?;
        }
        Some(Subcommand::Login(mut login_cli)) => {
            prepend_config_flags(
                &mut login_cli.config_overrides,
                root_config_overrides.clone(),
            );
            match login_cli.action {
                Some(LoginSubcommand::Status) => {
                    run_login_status(login_cli.config_overrides).await;
                }
                None => {
                    if login_cli.with_api_key && login_cli.with_access_token {
                        eprintln!(
                            "Choose one login credential source: --with-api-key or --with-access-token."
                        );
                        std::process::exit(1);
                    } else if login_cli.use_device_code {
                        run_login_with_device_code(
                            login_cli.config_overrides,
                            login_cli.issuer_base_url,
                            login_cli.client_id,
                        )
                        .await;
                    } else if login_cli.with_api_key {
                        let api_key = read_api_key_from_stdin();
                        run_login_with_api_key(login_cli.config_overrides, api_key).await;
                    } else if login_cli.with_access_token {
                        let access_token = read_access_token_from_stdin();
                        run_login_with_access_token(login_cli.config_overrides, access_token).await;
                    } else {
                        run_login_with_chatgpt(login_cli.config_overrides).await;
                    }
                }
            }
        }
        Some(Subcommand::Logout(mut logout_cli)) => {
            prepend_config_flags(
                &mut logout_cli.config_overrides,
                root_config_overrides.clone(),
            );
            run_logout(logout_cli.config_overrides).await;
        }
        Some(Subcommand::Completion(completion_cli)) => {
            print_completion(completion_cli);
        }
        Some(Subcommand::Doctor(doctor_cli)) => {
            doctor::run_doctor(
                doctor_cli,
                root_config_overrides.clone(),
                &root,
                &arg0_paths,
            )
            .await?;
        }
        Some(Subcommand::Sandbox(sandbox_cli)) => {
            if let Some(SandboxAction::Setup(setup_cli)) = sandbox_cli.action {
                anyhow::ensure!(
                    root.config_profile_v2.is_none() && root.cwd.is_none(),
                    "sandbox setup does not support --profile or --cd"
                );
                sandbox_setup::run(setup_cli).await?;
                return Ok(());
            }
            let mut sandbox_cli = sandbox_cli.execution;
            let config_profile = sandbox_cli
                .config_profile
                .as_ref()
                .or(root.config_profile_v2.as_ref());
            let loader_overrides = loader_overrides_for_profile(config_profile)?;
            prepend_config_flags(
                &mut sandbox_cli.config_overrides,
                root_config_overrides.clone(),
            );
            codex_cli::run_command_under_windows_sandbox(sandbox_cli, loader_overrides).await?;
        }
        Some(Subcommand::Debug(DebugCommand { subcommand })) => match subcommand {
            DebugSubcommand::Models(cmd) => {
                run_debug_models_command(cmd, root_config_overrides).await?;
            }
            DebugSubcommand::PromptInput(cmd) => {
                run_debug_prompt_input_command(
                    cmd,
                    root_config_overrides,
                    root,
                    arg0_paths.clone(),
                )
                .await?;
            }
        },
        Some(Subcommand::Execpolicy(ExecpolicyCommand { sub })) => match sub {
            ExecpolicySubcommand::Check(cmd) => cmd.run()?,
        },
        Some(Subcommand::Apply(mut apply_cli)) => {
            prepend_config_flags(
                &mut apply_cli.config_overrides,
                root_config_overrides.clone(),
            );
            run_apply_command(apply_cli, root.shared.cwd.clone()).await?;
        }
        Some(Subcommand::StdioToUds(cmd)) => {
            let socket_path = cmd.socket_path;
            codex_stdio_to_uds::run(socket_path.as_path()).await?;
        }
        Some(Subcommand::ExecServer(cmd)) => {
            let strict_config = cmd.strict_config || root_strict_config;
            run_exec_server_command(cmd, &arg0_paths, &root_config_overrides, strict_config)
                .await?;
        }
        Some(Subcommand::Features(FeaturesCli { sub })) => match sub {
            FeaturesSubcommand::List => {
                let mut cli_kv_overrides = root_config_overrides
                    .parse_overrides()
                    .map_err(anyhow::Error::msg)?;

                // Honor `--search` via the canonical web_search mode.
                if root.web_search {
                    cli_kv_overrides.push((
                        "web_search".to_string(),
                        toml::Value::String("live".to_string()),
                    ));
                }

                let config = ConfigBuilder::default()
                    .cli_overrides(cli_kv_overrides)
                    .build()
                    .await?;
                let mut rows = Vec::new();
                let mut name_width = 0;
                let mut stage_width = 0;
                for def in user_settable_features() {
                    let name = def.key;
                    let stage = stage_str(def.stage);
                    let enabled = config.features.enabled(def.id);
                    name_width = name_width.max(name.len());
                    stage_width = stage_width.max(stage.len());
                    rows.push((name, stage, enabled));
                }
                rows.sort_unstable_by_key(|(name, _, _)| *name);

                for (name, stage, enabled) in rows {
                    println!("{name:<name_width$}  {stage:<stage_width$}  {enabled}");
                }
            }
            FeaturesSubcommand::Enable(FeatureSetArgs { feature }) => {
                enable_feature_in_config(&feature, &root_config_overrides).await?;
            }
            FeaturesSubcommand::Disable(FeatureSetArgs { feature }) => {
                disable_feature_in_config(&feature).await?;
            }
        },
    }

    Ok(())
}

fn profile_v2_for_subcommand<'a>(
    root: &'a RootOptions,
    subcommand: &Subcommand,
) -> anyhow::Result<Option<&'a ProfileV2Name>> {
    let Some(profile_v2) = root.config_profile_v2.as_ref() else {
        return Ok(None);
    };

    if subcommand.routing_policy().supports_profile {
        Ok(Some(profile_v2))
    } else {
        anyhow::bail!(
            "--profile only applies to runtime commands and `codex mcp`: `codex exec`, `codex review`, `codex mcp`, `codex sandbox`, and `codex debug prompt-input`."
        )
    }
}

async fn run_exec_server_command(
    cmd: ExecServerCommand,
    arg0_paths: &Arg0DispatchPaths,
    root_config_overrides: &CliConfigOverrides,
    strict_config: bool,
) -> anyhow::Result<()> {
    let codex_self_exe = arg0_paths
        .codex_self_exe
        .clone()
        .ok_or_else(|| anyhow::anyhow!("Codex executable path is not configured"))?;
    let runtime_paths = codex_exec_server::ExecServerRuntimePaths::new(codex_self_exe)?;
    if let Some(base_url) = cmd.remote {
        let environment_id = cmd
            .environment_id
            .ok_or_else(|| anyhow::anyhow!("--environment-id is required when --remote is set"))?;
        let config = load_exec_server_config(root_config_overrides, strict_config).await?;
        let (_otel, telemetry) = exec_server_telemetry::init(Some(&config));
        let auth_provider =
            load_exec_server_remote_auth_provider(&config, &base_url, cmd.use_agent_identity_auth)
                .await?;
        let mut remote_config = codex_exec_server::RemoteEnvironmentConfig::new(
            base_url,
            environment_id,
            auth_provider,
        )?;
        if let Some(name) = cmd.name {
            remote_config.name = name;
        }
        let remote_config = remote_config.with_telemetry(telemetry)
            .with_http_client_factory(config.http_client_factory());
        exec_server_telemetry::run_until_shutdown(move |shutdown| async move {
            codex_exec_server::run_remote_environment_with_shutdown(
                remote_config,
                runtime_paths,
                shutdown,
            )
            .await
            .map_err(anyhow::Error::from)
        })
        .await?;
        Ok(())
    } else {
        let config_result = load_exec_server_config(root_config_overrides, strict_config).await;
        let config = if strict_config {
            Some(config_result?)
        } else {
            match config_result {
                Ok(config) => Some(config),
                Err(err) => {
                    eprintln!(
                        "Could not load exec-server telemetry configuration; continuing with default logging and no configured telemetry: {err:#}"
                    );
                    None
                }
            }
        };
        let (_otel, telemetry) = exec_server_telemetry::init(config.as_ref());
        let listen_url = cmd
            .listen
            .unwrap_or_else(|| codex_exec_server::DEFAULT_LISTEN_URL.to_string());
        exec_server_telemetry::run_until_shutdown(move |shutdown| async move {
            codex_exec_server::run_main_with_shutdown(
                &listen_url,
                runtime_paths,
                telemetry,
                shutdown,
            )
            .await
        })
        .await
        .map_err(anyhow::Error::from_boxed)
    }
}

async fn load_exec_server_remote_auth_provider(
    config: &codex_core::config::Config,
    base_url: &str,
    use_agent_identity_auth: bool,
) -> anyhow::Result<codex_api::SharedAuthProvider> {
    if use_agent_identity_auth {
        let agent_identity_jwt = read_codex_access_token_from_env().ok_or_else(|| {
            anyhow::anyhow!("CODEX_ACCESS_TOKEN is required when --use-agent-identity-auth is set")
        })?;
        let auth_route_config = config.auth_route_config();
        let auth = CodexAuth::from_agent_identity_jwt(
            &agent_identity_jwt,
            Some(&config.chatgpt_base_url),
            &auth_route_config,
        )
        .await?;
        return Ok(codex_model_provider::auth_provider_from_auth(&auth));
    }

    let auth = load_exec_server_remote_auth(
        config,
        "remote exec-server registration requires ChatGPT authentication or API key authentication; run `codex login` or set CODEX_API_KEY",
    )
    .await?;

    if !is_supported_exec_server_remote_auth(&auth) {
        anyhow::bail!(
            "remote exec-server registration requires ChatGPT authentication or API key authentication; Agent Identity auth requires --use-agent-identity-auth"
        );
    }

    if auth.is_api_key_auth() {
        validate_api_key_remote_host(base_url)?;
    }

    Ok(codex_model_provider::auth_provider_from_auth(&auth))
}

fn is_supported_exec_server_remote_auth(auth: &CodexAuth) -> bool {
    auth.is_chatgpt_auth() || auth.is_api_key_auth()
}

fn validate_api_key_remote_host(base_url: &str) -> anyhow::Result<()> {
    let url = url::Url::parse(base_url)
        .map_err(|err| anyhow::anyhow!("invalid remote exec-server registration URL: {err}"))?;
    let host = url.host().ok_or_else(|| {
        anyhow::anyhow!("remote exec-server registration URL must include a host")
    })?;

    let is_loopback = match &host {
        url::Host::Domain(host) => host.eq_ignore_ascii_case("localhost"),
        url::Host::Ipv4(ip) => ip.is_loopback(),
        url::Host::Ipv6(ip) => ip.is_loopback(),
    };
    let is_openai_host = match &host {
        url::Host::Domain(host) => ["openai.com", "openai.org"].into_iter().any(|domain| {
            host.eq_ignore_ascii_case(domain)
                || host.to_ascii_lowercase().ends_with(&format!(".{domain}"))
        }),
        _ => false,
    };
    let is_allowed = match url.scheme() {
        "https" => is_loopback || is_openai_host,
        "http" => is_loopback,
        _ => false,
    };

    if !is_allowed {
        anyhow::bail!(
            "remote exec-server API-key authentication is restricted to HTTPS openai.com and openai.org hosts and subdomains or loopback hosts"
        );
    }

    Ok(())
}

async fn load_exec_server_config(
    root_config_overrides: &CliConfigOverrides,
    strict_config: bool,
) -> anyhow::Result<codex_core::config::Config> {
    let cli_kv_overrides = root_config_overrides
        .parse_overrides()
        .map_err(anyhow::Error::msg)?;
    Ok(ConfigBuilder::default()
        .cli_overrides(cli_kv_overrides)
        .strict_config(strict_config)
        .build()
        .await?)
}

async fn load_exec_server_remote_auth(
    config: &codex_core::config::Config,
    missing_auth_error: &'static str,
) -> anyhow::Result<codex_login::CodexAuth> {
    let auth_manager =
        AuthManager::shared_from_config(config, /*enable_codex_api_key_env*/ true).await;

    let auth = match auth_manager.auth().await {
        Some(auth) => auth,
        None => {
            auth_manager.reload().await;
            auth_manager
                .auth()
                .await
                .ok_or_else(|| anyhow::anyhow!(missing_auth_error))?
        }
    };

    Ok(auth)
}

async fn enable_feature_in_config(
    feature: &str,
    overrides: &CliConfigOverrides,
) -> anyhow::Result<()> {
    FeatureToggles::validate_feature(feature)?;
    let codex_home = find_codex_home()?;
    ConfigEditsBuilder::new(&codex_home)
        .set_feature_enabled(feature, /*enabled*/ true)
        .apply()
        .await?;
    println!("Enabled feature `{feature}` in config.toml.");
    maybe_print_under_development_feature_warning(&codex_home, feature, overrides).await;
    Ok(())
}

async fn disable_feature_in_config(feature: &str) -> anyhow::Result<()> {
    FeatureToggles::validate_feature(feature)?;
    let codex_home = find_codex_home()?;
    ConfigEditsBuilder::new(&codex_home)
        .set_feature_enabled(feature, /*enabled*/ false)
        .apply()
        .await?;
    println!("Disabled feature `{feature}` in config.toml.");
    Ok(())
}

fn loader_overrides_for_profile(
    profile_v2: Option<&ProfileV2Name>,
) -> anyhow::Result<LoaderOverrides> {
    match profile_v2 {
        Some(profile_v2) => {
            let codex_home = find_codex_home()?;
            Ok(LoaderOverrides {
                user_config_path: Some(resolve_profile_v2_config_path(&codex_home, profile_v2)),
                user_config_profile: Some(profile_v2.clone()),
                ..Default::default()
            })
        }
        None => Ok(LoaderOverrides::default()),
    }
}

async fn maybe_print_under_development_feature_warning(
    codex_home: &std::path::Path,
    feature: &str,
    overrides: &CliConfigOverrides,
) {
    let Some(feature) = feature_for_key(feature) else {
        return;
    };
    if !matches!(feature.stage(), Stage::UnderDevelopment) {
        return;
    }
    let config = match async {
        let overrides = overrides.parse_overrides().map_err(anyhow::Error::msg)?;
        ConfigBuilder::default()
            .cli_overrides(overrides)
            .build()
            .await
            .map_err(anyhow::Error::from)
    }
    .await
    {
        Ok(config) => config,
        Err(error) => {
            eprintln!(
                "Feature was enabled, but its warning configuration could not be loaded: {error:#}"
            );
            return;
        }
    };
    let config_path = codex_home.join(codex_config::CONFIG_TOML_FILE);
    let effective = config.config_layer_stack.effective_config();
    if let Some(event) = codex_features::unstable_features_warning_event(
        effective.get("features").and_then(toml::Value::as_table),
        config.suppress_unstable_features_warning,
        &config.features,
        &config_path.display().to_string(),
    ) && let codex_protocol::protocol::EventMsg::Warning(warning) = event.msg
    {
        eprintln!("{}", warning.message);
    }
}

async fn run_debug_prompt_input_command(
    cmd: DebugPromptInputCommand,
    root_config_overrides: CliConfigOverrides,
    root: RootOptions,
    arg0_paths: Arg0DispatchPaths,
) -> anyhow::Result<()> {
    let loader_overrides = loader_overrides_for_profile(root.shared.config_profile_v2.as_ref())?;
    let shared = root.shared;
    let mut cli_kv_overrides = root_config_overrides
        .parse_overrides()
        .map_err(anyhow::Error::msg)?;
    if root.web_search {
        cli_kv_overrides.push((
            "web_search".to_string(),
            toml::Value::String("live".to_string()),
        ));
    }

    let resolved_provider = if shared.oss {
        let codex_home = find_codex_home()?;
        let config_cwd = shared
            .cwd
            .as_deref()
            .map(AbsolutePathBuf::relative_to_current_dir)
            .transpose()?;
        let bootstrap_config = load_config_toml_with_layer_stack(
            &codex_home,
            config_cwd.as_ref(),
            cli_kv_overrides.clone(),
            codex_config::ConfigLoadOptions {
                loader_overrides: loader_overrides.clone(),
                strict_config: root.strict_config,
                cloud_config_bundle: codex_config::CloudConfigBundleLoader::default(),
            },
        )
        .await?;
        resolve_oss_provider(
            shared.oss_provider.as_deref(),
            &bootstrap_config.config_toml,
        )
    } else {
        None
    };
    let (model, model_provider) =
        debug_prompt_oss_overrides(shared.oss, shared.model, resolved_provider)?;

    let approval_policy = if shared.dangerously_bypass_approvals_and_sandbox {
        Some(AskForApproval::Never)
    } else {
        root.approval_policy.map(Into::into)
    };
    let sandbox_mode = if shared.dangerously_bypass_approvals_and_sandbox {
        Some(codex_protocol::config_types::SandboxMode::DangerFullAccess)
    } else {
        shared.sandbox_mode.map(Into::into)
    };
    let overrides = ConfigOverrides {
        model,
        approval_policy,
        sandbox_mode,
        cwd: shared.cwd,
        model_provider,
        codex_self_exe: arg0_paths.codex_self_exe,
        show_raw_agent_reasoning: shared.oss.then_some(true),
        ephemeral: Some(true),
        bypass_hook_trust: shared.bypass_hook_trust.then_some(true),
        additional_writable_roots: shared.add_dir,
        ..Default::default()
    };
    let config = ConfigBuilder::default()
        .cli_overrides(cli_kv_overrides)
        .harness_overrides(overrides)
        .loader_overrides(loader_overrides)
        .build()
        .await?;

    let mut input = shared
        .images
        .into_iter()
        .chain(cmd.images)
        .map(|path| UserInput::LocalImage { path, detail: None })
        .collect::<Vec<_>>();
    if let Some(prompt) = cmd.prompt {
        input.push(UserInput::Text {
            text: codex_utils_string::normalize_newlines(prompt).into_owned(),
            text_elements: Vec::new(),
        });
    }

    let user_instructions_provider = Arc::new(CodexHomeUserInstructionsProvider::new(
        config.codex_home.clone(),
    ));
    let prompt_input = codex_core::build_prompt_input(
        config,
        input,
        /*state_db*/ None,
        user_instructions_provider,
    )
    .await?;
    println!("{}", serde_json::to_string_pretty(&prompt_input)?);

    Ok(())
}

fn debug_prompt_oss_overrides(
    oss: bool,
    model: Option<String>,
    resolved_provider: Option<String>,
) -> anyhow::Result<(Option<String>, Option<String>)> {
    if !oss {
        return Ok((model, None));
    }

    let model_provider = resolved_provider.ok_or_else(|| {
        anyhow::anyhow!(
            "No default OSS provider configured. Use --local-provider=provider or set oss_provider in config.toml"
        )
    })?;
    let model = model.or_else(|| {
        get_default_model_for_oss_provider(&model_provider).map(std::borrow::ToOwned::to_owned)
    });
    Ok((model, Some(model_provider)))
}

async fn run_debug_models_command(
    cmd: DebugModelsCommand,
    root_config_overrides: CliConfigOverrides,
) -> anyhow::Result<()> {
    let catalog = if cmd.bundled {
        bundled_models_response()?
    } else {
        let cli_overrides = root_config_overrides
            .parse_overrides()
            .map_err(anyhow::Error::msg)?;
        let config = ConfigBuilder::default()
            .cli_overrides(cli_overrides)
            .build()
            .await?;
        let auth_manager =
            AuthManager::shared_from_config(&config, /*enable_codex_api_key_env*/ true).await;
        let models_manager = build_models_manager(&config, auth_manager);
        models_manager
            .raw_model_catalog(
                RefreshStrategy::OnlineIfUncached,
                config.http_client_factory(),
            )
            .await?
    };

    serde_json::to_writer(std::io::stdout(), &catalog)?;
    println!();
    Ok(())
}

fn inherit_exec_root_options(exec: &mut ExecCli, root: &RootOptions) -> anyhow::Result<()> {
    anyhow::ensure!(
        root.approval_policy.is_none(),
        "--ask-for-approval is not supported with exec; headless execution never prompts"
    );
    exec.shared.inherit_exec_root_options(&root.shared);
    // The explicit --search switch selects live search, taking precedence over
    // config-file and -c web_search settings.
    if root.web_search {
        exec.config_overrides
            .raw_overrides
            .push("web_search=\"live\"".to_string());
    }
    Ok(())
}

/// Prepend root-level overrides so they have lower precedence than
/// CLI-specific ones specified after the subcommand (if any).
fn prepend_config_flags(
    subcommand_config_overrides: &mut CliConfigOverrides,
    cli_config_overrides: CliConfigOverrides,
) {
    subcommand_config_overrides.prepend_root_overrides(cli_config_overrides);
}

fn reject_root_strict_config_for_subcommand(
    strict_config: bool,
    subcommand: &Option<Subcommand>,
) -> anyhow::Result<()> {
    if !strict_config {
        return Ok(());
    }

    match unsupported_subcommand_name_for_strict_config(subcommand) {
        Some(subcommand_name) => {
            reject_strict_config_for_unsupported_subcommand(strict_config, subcommand_name)
        }
        None => Ok(()),
    }
}

/// Return the selected subcommand name when a root-level `--strict-config`
/// flag should be rejected after parsing.
///
/// `--strict-config` is parsed on the root CLI so wrappers can forward it into
/// the selected command. Clap accepts that root flag before the dispatcher
/// knows which subcommand the user selected, so unsupported subcommands need an
/// explicit post-parse reject path.
///
/// `Some(...)` returns the user-facing command name fragment to embed in the
/// rejection error, such as `mcp` or `app-server proxy`. `None` means the
/// selected command is allowed to inherit root `--strict-config`.
fn unsupported_subcommand_name_for_strict_config(
    subcommand: &Option<Subcommand>,
) -> Option<&'static str> {
    let policy = subcommand.as_ref()?.routing_policy();
    (!policy.supports_strict_config).then_some(policy.strict_config_name)
}

fn reject_strict_config_for_app_server_subcommand(
    strict_config: bool,
    subcommand: Option<&AppServerSubcommand>,
) -> anyhow::Result<()> {
    if subcommand.is_none() {
        return Ok(());
    }
    reject_strict_config_for_unsupported_subcommand(
        strict_config,
        app_server_subcommand_name(subcommand),
    )
}

fn reject_runtime_options_for_app_server_subcommand(
    command: &AppServerCommand,
) -> anyhow::Result<()> {
    let Some(subcommand) = command.subcommand.as_ref() else {
        return Ok(());
    };
    let option = if command.listen.is_some() {
        Some("--listen")
    } else if command.stdio {
        Some("--stdio")
    } else if command.remote_control {
        Some("--remote-control")
    } else if command.analytics_default_enabled {
        Some("--analytics-default-enabled")
    } else if command.auth != codex_app_server::AppServerWebsocketAuthArgs::default() {
        Some("websocket authentication options")
    } else {
        None
    };
    if let Some(option) = option {
        anyhow::bail!(
            "`{option}` is only supported when running `codex app-server` without a tooling subcommand; it does not apply to `codex {}`",
            app_server_subcommand_name(Some(subcommand))
        );
    }
    Ok(())
}

fn app_server_transport(
    listen: Option<codex_app_server::AppServerTransport>,
    stdio: bool,
) -> codex_app_server::AppServerTransport {
    if stdio {
        codex_app_server::AppServerTransport::Stdio
    } else {
        listen.unwrap_or(codex_app_server::AppServerTransport::Stdio)
    }
}

fn reject_strict_config_for_unsupported_subcommand(
    strict_config: bool,
    subcommand: &str,
) -> anyhow::Result<()> {
    if strict_config {
        anyhow::bail!("`--strict-config` is not supported for `codex {subcommand}`");
    }
    Ok(())
}

fn app_server_subcommand_name(subcommand: Option<&AppServerSubcommand>) -> &'static str {
    match subcommand {
        None => "app-server",
        Some(AppServerSubcommand::Proxy(_)) => "app-server proxy",
        Some(AppServerSubcommand::GenerateTs(_)) => "app-server generate-ts",
        Some(AppServerSubcommand::GenerateJsonSchema(_)) => "app-server generate-json-schema",
        Some(AppServerSubcommand::GenerateInternalJsonSchema(_)) => {
            "app-server generate-internal-json-schema"
        }
    }
}

fn print_completion(cmd: CompletionCommand) {
    let mut app = MultitoolCli::command();
    let name = "codex";
    generate(cmd.shell, &mut app, name, &mut std::io::stdout());
}


