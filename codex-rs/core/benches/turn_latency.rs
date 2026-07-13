use anyhow::Context;
use anyhow::Result;
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
use std::path::PathBuf;
#[cfg(windows)]
use std::process::Command;
#[cfg(windows)]
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Scenario {
    Deterministic,
    LoopbackWebsocket,
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

#[derive(Clone, Copy, Debug, Default, Serialize)]
struct Sample {
    duration_ns: u64,
    sampling_requests: u32,
    failed: bool,
    serialized_bytes: u64,
    cache_hits: u32,
}

#[derive(Debug, Serialize)]
struct VariantSummary {
    median_ms: f64,
    p95_ms: f64,
    sampling_request_median: f64,
    failure_rate: f64,
    serialized_bytes_median: f64,
    cache_hits_median: f64,
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
    websocket_addr: Option<SocketAddr>,
    websocket: Option<ClientWebsocket>,
    schema_cached: bool,
    persistence_dir: TempDir,
}

impl ScenarioState {
    fn new(websocket_addr: Option<SocketAddr>) -> Result<Self> {
        Ok(Self {
            websocket_addr,
            websocket: None,
            schema_cached: false,
            persistence_dir: tempfile::tempdir()?,
        })
    }

    async fn preconnect(&mut self) -> Result<()> {
        if self.websocket.is_none() {
            let addr = self.websocket_addr.context("loopback address missing")?;
            let (websocket, _) = connect_async(format!("ws://{addr}")).await?;
            self.websocket = Some(websocket);
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = parse_args()?;
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
    let loopback = if scenario == Scenario::LoopbackWebsocket {
        Some(LoopbackServer::start().await?)
    } else {
        None
    };
    let addr = loopback.as_ref().map(|server| server.addr);
    let mut clusters = Vec::with_capacity(args.clusters);
    for cluster in 0..args.clusters {
        let mut rng = StdRng::seed_from_u64(0x4b4434_u64 + cluster as u64);
        let mut baseline_state = ScenarioState::new(addr)?;
        let mut candidate_state = ScenarioState::new(addr)?;
        if mode == Mode::Warm {
            for _ in 0..args.warmups {
                let _ = run_sample(scenario, Variant::Baseline, &mut baseline_state).await;
                let _ = run_sample(scenario, Variant::Candidate, &mut candidate_state).await;
            }
        }
        let mut baseline = Vec::with_capacity(args.iterations);
        let mut candidate = Vec::with_capacity(args.iterations);
        for _ in 0..args.iterations {
            let candidate_first = rng.random_bool(0.5);
            if mode == Mode::Cold {
                baseline_state = ScenarioState::new(addr)?;
                candidate_state = ScenarioState::new(addr)?;
            }
            if scenario == Scenario::LoopbackWebsocket && candidate_first {
                candidate_state.preconnect().await?;
            }
            if candidate_first {
                candidate
                    .push(run_sample(scenario, Variant::Candidate, &mut candidate_state).await);
                baseline.push(run_sample(scenario, Variant::Baseline, &mut baseline_state).await);
            } else {
                baseline.push(run_sample(scenario, Variant::Baseline, &mut baseline_state).await);
                if scenario == Scenario::LoopbackWebsocket {
                    candidate_state.preconnect().await?;
                }
                candidate
                    .push(run_sample(scenario, Variant::Candidate, &mut candidate_state).await);
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
        });
    }
    let passed = clusters
        .iter()
        .all(|cluster| cluster.non_inferiority.passed);
    Ok(Report {
        schema_version: 1,
        scenario,
        mode,
        warmups: if mode == Mode::Warm { args.warmups } else { 0 },
        measured_iterations_per_cluster: args.iterations,
        clusters,
        passed,
        limitation: "controlled local benchmark only; it does not establish real-model or Desktop-visible latency gains",
    })
}

async fn run_sample(scenario: Scenario, variant: Variant, state: &mut ScenarioState) -> Sample {
    let started = Instant::now();
    let result = match scenario {
        Scenario::Deterministic => deterministic_sample(variant, state).await,
        Scenario::LoopbackWebsocket => websocket_sample(variant, state).await,
        Scenario::Persistence => persistence_sample(variant, state),
        Scenario::WindowsExecutor => windows_executor_sample(),
    };
    match result {
        Ok((sampling_requests, serialized_bytes, cache_hits)) => Sample {
            duration_ns: started.elapsed().as_nanos().min(u64::MAX as u128) as u64,
            sampling_requests,
            failed: false,
            serialized_bytes,
            cache_hits,
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
    let payload = serde_json::json!({"tools": ["shell", "mcp"], "schema": {"type": "object"}});
    let serialized = serde_json::to_vec(&payload)?;
    let cache_hits = u32::from(variant == Variant::Candidate && state.schema_cached);
    state.schema_cached = variant == Variant::Candidate;
    Ok((2, serialized.len() as u64, cache_hits))
}

async fn websocket_sample(variant: Variant, state: &mut ScenarioState) -> Result<(u32, u64, u32)> {
    if variant == Variant::Baseline {
        state.websocket = None;
    }
    state.preconnect().await?;
    let websocket = state.websocket.as_mut().context("websocket unavailable")?;
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
    #[cfg(windows)]
    {
        let status = Command::new("cmd")
            .args(["/d", "/c", "ver"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        anyhow::ensure!(status.success(), "executor probe failed");
        Ok((0, 0, 0))
    }
    #[cfg(not(windows))]
    anyhow::bail!("Windows-only scenario")
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
            / samples.len().max(1) as f64,
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
        / candidate.len().max(1) as f64
        - baseline.iter().filter(|sample| sample.failed).count() as f64
            / baseline.len().max(1) as f64;
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
    if values.is_empty() {
        return f64::INFINITY;
    }
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
    values.iter().sum::<f64>() / values.len().max(1) as f64
}

fn percentile(values: &[f64], quantile: f64) -> f64 {
    if values.is_empty() {
        return f64::NAN;
    }
    let mut values = values.to_vec();
    values.sort_by(f64::total_cmp);
    let index = ((values.len() - 1) as f64 * quantile).ceil() as usize;
    values[index]
}

fn parse_args() -> Result<Args> {
    let mut args = Args {
        scenario: None,
        mode: None,
        iterations: DEFAULT_ITERATIONS,
        warmups: DEFAULT_WARMUPS,
        clusters: DEFAULT_CLUSTERS,
        absolute_margin_ms: 3.0,
        relative_margin: 0.03,
    };
    let mut values = env::args().skip(1);
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
