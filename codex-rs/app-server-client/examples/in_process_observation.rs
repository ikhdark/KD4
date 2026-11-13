//! KDR's finite stdio adapter to the real public in-process client.
//! The diagnostic-only reconnect command is not a production app-server API.
use codex_app_server_client::InProcessAppServerClient;
use codex_app_server_client::InProcessClientStartArgs;
use codex_app_server_client::InProcessServerEvent;
use codex_app_server_protocol_types::ClientRequest;
use codex_app_server_protocol_types::InitializeParams;
use futures::FutureExt;
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
use std::collections::BTreeSet;
use std::io::BufRead;
use std::io::Read;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::task::JoinSet;

type Failure = Box<dyn Error + Send + Sync>;

fn error_observation(error: &(dyn Error + 'static)) -> Value {
    let mut sources = Vec::new();
    let mut next = error.source();
    while let Some(source) = next {
        sources.push(json!({"display": source.to_string(), "debug": format!("{source:?}")}));
        next = source.source();
    }
    json!({"display": error.to_string(), "debug": format!("{error:?}"), "sources": sources})
}

async fn remote_bundle_observation(params: Value, overrides: Vec<(String, toml::Value)>) -> Result<Value, Failure> {
    use codex_core_plugins::remote::RemotePluginServiceConfig;
    use codex_core_plugins::remote_bundle::{validate_remote_plugin_bundle, download_and_install_remote_plugin_bundle};
    let home = PathBuf::from(std::env::var_os("CODEX_HOME").ok_or("explicit private CODEX_HOME required")?).canonicalize()?;
    let url = params.get("url").and_then(Value::as_str).ok_or("url required")?;
    let bundle = match validate_remote_plugin_bundle("kdr-remote-id", "kdr", "kdr-plugin", Some("1.0.0"), Some(url), params.get("appManifest").cloned()) {
        Ok(bundle) => bundle,
        Err(error) => return Ok(json!({"validationError": error_observation(&error)})),
    };
    let debug = format!("{bundle:?}");
    let config = ConfigBuilder::default().codex_home(home.clone())
        .loader_overrides(observation_loader(&home)?)
        .cli_overrides(overrides).build().await?;
    let service = RemotePluginServiceConfig::new(config.chatgpt_base_url.clone(), config.http_client_factory());
    let operation = download_and_install_remote_plugin_bundle(home, bundle, service.http_clients());
    let result = if let Some(ms) = params.get("cancelAfterMs").and_then(Value::as_u64) {
        match tokio::time::timeout(std::time::Duration::from_millis(ms.min(60_000)), operation).await {
            Ok(result) => Some(result),
            Err(_) => None,
        }
    } else { Some(operation.await) };
    Ok(match result {
        None => json!({"bundleDebug": debug, "cancelled": true}),
        Some(Err(error)) => json!({"bundleDebug": debug, "cancelled": false, "installError": error_observation(&error)}),
        Some(Ok(result)) => json!({"bundleDebug": debug, "cancelled": false, "installedPath": result.installed_path.display().to_string(), "installDebug": format!("{result:?}")}),
    })
}

fn emit(output: &std::sync::mpsc::SyncSender<Value>, value: Value) -> Result<(), Failure> {
    output.try_send(value).map_err(|error| std::io::Error::new(
        std::io::ErrorKind::WouldBlock, format!("bounded observation stdout unavailable: {error}"),
    ))?;
    Ok(())
}

async fn start(
    cli: &PathBuf,
    overrides: &[(String, toml::Value)],
    initialize: &InitializeParams,
) -> Result<InProcessAppServerClient, Failure> {
    let home = PathBuf::from(
        std::env::var_os("CODEX_HOME").ok_or("explicit private CODEX_HOME required")?,
    );
    let config = ConfigBuilder::default()
        .codex_home(home.clone())
        .loader_overrides(observation_loader(&home)?)
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
    let capabilities = initialize.capabilities.clone().unwrap_or_default();
    let loader_overrides = observation_loader(&config.codex_home)?;
    Ok(InProcessAppServerClient::start(InProcessClientStartArgs {
        arg0_paths: Arg0DispatchPaths {
            codex_self_exe: Some(cli.clone()),
        },
        config: Arc::new(config),
        cli_overrides: overrides.to_vec(),
        loader_overrides,
        strict_config: false,
        cloud_config_bundle: CloudConfigBundleLoader::default(),
        feedback: CodexFeedback::new(),
        log_db: None,
        state_db,
        environment_manager: Arc::new(environments),
        config_warnings: Vec::new(),
        session_source: SessionSource::Exec,
        enable_codex_api_key_env: false,
        client_name: initialize.client_info.name.clone(),
        client_version: initialize.client_info.version.clone(),
        experimental_api: capabilities.experimental_api,
        mcp_server_openai_form_elicitation: capabilities.mcp_server_openai_form_elicitation,
        opt_out_notification_methods: capabilities.opt_out_notification_methods.unwrap_or_default(),
        channel_capacity: 8,
    })
    .await?)
}

async fn run() -> Result<(), Failure> {
    let mut arguments = std::env::args().skip(1);
    let first = arguments.next().ok_or("explicit matched Codex executable required")?;
    let cli = if first == "app-server" {
        if arguments.next().as_deref() != Some("--stdio") { return Err("stdio adapter required".into()); }
        PathBuf::from(std::env::var_os("CODEX_OBSERVATION_CLI").ok_or("explicit matched CLI environment required")?)
    } else { PathBuf::from(first) }.canonicalize()?;
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
    let mut client: Option<InProcessAppServerClient> = None;
    let mut initialize_params: Option<InitializeParams> = None;
    let (output_tx, output_rx) = std::sync::mpsc::sync_channel::<Value>(64);
    let (output_done_tx, mut output_done) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let result: Result<(), Failure> = (|| {
            let mut output = std::io::stdout().lock();
            while let Ok(value) = output_rx.recv() {
                serde_json::to_writer(&mut output, &value)?;
                output.write_all(b"\n")?;
                output.flush()?;
            }
            Ok(())
        })();
        let _ = output_done_tx.send(result);
    });
    let mut output_finished = false;
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
    let mut cleanup_failures = Vec::new();
    let mut input_closed = false;
    let mut eof_deadline = None;
    let mut started_threads = BTreeSet::new();
    let mut pending_thread_starts = BTreeSet::new();
    loop {
        let result: Result<bool, Failure> = async {
            tokio::select! {
                output = &mut output_done => {
                    output_finished = true;
                    output.map_err(|_| "stdout owner dropped")??;
                    return Err("stdout owner ended before admission closed".into());
                }
                _ = async {
                    match eof_deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending().await,
                    }
                }, if input_closed => return Err("admitted EOF request/event drain exceeded five seconds".into()),
                incoming = input_rx.recv(), if !input_closed => {
                    let Some(incoming) = incoming else {
                        input_closed = true;
                        eof_deadline = Some(tokio::time::Instant::now() + std::time::Duration::from_secs(5));
                        return Ok(true)
                    };
                    let value: Value = incoming.map_err(std::io::Error::other)?;
                    let id = value.get("id").cloned().unwrap_or(Value::Null);
                    match value.get("method").and_then(Value::as_str) {
                        Some("initialize") => {
                            let params = serde_json::from_value::<InitializeParams>(value.get("params").cloned().unwrap_or(Value::Null));
                            match params {
                                Ok(params) if !params.capabilities.as_ref().is_some_and(|caps| caps.request_attestation)
                                    && params.client_info.title.is_none()
                                    && initialize_params.as_ref().is_none_or(|prior| prior == &params) => {
                                    if client.is_none() { client = Some(start(&cli, &overrides, &params).await?); }
                                    initialize_params = Some(params);
                                    emit(&output_tx, json!({"id":id,"result":client.as_ref().ok_or("client retired")?.initialize_response().raw()}))?;
                                }
                                _ => emit(&output_tx, json!({"id":id,"error":{"code":-32602,"message":"invalid, unsupported, or changed initialize parameters"}}))?,
                            }
                        },
                        Some("initialized") => {}, // public start already performed this handshake
                        Some("kdr/remotePluginBundle") => {
                            if requests.len() >= 64 { return Err("observation request capacity reached".into()); }
                            let params = value.get("params").cloned().unwrap_or(Value::Null);
                            let overrides = overrides.clone();
                            requests.spawn(async move {
                                let response = match remote_bundle_observation(params, overrides).await {
                                    Ok(result) => json!({"id": id, "result": result}),
                                    Err(error) => json!({"id": id, "error": {"code": -32000, "message": error.to_string()}}),
                                };
                                (response, false)
                            });
                        },
                        Some("kdr/reconnect") => {
                            if !requests.is_empty() { return Err("reconnect requires settled requests".into()); }
                            let retiring = client.take().ok_or("client retired")?;
                            let stale = retiring.request_handle();
                            retiring.shutdown_until_complete().await?;
                            let request: ClientRequest = serde_json::from_value(json!({"id":999999,"method":"thread/loaded/list","params":{}}))?;
                            let retired = stale.request(request).await.is_err();
                            client = Some(start(&cli, &overrides, initialize_params.as_ref().ok_or("initialize required")?).await?);
                            emit(&output_tx, json!({"id":id,"result":{"shutdownJoined":true,"retiredHandleRejected":retired,
                                "initialize":client.as_ref().ok_or("client retired")?.initialize_response().raw()}}))?;
                        },
                        Some(_) => {
                            if requests.len() >= 64 {
                                emit(&output_tx, json!({"id":id,"error":codex_app_server_protocol_types::overloaded_error(
                                    codex_app_server_protocol_types::OverloadReason::InFlightTaskCapacity,
                                    "observation adapter request capacity reached",
                                )}))?;
                                return Ok(true);
                            }
                            let request: ClientRequest = serde_json::from_value(value)?;
                            let starts_thread = request.method_name() == "thread/start";
                            let handle = client.as_ref().ok_or("client retired")?.request_handle();
                            requests.spawn(async move {
                                let response = match handle.request(request).await {
                                    Ok(Ok(result)) => json!({"id":id,"result":result}),
                                    Ok(Err(error)) => json!({"id":id,"error":error}),
                                    Err(error) => json!({"id":id,"error":{"code":-32000,"message":error.to_string()}}),
                                };
                                (response, starts_thread)
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
                event = async { match client.as_mut() {
                    Some(client) => client.next_event().await,
                    None => std::future::pending().await,
                }} => match event {
                    Some(InProcessServerEvent::ServerNotification(notification)) => {
                        let value = serde_json::to_value(notification)?;
                        if value["method"] == "thread/started"
                            && let Some(thread) = value["params"]["thread"]["id"].as_str()
                            && !pending_thread_starts.remove(thread)
                        {
                            started_threads.insert(thread.to_owned());
                        }
                        emit(&output_tx, value)?;
                    },
                    Some(InProcessServerEvent::ServerRequest(request)) => {
                        if input_closed {
                            client.as_ref().ok_or("client retired")?.reject_server_request(request.id().clone(),
                                codex_app_server_protocol_types::JSONRPCErrorError { code: -32000, message: "observation input closed".into(), data: None }).await?;
                        } else { emit(&output_tx, serde_json::to_value(request)?)?; }
                    },
                    Some(InProcessServerEvent::Lagged { .. }) => return Err("finite in-process observation lost events".into()),
                    None => return Err("in-process event stream ended before shutdown".into()),
                },
                result = requests.join_next(), if !requests.is_empty() => {
                    let (response, starts_thread) = result.ok_or("missing request owner")??;
                    if starts_thread && let Some(thread) = response["result"]["thread"]["id"].as_str()
                        && !started_threads.remove(thread)
                    {
                        pending_thread_starts.insert(thread.to_owned());
                    }
                    emit(&output_tx, response)?;
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
        if input_closed && requests.is_empty() && pending_thread_starts.is_empty() {
            if let Some(client) = client.as_mut() {
                // Drain already admitted final events before explicit retirement.
                while let Some(Some(event)) = client.next_event().now_or_never() {
                    let result = match event {
                        InProcessServerEvent::ServerNotification(notification) => serde_json::to_value(notification)
                            .map_err(|error| -> Failure { error.into() }).and_then(|value| emit(&output_tx, value)),
                        InProcessServerEvent::ServerRequest(request) => client.reject_server_request(request.id().clone(),
                            codex_app_server_protocol_types::JSONRPCErrorError { code: -32000, message: "observation input closed".into(), data: None }).await.map_err(Into::into),
                        InProcessServerEvent::Lagged { .. } => Err("finite in-process observation lost events".into()),
                    };
                    if let Err(error) = result { failure = Some(error); break; }
                }
            }
            break;
        }
    }
    // Always use the production shutdown barrier, including on a failed observation.
    if let Some(client) = client
        && let Err(error) = client.shutdown_until_complete().await
    {
        if failure.is_none() { failure = Some(error.into()); }
        else { cleanup_failures.push(error.to_string()); }
    }
    while let Some(result) = requests.join_next().await {
        let result = result.map_err(|error| -> Failure { error.into() }).and_then(|(value, _)| emit(&output_tx, value));
        if let Err(error) = result {
            if failure.is_none() { failure = Some(error); }
            else { cleanup_failures.push(error.to_string()); }
        }
    }
    drop(output_tx);
    if !output_finished {
        let result: Result<(), Failure> = match tokio::time::timeout(std::time::Duration::from_secs(5), &mut output_done).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("stdout owner dropped".into()),
            Err(_) => Err("stdout owner remained blocked after cleanup".into()),
        };
        if let Err(error) = result {
            if failure.is_none() { failure = Some(error); }
            else { cleanup_failures.push(error.to_string()); }
        }
    }
    drop(input_rx);
    // On errors stdin can still be open; process ownership, not a blocking join,
    // bounds the failed observation. Normal EOF proves the reader has finished.
    if failure.is_none() {
        if input.join().is_err() { failure = Some("input reader panicked".into()); }
    }
    match failure {
        Some(error) if !cleanup_failures.is_empty() => Err(std::io::Error::other(format!(
            "{error}; secondary cleanup failures: {}", cleanup_failures.join("; "),
        )).into()),
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// Private diagnostic inputs only. Never redirects machine-managed files or
/// changes the production loader's authority or fallback behavior.
fn observation_loader(home: &std::path::Path) -> Result<LoaderOverrides, Failure> {
    let home = home.canonicalize()?;
    let mut loader = LoaderOverrides::default();
    for (name, slot) in [
        ("CODEX_OBSERVATION_REQUIREMENTS", &mut loader.system_requirements_path),
        ("CODEX_OBSERVATION_MANAGED", &mut loader.managed_config_path),
    ] {
        if let Some(path) = std::env::var_os(name) {
            let path = PathBuf::from(path).canonicalize()?;
            if !path.starts_with(&home) || !path.is_file() { return Err("observation input must be a regular private-home file".into()); }
            *slot = Some(path);
        }
    }
    Ok(loader)
}

fn main() -> Result<(), Failure> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(16 * 1024 * 1024)
        .build()?
        .block_on(run())
}
