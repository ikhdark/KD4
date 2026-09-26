use super::*;

#[test]
fn effective_config_checks_values_and_active_sources() {
    let temp = tempfile::tempdir().unwrap();
    let expected = json!({"model":"gpt-6-astra","model_reasoning_effort":"high","approval_policy":"never","features":{"kd4_runtime":false}});
    fs::write(
        temp.path().join("config.toml"),
        toml::to_string(&expected).unwrap(),
    )
    .unwrap();
    let mut response = json!({"config":expected,"layers":[{"name":{"type":"user","file":temp.path().join("config.toml")},"config":expected}]});
    verify_effective_config(&response, &expected, temp.path(), &[]).unwrap();
    response["config"]["model_reasoning_effort"] = json!("low");
    assert!(
        verify_effective_config(&response, &expected, temp.path(), &[])
            .unwrap_err()
            .to_string()
            .contains("model_reasoning_effort")
    );
    response["config"]["model_reasoning_effort"] = json!("high");
    response["config"]["service_tier"] = json!("priority");
    assert!(
        verify_effective_config(&response, &expected, temp.path(), &[])
            .unwrap_err()
            .to_string()
            .contains("service_tier")
    );
    response["config"]["service_tier"] = Value::Null;
    response["layers"].as_array_mut().unwrap().push(json!({"name":{"type":"project","dotCodexFolder":"C:/unexpected/.codex"},"config":{"instructions":"injected"}}));
    assert!(
        verify_effective_config(&response, &expected, temp.path(), &[])
            .unwrap_err()
            .to_string()
            .contains("unexpected nonempty configuration layer")
    );
}

#[test]
fn failed_startup_preserves_a_diagnostic_record_without_a_rollout() {
    let temp = tempfile::tempdir().unwrap();
    let request = NativeAttemptRequest {
        attempt_id: "startup-failure".into(),
        app_server: temp.path().join("missing-app-server.exe"),
        cwd: temp.path().to_path_buf(),
        codex_home: temp.path().join("home"),
        evidence_dir: temp.path().join("evidence"),
        env: BTreeMap::new(),
        config_overrides: vec![],
        expected_config: json!({}),
        prompt: "execute a tool".into(),
        timeout_ms: 1000,
        scenario: None,
    };
    let evidence = run_attempt(&request);
    assert_eq!(evidence.status, "setup_failed");
    assert_eq!(evidence.completed_turns, 0);
    assert!(evidence.rollout_paths.is_empty());
    assert_eq!(evidence.stdout_paths.len(), 1);
    assert_eq!(evidence.stderr_paths.len(), 1);
    assert!(evidence.stdout_paths[0].is_file());
    assert!(evidence.stderr_paths[0].is_file());
    let saved: Value = serde_json::from_slice(&fs::read(evidence.evidence_path).unwrap()).unwrap();
    assert_eq!(saved["attemptId"], "startup-failure");
    assert_eq!(saved["failure"]["kind"], "setup_failure");
    assert!(
        saved["failure"]["message"]
            .as_str()
            .unwrap()
            .contains("missing-app-server.exe")
    );
}

#[test]
fn configuration_requires_exact_prepared_session_flags() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("config.toml"), "model = 'gpt-6-astra'\n").unwrap();
    let base = json!({"model":"gpt-6-astra"});
    let flags = json!({"features":{"kd4_runtime":false},"model_providers":{"repo_benchmark":{"base_url":"http://127.0.0.1:5000"}}});
    let overrides = vec![
        "features.kd4_runtime=false".into(),
        "model_providers.repo_benchmark.base_url='http://127.0.0.1:5000'".into(),
    ];
    let mut effective = base.clone();
    merge_config(&mut effective, flags.clone()).unwrap();
    let mut response = json!({"config":effective,"layers":[{"name":{"type":"user","file":temp.path().join("config.toml")},"config":base},{"name":{"type":"sessionFlags"},"config":flags}]});
    verify_effective_config(&response, &base, temp.path(), &overrides).unwrap();
    response["layers"][1]["config"]["developer_instructions"] = json!("inherited instruction");
    assert!(
        verify_effective_config(&response, &base, temp.path(), &overrides)
            .unwrap_err()
            .to_string()
            .contains("other than the prepared overrides")
    );
    response["layers"].as_array_mut().unwrap().pop();
    assert!(
        verify_effective_config(&response, &base, temp.path(), &overrides)
            .unwrap_err()
            .to_string()
            .contains("prepared sessionFlags exactly once")
    );
}

#[cfg(windows)]
fn peer_request(directory: &Path, terminal: &str) -> NativeAttemptRequest {
    let peer = directory.join("peer.ps1");
    let wrapper = directory.join("peer.cmd");
    fs::create_dir(directory.join("home")).unwrap();
    fs::write(directory.join("home/config.toml"), "").unwrap();
    let response = json!({"config":{},"layers":[{"name":{"type":"user","file":directory.join("home/config.toml"),"profile":null},"config":{}}]});
    fs::write(
        directory.join("config-response.json"),
        serde_json::to_vec(&response).unwrap(),
    )
    .unwrap();
    fs::write(
        &wrapper,
        "@echo off\r\npowershell.exe -NoProfile -NonInteractive -File \"%~dp0peer.ps1\"\r\n",
    )
    .unwrap();
    let source = r#"
function Send($value) { [Console]::WriteLine(($value | ConvertTo-Json -Depth 30 -Compress)) }
while ($null -ne ($line = [Console]::In.ReadLine())) {
    $request = $line | ConvertFrom-Json
    switch ($request.method) {
        'initialize' { Send @{id=$request.id;result=@{userAgent='benchmark-native-test-peer'}} }
        'config/read' { Send @{id=$request.id;result=(Get-Content -LiteralPath (Join-Path $PSScriptRoot 'config-response.json') -Raw | ConvertFrom-Json)} }
        'thread/start' { Send @{id=$request.id;result=@{thread=@{id='thread-1'}}} }
        'thread/resume' { Send @{id=$request.id;result=@{thread=@{id=$request.params.threadId}}} }
        'turn/start' {
            Send @{method='item/started';params=@{threadId='thread-1';turnId='turn-1';item=@{id='tool-1';type='commandExecution'}}}
            if ('TERMINAL' -eq 'hang') { Start-Sleep -Seconds 30 }
            Send @{method='item/completed';params=@{threadId='thread-1';turnId='turn-1';item=@{id='tool-1';type='commandExecution';status='completed'}}}
            Send @{method='turn/completed';params=@{threadId='thread-1';turn=@{id='turn-1';status='TERMINAL';error=@{message='deliberate turn failure'}}}}
            Send @{id=$request.id;result=@{turn=@{id='turn-1'}}}
        }
    }
}

"#;
    fs::write(peer, source.replace("TERMINAL", terminal)).unwrap();
    NativeAttemptRequest {
        attempt_id: "native-test-peer".into(),
        app_server: wrapper,
        cwd: directory.to_path_buf(),
        codex_home: directory.join("home"),
        evidence_dir: directory.join("evidence"),
        env: std::env::vars().collect(),
        config_overrides: vec![],
        expected_config: json!({}),
        prompt: "execute and finish".into(),
        timeout_ms: 10000,
        scenario: None,
    }
}

#[test]
#[cfg(windows)]
fn native_restart_restores_prepared_config_and_preserves_sessions() {
    let temp = tempfile::tempdir().unwrap();
    let mut request = peer_request(temp.path(), "completed");
    let prepared_config = b"# Preserve exact prepared bytes\nmodel = 'benchmark-model'\nmodel_auto_compact_token_limit = 190000\n[features]\nkd4_runtime = false\n";
    request.expected_config = json!({"model": "benchmark-model", "model_auto_compact_token_limit": 190000, "features": {"kd4_runtime": false}});
    fs::write(request.codex_home.join("config.toml"), prepared_config).unwrap();
    let path = temp.path().join("config-response.json");
    let mut response: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    response["config"] = request.expected_config.clone();
    response["layers"][0]["config"] = request.expected_config.clone();
    fs::write(path, serde_json::to_vec(&response).unwrap()).unwrap();
    let mut evidence = run_attempt(&request);
    assert_eq!(evidence.status, "completed", "{:?}", evidence.failure);
    let sessions = request.codex_home.join("sessions");
    fs::create_dir(&sessions).unwrap();
    let rollout = sessions.join("thread-1.jsonl");
    fs::write(&rollout, "preserved session\n").unwrap();
    fs::write(
        request.codex_home.join("config.toml"),
        "[projects.'C:/benchmark/workspace']\ntrust_level = 'trusted'\n",
    )
    .unwrap();

    let started = Instant::now();
    let mut process = spawn_recorded(
        &request,
        &[],
        started,
        started + Duration::from_secs(10),
        Some(prepared_config),
        &mut evidence,
    )
    .unwrap();
    initialize(&mut process, &request, &[], &mut evidence).unwrap();
    let resumed = process
        .rpc("thread/resume", json!({"threadId": evidence.thread_id}))
        .unwrap();
    process.stop().unwrap();
    assert_eq!(resumed["thread"]["id"], "thread-1");
    assert_eq!(
        fs::read(request.codex_home.join("config.toml")).unwrap(),
        prepared_config
    );
    let config: toml::Value =
        toml::from_str(&fs::read_to_string(request.codex_home.join("config.toml")).unwrap())
            .unwrap();
    assert_eq!(
        serde_json::to_value(config).unwrap(),
        request.expected_config
    );
    assert_eq!(fs::read_to_string(rollout).unwrap(), "preserved session\n");
}

#[test]
#[cfg(windows)]
fn native_stdio_preserves_early_notifications_and_rejects_failed_terminal() {
    for (terminal, expected_status) in [("completed", "completed"), ("failed", "failed")] {
        let temp = tempfile::tempdir().unwrap();
        let request = peer_request(temp.path(), terminal);
        let evidence = run_attempt(&request);
        assert_eq!(evidence.status, expected_status, "{:?}", evidence.failure);
        assert_eq!(evidence.tool_executions, 1);
        assert_eq!(
            evidence.completed_turns,
            usize::from(terminal == "completed")
        );
        // Turn time excludes launch and the handshake; a failed turn records none.
        assert_eq!(evidence.turn_elapsed_ms.is_some(), terminal == "completed");
        assert!(
            evidence
                .turn_elapsed_ms
                .is_none_or(|turn| turn < evidence.elapsed_ms),
            "{:?} of {}",
            evidence.turn_elapsed_ms,
            evidence.elapsed_ms
        );
        assert!(
            evidence
                .events
                .iter()
                .any(|event| event["message"]["method"] == "turn/completed")
        );
        if terminal == "failed" {
            assert!(
                evidence
                    .failure
                    .unwrap()
                    .message
                    .contains("deliberate turn failure")
            );
        }
        assert!(
            fs::read_to_string(&evidence.stdout_paths[0])
                .unwrap()
                .contains("turn/completed")
        );
    }
}

#[test]
#[cfg(windows)]
fn native_stdio_accepts_normalized_isolated_home_and_migration_metadata() {
    let temp = tempfile::tempdir().unwrap();
    let mut request = peer_request(temp.path(), "completed");
    request.codex_home = fs::canonicalize(&request.codex_home).unwrap();
    let path = temp.path().join("config-response.json");
    let mut response: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let reported = response["layers"][0]["name"]["file"].as_str().unwrap();
    let reported = reported
        .strip_prefix(r"\\?\")
        .unwrap_or(reported)
        .to_owned();
    assert_ne!(Path::new(&reported), request.codex_home.join("config.toml"));
    response["layers"][0]["name"]["file"] = json!(reported);
    response["layers"][0]["config"]["config_version"] = json!(1);
    response["config"]["config_version"] = json!(1);
    response["layers"].as_array_mut().unwrap().push(json!({
        "name": {"type": "system", "file": "C:/ProgramData/OpenAI/Codex/config.toml"},
        "config": {"config_version": 1}
    }));
    fs::write(path, serde_json::to_vec(&response).unwrap()).unwrap();

    let evidence = run_attempt(&request);
    assert_eq!(evidence.status, "completed", "{:?}", evidence.failure);
    assert_eq!(evidence.thread_id.as_deref(), Some("thread-1"));
    assert_eq!(evidence.completed_turns, 1);
    assert_eq!(evidence.tool_executions, 1);
}

#[test]
#[cfg(windows)]
fn native_stdio_rejects_config_inheritance_before_starting_a_thread() {
    for (case, reason) in [
        ("wrong_file", "exact isolated home/config.toml"),
        ("relative_file", "exact isolated home/config.toml"),
        ("user_version", "reported user config differs"),
        ("system_version", "unexpected nonempty configuration layer"),
        ("system_settings", "unexpected nonempty configuration layer"),
        ("profile", "must not select a profile"),
        ("flags", "other than the prepared overrides"),
        ("missing_layer", "malformed or missing layer config"),
        ("malformed_layer", "malformed or missing layer config"),
        ("missing_effective", "returned no config"),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let request = peer_request(temp.path(), "completed");
        let path = temp.path().join("config-response.json");
        let mut response: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        match case {
            "wrong_file" => {
                let other = request.codex_home.join("other.toml");
                fs::write(&other, "").unwrap();
                response["layers"][0]["name"]["file"] = json!(other);
            }
            "relative_file" => response["layers"][0]["name"]["file"] = json!("home/config.toml"),
            "user_version" => response["layers"][0]["config"]["config_version"] = json!(2),
            "system_version" => response["layers"].as_array_mut().unwrap().push(json!({"name":{"type":"system"},"config":{"config_version":2}})),
            "system_settings" => response["layers"].as_array_mut().unwrap().push(json!({"name":{"type":"system"},"config":{"config_version":1,"developer_instructions":"inherited"}})),
            "profile" => response["layers"][0]["name"]["profile"] = json!("secret-profile"),
            "flags" => response["layers"].as_array_mut().unwrap().push(json!({"name":{"type":"sessionFlags"},"config":{"developer_instructions":"inherited"}})),
            "missing_layer" => { response["layers"][0].as_object_mut().unwrap().remove("config"); }
            "malformed_layer" => response["layers"][0]["config"] = json!([]),
            "missing_effective" => { response.as_object_mut().unwrap().remove("config"); }
            _ => unreachable!(),
        }
        fs::write(path, serde_json::to_vec(&response).unwrap()).unwrap();
        let evidence = run_attempt(&request);
        assert_eq!(
            evidence.status, "setup_failed",
            "case {case}: {:?}",
            evidence.failure
        );
        assert!(
            evidence.failure.unwrap().message.contains(reason),
            "case {case}"
        );
        assert!(evidence.thread_id.is_none());
        assert_eq!(evidence.completed_turns, 0);
        assert!(
            !evidence
                .events
                .iter()
                .any(|event| event["message"]["method"] == "item/started")
        );
    }
}

#[test]
#[cfg(windows)]
fn native_deadline_preserves_pending_tool_evidence_without_fabricating_completion() {
    let temp = tempfile::tempdir().unwrap();
    let mut request = peer_request(temp.path(), "hang");
    request.timeout_ms = 3000;
    let started = Instant::now();
    let evidence = run_attempt(&request);
    assert_eq!(evidence.status, "timeout", "{:?}", evidence.failure);
    assert_eq!(evidence.failure.as_ref().unwrap().kind, "attempt_timeout");
    assert_eq!(evidence.completed_turns, 0);
    assert!(started.elapsed() < Duration::from_secs(15));
    assert!(
        evidence
            .events
            .iter()
            .any(|event| event["message"]["method"] == "item/started")
    );
    assert!(
        !evidence
            .events
            .iter()
            .any(|event| event["message"]["method"] == "turn/completed")
    );
    let saved: Value = serde_json::from_slice(&fs::read(&evidence.evidence_path).unwrap()).unwrap();
    assert_eq!(saved["status"], "timeout");
    assert_eq!(saved["completedTurns"], 0);
}

#[test]
#[cfg(windows)]
fn delayed_native_interrupt_is_observed_before_the_next_tool_turn_completes() {
    let temp = tempfile::tempdir().unwrap();
    let request = peer_request(temp.path(), "unused");
    fs::create_dir(&request.evidence_dir).unwrap();
    fs::write(temp.path().join("peer.ps1"), r#"
function Send($value) { [Console]::WriteLine(($value | ConvertTo-Json -Depth 30 -Compress)) }
$turn = 0
$interrupted = $false
while ($null -ne ($line = [Console]::In.ReadLine())) {
    $request = $line | ConvertFrom-Json
    switch ($request.method) {
        'turn/start' {
            $turn++
            $id = "turn-$turn"
            Send @{method='item/started';params=@{threadId='thread-1';turnId=$id;item=@{id="tool-$turn";type='commandExecution'}}}
            Send @{id=$request.id;result=@{turn=@{id=$id}}}
            if ($turn -eq 1) {
                Start-Sleep -Milliseconds 500
                [System.IO.File]::WriteAllText((Join-Path $PSScriptRoot 'running-checkpoint.txt'), 'running')
            }
            if ($turn -gt 1) {
                if (-not $interrupted) { Send @{method='turn/completed';params=@{threadId='thread-1';turn=@{id=$id;status='failed'}}}; continue }
                [System.IO.File]::WriteAllText((Join-Path $PSScriptRoot 'next-tool.txt'), 'executed after interruption')
                Send @{method='item/completed';params=@{threadId='thread-1';turnId=$id;item=@{id="tool-$turn";type='commandExecution';status='completed'}}}
                Send @{method='turn/completed';params=@{threadId='thread-1';turn=@{id=$id;status='completed'}}}
            }
        }
        'turn/interrupt' {
            Start-Sleep -Milliseconds 500
            if ($request.params.turnId -ne 'turn-1') { Send @{id=$request.id;error=@{code=-1;message='wrong turn interrupted'}}; continue }
            $interrupted = $true
            Send @{id=$request.id;result=@{}}
            Send @{method='turn/completed';params=@{threadId='thread-1';turn=@{id='turn-1';status='interrupted'}}}
        }
    }
}
"#).unwrap();
    let started = Instant::now();
    let mut client =
        client::NativeClient::spawn(&request, &[], started, started + Duration::from_secs(10), 0)
            .unwrap();
    let first = client
        .rpc("turn/start", json!({"threadId":"thread-1"}))
        .unwrap();
    assert_eq!(first["turn"]["id"], "turn-1");
    let mut checkpoint = || {
        Ok(temp
            .path()
            .join("running-checkpoint.txt")
            .is_file()
            .then(|| json!({"runningCheckpoint":"running-checkpoint.txt"})))
    };
    let terminal = client
        .finish_turn("thread-1", "turn-1", Some(&mut checkpoint))
        .unwrap();
    assert_eq!(terminal.status, "interrupted");
    assert_eq!(terminal.tool_executions, 0);
    assert!(!temp.path().join("next-tool.txt").exists());
    let second = client
        .rpc("turn/start", json!({"threadId":"thread-1"}))
        .unwrap();
    assert_eq!(second["turn"]["id"], "turn-2");
    let terminal = client.finish_turn("thread-1", "turn-2", None).unwrap();
    assert_eq!(terminal.status, "completed");
    assert_eq!(terminal.tool_executions, 1);
    assert_eq!(
        fs::read_to_string(temp.path().join("next-tool.txt")).unwrap(),
        "executed after interruption"
    );
    client.stop().unwrap();
    let terminals = client
        .events
        .iter()
        .filter(|event| event["message"]["method"] == "turn/completed")
        .map(|event| {
            event["message"]["params"]["turn"]["status"]
                .as_str()
                .unwrap()
        })
        .collect::<Vec<_>>();
    assert_eq!(terminals, vec!["interrupted", "completed"]);
    let writes: Vec<Value> =
        fs::read_to_string(request.evidence_dir.join("app-server-0.requests.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
    let first_sent = writes
        .iter()
        .find(|event| event["message"]["method"] == "turn/start")
        .unwrap()["elapsedMs"]
        .as_u64()
        .unwrap();
    let interrupted_at = writes
        .iter()
        .find(|event| event["message"]["method"] == "turn/interrupt")
        .unwrap()["elapsedMs"]
        .as_u64()
        .unwrap();
    assert!(
        interrupted_at >= first_sent + 500,
        "outer item/started alone must not trigger interruption"
    );
}

#[test]
#[cfg(windows)]
fn native_deadline_terminates_a_peer_that_stops_reading_large_requests() {
    let temp = tempfile::tempdir().unwrap();
    let mut request = peer_request(temp.path(), "completed");
    let path = temp.path().join("peer.ps1");
    let original = fs::read_to_string(&path).unwrap();
    let target = "'thread/start' { Send @{id=$request.id;result=@{thread=@{id='thread-1'}}} }";
    assert_eq!(
        original.matches(target).count(),
        1,
        "fixture must suspend its only thread/start handler"
    );
    let source = original.replace(
        target,
        "'thread/start' { Send @{id=$request.id;result=@{thread=@{id='thread-1'}}}; Start-Sleep -Seconds 30 }",
    );
    fs::write(path, source).unwrap();
    request.prompt = "large history record ".repeat(512 * 1024);
    request.timeout_ms = 3000;
    let started = Instant::now();
    let evidence = run_attempt(&request);
    assert_eq!(evidence.status, "timeout", "{:?}", evidence.failure);
    assert_eq!(evidence.failure.as_ref().unwrap().kind, "attempt_timeout");
    assert_eq!(
        evidence.thread_id.as_deref(),
        Some("thread-1"),
        "peer completed initialization before ceasing to read"
    );
    assert_eq!(evidence.completed_turns, 0);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "stdin backpressure must not escape the attempt ceiling"
    );
    assert!(
        !evidence
            .events
            .iter()
            .any(|event| event["message"]["method"] == "item/started")
    );
    let writes: Vec<Value> =
        fs::read_to_string(request.evidence_dir.join("app-server-0.requests.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
    let sent = writes
        .iter()
        .find(|event| event["message"]["method"] == "turn/start")
        .unwrap();
    assert_eq!(
        sent["message"]["params"]["input"][0]["text"]
            .as_str()
            .unwrap()
            .len(),
        request.prompt.len()
    );
}
