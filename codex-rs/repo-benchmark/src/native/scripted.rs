use anyhow::{Context, Result, bail};
use codex_app_server_test_client::LoopbackResponsesServer;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptedScenario {
    LongHistoryInitial,
    LongHistoryContinuation,
    StableContextWarmCache,
    ContextChangeInvalidation,
    DirectTools,
    ParallelTools,
    ExclusiveTools,
    NestedTools,
    RetainedProcess,
    AbortDirectNested,
    AbortRetained,
    FollowUp,
    RestartResume,
    CancelThenPrompt,
}

impl ScriptedScenario {
    pub const ALL: [Self; 14] = [
        Self::LongHistoryInitial,
        Self::LongHistoryContinuation,
        Self::StableContextWarmCache,
        Self::ContextChangeInvalidation,
        Self::DirectTools,
        Self::ParallelTools,
        Self::ExclusiveTools,
        Self::NestedTools,
        Self::RetainedProcess,
        Self::AbortDirectNested,
        Self::AbortRetained,
        Self::FollowUp,
        Self::RestartResume,
        Self::CancelThenPrompt,
    ];

    pub fn turn_count(self) -> usize {
        match self {
            Self::LongHistoryInitial
            | Self::LongHistoryContinuation
            | Self::FollowUp
            | Self::RestartResume
            | Self::AbortDirectNested
            | Self::AbortRetained
            | Self::CancelThenPrompt => 2,
            Self::StableContextWarmCache | Self::ContextChangeInvalidation => 3,
            _ => 1,
        }
    }

    pub fn cancel_first(self) -> bool {
        matches!(
            self,
            Self::AbortDirectNested | Self::AbortRetained | Self::CancelThenPrompt
        )
    }

    pub fn prompt(self, base: &str, turn: usize) -> String {
        let mut prompt = format!(
            "{base}\nRepo Benchmark scripted scenario {self:?}, turn {turn}. Execute the supplied tool actions and finish."
        );
        if matches!(
            self,
            Self::LongHistoryInitial
                | Self::LongHistoryContinuation
                | Self::StableContextWarmCache
                | Self::ContextChangeInvalidation
        ) && turn == 0
        {
            // Deterministic substantial model-visible history, without token accounting.
            for index in 0..512 {
                prompt.push_str(&format!("\nHistorical record {index:04}: stable repository state; preserve prior behavior and independently verify tool results."));
            }
        }
        if matches!(self, Self::StableContextWarmCache) && turn > 0 {
            prompt =
                "Read the same unchanged benchmark context again, verify it, and finish.".into();
        }
        prompt
    }

    fn marker_count(self) -> usize {
        match self {
            Self::ParallelTools => 3,
            Self::NestedTools => 16,
            _ => 1,
        }
    }
}

pub(super) struct ScriptedProvider {
    server: LoopbackResponsesServer,
    state: Arc<Mutex<State>>,
    cancellation: Arc<(Mutex<Option<bool>>, Condvar)>,
    pub requests_path: PathBuf,
}

struct State {
    scenario: ScriptedScenario,
    turn: usize,
    step: usize,
    serial: usize,
    completed: bool,
    observed_tool_output: bool,
    retained_session: Option<u64>,
    started_process: Option<CancellationProcess>,
    exclusive_final_pending: bool,
    exclusive_final_call: Option<String>,
    adaptations: Vec<String>,
    requests: File,
}

impl ScriptedProvider {
    pub fn start(scenario: ScriptedScenario, evidence_dir: &Path) -> Result<Self> {
        let requests_path = evidence_dir.join("scripted-provider-requests.jsonl");
        let state = Arc::new(Mutex::new(State {
            scenario,
            turn: 0,
            step: 0,
            serial: 0,
            completed: false,
            observed_tool_output: false,
            retained_session: None,
            started_process: None,
            exclusive_final_pending: false,
            exclusive_final_call: None,
            adaptations: vec![],
            requests: OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&requests_path)?,
        }));
        let handler_state = Arc::clone(&state);
        // A yielded command can cause another provider request before the
        // app-server processes turn/interrupt. Hold that request until the
        // native interrupted terminal arrives; it is not a failed tool result.
        let cancellation = Arc::new((Mutex::new(None), Condvar::new()));
        let handler_cancellation = Arc::clone(&cancellation);
        let server = LoopbackResponsesServer::start_scripted(move |request| {
            let mut state = handler_state
                .lock()
                .map_err(|_| anyhow::anyhow!("scripted provider state poisoned"))?;
            let recorded = json!({"turnIndex":state.turn,"stepIndex":state.step,"request":request});
            writeln!(state.requests, "{recorded}")?;
            state.requests.flush()?;
            if state.scenario.cancel_first() && state.turn == 0 && state.step > 0 {
                state.retained_session = session_id(&tool_outputs(&request));
                drop(state);
                let (lock, ready) = &*handler_cancellation;
                let outcome = ready
                    .wait_while(lock.lock().unwrap(), |outcome| outcome.is_none())
                    .unwrap();
                if *outcome != Some(true) {
                    bail!("scripted cancellation stopped before an interrupted terminal");
                }
                // The native runtime has already cancelled this request. Do
                // not emit a completion or mutate the following turn's state.
                return Ok(vec![]);
            }
            state.respond(&request)
        })?;
        Ok(Self {
            server,
            state,
            cancellation,
            requests_path,
        })
    }

    pub fn config_overrides(&self) -> Vec<String> {
        vec![
            "model_provider=\"repo_benchmark\"".into(),
            "model_providers.repo_benchmark.name=\"Repo Benchmark scripted provider\"".into(),
            format!(
                "model_providers.repo_benchmark.base_url={}",
                json!(self.server.base_url())
            ),
            "model_providers.repo_benchmark.wire_api=\"responses\"".into(),
            "model_providers.repo_benchmark.requires_openai_auth=false".into(),
            "model_providers.repo_benchmark.supports_websockets=false".into(),
            "model_providers.repo_benchmark.request_max_retries=0".into(),
            "model_providers.repo_benchmark.stream_max_retries=0".into(),
        ]
    }

    pub fn begin_turn(&self, turn: usize) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("scripted provider state poisoned"))?;
        if state.scenario.cancel_first()
            && turn > 0
            && *self.cancellation.0.lock().unwrap() != Some(true)
        {
            bail!("next scripted turn requires a confirmed interrupted terminal");
        }
        state.turn = turn;
        state.step = 0;
        state.completed = false;
        state.observed_tool_output = false;
        state.exclusive_final_pending = false;
        state.exclusive_final_call = None;
        Ok(())
    }

    pub fn cancellation_checkpoint(
        &self,
        request: &super::NativeAttemptRequest,
        deadline: Instant,
    ) -> Result<Option<Value>> {
        let mut state = self.state.lock().unwrap();
        if state.scenario == ScriptedScenario::AbortRetained && state.retained_session.is_none() {
            return Ok(None);
        }
        let path = request.cwd.join("repo-benchmark-cancellation-started.json");
        if !path.is_file() {
            return Ok(None);
        }
        let process: CancellationProcess = serde_json::from_slice(&fs::read(&path)?)
            .context("invalid child-written cancellation checkpoint")?;
        if !process.is_alive(request, deadline)? {
            bail!("cancellation child exited before interruption exercised running work");
        }
        state.started_process = Some(process.clone());
        Ok(Some(
            json!({"process":process,"retainedSession":state.retained_session,"checkpoint":path}),
        ))
    }

    pub fn verify_cancelled(
        &self,
        request: &super::NativeAttemptRequest,
        deadline: Instant,
    ) -> Result<Value> {
        let process = self
            .state
            .lock()
            .unwrap()
            .started_process
            .clone()
            .context("cancelled scenario never established a running child process")?;
        let until = deadline.min(Instant::now() + Duration::from_secs(2));
        while process.is_alive(request, deadline)? {
            if Instant::now() >= until {
                bail!(
                    "native turn reported interrupted but cancellation child {} is still running",
                    process.pid
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let late_effect = request.cwd.join(marker_file(0, 0));
        if late_effect.exists() {
            bail!(
                "cancelled child produced its forbidden late effect: {}",
                late_effect.display()
            );
        }
        Ok(json!({"process":process,"terminated":true,"lateEffectAbsent":late_effect}))
    }

    pub fn confirm_interrupted(&self) {
        *self.cancellation.0.lock().unwrap() = Some(true);
        self.cancellation.1.notify_all();
    }

    pub fn adaptations(&self) -> Result<Vec<String>> {
        Ok(self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("scripted provider state poisoned"))?
            .adaptations
            .clone())
    }

    pub fn verify_turn(&self, cwd: &Path, turn: usize) -> Result<()> {
        let state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("scripted provider state poisoned"))?;
        if !state.completed {
            bail!("native completion occurred before scripted provider completed its scenario");
        }
        if state.scenario == ScriptedScenario::LongHistoryInitial {
            return Ok(());
        }
        if !state.observed_tool_output {
            bail!("scripted turn completed without model-visible tool output");
        }
        for index in 0..state.scenario.marker_count() {
            let content = fs::read_to_string(cwd.join(marker_file(turn, index)))
                .context("scripted tool did not create its independently verified marker")?;
            if content != marker(turn, index) {
                bail!("scripted tool marker has incorrect contents: {content:?}");
            }
        }
        if state.scenario == ScriptedScenario::ContextChangeInvalidation && turn == 2 {
            let content = fs::read_to_string(cwd.join("repo-benchmark-context.txt"))?;
            if content != "changed-context" {
                bail!("changed context did not persist");
            }
        }
        Ok(())
    }
}

impl Drop for ScriptedProvider {
    fn drop(&mut self) {
        // Release an in-flight handler before the loopback server joins it,
        // including setup errors and attempt deadlines.
        *self.cancellation.0.lock().unwrap() = Some(false);
        self.cancellation.1.notify_all();
    }
}

impl State {
    fn respond(&mut self, request: &Value) -> Result<Vec<Value>> {
        if self.turn > 0
            && matches!(
                self.scenario,
                ScriptedScenario::LongHistoryInitial | ScriptedScenario::LongHistoryContinuation
            )
            && !request["input"]
                .to_string()
                .contains("Historical record 0511:")
        {
            bail!("native follow-up request lost the seeded session history");
        }
        self.serial += 1;
        let id = format!("repo-benchmark-response-{}", self.serial);
        let mut events = vec![json!({"type":"response.created","response":{"id":id}})];
        let items = if self.scenario == ScriptedScenario::LongHistoryInitial {
            self.completed = true;
            vec![assistant_item("Scripted initial request completed.")]
        } else if self.step == 0 {
            self.step += 1;
            self.initial_calls(request)?
        } else {
            self.continue_calls(request)?
        };
        for item in items {
            events.push(json!({"type":"response.output_item.done","item":item}));
        }
        // Usage is intentionally absent: scripted execution does not measure tokens.
        events.push(json!({"type":"response.completed","response":{"id":id,"status":"completed"}}));
        Ok(events)
    }

    fn initial_calls(&mut self, request: &Value) -> Result<Vec<Value>> {
        let cancel = self.scenario.cancel_first() && self.turn == 0;
        let retained = self.scenario == ScriptedScenario::RetainedProcess
            || (self.scenario == ScriptedScenario::AbortRetained && cancel);
        let commands = if cancel {
            vec![format!(
                "$p = [System.Diagnostics.Process]::GetCurrentProcess(); [System.IO.File]::WriteAllText('repo-benchmark-cancellation-started.tmp', (@{{pid=$PID;startTimeUtcTicks=$p.StartTime.ToUniversalTime().Ticks}} | ConvertTo-Json -Compress)); [System.IO.File]::Move('repo-benchmark-cancellation-started.tmp','repo-benchmark-cancellation-started.json'); Start-Sleep -Seconds 30; {}",
                marker_command(self.turn, 0)
            )]
        } else {
            (0..self.scenario.marker_count()).map(|index| {
                let mut command = marker_command(self.turn, index);
                if self.scenario == ScriptedScenario::RetainedProcess {
                    command = format!("$null = [Console]::In.ReadLine(); [Console]::Out.WriteLine('repo-benchmark-retained-stage-1'); $null = [Console]::In.ReadLine(); {command}");
                }
                if matches!(self.scenario, ScriptedScenario::StableContextWarmCache | ScriptedScenario::ContextChangeInvalidation) {
                    let value = if self.scenario == ScriptedScenario::ContextChangeInvalidation && self.turn > 0 { "changed-context" } else { "stable-context" };
                    if self.turn == 0 || (self.scenario == ScriptedScenario::ContextChangeInvalidation && self.turn == 1) {
                        command = format!("[System.IO.File]::WriteAllText('repo-benchmark-context.txt','{value}'); {command}");
                    }
                    command.push_str("; [Console]::Out.Write([System.IO.File]::ReadAllText('repo-benchmark-context.txt'))");
                }
                command
            }).collect()
        };
        self.command_calls(request, &commands, retained || cancel)
    }

    fn command_calls(
        &mut self,
        request: &Value,
        commands: &[String],
        yielding: bool,
    ) -> Result<Vec<Value>> {
        let tools = advertised_tools(request);
        let nested = matches!(
            self.scenario,
            ScriptedScenario::NestedTools
                | ScriptedScenario::AbortDirectNested
                | ScriptedScenario::ExclusiveTools
        );
        let direct = tools.iter().find(|tool| {
            matches!(
                tool.name.as_str(),
                "exec_command" | "shell_command" | "shell"
            )
        });
        let exec = tools.iter().find(|tool| tool.name == "exec" && tool.custom);
        let interactive = self.scenario == ScriptedScenario::RetainedProcess
            || (self.scenario == ScriptedScenario::AbortRetained && self.turn == 0);
        if nested || direct.is_none() {
            if let Some(exec) = exec {
                let command_schema = tools.iter().find(|tool| tool.name == "exec_command");
                let args = commands
                    .iter()
                    .map(|cmd| exec_command_args(cmd, interactive, yielding, command_schema))
                    .collect::<Vec<_>>();
                let code = if args.len() > 1 {
                    format!(
                        "const results = await Promise.all([{}]); for (const result of results) text(result);",
                        args.iter()
                            .map(|args| format!("tools.exec_command({args})"))
                            .collect::<Vec<_>>()
                            .join(",")
                    )
                } else if self.scenario == ScriptedScenario::ExclusiveTools {
                    format!(
                        "const results = await Promise.all([tools.update_plan({{plan:[{{step:'Run independently verified command',status:'in_progress'}}]}}), tools.exec_command({})]); for (const result of results) text(result); text(await tools.update_plan({{plan:[{{step:'Run independently verified command',status:'completed'}}]}}));",
                        args[0]
                    )
                } else {
                    format!("text(await tools.exec_command({}));", args[0])
                };
                self.record_adaptation(
                    "commands packaged through advertised native code-mode exec",
                );
                return Ok(vec![
                    exec.call(&format!("call-{}", self.serial), Value::String(code)),
                ]);
            }
        }
        let direct = direct.context(
            "native provider request advertises no supported command tool or code-mode exec",
        )?;
        if nested {
            self.record_adaptation("reference has no advertised code-mode exec; equivalent logical commands use direct native tools");
        }
        let mut calls = Vec::new();
        if self.scenario == ScriptedScenario::ExclusiveTools {
            self.exclusive_final_pending = true;
            let update = tools
                .iter()
                .find(|tool| tool.name == "update_plan")
                .context("exclusive workload requires native update_plan")?;
            calls.push(update.call(&format!("plan-{}",self.serial), json!({"plan":[{"step":"Run independently verified command","status":"in_progress"}]})));
        }
        for (index, cmd) in commands.iter().enumerate() {
            let args = match direct.name.as_str() {
                "exec_command" => exec_command_args(cmd, interactive, yielding, Some(direct)),
                "shell_command" => {
                    json!({"command":cmd,"timeout_ms":if yielding {60000} else {10000}})
                }
                "shell" => {
                    json!({"command":["powershell.exe","-NoProfile","-NonInteractive","-Command",cmd],"timeout_ms":if yielding {60000} else {10000}})
                }
                _ => unreachable!(),
            };
            if yielding && interactive && direct.name != "exec_command" {
                bail!("retained-process workload requires native exec_command/write_stdin support");
            }
            calls.push(direct.call(&format!("call-{}-{index}", self.serial), args));
        }
        Ok(calls)
    }

    fn continue_calls(&mut self, request: &Value) -> Result<Vec<Value>> {
        let output = tool_outputs(request);
        if output.is_empty() {
            bail!("native continuation omitted all model-visible tool results");
        }
        let expected = (0..self.scenario.marker_count())
            .map(|index| marker(self.turn, index))
            .collect::<Vec<_>>();
        let complete = expected.iter().all(|marker| output.contains(marker));
        if self.scenario == ScriptedScenario::RetainedProcess && (self.step <= 2 || !complete) {
            if self.step > 8 {
                bail!("retained process did not finish after bounded polls");
            }
            let session = session_id(&output)
                .context("retained command did not supply a session id for write_stdin")?;
            self.step += 1;
            let tools = advertised_tools(request);
            let chars = if self.step == 2 {
                "continue-1\n"
            } else if self.step == 3 {
                "continue-2\n"
            } else {
                "progress\n"
            };
            let args = json!({"session_id":session,"chars":chars,"yield_time_ms":1000,"max_output_tokens":2000});
            if let Some(tool) = tools.iter().find(|tool| tool.name == "write_stdin") {
                return Ok(vec![tool.call(&format!("poll-{}", self.serial), args)]);
            }
            let exec = tools
                .iter()
                .find(|tool| tool.name == "exec" && tool.custom)
                .context("retained command has no native poll tool")?;
            return Ok(vec![exec.call(
                &format!("poll-{}", self.serial),
                Value::String(format!("text(await tools.write_stdin({args}));")),
            )]);
        }
        if !complete {
            bail!(
                "native tool results do not contain the expected scenario marker(s): {expected:?}; received {output}"
            );
        }
        if self.exclusive_final_pending {
            self.exclusive_final_pending = false;
            let tools = advertised_tools(request);
            let update = tools
                .iter()
                .find(|tool| tool.name == "update_plan")
                .context("exclusive workload requires native update_plan for final completion")?;
            let id = format!("plan-completed-{}", self.serial);
            self.exclusive_final_call = Some(id.clone());
            return Ok(vec![update.call(&id,json!({"plan":[{"step":"Run independently verified command","status":"completed"}]}))]);
        }
        if let Some(id) = &self.exclusive_final_call {
            if !request["input"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|item| {
                    item["type"] == "function_call_output"
                        && item["call_id"] == *id
                        && item["output"].to_string().contains("Plan updated")
                })
            {
                bail!("native exclusive continuation omitted the final plan-update success result");
            }
        }
        if matches!(
            self.scenario,
            ScriptedScenario::StableContextWarmCache | ScriptedScenario::ContextChangeInvalidation
        ) {
            let expected_context =
                if self.scenario == ScriptedScenario::ContextChangeInvalidation && self.turn > 0 {
                    "changed-context"
                } else {
                    "stable-context"
                };
            if !output.contains(expected_context) {
                bail!("native tool returned stale or missing context; expected {expected_context}");
            }
        }
        self.observed_tool_output = true;
        self.completed = true;
        Ok(vec![assistant_item("Verified scripted tool output. Done.")])
    }

    fn record_adaptation(&mut self, value: &str) {
        if !self.adaptations.iter().any(|entry| entry == value) {
            self.adaptations.push(value.into());
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CancellationProcess {
    pid: u32,
    start_time_utc_ticks: u64,
}

impl CancellationProcess {
    fn is_alive(&self, request: &super::NativeAttemptRequest, deadline: Instant) -> Result<bool> {
        if Instant::now() >= deadline {
            return Err(super::client::DeadlineExpired.into());
        }
        // Compare creation time as well as PID: a reused PID is not the child
        // whose cancellation is under test. The probe itself is deadline bound.
        let script = format!(
            "$ErrorActionPreference = 'Stop'; $p = Get-Process -Id {} -ErrorAction SilentlyContinue; if ($null -ne $p -and $p.StartTime.ToUniversalTime().Ticks -eq {}) {{ [Console]::Out.Write('running') }} else {{ [Console]::Out.Write('gone') }}",
            self.pid, self.start_time_utc_ticks
        );
        let mut command = Command::new("powershell.exe");
        command
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .env_clear()
            .envs(&request.env)
            .current_dir(&request.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000);
        }
        let mut child = command.spawn()?;
        loop {
            if let Some(status) = child.try_wait()? {
                let mut output = String::new();
                child.stdout.take().unwrap().read_to_string(&mut output)?;
                if !status.success() {
                    bail!("cancellation process probe failed: {status}");
                }
                return match output.as_str() {
                    "running" => Ok(true),
                    "gone" => Ok(false),
                    _ => bail!("cancellation process probe returned invalid evidence: {output:?}"),
                };
            }
            if Instant::now() >= deadline {
                codex_app_server_test_client::terminate_owned_process(&mut child)?;
                return Err(super::client::DeadlineExpired.into());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

fn exec_command_args(cmd: &str, interactive: bool, yielding: bool, schema: Option<&Tool>) -> Value {
    let mut args = json!({"cmd":cmd,"tty":interactive,"yield_time_ms":if yielding {250} else {1000},"max_output_tokens":2000});
    if schema.is_some_and(|tool| tool.properties.get("kind").is_some()) {
        args["kind"] = json!("script");
    }
    args
}

fn marker(turn: usize, index: usize) -> String {
    format!("repo-benchmark-turn-{turn}-tool-{index}-verified")
}
fn marker_file(turn: usize, index: usize) -> String {
    format!("repo-benchmark-marker-{turn}-{index}.txt")
}
fn marker_command(turn: usize, index: usize) -> String {
    format!(
        "[System.IO.File]::WriteAllText('{}','{}'); [Console]::Out.Write('{}')",
        marker_file(turn, index),
        marker(turn, index),
        marker(turn, index)
    )
}
fn assistant_item(text: &str) -> Value {
    json!({"type":"message","id":"repo-benchmark-final","role":"assistant","status":"completed","content":[{"type":"output_text","text":text}]})
}

#[derive(Debug)]
struct Tool {
    name: String,
    namespace: Option<String>,
    custom: bool,
    properties: Value,
}
impl Tool {
    fn call(&self, id: &str, args: Value) -> Value {
        let mut item = if self.custom {
            json!({"type":"custom_tool_call","call_id":id,"name":self.name,"input":args.as_str().unwrap_or_default()})
        } else {
            json!({"type":"function_call","call_id":id,"name":self.name,"arguments":args.to_string()})
        };
        if let Some(namespace) = &self.namespace {
            item["namespace"] = json!(namespace);
        }
        item
    }
}

fn advertised_tools(request: &Value) -> Vec<Tool> {
    fn collect(items: &[Value], namespace: Option<&str>, out: &mut Vec<Tool>) {
        for item in items {
            if item["type"] == "namespace" {
                if let Some(children) = item["tools"].as_array() {
                    collect(children, item["name"].as_str(), out);
                }
            } else {
                let function = item.get("function").unwrap_or(item);
                if let Some(name) = function["name"].as_str() {
                    out.push(Tool {
                        name: name.into(),
                        namespace: namespace.map(str::to_owned),
                        custom: item["type"] == "custom",
                        properties: function["parameters"]["properties"].clone(),
                    });
                }
            }
        }
    }
    let mut tools = vec![];
    if let Some(items) = request["tools"].as_array() {
        collect(items, None, &mut tools);
    }
    tools
}

fn tool_outputs(request: &Value) -> String {
    request["input"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| {
            matches!(
                item["type"].as_str(),
                Some("function_call_output" | "custom_tool_call_output" | "tool_search_output")
            )
        })
        .map(|item| item.get("output").unwrap_or(&Value::Null).to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

fn session_id(text: &str) -> Option<u64> {
    for prefix in [
        "session_id",
        "Session ID:",
        "Session ID ",
        "session ID ",
        "Process running with session ID ",
    ] {
        if let Some((_, suffix)) = text.rsplit_once(prefix) {
            let digits = suffix
                .trim_start_matches(|character: char| !character.is_ascii_digit())
                .chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>();
            if let Ok(id) = digits.parse() {
                return Some(id);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpStream;

    fn post(server: &ScriptedProvider, body: &Value) -> String {
        post_url(server.server.base_url(), body)
    }

    fn post_url(url: &str, body: &Value) -> String {
        post_url_with_timeout(url, body, Duration::from_secs(3))
    }

    fn post_url_with_timeout(url: &str, body: &Value, timeout: Duration) -> String {
        let mut socket = TcpStream::connect(url.trim_start_matches("http://")).unwrap();
        socket.set_read_timeout(Some(timeout)).unwrap();
        let body = body.to_string();
        write!(socket,"POST /responses HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",body.len()).unwrap();
        let mut response = String::new();
        socket.read_to_string(&mut response).unwrap();
        response
    }

    #[test]
    fn cancellation_continuation_waits_for_interrupt_then_next_turn_verifies() {
        for scenario in [
            ScriptedScenario::CancelThenPrompt,
            ScriptedScenario::AbortDirectNested,
            ScriptedScenario::AbortRetained,
        ] {
            let temp = tempfile::tempdir().unwrap();
            let provider = ScriptedProvider::start(scenario, temp.path()).unwrap();
            let tools = json!([{"type":"function","name":"exec_command"}]);
            let first = post(&provider, &json!({"tools":tools,"input":[]}));
            assert!(first.contains("Start-Sleep"));
            assert!(
                provider
                    .begin_turn(1)
                    .unwrap_err()
                    .to_string()
                    .contains("confirmed interrupted terminal")
            );
            let url = provider.server.base_url().to_owned();
            let body = json!({"tools":tools,"input":[{"type":"function_call_output","output":"Process running with session ID 4182; output pending"}]});
            let (send, receive) = std::sync::mpsc::channel();
            let worker = std::thread::spawn(move || send.send(post_url(&url, &body)).unwrap());
            // Deliberately exceed the 250ms yield: the continuation must not
            // manufacture a tool failure or complete the cancelled turn.
            assert!(matches!(
                receive.recv_timeout(std::time::Duration::from_millis(500)),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ));
            provider.confirm_interrupted();
            let cancelled_response = receive
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap();
            worker.join().unwrap();
            assert!(cancelled_response.starts_with("HTTP/1.1 200 OK"));
            assert!(!cancelled_response.contains("response.completed"));
            assert!(!cancelled_response.contains("expected scenario marker"));
            provider.begin_turn(1).unwrap();
            assert!(post(&provider, &json!({"tools":tools,"input":[]})).contains(&marker(1, 0)));
            let completed = post(
                &provider,
                &json!({"tools":tools,"input":[{"type":"function_call_output","output":marker(1,0)}]}),
            );
            assert!(completed.contains("Verified scripted tool output"));
            assert!(provider.verify_turn(temp.path(), 1).is_err());
            fs::write(temp.path().join(marker_file(1, 0)), marker(1, 0)).unwrap();
            provider.verify_turn(temp.path(), 1).unwrap();
            let captured: Vec<Value> = fs::read_to_string(&provider.requests_path)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(
                captured
                    .iter()
                    .map(|event| event["turnIndex"].as_u64().unwrap())
                    .collect::<Vec<_>>(),
                vec![0, 0, 1, 1]
            );
        }
    }

    #[test]
    fn nested_and_direct_commands_use_the_same_advertised_kind_contract() {
        for with_kind in [false, true] {
            let mut properties = json!({"cmd":{"type":"string"}});
            if with_kind {
                properties["kind"] = json!({"enum":["script","process"]});
            }
            let tools = json!([{"type":"custom","name":"exec"},{"type":"function","name":"exec_command","parameters":{"properties":properties}}]);
            let mut arguments = vec![];
            for scenario in [
                ScriptedScenario::DirectTools,
                ScriptedScenario::AbortDirectNested,
            ] {
                let temp = tempfile::tempdir().unwrap();
                let provider = ScriptedProvider::start(scenario, temp.path()).unwrap();
                // Turn one is the normal command following cancellation.
                provider.confirm_interrupted();
                provider.begin_turn(1).unwrap();
                let response = post(&provider, &json!({"tools":tools,"input":[]}));
                let item = response
                    .lines()
                    .filter_map(|line| line.strip_prefix("data: "))
                    .map(|line| serde_json::from_str::<Value>(line).unwrap())
                    .find_map(|event| event.get("item").cloned())
                    .unwrap();
                let args = if item["type"] == "function_call" {
                    serde_json::from_str::<Value>(item["arguments"].as_str().unwrap()).unwrap()
                } else {
                    let code = item["input"].as_str().unwrap();
                    serde_json::from_str::<Value>(
                        code.strip_prefix("text(await tools.exec_command(")
                            .unwrap()
                            .strip_suffix("));")
                            .unwrap(),
                    )
                    .unwrap()
                };
                assert_eq!(args.get("kind"), with_kind.then_some(&json!("script")));
                arguments.push(args);
            }
            assert_eq!(arguments[0], arguments[1]);
        }
    }

    #[test]
    #[cfg(windows)]
    fn cancellation_requires_the_actual_child_and_rejects_survival_or_late_effects() {
        use std::os::windows::process::CommandExt;
        struct OwnedChild(std::process::Child);
        impl Drop for OwnedChild {
            fn drop(&mut self) {
                let _ = codex_app_server_test_client::terminate_owned_process(&mut self.0);
            }
        }
        for scenario in [
            ScriptedScenario::AbortDirectNested,
            ScriptedScenario::AbortRetained,
        ] {
            let temp = tempfile::tempdir().unwrap();
            let provider = ScriptedProvider::start(scenario, temp.path()).unwrap();
            let request = super::super::NativeAttemptRequest {
                attempt_id: "cancellation-process-contract".into(),
                app_server: temp.path().join("unused.exe"),
                cwd: temp.path().to_path_buf(),
                codex_home: temp.path().join("home"),
                evidence_dir: temp.path().to_path_buf(),
                env: std::env::vars().collect(),
                config_overrides: vec![],
                expected_config: json!({}),
                prompt: String::new(),
                timeout_ms: 20000,
                scenario: Some(scenario),
            };
            let deadline = Instant::now() + Duration::from_secs(20);
            assert!(
                provider
                    .cancellation_checkpoint(&request, deadline)
                    .unwrap()
                    .is_none()
            );
            assert!(
                provider
                    .verify_cancelled(&request, deadline)
                    .unwrap_err()
                    .to_string()
                    .contains("never established a running child")
            );
            let tools =
                json!([{"type":"custom","name":"exec"},{"type":"function","name":"exec_command"}]);
            let first = post(&provider, &json!({"tools":tools,"input":[]}));
            let item = first
                .lines()
                .filter_map(|line| line.strip_prefix("data: "))
                .map(|line| serde_json::from_str::<Value>(line).unwrap())
                .find_map(|event| event.get("item").cloned())
                .unwrap();
            let args: Value = if scenario == ScriptedScenario::AbortDirectNested {
                assert_eq!(item["type"], "custom_tool_call");
                serde_json::from_str(
                    item["input"]
                        .as_str()
                        .unwrap()
                        .strip_prefix("text(await tools.exec_command(")
                        .unwrap()
                        .strip_suffix("));")
                        .unwrap(),
                )
                .unwrap()
            } else {
                serde_json::from_str(item["arguments"].as_str().unwrap()).unwrap()
            };
            let child = Command::new("powershell.exe")
                .args([
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    args["cmd"].as_str().unwrap(),
                ])
                .current_dir(temp.path())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .creation_flags(0x0800_0000)
                .spawn()
                .unwrap();
            let mut child = OwnedChild(child);
            let checkpoint_path = temp.path().join("repo-benchmark-cancellation-started.json");
            let until = Instant::now() + Duration::from_secs(5);
            while !checkpoint_path.is_file() && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(
                checkpoint_path.is_file(),
                "the nested/retained command itself must announce its process"
            );
            let identity: Value =
                serde_json::from_slice(&fs::read(&checkpoint_path).unwrap()).unwrap();
            assert_eq!(identity["pid"], child.0.id());
            if scenario == ScriptedScenario::AbortRetained {
                assert!(
                    provider
                        .cancellation_checkpoint(&request, deadline)
                        .unwrap()
                        .is_none(),
                    "a process alone does not establish a retained handle"
                );
            }
            let url = provider.server.base_url().to_owned();
            let (send, receive) = std::sync::mpsc::channel();
            let worker = std::thread::spawn(move || {
                let response = post_url_with_timeout(
                    &url,
                    &json!({"input":[{"type":"function_call_output","output":"Process running with session ID 7182"}]}),
                    Duration::from_secs(20),
                );
                send.send(response).unwrap();
            });
            let until = Instant::now() + Duration::from_secs(2);
            while provider.state.lock().unwrap().retained_session.is_none()
                && Instant::now() < until
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            let checkpoint = provider
                .cancellation_checkpoint(&request, deadline)
                .unwrap()
                .unwrap();
            assert_eq!(checkpoint["process"]["pid"], child.0.id());
            assert_eq!(checkpoint["retainedSession"], 7182);
            assert!(
                provider
                    .verify_cancelled(&request, deadline)
                    .unwrap_err()
                    .to_string()
                    .contains("still running")
            );
            codex_app_server_test_client::terminate_owned_process(&mut child.0).unwrap();
            let stopped = provider.verify_cancelled(&request, deadline).unwrap();
            assert_eq!(stopped["terminated"], true);
            fs::write(temp.path().join(marker_file(0, 0)), marker(0, 0)).unwrap();
            assert!(
                provider
                    .verify_cancelled(&request, deadline)
                    .unwrap_err()
                    .to_string()
                    .contains("forbidden late effect")
            );
            provider.confirm_interrupted();
            assert!(
                receive
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap()
                    .starts_with("HTTP/1.1 200 OK")
            );
            worker.join().unwrap();
        }
    }

    #[test]
    fn exclusive_direct_adaptation_executes_the_same_final_plan_update() {
        let temp = tempfile::tempdir().unwrap();
        let provider =
            ScriptedProvider::start(ScriptedScenario::ExclusiveTools, temp.path()).unwrap();
        let tools = json!([{"type":"function","name":"exec_command"},{"type":"function","name":"update_plan"}]);
        let first = post(&provider, &json!({"tools":tools,"input":[]}));
        assert_eq!(first.matches("\"name\":\"update_plan\"").count(), 1);
        assert!(first.contains("in_progress"));
        let completed_tool = json!({"type":"function_call_output","output":marker(0,0)});
        let second = post(&provider, &json!({"tools":tools,"input":[completed_tool]}));
        let item = second
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .find_map(|event| event.get("item").cloned())
            .unwrap();
        assert_eq!(item["name"], "update_plan");
        let args: Value = serde_json::from_str(item["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["plan"][0]["status"], "completed");
        assert!(
            provider.verify_turn(temp.path(), 0).is_err(),
            "completion requires the final plan update result"
        );
        let missing = post(&provider, &json!({"tools":tools,"input":[completed_tool]}));
        assert!(missing.contains("omitted the final plan-update success result"));
        let last = post(
            &provider,
            &json!({"tools":tools,"input":[completed_tool,{"type":"function_call_output","call_id":item["call_id"],"output":"Plan updated"}]}),
        );
        assert!(last.contains("Verified scripted tool output"));
        fs::write(temp.path().join(marker_file(0, 0)), marker(0, 0)).unwrap();
        provider.verify_turn(temp.path(), 0).unwrap();
    }

    #[test]
    fn history_scenario_rejects_a_follow_up_that_lost_prior_input() {
        let temp = tempfile::tempdir().unwrap();
        let provider =
            ScriptedProvider::start(ScriptedScenario::LongHistoryInitial, temp.path()).unwrap();
        assert_eq!(ScriptedScenario::LongHistoryInitial.turn_count(), 2);
        provider.begin_turn(0).unwrap();
        let original = ScriptedScenario::LongHistoryInitial.prompt("History task", 0);
        let first = post(
            &provider,
            &json!({"input":[{"role":"user","content":original}]}),
        );
        assert!(first.starts_with("HTTP/1.1 200 OK"));
        provider.begin_turn(1).unwrap();
        let follow_up = ScriptedScenario::LongHistoryInitial.prompt("History task", 1);
        assert!(!follow_up.contains("Historical record 0511:"));
        let lost = post(
            &provider,
            &json!({"input":[{"role":"user","content":follow_up}]}),
        );
        assert!(lost.starts_with("HTTP/1.1 400 Bad Request"));
        assert!(lost.contains("lost the seeded session history"));
        let retained = post(
            &provider,
            &json!({"input":[{"role":"user","content":original},{"role":"user","content":follow_up}]}),
        );
        assert!(retained.starts_with("HTTP/1.1 200 OK"));
        provider.verify_turn(temp.path(), 1).unwrap();
    }

    #[test]
    fn provider_exercises_native_tool_response_and_rejects_missing_effect() {
        let temp = tempfile::tempdir().unwrap();
        let provider = ScriptedProvider::start(ScriptedScenario::DirectTools, temp.path()).unwrap();
        provider.begin_turn(0).unwrap();
        let tools = json!([{"type":"namespace","name":"functions","tools":[{"type":"function","name":"exec_command","parameters":{"properties":{"cmd":{"type":"string"}}}}]}]);
        let first = post(&provider, &json!({"tools":tools,"input":[]}));
        assert!(first.starts_with("HTTP/1.1 200 OK"));
        assert!(first.contains("function_call"));
        assert!(first.contains("exec_command"));
        assert!(first.contains("namespace\":\"functions"));
        let second = post(
            &provider,
            &json!({"tools":tools,"input":[{"type":"function_call_output","call_id":"call-1-0","output":"tool failed: cannot access executable"}]}),
        );
        assert!(second.starts_with("HTTP/1.1 400 Bad Request"));
        assert!(second.contains("expected scenario marker"));
        assert!(
            provider
                .verify_turn(temp.path(), 0)
                .unwrap_err()
                .to_string()
                .contains("before scripted provider completed")
        );
        let captured: Vec<Value> = fs::read_to_string(&provider.requests_path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(captured.len(), 2);
        assert_eq!(
            captured[1]["request"]["input"][0]["output"],
            "tool failed: cannot access executable"
        );
    }

    #[test]
    fn provider_requires_both_model_visible_success_and_actual_file_effect() {
        let temp = tempfile::tempdir().unwrap();
        let provider = ScriptedProvider::start(ScriptedScenario::FollowUp, temp.path()).unwrap();
        let tools = json!([{"type":"function","name":"exec_command"}]);
        for turn in 0..2 {
            provider.begin_turn(turn).unwrap();
            let call = post(&provider, &json!({"tools":tools,"input":[]}));
            assert!(call.starts_with("HTTP/1.1 200 OK"));
            let completion = post(
                &provider,
                &json!({"tools":tools,"input":[{"type":"function_call_output","output":marker(turn,0)}]}),
            );
            assert!(completion.contains("Verified scripted tool output"));
            assert!(
                provider.verify_turn(temp.path(), turn).is_err(),
                "tool claims alone must not pass the verifier"
            );
            fs::write(temp.path().join(marker_file(turn, 0)), "incorrect").unwrap();
            assert!(
                provider
                    .verify_turn(temp.path(), turn)
                    .unwrap_err()
                    .to_string()
                    .contains("incorrect contents")
            );
            fs::write(temp.path().join(marker_file(turn, 0)), marker(turn, 0)).unwrap();
            provider.verify_turn(temp.path(), turn).unwrap();
        }
    }

    #[test]
    fn native_code_mode_packaging_preserves_sixteen_logical_actions() {
        let temp = tempfile::tempdir().unwrap();
        let provider = ScriptedProvider::start(ScriptedScenario::NestedTools, temp.path()).unwrap();
        let response = post(
            &provider,
            &json!({"tools":[{"type":"custom","name":"exec"}],"input":[]}),
        );
        let event = response
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .find(|event| event["item"]["type"] == "custom_tool_call")
            .unwrap();
        let code = event["item"]["input"].as_str().unwrap();
        assert!(
            !code.contains("\"kind\""),
            "do not invent an unadvertised nested argument"
        );
        assert_eq!(code.matches("tools.exec_command(").count(), 16);
        assert!(code.contains("Promise.all"));
        for index in 0..16 {
            assert!(code.contains(&marker(0, index)));
        }
        assert_eq!(
            provider.adaptations().unwrap(),
            vec!["commands packaged through advertised native code-mode exec"]
        );
    }

    #[test]
    fn retained_process_uses_the_returned_id_for_both_required_polls() {
        let temp = tempfile::tempdir().unwrap();
        let provider =
            ScriptedProvider::start(ScriptedScenario::RetainedProcess, temp.path()).unwrap();
        let tools = json!([{"type":"function","name":"exec_command"},{"type":"function","name":"write_stdin"}]);
        let first = post(&provider, &json!({"tools":tools,"input":[]}));
        let call = first
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .find(|event| event["item"]["name"] == "exec_command")
            .unwrap();
        let args: Value =
            serde_json::from_str(call["item"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["yield_time_ms"], 250);
        assert_eq!(args["tty"], true);
        assert_eq!(
            args["cmd"]
                .as_str()
                .unwrap()
                .matches("[Console]::In.ReadLine()")
                .count(),
            2
        );
        for step in 1..=2 {
            let response = post(
                &provider,
                &json!({"tools":tools,"input":[{"type":"function_call_output","output":"Process running with session ID 4182; output pending"}]}),
            );
            let event = response
                .lines()
                .filter_map(|line| line.strip_prefix("data: "))
                .map(|line| serde_json::from_str::<Value>(line).unwrap())
                .find(|event| event["item"]["name"] == "write_stdin")
                .unwrap();
            let args: Value =
                serde_json::from_str(event["item"]["arguments"].as_str().unwrap()).unwrap();
            assert_eq!(args["session_id"], 4182);
            assert_eq!(args["chars"], format!("continue-{step}\n"));
        }
        let completion = post(
            &provider,
            &json!({"tools":tools,"input":[{"type":"function_call_output","output":marker(0,0)}]}),
        );
        assert!(completion.contains("Verified scripted tool output"));
    }
}
