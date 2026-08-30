// Runtime fixtures and timing extraction for measured workloads.
//
// This file is included into the parent benchmark module so its private,
// benchmark-only contracts remain unchanged.

struct RequestCacheFixture {
    _server: wiremock::MockServer,
    test: TestCodex,
    request_capture: HighVolumeRequestCapture,
    workload: AbWorkload,
    sample_sequence: AtomicUsize,
    previous_cache_components: Mutex<Option<AbRequestComponentSnapshot>>,
}

struct RequestCacheResponder {
    fixture_id: String,
    workload: AbWorkload,
    response_count: AtomicUsize,
}

impl wiremock::Respond for RequestCacheResponder {
    fn respond(&self, request: &wiremock::Request) -> wiremock::ResponseTemplate {
        let request_index = self.response_count.fetch_add(1, Ordering::SeqCst);
        let response_id = format!("{}-{request_index}", self.fixture_id);
        let body = high_volume_request_body_json(request);
        let last_user = request_last_user_item(&body)
            .map(serde_json::Value::to_string)
            .unwrap_or_default();
        let events = if last_user.contains(AB_HISTORY_SEED_PREFIX) {
            vec![
                ev_assistant_message(&response_id, AB_HISTORY_SEED_REPLY),
                ev_completed_with_usage(&response_id, 512, 384, 8, 0),
            ]
        } else {
            match self.workload {
                AbWorkload::LongHistoryToolContinuation
                    if !request_contains_tool_output_after_current_input(&body) =>
                {
                    let call_id = format!("{response_id}-update-plan");
                    vec![
                        ev_response_created(&response_id),
                        ev_function_call(
                            &call_id,
                            "update_plan",
                            r#"{"plan":[{"step":"benchmark request-cache continuation","status":"in_progress"}]}"#,
                        ),
                        ev_completed_with_usage(&response_id, 4_096, 3_072, 24, 8),
                    ]
                }
                AbWorkload::LongHistoryToolContinuation => vec![
                    ev_assistant_message(&response_id, AB_LONG_HISTORY_TOOL_REPLY),
                    ev_completed_with_usage(&response_id, 4_352, 3_328, 16, 0),
                ],
                AbWorkload::LongHistoryNoToolInitial => vec![
                    ev_assistant_message(&response_id, AB_LONG_HISTORY_NO_TOOL_REPLY),
                    ev_completed_with_usage(&response_id, 4_096, 3_072, 16, 0),
                ],
                AbWorkload::StableContextWarmCache => vec![
                    ev_assistant_message(&response_id, AB_STABLE_CONTEXT_REPLY),
                    ev_completed_with_usage(&response_id, 4_096, 3_584, 16, 0),
                ],
                AbWorkload::ContextChangeInvalidation => vec![
                    ev_assistant_message(&response_id, AB_CONTEXT_CHANGE_REPLY),
                    ev_completed_with_usage(&response_id, 4_096, 3_072, 16, 0),
                ],
                other => panic!("request/cache responder cannot serve {}", other.name()),
            }
        };
        wiremock::ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(sse(events))
    }
}

impl RequestCacheFixture {
    async fn start(workload: AbWorkload, fixture_id: &str) -> Result<Self> {
        anyhow::ensure!(
            matches!(
                workload,
                AbWorkload::LongHistoryNoToolInitial
                    | AbWorkload::LongHistoryToolContinuation
                    | AbWorkload::StableContextWarmCache
                    | AbWorkload::ContextChangeInvalidation
            ),
            "request/cache fixture received incompatible workload {}",
            workload.name()
        );
        let server = start_mock_server().await;
        let request_capture = HighVolumeRequestCapture::default();
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path_regex(".*/responses$"))
            .and(request_capture.clone())
            .respond_with(RequestCacheResponder {
                fixture_id: fixture_id.to_string(),
                workload,
                response_count: AtomicUsize::new(0),
            })
            .mount(&server)
            .await;
        let test = test_codex()
            .with_model("test-gpt-5.1-codex")
            .with_config(|config| {
                pin_ab_reasoning_phase_efforts(config);
            })
            .build(&server)
            .await?;
        for seed_index in 0..AB_LONG_HISTORY_TURNS {
            let seed = format!(
                "{AB_HISTORY_SEED_PREFIX}{seed_index:02}:{}",
                "h".repeat(AB_LONG_HISTORY_SEED_BYTES)
            );
            let completion = test.submit_turn_and_capture_completion(&seed).await?;
            anyhow::ensure!(
                completion.error.is_none()
                    && completion.last_agent_message.as_deref() == Some(AB_HISTORY_SEED_REPLY),
                "failed to seed deterministic long history for {}",
                workload.name()
            );
        }
        Ok(Self {
            _server: server,
            test,
            request_capture,
            workload,
            sample_sequence: AtomicUsize::new(0),
            previous_cache_components: Mutex::new(None),
        })
    }

    fn prompt(&self, sequence: usize) -> &'static str {
        match self.workload {
            AbWorkload::LongHistoryNoToolInitial => AB_LONG_HISTORY_NO_TOOL_PROMPT,
            AbWorkload::LongHistoryToolContinuation => AB_LONG_HISTORY_TOOL_PROMPT,
            AbWorkload::StableContextWarmCache => AB_STABLE_CONTEXT_PROMPT,
            AbWorkload::ContextChangeInvalidation if sequence.is_multiple_of(2) => {
                AB_CONTEXT_CHANGE_PROMPT_A
            }
            AbWorkload::ContextChangeInvalidation => AB_CONTEXT_CHANGE_PROMPT_B,
            other => panic!("request/cache fixture cannot prompt {}", other.name()),
        }
    }

    fn expected_reply(&self) -> &'static str {
        match self.workload {
            AbWorkload::LongHistoryNoToolInitial => AB_LONG_HISTORY_NO_TOOL_REPLY,
            AbWorkload::LongHistoryToolContinuation => AB_LONG_HISTORY_TOOL_REPLY,
            AbWorkload::StableContextWarmCache => AB_STABLE_CONTEXT_REPLY,
            AbWorkload::ContextChangeInvalidation => AB_CONTEXT_CHANGE_REPLY,
            other => panic!("request/cache fixture cannot validate {}", other.name()),
        }
    }

    async fn sample(&self) -> Sample {
        let sequence = self.sample_sequence.fetch_add(1, Ordering::SeqCst);
        let prompt = self.prompt(sequence);
        let requests_before = self.request_capture.request_count();
        let started = Instant::now();
        let completion = self.test.submit_turn_and_capture_completion(prompt).await;
        let duration_ns = started.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        let turn_requests = self.request_capture.requests_since(requests_before);
        let mut sample = match completion {
            Ok(completion) => {
                let timing = completion.timing.as_ref();
                let mut sample = timing.map(sample_from_timing).unwrap_or_default();
                sample.duration_ns = duration_ns;
                sample.workload_subturns = 1;
                sample.request_components = turn_requests
                    .iter()
                    .enumerate()
                    .map(|(index, request)| {
                        request_component_snapshot(
                            &high_volume_request_body_json(request),
                            if index == 0 {
                                "initial"
                            } else {
                                "continuation"
                            },
                        )
                    })
                    .collect();
                sample.canonical_request_body_sha256 = turn_requests
                    .iter()
                    .map(canonical_request_body_sha256)
                    .collect();
                sample.history_seed_turns_visible = turn_requests
                    .first()
                    .map(high_volume_request_body_json)
                    .as_ref()
                    .map(history_seed_turns_visible)
                    .unwrap_or_default();
                if completion.error.is_some() {
                    sample
                        .failure_codes
                        .push("unexpected_terminal_error".to_string());
                }
                if completion.last_agent_message.as_deref() != Some(self.expected_reply()) {
                    sample.failure_codes.push("wrong_final_message".to_string());
                }
                if turn_requests.len() != self.workload.expected_logical_generations() as usize {
                    sample.failure_codes.push("request_count".to_string());
                }
                if sample.logical_generations != self.workload.expected_logical_generations()
                    || sample.provider_attempts != sample.logical_generations
                    || sample.sampling_requests != sample.logical_generations
                {
                    sample.failure_codes.push("generation_count".to_string());
                }
                if !timing.is_some_and(timing_reconciles) {
                    sample
                        .failure_codes
                        .push("timing_reconciliation".to_string());
                }
                if self.workload == AbWorkload::LongHistoryToolContinuation
                    && !turn_requests
                        .get(1)
                        .map(high_volume_request_body_json)
                        .as_ref()
                        .is_some_and(request_contains_tool_output_after_current_input)
                {
                    sample
                        .failure_codes
                        .push("missing_tool_continuation_output".to_string());
                }
                sample.serialized_bytes = turn_requests
                    .iter()
                    .map(|request| request.body.len() as u64)
                    .sum();
                sample.exec_description_tokens = turn_requests
                    .first()
                    .map(high_volume_request_body_json)
                    .as_ref()
                    .map(exec_description_tokens_from_body)
                    .unwrap_or_default();
                sample.prompt_input_tokens = turn_requests
                    .iter()
                    .map(high_volume_request_body_json)
                    .map(|body| prompt_input_tokens_from_body(&body))
                    .sum();
                sample
            }
            Err(error) => Sample {
                duration_ns,
                workload_subturns: 1,
                failed: true,
                failure_codes: vec![format!("completion_error:{error}")],
                ..Sample::default()
            },
        };
        if matches!(
            self.workload,
            AbWorkload::StableContextWarmCache | AbWorkload::ContextChangeInvalidation
        ) && let Some(current) = sample.request_components.first().cloned()
        {
            let mut previous = self
                .previous_cache_components
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(prior) = previous.as_ref() {
                sample.request_component_delta = Some(request_component_delta(prior, &current));
            }
            *previous = Some(current);
        }
        if let Err(error) = rollback_test_turns(&self.test, 1).await {
            sample
                .failure_codes
                .push(format!("sample_history_rollback:{error}"));
        }
        sample.failed = !sample.failure_codes.is_empty();
        sample
    }
}

fn request_last_user_index(body: &serde_json::Value) -> Option<usize> {
    body.get("input")?
        .as_array()?
        .iter()
        .rposition(|item| item.get("role").and_then(serde_json::Value::as_str) == Some("user"))
}

fn request_last_user_item(body: &serde_json::Value) -> Option<&serde_json::Value> {
    let input = body.get("input")?.as_array()?;
    input.get(request_last_user_index(body)?)
}

fn request_contains_tool_output_after_current_input(body: &serde_json::Value) -> bool {
    let Some(input) = body.get("input").and_then(serde_json::Value::as_array) else {
        return false;
    };
    let Some(current_input) = request_last_user_index(body) else {
        return false;
    };
    input[current_input + 1..].iter().any(|item| {
        matches!(
            item.get("type").and_then(serde_json::Value::as_str),
            Some("function_call_output") | Some("custom_tool_call_output")
        )
    })
}

fn request_function_call_outputs_after_current_input(
    body: &serde_json::Value,
) -> Vec<(String, String)> {
    let Some(input) = body.get("input").and_then(serde_json::Value::as_array) else {
        return Vec::new();
    };
    let Some(current_input) = request_last_user_index(body) else {
        return Vec::new();
    };
    input[current_input + 1..]
        .iter()
        .filter(|item| {
            item.get("type").and_then(serde_json::Value::as_str) == Some("function_call_output")
        })
        .filter_map(|item| {
            Some((
                item.get("call_id")?.as_str()?.to_string(),
                item.get("output")?.as_str()?.to_string(),
            ))
        })
        .collect()
}

fn request_custom_tool_output_count_after_current_input(body: &serde_json::Value) -> usize {
    let Some(input) = body.get("input").and_then(serde_json::Value::as_array) else {
        return 0;
    };
    let Some(current_input) = request_last_user_index(body) else {
        return 0;
    };
    input[current_input + 1..]
        .iter()
        .filter(|item| {
            item.get("type").and_then(serde_json::Value::as_str) == Some("custom_tool_call_output")
        })
        .count()
}

fn request_top_level_tool_output_count_after_current_input(body: &serde_json::Value) -> usize {
    let Some(input) = body.get("input").and_then(serde_json::Value::as_array) else {
        return 0;
    };
    let Some(current_input) = request_last_user_index(body) else {
        return 0;
    };
    input[current_input + 1..]
        .iter()
        .filter(|item| {
            matches!(
                item.get("type").and_then(serde_json::Value::as_str),
                Some("function_call_output") | Some("custom_tool_call_output")
            )
        })
        .count()
}

fn retained_session_id_from_output(output: &str) -> Option<String> {
    const PREFIX: &str = "Process running with session ID ";
    output
        .lines()
        .find_map(|line| line.trim().strip_prefix(PREFIX))
        .map(|value| {
            value
                .chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
        })
        .filter(|value| !value.is_empty())
}

fn retained_exit_code_from_output(output: &str) -> Option<i32> {
    const PREFIX: &str = "Process exited with code ";
    output
        .lines()
        .find_map(|line| line.trim().strip_prefix(PREFIX))
        .and_then(|value| {
            let value = value.trim_start();
            let digit_start = usize::from(value.starts_with('-'));
            let digit_len = value[digit_start..]
                .chars()
                .take_while(char::is_ascii_digit)
                .map(char::len_utf8)
                .sum::<usize>();
            (digit_len > 0).then(|| &value[..digit_start + digit_len])
        })
        .and_then(|value| value.parse().ok())
}

fn retained_output_for_call(requests: &[wiremock::Request], call_id: &str) -> Option<String> {
    requests
        .iter()
        .map(high_volume_request_body_json)
        .flat_map(|body| request_function_call_outputs_after_current_input(&body))
        .find_map(|(observed_call_id, output)| (observed_call_id == call_id).then_some(output))
}

fn request_component_snapshot(body: &serde_json::Value, stage: &str) -> AbRequestComponentSnapshot {
    let input = body
        .get("input")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    let current_index = request_last_user_index(body);
    let current_input = current_index
        .and_then(|index| input.get(index))
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let history = input
        .into_iter()
        .enumerate()
        .filter_map(|(index, item)| (Some(index) != current_index).then_some(item))
        .collect::<Vec<_>>();
    let mut history = serde_json::Value::Array(history);
    let mut current_input = current_input;
    canonicalize_request_identities(&mut history);
    canonicalize_request_identities(&mut current_input);
    let mut prompt_cache_key = body
        .get("prompt_cache_key")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    canonicalize_request_identities(&mut prompt_cache_key);
    AbRequestComponentSnapshot {
        stage: stage.to_string(),
        envelope_sha256: request_envelope_sha256(body),
        instructions_sha256: sha256_json(body.get("instructions")),
        tool_schemas_sha256: sha256_json(body.get("tools")),
        history_sha256: sha256_json(Some(&history)),
        current_input_sha256: sha256_json(Some(&current_input)),
        prompt_cache_key_sha256: sha256_json(Some(&prompt_cache_key)),
    }
}

fn request_envelope_sha256(body: &serde_json::Value) -> String {
    let mut envelope = body.clone();
    if let Some(object) = envelope.as_object_mut() {
        for prompt_field in ["instructions", "tools", "input", "prompt_cache_key"] {
            object.remove(prompt_field);
        }
    }
    canonicalize_request_identities(&mut envelope);
    sha256_json(Some(&envelope))
}

fn canonical_request_body_sha256(request: &wiremock::Request) -> String {
    let mut body = high_volume_request_body_json(request);
    canonicalize_request_identities(&mut body);
    sha256_json(Some(&body))
}

fn canonicalize_request_identities(value: &mut serde_json::Value) {
    fn is_opaque_identity_key(key: &str) -> bool {
        key == "id" || key == "prompt_cache_key" || key.ends_with("_id") || key.ends_with("Id")
    }

    fn visit(
        value: &mut serde_json::Value,
        identities: &mut BTreeMap<String, String>,
        next_identity: &mut usize,
    ) {
        match value {
            serde_json::Value::Array(values) => {
                for value in values {
                    visit(value, identities, next_identity);
                }
            }
            serde_json::Value::Object(object) => {
                for (key, value) in object {
                    if is_opaque_identity_key(key)
                        && let serde_json::Value::String(identity) = value
                    {
                        let placeholder = identities
                            .entry(identity.clone())
                            .or_insert_with(|| {
                                let placeholder =
                                    format!("<opaque-request-identity-{next_identity}>");
                                *next_identity = next_identity.saturating_add(1);
                                placeholder
                            })
                            .clone();
                        *value = serde_json::Value::String(placeholder);
                    } else {
                        visit(value, identities, next_identity);
                    }
                }
            }
            serde_json::Value::String(text) => {
                *text = canonicalize_volatile_request_text(text, identities, next_identity);
            }
            _ => {}
        }
    }

    visit(value, &mut BTreeMap::new(), &mut 0);
}

fn canonicalize_volatile_request_text(
    text: &str,
    identities: &mut BTreeMap<String, String>,
    next_identity: &mut usize,
) -> String {
    static UUID: OnceLock<Regex> = OnceLock::new();
    static SKILL_LOCATOR: OnceLock<Regex> = OnceLock::new();
    static TEMP_DIR: OnceLock<Regex> = OnceLock::new();
    static TURN_STARTED_AT: OnceLock<Regex> = OnceLock::new();

    let uuid = UUID.get_or_init(|| {
        Regex::new(r"(?i)[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}")
            .unwrap_or_else(|error| panic!("UUID benchmark regex must compile: {error}"))
    });
    let mut canonical = String::with_capacity(text.len());
    let mut cursor = 0;
    for matched in uuid.find_iter(text) {
        canonical.push_str(&text[cursor..matched.start()]);
        let identity = matched.as_str();
        let placeholder = identities
            .entry(identity.to_string())
            .or_insert_with(|| {
                let placeholder = format!("<opaque-request-identity-{next_identity}>");
                *next_identity = next_identity.saturating_add(1);
                placeholder
            })
            .clone();
        canonical.push_str(&placeholder);
        cursor = matched.end();
    }
    canonical.push_str(&text[cursor..]);

    let canonical = SKILL_LOCATOR
        .get_or_init(|| {
            Regex::new(r"skill:[0-9a-f]{24}")
                .unwrap_or_else(|error| panic!("skill locator regex must compile: {error}"))
        })
        .replace_all(&canonical, "skill:<opaque-locator>");
    let canonical = TEMP_DIR
        .get_or_init(|| {
            Regex::new(r"\.tmp[A-Za-z0-9]+")
                .unwrap_or_else(|error| panic!("temporary directory regex must compile: {error}"))
        })
        .replace_all(&canonical, ".tmp<opaque>");
    TURN_STARTED_AT
        .get_or_init(|| {
            Regex::new(r#"turn_started_at_unix_ms":\d+"#)
                .unwrap_or_else(|error| panic!("turn timestamp regex must compile: {error}"))
        })
        .replace_all(&canonical, "turn_started_at_unix_ms\":<time>")
        .into_owned()
}

fn sha256_json(value: Option<&serde_json::Value>) -> String {
    sha256_bytes(
        &serde_json::to_vec(value.unwrap_or(&serde_json::Value::Null))
            .unwrap_or_else(|error| panic!("benchmark JSON value must serialize: {error}")),
    )
}

fn request_component_delta(
    previous: &AbRequestComponentSnapshot,
    current: &AbRequestComponentSnapshot,
) -> AbRequestComponentDelta {
    let values = [
        (
            "instructions",
            &previous.instructions_sha256,
            &current.instructions_sha256,
        ),
        (
            "tool_schemas",
            &previous.tool_schemas_sha256,
            &current.tool_schemas_sha256,
        ),
        ("history", &previous.history_sha256, &current.history_sha256),
        (
            "current_input",
            &previous.current_input_sha256,
            &current.current_input_sha256,
        ),
        (
            "prompt_cache_key",
            &previous.prompt_cache_key_sha256,
            &current.prompt_cache_key_sha256,
        ),
    ];
    let mut changed_components = Vec::new();
    let mut reused_components = Vec::new();
    for (name, previous, current) in values {
        if previous == current {
            reused_components.push(name.to_string());
        } else {
            changed_components.push(name.to_string());
        }
    }
    AbRequestComponentDelta {
        compared_to_previous: true,
        changed_components,
        reused_components,
    }
}

fn history_seed_turns_visible(body: &serde_json::Value) -> u32 {
    body.get("input")
        .and_then(serde_json::Value::as_array)
        .map(|input| {
            input
                .iter()
                .filter(|item| {
                    item.get("role").and_then(serde_json::Value::as_str) == Some("user")
                        && item.to_string().contains(AB_HISTORY_SEED_PREFIX)
                })
                .count()
                .min(u32::MAX as usize) as u32
        })
        .unwrap_or_default()
}

const AB_ROLLBACK_TIMEOUT: Duration = Duration::from_secs(30);
const AB_ROLLBACK_RETRY_DELAY: Duration = Duration::from_millis(10);
const TURN_IN_PROGRESS_ROLLBACK_ERROR: &str = "Cannot rollback while a turn is in progress.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BenchmarkRollbackEventAction {
    Continue,
    Complete,
    Retry,
}

fn benchmark_rollback_event_action(event: &EventMsg) -> Result<BenchmarkRollbackEventAction> {
    match event {
        EventMsg::ThreadRolledBack(_) => Ok(BenchmarkRollbackEventAction::Complete),
        EventMsg::Error(error)
            if error.codex_error_info == Some(CodexErrorInfo::ThreadRollbackFailed)
                && error.message == TURN_IN_PROGRESS_ROLLBACK_ERROR =>
        {
            Ok(BenchmarkRollbackEventAction::Retry)
        }
        EventMsg::Error(error) => {
            anyhow::bail!("A/B benchmark rollback failed: {}", error.message)
        }
        _ => Ok(BenchmarkRollbackEventAction::Continue),
    }
}

async fn rollback_test_turns(test: &TestCodex, num_turns: u32) -> Result<()> {
    let deadline = Instant::now() + AB_ROLLBACK_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            anyhow::bail!("timed out rolling back A/B benchmark turns");
        }
        tokio::time::timeout(
            remaining,
            test.codex.submit(Op::ThreadRollback { num_turns }),
        )
        .await
        .context("timed out rolling back A/B benchmark turns")??;

        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                anyhow::bail!("timed out rolling back A/B benchmark turns");
            }
            let event = tokio::time::timeout(remaining, test.codex.next_event())
                .await
                .context("timed out rolling back A/B benchmark turns")??;
            match benchmark_rollback_event_action(&event.msg)? {
                BenchmarkRollbackEventAction::Continue => {}
                BenchmarkRollbackEventAction::Complete => return Ok(()),
                BenchmarkRollbackEventAction::Retry => break,
            }
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            anyhow::bail!("timed out rolling back A/B benchmark turns");
        }
        tokio::time::sleep(AB_ROLLBACK_RETRY_DELAY.min(remaining)).await;
    }
}

struct ToolGateFixture {
    _server: wiremock::MockServer,
    test: TestCodex,
    request_capture: HighVolumeRequestCapture,
    workload: AbWorkload,
}

struct ToolGateResponder {
    fixture_id: String,
    fixture_program: PathBuf,
    workload: AbWorkload,
    response_count: AtomicUsize,
}

fn tool_gate_continuation_events(
    response_id: &str,
    workload: AbWorkload,
    _observed_output_count: usize,
) -> Vec<serde_json::Value> {
    // The sample validator records an exact output-count failure. The provider
    // fixture must still finish the turn so a defective baseline is retained
    // as an A sample instead of panicking and retrying the request forever.
    let reply = match workload {
        AbWorkload::SingleDirectToolCall => AB_SINGLE_DIRECT_REPLY,
        AbWorkload::ParallelSafeTripleDirect => AB_PARALLEL_TRIPLE_REPLY,
        AbWorkload::ExclusiveGateSerialization => AB_EXCLUSIVE_GATE_REPLY,
        other => panic!("tool-gate responder cannot complete {}", other.name()),
    };
    vec![
        ev_assistant_message(response_id, reply),
        ev_completed_with_usage(response_id, 1_280, 1_024, 16, 0),
    ]
}

impl wiremock::Respond for ToolGateResponder {
    fn respond(&self, request: &wiremock::Request) -> wiremock::ResponseTemplate {
        let request_index = self.response_count.fetch_add(1, Ordering::SeqCst);
        let response_id = format!("{}-{request_index}", self.fixture_id);
        let body = high_volume_request_body_json(request);
        let outputs = request_function_call_outputs_after_current_input(&body);
        let events = if outputs.is_empty() {
            let mut events = vec![ev_response_created(&response_id)];
            for call_index in 0..self.workload.expected_direct_tool_calls() {
                let call_id = format!("{response_id}-direct-{call_index}");
                let (tool_name, arguments) = match self.workload {
                    AbWorkload::SingleDirectToolCall => {
                        ("test_sync_tool", serde_json::json!({"sleep_before_ms": 5}))
                    }
                    AbWorkload::ParallelSafeTripleDirect => (
                        "test_sync_tool",
                        serde_json::json!({
                            "barrier": {
                                "id": format!("{response_id}-parallel-barrier"),
                                "participants": 3,
                                "timeout_ms": 5_000,
                            },
                        }),
                    ),
                    AbWorkload::ExclusiveGateSerialization if call_index < 2 => (
                        "exec_command",
                        serde_json::json!({
                            "kind": "argv",
                            "program": self.fixture_program,
                            "args": ["ab-exclusive-gate-child"],
                            "yield_time_ms": AB_EXCLUSIVE_GATE_YIELD_TIME_MS,
                            "tty": true,
                        }),
                    ),
                    AbWorkload::ExclusiveGateSerialization => {
                        ("test_sync_tool", serde_json::json!({"sleep_before_ms": 75}))
                    }
                    other => panic!("tool-gate responder cannot serve {}", other.name()),
                };
                events.push(ev_function_call(
                    &call_id,
                    tool_name,
                    &serde_json::to_string(&arguments).unwrap_or_else(|error| {
                        panic!("serialize tool-gate benchmark arguments: {error}")
                    }),
                ));
            }
            events.push(ev_completed_with_usage(&response_id, 1_024, 768, 32, 8));
            events
        } else {
            tool_gate_continuation_events(&response_id, self.workload, outputs.len())
        };
        wiremock::ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(sse(events))
    }
}

impl ToolGateFixture {
    async fn start(workload: AbWorkload, fixture_id: &str) -> Result<Self> {
        anyhow::ensure!(
            matches!(
                workload,
                AbWorkload::SingleDirectToolCall
                    | AbWorkload::ParallelSafeTripleDirect
                    | AbWorkload::ExclusiveGateSerialization
            ),
            "tool-gate fixture received incompatible workload {}",
            workload.name()
        );
        let server = start_mock_server().await;
        let request_capture = HighVolumeRequestCapture::default();
        let fixture_program = std::env::current_exe()
            .context("resolve turn-latency exclusive-gate fixture executable")?;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path_regex(".*/responses$"))
            .and(request_capture.clone())
            .respond_with(ToolGateResponder {
                fixture_id: fixture_id.to_string(),
                fixture_program,
                workload,
                response_count: AtomicUsize::new(0),
            })
            .mount(&server)
            .await;
        let test = test_codex()
            .with_model("test-gpt-5.1-codex")
            .with_config(move |config| {
                pin_ab_reasoning_phase_efforts(config);
                if workload == AbWorkload::ExclusiveGateSerialization {
                    let _ = config.features.enable(Feature::UnifiedExec);
                }
            })
            .build(&server)
            .await?;
        if workload == AbWorkload::ExclusiveGateSerialization {
            fs::create_dir_all(test.config.cwd.join(".git"))
                .context("create exclusive-gate benchmark repository marker")?;
        }
        Ok(Self {
            _server: server,
            test,
            request_capture,
            workload,
        })
    }

    fn prompt(&self) -> &'static str {
        match self.workload {
            AbWorkload::SingleDirectToolCall => AB_SINGLE_DIRECT_PROMPT,
            AbWorkload::ParallelSafeTripleDirect => AB_PARALLEL_TRIPLE_PROMPT,
            AbWorkload::ExclusiveGateSerialization => AB_EXCLUSIVE_GATE_PROMPT,
            other => panic!("tool-gate fixture cannot prompt {}", other.name()),
        }
    }

    fn expected_reply(&self) -> &'static str {
        match self.workload {
            AbWorkload::SingleDirectToolCall => AB_SINGLE_DIRECT_REPLY,
            AbWorkload::ParallelSafeTripleDirect => AB_PARALLEL_TRIPLE_REPLY,
            AbWorkload::ExclusiveGateSerialization => AB_EXCLUSIVE_GATE_REPLY,
            other => panic!("tool-gate fixture cannot validate {}", other.name()),
        }
    }

    async fn sample(&self) -> Sample {
        let requests_before = self.request_capture.request_count();
        let started = Instant::now();
        let completion = self
            .test
            .submit_turn_and_capture_completion(self.prompt())
            .await;
        let duration_ns = started.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        let turn_requests = self.request_capture.requests_since(requests_before);
        let mut sample = match completion {
            Ok(completion) => {
                let timing = completion.timing.as_ref();
                let mut sample = timing.map(sample_from_timing).unwrap_or_default();
                sample.duration_ns = duration_ns;
                sample.workload_subturns = 1;
                sample.terminal_event = "turn_complete".to_string();
                sample.typed_error_count = u32::from(completion.error.is_some());
                sample.final_response_present = completion.last_agent_message.is_some();
                if completion.error.is_some() {
                    sample
                        .failure_codes
                        .push("unexpected_terminal_error".to_string());
                }
                if completion.last_agent_message.as_deref() != Some(self.expected_reply()) {
                    sample.failure_codes.push("wrong_final_message".to_string());
                }
                if turn_requests.len() != 2 {
                    sample.failure_codes.push("request_count".to_string());
                }
                if sample.logical_generations != 2
                    || sample.provider_attempts != 2
                    || sample.sampling_requests != 2
                {
                    sample.failure_codes.push("generation_count".to_string());
                }
                let expected_calls = self.workload.expected_direct_tool_calls();
                if sample.tool_calls != expected_calls
                    || sample.direct_tool_calls != expected_calls
                    || sample.nested_tool_calls != 0
                    || sample.paired_tool_calls != expected_calls
                {
                    sample.failure_codes.push("tool_graph_counts".to_string());
                }
                if !tool_graph_matches_workload(&sample, self.workload) {
                    sample.failure_codes.push("tool_graph_identity".to_string());
                }
                if !tool_gate_execution_matches(&sample, self.workload) {
                    sample.failure_codes.push("tool_gate_execution".to_string());
                }
                if !timing.is_some_and(timing_reconciles) {
                    sample
                        .failure_codes
                        .push("timing_reconciliation".to_string());
                }
                let outputs = turn_requests
                    .get(1)
                    .map(high_volume_request_body_json)
                    .as_ref()
                    .map(request_function_call_outputs_after_current_input)
                    .unwrap_or_default();
                if outputs.len() != expected_calls as usize {
                    sample.failure_codes.push("tool_output_count".to_string());
                }
                if self.workload == AbWorkload::ExclusiveGateSerialization {
                    let child_outputs = outputs
                        .iter()
                        .filter(|(_, output)| output.contains(AB_EXCLUSIVE_GATE_CHILD_MARKER))
                        .count();
                    let safe_outputs = outputs
                        .iter()
                        .filter(|(_, output)| {
                            !output.contains(AB_EXCLUSIVE_GATE_CHILD_MARKER)
                                && output.contains("ok")
                        })
                        .count();
                    if child_outputs != 2 || safe_outputs != 1 {
                        sample
                            .failure_codes
                            .push("exclusive_child_output".to_string());
                    }
                }
                if sample.incomplete_lifecycle_calls != 0 || !sample.lifecycle_complete {
                    sample.failure_codes.push("lifecycle_coverage".to_string());
                }
                if !sample.latency_eligible {
                    sample.failure_codes.push("latency_ineligible".to_string());
                }
                sample.serialized_bytes = turn_requests
                    .iter()
                    .map(|request| request.body.len() as u64)
                    .sum();
                sample.cache_hits = sample.workspace_evidence_cache_hits;
                sample.exec_description_tokens = turn_requests
                    .first()
                    .map(high_volume_request_body_json)
                    .as_ref()
                    .map(exec_description_tokens_from_body)
                    .unwrap_or_default();
                sample.prompt_input_tokens = turn_requests
                    .iter()
                    .map(high_volume_request_body_json)
                    .map(|body| prompt_input_tokens_from_body(&body))
                    .sum();
                sample
            }
            Err(error) => Sample {
                duration_ns,
                workload_subturns: 1,
                failed: true,
                failure_codes: vec![format!("completion_error:{error}")],
                ..Sample::default()
            },
        };
        if let Err(error) = rollback_test_turns(&self.test, 1).await {
            sample
                .failure_codes
                .push(format!("sample_history_rollback:{error}"));
        }
        sample.failed = !sample.failure_codes.is_empty();
        sample
    }
}

struct AbortDirectNestedFixture {
    _server: wiremock::MockServer,
    test: TestCodex,
    request_capture: HighVolumeRequestCapture,
}

struct AbortDirectNestedResponder {
    fixture_id: String,
    response_count: AtomicUsize,
}

impl wiremock::Respond for AbortDirectNestedResponder {
    fn respond(&self, request: &wiremock::Request) -> wiremock::ResponseTemplate {
        let request_index = self.response_count.fetch_add(1, Ordering::SeqCst);
        let response_id = format!("{}-{request_index}", self.fixture_id);
        let body = high_volume_request_body_json(request);
        let events = if request_contains_tool_output_after_current_input(&body) {
            vec![
                ev_assistant_message(&response_id, AB_ABORT_FORBIDDEN_RESUME_REPLY),
                ev_completed_with_usage(&response_id, 1_280, 1_024, 8, 0),
            ]
        } else {
            let call_id = format!("{response_id}-exec");
            vec![
                ev_response_created(&response_id),
                ev_custom_tool_call(&call_id, "exec", AB_ABORT_DIRECT_NESTED_SOURCE),
                ev_completed_with_usage(&response_id, 1_024, 768, 24, 16),
            ]
        };
        wiremock::ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(sse(events))
    }
}

async fn submit_abort_direct_nested_turn(test: &TestCodex) -> Result<String> {
    let (sandbox_policy, permission_profile) =
        turn_permission_fields(PermissionProfile::read_only(), test.config.cwd.as_path());
    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: AB_ABORT_DIRECT_NESTED_PROMPT.into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: codex_protocol::protocol::ThreadSettingsOverrides {
                approval_policy: Some(AskForApproval::OnRequest),
                sandbox_policy: Some(sandbox_policy),
                permission_profile,
                collaboration_mode: Some(codex_protocol::config_types::CollaborationMode {
                    mode: codex_protocol::config_types::ModeKind::Default,
                    settings: codex_protocol::config_types::Settings {
                        model: test.session_configured.model.clone(),
                        reasoning_effort: None,
                        developer_instructions: None,
                    },
                }),
                ..Default::default()
            },
        })
        .await?;

    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let event = test.codex.next_event().await?;
            match event.msg {
                EventMsg::TurnStarted(event) => return Ok(event.turn_id),
                EventMsg::Error(event) => {
                    anyhow::bail!("turn failed before start: {}", event.message)
                }
                _ => {}
            }
        }
    })
    .await
    .context("timed out waiting for abort benchmark turn start")?
}

fn abort_reason_name(reason: &TurnAbortReason) -> &'static str {
    // The benchmark sources are overlaid onto both revisions. Keep this match
    // compatible with baselines from before `InternalError` was added.
    #[allow(unreachable_patterns)]
    match reason {
        TurnAbortReason::Interrupted => "interrupted",
        TurnAbortReason::Replaced => "replaced",
        TurnAbortReason::ReviewEnded => "review_ended",
        TurnAbortReason::BudgetLimited => "budget_limited",
        _ => "internal_error",
    }
}

fn record_abort_registration_snapshot(
    sample: &mut Sample,
    timing: &TurnTiming,
    barrier_call_id: String,
) {
    let nested = timing.tool_calls.iter().find(|call| {
        call.source == TurnTimingToolCallSource::CodeMode && call.call_id == barrier_call_id
    });
    let direct = nested
        .and_then(|nested| nested.parent_call_id.as_deref())
        .and_then(|parent_call_id| {
            timing.tool_calls.iter().find(|call| {
                call.source == TurnTimingToolCallSource::Direct && call.call_id == parent_call_id
            })
        })
        .or_else(|| {
            timing
                .tool_calls
                .iter()
                .find(|call| call.source == TurnTimingToolCallSource::Direct)
        });
    for call in [direct, nested].into_iter().flatten() {
        sample.abort_registered_call_ids.push(call.call_id.clone());
        sample
            .abort_terminal_outcomes_by_registration
            .push(call.outcome.clone().unwrap_or_default());
    }
    sample.abort_barrier_call_id = Some(barrier_call_id);
    // Earlier retained-process resumes are necessary to issue the two
    // correlated polls. Only a resume from the abort-generation calls is
    // avoidable; the poll count is reported separately.
    sample.abort_model_resumed_call_count = [direct, nested]
        .into_iter()
        .flatten()
        .filter(|call| call.model_resumed_at_ms.is_some())
        .count()
        .min(u32::MAX as usize) as u32;
}

impl AbortDirectNestedFixture {
    async fn start(code_mode_host: &Path, fixture_id: &str) -> Result<Self> {
        let server = start_mock_server().await;
        let request_capture = HighVolumeRequestCapture::default();
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path_regex(".*/responses$"))
            .and(request_capture.clone())
            .respond_with(AbortDirectNestedResponder {
                fixture_id: fixture_id.to_string(),
                response_count: AtomicUsize::new(0),
            })
            .mount(&server)
            .await;
        let test = test_codex()
            .with_model("test-gpt-5.1-codex")
            .with_code_mode_host_program(code_mode_host.to_path_buf())
            .with_config(|config| {
                pin_ab_reasoning_phase_efforts(config);
                let _ = config.features.enable(Feature::CodeMode);
                let _ = config.features.enable(Feature::RequestPermissionsTool);
            })
            .build(&server)
            .await?;
        Ok(Self {
            _server: server,
            test,
            request_capture,
        })
    }

    async fn sample(&self) -> Sample {
        let requests_before = self.request_capture.request_count();
        let started = Instant::now();
        let mut failure_codes = Vec::new();
        let mut typed_error_count = 0_u32;
        let mut final_response_present = false;
        let mut forged_turn_complete_observed = false;

        let turn_id = match submit_abort_direct_nested_turn(&self.test).await {
            Ok(turn_id) => turn_id,
            Err(error) => {
                return Sample {
                    duration_ns: started.elapsed().as_nanos().min(u64::MAX as u128) as u64,
                    workload_subturns: 1,
                    failed: true,
                    failure_codes: vec![format!("turn_start:{error}")],
                    ..Sample::default()
                };
            }
        };

        let barrier = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let event = self.test.codex.next_event().await?;
                match event.msg {
                    EventMsg::RequestPermissions(request) if request.turn_id == turn_id => {
                        return Ok(request.call_id);
                    }
                    EventMsg::AgentMessage(_) => final_response_present = true,
                    EventMsg::Error(event) => {
                        anyhow::bail!("turn failed before interrupt: {}", event.message)
                    }
                    EventMsg::TurnComplete(event) if event.turn_id == turn_id => {
                        anyhow::bail!("turn completed before interrupt")
                    }
                    EventMsg::TurnAborted(event)
                        if event.turn_id.as_deref() == Some(turn_id.as_str()) =>
                    {
                        anyhow::bail!("turn aborted before explicit interrupt")
                    }
                    _ => {}
                }
            }
        })
        .await
        .context("timed out waiting for request_permissions interrupt barrier")
        .and_then(|result| result);

        let barrier_call_id = match barrier {
            Ok(call_id) => call_id,
            Err(error) => {
                failure_codes.push(format!("interrupt_barrier:{error}"));
                String::new()
            }
        };
        if let Err(error) = self.test.codex.submit(Op::Interrupt).await {
            failure_codes.push(format!("interrupt_submit:{error}"));
        }

        let terminal = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let event = self.test.codex.next_event().await?;
                match event.msg {
                    EventMsg::TurnAborted(event)
                        if event.turn_id.as_deref() == Some(turn_id.as_str()) =>
                    {
                        return Ok((
                            "turn_aborted".to_string(),
                            Some(abort_reason_name(&event.reason).to_string()),
                            event.timing,
                        ));
                    }
                    EventMsg::TurnComplete(event) if event.turn_id == turn_id => {
                        forged_turn_complete_observed = true;
                        final_response_present |= event.last_agent_message.is_some();
                        return Ok(("turn_complete".to_string(), None, event.timing));
                    }
                    EventMsg::AgentMessage(_) => final_response_present = true,
                    EventMsg::Error(error) => {
                        eprintln!("replay-retained-error {error:?}");
                        typed_error_count = typed_error_count.saturating_add(1);
                    }
                    _ => {}
                }
            }
        })
        .await
        .context("timed out waiting for terminal event after interrupt")
        .and_then(|result| result);

        let duration_ns = started.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        let (terminal_event, abort_reason, timing) = match terminal {
            Ok(terminal) => terminal,
            Err(error) => {
                failure_codes.push(format!("abort_terminal:{error}"));
                (String::new(), None, None)
            }
        };
        let mut sample = timing
            .as_ref()
            .map(sample_from_terminal_abort_timing)
            .unwrap_or_default();
        sample.duration_ns = duration_ns;
        sample.workload_subturns = 1;
        sample.terminal_event = terminal_event;
        sample.abort_reason = abort_reason;
        sample.typed_error_count = typed_error_count;
        sample.final_response_present = final_response_present;
        sample.forged_turn_complete_observed = forged_turn_complete_observed;
        sample.latency_eligible = false;
        if let Some(timing) = timing.as_ref() {
            record_abort_registration_snapshot(&mut sample, timing, barrier_call_id);
            if !timing_reconciles(timing) {
                failure_codes.push("timing_reconciliation".to_string());
            }
        }
        let turn_requests = self.request_capture.requests_since(requests_before);
        if turn_requests.len() != 1 {
            failure_codes.push("forbidden_model_resume".to_string());
        }
        sample.serialized_bytes = turn_requests
            .iter()
            .map(|request| request.body.len() as u64)
            .sum();
        sample.exec_description_tokens = turn_requests
            .first()
            .map(high_volume_request_body_json)
            .as_ref()
            .map(exec_description_tokens_from_body)
            .unwrap_or_default();
        sample.prompt_input_tokens = turn_requests
            .iter()
            .map(high_volume_request_body_json)
            .map(|body| prompt_input_tokens_from_body(&body))
            .sum();
        if !abort_direct_nested_lifecycle_matches(&sample) {
            failure_codes.push("abort_direct_nested_contract".to_string());
        }
        sample.failure_codes.append(&mut failure_codes);
        if let Err(error) = rollback_test_turns(&self.test, 1).await {
            sample
                .failure_codes
                .push(format!("sample_history_rollback:{error}"));
        }
        sample.failed = !sample.failure_codes.is_empty();
        sample
    }
}

struct AbortRetainedProcessFixture {
    _server: wiremock::MockServer,
    test: TestCodex,
    request_capture: HighVolumeRequestCapture,
}

struct AbortRetainedProcessResponder {
    fixture_id: String,
    fixture_program: PathBuf,
    response_count: AtomicUsize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AbortRetainedProcessResponseRoute {
    IssueExecCommand,
    RejectUnexpectedResume,
}

fn abort_retained_process_response_route(
    body: &serde_json::Value,
) -> AbortRetainedProcessResponseRoute {
    if request_function_call_outputs_after_current_input(body).is_empty() {
        AbortRetainedProcessResponseRoute::IssueExecCommand
    } else {
        AbortRetainedProcessResponseRoute::RejectUnexpectedResume
    }
}

impl wiremock::Respond for AbortRetainedProcessResponder {
    fn respond(&self, request: &wiremock::Request) -> wiremock::ResponseTemplate {
        let request_index = self.response_count.fetch_add(1, Ordering::SeqCst);
        let response_id = format!("{}-{request_index}", self.fixture_id);
        let body = high_volume_request_body_json(request);
        let events = match abort_retained_process_response_route(&body) {
            AbortRetainedProcessResponseRoute::IssueExecCommand => {
                let call_id = format!("{response_id}-exec-command");
                let arguments = serde_json::json!({
                    "kind": "argv",
                    "program": self.fixture_program,
                    "args": ["ab-retained-child"],
                    "yield_time_ms": AB_ABORT_RETAINED_YIELD_TIME_MS,
                    "tty": true,
                });
                vec![
                    ev_response_created(&response_id),
                    ev_function_call(
                        &call_id,
                        "exec_command",
                        &serde_json::to_string(&arguments).unwrap_or_else(|error| {
                            panic!("serialize retained abort exec arguments: {error}")
                        }),
                    ),
                    ev_completed_with_usage(&response_id, 1_024, 768, 24, 8),
                ]
            }
            AbortRetainedProcessResponseRoute::RejectUnexpectedResume => vec![
                ev_assistant_message(&response_id, AB_ABORT_RETAINED_FORBIDDEN_RESUME_REPLY),
                ev_completed_with_usage(&response_id, 1_280, 1_024, 8, 0),
            ],
        };
        wiremock::ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(sse(events))
    }
}

async fn submit_abort_retained_turn(
    test: &TestCodex,
    accept_nested_permission_call: bool,
) -> Result<String> {
    let (sandbox_policy, permission_profile) =
        turn_permission_fields(PermissionProfile::Disabled, test.config.cwd.as_path());
    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: AB_ABORT_RETAINED_PROMPT.into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: codex_protocol::protocol::ThreadSettingsOverrides {
                approval_policy: Some(if accept_nested_permission_call {
                    AskForApproval::OnRequest
                } else {
                    AskForApproval::Never
                }),
                sandbox_policy: Some(sandbox_policy),
                permission_profile,
                collaboration_mode: Some(codex_protocol::config_types::CollaborationMode {
                    mode: codex_protocol::config_types::ModeKind::Default,
                    settings: codex_protocol::config_types::Settings {
                        model: test.session_configured.model.clone(),
                        reasoning_effort: None,
                        developer_instructions: None,
                    },
                }),
                ..Default::default()
            },
        })
        .await?;

    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let event = test.codex.next_event().await?;
            match event.msg {
                EventMsg::TurnStarted(event) => return Ok(event.turn_id),
                EventMsg::Error(event) => {
                    anyhow::bail!("retained abort turn failed before start: {}", event.message)
                }
                _ => {}
            }
        }
    })
    .await
    .context("timed out waiting for retained abort turn start")?
}

fn retained_abort_identity_barrier(
    expected_turn_id: &str,
    event_turn_id: &str,
    call_id: String,
    process_id: Option<String>,
) -> Result<Option<(String, String)>> {
    if event_turn_id != expected_turn_id {
        return Ok(None);
    }
    let process_id = process_id.context("retained exec begin did not expose process identity")?;
    Ok(Some((call_id, process_id)))
}

impl AbortRetainedProcessFixture {
    async fn start(fixture_id: &str) -> Result<Self> {
        let server = start_mock_server().await;
        let request_capture = HighVolumeRequestCapture::default();
        let fixture_program = std::env::current_exe()
            .context("resolve turn-latency retained-abort fixture executable")?;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path_regex(".*/responses$"))
            .and(request_capture.clone())
            .respond_with(AbortRetainedProcessResponder {
                fixture_id: fixture_id.to_string(),
                fixture_program,
                response_count: AtomicUsize::new(0),
            })
            .mount(&server)
            .await;
        let test = test_codex()
            .with_model("test-gpt-5.1-codex")
            .with_config(|config| {
                pin_ab_reasoning_phase_efforts(config);
                let _ = config.features.enable(Feature::UnifiedExec);
            })
            .build(&server)
            .await?;
        Ok(Self {
            _server: server,
            test,
            request_capture,
        })
    }

    async fn sample(&self) -> Sample {
        let requests_before = self.request_capture.request_count();
        let started = Instant::now();
        let mut failure_codes = Vec::new();
        let mut typed_error_count = 0_u32;
        let mut final_response_present = false;
        let mut forged_turn_complete_observed = false;

        let turn_id = match submit_abort_retained_turn(&self.test, false).await {
            Ok(turn_id) => turn_id,
            Err(error) => {
                return Sample {
                    duration_ns: started.elapsed().as_nanos().min(u64::MAX as u128) as u64,
                    workload_subturns: 1,
                    failed: true,
                    failure_codes: vec![format!("turn_start:{error}")],
                    ..Sample::default()
                };
            }
        };

        let barrier = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let event = self.test.codex.next_event().await?;
                match event.msg {
                    EventMsg::ExecCommandBegin(event) => {
                        if let Some(identity) = retained_abort_identity_barrier(
                            &turn_id,
                            &event.turn_id,
                            event.call_id,
                            event.process_id,
                        )? {
                            return Ok(identity);
                        }
                    }
                    EventMsg::AgentMessage(_) => final_response_present = true,
                    EventMsg::Error(event) => {
                        anyhow::bail!("turn failed before retained interrupt: {}", event.message)
                    }
                    EventMsg::TurnComplete(event) if event.turn_id == turn_id => {
                        forged_turn_complete_observed = true;
                        anyhow::bail!("retained-process turn completed before interrupt")
                    }
                    EventMsg::TurnAborted(event)
                        if event.turn_id.as_deref() == Some(turn_id.as_str()) =>
                    {
                        anyhow::bail!("retained-process turn aborted before explicit interrupt")
                    }
                    _ => {}
                }
            }
        })
        .await
        .context("timed out waiting for retained-process identity barrier")
        .and_then(|result| result);

        let (call_id, process_id) = match barrier {
            Ok(barrier) => barrier,
            Err(error) => {
                failure_codes.push(format!("retained_interrupt_barrier:{error}"));
                (String::new(), String::new())
            }
        };
        let retained_before_interrupt = if call_id.is_empty() || process_id.is_empty() {
            Vec::new()
        } else {
            match tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let terminals = self.test.codex.list_background_terminals().await;
                    if terminals.iter().any(|terminal| {
                        terminal.item_id == call_id && terminal.process_id == process_id
                    }) {
                        break terminals;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            {
                Ok(terminals) => terminals,
                Err(_) => {
                    failure_codes.push("retained_ownership_timeout".to_string());
                    Vec::new()
                }
            }
        };
        let retained_process_owned_before_abort = retained_before_interrupt.len() == 1
            && retained_before_interrupt.first().is_some_and(|terminal| {
                terminal.item_id == call_id && terminal.process_id == process_id
            });
        if !retained_process_owned_before_abort {
            failure_codes.push("retained_ownership_not_exact".to_string());
        }

        if let Err(error) = self.test.codex.submit(Op::Interrupt).await {
            failure_codes.push(format!("interrupt_submit:{error}"));
        }

        let mut persisted_outputs = Vec::new();
        let terminal = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let event = self.test.codex.next_event().await?;
                match event.msg {
                    EventMsg::RawResponseItem(raw) => {
                        if let ResponseItem::FunctionCallOutput {
                            call_id: output_call_id,
                            output,
                            ..
                        } = raw.item
                            && output_call_id == call_id
                            && let Some(content) = output.text_content()
                        {
                            persisted_outputs.push(content.to_string());
                        }
                    }
                    EventMsg::TurnAborted(event)
                        if event.turn_id.as_deref() == Some(turn_id.as_str()) =>
                    {
                        return Ok((
                            "turn_aborted".to_string(),
                            Some(abort_reason_name(&event.reason).to_string()),
                            event.timing,
                        ));
                    }
                    EventMsg::TurnComplete(event) if event.turn_id == turn_id => {
                        forged_turn_complete_observed = true;
                        final_response_present |= event.last_agent_message.is_some();
                        return Ok(("turn_complete".to_string(), None, event.timing));
                    }
                    EventMsg::AgentMessage(_) => final_response_present = true,
                    EventMsg::Error(_) => {
                        typed_error_count = typed_error_count.saturating_add(1);
                    }
                    _ => {}
                }
            }
        })
        .await
        .context("timed out waiting for retained-process terminal event")
        .and_then(|result| result);

        let duration_ns = started.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        let (terminal_event, abort_reason, timing) = match terminal {
            Ok(terminal) => terminal,
            Err(error) => {
                failure_codes.push(format!("abort_terminal:{error}"));
                (String::new(), None, None)
            }
        };
        let mut sample = timing
            .as_ref()
            .map(sample_from_terminal_abort_timing)
            .unwrap_or_default();
        sample.duration_ns = duration_ns;
        sample.workload_subturns = 1;
        sample.terminal_event = terminal_event;
        sample.abort_reason = abort_reason;
        sample.final_response_present = final_response_present;
        sample.forged_turn_complete_observed = forged_turn_complete_observed;
        sample.retained_process_owned_before_abort = retained_process_owned_before_abort;
        sample.retained_process_count_before_abort =
            retained_before_interrupt.len().min(u32::MAX as usize) as u32;
        sample.retained_abort_process_id = (!process_id.is_empty()).then_some(process_id);
        sample.retained_abort_persisted_result_count =
            persisted_outputs.len().min(u32::MAX as usize) as u32;
        sample.retained_abort_cancellation_observed =
            persisted_outputs.len() == 1 && persisted_outputs[0].contains("aborted");
        let retained_process_cleanup_complete = wait_for_retained_process_cleanup(|| {
            let codex = &self.test.codex;
            async move { codex.list_background_terminals().await.is_empty() }
        })
        .await;
        apply_retained_process_cleanup_observation(&mut sample, retained_process_cleanup_complete);
        sample.retained_process_exit_observed = timing.as_ref().is_some_and(|timing| {
            timing.tool_calls.iter().any(|call| {
                call.call_id == call_id
                    && call.process_spawned_at_ms.is_some()
                    && call.process_exited_at_ms.is_some()
                    && call.process_spawned_at_ms <= call.process_exited_at_ms
            })
        });
        sample.latency_eligible = false;
        if let Some(timing) = timing.as_ref() {
            record_abort_registration_snapshot(&mut sample, timing, call_id);
            if !timing_reconciles(timing) {
                failure_codes.push("timing_reconciliation".to_string());
            }
        }

        let late_completion_deadline = tokio::time::Instant::now() + Duration::from_millis(250);
        loop {
            let remaining =
                late_completion_deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, self.test.codex.next_event()).await {
                Ok(Ok(event)) => match event.msg {
                    EventMsg::TurnComplete(event) if event.turn_id == turn_id => {
                        sample.forged_turn_complete_observed = true;
                    }
                    EventMsg::Error(_) => {
                        typed_error_count = typed_error_count.saturating_add(1);
                    }
                    EventMsg::AgentMessage(_) => sample.final_response_present = true,
                    _ => {}
                },
                Ok(Err(_)) | Err(_) => break,
            }
        }
        sample.typed_error_count = typed_error_count;
        let turn_requests = self.request_capture.requests_since(requests_before);
        if turn_requests.len() != 1 {
            failure_codes.push("forbidden_model_resume".to_string());
        }
        sample.serialized_bytes = turn_requests
            .iter()
            .map(|request| request.body.len() as u64)
            .sum();
        sample.exec_description_tokens = turn_requests
            .first()
            .map(high_volume_request_body_json)
            .as_ref()
            .map(exec_description_tokens_from_body)
            .unwrap_or_default();
        sample.prompt_input_tokens = turn_requests
            .iter()
            .map(high_volume_request_body_json)
            .map(|body| prompt_input_tokens_from_body(&body))
            .sum();
        if !abort_retained_process_lifecycle_matches(&sample) {
            failure_codes.push("abort_retained_process_contract".to_string());
        }
        sample.failure_codes.append(&mut failure_codes);
        if let Err(error) = rollback_test_turns(&self.test, 1).await {
            sample
                .failure_codes
                .push(format!("sample_history_rollback:{error}"));
        }
        sample.failed = !sample.failure_codes.is_empty();
        sample
    }
}

struct RetainedExecFixture {
    _server: wiremock::MockServer,
    test: TestCodex,
    request_capture: HighVolumeRequestCapture,
}

struct RetainedExecResponder {
    fixture_id: String,
    fixture_program: PathBuf,
    response_count: AtomicUsize,
}

impl wiremock::Respond for RetainedExecResponder {
    fn respond(&self, request: &wiremock::Request) -> wiremock::ResponseTemplate {
        let request_index = self.response_count.fetch_add(1, Ordering::SeqCst);
        let response_id = format!("{}-{request_index}", self.fixture_id);
        let body = high_volume_request_body_json(request);
        let outputs = request_function_call_outputs_after_current_input(&body);
        let events = match outputs.len() {
            0 => {
                let call_id = format!("{response_id}-exec-command");
                let arguments = serde_json::json!({
                    "kind": "argv",
                    "program": self.fixture_program,
                    "args": ["ab-retained-child"],
                    "yield_time_ms": 10,
                    "tty": true,
                });
                vec![
                    ev_response_created(&response_id),
                    ev_function_call(
                        &call_id,
                        "exec_command",
                        &serde_json::to_string(&arguments).unwrap_or_else(|error| {
                            panic!("serialize retained exec arguments: {error}")
                        }),
                    ),
                    ev_completed_with_usage(&response_id, 1_024, 768, 24, 8),
                ]
            }
            1 | 2 => {
                let Some(session_id) = retained_session_id_from_output(&outputs[0].1)
                    .and_then(|session_id| session_id.parse::<u64>().ok())
                else {
                    unreachable!("retained exec output must expose a numeric session identity");
                };
                let terminal_poll = outputs.len() == 2;
                let call_id = format!(
                    "{response_id}-{}",
                    if terminal_poll {
                        "terminal-poll"
                    } else {
                        "live-poll"
                    }
                );
                let arguments = serde_json::json!({
                    "session_id": session_id,
                    "chars": if terminal_poll { "finish\n" } else { "poll\n" },
                    "yield_time_ms": if terminal_poll { 10_000 } else { 250 },
                });
                let (input, cached) = if terminal_poll {
                    (1_536, 1_280)
                } else {
                    (1_280, 1_024)
                };
                vec![
                    ev_response_created(&response_id),
                    ev_function_call(
                        &call_id,
                        "write_stdin",
                        &serde_json::to_string(&arguments).unwrap_or_else(|error| {
                            panic!("serialize retained poll arguments: {error}")
                        }),
                    ),
                    ev_completed_with_usage(&response_id, input, cached, 16, 0),
                ]
            }
            3 => vec![
                ev_assistant_message(&response_id, AB_RETAINED_EXEC_REPLY),
                ev_completed_with_usage(&response_id, 1_792, 1_536, 8, 0),
            ],
            count => panic!("retained exec fixture observed {count} current-turn tool outputs"),
        };
        wiremock::ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(sse(events))
    }
}

impl RetainedExecFixture {
    async fn start(fixture_id: &str) -> Result<Self> {
        let server = start_mock_server().await;
        let request_capture = HighVolumeRequestCapture::default();
        let fixture_program = std::env::current_exe()
            .context("resolve turn-latency retained-process fixture executable")?;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path_regex(".*/responses$"))
            .and(request_capture.clone())
            .respond_with(RetainedExecResponder {
                fixture_id: fixture_id.to_string(),
                fixture_program,
                response_count: AtomicUsize::new(0),
            })
            .mount(&server)
            .await;
        let test = test_codex()
            .with_model("test-gpt-5.1-codex")
            .with_config(|config| {
                pin_ab_reasoning_phase_efforts(config);
                let _ = config.features.enable(Feature::UnifiedExec);
            })
            .build(&server)
            .await?;
        Ok(Self {
            _server: server,
            test,
            request_capture,
        })
    }

    async fn sample(&self) -> Sample {
        let requests_before = self.request_capture.request_count();
        let started = Instant::now();
        let completion = self
            .test
            .submit_turn_and_capture_completion(AB_RETAINED_EXEC_PROMPT)
            .await;
        let duration_ns = started.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        let turn_requests = self.request_capture.requests_since(requests_before);
        let mut sample = match completion {
            Ok(completion) => {
                let timing = completion.timing.as_ref();
                let mut sample = timing.map(sample_from_timing).unwrap_or_default();
                sample.duration_ns = duration_ns;
                sample.workload_subturns = 1;
                sample.terminal_event = "turn_complete".to_string();
                sample.typed_error_count = u32::from(completion.error.is_some());
                sample.final_response_present = completion.last_agent_message.is_some();

                let exec_calls = sample
                    .tool_call_graph
                    .iter()
                    .filter(|call| call.tool_name == "exec_command")
                    .collect::<Vec<_>>();
                let poll_calls = sample
                    .tool_call_graph
                    .iter()
                    .filter(|call| call.tool_name == "write_stdin")
                    .collect::<Vec<_>>();
                sample.retained_write_stdin_poll_count =
                    poll_calls.len().min(u32::MAX as usize) as u32;
                let initial_output = exec_calls
                    .first()
                    .and_then(|call| retained_output_for_call(&turn_requests, &call.call_id));
                let live_poll_output = poll_calls
                    .first()
                    .and_then(|call| retained_output_for_call(&turn_requests, &call.call_id));
                let terminal_poll_output = poll_calls
                    .get(1)
                    .and_then(|call| retained_output_for_call(&turn_requests, &call.call_id));
                sample.retained_session_ids =
                    [initial_output.as_deref(), live_poll_output.as_deref()]
                        .into_iter()
                        .flatten()
                        .filter_map(retained_session_id_from_output)
                        .collect();
                sample.retained_process_exit_observed =
                    terminal_poll_output.as_deref().is_some_and(|output| {
                        retained_exit_code_from_output(output) == Some(0)
                            && output.contains(AB_RETAINED_FINISHED_MARKER)
                    });
                sample.retained_process_cleanup_complete =
                    self.test.codex.list_background_terminals().await.is_empty();

                if completion.error.is_some() {
                    sample
                        .failure_codes
                        .push("unexpected_terminal_error".to_string());
                }
                if completion.last_agent_message.as_deref() != Some(AB_RETAINED_EXEC_REPLY) {
                    sample.failure_codes.push("wrong_final_message".to_string());
                }
                if turn_requests.len() != 4 {
                    sample.failure_codes.push("request_count".to_string());
                }
                if sample.logical_generations != 4
                    || sample.provider_attempts != 4
                    || sample.sampling_requests != 4
                {
                    sample.failure_codes.push("generation_count".to_string());
                }
                if sample.tool_calls != 3
                    || sample.direct_tool_calls != 3
                    || sample.nested_tool_calls != 0
                    || sample.paired_tool_calls != 3
                {
                    sample.failure_codes.push("tool_graph_counts".to_string());
                }
                if !timing.is_some_and(timing_reconciles) {
                    sample
                        .failure_codes
                        .push("timing_reconciliation".to_string());
                }
                if !initial_output
                    .as_deref()
                    .is_some_and(|output| output.contains(AB_RETAINED_READY_MARKER))
                    || !live_poll_output
                        .as_deref()
                        .is_some_and(|output| output.contains(AB_RETAINED_POLL_MARKER))
                {
                    sample
                        .failure_codes
                        .push("retained_control_markers".to_string());
                }
                if sample.retained_session_ids.len() != 2
                    || sample.retained_session_ids[0] != sample.retained_session_ids[1]
                {
                    sample
                        .failure_codes
                        .push("retained_session_correlation".to_string());
                }
                if !sample.retained_process_exit_observed {
                    sample
                        .failure_codes
                        .push("retained_process_exit".to_string());
                }
                if !sample.retained_process_cleanup_complete {
                    sample
                        .failure_codes
                        .push("retained_process_cleanup".to_string());
                }
                record_retained_lifecycle_coverage_failures(&mut sample);
                sample.serialized_bytes = turn_requests
                    .iter()
                    .map(|request| request.body.len() as u64)
                    .sum();
                sample.exec_description_tokens = turn_requests
                    .first()
                    .map(high_volume_request_body_json)
                    .as_ref()
                    .map(exec_description_tokens_from_body)
                    .unwrap_or_default();
                sample.prompt_input_tokens = turn_requests
                    .iter()
                    .map(high_volume_request_body_json)
                    .map(|body| prompt_input_tokens_from_body(&body))
                    .sum();
                sample
            }
            Err(error) => Sample {
                duration_ns,
                workload_subturns: 1,
                failed: true,
                failure_codes: vec![format!("completion_error:{error}")],
                ..Sample::default()
            },
        };
        if let Err(error) = rollback_test_turns(&self.test, 1).await {
            sample
                .failure_codes
                .push(format!("sample_history_rollback:{error}"));
        }
        sample.failed = !sample.failure_codes.is_empty();
        sample
    }
}

fn run_ab_exclusive_gate_child() -> Result<()> {
    std::thread::sleep(Duration::from_millis(AB_EXCLUSIVE_GATE_CHILD_DELAY_MS));
    println!("{AB_EXCLUSIVE_GATE_CHILD_MARKER}");
    Ok(())
}

fn run_ab_retained_child() -> Result<()> {
    fn wait_for_control(reader: &mut impl Read, expected: &[u8]) -> Result<()> {
        let mut received = Vec::new();
        loop {
            let mut chunk = [0_u8; 64];
            let count = reader.read(&mut chunk)?;
            anyhow::ensure!(count != 0, "retained-process control stdin closed");
            received.extend_from_slice(&chunk[..count]);
            if received
                .windows(expected.len())
                .any(|window| window == expected)
            {
                return Ok(());
            }
        }
    }

    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let stdout = std::io::stdout();
    let mut writer = stdout.lock();
    writeln!(writer, "{AB_RETAINED_READY_MARKER}")?;
    writer.flush()?;
    wait_for_control(&mut reader, b"poll")?;
    writeln!(writer, "{AB_RETAINED_POLL_MARKER}")?;
    writer.flush()?;
    wait_for_control(&mut reader, b"finish")?;
    writeln!(writer, "{AB_RETAINED_FINISHED_MARKER}")?;
    writer.flush()?;
    std::thread::sleep(Duration::from_millis(100));
    Ok(())
}

fn run_ab_replay_command(mode: &str, paths: &[PathBuf]) -> Result<()> {
    fn print_broad_paths(path: &Path, remaining: &mut usize) -> Result<()> {
        if *remaining == 0 {
            return Ok(());
        }
        if path.is_file() {
            println!("{}", path.display());
            *remaining -= 1;
            return Ok(());
        }
        let mut children = fs::read_dir(path)
            .with_context(|| format!("read replay directory {}", path.display()))?
            .collect::<std::io::Result<Vec<_>>>()?;
        children.sort_by_key(std::fs::DirEntry::path);
        for child in children {
            print_broad_paths(&child.path(), remaining)?;
            if *remaining == 0 {
                break;
            }
        }
        Ok(())
    }

    anyhow::ensure!(
        !paths.is_empty(),
        "replay command requires at least one path"
    );
    match mode {
        "read" => {
            for path in paths {
                anyhow::ensure!(
                    path.is_file(),
                    "replay read target missing: {}",
                    path.display()
                );
                println!("{}", path.display());
                std::io::stdout().write_all(&fs::read(path)?)?;
                println!();
            }
        }
        "broad" => {
            let mut remaining = 20;
            for path in paths {
                print_broad_paths(path, &mut remaining)?;
            }
        }
        "validate" => {
            anyhow::ensure!(
                paths.len() == 2,
                "replay validation requires target and test"
            );
            let source = fs::read_to_string(&paths[0])?;
            let direct_test = fs::read_to_string(&paths[1])?;
            anyhow::ensure!(
                source.contains(AB_REPLAY_MUTATED_MARKER)
                    && direct_test.contains(AB_REPLAY_MUTATED_MARKER),
                "replay mutation is not visible to its direct test"
            );
            println!("{AB_REPLAY_VALIDATION_MARKER}");
        }
        "evidence" => {
            let diff_check = Command::new("git")
                .args(["diff", "--check"])
                .output()
                .context("run replay git diff --check")?;
            anyhow::ensure!(
                diff_check.status.success(),
                "replay git diff --check failed: {}",
                String::from_utf8_lossy(&diff_check.stderr)
            );
            for path in paths {
                println!("{}", path.display());
                std::io::stdout().write_all(&fs::read(path)?)?;
                println!();
            }
        }
        "review" => {
            anyhow::ensure!(paths.len() == 1, "replay review requires the exact target");
            let source = fs::read_to_string(&paths[0])?;
            anyhow::ensure!(
                source.contains(AB_REPLAY_MUTATED_MARKER),
                "replay review did not observe the mutation"
            );
            println!("{AB_REPLAY_MUTATED_MARKER}");
        }
        "artifact" => {
            anyhow::ensure!(
                paths.len() == 1,
                "replay artifact requires the exact target"
            );
            fs::write(&paths[0], AB_REPLAY_FOLLOW_UP_ARTIFACT_CONTENT)
                .with_context(|| format!("write replay artifact {}", paths[0].display()))?;
            println!("{AB_REPLAY_FOLLOW_UP_ARTIFACT_CONTENT}");
        }
        other => anyhow::bail!("unknown replay child command `{other}`"),
    }
    Ok(())
}

struct HighVolumeCodeModeFixture {
    _server: wiremock::MockServer,
    test: TestCodex,
    request_capture: HighVolumeRequestCapture,
}

#[derive(Clone, Debug, Default)]
struct HighVolumeRequestCapture {
    requests: Arc<Mutex<Vec<wiremock::Request>>>,
}

impl HighVolumeRequestCapture {
    fn request_count(&self) -> usize {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    fn requests_since(&self, index: usize) -> Vec<wiremock::Request> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)[index..]
            .to_vec()
    }
}

impl wiremock::Match for HighVolumeRequestCapture {
    fn matches(&self, request: &wiremock::Request) -> bool {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(request.clone());
        true
    }
}

struct HighVolumeResponder {
    fixture_id: String,
    tool_request_count: AtomicUsize,
    follow_up_count: AtomicUsize,
}

impl wiremock::Respond for HighVolumeResponder {
    fn respond(&self, request: &wiremock::Request) -> wiremock::ResponseTemplate {
        let body = if high_volume_request_is_initial(request) {
            let request_index = self.tool_request_count.fetch_add(1, Ordering::SeqCst);
            let response_id = format!("{}-tool-{request_index}", self.fixture_id);
            let first_call_id = format!("{response_id}-outer-1");
            let second_call_id = format!("{response_id}-outer-2");
            sse(vec![
                ev_response_created(&response_id),
                ev_custom_tool_call(
                    &first_call_id,
                    "exec",
                    CODE_MODE_HIGH_VOLUME_SINGLE_NESTED_SOURCE,
                ),
                ev_custom_tool_call(
                    &second_call_id,
                    "exec",
                    CODE_MODE_HIGH_VOLUME_DOUBLE_NESTED_SOURCE,
                ),
                ev_completed_with_usage(&response_id, 1_024, 768, 48, 16),
            ])
        } else {
            let request_index = self.follow_up_count.fetch_add(1, Ordering::SeqCst);
            let response_id = format!("{}-follow-up-{request_index}", self.fixture_id);
            sse(vec![
                ev_assistant_message(&response_id, CODE_MODE_HIGH_VOLUME_FOLLOW_UP),
                ev_completed_with_usage(&response_id, 1_280, 1_024, 8, 0),
            ])
        };
        wiremock::ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(body)
    }
}

fn high_volume_request_body_json(request: &wiremock::Request) -> serde_json::Value {
    let is_zstd = request
        .headers
        .get("content-encoding")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .any(|encoding| encoding.trim().eq_ignore_ascii_case("zstd"))
        });
    let body = if is_zstd {
        zstd::stream::decode_all(std::io::Cursor::new(&request.body))
            .unwrap_or_else(|error| panic!("decode high-volume request body: {error}"))
    } else {
        request.body.clone()
    };
    serde_json::from_slice(&body)
        .unwrap_or_else(|error| panic!("high-volume request body must be valid JSON: {error}"))
}

fn high_volume_request_is_initial(request: &wiremock::Request) -> bool {
    let body = high_volume_request_body_json(request);
    high_volume_request_body_is_initial(&body)
}

fn high_volume_request_body_is_initial(body: &serde_json::Value) -> bool {
    let Some(input) = body.get("input").and_then(serde_json::Value::as_array) else {
        return false;
    };
    let Some(prompt_index) = input.iter().rposition(|item| {
        item.get("role").and_then(serde_json::Value::as_str) == Some("user")
            && item.to_string().contains(CODE_MODE_HIGH_VOLUME_PROMPT)
    }) else {
        return false;
    };
    !input[prompt_index + 1..].iter().any(|item| {
        matches!(
            item.get("type").and_then(serde_json::Value::as_str),
            Some("custom_tool_call") | Some("custom_tool_call_output")
        )
    })
}

impl HighVolumeCodeModeFixture {
    async fn start(code_mode_host: &Path, _samples: usize, fixture_id: &str) -> Result<Self> {
        let server = start_mock_server().await;
        let request_capture = HighVolumeRequestCapture::default();
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path_regex(".*/responses$"))
            .and(request_capture.clone())
            .respond_with(HighVolumeResponder {
                fixture_id: fixture_id.to_string(),
                tool_request_count: AtomicUsize::new(0),
                follow_up_count: AtomicUsize::new(0),
            })
            .mount(&server)
            .await;
        let test = test_codex()
            .with_model("test-gpt-5.1-codex")
            .with_code_mode_host_program(code_mode_host.to_path_buf())
            .with_config(|config| {
                pin_ab_reasoning_phase_efforts(config);
                let _ = config.features.enable(Feature::CodeModeOnly);
            })
            .build(&server)
            .await?;
        Ok(Self {
            _server: server,
            test,
            request_capture,
        })
    }

    async fn sample(&self) -> Sample {
        let started = Instant::now();
        let mut aggregate = None;
        for generation in 0..AB_HIGH_VOLUME_SUBTURNS {
            let requests_before = self.request_capture.request_count();
            let completion = submit_high_volume_turn(&self.test).await;
            let turn_requests = self.request_capture.requests_since(requests_before);
            let mut sample = match completion {
                Ok(completion) => {
                    let timing = completion.timing.as_ref();
                    let mut sample = timing.map(sample_from_timing).unwrap_or_default();
                    sample.workload_subturns = 1;
                    match turn_requests.len() {
                        2 => {
                            if completion.error.is_some() {
                                sample
                                    .failure_codes
                                    .push("unexpected_follow_up_error".to_string());
                            }
                            if completion.last_agent_message.as_deref()
                                != Some(CODE_MODE_HIGH_VOLUME_FOLLOW_UP)
                            {
                                sample
                                    .failure_codes
                                    .push("wrong_follow_up_message".to_string());
                            }
                        }
                        _ => sample.failure_codes.push("request_count".to_string()),
                    }
                    let request_count = turn_requests.len().min(u32::MAX as usize) as u32;
                    if request_count != 2
                        || sample.logical_generations != request_count
                        || sample.provider_attempts != request_count
                        || sample.sampling_requests != request_count
                    {
                        sample.failure_codes.push("generation_count".to_string());
                    }
                    if sample.tool_calls != 5
                        || sample.direct_tool_calls != 2
                        || sample.nested_tool_calls != 3
                        || sample.paired_tool_calls != 5
                    {
                        sample.failure_codes.push("tool_graph_counts".to_string());
                    }
                    if !timing.is_some_and(timing_reconciles) {
                        sample
                            .failure_codes
                            .push("timing_reconciliation".to_string());
                    }
                    sample.serialized_bytes = turn_requests
                        .first()
                        .map(|request| request.body.len() as u64)
                        .unwrap_or_default();
                    sample.cache_hits = sample.workspace_evidence_cache_hits;
                    sample.exec_description_tokens = turn_requests
                        .first()
                        .map(high_volume_request_body_json)
                        .as_ref()
                        .map(exec_description_tokens_from_body)
                        .unwrap_or_default();
                    sample.prompt_input_tokens = turn_requests
                        .iter()
                        .map(high_volume_request_body_json)
                        .map(|body| canonical_prompt_input_tokens_from_body(&body))
                        .sum();
                    sample
                }
                Err(error) => Sample {
                    workload_subturns: 1,
                    failed: true,
                    failure_codes: vec![format!("completion_error:{error}")],
                    ..Sample::default()
                },
            };
            for call in &mut sample.tool_call_graph {
                call.workload_generation_index = Some(generation as u32);
            }
            sample.failed = !sample.failure_codes.is_empty();
            merge_high_volume_sample(&mut aggregate, sample);
        }
        let duration_ns = started.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        let mut sample = aggregate.unwrap_or_default();
        sample.duration_ns = duration_ns;
        if let Err(error) = self.rollback_sample_subturns().await {
            sample
                .failure_codes
                .push(format!("sample_history_rollback:{error}"));
        }
        if sample.workload_subturns != AB_HIGH_VOLUME_SUBTURNS as u32 {
            sample
                .failure_codes
                .push("subturn_aggregation_shape".to_string());
        }
        if !tool_graph_matches_workload(&sample, AbWorkload::CodeModeHighVolume) {
            sample.failure_codes.push("tool_graph_identity".to_string());
        }
        sample.failed = !sample.failure_codes.is_empty();
        sample
    }

    async fn rollback_sample_subturns(&self) -> Result<()> {
        self.test
            .codex
            .submit(Op::ThreadRollback {
                num_turns: AB_HIGH_VOLUME_SUBTURNS as u32,
            })
            .await?;
        loop {
            let event = tokio::time::timeout(Duration::from_secs(30), self.test.codex.next_event())
                .await
                .context("timed out rolling back high-volume benchmark subturns")??;
            if matches!(event.msg, EventMsg::ThreadRolledBack(_)) {
                return Ok(());
            }
        }
    }
}

async fn submit_high_volume_turn(
    test: &TestCodex,
) -> Result<codex_protocol::protocol::TurnCompleteEvent> {
    let (sandbox_policy, permission_profile) =
        turn_permission_fields(PermissionProfile::Disabled, test.config.cwd.as_path());
    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: CODE_MODE_HIGH_VOLUME_PROMPT.into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: codex_protocol::protocol::ThreadSettingsOverrides {
                approval_policy: Some(AskForApproval::Never),
                sandbox_policy: Some(sandbox_policy),
                permission_profile,
                collaboration_mode: Some(codex_protocol::config_types::CollaborationMode {
                    mode: codex_protocol::config_types::ModeKind::Default,
                    settings: codex_protocol::config_types::Settings {
                        model: test.session_configured.model.clone(),
                        reasoning_effort: None,
                        developer_instructions: None,
                    },
                }),
                ..Default::default()
            },
        })
        .await?;

    let turn_id = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let event = test.codex.next_event().await?;
            match event.msg {
                EventMsg::TurnStarted(event) => return Ok(event.turn_id),
                EventMsg::Error(event) => {
                    anyhow::bail!("high-volume turn failed before start: {}", event.message)
                }
                _ => {}
            }
        }
    })
    .await
    .context("timed out waiting for high-volume benchmark turn start")??;

    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let event = test.codex.next_event().await?;
            match event.msg {
                EventMsg::TurnComplete(event) if event.turn_id == turn_id => return Ok(event),
                EventMsg::Error(event) => {
                    anyhow::bail!(
                        "high-volume turn failed before completion: {}",
                        event.message
                    )
                }
                _ => {}
            }
        }
    })
    .await
    .context("timed out waiting for high-volume benchmark completion")?
}

fn ev_completed_with_usage(
    id: &str,
    input_tokens: u64,
    cached_input_tokens: u64,
    visible_output_tokens: u64,
    reasoning_tokens: u64,
) -> serde_json::Value {
    serde_json::json!({
        "type": "response.completed",
        "response": {
            "id": id,
            "usage": {
                "input_tokens": input_tokens,
                "input_tokens_details": {"cached_tokens": cached_input_tokens},
                "output_tokens": visible_output_tokens.saturating_add(reasoning_tokens),
                "output_tokens_details": {"reasoning_tokens": reasoning_tokens},
                "total_tokens": input_tokens
                    .saturating_add(visible_output_tokens)
                    .saturating_add(reasoning_tokens)
            }
        }
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AbToolLifecycleRequirement {
    Full,
    TerminalAbort,
}

async fn wait_for_retained_process_cleanup<F, Fut>(mut cleanup_complete: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if cleanup_complete().await {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .is_ok()
}

fn apply_retained_process_cleanup_observation(sample: &mut Sample, cleanup_complete: bool) {
    sample.retained_process_cleanup_complete = cleanup_complete;
    if cleanup_complete {
        // The terminal timing snapshot is intentionally frozen before supervised
        // process cleanup finishes. A later authoritative empty-terminal poll
        // supersedes that snapshot's transient live-process observation.
        sample.unexpected_live_processes = 0;
    }
}

fn sample_from_timing(timing: &TurnTiming) -> Sample {
    sample_from_timing_with_lifecycle(timing, AbToolLifecycleRequirement::Full)
}

fn sample_from_terminal_abort_timing(timing: &TurnTiming) -> Sample {
    sample_from_timing_with_lifecycle(timing, AbToolLifecycleRequirement::TerminalAbort)
}

fn sample_from_timing_with_lifecycle(
    timing: &TurnTiming,
    lifecycle_requirement: AbToolLifecycleRequirement,
) -> Sample {
    let counters = &timing.counters;
    let mut sample = Sample {
        inclusive_duration_ns: timing.inclusive_duration_ns,
        machine_duration_ns: timing.machine_duration_ns,
        controllable_duration_ns: timing
            .exclusive
            .orchestration_ns
            .saturating_add(timing.exclusive.standalone_work_ns)
            .saturating_add(timing.exclusive.finalization_ns),
        model_wait_ns: timing.unions.model_stream_wait_union_ns,
        model_request_wait_ns: timing.unions.model_request_wait_union_ns,
        model_stream_processing_ns: timing.unions.model_stream_processing_union_ns,
        tool_active_ns: timing.unions.tool_active_union_ns,
        orchestration_ns: timing.exclusive.orchestration_ns,
        standalone_work_ns: timing.exclusive.standalone_work_ns,
        finalization_ns: timing.exclusive.finalization_ns,
        preparation_ns: timing.local.preparation_union_ns,
        planning_ns: timing.local.planning_union_ns,
        router_build_ns: timing.local.router_build_union_ns,
        persistence_union_ns: timing.local.persistence_union_ns,
        startup_prewarm_wait_ns: timing.local.startup_prewarm_wait_union_ns,
        pre_first_output_ns: timing
            .pre_first_model_output
            .as_ref()
            .map(|profile| profile.captured_at_ns)
            .unwrap_or_default(),
        first_request_dispatch_ready_ns: timing
            .pre_first_model_output
            .as_ref()
            .map(|profile| profile.first_request_dispatch_ready_ns)
            .unwrap_or_default(),
        pre_first_client_critical_path_ns: timing
            .pre_first_model_output
            .as_ref()
            .map(|profile| profile.client_critical_path_ns)
            .unwrap_or_default(),
        pre_first_attributed_client_union_ns: timing
            .pre_first_model_output
            .as_ref()
            .map(|profile| profile.attributed_client_union_ns)
            .unwrap_or_default(),
        pre_first_unattributed_ns: timing
            .pre_first_model_output
            .as_ref()
            .map(|profile| profile.unattributed_pre_output_ns)
            .unwrap_or_default(),
        history_snapshot_ns: timing
            .pre_first_model_output
            .as_ref()
            .map(|profile| profile.history_snapshot_ns)
            .unwrap_or_default(),
        normalization_ns: timing
            .pre_first_model_output
            .as_ref()
            .map(|profile| profile.normalization_ns)
            .unwrap_or_default(),
        prompt_construction_ns: timing
            .pre_first_model_output
            .as_ref()
            .map(|profile| profile.prompt_construction_ns)
            .unwrap_or_default(),
        request_transformation_ns: timing
            .pre_first_model_output
            .as_ref()
            .map(|profile| profile.request_transformation_ns)
            .unwrap_or_default(),
        serialization_ns: timing
            .pre_first_model_output
            .as_ref()
            .map(|profile| profile.serialization_ns)
            .unwrap_or_default(),
        transport_readiness_ns: timing
            .pre_first_model_output
            .as_ref()
            .map(|profile| profile.transport_readiness_ns)
            .unwrap_or_default(),
        logical_generations: counters.logical_generation_count,
        provider_attempts: counters.model_request_count,
        retry_attempts: counters.attempts_by_kind.retry,
        fallback_attempts: counters.attempts_by_kind.fallback,
        avoidable_generations: counters
            .generations_by_purpose
            .wait
            .saturating_add(counters.generations_by_purpose.failure_diagnosis)
            .saturating_add(counters.generations_by_purpose.repair)
            .saturating_add(counters.generations_by_reason.compaction),
        repeated_waits: counters.exact_repeated_wait_count,
        tool_router_reuse_count: counters.tool_router_reuse_count,
        tool_router_rebuild_count: counters.tool_router_rebuild_count,
        output_truncation_count: counters.tool_output_truncation_count,
        output_projected_token_count: counters.tool_output_projected_token_count,
        output_canonical_byte_count: counters.tool_output_canonical_byte_count,
        output_canonical_token_count: counters.tool_output_canonical_token_count,
        output_model_byte_count: counters.tool_output_model_byte_count,
        output_model_token_count: counters.tool_output_model_token_count,
        output_artifact_creation_count: counters.tool_output_artifact_creation_count,
        output_artifact_reuse_count: counters.tool_output_artifact_reuse_count,
        output_artifact_reread_count: counters.tool_output_artifact_reread_count,
        output_projection_truncation_count: counters.tool_output_projection_truncation_count,
        output_omitted_section_count: counters.tool_output_omitted_section_count,
        output_recovery_count: counters.tool_output_recovery_call_count,
        output_recovery_retruncation_count: counters.tool_output_recovery_retruncation_count,
        output_recursive_spill_count: counters.tool_output_recursive_spill_count,
        timing_overflow_count: timing
            .tool_call_timing_overflow
            .saturating_add(timing.deterministic_continuation_receipt_overflow),
        timing_anomaly_count: counters
            .invalid_transition_count
            .saturating_add(counters.clock_regression_count)
            .saturating_add(counters.saturation_count),
        unclassified_ns: timing
            .exclusive
            .unclassified_ns
            .saturating_add(timing.terminalization.unclassified_ns),
        timing_profile_valid: timing.profile_valid,
        classification_complete: timing.classification_complete,
        sampling_requests: counters.model_request_count,
        tool_calls: counters.tool_call_count,
        nonprogress_tokens: timing.observational_nonprogress_tokens.total_tokens,
        nonprogress_latency_ns: timing
            .observational_nonprogress_latency
            .model_stream_wait_ns
            .saturating_add(timing.observational_nonprogress_latency.decision_latency_ns),
        ..Sample::default()
    };

    let mut seen_calls = BTreeSet::new();
    let direct_ids = timing
        .tool_calls
        .iter()
        .filter(|call| call.source == TurnTimingToolCallSource::Direct)
        .map(|call| call.call_id.as_str())
        .collect::<BTreeSet<_>>();
    for call in &timing.tool_calls {
        if call.source == TurnTimingToolCallSource::Direct {
            sample.direct_tool_calls = sample.direct_tool_calls.saturating_add(1);
        } else {
            sample.nested_tool_calls = sample.nested_tool_calls.saturating_add(1);
        }
        if call.outcome.is_some() {
            sample.paired_tool_calls = sample.paired_tool_calls.saturating_add(1);
        } else {
            sample.unresolved_tool_calls = sample.unresolved_tool_calls.saturating_add(1);
        }
        if !seen_calls.insert(call.call_id.as_str())
            || (call.source == TurnTimingToolCallSource::Direct && call.parent_call_id.is_some())
            || (call.source == TurnTimingToolCallSource::CodeMode
                && !call
                    .parent_call_id
                    .as_deref()
                    .is_some_and(|parent| direct_ids.contains(parent)))
        {
            sample.orphan_tool_calls = sample.orphan_tool_calls.saturating_add(1);
        }
        if let Some(diagnostic) =
            tool_call_lifecycle_diagnostic_for_requirement(call, lifecycle_requirement)
        {
            sample.incomplete_lifecycle_calls = sample.incomplete_lifecycle_calls.saturating_add(1);
            sample.incomplete_tool_lifecycles.push(diagnostic);
        }
        if call.background_process_expected && call.running_process_after_cleanup {
            sample.expected_retained_processes =
                sample.expected_retained_processes.saturating_add(1);
        }
        if (call.process_alive_at_delivery || call.running_process_after_cleanup)
            && !call.background_process_expected
        {
            sample.unexpected_live_processes = sample.unexpected_live_processes.saturating_add(1);
        }
        if call.output_projection_ms.is_some() {
            sample.output_projection_count = sample.output_projection_count.saturating_add(1);
        }
        let parallel_gate_wait_ns = call
            .parallel_gate_wait_ms
            .unwrap_or_default()
            .saturating_mul(1_000_000);
        let parallel_gate_waiter_depth_max = call
            .lifecycle_events
            .iter()
            .map(|event| event.context.parallel_gate_waiter_count)
            .max()
            .unwrap_or_default();
        sample.parallel_gate_wait_ns = sample
            .parallel_gate_wait_ns
            .saturating_add(parallel_gate_wait_ns);
        sample.parallel_gate_wait_max_ns =
            sample.parallel_gate_wait_max_ns.max(parallel_gate_wait_ns);
        sample.parallel_gate_waiter_depth_max = sample
            .parallel_gate_waiter_depth_max
            .max(parallel_gate_waiter_depth_max);
        if parallel_gate_wait_ns > 0 {
            sample.convoy_count = sample.convoy_count.saturating_add(1);
        }
        if call.tool_name == "test_sync_tool" && parallel_gate_wait_ns > 0 {
            sample.unrelated_parallel_safe_convoy_count = sample
                .unrelated_parallel_safe_convoy_count
                .saturating_add(1);
        }
        sample.tool_gate_calls.push(AbToolGateCallCompat {
            call_id: call.call_id.clone(),
            tool_name: call.tool_name.clone(),
            outcome: call.outcome.clone(),
            parallel_gate_wait_ns,
            parallel_gate_waiter_depth_max,
            handler_entry_at_ms: call.handler_entry_at_ms,
            handler_exit_at_ms: call.handler_exit_at_ms,
        });
        sample.workspace_evidence_before_ns = sample.workspace_evidence_before_ns.saturating_add(
            call.workspace_evidence_before_ms
                .unwrap_or_default()
                .saturating_mul(1_000_000),
        );
        sample.workspace_evidence_after_ns = sample.workspace_evidence_after_ns.saturating_add(
            call.workspace_evidence_after_ms
                .unwrap_or_default()
                .saturating_mul(1_000_000),
        );
        if call.workspace_evidence_before_cache_hit == Some(true) {
            sample.workspace_evidence_cache_hits =
                sample.workspace_evidence_cache_hits.saturating_add(1);
        } else if call.workspace_evidence_before_ms.is_some() {
            sample.workspace_evidence_fresh_captures =
                sample.workspace_evidence_fresh_captures.saturating_add(1);
        }
        sample.workspace_evidence_timeouts = sample.workspace_evidence_timeouts.saturating_add(
            call.workspace_evidence_before_timed_out_git_dependencies
                .len()
                .min(u32::MAX as usize) as u32,
        );
    }

    for (index, request) in timing.model_requests.iter().enumerate() {
        sample.max_concurrent_tool_calls = sample
            .max_concurrent_tool_calls
            .max(request.executor_max_concurrent_tool_calls);
        if request.model_emitted_tool_call_count > 0 {
            sample.sampling_to_call_ns = sample
                .sampling_to_call_ns
                .max(request.decision_latency_ns.unwrap_or_default());
        }
        if let Some(usage) = request.token_usage.as_ref() {
            sample.token_usage_records = sample.token_usage_records.saturating_add(1);
            sample.provider_input_tokens = sample
                .provider_input_tokens
                .saturating_add(usage.input_tokens);
            sample.provider_cached_input_tokens = sample
                .provider_cached_input_tokens
                .saturating_add(usage.cached_input_tokens);
            sample.provider_visible_output_tokens = sample
                .provider_visible_output_tokens
                .saturating_add(usage.visible_output_tokens);
            sample.provider_reasoning_tokens = sample
                .provider_reasoning_tokens
                .saturating_add(usage.reasoning_tokens);
            sample.provider_total_tokens = sample
                .provider_total_tokens
                .saturating_add(usage.total_tokens);
            if index > 0 {
                sample.between_tools_peak_input_tokens = sample
                    .between_tools_peak_input_tokens
                    .max(usage.input_tokens);
            }
        } else {
            sample.missing_token_usage_records =
                sample.missing_token_usage_records.saturating_add(1);
        }
        if let Some(categories) = request.request_token_categories.as_ref() {
            sample.prompt_instruction_tokens = sample
                .prompt_instruction_tokens
                .saturating_add(categories.base_instructions);
            sample.prompt_schema_tokens = sample
                .prompt_schema_tokens
                .saturating_add(categories.tool_schemas);
            sample.prompt_history_tokens = sample
                .prompt_history_tokens
                .saturating_add(categories.conversation_history);
            sample.prompt_current_input_tokens = sample
                .prompt_current_input_tokens
                .saturating_add(categories.current_input);
            sample.prompt_repository_tokens = sample
                .prompt_repository_tokens
                .saturating_add(categories.repository_context);
            sample.prompt_skill_tokens =
                sample.prompt_skill_tokens.saturating_add(categories.skills);
            sample.prompt_injected_tokens = sample
                .prompt_injected_tokens
                .saturating_add(categories.other_injected_context);
            sample.prompt_reconciliation_residual =
                sample.prompt_reconciliation_residual.saturating_add(
                    categories
                        .provider_reconciliation_residual
                        .unwrap_or_default(),
                );
            sample.repeated_unchanged_context_tokens = sample
                .repeated_unchanged_context_tokens
                .saturating_add(categories.repeated_unchanged_context);
        }
    }
    match decode_tool_closure_compat(timing) {
        Ok(tool_closure) => sample.tool_closure = tool_closure,
        Err(error) => sample
            .failure_codes
            .push(format!("tool_closure_malformed:{error}")),
    }
    match decode_tool_graph_compat(timing) {
        Ok(tool_call_graph) => sample.tool_call_graph = tool_call_graph,
        Err(error) => sample
            .failure_codes
            .push(format!("tool_graph_malformed:{error}")),
    }
    sample.post_tool_handoff_ns = max_ready_to_sample_to_dispatch_ns(timing).unwrap_or_default();
    sample.token_coverage_complete = !timing.model_requests.is_empty()
        && sample.missing_token_usage_records == 0
        && timing
            .model_requests
            .iter()
            .all(|request| request.request_token_categories.is_some());
    sample.decision_coverage_complete = timing.model_requests.iter().all(|request| {
        request.model_emitted_tool_call_count == 0 || request.decision_latency_ns.is_some()
    });
    sample.lifecycle_complete = sample.unresolved_tool_calls == 0
        && sample.orphan_tool_calls == 0
        && sample.incomplete_lifecycle_calls == 0
        && sample.timing_overflow_count == 0
        && sample
            .tool_closure
            .as_ref()
            .is_none_or(|closure| tool_closure_matches_sample(&sample, closure));
    sample.latency_eligible = sample.timing_profile_valid
        && sample.classification_complete
        && sample.lifecycle_complete
        && sample.token_coverage_complete
        && sample.decision_coverage_complete
        && sample.timing_anomaly_count == 0
        && sample.unclassified_ns == 0
        && sample.controllable_duration_ns > 0;
    sample
}

fn decode_tool_closure_compat(timing: &TurnTiming) -> Result<Option<AbToolClosureCompat>> {
    decode_tool_closure_value(&serde_json::to_value(timing)?)
}

fn decode_tool_closure_value(value: &serde_json::Value) -> Result<Option<AbToolClosureCompat>> {
    let Some(closure) = value.get("toolClosure") else {
        return Ok(None);
    };
    serde_json::from_value(closure.clone())
        .context("decode exact tool-closure ledger")
        .map(Some)
}

fn decode_tool_graph_compat(timing: &TurnTiming) -> Result<Vec<AbToolGraphCallCompat>> {
    decode_tool_graph_value(&serde_json::to_value(timing)?)
}

fn decode_tool_graph_value(value: &serde_json::Value) -> Result<Vec<AbToolGraphCallCompat>> {
    let Some(calls) = value.get("toolCalls") else {
        return Ok(Vec::new());
    };
    let calls = calls
        .as_array()
        .context("toolCalls must be an array when present")?;
    calls
        .iter()
        .cloned()
        .map(|call| serde_json::from_value(call).context("decode tool-call graph identity"))
        .collect()
}

fn merge_tool_closure(
    aggregate: Option<AbToolClosureCompat>,
    next: Option<AbToolClosureCompat>,
) -> Option<AbToolClosureCompat> {
    let (Some(mut aggregate), Some(mut next)) = (aggregate, next) else {
        return None;
    };
    macro_rules! add_fields {
        ($($field:ident),+ $(,)?) => {
            $(aggregate.$field = aggregate.$field.saturating_add(next.$field);)+
        };
    }
    add_fields!(
        accepted_count,
        timing_paired_count,
        terminal_count,
        persisted_count,
        duplicate_call_id_count,
        duplicate_acceptance_count,
        duplicate_timing_count,
        duplicate_persistence_count,
        orphan_timing_count,
        orphan_persistence_count,
        overflow_count,
    );
    aggregate
        .unresolved_calls
        .append(&mut next.unresolved_calls);
    aggregate.orphan_calls.append(&mut next.orphan_calls);
    aggregate.complete &= next.complete;
    Some(aggregate)
}

fn merge_high_volume_sample(aggregate: &mut Option<Sample>, mut next: Sample) {
    let Some(aggregate) = aggregate.as_mut() else {
        *aggregate = Some(next);
        return;
    };
    macro_rules! add_fields {
        ($($field:ident),+ $(,)?) => {
            $(aggregate.$field = aggregate.$field.saturating_add(next.$field);)+
        };
    }
    add_fields!(
        inclusive_duration_ns,
        machine_duration_ns,
        controllable_duration_ns,
        model_wait_ns,
        tool_active_ns,
        orchestration_ns,
        standalone_work_ns,
        finalization_ns,
        preparation_ns,
        persistence_union_ns,
        pre_first_output_ns,
        sampling_to_call_ns,
        post_tool_handoff_ns,
        parallel_gate_wait_ns,
        convoy_count,
        unrelated_parallel_safe_convoy_count,
        workspace_evidence_before_ns,
        workspace_evidence_after_ns,
        workspace_evidence_cache_hits,
        workspace_evidence_fresh_captures,
        workspace_evidence_timeouts,
        logical_generations,
        provider_attempts,
        retry_attempts,
        fallback_attempts,
        avoidable_generations,
        provider_input_tokens,
        provider_cached_input_tokens,
        provider_visible_output_tokens,
        provider_reasoning_tokens,
        provider_total_tokens,
        token_usage_records,
        missing_token_usage_records,
        prompt_instruction_tokens,
        prompt_schema_tokens,
        prompt_history_tokens,
        prompt_current_input_tokens,
        prompt_repository_tokens,
        prompt_skill_tokens,
        prompt_injected_tokens,
        prompt_reconciliation_residual,
        repeated_unchanged_context_tokens,
        nonprogress_tokens,
        nonprogress_latency_ns,
        repeated_waits,
        tool_router_reuse_count,
        tool_router_rebuild_count,
        direct_tool_calls,
        nested_tool_calls,
        paired_tool_calls,
        unresolved_tool_calls,
        orphan_tool_calls,
        workload_subturns,
        failure_terminalized_subturns,
        typed_error_count,
        abort_model_resumed_call_count,
        retained_write_stdin_poll_count,
        retained_process_count_before_abort,
        retained_abort_persisted_result_count,
        incomplete_lifecycle_calls,
        unexpected_live_processes,
        expected_retained_processes,
        output_projection_count,
        output_truncation_count,
        output_projected_token_count,
        output_canonical_byte_count,
        output_canonical_token_count,
        output_model_byte_count,
        output_model_token_count,
        output_artifact_creation_count,
        output_artifact_reuse_count,
        output_artifact_reread_count,
        output_projection_truncation_count,
        output_omitted_section_count,
        output_recovery_count,
        output_recovery_retruncation_count,
        output_recursive_spill_count,
        timing_overflow_count,
        timing_anomaly_count,
        unclassified_ns,
        sampling_requests,
        serialized_bytes,
        cache_hits,
        exec_description_tokens,
        prompt_input_tokens,
        tool_calls,
    );
    aggregate.parallel_gate_wait_max_ns = aggregate
        .parallel_gate_wait_max_ns
        .max(next.parallel_gate_wait_max_ns);
    aggregate.parallel_gate_waiter_depth_max = aggregate
        .parallel_gate_waiter_depth_max
        .max(next.parallel_gate_waiter_depth_max);
    aggregate.max_concurrent_tool_calls = aggregate
        .max_concurrent_tool_calls
        .max(next.max_concurrent_tool_calls);
    aggregate.between_tools_peak_input_tokens = aggregate
        .between_tools_peak_input_tokens
        .max(next.between_tools_peak_input_tokens);
    aggregate.max_ready_to_sample_to_dispatch_ns = match (
        aggregate.max_ready_to_sample_to_dispatch_ns,
        next.max_ready_to_sample_to_dispatch_ns,
    ) {
        (Some(aggregate), Some(next)) => Some(aggregate.max(next)),
        (aggregate, next) => aggregate.or(next),
    };
    aggregate.timing_profile_valid &= next.timing_profile_valid;
    aggregate.classification_complete &= next.classification_complete;
    aggregate.lifecycle_complete &= next.lifecycle_complete;
    aggregate.token_coverage_complete &= next.token_coverage_complete;
    aggregate.decision_coverage_complete &= next.decision_coverage_complete;
    aggregate.latency_eligible &= next.latency_eligible;
    aggregate.failed |= next.failed;
    aggregate.final_response_present |= next.final_response_present;
    aggregate.forged_turn_complete_observed |= next.forged_turn_complete_observed;
    aggregate.retained_process_exit_observed |= next.retained_process_exit_observed;
    aggregate.retained_process_cleanup_complete |= next.retained_process_cleanup_complete;
    aggregate.retained_process_owned_before_abort |= next.retained_process_owned_before_abort;
    aggregate.retained_abort_cancellation_observed |= next.retained_abort_cancellation_observed;
    aggregate
        .retained_session_ids
        .append(&mut next.retained_session_ids);
    aggregate
        .abort_registered_call_ids
        .append(&mut next.abort_registered_call_ids);
    aggregate
        .abort_terminal_outcomes_by_registration
        .append(&mut next.abort_terminal_outcomes_by_registration);
    aggregate.abort_barrier_call_id = aggregate
        .abort_barrier_call_id
        .take()
        .or(next.abort_barrier_call_id.take());
    aggregate.retained_abort_process_id = aggregate
        .retained_abort_process_id
        .take()
        .or(next.retained_abort_process_id.take());
    aggregate.tool_call_graph.append(&mut next.tool_call_graph);
    aggregate.tool_gate_calls.append(&mut next.tool_gate_calls);
    aggregate
        .incomplete_tool_lifecycles
        .append(&mut next.incomplete_tool_lifecycles);
    aggregate.failure_codes.append(&mut next.failure_codes);
    aggregate.tool_closure =
        merge_tool_closure(aggregate.tool_closure.take(), next.tool_closure.take());
}

fn tool_graph_matches_workload(sample: &Sample, workload: AbWorkload) -> bool {
    match workload {
        AbWorkload::CodeModeNestedDispatch => return true,
        AbWorkload::LongHistoryNoToolInitial
        | AbWorkload::StableContextWarmCache
        | AbWorkload::ContextChangeInvalidation => {
            return sample.tool_call_graph.is_empty();
        }
        AbWorkload::LongHistoryToolContinuation => {
            return sample.tool_call_graph.len() == 1
                && sample.tool_call_graph[0].source.as_deref() == Some("direct")
                && sample.tool_call_graph[0].parent_call_id.is_none()
                && !sample.tool_call_graph[0].call_id.is_empty()
                && !sample.tool_call_graph[0].execution_id.is_empty()
                && sample.tool_call_graph[0]
                    .sampling_generation_id
                    .as_deref()
                    .is_some_and(|generation| !generation.is_empty());
        }
        AbWorkload::SingleDirectToolCall => {
            return direct_tool_graph_matches(sample, "test_sync_tool", 1);
        }
        AbWorkload::ParallelSafeTripleDirect => {
            return direct_tool_graph_matches(sample, "test_sync_tool", 3);
        }
        AbWorkload::ExclusiveGateSerialization => {
            return direct_tool_graph_multiset_matches(
                sample,
                &["exec_command", "exec_command", "test_sync_tool"],
            );
        }
        AbWorkload::RetainedExecWriteStdinLifecycle => {
            if sample.workload_subturns != 1 || sample.tool_call_graph.len() != 3 {
                return false;
            }
            let expected_tools = ["exec_command", "write_stdin", "write_stdin"];
            let mut call_ids = BTreeSet::new();
            let mut execution_ids = BTreeSet::new();
            let mut generation_ids = BTreeSet::new();
            return sample.tool_call_graph.iter().zip(expected_tools).all(
                |(call, expected_tool)| {
                    call.tool_name == expected_tool
                        && call.source.as_deref() == Some("direct")
                        && call.parent_call_id.is_none()
                        && !call.call_id.is_empty()
                        && !call.execution_id.is_empty()
                        && call_ids.insert(call.call_id.as_str())
                        && execution_ids.insert(call.execution_id.as_str())
                        && call
                            .sampling_generation_id
                            .as_deref()
                            .is_some_and(|generation| {
                                !generation.is_empty() && generation_ids.insert(generation)
                            })
                },
            );
        }
        AbWorkload::AbortDirectNestedInFlight => {
            if sample.workload_subturns != 1 || sample.tool_call_graph.len() != 2 {
                return false;
            }
            let Some(direct) = sample
                .tool_call_graph
                .iter()
                .find(|call| call.source.as_deref() == Some("direct"))
            else {
                return false;
            };
            let Some(nested) = sample
                .tool_call_graph
                .iter()
                .find(|call| call.source.as_deref() == Some("code_mode"))
            else {
                return false;
            };
            return direct.tool_name == "exec"
                && nested.tool_name == "request_permissions"
                && direct.parent_call_id.is_none()
                && nested.parent_call_id.as_deref() == Some(direct.call_id.as_str())
                && !direct.call_id.is_empty()
                && !nested.call_id.is_empty()
                && direct.call_id != nested.call_id
                && !direct.execution_id.is_empty()
                && !nested.execution_id.is_empty()
                && direct.execution_id != nested.execution_id
                && direct
                    .sampling_generation_id
                    .as_deref()
                    .is_some_and(|generation| {
                        !generation.is_empty()
                            && nested.sampling_generation_id.as_deref() == Some(generation)
                    });
        }
        AbWorkload::AbortRetainedProcess => {
            let Some(call) = sample.tool_call_graph.first() else {
                return false;
            };
            return sample.workload_subturns == 1
                && sample.tool_call_graph.len() == 1
                && call.tool_name == "exec_command"
                && call.source.as_deref() == Some("direct")
                && call.parent_call_id.is_none()
                && !call.call_id.is_empty()
                && !call.execution_id.is_empty()
                && call
                    .sampling_generation_id
                    .as_deref()
                    .is_some_and(|generation| !generation.is_empty());
        }
        AbWorkload::SessionReplay => {
            let targeted = sample
                .replay_targeted_action
                .as_ref()
                .is_some_and(|evidence| evidence.targeted);
            let (expected_direct, expected_nested, expected_total) =
                if targeted { (10, 6, 16) } else { (19, 16, 35) };
            let subturn_closure_matches = sample.replay_subturns.len() == 3
                && sample.replay_subturns[0].closure_complete
                && sample.replay_subturns[2].closure_complete
                && sample.replay_subturns[1].closure_complete == targeted;
            if sample.workload_subturns != 3
                || sample.replay_subturns.len() != 3
                || sample.direct_tool_calls != expected_direct
                || sample.nested_tool_calls != expected_nested
                || sample.tool_call_graph.len() != expected_total
                || !subturn_closure_matches
            {
                return false;
            }
            let direct_ids = sample
                .tool_call_graph
                .iter()
                .filter(|call| call.source.as_deref() == Some("direct"))
                .map(|call| call.call_id.as_str())
                .collect::<BTreeSet<_>>();
            let mut call_ids = BTreeSet::new();
            let mut execution_ids = BTreeSet::new();
            let abort_nested_is_parented = sample.tool_call_graph.iter().any(|nested| {
                nested.tool_name == "request_permissions"
                    && nested.source.as_deref() == Some("code_mode")
                    && nested
                        .parent_call_id
                        .as_deref()
                        .is_some_and(|parent| direct_ids.contains(parent))
            });
            return direct_ids.len() == expected_direct as usize
                && abort_nested_is_parented
                && sample.tool_call_graph.iter().all(|call| {
                    !call.call_id.is_empty()
                        && !call.execution_id.is_empty()
                        && call_ids.insert(call.call_id.as_str())
                        && execution_ids.insert(call.execution_id.as_str())
                        && call
                            .sampling_generation_id
                            .as_deref()
                            .is_some_and(|generation| !generation.is_empty())
                        && match call.source.as_deref() {
                            Some("direct") => call.parent_call_id.is_none(),
                            Some("code_mode") => call
                                .parent_call_id
                                .as_deref()
                                .is_some_and(|parent| direct_ids.contains(parent)),
                            _ => false,
                        }
                });
        }
        AbWorkload::CodeModeHighVolume => {}
    }
    let expected_calls = workload
        .expected_direct_tool_calls()
        .saturating_add(workload.expected_nested_tool_calls()) as usize;
    if sample.workload_subturns != AB_HIGH_VOLUME_SUBTURNS as u32
        || sample.tool_call_graph.len() != expected_calls
    {
        return false;
    }

    let mut call_ids = BTreeSet::new();
    let mut execution_ids = BTreeSet::new();
    let mut generations = BTreeMap::<u32, Vec<&AbToolGraphCallCompat>>::new();
    for call in &sample.tool_call_graph {
        let Some(generation) = call.workload_generation_index else {
            return false;
        };
        if generation >= AB_HIGH_VOLUME_SUBTURNS as u32
            || call.call_id.is_empty()
            || call.execution_id.is_empty()
            || !call_ids.insert(call.call_id.as_str())
            || !execution_ids.insert(call.execution_id.as_str())
            || call
                .sampling_generation_id
                .as_deref()
                .is_none_or(str::is_empty)
        {
            return false;
        }
        generations.entry(generation).or_default().push(call);
    }
    if generations.len() != AB_HIGH_VOLUME_SUBTURNS {
        return false;
    }
    for generation in 0..AB_HIGH_VOLUME_SUBTURNS as u32 {
        let Some(calls) = generations.get(&generation) else {
            return false;
        };
        let generation_ids = calls
            .iter()
            .filter_map(|call| call.sampling_generation_id.as_deref())
            .collect::<BTreeSet<_>>();
        // Sampling-generation identifiers are scoped to one turn. This
        // workload deliberately composes sixteen independent turns into one
        // sample, so `generation-0` may correctly repeat across workload
        // generations. `workload_generation_index` supplies the cross-turn
        // grouping while every call within one turn must retain one runtime
        // generation identity.
        if generation_ids.len() != 1 {
            return false;
        }
        let direct = calls
            .iter()
            .copied()
            .filter(|call| call.source.as_deref() == Some("direct"))
            .collect::<Vec<_>>();
        let nested = calls
            .iter()
            .copied()
            .filter(|call| call.source.as_deref() == Some("code_mode"))
            .collect::<Vec<_>>();
        if direct.len() != AB_HIGH_VOLUME_DIRECT_CALLS_PER_GENERATION
            || nested.len() != AB_HIGH_VOLUME_NESTED_CALLS_PER_GENERATION
            || direct.iter().any(|call| call.parent_call_id.is_some())
        {
            return false;
        }
        let direct_ids = direct
            .iter()
            .map(|call| call.call_id.as_str())
            .collect::<BTreeSet<_>>();
        let mut nested_by_parent = BTreeMap::<&str, usize>::new();
        for call in nested {
            let Some(parent) = call.parent_call_id.as_deref() else {
                return false;
            };
            if !direct_ids.contains(parent) {
                return false;
            }
            *nested_by_parent.entry(parent).or_default() += 1;
        }
        let mut child_counts = direct_ids
            .iter()
            .map(|parent| nested_by_parent.get(*parent).copied().unwrap_or_default())
            .collect::<Vec<_>>();
        child_counts.sort_unstable();
        if child_counts != [1, 2] {
            return false;
        }
    }
    true
}

fn direct_tool_graph_matches(sample: &Sample, expected_tool: &str, expected_calls: usize) -> bool {
    if sample.tool_call_graph.len() != expected_calls {
        return false;
    }
    let expected_tools = vec![expected_tool; expected_calls];
    direct_tool_graph_sequence_matches(sample, &expected_tools)
}

fn direct_tool_graph_sequence_matches(sample: &Sample, expected_tools: &[&str]) -> bool {
    if sample.workload_subturns != 1 || sample.tool_call_graph.len() != expected_tools.len() {
        return false;
    }
    let mut call_ids = BTreeSet::new();
    let mut execution_ids = BTreeSet::new();
    let mut generation_ids = BTreeSet::new();
    sample
        .tool_call_graph
        .iter()
        .zip(expected_tools)
        .all(|(call, expected_tool)| {
            let Some(generation) = call.sampling_generation_id.as_deref() else {
                return false;
            };
            generation_ids.insert(generation);
            call.tool_name == *expected_tool
                && call.source.as_deref() == Some("direct")
                && call.parent_call_id.is_none()
                && call.workload_generation_index.is_none()
                && !call.call_id.is_empty()
                && !call.execution_id.is_empty()
                && !generation.is_empty()
                && call_ids.insert(call.call_id.as_str())
                && execution_ids.insert(call.execution_id.as_str())
        })
        && generation_ids.len() == 1
}

fn direct_tool_graph_multiset_matches(sample: &Sample, expected_tools: &[&str]) -> bool {
    if sample.workload_subturns != 1 || sample.tool_call_graph.len() != expected_tools.len() {
        return false;
    }
    let mut actual_tools = sample
        .tool_call_graph
        .iter()
        .map(|call| call.tool_name.as_str())
        .collect::<Vec<_>>();
    let mut expected_tools = expected_tools.to_vec();
    actual_tools.sort_unstable();
    expected_tools.sort_unstable();
    let mut call_ids = BTreeSet::new();
    let mut execution_ids = BTreeSet::new();
    let generation_ids = sample
        .tool_call_graph
        .iter()
        .filter_map(|call| call.sampling_generation_id.as_deref())
        .collect::<BTreeSet<_>>();
    actual_tools == expected_tools
        && generation_ids.len() == 1
        && generation_ids
            .first()
            .is_some_and(|generation| !generation.is_empty())
        && sample.tool_call_graph.iter().all(|call| {
            call.source.as_deref() == Some("direct")
                && call.parent_call_id.is_none()
                && call.workload_generation_index.is_none()
                && !call.call_id.is_empty()
                && !call.execution_id.is_empty()
                && call_ids.insert(call.call_id.as_str())
                && execution_ids.insert(call.execution_id.as_str())
        })
}

fn tool_gate_execution_matches(sample: &Sample, workload: AbWorkload) -> bool {
    let common = sample.expected_retained_processes == 0
        && sample.unexpected_live_processes == 0
        && sample.incomplete_lifecycle_calls == 0
        && sample.incomplete_tool_lifecycles.is_empty()
        && sample.lifecycle_complete
        && sample.latency_eligible
        && sample.terminal_event == "turn_complete"
        && sample.abort_reason.is_none()
        && sample.typed_error_count == 0
        && sample.final_response_present
        && !sample.forged_turn_complete_observed
        && sample.tool_gate_calls.len() == sample.tool_call_graph.len();
    let diagnostics = sample
        .tool_call_graph
        .iter()
        .map(|graph| {
            sample
                .tool_gate_calls
                .iter()
                .find(|diagnostic| diagnostic.call_id == graph.call_id)
        })
        .collect::<Option<Vec<_>>>();
    common
        && diagnostics.is_some_and(|calls| {
            calls
                .iter()
                .all(|call| call.handler_entry_at_ms.is_some() && call.handler_exit_at_ms.is_some())
                && match workload {
                    AbWorkload::SingleDirectToolCall => {
                        sample.max_concurrent_tool_calls == 1
                            && sample.parallel_gate_waiter_depth_max == 0
                            && sample.convoy_count == 0
                            && sample.unrelated_parallel_safe_convoy_count == 0
                            && sample.parallel_gate_wait_ns == 0
                            && sample.parallel_gate_wait_max_ns == 0
                            && calls.len() == 1
                            && calls[0].tool_name == "test_sync_tool"
                            && calls[0].parallel_gate_wait_ns == 0
                            && calls[0].parallel_gate_waiter_depth_max == 0
                    }
                    AbWorkload::ParallelSafeTripleDirect => {
                        sample.max_concurrent_tool_calls == 3
                            && sample.parallel_gate_waiter_depth_max == 0
                            && sample.convoy_count == 0
                            && sample.unrelated_parallel_safe_convoy_count == 0
                            && sample.parallel_gate_wait_ns == 0
                            && sample.parallel_gate_wait_max_ns == 0
                            && calls.len() == 3
                            && calls.iter().all(|call| {
                                call.tool_name == "test_sync_tool"
                                    && call.parallel_gate_wait_ns == 0
                                    && call.parallel_gate_waiter_depth_max == 0
                            })
                    }
                    AbWorkload::ExclusiveGateSerialization => {
                        let mut exec_calls = calls
                            .iter()
                            .copied()
                            .filter(|call| call.tool_name == "exec_command")
                            .collect::<Vec<_>>();
                        let unrelated_calls = calls
                            .iter()
                            .copied()
                            .filter(|call| call.tool_name == "test_sync_tool")
                            .collect::<Vec<_>>();
                        if exec_calls.len() != 2 || unrelated_calls.len() != 1 {
                            return false;
                        }
                        exec_calls.sort_by_key(|call| call.handler_entry_at_ms);
                        let first_exec = exec_calls[0];
                        let second_exec = exec_calls[1];
                        let unrelated_safe = unrelated_calls[0];
                        let Some(first_exec_entry) = first_exec.handler_entry_at_ms else {
                            return false;
                        };
                        let Some(first_exec_exit) = first_exec.handler_exit_at_ms else {
                            return false;
                        };
                        let Some(second_exec_entry) = second_exec.handler_entry_at_ms else {
                            return false;
                        };
                        let Some(unrelated_entry) = unrelated_safe.handler_entry_at_ms else {
                            return false;
                        };
                        let Some(unrelated_exit) = unrelated_safe.handler_exit_at_ms else {
                            return false;
                        };
                        sample.max_concurrent_tool_calls == 2
                            && sample.parallel_gate_waiter_depth_max == 1
                            && sample.convoy_count == 1
                            && sample.unrelated_parallel_safe_convoy_count == 0
                            && sample.parallel_gate_wait_ns > 0
                            && sample.parallel_gate_wait_ns == sample.parallel_gate_wait_max_ns
                            && first_exec.parallel_gate_wait_ns == 0
                            && second_exec.parallel_gate_wait_ns > 0
                            && unrelated_safe.parallel_gate_wait_ns == 0
                            && first_exec_exit <= second_exec_entry
                            && first_exec_entry <= unrelated_exit
                            && unrelated_entry < first_exec_exit
                    }
                    _ => false,
                }
        })
}

fn tool_closure_matches_sample(sample: &Sample, closure: &AbToolClosureCompat) -> bool {
    let accepted_count = sample
        .direct_tool_calls
        .saturating_add(sample.nested_tool_calls);
    closure.accepted_count == accepted_count
        && closure.timing_paired_count == accepted_count
        && closure.terminal_count == accepted_count
        && closure.persisted_count == accepted_count
        && sample.paired_tool_calls == accepted_count
        && closure.duplicate_call_id_count == 0
        && closure.duplicate_acceptance_count == 0
        && closure.duplicate_timing_count == 0
        && closure.duplicate_persistence_count == 0
        && closure.orphan_timing_count == 0
        && closure.orphan_persistence_count == 0
        && closure.overflow_count == 0
        && closure.unresolved_calls.is_empty()
        && closure.orphan_calls.is_empty()
        && closure.complete
}

#[allow(dead_code)] // Cargo checks the no-harness benchmark with cfg(test).
fn tool_call_lifecycle_diagnostic(call: &TurnTimingToolCall) -> Option<AbIncompleteToolLifecycle> {
    tool_call_lifecycle_diagnostic_for_requirement(call, AbToolLifecycleRequirement::Full)
}

fn tool_call_lifecycle_diagnostic_for_requirement(
    call: &TurnTimingToolCall,
    requirement: AbToolLifecycleRequirement,
) -> Option<AbIncompleteToolLifecycle> {
    let mut missing_boundaries = [
        ("accepted_at_ms", call.accepted_at_ms),
        ("first_poll_at_ms", call.first_poll_at_ms),
        (
            "parallel_gate_admitted_at_ms",
            call.parallel_gate_admitted_at_ms,
        ),
        ("handler_entry_at_ms", call.handler_entry_at_ms),
        ("handler_exit_at_ms", call.handler_exit_at_ms),
        ("output_collected_at_ms", call.output_collected_at_ms),
    ]
    .into_iter()
    .filter(|(name, _)| {
        requirement == AbToolLifecycleRequirement::Full || *name != "handler_exit_at_ms"
    })
    .filter_map(|(name, value)| value.is_none().then_some(name.to_string()))
    .collect::<Vec<_>>();
    // Nested CodeMode results are delivered inside their owning direct call's
    // canonical projection, not through a separate model relay. Their closure
    // ledger entry still must be paired, terminal, and persisted; only direct
    // calls require an independent relay-delivery boundary.
    if call.source == TurnTimingToolCallSource::Direct && call.delivered_at_ms.is_none() {
        missing_boundaries.push("delivered_at_ms".to_string());
    }
    if call.source == TurnTimingToolCallSource::Direct && call.output_model_visible_at_ms.is_none()
    {
        missing_boundaries.push("output_model_visible_at_ms".to_string());
    }
    if call.process_spawned_at_ms.is_some()
        && call.process_exited_at_ms.is_none()
        && !call.background_process_expected
    {
        missing_boundaries.push("process_exited_at_ms".to_string());
    }

    let mut nonmonotonic_boundaries = match requirement {
        AbToolLifecycleRequirement::Full | AbToolLifecycleRequirement::TerminalAbort => {
            let mut regressions = lifecycle_boundary_regressions(&[
                ("accepted_at_ms", call.accepted_at_ms),
                ("first_poll_at_ms", call.first_poll_at_ms),
            ]);
            // Raw A reconstructs first-poll time from separately rounded
            // millisecond values, while admission is an exact lifecycle event.
            // Preserve both causal chains without ordering mixed projections.
            regressions.extend(lifecycle_boundary_regressions(&[
                (
                    "parallel_gate_admitted_at_ms",
                    call.parallel_gate_admitted_at_ms,
                ),
                ("handler_entry_at_ms", call.handler_entry_at_ms),
                ("handler_exit_at_ms", call.handler_exit_at_ms),
            ]));
            // `output_collected_at_ms` is reconstructed from separately
            // millisecond-rounded durations. Comparing it to exact lifecycle
            // event timestamps can report a false regression. The exact
            // handler and relay events below retain the causal-order gate
            // without mixing clock representations.
            regressions.extend(lifecycle_boundary_regressions(&[
                ("first_poll_at_ms", call.first_poll_at_ms),
                ("output_collected_at_ms", call.output_collected_at_ms),
            ]));
            if call.source == TurnTimingToolCallSource::Direct {
                regressions.extend(lifecycle_boundary_regressions(&[
                    ("handler_exit_at_ms", call.handler_exit_at_ms),
                    ("delivered_at_ms", call.delivered_at_ms),
                    (
                        "output_model_visible_at_ms",
                        call.output_model_visible_at_ms,
                    ),
                    ("model_resumed_at_ms", call.model_resumed_at_ms),
                ]));
            }
            regressions.extend(lifecycle_event_regressions(call));
            regressions
        }
    };
    nonmonotonic_boundaries.extend(lifecycle_boundary_regressions(&[
        ("handler_entry_at_ms", call.handler_entry_at_ms),
        ("process_spawned_at_ms", call.process_spawned_at_ms),
        ("process_exited_at_ms", call.process_exited_at_ms),
    ]));
    if call
        .process_spawned_at_ms
        .zip(call.handler_exit_at_ms)
        .is_some_and(|(spawned, handler_exit)| spawned > handler_exit)
    {
        nonmonotonic_boundaries.push("handler_exit_at_ms<process_spawned_at_ms".to_string());
    }
    if !call.background_process_expected
        && call
            .process_exited_at_ms
            .zip(call.handler_exit_at_ms)
            .is_some_and(|(process_exit, handler_exit)| process_exit > handler_exit)
    {
        nonmonotonic_boundaries.push("handler_exit_at_ms<process_exited_at_ms".to_string());
    }
    nonmonotonic_boundaries.sort();
    nonmonotonic_boundaries.dedup();

    (!missing_boundaries.is_empty() || !nonmonotonic_boundaries.is_empty()).then_some(
        AbIncompleteToolLifecycle {
            call_id: call.call_id.clone(),
            tool_name: call.tool_name.clone(),
            missing_boundaries,
            nonmonotonic_boundaries,
        },
    )
}

fn lifecycle_event_regressions(call: &TurnTimingToolCall) -> Vec<String> {
    call.lifecycle_events
        .windows(2)
        .filter_map(|events| {
            let previous = &events[0];
            let current = &events[1];
            (current.at_ms < previous.at_ms).then(|| {
                format!(
                    "lifecycle_event:{:?}<{:?}",
                    current.boundary, previous.boundary
                )
            })
        })
        .collect()
}

fn lifecycle_boundary_regressions(boundaries: &[(&str, Option<u64>)]) -> Vec<String> {
    let mut regressions = Vec::new();
    let mut previous = None;
    for &(name, value) in boundaries {
        let Some(value) = value else {
            continue;
        };
        if let Some((previous_name, previous_value)) = previous
            && value < previous_value
        {
            regressions.push(format!("{name}<{previous_name}"));
        }
        previous = Some((name, value));
    }
    regressions
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
        && timing.tool_calls.len() == timing.counters.tool_call_count as usize
        && timing
            .model_requests
            .iter()
            .map(|request| request.model_emitted_tool_call_count)
            .sum::<u32>()
            == timing
                .tool_calls
                .iter()
                .filter(|call| call.source == TurnTimingToolCallSource::Direct)
                .count()
                .min(u32::MAX as usize) as u32
}

fn exec_description_tokens(request: &ResponsesRequest) -> u64 {
    exec_description_tokens_from_body(&request.body_json())
}

fn exec_description_tokens_from_body(body: &serde_json::Value) -> u64 {
    body.get("tools")
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
    prompt_input_tokens_from_body(&request.body_json())
}

fn prompt_input_tokens_from_body(body: &serde_json::Value) -> u64 {
    let logical_prompt = serde_json::json!({
        "instructions": body.get("instructions"),
        "input": body.get("input"),
        "tools": body.get("tools"),
    });
    codex_utils_output_truncation::approx_token_count(&logical_prompt.to_string()) as u64
}

fn canonical_prompt_input_tokens_from_body(body: &serde_json::Value) -> u64 {
    let mut body = body.clone();
    canonicalize_request_identities(&mut body);
    prompt_input_tokens_from_body(&body)
}
