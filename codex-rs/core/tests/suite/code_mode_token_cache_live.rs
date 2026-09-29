//! Explicitly opted-in live forwarding of the same integrated benchmark fixture.
//! Secrets are read only by this process and sent only to the built-in HTTPS origin.
use super::assert_eq;
use super::*;
use codex_protocol::openai_models::ModelsResponse;
use std::collections::BTreeSet;
use std::path::PathBuf;

const MAX_REQUESTS: usize = 16;
const MAX_INPUT_TOKENS: u64 = 350_000;
const MAX_OUTPUT_TOKENS: u64 = 12_000;

pub(super) struct LiveContext {
    pub model: String,
    pub catalog: ModelsResponse,
    pub effort: String,
    client: codex_http_client::HttpClient,
    token: String,
    account: String,
    output: PathBuf,
    state: Mutex<LiveState>,
    runtime: tokio::runtime::Handle,
}

#[derive(Default)]
struct LiveState {
    attempts: Vec<Value>,
    exercised: BTreeSet<u32>,
    stopped: Option<String>,
}

fn usage_from_sse(text: &str) -> Option<Value> {
    text.lines().filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|data| serde_json::from_str::<Value>(data).ok())
        .filter(|event| event["type"] == "response.completed")
        .find_map(|event| {
            let usage = &event["response"]["usage"];
            let input = usage["input_tokens"].as_u64()?;
            let cached = usage["input_tokens_details"]["cached_tokens"].as_u64()?;
            let output = usage["output_tokens"].as_u64()?;
            if cached > input { return None; }
            Some(json!({"input_tokens":input,"cached_input_tokens":cached,
                "uncached_input_tokens":input-cached,"output_tokens":output,"total_tokens":input+output}))
        })
}

pub(super) fn assert_usage_parser_contract() {
    let control = json!({"model":"fixture","input":[]});
    assert_eq!(project(&control, true), control);
    let only = json!({"input":[{"type":"additional_tools","tools":[{"name":"exec","description":"prefix\n\nEager nested tool contracts: declarations"}]}]});
    assert_eq!(
        project(&only, true),
        only,
        "do not strip the only nested contracts"
    );
    let mut mixed = only.clone();
    mixed["input"][0]["tools"]
        .as_array_mut()
        .unwrap()
        .push(json!({"name":"read_file"}));
    assert!(
        !tool_schemas(&project(&mixed, true))[0]["description"]
            .as_str()
            .unwrap()
            .contains("declarations")
    );
    let event = |usage: Value| {
        format!(
            "data: {}\n\n",
            json!({"type":"response.completed","response":{"usage":usage}})
        )
    };
    assert_eq!(
        usage_from_sse(&event(
            json!({"input_tokens":100,"input_tokens_details":{"cached_tokens":60},"output_tokens":7})
        )),
        Some(
            json!({"input_tokens":100,"cached_input_tokens":60,"uncached_input_tokens":40,"output_tokens":7,"total_tokens":107})
        )
    );
    assert!(usage_from_sse(&event(json!({"input_tokens":100,"output_tokens":7}))).is_none());
    assert!(usage_from_sse(&event(json!({"input_tokens":100,"input_tokens_details":{"cached_tokens":101},"output_tokens":7}))).is_none());
    assert!(usage_from_sse("data: [DONE]\n").is_none());
}

fn append(path: &Path, value: &Value) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{value}")?;
    Ok(())
}

fn bridge<T: Send>(
    runtime: &tokio::runtime::Handle,
    future: impl std::future::Future<Output = T> + Send,
) -> T {
    std::thread::scope(|scope| {
        scope
            .spawn(|| runtime.block_on(future))
            .join()
            .expect("forwarding thread")
    })
}

pub(super) async fn assert_transport_bridge() -> Result<()> {
    let server = MockServer::start().await;
    let runtime = tokio::runtime::Handle::current();
    Mock::given(method("GET"))
        .respond_with(move |_: &wiremock::Request| {
            bridge(&runtime, async {
                tokio::time::sleep(Duration::from_millis(1)).await;
                ResponseTemplate::new(200).set_body_string("bridge-ok")
            })
        })
        .expect(1)
        .mount(&server)
        .await;
    let client = codex_http_client::HttpClientBuilder::new()
        .without_redirects()
        .build_with_transport_default_proxy()?;
    assert_eq!(
        client.get(server.uri()).send().await?.text().await?,
        "bridge-ok"
    );
    Ok(())
}

impl LiveContext {
    async fn load() -> Result<Self> {
        anyhow::ensure!(
            std::env::var("KD4_LIVE_CONFIRM").as_deref() == Ok("1"),
            "live requests require explicit runner opt-in"
        );
        let auth: Value = serde_json::from_slice(&fs::read(std::env::var("KD4_LIVE_AUTH_PATH")?)?)?;
        let token = auth["tokens"]["access_token"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("ChatGPT access token unavailable"))?
            .to_owned();
        let account = auth["tokens"]["account_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("ChatGPT account unavailable"))?
            .to_owned();
        let model = std::env::var("KD4_LIVE_MODEL")?;
        let effort = std::env::var("KD4_LIVE_EFFORT")?;
        let client = codex_http_client::HttpClientBuilder::new()
            .without_redirects()
            .without_request_logging()
            .timeout(Duration::from_secs(90))
            .build_with_transport_default_proxy()?;
        let url = format!(
            "{}/models?client_version=0.0.0",
            codex_model_provider_info::CHATGPT_CODEX_BASE_URL
        );
        let response = client
            .get(url)
            .bearer_auth(&token)
            .header("ChatGPT-Account-Id", &account)
            .header("originator", "codex_cli_rs")
            .send()
            .await?;
        anyhow::ensure!(
            response.status().is_success(),
            "live model catalog HTTP {}",
            response.status()
        );
        let catalog: ModelsResponse = response.json().await?;
        anyhow::ensure!(
            catalog.models.iter().any(|m| m.slug == model),
            "configured model absent from live catalog"
        );
        Ok(Self {
            model,
            catalog,
            effort,
            client,
            token,
            account,
            output: PathBuf::from(std::env::var("KD4_LIVE_OUTPUT")?),
            state: Mutex::new(LiveState::default()),
            runtime: tokio::runtime::Handle::current(),
        })
    }

    pub(super) fn forward(&self, raw: Value, visible: Value, candidate: bool) -> ResponseTemplate {
        // Wiremock owns a single-threaded runtime. Run HTTP work on the original
        // test runtime via a scoped thread; never block_on its reactor thread.
        bridge(&self.runtime, self.forward_async(raw, visible, candidate))
    }

    async fn forward_async(&self, raw: Value, visible: Value, candidate: bool) -> ResponseTemplate {
        let prefix = if candidate { "candidate" } else { "baseline" };
        let index = {
            let mut state = self.state.lock().unwrap();
            let input = state
                .attempts
                .iter()
                .filter_map(|v| v["usage"]["input_tokens"].as_u64())
                .sum::<u64>();
            let output_total = state
                .attempts
                .iter()
                .filter_map(|v| v["usage"]["output_tokens"].as_u64())
                .sum::<u64>();
            if state.attempts.len() >= MAX_REQUESTS
                || input >= MAX_INPUT_TOKENS
                || output_total >= MAX_OUTPUT_TOKENS
            {
                state.stopped = Some("live request/token budget reached".into());
            }
            if state.stopped.is_some() {
                return ResponseTemplate::new(400).set_body_json(json!({"error":{"message":"live benchmark stopped","type":"invalid_request_error"}}));
            }
            if candidate && raw["input"].is_array() {
                if tool_schemas(&raw) != tool_schemas(&visible) {
                    state.exercised.insert(2);
                }
                if freshness(&raw) == freshness(&visible)
                    && raw["input"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter(|v| notice(v).is_some())
                        .count()
                        > visible["input"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .filter(|v| notice(v).is_some())
                            .count()
                {
                    state.exercised.insert(10);
                }
                for item in raw["input"].as_array().unwrap() {
                    if item["type"] == "custom_tool_call" {
                        let code = item["input"].as_str().unwrap_or("");
                        if code.contains("exec_command")
                            && regex_lite::Regex::new(r#"["']?max_output_tokens["']?\s*:\s*2500"#)
                                .unwrap()
                                .is_match(code)
                        {
                            state.exercised.insert(15);
                        }
                    }
                    if item["type"] != "custom_tool_call_output" {
                        continue;
                    }
                    let id = item["call_id"].as_str().unwrap();
                    if output(&visible, id) == item {
                        continue;
                    }
                    for value in output_text(item)
                        .lines()
                        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                    {
                        match value["kind"].as_str() {
                            Some("file") => {
                                state.exercised.insert(17);
                            }
                            Some("search") => {
                                state.exercised.insert(5);
                            }
                            Some("mcp") => {
                                state.exercised.insert(18);
                            }
                            _ => {}
                        }
                    }
                }
            }
            state.attempts.len()
        };
        // Retain synthetic request evidence without HTTP headers/auth. Failure to
        // save evidence stops forwarding rather than silently making untracked calls.
        if append(
            &self.output.join(format!("{prefix}-requests.jsonl")),
            &json!({"index":index,"request":visible}),
        )
        .is_err()
        {
            self.state.lock().unwrap().stopped = Some("request evidence persistence failed".into());
            return ResponseTemplate::new(400);
        }
        let started = Instant::now();
        let url = format!(
            "{}/responses",
            codex_model_provider_info::CHATGPT_CODEX_BASE_URL
        );
        let response = self
            .client
            .post(url)
            .bearer_auth(&self.token)
            .header("ChatGPT-Account-Id", &self.account)
            .header("originator", "codex_cli_rs")
            .header("accept", "text/event-stream")
            .json(&visible)
            .send()
            .await;
        let (status, text) = match response {
            Ok(response) => {
                let status = response.status().as_u16();
                (status, response.text().await.unwrap_or_default())
            }
            Err(_) => (502, String::new()),
        };
        let usage = usage_from_sse(&text);
        let actions = text
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter_map(|data| serde_json::from_str::<Value>(data).ok())
            .filter(|event| event["type"] == "response.output_item.done")
            .map(|event| event["item"].clone())
            .filter(|item| {
                matches!(
                    item["type"].as_str(),
                    Some("custom_tool_call" | "function_call" | "message")
                )
            })
            .collect::<Vec<_>>();
        let mut record = json!({"index":index,"http_status":status,"elapsed_ms":started.elapsed().as_millis(),"usage_complete":usage.is_some(),"actions":actions});
        if let Some(usage) = usage {
            record["usage"] = usage;
        }
        if append(
            &self.output.join(format!("{prefix}-attempts.jsonl")),
            &record,
        )
        .is_err()
        {
            self.state.lock().unwrap().stopped = Some("usage evidence persistence failed".into());
        }
        let mut state = self.state.lock().unwrap();
        state.attempts.push(record);
        if status != 200 {
            state.stopped = Some(format!("upstream HTTP {status}; no retry"));
        }
        // Do not log upstream error bodies: they are unnecessary to report the
        // status and may include account information. No retries on this adapter.
        if status != 200 {
            return ResponseTemplate::new(400).set_body_json(json!({"error":{"message":format!("live provider HTTP {status}"),"type":"invalid_request_error"}}));
        }
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(text)
    }

    pub(super) async fn finish_turn(
        &self,
        test: &TestCodex,
        mcp_calls: &Arc<Mutex<Vec<Value>>>,
        evidence: &str,
        candidate: bool,
    ) -> Result<Value> {
        let budget = if candidate {
            "Set max_output_tokens:2500 on that exec_command."
        } else {
            "Do not set max_output_tokens on that exec_command; use its default."
        };
        let prompt = format!(
            r#"Run this bounded integration benchmark only inside the current temporary workspace. Do not inspect parent directories, credentials, or network services. Do not run producer.py more than once. Keep untouched.txt and evidence.txt unchanged. Use only the local bench MCP tool. Treat each numbered checkpoint as a separate exec cell and await every tool. Print complete result objects using the exact wrappers below; do not summarize them. Preserve handles in store/load as needed.
1. Read contract.txt with read_file and text({{kind:'file',result:r}}).
2. tool_search query mcp__bench__mirror limit 1; text({{kind:'search',result:r}}).
3. Repeat exactly that search in a separate cell and print the same wrapper. If its visible result is an already_available receipt, use the earlier visible schema named by previous_call_id, not another search.
4. Call the discovered mirror tool with message 'verified', ensure raw content has two entries, then text({{kind:'mcp',result:r}}).
5. Execute `python -u producer.py` with exec_command and yield_time_ms:1000. {budget} Store the full returned command result, including raw_output_artifact_id and session_id. Print only r.output. Do not directly read evidence.txt.
6. Poll the stored command session if running until exit_code 0. Recover line 501 from its raw_output_artifact_id using read_tool_output; this is ROW_0500. Print the recovered result. Do not rerun the command or read the source file.
7. Read contract.txt again in a separate cell; text({{kind:'file',result:r}}).
8. apply_patch contract.txt, replacing before with after while preserving the rest of the line. The resulting freshness notices invalidate the older reads, including records inside a batched results array.
9. Read the updated contract.txt; text({{kind:'file',result:r}}). Do not reuse old file contents as current.
10. Use apply_patch to create result.txt with exactly three newline-terminated lines: the updated contract line, the exact recovered ROW_0500 line, and the mirror structuredContent.message. No headings. Then reply only 'verified'."#
        );
        let start = Instant::now();
        let completion = tokio::time::timeout(Duration::from_secs(600), async {
            let (sandbox_policy, permission_profile) =
                turn_permission_fields(PermissionProfile::Disabled, test.config.cwd.as_path());
            test.codex
                .submit(Op::UserInput {
                    items: vec![UserInput::Text {
                        text: prompt,
                        text_elements: Vec::new(),
                    }],
                    final_output_json_schema: None,
                    responsesapi_client_metadata: None,
                    additional_context: Default::default(),
                    thread_settings: codex_protocol::protocol::ThreadSettingsOverrides {
                        approval_policy: Some(AskForApproval::Never),
                        sandbox_policy: Some(sandbox_policy),
                        permission_profile,
                        ..Default::default()
                    },
                })
                .await?;
            // Unlike TestCodex::submit_turn, do not impose the mock suite's
            // 30-second whole-turn deadline on actual inference.
            loop {
                let event = test.codex.next_event().await?;
                if let EventMsg::TurnComplete(completed) = event.msg {
                    return Ok::<_, anyhow::Error>(completed);
                }
            }
        })
        .await;
        let mut failure = None;
        match completion {
            Ok(Ok(_)) => {}
            Ok(Err(_)) => failure = Some("core turn failed"),
            Err(_) => {
                self.state.lock().unwrap().stopped = Some("600-second turn deadline".into());
                let _ = test.codex.submit(Op::Interrupt).await;
                failure = Some("turn deadline reached");
            }
        }
        let expected = format!("{AFTER}{ROW}\nverified\n");
        let result_correct =
            fs::read_to_string(test.cwd_path().join("result.txt")).is_ok_and(|v| v == expected);
        let producer_once = fs::read_to_string(test.cwd_path().join("producer-count.txt"))
            .is_ok_and(|v| v.replace("\r\n", "\n") == "run\n");
        let evidence_unchanged =
            fs::read_to_string(test.cwd_path().join("evidence.txt")).is_ok_and(|v| v == evidence);
        let sentinel_unchanged = fs::read_to_string(test.cwd_path().join("untouched.txt"))
            .is_ok_and(|v| v == "must not change\n");
        let mcp_count = mcp_calls.lock().unwrap().len();
        let state = self.state.lock().unwrap();
        let usage_complete = !state.attempts.is_empty()
            && state.attempts.iter().all(|r| r["usage_complete"] == true);
        let mut record = json!({"candidate":candidate,"model":self.model,"reasoning_effort":self.effort,"tool_mode":"code_mode (mixed override for finding 2)","model_requests":state.attempts.len(),"complete_turn_ms":start.elapsed().as_millis(),"usage_complete":usage_complete,
            "task_success":failure.is_none() && result_correct && producer_once && evidence_unchanged && sentinel_unchanged && mcp_count==1,
            "result_correct":result_correct,"producer_once":producer_once,"evidence_unchanged":evidence_unchanged,"sentinel_unchanged":sentinel_unchanged,"mcp_calls":mcp_count,"candidates_exercised":state.exercised,
            "scope":"live gpt model through test-only integrated provider-boundary adapter; not installed production optimization"});
        if usage_complete {
            for key in [
                "input_tokens",
                "cached_input_tokens",
                "uncached_input_tokens",
                "output_tokens",
                "total_tokens",
            ] {
                record[key] = json!(
                    state
                        .attempts
                        .iter()
                        .map(|r| r["usage"][key].as_u64().unwrap())
                        .sum::<u64>()
                );
            }
        }
        if let Some(reason) = &state.stopped {
            record["stopped"] = json!(reason);
        }
        if let Some(failure) = failure {
            record["failure"] = json!(failure);
        }
        append(&self.output.join("records.jsonl"), &record)?;
        Ok(record)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires explicit live opt-in; consumes real ChatGPT model usage"]
async fn integrated_live_model_pair() -> Result<()> {
    assert_usage_parser_contract();
    let live = Arc::new(LiveContext::load().await?);
    let baseline = one_turn(false, Some(Arc::clone(&live))).await?;
    anyhow::ensure!(
        baseline.get("stopped").is_none(),
        "baseline stopped; candidate not sent; inspect saved records"
    );
    *live.state.lock().unwrap() = LiveState::default();
    let candidate = one_turn(true, Some(live)).await?;
    anyhow::ensure!(
        baseline["task_success"] == true && candidate["task_success"] == true,
        "live task non-regression failed; inspect saved records"
    );
    anyhow::ensure!(
        baseline["usage_complete"] == true && candidate["usage_complete"] == true,
        "provider did not return complete usage; do not substitute estimates"
    );
    anyhow::ensure!(
        // #17/#18 now run in production in both arms, not in this adapter.
        candidate["candidates_exercised"] == json!([2, 5, 10, 15]),
        "live model did not exercise all remaining candidates; inspect saved records"
    );
    // No assertion assumes live inference must improve tokens. Regressions are
    // experimental results, not a reason to silently retry paid requests.
    Ok(())
}
