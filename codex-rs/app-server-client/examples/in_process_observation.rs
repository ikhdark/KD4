//! KDR's finite stdio adapter to the real public in-process client.
//! The diagnostic-only reconnect command is not a production app-server API.
use codex_app_server_client::InProcessAppServerClient;
use codex_app_server_client::InProcessClientStartArgs;
use codex_app_server_client::InProcessServerEvent;
use codex_app_server_protocol_types::ClientRequest;
use codex_app_server_protocol_types::RequestId;
use codex_arg0::Arg0DispatchPaths;
use codex_config::CloudConfigBundleLoader;
use codex_config::LoaderOverrides;
use codex_core::config::ConfigBuilder;
use codex_core::config::ConfigOverrides;
use codex_exec_server::EnvironmentManager;
use codex_exec_server::ExecServerRuntimePaths;
use codex_feedback::CodexFeedback;
use codex_protocol::protocol::SessionSource;
use serde_json::Value;
use serde_json::json;
use std::error::Error;
use std::io::BufRead;
use std::io::Read;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::task::JoinSet;

type Failure = Box<dyn Error + Send + Sync>;

fn emit(value: Value) -> Result<(), Failure> {
    let mut output = std::io::stdout().lock();
    serde_json::to_writer(&mut output, &value)?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}

async fn start(
    cli: &PathBuf,
    overrides: &[(String, toml::Value)],
) -> Result<InProcessAppServerClient, Failure> {
    let home = PathBuf::from(
        std::env::var_os("CODEX_HOME").ok_or("explicit private CODEX_HOME required")?,
    );
    let config = ConfigBuilder::default()
        .codex_home(home)
        .cli_overrides(overrides.to_vec())
        .harness_overrides(ConfigOverrides {
            codex_self_exe: Some(cli.clone()),
            ..Default::default()
        })
        .build()
        .await?;
    let paths = ExecServerRuntimePaths::from_optional_path(Some(cli.clone()))?;
    let environments = EnvironmentManager::from_codex_home_with_http_client_factory(
        config.codex_home.clone(),
        Some(paths),
        config.http_client_factory(),
    )
    .await?;
    let state_db = codex_core::init_state_db(&config).await;
    Ok(InProcessAppServerClient::start(InProcessClientStartArgs {
        arg0_paths: Arg0DispatchPaths {
            codex_self_exe: Some(cli.clone()),
        },
        config: Arc::new(config),
        cli_overrides: overrides.to_vec(),
        loader_overrides: LoaderOverrides::default(),
        strict_config: false,
        cloud_config_bundle: CloudConfigBundleLoader::default(),
        feedback: CodexFeedback::new(),
        log_db: None,
        state_db,
        environment_manager: Arc::new(environments),
        config_warnings: Vec::new(),
        session_source: SessionSource::Exec,
        enable_codex_api_key_env: false,
        client_name: "kdr_in_process_observation".into(),
        client_version: env!("CARGO_PKG_VERSION").into(),
        experimental_api: true,
        mcp_server_openai_form_elicitation: false,
        opt_out_notification_methods: Vec::new(),
        channel_capacity: 8,
    })
    .await?)
}

async fn run() -> Result<(), Failure> {
    let mut arguments = std::env::args().skip(1);
    let cli = PathBuf::from(
        arguments
            .next()
            .ok_or("explicit matched Codex executable required")?,
    )
    .canonicalize()?;
    let mut overrides = Vec::new();
    while let Some(flag) = arguments.next() {
        if flag != "-c" {
            return Err("only explicit -c overrides admitted".into());
        }
        let assignment = arguments.next().ok_or("missing override")?;
        let (key, value) = assignment.split_once('=').ok_or("invalid override")?;
        let mut parsed: toml::Table = toml::from_str(&format!("value={value}"))?;
        overrides.push((
            key.to_string(),
            parsed.remove("value").ok_or("missing override value")?,
        ));
    }
    let mut client = Some(start(&cli, &overrides).await?);
    let (input_tx, mut input_rx) = mpsc::channel(8);
    let input = std::thread::spawn(move || {
        let mut input = std::io::stdin().lock();
        loop {
            let mut line = Vec::new();
            let read = (&mut input)
                .take(8 * 1024 * 1024 + 1)
                .read_until(b'\n', &mut line);
            let value = match read {
                Ok(0) => break,
                Ok(_) if line.len() <= 8 * 1024 * 1024 => {
                    serde_json::from_slice(&line).map_err(|e| e.to_string())
                }
                Ok(_) => Err("bounded input frame exceeded".into()),
                Err(error) => Err(error.to_string()),
            };
            if input_tx.blocking_send(value).is_err() {
                break;
            }
        }
    });
    let mut requests = JoinSet::new();
    let mut failure: Option<Failure> = None;
    loop {
        let result: Result<bool, Failure> = async {
            tokio::select! {
                incoming = input_rx.recv() => {
                    let Some(incoming) = incoming else { return Ok(false) };
                    let value: Value = incoming.map_err(std::io::Error::other)?;
                    let id = value.get("id").cloned().unwrap_or(Value::Null);
                    match value.get("method").and_then(Value::as_str) {
                        Some("initialize") => emit(json!({"id":id,"result":client.as_ref().ok_or("client retired")?.initialize_response().raw()}))?,
                        Some("initialized") => {}, // public start already performed this handshake
                        Some("kdr/reconnect") => {
                            if !requests.is_empty() { return Err("reconnect requires settled requests".into()); }
                            let retiring = client.take().ok_or("client retired")?;
                            let stale = retiring.request_handle();
                            retiring.shutdown().await?;
                            let request: ClientRequest = serde_json::from_value(json!({"id":999999,"method":"thread/loaded/list","params":{}}))?;
                            let retired = stale.request(request).await.is_err();
                            client = Some(start(&cli, &overrides).await?);
                            emit(json!({"id":id,"result":{"shutdownJoined":true,"retiredHandleRejected":retired,
                                "initialize":client.as_ref().ok_or("client retired")?.initialize_response().raw()}}))?;
                        },
                        Some(_) => {
                            let request: ClientRequest = serde_json::from_value(value)?;
                            let handle = client.as_ref().ok_or("client retired")?.request_handle();
                            requests.spawn(async move {
                                match handle.request(request).await {
                                    Ok(Ok(result)) => json!({"id":id,"result":result}),
                                    Ok(Err(error)) => json!({"id":id,"error":error}),
                                    Err(error) => json!({"id":id,"error":{"code":-32000,"message":error.to_string()}}),
                                }
                            });
                        },
                        None => {
                            let id: RequestId = serde_json::from_value(id)?;
                            if let Some(error) = value.get("error") {
                                client.as_ref().ok_or("client retired")?.reject_server_request(id, serde_json::from_value(error.clone())?).await?;
                            } else {
                                client.as_ref().ok_or("client retired")?.resolve_server_request(id, value.get("result").cloned().ok_or("missing server response")?).await?;
                            }
                        },
                    }
                },
                event = client.as_mut().ok_or("client retired")?.next_event() => match event {
                    Some(InProcessServerEvent::ServerNotification(notification)) => emit(serde_json::to_value(notification)?)?,
                    Some(InProcessServerEvent::ServerRequest(request)) => emit(serde_json::to_value(request)?)?,
                    Some(InProcessServerEvent::Lagged { .. }) => return Err("finite in-process observation lost events".into()),
                    None => return Err("in-process event stream ended before shutdown".into()),
                },
                result = requests.join_next(), if !requests.is_empty() => {
                    emit(result.ok_or("missing request owner")??)?;
                },
            }
            Ok(true)
        }.await;
        match result {
            Ok(true) => {}
            Ok(false) => break,
            Err(error) => {
                failure = Some(error);
                break;
            }
        }
    }
    // Always use the production shutdown barrier, including on a failed observation.
    if let Some(client) = client {
        client.shutdown().await?;
    }
    while let Some(result) = requests.join_next().await {
        emit(result?)?;
    }
    drop(input_rx);
    // On errors stdin can still be open; process ownership, not a blocking join,
    // bounds the failed observation. Normal EOF proves the reader has finished.
    if failure.is_none() {
        input.join().map_err(|_| "input reader panicked")?;
    }
    match failure {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn main() -> Result<(), Failure> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(16 * 1024 * 1024)
        .build()?
        .block_on(run())
}
