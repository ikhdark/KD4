use std::future::Future;

use tracing_subscriber::EnvFilter;
use tracing_subscriber::prelude::*;

const DEFAULT_ANALYTICS_ENABLED: bool = false;
const DEFAULT_LOG_FILTER: &str = "error,opentelemetry_sdk=off,opentelemetry_otlp=off";
const OTEL_SERVICE_NAME: &str = "codex-exec-server";

pub(crate) fn init(
    config: Option<&codex_core::config::Config>,
) -> (impl Send + Sync, codex_exec_server::ExecServerTelemetry) {
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_filter(stderr_env_filter());
    let otel = match config {
        Some(config) => codex_core::otel_init::build_provider(
            config,
            env!("CARGO_PKG_VERSION"),
            Some(OTEL_SERVICE_NAME),
            DEFAULT_ANALYTICS_ENABLED,
        )
        .unwrap_or_else(|error| {
            eprintln!("Could not create otel exporter: {error}");
            None
        }),
        None => None,
    };
    let provider = otel.as_ref();
    codex_core::otel_init::record_process_start(provider, OTEL_SERVICE_NAME);

    let otel_logger_layer = provider.and_then(|otel| otel.logger_layer());
    let otel_tracing_layer = provider.and_then(|otel| otel.tracing_layer());
    let telemetry = provider
        .and_then(|otel| otel.metrics())
        .cloned()
        .map(codex_exec_server::ExecServerTelemetry::new)
        .unwrap_or_default();
    let _ = tracing_subscriber::registry()
        .with(fmt_layer)
        .with(otel_tracing_layer)
        .with(otel_logger_layer)
        .try_init();
    tracing::callsite::rebuild_interest_cache();
    (otel, telemetry)
}

pub(crate) async fn run_until_shutdown<F, Fut, E>(run: F) -> Result<(), E>
where
    F: FnOnce(codex_exec_server::ExecServerShutdown) -> Fut,
    Fut: Future<Output = Result<(), E>> + Send + 'static,
    E: From<std::io::Error> + Send + 'static,
{
    let shutdown_signal = match shutdown_signal() {
        Ok(signal) => Some(signal),
        Err(error) => {
            eprintln!("Could not listen for exec-server shutdown signal: {error}");
            None
        }
    };
    let shutdown = codex_exec_server::ExecServerShutdown::new();
    // Service custody survives cancellation of the signal observer. A timeout
    // reports incomplete shutdown, never successful future-drop cleanup.
    let mut run = tokio::spawn(run(shutdown.clone()));

    if let Some(shutdown_signal) = shutdown_signal {
        tokio::select! {
            result = &mut run => result.map_err(|error| E::from(std::io::Error::other(error)))?,
            signal = wait_for_shutdown_signal(shutdown_signal) => {
                match signal {
                    Ok(()) => {
                        shutdown.cancel();
                        tokio::select! {
                            result = &mut run => result.map_err(|error| E::from(std::io::Error::other(error)))?,
                            _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => Err(std::io::Error::new(
                                std::io::ErrorKind::TimedOut, "exec-server cleanup remains owned and pending after shutdown deadline").into()),
                            _ = tokio::signal::ctrl_c() => Err(std::io::Error::new(
                                std::io::ErrorKind::Interrupted, "exec-server forced shutdown requested; cleanup is unconfirmed").into()),
                        }
                    }
                    Err(error) => {
                        eprintln!("Could not listen for exec-server shutdown signal: {error}");
                        run.await.map_err(|error| E::from(std::io::Error::other(error)))?
                    }
                }
            }
        }
    } else {
        run.await
            .map_err(|error| E::from(std::io::Error::other(error)))?
    }
}

struct ShutdownSignal;

fn shutdown_signal() -> std::io::Result<ShutdownSignal> {
    Ok(ShutdownSignal)
}

async fn wait_for_shutdown_signal(_: ShutdownSignal) -> std::io::Result<()> {
    tokio::signal::ctrl_c().await
}

fn stderr_env_filter() -> EnvFilter {
    EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(DEFAULT_LOG_FILTER))
        .unwrap_or_else(|_| EnvFilter::new("error"))
}
