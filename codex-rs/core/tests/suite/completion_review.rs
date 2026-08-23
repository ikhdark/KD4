use std::collections::VecDeque;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Result;
use codex_config::config_toml::AfterAgentPolicy;
use codex_config::types::Personality;
use codex_core::config::AgentRoleConfig;
use codex_features::Feature;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::Settings;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::TaskCompletionStatus;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::user_input::UserInput;
use core_test_support::hooks::trust_discovered_hooks;
use core_test_support::responses::ResponseMock;
use core_test_support::responses::ev_apply_patch_custom_tool_call;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_sequence as mount_raw_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::sse_response;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::TestCodexBuilder;
use core_test_support::test_codex::test_codex;
use core_test_support::test_codex::turn_permission_fields;
use serde_json::Value;
use serde_json::json;
use tokio::time::sleep;
use tokio::time::timeout;
use wiremock::Mock;
use wiremock::Respond;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path_regex;

const CLASSIFICATION_REQUEST_MARKER: &str = "KD4_SOURCE_CLASSIFICATION_REQUEST_V1";
const LOCAL_CLASSIFICATION_REQUEST_MARKER: &str = "KD4_SOURCE_LOCAL_CLASSIFICATION_REQUEST_V4";
const RELATIONSHIP_RESOLUTION_REQUEST_MARKER: &str =
    "KD4_SOURCE_RELATIONSHIP_RESOLUTION_REQUEST_V1";
const REVIEW_REQUEST_MARKER: &str = "KD4_COMPLETION_REVIEW_REQUEST_V2";
const REPAIR_MARKER: &str = "<kd4_completion_repair>";
const RESPONSE_BUNDLE_SEPARATOR: &str = "\n<KD4_TEST_RESPONSE_BOUNDARY>\n";

fn completion_review_builder() -> TestCodexBuilder {
    completion_review_builder_with_role(true)
}

fn completion_review_builder_with_role(register_reviewer_role: bool) -> TestCodexBuilder {
    test_codex().with_config(move |config| {
        let initialized = Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&config.cwd)
            .status()
            .expect("run git init");
        assert!(
            initialized.success(),
            "initialize completion-review repository"
        );
        fs::write(
            config.cwd.join("kd4_features.toml"),
            "schema_version = 1\nfork = \"KD4\"\n",
        )
        .expect("write KD4 marker");
        fs::create_dir_all(config.cwd.join("src")).expect("create completion fixture root");
        fs::write(
            config.cwd.join("src/lib.rs"),
            concat!(
                "pub fn completion_state() -> &'static str { \"before\" }\n",
                "\n",
                "#[cfg(test)]\n",
                "mod tests {\n",
                "    #[test]\n",
                "    fn completion_state_is_available() {\n",
                "        assert!(!super::completion_state().is_empty());\n",
                "    }\n",
                "}\n",
            ),
        )
        .expect("write completion fixture source");
        fs::write(
            config.cwd.join("Cargo.toml"),
            "[package]\nname = \"completion-review-fixture\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
        )
        .expect("write completion fixture manifest");
        fs::write(config.cwd.join(".gitignore"), "target/\nCargo.lock\n")
            .expect("ignore completion fixture build output");
        let staged = Command::new("git")
            .args([
                "add",
                ".gitignore",
                "Cargo.toml",
                "kd4_features.toml",
                "src/lib.rs",
            ])
            .current_dir(&config.cwd)
            .status()
            .expect("stage KD4 marker");
        assert!(staged.success(), "stage completion-review baseline");
        let committed = Command::new("git")
            .args([
                "-c",
                "user.name=Codex Tests",
                "-c",
                "user.email=codex-tests@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "completion-review baseline",
            ])
            .current_dir(&config.cwd)
            .status()
            .expect("commit completion-review baseline");
        assert!(committed.success(), "commit completion-review baseline");
        if register_reviewer_role {
            let reviewer_role = config.codex_home.join("reviewer-test.toml");
            fs::write(
                &reviewer_role,
                "model_reasoning_effort = \"high\"\nsandbox_mode = \"read-only\"\n",
            )
            .expect("write reviewer role");
            config.agent_roles.insert(
                "reviewer".to_string(),
                AgentRoleConfig {
                    description: Some("Completion reviewer".to_string()),
                    config_file: Some(reviewer_role.to_path_buf()),
                    nickname_candidates: None,
                },
            );
        }
        config
            .features
            .enable(Feature::TaskCompletionReviewer)
            .expect("enable completion reviewer");
    })
}

fn write_explicit_stop_hook(home: &Path) {
    let (script_path, command) = if cfg!(windows) {
        let script_path = home.join("completion-review-stop.ps1");
        fs::write(
            &script_path,
            r#"[Console]::In.ReadToEnd() | Out-Null
Write-Output '{"continue":false,"stopReason":"explicit stop"}'
"#,
        )
        .expect("write explicit stop hook");
        (
            script_path.clone(),
            format!(
                "powershell.exe -NoProfile -ExecutionPolicy Bypass -File \"{}\"",
                script_path.display()
            ),
        )
    } else {
        let script_path = home.join("completion-review-stop.sh");
        fs::write(
            &script_path,
            "#!/bin/sh\ncat >/dev/null\nprintf '%s\\n' '{\"continue\":false,\"stopReason\":\"explicit stop\"}'\n",
        )
        .expect("write explicit stop hook");
        (
            script_path.clone(),
            format!("sh \"{}\"", script_path.display()),
        )
    };
    assert!(script_path.is_file());
    fs::write(
        home.join("hooks.json"),
        json!({
            "hooks": {
                "Stop": [{
                    "hooks": [{
                        "type": "command",
                        "command": command
                    }]
                }]
            }
        })
        .to_string(),
    )
    .expect("write explicit stop hook config");
}

fn write_single_continuation_stop_hook(home: &Path) {
    let prompt = "complete the stop-hook-requested follow-up";
    let (script_path, command) = if cfg!(windows) {
        let script_path = home.join("completion-review-continuation.ps1");
        fs::write(
            &script_path,
            format!(
                r#"[Console]::In.ReadToEnd() | Out-Null
$state = Join-Path $PSScriptRoot 'completion-review-stop-seen'
if (Test-Path -LiteralPath $state) {{
    Write-Output '{{}}'
}} else {{
    New-Item -ItemType File -Path $state | Out-Null
    Write-Output '{{"decision":"block","reason":"{prompt}"}}'
}}
"#
            ),
        )
        .expect("write continuation stop hook");
        (
            script_path.clone(),
            format!(
                "powershell.exe -NoProfile -ExecutionPolicy Bypass -File \"{}\"",
                script_path.display()
            ),
        )
    } else {
        let script_path = home.join("completion-review-continuation.sh");
        fs::write(
            &script_path,
            format!(
                "#!/bin/sh\ncat >/dev/null\nstate=\"$(dirname \"$0\")/completion-review-stop-seen\"\nif [ -f \"$state\" ]; then\n  printf '%s\\n' '{{}}'\nelse\n  : > \"$state\"\n  printf '%s\\n' '{{\"decision\":\"block\",\"reason\":\"{prompt}\"}}'\nfi\n"
            ),
        )
        .expect("write continuation stop hook");
        (
            script_path.clone(),
            format!("sh \"{}\"", script_path.display()),
        )
    };
    assert!(script_path.is_file());
    fs::write(
        home.join("hooks.json"),
        json!({
            "hooks": {
                "Stop": [{
                    "hooks": [{
                        "type": "command",
                        "command": command
                    }]
                }]
            }
        })
        .to_string(),
    )
    .expect("write continuation stop hook config");
}

fn completion_review_builder_with_after_agent_probe() -> TestCodexBuilder {
    completion_review_builder().with_config(|config| {
        let marker = config.codex_home.join("after-agent-order.txt");
        if cfg!(windows) {
            let script = config.codex_home.join("after-agent-order.ps1");
            fs::write(
                &script,
                r#"param([string]$Payload)
$receipt = Get-ChildItem -Path (Join-Path $PSScriptRoot 'task-evidence\*.json') -ErrorAction SilentlyContinue | Select-Object -First 1
$result = 'missing-review'
if ($null -ne $receipt) {
    $content = Get-Content -LiteralPath $receipt.FullName -Raw
    if ($content -match '"attempt_kind"\s*:\s*"initial_review"' -and $content -match '"review_clean"\s*:\s*true') {
        $result = 'reviewed'
    }
}

Set-Content -LiteralPath (Join-Path $PSScriptRoot 'after-agent-order.txt') -Value $result
"#,
            )
            .expect("write AfterAgent probe");
            config.notify = Some(vec![
                "powershell.exe".to_string(),
                "-NoProfile".to_string(),
                "-ExecutionPolicy".to_string(),
                "Bypass".to_string(),
                "-File".to_string(),
                script.display().to_string(),
            ]);
        } else {
            let script = config.codex_home.join("after-agent-order.sh");
            fs::write(
                &script,
                "#!/bin/sh\nif grep -Eq '\"attempt_kind\"[[:space:]]*:[[:space:]]*\"initial_review\"' \"$(dirname \"$0\")\"/task-evidence/*.json && grep -Eq '\"review_clean\"[[:space:]]*:[[:space:]]*true' \"$(dirname \"$0\")\"/task-evidence/*.json; then\n  printf reviewed > \"$(dirname \"$0\")/after-agent-order.txt\"\nelse\n  printf missing-review > \"$(dirname \"$0\")/after-agent-order.txt\"\nfi\n",
            )
            .expect("write AfterAgent probe");
            config.notify = Some(vec!["sh".to_string(), script.display().to_string()]);
        }
        assert_eq!(marker.file_name().and_then(|name| name.to_str()), Some("after-agent-order.txt"));
    })
}

fn completion_review_builder_with_mutating_finalizer(abort: bool) -> TestCodexBuilder {
    completion_review_builder().with_config(move |config| {
        config.after_agent_policy = AfterAgentPolicy::MutatingFinalizer;
        let workspace = config.cwd.display().to_string();
        if cfg!(windows) {
            let script = config.codex_home.join("mutating-finalizer.ps1");
            let exit = if abort { "exit 7" } else { "" };
            fs::write(
                &script,
                format!(
                    r#"param([string]$Workspace, [string]$Payload)
$countPath = Join-Path $PSScriptRoot 'mutating-finalizer-count.txt'
$count = 0
if (Test-Path -LiteralPath $countPath) {{ $count = [int](Get-Content -LiteralPath $countPath -Raw) }}
Set-Content -LiteralPath $countPath -Value ($count + 1)
Set-Content -LiteralPath (Join-Path $Workspace 'finalizer.txt') -Value 'finalized'
{exit}
"#
                ),
            )
            .expect("write mutating finalizer");
            config.notify = Some(vec![
                "powershell.exe".to_string(),
                "-NoProfile".to_string(),
                "-ExecutionPolicy".to_string(),
                "Bypass".to_string(),
                "-File".to_string(),
                script.display().to_string(),
                workspace,
            ]);
        } else {
            let script = config.codex_home.join("mutating-finalizer.sh");
            let exit = if abort { "exit 7" } else { "" };
            fs::write(
                &script,
                format!(
                    "#!/bin/sh\ncount_path=\"$(dirname \"$0\")/mutating-finalizer-count.txt\"\ncount=0\nif [ -f \"$count_path\" ]; then count=$(cat \"$count_path\"); fi\nprintf '%s\\n' \"$((count + 1))\" > \"$count_path\"\nprintf finalized > \"$1/finalizer.txt\"\n{exit}\n"
                ),
            )
            .expect("write mutating finalizer");
            config.notify = Some(vec![
                "sh".to_string(),
                script.display().to_string(),
                workspace,
            ]);
        }
    })
}

fn plan_response(response_id: &str, call_id: &str, status: &str) -> String {
    if status == "passed" {
        let implemented = plan_response(
            &format!("{response_id}-implemented"),
            &format!("{call_id}-implemented"),
            "implemented",
        );
        let validation = validation_response(response_id, call_id);
        return format!("{implemented}{RESPONSE_BUNDLE_SEPARATOR}{validation}");
    }
    let args = json!({
        "explanation": "completion review test",
        "plan": [{
            "id": "completion-step",
            "step": "Implement the requested completion behavior",
            "status": status,
            "depends_on": [],
            "acceptance_criteria": ["The requested file behavior is present"],
            "runtime_paths": ["src/lib.rs"],
            "generated_artifacts": [],
            "risks": [],
            "requires_desktop_activation": false,
            "validation_route": {
                "leaves": [{
                    "argv": [
                        "cargo",
                        "test",
                        "-p",
                        "completion-review-fixture",
                        "tests::completion_state_is_available",
                        "--",
                        "--exact"
                    ],
                    "uncertainty": "the requested file behavior remains present",
                    "covered_paths": ["src/lib.rs"],
                    "covered_contracts": ["The requested file behavior is present"],
                    "timeout_ms": 10000
                }]
            }
        }]
    })
    .to_string();
    sse(vec![
        ev_response_created(response_id),
        ev_function_call(call_id, "update_plan", &args),
        ev_completed(response_id),
    ])
}

fn validation_response(response_id: &str, call_id: &str) -> String {
    let validation_args = json!({
        "kind": "argv",
        "program": "cargo",
        "args": [
            "test",
            "-p",
            "completion-review-fixture",
            "tests::completion_state_is_available",
            "--",
            "--exact"
        ],
        "yield_time_ms": 30_000,
        "validation": {
            "uncertainty": "the requested file behavior remains present",
            "covered_paths": ["src/lib.rs"],
            "covered_contracts": ["The requested file behavior is present"]
        }
    })
    .to_string();
    sse(vec![
        ev_response_created(response_id),
        ev_function_call(call_id, "exec_command", &validation_args),
        ev_completed(response_id),
    ])
}

fn expand_response_bundles(responses: Vec<String>) -> Vec<String> {
    responses
        .into_iter()
        .flat_map(|response| {
            response
                .split(RESPONSE_BUNDLE_SEPARATOR)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .collect()
}

async fn mount_sse_sequence(server: &wiremock::MockServer, responses: Vec<String>) -> ResponseMock {
    mount_raw_sse_sequence(server, expand_response_bundles(responses)).await
}

fn patch_response(response_id: &str, call_id: &str, patch: &str) -> String {
    let (before, after) = if patch.contains("done with omitted requirement") {
        ("done", "done with omitted requirement")
    } else if patch.contains("done after repair") {
        ("done", "done after repair")
    } else if patch.contains("second-repair.txt") {
        ("done", "done after second repair")
    } else {
        ("before", "done")
    };
    let patch = format!(
        "*** Begin Patch\n*** Update File: src/lib.rs\n@@\n-pub fn completion_state() -> &'static str {{ \"{before}\" }}\n+pub fn completion_state() -> &'static str {{ \"{after}\" }}\n*** End Patch"
    );
    sse(vec![
        ev_response_created(response_id),
        ev_apply_patch_custom_tool_call(call_id, &patch),
        ev_completed(response_id),
    ])
}

fn message_response(response_id: &str, message_id: &str, text: &str) -> String {
    sse(vec![
        ev_response_created(response_id),
        ev_assistant_message(message_id, text),
        ev_completed(response_id),
    ])
}

#[derive(Clone)]
enum ReviewScenario {
    Clean,
    FindingThenClean {
        summary: String,
    },
    FindingThenUnresolvedThenClean {
        summary: String,
        unresolved_rereviews: usize,
    },
    Malformed,
    Oversized,
    ManifestGap,
}

#[derive(Default)]
struct CompletionReviewProbe {
    total_requests: AtomicUsize,
    classification_requests: AtomicUsize,
    relationship_resolution_requests: AtomicUsize,
    review_requests: AtomicUsize,
    rereview_requests: AtomicUsize,
    repair_requests: AtomicUsize,
    repair_payloads: Mutex<Vec<String>>,
    request_payloads: Mutex<Vec<String>>,
}

struct CompletionReviewResponder {
    ordinary_responses: Mutex<VecDeque<String>>,
    scenario: ReviewScenario,
    probe: Arc<CompletionReviewProbe>,
}

impl Respond for CompletionReviewResponder {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let body_bytes = decode_body_bytes(request);
        let body: Value = serde_json::from_slice(&body_bytes).unwrap_or(Value::Null);
        let mut strings = Vec::new();
        collect_strings(&body, &mut strings);
        let combined = strings.join("\n");
        self.probe
            .request_payloads
            .lock()
            .expect("request payload lock")
            .push(combined.clone());
        let request_index = self.probe.total_requests.fetch_add(1, Ordering::SeqCst);
        if let Some(request_text) = strings
            .iter()
            .find(|text| text.contains(LOCAL_CLASSIFICATION_REQUEST_MARKER))
        {
            self.probe
                .classification_requests
                .fetch_add(1, Ordering::SeqCst);
            let items = tagged_json(request_text, "source_local_items");
            let response = local_classification_response(
                &items,
                matches!(&self.scenario, ReviewScenario::ManifestGap),
            );
            return sse_response(message_response(
                &format!("local-classification-{request_index}"),
                &format!("local-classification-message-{request_index}"),
                &response.to_string(),
            ));
        }
        if let Some(request_text) = strings
            .iter()
            .find(|text| text.contains(CLASSIFICATION_REQUEST_MARKER))
        {
            self.probe
                .classification_requests
                .fetch_add(1, Ordering::SeqCst);
            let dossier = tagged_json(request_text, "source_ledger");
            let response = classification_response(
                &dossier,
                matches!(&self.scenario, ReviewScenario::ManifestGap),
            );
            return sse_response(message_response(
                &format!("classification-{request_index}"),
                &format!("classification-message-{request_index}"),
                &response.to_string(),
            ));
        }

        if let Some(request_text) = strings
            .iter()
            .find(|text| text.contains(RELATIONSHIP_RESOLUTION_REQUEST_MARKER))
        {
            self.probe
                .relationship_resolution_requests
                .fetch_add(1, Ordering::SeqCst);
            let input = tagged_json(request_text, "relationship_input");
            let response = relationship_resolution_response(&input);
            return sse_response(message_response(
                &format!("relationship-resolution-{request_index}"),
                &format!("relationship-resolution-message-{request_index}"),
                &response.to_string(),
            ));
        }

        if let Some(request_text) = strings
            .iter()
            .find(|text| text.contains(REVIEW_REQUEST_MARKER))
        {
            let review_index = self.probe.review_requests.fetch_add(1, Ordering::SeqCst);
            let dossier = tagged_json(request_text, "completion_dossier");
            if dossier["rereview"].as_bool().unwrap_or(false) {
                self.probe.rereview_requests.fetch_add(1, Ordering::SeqCst);
            }
            let response = match &self.scenario {
                ReviewScenario::Malformed => "not-json".to_string(),
                ReviewScenario::Oversized => json!({
                    "padding": "word ".repeat(10_000)
                })
                .to_string(),
                ReviewScenario::Clean => review_response(&dossier, ReviewResponseKind::Clean),
                ReviewScenario::FindingThenClean { summary } => {
                    if review_index == 0 {
                        review_response(&dossier, ReviewResponseKind::Finding(summary.as_str()))
                    } else {
                        review_response(&dossier, ReviewResponseKind::Clean)
                    }
                }
                ReviewScenario::FindingThenUnresolvedThenClean {
                    summary,
                    unresolved_rereviews,
                } => {
                    if review_index == 0 {
                        review_response(&dossier, ReviewResponseKind::Finding(summary.as_str()))
                    } else if review_index <= *unresolved_rereviews {
                        review_response(&dossier, ReviewResponseKind::Unresolved)
                    } else {
                        review_response(&dossier, ReviewResponseKind::Clean)
                    }
                }
                ReviewScenario::ManifestGap => {
                    if review_index == 0 {
                        review_response(&dossier, ReviewResponseKind::ManifestGap)
                    } else {
                        review_response(&dossier, ReviewResponseKind::Clean)
                    }
                }
            };
            return sse_response(message_response(
                &format!("review-{request_index}"),
                &format!("review-message-{request_index}"),
                &response,
            ));
        }

        if combined.contains(REPAIR_MARKER) {
            self.probe.repair_requests.fetch_add(1, Ordering::SeqCst);
            self.probe
                .repair_payloads
                .lock()
                .expect("repair payload lock")
                .push(combined);
        }
        let response = self
            .ordinary_responses
            .lock()
            .expect("ordinary response lock")
            .pop_front()
            .unwrap_or_else(|| {
                message_response(
                    &format!("unexpected-{request_index}"),
                    &format!("unexpected-message-{request_index}"),
                    "unexpected request",
                )
            });
        sse_response(response)
    }
}

#[derive(Clone, Copy)]
enum ReviewResponseKind<'a> {
    Clean,
    Finding(&'a str),
    Unresolved,
    ManifestGap,
}

async fn mount_completion_review_sequence(
    server: &wiremock::MockServer,
    ordinary_responses: Vec<String>,
    scenario: ReviewScenario,
) -> Arc<CompletionReviewProbe> {
    let probe = Arc::new(CompletionReviewProbe::default());
    let ordinary_responses = expand_response_bundles(ordinary_responses);
    Mock::given(method("POST"))
        .and(path_regex(".*/responses$"))
        .respond_with(CompletionReviewResponder {
            ordinary_responses: Mutex::new(ordinary_responses.into()),
            scenario,
            probe: Arc::clone(&probe),
        })
        .mount(server)
        .await;
    probe
}

fn decode_body_bytes(request: &wiremock::Request) -> Vec<u8> {
    let Some(encoding) = request
        .headers
        .get("content-encoding")
        .and_then(|value| value.to_str().ok())
    else {
        return request.body.clone();
    };
    if encoding
        .split(',')
        .any(|entry| entry.trim().eq_ignore_ascii_case("zstd"))
    {
        zstd::stream::decode_all(std::io::Cursor::new(&request.body))
            .unwrap_or_else(|_| request.body.clone())
    } else {
        request.body.clone()
    }
}

fn collect_strings(value: &Value, strings: &mut Vec<String>) {
    let mut pending = vec![value];
    while let Some(value) = pending.pop() {
        match value {
            Value::String(text) => strings.push(text.clone()),
            Value::Array(values) => pending.extend(values),
            Value::Object(values) => pending.extend(values.values()),
            Value::Null | Value::Bool(_) | Value::Number(_) => {}
        }
    }
}

fn tagged_json(text: &str, tag: &str) -> Value {
    let start_tag = format!("<{tag}>");
    let end_tag = format!("</{tag}>");
    let start = text.find(&start_tag).expect("tag start") + start_tag.len();
    let end = text[start..].find(&end_tag).expect("tag end") + start;
    serde_json::from_str(text[start..end].trim()).expect("tagged dossier JSON")
}

fn empty_text_span() -> Value {
    json!({
        "kind": "text",
        "start": 0,
        "end": 0,
        "reference": "",
        "subreference": "",
    })
}

fn source_span(source: &Value) -> Value {
    let material = source["exact_material"].as_str().unwrap_or_default();
    match source["source_kind"].as_str().unwrap_or("text") {
        "image" => json!({
            "kind": "image",
            "start": 0,
            "end": 0,
            "reference": material,
            "subreference": "",
        }),
        "attachment" => json!({
            "kind": "attachment",
            "start": 0,
            "end": 0,
            "reference": material,
            "subreference": "",
        }),
        _ => json!({
            "kind": "text",
            "start": 0,
            "end": material.len(),
            "reference": "",
            "subreference": "",
        }),
    }
}

fn classification_response(dossier: &Value, classify_as_context: bool) -> Value {
    let sources = dossier["sources"]
        .as_array()
        .expect("classification sources")
        .iter()
        .map(|source| {
            let source_id = source["source_id"].as_str().expect("source ID");
            if source["availability"].as_str() != Some("available") {
                return json!({
                    "source_id": source_id,
                    "result": "unavailable_or_truncated",
                    "requirements": [],
                    "reason": "",
                });
            }
            if classify_as_context {
                return json!({
                    "source_id": source_id,
                    "result": "non_requirement",
                    "requirements": [],
                    "reason": "initial classification treated this immutable text as context",
                });
            }
            json!({
                "source_id": source_id,
                "result": "requirement_bearing",
                "requirements": [{
                    "source_span": source_span(source),
                    "status": "active",
                    "superseded_by_source_id": "",
                    "superseded_by_span": empty_text_span(),
                }],
                "reason": "",
            })
        })
        .collect::<Vec<_>>();
    json!({ "sources": sources })
}

fn local_classification_response(items: &Value, classify_as_context: bool) -> Value {
    let items = items
        .as_array()
        .expect("local classification items")
        .iter()
        .map(|item| {
            let item_id = item["item_id"].as_str().expect("local item ID");
            if classify_as_context {
                return json!({
                    "item_id": item_id,
                    "local_kind": "non_requirement",
                    "requirement_spans": [],
                    "local_semantic_cues": [],
                    "reason": "initial classification treated this immutable text as context",
                });
            }
            let source = json!({
                "source_kind": item["source_kind"],
                "exact_material": item["exact_material"],
            });
            let span = source_span(&source);
            json!({
                "item_id": item_id,
                "local_kind": "requirement_bearing",
                "requirement_spans": [span],
                "local_semantic_cues": [{
                    "kind": "assertion",
                    "source_span": span,
                }],
                "reason": "the source states the requested implementation requirement",
            })
        })
        .collect::<Vec<_>>();
    json!({ "items": items })
}

fn relationship_resolution_response(input: &Value) -> Value {
    let sources = input["sources"]
        .as_array()
        .expect("relationship resolution sources")
        .iter()
        .map(|source| {
            let local = &source["local_classification"];
            let source_relationship = match local["local_kind"].as_str() {
                Some("relationship_only_context") => "superseded_context",
                _ => "none",
            };
            let requirements = local["requirement_spans"]
                .as_array()
                .expect("relationship resolution requirement spans")
                .iter()
                .map(|source_span| {
                    let source_span = json!({
                        "kind": source_span["kind"],
                        "start": source_span["start"],
                        "end": source_span["end"],
                        "reference": source_span
                            .get("reference")
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                        "subreference": source_span
                            .get("subreference")
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                    });
                    json!({
                        "source_span": source_span,
                        "status": "active",
                        "superseded_by_source_id": "",
                        "superseded_by_span": empty_text_span(),
                    })
                })
                .collect::<Vec<_>>();
            json!({
                "source_id": source["source_id"],
                "source_relationship": source_relationship,
                "requirements": requirements,
            })
        })
        .collect::<Vec<_>>();
    json!({ "sources": sources })
}

fn review_response(dossier: &Value, kind: ReviewResponseKind<'_>) -> String {
    let requirements = dossier["requirements"]
        .as_array()
        .expect("review requirements");
    let finding_requirement_id = requirements
        .iter()
        .next()
        .and_then(|requirement| requirement["requirement_id"].as_str());
    let finding_lens = dossier["review_lenses"]
        .as_array()
        .and_then(|lenses| lenses.first())
        .and_then(Value::as_str);
    let manifest_gaps = if matches!(kind, ReviewResponseKind::ManifestGap) {
        let source = dossier["sources"]
            .as_array()
            .and_then(|sources| sources.first())
            .expect("review source");
        vec![json!({
            "source_id": source["source_id"],
            "omitted_source_spans": [source_span(source)],
        })]
    } else {
        Vec::new()
    };
    let mut unsatisfied_requirement_ids = Vec::new();
    if matches!(kind, ReviewResponseKind::Finding(_)) {
        unsatisfied_requirement_ids.push(
            finding_requirement_id
                .expect("active requirement")
                .to_string(),
        );
    }
    if matches!(kind, ReviewResponseKind::Unresolved) {
        for finding in dossier["original_findings"]
            .as_array()
            .into_iter()
            .flatten()
        {
            for requirement_id in finding["requirement_ids"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                if !unsatisfied_requirement_ids
                    .iter()
                    .any(|candidate| candidate == requirement_id)
                {
                    unsatisfied_requirement_ids.push(requirement_id.to_string());
                }
            }
        }
    }
    let unsatisfied_requirements = unsatisfied_requirement_ids
        .iter()
        .map(|requirement_id| {
            json!({
                "requirement_id": requirement_id,
                "evidence": "the current candidate does not satisfy this active requirement",
            })
        })
        .collect::<Vec<_>>();
    let findings = match kind {
        ReviewResponseKind::Finding(summary) => vec![json!({
            "finding_local_ordinal": 1,
            "requirement_ids": [finding_requirement_id.expect("active requirement")],
            "lens": finding_lens.expect("selected review lens"),
            "contract_surface": "src/lib.rs behavior",
            "severity": "high",
            "concrete_evidence": summary,
            "smallest_correction": "add the omitted requirement to src/lib.rs",
            "focused_proof_route": "cargo test -p codex-core completion_review",
        })],
        ReviewResponseKind::Clean
        | ReviewResponseKind::Unresolved
        | ReviewResponseKind::ManifestGap => Vec::new(),
    };
    let dispositions = dossier["original_findings"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|finding| {
            json!({
                "finding_id": finding["finding_id"],
                "disposition": if matches!(kind, ReviewResponseKind::Unresolved) {
                    "still_present"
                } else {
                    "resolved"
                },
                "evidence": if matches!(kind, ReviewResponseKind::Unresolved) {
                    "the original defect remains after the one correction phase"
                } else {
                    "fresh proof covers the correction without regression"
                },
            })
        })
        .collect::<Vec<_>>();
    json!({
        "manifest_gaps": manifest_gaps,
        "unsatisfied_requirements": unsatisfied_requirements,
        "lens_observations": [],
        "findings": findings,
        "prior_finding_dispositions": dispositions,
    })
    .to_string()
}

fn reviewer_request_count(requests: &[core_test_support::responses::ResponsesRequest]) -> usize {
    requests
        .iter()
        .filter(|request| request.body_contains_text(REVIEW_REQUEST_MARKER))
        .count()
}

async fn submit_turn_and_capture_completion(
    test: &TestCodex,
    prompt: &str,
) -> Result<TurnCompleteEvent> {
    submit_turn_and_capture_completion_with(test, prompt, ModeKind::Default, None).await
}

async fn submit_turn_and_capture_completion_with(
    test: &TestCodex,
    prompt: &str,
    mode: ModeKind,
    final_output_json_schema: Option<Value>,
) -> Result<TurnCompleteEvent> {
    let (sandbox_policy, permission_profile) =
        turn_permission_fields(PermissionProfile::Disabled, test.config.cwd.as_path());
    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: prompt.to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: ThreadSettingsOverrides {
                environments: None,
                approval_policy: Some(AskForApproval::Never),
                sandbox_policy: Some(sandbox_policy),
                permission_profile,
                collaboration_mode: Some(CollaborationMode {
                    mode,
                    settings: Settings {
                        model: test.session_configured.model.clone(),
                        reasoning_effort: None,
                        developer_instructions: None,
                    },
                }),
                ..Default::default()
            },
        })
        .await?;

    loop {
        let event = timeout(Duration::from_secs(30), test.codex.next_event()).await??;
        if let EventMsg::TurnComplete(completed) = event.msg {
            return Ok(completed);
        }
    }
}

async fn assert_no_additional_turn_complete(test: &TestCodex) {
    let additional = timeout(Duration::from_millis(100), async {
        loop {
            let Ok(event) = test.codex.next_event().await else {
                return false;
            };
            if matches!(event.msg, EventMsg::TurnComplete(_)) {
                return true;
            }
        }
    })
    .await;
    assert!(!matches!(additional, Ok(true)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clean_review_finishes_without_a_repair_continuation() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let probe = mount_completion_review_sequence(
        &server,
        vec![
            plan_response("plan-start", "plan-start-call", "in_progress"),
            patch_response(
                "initial-patch",
                "initial-patch-call",
                "*** Begin Patch\n*** Add File: completed.txt\n+done\n*** End Patch",
            ),
            plan_response("plan-pass", "plan-pass-call", "passed"),
            message_response("candidate", "candidate-message", "implementation complete"),
        ],
        ReviewScenario::Clean,
    )
    .await;
    let mut builder = completion_review_builder()
        .with_model("gpt-5.4")
        .with_config(|config| {
            config
                .features
                .enable(Feature::Personality)
                .expect("enable personality for reviewer-isolation proof");
            config.personality = Some(Personality::Friendly);
        });
    let test = builder.build(&server).await?;
    let completion =
        submit_turn_and_capture_completion(&test, "Implement the requested completion behavior")
            .await?;
    assert_eq!(
        completion.completion.as_ref().map(|gate| gate.status),
        Some(TaskCompletionStatus::Passed),
        "reviews={}, rereviews={}, repairs={}, total_requests={}, gate={:?}",
        probe.review_requests.load(Ordering::SeqCst),
        probe.rereview_requests.load(Ordering::SeqCst),
        probe.repair_requests.load(Ordering::SeqCst),
        probe.total_requests.load(Ordering::SeqCst),
        completion.completion,
    );
    assert_no_additional_turn_complete(&test).await;

    assert_eq!(probe.total_requests.load(Ordering::SeqCst), 7);
    assert_eq!(probe.classification_requests.load(Ordering::SeqCst), 1);
    assert_eq!(probe.review_requests.load(Ordering::SeqCst), 1);
    assert_eq!(probe.rereview_requests.load(Ordering::SeqCst), 0);
    assert_eq!(probe.repair_requests.load(Ordering::SeqCst), 0);
    let personality_fragment =
        "You optimize for team morale and being a supportive teammate as much as code quality.";
    let payloads = probe.request_payloads.lock().expect("request payloads");
    assert!(
        payloads
            .iter()
            .any(|payload| payload.contains(personality_fragment)),
        "the parent request should make the personality isolation assertion sensitive"
    );
    for reviewer_payload in payloads.iter().filter(|payload| {
        payload.contains(CLASSIFICATION_REQUEST_MARKER) || payload.contains(REVIEW_REQUEST_MARKER)
    }) {
        assert!(!reviewer_payload.contains(personality_fragment));
        assert!(!reviewer_payload.contains("<personality_spec>"));
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manifest_gap_rebuilds_the_manifest_and_starts_a_fresh_initial_review() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let probe = mount_completion_review_sequence(
        &server,
        vec![
            plan_response("plan-start", "plan-start-call", "in_progress"),
            patch_response(
                "initial-patch",
                "initial-patch-call",
                "*** Begin Patch\n*** Add File: completed.txt\n+done\n*** End Patch",
            ),
            plan_response("plan-pass", "plan-pass-call", "passed"),
            message_response("candidate", "candidate-message", "implementation complete"),
        ],
        ReviewScenario::ManifestGap,
    )
    .await;
    let mut builder = completion_review_builder();
    let test = builder.build(&server).await?;

    let completion =
        submit_turn_and_capture_completion(&test, "Implement the requirement in this message")
            .await?;
    assert_eq!(
        completion.completion.as_ref().map(|gate| gate.status),
        Some(TaskCompletionStatus::Passed),
        "unexpected completion gate: {:?}",
        completion.completion
    );
    assert_no_additional_turn_complete(&test).await;

    assert_eq!(
        probe.total_requests.load(Ordering::SeqCst),
        9,
        "classifications={}, relationships={}, reviews={}, rereviews={}, repairs={}",
        probe.classification_requests.load(Ordering::SeqCst),
        probe
            .relationship_resolution_requests
            .load(Ordering::SeqCst),
        probe.review_requests.load(Ordering::SeqCst),
        probe.rereview_requests.load(Ordering::SeqCst),
        probe.repair_requests.load(Ordering::SeqCst),
    );
    assert_eq!(probe.classification_requests.load(Ordering::SeqCst), 1);
    assert_eq!(
        probe
            .relationship_resolution_requests
            .load(Ordering::SeqCst),
        1
    );
    assert_eq!(probe.review_requests.load(Ordering::SeqCst), 2);
    assert_eq!(probe.rereview_requests.load(Ordering::SeqCst), 0);
    assert_eq!(probe.repair_requests.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reviewer_finding_injects_one_repair_and_repair_mutation_cannot_rearm_review() -> Result<()>
{
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let finding = "the user requirement was omitted";
    let probe = mount_completion_review_sequence(
        &server,
        vec![
            plan_response("plan-start", "plan-start-call", "in_progress"),
            patch_response(
                "initial-patch",
                "initial-patch-call",
                "*** Begin Patch\n*** Add File: completed.txt\n+done\n*** End Patch",
            ),
            plan_response("plan-pass", "plan-pass-call", "passed"),
            message_response("candidate", "candidate-message", "implementation complete"),
            plan_response(
                "repair-plan-start",
                "repair-plan-start-call",
                "in_progress",
            ),
            patch_response(
                "repair-patch",
                "repair-patch-call",
                "*** Begin Patch\n*** Update File: completed.txt\n@@\n-done\n+done with omitted requirement\n*** End Patch",
            ),
            plan_response("repair-plan-pass", "repair-plan-pass-call", "passed"),
            message_response("repaired", "repaired-message", "repair complete"),
        ],
        ReviewScenario::FindingThenClean {
            summary: finding.to_string(),
        },
    )
    .await;
    let mut builder = completion_review_builder();
    let test = builder.build(&server).await?;

    let completion =
        submit_turn_and_capture_completion(&test, "Implement every stated requirement").await?;
    assert_eq!(
        completion.completion.as_ref().map(|gate| gate.status),
        Some(TaskCompletionStatus::Passed),
        "reviews={}, rereviews={}, repairs={}, total_requests={}, gate={:?}",
        probe.review_requests.load(Ordering::SeqCst),
        probe.rereview_requests.load(Ordering::SeqCst),
        probe.repair_requests.load(Ordering::SeqCst),
        probe.total_requests.load(Ordering::SeqCst),
        completion.completion,
    );
    assert_no_additional_turn_complete(&test).await;

    assert_eq!(probe.total_requests.load(Ordering::SeqCst), 13);
    assert_eq!(probe.classification_requests.load(Ordering::SeqCst), 1);
    assert_eq!(probe.review_requests.load(Ordering::SeqCst), 2);
    assert_eq!(probe.rereview_requests.load(Ordering::SeqCst), 1);
    assert_eq!(probe.repair_requests.load(Ordering::SeqCst), 5);
    let repair_payloads = probe.repair_payloads.lock().expect("repair payloads");
    assert!(
        repair_payloads
            .iter()
            .all(|payload| payload.matches(REPAIR_MARKER).count() == 1)
    );
    assert!(repair_payloads[0].contains(finding));
    assert!(
        fs::read_to_string(test.workspace_path("src/lib.rs"))?
            .contains("done with omitted requirement")
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ordinary_nonpassed_evidence_remains_partial_after_supplemental_review() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let probe = mount_completion_review_sequence(
        &server,
        vec![
            plan_response("plan-start", "plan-start-call", "in_progress"),
            patch_response(
                "initial-patch",
                "initial-patch-call",
                "*** Begin Patch\n*** Add File: completed.txt\n+done\n*** End Patch",
            ),
            message_response("candidate", "candidate-message", "implementation complete"),
        ],
        ReviewScenario::Clean,
    )
    .await;
    let mut builder = completion_review_builder();
    let test = builder.build(&server).await?;

    let completion =
        submit_turn_and_capture_completion(&test, "Implement and prove the completion behavior")
            .await?;
    assert_eq!(
        completion.completion.as_ref().map(|gate| gate.status),
        Some(TaskCompletionStatus::Partial),
        "reviews={}, rereviews={}, repairs={}, total_requests={}, gate={:?}",
        probe.review_requests.load(Ordering::SeqCst),
        probe.rereview_requests.load(Ordering::SeqCst),
        probe.repair_requests.load(Ordering::SeqCst),
        probe.total_requests.load(Ordering::SeqCst),
        completion.completion,
    );

    assert_eq!(probe.classification_requests.load(Ordering::SeqCst), 1);
    assert_eq!(probe.review_requests.load(Ordering::SeqCst), 1);
    assert_eq!(probe.rereview_requests.load(Ordering::SeqCst), 0);
    assert_eq!(probe.repair_requests.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unresolved_rereview_stops_after_one_correction() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let finding = "reviewer-specific omitted requirement";
    let probe = mount_completion_review_sequence(
        &server,
        vec![
            plan_response("plan-start", "plan-start-call", "in_progress"),
            patch_response(
                "initial-patch",
                "initial-patch-call",
                "*** Begin Patch\n*** Add File: completed.txt\n+done\n*** End Patch",
            ),
            plan_response("plan-pass", "plan-pass-call", "passed"),
            message_response("candidate", "candidate-message", "implementation complete"),
            plan_response("repair-plan-pass", "repair-plan-pass-call", "passed"),
            message_response("repaired", "repaired-message", "combined repair complete"),
            patch_response(
                "second-repair-patch",
                "second-repair-patch-call",
                "*** Begin Patch\n*** Add File: second-repair.txt\n+done\n*** End Patch",
            ),
            plan_response(
                "second-repair-plan-pass",
                "second-repair-plan-pass-call",
                "passed",
            ),
            message_response(
                "second-repaired",
                "second-repaired-message",
                "second combined repair complete",
            ),
            message_response(
                "third-repaired",
                "third-repaired-message",
                "third combined repair complete",
            ),
        ],
        ReviewScenario::FindingThenUnresolvedThenClean {
            summary: finding.to_string(),
            unresolved_rereviews: 2,
        },
    )
    .await;
    let mut builder = completion_review_builder();
    let test = builder.build(&server).await?;

    let completion = submit_turn_and_capture_completion(
        &test,
        "Implement every requirement and prove completion",
    )
    .await?;
    assert_eq!(
        completion.completion.as_ref().map(|gate| gate.status),
        Some(TaskCompletionStatus::Partial),
        "reviews={}, rereviews={}, repairs={}, total_requests={}, gate={:?}",
        probe.review_requests.load(Ordering::SeqCst),
        probe.rereview_requests.load(Ordering::SeqCst),
        probe.repair_requests.load(Ordering::SeqCst),
        probe.total_requests.load(Ordering::SeqCst),
        completion.completion,
    );
    assert_eq!(probe.review_requests.load(Ordering::SeqCst), 2);
    assert_eq!(probe.rereview_requests.load(Ordering::SeqCst), 1);
    assert_eq!(probe.repair_requests.load(Ordering::SeqCst), 3);
    assert!(
        probe
            .repair_payloads
            .lock()
            .expect("repair payloads")
            .iter()
            .all(|payload| payload.contains(finding))
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_supplemental_reviewer_output_does_not_worsen_completion() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let probe = mount_completion_review_sequence(
        &server,
        vec![
            plan_response("plan-start", "plan-start-call", "in_progress"),
            patch_response(
                "initial-patch",
                "initial-patch-call",
                "*** Begin Patch\n*** Add File: completed.txt\n+done\n*** End Patch",
            ),
            plan_response("plan-pass", "plan-pass-call", "passed"),
            message_response("candidate", "candidate-message", "implementation complete"),
        ],
        ReviewScenario::Malformed,
    )
    .await;
    let mut builder = completion_review_builder();
    let test = builder.build(&server).await?;

    let completion =
        submit_turn_and_capture_completion(&test, "Implement the requested behavior").await?;
    let gate = completion.completion.as_ref().expect("completion report");
    assert_eq!(
        gate.status,
        TaskCompletionStatus::Passed,
        "total_requests={}, classifications={}, reviews={}, gate={gate:?}",
        probe.total_requests.load(Ordering::SeqCst),
        probe.classification_requests.load(Ordering::SeqCst),
        probe.review_requests.load(Ordering::SeqCst),
    );
    assert!(gate.reasons.is_empty());
    assert_eq!(probe.classification_requests.load(Ordering::SeqCst), 1);
    assert_eq!(probe.review_requests.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_supplemental_reviewer_output_does_not_worsen_completion() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let probe = mount_completion_review_sequence(
        &server,
        vec![
            plan_response("plan-start", "plan-start-call", "in_progress"),
            patch_response(
                "initial-patch",
                "initial-patch-call",
                "*** Begin Patch\n*** Add File: completed.txt\n+done\n*** End Patch",
            ),
            plan_response("plan-pass", "plan-pass-call", "passed"),
            message_response("candidate", "candidate-message", "implementation complete"),
        ],
        ReviewScenario::Oversized,
    )
    .await;
    let mut builder = completion_review_builder();
    let test = builder.build(&server).await?;

    let completion =
        submit_turn_and_capture_completion(&test, "Implement the requested behavior").await?;
    let gate = completion.completion.expect("completion report");
    assert_eq!(
        gate.status,
        TaskCompletionStatus::Passed,
        "total_requests={}, classifications={}, reviews={}, gate={gate:?}",
        probe.total_requests.load(Ordering::SeqCst),
        probe.classification_requests.load(Ordering::SeqCst),
        probe.review_requests.load(Ordering::SeqCst),
    );
    assert!(gate.reasons.is_empty());
    assert_eq!(probe.classification_requests.load(Ordering::SeqCst), 1);
    assert_eq!(probe.review_requests.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supplemental_reviewer_spawn_failure_does_not_worsen_completion() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let probe = mount_completion_review_sequence(
        &server,
        vec![
            plan_response("plan-start", "plan-start-call", "in_progress"),
            patch_response(
                "initial-patch",
                "initial-patch-call",
                "*** Begin Patch\n*** Add File: completed.txt\n+done\n*** End Patch",
            ),
            plan_response("plan-pass", "plan-pass-call", "passed"),
            message_response("candidate", "candidate-message", "implementation complete"),
        ],
        ReviewScenario::Clean,
    )
    .await;
    let mut builder = completion_review_builder_with_role(false);
    let test = builder.build(&server).await?;

    let completion =
        submit_turn_and_capture_completion(&test, "Implement the requested behavior").await?;
    let gate = completion.completion.expect("completion report");
    assert_eq!(
        gate.status,
        TaskCompletionStatus::Passed,
        "total_requests={}, classifications={}, reviews={}, gate={gate:?}",
        probe.total_requests.load(Ordering::SeqCst),
        probe.classification_requests.load(Ordering::SeqCst),
        probe.review_requests.load(Ordering::SeqCst),
    );
    assert!(gate.reasons.is_empty());
    assert_eq!(probe.total_requests.load(Ordering::SeqCst), 5);
    assert_eq!(probe.classification_requests.load(Ordering::SeqCst), 0);
    assert_eq!(probe.review_requests.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_stop_exits_before_reviewer_without_a_false_pass() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            plan_response("plan-start", "plan-start-call", "in_progress"),
            patch_response(
                "initial-patch",
                "initial-patch-call",
                "*** Begin Patch\n*** Add File: completed.txt\n+done\n*** End Patch",
            ),
            plan_response("plan-pass", "plan-pass-call", "passed"),
            message_response("candidate", "candidate-message", "implementation complete"),
        ],
    )
    .await;
    let mut builder = completion_review_builder()
        .with_pre_build_hook(write_explicit_stop_hook)
        .with_config(trust_discovered_hooks);
    let test = builder.build(&server).await?;

    let completion =
        submit_turn_and_capture_completion(&test, "Implement the requested behavior").await?;
    assert_eq!(
        completion.completion.as_ref().map(|gate| gate.status),
        Some(TaskCompletionStatus::Passed),
        "unexpected completion gate: {:?}",
        completion.completion
    );
    let requests = response_mock.requests();
    assert_eq!(requests.len(), 5);
    assert_eq!(reviewer_request_count(&requests), 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_hook_continuation_runs_before_the_single_reviewer() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let continuation = "complete the stop-hook-requested follow-up";
    let probe = mount_completion_review_sequence(
        &server,
        vec![
            plan_response("plan-start", "plan-start-call", "in_progress"),
            patch_response(
                "initial-patch",
                "initial-patch-call",
                "*** Begin Patch\n*** Add File: completed.txt\n+done\n*** End Patch",
            ),
            plan_response("plan-pass", "plan-pass-call", "passed"),
            message_response("candidate", "candidate-message", "implementation complete"),
            message_response(
                "stop-continuation",
                "stop-continuation-message",
                "follow-up complete",
            ),
        ],
        ReviewScenario::Clean,
    )
    .await;
    let mut builder = completion_review_builder()
        .with_pre_build_hook(write_single_continuation_stop_hook)
        .with_config(trust_discovered_hooks);
    let test = builder.build(&server).await?;

    let completion =
        submit_turn_and_capture_completion(&test, "Implement the requested behavior").await?;
    assert_eq!(
        completion.completion.as_ref().map(|gate| gate.status),
        Some(TaskCompletionStatus::Passed)
    );
    assert_eq!(probe.total_requests.load(Ordering::SeqCst), 8);
    assert_eq!(probe.classification_requests.load(Ordering::SeqCst), 1);
    assert_eq!(probe.review_requests.load(Ordering::SeqCst), 1);
    assert!(
        probe
            .request_payloads
            .lock()
            .expect("request payloads")
            .iter()
            .any(|payload| payload.contains(continuation))
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reviewer_finishes_before_legacy_after_agent_hook() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let probe = mount_completion_review_sequence(
        &server,
        vec![
            plan_response("plan-start", "plan-start-call", "in_progress"),
            patch_response(
                "initial-patch",
                "initial-patch-call",
                "*** Begin Patch\n*** Add File: completed.txt\n+done\n*** End Patch",
            ),
            plan_response("plan-pass", "plan-pass-call", "passed"),
            message_response("candidate", "candidate-message", "implementation complete"),
        ],
        ReviewScenario::Clean,
    )
    .await;
    let mut builder = completion_review_builder_with_after_agent_probe();
    let test = builder.build(&server).await?;

    let completion =
        submit_turn_and_capture_completion(&test, "Implement the requested behavior").await?;
    assert_eq!(
        completion.completion.as_ref().map(|gate| gate.status),
        Some(TaskCompletionStatus::Passed)
    );
    assert_eq!(probe.classification_requests.load(Ordering::SeqCst), 1);
    assert_eq!(probe.review_requests.load(Ordering::SeqCst), 1);

    let marker = test.home.path().join("after-agent-order.txt");
    for _ in 0..100 {
        if marker.is_file() {
            break;
        }
        sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(fs::read_to_string(marker)?.trim(), "reviewed");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mutating_finalizer_runs_before_clean_completion_review() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let probe = mount_completion_review_sequence(
        &server,
        vec![
            plan_response("plan-start", "plan-start-call", "in_progress"),
            patch_response(
                "initial-patch",
                "initial-patch-call",
                "*** Begin Patch\n*** Add File: completed.txt\n+done\n*** End Patch",
            ),
            plan_response("plan-pass", "plan-pass-call", "passed"),
            message_response("candidate", "candidate-message", "implementation complete"),
        ],
        ReviewScenario::Clean,
    )
    .await;
    let mut builder = completion_review_builder_with_mutating_finalizer(false);
    let test = builder.build(&server).await?;

    let completion =
        submit_turn_and_capture_completion(&test, "Implement the requested behavior").await?;
    assert_eq!(
        completion.completion.as_ref().map(|gate| gate.status),
        Some(TaskCompletionStatus::Passed),
        "unexpected completion gate: {:?}",
        completion.completion
    );
    assert_eq!(probe.review_requests.load(Ordering::SeqCst), 1);
    assert_eq!(
        fs::read_to_string(test.home.path().join("mutating-finalizer-count.txt"))?.trim(),
        "1"
    );
    assert!(test.workspace_path("finalizer.txt").is_file());
    assert!(
        probe
            .request_payloads
            .lock()
            .expect("request payloads")
            .iter()
            .any(|payload| payload.contains(REVIEW_REQUEST_MARKER)
                && payload.contains("finalizer.txt"))
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mutating_finalizer_does_not_rerun_during_review_repair() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let probe = mount_completion_review_sequence(
        &server,
        vec![
            plan_response("plan-start", "plan-start-call", "in_progress"),
            patch_response(
                "initial-patch",
                "initial-patch-call",
                "*** Begin Patch\n*** Add File: completed.txt\n+done\n*** End Patch",
            ),
            plan_response("plan-pass", "plan-pass-call", "passed"),
            message_response("candidate", "candidate-message", "implementation complete"),
            plan_response(
                "repair-plan-start",
                "repair-plan-start-call",
                "in_progress",
            ),
            patch_response(
                "repair-patch",
                "repair-patch-call",
                "*** Begin Patch\n*** Update File: completed.txt\n@@\n-done\n+done after repair\n*** End Patch",
            ),
            plan_response("repair-plan-pass", "repair-plan-pass-call", "passed"),
            message_response("repaired", "repaired-message", "repair complete"),
        ],
        ReviewScenario::FindingThenClean {
            summary: "repair required".to_string(),
        },
    )
    .await;
    let mut builder = completion_review_builder_with_mutating_finalizer(false);
    let test = builder.build(&server).await?;

    let completion =
        submit_turn_and_capture_completion(&test, "Implement the requested behavior").await?;
    assert_eq!(
        completion.completion.as_ref().map(|gate| gate.status),
        Some(TaskCompletionStatus::Passed)
    );
    assert_eq!(probe.review_requests.load(Ordering::SeqCst), 2);
    assert_eq!(probe.rereview_requests.load(Ordering::SeqCst), 1);
    assert_eq!(
        fs::read_to_string(test.home.path().join("mutating-finalizer-count.txt"))?.trim(),
        "1"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mutating_finalizer_abort_is_returned_after_reviewing_its_mutation() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let probe = mount_completion_review_sequence(
        &server,
        vec![
            plan_response("plan-start", "plan-start-call", "in_progress"),
            patch_response(
                "initial-patch",
                "initial-patch-call",
                "*** Begin Patch\n*** Add File: completed.txt\n+done\n*** End Patch",
            ),
            plan_response("plan-pass", "plan-pass-call", "passed"),
            message_response("candidate", "candidate-message", "implementation complete"),
        ],
        ReviewScenario::Clean,
    )
    .await;
    let mut builder = completion_review_builder_with_mutating_finalizer(true);
    let test = builder.build(&server).await?;

    let completion =
        submit_turn_and_capture_completion(&test, "Implement the requested behavior").await?;
    let gate = completion.completion.as_ref().expect("completion gate");
    assert_eq!(gate.status, TaskCompletionStatus::Partial);
    assert!(
        gate.reasons
            .iter()
            .any(|reason| reason == "the reviewed candidate changed during terminal finalization"),
        "unexpected completion gate: {:?}",
        completion.completion
    );
    assert_eq!(probe.review_requests.load(Ordering::SeqCst), 1);
    assert_eq!(
        fs::read_to_string(test.home.path().join("mutating-finalizer-count.txt"))?.trim(),
        "1"
    );
    assert!(
        probe
            .request_payloads
            .lock()
            .expect("request payloads")
            .iter()
            .any(|payload| payload.contains(REVIEW_REQUEST_MARKER)
                && payload.contains("finalizer.txt"))
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabled_feature_skips_review_without_changing_the_evidence_gate() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            plan_response("plan-start", "plan-start-call", "in_progress"),
            patch_response(
                "initial-patch",
                "initial-patch-call",
                "*** Begin Patch\n*** Add File: completed.txt\n+done\n*** End Patch",
            ),
            plan_response("plan-pass", "plan-pass-call", "passed"),
            message_response("candidate", "candidate-message", "implementation complete"),
        ],
    )
    .await;
    let mut builder = completion_review_builder().with_config(|config| {
        config
            .features
            .disable(Feature::TaskCompletionReviewer)
            .expect("disable completion reviewer");
    });
    let test = builder.build(&server).await?;

    let completion =
        submit_turn_and_capture_completion(&test, "Implement the requested behavior").await?;
    assert_eq!(
        completion.completion.as_ref().map(|gate| gate.status),
        Some(TaskCompletionStatus::Passed),
        "unexpected completion gate: {:?}",
        completion.completion
    );
    let requests = response_mock.requests();
    assert_eq!(requests.len(), 5);
    assert_eq!(reviewer_request_count(&requests), 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_only_turn_skips_review() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            plan_response("plan-start", "plan-start-call", "in_progress"),
            plan_response("plan-pass", "plan-pass-call", "passed"),
            message_response("candidate", "candidate-message", "implementation complete"),
        ],
    )
    .await;
    let mut builder = completion_review_builder();
    let test = builder.build(&server).await?;

    submit_turn_and_capture_completion(&test, "Inspect the requested behavior").await?;
    let requests = response_mock.requests();
    assert_eq!(requests.len(), 4);
    assert_eq!(reviewer_request_count(&requests), 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_kd4_repository_skips_review() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            plan_response("plan-start", "plan-start-call", "in_progress"),
            patch_response(
                "initial-patch",
                "initial-patch-call",
                "*** Begin Patch\n*** Add File: completed.txt\n+done\n*** End Patch",
            ),
            plan_response("plan-pass", "plan-pass-call", "passed"),
            message_response("candidate", "candidate-message", "implementation complete"),
        ],
    )
    .await;
    let mut builder = completion_review_builder().with_config(|config| {
        fs::remove_file(config.cwd.join("kd4_features.toml")).expect("remove KD4 marker");
    });
    let test = builder.build(&server).await?;

    submit_turn_and_capture_completion(&test, "Implement the requested behavior").await?;
    let requests = response_mock.requests();
    assert_eq!(requests.len(), 5);
    assert_eq!(reviewer_request_count(&requests), 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plan_mode_does_not_bypass_review() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let probe = mount_completion_review_sequence(
        &server,
        vec![
            plan_response("plan-start", "plan-start-call", "in_progress"),
            patch_response(
                "initial-patch",
                "initial-patch-call",
                "*** Begin Patch\n*** Add File: completed.txt\n+done\n*** End Patch",
            ),
            plan_response("plan-pass", "plan-pass-call", "passed"),
            message_response("candidate", "candidate-message", "implementation complete"),
        ],
        ReviewScenario::Clean,
    )
    .await;
    let mut builder = completion_review_builder();
    let test = builder.build(&server).await?;

    submit_turn_and_capture_completion_with(
        &test,
        "Plan the requested behavior",
        ModeKind::Plan,
        None,
    )
    .await?;
    assert_eq!(probe.review_requests.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn structured_output_turn_does_not_bypass_review() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let probe = mount_completion_review_sequence(
        &server,
        vec![
            plan_response("plan-start", "plan-start-call", "in_progress"),
            patch_response(
                "initial-patch",
                "initial-patch-call",
                "*** Begin Patch\n*** Add File: completed.txt\n+done\n*** End Patch",
            ),
            plan_response("plan-pass", "plan-pass-call", "passed"),
            message_response("candidate", "candidate-message", "{}"),
        ],
        ReviewScenario::Clean,
    )
    .await;
    let mut builder = completion_review_builder();
    let test = builder.build(&server).await?;

    submit_turn_and_capture_completion_with(
        &test,
        "Implement the requested behavior",
        ModeKind::Default,
        Some(json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })),
    )
    .await?;
    assert_eq!(probe.review_requests.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_root_agent_does_not_bypass_review() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let probe = mount_completion_review_sequence(
        &server,
        vec![
            plan_response("plan-start", "plan-start-call", "in_progress"),
            patch_response(
                "initial-patch",
                "initial-patch-call",
                "*** Begin Patch\n*** Add File: completed.txt\n+done\n*** End Patch",
            ),
            plan_response("plan-pass", "plan-pass-call", "passed"),
            message_response("candidate", "candidate-message", "implementation complete"),
        ],
        ReviewScenario::Clean,
    )
    .await;
    let mut builder = completion_review_builder().with_session_source(SessionSource::SubAgent(
        SubAgentSource::Other("completion-review-test".to_string()),
    ));
    let test = builder.build(&server).await?;

    let completion =
        submit_turn_and_capture_completion(&test, "Implement the requested behavior").await?;
    assert_eq!(
        probe.review_requests.load(Ordering::SeqCst),
        1,
        "total={}, classifications={}, relationships={}, gate={:?}",
        probe.total_requests.load(Ordering::SeqCst),
        probe.classification_requests.load(Ordering::SeqCst),
        probe
            .relationship_resolution_requests
            .load(Ordering::SeqCst),
        completion.completion,
    );
    Ok(())
}
