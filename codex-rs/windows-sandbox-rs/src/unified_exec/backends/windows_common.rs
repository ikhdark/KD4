use crate::ipc_framed::EmptyPayload;
use crate::ipc_framed::FramedMessage;
use crate::ipc_framed::IPC_PROTOCOL_VERSION;
use crate::ipc_framed::Message;
use crate::ipc_framed::OutputStream;
use crate::ipc_framed::ResizePayload;
use crate::ipc_framed::StdinPayload;
use crate::ipc_framed::decode_bytes;
use crate::ipc_framed::encode_bytes;
use anyhow::Result;
use codex_utils_pty::ProcessDriver;
use codex_utils_pty::SpawnedProcess;
use codex_utils_pty::TerminalSize;
use codex_utils_pty::WindowsTtyInputNormalizer;
use codex_utils_pty::spawn_from_driver;
use std::fs::File;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

pub(crate) fn finish_driver_spawn(driver: ProcessDriver, stdin_open: bool) -> SpawnedProcess {
    let spawned = spawn_from_driver(driver);
    if !stdin_open {
        spawned.session.close_stdin();
    }
    spawned
}

pub(crate) fn start_runner_pipe_writer(
    mut pipe_write: File,
) -> std::sync::mpsc::Sender<FramedMessage> {
    let (outbound_tx, outbound_rx) = std::sync::mpsc::channel::<FramedMessage>();
    tokio::task::spawn_blocking(move || {
        while let Ok(msg) = outbound_rx.recv() {
            if crate::ipc_framed::write_frame(&mut pipe_write, &msg).is_err() {
                break;
            }
        }
    });
    outbound_tx
}

pub(crate) fn start_runner_stdin_writer(
    mut writer_rx: mpsc::Receiver<Vec<u8>>,
    outbound_tx: std::sync::mpsc::Sender<FramedMessage>,
    normalize_newlines: bool,
    stdin_open: bool,
) -> tokio::task::JoinHandle<()> {
    tokio::task::spawn_blocking(move || {
        let mut windows_input = WindowsTtyInputNormalizer::default();
        while let Some(bytes) = writer_rx.blocking_recv() {
            let bytes = if normalize_newlines {
                windows_input.normalize(&bytes)
            } else {
                bytes
            };
            let msg = FramedMessage {
                version: IPC_PROTOCOL_VERSION,
                message: Message::Stdin {
                    payload: StdinPayload {
                        data_b64: encode_bytes(&bytes),
                    },
                },
            };
            if outbound_tx.send(msg).is_err() {
                break;
            }
        }
        if stdin_open {
            let _ = outbound_tx.send(FramedMessage {
                version: IPC_PROTOCOL_VERSION,
                message: Message::CloseStdin {
                    payload: EmptyPayload::default(),
                },
            });
        }
    })
}

pub(crate) fn start_runner_stdout_reader(
    mut pipe_read: File,
    stdout_tx: broadcast::Sender<Vec<u8>>,
    stderr_tx: Option<broadcast::Sender<Vec<u8>>>,
    exit_tx: oneshot::Sender<i32>,
) {
    std::thread::spawn(move || {
        loop {
            let msg = match crate::ipc_framed::read_frame(&mut pipe_read) {
                Ok(Some(v)) => v,
                Ok(None) => {
                    send_runner_error(
                        "runner pipe closed before exit",
                        &stdout_tx,
                        stderr_tx.as_ref(),
                    );
                    finish_runner_transport_failure(exit_tx);
                    break;
                }
                Err(err) => {
                    send_runner_error(
                        &format!("runner read failed: {err}"),
                        &stdout_tx,
                        stderr_tx.as_ref(),
                    );
                    finish_runner_transport_failure(exit_tx);
                    break;
                }
            };

            match msg.message {
                Message::Output { payload } => {
                    if let Ok(data) = decode_bytes(&payload.data_b64) {
                        match payload.stream {
                            OutputStream::Stdout => {
                                let _ = stdout_tx.send(data);
                            }
                            OutputStream::Stderr => {
                                if let Some(stderr_tx) = stderr_tx.as_ref() {
                                    let _ = stderr_tx.send(data);
                                } else {
                                    let _ = stdout_tx.send(data);
                                }
                            }
                        }
                    }
                }
                Message::Exit { payload } => {
                    let _ = exit_tx.send(payload.exit_code);
                    break;
                }
                Message::Error { payload } => {
                    send_runner_error(&payload.message, &stdout_tx, stderr_tx.as_ref());
                    finish_runner_transport_failure(exit_tx);
                    break;
                }
                Message::SpawnReady { .. }
                | Message::Stdin { .. }
                | Message::CloseStdin { .. }
                | Message::Resize { .. }
                | Message::SpawnRequest { .. }
                | Message::Terminate { .. } => {}
            }
        }
    });
}

fn finish_runner_transport_failure(exit_tx: oneshot::Sender<i32>) {
    // Closing the driver channel preserves the legacy synthetic `-1` process
    // state while remaining distinguishable from a runner Exit frame. A
    // session promoted to strict containment later must never mistake a
    // transport failure for Job-empty acknowledgement.
    drop(exit_tx);
}

pub(crate) fn make_runner_resizer(
    outbound_tx: std::sync::mpsc::Sender<FramedMessage>,
) -> Box<dyn FnMut(TerminalSize) -> Result<()> + Send> {
    Box::new(move |size: TerminalSize| {
        outbound_tx
            .send(FramedMessage {
                version: IPC_PROTOCOL_VERSION,
                message: Message::Resize {
                    payload: ResizePayload {
                        rows: size.rows,
                        cols: size.cols,
                    },
                },
            })
            .map_err(|_| anyhow::anyhow!("runner resize pipe closed"))
    })
}

fn send_runner_error(
    message: &str,
    stdout_tx: &broadcast::Sender<Vec<u8>>,
    stderr_tx: Option<&broadcast::Sender<Vec<u8>>>,
) {
    let formatted = format!("runner error: {message}\n").into_bytes();
    if let Some(stderr_tx) = stderr_tx {
        let _ = stderr_tx.send(formatted);
    } else {
        let _ = stdout_tx.send(formatted);
    }
}

#[cfg(test)]
mod tests {
    use super::finish_runner_transport_failure;
    use tokio::sync::oneshot;

    #[tokio::test]
    async fn contained_transport_failure_does_not_acknowledge_exit() {
        let (exit_tx, exit_rx) = oneshot::channel();
        finish_runner_transport_failure(exit_tx);

        assert!(
            exit_rx.await.is_err(),
            "transport closure must not send an Exit acknowledgement"
        );
    }

    #[tokio::test]
    async fn uncontained_transport_failure_closes_the_driver_exit_channel() {
        let (exit_tx, exit_rx) = oneshot::channel();
        finish_runner_transport_failure(exit_tx);

        assert!(exit_rx.await.is_err());
    }
}
