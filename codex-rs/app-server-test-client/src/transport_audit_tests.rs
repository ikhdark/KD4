use super::*;
use serde_json::json;

fn accept_peer(listener: &std::net::TcpListener) -> std::net::TcpStream {
    listener.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).unwrap();
                return stream;
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(10))
            }
            Err(error) => panic!("bounded peer accept failed: {error}"),
        }
    }
}

#[test]
fn finite_command_cli_budget_is_explicit_and_rejects_watch_and_login() {
    for command in ["send-message", "send-message-v2"] {
        let cli = Cli::try_parse_from([
            "client",
            "--command-timeout-seconds",
            "600",
            command,
            "hello",
        ])
        .unwrap();
        assert_eq!(cli.command_timeout_seconds, Some(600));
        validate_endpoint_options(&cli).unwrap();
    }
    for command in ["watch", "test-login"] {
        let cli =
            Cli::try_parse_from(["client", "--command-timeout-seconds", "600", command]).unwrap();
        assert!(validate_endpoint_options(&cli).is_err());
        let interactive = Cli::try_parse_from(["client", command]).unwrap();
        assert!(interactive.command_timeout_seconds.is_none());
        validate_endpoint_options(&interactive).unwrap();
    }
    assert!(Cli::try_parse_from(["client", "--command-timeout-seconds", "0", "watch"]).is_err());
}

fn initialize_result(home: &Path) -> Value {
    json!({"userAgent":"deadline-peer", "codexHome":home, "platformFamily":"windows", "platformOs":"windows"})
}

fn run_finite_turn(client: &mut CodexClient, endpoint: &Endpoint) -> Result<()> {
    run_client_command(client, endpoint, |client| {
        client.initialize()?;
        let response = client.turn_start(TurnStartParams {
            thread_id: "thread".into(),
            input: vec![],
            ..Default::default()
        })?;
        client.stream_turn("thread", &response.turn.id)
    })
}

#[test]
fn finite_websocket_command_budget_survives_turn_start_response() {
    for complete in [false, true] {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let home = tempfile::tempdir().unwrap();
        let initialize = initialize_result(home.path());
        let peer = thread::spawn(move || {
            let stream = accept_peer(&listener);
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            let request: Value =
                serde_json::from_str(socket.read().unwrap().to_text().unwrap()).unwrap();
            assert_eq!(request["method"], "initialize");
            socket
                .send(Message::Text(
                    json!({"id":request["id"], "result":initialize})
                        .to_string()
                        .into(),
                ))
                .unwrap();
            let initialized: Value =
                serde_json::from_str(socket.read().unwrap().to_text().unwrap()).unwrap();
            assert_eq!(initialized["method"], "initialized");
            let turn: Value =
                serde_json::from_str(socket.read().unwrap().to_text().unwrap()).unwrap();
            assert_eq!(turn["method"], "turn/start");
            socket.send(Message::Text(json!({"id":turn["id"], "result":{"turn":{"id":"turn", "items":[], "status":"inProgress"}}}).to_string().into())).unwrap();
            if complete {
                thread::sleep(Duration::from_millis(250));
                socket.send(Message::Text(json!({"method":"turn/completed", "params":{"threadId":"thread", "turn":{"id":"turn", "items":[], "status":"completed"}}}).to_string().into())).unwrap();
            } else {
                let error = socket.read().unwrap_err();
                assert!(
                    !matches!(error, tungstenite::Error::Io(ref e) if matches!(e.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock)),
                    "{error}"
                );
            }
        });
        let endpoint = Endpoint::Budgeted(
            Box::new(Endpoint::ConnectWs(format!("ws://{address}"))),
            Duration::from_millis(if complete { 2000 } else { 200 }),
        );
        let mut client = CodexClient::connect(&endpoint, &[]).unwrap();
        let start = Instant::now();
        let result = run_finite_turn(&mut client, &endpoint);
        if complete {
            result.unwrap();
        } else {
            let error = format!("{:#}", result.unwrap_err());
            assert!(
                error.contains("deadline") && error.contains("may still be running"),
                "{error}"
            );
        }
        assert!(start.elapsed() < Duration::from_secs(3));
        peer.join().unwrap();
    }
}

#[test]
fn finite_stdio_command_budget_survives_turn_start_response() {
    for complete in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let log = temp.path().join("requests.jsonl");
        let initialize = initialize_result(temp.path())
            .to_string()
            .replace('\'', "''");
        let after_turn = if complete {
            "Start-Sleep -Milliseconds 250; [Console]::WriteLine('{\"method\":\"turn/completed\",\"params\":{\"threadId\":\"thread\",\"turn\":{\"id\":\"turn\",\"items\":[],\"status\":\"completed\"}}}')"
        } else {
            "Start-Sleep -Seconds 10"
        };
        let body = format!(
            "if ($request.method -eq 'initialize') {{ $result = '{initialize}' | ConvertFrom-Json; [Console]::WriteLine((@{{id=$request.id; result=$result}} | ConvertTo-Json -Depth 20 -Compress)) }} elseif ($request.method -eq 'turn/start') {{ [Console]::WriteLine((@{{id=$request.id; result=@{{turn=@{{id='turn'; items=@(); status='inProgress'}}}}}} | ConvertTo-Json -Depth 20 -Compress)); {after_turn} }}"
        );
        let mut client = tests::smoke_deadline_client(&log, &body);
        let endpoint = Endpoint::Budgeted(
            Box::new(Endpoint::SpawnCodex(PathBuf::from("unused"))),
            Duration::from_secs(if complete { 3 } else { 1 }),
        );
        let start = Instant::now();
        let result = run_finite_turn(&mut client, &endpoint);
        if complete {
            result.unwrap();
        } else {
            let error = format!("{:#}", result.unwrap_err());
            assert!(
                error.contains("deadline") && error.contains("may still be running"),
                "{error}"
            );
            assert!(client.child_exit_status().unwrap().is_some());
        }
        assert!(start.elapsed() < Duration::from_secs(4));
        let requests = fs::read_to_string(log)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            requests
                .iter()
                .map(|request| request["method"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["initialize", "initialized", "turn/start"]
        );
    }
}

#[test]
fn permanent_websocket_rejection_is_not_retried() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (stop_tx, stop_rx) = mpsc::channel();
    let peer = thread::spawn(move || {
        let stream = accept_peer(&listener);
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut stream = BufReader::new(stream);
        loop {
            let mut line = String::new();
            assert_ne!(stream.read_line(&mut line).unwrap(), 0);
            if line == "\r\n" {
                break;
            }
        }
        stream
            .get_mut()
            .write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n")
            .unwrap();
        drop(stream);
        stop_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        listener.set_nonblocking(true).unwrap();
        assert!(
            listener.accept().is_err(),
            "permanent rejection must make exactly one connection attempt"
        );
    });
    let error = BlockingWebSocket::connect_with_runtime(
        &format!("ws://{address}"),
        Duration::from_millis(700),
        || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
        },
    )
    .err()
    .expect("rejected connection");
    let error = format!("{error:#}");
    stop_tx.send(()).unwrap();
    peer.join().unwrap();
    assert!(
        error.contains("401") && !error.contains("deadline expired"),
        "{error}"
    );
}

#[test]
fn transient_websocket_startup_refusal_still_retries() {
    let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = reservation.local_addr().unwrap();
    drop(reservation);
    let caller = thread::spawn(move || {
        BlockingWebSocket::connect_with_runtime(
            &format!("ws://{address}"),
            Duration::from_secs(3),
            || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
            },
        )
    });
    thread::sleep(Duration::from_millis(150));
    let listener = std::net::TcpListener::bind(address).unwrap();
    let stream = accept_peer(&listener);
    let _socket = tungstenite::accept(stream).unwrap();
    drop(
        caller
            .join()
            .unwrap()
            .expect("server became available inside the original budget"),
    );
}

#[test]
fn blocked_diagnostics_do_not_block_readiness_or_child_pipe_drain() {
    struct BlockedWriter {
        entered: Option<mpsc::Sender<()>>,
        release: mpsc::Receiver<()>,
    }
    impl Write for BlockedWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if let Some(entered) = self.entered.take() {
                entered.send(()).unwrap();
                self.release.recv().unwrap();
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::channel();
    let logs = format!(
        "title\n{}listening on: ws://127.0.0.1:1234\n{}",
        "before\n".repeat(300),
        "after\n".repeat(300)
    );
    let parser = relay_app_server_diagnostics(
        std::io::Cursor::new(logs),
        BlockedWriter {
            entered: Some(entered_tx),
            release: release_rx,
        },
        ready_tx,
    );
    entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(
        ready_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
        "ws://127.0.0.1:1234"
    );
    parser.join().unwrap(); // Entire child pipe drained while presentation is blocked.
    release_tx.send(()).unwrap();
}

#[test]
fn capture_readiness_reports_owned_child_exit_without_five_second_wait() {
    use std::os::windows::process::CommandExt;
    let temp = tempfile::tempdir().unwrap();
    let mut child = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", "exit 19"])
        .creation_flags(0x0800_0000)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let stdin = child.stdin.take();
    let stdout = BufReader::new(child.stdout.take().unwrap()).into();
    assert_eq!(child.wait().unwrap().code(), Some(19));
    let mut client = CodexClient::from_transport(ClientTransport::Stdio {
        child: std::sync::Arc::new(std::sync::Mutex::new(child)),
        stdin,
        stdout,
    });
    let started = Instant::now();
    let error = plugin_analytics_smoke::wait_until_capture_is_ready(
        &temp.path().join("missing.jsonl"),
        &mut client,
    )
    .unwrap_err();
    assert!(error.to_string().contains("19"), "{error:#}");
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn stdio_read_deadline_does_not_depend_on_pipe_eof() {
    let (_writer_still_alive, lines) = mpsc::sync_channel(1);
    let reader = StdioLineReader { lines };
    let started = Instant::now();
    let error = reader
        .read(Some(started + Duration::from_millis(20)))
        .unwrap_err();
    assert!(error.to_string().contains("deadline"), "{error:#}");
    assert!(started.elapsed() < Duration::from_secs(1));
}
