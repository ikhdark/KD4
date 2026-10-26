use std::collections::VecDeque;
use std::fs;
use std::fs::OpenOptions;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::ChildStdin;
use std::process::ChildStdout;
use std::process::Command;
use std::process::Stdio;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use clap::ArgAction;
use clap::Parser;
use clap::Subcommand;
use codex_app_server_protocol::AccountLoginCompletedNotification;
use codex_app_server_protocol::ClientInfo;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::CommandExecutionApprovalDecision;
use codex_app_server_protocol::CommandExecutionRequestApprovalParams;
use codex_app_server_protocol::CommandExecutionRequestApprovalResponse;
use codex_app_server_protocol::DynamicToolSpec;
use codex_app_server_protocol::FileChangeApprovalDecision;
use codex_app_server_protocol::FileChangeRequestApprovalParams;
use codex_app_server_protocol::FileChangeRequestApprovalResponse;
use codex_app_server_protocol::GetAccountRateLimitsResponse;
use codex_app_server_protocol::InitializeCapabilities;
use codex_app_server_protocol::InitializeParams;
use codex_app_server_protocol::InitializeResponse;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::JSONRPCNotification;
use codex_app_server_protocol::JSONRPCRequest;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::LoginAccountResponse;
use codex_app_server_protocol::ModelListParams;
use codex_app_server_protocol::ModelListResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ServerRequest;
use codex_app_server_protocol::ThreadDecrementElicitationParams;
use codex_app_server_protocol::ThreadDecrementElicitationResponse;
use codex_app_server_protocol::ThreadIncrementElicitationParams;
use codex_app_server_protocol::ThreadIncrementElicitationResponse;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadListParams;
use codex_app_server_protocol::ThreadListResponse;
use codex_app_server_protocol::ThreadResumeParams;
use codex_app_server_protocol::ThreadResumeResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::UserInput as V2UserInput;
use codex_core::config::Config;
use codex_otel::OtelProvider;
use codex_otel::current_span_w3c_trace_context;
use codex_protocol::dynamic_tools::normalize_dynamic_tool_specs;
use codex_protocol::protocol::W3cTraceContext;
use codex_utils_cli::CliConfigOverrides;
use futures::SinkExt;
use futures::StreamExt;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio_tungstenite::tungstenite::Message;
use tracing::info_span;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use url::Url;
use uuid::Uuid;

mod process;
mod request_user_input;

use process::add_codex_parent_to_path;
use process::build_serve_command;
use process::listener_pids_on_port;
use process::listeners_after_grace;
use process::preflight_serve_restart;
use process::runtime_dir;
use process::start_prepared_serve;
use process::terminate_process;

const NOTIFICATIONS_TO_OPT_OUT: &[&str] = &[
    // v2 item deltas.
    "command/exec/outputDelta",
    "item/agentMessage/delta",
    "item/plan/delta",
    "item/reasoning/summaryTextDelta",
    "item/reasoning/textDelta",
];
const APP_SERVER_GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const APP_SERVER_RESTART_TIMEOUT: Duration = Duration::from_secs(30);
const APP_SERVER_GRACEFUL_SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_millis(100);
const RPC_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_ANALYTICS_ENABLED: bool = true;
const OTEL_SERVICE_NAME: &str = "codex-app-server-test-client";
const TRACE_DISABLED_MESSAGE: &str =
    "Not enabled - enable tracing in $CODEX_HOME/config.toml to get a trace URL!";

/// Minimal launcher that initializes the Codex app-server and logs the handshake.
#[derive(Parser)]
#[command(
    author = "Codex",
    version,
    about = "Bootstrap Codex app-server",
    long_about = "Bootstrap Codex app-server. Use --command-timeout-seconds 600 for bounded, noninteractive finite commands (including turn completion); terminal user-input requests are rejected in this mode. On timeout, a remote turn may still be running: inspect its state rather than automatically resubmitting. Watch and login retain intentional unbounded waits. Set CODEX_APP_SERVER_TEST_CLIENT_WIRE_DUMP to enable full wire diagnostics (user input answers are redacted)."
)]
struct Cli {
    /// Path to the `codex` CLI binary. When set, requests use stdio by
    /// spawning `codex app-server` as a child process.
    #[arg(long, env = "CODEX_BIN", global = true)]
    codex_bin: Option<PathBuf>,

    /// Existing websocket server URL to connect to.
    ///
    /// If neither `--codex-bin` nor `--url` is provided, defaults to
    /// `ws://127.0.0.1:4222`.
    #[arg(long, env = "CODEX_APP_SERVER_URL", global = true)]
    url: Option<String>,

    /// Forwarded to the `codex` CLI as `--config key=value`. Repeatable.
    ///
    /// Example:
    ///   `--config 'model_providers.mock.base_url="http://localhost:4010/v2"'`
    #[arg(
        short = 'c',
        long = "config",
        value_name = "key=value",
        action = ArgAction::Append,
        global = true
    )]
    config_overrides: Vec<String>,

    /// JSON array of dynamic tool specs or a single tool object.
    /// Registration only: this client cannot execute dynamic tool calls.
    /// Prefix a filename with '@' to read from a file.
    ///
    /// Example:
    ///   --dynamic-tools '[{"type":"function","name":"demo","description":"Demo","inputSchema":{"type":"object"}}]'
    ///   --dynamic-tools @/path/to/tools.json
    #[arg(long, value_name = "json-or-@file", global = true)]
    dynamic_tools: Option<String>,

    /// Bound a finite command, including turn completion. This noninteractive
    /// mode rejects terminal request_user_input; watch and login remain unbounded.
    #[arg(long, value_name = "seconds", value_parser = clap::value_parser!(u64).range(1..), global = true)]
    command_timeout_seconds: Option<u64>,

    #[command(subcommand)]
    command: CliCommand,
}

#[derive(Subcommand)]
enum CliCommand {
    /// Start `codex app-server` on a websocket endpoint in the background.
    ///
    /// Logs are written under the platform's temporary directory in
    /// `codex-app-server-test-client/`.
    Serve {
        /// WebSocket listen URL passed to `codex app-server --listen`.
        #[arg(long, default_value = "ws://127.0.0.1:4222")]
        listen: String,
        /// Kill any process listening on the same port before starting.
        #[arg(long, default_value_t = false)]
        kill: bool,
    },
    /// Send a user message through the Codex app-server.
    SendMessage {
        /// User message to send to Codex.
        user_message: String,
    },
    /// Send a user message through the app-server V2 thread/turn APIs.
    SendMessageV2 {
        /// Opt into experimental app-server methods and fields.
        #[arg(long)]
        experimental_api: bool,
        /// User message to send to Codex.
        user_message: String,
    },
    /// Resume a V2 thread by id, then send a user message.
    ResumeMessageV2 {
        /// Existing thread id to resume.
        thread_id: String,
        /// User message to send to Codex.
        user_message: String,
    },
    /// Resume a V2 thread and continuously stream notifications/events.
    ///
    /// This command does not auto-exit; stop it with Ctrl+C or terminate the process.
    ThreadResume {
        /// Existing thread id to resume.
        thread_id: String,
    },
    /// Initialize the app-server and dump all inbound messages until interrupted.
    ///
    /// This command does not auto-exit; stop it with Ctrl+C or terminate the process.
    Watch,
    /// Trigger the ChatGPT login flow and wait for completion.
    TestLogin {
        /// Use the device-code login flow instead of the browser callback flow.
        #[arg(long, default_value_t = false)]
        device_code: bool,
    },
    /// Fetch the current account rate limits from the Codex app-server.
    GetAccountRateLimits,
    /// List the available models from the Codex app-server.
    #[command(name = "model-list")]
    ModelList,
    /// List stored threads from the Codex app-server.
    #[command(name = "thread-list")]
    ThreadList {
        /// Number of threads to return.
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    /// Acquire an out-of-band elicitation lease for a thread until this command disconnects.
    #[command(name = "thread-increment-elicitation")]
    ThreadIncrementElicitation {
        /// Existing thread id to update.
        thread_id: String,
    },
    /// Attempt an idempotent lease release from a new connection.
    #[command(name = "thread-decrement-elicitation")]
    ThreadDecrementElicitation {
        /// Existing thread id to update.
        thread_id: String,
        /// Lease token to probe; a different connection cannot release it.
        lease_id: String,
    },
    /// Hold one out-of-band elicitation lease on a single connection.
    #[command(name = "thread-hold-elicitation")]
    ThreadHoldElicitation {
        /// Existing thread id to pause.
        thread_id: String,
        /// Number of seconds to retain the lease before releasing it.
        #[arg(long)]
        hold_seconds: u64,
    },
}

pub async fn run() -> Result<()> {
    let cli = Cli::parse();
    validate_endpoint_options(&cli)?;
    let Cli {
        codex_bin,
        url,
        config_overrides,
        dynamic_tools,
        command_timeout_seconds,
        command,
    } = cli;

    let dynamic_tools = parse_dynamic_tools_arg(&dynamic_tools)?;
    let command_timeout = command_timeout_seconds.map(Duration::from_secs);
    let resolve_endpoint = |codex_bin, url| {
        let endpoint = resolve_endpoint(codex_bin, url)?;
        Ok::<_, anyhow::Error>(match command_timeout {
            Some(timeout) => Endpoint::Budgeted(Box::new(endpoint), timeout),
            None => endpoint,
        })
    };

    match command {
        CliCommand::Serve { listen, kill } => {
            ensure_dynamic_tools_unused(&dynamic_tools, "serve")?;
            let codex_bin = codex_bin.unwrap_or_else(|| PathBuf::from("codex"));
            serve(&codex_bin, &config_overrides, &listen, kill)
        }
        CliCommand::SendMessage { user_message } => {
            ensure_dynamic_tools_unused(&dynamic_tools, "send-message")?;
            let endpoint = resolve_endpoint(codex_bin, url)?;
            send_message(&endpoint, &config_overrides, user_message).await
        }
        CliCommand::SendMessageV2 {
            experimental_api,
            user_message,
        } => {
            let endpoint = resolve_endpoint(codex_bin, url)?;
            send_message_v2_endpoint(
                &endpoint,
                &config_overrides,
                user_message,
                experimental_api,
                &dynamic_tools,
            )
            .await
        }
        CliCommand::ResumeMessageV2 {
            thread_id,
            user_message,
        } => {
            let endpoint = resolve_endpoint(codex_bin, url)?;
            resume_message_v2(
                &endpoint,
                &config_overrides,
                thread_id,
                user_message,
                &dynamic_tools,
            )
            .await
        }
        CliCommand::ThreadResume { thread_id } => {
            ensure_dynamic_tools_unused(&dynamic_tools, "thread-resume")?;
            let endpoint = resolve_endpoint(codex_bin, url)?;
            thread_resume_follow(&endpoint, &config_overrides, thread_id).await
        }
        CliCommand::Watch => {
            ensure_dynamic_tools_unused(&dynamic_tools, "watch")?;
            let endpoint = resolve_endpoint(codex_bin, url)?;
            watch(&endpoint, &config_overrides).await
        }
        CliCommand::TestLogin { device_code } => {
            ensure_dynamic_tools_unused(&dynamic_tools, "test-login")?;
            let endpoint = resolve_endpoint(codex_bin, url)?;
            test_login(&endpoint, &config_overrides, device_code).await
        }
        CliCommand::GetAccountRateLimits => {
            ensure_dynamic_tools_unused(&dynamic_tools, "get-account-rate-limits")?;
            let endpoint = resolve_endpoint(codex_bin, url)?;
            get_account_rate_limits(&endpoint, &config_overrides).await
        }
        CliCommand::ModelList => {
            ensure_dynamic_tools_unused(&dynamic_tools, "model-list")?;
            let endpoint = resolve_endpoint(codex_bin, url)?;
            model_list(&endpoint, &config_overrides).await
        }
        CliCommand::ThreadList { limit } => {
            ensure_dynamic_tools_unused(&dynamic_tools, "thread-list")?;
            let endpoint = resolve_endpoint(codex_bin, url)?;
            thread_list(&endpoint, &config_overrides, limit).await
        }
        CliCommand::ThreadIncrementElicitation { thread_id } => {
            ensure_dynamic_tools_unused(&dynamic_tools, "thread-increment-elicitation")?;
            let url = resolve_shared_websocket_url(codex_bin, url, "thread-increment-elicitation")?;
            thread_increment_elicitation(&url, thread_id)
        }
        CliCommand::ThreadDecrementElicitation {
            thread_id,
            lease_id,
        } => {
            ensure_dynamic_tools_unused(&dynamic_tools, "thread-decrement-elicitation")?;
            let url = resolve_shared_websocket_url(codex_bin, url, "thread-decrement-elicitation")?;
            thread_decrement_elicitation(&url, thread_id, lease_id)
        }
        CliCommand::ThreadHoldElicitation {
            thread_id,
            hold_seconds,
        } => {
            ensure_dynamic_tools_unused(&dynamic_tools, "thread-hold-elicitation")?;
            let url = resolve_shared_websocket_url(codex_bin, url, "thread-hold-elicitation")?;
            thread_hold_elicitation(&url, thread_id, hold_seconds)
        }
    }
}

enum Endpoint {
    SpawnCodex(PathBuf),
    ConnectWs(String),
    Budgeted(Box<Endpoint>, Duration),
}


fn validate_endpoint_options(cli: &Cli) -> Result<()> {
    if cli.command_timeout_seconds.is_some()
        && !matches!(
            cli.command,
            CliCommand::SendMessage { .. }
                | CliCommand::SendMessageV2 { .. }
                | CliCommand::ResumeMessageV2 { .. }
                | CliCommand::GetAccountRateLimits
                | CliCommand::ModelList
                | CliCommand::ThreadList { .. }
        )
    {
        bail!("--command-timeout-seconds requires a finite noninteractive command");
    }
    if matches!(cli.command, CliCommand::Serve { .. }) {
        if cli.url.is_some() {
            bail!("serve does not support --url; use --listen to choose the binding");
        }
        return Ok(());
    }
    if matches!(
        cli.command,
        CliCommand::ThreadIncrementElicitation { .. }
            | CliCommand::ThreadDecrementElicitation { .. }
            | CliCommand::ThreadHoldElicitation { .. }
    ) {
        if !cli.config_overrides.is_empty() {
            bail!(
                "elicitation commands do not support --config; configure the existing websocket app-server instead"
            );
        }
        resolve_shared_websocket_url(
            cli.codex_bin.clone(),
            cli.url.clone(),
            "elicitation command",
        )?;
        return Ok(());
    }
    let endpoint = resolve_endpoint(cli.codex_bin.clone(), cli.url.clone())?;
    if matches!(endpoint, Endpoint::ConnectWs(_)) && !cli.config_overrides.is_empty() {
        bail!(
            "--config only configures a spawned app-server; use --codex-bin or configure the existing websocket server instead"
        );
    }
    Ok(())
}

fn resolve_endpoint(codex_bin: Option<PathBuf>, url: Option<String>) -> Result<Endpoint> {
    if codex_bin.is_some() && url.is_some() {
        bail!("--codex-bin and --url are mutually exclusive");
    }
    if let Some(codex_bin) = codex_bin {
        return Ok(Endpoint::SpawnCodex(codex_bin));
    }
    if let Some(url) = url {
        return Ok(Endpoint::ConnectWs(url));
    }
    Ok(Endpoint::ConnectWs("ws://127.0.0.1:4222".to_string()))
}

fn resolve_shared_websocket_url(
    codex_bin: Option<PathBuf>,
    url: Option<String>,
    command: &str,
) -> Result<String> {
    if codex_bin.is_some() {
        bail!(
            "{command} requires --url or an already-running websocket app-server; --codex-bin would spawn a private stdio app-server instead"
        );
    }

    Ok(url.unwrap_or_else(|| "ws://127.0.0.1:4222".to_string()))
}




fn serve(codex_bin: &Path, config_overrides: &[String], listen: &str, kill: bool) -> Result<()> {
    let runtime_path = runtime_dir();
    fs::create_dir_all(&runtime_path)
        .with_context(|| format!("failed to create runtime dir {}", runtime_path.display()))?;
    let log_path = runtime_path.join("app-server.log");
    let mut command = build_serve_command(codex_bin, config_overrides, listen)?;

    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("failed to open log file {}", log_path.display()))?;
    let log_file_stderr = log_file
        .try_clone()
        .with_context(|| format!("failed to clone log file handle {}", log_path.display()))?;

    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_file_stderr));

    let restart_deadline = Instant::now() + APP_SERVER_RESTART_TIMEOUT;
    let child = start_prepared_serve(
        kill,
        || preflight_serve_restart(codex_bin, config_overrides, restart_deadline),
        || kill_listeners_on_same_port(listen, restart_deadline),
        || {
            if kill && Instant::now() >= restart_deadline {
                bail!("restart operation deadline expired before spawn");
            }
            command
                .spawn()
                .with_context(|| format!("failed to start `{}` app-server", codex_bin.display()))
        },
    )?;

    let pid = child.id();

    println!("spawned codex app-server (readiness not checked)");
    println!("listen: {listen}");
    println!("pid: {pid} (app-server process)");
    println!("log: {}", log_path.display());

    Ok(())
}

fn kill_listeners_on_same_port(listen: &str, deadline: Instant) -> Result<()> {
    let url = Url::parse(listen).with_context(|| format!("invalid --listen URL `{listen}`"))?;
    let port = url
        .port_or_known_default()
        .with_context(|| format!("unable to infer port from --listen URL `{listen}`"))?;

    let pids = listener_pids_on_port(port, deadline)?;
    if pids.is_empty() {
        return Ok(());
    }

    let grace_deadline = Instant::now() + Duration::from_millis(300);
    for pid in pids {
        println!("killing listener pid {pid} on port {port}");
        let term_status = terminate_process(pid, false, deadline)?;
        if !term_status.success() {
            continue;
        }
    }

    let remaining = listeners_after_grace(
        || listener_pids_on_port(port, deadline),
        grace_deadline,
        deadline,
        thread::sleep,
    )?;
    if remaining.is_empty() {
        return Ok(());
    }
    for pid in remaining {
        println!("force killing remaining listener pid {pid} on port {port}");
        let status = terminate_process(pid, true, deadline)?;
        if !status.success() {
            bail!("failed to force terminate listener pid {pid}: {status}");
        }
    }
    if !listener_pids_on_port(port, deadline)?.is_empty() {
        bail!("port {port} still has listeners after restart cleanup");
    }
    Ok(())
}

struct SendMessagePolicies<'a> {
    command_name: &'static str,
    experimental_api: bool,
    dynamic_tools: &'a Option<Vec<DynamicToolSpec>>,
}

async fn send_message(
    endpoint: &Endpoint,
    config_overrides: &[String],
    user_message: String,
) -> Result<()> {
    let dynamic_tools = None;
    send_message_v2_with_policies(
        endpoint,
        config_overrides,
        user_message,
        SendMessagePolicies {
            command_name: "send-message",
            experimental_api: false,
            dynamic_tools: &dynamic_tools,
        },
    )
    .await
}

pub async fn send_message_v2(
    codex_bin: &Path,
    config_overrides: &[String],
    user_message: String,
    dynamic_tools: &Option<Vec<DynamicToolSpec>>,
) -> Result<()> {
    let endpoint = Endpoint::SpawnCodex(codex_bin.to_path_buf());
    send_message_v2_endpoint(
        &endpoint,
        config_overrides,
        user_message,
        /*experimental_api*/ true,
        dynamic_tools,
    )
    .await
}

async fn send_message_v2_endpoint(
    endpoint: &Endpoint,
    config_overrides: &[String],
    user_message: String,
    experimental_api: bool,
    dynamic_tools: &Option<Vec<DynamicToolSpec>>,
) -> Result<()> {
    if dynamic_tools.is_some() && !experimental_api {
        bail!("--dynamic-tools requires --experimental-api for send-message-v2");
    }

    send_message_v2_with_policies(
        endpoint,
        config_overrides,
        user_message,
        SendMessagePolicies {
            command_name: "send-message-v2",
            experimental_api,
            dynamic_tools,
        },
    )
    .await
}

async fn resume_message_v2(
    endpoint: &Endpoint,
    config_overrides: &[String],
    thread_id: String,
    user_message: String,
    dynamic_tools: &Option<Vec<DynamicToolSpec>>,
) -> Result<()> {
    ensure_dynamic_tools_unused(dynamic_tools, "resume-message-v2")?;

    with_client("resume-message-v2", endpoint, config_overrides, |client| {
        let initialize = client.initialize()?;
        println!("< initialize response: {initialize:?}");

        let resume_response = client.thread_resume(ThreadResumeParams {
            thread_id,
            ..Default::default()
        })?;
        println!("< thread/resume: {}", resume_response.thread.id);

        let turn_response = client.turn_start(TurnStartParams {
            thread_id: resume_response.thread.id.clone(),
            client_user_message_id: None,
            input: vec![V2UserInput::Text {
                text: user_message,
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })?;
        println!("< turn/start: {}", turn_response.turn.id);

        client.stream_turn(&resume_response.thread.id, &turn_response.turn.id)?;

        Ok(())
    })
    .await
}

async fn thread_resume_follow(
    endpoint: &Endpoint,
    config_overrides: &[String],
    thread_id: String,
) -> Result<()> {
    with_client("thread-resume", endpoint, config_overrides, |client| {
        let initialize = client.initialize()?;
        println!("< initialize response: {initialize:?}");

        let resume_response = client.thread_resume(ThreadResumeParams {
            thread_id,
            ..Default::default()
        })?;
        println!("< thread/resume: {}", resume_response.thread.id);
        println!("< streaming notifications until process is terminated");

        client.stream_notifications_forever()
    })
    .await
}

async fn watch(endpoint: &Endpoint, config_overrides: &[String]) -> Result<()> {
    with_client("watch", endpoint, config_overrides, |client| {
        let initialize = client.initialize()?;
        println!("< initialize response: {initialize:?}");
        println!("< streaming inbound messages until process is terminated");

        client.stream_notifications_forever()
    })
    .await
}




async fn send_message_v2_with_policies(
    endpoint: &Endpoint,
    config_overrides: &[String],
    user_message: String,
    policies: SendMessagePolicies<'_>,
) -> Result<()> {
    with_client(
        policies.command_name,
        endpoint,
        config_overrides,
        |client| {
            let initialize = client.initialize_with_experimental_api(policies.experimental_api)?;
            println!("< initialize response: {initialize:?}");

            let thread_response = client.thread_start(ThreadStartParams {
                dynamic_tools: policies.dynamic_tools.clone(),
                ..Default::default()
            })?;
            println!("< thread/start: {}", thread_response.thread.id);
            let turn_params = TurnStartParams {
                thread_id: thread_response.thread.id.clone(),
                client_user_message_id: None,
                input: vec![V2UserInput::Text {
                    text: user_message,
                    // Test client sends plain text without UI element ranges.
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            };

            let turn_response = client.turn_start(turn_params)?;
            println!("< turn/start: {}", turn_response.turn.id);

            client.stream_turn(&thread_response.thread.id, &turn_response.turn.id)?;

            Ok(())
        },
    )
    .await
}


async fn test_login(
    endpoint: &Endpoint,
    config_overrides: &[String],
    device_code: bool,
) -> Result<()> {
    with_client("test-login", endpoint, config_overrides, |client| {
        let initialize = client.initialize()?;
        println!("< initialize response: {initialize:?}");

        let login_response = if device_code {
            client.login_account_chatgpt_device_code()?
        } else {
            client.login_account_chatgpt()?
        };
        println!("< account/login/start response: {login_response:?}");
        let login_id = match login_response {
            LoginAccountResponse::Chatgpt { login_id, auth_url } => {
                println!("Open the following URL in your browser to continue:\n{auth_url}");
                login_id
            }
            LoginAccountResponse::ChatgptDeviceCode {
                login_id,
                verification_url,
                user_code,
            } => {
                println!(
                    "Open the following URL and enter the code to continue:\n{verification_url}\n\nCode: {user_code}"
                );
                login_id
            }
            _ => bail!("expected chatgpt login response"),
        };

        let completion = client.wait_for_account_login_completion(&login_id)?;
        println!("< account/login/completed notification: {completion:?}");

        if completion.success {
            println!("Login succeeded.");
            Ok(())
        } else {
            bail!(
                "login failed: {}",
                completion
                    .error
                    .as_deref()
                    .unwrap_or("unknown error from account/login/completed")
            );
        }
    })
    .await
}

async fn get_account_rate_limits(endpoint: &Endpoint, config_overrides: &[String]) -> Result<()> {
    with_client(
        "get-account-rate-limits",
        endpoint,
        config_overrides,
        |client| {
            let initialize = client.initialize()?;
            println!("< initialize response: {initialize:?}");

            let response = client.get_account_rate_limits()?;
            println!("< account/rateLimits/read response: {response:?}");

            Ok(())
        },
    )
    .await
}

async fn model_list(endpoint: &Endpoint, config_overrides: &[String]) -> Result<()> {
    with_client("model-list", endpoint, config_overrides, |client| {
        let initialize = client.initialize()?;
        println!("< initialize response: {initialize:?}");

        let response = client.model_list(ModelListParams::default())?;
        println!("< model/list response: {response:?}");

        Ok(())
    })
    .await
}

async fn thread_list(endpoint: &Endpoint, config_overrides: &[String], limit: u32) -> Result<()> {
    with_client("thread-list", endpoint, config_overrides, |client| {
        let initialize = client.initialize()?;
        println!("< initialize response: {initialize:?}");

        let response = client.thread_list(ThreadListParams {
            project_id: None,
            cursor: None,
            limit: Some(limit),
            sort_key: None,
            sort_direction: None,
            model_providers: None,
            source_kinds: None,
            archived: None,
            parent_thread_id: None,
            ancestor_thread_id: None,
            cwd: None,
            use_state_db_only: None,
            search_term: None,
        })?;
        println!("< thread/list response: {response:?}");

        Ok(())
    })
    .await
}

async fn with_client<T>(
    command_name: &'static str,
    endpoint: &Endpoint,
    config_overrides: &[String],
    f: impl FnOnce(&mut CodexClient) -> Result<T>,
) -> Result<T> {
    let tracing = TestClientTracing::initialize(config_overrides).await?;
    let command_span = info_span!(
        "app_server_test_client.command",
        otel.kind = "client",
        otel.name = command_name,
        app_server_test_client.command = command_name,
    );
    let trace_summary = command_span.in_scope(|| TraceSummary::capture(tracing.traces_enabled));
    let result = command_span.in_scope(|| {
        let mut client = CodexClient::connect(endpoint, config_overrides)?;
        run_client_command(&mut client, endpoint, f)
    });
    print_trace_summary(&trace_summary);
    result
}

fn run_client_command<T>(
    client: &mut CodexClient,
    endpoint: &Endpoint,
    operation: impl FnOnce(&mut CodexClient) -> Result<T>,
) -> Result<T> {
    match endpoint {
        Endpoint::Budgeted(_, timeout) => client
            .with_operation_deadline(Instant::now() + *timeout, operation)
            .context("finite command failed; remote turn cancellation was not confirmed and it may still be running; do not automatically resubmit"),
        _ => operation(client),
    }
}

fn thread_increment_elicitation(url: &str, thread_id: String) -> Result<()> {
    let endpoint = Endpoint::ConnectWs(url.to_string());
    let mut client = CodexClient::connect(&endpoint, &[])?;

    let initialize = client.initialize()?;
    println!("< initialize response: {initialize:?}");

    let response =
        client.thread_increment_elicitation(ThreadIncrementElicitationParams { thread_id })?;
    println!("< thread/incrementElicitation response: {response:?}");

    Ok(())
}

fn thread_decrement_elicitation(url: &str, thread_id: String, lease_id: String) -> Result<()> {
    let endpoint = Endpoint::ConnectWs(url.to_string());
    let mut client = CodexClient::connect(&endpoint, &[])?;

    let initialize = client.initialize()?;
    println!("< initialize response: {initialize:?}");

    let response = client.thread_decrement_elicitation(ThreadDecrementElicitationParams {
        thread_id,
        lease_id,
    })?;
    println!("< thread/decrementElicitation response: {response:?}");

    Ok(())
}

fn thread_hold_elicitation(url: &str, thread_id: String, hold_seconds: u64) -> Result<()> {
    let endpoint = Endpoint::ConnectWs(url.to_string());
    let mut client = CodexClient::connect(&endpoint, &[])?;

    let initialize = client.initialize()?;
    println!("< initialize response: {initialize:?}");
    let response = client.thread_increment_elicitation(ThreadIncrementElicitationParams {
        thread_id: thread_id.clone(),
    })?;
    println!("< thread/incrementElicitation response: {response:?}");
    std::thread::sleep(Duration::from_secs(hold_seconds));
    let response = client.thread_decrement_elicitation(ThreadDecrementElicitationParams {
        thread_id,
        lease_id: response.lease_id,
    })?;
    println!("< thread/decrementElicitation response: {response:?}");
    Ok(())
}


fn ensure_dynamic_tools_unused(
    dynamic_tools: &Option<Vec<DynamicToolSpec>>,
    command: &str,
) -> Result<()> {
    if dynamic_tools.is_some() {
        bail!(
            "dynamic tools are only supported for v2 thread/start; remove --dynamic-tools for {command} or use send-message-v2"
        );
    }
    Ok(())
}

fn parse_dynamic_tools_arg(dynamic_tools: &Option<String>) -> Result<Option<Vec<DynamicToolSpec>>> {
    let Some(raw_arg) = dynamic_tools.as_deref() else {
        return Ok(None);
    };

    let raw_json = if let Some(path) = raw_arg.strip_prefix('@') {
        fs::read_to_string(Path::new(path))
            .with_context(|| format!("read dynamic tools file {path}"))?
    } else {
        raw_arg.to_string()
    };

    let value: Value = serde_json::from_str(&raw_json).context("parse dynamic tools JSON")?;
    let values = match value {
        Value::Array(values) => values,
        Value::Object(_) => vec![value],
        _ => bail!("dynamic tools JSON must be an object or array"),
    };
    let tools = normalize_dynamic_tool_specs(values).context("decode dynamic tools")?;

    Ok(Some(tools))
}

enum WebSocketOperation {
    Read(Option<Instant>, mpsc::Sender<Result<Message>>),
    Send(Message, Option<Instant>, mpsc::Sender<Result<()>>),
}

/// Own the async socket and runtime together so connection cancellation closes
/// the socket, including during proxy, TLS, and HTTP handshakes. The rest of the
/// interactive client keeps its synchronous request/response interface.
struct BlockingWebSocket {
    operations: Option<mpsc::Sender<WebSocketOperation>>,
    worker: Option<thread::JoinHandle<()>>,
}

impl BlockingWebSocket {
    fn connect(url: &str) -> Result<Self> {
        Self::connect_with_runtime(url, Duration::from_secs(10), || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
        })
    }

    fn connect_with_runtime(
        url: &str,
        timeout: Duration,
        build_runtime: impl FnOnce() -> std::io::Result<tokio::runtime::Runtime> + Send + 'static,
    ) -> Result<Self> {
        let (operations_tx, operations_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let url = url.to_string();
        let worker = thread::spawn(move || {
            let runtime = match build_runtime() {
                Ok(runtime) => runtime,
                Err(error) => {
                    // If the receiver has gone away, there is no caller left to report to.
                    let _ = ready_tx.send(Err(anyhow::Error::from(error)));
                    return;
                }
            };
            let connection = runtime.block_on(async {
                let mut last_error = None;
                let result = tokio::time::timeout(timeout, async {
                    loop {
                        let mut target = url.clone();
                        // Blocking tungstenite::connect followed up to three redirects.
                        for redirect in 0..=3 {
                            match tokio_tungstenite::connect_async_with_config(
                                target.as_str(),
                                None,
                                true,
                            )
                            .await
                            {
                                Ok((socket, _response)) => return Ok(socket),
                                Err(tokio_tungstenite::tungstenite::Error::Http(response))
                                    if response.status().is_redirection() && redirect < 3 =>
                                {
                                    let location = response
                                        .headers()
                                        .get("Location")
                                        .and_then(|location| location.to_str().ok())
                                        .and_then(|location| {
                                            Url::parse(&target).ok()?.join(location).ok()
                                        });
                                    let Some(location) = location else {
                                        return Err(tokio_tungstenite::tungstenite::Error::Http(
                                            response,
                                        ));
                                    };
                                    target = location.to_string();
                                }
                                Err(error) => {
                                    let Some(delay) = websocket_connect_retry_delay(&error) else {
                                        return Err(error);
                                    };
                                    last_error = Some(error);
                                    tokio::time::sleep(delay).await;
                                    break;
                                }
                            }
                        }
                    }
                })
                .await;
                match result {
                    Ok(result) => result.map_err(anyhow::Error::from),
                    Err(error) => Err(last_error
                        .map(anyhow::Error::from)
                        .unwrap_or_else(|| error.into()))
                    .context("websocket connection deadline expired"),
                }
            });
            let mut socket = match connection {
                Ok(socket) => socket,
                Err(error) => {
                    // A timed-out connect has dropped its owned socket futures.
                    // An already-running OS DNS lookup cannot be cancelled; do
                    // not make runtime teardown extend the caller's deadline.
                    runtime.shutdown_timeout(Duration::ZERO);
                    let _ = ready_tx.send(Err(error));
                    return;
                }
            };
            if ready_tx.send(Ok(())).is_err() {
                return;
            }
            while let Ok(operation) = operations_rx.recv() {
                match operation {
                    WebSocketOperation::Read(deadline, response) => {
                        let result = runtime.block_on(socket_operation_until(deadline, async {
                            socket
                                .next()
                                .await
                                .context("websocket stream ended")?
                                .map_err(anyhow::Error::from)
                        }));
                        let failed = result.is_err();
                        if response.send(result).is_err() || failed {
                            break;
                        }
                    }
                    WebSocketOperation::Send(message, deadline, response) => {
                        let result = runtime.block_on(socket_operation_until(deadline, async {
                            socket.send(message).await.map_err(anyhow::Error::from)
                        }));
                        let failed = result.is_err();
                        if response.send(result).is_err() || failed {
                            break;
                        }
                    }
                }
            }
        });
        let client = Self {
            operations: Some(operations_tx),
            worker: Some(worker),
        };
        ready_rx
            .recv()
            .context("websocket connection worker stopped")??;
        Ok(client)
    }

    fn read(&self, deadline: Option<Instant>) -> Result<Message> {
        let (response, receiver) = mpsc::channel();
        self.operations
            .as_ref()
            .context("websocket worker closed")?
            .send(WebSocketOperation::Read(deadline, response))
            .context("websocket worker stopped")?;
        receiver.recv().context("websocket read worker stopped")?
    }

    fn send(&self, message: Message, deadline: Option<Instant>) -> Result<()> {
        let (response, receiver) = mpsc::channel();
        self.operations
            .as_ref()
            .context("websocket worker closed")?
            .send(WebSocketOperation::Send(message, deadline, response))
            .context("websocket worker stopped")?;
        receiver.recv().context("websocket write worker stopped")?
    }
}

fn websocket_connect_retry_delay(error: &tungstenite::Error) -> Option<Duration> {
    match error {
        tungstenite::Error::Io(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionRefused
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::WouldBlock
                    | std::io::ErrorKind::Interrupted
            ) =>
        {
            Some(Duration::from_millis(50))
        }
        tungstenite::Error::Http(response)
            if matches!(response.status().as_u16(), 429 | 502 | 503 | 504) =>
        {
            Some(
                response
                    .headers()
                    .get("Retry-After")
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.parse::<u64>().ok())
                    .map(Duration::from_secs)
                    .unwrap_or(Duration::from_millis(50)),
            )
        }
        _ => None,
    }
}

async fn socket_operation_until<T>(
    deadline: Option<Instant>,
    operation: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    match deadline {
        Some(deadline) => tokio::time::timeout_at(deadline.into(), operation)
            .await
            .context("websocket operation deadline expired")?,
        None => operation.await,
    }
}

impl Drop for BlockingWebSocket {
    fn drop(&mut self) {
        drop(self.operations.take());
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            eprintln!("websocket client worker panicked");
        }
    }
}

struct StdioLineReader {
    lines: mpsc::Receiver<std::io::Result<String>>,
}

impl From<BufReader<ChildStdout>> for StdioLineReader {
    fn from(mut stdout: BufReader<ChildStdout>) -> Self {
        let (sender, lines) = mpsc::sync_channel(1);
        // A descendant can retain the pipe after the direct child is killed.
        // Keep native reads detached, but bound read-ahead and caller waiting.
        thread::spawn(move || {
            loop {
                let mut line = String::new();
                let result = std::io::Read::take(&mut stdout, 32 * 1024 * 1024 + 1)
                    .read_line(&mut line)
                    .and_then(|bytes| {
                        if bytes == 0 {
                            Err(std::io::Error::new(
                                std::io::ErrorKind::UnexpectedEof,
                                "codex app-server closed stdout",
                            ))
                        } else if line.len() > 32 * 1024 * 1024 {
                            Err(std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                "codex app-server stdout line exceeds 32 MiB",
                            ))
                        } else {
                            Ok(line)
                        }
                    });
                let failed = result.is_err();
                if sender.send(result).is_err() || failed {
                    break;
                }
            }
        });
        Self { lines }
    }
}

impl StdioLineReader {
    fn read(&self, deadline: Option<Instant>) -> Result<String> {
        let result = match deadline {
            Some(deadline) => self
                .lines
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .context("stdio read operation deadline expired or reader stopped")?,
            None => self.lines.recv().context("stdio reader stopped")?,
        };
        result.context("failed to read from codex app-server")
    }
}

enum ClientTransport {
    Stdio {
        child: std::sync::Arc<std::sync::Mutex<Child>>,
        stdin: Option<ChildStdin>,
        stdout: StdioLineReader,
    },
    WebSocket {
        url: String,
        socket: Box<BlockingWebSocket>,
    },
}

struct CodexClient {
    transport: ClientTransport,
    shutdown_grace: Duration,
    operation_deadline: Option<Instant>,
    wire_dump: bool,
    pending_notifications: VecDeque<JSONRPCNotification>,
    command_approval_behavior: CommandApprovalBehavior,
    command_approval_count: usize,
    last_turn_error_message: Option<String>,
}

#[derive(Debug, Clone, Copy)]
enum CommandApprovalBehavior {
    AlwaysAccept,
}



impl CodexClient {
    fn connect(endpoint: &Endpoint, config_overrides: &[String]) -> Result<Self> {
        match endpoint {
            Endpoint::SpawnCodex(codex_bin) => Self::spawn_stdio(codex_bin, config_overrides),
            Endpoint::ConnectWs(url) => Self::connect_websocket(url),
            Endpoint::Budgeted(endpoint, _) => Self::connect(endpoint, config_overrides),
        }
    }


    fn spawn_stdio(
        codex_bin: &Path,
        config_overrides: &[String],
    ) -> Result<Self> {
        let codex_bin_display = codex_bin.display();
        let mut cmd = Command::new(codex_bin);
        add_codex_parent_to_path(&mut cmd, codex_bin)?;
        for override_kv in config_overrides {
            cmd.arg("--config").arg(override_kv);
        }
        let mut codex_app_server = cmd
            .arg("app-server")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("failed to start `{codex_bin_display}` app-server"))?;

        let stdin = codex_app_server
            .stdin
            .take()
            .context("codex app-server stdin unavailable")?;
        let stdout = codex_app_server
            .stdout
            .take()
            .context("codex app-server stdout unavailable")?;

        Ok(Self::from_transport(ClientTransport::Stdio {
            child: std::sync::Arc::new(std::sync::Mutex::new(codex_app_server)),
            stdin: Some(stdin),
            stdout: BufReader::new(stdout).into(),
        }))
    }

    fn connect_websocket(url: &str) -> Result<Self> {
        let parsed = Url::parse(url).with_context(|| format!("invalid websocket URL `{url}`"))?;
        let socket = BlockingWebSocket::connect(parsed.as_str()).with_context(|| {
            format!(
                "failed to connect to websocket app-server at `{url}`; if no server is running, start one with `codex-app-server-test-client serve --listen {url}`"
            )
        })?;
        Ok(Self::from_transport(ClientTransport::WebSocket {
            url: url.to_string(),
            socket: Box::new(socket),
        }))
    }

    fn from_transport(transport: ClientTransport) -> Self {
        Self {
            transport,
            shutdown_grace: APP_SERVER_GRACEFUL_SHUTDOWN_TIMEOUT,
            operation_deadline: None,
            wire_dump: std::env::var_os("CODEX_APP_SERVER_TEST_CLIENT_WIRE_DUMP").is_some(),
            pending_notifications: VecDeque::new(),
            command_approval_behavior: CommandApprovalBehavior::AlwaysAccept,
            command_approval_count: 0,
            last_turn_error_message: None,
        }
    }


    /// Bounds a finite client operation, including its synchronous child RPC/turn IO.
    /// Expiry terminates this owned stdio server; callers must not treat its remote state as known.
    fn with_operation_deadline<T>(
        &mut self,
        deadline: Instant,
        operation: impl FnOnce(&mut Self) -> Result<T>,
    ) -> Result<T> {
        let deadline = self
            .operation_deadline
            .map_or(deadline, |prior| prior.min(deadline));
        deadline
            .checked_duration_since(Instant::now())
            .context("smoke operation deadline expired before starting IO")?;
        // Reuse an enclosing watchdog when it already enforces this budget.
        if self
            .operation_deadline
            .is_some_and(|prior| prior <= deadline)
        {
            return operation(self);
        }
        let ClientTransport::Stdio { child, .. } = &self.transport else {
            let previous = self.operation_deadline.replace(deadline);
            let result = operation(self);
            self.operation_deadline = previous;
            return result;
        };
        let child = std::sync::Arc::clone(child);
        let previous = self.operation_deadline.replace(deadline);
        let result = thread::scope(|scope| {
            let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
            let watchdog = scope.spawn(move || -> Result<()> {
                if matches!(
                    stop_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                ) || Instant::now() >= deadline
                {
                    let mut child = child
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if child.try_wait()?.is_none() {
                        child
                            .kill()
                            .context("terminate timed-out smoke app-server")?;
                    }
                    child.wait().context("reap timed-out smoke app-server")?;
                }
                Ok(())
            });
            let result = operation(self);
            let _ = stop_tx.send(());
            watchdog
                .join()
                .map_err(|_| anyhow::anyhow!("smoke deadline watchdog panicked"))??;
            if Instant::now() >= deadline {
                bail!("smoke operation deadline expired; app-server state may be unknown");
            }
            result
        });
        self.operation_deadline = previous;
        result
    }

    fn check_operation_deadline(&self) -> Result<()> {
        if self
            .operation_deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            bail!("smoke operation deadline expired before further IO");
        }
        Ok(())
    }


    fn initialize(&mut self) -> Result<InitializeResponse> {
        self.initialize_with_experimental_api(/*experimental_api*/ true)
    }

    fn initialize_with_experimental_api(
        &mut self,
        experimental_api: bool,
    ) -> Result<InitializeResponse> {
        let request_id = self.request_id();
        let request = ClientRequest::Initialize {
            request_id: request_id.clone(),
            params: InitializeParams {
                client_info: ClientInfo {
                    name: "codex-toy-app-server".to_string(),
                    title: Some("Codex Toy App Server".to_string()),
                    version: env!("CARGO_PKG_VERSION").to_string(),
                },
                capabilities: Some(InitializeCapabilities {
                    experimental_api,
                    request_attestation: false,
                    opt_out_notification_methods: Some(
                        NOTIFICATIONS_TO_OPT_OUT
                            .iter()
                            .map(|method| (*method).to_string())
                            .collect(),
                    ),
                    mcp_server_openai_form_elicitation: false,
                }),
            },
        };

        self.with_operation_deadline(Instant::now() + RPC_TIMEOUT, |client| {
            let response: InitializeResponse =
                client.send_request(request, request_id, "initialize")?;
            // Complete the initialize handshake within the same IO budget.
            client.write_jsonrpc_message(JSONRPCMessage::Notification(JSONRPCNotification {
                method: "initialized".to_string(),
                params: None,
            }))?;
            Ok(response)
        })
    }

    fn thread_start(&mut self, params: ThreadStartParams) -> Result<ThreadStartResponse> {
        if params
            .dynamic_tools
            .as_ref()
            .is_some_and(|tools| !tools.is_empty())
        {
            eprintln!(
                "dynamic tools are registered only; this client does not execute dynamic tool calls"
            );
        }
        let request_id = self.request_id();
        let request = ClientRequest::ThreadStart {
            request_id: request_id.clone(),
            params,
        };

        self.send_request(request, request_id, "thread/start")
    }

    fn thread_resume(&mut self, params: ThreadResumeParams) -> Result<ThreadResumeResponse> {
        let request_id = self.request_id();
        let request = ClientRequest::ThreadResume {
            request_id: request_id.clone(),
            params,
        };

        self.send_request(request, request_id, "thread/resume")
    }

    fn turn_start(&mut self, params: TurnStartParams) -> Result<TurnStartResponse> {
        let request_id = self.request_id();
        let request = ClientRequest::TurnStart {
            request_id: request_id.clone(),
            params,
        };

        self.send_request(request, request_id, "turn/start")
    }

    fn login_account_chatgpt(&mut self) -> Result<LoginAccountResponse> {
        let request_id = self.request_id();
        let request = ClientRequest::LoginAccount {
            request_id: request_id.clone(),
            params: codex_app_server_protocol::LoginAccountParams::Chatgpt {
                app_brand: None,
                codex_streamlined_login: false,
                use_hosted_login_success_page: false,
            },
        };

        self.send_request(request, request_id, "account/login/start")
    }

    fn login_account_chatgpt_device_code(&mut self) -> Result<LoginAccountResponse> {
        let request_id = self.request_id();
        let request = ClientRequest::LoginAccount {
            request_id: request_id.clone(),
            params: codex_app_server_protocol::LoginAccountParams::ChatgptDeviceCode,
        };

        self.send_request(request, request_id, "account/login/start")
    }

    fn get_account_rate_limits(&mut self) -> Result<GetAccountRateLimitsResponse> {
        let request_id = self.request_id();
        let request = ClientRequest::GetAccountRateLimits {
            request_id: request_id.clone(),
            params: None,
        };

        self.send_request(request, request_id, "account/rateLimits/read")
    }

    fn model_list(&mut self, params: ModelListParams) -> Result<ModelListResponse> {
        let request_id = self.request_id();
        let request = ClientRequest::ModelList {
            request_id: request_id.clone(),
            params,
        };

        self.send_request(request, request_id, "model/list")
    }

    fn thread_list(&mut self, params: ThreadListParams) -> Result<ThreadListResponse> {
        let request_id = self.request_id();
        let request = ClientRequest::ThreadList {
            request_id: request_id.clone(),
            params,
        };

        self.send_request(request, request_id, "thread/list")
    }

    fn thread_increment_elicitation(
        &mut self,
        params: ThreadIncrementElicitationParams,
    ) -> Result<ThreadIncrementElicitationResponse> {
        let request_id = self.request_id();
        let request = ClientRequest::ThreadIncrementElicitation {
            request_id: request_id.clone(),
            params,
        };

        self.send_request(request, request_id, "thread/incrementElicitation")
    }

    fn thread_decrement_elicitation(
        &mut self,
        params: ThreadDecrementElicitationParams,
    ) -> Result<ThreadDecrementElicitationResponse> {
        let request_id = self.request_id();
        let request = ClientRequest::ThreadDecrementElicitation {
            request_id: request_id.clone(),
            params,
        };

        self.send_request(request, request_id, "thread/decrementElicitation")
    }

    fn wait_for_account_login_completion(
        &mut self,
        expected_login_id: &str,
    ) -> Result<AccountLoginCompletedNotification> {
        loop {
            let notification = self.next_notification()?;

            if let Ok(server_notification) = ServerNotification::try_from(notification) {
                match server_notification {
                    ServerNotification::AccountLoginCompleted(completion) => {
                        if completion.login_id.as_deref() == Some(expected_login_id) {
                            return Ok(completion);
                        }

                        println!(
                            "[ignoring account/login/completed for unexpected login_id: {:?}]",
                            completion.login_id
                        );
                    }
                    ServerNotification::AccountRateLimitsUpdated(snapshot) => {
                        println!("< accountRateLimitsUpdated notification: {snapshot:?}");
                    }
                    _ => {}
                }
            }
        }
    }

    fn stream_turn(&mut self, thread_id: &str, turn_id: &str) -> Result<()> {
        self.last_turn_error_message = None;
        loop {
            let notification = self.next_notification()?;

            let Ok(server_notification) = ServerNotification::try_from(notification) else {
                continue;
            };

            match server_notification {
                ServerNotification::ThreadStarted(payload) => {
                    if payload.thread.id == thread_id {
                        println!("< thread/started: {}", payload.thread.id);
                    }
                }
                ServerNotification::TurnStarted(payload) => {
                    if payload.thread_id == thread_id && payload.turn.id == turn_id {
                        println!("< turn/started notification: {:?}", payload.turn.status);
                    }
                }
                ServerNotification::AgentMessageDelta(delta) => {
                    if delta.thread_id != thread_id || delta.turn_id != turn_id {
                        continue;
                    }
                    print!("{}", delta.delta);
                    std::io::stdout().flush().ok();
                }
                ServerNotification::CommandExecutionOutputDelta(delta) => {
                    if delta.thread_id != thread_id || delta.turn_id != turn_id {
                        continue;
                    }
                    print!("{}", delta.delta);
                    std::io::stdout().flush().ok();
                }
                ServerNotification::TerminalInteraction(delta) => {
                    if delta.thread_id != thread_id || delta.turn_id != turn_id {
                        continue;
                    }
                    println!("[stdin sent: {}]", delta.stdin);
                    std::io::stdout().flush().ok();
                }
                ServerNotification::ItemStarted(payload) => {
                    if payload.thread_id != thread_id || payload.turn_id != turn_id {
                        continue;
                    }
                    println!("\n< item started: {}", payload.item.id());
                }
                ServerNotification::ItemCompleted(payload) => {
                    if payload.thread_id != thread_id || payload.turn_id != turn_id {
                        continue;
                    }
                    if let ThreadItem::CommandExecution {
                        aggregated_output,
                        ..
                    } = &payload.item
                    {
                        if let Some(output) = aggregated_output {
                            println!("{output}");
                        }
                    } else if let ThreadItem::AgentMessage { text, .. } = &payload.item {
                        println!("{text}");
                    }
                    println!("< item completed: {}", payload.item.id());
                }
                ServerNotification::TurnCompleted(payload) => {
                    if payload.thread_id == thread_id && payload.turn.id == turn_id {
                        self.last_turn_error_message = payload
                            .turn
                            .error
                            .as_ref()
                            .map(|error| error.message.clone());
                        println!("\n< turn/completed notification: {:?}", payload.turn.status);
                        if payload.turn.status == TurnStatus::Failed
                            && let Some(error) = payload.turn.error
                        {
                            println!("[turn error] {}", error.message);
                        }
                        if payload.turn.status != TurnStatus::Completed {
                            bail!(
                                "turn {turn_id} ended with {:?}: {}",
                                payload.turn.status,
                                self.last_turn_error_message
                                    .as_deref()
                                    .unwrap_or("no error details")
                            );
                        }
                        break;
                    }
                }
                ServerNotification::McpToolCallProgress(payload) => {
                    if payload.thread_id != thread_id || payload.turn_id != turn_id {
                        continue;
                    }
                    println!("< MCP tool progress: {}", payload.message);
                }
                _ => {
                    if self.wire_dump {
                        println!("[UNKNOWN SERVER NOTIFICATION] {server_notification:?}");
                    }
                }
            }
        }

        Ok(())
    }


    fn stream_notifications_forever(&mut self) -> Result<()> {
        self.wire_dump = true;
        loop {
            let _ = self.next_notification()?;
        }
    }

    fn send_request<T>(
        &mut self,
        request: ClientRequest,
        request_id: RequestId,
        method: &str,
    ) -> Result<T>
    where
        T: DeserializeOwned,
    {
        let request_span = info_span!(
            "app_server_test_client.request",
            otel.kind = "client",
            otel.name = method,
            rpc.system = "jsonrpc",
            rpc.method = method,
            rpc.request_id = ?request_id,
        );
        request_span.in_scope(|| {
            self.with_operation_deadline(Instant::now() + RPC_TIMEOUT, |client| {
                client.write_request(&request)?;
                client.wait_for_response(request_id, method)
            })
        })
    }

    fn write_request(&mut self, request: &ClientRequest) -> Result<()> {
        let request_value = serde_json::to_value(request)?;
        let mut request: JSONRPCRequest = serde_json::from_value(request_value)
            .context("client request was not a valid JSON-RPC request")?;
        request.trace = current_span_w3c_trace_context();
        let request_json = serde_json::to_string(&request)?;
        if self.wire_dump {
            print_multiline_with_prefix("> ", &serde_json::to_string_pretty(&request)?);
        }
        self.write_payload(&request_json)
    }

    fn wait_for_response<T>(&mut self, request_id: RequestId, method: &str) -> Result<T>
    where
        T: DeserializeOwned,
    {
        loop {
            let message = self.read_jsonrpc_message()?;

            match message {
                JSONRPCMessage::Response(JSONRPCResponse { id, result }) => {
                    if id == request_id {
                        return serde_json::from_value(result)
                            .with_context(|| format!("{method} response missing payload"));
                    }
                }
                JSONRPCMessage::Error(err) => {
                    if err.id == request_id {
                        bail!("{method} failed: {err:?}");
                    }
                }
                JSONRPCMessage::Notification(notification) => {
                    self.pending_notifications.push_back(notification);
                }
                JSONRPCMessage::Request(request) => {
                    self.handle_server_request(request)?;
                }
            }
        }
    }

    fn next_notification(&mut self) -> Result<JSONRPCNotification> {
        self.check_operation_deadline()?;
        if let Some(notification) = self.pending_notifications.pop_front() {
            return Ok(notification);
        }

        loop {
            let message = self.read_jsonrpc_message()?;

            match message {
                JSONRPCMessage::Notification(notification) => return Ok(notification),
                JSONRPCMessage::Response(_) | JSONRPCMessage::Error(_) => {
                    // No outstanding requests, so ignore stray responses/errors for now.
                    continue;
                }
                JSONRPCMessage::Request(request) => {
                    self.handle_server_request(request)?;
                }
            }
        }
    }

    fn read_jsonrpc_message(&mut self) -> Result<JSONRPCMessage> {
        loop {
            let raw = self.read_payload()?;
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                continue;
            }

            let parsed: Value =
                serde_json::from_str(trimmed).context("response was not valid JSON-RPC")?;
            if self.wire_dump {
                print_multiline_with_prefix("< ", &serde_json::to_string_pretty(&parsed)?);
            }
            let message: JSONRPCMessage = serde_json::from_value(parsed)
                .context("response was not a valid JSON-RPC message")?;
            return Ok(message);
        }
    }

    fn request_id(&self) -> RequestId {
        RequestId::String(Uuid::new_v4().to_string())
    }

    fn handle_server_request(&mut self, request: JSONRPCRequest) -> Result<()> {
        let server_request = ServerRequest::try_from(request)
            .context("failed to deserialize ServerRequest from JSONRPCRequest")?;

        match server_request {
            ServerRequest::CommandExecutionRequestApproval { request_id, params } => {
                self.handle_command_execution_request_approval(request_id, params)?;
            }
            ServerRequest::FileChangeRequestApproval { request_id, params } => {
                self.approve_file_change_request(request_id, params)?;
            }
            ServerRequest::ToolRequestUserInput { request_id, params } => {
                if self.operation_deadline.is_some() {
                    bail!("request_user_input cannot block terminal input during a finite RPC");
                }
                let response = request_user_input::prompt_for_answers(&params)?;
                self.send_server_request_response(request_id, &response)?;
            }
            other => {
                bail!("received unsupported server request: {other:?}");
            }
        }

        Ok(())
    }

    fn handle_command_execution_request_approval(
        &mut self,
        request_id: RequestId,
        params: CommandExecutionRequestApprovalParams,
    ) -> Result<()> {
        let CommandExecutionRequestApprovalParams {
            thread_id,
            turn_id,
            item_id,
            started_at_ms: _,
            approval_id,
            environment_id,
            reason,
            network_approval_context,
            command,
            cwd,
            command_actions,
            additional_permissions,
            proposed_execpolicy_amendment,
            proposed_network_policy_amendments,
            available_decisions,
        } = params;

        println!(
            "\n< commandExecution approval requested for thread {thread_id}, turn {turn_id}, item {item_id}, approval {}",
            approval_id.as_deref().unwrap_or("<none>")
        );
        self.command_approval_count += 1;
        if let Some(environment_id) = environment_id.as_deref() {
            println!("< environment: {environment_id}");
        }
        if let Some(reason) = reason.as_deref() {
            println!("< reason: {reason}");
        }
        if let Some(network_approval_context) = network_approval_context.as_ref() {
            println!("< network approval context: {network_approval_context:?}");
        }
        if let Some(available_decisions) = available_decisions.as_ref() {
            println!("< available decisions: {available_decisions:?}");
        }
        if let Some(command) = command.as_deref() {
            println!("< command: {command}");
        }
        if let Some(cwd) = cwd.as_ref() {
            println!("< cwd: {cwd}");
        }
        if let Some(command_actions) = command_actions.as_ref()
            && !command_actions.is_empty()
        {
            println!("< command actions: {command_actions:?}");
        }
        if let Some(additional_permissions) = additional_permissions.as_ref() {
            println!("< additional permissions: {additional_permissions:?}");
        }
        if let Some(execpolicy_amendment) = proposed_execpolicy_amendment.as_ref() {
            println!("< proposed execpolicy amendment: {execpolicy_amendment:?}");
        }
        if let Some(network_policy_amendments) = proposed_network_policy_amendments.as_ref() {
            println!("< proposed network policy amendments: {network_policy_amendments:?}");
        }

        let decision = match self.command_approval_behavior {
            CommandApprovalBehavior::AlwaysAccept => CommandExecutionApprovalDecision::Accept,
        };
        let response = CommandExecutionRequestApprovalResponse {
            decision: decision.clone(),
        };
        self.send_server_request_response(request_id, &response)?;
        println!(
            "< commandExecution decision for approval #{} on item {item_id}: {:?}",
            self.command_approval_count, decision
        );
        Ok(())
    }

    fn approve_file_change_request(
        &mut self,
        request_id: RequestId,
        params: FileChangeRequestApprovalParams,
    ) -> Result<()> {
        let FileChangeRequestApprovalParams {
            thread_id,
            turn_id,
            item_id,
            started_at_ms: _,
            reason,
            grant_root,
        } = params;

        println!(
            "\n< fileChange approval requested for thread {thread_id}, turn {turn_id}, item {item_id}"
        );
        if let Some(reason) = reason.as_deref() {
            println!("< reason: {reason}");
        }
        if let Some(grant_root) = grant_root.as_deref() {
            println!("< grant root: {}", grant_root.display());
        }

        let response = FileChangeRequestApprovalResponse {
            decision: FileChangeApprovalDecision::Accept,
        };
        self.send_server_request_response(request_id, &response)?;
        println!("< approved fileChange request for item {item_id}");
        Ok(())
    }

    fn send_server_request_response<T>(&mut self, request_id: RequestId, response: &T) -> Result<()>
    where
        T: Serialize,
    {
        let message = JSONRPCMessage::Response(JSONRPCResponse {
            id: request_id,
            result: serde_json::to_value(response)?,
        });
        self.write_jsonrpc_message(message)
    }

    fn write_jsonrpc_message(&mut self, message: JSONRPCMessage) -> Result<()> {
        let payload = serde_json::to_string(&message)?;
        if let Some(pretty) = wire_diagnostic(&message, self.wire_dump)? {
            print_multiline_with_prefix("> ", &pretty);
        }
        self.write_payload(&payload)
    }

    fn write_payload(&mut self, payload: &str) -> Result<()> {
        self.check_operation_deadline()?;
        match &mut self.transport {
            ClientTransport::Stdio { stdin, .. } => {
                if let Some(stdin) = stdin.as_mut() {
                    writeln!(stdin, "{payload}")?;
                    stdin
                        .flush()
                        .context("failed to flush payload to codex app-server")?;
                    return Ok(());
                }
                bail!("codex app-server stdin closed")
            }
            ClientTransport::WebSocket { socket, url } => {
                socket
                    .send(
                        Message::Text(payload.to_string().into()),
                        Some(
                            self.operation_deadline
                                .unwrap_or_else(|| Instant::now() + RPC_TIMEOUT),
                        ),
                    )
                    .with_context(|| format!("failed to write websocket message to `{url}`"))?;
                Ok(())
            }
        }
    }

    fn read_payload(&mut self) -> Result<String> {
        self.check_operation_deadline()?;
        match &mut self.transport {
            ClientTransport::Stdio { stdout, .. } => stdout.read(self.operation_deadline),
            ClientTransport::WebSocket { socket, url } => loop {
                let frame = socket
                    .read(self.operation_deadline)
                    .with_context(|| format!("failed to read websocket message from `{url}`"))?;
                match frame {
                    Message::Text(text) => return Ok(text.to_string()),
                    Message::Binary(_) | Message::Ping(_) | Message::Pong(_) => continue,
                    Message::Close(_) => {
                        bail!("websocket app-server at `{url}` closed the connection")
                    }
                    Message::Frame(_) => continue,
                }
            },
        }
    }
}


// Answer contents never belong in diagnostics, even with explicit wire logging.
fn wire_diagnostic(message: &JSONRPCMessage, enabled: bool) -> Result<Option<String>> {
    if !enabled {
        return Ok(None);
    }
    if let JSONRPCMessage::Response(response) = message
        && response.result.get("answers").is_some()
    {
        return Ok(Some(format!(
            "response {:?}: [user input answers redacted]",
            response.id
        )));
    }
    Ok(Some(serde_json::to_string_pretty(message)?))
}

fn print_multiline_with_prefix(prefix: &str, payload: &str) {
    for line in payload.lines() {
        println!("{prefix}{line}");
    }
}

struct TestClientTracing {
    _otel_provider: Option<OtelProvider>,
    traces_enabled: bool,
}

impl TestClientTracing {
    async fn initialize(config_overrides: &[String]) -> Result<Self> {
        let cli_kv_overrides = CliConfigOverrides {
            raw_overrides: config_overrides.to_vec(),
        }
        .parse_overrides()
        .map_err(|e| anyhow::anyhow!("error parsing -c overrides: {e}"))?;
        let config = Config::load_with_cli_overrides(cli_kv_overrides)
            .await
            .context("error loading config")?;
        let otel_provider = codex_core::otel_init::build_provider(
            &config,
            env!("CARGO_PKG_VERSION"),
            Some(OTEL_SERVICE_NAME),
            DEFAULT_ANALYTICS_ENABLED,
        )
        .map_err(|e| anyhow::anyhow!("error loading otel config: {e}"))?;
        let traces_enabled = otel_provider
            .as_ref()
            .and_then(|provider| provider.tracer_provider.as_ref())
            .is_some();
        if let Some(provider) = otel_provider.as_ref()
            && traces_enabled
        {
            let _ = tracing_subscriber::registry()
                .with(provider.tracing_layer())
                .try_init();
        }
        Ok(Self {
            traces_enabled,
            _otel_provider: otel_provider,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TraceSummary {
    Enabled { url: String },
    Disabled,
}

impl TraceSummary {
    fn capture(traces_enabled: bool) -> Self {
        if !traces_enabled {
            return Self::Disabled;
        }
        current_span_w3c_trace_context()
            .as_ref()
            .and_then(trace_url_from_context)
            .map_or(Self::Disabled, |url| Self::Enabled { url })
    }
}

fn trace_url_from_context(trace: &W3cTraceContext) -> Option<String> {
    let traceparent = trace.traceparent.as_deref()?;
    let mut parts = traceparent.split('-');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(_version), Some(trace_id), Some(_span_id), Some(_trace_flags))
            if trace_id.len() == 32 =>
        {
            Some(format!("go/trace/{trace_id}"))
        }
        _ => None,
    }
}

fn print_trace_summary(trace_summary: &TraceSummary) {
    println!("\n[Datadog trace]");
    match trace_summary {
        TraceSummary::Enabled { url } => println!("{url}\n"),
        TraceSummary::Disabled => println!("{TRACE_DISABLED_MESSAGE}\n"),
    }
}

impl Drop for CodexClient {
    fn drop(&mut self) {
        let ClientTransport::Stdio { child, stdin, .. } = &mut self.transport else {
            return;
        };

        let _ = stdin.take();
        let mut child = child
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        if let Ok(Some(status)) = child.try_wait() {
            println!("[codex app-server exited: {status}]");
            return;
        }

        let deadline = Instant::now() + self.shutdown_grace;
        loop {
            if let Ok(Some(status)) = child.try_wait() {
                println!("[codex app-server exited: {status}]");
                return;
            }

            if Instant::now() >= deadline {
                break;
            }

            thread::sleep(
                APP_SERVER_GRACEFUL_SHUTDOWN_POLL_INTERVAL
                    .min(deadline.saturating_duration_since(Instant::now())),
            );
        }

        let _ = child.kill();
        let _ = child.wait();
    }
}


