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
use codex_responses_api_proxy::Args as ResponsesApiProxyArgs;
use codex_rollout_trace::REDUCED_STATE_FILE_NAME;
use codex_rollout_trace::replay_bundle;

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

    /// Internal: run the responses API proxy.
    #[clap(hide = true)]
    ResponsesApiProxy(ResponsesApiProxyArgs),

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
                    DebugSubcommand::AppServer(_) => ("debug app-server", false),
                    DebugSubcommand::PromptInput(_) => ("debug prompt-input", true),
                    DebugSubcommand::TraceReduce(_) => ("debug trace-reduce", false),
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
            Self::ResponsesApiProxy(_) => {
                SubcommandRoutingPolicy::new("responses-api-proxy", false, false)
            }
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

    /// Tooling: helps debug the app server.
    AppServer(DebugAppServerCommand),

    /// Render the model-visible prompt input list as JSON.
    PromptInput(DebugPromptInputCommand),

    /// Replay a rollout trace bundle and write reduced state JSON.
    #[clap(hide = true)]
    TraceReduce(DebugTraceReduceCommand),
}

#[derive(Debug, Parser)]
struct DebugAppServerCommand {
    #[command(subcommand)]
    subcommand: DebugAppServerSubcommand,
}

#[derive(Debug, clap::Subcommand)]
enum DebugAppServerSubcommand {
    // Send message to app server V2.
    SendMessageV2(DebugAppServerSendMessageV2Command),
}

#[derive(Debug, Parser)]
struct DebugAppServerSendMessageV2Command {
    #[arg(value_name = "USER_MESSAGE", required = true)]
    user_message: String,
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
struct DebugTraceReduceCommand {
    /// Trace bundle directory containing manifest.json and trace.jsonl.
    #[arg(value_name = "TRACE_BUNDLE")]
    trace_bundle: PathBuf,

    /// Output path for reduced RolloutTrace JSON. Defaults to TRACE_BUNDLE/state.json.
    #[arg(long = "output", short = 'o', value_name = "FILE")]
    output: Option<PathBuf>,
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

    /// Transport endpoint URL. Supported values: `ws://IP:PORT` (default), `stdio`, `stdio://`.
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

async fn run_debug_app_server_command(
    cmd: DebugAppServerCommand,
    config_overrides: CliConfigOverrides,
) -> anyhow::Result<()> {
    config_overrides
        .parse_overrides()
        .map_err(anyhow::Error::msg)?;
    match cmd.subcommand {
        DebugAppServerSubcommand::SendMessageV2(cmd) => {
            let codex_bin = std::env::current_exe()?;
            codex_app_server_test_client::send_message_v2(
                &codex_bin,
                &config_overrides.raw_overrides,
                cmd.user_message,
                &None,
            )
            .await
        }
    }
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
        let cli = parse_multitool_cli(std::env::args_os());
        if matches!(&cli.subcommand, Some(Subcommand::ResponsesApiProxy(_))) {
            // SAFETY: arg0 has finished loading dotenv and has not created any
            // threads. Only the proxy opts into environment sanitation.
            unsafe { codex_process_hardening::pre_main_hardening() };
        }
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
            DebugSubcommand::AppServer(cmd) => {
                run_debug_app_server_command(cmd, root_config_overrides).await?;
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
            DebugSubcommand::TraceReduce(cmd) => {
                run_debug_trace_reduce_command(cmd).await?;
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
        Some(Subcommand::ResponsesApiProxy(args)) => {
            tokio::task::spawn_blocking(move || codex_responses_api_proxy::run_main(args))
                .await??;
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
        let remote_config = remote_config.with_telemetry(telemetry);
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

async fn run_debug_trace_reduce_command(cmd: DebugTraceReduceCommand) -> anyhow::Result<()> {
    let output = cmd
        .output
        .unwrap_or_else(|| cmd.trace_bundle.join(REDUCED_STATE_FILE_NAME));

    let trace = replay_bundle(&cmd.trace_bundle)?;
    let reduced_json = serde_json::to_vec_pretty(&trace)?;
    tokio::fs::write(&output, reduced_json).await?;
    println!("{}", output.display());

    Ok(())
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

#[cfg(test)]
mod routing_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use pretty_assertions::assert_eq;

    #[test]
    fn exec_server_remote_auth_accepts_api_key_auth() {
        let auth = CodexAuth::from_api_key("sk-test");

        assert!(is_supported_exec_server_remote_auth(&auth));

        // Control: a credential that is neither ChatGPT nor an OpenAI API key is not accepted.
        let bedrock = CodexAuth::BedrockApiKey(codex_login::auth::BedrockApiKeyAuth {
            api_key: "bedrock-test".to_string(),
            region: "us-east-1".to_string(),
        });
        assert!(!is_supported_exec_server_remote_auth(&bedrock));
    }

    #[test]
    fn exec_server_remote_api_key_auth_restricts_destinations() {
        for base_url in [
            "https://openai.com/api",
            "https://service.openai.com/api",
            "https://openai.org/api",
            "https://service.openai.org/api",
            "http://localhost:8098/api",
            "http://127.0.0.1:8098/api",
            "http://[::1]:8098/api",
            "https://127.0.0.1:8098/api",
        ] {
            assert!(validate_api_key_remote_host(base_url).is_ok(), "{base_url}");
        }
        for base_url in [
            "http://service.openai.com/api",
            "http://service.openai.org/api",
            "https://service.openai.org.evil.example/api",
            "https://evilopenai.org/api",
            "ftp://localhost/api",
        ] {
            let error = validate_api_key_remote_host(base_url)
                .expect_err("reject an untrusted destination");
            assert_eq!(
                error.to_string(),
                "remote exec-server API-key authentication is restricted to HTTPS openai.com and openai.org hosts and subdomains or loopback hosts",
                "{base_url}"
            );
        }
    }

    fn profile_v2_for_args(args: &[&str]) -> anyhow::Result<Option<String>> {
        let cli = MultitoolCli::try_parse_from(args).expect("parse");
        let Some(subcommand) = cli.subcommand.as_ref() else {
            return Ok(cli
                .root
                .config_profile_v2
                .as_ref()
                .map(std::string::ToString::to_string));
        };
        Ok(profile_v2_for_subcommand(&cli.root, subcommand)?.map(ToString::to_string))
    }

    fn routing_policy_for_args(args: &[&str]) -> SubcommandRoutingPolicy {
        let cli = MultitoolCli::try_parse_from(args).expect("parse");
        cli.subcommand
            .as_ref()
            .expect("subcommand present")
            .routing_policy()
    }

    #[test]
    fn root_option_routing_is_derived_from_one_subcommand_policy() {
        assert_eq!(
            routing_policy_for_args(&["codex", "exec"]),
            SubcommandRoutingPolicy::new("exec", true, true)
        );
        assert_eq!(
            routing_policy_for_args(&["codex", "debug", "prompt-input"]),
            SubcommandRoutingPolicy::new("debug prompt-input", true, false)
                .with_strict_config_name("debug")
        );
        assert_eq!(
            routing_policy_for_args(&["codex", "features", "list"]),
            SubcommandRoutingPolicy::new("features list", false, false)
                .with_strict_config_name("features")
        );
    }

    #[test]
    fn profile_v2_is_rejected_for_config_management_subcommands() {
        assert!(profile_v2_for_args(&["codex", "--profile", "work", "features", "list"]).is_err());
    }

    #[test]
    fn profile_v2_is_allowed_for_runtime_subcommands() {
        assert_eq!(
            profile_v2_for_args(&["codex", "--profile", "work", "exec"])
                .expect("exec supports profile-v2")
                .as_deref(),
            Some("work")
        );
        assert_eq!(
            profile_v2_for_args(&["codex", "--profile", "work", "debug", "prompt-input"])
                .expect("debug prompt-input supports profile-v2")
                .as_deref(),
            Some("work")
        );
        assert_eq!(
            profile_v2_for_args(&["codex", "--profile", "work", "mcp", "list"])
                .expect("mcp supports profile-v2")
                .as_deref(),
            Some("work")
        );
        assert_eq!(
            profile_v2_for_args(&["codex", "--profile", "work", "sandbox"])
                .expect("sandbox supports config profile")
                .as_deref(),
            Some("work")
        );
    }

    #[test]
    fn config_overrides_before_and_after_subcommands_are_all_applied() {
        let parse = |args: &[&str]| parse_multitool_cli(args.iter().map(std::ffi::OsString::from));

        for (args, expected) in [
            (vec!["codex", "exec", "task"], vec![]),
            (vec!["codex", "mcp", "list"], vec![]),
            (vec!["codex", "exec", "--", "-c"], vec![]),
            (vec!["codex", "-c", "a=1", "mcp", "list"], vec!["a=1"]),
            (vec!["codex", "mcp", "list", "--config=a=2"], vec!["a=2"]),
            (
                vec!["codex", "-ca=1", "mcp", "list", "--config=a=2"],
                vec!["a=1", "a=2"],
            ),
        ] {
            assert_eq!(
                parse(&args).config_overrides.raw_overrides,
                expected,
                "{args:?}"
            );
        }

        let cli = parse(&[
            "codex",
            "-c",
            "model=root",
            "exec",
            "-c",
            "effort=high",
            "task",
        ]);
        assert_eq!(
            cli.config_overrides.raw_overrides,
            vec!["model=root", "effort=high"]
        );

        let cli = parse(&[
            "codex", "-c", "a=1", "-c", "b=1", "mcp", "-c", "a=2", "list",
        ]);
        let Some(Subcommand::Mcp(mut mcp_cli)) = cli.subcommand else {
            panic!("expected mcp subcommand");
        };
        prepend_config_flags(&mut mcp_cli.config_overrides, cli.config_overrides);
        let effective: std::collections::HashMap<_, _> = mcp_cli
            .config_overrides
            .parse_overrides()
            .expect("parse overrides")
            .into_iter()
            .collect();
        assert_eq!(
            (effective.get("a"), effective.get("b")),
            (
                Some(&toml::Value::Integer(2)),
                Some(&toml::Value::Integer(1))
            )
        );
    }

    #[test]
    fn root_requires_a_command_instead_of_a_prompt() {
        let err = MultitoolCli::try_parse_from(["codex", "import"])
            .expect_err("a bare prompt is not a command");
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidSubcommand);

        let err = MultitoolCli::try_parse_from(["codex"]).expect_err("help without a command");
        assert_eq!(
            err.kind(),
            clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        );
    }

    #[test]
    fn version_output_remains_single_line() {
        let err = MultitoolCli::try_parse_from(["codex", "--version"])
            .expect_err("--version should render and stop parsing");

        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
        let rendered = err.to_string();
        assert_eq!(rendered.lines().filter(|line| !line.is_empty()).count(), 1);
        assert!(!rendered.contains("commit:"));
    }

    #[test]
    fn profile_v2_rejects_non_plain_names_at_parse_time() {
        assert!(
            MultitoolCli::try_parse_from(["codex", "--profile", "nested/work", "exec"]).is_err()
        );
    }

    #[test]
    fn exec_resume_last_accepts_prompt_positional() {
        let cli =
            MultitoolCli::try_parse_from(["codex", "exec", "--json", "resume", "--last", "2+2"])
                .expect("parse should succeed");

        let Some(Subcommand::Exec(exec)) = cli.subcommand else {
            panic!("expected exec subcommand");
        };
        let Some(codex_exec::Command::Resume(args)) = exec.command else {
            panic!("expected exec resume");
        };

        assert!(args.last);
        assert_eq!(args.session_id, None);
        assert_eq!(args.prompt.as_deref(), Some("2+2"));
    }

    #[test]
    fn exec_resume_accepts_output_flags_after_subcommand() {
        let cli = MultitoolCli::try_parse_from([
            "codex",
            "exec",
            "resume",
            "session-123",
            "-o",
            "/tmp/resume-output.md",
            "--output-schema",
            "/tmp/schema.json",
            "re-review",
        ])
        .expect("parse should succeed");

        let Some(Subcommand::Exec(exec)) = cli.subcommand else {
            panic!("expected exec subcommand");
        };
        let Some(codex_exec::Command::Resume(args)) = exec.command else {
            panic!("expected exec resume");
        };

        assert_eq!(
            exec.last_message_file,
            Some(std::path::PathBuf::from("/tmp/resume-output.md"))
        );
        assert_eq!(
            exec.output_schema,
            Some(std::path::PathBuf::from("/tmp/schema.json"))
        );
        assert_eq!(args.session_id.as_deref(), Some("session-123"));
        assert_eq!(args.prompt.as_deref(), Some("re-review"));
    }

    #[test]
    fn dangerous_bypass_conflicts_with_approval_policy() {
        let err = MultitoolCli::try_parse_from([
            "codex",
            "--dangerously-bypass-approvals-and-sandbox",
            "--ask-for-approval",
            "on-request",
            "exec",
        ])
        .expect_err("conflicting permission flags should be rejected");

        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn command_wiring_login_credential_options_are_mutually_exclusive() {
        for args in [
            vec!["codex", "login", "--with-api-key", "--with-access-token"],
            vec!["codex", "login", "--device-auth", "--with-api-key"],
            vec!["codex", "login", "--device-auth", "--with-access-token"],
        ] {
            let err = MultitoolCli::try_parse_from(args)
                .expect_err("conflicting login credential options should be rejected");
            assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
        }
    }

    #[test]
    fn command_wiring_login_status_rejects_login_credential_options() {
        let err = MultitoolCli::try_parse_from(["codex", "login", "--with-api-key", "status"])
            .expect_err("status should reject login credential options");

        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn command_wiring_custom_login_oauth_options_require_device_auth() {
        for option in ["--experimental_issuer", "--experimental_client-id"] {
            let err = MultitoolCli::try_parse_from(["codex", "login", option, "value"])
                .expect_err("custom OAuth options should require device auth");
            assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
        }
    }

    #[test]
    fn command_wiring_exec_server_remote_metadata_requires_remote_registration() {
        for option in ["--environment-id", "--name"] {
            let err = MultitoolCli::try_parse_from(["codex", "exec-server", option, "value"])
                .expect_err("remote metadata should require --remote");
            assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
        }
    }

    fn app_server_from_args(args: &[&str]) -> AppServerCommand {
        let cli = MultitoolCli::try_parse_from(args).expect("parse");
        let Subcommand::AppServer(app_server) = cli.subcommand.expect("app-server present") else {
            unreachable!()
        };
        app_server
    }

    fn default_app_server_socket_path() -> AbsolutePathBuf {
        let codex_home = find_codex_home().expect("codex home");
        codex_app_server::app_server_control_socket_path(&codex_home)
            .expect("default app-server socket path")
    }

    #[test]
    fn debug_prompt_input_parses_prompt_and_images() {
        let cli = MultitoolCli::try_parse_from([
            "codex",
            "debug",
            "prompt-input",
            "hello",
            "--image",
            "/tmp/a.png,/tmp/b.png",
        ])
        .expect("parse");

        let Some(Subcommand::Debug(DebugCommand {
            subcommand: DebugSubcommand::PromptInput(cmd),
        })) = cli.subcommand
        else {
            panic!("expected debug prompt-input subcommand");
        };

        assert_eq!(cmd.prompt.as_deref(), Some("hello"));
        assert_eq!(
            cmd.images,
            vec![PathBuf::from("/tmp/a.png"), PathBuf::from("/tmp/b.png")]
        );
    }

    #[test]
    fn command_wiring_debug_prompt_input_propagates_oss_provider_and_default_model() {
        let cli = MultitoolCli::try_parse_from([
            "codex",
            "--oss",
            "--local-provider",
            "ollama",
            "debug",
            "prompt-input",
            "hello",
        ])
        .expect("parse");
        let shared = cli.root.shared;
        let (model, model_provider) =
            debug_prompt_oss_overrides(shared.oss, shared.model, shared.oss_provider)
                .expect("resolve OSS overrides");

        assert_eq!(model_provider.as_deref(), Some("ollama"));
        assert_eq!(
            model.as_deref(),
            get_default_model_for_oss_provider("ollama")
        );
    }

    #[test]
    fn command_wiring_local_provider_requires_oss() {
        let cli = MultitoolCli::try_parse_from([
            "codex",
            "--local-provider",
            "ollama",
            "debug",
            "prompt-input",
        ])
        .expect("root requirements are validated after subcommand routing");
        let err = validate_root_shared_options(&cli.root.shared)
            .expect_err("a local provider without OSS mode should be rejected");

        assert_eq!(err.to_string(), "--local-provider requires --oss");
    }

    #[test]
    fn debug_models_parses_bundled_flag() {
        let cli =
            MultitoolCli::try_parse_from(["codex", "debug", "models", "--bundled"]).expect("parse");

        let Some(Subcommand::Debug(DebugCommand {
            subcommand: DebugSubcommand::Models(cmd),
        })) = cli.subcommand
        else {
            panic!("expected debug models subcommand");
        };

        assert!(cmd.bundled);
    }

    #[test]
    fn responses_subcommand_is_not_registered() {
        let command = MultitoolCli::command();
        assert!(
            command
                .get_subcommands()
                .all(|subcommand| subcommand.get_name() != "responses")
        );
    }

    #[test]
    fn review_is_hidden_and_explains_protocol_limit() {
        let help = help_from_args(&["codex", "--help"]);
        assert!(
            !help.contains("Run a code review non-interactively"),
            "{help}"
        );
        assert!(
            !help
                .lines()
                .any(|line| line.trim_start().starts_with("review ")),
            "{help}"
        );
        let review_help = help_from_args(&["codex", "review", "--help"]);
        assert!(
            review_help.contains("review is not supported by the exec app-server protocol"),
            "{review_help}"
        );
    }

    fn help_from_args(args: &[&str]) -> String {
        let err = MultitoolCli::try_parse_from(args).expect_err("help should short-circuit");
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp);
        err.to_string()
    }

    #[test]
    fn plugin_marketplace_help_uses_plugin_namespace() {
        let help = help_from_args(&["codex", "plugin", "marketplace", "--help"]);
        assert!(
            help.contains("Usage: codex plugin marketplace [OPTIONS] <COMMAND>"),
            "{help}"
        );

        for (subcommand, usage) in [
            ("add", "Usage: codex plugin marketplace add"),
            ("list", "Usage: codex plugin marketplace list"),
            ("upgrade", "Usage: codex plugin marketplace upgrade"),
            ("remove", "Usage: codex plugin marketplace remove"),
        ] {
            let help = help_from_args(&["codex", "plugin", "marketplace", subcommand, "--help"]);
            assert!(help.contains(usage), "{help}");
        }
    }

    #[test]
    fn sandbox_parses_permission_profile_aliases() {
        for flag in ["--permission-profile", "--permissions-profile", "-P"] {
            let cli = MultitoolCli::try_parse_from([
                "codex",
                "sandbox",
                flag,
                ":workspace",
                "--",
                "echo",
            ])
            .expect("parse");
            let Some(Subcommand::Sandbox(command)) = cli.subcommand else {
                panic!("expected sandbox command");
            };
            assert_eq!(
                command.execution.permissions_profile.as_deref(),
                Some(":workspace"),
                "{flag}"
            );
            assert_eq!(command.execution.command, vec!["echo"], "{flag}");
        }
    }

    #[test]
    fn sandbox_help_only_shows_singular_permission_profile() {
        let help = help_from_args(&["codex", "sandbox", "--help"]);
        assert!(help.contains("--permission-profile"), "{help}");
        assert!(!help.contains("--permissions-profile"), "{help}");
    }

    #[test]
    fn sandbox_parses_config_profile() {
        let cli =
            MultitoolCli::try_parse_from(["codex", "sandbox", "--profile", "work", "--", "echo"])
                .expect("parse");

        let Some(Subcommand::Sandbox(command)) = cli.subcommand else {
            panic!("expected sandbox command");
        };

        assert_eq!(command.execution.config_profile.as_deref(), Some("work"));
        assert_eq!(command.execution.command, vec!["echo"]);
    }

    #[test]
    fn sandbox_rejects_explicit_profile_controls_without_profile() {
        let err = MultitoolCli::try_parse_from(["codex", "sandbox", "-C", "/tmp"])
            .expect_err("parse should fail");

        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn marketplace_no_longer_parses_at_top_level() {
        let add_result =
            MultitoolCli::try_parse_from(["codex", "marketplace", "add", "owner/repo"]);
        assert!(add_result.is_err());

        let upgrade_result =
            MultitoolCli::try_parse_from(["codex", "marketplace", "upgrade", "debug"]);
        assert!(upgrade_result.is_err());

        let remove_result =
            MultitoolCli::try_parse_from(["codex", "marketplace", "remove", "debug"]);
        assert!(remove_result.is_err());
    }

    #[test]
    fn removed_full_auto_flag_is_rejected_at_each_command_level() {
        for args in [
            vec!["codex", "--full-auto"],
            vec!["codex", "exec", "--full-auto", "summarize"],
            vec!["codex", "sandbox", "--full-auto", "--"],
        ] {
            let error = MultitoolCli::try_parse_from(&args).expect_err("removed flag");
            assert_eq!(
                error.kind(),
                clap::error::ErrorKind::UnknownArgument,
                "{args:?}"
            );
        }
    }

    #[test]
    fn command_wiring_app_server_defaults_to_stdio_without_listen_flag() {
        let app_server = app_server_from_args(["codex", "app-server"].as_ref());
        assert!(!app_server.analytics_default_enabled);
        assert!(!app_server.remote_control);
        assert_eq!(app_server.listen, None);
        assert_eq!(
            app_server_transport(app_server.listen, app_server.stdio),
            codex_app_server::AppServerTransport::Stdio
        );
    }

    #[test]
    fn app_server_remote_control_startup_flag_enables_remote_control() {
        let enabled = app_server_from_args(["codex", "app-server", "--remote-control"].as_ref());
        assert!(enabled.remote_control);
    }

    #[test]
    fn app_server_analytics_default_enabled_with_flag() {
        let app_server =
            app_server_from_args(["codex", "app-server", "--analytics-default-enabled"].as_ref());
        assert!(app_server.analytics_default_enabled);
    }

    #[test]
    fn strict_config_parses_for_supported_commands() {
        let cli =
            MultitoolCli::try_parse_from(["codex", "--strict-config", "exec"]).expect("parse");
        assert!(cli.root.strict_config);

        let cli =
            MultitoolCli::try_parse_from(["codex", "review", "--strict-config", "--uncommitted"])
                .expect("parse");
        assert_matches!(
            cli.subcommand,
            Some(Subcommand::Review(ReviewCommand {
                strict_config: true,
                ..
            }))
        );

        let cli = MultitoolCli::try_parse_from(["codex", "exec-server", "--strict-config"])
            .expect("parse");
        assert_matches!(
            cli.subcommand,
            Some(Subcommand::ExecServer(ExecServerCommand {
                strict_config: true,
                ..
            }))
        );
    }

    #[test]
    fn root_strict_config_is_supported_for_exec_server() {
        let cli = MultitoolCli::try_parse_from(["codex", "--strict-config", "exec-server"])
            .expect("parse");

        // The check below returns Ok without consulting the subcommand when the flag is unset.
        assert!(cli.root.strict_config);
        reject_root_strict_config_for_subcommand(cli.root.strict_config, &cli.subcommand)
            .expect("exec-server should support root --strict-config");
    }

    #[test]
    fn root_strict_config_is_rejected_for_unsupported_subcommands() {
        let cli = MultitoolCli::try_parse_from(["codex", "--strict-config", "mcp", "list"])
            .expect("parse");
        let err = reject_root_strict_config_for_subcommand(cli.root.strict_config, &cli.subcommand)
            .expect_err("mcp should not support root --strict-config");

        assert_eq!(
            err.to_string(),
            "`--strict-config` is not supported for `codex mcp`"
        );
    }

    #[test]
    fn app_server_subcommands_reject_strict_config() {
        let app_server =
            app_server_from_args(["codex", "app-server", "--strict-config", "proxy"].as_ref());
        let err = reject_strict_config_for_app_server_subcommand(
            app_server.strict_config,
            app_server.subcommand.as_ref(),
        )
        .expect_err("app-server proxy should not support --strict-config");

        assert_eq!(
            err.to_string(),
            "`--strict-config` is not supported for `codex app-server proxy`"
        );
    }

    #[test]
    fn command_wiring_app_server_tooling_subcommands_reject_runtime_options() {
        for args in [
            vec!["codex", "app-server", "--listen", "off", "proxy"],
            vec!["codex", "app-server", "--stdio", "proxy"],
            vec!["codex", "app-server", "--remote-control", "proxy"],
            vec![
                "codex",
                "app-server",
                "--analytics-default-enabled",
                "proxy",
            ],
            vec![
                "codex",
                "app-server",
                "--ws-auth",
                "capability-token",
                "proxy",
            ],
        ] {
            let app_server = app_server_from_args(&args);
            let err = reject_runtime_options_for_app_server_subcommand(&app_server)
                .expect_err("tooling subcommands should reject runtime-only options");
            assert!(err.to_string().contains("does not apply"));
        }
    }

    #[test]
    fn app_server_listen_websocket_url_parses() {
        let app_server = app_server_from_args(
            ["codex", "app-server", "--listen", "ws://127.0.0.1:4500"].as_ref(),
        );
        assert_eq!(
            app_server.listen,
            Some(codex_app_server::AppServerTransport::WebSocket {
                bind_address: "127.0.0.1:4500".parse().expect("valid socket address"),
            })
        );
    }

    #[test]
    fn app_server_listen_stdio_url_parses() {
        let app_server =
            app_server_from_args(["codex", "app-server", "--listen", "stdio://"].as_ref());
        assert_eq!(
            app_server.listen,
            Some(codex_app_server::AppServerTransport::Stdio)
        );
    }

    #[test]
    fn app_server_stdio_flag_parses() {
        let app_server = app_server_from_args(["codex", "app-server", "--stdio"].as_ref());
        assert!(app_server.stdio);
    }

    #[test]
    fn app_server_stdio_flag_conflicts_with_listen() {
        let err = MultitoolCli::try_parse_from([
            "codex",
            "app-server",
            "--stdio",
            "--listen",
            "stdio://",
        ])
        .expect_err("--stdio and --listen should be rejected together");
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn app_server_listen_unix_socket_url_parses() {
        let app_server =
            app_server_from_args(["codex", "app-server", "--listen", "unix://"].as_ref());
        assert_eq!(
            app_server.listen,
            Some(codex_app_server::AppServerTransport::UnixSocket {
                socket_path: default_app_server_socket_path()
            })
        );
    }

    #[test]
    fn app_server_listen_unix_socket_path_parses() {
        let app_server = app_server_from_args(
            ["codex", "app-server", "--listen", "unix:///tmp/codex.sock"].as_ref(),
        );
        assert_eq!(
            app_server.listen,
            Some(codex_app_server::AppServerTransport::UnixSocket {
                socket_path: AbsolutePathBuf::from_absolute_path("/tmp/codex.sock")
                    .expect("absolute path should parse")
            })
        );
    }

    #[test]
    fn app_server_listen_off_parses() {
        let app_server = app_server_from_args(["codex", "app-server", "--listen", "off"].as_ref());
        assert_eq!(
            app_server.listen,
            Some(codex_app_server::AppServerTransport::Off)
        );
    }

    #[test]
    fn app_server_listen_invalid_url_fails_to_parse() {
        let parse_result =
            MultitoolCli::try_parse_from(["codex", "app-server", "--listen", "http://foo"]);
        assert!(parse_result.is_err());
    }

    #[test]
    fn app_server_proxy_subcommand_parses() {
        let app_server = app_server_from_args(["codex", "app-server", "proxy"].as_ref());
        assert!(matches!(
            app_server.subcommand,
            Some(AppServerSubcommand::Proxy(AppServerProxyCommand {
                socket_path: None
            }))
        ));
    }

    #[test]
    fn app_server_proxy_sock_path_parses() {
        let app_server =
            app_server_from_args(["codex", "app-server", "proxy", "--sock", "codex.sock"].as_ref());
        let Some(AppServerSubcommand::Proxy(proxy)) = app_server.subcommand else {
            panic!("expected proxy subcommand");
        };
        assert_eq!(
            proxy.socket_path,
            Some(
                AbsolutePathBuf::relative_to_current_dir("codex.sock")
                    .expect("relative path should resolve")
            )
        );
    }

    #[test]
    fn app_server_capability_token_flags_parse() {
        let app_server = app_server_from_args(
            [
                "codex",
                "app-server",
                "--ws-auth",
                "capability-token",
                "--ws-token-file",
                "/tmp/codex-token",
            ]
            .as_ref(),
        );
        assert_eq!(
            app_server.auth.ws_auth,
            Some(codex_app_server::WebsocketAuthCliMode::CapabilityToken)
        );
        assert_eq!(
            app_server.auth.ws_token_file,
            Some(PathBuf::from("/tmp/codex-token"))
        );
    }

    #[test]
    fn app_server_signed_bearer_flags_parse() {
        let app_server = app_server_from_args(
            [
                "codex",
                "app-server",
                "--ws-auth",
                "signed-bearer-token",
                "--ws-shared-secret-file",
                "/tmp/codex-secret",
                "--ws-issuer",
                "issuer",
                "--ws-audience",
                "audience",
                "--ws-max-clock-skew-seconds",
                "9",
            ]
            .as_ref(),
        );
        assert_eq!(
            app_server.auth.ws_auth,
            Some(codex_app_server::WebsocketAuthCliMode::SignedBearerToken)
        );
        assert_eq!(
            app_server.auth.ws_shared_secret_file,
            Some(PathBuf::from("/tmp/codex-secret"))
        );
        assert_eq!(app_server.auth.ws_issuer.as_deref(), Some("issuer"));
        assert_eq!(app_server.auth.ws_audience.as_deref(), Some("audience"));
        assert_eq!(app_server.auth.ws_max_clock_skew_seconds, Some(9));
    }

    #[test]
    fn app_server_rejects_removed_insecure_non_loopback_flag() {
        let parse_result = MultitoolCli::try_parse_from([
            "codex",
            "app-server",
            "--allow-unauthenticated-non-loopback-ws",
        ]);
        assert!(parse_result.is_err());
    }

    #[test]
    fn features_enable_parses_feature_name() {
        let cli = MultitoolCli::try_parse_from(["codex", "features", "enable", "unified_exec"])
            .expect("parse should succeed");
        let Some(Subcommand::Features(FeaturesCli { sub })) = cli.subcommand else {
            panic!("expected features subcommand");
        };
        let FeaturesSubcommand::Enable(FeatureSetArgs { feature }) = sub else {
            panic!("expected features enable");
        };
        assert_eq!(feature, "unified_exec");
    }

    #[test]
    fn features_disable_parses_feature_name() {
        let cli = MultitoolCli::try_parse_from(["codex", "features", "disable", "shell_tool"])
            .expect("parse should succeed");
        let Some(Subcommand::Features(FeaturesCli { sub })) = cli.subcommand else {
            panic!("expected features subcommand");
        };
        let FeaturesSubcommand::Disable(FeatureSetArgs { feature }) = sub else {
            panic!("expected features disable");
        };
        assert_eq!(feature, "shell_tool");
    }

    #[test]
    fn feature_toggles_known_features_generate_overrides() {
        let toggles = FeatureToggles {
            enable: vec!["plugins".to_string()],
            disable: vec!["unified_exec".to_string()],
        };
        let overrides = toggles.to_overrides().expect("valid features");
        assert_eq!(
            overrides,
            vec![
                "features.plugins=true".to_string(),
                "features.unified_exec=false".to_string(),
            ]
        );
    }

    #[test]
    fn feature_toggles_reject_unknown_removed_and_legacy_keys() {
        for key in [
            "use_linux_sandbox_bwrap",
            "image_detail_original",
            "experimental_use_unified_exec_tool",
            "does_not_exist",
        ] {
            for flag in ["--enable", "--disable"] {
                let cli = MultitoolCli::try_parse_from(["codex", flag, key, "features", "list"])
                    .expect("parse toggle");
                assert_eq!(
                    cli.feature_toggles
                        .to_overrides()
                        .expect_err("invalid feature")
                        .to_string(),
                    format!("Unknown feature flag: {key}"),
                    "{flag}"
                );
            }
        }
    }

    #[test]
    fn removed_memory_feature_cannot_be_enabled_or_cleared() {
        assert!(MultitoolCli::try_parse_from(["codex", "debug", "clear-memories"]).is_err());
        for key in ["memories", "memory_tool"] {
            let cli = MultitoolCli::try_parse_from(["codex", "--enable", key, "features", "list"])
                .expect("parse feature override");
            assert_eq!(
                cli.feature_toggles
                    .to_overrides()
                    .expect_err("removed feature")
                    .to_string(),
                format!("Unknown feature flag: {key}")
            );
        }
    }

    #[test]
    fn strict_config_rejects_unknown_and_compound_feature_toggles() {
        for flag in ["--enable", "--disable"] {
            for key in ["does_not_exist", "multi_agent_v2.subagent_usage_hint_text"] {
                let err = strict_config_feature_toggle_error(&[flag, key]);
                assert_eq!(err.to_string(), format!("Unknown feature flag: {key}"));
            }
        }
    }

    fn strict_config_feature_toggle_error(args: &[&str]) -> anyhow::Error {
        let cli_args = std::iter::once("codex")
            .chain(std::iter::once("--strict-config"))
            .chain(args.iter().copied())
            .chain(std::iter::once("exec"));
        let cli = MultitoolCli::try_parse_from(cli_args).expect("parse should succeed");
        assert!(cli.root.strict_config);
        cli.feature_toggles
            .to_overrides()
            .expect_err("feature should be rejected")
    }
}
