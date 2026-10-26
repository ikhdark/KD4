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

