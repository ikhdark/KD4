//! Bounded parent-side data and coalesced control admission. Native pipe writes
//! never occupy Tokio's blocking pool. Lock ordering: only this short state lock;
//! it is released before native IO and before awaiting queue/credit changes.
//! Its critical sections only assign fields and cannot panic, so a poisoned
//! lock still guards consistent state.
use crate::ipc_framed::EmptyPayload;
use crate::ipc_framed::FramedMessage;
use crate::ipc_framed::IO_WINDOW_CHUNKS;
use crate::ipc_framed::IPC_PROTOCOL_VERSION;
use crate::ipc_framed::Message;
use crate::ipc_framed::OutputCredit;
use crate::ipc_framed::OutputStream;
use crate::ipc_framed::ResizePayload;
use crate::ipc_framed::write_frame;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::PoisonError;
use tokio::sync::Notify;

struct State {
    data: Option<FramedMessage>,
    resize: Option<ResizePayload>,
    output_acks: [usize; 2],
    credits: usize,
    input_closed: bool,
    stopped: bool,
    draining: bool,
    terminate: bool,
}

struct Shared {
    state: Mutex<State>,
    ready: Condvar,
    changed: Notify,
}
struct Senders(Arc<Shared>);
impl Drop for Senders {
    fn drop(&mut self) {
        self.0
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .draining = true;
        self.0.ready.notify_all();
    }
}

#[derive(Clone)]
pub(crate) struct RunnerOutbound(Arc<Senders>);

fn stop(shared: &Shared) {
    shared
        .state
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .stopped = true;
    shared.ready.notify_all();
    shared.changed.notify_waiters();
}

impl RunnerOutbound {
    pub(super) fn start(mut pipe: std::fs::File) -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                data: None,
                resize: None,
                output_acks: [0; 2],
                credits: IO_WINDOW_CHUNKS,
                input_closed: false,
                stopped: false,
                draining: false,
                terminate: false,
            }),
            ready: Condvar::new(),
            changed: Notify::new(),
        });
        let worker = Arc::clone(&shared);
        std::thread::spawn(move || {
            loop {
                let frame = {
                    let mut state = worker.state.lock().unwrap_or_else(PoisonError::into_inner);
                    loop {
                        let message = if std::mem::take(&mut state.terminate) {
                            Some(Message::Terminate {
                                payload: EmptyPayload::default(),
                            })
                        } else if state.stopped {
                            break None;
                        } else if let Some(index) = state.output_acks.iter().position(|n| *n != 0) {
                            Some(Message::OutputAck {
                                payload: OutputCredit {
                                    stream: if index == 0 {
                                        OutputStream::Stdout
                                    } else {
                                        OutputStream::Stderr
                                    },
                                    chunks: std::mem::take(&mut state.output_acks[index]),
                                },
                            })
                        } else if let Some(payload) = state.resize.take() {
                            Some(Message::Resize { payload })
                        } else if let Some(frame) = state.data.take() {
                            worker.changed.notify_waiters();
                            break Some(frame);
                        } else if state.draining {
                            break None;
                        } else {
                            None
                        };
                        if let Some(message) = message {
                            break Some(FramedMessage {
                                version: IPC_PROTOCOL_VERSION,
                                message,
                            });
                        }
                        state = worker
                            .ready
                            .wait(state)
                            .unwrap_or_else(PoisonError::into_inner);
                    }
                };
                let Some(frame) = frame else {
                    break;
                };
                if write_frame(&mut pipe, &frame).is_err() {
                    break;
                }
            }
            stop(&worker);
        });
        Self(Arc::new(Senders(shared)))
    }

    pub(super) fn stop(&self) {
        stop(&self.0.0);
    }
    pub(super) fn close_input(&self) {
        self.0
            .0
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .input_closed = true;
        self.0.0.changed.notify_waiters();
    }

    pub(super) async fn input_closed(&self) {
        loop {
            let notified = self.0.0.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let state = self
                    .0
                    .0
                    .state
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                if state.stopped || state.input_closed {
                    return;
                }
            }
            notified.await;
        }
    }

    pub(super) fn acknowledge_input(&self) -> anyhow::Result<()> {
        let mut state = self
            .0
            .0
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        anyhow::ensure!(
            state.credits < IO_WINDOW_CHUNKS,
            "runner over-credited stdin"
        );
        state.credits += 1;
        self.0.0.changed.notify_waiters();
        Ok(())
    }

    pub(super) async fn send_data(&self, message: Message) -> anyhow::Result<()> {
        let stdin = matches!(message, Message::Stdin { .. });
        loop {
            let notified = self.0.0.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut state = self
                    .0
                    .0
                    .state
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                anyhow::ensure!(!state.stopped && !state.input_closed, "runner stdin closed");
                if state.data.is_none() && (!stdin || state.credits > 0) {
                    if stdin {
                        state.credits -= 1;
                    }
                    state.data = Some(FramedMessage {
                        version: IPC_PROTOCOL_VERSION,
                        message,
                    });
                    self.0.0.ready.notify_one();
                    return Ok(());
                }
            }
            notified.await;
        }
    }

    pub(crate) fn send(&self, frame: FramedMessage) -> anyhow::Result<()> {
        let mut state = self
            .0
            .0
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        anyhow::ensure!(!state.stopped, "runner control pipe closed");
        match frame.message {
            Message::Terminate { .. } => {
                state.terminate = true;
                state.stopped = true;
                state.data = None;
            }
            Message::Resize { payload } => state.resize = Some(payload),
            Message::OutputAck { payload } => {
                let index = if payload.stream == OutputStream::Stdout {
                    0
                } else {
                    1
                };
                anyhow::ensure!(
                    payload.chunks > 0
                        && payload.chunks <= IO_WINDOW_CHUNKS
                        && state.output_acks[index] <= IO_WINDOW_CHUNKS - payload.chunks,
                    "output credit overflow"
                );
                state.output_acks[index] += payload.chunks;
            }
            _ => anyhow::bail!("data must use bounded async admission"),
        }
        self.0.0.ready.notify_one();
        self.0.0.changed.notify_waiters();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc_framed::IO_CHUNK_BYTES;
    use crate::ipc_framed::StdinPayload;
    use crate::ipc_framed::encode_bytes;

    #[tokio::test]
    async fn stdin_window_stops_acceptance_until_child_write_receipt_and_termination_wakes_it() {
        let outbound = RunnerOutbound::start(tempfile::tempfile().unwrap());
        for _ in 0..IO_WINDOW_CHUNKS {
            outbound
                .send_data(Message::Stdin {
                    payload: StdinPayload {
                        data_b64: encode_bytes(&vec![0; IO_CHUNK_BYTES]),
                    },
                })
                .await
                .unwrap();
        }
        let next = || Message::Stdin {
            payload: StdinPayload {
                data_b64: encode_bytes(b"next"),
            },
        };
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(50),
                outbound.send_data(next())
            )
            .await
            .is_err()
        );
        outbound.acknowledge_input().unwrap();
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            outbound.send_data(next()),
        )
        .await
        .unwrap()
        .unwrap();
        outbound
            .send(FramedMessage {
                version: IPC_PROTOCOL_VERSION,
                message: Message::Terminate {
                    payload: EmptyPayload::default(),
                },
            })
            .unwrap();
        assert!(outbound.send_data(next()).await.is_err());
        outbound.input_closed().await;
    }
}
