use std::io::Read;
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use codex_utils_pty::SpawnedProcess;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

#[derive(Debug)]
pub struct StdioBridgeOutcome {
    pub child_exit_code: i32,
    pub delivery_errors: Vec<String>,
}

impl StdioBridgeOutcome {
    /// Keep the child's failure status; successful child exit cannot conceal
    /// incomplete or failed stdio delivery.
    pub fn exit_code(&self) -> i32 {
        if self.child_exit_code != 0 {
            self.child_exit_code
        } else if self.delivery_errors.is_empty() {
            0
        } else {
            1
        }
    }
}

/// Child completion and physical stdio delivery are independent outcomes.
pub async fn forward_sandbox_session_stdio(spawned: SpawnedProcess) -> StdioBridgeOutcome {
    let session = Arc::new(spawned.session);
    let tokio_runtime = tokio::runtime::Handle::current();
    // Give large or slow tail output a better chance to finish draining without
    // letting rare EOF issues hang the wrapper indefinitely.
    let output_drain_timeout = Duration::from_secs(5);
    // A helper thread watches our stdin. When the input source closes it, the
    // thread tells the main async code so we can also close stdin for the
    // sandboxed child process.
    let (stdin_eof_tx, mut stdin_eof_rx) = oneshot::channel();

    // Start background threads that copy stdin/stdout/stderr. We intentionally
    // do not keep their JoinHandles; dropping the handle does not stop the
    // thread, it just means we are not going to wait on it later.
    drop(spawn_input_forwarder(
        std::io::stdin(),
        session.writer_sender(),
        stdin_eof_tx,
    ));
    let (stdout_forwarder, stdout_forwarder_done_rx) =
        spawn_output_forwarder(tokio_runtime.clone(), spawned.stdout_rx, std::io::stdout());
    drop(stdout_forwarder);
    let (stderr_forwarder, stderr_forwarder_done_rx) =
        spawn_output_forwarder(tokio_runtime.clone(), spawned.stderr_rx, std::io::stderr());
    drop(stderr_forwarder);

    let mut delivery_errors = Vec::new();
    let mut stdin_done = false;
    let mut exit_rx = spawned.exit_rx;
    let interrupt = tokio::signal::ctrl_c();
    tokio::pin!(interrupt);
    let exit_code = loop {
        tokio::select! {
            biased;
            result = &mut stdin_eof_rx, if !stdin_done => {
                stdin_done = true;
                record_completion("stdin", result, &mut delivery_errors);
                session.close_stdin();
            }
            res = &mut exit_rx => break res.unwrap_or(-1),
            res = &mut interrupt => {
                if let Err(err) = res { delivery_errors.push(format!("signal handler: {err}")); }
                if let Err(err) = session.request_terminate() { delivery_errors.push(format!("terminate: {err}")); }
                break exit_rx.await.unwrap_or(-1);
            }
        }
    };

    let (stdout, stderr) = tokio::join!(
        tokio::time::timeout(output_drain_timeout, stdout_forwarder_done_rx),
        tokio::time::timeout(output_drain_timeout, stderr_forwarder_done_rx),
    );
    for (name, result) in [("stdout", stdout), ("stderr", stderr)] {
        match result {
            Ok(result) => record_completion(name, result, &mut delivery_errors),
            Err(_) => delivery_errors.push(format!(
                "{name} delivery incomplete: five-second drain timeout"
            )),
        }
    }
    // Stdin may legitimately stay open after child exit. Report a completed
    // read error, but do not wait for an interactive input source to close.
    if !stdin_done && let Ok(result) = stdin_eof_rx.try_recv() {
        record_completion("stdin", Ok(result), &mut delivery_errors);
    }
    for error in &delivery_errors {
        eprintln!("windows sandbox stdio: {error}");
    }
    StdioBridgeOutcome {
        child_exit_code: exit_code,
        delivery_errors,
    }
}

fn record_completion(
    name: &str,
    result: Result<std::io::Result<()>, oneshot::error::RecvError>,
    errors: &mut Vec<String>,
) {
    match result {
        Ok(Ok(())) => {}
        Ok(Err(err)) => errors.push(format!("{name} delivery failed: {err}")),
        Err(err) => errors.push(format!(
            "{name} forwarder stopped without a delivery receipt: {err}"
        )),
    }
}

fn spawn_input_forwarder<R>(
    mut input: R,
    writer_tx: mpsc::Sender<Vec<u8>>,
    stdin_eof_tx: oneshot::Sender<std::io::Result<()>>,
) -> std::thread::JoinHandle<()>
where
    R: Read + Send + 'static,
{
    const STDIN_FORWARD_CHUNK_SIZE: usize = 8 * 1024;
    std::thread::spawn(move || {
        let mut buffer = [0_u8; STDIN_FORWARD_CHUNK_SIZE];
        let result = loop {
            match input.read(&mut buffer) {
                Ok(0) => break Ok(()),
                Ok(n) => {
                    if writer_tx.blocking_send(buffer[..n].to_vec()).is_err() {
                        // Child input can close before exit is published. It is
                        // not an input-source read error and needs no replay.
                        break Ok(());
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(err) => {
                    break Err(err);
                }
            }
        };
        let _ = stdin_eof_tx.send(result);
    })
}

fn spawn_output_forwarder<W>(
    tokio_runtime: tokio::runtime::Handle,
    output_rx: mpsc::Receiver<Vec<u8>>,
    mut writer: W,
) -> (
    std::thread::JoinHandle<()>,
    oneshot::Receiver<std::io::Result<()>>,
)
where
    W: Write + Send + 'static,
{
    let (done_tx, done_rx) = oneshot::channel();
    // The sandbox session emits output on Tokio channels, but writing to the
    // caller's stdio is simplest from a dedicated blocking thread.
    let handle = std::thread::spawn(move || {
        let mut output_rx = output_rx;
        let result = (|| {
            while let Some(chunk) = tokio_runtime.block_on(output_rx.recv()) {
                writer.write_all(&chunk)?;
                writer.flush()?;
            }
            writer.flush()
        })();
        let _ = done_tx.send(result);
    });
    (handle, done_rx)
}

#[cfg(test)]
#[path = "stdio_bridge_tests.rs"]
mod tests;
