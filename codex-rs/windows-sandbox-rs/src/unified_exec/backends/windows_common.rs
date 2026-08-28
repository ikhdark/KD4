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
    stdout_tx: mpsc::Sender<Vec<u8>>,
    stderr_tx: Option<mpsc::Sender<Vec<u8>>>,
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
                    let _ = exit_tx.send(-1);
                    break;
                }
                Err(err) => {
                    send_runner_error(
                        &format!("runner read failed: {err}"),
                        &stdout_tx,
                        stderr_tx.as_ref(),
                    );
                    let _ = exit_tx.send(-1);
                    break;
                }
            };

            match msg.message {
                Message::Output { payload } => {
                    let data = match decode_bytes(&payload.data_b64) {
                        Ok(data) => data,
                        Err(err) => {
                            send_runner_error(
                                &format!("runner sent invalid output payload: {err}"),
                                &stdout_tx,
                                stderr_tx.as_ref(),
                            );
                            let _ = exit_tx.send(-1);
                            break;
                        }
                    };
                    match payload.stream {
                        OutputStream::Stdout => {
                            let _ = stdout_tx.blocking_send(data);
                        }
                        OutputStream::Stderr => {
                            if let Some(stderr_tx) = stderr_tx.as_ref() {
                                let _ = stderr_tx.blocking_send(data);
                            } else {
                                let _ = stdout_tx.blocking_send(data);
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
                    let _ = exit_tx.send(-1);
                    break;
                }
                other => {
                    send_runner_error(
                        &format!("unexpected runner message after startup: {other:?}"),
                        &stdout_tx,
                        stderr_tx.as_ref(),
                    );
                    let _ = exit_tx.send(-1);
                    break;
                }
            }
        }
    });
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
    stdout_tx: &mpsc::Sender<Vec<u8>>,
    stderr_tx: Option<&mpsc::Sender<Vec<u8>>>,
) {
    let formatted = format!("runner error: {message}\n").into_bytes();
    if let Some(stderr_tx) = stderr_tx {
        let _ = stderr_tx.blocking_send(formatted);
    } else {
        let _ = stdout_tx.blocking_send(formatted);
    }
}

#[cfg(test)]
mod tests {
    use super::start_runner_stdout_reader;
    use crate::ipc_framed::FramedMessage;
    use crate::ipc_framed::IPC_PROTOCOL_VERSION;
    use crate::ipc_framed::Message;
    use crate::ipc_framed::OutputPayload;
    use crate::ipc_framed::OutputStream;
    use crate::ipc_framed::write_frame;
    use std::io::Seek;
    use std::time::Duration;
    use tokio::sync::mpsc;
    use tokio::sync::oneshot;

    #[tokio::test]
    async fn invalid_output_payload_fails_the_runner_session() -> anyhow::Result<()> {
        let mut pipe_read = tempfile::tempfile()?;
        write_frame(
            &mut pipe_read,
            &FramedMessage {
                version: IPC_PROTOCOL_VERSION,
                message: Message::Output {
                    payload: OutputPayload {
                        data_b64: "not base64".to_string(),
                        stream: OutputStream::Stdout,
                    },
                },
            },
        )?;
        pipe_read.rewind()?;

        let (stdout_tx, mut stdout_rx) = mpsc::channel(1);
        let (exit_tx, exit_rx) = oneshot::channel();
        start_runner_stdout_reader(pipe_read, stdout_tx, None, exit_tx);

        let exit_code = tokio::time::timeout(Duration::from_secs(2), exit_rx)
            .await
            .expect("runner stdout reader must finish")?;
        assert_eq!(exit_code, -1);
        let error = stdout_rx.recv().await.expect("runner error output");
        assert!(
            String::from_utf8_lossy(&error).contains("runner sent invalid output payload"),
            "unexpected runner error: {}",
            String::from_utf8_lossy(&error)
        );
        Ok(())
    }
}
