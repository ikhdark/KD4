// Two intentionally separate latency workflows share this executable:
//
// - `code-mode-turn` captures one fixed warm workload for one identified build. It
//   emits raw samples and a nested-dispatch quality verdict; cross-build comparison
//   belongs to the external benchmark runner.
// - Synthetic scenarios run local baseline/candidate pairs and own the statistical
//   non-inferiority verdict.
//
// `BenchmarkCommand` is the ownership boundary. Keep capture-only fields out of
// `Report` and paired-comparison fields out of `CodeModeCaptureReport`.

use anyhow::Context;
use anyhow::Result;
use codex_features::Feature;
use codex_protocol::protocol::TurnTiming;
use core_test_support::responses::ResponseMock;
use core_test_support::responses::ResponsesRequest;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_custom_tool_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use futures::SinkExt;
use futures::StreamExt;
use rand::Rng;
use rand::SeedableRng;
use rand::rngs::StdRng;
use serde::Serialize;
use std::env;
use std::fs::OpenOptions;
use std::io::Write;
use std::net::SocketAddr;
use std::path::Path;
use std::path::PathBuf;

use std::process::Command;

use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::task::JoinHandle;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

const DEFAULT_WARMUPS: usize = 10;
const DEFAULT_ITERATIONS: usize = 100;
const DEFAULT_CLUSTERS: usize = 3;
const RELIABILITY_ITERATIONS: usize = 600;
const CODE_MODE_WARMUPS: usize = 5;
const CODE_MODE_ITERATIONS: usize = 30;
const CODE_MODE_CLUSTERS: usize = 3;
const MAX_READY_TO_SAMPLE_TO_DISPATCH_NS: u64 = 1_000_000_000;
const CODE_MODE_NESTED_DISPATCH_SOURCE: &str = r#"
const dispatched = await tools.update_plan({
  plan: [{ step: "benchmark nested dispatch", status: "in_progress" }],
});
text(JSON.stringify({
  dispatched: typeof dispatched?.message === "string",
}));
"#;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Scenario {
    Deterministic,
    LoopbackWebsocket,
    Persistence,
    WindowsExecutor,
}

#[derive(Clone, Copy, Debug)]
enum ScenarioWorkload {
    Deterministic,
    LoopbackWebsocket(SocketAddr),
    Persistence,
    WindowsExecutor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Mode {
    Cold,
    Warm,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Variant {
    Baseline,
    Candidate,
}

#[derive(Debug)]
struct Args {
    scenario: Option<Scenario>,
    mode: Option<Mode>,
    iterations: usize,
    warmups: usize,
    clusters: usize,
    absolute_margin_ms: f64,
    relative_margin: f64,
}

#[derive(Debug)]
enum BenchmarkCommand {
    CodeModeCapture { host: PathBuf },
    Synthetic(Args),
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
struct Sample {
    duration_ns: u64,
    sampling_requests: u32,
    failed: bool,
    serialized_bytes: u64,
    cache_hits: u32,
    exec_description_tokens: u64,
    prompt_input_tokens: u64,
    tool_calls: u32,
    max_ready_to_sample_to_dispatch_ns: Option<u64>,
}

#[derive(Debug, Serialize)]
struct VariantSummary {
    median_ms: f64,
    p95_ms: f64,
    sampling_request_median: f64,
    failure_rate: f64,
    serialized_bytes_median: f64,
    cache_hits_median: f64,
    exec_description_tokens_median: f64,
    prompt_input_tokens_median: f64,
    tool_call_median: f64,
}

#[derive(Debug, Serialize)]
struct NonInferiority {
    absolute_regression_ucb_ms: f64,
    relative_regression_ucb: f64,
    sampling_request_mean_delta: f64,
    failure_rate_delta: f64,
    absolute_margin_ms: f64,
    relative_margin: f64,
    passed: bool,
}

#[derive(Debug, Serialize)]
struct ClusterReport {
    cluster: usize,
    baseline: VariantSummary,
    candidate: VariantSummary,
    non_inferiority: NonInferiority,
    baseline_samples: Vec<Sample>,
    candidate_samples: Vec<Sample>,
}

#[derive(Debug, Serialize)]
struct Report {
    schema_version: u16,
    scenario: Scenario,
    mode: Mode,
    warmups: usize,
    measured_iterations_per_cluster: usize,
    clusters: Vec<ClusterReport>,
    passed: bool,
    limitation: &'static str,
}

#[derive(Debug, Serialize)]
struct CodeModeClusterReport {
    cluster: usize,
    capture: VariantSummary,
    samples: Vec<Sample>,
}

#[derive(Debug, Serialize)]
struct CodeModeCaptureReport {
    schema_version: u16,
    scenario: &'static str,
    mode: &'static str,
    warmups: usize,
    measured_iterations_per_cluster: usize,
    clusters: Vec<CodeModeClusterReport>,
    passed: bool,
    limitation: &'static str,
}

struct LoopbackServer {
    addr: SocketAddr,
    task: JoinHandle<()>,
}

impl LoopbackServer {
    async fn start() -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let Ok(mut websocket) = accept_async(stream).await else {
                        return;
                    };
                    while let Some(Ok(message)) = websocket.next().await {
                        tokio::time::sleep(Duration::from_millis(8)).await;
                        if websocket.send(message).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
        Ok(Self { addr, task })
    }
}

impl Drop for LoopbackServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

type ClientWebsocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct ScenarioState {
    websocket: Option<ClientWebsocket>,
    serialized_schema: Option<Vec<u8>>,
    persistence_dir: TempDir,
}

impl ScenarioState {
    fn new() -> Result<Self> {
        Ok(Self {
            websocket: None,
            serialized_schema: None,
            persistence_dir: tempfile::tempdir()?,
        })
    }

    async fn preconnect(&mut self, addr: SocketAddr) -> Result<&mut ClientWebsocket> {
        if self.websocket.is_none() {
            let (websocket, _) = connect_async(format!("ws://{addr}")).await?;
            self.websocket = Some(websocket);
        }
        self.websocket
            .as_mut()
            .context("preconnected websocket should be initialized")
    }
}

struct CodeModeFixture {
    _server: wiremock::MockServer,
    test: TestCodex,
    response_mock: ResponseMock,
}

impl CodeModeFixture {
    async fn start(code_mode_host: &Path, turns: usize, fixture_id: &str) -> Result<Self> {
        let server = start_mock_server().await;
        let mut sequence = Vec::with_capacity(turns * 2);
        for turn in 0..turns {
            let response_id = format!("{fixture_id}-exec-{turn}");
            let call_id = format!("{fixture_id}-call-{turn}");
            sequence.push(sse(vec![
                ev_response_created(&response_id),
                ev_custom_tool_call(&call_id, "exec", CODE_MODE_NESTED_DISPATCH_SOURCE),
                ev_completed(&response_id),
            ]));
            let completion_id = format!("{fixture_id}-completion-{turn}");
            sequence.push(sse(vec![
                ev_assistant_message(&completion_id, "done"),
                ev_completed(&completion_id),
            ]));
        }
        let response_mock = mount_sse_sequence(&server, sequence).await;
        let test = test_codex()
            .with_model("test-gpt-5.1-codex")
            .with_code_mode_host_program(code_mode_host.to_path_buf())
            .with_config(|config| {
                let _ = config.features.enable(Feature::CodeModeOnly);
                let _ = config.features.disable(Feature::TaskCompletionReviewer);
            })
            .build(&server)
            .await?;
        Ok(Self {
            _server: server,
            test,
            response_mock,
        })
    }

    async fn sample(&self) -> Sample {
        let requests_before = self.response_mock.requests().len();
        let started = Instant::now();
        let completion = self
            .test
            .submit_turn_and_capture_completion(
                "Run the fixed CodeModeOnly nested-dispatch benchmark and finish.",
            )
            .await;
        let duration_ns = started.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        let requests = self.response_mock.requests();
        let turn_requests = &requests[requests_before..];
        match completion {
            Ok(completion) => {
                let timing = completion.timing.as_ref();
                let first_request = turn_requests.first();
                let sampling_requests = timing
                    .map(|timing| timing.counters.model_request_count)
                    .unwrap_or_default();
                let tool_calls = timing
                    .map(|timing| timing.counters.tool_call_count)
                    .unwrap_or_default();
                let semantic_output_ok =
                    turn_requests.get(1).is_some_and(nested_output_is_expected);
                let max_ready_to_sample_to_dispatch_ns =
                    timing.and_then(max_ready_to_sample_to_dispatch_ns);
                let failed = completion.last_agent_message.as_deref() != Some("done")
                    || completion.error.is_some()
                    || turn_requests.len() != 2
                    || sampling_requests != 2
                    || tool_calls != 2
                    || !timing.is_some_and(timing_reconciles)
                    || !semantic_output_ok
                    || !ready_to_sample_dispatch_gate_passes(max_ready_to_sample_to_dispatch_ns);
                Sample {
                    duration_ns,
                    sampling_requests,
                    failed,
                    serialized_bytes: first_request
                        .map(|request| request.body_bytes().len() as u64)
                        .unwrap_or_default(),
                    cache_hits: 0,
                    exec_description_tokens: first_request
                        .map(exec_description_tokens)
                        .unwrap_or_default(),
                    prompt_input_tokens: first_request.map(prompt_input_tokens).unwrap_or_default(),
                    tool_calls,
                    max_ready_to_sample_to_dispatch_ns,
                }
            }
            Err(_) => Sample {
                duration_ns,
                failed: true,
                ..Sample::default()
            },
        }
    }
}

fn max_ready_to_sample_to_dispatch_ns(timing: &TurnTiming) -> Option<u64> {
    timing
        .tool_calls
        .iter()
        .filter_map(|call| call.ready_to_sample_to_dispatch_ns)
        .max()
}

fn ready_to_sample_dispatch_gate_passes(measured_ns: Option<u64>) -> bool {
    measured_ns.is_some_and(|measured_ns| measured_ns <= MAX_READY_TO_SAMPLE_TO_DISPATCH_NS)
}

fn nested_output_is_expected(request: &ResponsesRequest) -> bool {
    request
        .body_json()
        .get("input")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|items| {
            items.iter().any(|item| {
                item.get("type").and_then(serde_json::Value::as_str)
                    == Some("custom_tool_call_output")
                    && item
                        .get("output")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|output| output.contains("\"dispatched\":true"))
            })
        })
}

fn timing_reconciles(timing: &TurnTiming) -> bool {
    timing.model_requests.len() == timing.counters.model_request_count as usize
        && timing.counters.model_request_count
            == timing.counters.attempts_by_kind.primary
                + timing.counters.attempts_by_kind.retry
                + timing.counters.attempts_by_kind.fallback
        && timing
            .model_requests
            .iter()
            .map(|request| request.tool_call_count)
            .sum::<u32>()
            == timing.counters.tool_call_count
}

fn exec_description_tokens(request: &ResponsesRequest) -> u64 {
    request
        .body_json()
        .get("tools")
        .and_then(serde_json::Value::as_array)
        .and_then(|tools| {
            tools.iter().find_map(|tool| {
                (tool.get("name").and_then(serde_json::Value::as_str) == Some("exec"))
                    .then(|| tool.get("description").and_then(serde_json::Value::as_str))
                    .flatten()
            })
        })
        .map(codex_utils_output_truncation::approx_token_count)
        .unwrap_or_default() as u64
}

fn prompt_input_tokens(request: &ResponsesRequest) -> u64 {
    let body = request.body_json();
    let logical_prompt = serde_json::json!({
        "instructions": body.get("instructions"),
        "input": body.get("input"),
        "tools": body.get("tools"),
    });
    codex_utils_output_truncation::approx_token_count(&logical_prompt.to_string()) as u64
}

#[tokio::main]
async fn main() -> Result<()> {
    match parse_command()? {
        BenchmarkCommand::CodeModeCapture { host } => {
            let report = run_code_mode_capture(&host).await?;
            let passed = report.passed;
            println!("{}", serde_json::to_string(&report)?);
            if !passed {
                anyhow::bail!("code-mode capture failed its nested-dispatch quality gate");
            }
            Ok(())
        }
        BenchmarkCommand::Synthetic(args) => run_synthetic_reports(args).await,
    }
}

async fn run_synthetic_reports(args: Args) -> Result<()> {
    let scenarios = args.scenario.map_or_else(
        || {
            vec![
                Scenario::Deterministic,
                Scenario::LoopbackWebsocket,
                Scenario::Persistence,
                Scenario::WindowsExecutor,
            ]
        },
        |scenario| vec![scenario],
    );
    let modes = args
        .mode
        .map_or_else(|| vec![Mode::Cold, Mode::Warm], |mode| vec![mode]);
    let mut any_failed = false;
    for scenario in scenarios {
        if scenario == Scenario::WindowsExecutor && !cfg!(windows) {
            continue;
        }
        for mode in &modes {
            let report = run_report(scenario, *mode, &args).await?;
            any_failed |= !report.passed;
            println!("{}", serde_json::to_string(&report)?);
        }
    }
    if any_failed {
        anyhow::bail!("one or more independent benchmark clusters failed non-inferiority")
    }
    Ok(())
}

async fn run_report(scenario: Scenario, mode: Mode, args: &Args) -> Result<Report> {
    let (workload, _loopback) = match scenario {
        Scenario::Deterministic => (ScenarioWorkload::Deterministic, None),
        Scenario::LoopbackWebsocket => {
            let server = LoopbackServer::start().await?;
            (
                ScenarioWorkload::LoopbackWebsocket(server.addr),
                Some(server),
            )
        }
        Scenario::Persistence => (ScenarioWorkload::Persistence, None),
        Scenario::WindowsExecutor => (ScenarioWorkload::WindowsExecutor, None),
    };
    let mut clusters = Vec::with_capacity(args.clusters);
    for cluster in 0..args.clusters {
        let mut rng = StdRng::seed_from_u64(0x4b4434_u64 + cluster as u64);
        let mut baseline_state = ScenarioState::new()?;
        let mut candidate_state = ScenarioState::new()?;
        if mode == Mode::Warm {
            for _ in 0..args.warmups {
                let _ = run_sample(workload, Variant::Baseline, &mut baseline_state).await;
                let _ = run_sample(workload, Variant::Candidate, &mut candidate_state).await;
            }
        }
        let mut baseline = Vec::with_capacity(args.iterations);
        let mut candidate = Vec::with_capacity(args.iterations);
        for _ in 0..args.iterations {
            let candidate_first = rng.random_bool(0.5);
            if mode == Mode::Cold {
                baseline_state = ScenarioState::new()?;
                candidate_state = ScenarioState::new()?;
            }
            if candidate_first && let ScenarioWorkload::LoopbackWebsocket(addr) = workload {
                candidate_state.preconnect(addr).await?;
            }
            if candidate_first {
                candidate
                    .push(run_sample(workload, Variant::Candidate, &mut candidate_state).await);
                baseline.push(run_sample(workload, Variant::Baseline, &mut baseline_state).await);
            } else {
                baseline.push(run_sample(workload, Variant::Baseline, &mut baseline_state).await);
                if let ScenarioWorkload::LoopbackWebsocket(addr) = workload {
                    candidate_state.preconnect(addr).await?;
                }
                candidate
                    .push(run_sample(workload, Variant::Candidate, &mut candidate_state).await);
            }
        }
        let gate = non_inferiority(
            &baseline,
            &candidate,
            args.absolute_margin_ms,
            args.relative_margin,
        );
        clusters.push(ClusterReport {
            cluster: cluster + 1,
            baseline: summarize(&baseline),
            candidate: summarize(&candidate),
            non_inferiority: gate,
            baseline_samples: baseline,
            candidate_samples: candidate,
        });
    }
    let passed = clusters
        .iter()
        .all(|cluster| cluster.non_inferiority.passed);
    Ok(Report {
        schema_version: 2,
        scenario,
        mode,
        warmups: if mode == Mode::Warm { args.warmups } else { 0 },
        measured_iterations_per_cluster: args.iterations,
        clusters,
        passed,
        limitation: "controlled local benchmark only; it does not establish real-model or Desktop-visible latency gains",
    })
}

async fn run_code_mode_capture(code_mode_host: &Path) -> Result<CodeModeCaptureReport> {
    anyhow::ensure!(
        code_mode_host.is_file(),
        "code-mode host does not exist: {}",
        code_mode_host.display()
    );
    let turns = CODE_MODE_WARMUPS + CODE_MODE_ITERATIONS;
    let mut clusters = Vec::with_capacity(CODE_MODE_CLUSTERS);
    for cluster in 0..CODE_MODE_CLUSTERS {
        let fixture =
            CodeModeFixture::start(code_mode_host, turns, &format!("capture-{}", cluster + 1))
                .await?;
        for _ in 0..CODE_MODE_WARMUPS {
            let sample = fixture.sample().await;
            anyhow::ensure!(
                !sample.failed,
                "CodeModeOnly warmup failed its nested-dispatch quality gate"
            );
        }
        let mut samples = Vec::with_capacity(CODE_MODE_ITERATIONS);
        for _ in 0..CODE_MODE_ITERATIONS {
            samples.push(fixture.sample().await);
        }
        clusters.push(CodeModeClusterReport {
            cluster: cluster + 1,
            capture: summarize(&samples),
            samples,
        });
    }
    Ok(code_mode_capture_report(clusters))
}

fn code_mode_capture_report(clusters: Vec<CodeModeClusterReport>) -> CodeModeCaptureReport {
    let passed = clusters
        .iter()
        .flat_map(|cluster| &cluster.samples)
        .all(|sample| !sample.failed);
    CodeModeCaptureReport {
        schema_version: 4,
        scenario: "code_mode_turn",
        mode: "warm",
        warmups: CODE_MODE_WARMUPS,
        measured_iterations_per_cluster: CODE_MODE_ITERATIONS,
        clusters,
        passed,
        limitation: "single-build capture only: an external runner must attach build identity and compare this raw report with a separately captured build; this local mock benchmark does not establish Desktop-visible or real-model latency",
    }
}

async fn run_sample(
    workload: ScenarioWorkload,
    variant: Variant,
    state: &mut ScenarioState,
) -> Sample {
    let started = Instant::now();
    let result = match workload {
        ScenarioWorkload::Deterministic => deterministic_sample(variant, state).await,
        ScenarioWorkload::LoopbackWebsocket(addr) => websocket_sample(variant, state, addr).await,
        ScenarioWorkload::Persistence => persistence_sample(variant, state),
        ScenarioWorkload::WindowsExecutor => windows_executor_sample(),
    };
    match result {
        Ok((sampling_requests, serialized_bytes, cache_hits)) => Sample {
            duration_ns: started.elapsed().as_nanos().min(u64::MAX as u128) as u64,
            sampling_requests,
            failed: false,
            serialized_bytes,
            cache_hits,
            ..Sample::default()
        },
        Err(_) => Sample {
            duration_ns: started.elapsed().as_nanos().min(u64::MAX as u128) as u64,
            failed: true,
            ..Sample::default()
        },
    }
}

async fn deterministic_sample(
    variant: Variant,
    state: &mut ScenarioState,
) -> Result<(u32, u64, u32)> {
    match variant {
        Variant::Baseline => {
            tokio::time::sleep(Duration::from_millis(3)).await;
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        Variant::Candidate => {
            tokio::join!(
                tokio::time::sleep(Duration::from_millis(3)),
                tokio::time::sleep(Duration::from_millis(2))
            );
        }
    }
    let (serialized_bytes, cache_hits) = match variant {
        Variant::Baseline => {
            let payload =
                serde_json::json!({"tools": ["shell", "mcp"], "schema": {"type": "object"}});
            (serde_json::to_vec(&payload)?.len() as u64, 0)
        }
        Variant::Candidate => {
            let cache_hit = state.serialized_schema.is_some();
            if state.serialized_schema.is_none() {
                let payload = serde_json::json!({
                    "tools": ["shell", "mcp"],
                    "schema": {"type": "object"}
                });
                state.serialized_schema = Some(serde_json::to_vec(&payload)?);
            }
            let serialized_bytes = state
                .serialized_schema
                .as_ref()
                .map_or(0, |serialized| serialized.len() as u64);
            (serialized_bytes, u32::from(cache_hit))
        }
    };
    Ok((2, serialized_bytes, cache_hits))
}

async fn websocket_sample(
    variant: Variant,
    state: &mut ScenarioState,
    addr: SocketAddr,
) -> Result<(u32, u64, u32)> {
    if variant == Variant::Baseline {
        state.websocket = None;
    }
    let websocket = state.preconnect(addr).await?;
    websocket
        .send(Message::Binary(vec![1, 2, 3, 4].into()))
        .await?;
    websocket.next().await.context("websocket closed")??;
    Ok((1, 4, u32::from(variant == Variant::Candidate)))
}

fn persistence_sample(variant: Variant, state: &mut ScenarioState) -> Result<(u32, u64, u32)> {
    let path = state.persistence_dir.path().join("rollout.jsonl");
    let items = (0..8)
        .map(|index| format!("{{\"id\":{index},\"value\":\"item\"}}\n"))
        .collect::<Vec<_>>();
    match variant {
        Variant::Baseline => {
            for item in &items {
                append_and_flush(&path, item.as_bytes())?;
            }
        }
        Variant::Candidate => append_and_flush(&path, items.concat().as_bytes())?,
    }
    Ok((0, items.iter().map(String::len).sum::<usize>() as u64, 0))
}

fn append_and_flush(path: &PathBuf, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(bytes)?;
    file.flush()?;
    file.sync_all()?;
    Ok(())
}

fn windows_executor_sample() -> Result<(u32, u64, u32)> {
    {
        let status = Command::new("cmd")
            .args(["/d", "/c", "ver"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        anyhow::ensure!(status.success(), "executor probe failed");
        Ok((0, 0, 0))
    }
}

fn summarize(samples: &[Sample]) -> VariantSummary {
    let durations = samples
        .iter()
        .map(|sample| sample.duration_ns as f64 / 1_000_000.0)
        .collect::<Vec<_>>();
    VariantSummary {
        median_ms: percentile(&durations, 0.5),
        p95_ms: percentile(&durations, 0.95),
        sampling_request_median: percentile(
            &samples
                .iter()
                .map(|sample| sample.sampling_requests as f64)
                .collect::<Vec<_>>(),
            0.5,
        ),
        failure_rate: samples.iter().filter(|sample| sample.failed).count() as f64
            / samples.len() as f64,
        serialized_bytes_median: percentile(
            &samples
                .iter()
                .map(|sample| sample.serialized_bytes as f64)
                .collect::<Vec<_>>(),
            0.5,
        ),
        cache_hits_median: percentile(
            &samples
                .iter()
                .map(|sample| sample.cache_hits as f64)
                .collect::<Vec<_>>(),
            0.5,
        ),
        exec_description_tokens_median: percentile(
            &samples
                .iter()
                .map(|sample| sample.exec_description_tokens as f64)
                .collect::<Vec<_>>(),
            0.5,
        ),
        prompt_input_tokens_median: percentile(
            &samples
                .iter()
                .map(|sample| sample.prompt_input_tokens as f64)
                .collect::<Vec<_>>(),
            0.5,
        ),
        tool_call_median: percentile(
            &samples
                .iter()
                .map(|sample| sample.tool_calls as f64)
                .collect::<Vec<_>>(),
            0.5,
        ),
    }
}

fn non_inferiority(
    baseline: &[Sample],
    candidate: &[Sample],
    absolute_margin_ms: f64,
    relative_margin: f64,
) -> NonInferiority {
    let absolute = baseline
        .iter()
        .zip(candidate)
        .map(|(baseline, candidate)| {
            (candidate.duration_ns as f64 - baseline.duration_ns as f64) / 1_000_000.0
        })
        .collect::<Vec<_>>();
    let relative = baseline
        .iter()
        .zip(candidate)
        .map(|(baseline, candidate)| {
            candidate.duration_ns as f64 / baseline.duration_ns.max(1) as f64 - 1.0
        })
        .collect::<Vec<_>>();
    let absolute_regression_ucb_ms = one_sided_95_ucb(&absolute);
    let relative_regression_ucb = one_sided_95_ucb(&relative);
    let sampling_request_mean_delta = mean(
        &candidate
            .iter()
            .map(|sample| sample.sampling_requests as f64)
            .collect::<Vec<_>>(),
    ) - mean(
        &baseline
            .iter()
            .map(|sample| sample.sampling_requests as f64)
            .collect::<Vec<_>>(),
    );
    let failure_rate_delta = candidate.iter().filter(|sample| sample.failed).count() as f64
        / candidate.len() as f64
        - baseline.iter().filter(|sample| sample.failed).count() as f64 / baseline.len() as f64;
    NonInferiority {
        absolute_regression_ucb_ms,
        relative_regression_ucb,
        sampling_request_mean_delta,
        failure_rate_delta,
        absolute_margin_ms,
        relative_margin,
        passed: absolute_regression_ucb_ms <= absolute_margin_ms
            && relative_regression_ucb <= relative_margin
            && sampling_request_mean_delta <= 0.0
            && failure_rate_delta <= 0.0,
    }
}

fn one_sided_95_ucb(values: &[f64]) -> f64 {
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    if values.len() == 1 {
        return mean;
    }
    let variance = values
        .iter()
        .map(|value| (value - mean).powi(2))
        .sum::<f64>()
        / (values.len() - 1) as f64;
    mean + 1.645 * (variance / values.len() as f64).sqrt()
}

fn mean(values: &[f64]) -> f64 {
    values.iter().sum::<f64>() / values.len() as f64
}

fn percentile(values: &[f64], quantile: f64) -> f64 {
    let mut values = values.to_vec();
    values.sort_by(f64::total_cmp);
    let index = ((values.len() - 1) as f64 * quantile).ceil() as usize;
    values[index]
}

fn parse_command() -> Result<BenchmarkCommand> {
    parse_command_from(env::args().skip(1))
}

fn parse_command_from(values: impl IntoIterator<Item = String>) -> Result<BenchmarkCommand> {
    let mut values = values.into_iter();
    let Some(first) = values.next() else {
        return parse_synthetic_args_from(std::iter::empty()).map(BenchmarkCommand::Synthetic);
    };
    if first == "code-mode-turn" {
        return parse_code_mode_args_from(values);
    }
    parse_synthetic_args_from(std::iter::once(first).chain(values)).map(BenchmarkCommand::Synthetic)
}

fn parse_code_mode_args_from(values: impl IntoIterator<Item = String>) -> Result<BenchmarkCommand> {
    let mut host = None;
    let mut values = values.into_iter();
    while let Some(flag) = values.next() {
        match flag.as_str() {
            "--code-mode-host" => {
                anyhow::ensure!(host.is_none(), "--code-mode-host supplied more than once");
                host = Some(PathBuf::from(
                    values.next().context("missing code-mode host path")?,
                ));
            }
            other => anyhow::bail!(
                "unknown code-mode-turn argument `{other}`; the workload is fixed and accepts only --code-mode-host <existing executable>"
            ),
        }
    }
    Ok(BenchmarkCommand::CodeModeCapture {
        host: host.context("code-mode-turn requires --code-mode-host <existing executable>")?,
    })
}

fn parse_synthetic_args_from(values: impl IntoIterator<Item = String>) -> Result<Args> {
    let mut args = Args {
        scenario: None,
        mode: None,
        iterations: DEFAULT_ITERATIONS,
        warmups: DEFAULT_WARMUPS,
        clusters: DEFAULT_CLUSTERS,
        absolute_margin_ms: 3.0,
        relative_margin: 0.03,
    };
    let mut values = values.into_iter();
    while let Some(flag) = values.next() {
        match flag.as_str() {
            "--scenario" => {
                args.scenario = Some(match values.next().context("missing scenario")?.as_str() {
                    "deterministic" => Scenario::Deterministic,
                    "loopback-websocket" => Scenario::LoopbackWebsocket,
                    "persistence" => Scenario::Persistence,
                    "windows-executor" => Scenario::WindowsExecutor,
                    other => anyhow::bail!("unknown scenario `{other}`"),
                });
            }
            "--mode" => {
                args.mode = Some(match values.next().context("missing mode")?.as_str() {
                    "cold" => Mode::Cold,
                    "warm" => Mode::Warm,
                    other => anyhow::bail!("unknown mode `{other}`"),
                });
            }
            "--iterations" => {
                args.iterations = values.next().context("missing iterations")?.parse()?
            }
            "--warmups" => args.warmups = values.next().context("missing warmups")?.parse()?,
            "--clusters" => args.clusters = values.next().context("missing clusters")?.parse()?,
            "--absolute-margin-ms" => {
                args.absolute_margin_ms =
                    values.next().context("missing absolute margin")?.parse()?
            }
            "--relative-margin" => {
                args.relative_margin = values.next().context("missing relative margin")?.parse()?
            }
            "--reliability" => args.iterations = RELIABILITY_ITERATIONS,
            other => anyhow::bail!("unknown argument `{other}`"),
        }
    }
    anyhow::ensure!(args.iterations > 0, "iterations must be positive");
    anyhow::ensure!(args.clusters > 0, "clusters must be positive");
    Ok(args)
}

#[cfg(test)]
#[allow(dead_code, unused_imports)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn code_mode_command_has_one_fixed_configuration() {
        let command = parse_command_from(strings(&[
            "code-mode-turn",
            "--code-mode-host",
            "code-mode-host",
        ]))
        .expect("fixed code-mode command should parse");

        let BenchmarkCommand::CodeModeCapture { host } = command else {
            panic!("expected code-mode capture command");
        };
        assert_eq!(host, PathBuf::from("code-mode-host"));

        let error = parse_command_from(strings(&[
            "code-mode-turn",
            "--code-mode-host",
            "code-mode-host",
            "--mode",
            "warm",
        ]))
        .expect_err("generic mode must not be part of code-mode capture");
        assert!(error.to_string().contains("workload is fixed"));
    }

    #[test]
    fn synthetic_scenarios_do_not_include_code_mode_capture() {
        let command = parse_command_from(strings(&["--scenario", "deterministic"]))
            .expect("synthetic command should parse");
        let BenchmarkCommand::Synthetic(args) = command else {
            panic!("expected synthetic command");
        };
        assert_eq!(args.scenario, Some(Scenario::Deterministic));

        let error = parse_command_from(strings(&["--scenario", "code-mode-turn"]))
            .expect_err("code mode must use its dedicated command");
        assert!(error.to_string().contains("unknown scenario"));
    }

    #[test]
    fn code_mode_capture_report_has_no_local_ab_gate() {
        let samples = vec![Sample {
            duration_ns: 1_000_000,
            ..Sample::default()
        }];
        let report = code_mode_capture_report(vec![CodeModeClusterReport {
            cluster: 1,
            capture: summarize(&samples),
            samples,
        }]);
        let value = serde_json::to_value(report).expect("capture report should serialize");

        assert_eq!(value["schema_version"], 4);
        assert_eq!(value["scenario"], "code_mode_turn");
        assert!(value["clusters"][0].get("capture").is_some());
        for paired_field in [
            "baseline",
            "candidate",
            "non_inferiority",
            "baseline_samples",
            "candidate_samples",
        ] {
            assert!(value["clusters"][0].get(paired_field).is_none());
        }
    }

    #[test]
    fn ready_to_sample_dispatch_gate_rejects_multi_second_stalls() {
        assert!(!ready_to_sample_dispatch_gate_passes(None));
        assert!(!ready_to_sample_dispatch_gate_passes(Some(
            5_000_000_000_u64
        )));
        assert!(ready_to_sample_dispatch_gate_passes(Some(500_000_000_u64)));
    }

    #[test]
    fn code_mode_workload_dispatches_exactly_one_nested_tool() {
        assert_eq!(
            CODE_MODE_NESTED_DISPATCH_SOURCE
                .matches("tools.update_plan")
                .count(),
            1
        );
        assert!(CODE_MODE_NESTED_DISPATCH_SOURCE.contains("dispatched"));
        assert!(!CODE_MODE_NESTED_DISPATCH_SOURCE.contains("completed"));
    }

    #[tokio::test]
    async fn deterministic_cache_hit_reuses_the_cached_serialization() {
        let mut state = ScenarioState::new().expect("scenario state should initialize");
        let (_, _, first_cache_hits) = deterministic_sample(Variant::Candidate, &mut state)
            .await
            .expect("cold candidate sample should succeed");
        assert_eq!(first_cache_hits, 0);

        state.serialized_schema = Some(vec![0; 7]);
        let (_, serialized_bytes, second_cache_hits) =
            deterministic_sample(Variant::Candidate, &mut state)
                .await
                .expect("warm candidate sample should succeed");

        assert_eq!(serialized_bytes, 7);
        assert_eq!(second_cache_hits, 1);
    }

    #[test]
    fn non_empty_samples_drive_summary_and_non_inferiority_statistics() {
        let baseline = [Sample {
            duration_ns: 1_000_000,
            sampling_requests: 2,
            ..Sample::default()
        }];
        let candidate = baseline;

        let summary = summarize(&baseline);
        let gate = non_inferiority(&baseline, &candidate, 0.0, 0.0);

        assert_eq!(summary.median_ms, 1.0);
        assert_eq!(summary.failure_rate, 0.0);
        assert_eq!(gate.absolute_regression_ucb_ms, 0.0);
        assert_eq!(gate.relative_regression_ucb, 0.0);
        assert!(gate.passed);
    }
}
