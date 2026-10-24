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

#[cfg(test)]
mod tests {
    use super::host_telemetry_writer;
    use std::io::Write;
    use std::sync::mpsc;
    use std::time::Duration;

    struct BlockedWriter(mpsc::Receiver<()>);

    impl Write for BlockedWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let _ = self.0.recv();
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn blocked_stderr_drops_diagnostics_instead_of_blocking_the_host() {
        let (release, blocked) = mpsc::channel();
        let (mut writer, guard) = host_telemetry_writer(BlockedWriter(blocked));
        let errors = writer.error_counter();
        let (done, finished) = mpsc::channel();
        let producer = std::thread::spawn(move || {
            for _ in 0..1024 {
                writer.write_all(b"observation\n").unwrap();
            }
            done.send(()).unwrap();
        });
        let outcome = finished.recv_timeout(Duration::from_secs(2));
        // Always release the sink before asserting, including on failure.
        drop(release);
        producer.join().unwrap();
        assert!(outcome.is_ok());
        assert!(errors.dropped_lines() > 0);
        drop(guard);
    }
}
