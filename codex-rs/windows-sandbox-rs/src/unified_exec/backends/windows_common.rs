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

#[path = "runner_outbound.rs"]
mod runner_outbound;
use crate::ipc_framed::IO_CHUNK_BYTES;
use crate::ipc_framed::IO_WINDOW_CHUNKS;
use crate::ipc_framed::OutputCredit;
use runner_outbound::RunnerOutbound;

pub(crate) fn finish_driver_spawn(driver: ProcessDriver, stdin_open: bool) -> SpawnedProcess {
    let spawned = spawn_from_driver(driver);
    if !stdin_open {
        spawned.session.close_stdin();
    }
    spawned
}

pub(crate) fn start_runner_pipe_writer(pipe_write: File) -> RunnerOutbound {
    RunnerOutbound::start(pipe_write)
}

pub(crate) fn start_runner_stdin_writer(
    mut writer_rx: mpsc::Receiver<Vec<u8>>,
    outbound_tx: RunnerOutbound,
    normalize_newlines: bool,
    stdin_open: bool,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut windows_input = WindowsTtyInputNormalizer::default();
        loop {
            let bytes = tokio::select! {
                biased;
                _ = outbound_tx.input_closed() => return,
                bytes = writer_rx.recv() => match bytes { Some(bytes) => bytes, None => break },
            };
            let bytes = if normalize_newlines {
                windows_input.normalize(&bytes)
            } else {
                bytes
            };
            for chunk in bytes.chunks(IO_CHUNK_BYTES) {
                let message = Message::Stdin {
                    payload: StdinPayload {
                        data_b64: encode_bytes(chunk),
                    },
                };
                if outbound_tx.send_data(message).await.is_err() {
                    return;
                }
            }
        }
        if stdin_open {
            let _ = outbound_tx
                .send_data(Message::CloseStdin {
                    payload: EmptyPayload::default(),
                })
                .await;
        }
    })
}

pub(crate) fn start_runner_stdout_reader(
    mut pipe_read: File,
    stdout_tx: mpsc::Sender<Vec<u8>>,
    stderr_tx: Option<mpsc::Sender<Vec<u8>>>,
    exit_tx: oneshot::Sender<i32>,
    outbound: Option<RunnerOutbound>,
) {
    // Independent bounded stream consumers never hold up the frame parser.
    // The runner's per-stream window makes try_send below nonblocking even
    // while either downstream output channel is full.
    let stdout_queue =
        start_output_delivery(stdout_tx.clone(), OutputStream::Stdout, outbound.clone());
    let stderr_queue = start_output_delivery(
        stderr_tx.clone().unwrap_or_else(|| stdout_tx.clone()),
        OutputStream::Stderr,
        outbound.clone(),
    );
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
                    let queue = if payload.stream == OutputStream::Stdout {
                        &stdout_queue
                    } else {
                        &stderr_queue
                    };
                    if data.len() > IO_CHUNK_BYTES || queue.try_send(data).is_err() {
                        send_runner_error(
                            "runner exceeded output window or output sink closed",
                            &stdout_tx,
                            stderr_tx.as_ref(),
                        );
                        let _ = exit_tx.send(-1);
                        break;
                    }
                }
                Message::StdinAck { .. } => {
                    if let Some(outbound) = &outbound
                        && outbound.acknowledge_input().is_err()
                    {
                        let _ = exit_tx.send(-1);
                        break;
                    }
                }
                Message::StdinClosed { .. } => {
                    if let Some(outbound) = &outbound {
                        outbound.close_input();
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
        if let Some(outbound) = outbound {
            outbound.stop();
        }
    });
}

fn start_output_delivery(
    sender: mpsc::Sender<Vec<u8>>,
    stream: OutputStream,
    outbound: Option<RunnerOutbound>,
) -> std::sync::mpsc::SyncSender<Vec<u8>> {
    let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(IO_WINDOW_CHUNKS);
    std::thread::spawn(move || {
        while let Ok(bytes) = rx.recv() {
            if sender.blocking_send(bytes).is_err() {
                break;
            }
            if let Some(outbound) = &outbound {
                let _ = outbound.send(FramedMessage {
                    version: IPC_PROTOCOL_VERSION,
                    message: Message::OutputAck {
                        payload: OutputCredit { stream, chunks: 1 },
                    },
                });
            }
        }
    });
    tx
}

pub(crate) fn make_runner_resizer(
    outbound_tx: RunnerOutbound,
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
        let _ = stderr_tx.try_send(formatted);
    } else {
        let _ = stdout_tx.try_send(formatted);
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
    async fn stalled_stdout_cannot_hide_stdin_receipts_or_exit() -> anyhow::Result<()> {
        let outbound = super::start_runner_pipe_writer(tempfile::tempfile()?);
        outbound
            .send_data(Message::Stdin {
                payload: crate::ipc_framed::StdinPayload {
                    data_b64: crate::ipc_framed::encode_bytes(b"input"),
                },
            })
            .await?;
        let mut pipe = tempfile::tempfile()?;
        for _ in 0..super::IO_WINDOW_CHUNKS {
            write_frame(
                &mut pipe,
                &FramedMessage {
                    version: IPC_PROTOCOL_VERSION,
                    message: Message::Output {
                        payload: OutputPayload {
                            stream: OutputStream::Stdout,
                            data_b64: crate::ipc_framed::encode_bytes(&vec![
                                0;
                                super::IO_CHUNK_BYTES
                            ]),
                        },
                    },
                },
            )?;
        }
        write_frame(
            &mut pipe,
            &FramedMessage {
                version: IPC_PROTOCOL_VERSION,
                message: Message::StdinAck {
                    payload: crate::ipc_framed::EmptyPayload::default(),
                },
            },
        )?;
        write_frame(
            &mut pipe,
            &FramedMessage {
                version: IPC_PROTOCOL_VERSION,
                message: Message::Exit {
                    payload: crate::ipc_framed::ExitPayload {
                        exit_code: 0,
                        timed_out: false,
                    },
                },
            },
        )?;
        pipe.rewind()?;
        let (stdout, _held_stdout) = mpsc::channel(1);
        let (exit, receipt) = oneshot::channel();
        start_runner_stdout_reader(pipe, stdout, None, exit, Some(outbound.clone()));
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), receipt).await??,
            0
        );
        tokio::time::timeout(Duration::from_secs(1), outbound.input_closed()).await?;
        Ok(())
    }

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
        start_runner_stdout_reader(pipe_read, stdout_tx, None, exit_tx, None);

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
