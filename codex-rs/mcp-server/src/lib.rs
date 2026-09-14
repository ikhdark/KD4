//! Prototype MCP server.
#![deny(clippy::print_stdout, clippy::print_stderr)]

use std::io::BufRead;
use std::io::ErrorKind;
use std::io::Result as IoResult;
use std::sync::Arc;

use codex_arg0::Arg0DispatchPaths;
use codex_core::config::ConfigBuilder;
use codex_core::resolve_installation_id;
use codex_exec_server::EnvironmentManager;
use codex_exec_server::ExecServerRuntimePaths;
use codex_login::default_client::set_default_client_residency_requirement;
use codex_utils_cli::CliConfigOverrides;

use rmcp::model::ClientNotification;
use rmcp::model::ClientRequest;
use rmcp::model::JsonRpcMessage;
use serde_json::Value;
use std::io::Write;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tracing::debug;
use tracing::info;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::prelude::*;

mod approval_response;
mod codex_tool_config;
mod codex_tool_runner;
mod exec_approval;
pub(crate) mod message_processor;
mod outgoing_message;
mod patch_approval;

use crate::message_processor::MessageProcessor;
use crate::outgoing_message::OutgoingError;
use crate::outgoing_message::OutgoingJsonRpcMessage;
use crate::outgoing_message::OutgoingMessage;
use crate::outgoing_message::OutgoingMessageSender;

pub use crate::codex_tool_config::CodexToolCallParam;
pub use crate::codex_tool_config::CodexToolCallReplyParam;
pub use crate::exec_approval::ExecApprovalElicitRequestParams;
pub use crate::exec_approval::ExecApprovalResponse;
pub use crate::patch_approval::PatchApprovalElicitRequestParams;
pub use crate::patch_approval::PatchApprovalResponse;

/// Size of the bounded channels used to communicate between tasks. The value
/// is a balance between throughput and memory usage – 128 messages should be
/// plenty for an interactive CLI.
const CHANNEL_CAPACITY: usize = 128;
const DEFAULT_ANALYTICS_ENABLED: bool = true;
const OTEL_SERVICE_NAME: &str = "codex_mcp_server";

type IncomingMessage = JsonRpcMessage<ClientRequest, Value, ClientNotification>;

fn spawn_stdin_line_reader(
    input_closed: CancellationToken,
) -> IoResult<mpsc::Receiver<IoResult<String>>> {
    // Tokio's stdin reader uses an uncancellable blocking read that runtime shutdown waits for.
    // Keep that read on a detached OS thread so closing the async receiver lets this server and its
    // runtime finish even when the client deliberately leaves stdin open.
    let (line_tx, line_rx) = mpsc::channel(CHANNEL_CAPACITY);
    std::thread::Builder::new()
        .name("codex-mcp-server-stdin".to_string())
        .spawn(move || {
            let _closed_guard = input_closed.drop_guard();
            let stdin = std::io::stdin();
            let mut stdin = stdin.lock();
            loop {
                let mut line = String::new();
                match stdin.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        while matches!(line.as_bytes().last(), Some(b'\n' | b'\r')) {
                            line.pop();
                        }
                        if line_tx.blocking_send(Ok(line)).is_err() {
                            break;
                        }
                    }
                    Err(err) => {
                        let _ = line_tx.blocking_send(Err(err));
                        break;
                    }
                }
            }
        })?;
    Ok(line_rx)
}

fn write_outgoing_messages<W: Write>(
    mut outgoing_rx: mpsc::Receiver<OutgoingMessage>,
    mut stdout: W,
) -> IoResult<()> {
    while let Some(outgoing_message) = outgoing_rx.blocking_recv() {
        let msg: OutgoingJsonRpcMessage = outgoing_message.into();
        let mut json = serde_json::to_vec(&msg)?;
        json.push(b'\n');
        stdout.write_all(&json)?;
        stdout.flush()?;
    }
    Ok(())
}

fn spawn_stdout_writer(
    outgoing_rx: mpsc::Receiver<OutgoingMessage>,
    output_failed: CancellationToken,
) -> IoResult<oneshot::Receiver<IoResult<()>>> {
    let (done_tx, done_rx) = oneshot::channel();
    // Like stdin, a blocked stdout write must not keep Tokio's blocking pool
    // alive after the bounded final drain. The process owns this detached thread.
    std::thread::Builder::new()
        .name("codex-mcp-server-stdout".to_string())
        .spawn(move || {
            let _failure_guard = output_failed.drop_guard();
            let result = write_outgoing_messages(outgoing_rx, std::io::stdout().lock());
            let _ = done_tx.send(result);
        })?;
    Ok(done_rx)
}

fn decode_incoming_message(line: &str) -> Result<IncomingMessage, OutgoingError> {
    serde_json::from_str(line).map_err(|error| {
        // Successful frames are parsed once. Only failed frames need inspection
        // to distinguish invalid JSON from an invalid request and recover its ID.
        let (id, code) = match serde_json::from_str::<Value>(line) {
            Ok(value) => (
                value
                    .get("id")
                    .and_then(|id| serde_json::from_value(id.clone()).ok()),
                rmcp::model::ErrorCode::INVALID_REQUEST,
            ),
            Err(_) => (None, rmcp::model::ErrorCode::PARSE_ERROR),
        };
        OutgoingError {
            id,
            error: rmcp::model::ErrorData::new(code, error.to_string(), None),
        }
    })
}

pub async fn run_main(
    arg0_paths: Arg0DispatchPaths,
    cli_config_overrides: CliConfigOverrides,
    strict_config: bool,
) -> IoResult<()> {
    // Parse CLI overrides once and derive the base Config eagerly so later
    // components do not need to work with raw TOML values.
    let cli_kv_overrides = cli_config_overrides.parse_overrides().map_err(|e| {
        std::io::Error::new(
            ErrorKind::InvalidInput,
            format!("error parsing -c overrides: {e}"),
        )
    })?;
    let config = ConfigBuilder::default()
        .cli_overrides(cli_kv_overrides)
        .strict_config(strict_config)
        .build()
        .await
        .map_err(|e| {
            std::io::Error::new(ErrorKind::InvalidData, format!("error loading config: {e}"))
        })?;
    set_default_client_residency_requirement(config.enforce_residency.value());
    let otel = codex_core::otel_init::build_provider(
        &config,
        env!("CARGO_PKG_VERSION"),
        Some(OTEL_SERVICE_NAME),
        DEFAULT_ANALYTICS_ENABLED,
    )
    .map_err(|e| {
        std::io::Error::new(
            ErrorKind::InvalidData,
            format!("error loading otel config: {e}"),
        )
    })?;
    codex_core::otel_init::record_process_start(otel.as_ref(), OTEL_SERVICE_NAME);
    codex_core::otel_init::install_sqlite_telemetry(otel.as_ref(), OTEL_SERVICE_NAME);
    let state_db = codex_core::init_state_db(&config).await;
    let environment_manager = Arc::new(
        EnvironmentManager::from_codex_home(
            config.codex_home.clone(),
            Some(ExecServerRuntimePaths::from_optional_path(
                arg0_paths.codex_self_exe.clone(),
            )?),
        )
        .await
        .map_err(std::io::Error::other)?,
    );

    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_filter(EnvFilter::from_default_env());
    let otel_logger_layer = otel.as_ref().and_then(|provider| provider.logger_layer());
    let otel_tracing_layer = otel.as_ref().and_then(|provider| provider.tracing_layer());

    let _ = tracing_subscriber::registry()
        .with(fmt_layer)
        .with(otel_logger_layer)
        .with(otel_tracing_layer)
        .try_init();

    // Set up channels.
    let (incoming_tx, mut incoming_rx) = mpsc::channel::<IncomingMessage>(CHANNEL_CAPACITY);
    let (outgoing_tx, outgoing_rx) = mpsc::channel::<OutgoingMessage>(CHANNEL_CAPACITY);
    let installation_id = resolve_installation_id(&config.codex_home).await?;
    let output_failed = CancellationToken::new();
    let input_closed = CancellationToken::new();

    let stdout_writer_done = spawn_stdout_writer(outgoing_rx, output_failed.clone())?;
    let mut stdin_lines = spawn_stdin_line_reader(input_closed.clone())?;

    // Task: read from stdin, push to `incoming_tx`.
    let stdin_reader_handle = tokio::spawn({
        let output_failed = output_failed.clone();
        let outgoing_tx = outgoing_tx.clone();
        async move {
            loop {
                let line = tokio::select! {
                    biased;
                    _ = output_failed.cancelled() => break,
                    line = stdin_lines.recv() => line,
                };
                let Some(line) = line else {
                    break;
                };
                let line = match line {
                    Ok(line) => line,
                    Err(err) => {
                        return Err(err);
                    }
                };
                match decode_incoming_message(&line) {
                    Ok(msg) => {
                        let sent = tokio::select! {
                            biased;
                            _ = output_failed.cancelled() => false,
                            result = incoming_tx.send(msg) => result.is_ok(),
                        };
                        if !sent {
                            break;
                        }
                    }
                    Err(error) => {
                        tokio::select! {
                            biased;
                            _ = output_failed.cancelled() => break,
                            result = outgoing_tx.send(OutgoingMessage::Error(error)) => {
                                if result.is_err() { break; }
                            }
                        }
                    }
                }
            }

            debug!("stdin reader finished (EOF)");
            Ok::<(), std::io::Error>(())
        }
    });

    // Task: process incoming messages.
    let processor_handle = tokio::spawn({
        let outgoing_message_sender = OutgoingMessageSender::new(outgoing_tx);
        let output_failed = output_failed.clone();
        let mut processor = MessageProcessor::new(
            outgoing_message_sender,
            arg0_paths,
            Arc::new(config),
            environment_manager,
            state_db,
            installation_id,
        )
        .await;
        async move {
            let _shutdown_guard = output_failed.clone().drop_guard();
            loop {
                let msg = tokio::select! {
                    biased;
                    _ = output_failed.cancelled() => break,
                    msg = incoming_rx.recv() => msg,
                };
                let Some(msg) = msg else {
                    break;
                };
                tokio::select! {
                    biased;
                    _ = output_failed.cancelled() => break,
                    _ = async {
                        match msg {
                            JsonRpcMessage::Request(r) => processor.process_request(r).await,
                            JsonRpcMessage::Response(r) => processor.process_response(r).await,
                            JsonRpcMessage::Notification(n) => processor.process_notification(n).await,
                            JsonRpcMessage::Error(e) => processor.process_error(e).await,
                        }
                    } => {}
                }
            }

            processor.shutdown().await;
            info!("processor task exited (channel closed)");
        }
    });

    let reader_abort = stdin_reader_handle.abort_handle();
    let processor_abort = processor_handle.abort_handle();
    let finish = async {
        let (reader_result, processor_result) = tokio::join!(stdin_reader_handle, processor_handle);
        let writer_result = stdout_writer_done
            .await
            .map_err(|_| std::io::Error::other("stdout writer terminated without a result"))?;
        writer_result?;
        reader_result.map_err(std::io::Error::other)??;
        processor_result.map_err(std::io::Error::other)?;
        Ok(())
    };
    tokio::pin!(finish);
    tokio::select! {
        result = &mut finish => result,
        _ = input_closed.cancelled() => {
            // Start the deadline at the blocking reader's EOF, even if dispatch
            // is still waiting for space in the outgoing channel.
            match tokio::time::timeout(Duration::from_secs(20), &mut finish).await {
                Ok(result) => result,
                Err(_) => {
                    output_failed.cancel();
                    reader_abort.abort();
                    processor_abort.abort();
                    Err(std::io::Error::new(ErrorKind::TimedOut, "MCP transport did not drain during shutdown"))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_config::types::OtelExporterKind;
    use codex_core::config::ConfigBuilder;
    use pretty_assertions::assert_eq;
    use std::collections::HashMap;
    use tempfile::TempDir;

    #[tokio::test]
    async fn mcp_server_defaults_analytics_to_enabled() -> anyhow::Result<()> {
        let codex_home = TempDir::new()?;
        let mut config = ConfigBuilder::default()
            .codex_home(codex_home.path().to_path_buf())
            .build()
            .await?;
        config.otel.exporter = OtelExporterKind::None;
        config.otel.trace_exporter = OtelExporterKind::None;
        config.otel.metrics_exporter = OtelExporterKind::OtlpGrpc {
            endpoint: "http://localhost:4317".to_string(),
            headers: HashMap::new(),
            tls: None,
        };
        for (configured, expected_enabled) in [(None, true), (Some(false), false)] {
            config.analytics_enabled = configured;
            let provider = codex_core::otel_init::build_provider(
                &config,
                "0.0.0-test",
                Some(OTEL_SERVICE_NAME),
                DEFAULT_ANALYTICS_ENABLED,
            )
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
            assert_eq!(
                provider
                    .as_ref()
                    .and_then(|provider| provider.metrics())
                    .is_some(),
                expected_enabled,
            );
            if let Some(provider) = provider {
                provider.shutdown();
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn mcp_server_builds_otel_provider_with_logs_traces_and_metrics() -> anyhow::Result<()> {
        let codex_home = TempDir::new()?;
        let mut config = ConfigBuilder::default()
            .codex_home(codex_home.path().to_path_buf())
            .build()
            .await?;
        let exporter = OtelExporterKind::OtlpGrpc {
            endpoint: "http://localhost:4317".to_string(),
            headers: HashMap::new(),
            tls: None,
        };
        config.otel.exporter = exporter.clone();
        config.otel.trace_exporter = exporter.clone();
        config.otel.metrics_exporter = exporter;
        config.analytics_enabled = None;

        let provider = codex_core::otel_init::build_provider(
            &config,
            "0.0.0-test",
            Some(OTEL_SERVICE_NAME),
            DEFAULT_ANALYTICS_ENABLED,
        )
        .map_err(|err| anyhow::anyhow!(err.to_string()))?
        .expect("otel provider");

        assert!(provider.logger.is_some(), "expected log exporter");
        assert!(
            provider.tracer_provider.is_some(),
            "expected trace exporter"
        );
        assert!(provider.metrics().is_some(), "expected metrics exporter");
        provider.shutdown();

        Ok(())
    }
}
