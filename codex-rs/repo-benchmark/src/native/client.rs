use super::NativeAttemptRequest;
use anyhow::{Context, Result, bail};
use codex_app_server_test_client::{native_stdio_command, terminate_owned_process};
use serde_json::{Value, json};
use std::collections::{BTreeSet, VecDeque};
use std::fmt;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[derive(Debug)]
pub(super) struct DeadlineExpired;
impl fmt::Display for DeadlineExpired {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "native attempt deadline expired")
    }
}
impl std::error::Error for DeadlineExpired {}

pub(super) struct NativeClient {
    child: Child,
    outgoing: Option<Sender<(Value, Sender<Result<()>>)>>,
    writer: Option<JoinHandle<()>>,
    writer_done: Receiver<()>,
    incoming: Receiver<Result<Value>>,
    reader: Option<JoinHandle<()>>,
    pending: VecDeque<Value>,
    next_id: u64,
    started: Instant,
    deadline: Instant,
    pub events: Vec<Value>,
}

pub(super) struct TurnTerminal {
    pub status: String,
    pub error: Value,
    pub tool_executions: usize,
}

impl NativeClient {
    pub fn spawn(
        request: &NativeAttemptRequest,
        overrides: &[String],
        started: Instant,
        deadline: Instant,
        launch: usize,
    ) -> Result<Self> {
        let stdout_path = request
            .evidence_dir
            .join(format!("app-server-{launch}.stdout.jsonl"));
        let stderr_path = request
            .evidence_dir
            .join(format!("app-server-{launch}.stderr.log"));
        let stdout_log = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&stdout_path)?;
        let stderr_log = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&stderr_path)?;
        let requests = OpenOptions::new().write(true).create_new(true).open(
            request
                .evidence_dir
                .join(format!("app-server-{launch}.requests.jsonl")),
        )?;
        let mut command = native_stdio_command(&request.app_server, overrides);
        command
            .env_clear()
            .envs(&request.env)
            .env("CODEX_HOME", &request.codex_home)
            .current_dir(&request.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(stderr_log));
        let mut child = command.spawn().with_context(|| {
            format!("launch native app-server {}", request.app_server.display())
        })?;
        let mut stdin = child
            .stdin
            .take()
            .context("native app-server stdin unavailable")?;
        let (outgoing, writes) = mpsc::channel::<(Value, Sender<Result<()>>)>();
        let (writer_finished, writer_done) = mpsc::channel();
        let writer = thread::spawn(move || {
            let mut requests = requests;
            while let Ok((message, acknowledged)) = writes.recv() {
                let result = (|| -> Result<()> {
                    writeln!(
                        requests,
                        "{}",
                        json!({"elapsedMs":started.elapsed().as_millis() as u64,"message":message})
                    )?;
                    requests.flush()?;
                    serde_json::to_writer(&mut stdin, &message)?;
                    stdin.write_all(b"\n")?;
                    stdin.flush()?;
                    Ok(())
                })();
                let failed = result.is_err();
                if acknowledged.send(result).is_err() || failed {
                    break;
                }
            }
            drop(stdin);
            let _ = writer_finished.send(());
        });
        let stdout = child
            .stdout
            .take()
            .context("native app-server stdout unavailable")?;
        let (send, incoming) = mpsc::channel();
        let reader = thread::spawn(move || {
            let mut log = stdout_log;
            for line in BufReader::new(stdout).lines() {
                let event = (|| -> Result<Value> {
                    let line = line.context("read app-server stdout")?;
                    writeln!(log, "{line}")?;
                    log.flush()?;
                    let message: Value = serde_json::from_str(&line)
                        .context("app-server emitted non-JSON stdout")?;
                    Ok(
                        json!({"elapsedMs": started.elapsed().as_millis() as u64, "message": message}),
                    )
                })();
                let failed = event.is_err();
                if send.send(event).is_err() || failed {
                    break;
                }
            }
        });
        Ok(Self {
            child,
            outgoing: Some(outgoing),
            writer: Some(writer),
            writer_done,
            incoming,
            reader: Some(reader),
            pending: VecDeque::new(),
            next_id: 1,
            started,
            deadline,
            events: vec![],
        })
    }

    fn write(&mut self, message: Value) -> Result<()> {
        if Instant::now() >= self.deadline {
            return Err(DeadlineExpired.into());
        }
        let (acknowledge, written) = mpsc::channel();
        self.outgoing
            .as_ref()
            .context("app-server stdin is closed")?
            .send((message, acknowledge))
            .context("app-server writer stopped")?;
        let remaining = self
            .deadline
            .checked_duration_since(Instant::now())
            .ok_or(DeadlineExpired)?;
        match written.recv_timeout(remaining) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => Err(DeadlineExpired.into()),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                bail!("app-server writer stopped before acknowledging request")
            }
        }
    }

    pub fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.write(json!({"method": method, "params": params}))
    }

    fn receive(&mut self) -> Result<Value> {
        self.receive_before(self.deadline)?
            .ok_or_else(|| DeadlineExpired.into())
    }

    fn receive_before(&mut self, until: Instant) -> Result<Option<Value>> {
        let remaining = self
            .deadline
            .min(until)
            .checked_duration_since(Instant::now())
            .ok_or(DeadlineExpired)?;
        let event = match self.incoming.recv_timeout(remaining) {
            Ok(event) => event?,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if Instant::now() >= self.deadline {
                    return Err(DeadlineExpired.into());
                }
                return Ok(None);
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                bail!("native app-server stdout closed before the expected response/terminal event")
            }
        };
        let message = event["message"].clone();
        self.events.push(event);
        if message.get("method").is_some() && message.get("id").is_some() {
            self.write(json!({"id":message["id"],"error":{"code":-32601,"message":"unexpected interactive request under approval_policy=never"}}))?;
            bail!(
                "native app-server requested unsupported interaction: {}",
                message["method"]
            );
        }
        Ok(Some(message))
    }

    pub fn rpc(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.write(json!({"id": id, "method": method, "params": params}))?;
        loop {
            let message = self.receive()?;
            if message.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(error) = message.get("error") {
                    bail!("{method} RPC failed: {error}");
                }
                return message
                    .get("result")
                    .cloned()
                    .with_context(|| format!("{method} response has no result"));
            }
            self.pending.push_back(message);
        }
    }

    pub fn finish_turn(
        &mut self,
        thread_id: &str,
        turn_id: &str,
        mut cancellation_checkpoint: Option<&mut dyn FnMut() -> Result<Option<Value>>>,
    ) -> Result<TurnTerminal> {
        let mut tools = BTreeSet::new();
        let mut interrupted = false;
        let mut tool_started = false;
        loop {
            if Instant::now() >= self.deadline {
                return Err(DeadlineExpired.into());
            }
            if tool_started && !interrupted {
                if let Some(checkpoint) = cancellation_checkpoint.as_mut() {
                    if let Some(evidence) = checkpoint()? {
                        self.events.push(json!({"elapsedMs":self.started.elapsed().as_millis() as u64,"message":{"method":"repoBenchmark/cancellationStarted","params":evidence}}));
                        self.rpc(
                            "turn/interrupt",
                            json!({"threadId":thread_id,"turnId":turn_id}),
                        )?;
                        interrupted = true;
                    }
                }
            }
            let message = if let Some(message) = self.pending.pop_front() {
                message
            } else if cancellation_checkpoint.is_some() && !interrupted {
                let Some(message) =
                    self.receive_before(Instant::now() + Duration::from_millis(25))?
                else {
                    continue;
                };
                message
            } else {
                self.receive()?
            };
            let params = &message["params"];
            if params.get("threadId").and_then(Value::as_str) != Some(thread_id) {
                continue;
            }
            let method = message["method"].as_str().unwrap_or_default();
            if params.get("turnId").and_then(Value::as_str) == Some(turn_id) {
                let item = &params["item"];
                let kind = item["type"].as_str().unwrap_or_default();
                let is_tool = matches!(
                    kind,
                    "commandExecution"
                        | "dynamicToolCall"
                        | "mcpToolCall"
                        | "fileChange"
                        | "webSearch"
                        | "toolCall"
                );
                if method == "item/completed" && is_tool {
                    if let Some(id) = item["id"].as_str() {
                        tools.insert(id.to_owned());
                    }
                }
                tool_started |= method == "item/started" && is_tool;
            }
            if method == "turn/completed"
                && params.pointer("/turn/id").and_then(Value::as_str) == Some(turn_id)
            {
                if cancellation_checkpoint.is_some() && !interrupted {
                    bail!(
                        "turn completed without exercising cancellation after its running-child checkpoint"
                    );
                }
                return Ok(TurnTerminal {
                    status: params
                        .pointer("/turn/status")
                        .and_then(Value::as_str)
                        .context("turn/completed omitted status")?
                        .into(),
                    error: params["turn"]["error"].clone(),
                    tool_executions: tools.len(),
                });
            }
        }
    }

    pub fn stop(&mut self) -> Result<()> {
        self.outgoing.take();
        let until = Instant::now() + Duration::from_secs(1);
        while self.child.try_wait()?.is_none() && Instant::now() < until {
            thread::sleep(Duration::from_millis(10));
        }
        terminate_owned_process(&mut self.child)?;
        if let Some(writer) = self.writer.take() {
            self.writer_done
                .recv_timeout(Duration::from_secs(2))
                .context("app-server writer did not stop after native process cleanup")?;
            writer
                .join()
                .map_err(|_| anyhow::anyhow!("app-server stdin writer panicked"))?;
        }
        if let Some(reader) = self.reader.take() {
            reader
                .join()
                .map_err(|_| anyhow::anyhow!("app-server stdout reader panicked"))?;
        }
        while let Ok(event) = self.incoming.try_recv() {
            self.events.push(event?);
        }
        Ok(())
    }
}

impl Drop for NativeClient {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
