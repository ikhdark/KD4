//! Shared implementation for `codex archive`, `codex delete`, and `codex unarchive`.
//!
//! The CLI commands are thin app-server clients: resolve a user-provided UUID or exact session
//! name, then call the corresponding app-server RPC.

use std::collections::HashSet;
use std::io::IsTerminal;
use std::io::Write;
use std::sync::Arc;

use crate::Cli;
use crate::app_server_session::AppServerSession;
use crate::legacy_core::config::ConfigBuilder;
use crate::legacy_core::config::ConfigOverrides;
use crate::legacy_core::config::find_codex_home_async;
use crate::legacy_core::config::load_config_toml_with_layer_stack;
use crate::legacy_core::config::resolve_bootstrap_auth_keyring_backend_kind;
use crate::legacy_core::config::resolve_bootstrap_auth_route_config;
use crate::legacy_core::config::resolve_oss_provider;
use crate::legacy_core::config::resolve_profile_v2_config_path;
use codex_app_server_protocol::Thread as AppServerThread;
use codex_app_server_protocol::ThreadListParams;
use codex_app_server_protocol::ThreadSortKey;
use codex_arg0::Arg0DispatchPaths;
use codex_cloud_config::cloud_config_bundle_loader_for_storage;
use codex_config::CloudConfigBundleLoader;
use codex_config::ConfigLoadOptions;
use codex_config::DEFAULT_CHATGPT_BASE_URL;
use codex_config::LoaderOverrides;
use codex_config::canonicalize_chatgpt_base_url;
use codex_exec_server::EnvironmentManager;
use codex_exec_server::ExecServerRuntimePaths;
use codex_protocol::ThreadId;
use codex_utils_cli::CliConfigOverrides;
use codex_utils_oss::get_default_model_for_oss_provider;
use color_eyre::eyre::Result;
use color_eyre::eyre::WrapErr;
use color_eyre::eyre::eyre;

use super::RemoteAppServerEndpoint;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteConfirmation {
    Prompt,
    Skip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionArchiveAction {
    Archive,
    Delete(DeleteConfirmation),
    Unarchive,
}

pub struct SessionArchiveCommandOptions {
    pub cli: Cli,
    pub arg0_paths: Arg0DispatchPaths,
    pub explicit_remote_endpoint: Option<RemoteAppServerEndpoint>,
}

fn success_message(
    action: SessionArchiveAction,
    session_id: ThreadId,
    session_name: Option<&str>,
) -> String {
    let action = match action {
        SessionArchiveAction::Archive => "Archived",
        SessionArchiveAction::Delete(_) => "Deleted",
        SessionArchiveAction::Unarchive => "Unarchived",
    };
    match session_name {
        Some(name) => format!("{action} session {name} ({session_id})."),
        None => format!("{action} session {session_id}."),
    }
}

struct ResolvedSessionTarget {
    session_id: ThreadId,
    session_name: Option<String>,
}

pub async fn run_session_archive_command(
    action: SessionArchiveAction,
    target: String,
    options: SessionArchiveCommandOptions,
) -> Result<String> {
    let mut app_server = start_app_server_for_archive_command(options).await?;
    run_session_archive_action_with_app_server(&mut app_server, action, &target).await
}

async fn run_session_archive_action_with_app_server(
    app_server: &mut AppServerSession,
    action: SessionArchiveAction,
    target: &str,
) -> Result<String> {
    let resolved = resolve_session_target(app_server, action, target).await?;
    let session_name = match action {
        SessionArchiveAction::Archive => {
            app_server.thread_archive(resolved.session_id).await?;
            resolved.session_name
        }
        SessionArchiveAction::Delete(confirmation) => {
            if matches!(confirmation, DeleteConfirmation::Prompt)
                && !confirm_session_delete(&resolved)?
            {
                return Ok("Delete cancelled.".to_string());
            }
            app_server.thread_delete(resolved.session_id).await?;
            resolved.session_name
        }
        SessionArchiveAction::Unarchive => {
            let thread = app_server.thread_unarchive(resolved.session_id).await?;
            thread.name.or(resolved.session_name)
        }
    };
    Ok(success_message(
        action,
        resolved.session_id,
        session_name.as_deref(),
    ))
}

async fn resolve_session_target(
    app_server: &mut AppServerSession,
    action: SessionArchiveAction,
    target: &str,
) -> Result<ResolvedSessionTarget> {
    if let Ok(session_id) = ThreadId::from_string(target) {
        if matches!(
            action,
            SessionArchiveAction::Delete(DeleteConfirmation::Prompt)
        ) {
            let thread = app_server
                .thread_read(session_id, /*include_turns*/ false)
                .await
                .with_context(|| {
                    format!("No active or archived session found matching '{target}'.")
                })?;
            return Ok(ResolvedSessionTarget {
                session_id,
                session_name: thread.name,
            });
        }
        return Ok(ResolvedSessionTarget {
            session_id,
            session_name: None,
        });
    }

    let (search_scope, archived_values): (&str, &[bool]) = match action {
        SessionArchiveAction::Archive => ("active", &[false]),
        SessionArchiveAction::Delete(_) => ("active or archived", &[false, true]),
        SessionArchiveAction::Unarchive => ("archived", &[true]),
    };
    for &archived in archived_values {
        if let Some(thread) = lookup_session_by_exact_name(app_server, target, archived).await? {
            return session_target_from_app_server_thread(thread);
        }
    }
    Err(eyre!(
        "No {search_scope} session found matching '{target}'."
    ))
}

async fn lookup_session_by_exact_name(
    app_server: &mut AppServerSession,
    name: &str,
    archived: bool,
) -> Result<Option<AppServerThread>> {
    // Search is the fast path, but some stores attach renamed titles after applying the filter.
    for search_term in [Some(name), None] {
        let mut cursor = None;
        let mut seen_cursors = HashSet::new();
        loop {
            let response = app_server
                .thread_list(ThreadListParams {
                    cursor: cursor.clone(),
                    limit: Some(100),
                    sort_key: Some(ThreadSortKey::UpdatedAt),
                    sort_direction: None,
                    model_providers: None,
                    source_kinds: Some(super::resume_source_kinds(
                        /*include_non_interactive*/ false,
                    )),
                    archived: Some(archived),
                    parent_thread_id: None,
                    ancestor_thread_id: None,
                    cwd: None,
                    use_state_db_only: None,
                    search_term: search_term.map(str::to_string),
                })
                .await
                .wrap_err("failed to list sessions while resolving session name")?;

            if let Some(thread) = response
                .data
                .into_iter()
                .find(|thread| thread.name.as_deref() == Some(name))
            {
                return Ok(Some(thread));
            }
            let Some(next_cursor) = response.next_cursor else {
                break;
            };
            if !seen_cursors.insert(next_cursor.clone()) {
                return Err(eyre!(
                    "app server repeated a pagination cursor while resolving session '{name}'"
                ));
            }
            cursor = Some(next_cursor);
        }
    }
    Ok(None)
}

fn session_target_from_app_server_thread(thread: AppServerThread) -> Result<ResolvedSessionTarget> {
    let session_id = ThreadId::from_string(&thread.id)
        .wrap_err_with(|| format!("app server returned invalid session id `{}`", thread.id))?;
    Ok(ResolvedSessionTarget {
        session_id,
        session_name: thread.name,
    })
}

fn confirm_session_delete(target: &ResolvedSessionTarget) -> Result<bool> {
    if !(std::io::stdin().is_terminal() && std::io::stderr().is_terminal()) {
        return Err(eyre!(
            "cannot confirm session deletion without an interactive terminal; rerun with --force and a session UUID"
        ));
    }

    let mut stderr = std::io::stderr().lock();
    match target.session_name.as_deref() {
        Some(name) => writeln!(
            stderr,
            "Permanently delete session '{name}' ({})?",
            target.session_id
        ),
        None => writeln!(stderr, "Permanently delete session {}?", target.session_id),
    }?;
    writeln!(
        stderr,
        "This cannot be undone. Subagent threads will also be deleted."
    )?;
    write!(stderr, "Continue? [y/N]: ")?;
    stderr.flush()?;

    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let answer = input.trim();
    Ok(answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes"))
}

async fn start_app_server_for_archive_command(
    options: SessionArchiveCommandOptions,
) -> Result<AppServerSession> {
    let SessionArchiveCommandOptions {
        cli,
        arg0_paths,
        explicit_remote_endpoint,
    } = options;
    let loader_overrides = LoaderOverrides::default();
    let strict_config = cli.strict_config;
    let raw_overrides = cli.config_overrides.raw_overrides.clone();
    let overrides_cli = CliConfigOverrides { raw_overrides };
    let cli_kv_overrides = overrides_cli
        .parse_overrides()
        .map_err(|err| eyre!("failed to parse -c overrides: {err}"))?;
    let codex_home = find_codex_home_async()
        .await
        .wrap_err("failed to find Codex home")?;

    let mut launch_loader_overrides = loader_overrides.clone();
    if let Some(profile_v2) = cli.config_profile_v2.as_ref() {
        launch_loader_overrides.user_config_path = Some(resolve_profile_v2_config_path(
            codex_home.as_path(),
            profile_v2,
        ));
        launch_loader_overrides.user_config_profile = Some(profile_v2.clone());
    }

    let reuse_implicit_local_daemon = super::can_reuse_implicit_local_daemon(
        &cli_kv_overrides,
        &launch_loader_overrides,
        strict_config,
        cli.bypass_hook_trust,
    );
    let default_daemon = if explicit_remote_endpoint.is_none() && reuse_implicit_local_daemon {
        super::maybe_probe_default_daemon_socket(codex_home.as_path()).await
    } else {
        None
    };
    let app_server_target = super::app_server_target_for_launch(
        explicit_remote_endpoint,
        default_daemon,
        reuse_implicit_local_daemon,
    );
    let remote_cwd_override = cli
        .cwd
        .clone()
        .filter(|_| app_server_target.uses_remote_workspace());

    let local_runtime_paths =
        ExecServerRuntimePaths::from_optional_path(arg0_paths.codex_self_exe.clone())
            .wrap_err("failed to resolve local runtime paths")?;
    let environment_manager = EnvironmentManager::from_env(Some(local_runtime_paths))
        .await
        .map(Arc::new)
        .wrap_err("failed to initialize environment manager")?;
    let config_cwd = super::config_cwd_for_app_server_target(
        cli.cwd.as_deref(),
        &app_server_target,
        &environment_manager,
    )
    .wrap_err("failed to resolve config cwd")?;

    let mut loader_overrides = loader_overrides;
    if let Some(profile_v2) = cli.config_profile_v2.as_ref() {
        loader_overrides.user_config_path = Some(resolve_profile_v2_config_path(
            codex_home.as_path(),
            profile_v2,
        ));
        loader_overrides.user_config_profile = Some(profile_v2.clone());
    }

    let bootstrap_config = load_config_toml_with_layer_stack(
        codex_home.as_path(),
        config_cwd.as_ref(),
        cli_kv_overrides.clone(),
        ConfigLoadOptions {
            loader_overrides: loader_overrides.clone(),
            strict_config,
            cloud_config_bundle: CloudConfigBundleLoader::default(),
        },
    )
    .await
    .wrap_err("failed to load config.toml")?;
    let config_toml = &bootstrap_config.config_toml;
    let chatgpt_base_url = canonicalize_chatgpt_base_url(
        config_toml
            .chatgpt_base_url
            .as_deref()
            .unwrap_or(DEFAULT_CHATGPT_BASE_URL),
    );
    let auth_route_config = resolve_bootstrap_auth_route_config(
        config_toml,
        bootstrap_config
            .config_layer_stack
            .requirements()
            .feature_requirements
            .as_ref(),
    )?;
    let cloud_config_bundle = cloud_config_bundle_loader_for_storage(
        codex_home.to_path_buf(),
        /*enable_codex_api_key_env*/ false,
        config_toml.cli_auth_credentials_store.unwrap_or_default(),
        resolve_bootstrap_auth_keyring_backend_kind(&bootstrap_config)?,
        chatgpt_base_url,
        auth_route_config,
    )
    .await;

    let model_provider = if cli.oss {
        resolve_oss_provider(cli.oss_provider.as_deref(), config_toml)
    } else {
        None
    };
    let model = cli.model.clone().or_else(|| {
        model_provider
            .as_deref()
            .and_then(get_default_model_for_oss_provider)
            .map(ToOwned::to_owned)
    });
    let cwd = cli.cwd.clone();
    let config = ConfigBuilder::default()
        .cli_overrides(cli_kv_overrides.clone())
        .harness_overrides(ConfigOverrides {
            model,
            cwd: if app_server_target.uses_remote_workspace() {
                None
            } else {
                cwd
            },
            model_provider,
            codex_self_exe: arg0_paths.codex_self_exe.clone(),
            show_raw_agent_reasoning: cli.oss.then_some(true),
            bypass_hook_trust: cli.bypass_hook_trust.then_some(true),
            ..Default::default()
        })
        .loader_overrides(loader_overrides.clone())
        .strict_config(strict_config)
        .cloud_config_bundle(cloud_config_bundle.clone())
        .build()
        .await
        .wrap_err("failed to load configuration")?;
    let state_db = super::init_state_db_for_app_server_target(&config, &app_server_target)
        .await
        .wrap_err("failed to initialize state database")?;
    let app_server = super::start_app_server(
        &app_server_target,
        arg0_paths,
        config,
        cli_kv_overrides,
        loader_overrides,
        strict_config,
        cloud_config_bundle,
        codex_feedback::CodexFeedback::new(),
        /*log_db*/ None,
        state_db,
        environment_manager,
    )
    .await?;
    Ok(
        AppServerSession::new(app_server, app_server_target.thread_params_mode())
            .with_remote_cwd_override(remote_cwd_override),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_app_server_client::AppServerClient;
    use codex_app_server_client::RemoteAppServerClient;
    use codex_app_server_client::RemoteAppServerConnectArgs;
    use futures::SinkExt;
    use futures::StreamExt;
    use std::time::Duration;
    use tokio_tungstenite::tungstenite::Message;

    #[tokio::test]
    async fn archive_name_pagination_rejects_cycles_without_mutations_and_drop_closes_remote() {
        let cases: &[(&[Option<&str>], bool)] = &[
            (&[Some("a"), Some("a")], true),
            (&[Some("a"), Some("b"), Some("a")], true),
            (&[Some("a"), None, Some("a"), None], false),
        ];
        for action in [
            SessionArchiveAction::Archive,
            SessionArchiveAction::Delete(DeleteConfirmation::Skip),
            SessionArchiveAction::Unarchive,
        ] {
            for &(next_cursors, cyclic) in cases {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let endpoint = format!("ws://{}", listener.local_addr().unwrap());
                let next_cursors: Vec<Option<String>> = next_cursors
                    .iter()
                    .map(|cursor| cursor.map(str::to_string))
                    .collect();
                let expected_lists = next_cursors.len();
                let peer = tokio::spawn(async move {
                    let (stream, _) = listener.accept().await.unwrap();
                    let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
                    let initialize = socket.next().await.unwrap().unwrap();
                    let initialize: serde_json::Value =
                        serde_json::from_str(initialize.to_text().unwrap()).unwrap();
                    assert_eq!(initialize["method"], "initialize");
                    socket
                        .send(Message::Text(
                            serde_json::json!({"id":initialize["id"],"result":{}})
                                .to_string()
                                .into(),
                        ))
                        .await
                        .unwrap();
                    let initialized = socket.next().await.unwrap().unwrap();
                    let initialized: serde_json::Value =
                        serde_json::from_str(initialized.to_text().unwrap()).unwrap();
                    assert_eq!(initialized["method"], "initialized");
                    let scopes = if !cyclic && matches!(action, SessionArchiveAction::Delete(_)) {
                        2
                    } else {
                        1
                    };
                    let mut request_count = 0;
                    for scope in 0..scopes {
                        let mut cursor: Option<String> = None;
                        let mut search_term = Some("missing-session");
                        for next_cursor in &next_cursors {
                            let request = socket.next().await.unwrap().unwrap();
                            let request: serde_json::Value =
                                serde_json::from_str(request.to_text().unwrap()).unwrap();
                            assert_eq!(
                                request["method"], "thread/list",
                                "lookup must not mutate sessions"
                            );
                            assert_eq!(request["params"]["cursor"], serde_json::json!(cursor));
                            assert_eq!(
                                request["params"]["searchTerm"],
                                serde_json::json!(search_term)
                            );
                            assert_eq!(
                                request["params"]["archived"],
                                matches!(action, SessionArchiveAction::Unarchive) || scope == 1
                            );
                            assert_eq!(request["params"]["limit"], 100);
                            request_count += 1;
                            socket.send(Message::Text(serde_json::json!({"id":request["id"],"result":{"data":[],"nextCursor":next_cursor}}).to_string().into())).await.unwrap();
                            cursor = next_cursor.clone();
                            if cursor.is_none() {
                                search_term = None;
                            }
                        }
                    }
                    let closed = tokio::time::timeout(Duration::from_secs(5), socket.next())
                        .await
                        .expect("dropping the session must close its remote connection")
                        .expect("close frame")
                        .expect("valid close frame");
                    assert!(
                        matches!(closed, Message::Close(_)),
                        "unexpected request after lookup: {closed:?}"
                    );
                    assert_eq!(request_count, expected_lists * scopes);
                });
                let client = RemoteAppServerClient::connect(RemoteAppServerConnectArgs {
                    endpoint: codex_app_server_client::RemoteAppServerEndpoint::WebSocket {
                        websocket_url: endpoint,
                        auth_token: None,
                    },
                    client_name: "archive-test".to_string(),
                    client_version: "test".to_string(),
                    experimental_api: true,
                    mcp_server_openai_form_elicitation: false,
                    opt_out_notification_methods: Vec::new(),
                    channel_capacity: 8,
                })
                .await
                .expect("connect archive client");
                let mut session = AppServerSession::new(
                    AppServerClient::Remote(client),
                    crate::app_server_session::ThreadParamsMode::Remote,
                );
                let error = tokio::time::timeout(
                    Duration::from_secs(5),
                    run_session_archive_action_with_app_server(
                        &mut session,
                        action,
                        "missing-session",
                    ),
                )
                .await
                .expect("name lookup must stop at cycle or finite end")
                .expect_err("no matching session should not mutate")
                .to_string();
                if cyclic {
                    assert!(error.contains("repeated a pagination cursor"), "{error}");
                } else {
                    assert!(
                        error.contains("No ") && error.contains("session found matching"),
                        "{error}"
                    );
                }
                assert!(error.contains("missing-session"));
                drop(session);
                tokio::time::timeout(Duration::from_secs(5), peer)
                    .await
                    .expect("peer finishes")
                    .expect("peer assertions");
            }
        }
    }

    #[test]
    fn home_resolution_normal_startup_and_archive_subprocess() -> Result<()> {
        use clap::Parser;
        use codex_app_server_protocol::ClientRequest;
        use codex_app_server_protocol::ConfigLayerSource;
        use codex_app_server_protocol::ConfigReadParams;
        use codex_app_server_protocol::ConfigReadResponse;
        use codex_app_server_protocol::RequestId;
        use std::path::PathBuf;
        use std::process::Command;

        const TEST_NAME: &str = "session_archive_commands::tests::home_resolution_normal_startup_and_archive_subprocess";
        const MODE: &str = "CODEX_TEST_HOME_BOUNDARY_MODE";
        const PROJECT: &str = "CODEX_TEST_HOME_BOUNDARY_PROJECT";
        const MODEL: &str = "home-routing-behavior-test-model";
        const CONFIG: &str = "# home routing sentinel\nmodel = \"home-routing-behavior-test-model\"\ncli_auth_credentials_store = \"file\"\n";
        const PASSED: &str = "HOME_BOUNDARY_CHILD_ASSERTIONS_PASSED";

        if let Ok(mode) = std::env::var(MODE) {
            let home = PathBuf::from(std::env::var_os("CODEX_HOME").expect("child HOME"));
            let project = PathBuf::from(std::env::var_os(PROJECT).expect("child project"));
            let mut cli = Cli::parse_from(["codex"]);
            cli.strict_config = true;
            cli.cwd = Some(project.clone());
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(Box::pin(async move {
                if mode.starts_with("startup-") {
                    let _ = crate::run_main(
                        cli,
                        Arg0DispatchPaths::default(),
                        LoaderOverrides::default(),
                        None,
                    )
                    .await;
                    panic!("invalid HOME must exit before TUI startup");
                }
                if mode == "archive-valid" {
                    let config_path = dunce::canonicalize(home.join("config.toml"))?;
                    use tracing_subscriber::prelude::*;
                    struct MetricName(Option<String>);
                    impl tracing::field::Visit for MetricName {
                        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                            if field.name() == "instrument_name" {
                                self.0 = Some(value.to_string());
                            }
                        }
                        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                            if field.name() == "instrument_name" {
                                self.0 = Some(format!("{value:?}").trim_matches('"').to_string());
                            }
                        }
                    }
                    struct SdkGate {
                        entered: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<std::thread::ThreadId>>>,
                        release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
                    }
                    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for SdkGate {
                        fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
                            if event.metadata().name() != "Metrics.InstrumentCreated" {
                                return;
                            }
                            let mut name = MetricName(None);
                            event.record(&mut name);
                            if name.0.as_deref() == Some(codex_state::DB_METRIC_BACKFILL)
                                && let Some(entered) = self.entered.lock().unwrap().take()
                            {
                                entered.send(std::thread::current().id()).unwrap();
                                self.release.lock().unwrap().recv_timeout(Duration::from_secs(5)).expect("main executor must release real SDK registration");
                            }
                        }
                    }
                    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
                    let (release_tx, release_rx) = std::sync::mpsc::channel();
                    tracing::subscriber::set_global_default(tracing_subscriber::registry().with(SdkGate {
                        entered: std::sync::Mutex::new(Some(entered_tx)),
                        release: std::sync::Mutex::new(release_rx),
                    }))?;
                    let _provider = codex_otel::OtelProvider::from(&codex_otel::OtelSettings {
                        environment: "test".to_string(),
                        service_name: "archive-backfill-test".to_string(),
                        service_version: "test".to_string(),
                        codex_home: home.clone(),
                        exporter: codex_otel::OtelExporter::None,
                        trace_exporter: codex_otel::OtelExporter::None,
                        metrics_exporter: codex_otel::OtelExporter::OtlpHttp {
                            endpoint: "http://127.0.0.1:9/metrics".to_string(),
                            headers: Default::default(),
                            protocol: codex_otel::OtelHttpProtocol::Json,
                            tls: None,
                        },
                        runtime_metrics: true,
                        span_attributes: Default::default(),
                        tracestate: Default::default(),
                    }).map_err(|error| color_eyre::eyre::eyre!("{error}"))?.expect("explicit metrics exporter");
                    let mut startup = Box::pin(start_app_server_for_archive_command(
                        SessionArchiveCommandOptions {
                            cli,
                            arg0_paths: Arg0DispatchPaths {
                                codex_self_exe: Some(codex_utils_cargo_bin::cargo_bin("codex")?),
                            },
                            explicit_remote_endpoint: None,
                        },
                    ));
                    let sdk_thread = tokio::select! {
                        entered = entered_rx => entered?,
                        result = &mut startup => panic!("archive startup must reach real cold backfill instrumentation first: {:?}", result.err()),
                    };
                    assert_ne!(sdk_thread, std::thread::current().id(), "SDK instrumentation must execute off the archive executor");
                    assert!(tokio::time::timeout(Duration::from_millis(20), &mut startup).await.is_err(), "startup must await admitted metric completion while the executor timer progresses");
                    release_tx.send(())?;
                    let session = startup.await?;
                    let response: ConfigReadResponse = session
                        .request_handle()
                        .request_typed(ClientRequest::ConfigRead {
                            request_id: RequestId::Integer(100),
                            params: ConfigReadParams {
                                include_layers: true,
                                cwd: Some(project.to_string_lossy().into_owned()),
                            },
                        })
                        .await?;
                    session.shutdown().await?;
                    assert_eq!(response.config.model.as_deref(), Some(MODEL));
                    let user_paths: Vec<_> = response
                        .layers
                        .expect("requested config layers")
                        .into_iter()
                        .filter_map(|layer| match layer.name {
                            ConfigLayerSource::User { file, .. } => Some(file),
                            _ => None,
                        })
                        .collect();
                    assert_eq!(user_paths.len(), 1, "one configured user layer");
                    assert_eq!(user_paths[0].as_path(), config_path.as_path());
                    assert_eq!(std::fs::read(&config_path)?, CONFIG.as_bytes());
                    let state_db = codex_state::StateRuntime::init(
                        PathBuf::from(std::env::var_os("CODEX_SQLITE_HOME").expect("child sqlite home")),
                        "openai".to_string(),
                    )
                    .await.expect("open completed archive state database");
                    assert_eq!(
                        state_db.get_backfill_state().await.expect("read completed archive backfill").status,
                        codex_state::BackfillStatus::Complete,
                        "normal archive startup must finish durable backfill before becoming usable",
                    );
                    state_db.close().await;
                    let snapshot = codex_otel::global().expect("normal global metrics provider").snapshot()?;
                    assert!(snapshot.scope_metrics().flat_map(|scope| scope.metrics()).any(|metric| metric.name() == codex_state::DB_METRIC_BACKFILL), "normal archive backfill must publish its actual SDK counter before startup completes");
                } else {
                    let detail = if mode == "archive-missing" {
                        "does not exist"
                    } else {
                        assert_eq!(mode, "archive-file");
                        "is not a directory"
                    };
                    let error = run_session_archive_command(
                        SessionArchiveAction::Archive,
                        "home-error-must-not-reach-archive".to_owned(),
                        SessionArchiveCommandOptions {
                            cli,
                            arg0_paths: Arg0DispatchPaths::default(),
                            explicit_remote_endpoint: None,
                        },
                    )
                    .await
                    .expect_err("invalid HOME must prevent archive startup");
                    assert_eq!(
                        format!("{error:#}"),
                        format!(
                            "failed to find Codex home: CODEX_HOME points to {:?}, but that path {detail}",
                            home.as_os_str()
                        )
                    );
                    if mode == "archive-missing" {
                        assert!(!home.exists(), "startup must not create invalid HOME");
                    } else {
                        assert_eq!(std::fs::read(&home)?, b"home is a file\n");
                    }
                }
                println!("{PASSED}");
                Ok::<(), color_eyre::eyre::Report>(())
            }))?;
            return Ok(());
        }

        let fixture = tempfile::tempdir()?;
        let project = fixture.path().join("project");
        std::fs::create_dir(&project)?;
        let missing = fixture.path().join("missing-home");
        let file = fixture.path().join("file-home");
        std::fs::write(&file, b"home is a file\n")?;
        let home = fixture.path().join("configured-home");
        std::fs::create_dir(&home)?;
        std::fs::write(home.join("config.toml"), CONFIG)?;
        // The input deliberately contains a dot segment; the exposed user layer must be canonical.
        let configured_home = home.join(".");
        for (mode, selected_home, detail) in [
            ("startup-missing", &missing, Some("does not exist")),
            ("startup-file", &file, Some("is not a directory")),
            ("archive-missing", &missing, None),
            ("archive-file", &file, None),
            ("archive-valid", &configured_home, None),
        ] {
            let output = Command::new(std::env::current_exe()?)
                .args(["--exact", TEST_NAME, "--nocapture", "--test-threads=1"])
                .env(MODE, mode)
                .env(PROJECT, &project)
                .env("CODEX_HOME", selected_home)
                .env("CODEX_SQLITE_HOME", fixture.path().join("sqlite"))
                .current_dir(&project)
                .output()?;
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            if let Some(detail) = detail {
                assert_eq!(output.status.code(), Some(1), "{mode}: {stdout}\n{stderr}");
                let home_error = format!(
                    "CODEX_HOME points to {:?}, but that path {detail}",
                    selected_home.as_os_str()
                );
                assert_eq!(
                    stderr.trim_end(),
                    format!(
                        "WARNING: proceeding, even though we could not create PATH aliases: {home_error}\nError finding codex home: {home_error}"
                    ),
                    "{mode}"
                );
                assert!(!stdout.contains(PASSED));
            } else {
                assert!(output.status.success(), "{mode}: {stdout}\n{stderr}");
                assert!(
                    stdout.contains(PASSED),
                    "child test must actually run: {stdout}"
                );
            }
            assert!(!missing.exists());
            assert_eq!(std::fs::read(&file)?, b"home is a file\n");
            assert_eq!(std::fs::read(home.join("config.toml"))?, CONFIG.as_bytes());
        }
        Ok(())
    }
}
