fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(%error, "code-mode host failed");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> anyhow::Result<()> {
    // A stalled parent must not block the single runtime thread on stderr.
    let (telemetry_writer, telemetry_guard) = host_telemetry_writer(std::io::stderr());
    // The appender guard's full-queue shutdown fallback prints to stdout.
    // This dedicated process owns one bounded worker for its entire lifetime;
    // leave its best-effort queue to process teardown rather than corrupt IPC.
    std::mem::forget(telemetry_guard);
    // Never invoke the default synchronous stderr panic hook. The lossy sink
    // also bounds panic reporting before supervised tasks can unwind.
    std::panic::set_hook(Box::new(|panic| tracing::error!(%panic, "code-mode host panicked")));
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_ansi(false)
        .with_writer(telemetry_writer)
        .try_init()
        .map_err(|error| anyhow::anyhow!("initialize code-mode host telemetry: {error}"))?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(codex_code_mode_host::run_stdio());
    // Tokio's standard streams can retain uncancellable blocking operations.
    // This dedicated child must also bound the final runtime teardown.
    runtime.shutdown_timeout(std::time::Duration::from_secs(1));
    result
}

fn host_telemetry_writer(
    writer: impl std::io::Write + Send + 'static,
) -> (
    tracing_appender::non_blocking::NonBlocking,
    tracing_appender::non_blocking::WorkerGuard,
) {
    tracing_appender::non_blocking::NonBlockingBuilder::default()
        .buffered_lines_limit(256)
        .lossy(true)
        .finish(writer)
}
