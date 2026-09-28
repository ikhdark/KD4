//! Dispatcher regressions and opt-in measurements, not end-to-end turn speedups.

use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use codex_utils_path_uri::PathUri;
use futures::future::join_all;
use serde_json::json;
use tokio::time::timeout;

use super::run_connection;
use crate::ExecParams;
use crate::ExecServerClient;
use crate::ExecServerClientConnectOptions;
use crate::ExecServerRuntimePaths;
use crate::ExecServerTelemetry;
use crate::FsGetMetadataParams;
use crate::FsReadFileBoundedParams;
use crate::ProcessId;
use crate::ReadParams;
use crate::TerminateParams;
use crate::WriteParams;
use crate::WriteStatus;
use crate::connection::JsonRpcConnection;
use crate::server::session_registry::SessionRegistry;
use crate::telemetry::ConnectionTransport;

const LIMIT: Duration = Duration::from_secs(10);

async fn peer(registry: Arc<SessionRegistry>) -> (ExecServerClient, tokio::task::JoinHandle<()>) {
    let (client_writer, server_reader) = tokio::io::duplex(4 * 1024 * 1024);
    let (server_writer, client_reader) = tokio::io::duplex(4 * 1024 * 1024);
    let server = tokio::spawn(run_connection(
        JsonRpcConnection::from_stdio(server_reader, server_writer, "latency-server".into()),
        registry,
        ExecServerRuntimePaths::new(std::env::current_exe().unwrap()).unwrap(),
        ExecServerTelemetry::default(),
        ConnectionTransport::Stdio,
    ));
    let client = ExecServerClient::connect(
        JsonRpcConnection::from_stdio(client_reader, client_writer, "latency-client".into()),
        ExecServerClientConnectOptions::default(),
    )
    .await
    .unwrap();
    (client, server)
}

fn params(id: &str, cwd: &std::path::Path, script: &str) -> ExecParams {
    ExecParams {
        process_id: ProcessId::from(id),
        argv: vec!["cmd.exe".into(), "/d".into(), "/c".into(), script.into()],
        cwd: PathUri::from_host_native_path(cwd).unwrap(),
        env_policy: None,
        env: std::env::vars().collect(),
        tty: false,
        pipe_stdin: false,
        arg0: None,
        sandbox: None,
        enforce_managed_network: false,
        managed_network: None,
    }
}

fn report(value: serde_json::Value) {
    println!("LATENCY_PROBE={value}");
}

#[test]
#[cfg_attr(not(windows), ignore = "uses Windows process fixtures")]
fn latency_probe_cancel_pending_start() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    runtime.block_on(async {
        let mut samples = Vec::new();
        for sample in 0..5 {
            for direct_control in [false, true] {
                let registry = SessionRegistry::new(ExecServerTelemetry::default());
                let (client, server) = peer(Arc::clone(&registry)).await;
                let backend = registry
                    .process_for_test(&client.session_id().unwrap())
                    .await;
                let temp = tempfile::tempdir().unwrap();
                let marker = temp.path().join("launched.txt");
                let request = params("cancel-probe", temp.path(), "echo launched>launched.txt");
                let process_id = request.process_id.clone();
                let (occupied_tx, occupied_rx) = tokio::sync::oneshot::channel();
                let (release_tx, release_rx) = std::sync::mpsc::channel();
                let worker = tokio::task::spawn_blocking(move || {
                    occupied_tx.send(()).unwrap();
                    // Dropping release_tx also releases the worker on a failed assertion.
                    let _ = release_rx.recv_timeout(LIMIT);
                });
                occupied_rx.await.unwrap();
                let start_client = client.clone();
                let start = tokio::spawn(async move { start_client.exec(request).await });
                timeout(LIMIT, async {
                    loop {
                        let state = backend
                            .exec_read(ReadParams {
                                process_id: process_id.clone(),
                                after_seq: None,
                                max_bytes: None,
                                wait_ms: None,
                            })
                            .await;
                        if state.is_err_and(|error| error.message.contains("is starting")) {
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                let before = Instant::now();
                let cancel_client = client.clone();
                let cancel_backend = backend.clone();
                let cancel_id = process_id.clone();
                let cancel = tokio::spawn(async move {
                    if direct_control {
                        cancel_backend
                            .terminate(TerminateParams {
                                process_id: cancel_id,
                            })
                            .await
                            .map_err(|error| error.message)
                    } else {
                        cancel_client
                            .terminate(&cancel_id)
                            .await
                            .map_err(|error| error.to_string())
                    }
                    .map(|response| (response, before.elapsed().as_secs_f64() * 1000.0))
                });
                let (_, cancel_ms) = timeout(Duration::from_secs(2), cancel)
                    .await
                    .expect("cancellation must finish while preparation is held")
                    .unwrap()
                    .unwrap();
                assert!(
                    !marker.exists(),
                    "occupied preparation must not launch the child"
                );
                release_tx.send(()).unwrap();
                worker.await.unwrap();
                let started = timeout(LIMIT, start).await.unwrap().unwrap();
                assert!(started.is_err(), "cancelled start must not succeed");
                backend.shutdown().await;
                tokio::task::spawn_blocking(|| ()).await.unwrap();
                let marker_created = marker.exists();
                assert!(
                    !marker_created,
                    "cancelled reservation must not launch later"
                );
                samples.push(
                    json!({"sample": sample, "direct_backend_control": direct_control,
                    "cancel_ms": cancel_ms, "completed_while_held": true,
                    "marker_created": marker_created}),
                );
                drop(client);
                timeout(LIMIT, server).await.unwrap().unwrap();
                registry.shutdown().await;
            }
        }
        report(json!({"probe": "cancel_pending_start", "samples": samples}));
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "opt-in latency probe: warm-cache real-file reads over the real RPC dispatcher"]
async fn latency_probe_parallel_file_reads() {
    let registry = SessionRegistry::new(ExecServerTelemetry::default());
    let mut clients = Vec::new();
    let mut servers = Vec::new();
    for _ in 0..8 {
        let (client, server) = peer(Arc::clone(&registry)).await;
        clients.push(client);
        servers.push(server);
    }
    let temp = tempfile::tempdir().unwrap();
    let mut samples = Vec::new();
    for bytes in [64 * 1024, 1024 * 1024] {
        let contents = vec![b'x'; bytes];
        let mut paths = Vec::new();
        for i in 0..8 {
            let path = temp.path().join(format!("file-{i}"));
            std::fs::write(&path, &contents).unwrap();
            paths.push(PathUri::from_host_native_path(path).unwrap());
        }
        for repetition in 0..13 {
            // Alternate order; repetition zero warms both cases and is not reported.
            for independent_connections in if repetition % 2 == 0 {
                [false, true]
            } else {
                [true, false]
            } {
                let before = Instant::now();
                let requests = paths.iter().enumerate().map(|(index, path)| {
                    clients[if independent_connections { index } else { 0 }].fs_read_file_bounded(
                        FsReadFileBoundedParams {
                            path: path.clone(),
                            max_bytes: bytes,
                            confined_root: None,
                            sandbox: None,
                        },
                    )
                });
                let responses = timeout(LIMIT, join_all(requests)).await.unwrap();
                let elapsed_ms = before.elapsed().as_secs_f64() * 1000.0;
                for response in responses {
                    assert_eq!(
                        STANDARD
                            .decode(response.unwrap().data_base64.unwrap())
                            .unwrap(),
                        contents
                    );
                }
                if repetition > 0 {
                    samples.push(json!({"bytes_per_file": bytes, "repetition": repetition,
                        "independent_connections": independent_connections, "elapsed_ms": elapsed_ms}));
                }
            }
        }
    }
    report(json!({"probe": "parallel_file_reads", "files_per_batch": 8, "samples": samples}));
    drop(clients);
    for server in servers {
        timeout(LIMIT, server).await.unwrap().unwrap();
    }
    registry.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg_attr(not(windows), ignore = "uses Windows process fixtures")]
async fn latency_probe_stdin_head_of_line() {
    let registry = SessionRegistry::new(ExecServerTelemetry::default());
    let (client, server) = peer(Arc::clone(&registry)).await;
    let (control, control_server) = peer(Arc::clone(&registry)).await;
    let backend = registry
        .process_for_test(&client.session_id().unwrap())
        .await;
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("metadata.txt");
    std::fs::write(&file, b"real file").unwrap();
    let mut request = params("stdin-probe", temp.path(), "unused");
    request.argv = vec![
        "powershell.exe".into(),
        "-NoProfile".into(),
        "-Command".into(),
        "[Console]::Out.WriteLine('ready'); Start-Sleep -Seconds 30".into(),
    ];
    request.pipe_stdin = true;
    let process_id = request.process_id.clone();
    client.exec(request).await.unwrap();
    let ready = backend
        .exec_read(ReadParams {
            process_id: process_id.clone(),
            after_seq: None,
            max_bytes: None,
            wait_ms: Some(5_000),
        })
        .await
        .unwrap();
    assert!(
        ready
            .chunks
            .iter()
            .any(|chunk| String::from_utf8_lossy(&chunk.chunk.0).contains("ready"))
    );
    let mut accepted = 0;
    for index in 0..140 {
        let write = backend.exec_write(WriteParams {
            process_id: process_id.clone(),
            chunk: vec![b'x'; 64 * 1024].into(),
            write_id: format!("fill-{index}"),
        });
        match timeout(Duration::from_millis(50), write).await {
            Ok(response) => {
                assert_eq!(response.unwrap().status, WriteStatus::Accepted);
                accepted += 1;
            }
            Err(_) => break,
        }
    }
    assert!(
        accepted > 0 && accepted < 140,
        "real stdin queue must be saturated"
    );
    let write_client = client.clone();
    let write_id = process_id.clone();
    let blocked_write = tokio::spawn(async move {
        write_client
            .write(&write_id, vec![b'x'], "blocked".into())
            .await
    });
    // A successful independent RPC on this connection fences transport delivery.
    client.environment_info().await.unwrap();
    let metadata = FsGetMetadataParams {
        path: PathUri::from_host_native_path(file).unwrap(),
        sandbox: None,
    };
    let same_client = client.clone();
    let same_params = metadata.clone();
    let before = Instant::now();
    let same = tokio::spawn(async move {
        let result = same_client.fs_get_metadata(same_params).await.unwrap();
        (result, before.elapsed().as_secs_f64() * 1000.0)
    });
    let control_before = Instant::now();
    let control_result = timeout(LIMIT, control.fs_get_metadata(metadata))
        .await
        .unwrap()
        .unwrap();
    let control_ms = control_before.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(control_result.size, 9);
    tokio::time::sleep(Duration::from_millis(250)).await;
    let same_completed_while_blocked = same.is_finished();
    assert!(
        !blocked_write.is_finished(),
        "stdin pressure must remain present"
    );
    assert!(
        same_completed_while_blocked,
        "unrelated metadata must bypass blocked stdin"
    );
    client.terminate(&process_id).await.unwrap();
    let _ = timeout(LIMIT, blocked_write).await.unwrap().unwrap();
    let (result, same_ms) = timeout(LIMIT, same).await.unwrap().unwrap();
    assert_eq!(result.size, 9);
    report(
        json!({"probe": "stdin_head_of_line", "accepted_fill_writes": accepted,
        "same_connection_ms": same_ms, "independent_connection_ms": control_ms,
        "same_completed_while_blocked": same_completed_while_blocked}),
    );
    backend.shutdown().await;
    drop(client);
    drop(control);
    timeout(LIMIT, server).await.unwrap().unwrap();
    timeout(LIMIT, control_server).await.unwrap().unwrap();
    registry.shutdown().await;
}
