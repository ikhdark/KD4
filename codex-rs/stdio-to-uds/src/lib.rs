#![deny(clippy::print_stdout)]

use std::io;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Context;
use codex_uds::UnixStream;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

const CHUNK_BYTES: usize = 8192;
const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
static STDIO_CLAIMED: AtomicBool = AtomicBool::new(false);

/// Connects to the Unix Domain Socket at `socket_path` and relays data between
/// standard input/output and the socket.
///
/// Process-scoped, single use: cancellation may leave at most one detached native
/// stdin/stdout worker blocked until process exit. No Tokio blocking-pool work is
/// retained. Peer read EOF ends the session even when stdin remains open; stdin
/// EOF only half-closes uploads. Failed uploads drain output for up to five
/// seconds, and a stalled native output operation has the same finite bound.
pub async fn run(socket_path: &Path) -> anyhow::Result<()> {
    let stream = UnixStream::connect(socket_path)
        .await
        .with_context(|| format!("failed to connect to socket at {}", socket_path.display()))?;
    anyhow::ensure!(
        !STDIO_CLAIMED.swap(true, Ordering::AcqRel),
        "stdio bridge may run only once per process"
    );
    let input = native_input(std::io::stdin())?;
    let output = native_output(std::io::stdout())?;
    relay(stream, input, output).await
}

fn native_input<R: Read + Send + 'static>(
    mut input: R,
) -> io::Result<mpsc::Receiver<io::Result<Vec<u8>>>> {
    let (tx, rx) = mpsc::channel(1);
    std::thread::Builder::new()
        .name("codex-uds-stdin".into())
        .spawn(move || {
            let mut bytes = [0; CHUNK_BYTES];
            loop {
                let result = match input.read(&mut bytes) {
                    Ok(0) => return,
                    Ok(n) => Ok(bytes[..n].to_vec()),
                    Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                    Err(err) => Err(err),
                };
                let failed = result.is_err();
                if tx.blocking_send(result).is_err() || failed {
                    return;
                }
            }
        })?;
    Ok(rx)
}

type OutputOperation = (Vec<u8>, oneshot::Sender<io::Result<()>>);

fn native_output<W: Write + Send + 'static>(
    mut output: W,
) -> io::Result<std::sync::mpsc::SyncSender<OutputOperation>> {
    let (tx, rx) = std::sync::mpsc::sync_channel::<OutputOperation>(1);
    std::thread::Builder::new()
        .name("codex-uds-stdout".into())
        .spawn(move || {
            while let Ok((bytes, completion)) = rx.recv() {
                let result = output.write_all(&bytes).and_then(|()| output.flush());
                let failed = result.is_err();
                let _ = completion.send(result);
                if failed {
                    break;
                }
            }
        })?;
    Ok(tx)
}

async fn relay<S: AsyncRead + AsyncWrite + Unpin>(
    stream: S,
    mut input: mpsc::Receiver<io::Result<Vec<u8>>>,
    output: std::sync::mpsc::SyncSender<OutputOperation>,
) -> anyhow::Result<()> {
    let (mut socket_reader, mut socket_writer) = tokio::io::split(stream);
    let upload = async move {
        let input_result = async {
            while let Some(bytes) = input.recv().await {
                socket_writer
                    .write_all(&bytes?)
                    .await
                    .context("failed to copy stdin to socket")?;
            }
            anyhow::Ok(())
        }
        .await;

        (input_result, socket_writer)
    };

    let download = async {
        let mut bytes = [0; CHUNK_BYTES];
        loop {
            let n = socket_reader.read(&mut bytes).await?;
            let (completion, receipt) = oneshot::channel();
            output
                .try_send((bytes[..n].to_vec(), completion))
                .map_err(|_| {
                    io::Error::new(io::ErrorKind::BrokenPipe, "native stdout worker stopped")
                })?;
            tokio::time::timeout(DRAIN_TIMEOUT, receipt)
                .await
                .context("stdout delivery incomplete: native write/flush timed out")?
                .context("native stdout worker stopped before delivery")??;
            if n == 0 {
                return anyhow::Ok(());
            }
        }
    };
    tokio::pin!(upload, download);
    tokio::select! {
        biased;
        // Preserve an input failure already ready when output EOF arrives.
        (result, mut writer) = &mut upload => match result {
            Ok(()) => {
                shutdown_upload(&mut writer).await?;
                download.await
            },
            Err(input_error) => {
                // Keep the already-observed input error outside shutdown's
                // await, so simultaneous output EOF cannot turn it into success.
                let finish = async {
                    let shutdown = shutdown_upload(&mut writer).await;
                    let delivered = download.await;
                    shutdown.and(delivered)
                };
                match tokio::time::timeout(DRAIN_TIMEOUT, finish).await {
                    Ok(Ok(())) => Err(input_error),
                    Ok(Err(output_error)) => Err(input_error.context(format!("output delivery also failed: {output_error:#}"))),
                    Err(_) => Err(input_error.context("output delivery incomplete: drain timed out")),
                }
            }
        },
        // EOF plus acknowledged flush terminates without waiting for stdin.
        result = &mut download => result,
    }
}

async fn shutdown_upload<W: AsyncWrite + Unpin>(writer: &mut W) -> anyhow::Result<()> {
    match tokio::time::timeout(DRAIN_TIMEOUT, writer.shutdown()).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(err)) if err.kind() == io::ErrorKind::NotConnected => Ok(()),
        Ok(Err(err)) => Err(err).context("failed to shutdown socket writer"),
        Err(_) => anyhow::bail!("socket writer shutdown timed out"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn cancelled_blocked_native_output_does_not_retain_the_runtime() {
        struct BlockedOutput {
            started: std::sync::mpsc::Sender<()>,
            release: std::sync::mpsc::Receiver<()>,
        }
        impl Write for BlockedOutput {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                let _ = self.started.send(());
                let _ = self.release.recv();
                Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "fixture released",
                ))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (finished_tx, finished_rx) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let runtime = tokio::runtime::Runtime::new().unwrap();
            let cancelled = runtime.block_on(async {
                let (stream, mut peer) = tokio::io::duplex(128);
                peer.write_all(b"blocked output").await.unwrap();
                let (_held_input, rx) = mpsc::channel(1);
                let output = native_output(BlockedOutput {
                    started: started_tx,
                    release: release_rx,
                })
                .unwrap();
                tokio::time::timeout(Duration::from_millis(100), relay(stream, rx, output))
                    .await
                    .is_err()
            });
            drop(runtime);
            let _ = finished_tx.send(cancelled);
        });
        let started = started_rx.recv_timeout(Duration::from_secs(2));
        let finished = finished_rx.recv_timeout(Duration::from_secs(2));
        let _ = release_tx.send(());
        waiter.join().unwrap();
        started.expect("physical output write must start");
        assert!(finished.expect("runtime must drop independently of held native pipes"));
    }

    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl Write for Sink {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let n = bytes.len().min(97);
            self.0.lock().unwrap().extend_from_slice(&bytes[..n]);
            Ok(n)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn input_error_preserves_independent_final_output() {
        let (stream, mut peer) = tokio::io::duplex(8192);
        let (tx, rx) = mpsc::channel(1);
        let sink = Sink::default();
        let observed = sink.clone();
        let output = native_output(sink).unwrap();
        let expected: Vec<u8> = (0..65536).map(|n| (n % 256) as u8).collect();
        let bytes = expected.clone();
        let peer = async move {
            peer.write_all(&bytes).await.unwrap();
            peer.shutdown().await.unwrap();
        };
        // The error is known before the response completes: try_join would
        // cancel the opposite future rather than delivering this full oracle.
        tx.send(Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "injected input failure",
        )))
        .await
        .unwrap();
        let (result, ()) = tokio::join!(relay(stream, rx, output), peer);
        assert!(format!("{:#}", result.unwrap_err()).contains("injected input failure"));
        assert_eq!(*observed.0.lock().unwrap(), expected);
    }

    #[tokio::test]
    async fn input_error_half_closes_before_draining_eof_dependent_peer() {
        let (stream, mut peer) = tokio::io::duplex(128);
        let (tx, rx) = mpsc::channel(1);
        let sink = Sink::default();
        let observed = sink.clone();
        tx.send(Err(io::Error::other("injected input failure")))
            .await
            .unwrap();
        let peer = async move {
            let mut uploaded = Vec::new();
            peer.read_to_end(&mut uploaded).await.unwrap();
            assert!(uploaded.is_empty());
            peer.write_all(b"response after EOF").await.unwrap();
            peer.shutdown().await.unwrap();
        };
        let (result, ()) = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::join!(relay(stream, rx, native_output(sink).unwrap()), peer)
        })
        .await
        .expect("input failure must not withhold EOF until the drain timeout");
        assert!(format!("{:#}", result.unwrap_err()).contains("injected input failure"));
        assert_eq!(*observed.0.lock().unwrap(), b"response after EOF");
    }

    #[tokio::test]
    async fn peer_eof_does_not_wait_for_open_input() {
        let (stream, mut peer) = tokio::io::duplex(128);
        let (_held_input, rx) = mpsc::channel(1);
        let sink = Sink::default();
        let observed = sink.clone();
        peer.write_all(b"final bytes").await.unwrap();
        peer.shutdown().await.unwrap();
        tokio::time::timeout(
            Duration::from_secs(1),
            relay(stream, rx, native_output(sink).unwrap()),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(*observed.0.lock().unwrap(), b"final bytes");
    }
}
