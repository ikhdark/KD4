mod file_system_handler;
mod handler;
mod process_handler;
mod processor;
mod registry;
mod session_registry;
mod transport;

pub(crate) use handler::ExecServerHandler;
pub(crate) use processor::ConnectionProcessor;
pub use transport::DEFAULT_LISTEN_URL;
pub use transport::ExecServerListenUrlParseError;
pub use transport::EXEC_SERVER_AUTH_TOKEN_ENV_VAR;
pub(crate) use transport::loopback_authorization;

use crate::ExecServerRuntimePaths;
use crate::ExecServerTelemetry;

pub async fn run_main(
    listen_url: &str,
    runtime_paths: ExecServerRuntimePaths,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    run_main_with_telemetry(listen_url, runtime_paths, ExecServerTelemetry::default()).await
}

#[tracing::instrument(
    name = "codex.exec_server",
    skip_all,
    fields(otel.kind = "internal")
)]
pub async fn run_main_with_telemetry(
    listen_url: &str,
    runtime_paths: ExecServerRuntimePaths,
    telemetry: ExecServerTelemetry,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    transport::run_transport(listen_url, runtime_paths, telemetry).await
}

#[tracing::instrument(name = "codex.exec_server", skip_all, fields(otel.kind = "internal"))]
pub async fn run_main_with_shutdown(
    listen_url: &str,
    runtime_paths: ExecServerRuntimePaths,
    telemetry: ExecServerTelemetry,
    shutdown: crate::ExecServerShutdown,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    transport::run_transport_with_shutdown(listen_url, runtime_paths, telemetry, shutdown).await
}

