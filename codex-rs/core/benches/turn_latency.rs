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
use codex_protocol::models::PermissionProfile;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::TaskCompletionStatus;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnTiming;
use codex_protocol::protocol::TurnTimingToolCall;
use codex_protocol::protocol::TurnTimingToolCallSource;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ResponseMock;
use core_test_support::responses::ResponsesRequest;
use core_test_support::responses::ev_apply_patch_custom_tool_call;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_custom_tool_call;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use core_test_support::test_codex::turn_permission_fields;
use futures::SinkExt;
use futures::StreamExt;
use rand::Rng;
use rand::SeedableRng;
use rand::rngs::StdRng;
use regex_lite::Regex;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::fs::OpenOptions;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Read;
use std::io::Write;
use std::net::SocketAddr;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::ChildStdin;
use std::process::ChildStdout;
use std::process::Command;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::mpsc::Receiver;
use std::sync::mpsc::RecvTimeoutError;
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
const DEFAULT_CLUSTERS: usize = 5;
const RELIABILITY_ITERATIONS: usize = 600;
const CODE_MODE_WARMUPS: usize = 5;
const CODE_MODE_ITERATIONS: usize = 30;
const CODE_MODE_CLUSTERS: usize = 3;
const AB_WARMUPS: usize = 3;
const AB_ITERATIONS: usize = 30;
const AB_CLUSTERS: usize = 3;
const AB_BOOTSTRAP_REPLICATES: usize = 10_000;
const AB_BOOTSTRAP_SEED: u64 = 0x4b44_345f_4142_7631;
const AB_FAMILY_WISE_ALPHA: f64 = 0.05;
const AB_QUICK_LOOKS: [usize; 1] = [10];
const AB_BATCH_LOOKS: [usize; 1] = [20];
const AB_FINAL_LOOKS: [usize; 3] = [10, 20, 30];
const AB_REPLAY_LOOKS: [usize; 1] = [10];
const AB_CORRECTNESS_ONLY_LOOKS: [usize; 1] = [1];
const AB_MEDIAN_RATIO_UCB_LIMIT: f64 = 0.75;
const AB_P95_RATIO_UCB_LIMIT: f64 = 1.00;
const AB_RATIO_TARGET: f64 = 0.50;
const AB_WORKLOAD_SCHEMA_VERSION: u16 = 15;
const AB_BASELINE_STATE_SCHEMA_VERSION: u16 = 2;
const AB_FILTERED_TREE_IDENTITY_VERSION: u16 = 1;
const AB_METRIC_GATE_VERSION: u16 = 16;
const AB_REPORT_SCHEMA_VERSION: u16 = 17;
const AB_REPLAY_SESSION_AUDIT_EVIDENCE_VERSION: u16 = 1;
const AB_PREPARED_MANIFEST_SCHEMA_VERSION: u16 = 1;
const AB_WORKER_STACK_BYTES: &str = "16777216";
const AB_OVERLAY_REPOSITORY_PATH: &[u8] = b"codex-rs/core/benches/turn_latency.rs";
const MAX_READY_TO_SAMPLE_TO_DISPATCH_NS: u64 = 1_000_000_000;
const AB_HIGH_VOLUME_SUBTURNS: usize = 16;
const AB_HIGH_VOLUME_DIRECT_CALLS_PER_GENERATION: usize = 2;
const AB_HIGH_VOLUME_NESTED_CALLS_PER_GENERATION: usize = 3;
const AB_REPLAY_PAIRS: usize = 10;
const AB_REPLAY_A_GENERATIONS: u32 = 15;
const AB_REPLAY_B_GENERATIONS: u32 = 10;
const AB_LONG_HISTORY_TURNS: usize = 32;
const AB_LONG_HISTORY_SEED_BYTES: usize = 512;
const AB_REQUEST_COMPONENT_NAMES: [&str; 5] = [
    "instructions",
    "tool_schemas",
    "history",
    "current_input",
    "prompt_cache_key",
];
const AB_LONG_HISTORY_NO_TOOL_PROMPT: &str =
    "Answer the deterministic long-history benchmark without calling tools.";
const AB_LONG_HISTORY_NO_TOOL_REPLY: &str = "long-history no-tool benchmark complete";
const AB_LONG_HISTORY_TOOL_PROMPT: &str =
    "Update the deterministic benchmark plan once, then report completion.";
const AB_LONG_HISTORY_TOOL_REPLY: &str = "long-history tool continuation complete";
const AB_STABLE_CONTEXT_PROMPT: &str = "Repeat the stable request-cache benchmark response.";
const AB_STABLE_CONTEXT_REPLY: &str = "stable request-cache benchmark complete";
const AB_CONTEXT_CHANGE_PROMPT_A: &str = "Run context invalidation probe alpha.";
const AB_CONTEXT_CHANGE_PROMPT_B: &str = "Run context invalidation probe beta.";
const AB_CONTEXT_CHANGE_REPLY: &str = "context invalidation benchmark complete";
const AB_SINGLE_DIRECT_PROMPT: &str = "Run the deterministic single direct tool benchmark.";
const AB_SINGLE_DIRECT_REPLY: &str = "single direct tool benchmark complete";
const AB_PARALLEL_TRIPLE_PROMPT: &str =
    "Run exactly three deterministic parallel-safe direct tool calls.";
const AB_PARALLEL_TRIPLE_REPLY: &str = "parallel-safe triple benchmark complete";
const AB_EXCLUSIVE_GATE_PROMPT: &str =
    "Run two same-workspace exec calls and one unrelated parallel-safe direct tool call.";
const AB_EXCLUSIVE_GATE_REPLY: &str = "exclusive-gate benchmark complete";
const AB_EXCLUSIVE_GATE_CHILD_MARKER: &str = "__KD4_EXCLUSIVE_GATE_CHILD_COMPLETE__";
const AB_EXCLUSIVE_GATE_CHILD_DELAY_MS: u64 = 100;
const AB_EXCLUSIVE_GATE_YIELD_TIME_MS: u64 = 10_000;
const AB_RETAINED_EXEC_PROMPT: &str =
    "Run the deterministic retained exec lifecycle through exactly two write_stdin polls.";
const AB_RETAINED_EXEC_REPLY: &str = "retained exec lifecycle complete";
const AB_ABORT_DIRECT_NESTED_PROMPT: &str =
    "Run one CodeMode tool that reaches the deterministic permission barrier.";
const AB_ABORT_FORBIDDEN_RESUME_REPLY: &str =
    "forbidden model resume after abort-direct-nested interrupt";
const AB_ABORT_RETAINED_PROMPT: &str =
    "Start the deterministic retained process and wait for an explicit interrupt.";
const AB_ABORT_RETAINED_FORBIDDEN_RESUME_REPLY: &str =
    "forbidden model resume after retained-process interrupt";
const AB_ABORT_RETAINED_YIELD_TIME_MS: u64 = 10;
const AB_ABORT_DIRECT_NESTED_SOURCE: &str = r#"// @exec: {"yield_time_ms": 1000}
await tools.request_permissions({
  reason: "benchmark interrupt barrier",
  permissions: { network: { enabled: true } },
});"#;
const AB_RETAINED_READY_MARKER: &str = "__KD4_RETAINED_READY__";
const AB_RETAINED_POLL_MARKER: &str = "__KD4_RETAINED_POLL_ACK__";
const AB_RETAINED_FINISHED_MARKER: &str = "__KD4_RETAINED_FINISHED__";
const AB_HISTORY_SEED_PREFIX: &str = "request-cache-history-seed-";
const AB_HISTORY_SEED_REPLY: &str = "request-cache deterministic seed reply";
const CODE_MODE_HIGH_VOLUME_PROMPT: &str =
    "Run one deterministic high-volume CodeMode dispatch subturn.";
const CODE_MODE_HIGH_VOLUME_FOLLOW_UP: &str = "high-volume CodeMode dispatch complete";
const CODE_MODE_NESTED_DISPATCH_SOURCE: &str = r#"
const dispatched = await tools.update_plan({
  plan: [{ step: "benchmark nested dispatch", status: "in_progress" }],
});
text(JSON.stringify({
  dispatched: typeof dispatched?.message === "string",
}));
"#;
const CODE_MODE_HIGH_VOLUME_SINGLE_NESTED_SOURCE: &str = r#"
await tools.update_plan({
  plan: [{ step: "benchmark high-volume nested dispatch one", status: "in_progress" }],
});
"#;
const CODE_MODE_HIGH_VOLUME_DOUBLE_NESTED_SOURCE: &str = r#"
await Promise.all([
  tools.update_plan({
    plan: [{ step: "benchmark high-volume nested dispatch two-a", status: "in_progress" }],
  }),
  tools.update_plan({
    plan: [{ step: "benchmark high-volume nested dispatch two-b", status: "in_progress" }],
  }),
]);
"#;

#[cfg(test)]
static AB_BUILD_COMMAND_INVOCATIONS: AtomicUsize = AtomicUsize::new(0);

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

include!("turn_latency/ab_contract.rs");

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
                ev_completed_with_usage(&response_id, 1_024, 768, 24, 16),
            ]));
            let completion_id = format!("{fixture_id}-completion-{turn}");
            sequence.push(sse(vec![
                ev_assistant_message(&completion_id, "done"),
                ev_completed_with_usage(&completion_id, 1_280, 1_024, 8, 0),
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
                let mut sample = timing.map(sample_from_timing).unwrap_or_default();
                sample.duration_ns = duration_ns;
                let semantic_output_ok =
                    turn_requests.get(1).is_some_and(nested_output_is_expected);
                let max_ready_to_sample_to_dispatch_ns =
                    timing.and_then(max_ready_to_sample_to_dispatch_ns);
                if completion.last_agent_message.as_deref() != Some("done") {
                    sample.failure_codes.push("wrong_final_message".to_string());
                }
                if completion.error.is_some() {
                    sample
                        .failure_codes
                        .push("unexpected_terminal_error".to_string());
                }
                if turn_requests.len() != 2 {
                    sample.failure_codes.push("request_count".to_string());
                }
                if sample.sampling_requests != 2 {
                    sample.failure_codes.push("generation_count".to_string());
                }
                if sample.tool_calls != 2
                    || sample.direct_tool_calls != 1
                    || sample.nested_tool_calls != 1
                {
                    sample.failure_codes.push("tool_graph".to_string());
                }
                if !timing.is_some_and(timing_reconciles) {
                    sample
                        .failure_codes
                        .push("timing_reconciliation".to_string());
                }
                if !semantic_output_ok {
                    sample.failure_codes.push("nested_output".to_string());
                }
                if !ready_to_sample_dispatch_gate_passes(max_ready_to_sample_to_dispatch_ns) {
                    sample.failure_codes.push("post_tool_handoff".to_string());
                }
                sample.serialized_bytes = first_request
                    .map(|request| request.body_bytes().len() as u64)
                    .unwrap_or_default();
                sample.cache_hits = sample.workspace_evidence_cache_hits;
                sample.exec_description_tokens = first_request
                    .map(exec_description_tokens)
                    .unwrap_or_default();
                sample.prompt_input_tokens =
                    first_request.map(prompt_input_tokens).unwrap_or_default();
                sample.max_ready_to_sample_to_dispatch_ns = max_ready_to_sample_to_dispatch_ns;
                sample.failed = !sample.failure_codes.is_empty();
                sample
            }
            Err(error) => Sample {
                duration_ns,
                failed: true,
                failure_codes: vec![format!("completion_error:{error}")],
                ..Sample::default()
            },
        }
    }
}

include!("turn_latency/runtime_fixtures.rs");

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
        BenchmarkCommand::AbCapture(args) => capture_ab_baseline(&args),
        BenchmarkCommand::AbPrepare(args) => run_ab_prepare(&args),
        BenchmarkCommand::AbCompare(args) => run_ab_compare(&args),
        BenchmarkCommand::AbImportReport(args) => {
            let receipt = import_accepted_ab_report(&args)?;
            println!("{}", serde_json::to_string(&receipt)?);
            Ok(())
        }
        BenchmarkCommand::AbWorker(args) => run_ab_worker(&args).await,
        BenchmarkCommand::AbExclusiveGateChild => run_ab_exclusive_gate_child(),
        BenchmarkCommand::AbRetainedChild => run_ab_retained_child(),
        BenchmarkCommand::AbReplayCommand { mode, paths } => run_ab_replay_command(&mode, &paths),
        BenchmarkCommand::Synthetic(args) => run_synthetic_reports(args).await,
    }
}

async fn run_synthetic_reports(args: Args) -> Result<()> {
    let scenarios = args
        .scenario
        .map_or_else(default_synthetic_scenarios, |scenario| vec![scenario]);
    let modes = args
        .mode
        .map_or_else(|| vec![Mode::Cold, Mode::Warm], |mode| vec![mode]);
    let mut any_failed = false;
    for scenario in scenarios {
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

fn default_synthetic_scenarios() -> Vec<Scenario> {
    vec![
        Scenario::Deterministic,
        Scenario::LoopbackWebsocket,
        Scenario::Persistence,
        Scenario::WindowsExecutor,
    ]
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
    let all_samples_succeeded = baseline
        .iter()
        .chain(candidate)
        .all(|sample| !sample.failed);
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
            && failure_rate_delta <= 0.0
            && all_samples_succeeded,
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

include!("turn_latency/ab_runner.rs");

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
    if first == "ab-capture" {
        return parse_ab_capture_args_from(values);
    }
    if first == "ab-prepare" {
        return parse_ab_prepare_args_from(values);
    }
    if first == "ab-compare" {
        return parse_ab_compare_args_from(values);
    }
    if first == "ab-import-report" {
        return parse_ab_import_report_args_from(values);
    }
    if first == "ab-worker" {
        return parse_ab_worker_args_from(values);
    }
    if first == "ab-exclusive-gate-child" {
        anyhow::ensure!(
            values.next().is_none(),
            "ab-exclusive-gate-child accepts no arguments"
        );
        return Ok(BenchmarkCommand::AbExclusiveGateChild);
    }
    if first == "ab-retained-child" {
        anyhow::ensure!(
            values.next().is_none(),
            "ab-retained-child accepts no arguments"
        );
        return Ok(BenchmarkCommand::AbRetainedChild);
    }
    if first == "ab-replay-command" {
        let mode = values.next().context("ab-replay-command requires a mode")?;
        let paths = values.map(PathBuf::from).collect::<Vec<_>>();
        anyhow::ensure!(
            !paths.is_empty(),
            "ab-replay-command requires at least one path"
        );
        return Ok(BenchmarkCommand::AbReplayCommand { mode, paths });
    }
    parse_synthetic_args_from(std::iter::once(first).chain(values)).map(BenchmarkCommand::Synthetic)
}

fn parse_flag_once<T>(
    slot: &mut Option<T>,
    flag: &str,
    parse_value: impl FnOnce() -> Result<T>,
) -> Result<()> {
    anyhow::ensure!(slot.is_none(), "{flag} supplied more than once");
    *slot = Some(parse_value()?);
    Ok(())
}

fn parse_ab_import_report_args_from(
    values: impl IntoIterator<Item = String>,
) -> Result<BenchmarkCommand> {
    let mut report = None;
    let mut repo = None;
    let mut values = values.into_iter();
    while let Some(flag) = values.next() {
        match flag.as_str() {
            "--report" => {
                parse_flag_once(&mut report, "--report", || {
                    Ok(PathBuf::from(values.next().context("missing report path")?))
                })?;
            }
            "--repo" => {
                parse_flag_once(&mut repo, "--repo", || {
                    Ok(PathBuf::from(values.next().context("missing repo path")?))
                })?;
            }
            other => anyhow::bail!("unknown ab-import-report argument `{other}`"),
        }
    }
    Ok(BenchmarkCommand::AbImportReport(AbImportReportArgs {
        report: report.context("ab-import-report requires --report <path>")?,
        repo: repo.unwrap_or_else(|| PathBuf::from(".")),
    }))
}

fn parse_ab_capture_args_from(
    values: impl IntoIterator<Item = String>,
) -> Result<BenchmarkCommand> {
    let mut repo = None;
    let mut state = None;
    let mut values = values.into_iter();
    while let Some(flag) = values.next() {
        match flag.as_str() {
            "--repo" => parse_flag_once(&mut repo, "--repo", || {
                Ok(PathBuf::from(values.next().context("missing repo path")?))
            })?,
            "--state" => parse_flag_once(&mut state, "--state", || {
                Ok(PathBuf::from(values.next().context("missing state path")?))
            })?,
            other => anyhow::bail!("unknown ab-capture argument `{other}`"),
        }
    }
    Ok(BenchmarkCommand::AbCapture(AbCaptureArgs {
        repo: repo.unwrap_or_else(|| PathBuf::from(".")),
        state: state.context("ab-capture requires --state <path>")?,
    }))
}

fn parse_ab_prepare_args_from(
    values: impl IntoIterator<Item = String>,
) -> Result<BenchmarkCommand> {
    let mut state = None;
    let mut candidate_repo = None;
    let mut work_root = None;
    let mut manifest = None;
    let mut baseline_target_dir = None;
    let mut candidate_target_dir = None;
    let mut reuse_work_root = None;
    let mut values = values.into_iter();
    while let Some(flag) = values.next() {
        match flag.as_str() {
            "--state" => parse_flag_once(&mut state, "--state", || {
                Ok(PathBuf::from(values.next().context("missing state path")?))
            })?,
            "--candidate-repo" => {
                parse_flag_once(&mut candidate_repo, "--candidate-repo", || {
                    Ok(PathBuf::from(
                        values.next().context("missing candidate repo path")?,
                    ))
                })?;
            }
            "--work-root" => {
                parse_flag_once(&mut work_root, "--work-root", || {
                    Ok(PathBuf::from(values.next().context("missing work root")?))
                })?;
            }
            "--manifest" => {
                parse_flag_once(&mut manifest, "--manifest", || {
                    Ok(PathBuf::from(
                        values.next().context("missing manifest path")?,
                    ))
                })?;
            }
            "--baseline-target-dir" => {
                parse_flag_once(&mut baseline_target_dir, "--baseline-target-dir", || {
                    Ok(PathBuf::from(
                        values.next().context("missing baseline target directory")?,
                    ))
                })?;
            }
            "--candidate-target-dir" => {
                parse_flag_once(&mut candidate_target_dir, "--candidate-target-dir", || {
                    Ok(PathBuf::from(
                        values
                            .next()
                            .context("missing candidate target directory")?,
                    ))
                })?;
            }
            "--reuse-work-root" => {
                parse_flag_once(&mut reuse_work_root, "--reuse-work-root", || Ok(()))?;
            }
            other => anyhow::bail!("unknown ab-prepare argument `{other}`"),
        }
    }
    anyhow::ensure!(
        baseline_target_dir.is_some() == candidate_target_dir.is_some(),
        "ab-prepare requires --baseline-target-dir and --candidate-target-dir together"
    );
    Ok(BenchmarkCommand::AbPrepare(AbPrepareArgs {
        state: state.context("ab-prepare requires --state <path>")?,
        candidate_repo: candidate_repo.unwrap_or_else(|| PathBuf::from(".")),
        work_root: work_root.context("ab-prepare requires --work-root <path>")?,
        manifest: manifest.context("ab-prepare requires --manifest <path>")?,
        baseline_target_dir,
        candidate_target_dir,
        reuse_work_root: reuse_work_root.is_some(),
    }))
}

fn parse_ab_compare_args_from(
    values: impl IntoIterator<Item = String>,
) -> Result<BenchmarkCommand> {
    let mut manifest = None;
    let mut report = None;
    let mut profile = None;
    let mut requested_workloads = Vec::new();
    let mut values = values.into_iter();
    while let Some(flag) = values.next() {
        match flag.as_str() {
            "--manifest" => {
                parse_flag_once(&mut manifest, "--manifest", || {
                    Ok(PathBuf::from(
                        values.next().context("missing manifest path")?,
                    ))
                })?;
            }
            "--report" => {
                parse_flag_once(&mut report, "--report", || {
                    Ok(PathBuf::from(values.next().context("missing report path")?))
                })?;
            }
            "--profile" => {
                parse_flag_once(&mut profile, "--profile", || {
                    AbExecutionProfile::parse(&values.next().context("missing execution profile")?)
                })?;
            }
            "--workload" => {
                let workload = AbWorkload::parse(&values.next().context("missing workload")?)?;
                anyhow::ensure!(
                    !requested_workloads.contains(&workload),
                    "duplicate A/B workload selection `{}`",
                    workload.name()
                );
                requested_workloads.push(workload);
            }
            other => anyhow::bail!("unknown ab-compare argument `{other}`"),
        }
    }
    let profile = profile.context("ab-compare requires --profile <quick|batch|final|replay>")?;
    ab_profile_workloads(profile, &requested_workloads)?;
    Ok(BenchmarkCommand::AbCompare(AbCompareArgs {
        manifest: manifest.context("ab-compare requires --manifest <path>")?,
        report: report.context("ab-compare requires --report <path>")?,
        profile,
        requested_workloads,
    }))
}

fn parse_ab_worker_args_from(values: impl IntoIterator<Item = String>) -> Result<BenchmarkCommand> {
    let mut code_mode_host = None;
    let mut variant = None;
    let mut cluster = None;
    let mut workload = None;
    let mut warmups = None;
    let mut samples = None;
    let mut values = values.into_iter();
    while let Some(flag) = values.next() {
        match flag.as_str() {
            "--code-mode-host" => {
                parse_flag_once(&mut code_mode_host, "--code-mode-host", || {
                    Ok(PathBuf::from(
                        values.next().context("missing code mode host path")?,
                    ))
                })?;
            }
            "--variant" => parse_flag_once(&mut variant, "--variant", || {
                values.next().context("missing variant")
            })?,
            "--cluster" => {
                parse_flag_once(&mut cluster, "--cluster", || {
                    Ok(values.next().context("missing cluster")?.parse::<usize>()?)
                })?;
            }
            "--workload" => {
                parse_flag_once(&mut workload, "--workload", || {
                    AbWorkload::parse(&values.next().context("missing workload")?)
                })?;
            }
            "--warmups" => {
                parse_flag_once(&mut warmups, "--warmups", || {
                    Ok(values.next().context("missing warmups")?.parse::<usize>()?)
                })?;
            }
            "--samples" => {
                parse_flag_once(&mut samples, "--samples", || {
                    Ok(values.next().context("missing samples")?.parse::<usize>()?)
                })?;
            }
            other => anyhow::bail!("unknown ab-worker argument `{other}`"),
        }
    }
    let variant = variant.context("ab-worker requires --variant <A|B>")?;
    anyhow::ensure!(
        variant == "A" || variant == "B",
        "worker variant must be A or B"
    );
    let cluster = cluster.context("ab-worker requires --cluster <n>")?;
    anyhow::ensure!(cluster > 0, "worker cluster is out of range");
    let warmups = warmups.context("ab-worker requires --warmups <n>")?;
    let samples = samples.context("ab-worker requires --samples <n>")?;
    let workload = workload.context("ab-worker requires --workload <name>")?;
    anyhow::ensure!(samples > 0, "worker samples must be positive");
    Ok(BenchmarkCommand::AbWorker(AbWorkerArgs {
        code_mode_host: code_mode_host.context("ab-worker requires --code-mode-host <path>")?,
        variant,
        cluster,
        workload,
        warmups,
        samples,
    }))
}

fn parse_code_mode_args_from(values: impl IntoIterator<Item = String>) -> Result<BenchmarkCommand> {
    let mut host = None;
    let mut values = values.into_iter();
    while let Some(flag) = values.next() {
        match flag.as_str() {
            "--code-mode-host" => {
                parse_flag_once(&mut host, "--code-mode-host", || {
                    Ok(PathBuf::from(
                        values.next().context("missing code-mode host path")?,
                    ))
                })?;
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
    let mut scenario = None;
    let mut mode = None;
    let mut iterations = None;
    let mut warmups = None;
    let mut clusters = None;
    let mut absolute_margin_ms = None;
    let mut relative_margin = None;
    let mut values = values.into_iter();
    while let Some(flag) = values.next() {
        match flag.as_str() {
            "--scenario" => {
                parse_flag_once(&mut scenario, "--scenario", || {
                    Ok(match values.next().context("missing scenario")?.as_str() {
                        "deterministic" => Scenario::Deterministic,
                        "loopback-websocket" => Scenario::LoopbackWebsocket,
                        "persistence" => Scenario::Persistence,
                        "windows-executor" => Scenario::WindowsExecutor,
                        other => anyhow::bail!("unknown scenario `{other}`"),
                    })
                })?;
            }
            "--mode" => {
                parse_flag_once(&mut mode, "--mode", || {
                    Ok(match values.next().context("missing mode")?.as_str() {
                        "cold" => Mode::Cold,
                        "warm" => Mode::Warm,
                        other => anyhow::bail!("unknown mode `{other}`"),
                    })
                })?;
            }
            "--iterations" => {
                parse_flag_once(&mut iterations, "--iterations", || {
                    Ok(values.next().context("missing iterations")?.parse()?)
                })?;
            }
            "--warmups" => {
                parse_flag_once(&mut warmups, "--warmups", || {
                    Ok(values.next().context("missing warmups")?.parse()?)
                })?;
            }
            "--clusters" => {
                parse_flag_once(&mut clusters, "--clusters", || {
                    Ok(values.next().context("missing clusters")?.parse()?)
                })?;
            }
            "--absolute-margin-ms" => {
                parse_flag_once(&mut absolute_margin_ms, "--absolute-margin-ms", || {
                    Ok(values.next().context("missing absolute margin")?.parse()?)
                })?;
            }
            "--relative-margin" => {
                parse_flag_once(&mut relative_margin, "--relative-margin", || {
                    Ok(values.next().context("missing relative margin")?.parse()?)
                })?;
            }
            "--reliability" => parse_flag_once(&mut iterations, "--reliability", || {
                Ok(RELIABILITY_ITERATIONS)
            })?,
            other => anyhow::bail!("unknown argument `{other}`"),
        }
    }
    let args = Args {
        scenario,
        mode,
        iterations: iterations.unwrap_or(DEFAULT_ITERATIONS),
        warmups: warmups.unwrap_or(DEFAULT_WARMUPS),
        clusters: clusters.unwrap_or(DEFAULT_CLUSTERS),
        absolute_margin_ms: absolute_margin_ms.unwrap_or(3.0),
        relative_margin: relative_margin.unwrap_or(0.03),
    };
    anyhow::ensure!(args.iterations > 0, "iterations must be positive");
    anyhow::ensure!(args.clusters > 0, "clusters must be positive");
    anyhow::ensure!(
        args.absolute_margin_ms.is_finite() && args.absolute_margin_ms >= 0.0,
        "absolute margin must be finite and non-negative"
    );
    anyhow::ensure!(
        args.relative_margin.is_finite() && args.relative_margin >= 0.0,
        "relative margin must be finite and non-negative"
    );
    Ok(args)
}

#[cfg(test)]
#[allow(dead_code, unused_imports)]
mod tests {
    include!("turn_latency/tests.rs");
}
