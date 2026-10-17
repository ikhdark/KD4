// PRESERVED BENCHMARK HARNESS, NOT REGISTERED IN TEST SUITE.
// Compiled and run on 2026-10-16 as a Windows-only child module of
// codex-rs/core/src/unified_exec/process_manager.rs, declared at the end of that file:
//
//     #[cfg(all(test, windows))]
//     #[path = "backend_route_bench_tests.rs"]
//     mod backend_route_bench_tests;
//
// To rerun, copy this file to codex-rs/core/src/unified_exec/backend_route_bench_tests.rs
// without this header, add the declaration, and run from the repository root:
//
//     NEXTEST_TEST_THREADS=1 CODEX_BACKEND_ROUTE_BENCH_OUT='C:\path\results.jsonl' \
//       just core-test-fast core_lib --run-ignored only --success-output immediate \
//       -E 'test(/route_bench_/)'
//
// The output path must be a native Windows path: `record` panics when it cannot open
// the file, after printing the `BENCH {json}` line to stderr. The recorded runs hit that
// panic (POSIX path), so their JSONL was reconstructed from those stderr lines.
// Results and analysis: .codex/audits/backend-route-bench-2026-10-16/.
// The interrupt scenario fails on both routes: neither process backend supports
// interrupt for these processes.

//! Temporary equivalence benchmark: the direct local route used by unified exec
//! today versus the same request through `ExecBackend` (`LocalProcess`), the route
//! a consolidation would use. Ignored by default; results are JSON lines.

use super::*;
use crate::exec::ExecCapturePolicy;
use crate::exec::ExecExpiration;
use crate::tools::command_output_artifact::RawOutputArtifact;
use crate::unified_exec::NoopSpawnLifecycle;
use codex_protocol::config_types::WindowsSandboxLevel;
use codex_protocol::models::PermissionProfile;
use codex_sandboxing::SandboxType;
use std::io::Write as _;
use std::path::PathBuf;
use std::time::Instant as StdInstant;

#[derive(Clone, Copy, Debug)]
enum Route {
    Direct,
    Backend,
}

impl Route {
    fn name(self) -> &'static str {
        match self {
            Route::Direct => "direct",
            Route::Backend => "backend",
        }
    }
}

// PROCESS_MEMORY_COUNTERS_EX layout; only some counters are reported.
#[allow(dead_code)]
#[repr(C)]
#[derive(Default)]
struct ProcessMemoryCounters {
    cb: u32,
    page_fault_count: u32,
    peak_working_set_size: usize,
    working_set_size: usize,
    quota_peak_paged_pool_usage: usize,
    quota_paged_pool_usage: usize,
    quota_peak_non_paged_pool_usage: usize,
    quota_non_paged_pool_usage: usize,
    pagefile_usage: usize,
    peak_pagefile_usage: usize,
    private_usage: usize,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentProcess() -> *mut std::ffi::c_void;
    fn K32GetProcessMemoryInfo(
        process: *mut std::ffi::c_void,
        counters: *mut ProcessMemoryCounters,
        cb: u32,
    ) -> i32;
}

fn memory() -> ProcessMemoryCounters {
    let mut counters = ProcessMemoryCounters {
        cb: std::mem::size_of::<ProcessMemoryCounters>() as u32,
        ..Default::default()
    };
    // SAFETY: the current-process pseudo-handle is always valid and `cb` matches the buffer.
    let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) };
    assert_ne!(ok, 0, "K32GetProcessMemoryInfo failed");
    counters
}

fn mib(bytes: i64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

fn stats(samples: &mut [f64]) -> serde_json::Value {
    samples.sort_by(f64::total_cmp);
    let pick = |q: f64| samples[((samples.len() - 1) as f64 * q).round() as usize];
    let mean = samples.iter().sum::<f64>() / samples.len() as f64;
    serde_json::json!({
        "n": samples.len(),
        "min": samples[0],
        "median": pick(0.5),
        "p90": pick(0.9),
        "max": samples[samples.len() - 1],
        "mean": mean,
    })
}

fn record(route: Route, scenario: &str, mut value: serde_json::Value) {
    value["route"] = route.name().into();
    value["scenario"] = scenario.into();
    let line = value.to_string();
    eprintln!("BENCH {line}");
    let path = std::env::var_os("CODEX_BACKEND_ROUTE_BENCH_OUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("codex-backend-route-bench.jsonl"));
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    writeln!(file, "{line}").unwrap();
}

struct Bench {
    temp: tempfile::TempDir,
    home: AbsolutePathBuf,
    cwd: AbsolutePathBuf,
    environment: codex_exec_server::Environment,
    manager: UnifiedExecProcessManager,
    env: HashMap<String, String>,
    exec_server_env_config: ExecServerEnvConfig,
    next_process_id: u32,
}

impl Bench {
    fn new() -> anyhow::Result<Self> {
        let temp = tempfile::tempdir()?;
        let home = temp.path().join("home");
        let cwd = temp.path().join("work");
        std::fs::create_dir_all(&home)?;
        std::fs::create_dir_all(&cwd)?;
        // Mirror open_session_with_sandbox: a policy-built base plus a request overlay.
        let policy = ShellEnvironmentPolicy::default();
        let local_policy_env = create_env(&policy, /*thread_id*/ None);
        let mut env = local_policy_env.clone();
        env.insert("CODEX_BACKEND_ROUTE_BENCH".to_string(), "1".to_string());
        Ok(Self {
            home: AbsolutePathBuf::from_absolute_path(&home)?,
            cwd: AbsolutePathBuf::from_absolute_path(&cwd)?,
            temp,
            environment: codex_exec_server::Environment::default_for_tests(),
            manager: UnifiedExecProcessManager::default(),
            env,
            exec_server_env_config: ExecServerEnvConfig {
                policy: exec_env_policy_from_shell_policy(&policy),
                local_policy_env,
            },
            next_process_id: 1_000,
        })
    }

    fn fixture(&self, name: &str, bytes: usize) -> anyhow::Result<PathBuf> {
        let path = self.temp.path().join(name);
        let line = format!("{}\r\n", "x".repeat(98));
        let mut data = line.repeat(bytes / line.len() + 1).into_bytes();
        data.truncate(bytes);
        std::fs::write(&path, data)?;
        Ok(path)
    }

    fn request(&self, command: &[&str]) -> ExecRequest {
        let mut request = ExecRequest::new(
            command.iter().map(|arg| (*arg).to_string()).collect(),
            self.home.clone(),
            self.cwd.clone(),
            self.env.clone(),
            /*network*/ None,
            /*network_environment_id*/ None,
            ExecExpiration::DefaultTimeout,
            ExecCapturePolicy::ShellTool,
            SandboxType::None,
            vec![self.cwd.clone()],
            WindowsSandboxLevel::Disabled,
            /*windows_sandbox_private_desktop*/ false,
            PermissionProfile::Disabled,
            /*arg0*/ None,
        );
        request.exec_server_env_config = Some(self.exec_server_env_config.clone());
        request
    }

    async fn spawn(
        &mut self,
        route: Route,
        request: &ExecRequest,
        tty: bool,
        artifact: Option<RawOutputArtifact>,
    ) -> anyhow::Result<Arc<UnifiedExecProcess>> {
        self.next_process_id += 1;
        let process_id = self.next_process_id;
        let pending = PendingSpawnRegistration::default();
        match route {
            Route::Direct => Ok(self
                .manager
                .open_session_with_prepared_exec_env(
                    process_id,
                    request,
                    tty,
                    Box::new(NoopSpawnLifecycle),
                    artifact,
                    &self.environment,
                    &pending,
                )
                .await?),
            Route::Backend => {
                let params = exec_server_params_for_request(process_id, request, tty).await?;
                let started = self.environment.get_exec_backend().start(params).await?;
                Ok(UnifiedExecProcess::from_exec_server_started(started, artifact, &pending).await?)
            }
        }
    }
}

async fn wait_exited_and_closed(
    process: &UnifiedExecProcess,
    deadline: Duration,
) -> anyhow::Result<()> {
    let handles = process.output_handles();
    let started = StdInstant::now();
    loop {
        let closed = handles.output_closed_notify.notified();
        tokio::pin!(closed);
        closed.as_mut().enable();
        let exited = process.has_exited();
        if exited && process.output_is_closed() {
            return Ok(());
        }
        anyhow::ensure!(
            started.elapsed() < deadline,
            "exit={exited} closed={} after {deadline:?}",
            process.output_is_closed()
        );
        let exit = process.cancellation_token();
        tokio::select! {
            _ = &mut closed => {}
            _ = exit.cancelled(), if !exited => {}
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
    }
}

async fn wait_for_output(
    process: &UnifiedExecProcess,
    marker: &str,
    deadline: Duration,
) -> anyhow::Result<()> {
    let handles = process.output_handles();
    let started = StdInstant::now();
    loop {
        let notified = handles.output_notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if String::from_utf8_lossy(&process.snapshot_output().await).contains(marker) {
            return Ok(());
        }
        anyhow::ensure!(started.elapsed() < deadline, "{marker} not observed");
        tokio::select! {
            _ = &mut notified => {}
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
    }
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

async fn fast_exit(route: Route) -> anyhow::Result<()> {
    let mut bench = Bench::new()?;
    let request = bench.request(&["cmd.exe", "/D", "/S", "/C", "exit 0"]);
    for _ in 0..3 {
        let process = bench.spawn(route, &request, false, None).await?;
        wait_exited_and_closed(&process, Duration::from_secs(10)).await?;
    }
    let mut spawn = Vec::new();
    let mut total = Vec::new();
    for _ in 0..40 {
        let started = StdInstant::now();
        let process = bench.spawn(route, &request, false, None).await?;
        spawn.push(ms(started.elapsed()));
        wait_exited_and_closed(&process, Duration::from_secs(10)).await?;
        total.push(ms(started.elapsed()));
        anyhow::ensure!(process.exit_code() == Some(0), "{:?}", process.exit_code());
    }
    record(
        route,
        "fast_exit",
        serde_json::json!({ "spawn_ms": stats(&mut spawn), "exit_observed_ms": stats(&mut total) }),
    );
    Ok(())
}

async fn large_output(route: Route) -> anyhow::Result<()> {
    let mut bench = Bench::new()?;
    // Below the 16 MiB artifact cap, so truncation can only mean lost output.
    let size = 12 * 1024 * 1024;
    let file = bench.fixture("large.txt", size)?;
    let command = format!("type \"{}\"", file.display());
    let request = bench.request(&["cmd.exe", "/D", "/S", "/C", &command]);
    let before = memory();
    let started = StdInstant::now();
    let artifact = RawOutputArtifact::pending(bench.home.as_path(), "bench");
    let process = bench.spawn(route, &request, false, Some(artifact)).await?;
    wait_exited_and_closed(&process, Duration::from_secs(120)).await?;
    let elapsed = started.elapsed();
    let artifact = process
        .raw_output_artifact()
        .await
        .ok_or_else(|| anyhow::anyhow!("artifact unavailable"))?;
    let after = memory();
    let lagged = process
        .output_handles()
        .completion_output_buffer
        .lock()
        .await
        .lagged_chunks();
    record(
        route,
        "large_output",
        serde_json::json!({
            "bytes": size,
            "elapsed_ms": ms(elapsed),
            "mib_per_s": mib(size as i64) / elapsed.as_secs_f64(),
            "artifact_bytes": artifact.retained_bytes(),
            "artifact_truncated": artifact.retention_limit_hit(),
            "lagged_chunks": lagged,
            "private_delta_mib": mib(after.private_usage as i64 - before.private_usage as i64),
            "working_set_delta_mib": mib(after.working_set_size as i64 - before.working_set_size as i64),
            "peak_working_set_mib": mib(after.peak_working_set_size as i64),
        }),
    );
    drop(process);
    Ok(())
}

async fn concurrent_output(route: Route) -> anyhow::Result<()> {
    let mut bench = Bench::new()?;
    let size = 4 * 1024 * 1024;
    let count = 8;
    let file = bench.fixture("concurrent.txt", size)?;
    let command = format!("type \"{}\"", file.display());
    let request = bench.request(&["cmd.exe", "/D", "/S", "/C", &command]);
    let before = memory();
    let started = StdInstant::now();
    let mut processes = Vec::new();
    for _ in 0..count {
        let artifact = RawOutputArtifact::pending(bench.home.as_path(), "bench");
        processes.push(bench.spawn(route, &request, false, Some(artifact)).await?);
    }
    for process in &processes {
        wait_exited_and_closed(process, Duration::from_secs(120)).await?;
    }
    let elapsed = started.elapsed();
    let mut complete = 0;
    let mut lagged = 0;
    let mut truncated = 0;
    for process in &processes {
        let artifact = process.raw_output_artifact().await;
        let bytes = artifact.as_ref().and_then(RawOutputArtifact::retained_bytes);
        if artifact
            .as_ref()
            .is_some_and(RawOutputArtifact::retention_limit_hit)
        {
            truncated += 1;
        }
        if bytes == Some(size as u64) {
            complete += 1;
        }
        lagged += process
            .output_handles()
            .completion_output_buffer
            .lock()
            .await
            .lagged_chunks();
    }
    let after = memory();
    record(
        route,
        "concurrent_output",
        serde_json::json!({
            "processes": count,
            "bytes_each": size,
            "elapsed_ms": ms(elapsed),
            "mib_per_s": mib((size * count) as i64) / elapsed.as_secs_f64(),
            "complete_artifacts": complete,
            "truncated_artifacts": truncated,
            "lagged_chunks": lagged,
            "private_delta_mib": mib(after.private_usage as i64 - before.private_usage as i64),
            "peak_working_set_mib": mib(after.peak_working_set_size as i64),
        }),
    );
    drop(processes);
    Ok(())
}

async fn stdin_roundtrip(route: Route) -> anyhow::Result<()> {
    let mut bench = Bench::new()?;
    let request = bench.request(&["cmd.exe", "/D", "/Q", "/K"]);
    let process = bench.spawn(route, &request, true, None).await?;
    let mut rtt = Vec::new();
    for index in 0..20 {
        // The console echoes the typed `^`; only command output has the joined marker.
        let started = StdInstant::now();
        process
            .write(format!("echo ab^cd{index}x\r\n").as_bytes())
            .await?;
        wait_for_output(&process, &format!("abcd{index}x"), Duration::from_secs(10)).await?;
        rtt.push(ms(started.elapsed()));
    }
    process.write(b"exit\r\n").await?;
    wait_exited_and_closed(&process, Duration::from_secs(10)).await?;
    record(
        route,
        "stdin_roundtrip",
        serde_json::json!({ "roundtrip_ms": stats(&mut rtt), "exit_code": process.exit_code() }),
    );
    Ok(())
}

async fn terminate(route: Route) -> anyhow::Result<()> {
    let mut bench = Bench::new()?;
    let request = bench.request(&["cmd.exe", "/D", "/S", "/C", "ping -n 30 127.0.0.1 >NUL"]);
    let mut latency = Vec::new();
    for _ in 0..8 {
        let process = bench.spawn(route, &request, false, None).await?;
        tokio::time::sleep(Duration::from_millis(300)).await;
        anyhow::ensure!(!process.has_exited(), "exited before termination");
        let started = StdInstant::now();
        process.terminate_confirmed().await?;
        latency.push(ms(started.elapsed()));
        anyhow::ensure!(process.has_exited(), "termination was not confirmed");
    }
    record(
        route,
        "terminate",
        serde_json::json!({ "terminate_confirmed_ms": stats(&mut latency) }),
    );
    Ok(())
}

async fn interrupt(route: Route) -> anyhow::Result<()> {
    let mut bench = Bench::new()?;
    let request = bench.request(&["cmd.exe", "/D", "/S", "/C", "ping -n 30 127.0.0.1"]);
    let mut latency = Vec::new();
    let mut exited = 0;
    for _ in 0..5 {
        let process = bench.spawn(route, &request, true, None).await?;
        tokio::time::sleep(Duration::from_millis(500)).await;
        let started = StdInstant::now();
        process.interrupt().await?;
        if wait_exited_and_closed(&process, Duration::from_secs(5))
            .await
            .is_ok()
        {
            exited += 1;
            latency.push(ms(started.elapsed()));
        }
        process.terminate_confirmed().await?;
    }
    let latency = if latency.is_empty() {
        serde_json::Value::Null
    } else {
        stats(&mut latency)
    };
    record(
        route,
        "interrupt",
        serde_json::json!({ "attempts": 5, "exited": exited, "exit_ms": latency }),
    );
    Ok(())
}

macro_rules! bench_pair {
    ($scenario:ident, $direct:ident, $backend:ident) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        #[ignore = "benchmark; run with --run-ignored only"]
        async fn $direct() -> anyhow::Result<()> {
            $scenario(Route::Direct).await
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        #[ignore = "benchmark; run with --run-ignored only"]
        async fn $backend() -> anyhow::Result<()> {
            $scenario(Route::Backend).await
        }
    };
}

bench_pair!(fast_exit, route_bench_fast_exit_direct, route_bench_fast_exit_backend);
bench_pair!(large_output, route_bench_large_output_direct, route_bench_large_output_backend);
bench_pair!(
    concurrent_output,
    route_bench_concurrent_output_direct,
    route_bench_concurrent_output_backend
);
bench_pair!(
    stdin_roundtrip,
    route_bench_stdin_roundtrip_direct,
    route_bench_stdin_roundtrip_backend
);
bench_pair!(terminate, route_bench_terminate_direct, route_bench_terminate_backend);
bench_pair!(interrupt, route_bench_interrupt_direct, route_bench_interrupt_backend);
