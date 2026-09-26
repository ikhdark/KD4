#![deny(clippy::print_stdout)]

use std::io;
use std::io::Read;
use std::path::Path;

use anyhow::Context;
use codex_uds::UnixStream;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

const STDIN_CHUNK_BYTES: usize = 8 * 1024;

/// Connects to the Unix Domain Socket at `socket_path` and relays data between
/// standard input/output and the socket.
///
/// The session ends when the peer closes its side and its output has been flushed to stdout,
/// even if stdin is still open. Stdin EOF only half-closes the socket so the peer's response can
/// still drain.
pub async fn run(socket_path: &Path) -> anyhow::Result<()> {
    let stream = UnixStream::connect(socket_path)
        .await
        .with_context(|| format!("failed to connect to socket at {}", socket_path.display()))?;
    let (mut socket_reader, mut socket_writer) = tokio::io::split(stream);
    let mut stdin_chunks = spawn_stdin_reader().context("failed to start stdin reader")?;

    let copy_socket_to_stdout = async {
        let mut stdout = tokio::io::stdout();
        tokio::io::copy(&mut socket_reader, &mut stdout)
            .await
            .context("failed to copy data from socket to stdout")?;
        stdout.flush().await.context("failed to flush stdout")?;
        anyhow::Ok(())
    };
    let copy_stdin_to_socket = async {
        while let Some(chunk) = stdin_chunks.recv().await {
            let chunk = chunk.context("failed to read data from stdin")?;
            socket_writer
                .write_all(&chunk)
                .await
                .context("failed to copy data from stdin to socket")?;
        }

        // The peer can close immediately after sending its response; in that
        // race, half-closing our write side can report NotConnected on some
        // platforms.
        if let Err(err) = socket_writer.shutdown().await
            && err.kind() != io::ErrorKind::NotConnected
        {
            return Err(err).context("failed to shutdown socket writer");
        }

        anyhow::Ok(())
    };

    tokio::pin!(copy_socket_to_stdout);
    let relayed = tokio::select! {
        result = &mut copy_socket_to_stdout => result,
        result = copy_stdin_to_socket => match result {
            Ok(()) => copy_socket_to_stdout.await,
            Err(err) => Err(err),
        },
    };
    relayed.context("failed to relay data between stdio and socket")
}

/// Reads stdin on a detached thread. Tokio's stdin reads on its blocking pool, and an idle read
/// there can neither be cancelled nor outlived by runtime shutdown, so a client that keeps stdin
/// open after the peer closed would keep this process alive indefinitely.
fn spawn_stdin_reader() -> io::Result<mpsc::Receiver<io::Result<Vec<u8>>>> {
    // A single queued chunk keeps stdin backpressured by the socket writer.
    let (chunk_tx, chunk_rx) = mpsc::channel(1);
    std::thread::Builder::new()
        .name("stdio-to-uds-stdin".to_string())
        .spawn(move || {
            let mut stdin = io::stdin().lock();
            let mut buffer = vec![0; STDIN_CHUNK_BYTES];
            loop {
                let chunk = match stdin.read(&mut buffer) {
                    Ok(0) => return,
                    Ok(len) => Ok(buffer[..len].to_vec()),
                    Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                    Err(err) => Err(err),
                };
                let failed = chunk.is_err();
                if chunk_tx.blocking_send(chunk).is_err() || failed {
                    return;
                }
            }
        })?;
    Ok(chunk_rx)
}
