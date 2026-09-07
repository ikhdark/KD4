use anyhow::Context;
use anyhow::Result;
use codex_config::config_toml::AfterAgentPolicy;
use codex_core::CodexThread;
use codex_core::config::Config;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionRegistry;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::ExtensionTurnItem;
use codex_extension_api::FunctionCallError;
use codex_extension_api::JsonToolOutput;
use codex_extension_api::ResponsesApiTool;
use codex_extension_api::ToolCall as ExtensionToolCall;
use codex_extension_api::ToolContributor;
use codex_extension_api::ToolExecutor;
use codex_extension_api::ToolName;
use codex_extension_api::ToolOutput;
use codex_extension_api::ToolSpec;
use codex_extension_items::ExtensionItem;
use codex_extension_items::web_search::WebSearchItem;
use codex_features::Feature;
use codex_protocol::items::AgentMessageContent;
use codex_protocol::items::TurnItem;
use codex_protocol::models::PermissionProfile;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AgentMessageEvent;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InternalSessionSource;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ReviewDecision;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::user_input::UserInput;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_validation_contracts::canonical::MustBeNullV1;
use codex_validation_contracts::canonical::Sha256HexV1;
use codex_validation_contracts::canonical::canonical_jcs_of;
use codex_validation_contracts::canonical::proof_hash;
use codex_validation_contracts::focused_live_successor::FocusedLiveSuccessorCatalogV1;
use codex_validation_contracts::focused_replacement_approval::FocusedReplacementApprovalCurrentContextV1;
use codex_validation_contracts::focused_replacement_approval::FocusedReplacementApprovalReceiptV1;
use codex_validation_contracts::historical_replacement_acceptance::FocusedReplacementApprovalReceiptRefV1;
use codex_validation_contracts::historical_replacement_acceptance::HistoricalReplacementAcceptanceProposalV1;
use codex_validation_contracts::historical_replacement_acceptance::HistoricalReplacementScopeReviewDispositionV1;
use codex_validation_contracts::historical_replacement_acceptance::HistoricalReplacementScopeReviewV1;
use core_test_support::AcceptedCompletionProofFixture;
use core_test_support::CanonicalAttemptMode;
use core_test_support::CanonicalRunnerAttestation;
use core_test_support::FIXTURE_BASELINE_ID;
use core_test_support::FIXTURE_SECOND_BASELINE_ID;
use core_test_support::FIXTURE_SECOND_TEST_ID;
use core_test_support::FIXTURE_SECOND_VALIDATION_ID;
use core_test_support::FIXTURE_TEST_ID;
use core_test_support::FIXTURE_VALIDATION_ID;
use core_test_support::PathBufExt;
use core_test_support::PathExt;
use core_test_support::completion_proof_script;
use core_test_support::focused_completion_proof_script;
use core_test_support::responses::ResponseMock;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_function_call_with_namespace;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_response_once_match;
use core_test_support::responses::mount_sse_once_match;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::sse_response;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::TestCodexHarness;
use core_test_support::test_codex::local_selections;
use core_test_support::test_codex::test_codex;
use core_test_support::test_codex::turn_permission_fields;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;
use std::fs;
use std::future::Future;
#[cfg(windows)]
use std::net::IpAddr;
#[cfg(windows)]
use std::net::Ipv4Addr;
#[cfg(windows)]
use std::net::SocketAddr;
#[cfg(windows)]
use std::net::TcpListener;
#[cfg(windows)]
use std::net::TcpStream;
#[cfg(windows)]
use std::net::UdpSocket;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;

const LEGACY_EXTENSION_TOOL_NAME: &str = "completion_proof_legacy_event";
const LEGACY_EXTENSION_PRIVATE_MESSAGE: &str = "legacy extension assistant output stays private";
const REQUIRED_FAILURE_TOOL_NAME: &str = "completion_proof_required_failure";
const MAX_REGULAR_LOGICAL_GENERATIONS: usize = 32;
const TOTAL_GENERATIONS_WITH_FORCED_TERMINAL: usize = MAX_REGULAR_LOGICAL_GENERATIONS + 1;

#[test]
fn inventory_v2_activation_rejects_public_approval_and_untrusted_policy() -> Result<()> {
    run_session_path_test(
        "inventory_v2_activation_rejects_public_approval_and_untrusted_policy",
        inventory_v2_activation_rejects_public_approval_and_untrusted_policy_impl,
    )
}

async fn inventory_v2_activation_rejects_public_approval_and_untrusted_policy_impl() -> Result<()> {
    let fixture = CompletionProofFixture::new()?;
    let config = fixture
        .repo_path
        .join(".codex/validation/completion-proof.toml");
    let before = fs::read(&config)?;
    let harness = fixture.harness_with_raw_response_items().await?;
    mount_sse_sequence(
        harness.server(),
        vec![
            sse(vec![
                ev_response_created("forged-activation"),
                ev_function_call(
                    "public-activation",
                    "activate_inventory_v2",
                    r#"{"approved":true,"receipt":"self-authored"}"#,
                ),
                ev_completed("forged-activation"),
            ]),
            sse(vec![
                ev_response_created("untrusted-activation"),
                ev_function_call("untrusted-activation", "activate_inventory_v2", "{}"),
                ev_completed("untrusted-activation"),
            ]),
            required_failure_call_response("stop-activation"),
        ],
    )
    .await;
    let events =
        submit_and_collect(harness.test(), "exercise the activation authority boundary").await?;
    assert!(
        function_call_output(&events, "public-activation")
            .context("missing public approval rejection")?
            .contains("accepts no caller-authored")
    );
    assert!(
        function_call_output(&events, "untrusted-activation")
            .context("missing policy rejection")?
            .contains("requires the compiled KD4 authority")
    );
    assert_eq!(fs::read(config)?, before);
    assert!(
        !fixture
            .repo_path
            .join(".codex/validation/inventory-generations")
            .exists()
    );
    assert!(!fixture.marker_path.exists());
    Ok(())
}

const QUALITY_TEST_ID: &str = "__main__.PriceContract.test_discount";
const QUALITY_TEST_BODY: &str = r#"    def test_discount(self):
        result = subprocess.run([sys.executable, '-B', 'src/product.py', '100'], capture_output=True, text=True, check=True)
        with Path('.fixture-state/quality-executions').open('a') as marker:
            marker.write(json.dumps({'execution': str(uuid.uuid4()), 'actual': result.stdout.strip()}) + '\n')
        self.assertEqual(result.stdout.strip(), '95')
"#;

const QUALITY_RUST_TEST_BODY: &str = r#"#[test]
fn test_discount() {
    use std::io::Write;
    let result = std::process::Command::new("python")
        .args(["-B", "src/product.py", "100"]).output().unwrap();
    assert!(result.status.success());
    let actual = String::from_utf8(result.stdout).unwrap();
    let mut marker = std::fs::OpenOptions::new().create(true).append(true)
        .open(".fixture-state/quality-executions").unwrap();
    let execution = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    writeln!(marker, "{{\"execution\":\"{execution}\",\"actual\":\"{}\"}}", actual.trim()).unwrap();
    assert_eq!(actual.trim(), "95");
}
"#;

fn install_rust_quality_test(fixture: &CompletionProofFixture) -> Result<()> {
    fs::remove_file(fixture.repo_path.join("src/test_behavior.py"))?;
    let neighbor = r#"#[cfg(test)]
mod stable_neighbor {
    fn invalid_price_is_rejected() -> bool {
        !std::process::Command::new("python")
            .args(["-B", "src/product.py", "invalid"])
            .output().unwrap().status.success()
    }
    #[test]
    fn unchanged_crlf_neighbor() {
        assert!(invalid_price_is_rejected());
    }
}
"#;
    let current = format!("{QUALITY_RUST_TEST_BODY}\n{neighbor}");
    fs::write(fixture.repo_path.join("src/test_behavior.rs"), &current)?;
    let path = fixture.repo_path.join("focused.py");
    let mut script = fs::read_to_string(&path)?;
    let needle = "launch_target_identity = start_identity(sys.executable)";
    let replacement = r#"if VALIDATION_ID != 'fixture.other':
    build = subprocess.run(['rustc', '--edition=2024', '--test', 'src/test_behavior.rs', '-o', '.fixture-state/quality-test.exe'], capture_output=True)
    if build.returncode:
        sys.stderr.buffer.write(build.stderr)
        raise SystemExit(build.returncode)
launch_target_identity = start_identity(sys.executable if VALIDATION_ID == 'fixture.other' else '.fixture-state/quality-test.exe')"#;
    anyhow::ensure!(
        script.matches(needle).count() == 1,
        "missing Rust fixture compile boundary"
    );
    script = script.replace(needle, replacement);
    let start = script
        .find("command = [")
        .context("missing native fixture invocation")?;
    let end = start
        + script[start..]
            .find("\nstarted_at =")
            .context("missing native fixture timing boundary")?;
    script.replace_range(start..end, "command = [launch_target_identity['resolved_path'], '-B', 'src/other/test_product.py'] if VALIDATION_ID == 'fixture.other' else [launch_target_identity['resolved_path'], '--exact', 'test_discount', '--nocapture']");
    script = script.replace(QUALITY_TEST_ID, "test_discount");
    fs::write(path, script)?;
    run_git(&fixture.repo_path, &["add", "focused.py"])?;
    run_git(
        &fixture.repo_path,
        &["commit", "--quiet", "--amend", "--no-edit"],
    )?;
    install_mixed_eol_quality_baseline(
        fixture,
        "src/test_behavior.rs",
        &current.replace(
            "assert_eq!(actual.trim(), \"95\")",
            "assert_eq!(actual.trim(), \"100\")",
        ),
        &current,
    )?;
    Ok(())
}

fn install_mixed_eol_quality_baseline(
    fixture: &CompletionProofFixture,
    relative: &str,
    baseline: &str,
    current: &str,
) -> Result<()> {
    assert_ne!(
        baseline, current,
        "fixture needs a substantive assertion change"
    );
    run_git(&fixture.repo_path, &["config", "core.autocrlf", "false"])?;
    let baseline = baseline.replace("\r\n", "\n").replace('\n', "\r\n");
    let path = fixture.repo_path.join(relative);
    fs::write(&path, &baseline)?;
    run_git(&fixture.repo_path, &["add", relative])?;
    run_git(
        &fixture.repo_path,
        &["commit", "--quiet", "--amend", "--no-edit"],
    )?;
    let stored = Command::new("git")
        .args(["show", &format!("HEAD:{relative}")])
        .current_dir(&fixture.repo_path)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()?;
    anyhow::ensure!(stored.status.success(), "read mixed-EOL fixture baseline");
    assert_eq!(stored.stdout, baseline.as_bytes());
    fs::write(path, current)?;
    Ok(())
}

fn install_quality_behavior_fixture(
    fixture: &CompletionProofFixture,
    weak: bool,
) -> Result<String> {
    fs::write(
        fixture.repo_path.join("src/runtime.rs"),
        "pub const VALUE: u8 = 1;\n",
    )?;
    let focused_path = fixture.repo_path.join("focused.py");
    let mut script = fs::read_to_string(&focused_path)?;
    script = script.replace(
        "    \"-c\",\n    \"raise SystemExit(7)\" if failure else \"raise SystemExit(0)\",",
        "    \"-B\",\n    \"src/test_behavior.py\",",
    );
    script = script.replace(
        "child.communicate()",
        "child_stdout, child_stderr = child.communicate()\nfailure = child.returncode != 0",
    );
    script = script.replace(
        "TEST_ID = \"fixture.test\"",
        &format!("TEST_ID = {QUALITY_TEST_ID:?}"),
    );
    script = script.replace(
        "\"fixture confirmed validation failure\" if failure else \"\"",
        "child_stderr.decode('utf-8', errors='replace')",
    );
    let inventory_path = fixture
        .repo_path
        .join(".codex/validation/frozen-test-inventory-v1.json");
    let inventory: serde_json::Value = serde_json::from_str(&fs::read_to_string(&inventory_path)?)?;
    script = script.replace("inventory = json.loads(\n    Path(\".codex/validation/frozen-test-inventory-v1.json\").read_text(encoding=\"utf-8\")\n)", &format!("inventory = {{'inventory_hash': {}}}", inventory["inventory_hash"]));
    script = script.replace("VALIDATION_ID = \"fixture.validation\"", "VALIDATION_ID = sys.argv[1]\nEXACT_COMMAND = EXACT_COMMAND.replace('fixture.validation', VALIDATION_ID)");
    script = script.replace(&format!("TEST_ID = {QUALITY_TEST_ID:?}"), &format!("TEST_ID = '__main__.OtherContract.test_value' if VALIDATION_ID == 'fixture.other' else {QUALITY_TEST_ID:?}"));
    script = script.replace("    \"src/test_behavior.py\",", "    \"src/other/test_product.py\" if VALIDATION_ID == 'fixture.other' else \"src/test_behavior.py\",");
    script = script.replace("\"runner_selector\": \"fixture-gate\"", "\"runner_selector\": 'fixture-other-gate' if VALIDATION_ID == 'fixture.other' else 'fixture-gate'");
    let config_path = fixture
        .repo_path
        .join(".codex/validation/completion-proof.toml");
    let config = fs::read_to_string(&config_path)?;
    fs::write(
        &config_path,
        format!(
            "{config}\n[[validation]]\nid = \"fixture.other\"\nrunner = \"rust-gate\"\ngate = \"fixture-other-gate\"\nowned_paths = [\"src/other/**\"]\nconsumed_paths = [\"src/other/**\"]\ntimeout_seconds = 30\n"
        ),
    )?;
    fs::create_dir_all(fixture.repo_path.join("src/other"))?;
    fs::write(
        fixture.repo_path.join("src/other/product.py"),
        "print('baseline')\n",
    )?;
    fs::write(
        fixture.repo_path.join("src/other/test_product.py"),
        "import subprocess\nimport sys\nimport unittest\nfrom pathlib import Path\nclass OtherContract(unittest.TestCase):\n    def test_value(self):\n        result = subprocess.run([sys.executable, '-B', 'src/other/product.py'], capture_output=True, text=True, check=True)\n        with Path('.fixture-state/other-executions').open('a') as marker:\n            marker.write(result.stdout)\n        self.assertEqual(result.stdout.strip(), 'updated')\nif __name__ == '__main__':\n    unittest.main()\n",
    )?;
    fs::write(
        fixture.repo_path.join("change_other.py"),
        "from pathlib import Path\nPath('src/other/product.py').write_text(\"print('updated')\\n\")\n",
    )?;
    fs::write(focused_path, script)?;
    // This ordinary future task has no migration inventory or replacement ledger.
    fs::remove_file(inventory_path)?;
    fs::remove_file(
        fixture
            .repo_path
            .join(".codex/validation/test-replacements-v1.json"),
    )?;
    fs::write(
        fixture.repo_path.join("src/product.py"),
        "import sys\nprint(int(sys.argv[1]))\n",
    )?;
    fs::write(
        fixture.repo_path.join("fix_price.py"),
        "from pathlib import Path\nPath('src/product.py').write_bytes(b'import sys\\nprint(int(sys.argv[1]) - 5)\\n')\n",
    )?;
    run_git(
        &fixture.repo_path,
        &[
            "add",
            "focused.py",
            "src/product.py",
            "src/other",
            "change_other.py",
            "fix_price.py",
            ".codex/validation",
        ],
    )?;
    run_git(
        &fixture.repo_path,
        &["commit", "--quiet", "--amend", "--no-edit"],
    )?;
    let body = if weak {
        QUALITY_TEST_BODY.replace(
            "self.assertEqual(result.stdout.strip(), '95')",
            "self.assertTrue(result.stdout.strip())",
        )
    } else {
        QUALITY_TEST_BODY.to_owned()
    };
    fs::write(
        fixture.repo_path.join("src/test_behavior.py"),
        format!(
            "import json\nimport subprocess\nimport sys\nimport unittest\nimport uuid\nfrom pathlib import Path\n\nclass PriceContract(unittest.TestCase):\n{body}\nif __name__ == '__main__':\n    unittest.main()\n"
        ),
    )?;
    Ok(format!("{} fix_price.py", available_python_command()?))
}

#[test]
fn test_quality_weak_test_pass_cannot_complete_ordinary_task() -> Result<()> {
    run_session_path_test(
        "test_quality_weak_test_pass_cannot_complete_ordinary_task",
        || async {
            let fixture = CompletionProofFixture::new()?;
            install_quality_behavior_fixture(&fixture, true)?;
            let harness = fixture.harness_with_raw_response_items().await?;
            let mut responses = vec![
                exec_command_call_response(
                    "weak-test-passes-broken-cli",
                    &fixture.exact_focused_command(),
                    &fixture.repo_path,
                ),
                sse(vec![
                    ev_response_created("quality-review-request"),
                    ev_function_call("quality-review", "review_test_quality", "{}"),
                    ev_completed("quality-review-request"),
                ]),
            ];
            responses.extend(repeated_terminal_candidates(
                MAX_REGULAR_LOGICAL_GENERATIONS - 1,
                "the weak test passed so the task is complete",
            ));
            mount_sse_sequence(harness.server(), responses).await;
            let events = submit_and_collect(harness.test(), "Make the price CLI subtract five: input 100 must output 95. Add a regression test and validate this small change.").await?;
            assert!(events.iter().any(|e| matches!(e, EventMsg::ExecCommandEnd(end) if end.call_id == "weak-test-passes-broken-cli" && end.exit_code == 0)));
            assert_eq!(
                function_call_output_success(&events, "quality-review"),
                Some(false)
            );
            assert!(
                function_call_output(&events, "quality-review")
                    .context("missing quality rejection")?
                    .contains("No trusted failing execution")
            );
            assert!(events.iter().all(|event| !is_assistant_output(event)));
            assert!(events.iter().any(|event| matches!(event, EventMsg::TurnComplete(done) if done.error.is_some() && done.last_agent_message.is_none())));
            assert!(!fixture.marker_path.exists());
            let executions =
                fs::read_to_string(fixture.repo_path.join(".fixture-state/quality-executions"))?;
            assert_eq!(executions.lines().count(), 1);
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(executions.trim())?["actual"],
                "100"
            );
            Ok(())
        },
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum QualityScenario {
    Accepted,
    RustAccepted,
    Recheck,
    ChangedAssertions,
    OmittedTest,
    ForgedAttempt,
    PinnedProduct,
    PartialBatch,
    Retired,
    RetirementWithoutAuthority,
    OmittedRetirement,
    AlteredRetirement,
    RetirementWithoutReplacement,
}

impl QualityScenario {
    fn reviews_retirement(self) -> bool {
        matches!(
            self,
            Self::Retired
                | Self::RetirementWithoutAuthority
                | Self::OmittedRetirement
                | Self::AlteredRetirement
                | Self::RetirementWithoutReplacement
        )
    }
}

const QUALITY_RETIREMENT_REQUEST: &str = "Retire the old identity-price behavior and its tests; the price CLI must subtract five instead.";
const QUALITY_RETIRED_BODY: &str = "    def test_legacy_identity(self):\n        result = subprocess.run([sys.executable, '-B', 'src/product.py', '100'], capture_output=True, text=True, check=True)\n        self.assertEqual(result.stdout.strip(), '100')\n";

#[test]
fn test_quality_retired_tests_require_authorized_behavior_replacement() -> Result<()> {
    run_session_path_test(
        "test_quality_retired_tests_require_authorized_behavior_replacement",
        || async {
            let mut errors = Vec::new();
            for scenario in [
                QualityScenario::Retired,
                QualityScenario::RetirementWithoutAuthority,
                QualityScenario::OmittedRetirement,
                QualityScenario::AlteredRetirement,
                QualityScenario::RetirementWithoutReplacement,
            ] {
                if let Err(error) = run_quality_scenario(scenario).await {
                    errors.push(format!("{scenario:?}: {error:#}"));
                }
            }
            anyhow::ensure!(errors.is_empty(), "{}", errors.join("\n"));
            Ok(())
        },
    )
}

#[test]
fn test_quality_reviews_pinned_product_and_keeps_unreviewed_batch_blocked() -> Result<()> {
    run_session_path_test(
        "test_quality_reviews_pinned_product_and_keeps_unreviewed_batch_blocked",
        || async {
            run_quality_scenario(QualityScenario::PinnedProduct).await?;
            run_quality_scenario(QualityScenario::PartialBatch).await
        },
    )
}

#[test]
fn test_quality_observed_defect_and_independent_review_complete_ordinary_task() -> Result<()> {
    run_session_path_test(
        "test_quality_observed_defect_and_independent_review_complete_ordinary_task",
        || run_quality_scenario(QualityScenario::Accepted),
    )
}

#[test]
fn test_quality_native_rust_assertion_protects_real_cli_behavior() -> Result<()> {
    run_session_path_test(
        "test_quality_native_rust_assertion_protects_real_cli_behavior",
        || run_quality_scenario(QualityScenario::RustAccepted),
    )
}

#[test]
fn test_quality_rejects_changed_assertions_between_failed_and_passing_runs() -> Result<()> {
    run_session_path_test(
        "test_quality_rejects_changed_assertions_between_failed_and_passing_runs",
        || run_quality_scenario(QualityScenario::ChangedAssertions),
    )
}

#[test]
fn test_quality_rejects_review_that_omits_another_changed_test() -> Result<()> {
    run_session_path_test(
        "test_quality_rejects_review_that_omits_another_changed_test",
        || run_quality_scenario(QualityScenario::OmittedTest),
    )
}

#[test]
fn test_quality_rejects_reviewer_invented_execution() -> Result<()> {
    run_session_path_test("test_quality_rejects_reviewer_invented_execution", || {
        run_quality_scenario(QualityScenario::ForgedAttempt)
    })
}

fn quality_review_result(
    request: &wiremock::Request,
    scenario: QualityScenario,
    test_path: &str,
    test_body: &str,
    test_id: &str,
) -> serde_json::Value {
    let body: serde_json::Value =
        serde_json::from_slice(&request.body).expect("review request JSON");
    assert!(
        body["instructions"]
            .as_str()
            .is_some_and(|s| s.contains("independently review test QUALITY"))
    );
    let format = &body["text"]["format"];
    assert_eq!(
        format["type"], "json_schema",
        "the real reviewer request must constrain its result"
    );
    assert_eq!(format["strict"], true);
    let declaration_schema =
        &format["schema"]["properties"]["retirements"]["items"]["properties"]["declaration"];
    assert_eq!(
        declaration_schema["type"], "object",
        "a retirement signature string is not a declaration"
    );
    assert_eq!(declaration_schema["additionalProperties"], false);
    for field in ["name", "body", "context"] {
        assert_eq!(declaration_schema["properties"][field]["type"], "string");
        assert!(
            declaration_schema["required"]
                .as_array()
                .unwrap()
                .contains(&json!(field))
        );
    }
    let packet = body["input"]
        .as_array()
        .expect("review input")
        .iter()
        .flat_map(|i| i["content"].as_array().into_iter().flatten())
        .filter_map(|c| c["text"].as_str())
        .filter_map(|t| serde_json::from_str::<serde_json::Value>(t).ok())
        .find(|v| v.get("changed_test_paths").is_some())
        .expect("private quality packet");
    assert!(
        packet["user_requirements"]
            .as_str()
            .expect("requirements")
            .contains("input 100 must output 95")
    );
    assert_eq!(
        packet["failing_executions"][FIXTURE_VALIDATION_ID]["report"]["outcomes"][0]["outcome"],
        "failed"
    );
    assert_eq!(
        packet["passing_executions"]
            .as_object()
            .expect("retained passing executions")
            .values()
            .find(|execution| execution["report"]["id"] == FIXTURE_VALIDATION_ID)
            .expect("a prior actual pass must remain visible, even when revoked")["report"]["outcomes"]
            [0]["outcome"],
        "passed"
    );
    if matches!(
        scenario,
        QualityScenario::Accepted | QualityScenario::RustAccepted
    ) {
        let required = packet["required_test_declarations"][test_path]
            .as_array()
            .expect("current test declarations");
        assert_eq!(
            required.len(),
            1,
            "unchanged CRLF neighbors are not new obligations"
        );
        assert_eq!(required[0]["name"], "test_discount");
        assert_eq!(
            required[0]["body"], test_body,
            "review still receives the exact current body"
        );
        let comparison = packet["comparison_index"]
            .as_array()
            .expect("runtime comparison navigation")
            .iter()
            .find(|entry| entry["test_id"] == test_id)
            .expect("current test comparison");
        assert!(
            comparison["current_passing_attempt_ids"]
                .as_array()
                .unwrap()
                .iter()
                .any(|id| {
                    packet["passing_executions"]
                        .as_object()
                        .unwrap()
                        .values()
                        .any(|pass| &pass["attempt_id"] == id)
                })
        );
        assert!(
            comparison["unchanged_failing_attempt_ids"]
                .as_array()
                .unwrap()
                .contains(&packet["failing_executions"][FIXTURE_VALIDATION_ID]["attempt_id"])
        );
    }
    if scenario == QualityScenario::ChangedAssertions {
        assert!(
            packet["comparison_index"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|entry| entry["test_id"] == test_id)
                .all(|entry| entry["unchanged_failing_attempt_ids"]
                    .as_array()
                    .unwrap()
                    .is_empty()),
            "a failed different assertion must not be suggested as an unchanged comparison"
        );
    }
    let mut review = json!({"approved":true,"explanation":"The actual CLI returns 100 in the broken run and 95 after the product correction. The same assertion checks the explicit user's 95 contract.",
                        "evaluated_test_paths":[test_path],"input_paths":[test_path,"src/product.py"],"retirements":[],"obligations":[{
                            "validation_id":FIXTURE_VALIDATION_ID,"test_id":test_id,"test_path":test_path,"test_body":test_body,
                            "oracle_source":"Current user request: input 100 must output 95","expected_behavior":"subtract five from the CLI input","runtime_path":"python src/product.py 100 subprocess stdout",
                            "defect_explanation":"The product returned its input without applying the five-unit discount; the unchanged assertion failed on 100 and passed on 95.",
                            "failed_attempt_id":if scenario == QualityScenario::ForgedAttempt { json!("author-invented-attempt") } else { packet["failing_executions"][FIXTURE_VALIDATION_ID]["attempt_id"].clone() },"product_paths":["src/product.py"]}]});
    if scenario.reviews_retirement() {
        let retired = packet["retired_test_declarations"]
            .as_object()
            .expect("runtime-derived retired declarations");
        assert_eq!(
            retired.len(),
            2,
            "both rename and deleted file stay obligations"
        );
        let mut retirements = Vec::new();
        for (path, declarations) in retired {
            assert!(matches!(
                path.as_str(),
                "src/test_behavior.py" | "src/retired_test.py"
            ));
            let declarations = declarations.as_array().expect("retired tests");
            assert_eq!(declarations.len(), 1);
            let declaration = &declarations[0];
            assert_eq!(declaration["name"], "test_legacy_identity");
            assert_eq!(declaration["body"], QUALITY_RETIRED_BODY);
            retirements.push(json!({
                "test_path":path,"declaration":declaration,
                "authorization_quote":QUALITY_RETIREMENT_REQUEST,
                "explanation":"The user retired identity pricing. The unchanged subprocess assertion now requires the five-unit discount and detects the old identity behavior.",
                "replacement_validation_id":FIXTURE_VALIDATION_ID,"replacement_test_id":test_id
            }));
        }
        match scenario {
            QualityScenario::RetirementWithoutAuthority => {
                retirements[0]["authorization_quote"] =
                    json!("Invented permission to delete every test");
            }
            QualityScenario::OmittedRetirement => {
                retirements.pop();
            }
            QualityScenario::AlteredRetirement => {
                retirements[0]["declaration"]["body"] =
                    json!("    def test_legacy_identity(self): pass\n");
            }
            QualityScenario::RetirementWithoutReplacement => {
                retirements[0]["replacement_test_id"] = json!("unexecuted.replacement");
            }
            _ => {}
        }
        review["evaluated_test_paths"] = json!([test_path, "src/retired_test.py"]);
        review["input_paths"] = json!([test_path, "src/product.py", "src/retired_test.py"]);
        review["retirements"] = json!(retirements);
    }
    review
}

#[test]
fn test_quality_rechecks_unchanged_test_after_later_regression() -> Result<()> {
    run_session_path_test(
        "test_quality_rechecks_unchanged_test_after_later_regression",
        || run_quality_scenario(QualityScenario::Recheck),
    )
}

fn quality_fixture_yielded_process(request: &wiremock::Request) -> Option<u32> {
    let body: serde_json::Value = serde_json::from_slice(&request.body).ok()?;
    // Older results stay in the request history. Only the latest result can
    // describe the command that this model response must finish collecting.
    let result = body["input"]
        .as_array()?
        .iter()
        .rev()
        .find(|item| item["type"] == "function_call_output")?;
    let tail = result["output"]
        .as_str()?
        .strip_prefix("Process running with session ID ")?;
    tail.split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()
}

#[test]
fn test_quality_accepts_batched_corrections_and_rejects_uncovered_versions() -> Result<()> {
    run_session_path_test(
        "test_quality_accepts_batched_corrections_and_rejects_uncovered_versions",
        || async {
            use futures::FutureExt;
            let mut errors = Vec::new();
            for (review_other, mismatched_before) in [(true, false), (false, false), (true, true)] {
                let result = std::panic::AssertUnwindSafe(run_quality_batch_scenario(
                    review_other,
                    mismatched_before,
                    false,
                ))
                .catch_unwind()
                .await;
                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => errors.push(format!(
                        "review_other={review_other}, mismatched_before={mismatched_before}: {error:#}"
                    )),
                    Err(_) => errors.push(format!(
                        "review_other={review_other}, mismatched_before={mismatched_before}: assertion failed"
                    )),
                }
            }
            anyhow::ensure!(errors.is_empty(), "{}", errors.join("\n"));
            Ok(())
        },
    )
}

#[test]
fn test_quality_reaches_review_with_hash_bound_oversized_workspace_diff() -> Result<()> {
    run_session_path_test(
        "test_quality_reaches_review_with_hash_bound_oversized_workspace_diff",
        || async { run_quality_batch_scenario(true, false, true).await },
    )
}

async fn run_quality_batch_scenario(
    review_other: bool,
    mismatched_before: bool,
    oversized_diff: bool,
) -> Result<()> {
    use base64::Engine;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    let fixture = CompletionProofFixture::new()?;
    install_quality_behavior_fixture(&fixture, false)?;
    if oversized_diff {
        let path = fixture.repo_path.join("src/test_behavior.py");
        let source = fs::read_to_string(&path)?;
        fs::write(
            path,
            format!(
                "import sys\nsys.stderr.write('DIAGNOSTIC-BEGIN\\n' + 'padding\\n' * 1600 + 'DIAGNOSTIC-END\\n')\n{source}"
            ),
        )?;
    }
    let other_path = "src/other/test_product.py";
    let other_source = fs::read_to_string(fixture.repo_path.join(other_path))?;
    fs::write(
        fixture.repo_path.join(other_path),
        other_source.replace("'updated'", "'baseline'"),
    )?;
    fs::write(
        fixture.repo_path.join("fix_both.py"),
        "import runpy\nrunpy.run_path('fix_price.py')\nrunpy.run_path('change_other.py')\n",
    )?;
    fs::write(
        fixture.repo_path.join("change_intermediate.py"),
        "from pathlib import Path\nPath('src/other/product.py').write_text(\"print('intermediate')\\n\")\n",
    )?;
    let generated_path = "src/generated/catalog.json";
    let review_input_paths = (0..129)
        .map(|index| format!("src/generated/review-input-{index}.json"))
        .collect::<Vec<_>>();
    if oversized_diff {
        fs::create_dir_all(fixture.repo_path.join("src/generated"))?;
        fs::write(
            fixture.repo_path.join(generated_path),
            "{\"begin\":\"BASELINE-BEGIN\",\"data\":\"\",\"end\":\"BASELINE-END\"}\n",
        )?;
        run_git(&fixture.repo_path, &["add", generated_path])?;
    }
    run_git(
        &fixture.repo_path,
        &["add", other_path, "fix_both.py", "change_intermediate.py"],
    )?;
    run_git(
        &fixture.repo_path,
        &["commit", "--quiet", "--amend", "--no-edit"],
    )?;
    fs::write(fixture.repo_path.join(other_path), &other_source)?;
    if oversized_diff {
        // Escaping makes the serialized diff exceed the review's wire limit.
        // The source itself remains small enough for ordinary input capture.
        let generated = serde_json::json!({
            "begin": "OVERSIZED-DIFF-BEGIN",
            "data": "\"".repeat(700 * 1024),
            "end": "OVERSIZED-DIFF-END",
        })
        .to_string()
            + "\n";
        assert!(generated.len() < 2 * 1024 * 1024);
        assert!(serde_json::to_string(&generated)?.len() > 2 * 1024 * 1024);
        let parsed: serde_json::Value = serde_json::from_str(&generated)?;
        assert_eq!(parsed["begin"], "OVERSIZED-DIFF-BEGIN");
        assert_eq!(parsed["end"], "OVERSIZED-DIFF-END");
        fs::write(fixture.repo_path.join(generated_path), generated)?;
        // A real review can reference more artifacts than the inactive-output
        // retention count. Capture distinct inputs through the ordinary runner
        // before asking the child to read its earliest diff/source artifacts.
        for (index, path) in review_input_paths.iter().enumerate() {
            fs::write(
                fixture.repo_path.join(path),
                format!("{{\"input\":{index}}}\n"),
            )?;
        }
        // Introduce the faulty discount after the baseline commit so the real
        // execution captures this changed product's historical source. The
        // unchanged assertion must reject zero and accept the later five.
        fs::write(
            fixture.repo_path.join("src/product.py"),
            "import sys\nprint(int(sys.argv[1]) - 0)\n",
        )?;
    }
    let price_source = fs::read(fixture.repo_path.join("src/test_behavior.py"))?;
    let historical_product_source = fs::read(fixture.repo_path.join("src/product.py"))?;
    let corrected_product_source = b"import sys\nprint(int(sys.argv[1]) - 5)\n".to_vec();
    assert_ne!(historical_product_source, corrected_product_source);
    let harness = fixture.harness_with_raw_response_items().await?;
    let reviews = Arc::new(AtomicUsize::new(0));
    let observed_reviews = Arc::clone(&reviews);
    let review_repo = fixture.repo_path.clone();
    let expected_historical_product_source = historical_product_source.clone();
    let expected_corrected_product_source = corrected_product_source.clone();
    let boundary_errors = Arc::new(Mutex::new(Vec::<String>::new()));
    let review_errors = Arc::clone(&boundary_errors);
    let schema_errors = Arc::clone(&boundary_errors);
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path_regex(".*/responses"))
        .and(|request: &wiremock::Request| {
            serde_json::from_slice::<serde_json::Value>(&request.body)
                .ok()
                .is_some_and(|body| {
                    body["instructions"]
                        .as_str()
                        .is_some_and(|text| text.contains("independently review test QUALITY"))
                })
        })
        .respond_with(move |request: &wiremock::Request| {
            let review_step = observed_reviews.fetch_add(1, Ordering::SeqCst);
            let mut review = quality_review_result(
                request,
                QualityScenario::Accepted,
                "src/test_behavior.py",
                QUALITY_TEST_BODY,
                QUALITY_TEST_ID,
            );
            if review_other {
                let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                let packet = body["input"]
                    .as_array().unwrap().iter()
                    .flat_map(|item| item["content"].as_array().into_iter().flatten())
                    .filter_map(|item| item["text"].as_str())
                    .filter_map(|text| serde_json::from_str::<serde_json::Value>(text).ok())
                    .find(|value| value.get("changed_test_paths").is_some()).unwrap();
                if oversized_diff {
                    let reference = &packet["workspace_diff"];
                    // The reviewer must read the bytes through its own tool
                    // capability, not through a Command run by this mock server.
                    let inspected = (|| -> Result<()> {
                        anyhow::ensure!(reference["kind"] == "tool-output-reference-v1", "oversized diff has no reviewer-readable artifact");
                        anyhow::ensure!(reference["repository_root"].as_str() == Some(review_repo.to_string_lossy().as_ref()), "diff repository changed");
                        anyhow::ensure!(reference["workspace_fingerprint"].as_str().is_some_and(|value| value.len() == 64), "missing workspace binding");
                        let bytes = reference["byte_length"].as_u64().context("missing diff length")?;
                        anyhow::ensure!(bytes > 4096, "fixture did not produce an oversized diff");
                        if review_step == 0 {
                            anyhow::ensure!(packet["source_blobs"].as_object().context("source blobs")?.len() > 128, "fixture did not cross inactive artifact retention");
                            return Ok(());
                        }
                        let output = body["input"].as_array().context("review inputs")?.iter()
                            .find(|item| item["type"] == "function_call_output" && item["call_id"] == "review-read-diff")
                            .context("reviewer's actual read result was not returned")?;
                        let encoded = match &output["output"] {
                            serde_json::Value::String(text) => text.clone(),
                            serde_json::Value::Array(items) => items.iter().filter_map(|item| item["text"].as_str()).collect::<Vec<_>>().join("\n"),
                            _ => anyhow::bail!("review read did not return text"),
                        };
                        let read: serde_json::Value = serde_json::from_str(&encoded)
                            .with_context(|| format!("reviewer's diff artifact read failed: {encoded}"))?;
                        anyhow::ensure!(read["artifact_id"] == reference["artifact_id"] && read["canonical_sha256"] == reference["sha256"] && read["canonical_bytes"] == reference["byte_length"], "review read did not verify the exact diff bytes");
                        anyhow::ensure!(read["complete"] == true && (read.get("unavailable_ranges").is_none() || read["unavailable_ranges"] == json!([])), "review diff read was incomplete");
                        let mut excerpts = Vec::new();
                        for selected in read["results"].as_array().context("review read selections")? {
                            anyhow::ensure!(selected["status"] == "ok" && selected["complete"] == true, "review selection failed");
                            if let Some(text) = selected["text"].as_str() {
                                excerpts.extend_from_slice(text.as_bytes());
                            } else {
                                excerpts.extend(base64::engine::general_purpose::STANDARD.decode(selected["data_base64"].as_str().context("exact diff bytes")?)?);
                            }
                        }
                        let excerpts = String::from_utf8(excerpts)?;
                        anyhow::ensure!(excerpts.contains("OVERSIZED-DIFF-BEGIN") && excerpts.contains("OVERSIZED-DIFF-END"), "reviewer could not read both ends of the actual diff");
                        if review_step >= 2 {
                            let source = packet["source_blobs"].as_object().context("source blobs")?.values()
                                .find(|source| source["repository_path"] == "src/test_behavior.py")
                                .context("current test source reference")?;
                            let output = body["input"].as_array().context("review inputs")?.iter()
                                .find(|item| item["type"] == "function_call_output" && item["call_id"] == "review-read-source")
                                .context("reviewer's source read result was not returned")?;
                            let encoded = match &output["output"] {
                                serde_json::Value::String(text) => text.clone(),
                                serde_json::Value::Array(items) => items.iter().filter_map(|item| item["text"].as_str()).collect::<Vec<_>>().join("\n"),
                                _ => anyhow::bail!("source read did not return text"),
                            };
                            let read: serde_json::Value = serde_json::from_str(&encoded)?;
                            anyhow::ensure!(read["artifact_id"] == source["artifact_id"] && read["canonical_sha256"] == source["sha256"] && read["canonical_bytes"] == source["byte_length"] && read["complete"] == true, "review source read did not verify exact bytes");
                            anyhow::ensure!(read["results"][0]["text"].as_str().is_some_and(|text| text.contains(QUALITY_TEST_BODY)), "reviewer could not inspect the unchanged assertion body");
                        }
                        if review_step >= 3 {
                            let source_ref = packet["failing_executions"][FIXTURE_VALIDATION_ID]
                                ["inputs"]["sources"]["src/product.py"]["source_ref"]
                                .as_str()
                                .context("historical product source reference")?;
                            let source = &packet["source_blobs"][source_ref];
                            anyhow::ensure!(source["snapshot_path"] == "src/product.py", "historical source lost its snapshot path");
                            anyhow::ensure!(source.get("repository_path").is_none() && source.get("text").is_none() && source.get("base_source").is_none() && source.get("unified_diff").is_none(), "historical source was not represented only by its reviewer-readable snapshot artifact");
                            let output = body["input"].as_array().context("review inputs")?.iter()
                                .find(|item| item["type"] == "function_call_output" && item["call_id"] == "review-read-historical-source")
                                .context("reviewer's historical source read result was not returned")?;
                            let encoded = match &output["output"] {
                                serde_json::Value::String(text) => text.clone(),
                                serde_json::Value::Array(items) => items.iter().filter_map(|item| item["text"].as_str()).collect::<Vec<_>>().join("\n"),
                                _ => anyhow::bail!("historical source read did not return text"),
                            };
                            let read: serde_json::Value = serde_json::from_str(&encoded)?;
                            anyhow::ensure!(read["artifact_id"] == source["artifact_id"] && read["canonical_sha256"] == source["sha256"] && read["canonical_bytes"] == source["byte_length"], "historical source read did not verify the exact artifact identity");
                            anyhow::ensure!(read["complete"] == true && (read.get("unavailable_ranges").is_none() || read["unavailable_ranges"] == json!([])), "historical source read was incomplete");
                            let selected = &read["results"][0];
                            anyhow::ensure!(selected["status"] == "ok" && selected["complete"] == true, "historical source selection failed");
                            let historical_bytes = if let Some(text) = selected["text"].as_str() {
                                text.as_bytes().to_vec()
                            } else {
                                base64::engine::general_purpose::STANDARD.decode(selected["data_base64"].as_str().context("exact historical source bytes")?)?
                            };
                            anyhow::ensure!(historical_bytes.len() as u64 == source["byte_length"].as_u64().context("historical source length")?, "historical source byte length changed");
                            anyhow::ensure!(format!("{:x}", Sha256::digest(&historical_bytes)) == source["sha256"].as_str().context("historical source digest")?, "historical source bytes did not match their digest");
                            anyhow::ensure!(historical_bytes == expected_historical_product_source, "reviewer did not receive the failed product version");
                            let current_product_source = fs::read(review_repo.join("src/product.py"))?;
                            anyhow::ensure!(current_product_source == expected_corrected_product_source, "fixture did not retain the corrected current product version");
                            anyhow::ensure!(historical_bytes != current_product_source, "historical source artifact was replaced by current product bytes");
                        }
                        if review_step >= 4 {
                            let diagnostic_ref = packet["failing_executions"][FIXTURE_VALIDATION_ID]
                                ["report"]["diagnostic"]["diagnostic_ref"].as_str().context("complete diagnostic reference")?;
                            let diagnostic = &packet["diagnostic_blobs"][diagnostic_ref];
                            let output = body["input"].as_array().context("review inputs")?.iter()
                                .find(|item| item["type"] == "function_call_output" && item["call_id"] == "review-read-diagnostic")
                                .context("reviewer's diagnostic read was not returned")?;
                            let encoded = match &output["output"] {
                                serde_json::Value::String(text) => text.clone(),
                                serde_json::Value::Array(items) => items.iter().filter_map(|item| item["text"].as_str()).collect::<Vec<_>>().join("\n"),
                                _ => anyhow::bail!("diagnostic read did not return text"),
                            };
                            let read: serde_json::Value = serde_json::from_str(&encoded)?;
                            anyhow::ensure!(read["artifact_id"] == diagnostic["artifact_id"] && read["canonical_sha256"] == diagnostic["sha256"] && read["canonical_bytes"] == diagnostic["byte_length"] && read["complete"] == true, "diagnostic read did not verify complete canonical identity");
                            let mut excerpts = Vec::new();
                            for selected in read["results"].as_array().context("diagnostic selections")? {
                                anyhow::ensure!(selected["status"] == "ok" && selected["complete"] == true, "diagnostic selection was incomplete");
                                if let Some(text) = selected["text"].as_str() {
                                    excerpts.extend_from_slice(text.as_bytes());
                                } else {
                                    excerpts.extend(base64::engine::general_purpose::STANDARD.decode(selected["data_base64"].as_str().context("diagnostic bytes")?)?);
                                }
                            }
                            let excerpts = String::from_utf8(excerpts)?;
                            anyhow::ensure!(excerpts.contains("DIAGNOSTIC-BEGIN") && excerpts.contains("DIAGNOSTIC-END") && excerpts.contains("AssertionError: '100' != '95'"), "reviewer did not receive both diagnostic ends and the actual outer assertion failure");
                        }
                        if review_step >= 5 {
                            let pass = packet["passing_executions"].as_object().context("passing executions")?.values()
                                .find(|execution| execution["report"]["id"] == FIXTURE_VALIDATION_ID)
                                .context("passing price execution")?;
                            let data_ref = pass["inputs"]["hashes"]["data_ref"].as_str().context("passing input reference")?;
                            let reference = &packet["input_blobs"][data_ref];
                            let output = body["input"].as_array().context("review inputs")?.iter()
                                .find(|item| item["type"] == "function_call_output" && item["call_id"] == "review-read-inputs")
                                .context("reviewer's input map read was not returned")?;
                            let encoded = match &output["output"] {
                                serde_json::Value::String(text) => text.clone(),
                                serde_json::Value::Array(items) => items.iter().filter_map(|item| item["text"].as_str()).collect::<Vec<_>>().join("\n"),
                                _ => anyhow::bail!("input map read did not return text"),
                            };
                            let read: serde_json::Value = serde_json::from_str(&encoded)?;
                            anyhow::ensure!(read["artifact_id"] == reference["artifact_id"] && read["canonical_sha256"] == reference["sha256"] && read["canonical_bytes"] == reference["byte_length"] && read["complete"] == true, "input map read did not verify canonical identity");
                            anyhow::ensure!(read.get("unavailable_ranges").is_none() || read["unavailable_ranges"] == json!([]), "input map has missing ranges");
                            let selected = &read["results"][0];
                            anyhow::ensure!(selected["status"] == "ok" && selected["complete"] == true, "input map selection was incomplete");
                            let bytes = if let Some(text) = selected["text"].as_str() {
                                text.as_bytes().to_vec()
                            } else {
                                base64::engine::general_purpose::STANDARD.decode(selected["data_base64"].as_str().context("exact input map bytes")?)?
                            };
                            anyhow::ensure!(bytes.len() as u64 == reference["byte_length"].as_u64().context("input map length")? && format!("{:x}", Sha256::digest(&bytes)) == reference["sha256"].as_str().context("input map digest")?, "input map bytes changed");
                            let passing: serde_json::Value = serde_json::from_slice(&bytes)?;
                            anyhow::ensure!(format!("{:x}", Sha256::digest(serde_json::to_vec(&passing)?)) == data_ref, "passing map did not match original input identity");
                            let failed_ref = packet["failing_executions"][FIXTURE_VALIDATION_ID]["inputs"]["hashes"]["data_ref"].as_str().context("failing input reference")?;
                            let delta = &packet["input_blobs"][failed_ref];
                            anyhow::ensure!(delta["base_data"] == data_ref, "failed map lost its passing base");
                            let mut failing = passing.as_object().context("passing input map")?.clone();
                            for removed in delta["remove"].as_array().context("removed inputs")? {
                                failing.remove(removed.as_str().context("removed input path")?);
                            }
                            failing.extend(delta["set"].as_object().context("changed inputs")?.clone());
                            anyhow::ensure!(format!("{:x}", Sha256::digest(serde_json::to_vec(&failing)?)) == failed_ref, "reconstructed failure map lost its original identity");
                            for (map, product) in [(passing.as_object().unwrap(), &expected_corrected_product_source), (&failing, &expected_historical_product_source)] {
                                anyhow::ensure!(map["src/product.py"] == format!("file:{}:{:x}", product.len(), Sha256::digest(product)), "reviewer received the wrong product input version");
                                for (index, path) in review_input_paths.iter().enumerate() {
                                    let expected = format!("{{\"input\":{index}}}\n");
                                    anyhow::ensure!(map[path] == format!("file:{}:{:x}", expected.len(), Sha256::digest(expected.as_bytes())), "review input was lost or changed");
                                }
                            }
                        }
                        Ok(())
                    })();
                    if let Err(error) = inspected {
                        review_errors.lock().unwrap().push(error.to_string());
                        review["approved"] = json!(false);
                        review["explanation"] = json!(format!("Required review input could not be verified: {error}"));
                        return sse_response(terminal_candidate(10, &review.to_string()));
                    }
                    if review_step == 0 {
                        let bytes = reference["byte_length"].as_u64().unwrap();
                        return sse_response(sse(vec![
                            ev_response_created("review-read-diff-response"),
                            ev_function_call("review-read-diff", "read_tool_output", &json!({
                                "artifact_id": reference["artifact_id"],
                                "selectors": [{"kind":"bytes","start":0,"end":2048}, {"kind":"bytes","start":bytes-2048,"end":bytes}]
                            }).to_string()),
                            ev_completed("review-read-diff-response"),
                        ]));
                    }
                    if review_step == 1 {
                        let source = packet["source_blobs"].as_object().unwrap().values()
                            .find(|source| source["repository_path"] == "src/test_behavior.py");
                        if let Some(source) = source.filter(|source| source["artifact_id"].is_string() && source["byte_length"].is_u64()) {
                            return sse_response(sse(vec![
                                ev_response_created("review-read-source-response"),
                                ev_function_call("review-read-source", "read_tool_output", &json!({
                                    "artifact_id": source["artifact_id"],
                                    "selectors": [{"kind":"bytes","start":0,"end":source["byte_length"]}]
                                }).to_string()),
                                ev_completed("review-read-source-response"),
                            ]));
                        }
                        review_errors.lock().unwrap().push("current source has no reviewer-readable artifact".to_owned());
                        review["approved"] = json!(false);
                        review["explanation"] = json!("Current test source could not be authenticated through the reviewer's tools.");
                        return sse_response(terminal_candidate(10, &review.to_string()));
                    }
                    if review_step == 2 {
                        let source_ref = packet["failing_executions"][FIXTURE_VALIDATION_ID]
                            ["inputs"]["sources"]["src/product.py"]["source_ref"]
                            .as_str();
                        if let Some(source) = source_ref
                            .and_then(|source_ref| packet["source_blobs"].get(source_ref))
                            .filter(|source| source["snapshot_path"] == "src/product.py" && source["artifact_id"].is_string() && source["sha256"].is_string() && source["byte_length"].is_u64())
                        {
                            return sse_response(sse(vec![
                                ev_response_created("review-read-historical-source-response"),
                                ev_function_call("review-read-historical-source", "read_tool_output", &json!({
                                    "artifact_id": source["artifact_id"],
                                    "selectors": [{"kind":"bytes","start":0,"end":source["byte_length"]}]
                                }).to_string()),
                                ev_completed("review-read-historical-source-response"),
                            ]));
                        }
                        review_errors.lock().unwrap().push("historical source has no reviewer-readable artifact".to_owned());
                        review["approved"] = json!(false);
                        review["explanation"] = json!("Historical product source could not be authenticated through the reviewer's tools.");
                        return sse_response(terminal_candidate(10, &review.to_string()));
                    }
                    if review_step == 3 {
                        let diagnostic_ref = packet["failing_executions"][FIXTURE_VALIDATION_ID]
                            ["report"]["diagnostic"]["diagnostic_ref"].as_str();
                        if let Some(diagnostic) = diagnostic_ref.and_then(|hash| packet["diagnostic_blobs"].get(hash))
                            .filter(|entry| entry["artifact_id"].is_string() && entry["byte_length"].as_u64().is_some_and(|bytes| bytes > 8000))
                        {
                            let bytes = diagnostic["byte_length"].as_u64().unwrap();
                            return sse_response(sse(vec![
                                ev_response_created("review-read-diagnostic-response"),
                                ev_function_call("review-read-diagnostic", "read_tool_output", &json!({
                                    "artifact_id":diagnostic["artifact_id"],
                                    "selectors":[{"kind":"bytes","start":0,"end":128},{"kind":"bytes","start":bytes-2048,"end":bytes}]
                                }).to_string()),
                                ev_completed("review-read-diagnostic-response"),
                            ]));
                        }
                        review_errors.lock().unwrap().push("complete failure diagnostic has no reviewer-readable artifact".to_owned());
                        review["approved"] = json!(false);
                        return sse_response(terminal_candidate(10, &review.to_string()));
                    }
                    if review_step == 4 {
                        let pass = packet["passing_executions"].as_object().unwrap().values()
                            .find(|execution| execution["report"]["id"] == FIXTURE_VALIDATION_ID).unwrap();
                        let reference = &packet["input_blobs"][pass["inputs"]["hashes"]["data_ref"].as_str().unwrap()];
                        if reference["kind"] == "tool-output-reference-v1" && reference["artifact_id"].is_string() && reference["byte_length"].as_u64().is_some_and(|bytes| bytes > 4096) {
                            return sse_response(sse(vec![
                                ev_response_created("review-read-inputs-response"),
                                ev_function_call("review-read-inputs", "read_tool_output", &json!({
                                    "artifact_id":reference["artifact_id"],
                                    "selectors":[{"kind":"bytes","start":0,"end":reference["byte_length"]}]
                                }).to_string()),
                                ev_completed("review-read-inputs-response"),
                            ]));
                        }
                        review_errors.lock().unwrap().push("large input map has no reviewer-readable artifact".to_owned());
                        review["approved"] = json!(false);
                        return sse_response(terminal_candidate(10, &review.to_string()));
                    }
                } else {
                    assert!(packet["workspace_diff"].is_string());
                }
                let declarations = packet["required_test_declarations"][other_path].as_array().unwrap();
                assert_eq!(declarations.len(), 1);
                review["evaluated_test_paths"] = json!(["src/test_behavior.py", other_path]);
                let mut input_paths = vec![
                    "src/test_behavior.py",
                    "src/product.py",
                    other_path,
                    "src/other/product.py",
                ];
                if oversized_diff {
                    input_paths.push(generated_path);
                    input_paths.extend(review_input_paths.iter().map(String::as_str));
                }
                review["input_paths"] = json!(input_paths);
                review["obligations"].as_array_mut().unwrap().push(json!({
                    "validation_id":"fixture.other", "test_id":"__main__.OtherContract.test_value",
                    "test_path":other_path, "test_body":declarations[0]["body"],
                    "oracle_source":"Current user request: the other CLI must output updated",
                    "expected_behavior":"the other CLI prints updated",
                    "runtime_path":"python src/other/product.py subprocess stdout",
                    "defect_explanation":"The unchanged subprocess assertion rejects the old CLI output and passes after the combined product correction.",
                    "failed_attempt_id":packet["failing_executions"]["fixture.other"]["attempt_id"],
                    "product_paths":["src/other/product.py"]
                }));
            }
            sse_response(terminal_candidate(10, &review.to_string()))
        })
        .with_priority(1)
        .up_to_n_times(if oversized_diff { 6 } else { 1 })
        .mount(harness.server()).await;
    let polls = Arc::new(AtomicUsize::new(0));
    let observed_polls = Arc::clone(&polls);
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path_regex(".*/responses"))
        .and(|request: &wiremock::Request| quality_fixture_yielded_process(request).is_some())
        .respond_with(move |request: &wiremock::Request| {
            let poll = observed_polls.fetch_add(1, Ordering::SeqCst);
            sse_response(write_stdin_call_response(
                &format!("batch-process-poll-{poll}"),
                quality_fixture_yielded_process(request).unwrap(),
            ))
        })
        .with_priority(1)
        .mount(harness.server())
        .await;
    let price_command = fixture.exact_focused_command();
    let other_command = price_command.replace(FIXTURE_VALIDATION_ID, "fixture.other");
    let mut responses = vec![exec_command_call_response(
        "batch-price-fails",
        &price_command,
        &fixture.repo_path,
    )];
    if mismatched_before {
        responses.push(exec_command_call_response(
            "different-broken-version",
            &format!("{} change_intermediate.py", available_python_command()?),
            &fixture.repo_path,
        ));
    }
    responses.extend([
        exec_command_call_response("batch-other-fails", &other_command, &fixture.repo_path),
        exec_command_call_response(
            "fix-both-products",
            &format!("{} fix_both.py", available_python_command()?),
            &fixture.repo_path,
        ),
        exec_command_call_response("batch-price-passes", &price_command, &fixture.repo_path),
        exec_command_call_response("batch-other-passes", &other_command, &fixture.repo_path),
        sse(vec![
            ev_response_created("batch-review"),
            ev_function_call(
                "batch-review",
                "review_test_quality",
                if oversized_diff {
                    r#"{"test_paths":null}"#
                } else if review_other {
                    "{}"
                } else {
                    r#"{"test_paths":["src/test_behavior.py"]}"#
                },
            ),
            ev_completed("batch-review"),
        ]),
    ]);
    let accepted = review_other && !mismatched_before;
    let scripted_calls = responses.len();
    let calls = Arc::new(AtomicUsize::new(0));
    let observed_calls = Arc::clone(&calls);
    // Tool calls and process polls already consume logical generations. Keep
    // offering the terminal candidate until the real gate ends the rejected
    // turn, rather than requiring 32 additional responses after the tools.
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path_regex(".*/responses"))
        .respond_with(move |request: &wiremock::Request| {
            let index = observed_calls.fetch_add(1, Ordering::SeqCst);
            if oversized_diff && index == 0 {
                // Check the actual provider-facing runtime registration. A mock
                // provider otherwise accepts a schema the real API rejects.
                let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                let tool = body["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|tool| tool["name"] == "review_test_quality");
                let valid = tool.is_some_and(|tool| {
                    tool["strict"] == true
                        && tool["parameters"]["additionalProperties"] == false
                        && tool["parameters"]["required"] == json!(["test_paths"])
                        && tool["parameters"]["properties"]["test_paths"]["anyOf"]
                            .as_array()
                            .is_some_and(|variants| {
                                variants.iter().any(|v| v["type"] == "null")
                                    && variants.iter().any(|v| {
                                        v["type"] == "array" && v["items"]["type"] == "string"
                                    })
                            })
                });
                if !valid {
                    schema_errors.lock().unwrap().push(
                        "real provider would reject the review_test_quality schema".to_owned(),
                    );
                }
            }
            if let Some(response) = responses.get(index) {
                return sse_response(response.clone());
            }
            if accepted && !oversized_diff {
                assert_eq!(index, scripted_calls, "unexpected extra model generation");
            }
            if oversized_diff && index > scripted_calls {
                // A rejected review must reach the assertions below promptly.
                // The successful path ends on the first terminal candidate.
                return sse_response(required_failure_call_response(
                    "stop-after-oversized-review-rejection",
                ));
            }
            sse_response(terminal_candidate(
                index,
                "Both requested CLI behaviors are corrected and checked.",
            ))
        })
        .up_to_n_times((MAX_REGULAR_LOGICAL_GENERATIONS + 1) as u64)
        .mount(harness.server())
        .await;
    let events = submit_and_collect(harness.test(), "Fix both CLI behaviors together: input 100 must output 95; the other CLI must output updated. Collect both failures, fix both products, then validate both.").await?;
    for call in ["batch-price-fails", "batch-other-fails"] {
        assert!(events.iter().any(|event| matches!(event, EventMsg::ExecCommandEnd(end) if end.call_id == call && end.exit_code != 0)));
    }
    for call in [
        "fix-both-products",
        "batch-price-passes",
        "batch-other-passes",
    ] {
        assert!(events.iter().any(|event| matches!(event, EventMsg::ExecCommandEnd(end) if end.call_id == call && end.exit_code == 0)));
    }
    let boundary_errors = boundary_errors.lock().unwrap().clone();
    assert!(boundary_errors.is_empty(), "{boundary_errors:?}");
    assert_eq!(
        function_call_output_success(&events, "batch-review"),
        Some(accepted),
        "{:?}",
        function_call_output(&events, "batch-review")
    );
    if !accepted {
        assert!(
            function_call_output(&events, "batch-review")
                .unwrap()
                .contains("product correction without observed test coverage")
        );
        assert!(events.iter().all(|event| !is_assistant_output(event)));
    }
    assert!(events.iter().any(|event| matches!(event, EventMsg::TurnComplete(done) if done.error.is_none() == accepted && done.last_agent_message.is_some() == accepted)));
    assert_eq!(
        reviews.load(Ordering::SeqCst),
        if oversized_diff { 6 } else { 1 }
    );
    assert_eq!(
        calls.load(Ordering::SeqCst)
            + if accepted {
                0
            } else {
                polls.load(Ordering::SeqCst)
            },
        if accepted {
            scripted_calls + 1
        } else {
            MAX_REGULAR_LOGICAL_GENERATIONS + 1
        },
        "the actual turn must stop at successful completion or its generation limit"
    );
    assert_eq!(
        fs::read(fixture.repo_path.join("src/test_behavior.py"))?,
        price_source
    );
    assert_eq!(
        fs::read_to_string(fixture.repo_path.join(other_path))?,
        other_source
    );
    let price_runs =
        fs::read_to_string(fixture.repo_path.join(".fixture-state/quality-executions"))?;
    let price_runs = price_runs
        .lines()
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    assert_eq!(price_runs.len(), 2);
    assert_eq!(price_runs[0]["actual"], "100");
    assert_eq!(price_runs[1]["actual"], "95");
    let other_runs = fs::read_to_string(fixture.repo_path.join(".fixture-state/other-executions"))?;
    assert_eq!(
        other_runs.lines().collect::<Vec<_>>(),
        vec![
            if mismatched_before {
                "intermediate"
            } else {
                "baseline"
            },
            "updated"
        ]
    );
    assert!(!fixture.marker_path.exists());
    harness.test().codex.shutdown_and_wait().await?;
    Ok(())
}

async fn run_quality_scenario(scenario: QualityScenario) -> Result<()> {
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    let fixture = CompletionProofFixture::new()?;
    let fix = install_quality_behavior_fixture(&fixture, false)?;
    if scenario == QualityScenario::PinnedProduct {
        let config_path = fixture
            .repo_path
            .join(".codex/validation/completion-proof.toml");
        let config = fs::read_to_string(&config_path)?;
        let line = config
            .lines()
            .find(|line| line.starts_with("trusted_bundle_paths ="))
            .context("trusted fixture bundle")?;
        fs::write(
            &config_path,
            config.replace(line, &line.replace(']', ", \"src/product.py\"]")),
        )?;
        let path = fixture.repo_path.join("fix_price.py");
        let script = fs::read_to_string(&path)?;
        fs::write(
            path,
            format!(
                "{script}import subprocess\nsubprocess.run(['git', 'add', 'src/product.py'], check=True)\nsubprocess.run(['git', 'commit', '--quiet', '-m', 'fixture pinned product correction'], check=True)\n"
            ),
        )?;
        run_git(
            &fixture.repo_path,
            &[
                "add",
                ".codex/validation/completion-proof.toml",
                "fix_price.py",
            ],
        )?;
        run_git(
            &fixture.repo_path,
            &["commit", "--quiet", "--amend", "--no-edit"],
        )?;
    }
    if scenario == QualityScenario::PartialBatch {
        // Two consumed data files exceed the old combined prompt-sized capture
        // limit. A narrow review must retain their identities without needing
        // their repeated full contents in every execution's model input.
        for name in ["catalog-a.json", "catalog-b.json"] {
            fs::write(
                fixture.repo_path.join("src").join(name),
                format!("{{\"data\":\"{}\"}}", "x".repeat(1100 * 1024)),
            )?;
        }
        let path = fixture.repo_path.join("src/other/test_product.py");
        fs::write(
            &path,
            format!(
                "{}\n# Changed fixture remains an independent obligation.\n",
                fs::read_to_string(&path)?
            ),
        )?;
    }
    let accepted = matches!(
        scenario,
        QualityScenario::Accepted
            | QualityScenario::RustAccepted
            | QualityScenario::Recheck
            | QualityScenario::PinnedProduct
            | QualityScenario::Retired
    );
    let review_accepted = accepted || scenario == QualityScenario::PartialBatch;
    let (test_path, test_body, test_id) = if scenario == QualityScenario::RustAccepted {
        install_rust_quality_test(&fixture)?;
        (
            "src/test_behavior.rs",
            QUALITY_RUST_TEST_BODY,
            "test_discount",
        )
    } else {
        ("src/test_behavior.py", QUALITY_TEST_BODY, QUALITY_TEST_ID)
    };
    if scenario == QualityScenario::Accepted {
        let focused_path = fixture.repo_path.join("focused.py");
        let script = fs::read_to_string(&focused_path)?;
        anyhow::ensure!(
            script.matches("\nstarted_at =").count() == 1,
            "native fixture timing boundary"
        );
        fs::write(&focused_path, script.replace("\nstarted_at =", "\nif VALIDATION_ID != 'fixture.other':\n    command.append('PriceContract.test_discount')\nstarted_at ="))?;
        run_git(&fixture.repo_path, &["add", "focused.py"])?;
        let path = fixture.repo_path.join(test_path);
        let current = fs::read_to_string(&path)?.replace(
            "if __name__ == '__main__':",
            "    def test_rejects_non_numeric_price(self):\n        result = subprocess.run([sys.executable, '-B', 'src/product.py', 'invalid'], capture_output=True)\n        self.assertNotEqual(result.returncode, 0)\n\nif __name__ == '__main__':",
        );
        install_mixed_eol_quality_baseline(
            &fixture,
            test_path,
            &current.replace(
                "self.assertEqual(result.stdout.strip(), '95')",
                "self.assertEqual(result.stdout.strip(), '100')",
            ),
            &current,
        )?;
    }
    if scenario.reviews_retirement() {
        let path = fixture.repo_path.join(test_path);
        let current = fs::read_to_string(&path)?;
        let old = current.replace(QUALITY_TEST_BODY, QUALITY_RETIRED_BODY);
        fs::write(&path, &old)?;
        fs::write(fixture.repo_path.join("src/retired_test.py"), old)?;
        run_git(
            &fixture.repo_path,
            &["add", test_path, "src/retired_test.py"],
        )?;
        run_git(
            &fixture.repo_path,
            &["commit", "--quiet", "--amend", "--no-edit"],
        )?;
        fs::write(path, current)?;
        fs::remove_file(fixture.repo_path.join("src/retired_test.py"))?;
    }
    if scenario == QualityScenario::ChangedAssertions {
        let path = fixture.repo_path.join("fix_price.py");
        let fix_script = fs::read_to_string(&path)?;
        fs::write(
            &path,
            format!(
                "{fix_script}p = Path('src/test_behavior.py')\np.write_text(p.read_text().replace(\"self.assertEqual(result.stdout.strip(), '95')\", \"self.assertTrue(result.stdout.strip())\"))\n"
            ),
        )?;
        run_git(&fixture.repo_path, &["add", "fix_price.py"])?;
        run_git(
            &fixture.repo_path,
            &["commit", "--quiet", "--amend", "--no-edit"],
        )?;
    }
    if scenario == QualityScenario::OmittedTest {
        let path = fixture.repo_path.join("src/test_behavior.py");
        let source = fs::read_to_string(&path)?;
        fs::write(&path, source.replace("if __name__ == '__main__':", "    def test_missing_behavior(self):\n        self.assertTrue(True)\n\nif __name__ == '__main__':"))?;
    }
    let harness = fixture.harness_with_raw_response_items().await?;
    // A validation can yield even with a long requested wait. Keep its native
    // process running and collect its terminal result before the scripted model
    // changes the product, starts another validation, or requests review.
    let polls = Arc::new(AtomicUsize::new(0));
    let observed_polls = Arc::clone(&polls);
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path_regex(".*/responses"))
        .and(|request: &wiremock::Request| quality_fixture_yielded_process(request).is_some())
        .respond_with(move |request: &wiremock::Request| {
            let process = quality_fixture_yielded_process(request).expect("yielded process");
            let poll = observed_polls.fetch_add(1, Ordering::SeqCst);
            sse_response(write_stdin_call_response(
                &format!("quality-process-poll-{poll}"),
                process,
            ))
        })
        .with_priority(1)
        .mount(harness.server())
        .await;
    let source_before = fs::read(fixture.repo_path.join(test_path))?;
    let responses = [
        exec_command_call_response(
            "behavior-detects-broken-cli",
            &fixture.exact_focused_command(),
            &fixture.repo_path,
        ),
        exec_command_call_response("fix-product-behavior", &fix, &fixture.repo_path),
        exec_command_call_response(
            "same-test-passes-corrected-cli",
            &fixture.exact_focused_command(),
            &fixture.repo_path,
        ),
        sse(vec![
            ev_response_created("quality-review-request"),
            ev_function_call(
                "quality-review",
                "review_test_quality",
                if scenario == QualityScenario::PartialBatch {
                    r#"{"test_paths":["src/test_behavior.py"]}"#
                } else {
                    "{}"
                },
            ),
            ev_completed("quality-review-request"),
        ]),
    ];
    let calls = Arc::new(AtomicUsize::new(0));
    let observed_calls = Arc::clone(&calls);
    wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path_regex(".*/responses"))
            .respond_with(move |request: &wiremock::Request| {
                let index = observed_calls.fetch_add(1, Ordering::SeqCst);
                if index < 4 { return sse_response(responses[index].clone()); }
                if index == 4 {
                    let review = quality_review_result(request, scenario, test_path, test_body, test_id);
                    return sse_response(terminal_candidate(4, &review.to_string()));
                }
                if accepted { assert_eq!(index, 5, "unexpected extra model/review generation"); }
                sse_response(terminal_candidate(index, "The price CLI now subtracts five, and the regression test detects the original bug."))
            }).up_to_n_times(if accepted { 6 } else { (MAX_REGULAR_LOGICAL_GENERATIONS + 2) as u64 })
            .mount(harness.server()).await;
    let mut request = "Make the price CLI subtract five: input 100 must output 95. Add a regression test and validate this small change.".to_owned();
    if scenario.reviews_retirement() {
        request.push_str(QUALITY_RETIREMENT_REQUEST);
    }
    let events = submit_and_collect(harness.test(), &request).await?;
    assert!(events.iter().any(|e| matches!(e, EventMsg::ExecCommandEnd(end) if end.call_id == "behavior-detects-broken-cli" && end.exit_code != 0)));
    assert!(events.iter().any(|e| matches!(e, EventMsg::ExecCommandEnd(end) if end.call_id == "same-test-passes-corrected-cli" && end.exit_code == 0)));
    assert_eq!(
        function_call_output_success(&events, "quality-review"),
        Some(review_accepted),
        "quality tool output: {:?}",
        function_call_output(&events, "quality-review")
    );
    assert!(events.iter().any(|event| matches!(event, EventMsg::TurnComplete(done) if done.error.is_none() == accepted && done.last_agent_message.is_some() == accepted)));
    if !accepted {
        assert!(events.iter().all(|event| !is_assistant_output(event)));
    }
    if scenario.reviews_retirement() {
        assert!(!fixture.repo_path.join("src/retired_test.py").exists());
        if !review_accepted {
            let output = function_call_output(&events, "quality-review")
                .context("missing retirement rejection")?;
            let expected = match scenario {
                QualityScenario::OmittedRetirement => "omitted retired test obligations",
                QualityScenario::AlteredRetirement => {
                    "changed or duplicated a retired test declaration"
                }
                _ => "lacks user authority or a verified behavior replacement",
            };
            assert!(
                output.contains(expected),
                "wrong retirement rejection: {output}"
            );
        }
    }
    if scenario != QualityScenario::ChangedAssertions {
        assert_eq!(fs::read(fixture.repo_path.join(test_path))?, source_before);
    }
    let executions =
        fs::read_to_string(fixture.repo_path.join(".fixture-state/quality-executions"))?
            .lines()
            .map(serde_json::from_str::<serde_json::Value>)
            .collect::<std::result::Result<Vec<_>, _>>()?;
    assert_eq!(executions.len(), 2);
    assert_eq!(executions[0]["actual"], "100");
    assert_eq!(executions[1]["actual"], "95");
    assert_ne!(executions[0]["execution"], executions[1]["execution"]);
    assert!(!fixture.marker_path.exists());
    assert_eq!(
        calls.load(Ordering::SeqCst)
            + if accepted {
                0
            } else {
                polls.load(Ordering::SeqCst)
            },
        if accepted {
            6
        } else {
            MAX_REGULAR_LOGICAL_GENERATIONS + 2
        }
    );
    if scenario == QualityScenario::Recheck {
        // A later task edits only the product. The existing test must stay
        // an obligation after its old review is revoked by a real failure.
        run_git(
            &fixture.repo_path,
            &["add", "src/product.py", "src/test_behavior.py"],
        )?;
        run_git(
            &fixture.repo_path,
            &[
                "commit",
                "--quiet",
                "-m",
                "fixture completed price behavior",
            ],
        )?;
        fs::write(
            fixture.repo_path.join("src/product.py"),
            "import sys\nprint(int(sys.argv[1]))\n",
        )?;
        // Restoring the prior correct bytes does not revive a pass that predates
        // an observed regression, even if a reviewer tries to approve it.
        let early = [
            exec_command_call_response(
                "regression-before-retest",
                &fixture.exact_focused_command(),
                &fixture.repo_path,
            ),
            exec_command_call_response("restore-before-retest", &fix, &fixture.repo_path),
            sse(vec![
                ev_response_created("premature-review"),
                ev_function_call("premature-review", "review_test_quality", "{}"),
                ev_completed("premature-review"),
            ]),
        ];
        let early_calls = Arc::new(AtomicUsize::new(0));
        let observed_early = Arc::clone(&early_calls);
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path_regex(".*/responses"))
            .respond_with(move |request: &wiremock::Request| {
                let index = observed_early.fetch_add(1, Ordering::SeqCst);
                if index < early.len() {
                    return sse_response(early[index].clone());
                }
                if index == early.len() {
                    let review =
                        quality_review_result(request, scenario, test_path, test_body, test_id);
                    return sse_response(terminal_candidate(60, &review.to_string()));
                }
                sse_response(terminal_candidate(
                    index,
                    "The prior passing bytes were restored.",
                ))
            })
            .up_to_n_times((MAX_REGULAR_LOGICAL_GENERATIONS + 2) as u64)
            .mount(harness.server())
            .await;
        let premature = submit_and_collect(harness.test(), "The price regressed: input 100 must output 95. Restore the code and assess the retained evidence before retesting.").await?;
        assert_eq!(
            function_call_output_success(&premature, "premature-review"),
            Some(false),
            "an old pass cannot survive a later observed regression"
        );
        assert!(premature.iter().all(|event| !is_assistant_output(event)));
        fs::write(
            fixture.repo_path.join("src/product.py"),
            "import sys\nprint(int(sys.argv[1]))\n",
        )?;
        let responses = [
            exec_command_call_response(
                "later-regression-detected",
                &fixture.exact_focused_command(),
                &fixture.repo_path,
            ),
            exec_command_call_response("later-product-corrected", &fix, &fixture.repo_path),
            exec_command_call_response(
                "existing-test-passes-again",
                &fixture.exact_focused_command(),
                &fixture.repo_path,
            ),
            sse(vec![
                ev_response_created("renew-quality"),
                ev_function_call("renew-quality", "review_test_quality", "{}"),
                ev_completed("renew-quality"),
            ]),
        ];
        let requests = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&requests);
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path_regex(".*/responses"))
            .respond_with(move |request: &wiremock::Request| {
                let index = observed.fetch_add(1, Ordering::SeqCst);
                if index < 4 {
                    return sse_response(responses[index].clone());
                }
                if index == 4 {
                    let mut review =
                        quality_review_result(request, scenario, test_path, test_body, test_id);
                    let body: serde_json::Value =
                        serde_json::from_slice(&request.body).expect("review JSON");
                    let packet = body["input"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .flat_map(|item| item["content"].as_array().into_iter().flatten())
                        .filter_map(|item| item["text"].as_str())
                        .filter_map(|text| serde_json::from_str::<serde_json::Value>(text).ok())
                        .find(|value| value.get("failing_executions").is_some())
                        .expect("quality packet");
                    let earlier = packet["failing_executions"]
                        .as_object()
                        .unwrap()
                        .iter()
                        .find(|(key, _)| key.starts_with("attempt:"))
                        .expect("the earlier observed failure must survive the later regression");
                    review["obligations"][0]["failed_attempt_id"] = earlier.1["attempt_id"].clone();
                    return sse_response(terminal_candidate(40, &review.to_string()));
                }
                assert_eq!(index, 5);
                sse_response(terminal_candidate(
                    41,
                    "The later regression is corrected and the unchanged test detects it.",
                ))
            })
            .up_to_n_times(6)
            .expect(6)
            .mount(harness.server())
            .await;
        let renewed = submit_and_collect(harness.test(), "Correct the later price regression: input 100 must output 95. Validate with the existing test and renew its evidence.").await?;
        assert_eq!(
            function_call_output_success(&renewed, "renew-quality"),
            Some(true),
            "renewal failed: {:?}",
            function_call_output(&renewed, "renew-quality")
        );
        assert!(renewed.iter().any(|event| matches!(event, EventMsg::TurnComplete(done) if done.error.is_none() && done.last_agent_message.is_some())));
        assert_eq!(
            requests.load(Ordering::SeqCst),
            6,
            "missing fresh independent review"
        );
        assert_eq!(fs::read(fixture.repo_path.join(test_path))?, source_before);
        let executions =
            fs::read_to_string(fixture.repo_path.join(".fixture-state/quality-executions"))?
                .lines()
                .map(serde_json::from_str::<serde_json::Value>)
                .collect::<std::result::Result<Vec<_>, _>>()?;
        assert_eq!(
            executions
                .iter()
                .map(|e| e["actual"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["100", "95", "100", "100", "95"]
        );
        assert!(!fixture.marker_path.exists());
    }
    if scenario == QualityScenario::Accepted {
        // The following ordinary turn reloads this authenticated state. Reviews
        // without retirements must keep the old serialized shape so that their
        // authentication also survives the newly added optional field.
        let (_, saved) = read_authenticated_completion_proof_state(
            harness.test().codex_home_path(),
            &fixture.repo_path,
        )?;
        let quality = saved["state"]["focused_completion"]["quality"]
            .as_array()
            .context("persisted quality evidence")?;
        assert_eq!(quality.len(), 1);
        assert!(quality[0]["review"].get("retirements").is_none());
        // An unrelated documentation edit runs only its existing doc check.
        // The real behavior executions and independent review remain reusable.
        mount_sse_sequence(
            harness.server(),
            vec![
                exec_command_call_response(
                    "unrelated-documentation-edit",
                    &fixture.documentation_markdown_mutation_command,
                    &fixture.repo_path,
                ),
                exec_command_call_response(
                    "nearest-documentation-check",
                    &fixture.documentation_command,
                    &fixture.repo_path,
                ),
                sse(vec![
                    ev_response_created("reuse-quality"),
                    ev_function_call("reuse-quality", "review_test_quality", "{}"),
                    ev_completed("reuse-quality"),
                ]),
                terminal_candidate(20, "The documentation change is checked."),
            ],
        )
        .await;
        let docs = submit_and_collect(harness.test(), "Update only the documentation, run its normal check, reuse current test evidence, and finish.").await?;
        assert_eq!(
            function_call_output_success(&docs, "reuse-quality"),
            Some(true)
        );
        assert!(docs.iter().any(|event| matches!(event, EventMsg::TurnComplete(done) if done.error.is_none() && done.last_agent_message.is_some())));
        assert_eq!(
            fs::read_to_string(fixture.repo_path.join(".fixture-state/quality-executions"))?
                .lines()
                .count(),
            2
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            6,
            "unrelated edit repeated independent review"
        );

        // A second source change is inside the first validator's broad
        // declared group but outside the reviewed price dependencies.
        let change_other = format!("{} change_other.py", available_python_command()?);
        let validate_other = fixture
            .focused_command
            .replace("{validation_id}", "fixture.other");
        mount_sse_sequence(
            harness.server(),
            vec![
                exec_command_call_response(
                    "change-independent-product",
                    &change_other,
                    &fixture.repo_path,
                ),
                exec_command_call_response(
                    "validate-only-independent-product",
                    &validate_other,
                    &fixture.repo_path,
                ),
                sse(vec![
                    ev_response_created("reuse-price-quality"),
                    ev_function_call("reuse-price-quality", "review_test_quality", "{}"),
                    ev_completed("reuse-price-quality"),
                ]),
                terminal_candidate(22, "The independent product change is checked."),
            ],
        )
        .await;
        let independent = submit_and_collect(harness.test(), "Make the other CLI output updated, validate only that change, and retain the established price test evidence.").await?;
        assert_eq!(
            function_call_output_success(&independent, "reuse-price-quality"),
            Some(true),
            "unexpected quality rerun: {:?}",
            function_call_output(&independent, "reuse-price-quality")
        );
        let requests = harness
            .server()
            .received_requests()
            .await
            .unwrap_or_default();
        let last_input = requests
            .last()
            .and_then(|request| serde_json::from_slice::<serde_json::Value>(&request.body).ok())
            .map(|body| {
                body["input"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .flat_map(|item| item["content"].as_array().into_iter().flatten())
                    .filter_map(|item| item["text"].as_str())
                    .filter(|text| text.contains("CompletionProofGate"))
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            });
        assert!(independent.iter().any(|event| matches!(event, EventMsg::TurnComplete(done) if done.error.is_none() && done.last_agent_message.is_some())), "independent task did not complete; validation={:?}; last model input={last_input:?}", function_call_output(&independent, "validate-only-independent-product"));
        assert_eq!(
            fs::read_to_string(fixture.repo_path.join(".fixture-state/quality-executions"))?
                .lines()
                .count(),
            2
        );
        assert_eq!(
            fs::read_to_string(fixture.repo_path.join(".fixture-state/other-executions"))?
                .lines()
                .collect::<Vec<_>>(),
            vec!["updated"]
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            6,
            "unrelated source edit repeated independent review"
        );

        let home = Arc::clone(&harness.test().home);
        let rollout = harness
            .test()
            .codex
            .rollout_path()
            .context("missing ordinary task rollout")?;
        harness.test().codex.shutdown_and_wait().await?;
        let cwd = fixture.repo_path.abs();
        let mut builder = test_codex()
            .with_raw_response_items()
            .with_config(move |config| set_fixture_workspace(config, cwd));
        let resumed = builder
            .resume(harness.server(), Arc::clone(&home), rollout)
            .await?;
        mount_sse_sequence(
            harness.server(),
            vec![
                sse(vec![
                    ev_response_created("resume-quality"),
                    ev_function_call("resume-quality", "review_test_quality", "{}"),
                    ev_completed("resume-quality"),
                ]),
                terminal_candidate(21, "Current focused evidence survived the matching resume."),
            ],
        )
        .await;
        let resumed_events = submit_and_collect(
            &resumed,
            "Continue the same completed task with its current test evidence.",
        )
        .await?;
        assert_eq!(
            function_call_output_success(&resumed_events, "resume-quality"),
            Some(true)
        );
        assert!(resumed_events.iter().any(|event| matches!(event, EventMsg::TurnComplete(done) if done.error.is_none() && done.last_agent_message.is_some())));

        // A later assertion edit invalidates quality without launching tests.
        let test_path = fixture.repo_path.join("src/test_behavior.py");
        fs::write(
            &test_path,
            fs::read_to_string(&test_path)?.replace(
                "self.assertEqual(result.stdout.strip(), '95')",
                "self.assertTrue(result.stdout.strip())",
            ),
        )?;
        mount_sse_sequence(
            harness.server(),
            repeated_terminal_candidates(
                TOTAL_GENERATIONS_WITH_FORCED_TERMINAL,
                "The old review still covers this changed assertion.",
            ),
        )
        .await;
        let stale = submit_and_collect(&resumed, "Finish after the latest test edit.").await?;
        assert!(stale.iter().all(|event| !is_assistant_output(event)));
        assert!(stale.iter().any(|event| matches!(event, EventMsg::TurnComplete(done) if done.error.is_some() && done.last_agent_message.is_none())));
        let resumed_rollout = resumed
            .codex
            .rollout_path()
            .context("missing resumed rollout")?;
        resumed.codex.shutdown_and_wait().await?;

        #[cfg(windows)]
        {
            let (state_path, mut forged) =
                read_authenticated_completion_proof_state(home.path(), &fixture.repo_path)?;
            forged["state"]["focused_completion"]["pending_paths"] = json!([]);
            forged["state"]["requires_non_documentation_proof"] = json!(false);
            fs::write(state_path, serde_json::to_vec(&forged)?)?;
            let cwd = fixture.repo_path.abs();
            let mut builder = test_codex()
                .with_raw_response_items()
                .with_config(move |config| set_fixture_workspace(config, cwd));
            let forged_resume = builder
                .resume(harness.server(), Arc::clone(&home), resumed_rollout)
                .await?;
            mount_sse_sequence(
                harness.server(),
                repeated_terminal_candidates(
                    TOTAL_GENERATIONS_WITH_FORCED_TERMINAL,
                    "The caller-edited quality state says this is complete.",
                ),
            )
            .await;
            let forged_events =
                submit_and_collect(&forged_resume, "Finish using the retained evidence.").await?;
            assert!(
                forged_events
                    .iter()
                    .all(|event| !is_assistant_output(event))
            );
            assert!(forged_events.iter().any(|event| matches!(event, EventMsg::TurnComplete(done) if done.error.as_ref().is_some_and(|error| error.message.contains("authenticated")))));
            forged_resume.codex.shutdown_and_wait().await?;
        }
        assert_eq!(
            fs::read_to_string(fixture.repo_path.join(".fixture-state/quality-executions"))?
                .lines()
                .count(),
            2
        );
        assert!(!fixture.marker_path.exists());
    }
    Ok(())
}

#[test]
fn test_quality_changed_doctest_is_unsatisfied_without_remigrating_unchanged_docs() -> Result<()> {
    run_session_path_test(
        "test_quality_changed_doctest_is_unsatisfied_without_remigrating_unchanged_docs",
        || async {
            for changed_doctest in [true, false] {
                let fixture = CompletionProofFixture::new()?;
                run_git(&fixture.repo_path, &["config", "core.autocrlf", "false"])?;
                let path = fixture.repo_path.join("src/inline_docs.rs");
                let eol_only_test_path = fixture.repo_path.join("src/eol_only_test.rs");
                let mixed_eol_path = fixture.repo_path.join("src/inline_eol.rs");
                let mixed_eol_source = "pub const VALUE: u8 = 1;\n#[cfg(test)]\nmod tests {\n    fn value() -> u8 { 4 }\n    #[test]\n    fn stable_value() { assert_eq!(value(), 4); }\n}\n";
                fs::write(&mixed_eol_path, mixed_eol_source.replace('\n', "\r\n"))?;
                let helper_module_path = fixture.repo_path.join("src/product_helpers.py");
                let baseline =
                    "//! ```\n//! assert_eq!(2 + 2, 4);\n//! ```\npub const VALUE: u8 = 1;\n";
                fs::write(&path, baseline)?;
                let eol_only_baseline =
                    b"#[test]\r\nfn preserves_behavior() {\r\n    assert_eq!(2 + 2, 4);\r\n}\r\n";
                fs::write(&eol_only_test_path, eol_only_baseline)?;
                fs::write(
                    &helper_module_path,
                    "def test_normalize_helper(value):\n    return value\n\nDEFAULT_LIMIT = 8\n",
                )?;
                run_git(
                    &fixture.repo_path,
                    &[
                        "add",
                        "src/inline_docs.rs",
                        "src/eol_only_test.rs",
                        "src/inline_eol.rs",
                        "src/product_helpers.py",
                    ],
                )?;
                run_git(
                    &fixture.repo_path,
                    &["commit", "--quiet", "--amend", "--no-edit"],
                )?;
                let stored_eol_only_test = Command::new("git")
                    .args(["show", "HEAD:src/eol_only_test.rs"])
                    .current_dir(&fixture.repo_path)
                    .env("GIT_OPTIONAL_LOCKS", "0")
                    .output()?;
                anyhow::ensure!(
                    stored_eol_only_test.status.success(),
                    "read committed EOL-only test: {}",
                    String::from_utf8_lossy(&stored_eol_only_test.stderr)
                );
                assert_eq!(
                    stored_eol_only_test.stdout.as_slice(),
                    eol_only_baseline.as_slice()
                );
                fs::write(
                    &path,
                    if changed_doctest {
                        baseline.replace("2 + 2, 4", "2 + 2, 5")
                    } else {
                        baseline.replace("VALUE: u8 = 1", "VALUE: u8 = 2")
                    },
                )?;
                let eol_only_current =
                    b"#[test]\nfn preserves_behavior() {\n    assert_eq!(2 + 2, 4);\n}\n";
                fs::write(&eol_only_test_path, eol_only_current)?;
                // The surrounding product changes, while the inline test and
                // its helper change only from CRLF to LF.
                fs::write(
                    &mixed_eol_path,
                    mixed_eol_source.replace("VALUE: u8 = 1", "VALUE: u8 = 2"),
                )?;
                fs::write(
                    &helper_module_path,
                    "def test_normalize_helper(value):\n    return value.strip()\n\nDEFAULT_LIMIT = 8\n",
                )?;
                assert_ne!(eol_only_baseline.as_slice(), eol_only_current.as_slice());
                assert_eq!(
                    String::from_utf8_lossy(eol_only_baseline).replace("\r\n", "\n"),
                    String::from_utf8_lossy(eol_only_current)
                );
                let eol_only_diff = Command::new("git")
                    .args(["diff", "--no-ext-diff", "--", "src/eol_only_test.rs"])
                    .current_dir(&fixture.repo_path)
                    .env("GIT_OPTIONAL_LOCKS", "0")
                    .output()?;
                anyhow::ensure!(
                    eol_only_diff.status.success(),
                    "inspect EOL-only worktree change: {}",
                    String::from_utf8_lossy(&eol_only_diff.stderr)
                );
                assert!(
                    !eol_only_diff.stdout.is_empty(),
                    "the EOL-only dedicated test must remain a visible worktree change"
                );
                // Line-ending-only edits do not change a dedicated test, and a
                // normal product helper named test_* is not a unittest test.
                // Actual inline declarations and changed doctests still count.
                fs::write(
                    fixture.repo_path.join("src/test_quality.rs"),
                    "pub const REVIEW_LIMIT: usize = 8;\n",
                )?;
                let harness = fixture.harness_with_raw_response_items().await?;
                let mut responses = vec![exec_command_call_response(
                    "scoped-pass",
                    &fixture.exact_focused_command(),
                    &fixture.repo_path,
                )];
                responses.extend(repeated_terminal_candidates(
                    if changed_doctest {
                        MAX_REGULAR_LOGICAL_GENERATIONS
                    } else {
                        1
                    },
                    "The focused check passed.",
                ));
                mount_sse_sequence(harness.server(), responses).await;
                let events = submit_and_collect(harness.test(), "Validate the focused source change. Changed doctests need their own trustworthy quality evidence; retain unchanged legacy tests.").await?;
                assert!(events.iter().any(|event| matches!(event, EventMsg::ExecCommandEnd(end) if end.call_id == "scoped-pass" && end.exit_code == 0)));
                assert!(events.iter().any(|event| matches!(event, EventMsg::TurnComplete(done) if done.error.is_some() == changed_doctest && done.last_agent_message.is_none() == changed_doctest)));
                if changed_doctest {
                    assert!(events.iter().all(|event| !is_assistant_output(event)));
                }
                assert!(!fixture.marker_path.exists());
            }
            Ok(())
        },
    )
}

fn active_inventory_platform_name() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    }
}

fn set_fixture_workspace(config: &mut Config, cwd: AbsolutePathBuf) {
    #[cfg(windows)]
    {
        // Every harness variant needs the same bounded sandbox temporary root,
        // including raw-response, resumed-session, and extension fixtures.
        let sandbox_temp = cwd.as_path().join(".fixture-state/sandbox-temp");
        fs::create_dir_all(&sandbox_temp).expect("create fixture sandbox temporary directory");
        for name in ["TEMP", "TMP"] {
            config.permissions.shell_environment_policy.r#set.insert(
                name.to_string(),
                sandbox_temp.to_string_lossy().into_owned(),
            );
        }
    }
    config.cwd = cwd.clone();
    let workspace_roots = vec![cwd];
    config.workspace_roots = workspace_roots.clone();
    config.permissions.set_workspace_roots(workspace_roots);
}

#[cfg(windows)]
fn differently_cased_windows_path(path: &Path) -> PathBuf {
    PathBuf::from(
        path.to_string_lossy()
            .chars()
            .map(|character| {
                if character.is_ascii_lowercase() {
                    character.to_ascii_uppercase()
                } else if character.is_ascii_uppercase() {
                    character.to_ascii_lowercase()
                } else {
                    character
                }
            })
            .collect::<String>(),
    )
}

fn failing_after_agent_command() -> Vec<String> {
    #[cfg(windows)]
    {
        vec!["cmd".to_string(), "/C".to_string(), "exit 7".to_string()]
    }
    #[cfg(not(windows))]
    {
        vec!["sh".to_string(), "-c".to_string(), "exit 7".to_string()]
    }
}

struct FileSignalOnDrop {
    path: PathBuf,
}

impl FileSignalOnDrop {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl Drop for FileSignalOnDrop {
    fn drop(&mut self) {
        let _ = fs::write(&self.path, "release\n");
    }
}

struct LegacyAssistantEventContributor;

impl ToolContributor for LegacyAssistantEventContributor {
    fn tools(
        &self,
        _session_store: &ExtensionData,
        _thread_store: &ExtensionData,
    ) -> Vec<Arc<dyn ToolExecutor<ExtensionToolCall>>> {
        vec![Arc::new(LegacyAssistantEventExecutor)]
    }
}

struct LegacyAssistantEventExecutor;

impl ToolExecutor<ExtensionToolCall> for LegacyAssistantEventExecutor {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(LEGACY_EXTENSION_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(ResponsesApiTool {
            name: LEGACY_EXTENSION_TOOL_NAME.to_string(),
            description: "Emits a legacy assistant event through a registered extension tool."
                .to_string(),
            strict: true,
            parameters: codex_extension_api::parse_tool_input_schema(&json!({
                "type": "object",
                "properties": {},
                "required": [],
                "additionalProperties": false,
            }))
            .expect("legacy-event extension schema should parse"),
            output_schema: None,
            defer_loading: None,
        })
    }

    fn handle(&self, call: ExtensionToolCall) -> codex_extension_api::ToolExecutorFuture<'_> {
        Box::pin(async move {
            call.turn_item_emitter
                .emit_started(ExtensionTurnItem {
                    item: ExtensionItem::WebSearch(WebSearchItem {
                        id: call.call_id.clone(),
                        query: "completion proof legacy event".to_string(),
                        action: None,
                    }),
                    legacy_events: vec![EventMsg::AgentMessage(AgentMessageEvent {
                        message: LEGACY_EXTENSION_PRIVATE_MESSAGE.to_string(),
                        phase: None,
                        memory_citation: None,
                    })],
                })
                .await;
            Ok(Box::new(JsonToolOutput::new(json!({ "ok": true }))) as Box<dyn ToolOutput>)
        })
    }
}

fn legacy_assistant_event_extensions() -> Arc<ExtensionRegistry<Config>> {
    let mut builder = ExtensionRegistryBuilder::new();
    builder.tool_contributor(Arc::new(LegacyAssistantEventContributor));
    Arc::new(builder.build())
}

struct RequiredToolFailureContributor;

impl ToolContributor for RequiredToolFailureContributor {
    fn tools(
        &self,
        _session_store: &ExtensionData,
        _thread_store: &ExtensionData,
    ) -> Vec<Arc<dyn ToolExecutor<ExtensionToolCall>>> {
        vec![Arc::new(RequiredToolFailureExecutor)]
    }
}

struct RequiredToolFailureExecutor;

impl ToolExecutor<ExtensionToolCall> for RequiredToolFailureExecutor {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(REQUIRED_FAILURE_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(ResponsesApiTool {
            name: REQUIRED_FAILURE_TOOL_NAME.to_string(),
            description: "Returns a registered required-tool rejection.".to_string(),
            strict: true,
            parameters: codex_extension_api::parse_tool_input_schema(&json!({
                "type": "object",
                "properties": {},
                "required": [],
                "additionalProperties": false,
            }))
            .expect("required-failure extension schema should parse"),
            output_schema: None,
            defer_loading: None,
        })
    }

    fn handle(&self, _call: ExtensionToolCall) -> codex_extension_api::ToolExecutorFuture<'_> {
        Box::pin(async move {
            Err(FunctionCallError::DeniedToModel(
                "registered required operation was rejected".to_string(),
            ))
        })
    }
}

fn required_tool_failure_extensions() -> Arc<ExtensionRegistry<Config>> {
    let mut builder = ExtensionRegistryBuilder::new();
    builder.tool_contributor(Arc::new(RequiredToolFailureContributor));
    Arc::new(builder.build())
}

type FixtureCredentials =
    Arc<std::sync::Mutex<Vec<codex_core::test_support::TemporaryCompletionProofCredential>>>;

fn retain_fixture_credential(credentials: &FixtureCredentials, config: &Config) {
    credentials.lock().expect("fixture credential scope").push(
        codex_core::test_support::temporary_completion_proof_credential(
            &config.codex_home,
            config.cwd.as_path(),
        )
        .expect("own only a new disposable fixture credential"),
    );
}

struct CompletionProofFixture {
    credentials: FixtureCredentials,
    _repo: TempDir,
    repo_path: PathBuf,
    marker_path: PathBuf,
    rejected_command_marker_path: PathBuf,
    mutate_restore_marker_path: PathBuf,
    canonical_command: String,
    focused_command: String,
    documentation_command: String,
    documentation_marker_path: PathBuf,
    documentation_json_mutation_command: String,
    documentation_markdown_mutation_command: String,
    mutation_command: String,
    restore_failed_input_command: String,
    mutation_and_commit_command: String,
    commit_existing_product_command: String,
    unrelated_mutation_and_commit_command: String,
    consolidate_validations_and_commit_command: String,
    post_proof_mutate_restore_command: String,
    rename_into_source_command: String,
    rename_into_documentation_command: String,
    no_op_command: String,
    rejected_command: String,
}

impl CompletionProofFixture {
    fn new() -> Result<Self> {
        Self::with_options(
            false,
            CanonicalRunnerAttestation::Valid,
            CanonicalAttemptMode::AlwaysPass,
            true,
        )
    }

    fn with_failed_focused_validation() -> Result<Self> {
        Self::with_options(
            true,
            CanonicalRunnerAttestation::Valid,
            CanonicalAttemptMode::AlwaysPass,
            true,
        )
    }

    fn with_missing_runner_attestation() -> Result<Self> {
        Self::with_options(
            false,
            CanonicalRunnerAttestation::Missing,
            CanonicalAttemptMode::AlwaysPass,
            true,
        )
    }

    fn with_mismatched_runner_identity() -> Result<Self> {
        Self::with_options(
            false,
            CanonicalRunnerAttestation::MismatchedReportIdentity,
            CanonicalAttemptMode::AlwaysPass,
            true,
        )
    }

    fn with_mismatched_validation_contract() -> Result<Self> {
        Self::with_options(
            false,
            CanonicalRunnerAttestation::MismatchedValidationContract,
            CanonicalAttemptMode::AlwaysPass,
            true,
        )
    }

    fn with_transient_canonical_pre_result() -> Result<Self> {
        Self::with_options(
            false,
            CanonicalRunnerAttestation::Valid,
            CanonicalAttemptMode::MissingAttestationFirstThenPass,
            true,
        )
    }

    fn with_confirmed_canonical_failure_then_pass() -> Result<Self> {
        Self::with_options(
            false,
            CanonicalRunnerAttestation::Valid,
            CanonicalAttemptMode::ConfirmedFailureFirstThenPass,
            true,
        )
    }

    fn with_confirmed_canonical_failure_then_infrastructure_error() -> Result<Self> {
        Self::with_options(
            false,
            CanonicalRunnerAttestation::Valid,
            CanonicalAttemptMode::ConfirmedFailureThenInfrastructureErrorFirstThenPass,
            true,
        )
    }

    fn without_documentation_validation() -> Result<Self> {
        Self::with_options(
            false,
            CanonicalRunnerAttestation::Valid,
            CanonicalAttemptMode::AlwaysPass,
            false,
        )
    }

    fn with_options(
        focused_failure: bool,
        canonical_attestation: CanonicalRunnerAttestation,
        canonical_attempt_mode: CanonicalAttemptMode,
        include_documentation_validation: bool,
    ) -> Result<Self> {
        let repo = TempDir::new().context("create completion-proof fixture repository")?;
        let repo_path = repo.path().to_path_buf();
        let marker_root = repo_path.join(".fixture-state");
        fs::create_dir_all(&marker_root)?;
        let marker_path = marker_root.join("canonical-command-launched");
        let rejected_command_marker_path = marker_root.join("rejected-command-launched");
        let mutate_restore_marker_path = marker_root.join("mutate-and-restore");
        let documentation_marker_path = marker_root.join("documentation-validated");
        let focused_failure_path = marker_root.join("focused-validation-fails");
        if focused_failure {
            fs::write(&focused_failure_path, "fail")?;
        }
        let python = available_python_command()?;
        let canonical_command = format!("{python} proof.py");
        let focused_command = format!("{python} focused.py {{validation_id}}");
        let documentation_command = format!("{python} validate_docs.py");
        let documentation_json_mutation_command = format!("{python} mutate_docs_json.py");
        let documentation_markdown_mutation_command = format!("{python} mutate_docs_markdown.py");
        let mutation_command = format!("{python} mutate.py");
        let restore_failed_input_command = format!("{python} restore_failed_input.py");
        let mutation_and_commit_command = format!("{python} mutate_and_commit.py");
        let commit_existing_product_command = format!("{python} commit_existing_product.py");
        let unrelated_mutation_and_commit_command =
            format!("{python} mutate_unrelated_and_commit.py");
        let consolidate_validations_and_commit_command =
            format!("{python} consolidate_validations_and_commit.py");
        let post_proof_mutate_restore_command = format!("{python} mutate_and_restore.py");
        let rename_into_source_command = format!("{python} rename_into_source.py");
        let rename_into_documentation_command = format!("{python} rename_into_documentation.py");
        let no_op_command = format!("{python} rewrite_identical.py");
        let rejected_command = format!("{python} rejected.py");

        fs::create_dir_all(repo_path.join(".codex/validation"))?;
        fs::create_dir_all(repo_path.join("src"))?;
        fs::write(repo_path.join(".gitignore"), "/.fixture-state/\n")?;

        let include_infrastructure_validation = matches!(
            canonical_attempt_mode,
            CanonicalAttemptMode::ConfirmedFailureThenInfrastructureErrorFirstThenPass
        );
        let mut inventory_rows = vec![json!({
            "baseline_id": FIXTURE_BASELINE_ID,
            "framework": "fixture",
            "ignored": false,
            "native_id": FIXTURE_BASELINE_ID,
            "platforms": ["windows"],
            "source": "src/runtime.rs",
        })];
        if include_infrastructure_validation {
            inventory_rows.push(json!({
                "baseline_id": FIXTURE_SECOND_BASELINE_ID,
                "framework": "fixture",
                "ignored": false,
                "native_id": FIXTURE_SECOND_BASELINE_ID,
                "platforms": ["windows"],
                "source": "infrastructure/runner.txt",
            }));
        }
        inventory_rows.sort_by(|left, right| {
            left["baseline_id"]
                .as_str()
                .cmp(&right["baseline_id"].as_str())
        });
        let inventory_hash = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&json!({
                "schema_version": 1,
                "tests": inventory_rows.clone(),
            }))?)
        );
        fs::write(
            repo_path.join(".codex/validation/frozen-test-inventory-v1.json"),
            serde_json::to_vec_pretty(&json!({
                "schema_version": 1,
                "inventory_hash": inventory_hash.clone(),
                "tests": inventory_rows,
            }))?,
        )?;

        let documentation_command_line = include_documentation_validation
            .then(|| {
                format!(
                    "documentation_command = {}\n",
                    serde_json::to_string(&documentation_command)
                        .expect("serialize documentation command")
                )
            })
            .unwrap_or_default();
        let trusted_bundle_paths = if include_documentation_validation {
            r#"["focused.py", "proof.py", "validate_docs.py"]"#
        } else {
            r#"["focused.py", "proof.py"]"#
        };
        let infrastructure_validation_config = include_infrastructure_validation
            .then(|| {
                format!(
                    r#"
[[validation]]
id = "{FIXTURE_SECOND_VALIDATION_ID}"
runner = "rust-gate"
gate = "fixture-infrastructure-gate"
owned_paths = ["infrastructure/**"]
consumed_paths = ["infrastructure/**"]
timeout_seconds = 30
"#
                )
            })
            .unwrap_or_default();
        fs::write(
            repo_path.join(".codex/validation/completion-proof.toml"),
            format!(
                r#"schema_version = 2
policy_id = "fixture.completion-proof.v1"
canonical_command = {canonical_command_literal}
focused_command = {focused_command_literal}
{documentation_command_line}frozen_inventory_hash = "{inventory_hash}"
trusted_bundle_paths = {trusted_bundle_paths}
trusted_runner_entrypoints = {trusted_bundle_paths}

[[validation]]
id = "{FIXTURE_VALIDATION_ID}"
runner = "rust-gate"
gate = "fixture-gate"
owned_paths = ["src/**"]
consumed_paths = ["src/**"]
timeout_seconds = 30
{infrastructure_validation_config}
"#,
                canonical_command_literal = serde_json::to_string(&canonical_command)?,
                focused_command_literal = serde_json::to_string(&focused_command)?,
            ),
        )?;
        let mut replacement_rows = vec![json!({
            "baseline_id": FIXTURE_BASELINE_ID,
            "resolution": "replacement",
            "replacement_ids": [FIXTURE_TEST_ID],
            "preserved_behavior": "the configured validation executes",
            "product_path": "the real shell/runtime session path",
            "validation_id": FIXTURE_VALIDATION_ID,
        })];
        if include_infrastructure_validation {
            replacement_rows.push(json!({
                "baseline_id": FIXTURE_SECOND_BASELINE_ID,
                "resolution": "replacement",
                "replacement_ids": [FIXTURE_SECOND_TEST_ID],
                "preserved_behavior": "the second configured validation executes",
                "product_path": "the real shell/runtime session path",
                "validation_id": FIXTURE_SECOND_VALIDATION_ID,
            }));
        }
        fs::write(
            repo_path.join(".codex/validation/test-replacements-v1.json"),
            serde_json::to_vec_pretty(&json!({
                "schema_version": 1,
                "frozen_inventory_hash": inventory_hash,
                "rows": replacement_rows,
                "overrides": [],
            }))?,
        )?;
        fs::write(
            repo_path.join("proof.py"),
            completion_proof_script(
                &canonical_command,
                &marker_path,
                &mutate_restore_marker_path,
                canonical_attestation,
                canonical_attempt_mode,
            ),
        )?;
        fs::write(
            repo_path.join("focused.py"),
            focused_completion_proof_script(&focused_command, &focused_failure_path),
        )?;
        fs::write(
            repo_path.join("consolidate_validations_and_commit.py"),
            r#"from pathlib import Path
import json
import subprocess

config_path = Path('.codex/validation/completion-proof.toml')
config = config_path.read_text(encoding='utf-8')
primary_marker = '[[validation]]\nid = "fixture.validation"\n'
second_marker = '[[validation]]\nid = "fixture.validation.infrastructure"\n'
primary_start = config.index(primary_marker)
second_start = config.index(second_marker)
if primary_start >= second_start:
    raise SystemExit('fixture validation blocks were not ordered as expected')
config_path.write_text(config[:primary_start] + config[second_start:], encoding='utf-8')

ledger_path = Path('.codex/validation/test-replacements-v1.json')
ledger = json.loads(ledger_path.read_text(encoding='utf-8'))
for row in ledger['rows']:
    row['replacement_ids'] = ['fixture.infrastructure-test']
    row['validation_id'] = 'fixture.validation.infrastructure'
ledger_path.write_text(json.dumps(ledger, indent=2), encoding='utf-8')

for script_name in ('proof.py', 'focused.py'):
    script_path = Path(script_name)
    script = script_path.read_text(encoding='utf-8')
    script = script.replace(
        'VALIDATION_ID = "fixture.validation"',
        'VALIDATION_ID = "fixture.validation.infrastructure"',
        1,
    )
    script = script.replace(
        'TEST_ID = "fixture.test"',
        'TEST_ID = "fixture.infrastructure-test"',
        1,
    )
    if script_name == 'proof.py':
        script_lines = script.splitlines()
        script_lines = [
            'ATTEMPT_MODE = "always-pass"' if line.startswith('ATTEMPT_MODE = ') else line
            for line in script_lines
        ]
        script = '\n'.join(script_lines) + '\n'
    script_path.write_text(script, encoding='utf-8')

subprocess.run(
    [
        'git',
        'add',
        '.codex/validation/completion-proof.toml',
        '.codex/validation/test-replacements-v1.json',
        'proof.py',
        'focused.py',
    ],
    check=True,
)
subprocess.run(
    [
        'git',
        '-c',
        'commit.gpgsign=false',
        'commit',
        '--quiet',
        '-m',
        'consolidate required validation',
    ],
    check=True,
)
"#,
        )?;
        if include_documentation_validation {
            fs::write(
                repo_path.join("validate_docs.py"),
                format!(
                    "from pathlib import Path\nwith Path({}).open('a', encoding='utf-8') as marker:\n    marker.write('validated\\n')\n",
                    serde_json::to_string(&documentation_marker_path.to_string_lossy())?
                ),
            )?;
        }
        fs::write(
            repo_path.join("mutate_docs_json.py"),
            "from pathlib import Path\nPath('docs/tracked.json').write_text('{\\\"changed\\\": true}\\n', encoding='utf-8')\n",
        )?;
        fs::write(
            repo_path.join("mutate_docs_markdown.py"),
            "from pathlib import Path\nPath('docs/tracked.md').write_text('# Tracked documentation\\n\\nUpdated.\\n', encoding='utf-8')\n",
        )?;
        fs::write(
            repo_path.join("rejected.py"),
            format!(
                "from pathlib import Path\nPath({}).write_text('launched', encoding='utf-8')\n",
                serde_json::to_string(&rejected_command_marker_path.to_string_lossy())?
            ),
        )?;
        fs::write(
            repo_path.join("mutate.py"),
            "from pathlib import Path\nPath('src/runtime.rs').write_text('pub const VALUE: u8 = 3;\\n', encoding='utf-8')\nfailure_marker = Path('.fixture-state/focused-validation-fails')\nif failure_marker.exists():\n    failure_marker.unlink()\n",
        )?;
        fs::write(
            repo_path.join("restore_failed_input.py"),
            "from pathlib import Path\nPath('src/runtime.rs').write_bytes(b'pub const VALUE: u8 = 2;\\n')\n",
        )?;
        fs::write(
            repo_path.join("mutate_and_commit.py"),
            "from pathlib import Path\nimport subprocess\nPath('src/runtime.rs').write_text('pub const VALUE: u8 = 3;\\n', encoding='utf-8')\nsubprocess.run(['git', 'add', 'src/runtime.rs'], check=True)\nsubprocess.run(['git', '-c', 'commit.gpgsign=false', 'commit', '--quiet', '-m', 'relevant correction'], check=True)\n",
        )?;
        fs::write(
            repo_path.join("commit_existing_product.py"),
            "import subprocess\nsubprocess.run(['git', 'add', 'src/runtime.rs'], check=True)\nsubprocess.run(['git', '-c', 'commit.gpgsign=false', 'commit', '--quiet', '-m', 'commit existing product bytes'], check=True)\n",
        )?;
        fs::write(
            repo_path.join("mutate_unrelated_and_commit.py"),
            "from pathlib import Path\nimport subprocess\nPath('notes.txt').write_text('unrelated change\\n', encoding='utf-8')\nsubprocess.run(['git', 'add', 'notes.txt'], check=True)\nsubprocess.run(['git', '-c', 'commit.gpgsign=false', 'commit', '--quiet', '-m', 'unrelated change'], check=True)\n",
        )?;
        fs::write(
            repo_path.join("mutate_and_restore.py"),
            "from pathlib import Path\npath = Path('src/runtime.rs')\ncontents = path.read_bytes()\npath.write_text('pub const VALUE: u8 = 9;\\n', encoding='utf-8')\npath.write_bytes(contents)\n",
        )?;
        fs::write(
            repo_path.join("rename_into_source.py"),
            "from pathlib import Path\nPath('docs/tracked.md').rename('src/renamed_runtime.rs')\n",
        )?;
        fs::write(
            repo_path.join("rename_into_documentation.py"),
            "from pathlib import Path\nPath('src/runtime.rs').rename('docs/renamed_runtime.rs')\n",
        )?;
        fs::write(
            repo_path.join("rewrite_identical.py"),
            "from pathlib import Path\npath = Path('src/runtime.rs')\ncontents = path.read_bytes()\npath.write_bytes(contents)\n",
        )?;
        fs::write(
            repo_path.join("mutate_eval.py"),
            "from pathlib import Path\npath = Path('.codex/evals/result.json')\npath.parent.mkdir(parents=True, exist_ok=True)\npath.write_text('{\\\"changed\\\": true}\\n', encoding='utf-8')\n",
        )?;
        fs::write(
            repo_path.join("src/runtime.rs"),
            "pub const VALUE: u8 = 1;\n",
        )?;
        if include_infrastructure_validation {
            fs::create_dir_all(repo_path.join("infrastructure"))?;
            fs::write(
                repo_path.join("infrastructure/runner.txt"),
                "fixture infrastructure validation input\n",
            )?;
        }
        fs::create_dir_all(repo_path.join("docs"))?;
        fs::write(
            repo_path.join("docs/tracked.md"),
            "# Tracked documentation\n",
        )?;
        fs::write(
            repo_path.join("docs/tracked.json"),
            "{\"changed\": false}\n",
        )?;

        run_git(&repo_path, &["init", "--quiet"])?;
        run_git(&repo_path, &["config", "core.autocrlf", "false"])?;
        run_git(&repo_path, &["config", "user.name", "KD4 Test"])?;
        run_git(
            &repo_path,
            &["config", "user.email", "kd4-test@example.invalid"],
        )?;
        run_git(&repo_path, &["add", "."])?;
        run_git(&repo_path, &["commit", "--quiet", "-m", "fixture baseline"])?;

        // Build the real session against a dirty non-documentation workspace so
        // terminal completion requires a current canonical artifact.
        fs::write(
            repo_path.join("src/runtime.rs"),
            "pub const VALUE: u8 = 2;\n",
        )?;

        Ok(Self {
            credentials: Default::default(),
            _repo: repo,
            repo_path,
            marker_path,
            rejected_command_marker_path,
            mutate_restore_marker_path,
            canonical_command,
            focused_command,
            documentation_command,
            documentation_marker_path,
            documentation_json_mutation_command,
            documentation_markdown_mutation_command,
            mutation_command,
            restore_failed_input_command,
            mutation_and_commit_command,
            commit_existing_product_command,
            unrelated_mutation_and_commit_command,
            consolidate_validations_and_commit_command,
            post_proof_mutate_restore_command,
            rename_into_source_command,
            rename_into_documentation_command,
            no_op_command,
            rejected_command,
        })
    }

    fn exact_focused_command(&self) -> String {
        self.focused_command
            .replace("{validation_id}", FIXTURE_VALIDATION_ID)
    }

    fn commit_current_product_state(&self) -> Result<()> {
        run_git(&self.repo_path, &["add", "src/runtime.rs"])?;
        run_git(
            &self.repo_path,
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "-m",
                "clean product state",
            ],
        )?;
        Ok(())
    }

    fn configure_validation_projection(
        &self,
        owned_paths: &[&str],
        consumed_paths: &[&str],
        path_set_paths: &[&str],
        evidence_path_manifests: &[&str],
        mutation_script: &str,
    ) -> Result<String> {
        anyhow::ensure!(!owned_paths.is_empty() && !consumed_paths.is_empty());
        let config_path = self
            .repo_path
            .join(".codex/validation/completion-proof.toml");
        let config = fs::read_to_string(&config_path)?;
        anyhow::ensure!(config.matches("owned_paths = [\"src/**\"]").count() == 1);
        anyhow::ensure!(config.matches("consumed_paths = [\"src/**\"]").count() == 1);
        let mut replacement = format!(
            "owned_paths = {}\nconsumed_paths = {}",
            serde_json::to_string(owned_paths)?,
            serde_json::to_string(consumed_paths)?,
        );
        if !path_set_paths.is_empty() {
            replacement.push_str(&format!(
                "\npath_set_paths = {}",
                serde_json::to_string(path_set_paths)?
            ));
        }
        if !evidence_path_manifests.is_empty() {
            replacement.push_str(&format!(
                "\nevidence_path_manifests = {}",
                serde_json::to_string(evidence_path_manifests)?
            ));
        }
        let config = config.replacen(
            "owned_paths = [\"src/**\"]\nconsumed_paths = [\"src/**\"]",
            &replacement,
            1,
        );
        fs::write(config_path, config)?;
        fs::write(
            self.repo_path.join("projection_mutation.py"),
            mutation_script,
        )?;
        run_git(&self.repo_path, &["add", "."])?;
        run_git(
            &self.repo_path,
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "-m",
                "configure validation projection fixture",
            ],
        )?;
        Ok(format!(
            "{} projection_mutation.py",
            available_python_command()?
        ))
    }

    fn remove_git_repository_before_session_start(&self) -> Result<()> {
        let git_path = self.repo_path.join(".git");
        anyhow::ensure!(
            git_path.is_dir(),
            "completion-proof fixture did not contain Git metadata"
        );
        fs::remove_dir_all(git_path).context("remove fixture Git metadata before session start")?;
        Ok(())
    }

    fn initialize_git_repository_after_session_start(&self) -> Result<()> {
        run_git(&self.repo_path, &["init", "--quiet"])?;
        run_git(&self.repo_path, &["config", "core.autocrlf", "false"])?;
        run_git(&self.repo_path, &["config", "user.name", "KD4 Test"])?;
        run_git(
            &self.repo_path,
            &["config", "user.email", "kd4-test@example.invalid"],
        )?;
        run_git(&self.repo_path, &["add", "."])?;
        run_git(
            &self.repo_path,
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "-m",
                "late repository initialization",
            ],
        )?;
        fs::write(
            self.repo_path.join("src/runtime.rs"),
            "pub const VALUE: u8 = 3;\n",
        )?;
        Ok(())
    }

    fn worktree_is_clean(&self) -> Result<bool> {
        let output = Command::new("git")
            .args(["status", "--porcelain=v1", "--untracked-files=all"])
            .current_dir(&self.repo_path)
            .output()?;
        anyhow::ensure!(output.status.success(), "git status failed in fixture");
        Ok(output.stdout.is_empty())
    }

    fn canonical_launch_count(&self) -> Result<usize> {
        if !self.marker_path.exists() {
            return Ok(0);
        }
        Ok(fs::read_to_string(&self.marker_path)?.lines().count())
    }

    fn documentation_launch_count(&self) -> Result<usize> {
        if !self.documentation_marker_path.exists() {
            return Ok(0);
        }
        Ok(fs::read_to_string(&self.documentation_marker_path)?
            .lines()
            .count())
    }

    fn delay_canonical_runner_for_yield(&self) -> Result<()> {
        const INSERTION_POINT: &str = "execution_id = str(uuid.uuid4())";

        let proof_path = self.repo_path.join("proof.py");
        let contents = fs::read_to_string(&proof_path)?;
        anyhow::ensure!(
            contents.contains(INSERTION_POINT),
            "completion-proof fixture delay insertion point is missing"
        );
        fs::write(
            &proof_path,
            contents.replacen(
                INSERTION_POINT,
                &format!("time.sleep(2)\n{INSERTION_POINT}"),
                1,
            ),
        )?;
        run_git(&self.repo_path, &["add", "proof.py"])?;
        run_git(
            &self.repo_path,
            &["commit", "--quiet", "--amend", "--no-edit"],
        )?;
        Ok(())
    }

    fn install_trusted_helper_authority_probe(&self) -> Result<String> {
        const TRUSTED_HELPER_SOURCE: &str = "VALUE = \"trusted\"\n";
        const CHANGED_HELPER_SOURCE: &str = "VALUE = \"changed during validation\"\n";
        const FOCUSED_INSERTION_POINT: &str =
            "execution_id = str(uuid.uuid4())\nlaunch_target_identity";
        const CANONICAL_INSERTION_POINT: &str =
            "execution_id = str(uuid.uuid4())\nvalidation, child_report";

        let config_path = self
            .repo_path
            .join(".codex/validation/completion-proof.toml");
        let config = fs::read_to_string(&config_path)?;
        let trusted_bundle_line = config
            .lines()
            .find(|line| line.starts_with("trusted_bundle_paths = ["))
            .context("completion-proof fixture trusted bundle declaration is missing")?;
        let trusted_bundle_prefix = trusted_bundle_line
            .strip_suffix(']')
            .context("completion-proof fixture trusted bundle declaration is malformed")?;
        let expanded_trusted_bundle_line =
            format!("{trusted_bundle_prefix}, \"trusted_helper.py\"]");
        let config = config.replacen(trusted_bundle_line, &expanded_trusted_bundle_line, 1);
        anyhow::ensure!(
            !config
                .lines()
                .find(|line| line.starts_with("trusted_runner_entrypoints = ["))
                .context("completion-proof fixture trusted entrypoint declaration is missing")?
                .contains("trusted_helper.py"),
            "source-only helper was added to executable entrypoints"
        );
        fs::write(&config_path, config)?;

        let focused_path = self.repo_path.join("focused.py");
        let focused = fs::read_to_string(&focused_path)?;
        anyhow::ensure!(
            focused.matches(FOCUSED_INSERTION_POINT).count() == 1,
            "focused fixture helper-mutation insertion point is not unique"
        );
        let focused_mutation = format!(
            "if failure:\n    Path(\"trusted_helper.py\").write_bytes({}.encode(\"utf-8\"))\n{FOCUSED_INSERTION_POINT}",
            serde_json::to_string(CHANGED_HELPER_SOURCE)?
        );
        fs::write(
            &focused_path,
            focused.replacen(FOCUSED_INSERTION_POINT, &focused_mutation, 1),
        )?;

        let proof_path = self.repo_path.join("proof.py");
        let proof = fs::read_to_string(&proof_path)?;
        anyhow::ensure!(
            proof.matches(CANONICAL_INSERTION_POINT).count() == 1,
            "canonical fixture helper-mutation insertion point is not unique"
        );
        let canonical_mutation = format!(
            "if primary_failure:\n    Path(\"trusted_helper.py\").write_bytes({}.encode(\"utf-8\"))\n{CANONICAL_INSERTION_POINT}",
            serde_json::to_string(CHANGED_HELPER_SOURCE)?
        );
        fs::write(
            &proof_path,
            proof.replacen(CANONICAL_INSERTION_POINT, &canonical_mutation, 1),
        )?;

        fs::write(
            self.repo_path.join("trusted_helper.py"),
            TRUSTED_HELPER_SOURCE,
        )?;
        fs::write(
            self.repo_path.join("restore_trusted_helper.py"),
            format!(
                "from pathlib import Path\nPath(\"trusted_helper.py\").write_bytes({}.encode(\"utf-8\"))\nfailure_marker = Path(\".fixture-state/focused-validation-fails\")\nif failure_marker.exists():\n    failure_marker.unlink()\n",
                serde_json::to_string(TRUSTED_HELPER_SOURCE)?
            ),
        )?;
        run_git(
            &self.repo_path,
            &[
                "add",
                "--",
                ".codex/validation/completion-proof.toml",
                "focused.py",
                "proof.py",
                "trusted_helper.py",
                "restore_trusted_helper.py",
            ],
        )?;
        run_git(
            &self.repo_path,
            &["commit", "--quiet", "--amend", "--no-edit"],
        )?;
        Ok(format!(
            "{} restore_trusted_helper.py",
            available_python_command()?
        ))
    }

    #[cfg(windows)]
    fn install_network_confinement_probe(
        &self,
        loopback_address: SocketAddr,
        non_loopback_address: SocketAddr,
    ) -> Result<()> {
        const INSERTION_POINT: &str = "execution_id = str(uuid.uuid4())";
        const CHILD_PROBE: &str = r#"import os
import socket
import sys

def address(value):
    host, port = value.rsplit(":", 1)
    return host, int(port)

blocked_proxy_names = {
    "http_proxy", "https_proxy", "all_proxy", "ftp_proxy",
    "ws_proxy", "wss_proxy", "no_proxy", "codex_network_proxy_active",
}

surviving_proxy_names = sorted(
    name for name in os.environ if name.lower() in blocked_proxy_names
)
if surviving_proxy_names:
    raise SystemExit("proxy environment survived confinement: " + ", ".join(surviving_proxy_names))

with socket.create_connection(address(sys.argv[1]), timeout=5):
    pass
try:
    with socket.create_connection(address(sys.argv[2]), timeout=2):
        pass
except OSError:
    sys.exit(0)
sys.exit(97)
"#;

        let proof_path = self.repo_path.join("proof.py");
        let contents = fs::read_to_string(&proof_path)?;
        anyhow::ensure!(
            contents.contains(INSERTION_POINT),
            "completion-proof fixture probe insertion point is missing"
        );
        let probe = format!(
            "probe = subprocess.run(\n    [sys.executable, \"-c\", {child_probe}, {loopback}, {non_loopback}],\n    stdout=subprocess.PIPE,\n    stderr=subprocess.PIPE,\n)\nif probe.returncode != 0:\n    raise SystemExit(f\"network confinement probe failed: {{probe.returncode}} {{probe.stderr.decode(errors='replace')}}\")\n{INSERTION_POINT}",
            child_probe = serde_json::to_string(CHILD_PROBE)?,
            loopback = serde_json::to_string(&loopback_address.to_string())?,
            non_loopback = serde_json::to_string(&non_loopback_address.to_string())?,
        );
        fs::write(proof_path, contents.replacen(INSERTION_POINT, &probe, 1))?;
        // Canonical runner entrypoints are trusted from their committed bytes.
        // Keep this test-only probe inside that boundary before the session
        // loads its authority; the fixture's dirty product source remains
        // unstaged and still requires certification.
        run_git(&self.repo_path, &["add", "proof.py"])?;
        run_git(
            &self.repo_path,
            &["commit", "--quiet", "--amend", "--no-edit"],
        )?;
        Ok(())
    }

    fn prepare_documentation_only_change(&self) -> Result<()> {
        fs::write(
            self.repo_path.join("src/runtime.rs"),
            "pub const VALUE: u8 = 1;\n",
        )?;
        fs::create_dir_all(self.repo_path.join("docs"))?;
        fs::write(
            self.repo_path.join("docs/completion-policy.md"),
            "# Completion policy\n\nDocumentation-only change.\n",
        )?;
        fs::write(
            self.repo_path.join("README.md"),
            "# Fixture repository\n\nDocumentation-only change.\n",
        )?;
        fs::write(
            self.repo_path.join("CHANGELOG.rst"),
            "Fixture changes\n===============\n\nDocumentation-only change.\n",
        )?;
        fs::create_dir_all(self.repo_path.join("sdk/typescript"))?;
        fs::write(
            self.repo_path.join("sdk/typescript/README.md"),
            "# TypeScript SDK\n\nNested documentation-only change.\n",
        )?;
        Ok(())
    }

    fn prepare_source_markdown_and_schema_changes(&self) -> Result<()> {
        fs::write(
            self.repo_path.join("src/runtime.rs"),
            "pub const VALUE: u8 = 1;\n",
        )?;
        fs::create_dir_all(self.repo_path.join("codex-rs/core/src"))?;
        fs::create_dir_all(self.repo_path.join(".codex/skills/reviewer"))?;
        fs::create_dir_all(self.repo_path.join("tests"))?;
        fs::write(
            self.repo_path.join("codex-rs/core/src/lib.rs"),
            "pub fn runtime_contract() {}\n",
        )?;
        fs::write(
            self.repo_path.join("codex-rs/core/prompt.md"),
            "# Runtime prompt contract\n",
        )?;
        fs::write(
            self.repo_path.join(".codex/skills/reviewer/SKILL.md"),
            "# Runtime skill contract\n",
        )?;
        fs::write(self.repo_path.join("requirements.txt"), "runtime-package\n")?;
        fs::write(self.repo_path.join("AGENTS.md"), "# Runtime instructions\n")?;
        fs::write(self.repo_path.join("SOURCEMAP.md"), "# Runtime ownership\n")?;
        fs::write(
            self.repo_path.join("tests/README.rs"),
            "fn test_contract() {}\n",
        )?;
        fs::write(
            self.repo_path.join("src/runtime.schema.json"),
            "{\"type\":\"object\"}\n",
        )?;
        Ok(())
    }

    fn prepare_isolated_non_documentation_change(
        &self,
        relative_path: &str,
        contents: &str,
    ) -> Result<()> {
        fs::write(
            self.repo_path.join("src/runtime.rs"),
            "pub const VALUE: u8 = 1;\n",
        )?;
        let path = self.repo_path.join(relative_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, contents)?;
        Ok(())
    }

    fn enable_mutate_and_restore_during_canonical(&self) -> Result<()> {
        fs::write(&self.mutate_restore_marker_path, "always")?;
        Ok(())
    }

    fn enable_mutate_and_restore_after_first_canonical(&self) -> Result<()> {
        fs::write(&self.mutate_restore_marker_path, "after-first")?;
        Ok(())
    }

    fn install_focused_failure_unrelated_workspace_drift(&self) -> Result<String> {
        let focused_path = self.repo_path.join("focused.py");
        let focused_script = fs::read_to_string(&focused_path)?;
        let child_completion = "child.communicate()\nended_at = time.time_ns()";
        anyhow::ensure!(focused_script.matches(child_completion).count() == 1);
        fs::write(
            &focused_path,
            focused_script.replacen(
                child_completion,
                "child.communicate()\nif failure:\n    Path('notes.txt').write_text('changed note\\n', encoding='utf-8')\nended_at = time.time_ns()",
                1,
            ),
        )?;
        fs::write(self.repo_path.join("notes.txt"), "initial note\n")?;
        fs::write(
            self.repo_path.join("restore_unrelated_focused_drift.py"),
            "from pathlib import Path\nPath('notes.txt').write_text('initial note\\n', encoding='utf-8')\nfailure_marker = Path('.fixture-state/focused-validation-fails')\nif failure_marker.exists():\n    failure_marker.unlink()\n",
        )?;
        run_git(
            &self.repo_path,
            &[
                "add",
                "focused.py",
                "notes.txt",
                "restore_unrelated_focused_drift.py",
            ],
        )?;
        run_git(
            &self.repo_path,
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "-m",
                "install focused drift probe",
            ],
        )?;
        Ok(format!(
            "{} restore_unrelated_focused_drift.py",
            available_python_command()?
        ))
    }

    fn enable_mutate_and_restore_during_first_canonical(&self) -> Result<()> {
        let proof_path = self.repo_path.join("proof.py");
        let proof_script = fs::read_to_string(&proof_path)?;
        let mutate_condition = r#"if mutate_restore_mode == "always" or (
    mutate_restore_mode == "after-first" and prior_launch_count >= 1
):"#;
        anyhow::ensure!(proof_script.matches(mutate_condition).count() == 1);
        anyhow::ensure!(proof_script.matches("time.sleep(0.2)").count() == 2);
        let escaped_mutation =
            r#"runtime_path.write_bytes(original + b"\\n// transient canonical mutation\\n")"#;
        anyhow::ensure!(proof_script.matches(escaped_mutation).count() == 1);
        let proof_script = proof_script
            .replacen(
                mutate_condition,
                r#"if mutate_restore_mode == "always" or (
    mutate_restore_mode == "first-only" and prior_launch_count == 0
) or (
    mutate_restore_mode == "after-first" and prior_launch_count >= 1
):"#,
                1,
            )
            .replace("time.sleep(0.2)", "time.sleep(2.0)")
            .replacen(
                escaped_mutation,
                r#"runtime_path.write_bytes(original + b"\n// transient canonical mutation\n")"#,
                1,
            )
            .replacen(
                "runtime_path = Path(\"src/runtime.rs\")",
                "runtime_path = Path(\"scratch/observer-probe.txt\") if mutate_restore_mode == \"first-only\" else Path(\"src/runtime.rs\")",
                1,
            )
            .replacen(
                "    runtime_path.write_bytes(original)\n    time.sleep(2.0)",
                "    if mutate_restore_mode != \"first-only\":\n        runtime_path.write_bytes(original)\n    time.sleep(2.0)",
                1,
            );
        fs::write(&proof_path, proof_script)?;
        fs::create_dir_all(self.repo_path.join("scratch"))?;
        fs::write(
            self.repo_path.join("scratch/observer-probe.txt"),
            "initial probe\n",
        )?;
        run_git(
            &self.repo_path,
            &["add", "proof.py", "scratch/observer-probe.txt"],
        )?;
        run_git(
            &self.repo_path,
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "-m",
                "install first-only canonical drift probe",
            ],
        )?;
        fs::write(&self.mutate_restore_marker_path, "first-only")?;
        Ok(())
    }

    fn make_later_infrastructure_result_a_malformed_claimed_failure(&self) -> Result<()> {
        let proof_path = self.repo_path.join("proof.py");
        let proof_script = fs::read_to_string(&proof_path)?;
        let classification = r#""classification": "pre_result_error","#;
        anyhow::ensure!(proof_script.matches(classification).count() == 1);
        fs::write(
            &proof_path,
            proof_script.replacen(
                classification,
                r#""classification": "confirmed_validation_failure","#,
                1,
            ),
        )?;
        run_git(&self.repo_path, &["add", "proof.py"])?;
        run_git(
            &self.repo_path,
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "-m",
                "install malformed claimed failure fixture",
            ],
        )?;
        Ok(())
    }

    fn add_unknown_validation_field(&self) -> Result<()> {
        let path = self
            .repo_path
            .join(".codex/validation/completion-proof.toml");
        let contents = fs::read_to_string(&path)?;
        anyhow::ensure!(
            contents.contains("gate = \"fixture-gate\""),
            "fixture config did not contain the expected Rust gate"
        );
        fs::write(
            path,
            contents.replace(
                "gate = \"fixture-gate\"",
                "gate = \"fixture-gate\"\nforged_selection = true",
            ),
        )?;
        Ok(())
    }

    fn add_malformed_policy_addition(&self) -> Result<()> {
        let path = self
            .repo_path
            .join(".codex/validation/test-replacements-v1.json");
        let mut ledger: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
        ledger["additions"] = json!([{
            "test_id": "fixture.added-policy-test",
            "preserved_behavior": "the compiled gate rejects malformed policy additions",
            "product_path": "the real shell and terminal publication path",
            "validation_id": FIXTURE_VALIDATION_ID,
            "provenance": {
                "kind": "policy-addition",
                "source": "locked completion-proof plan",
                "text": "added after the frozen fixture baseline",
                "forged": true,
            },
        }]);
        fs::write(path, serde_json::to_vec_pretty(&ledger)?)?;
        Ok(())
    }

    fn replace_baseline_with_exception(
        &self,
        kind: &str,
        ignored: bool,
        platforms: &[&str],
    ) -> Result<()> {
        let inventory_path = self
            .repo_path
            .join(".codex/validation/frozen-test-inventory-v1.json");
        let mut inventory: serde_json::Value = serde_json::from_slice(&fs::read(&inventory_path)?)?;
        let old_hash = inventory["inventory_hash"]
            .as_str()
            .context("fixture frozen inventory omitted its hash")?
            .to_string();
        let inventory_rows = inventory["tests"]
            .as_array_mut()
            .context("fixture frozen inventory omitted its rows")?;
        anyhow::ensure!(
            inventory_rows.len() == 1,
            "exception fixture requires exactly one frozen row"
        );
        inventory_rows[0]["ignored"] = json!(ignored);
        inventory_rows[0]["platforms"] = json!(platforms);
        let normalized_rows = inventory_rows.clone();
        let new_hash = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&json!({
                "schema_version": 1,
                "tests": normalized_rows,
            }))?)
        );
        inventory["inventory_hash"] = json!(new_hash.clone());
        fs::write(&inventory_path, serde_json::to_vec_pretty(&inventory)?)?;

        let provenance = json!({
            "kind": kind,
            "source": "completion-proof gate fixture",
            "text": "the fixture exception remains explicitly quarantined",
        });
        let ledger_path = self
            .repo_path
            .join(".codex/validation/test-replacements-v1.json");
        let mut ledger: serde_json::Value = serde_json::from_slice(&fs::read(&ledger_path)?)?;
        ledger["frozen_inventory_hash"] = json!(new_hash.clone());
        let ledger_rows = ledger["rows"]
            .as_array_mut()
            .context("fixture replacement ledger omitted its rows")?;
        anyhow::ensure!(
            ledger_rows.len() == 1,
            "exception fixture requires exactly one replacement row"
        );
        ledger_rows[0] = json!({
            "baseline_id": FIXTURE_BASELINE_ID,
            "resolution": "exception",
            "provenance": provenance,
        });
        fs::write(&ledger_path, serde_json::to_vec_pretty(&ledger)?)?;

        let config_path = self
            .repo_path
            .join(".codex/validation/completion-proof.toml");
        let config = fs::read_to_string(&config_path)?;
        let old_hash_line = format!("frozen_inventory_hash = \"{old_hash}\"");
        anyhow::ensure!(
            config.matches(&old_hash_line).count() == 1,
            "fixture config did not contain exactly one old frozen hash"
        );
        let config = config.replace(
            &old_hash_line,
            &format!("frozen_inventory_hash = \"{new_hash}\""),
        );
        let exception_rule = format!(
            r#"
[[baseline_exception]]
id_prefix = {baseline_id}
kind = {kind}
source = "completion-proof gate fixture"
text = "the fixture exception remains explicitly quarantined"
"#,
            baseline_id = serde_json::to_string(FIXTURE_BASELINE_ID)?,
            kind = serde_json::to_string(kind)?,
        );
        fs::write(config_path, format!("{config}{exception_rule}"))?;
        run_git(
            &self.repo_path,
            &[
                "add",
                "--",
                ".codex/validation/frozen-test-inventory-v1.json",
                ".codex/validation/test-replacements-v1.json",
                ".codex/validation/completion-proof.toml",
            ],
        )?;
        run_git(
            &self.repo_path,
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "-m",
                "trusted exception fixture",
            ],
        )?;
        Ok(())
    }

    async fn harness(&self) -> Result<TestCodexHarness> {
        let cwd = self.repo_path.abs();
        let credentials = Arc::clone(&self.credentials);
        TestCodexHarness::with_builder(test_codex().with_config(move |config| {
            set_fixture_workspace(config, cwd);
            retain_fixture_credential(&credentials, config);
        }))
        .await
    }

    async fn harness_with_raw_response_items(&self) -> Result<TestCodexHarness> {
        let cwd = self.repo_path.abs();
        let credentials = Arc::clone(&self.credentials);
        TestCodexHarness::with_builder(test_codex().with_raw_response_items().with_config(
            move |config| {
                set_fixture_workspace(config, cwd);
                retain_fixture_credential(&credentials, config);
            },
        ))
        .await
    }

    async fn harness_with_raw_response_items_and_extensions(
        &self,
        extensions: Arc<ExtensionRegistry<Config>>,
    ) -> Result<TestCodexHarness> {
        let cwd = self.repo_path.abs();
        let credentials = Arc::clone(&self.credentials);
        TestCodexHarness::with_builder(
            test_codex()
                .with_raw_response_items()
                .with_extensions(extensions)
                .with_config(move |config| {
                    set_fixture_workspace(config, cwd);
                    retain_fixture_credential(&credentials, config);
                }),
        )
        .await
    }

    async fn harness_with_home(&self, home: Arc<TempDir>) -> Result<TestCodexHarness> {
        let cwd = self.repo_path.abs();
        let credentials = Arc::clone(&self.credentials);
        TestCodexHarness::with_builder(test_codex().with_home(home).with_config(move |config| {
            set_fixture_workspace(config, cwd);
            retain_fixture_credential(&credentials, config);
        }))
        .await
    }

    async fn harness_with_home_and_raw_response_items(
        &self,
        home: Arc<TempDir>,
    ) -> Result<TestCodexHarness> {
        let cwd = self.repo_path.abs();
        let credentials = Arc::clone(&self.credentials);
        TestCodexHarness::with_builder(
            test_codex()
                .with_home(home)
                .with_raw_response_items()
                .with_config(move |config| {
                    set_fixture_workspace(config, cwd);
                    retain_fixture_credential(&credentials, config);
                }),
        )
        .await
    }

    async fn multi_agent_harness(&self) -> Result<TestCodexHarness> {
        let cwd = self.repo_path.abs();
        let credentials = Arc::clone(&self.credentials);
        TestCodexHarness::with_builder(test_codex().with_config(move |config| {
            set_fixture_workspace(config, cwd);
            retain_fixture_credential(&credentials, config);
            config
                .features
                .enable(Feature::Collab)
                .expect("test config should allow collaboration");
            config
                .features
                .enable(Feature::MultiAgentV2)
                .expect("test config should allow multi-agent v2");
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(0);
            config.model_provider.supports_websockets = false;
        }))
        .await
    }
}

#[cfg(windows)]
struct CurrentEvidenceFixture {
    credentials: FixtureCredentials,
    _repo: TempDir,
    _outside: TempDir,
    repo_path: PathBuf,
    outside_path: PathBuf,
    exact_command: String,
    reconciliation_command: String,
    canonical_marker: PathBuf,
    unit_marker: PathBuf,
    pytest_marker: PathBuf,
    control_path: PathBuf,
    barrier_hold: PathBuf,
    barrier_ready: PathBuf,
    driver_started: PathBuf,
    driver_diagnostic: PathBuf,
    historical_review_plan: PathBuf,
}

#[cfg(windows)]
impl CurrentEvidenceFixture {
    fn new() -> Result<Self> {
        Self::with_resolved_reconciliation(false)
    }

    fn new_resolved() -> Result<Self> {
        Self::with_resolved_reconciliation(true)
    }

    fn new_historical_resolved() -> Result<Self> {
        Self::with_materialization_mode("historical")
    }

    fn with_resolved_reconciliation(resolved: bool) -> Result<Self> {
        Self::with_materialization_mode(if resolved { "resolved" } else { "unresolved" })
    }

    fn with_materialization_mode(mode: &str) -> Result<Self> {
        let repo = TempDir::new().context("create current-evidence fixture repository")?;
        let outside = TempDir::new().context("create current-evidence fixture control root")?;
        let repo_path = repo.path().to_path_buf();
        let outside_path = outside.path().to_path_buf();
        let source_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .context("resolve KD4 source root for current-evidence fixture")?;
        let materializer = outside_path.join("materialize.py");
        fs::write(&materializer, CURRENT_EVIDENCE_MATERIALIZER)?;
        let output = Command::new(available_python_command()?)
            .arg(&materializer)
            .arg(&source_root)
            .arg(&repo_path)
            .arg(&outside_path)
            .arg(mode)
            .output()
            .context("materialize bounded current-evidence fixture")?;
        anyhow::ensure!(
            output.status.success(),
            "current-evidence fixture materializer failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(Self {
            credentials: Default::default(),
            _repo: repo,
            _outside: outside,
            repo_path: repo_path.clone(),
            exact_command: "just completion-focused inventory.current-evidence".to_string(),
            reconciliation_command: "just completion-focused inventory.frozen-reconciliation"
                .to_string(),
            canonical_marker: outside_path.join("canonical-launched.txt"),
            unit_marker: outside_path.join("unittest-executions.txt"),
            pytest_marker: outside_path.join("pytest-executions.txt"),
            control_path: outside_path.join("control.txt"),
            barrier_hold: outside_path.join("report.hold"),
            barrier_ready: outside_path.join("report.ready"),
            driver_started: outside_path.join("driver.started"),
            driver_diagnostic: outside_path.join("driver.diagnostic"),
            historical_review_plan: repo_path.join(".fixture-state/historical-review-plan.json"),
            outside_path,
        })
    }

    fn driver_diagnostics(&self) -> String {
        let started = self.driver_started.exists();
        let diagnostic = fs::read_to_string(&self.driver_diagnostic)
            .unwrap_or_else(|error| format!("unavailable ({error})"));
        format!("driver_started={started}; driver_diagnostic={diagnostic}")
    }

    async fn harness(&self) -> Result<TestCodexHarness> {
        self.harness_with_home(Arc::new(TempDir::new()?)).await
    }

    async fn harness_with_home(&self, home: Arc<TempDir>) -> Result<TestCodexHarness> {
        self.harness_with_home_and_features(home, false).await
    }

    async fn historical_harness_with_home(&self, home: Arc<TempDir>) -> Result<TestCodexHarness> {
        self.harness_with_home_and_features(home, true).await
    }

    async fn harness_with_home_and_features(
        &self,
        home: Arc<TempDir>,
        multi_agent_v2: bool,
    ) -> Result<TestCodexHarness> {
        let cwd = self.repo_path.abs();
        let credentials = Arc::clone(&self.credentials);
        let outside = self.outside_path.abs();
        let fake_bin = self.repo_path.join(".fixture-bin");
        let sandbox_temp = self.repo_path.join(".fixture-state/sandbox-temp");
        let path = format!(
            "{};{}",
            fake_bin.to_string_lossy(),
            std::env::var("PATH").unwrap_or_default()
        );
        TestCodexHarness::with_builder(
            test_codex()
                .with_home(home)
                .with_raw_response_items()
                .with_extensions(required_tool_failure_extensions())
                .with_config(move |config| {
                    if multi_agent_v2 {
                        config
                            .features
                            .enable(Feature::Collab)
                            .expect("historical fixture enables collaboration");
                        config
                            .features
                            .enable(Feature::MultiAgentV2)
                            .expect("historical fixture enables typed agents");
                        config.multi_agent_v2.tool_namespace = Some("agents".to_string());
                    }
                    config.cwd = cwd.clone();
                    retain_fixture_credential(&credentials, config);
                    let roots = vec![cwd.clone(), outside.clone()];
                    config.workspace_roots = roots.clone();
                    config.permissions.set_workspace_roots(roots);
                    for (name, value) in [
                        ("PATH", path.clone()),
                        ("TEMP", sandbox_temp.to_string_lossy().into_owned()),
                        ("TMP", sandbox_temp.to_string_lossy().into_owned()),
                    ] {
                        config
                            .permissions
                            .shell_environment_policy
                            .r#set
                            .insert(name.to_string(), value);
                    }
                }),
        )
        .await
    }

    fn historical_review_plan(&self) -> Result<serde_json::Value> {
        serde_json::from_slice(&fs::read(&self.historical_review_plan)?)
            .context("parse fixture historical review plan")
    }

    fn historical_scope_ids(&self) -> Result<Vec<String>> {
        self.historical_review_plan()?["review_scopes"]
            .as_array()
            .context("historical review plan omitted review scopes")?
            .iter()
            .map(|scope| {
                scope["review_scope_id"]
                    .as_str()
                    .map(str::to_owned)
                    .context("historical review scope omitted its ID")
            })
            .collect()
    }

    fn write_historical_proposal(
        &self,
        relative_path: &str,
        approval: &FocusedReplacementApprovalReceiptV1,
        stale_receipt_ref: bool,
    ) -> Result<HistoricalReplacementAcceptanceProposalV1> {
        let plan = self.historical_review_plan()?;
        let mut scope_reviews = plan["review_scopes"]
            .as_array()
            .context("historical review plan omitted review scopes")?
            .iter()
            .map(|scope| {
                Ok(HistoricalReplacementScopeReviewV1 {
                    review_scope_id: scope["review_scope_id"]
                        .as_str()
                        .context("historical review scope omitted its ID")?
                        .to_owned(),
                    review_scope_sha256: Sha256HexV1::parse(
                        scope["review_scope_sha256"]
                            .as_str()
                            .context("historical review scope omitted its hash")?
                            .to_owned(),
                    )?,
                    disposition:
                        HistoricalReplacementScopeReviewDispositionV1::ReviewedNoIncorrectBehavior,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        scope_reviews.sort_by(|left, right| left.review_scope_id.cmp(&right.review_scope_id));
        let mut proposal = HistoricalReplacementAcceptanceProposalV1 {
            format_id: HistoricalReplacementAcceptanceProposalV1::FORMAT_ID.to_owned(),
            schema_version: 1,
            frozen_graph_sha256: Sha256HexV1::parse(
                HistoricalReplacementAcceptanceProposalV1::FROZEN_GRAPH_SHA256.to_owned(),
            )?,
            review_plan_sha256: Sha256HexV1::parse(
                plan["review_plan_sha256"]
                    .as_str()
                    .context("historical review plan omitted its hash")?
                    .to_owned(),
            )?,
            baseline_count: HistoricalReplacementAcceptanceProposalV1::BASELINE_COUNT,
            edge_count: HistoricalReplacementAcceptanceProposalV1::EDGE_COUNT,
            successor_count: HistoricalReplacementAcceptanceProposalV1::SUCCESSOR_COUNT,
            review_scope_count: HistoricalReplacementAcceptanceProposalV1::REVIEW_SCOPE_COUNT,
            focused_replacement_approval_receipt_ref: FocusedReplacementApprovalReceiptRefV1 {
                format_id: FocusedReplacementApprovalReceiptV1::FORMAT_ID.to_owned(),
                schema_version: 1,
                attempt_id: if stale_receipt_ref {
                    "stale-focused-reconciliation-attempt".to_owned()
                } else {
                    approval.attempt_id.clone()
                },
                focused_validation_id: approval.focused_validation_id.clone(),
                receipt_sha256: approval.receipt_sha256.clone(),
            },
            scope_review_set_sha256: proof_hash(
                HistoricalReplacementAcceptanceProposalV1::SCOPE_REVIEW_SET_HASH_DOMAIN,
                &scope_reviews,
            )?,
            scope_reviews,
            activation_authority: MustBeNullV1,
            proposal_sha256: Sha256HexV1::parse("0".repeat(64))?,
        };
        proposal.proposal_sha256 = proposal.proposal_sha256()?;
        if !stale_receipt_ref {
            let predecessor: serde_json::Value = serde_json::from_slice(include_bytes!(
                "../../../../.codex/validation/test-replacements-v1.json"
            ))?;
            proposal.validate(&predecessor, approval)?;
        }
        let path = self.repo_path.join(relative_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, canonical_jcs_of(&proposal)?)?;
        Ok(proposal)
    }
}

#[cfg(windows)]
const CURRENT_EVIDENCE_MATERIALIZER: &str = r###"
import hashlib
import json
import os
import pathlib
import runpy
import shutil
import subprocess
import sys
import tempfile

source = pathlib.Path(sys.argv[1])
repo = pathlib.Path(sys.argv[2])
outside = pathlib.Path(sys.argv[3])
mode = sys.argv[4]
resolved = mode in {"resolved", "historical"}
historical = mode == "historical"

historical_rows = []
historical_successor_rows = []
historical_review_plan = None
if historical:
    source_ledger = json.loads(
        (source / ".codex/validation/test-replacements-v1.json").read_text(
            encoding="utf-8"
        )
    )
    historical_rows = [
        dict(row)
        for row in source_ledger["rows"]
        if row.get("resolution") == "replacement"
    ]
    historical_successor_ids = sorted(
        {
            successor_id
            for row in historical_rows
            for successor_id in row["replacement_ids"]
        }
    )
    if (
        len(historical_rows) != 644
        or sum(len(row["replacement_ids"]) for row in historical_rows) != 685
        or len(historical_successor_ids) != 572
    ):
        raise RuntimeError("source historical replacement graph changed")
    historical_successor_rows = [
        {
            "baseline_id": successor_id,
            "framework": "rust-nextest",
            "native_id": f"fixture-history::{successor_id}",
            "source": "src/historical.rs",
            "ignored": False,
            "platforms": ["windows"],
        }
        for successor_id in historical_successor_ids
    ]
    original_sys_path = list(sys.path)
    sys.path.insert(0, str(source / "scripts"))
    try:
        inventory_module = runpy.run_path(
            str(source / "scripts/completion_proof_inventory_v2.py")
        )
        admission_module = runpy.run_path(
            str(source / "scripts/replacement_admission.py")
        )
        graph = inventory_module[
            "derive_frozen_v1_historical_replacement_graph_v1"
        ](source_ledger)
        historical_review_plan = admission_module[
            "_historical_replacement_review_plan_v1"
        ](graph)
    finally:
        sys.path[:] = original_sys_path

def write(relative, content):
    path = repo / relative
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")

def copy(relative):
    path = repo / relative
    path.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(source / relative, path)

for relative in (
    "scripts/completion_proof.py",
    "scripts/completion_proof_canonical.py",
    "scripts/completion_proof_inventory_v2.py",
    "scripts/current_evidence_successor_projection.py",
    "scripts/completion_proof_unittest.py",
    "scripts/completion_proof_pytest.py",
    "scripts/focused_live_successor_catalog.py",
    "scripts/bounded_process.py",
    "scripts/child_validation_report.py",
    "scripts/pyproject.toml",
    "scripts/uv.lock",
    "sdk/python/pyproject.toml",
    "sdk/python/uv.lock",
    "sdk/python/README.md",
):
    copy(relative)
shutil.copytree(source / "sdk/python/src", repo / "sdk/python/src")

write(".gitignore", "/.fixture-state/\n**/.venv/\n**/.pytest_cache/\n**/__pycache__/\n")
(repo / ".fixture-state/sandbox-temp").mkdir(parents=True)
write("scripts/root_maintenance.py", "def python_unittest_targets():\n    return ['scripts.test_runtime_path']\n")
write("scripts/test_runtime_path.py", f'''import pathlib
import unittest
import uuid
MARKER = pathlib.Path({str(outside / "unittest-executions.txt")!r})
CONTROL = pathlib.Path({str(outside / "control.txt")!r})
class RuntimePathTest(unittest.TestCase):
    def test_runs(self):
        with MARKER.open("a", encoding="utf-8") as output:
            output.write(str(uuid.uuid4()) + "\\n")
        if CONTROL.exists() and "unit-fail" in CONTROL.read_text(encoding="utf-8"):
            self.fail("fixture unittest failure")
''')
write("sdk/python/tests/test_runtime_path.py", f'''import pathlib
import uuid
import pytest
MARKER = pathlib.Path({str(outside / "pytest-executions.txt")!r})
CONTROL = pathlib.Path({str(outside / "control.txt")!r})
def skipped():
    return CONTROL.exists() and "pytest-skip" in CONTROL.read_text(encoding="utf-8")
@pytest.mark.skipif(skipped(), reason="fixture pre-result skip")
def test_runs():
    with MARKER.open("a", encoding="utf-8") as output:
        output.write(str(uuid.uuid4()) + "\\n")
''')
write("sdk/typescript/tests/runtime_path.test.ts", 'test("inventory path", () => {});\n')
write("codex-rs/Cargo.toml", '[workspace]\nmembers = ["fixture"]\nresolver = "2"\n')
write("codex-rs/fixture/Cargo.toml", '[package]\nname = "fixture-package"\nversion = "0.1.0"\nedition = "2021"\n')
write("codex-rs/fixture/src/lib.rs", '/// ```\n/// assert_eq!(2 + 2, 4);\n/// ```\npub fn fixture() {}\n#[cfg(test)] mod tests { #[test] fn runtime_path() {} }\n')
write("tools/argument-comment-lint/src/comment_parser.rs", "// fixture inventory source\n")
write("tools/argument-comment-lint/native_test_runner.py", '''import json
print(json.dumps({"schema_version": 1, "report_type": "ArgumentCommentLintNativeTestInventoryV1", "count": 1, "tests": [{"id": "argument-comment-lint::rust-lib::comment_parser.parses_prefix_comment", "kind": "rust-lib", "cargo_target": ["--lib"], "native_id": "comment_parser::tests::parses_prefix_comment", "ui_case": None, "doctest_item": None, "doctest_ordinal": None}]}))
''')
write("codex-rs/windows-sandbox-rs/sandbox_smoketests.py", '''import json
cases = [{"id": f"python-script-case::windows-sandbox-smoke::fixture-{index:02d}", "name": f"fixture {index:02d}"} for index in range(46)]
print(json.dumps({"schema_version": 1, "report_type": "WindowsSandboxSmokeCaseListV1", "validation_id": "windows-sandbox-smoke", "host_platform": "windows", "cases": cases}))
''')
write(".fixture-bin/fake_cargo.py", f'''import json
import pathlib
import sys
control = pathlib.Path({str(outside / "control.txt")!r})
if control.exists() and "pre-result" in control.read_text(encoding="utf-8"):
    raise SystemExit(17)
if "nextest" in sys.argv:
    print(json.dumps({{"test-count": 1, "rust-suites": {{"fixture": {{"package-name": "fixture-package", "binary-name": "fixture-test", "cwd": {str(repo / "codex-rs/fixture")!r}, "testcases": {{"runtime_path": {{"ignored": False}}}}}}}}}}))
else:
    print("fixture/src/lib.rs - fixture (line 1): test")
''')
write(".fixture-bin/cargo.cmd", f'@echo off\r\n"{sys.executable}" "%~dp0fake_cargo.py" %*\r\n')

runner_environment = repo / ".fixture-state/runner-environment"
subprocess.run([sys.executable, "-m", "venv", "--without-pip", str(runner_environment)], check=True)
# Exercise Windows venv forwarding: the reported executable must be the actual
# interpreter authenticated by Core, even when sys.executable names a launcher.
python = json.dumps(str(runner_environment / "Scripts/python.exe"))
justfile_content = f'''[no-cd]
completion-focused validation_id:
    {python} runner_driver.py {{{{validation_id}}}}

[no-cd]
completion-proof:
    {python} canonical_marker.py
'''
write("justfile", justfile_content)
write("runner_driver.py", f'''import json
import pathlib
import runpy
import sys
import time
import traceback
started = pathlib.Path({str(outside / "driver.started")!r})
diagnostic = pathlib.Path({str(outside / "driver.diagnostic")!r})
started.write_text("started\\n", encoding="utf-8")
module = runpy.run_path(str(pathlib.Path("scripts/completion_proof.py").resolve()))
# Pin only the two fixture recipes as the reviewed command surface. All other
# marker discovery and ownership checks still run through the real collector.
module["TEST_SURFACE_REVIEWED_JUSTFILE_SHA256"]["justfile"] = {hashlib.sha256(justfile_content.encode("utf-8")).hexdigest()!r}
historical_successor_rows = json.loads({json.dumps(historical_successor_rows)!r})
if historical_successor_rows:
    real_discover_inventory = module["discover_inventory"]
    def fixture_discover_inventory(*args, **kwargs):
        rows, jest_observation = real_discover_inventory(*args, **kwargs)
        rows_by_id = {{row["baseline_id"]: row for row in rows}}
        rows_by_id.update(
            {{row["baseline_id"]: row for row in historical_successor_rows}}
        )
        return [rows_by_id[test_id] for test_id in sorted(rows_by_id)], jest_observation
    module["_unittest_main"].__globals__["discover_inventory"] = fixture_discover_inventory
try:
    result = module["_unittest_main"](["--config", str(pathlib.Path(".codex/validation/runner.toml").resolve()), "focused", sys.argv[1]])
except BaseException:
    diagnostic.write_text(traceback.format_exc(), encoding="utf-8")
    raise
diagnostic.write_text(f"runner_result={{result}}\\n", encoding="utf-8")
hold = pathlib.Path({str(outside / "report.hold")!r})
ready = pathlib.Path({str(outside / "report.ready")!r})
if hold.exists():
    ready.write_text("ready\\n", encoding="utf-8")
    while hold.exists():
        time.sleep(0.005)
raise SystemExit(result)
''')
write("canonical_marker.py", f'from pathlib import Path\nPath({str(outside / "canonical-launched.txt")!r}).write_text("launched\\n", encoding="utf-8")\n')

if historical:
    source_inventory = json.loads(
        (source / ".codex/validation/frozen-test-inventory-v1.json").read_text(
            encoding="utf-8"
        )
    )
    historical_baseline_ids = {row["baseline_id"] for row in historical_rows}
    baseline = [
        dict(row)
        for row in source_inventory["tests"]
        if row["baseline_id"] in historical_baseline_ids
    ]
    if len(baseline) != 644:
        raise RuntimeError("source frozen inventory no longer contains the historical subset")
else:
    baseline = [{"baseline_id": "fixture-history::unresolved-source", "framework": "fixture-history", "native_id": "unresolved-source", "source": "src/historical.rs", "ignored": False, "platforms": ["windows"]}]
canonical_inventory = json.dumps({"schema_version": 1, "tests": baseline}, sort_keys=True, separators=(",", ":")).encode()
inventory_hash = hashlib.sha256(canonical_inventory).hexdigest()
def write_json(relative, value):
    write(relative, json.dumps(value, indent=2, sort_keys=True) + "\n")

write_json(".codex/validation/frozen-test-inventory-v1.json", {"schema_version": 1, "host_platform": "windows", "inventory_hash": inventory_hash, "tests": baseline})
initial_ledger_rows = (
    [dict(row, validation_id="maintenance.root-unittest") for row in historical_rows]
    if historical
    else [{"baseline_id": baseline[0]["baseline_id"], "resolution": "unresolved"}]
)
write_json(".codex/validation/test-replacements-v1.json", {"schema_version": 1, "frozen_inventory_hash": inventory_hash, "rows": initial_ledger_rows, "overrides": []})
trusted = ["justfile", "runner_driver.py", ".codex/validation/runner.toml", "scripts/completion_proof.py", "scripts/completion_proof_canonical.py", "scripts/completion_proof_inventory_v2.py", "scripts/current_evidence_successor_projection.py", "scripts/completion_proof_unittest.py", "scripts/completion_proof_pytest.py", "scripts/focused_live_successor_catalog.py", "scripts/bounded_process.py", "scripts/child_validation_report.py", "scripts/root_maintenance.py", ".fixture-bin/cargo.cmd", ".fixture-bin/fake_cargo.py", "tools/argument-comment-lint/native_test_runner.py", "codex-rs/windows-sandbox-rs/sandbox_smoketests.py"]
runner_config = f'''schema_version = 2
policy_id = "fixture.current-evidence.v1"
canonical_command = "just completion-proof"
focused_command = "just completion-focused {{validation_id}}"
documentation_command = "just source-map-check"
frozen_inventory_hash = "{inventory_hash}"
frozen_inventory = ".codex/validation/frozen-test-inventory-v1.json"
replacement_ledger = ".codex/validation/test-replacements-v1.json"
host_platform = "windows"
[focused_inventory_evidence]
validation_ids = ["maintenance.root-unittest", "sdk.python.pytest"]

[[validation]]
id = "inventory.frozen-reconciliation"
runner = "inventory-reconciliation"
owned_paths = [".codex/validation/**", "scripts/completion_proof*.py"]
consumed_paths = ["justfile", "**/Cargo.toml", "codex-rs/**/*.rs", "tools/argument-comment-lint/**", "codex-rs/windows-sandbox-rs/sandbox_smoketests.py", "**/test_*.py", "**/*_test.py", "**/package.json", "**/pyproject.toml", "**/pytest.ini", "**/setup.cfg", "**/tox.ini", "**/jest.config.cjs", "**/*.test.ts"]
timeout_seconds = 120

[[validation]]
id = "maintenance.root-unittest"
runner = "python-unittest"
owned_paths = ["scripts/test_runtime_path.py"]
consumed_paths = ["scripts/test_runtime_path.py", "scripts/completion_proof_unittest.py", "scripts/root_maintenance.py", "scripts/pyproject.toml", "scripts/uv.lock"]
timeout_seconds = 120

[[validation]]
id = "sdk.python.pytest"
runner = "python-pytest"
owned_paths = ["sdk/python/tests/**"]
consumed_paths = ["sdk/python/tests/**", "scripts/completion_proof_pytest.py", "sdk/python/pyproject.toml", "sdk/python/uv.lock"]
timeout_seconds = 120
'''
# Core's generic-repository trust metadata is separate from the KD4 runner policy.
# Both configurations share the exact same validation declarations, and Core pins
# the runner configuration as part of its trusted bundle.
write(".codex/validation/runner.toml", runner_config)
write(".codex/validation/completion-proof.toml", f'''trusted_bundle_paths = {json.dumps(trusted)}
trusted_runner_entrypoints = ["scripts/completion_proof.py"]
''' + runner_config)
write("README.md", "fixture documentation\n")
write("src/runtime.rs", "pub const VALUE: u8 = 1;\n")
write("src/historical.rs", "// unresolved history\n")
subprocess.run(["git", "init", "--quiet"], cwd=repo, check=True)
subprocess.run(["git", "config", "user.email", "fixture@example.invalid"], cwd=repo, check=True)
subprocess.run(["git", "config", "user.name", "Current Evidence Fixture"], cwd=repo, check=True)
subprocess.run(["git", "add", "."], cwd=repo, check=True)
subprocess.run(["git", "-c", "commit.gpgsign=false", "commit", "--quiet", "-m", "fixture"], cwd=repo, check=True)
if resolved:
    original_path = os.environ.get("PATH", "")
    original_sys_path = list(sys.path)
    os.environ["PATH"] = str(repo / ".fixture-bin") + os.pathsep + original_path
    sys.path.insert(0, str(repo / "scripts"))
    try:
        module = runpy.run_path(str(repo / "scripts/completion_proof.py"))
        module["TEST_SURFACE_REVIEWED_JUSTFILE_SHA256"]["justfile"] = hashlib.sha256(
            justfile_content.encode("utf-8")
        ).hexdigest()
        with tempfile.TemporaryDirectory(prefix="kd4-resolved-fixture-", dir=outside) as temp_name:
            current_rows, _ = module["discover_inventory"](
                repo,
                temp_dir=pathlib.Path(temp_name),
                jest_observation={},
            )
    finally:
        os.environ["PATH"] = original_path
        sys.path[:] = original_sys_path
    if historical:
        current_rows_by_id = {row["baseline_id"]: row for row in current_rows}
        current_rows_by_id.update(
            {row["baseline_id"]: row for row in historical_successor_rows}
        )
        current_rows = [
            current_rows_by_id[test_id] for test_id in sorted(current_rows_by_id)
        ]
    replacement_ids = sorted(row["baseline_id"] for row in current_rows)
    if not replacement_ids:
        raise RuntimeError("resolved fixture discovered zero replacement IDs")
    if historical:
        successor_ids = {row["baseline_id"] for row in historical_successor_rows}
        addition_ids = [test_id for test_id in replacement_ids if test_id not in successor_ids]
        ledger_rows = [
            dict(row, validation_id="maintenance.root-unittest")
            for row in historical_rows
        ]
        ledger_additions = [
            {
                "test_id": test_id,
                "preserved_behavior": "the bounded fixture's current runtime inventory remains executable",
                "product_path": "the real current-evidence and frozen-reconciliation session path",
                "validation_id": "maintenance.root-unittest",
                "provenance": {
                    "kind": "policy-addition",
                    "source": "bounded completion-proof fixture",
                    "text": "the test is discovered by the fixture's real inventory path",
                },
            }
            for test_id in addition_ids
        ]
    else:
        ledger_additions = []
        ledger_rows = [{
            "baseline_id": baseline[0]["baseline_id"],
            "resolution": "replacement",
            "replacement_ids": replacement_ids,
            "preserved_behavior": "the configured runtime inventory remains executable",
            "product_path": "the real current-evidence and frozen-reconciliation session path",
            "validation_id": "maintenance.root-unittest",
        }]
    write_json(".codex/validation/test-replacements-v1.json", {
        "schema_version": 1,
        "frozen_inventory_hash": inventory_hash,
        "rows": ledger_rows,
        "additions": ledger_additions,
        "overrides": [],
    })
    subprocess.run(["git", "add", ".codex/validation/test-replacements-v1.json"], cwd=repo, check=True)
    subprocess.run(["git", "-c", "commit.gpgsign=false", "commit", "--quiet", "--amend", "--no-edit"], cwd=repo, check=True)
if historical:
    write_json(".fixture-state/historical-review-plan.json", historical_review_plan)
write("src/runtime.rs", "pub const VALUE: u8 = 2;\n")
"###;

fn available_python_command() -> Result<&'static str> {
    for candidate in if cfg!(windows) {
        ["python", "python3"]
    } else {
        ["python3", "python"]
    } {
        if Command::new(candidate)
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
        {
            return Ok(candidate);
        }
    }
    anyhow::bail!("completion-proof integration fixture requires Python")
}

fn run_git(repo: &Path, args: &[&str]) -> Result<()> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .with_context(|| format!("run git {}", args.join(" ")))?;
    anyhow::ensure!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

#[cfg(unix)]
fn create_file_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn create_file_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_file(target, link)
}

fn terminal_candidate(index: usize, text: &str) -> String {
    let response_id = format!("proof-response-{index}");
    let message_id = format!("proof-message-{index}");
    sse(vec![
        ev_response_created(&response_id),
        ev_assistant_message(&message_id, text),
        ev_completed(&response_id),
    ])
}

fn repeated_terminal_candidates(count: usize, text: &str) -> Vec<String> {
    (0..count)
        .map(|index| terminal_candidate(index, &format!("{text} #{index}")))
        .collect()
}

fn exec_command_call_response(call_id: &str, command: &str, workdir: &Path) -> String {
    let arguments = serde_json::to_string(&json!({
        "kind": "script",
        "cmd": command,
        "workdir": workdir.abs(),
        "tty": false,
        "yield_time_ms": 30_000,
    }))
    .expect("serialize exec_command arguments");
    sse(vec![
        ev_response_created("proof-exec-response"),
        ev_function_call(call_id, "exec_command", &arguments),
        ev_completed("proof-exec-response"),
    ])
}

fn shell_command_call_response(call_id: &str, command: &str) -> String {
    let arguments = serde_json::to_string(&json!({
        "kind": "script",
        "command": command,
        "timeout_ms": 180_000,
        "login": false,
    }))
    .expect("serialize shell_command arguments");
    sse(vec![
        ev_response_created("proof-shell-response"),
        ev_function_call(call_id, "shell_command", &arguments),
        ev_completed("proof-shell-response"),
    ])
}

fn required_failure_call_response(call_id: &str) -> String {
    sse(vec![
        ev_response_created("proof-required-failure-response"),
        ev_function_call(call_id, REQUIRED_FAILURE_TOOL_NAME, "{}"),
        ev_completed("proof-required-failure-response"),
    ])
}

fn agent_tool_call_response(call_id: &str, tool_name: &str, arguments: &str) -> String {
    let response_id = format!("agent-tool-response-{call_id}");
    sse(vec![
        ev_response_created(&response_id),
        ev_function_call_with_namespace(call_id, "agents", tool_name, arguments),
        ev_completed(&response_id),
    ])
}

async fn wait_for_response_request(mock_response: &ResponseMock, label: &str) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while mock_response.requests().is_empty() {
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {label}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Ok(())
}

fn tool_output_json(output: &str, pointer: &str) -> Result<serde_json::Value> {
    serde_json::Deserializer::from_str(output)
        .into_iter::<serde_json::Value>()
        .collect::<serde_json::Result<Vec<_>>>()?
        .into_iter()
        .find(|value| value.pointer(pointer).is_some())
        .with_context(|| format!("tool output omitted {pointer}: {output}"))
}

#[cfg(windows)]
fn current_evidence_report_files(root: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    if !root.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            current_evidence_report_files(&entry.path(), files)?;
        } else if entry.path().extension().and_then(|value| value.to_str()) == Some("json") {
            files.push(entry.path());
        }
    }
    Ok(())
}

#[cfg(windows)]
fn current_evidence_report_barrier(
    fixture: &CurrentEvidenceFixture,
    codex_home: &Path,
    replay: Option<Vec<u8>>,
) -> Result<(
    Arc<std::sync::atomic::AtomicBool>,
    std::thread::JoinHandle<Result<Vec<u8>>>,
)> {
    if fixture.barrier_ready.exists() {
        fs::remove_file(&fixture.barrier_ready)?;
    }
    for diagnostic in [&fixture.driver_started, &fixture.driver_diagnostic] {
        if diagnostic.exists() {
            fs::remove_file(diagnostic)?;
        }
    }
    fs::write(&fixture.barrier_hold, "hold\n")?;
    let ready = fixture.barrier_ready.clone();
    let hold = fixture.barrier_hold.clone();
    let attempts = codex_home.join("completion-proof/attempts");
    let tool_completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let barrier_tool_completed = Arc::clone(&tool_completed);
    let thread = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(180);
        while !ready.exists() {
            if barrier_tool_completed.load(std::sync::atomic::Ordering::Acquire) && !ready.exists()
            {
                let _ = fs::remove_file(&hold);
                anyhow::bail!(
                    "tool call returned before the fixture runner reached its report barrier"
                );
            }
            anyhow::ensure!(
                std::time::Instant::now() < deadline,
                "timed out waiting for current-evidence report barrier"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut report_path = None;
        while report_path.is_none() {
            let mut candidates = Vec::new();
            current_evidence_report_files(&attempts, &mut candidates)?;
            candidates.sort();
            report_path = candidates.into_iter().find(|path| {
                fs::read(path)
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                    .is_some_and(|value| {
                        value.get("report_type").and_then(serde_json::Value::as_str)
                            == Some("FocusedValidationAttemptReportV2")
                            && value
                                .get("focused_validation_id")
                                .and_then(serde_json::Value::as_str)
                                == Some("inventory.current-evidence")
                    })
            });
            anyhow::ensure!(
                std::time::Instant::now() < deadline,
                "timed out locating private current-evidence report"
            );
            if report_path.is_none() {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        let report_path = report_path.expect("report path checked above");
        let captured = fs::read(&report_path)?;
        if let Some(replay) = replay {
            fs::write(&report_path, replay)?;
        }
        fs::remove_file(&hold)?;
        let _ = fs::remove_file(&ready);
        Ok(captured)
    });
    Ok((tool_completed, thread))
}

#[cfg(windows)]
fn current_evidence_report_diagnostics(report: &Result<Vec<u8>>) -> String {
    let report = match report {
        Ok(report) => report,
        Err(error) => return error.to_string(),
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(report) else {
        return "captured report was not valid JSON".to_string();
    };
    json!({
        "attempt_classification": value.get("attempt_classification"),
        "fatal_error": value.get("fatal_error"),
        "runner_process_identity": value.get("runner_process_identity"),
        "validations": value.get("validations"),
    })
    .to_string()
}

#[cfg(windows)]
fn read_authenticated_completion_proof_state(
    codex_home: &Path,
    repository: &Path,
) -> Result<(PathBuf, serde_json::Value)> {
    let mut state_path =
        codex_core::test_support::completion_proof_state_lock_path(codex_home, repository);
    state_path.set_extension("json");
    let state = serde_json::from_slice(&fs::read(&state_path).with_context(|| {
        format!(
            "read authenticated completion-proof state {}",
            state_path.display()
        )
    })?)?;
    Ok((state_path, state))
}

fn write_stdin_call_response(call_id: &str, process_id: u32) -> String {
    let arguments = serde_json::to_string(&json!({
        "session_id": process_id,
        "chars": "",
        "yield_time_ms": 30_000,
    }))
    .expect("serialize write_stdin arguments");
    sse(vec![
        ev_response_created("proof-write-stdin-response"),
        ev_function_call(call_id, "write_stdin", &arguments),
        ev_completed("proof-write-stdin-response"),
    ])
}

fn phased_messages_and_exec_command_response(
    response_id: &str,
    commentary_id: &str,
    commentary: &str,
    final_answer_id: &str,
    final_answer: &str,
    call_id: &str,
    command: &str,
    workdir: &Path,
) -> String {
    let mut commentary_event = ev_assistant_message(commentary_id, commentary);
    commentary_event["item"]["phase"] = json!("commentary");
    let mut final_answer_event = ev_assistant_message(final_answer_id, final_answer);
    final_answer_event["item"]["phase"] = json!("final_answer");
    let arguments = serde_json::to_string(&json!({
        "kind": "script",
        "cmd": command,
        "workdir": workdir.abs(),
        "tty": false,
        "yield_time_ms": 30_000,
    }))
    .expect("serialize exec_command arguments");
    sse(vec![
        ev_response_created(response_id),
        commentary_event,
        final_answer_event,
        ev_function_call(call_id, "exec_command", &arguments),
        ev_completed(response_id),
    ])
}

fn commentary_and_unphased_message_and_exec_command_response(
    response_id: &str,
    commentary_id: &str,
    commentary: &str,
    unphased_id: &str,
    unphased: &str,
    call_id: &str,
    command: &str,
    workdir: &Path,
) -> String {
    let mut commentary_event = ev_assistant_message(commentary_id, commentary);
    commentary_event["item"]["phase"] = json!("commentary");
    let arguments = serde_json::to_string(&json!({
        "kind": "script",
        "cmd": command,
        "workdir": workdir.abs(),
        "tty": false,
        "yield_time_ms": 30_000,
    }))
    .expect("serialize exec_command arguments");
    sse(vec![
        ev_response_created(response_id),
        commentary_event,
        ev_assistant_message(unphased_id, unphased),
        ev_function_call(call_id, "exec_command", &arguments),
        ev_completed(response_id),
    ])
}

fn escalated_exec_command_call_response(
    call_id: &str,
    command: &str,
    workdir: &Path,
) -> Result<String> {
    let arguments = serde_json::to_string(&json!({
        "kind": "script",
        "cmd": command,
        "workdir": workdir.abs(),
        "tty": false,
        "yield_time_ms": 30_000,
        "sandbox_permissions": "require_escalated",
        "justification": "exercise the real required-tool terminal path",
    }))?;
    Ok(sse(vec![
        ev_response_created("required-tool-response"),
        ev_function_call(call_id, "exec_command", &arguments),
        ev_completed("required-tool-response"),
    ]))
}

async fn submit_and_collect(test: &TestCodex, prompt: &str) -> Result<Vec<EventMsg>> {
    submit_without_wait(test, prompt).await?;

    // Windows canonical certification performs a real confined-sandbox preflight before the
    // process is launched. On a loaded validation host that preparation alone can legitimately
    // approach the old 90-second whole-turn deadline, leaving no time for the required follow-up
    // model response. Keep the test bounded while allowing the real runtime path to complete.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
    let mut events = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        anyhow::ensure!(!remaining.is_zero(), "timed out waiting for TurnComplete");
        let event = tokio::time::timeout(remaining, test.codex.next_event())
            .await
            .with_context(|| {
                format!("timed out waiting for completion-proof session event; observed events: {events:#?}")
            })??;
        let terminal = matches!(event.msg, EventMsg::TurnComplete(_));
        events.push(event.msg);
        if terminal {
            return Ok(events);
        }
    }
}

async fn submit_and_collect_thread(
    thread: &Arc<CodexThread>,
    config: &Config,
    prompt: &str,
) -> Result<Vec<EventMsg>> {
    let cwd = config.cwd.clone();
    let (sandbox_policy, permission_profile) =
        turn_permission_fields(PermissionProfile::Disabled, cwd.as_path());
    thread
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: prompt.to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: ThreadSettingsOverrides {
                environments: Some(local_selections(cwd)),
                approval_policy: Some(AskForApproval::Never),
                sandbox_policy: Some(sandbox_policy),
                permission_profile,
                ..Default::default()
            },
        })
        .await?;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
    let mut events = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        anyhow::ensure!(!remaining.is_zero(), "timed out waiting for TurnComplete");
        let event = match tokio::time::timeout(remaining, thread.next_event()).await {
            Ok(event) => event?,
            Err(err) => {
                return Err(err).context(format!(
                    "timed out waiting for completion-proof session event; observed events: {events:#?}"
                ));
            }
        };
        let terminal = matches!(event.msg, EventMsg::TurnComplete(_));
        events.push(event.msg);
        if terminal {
            return Ok(events);
        }
    }
}

async fn submit_without_wait(test: &TestCodex, prompt: &str) -> Result<()> {
    let cwd = test.config.cwd.clone();
    let (sandbox_policy, permission_profile) =
        turn_permission_fields(PermissionProfile::Disabled, cwd.as_path());
    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: prompt.to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: ThreadSettingsOverrides {
                environments: Some(local_selections(cwd)),
                approval_policy: Some(AskForApproval::Never),
                sandbox_policy: Some(sandbox_policy),
                permission_profile,
                ..Default::default()
            },
        })
        .await?;
    Ok(())
}

async fn submit_escalated_call_deny_and_collect(
    test: &TestCodex,
    prompt: &str,
) -> Result<Vec<EventMsg>> {
    let cwd = test.config.cwd.clone();
    let (sandbox_policy, permission_profile) =
        turn_permission_fields(PermissionProfile::read_only(), cwd.as_path());
    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: prompt.to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: ThreadSettingsOverrides {
                environments: Some(local_selections(cwd)),
                approval_policy: Some(AskForApproval::OnRequest),
                sandbox_policy: Some(sandbox_policy),
                permission_profile,
                ..Default::default()
            },
        })
        .await?;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    let mut events = Vec::new();
    let mut rejected = false;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        anyhow::ensure!(!remaining.is_zero(), "timed out waiting for TurnComplete");
        let event = tokio::time::timeout(remaining, test.codex.next_event())
            .await
            .context("timed out waiting for required-tool session event")??;
        let approval_id = match &event.msg {
            EventMsg::ExecApprovalRequest(approval) => {
                anyhow::ensure!(!rejected, "received a second exec approval request");
                Some(approval.effective_approval_id())
            }
            _ => None,
        };
        let terminal = matches!(event.msg, EventMsg::TurnComplete(_));
        events.push(event.msg);
        if let Some(id) = approval_id {
            rejected = true;
            test.codex
                .submit(Op::ExecApproval {
                    id,
                    turn_id: None,
                    decision: ReviewDecision::Denied,
                })
                .await?;
        }
        if terminal {
            anyhow::ensure!(rejected, "turn completed before requesting approval");
            return Ok(events);
        }
    }
}

fn response_request_contains(request: &wiremock::Request, text: &str) -> bool {
    let is_zstd = request
        .headers
        .get("content-encoding")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .any(|entry| entry.trim().eq_ignore_ascii_case("zstd"))
        });
    let body = if is_zstd {
        zstd::stream::decode_all(std::io::Cursor::new(&request.body)).ok()
    } else {
        Some(request.body.clone())
    };
    body.and_then(|body| String::from_utf8(body).ok())
        .is_some_and(|body| body.contains(text))
}

fn is_assistant_output(event: &EventMsg) -> bool {
    match event {
        EventMsg::AgentMessage(_) | EventMsg::AgentMessageContentDelta(_) => true,
        EventMsg::ItemStarted(event) => matches!(event.item, TurnItem::AgentMessage(_)),
        EventMsg::ItemCompleted(event) => matches!(event.item, TurnItem::AgentMessage(_)),
        EventMsg::RawResponseItem(event) => {
            matches!(&event.item, ResponseItem::Message { role, .. } if role == "assistant")
        }
        _ => false,
    }
}

fn assistant_output_contains(event: &EventMsg, expected: &str) -> bool {
    match event {
        EventMsg::AgentMessage(message) => message.message.contains(expected),
        EventMsg::AgentMessageContentDelta(delta) => delta.delta.contains(expected),
        EventMsg::ItemStarted(event) => turn_item_contains(&event.item, expected),
        EventMsg::ItemCompleted(event) => turn_item_contains(&event.item, expected),
        EventMsg::RawResponseItem(event) => response_item_contains(&event.item, expected),
        _ => false,
    }
}

fn turn_item_contains(item: &TurnItem, expected: &str) -> bool {
    let TurnItem::AgentMessage(message) = item else {
        return false;
    };
    message.content.iter().any(
        |content| matches!(content, AgentMessageContent::Text { text } if text.contains(expected)),
    )
}

fn response_item_contains(item: &ResponseItem, expected: &str) -> bool {
    let ResponseItem::Message { role, content, .. } = item else {
        return false;
    };
    role == "assistant"
        && content.iter().any(|content| {
            matches!(
                content,
                codex_protocol::models::ContentItem::OutputText { text }
                    if text.contains(expected)
            )
        })
}

fn function_call_output<'a>(events: &'a [EventMsg], call_id: &str) -> Option<&'a str> {
    events.iter().find_map(|event| {
        let EventMsg::RawResponseItem(event) = event else {
            return None;
        };
        let ResponseItem::FunctionCallOutput {
            call_id: output_call_id,
            output,
            ..
        } = &event.item
        else {
            return None;
        };
        (output_call_id == call_id)
            .then(|| output.text_content())
            .flatten()
    })
}

fn function_call_output_success(events: &[EventMsg], call_id: &str) -> Option<bool> {
    events.iter().find_map(|event| {
        let EventMsg::RawResponseItem(event) = event else {
            return None;
        };
        let ResponseItem::FunctionCallOutput {
            call_id: output_call_id,
            output,
            ..
        } = &event.item
        else {
            return None;
        };
        (output_call_id == call_id)
            .then_some(output.success)
            .flatten()
    })
}

fn run_session_path_test<F, Fut>(test_name: &'static str, test: F) -> Result<()>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    const TEST_STACK_SIZE_BYTES: usize = 8 * 1024 * 1024;

    let handle = std::thread::Builder::new()
        .name(test_name.to_string())
        .stack_size(TEST_STACK_SIZE_BYTES)
        .spawn(move || -> Result<()> {
            #[cfg(windows)]
            let _windows_sandbox_test_lock = super::lock_windows_sandbox_tests()?;
            #[cfg(windows)]
            super::stage_windows_sandbox_helpers()?;

            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_stack_size(TEST_STACK_SIZE_BYTES)
                .enable_all()
                .build()
                .context("build completion-proof session test runtime")?;
            runtime.block_on(test())
        })?;

    match handle.join() {
        Ok(result) => result,
        Err(_) => Err(anyhow::anyhow!("{test_name} thread panicked")),
    }
}

#[test]
fn missing_proof_hides_terminal_output_and_gate_launches_no_command() -> Result<()> {
    run_session_path_test(
        "missing_proof_hides_terminal_output_and_gate_launches_no_command",
        missing_proof_hides_terminal_output_and_gate_launches_no_command_impl,
    )
}

#[test]
fn missing_canonical_command_blocks_a_changed_repository_from_a_fresh_home() -> Result<()> {
    run_session_path_test(
        "missing_canonical_command_blocks_a_changed_repository_from_a_fresh_home",
        missing_canonical_command_blocks_a_changed_repository_from_a_fresh_home_impl,
    )
}

#[test]
fn contended_private_state_lock_fails_closed_without_blocking_session_start_or_launching_certification()
-> Result<()> {
    run_session_path_test(
        "contended_private_state_lock_fails_closed_without_blocking_session_start_or_launching_certification",
        contended_private_state_lock_fails_closed_without_blocking_session_start_or_launching_certification_impl,
    )
}

async fn contended_private_state_lock_fails_closed_without_blocking_session_start_or_launching_certification_impl()
-> Result<()> {
    let fixture = CompletionProofFixture::new()?;
    let fresh_home = Arc::new(TempDir::new()?);
    let lock_path = codex_core::test_support::completion_proof_state_lock_path(
        fresh_home.path(),
        &fixture.repo_path,
    );
    fs::create_dir_all(
        lock_path
            .parent()
            .context("completion-proof state lock path has no parent")?,
    )?;
    let held_lock = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&lock_path)?;
    fs2::FileExt::lock_exclusive(&held_lock)?;
    let physical_credential_before =
        codex_core::test_support::completion_proof_physical_credential_snapshot(
            fresh_home.path(),
            &fixture.repo_path,
        )
        .map_err(anyhow::Error::msg)?;

    let startup_started = tokio::time::Instant::now();
    let harness = tokio::time::timeout(
        Duration::from_secs(15),
        fixture.harness_with_home(Arc::clone(&fresh_home)),
    )
    .await
    .context("session startup did not return after the private state lock deadline")??;
    let startup_elapsed = startup_started.elapsed();
    assert!(
        startup_elapsed >= Duration::from_secs(9),
        "session startup returned before exercising the private state lock deadline: {startup_elapsed:?}"
    );
    assert!(
        startup_elapsed < Duration::from_secs(15),
        "session startup exceeded the bounded private state lock deadline"
    );
    let physical_credential_after =
        codex_core::test_support::completion_proof_physical_credential_snapshot(
            fresh_home.path(),
            &fixture.repo_path,
        )
        .map_err(anyhow::Error::msg)?;
    assert_eq!(
        physical_credential_after, physical_credential_before,
        "contended startup changed the exact physical completion-proof credential"
    );
    assert!(
        !fixture.marker_path.exists(),
        "session startup launched canonical certification while the state lock was contended"
    );
    drop(held_lock);

    mount_sse_sequence(
        harness.server(),
        repeated_terminal_candidates(
            TOTAL_GENERATIONS_WITH_FORCED_TERMINAL,
            "a contended private state lock must keep terminal output private",
        ),
    )
    .await;
    let events = submit_and_collect(harness.test(), "finish without running certification").await?;
    assert!(events.iter().all(|event| !is_assistant_output(event)));
    let completion = events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing TurnComplete")?;
    assert!(completion.last_agent_message.is_none());
    let error = completion
        .error
        .as_ref()
        .context("contended private state lock should block terminal completion")?;
    assert!(
        error
            .message
            .contains("state lock was unavailable while loading")
            && error.message.contains("timed out after 10 seconds"),
        "unexpected contended-lock completion error: {}",
        error.message
    );
    assert!(
        !fixture.marker_path.exists(),
        "the completion gate launched canonical certification after a state-lock timeout"
    );

    Ok(())
}

#[test]
fn current_user_relaxation_enters_through_the_real_turn_path() -> Result<()> {
    run_session_path_test(
        "current_user_relaxation_enters_through_the_real_turn_path",
        current_user_relaxation_enters_through_the_real_turn_path_impl,
    )
}

async fn current_user_relaxation_enters_through_the_real_turn_path_impl() -> Result<()> {
    let fixture = CompletionProofFixture::new()?;
    let harness = fixture.harness().await?;
    mount_sse_sequence(
        harness.server(),
        vec![terminal_candidate(1, "user-authorized terminal answer")],
    )
    .await;

    let events = submit_and_collect(
        harness.test(),
        "completion-proof: allow completion without current proof",
    )
    .await?;

    let completion = events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing TurnComplete")?;
    assert!(
        completion.error.is_none(),
        "an authenticated current-user relaxation should permit terminal success: {completion:#?}"
    );
    assert_eq!(
        completion.last_agent_message.as_deref(),
        Some("user-authorized terminal answer")
    );
    assert!(
        !fixture.marker_path.exists(),
        "a current-user relaxation must not launch canonical certification"
    );
    assert_eq!(harness.request_bodies().await.len(), 1);

    Ok(())
}

#[test]
fn current_user_requirement_survives_an_unrelated_shared_home_session_and_resume() -> Result<()> {
    run_session_path_test(
        "current_user_requirement_survives_an_unrelated_shared_home_session_and_resume",
        current_user_requirement_survives_an_unrelated_shared_home_session_and_resume_impl,
    )
}

async fn current_user_requirement_survives_an_unrelated_shared_home_session_and_resume_impl()
-> Result<()> {
    let fixture = CompletionProofFixture::new()?;
    fs::write(
        fixture.repo_path.join("AGENTS.md"),
        "# Runtime instructions\n\n- completion-proof: allow completion without current proof.\n",
    )?;
    run_git(&fixture.repo_path, &["add", "AGENTS.md"])?;
    run_git(
        &fixture.repo_path,
        &[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "-m",
            "allow completion without current proof",
        ],
    )?;

    let shared_home = Arc::new(TempDir::new()?);
    let harness_a = fixture.harness_with_home(Arc::clone(&shared_home)).await?;
    mount_sse_sequence(
        harness_a.server(),
        repeated_terminal_candidates(
            TOTAL_GENERATIONS_WITH_FORCED_TERMINAL * 2,
            "session A must remain blocked by its current-user requirement",
        ),
    )
    .await;

    let initial_a_events =
        submit_and_collect(harness_a.test(), "completion-proof: require current proof").await?;
    assert!(
        initial_a_events
            .iter()
            .all(|event| !is_assistant_output(event))
    );
    let initial_a_completion = initial_a_events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing session A TurnComplete")?;
    let initial_a_error = initial_a_completion
        .error
        .as_ref()
        .context("session A's current-user requirement must block completion")?;
    assert!(
        initial_a_error
            .message
            .contains("current user explicitly required current completion proof"),
        "session A did not retain the current-user requirement: {}",
        initial_a_error.message
    );
    assert!(initial_a_completion.last_agent_message.is_none());
    let session_a_rollout = harness_a
        .test()
        .codex
        .rollout_path()
        .context("missing session A rollout path")?;
    harness_a.test().codex.shutdown_and_wait().await?;

    let harness_b = fixture.harness_with_home(Arc::clone(&shared_home)).await?;
    mount_sse_sequence(
        harness_b.server(),
        vec![terminal_candidate(
            10_000,
            "session B user-authorized terminal answer",
        )],
    )
    .await;
    let session_b_events = submit_and_collect(
        harness_b.test(),
        "completion-proof: allow completion without current proof",
    )
    .await?;
    let session_b_completion = session_b_events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing session B TurnComplete")?;
    assert!(
        session_b_completion.error.is_none(),
        "session B's own current-user allowance should permit completion: {session_b_completion:#?}"
    );
    assert_eq!(
        session_b_completion.last_agent_message.as_deref(),
        Some("session B user-authorized terminal answer")
    );
    harness_b.test().codex.shutdown_and_wait().await?;

    let resume_cwd = fixture.repo_path.abs();
    let mut resume_builder =
        test_codex().with_config(move |config| set_fixture_workspace(config, resume_cwd));
    let resumed_a = resume_builder
        .resume(
            harness_a.server(),
            Arc::clone(&shared_home),
            session_a_rollout,
        )
        .await?;
    let resumed_a_events = submit_and_collect(&resumed_a, "finish resumed session A").await?;
    assert!(
        resumed_a_events
            .iter()
            .all(|event| !is_assistant_output(event))
    );
    let resumed_a_completion = resumed_a_events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing resumed session A TurnComplete")?;
    let resumed_a_error = resumed_a_completion
        .error
        .as_ref()
        .context("resumed session A must remain blocked by its current-user requirement")?;
    assert!(
        resumed_a_error
            .message
            .contains("current user explicitly required current completion proof"),
        "resumed session A used another lineage or repository permission: {}",
        resumed_a_error.message
    );
    assert!(resumed_a_completion.last_agent_message.is_none());
    assert_eq!(
        fixture.canonical_launch_count()?,
        0,
        "lineage relaxation checks must not launch the canonical runner"
    );

    Ok(())
}

#[test]
fn assistant_text_cannot_create_a_completion_proof_relaxation() -> Result<()> {
    run_session_path_test(
        "assistant_text_cannot_create_a_completion_proof_relaxation",
        assistant_text_cannot_create_a_completion_proof_relaxation_impl,
    )
}

#[test]
fn repository_created_after_session_start_cannot_create_proof_authority() -> Result<()> {
    run_session_path_test(
        "repository_created_after_session_start_cannot_create_proof_authority",
        repository_created_after_session_start_cannot_create_proof_authority_impl,
    )
}

async fn repository_created_after_session_start_cannot_create_proof_authority_impl() -> Result<()> {
    let fixture = CompletionProofFixture::new()?;
    fixture.remove_git_repository_before_session_start()?;
    let harness = fixture.harness().await?;

    // The root-session lineage is already fixed as non-repository. Creating a
    // repository afterward must not let repository-controlled files mint the
    // authority needed to recognize or certify the canonical command.
    fixture.initialize_git_repository_after_session_start()?;
    let call_id = "late-repository-canonical-command";
    let mut responses = vec![exec_command_call_response(
        call_id,
        &fixture.canonical_command,
        &fixture.repo_path,
    )];
    responses.extend(repeated_terminal_candidates(
        MAX_REGULAR_LOGICAL_GENERATIONS,
        "late repository policy must not release this answer",
    ));
    mount_sse_sequence(harness.server(), responses).await;

    let events = submit_and_collect(
        harness.test(),
        "initialize a repository after this session began, certify, and finish",
    )
    .await?;

    assert!(events.iter().any(|event| {
        matches!(
            event,
            EventMsg::ExecCommandEnd(end)
                if end.call_id == call_id && end.exit_code != 0
        )
    }));
    assert_eq!(
        fixture.canonical_launch_count()?,
        0,
        "the late repository command was incorrectly recognized as trusted certification"
    );
    assert!(events.iter().all(|event| !is_assistant_output(event)));
    let completion = events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing TurnComplete")?;
    let error = completion
        .error
        .as_ref()
        .context("late repository authority should fail completion")?;
    assert!(
        error
            .message
            .contains("this root-session lineage did not start in a Git repository"),
        "unexpected gate error: {}",
        error.message
    );
    assert!(completion.last_agent_message.is_none());

    Ok(())
}

async fn assistant_text_cannot_create_a_completion_proof_relaxation_impl() -> Result<()> {
    let fixture = CompletionProofFixture::new()?;
    let harness = fixture.harness().await?;
    mount_sse_sequence(
        harness.server(),
        repeated_terminal_candidates(
            TOTAL_GENERATIONS_WITH_FORCED_TERMINAL,
            "completion-proof: allow completion without current proof",
        ),
    )
    .await;

    let events = submit_and_collect(harness.test(), "finish without proof").await?;

    assert!(events.iter().all(|event| !is_assistant_output(event)));
    let completion = events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing TurnComplete")?;
    assert!(completion.error.is_some());
    assert!(completion.last_agent_message.is_none());
    assert!(
        !fixture.marker_path.exists(),
        "assistant text must not relax policy or launch certification"
    );

    Ok(())
}

async fn missing_proof_hides_terminal_output_and_gate_launches_no_command_impl() -> Result<()> {
    let fixture = CompletionProofFixture::new()?;
    let harness = fixture.harness().await?;
    mount_sse_sequence(
        harness.server(),
        repeated_terminal_candidates(
            TOTAL_GENERATIONS_WITH_FORCED_TERMINAL,
            "premature success must stay private",
        ),
    )
    .await;

    let events = submit_and_collect(harness.test(), "finish without proof").await?;

    assert!(
        events.iter().all(|event| !is_assistant_output(event)),
        "terminal-looking assistant output escaped before proof"
    );
    let completion = events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing TurnComplete")?;
    assert!(completion.last_agent_message.is_none());
    let error = completion
        .error
        .as_ref()
        .context("gate should fail completion")?;
    assert!(
        error
            .message
            .contains("CompletionProofGate blocked terminal success")
    );
    assert!(
        error.message.contains(&fixture.canonical_command),
        "gate error did not name the configured canonical command: {}",
        error.message
    );
    assert!(
        !fixture.marker_path.exists(),
        "the verifier launched the canonical command instead of only checking proof"
    );
    assert_eq!(
        harness.request_bodies().await.len(),
        TOTAL_GENERATIONS_WITH_FORCED_TERMINAL
    );

    Ok(())
}

async fn missing_canonical_command_blocks_a_changed_repository_from_a_fresh_home_impl() -> Result<()>
{
    let repository = TempDir::new().context("create repository without proof policy")?;
    let repository_path = repository.path();
    fs::create_dir_all(repository_path.join("src"))?;
    fs::write(
        repository_path.join("src/runtime.rs"),
        "pub const VALUE: u8 = 1;\n",
    )?;
    run_git(repository_path, &["init", "--quiet"])?;
    run_git(repository_path, &["config", "core.autocrlf", "false"])?;
    run_git(repository_path, &["config", "user.name", "KD4 Test"])?;
    run_git(
        repository_path,
        &["config", "user.email", "kd4-test@example.invalid"],
    )?;
    run_git(repository_path, &["add", "."])?;
    run_git(
        repository_path,
        &["commit", "--quiet", "-m", "unconfigured baseline"],
    )?;
    fs::write(
        repository_path.join("src/runtime.rs"),
        "pub const VALUE: u8 = 2;\n",
    )?;

    let fresh_home = Arc::new(TempDir::new()?);
    anyhow::ensure!(
        fs::read_dir(fresh_home.path())?.next().is_none(),
        "missing-command test requires an initially empty CODEX_HOME"
    );
    let cwd = repository_path.to_path_buf().abs();
    let harness = TestCodexHarness::with_builder(
        test_codex()
            .with_home(fresh_home)
            .with_config(move |config| set_fixture_workspace(config, cwd)),
    )
    .await?;
    mount_sse_sequence(
        harness.server(),
        repeated_terminal_candidates(
            TOTAL_GENERATIONS_WITH_FORCED_TERMINAL,
            "a repository without a canonical command must not finish",
        ),
    )
    .await;

    let events = submit_and_collect(harness.test(), "finish this changed repository").await?;

    assert!(events.iter().all(|event| !is_assistant_output(event)));
    let completion = events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing TurnComplete")?;
    assert!(completion.last_agent_message.is_none());
    let error = completion
        .error
        .as_ref()
        .context("missing canonical command should block terminal completion")?;
    assert!(
        error.message.contains(
            "does not define a valid explicitly trusted canonical completion-proof command"
        ),
        "unexpected missing-command gate error: {}",
        error.message
    );
    assert!(
        error
            .message
            .contains("CompletionProofGate will not invent or launch one"),
        "missing-command error did not preserve verifier-only behavior: {}",
        error.message
    );
    assert_eq!(
        harness.request_bodies().await.len(),
        TOTAL_GENERATIONS_WITH_FORCED_TERMINAL
    );

    Ok(())
}

#[test]
fn source_markdown_and_schema_changes_require_canonical_proof_through_real_session_path()
-> Result<()> {
    run_session_path_test(
        "source_markdown_and_schema_changes_require_canonical_proof_through_real_session_path",
        source_markdown_and_schema_changes_require_canonical_proof_through_real_session_path_impl,
    )
}

async fn source_markdown_and_schema_changes_require_canonical_proof_through_real_session_path_impl()
-> Result<()> {
    let fixture = CompletionProofFixture::new()?;
    fixture.prepare_source_markdown_and_schema_changes()?;
    let harness = fixture.harness().await?;
    mount_sse_sequence(
        harness.server(),
        repeated_terminal_candidates(
            TOTAL_GENERATIONS_WITH_FORCED_TERMINAL,
            "source contract files must not be treated as documentation",
        ),
    )
    .await;

    let events = submit_and_collect(harness.test(), "finish after source contract edits").await?;

    assert!(events.iter().all(|event| !is_assistant_output(event)));
    let completion = events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing TurnComplete")?;
    assert!(completion.last_agent_message.is_none());
    let error = completion
        .error
        .as_ref()
        .context("source Markdown and schema changes must block completion")?;
    assert!(error.message.contains(&fixture.canonical_command));
    assert!(
        !error.message.contains("Documentation changed"),
        "a Markdown or schema-like file under source was misclassified as documentation: {}",
        error.message
    );
    assert_eq!(fixture.canonical_launch_count()?, 0);
    assert_eq!(fixture.documentation_launch_count()?, 0);

    Ok(())
}

#[test]
fn isolated_non_documentation_files_require_canonical_proof_through_real_session_path() -> Result<()>
{
    run_session_path_test(
        "isolated_non_documentation_files_require_canonical_proof_through_real_session_path",
        isolated_non_documentation_files_require_canonical_proof_through_real_session_path_impl,
    )
}

async fn isolated_non_documentation_files_require_canonical_proof_through_real_session_path_impl()
-> Result<()> {
    for (relative_path, contents) in [
        ("codex-rs/core/prompt.md", "# Runtime prompt contract\n"),
        (
            ".codex/skills/reviewer/SKILL.md",
            "# Runtime skill contract\n",
        ),
        ("AGENTS.md", "# Runtime instructions\n"),
        ("SOURCEMAP.md", "# Runtime ownership\n"),
        ("src/runtime.schema.json", "{\"type\":\"object\"}\n"),
        ("docs/runtime.json", "{\"runtime\":true}\n"),
    ] {
        let fixture = CompletionProofFixture::new()?;
        fixture.prepare_isolated_non_documentation_change(relative_path, contents)?;
        let harness = fixture.harness().await?;
        mount_sse_sequence(
            harness.server(),
            repeated_terminal_candidates(
                TOTAL_GENERATIONS_WITH_FORCED_TERMINAL,
                "an isolated runtime contract file must require canonical proof",
            ),
        )
        .await;

        let events = submit_and_collect(harness.test(), "finish after one runtime contract edit")
            .await
            .with_context(|| format!("submit isolated change for {relative_path}"))?;

        assert!(
            events.iter().all(|event| !is_assistant_output(event)),
            "premature output escaped for isolated change {relative_path}"
        );
        let completion = events
            .iter()
            .find_map(|event| match event {
                EventMsg::TurnComplete(completion) => Some(completion),
                _ => None,
            })
            .with_context(|| format!("missing TurnComplete for {relative_path}"))?;
        assert!(completion.last_agent_message.is_none());
        let error = completion
            .error
            .as_ref()
            .with_context(|| format!("{relative_path} must block completion"))?;
        assert!(
            error.message.contains(&fixture.canonical_command),
            "{relative_path} did not require the canonical command: {}",
            error.message
        );
        assert!(
            !error.message.contains("Documentation changed"),
            "{relative_path} was misclassified as documentation: {}",
            error.message
        );
        assert_eq!(fixture.canonical_launch_count()?, 0);
        assert_eq!(fixture.documentation_launch_count()?, 0);
    }

    Ok(())
}

#[test]
fn documentation_only_change_uses_exact_configured_validation_through_real_session_path()
-> Result<()> {
    run_session_path_test(
        "documentation_only_change_uses_exact_configured_validation_through_real_session_path",
        documentation_only_change_uses_exact_configured_validation_through_real_session_path_impl,
    )
}

async fn documentation_only_change_uses_exact_configured_validation_through_real_session_path_impl()
-> Result<()> {
    let fixture = CompletionProofFixture::new()?;
    fixture.prepare_documentation_only_change()?;
    let harness = fixture.harness().await?;
    let focused_call_id = "behavior-test-cannot-validate-docs";
    let documentation_call_id = "exact-documentation-validation";
    mount_sse_sequence(
        harness.server(),
        vec![
            exec_command_call_response(
                focused_call_id,
                &fixture.exact_focused_command(),
                &fixture.repo_path,
            ),
            terminal_candidate(1, "a behavior test must not satisfy documentation policy"),
            exec_command_call_response(
                documentation_call_id,
                &fixture.documentation_command,
                &fixture.repo_path,
            ),
            terminal_candidate(2, "documentation validation released this answer"),
        ],
    )
    .await;

    let events = submit_and_collect(
        harness.test(),
        "completion-proof: allow completion without current proof\nrun a behavior test, try to finish, then run the configured documentation validation and finish",
    )
    .await?;

    assert!(events.iter().any(|event| {
        matches!(
            event,
            EventMsg::ExecCommandEnd(end)
                if end.call_id == focused_call_id && end.exit_code == 0
        )
    }));
    assert!(events.iter().any(|event| {
        matches!(
            event,
            EventMsg::ExecCommandEnd(end)
                if end.call_id == documentation_call_id && end.exit_code == 0
        )
    }));
    assert_eq!(
        fixture.documentation_launch_count()?,
        1,
        "only the exact configured documentation command may clear the docs-only gate"
    );
    assert_eq!(fixture.canonical_launch_count()?, 0);
    let completion = events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing TurnComplete")?;
    assert!(
        completion.error.is_none(),
        "the exact documentation validation should release completion: {completion:#?}"
    );
    assert_eq!(
        completion.last_agent_message.as_deref(),
        Some("documentation validation released this answer")
    );
    assert_eq!(
        harness.request_bodies().await.len(),
        4,
        "the focused pass and current-user relaxation must not bypass the documentation-validation turn"
    );

    Ok(())
}

#[test]
fn documentation_json_mutation_stales_proof_after_docs_validation_without_rerunning_certification()
-> Result<()> {
    run_session_path_test(
        "documentation_json_mutation_stales_proof_after_docs_validation_without_rerunning_certification",
        documentation_json_mutation_stales_proof_after_docs_validation_without_rerunning_certification_impl,
    )
}

async fn documentation_json_mutation_stales_proof_after_docs_validation_without_rerunning_certification_impl()
-> Result<()> {
    let fixture = CompletionProofFixture::new()?;
    let harness = fixture.harness().await?;
    let canonical_call_id = "canonical-before-docs-json-mutation";
    let mutation_call_id = "mutate-tracked-docs-json";
    let documentation_call_id = "docs-command-cannot-cover-json";
    let mut responses = vec![
        exec_command_call_response(
            canonical_call_id,
            &fixture.canonical_command,
            &fixture.repo_path,
        ),
        exec_command_call_response(
            mutation_call_id,
            &fixture.documentation_json_mutation_command,
            &fixture.repo_path,
        ),
        exec_command_call_response(
            documentation_call_id,
            &fixture.documentation_command,
            &fixture.repo_path,
        ),
    ];
    responses.extend(repeated_terminal_candidates(
        MAX_REGULAR_LOGICAL_GENERATIONS - 2,
        "schema-like docs mutation must keep this output private",
    ));
    mount_sse_sequence(harness.server(), responses).await;

    let events = submit_and_collect(
        harness.test(),
        "certify, mutate a tracked docs JSON file, run the docs command, then finish",
    )
    .await?;

    for call_id in [canonical_call_id, mutation_call_id, documentation_call_id] {
        assert!(events.iter().any(|event| {
            matches!(
                event,
                EventMsg::ExecCommandEnd(end)
                    if end.call_id == call_id && end.exit_code == 0
            )
        }));
    }
    assert!(
        fs::read_to_string(fixture.repo_path.join("docs/tracked.json"))?
            .contains("\"changed\": true")
    );
    assert_eq!(fixture.documentation_launch_count()?, 1);
    assert_eq!(
        fixture.canonical_launch_count()?,
        1,
        "a docs JSON mutation must stale proof without automatically rerunning certification"
    );
    assert!(events.iter().all(|event| !is_assistant_output(event)));
    let completion = events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing TurnComplete")?;
    let error = completion
        .error
        .as_ref()
        .context("a docs JSON mutation must stale canonical proof")?;
    assert!(error.message.contains(&fixture.canonical_command));
    assert!(!error.message.contains("Documentation changed"));
    assert!(completion.last_agent_message.is_none());

    Ok(())
}

#[test]
fn documentation_markdown_mutation_reuses_proof_only_after_exact_docs_validation() -> Result<()> {
    run_session_path_test(
        "documentation_markdown_mutation_reuses_proof_only_after_exact_docs_validation",
        documentation_markdown_mutation_reuses_proof_only_after_exact_docs_validation_impl,
    )
}

async fn documentation_markdown_mutation_reuses_proof_only_after_exact_docs_validation_impl()
-> Result<()> {
    let fixture = CompletionProofFixture::new()?;
    let harness = fixture.harness().await?;
    let canonical_call_id = "canonical-before-docs-markdown-mutation";
    let mutation_call_id = "mutate-tracked-docs-markdown";
    let documentation_call_id = "validate-docs-markdown-mutation";
    let premature_answer = "documentation mutation reused proof before validation";
    mount_sse_sequence(
        harness.server(),
        vec![
            exec_command_call_response(
                canonical_call_id,
                &fixture.canonical_command,
                &fixture.repo_path,
            ),
            exec_command_call_response(
                mutation_call_id,
                &fixture.documentation_markdown_mutation_command,
                &fixture.repo_path,
            ),
            terminal_candidate(1, premature_answer),
            exec_command_call_response(
                documentation_call_id,
                &fixture.documentation_command,
                &fixture.repo_path,
            ),
            terminal_candidate(2, "documentation validation released reused proof"),
        ],
    )
    .await;

    let events = submit_and_collect(
        harness.test(),
        "certify, mutate tracked Markdown documentation, try to finish, validate docs, and finish",
    )
    .await?;

    for call_id in [canonical_call_id, mutation_call_id, documentation_call_id] {
        assert!(events.iter().any(|event| {
            matches!(
                event,
                EventMsg::ExecCommandEnd(end)
                    if end.call_id == call_id && end.exit_code == 0
            )
        }));
    }
    assert!(fs::read_to_string(fixture.repo_path.join("docs/tracked.md"))?.contains("Updated."));
    assert_eq!(fixture.canonical_launch_count()?, 1);
    assert_eq!(fixture.documentation_launch_count()?, 1);
    assert!(
        events
            .iter()
            .all(|event| !assistant_output_contains(event, premature_answer)),
        "completion escaped before the exact documentation validation"
    );
    let completion = events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing TurnComplete")?;
    assert!(
        completion.error.is_none(),
        "the exact documentation validation should release the reused proof: {completion:#?}"
    );
    assert_eq!(
        completion.last_agent_message.as_deref(),
        Some("documentation validation released reused proof")
    );
    assert_eq!(harness.request_bodies().await.len(), 5);

    Ok(())
}

#[test]
fn renamed_non_documentation_destination_requires_canonical_proof_through_real_session_path()
-> Result<()> {
    run_session_path_test(
        "renamed_non_documentation_destination_requires_canonical_proof_through_real_session_path",
        renamed_non_documentation_destination_requires_canonical_proof_through_real_session_path_impl,
    )
}

async fn renamed_non_documentation_destination_requires_canonical_proof_through_real_session_path_impl()
-> Result<()> {
    let fixture = CompletionProofFixture::new()?;
    fs::write(
        fixture.repo_path.join("src/runtime.rs"),
        "pub const VALUE: u8 = 1;\n",
    )?;
    let harness = fixture.harness().await?;
    let rename_call_id = "rename-documentation-into-source";
    let documentation_call_id = "docs-command-cannot-cover-source-rename";
    let mut responses = vec![
        exec_command_call_response(
            rename_call_id,
            &fixture.rename_into_source_command,
            &fixture.repo_path,
        ),
        exec_command_call_response(
            documentation_call_id,
            &fixture.documentation_command,
            &fixture.repo_path,
        ),
    ];
    responses.extend(repeated_terminal_candidates(
        MAX_REGULAR_LOGICAL_GENERATIONS - 1,
        "a non-documentation rename destination must remain private",
    ));
    mount_sse_sequence(harness.server(), responses).await;

    let events = submit_and_collect(
        harness.test(),
        "rename tracked documentation into source, try the docs check, then finish",
    )
    .await?;

    assert!(events.iter().any(|event| {
        matches!(
            event,
            EventMsg::ExecCommandEnd(end)
                if end.call_id == rename_call_id && end.exit_code == 0
        )
    }));
    assert!(events.iter().any(|event| {
        matches!(
            event,
            EventMsg::ExecCommandEnd(end)
                if end.call_id == documentation_call_id && end.exit_code == 0
        )
    }));
    assert!(!fixture.repo_path.join("docs/tracked.md").exists());
    assert!(fixture.repo_path.join("src/renamed_runtime.rs").is_file());
    assert_eq!(fixture.documentation_launch_count()?, 1);
    assert_eq!(fixture.canonical_launch_count()?, 0);
    assert!(events.iter().all(|event| !is_assistant_output(event)));
    let completion = events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing TurnComplete")?;
    let error = completion
        .error
        .as_ref()
        .context("renaming documentation into source must require canonical proof")?;
    assert!(error.message.contains(&fixture.canonical_command));
    assert!(
        !error.message.contains("Documentation changed"),
        "the runtime classified the rename by its old documentation path: {}",
        error.message
    );
    assert!(completion.last_agent_message.is_none());
    assert_eq!(
        harness.request_bodies().await.len(),
        TOTAL_GENERATIONS_WITH_FORCED_TERMINAL
    );

    Ok(())
}

#[test]
fn renamed_non_documentation_source_requires_canonical_proof_through_real_session_path()
-> Result<()> {
    run_session_path_test(
        "renamed_non_documentation_source_requires_canonical_proof_through_real_session_path",
        renamed_non_documentation_source_requires_canonical_proof_through_real_session_path_impl,
    )
}

async fn renamed_non_documentation_source_requires_canonical_proof_through_real_session_path_impl()
-> Result<()> {
    let fixture = CompletionProofFixture::new()?;
    fs::write(
        fixture.repo_path.join("src/runtime.rs"),
        "pub const VALUE: u8 = 1;\n",
    )?;
    let harness = fixture.harness().await?;
    let rename_call_id = "rename-source-into-documentation";
    let documentation_call_id = "docs-command-cannot-cover-source-rename";
    let mut responses = vec![
        exec_command_call_response(
            rename_call_id,
            &fixture.rename_into_documentation_command,
            &fixture.repo_path,
        ),
        exec_command_call_response(
            documentation_call_id,
            &fixture.documentation_command,
            &fixture.repo_path,
        ),
    ];
    responses.extend(repeated_terminal_candidates(
        MAX_REGULAR_LOGICAL_GENERATIONS - 1,
        "a non-documentation rename source must remain private",
    ));
    mount_sse_sequence(harness.server(), responses).await;

    let events = submit_and_collect(
        harness.test(),
        "rename tracked source into documentation, try the docs check, then finish",
    )
    .await?;

    assert!(events.iter().any(|event| {
        matches!(
            event,
            EventMsg::ExecCommandEnd(end)
                if end.call_id == rename_call_id && end.exit_code == 0
        )
    }));
    assert!(events.iter().any(|event| {
        matches!(
            event,
            EventMsg::ExecCommandEnd(end)
                if end.call_id == documentation_call_id && end.exit_code == 0
        )
    }));
    assert!(!fixture.repo_path.join("src/runtime.rs").exists());
    assert!(fixture.repo_path.join("docs/renamed_runtime.rs").is_file());
    assert_eq!(fixture.documentation_launch_count()?, 1);
    assert_eq!(fixture.canonical_launch_count()?, 0);
    assert!(events.iter().all(|event| !is_assistant_output(event)));
    let completion = events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing TurnComplete")?;
    let error = completion
        .error
        .as_ref()
        .context("renaming source into documentation must require canonical proof")?;
    assert!(error.message.contains(&fixture.canonical_command));
    assert!(
        !error.message.contains("Documentation changed"),
        "the runtime ignored the rename's non-documentation source path: {}",
        error.message
    );
    assert!(completion.last_agent_message.is_none());
    assert_eq!(
        harness.request_bodies().await.len(),
        TOTAL_GENERATIONS_WITH_FORCED_TERMINAL
    );

    Ok(())
}

#[test]
fn renamed_paths_can_receive_canonical_proof_through_real_session_path() -> Result<()> {
    run_session_path_test(
        "renamed_paths_can_receive_canonical_proof_through_real_session_path",
        renamed_paths_can_receive_canonical_proof_through_real_session_path_impl,
    )
}

async fn renamed_paths_can_receive_canonical_proof_through_real_session_path_impl() -> Result<()> {
    let fixture = CompletionProofFixture::new()?;
    let harness = fixture.harness().await?;
    let rename_call_id = "rename-source-before-canonical-proof";
    let canonical_call_id = "canonical-proof-after-rename";
    mount_sse_sequence(
        harness.server(),
        vec![
            exec_command_call_response(
                rename_call_id,
                &fixture.rename_into_documentation_command,
                &fixture.repo_path,
            ),
            exec_command_call_response(
                canonical_call_id,
                &fixture.canonical_command,
                &fixture.repo_path,
            ),
            terminal_candidate(1, "renamed workspace received current proof"),
        ],
    )
    .await;

    let events = submit_and_collect(
        harness.test(),
        "rename a tracked source path, certify the resulting workspace, and finish",
    )
    .await?;

    for call_id in [rename_call_id, canonical_call_id] {
        assert!(events.iter().any(|event| {
            matches!(
                event,
                EventMsg::ExecCommandEnd(end) if end.call_id == call_id && end.exit_code == 0
            )
        }));
    }
    assert!(!fixture.repo_path.join("src/runtime.rs").exists());
    assert!(fixture.repo_path.join("docs/renamed_runtime.rs").is_file());
    assert_eq!(fixture.canonical_launch_count()?, 1);
    let completion = events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing TurnComplete")?;
    assert!(
        completion.error.is_none(),
        "matching runtime and canonical fingerprints should certify a rename: {completion:#?}"
    );
    assert_eq!(
        completion.last_agent_message.as_deref(),
        Some("renamed workspace received current proof")
    );

    Ok(())
}

#[test]
fn documentation_only_change_without_validation_requires_explicit_override_through_real_turn_path()
-> Result<()> {
    run_session_path_test(
        "documentation_only_change_without_validation_requires_explicit_override_through_real_turn_path",
        documentation_only_change_without_validation_requires_explicit_override_through_real_turn_path_impl,
    )
}

async fn documentation_only_change_without_validation_requires_explicit_override_through_real_turn_path_impl()
-> Result<()> {
    let blocked_fixture = CompletionProofFixture::without_documentation_validation()?;
    blocked_fixture.prepare_documentation_only_change()?;
    let blocked_harness = blocked_fixture.harness().await?;
    mount_sse_sequence(
        blocked_harness.server(),
        repeated_terminal_candidates(
            TOTAL_GENERATIONS_WITH_FORCED_TERMINAL,
            "docs without validation must remain blocked",
        ),
    )
    .await;
    let blocked_events = submit_and_collect(
        blocked_harness.test(),
        "finish without a documentation validation or override",
    )
    .await?;
    assert!(
        blocked_events
            .iter()
            .all(|event| !is_assistant_output(event))
    );
    let blocked_completion = blocked_events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing blocked TurnComplete")?;
    assert!(
        blocked_completion.error.is_some(),
        "missing documentation validation and override must block completion"
    );
    assert!(blocked_completion.last_agent_message.is_none());
    let blocked_request_bodies = blocked_harness.request_bodies().await;
    assert!(
        blocked_request_bodies.iter().any(|body| {
            let body = body.to_string();
            body.contains("Documentation changed") && body.contains("provenance-backed override")
        }),
        "the real session did not receive the docs-only override requirement: \
         {blocked_request_bodies:#?}"
    );
    assert_eq!(
        blocked_request_bodies.len(),
        TOTAL_GENERATIONS_WITH_FORCED_TERMINAL
    );
    assert_eq!(blocked_fixture.canonical_launch_count()?, 0);
    assert_eq!(blocked_fixture.documentation_launch_count()?, 0);

    let override_fixture = CompletionProofFixture::without_documentation_validation()?;
    override_fixture.prepare_documentation_only_change()?;
    let override_harness = override_fixture.harness().await?;
    mount_sse_sequence(
        override_harness.server(),
        vec![terminal_candidate(
            1,
            "explicit override released documentation",
        )],
    )
    .await;
    let override_events = submit_and_collect(
        override_harness.test(),
        "completion-proof: allow completion without current proof",
    )
    .await?;
    let override_completion = override_events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing overridden TurnComplete")?;
    assert!(
        override_completion.error.is_none(),
        "a current-user override with real message provenance should release docs-only completion: {override_completion:#?}"
    );
    assert_eq!(
        override_completion.last_agent_message.as_deref(),
        Some("explicit override released documentation")
    );
    assert_eq!(override_fixture.canonical_launch_count()?, 0);
    assert_eq!(override_fixture.documentation_launch_count()?, 0);

    Ok(())
}

#[test]
fn spawned_agent_returns_evidence_while_only_root_completion_is_gated() -> Result<()> {
    run_session_path_test(
        "spawned_agent_returns_evidence_while_only_root_completion_is_gated",
        spawned_agent_returns_evidence_while_only_root_completion_is_gated_impl,
    )
}

async fn spawned_agent_returns_evidence_while_only_root_completion_is_gated_impl() -> Result<()> {
    const ROOT_PROMPT: &str = "delegate proof evidence, wait for it, then finish";
    const CHILD_PROMPT: &str = "collect completion-proof evidence for the root";
    const CHILD_EVIDENCE: &str = "child evidence complete";
    const AGENTS_NAMESPACE: &str = "agents";

    let fixture = CompletionProofFixture::new()?;
    let harness = fixture.multi_agent_harness().await?;
    let spawn_args = serde_json::to_string(&json!({
        "message": CHILD_PROMPT,
        "task_name": "proof_evidence",
        "fork_turns": "none",
    }))?;

    mount_sse_once_match(
        harness.server(),
        |request: &wiremock::Request| {
            response_request_contains(request, ROOT_PROMPT)
                && !response_request_contains(request, "function_call_output")
        },
        sse(vec![
            ev_response_created("root-spawn-response"),
            ev_function_call_with_namespace(
                "root-spawn-child",
                AGENTS_NAMESPACE,
                "spawn_agent",
                &spawn_args,
            ),
            ev_completed("root-spawn-response"),
        ]),
    )
    .await;

    let canonical_command = fixture.canonical_command.clone();
    mount_sse_once_match(
        harness.server(),
        |request: &wiremock::Request| {
            response_request_contains(request, "<task_capsule_v1>")
                && response_request_contains(request, CHILD_PROMPT)
                && !response_request_contains(request, "child-canonical-attempt")
        },
        exec_command_call_response(
            "child-canonical-attempt",
            &canonical_command,
            &fixture.repo_path,
        ),
    )
    .await;
    mount_sse_once_match(
        harness.server(),
        |request: &wiremock::Request| response_request_contains(request, "child-canonical-attempt"),
        sse(vec![
            ev_response_created("child-evidence-response"),
            ev_assistant_message("child-evidence-message", CHILD_EVIDENCE),
            ev_completed("child-evidence-response"),
        ]),
    )
    .await;

    mount_sse_once_match(
        harness.server(),
        |request: &wiremock::Request| {
            response_request_contains(request, ROOT_PROMPT)
                && response_request_contains(request, "root-spawn-child")
                && !response_request_contains(request, "root-wait-child")
        },
        sse(vec![
            ev_response_created("root-wait-response"),
            ev_function_call_with_namespace(
                "root-wait-child",
                AGENTS_NAMESPACE,
                "wait_agent",
                "{}",
            ),
            ev_completed("root-wait-response"),
        ]),
    )
    .await;
    mount_sse_once_match(
        harness.server(),
        |request: &wiremock::Request| {
            response_request_contains(request, ROOT_PROMPT)
                && response_request_contains(request, "root-wait-child")
                && !response_request_contains(request, CHILD_EVIDENCE)
        },
        sse(vec![
            ev_response_created("root-wait-again-response"),
            ev_function_call_with_namespace(
                "root-wait-child-again",
                AGENTS_NAMESPACE,
                "wait_agent",
                "{}",
            ),
            ev_completed("root-wait-again-response"),
        ]),
    )
    .await;
    mount_sse_once_match(
        harness.server(),
        |request: &wiremock::Request| {
            response_request_contains(request, ROOT_PROMPT)
                && response_request_contains(request, "root-wait-child-again")
                && response_request_contains(request, CHILD_EVIDENCE)
        },
        terminal_candidate(1, "root premature success"),
    )
    .await;

    // Hold the gate-directed retry open long enough to observe that root success was withheld.
    mount_response_once_match(
        harness.server(),
        |request: &wiremock::Request| {
            response_request_contains(
                request,
                "CompletionProofGate blocked successful terminal completion",
            )
        },
        sse_response(terminal_candidate(2, "must remain private"))
            .set_delay(Duration::from_secs(30)),
    )
    .await;

    submit_without_wait(harness.test(), ROOT_PROMPT).await?;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut events = Vec::new();
    let request_bodies = loop {
        let bodies = harness.request_bodies().await;
        if bodies.iter().any(|body| {
            body.to_string()
                .contains("CompletionProofGate blocked successful terminal completion")
        }) {
            break bodies;
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        anyhow::ensure!(
            !remaining.is_zero(),
            "timed out waiting for the root completion-proof retry; request_count={}, child_attempt_seen={}, child_evidence_seen={}, root_spawn_seen={}, root_wait_seen={}",
            bodies.len(),
            bodies
                .iter()
                .any(|body| body.to_string().contains("child-canonical-attempt")),
            bodies
                .iter()
                .any(|body| body.to_string().contains(CHILD_EVIDENCE)),
            bodies
                .iter()
                .any(|body| body.to_string().contains("root-spawn-child")),
            bodies
                .iter()
                .any(|body| body.to_string().contains("root-wait-child")),
        );
        if let Ok(event) = tokio::time::timeout(
            remaining.min(Duration::from_millis(50)),
            harness.test().codex.next_event(),
        )
        .await
        {
            let event = event?;
            anyhow::ensure!(
                !matches!(
                    &event.msg,
                    EventMsg::TurnComplete(completion)
                        if completion.error.is_none()
                            && completion.last_agent_message.as_deref()
                                == Some("root premature success")
                ),
                "root terminal success escaped before certification"
            );
            events.push(event.msg);
        }
    };

    assert!(
        request_bodies.iter().any(|body| {
            let body = body.to_string();
            body.contains("child-canonical-attempt")
                && body.contains("only the root Codex session may own and register")
        }),
        "the child did not receive the root-only canonical-command rejection: {request_bodies:#?}"
    );
    assert!(
        request_bodies.iter().any(|body| {
            let body = body.to_string();
            body.contains("root-wait-child") && body.contains(CHILD_EVIDENCE)
        }),
        "wait_agent did not return the child's completed evidence to the root: {request_bodies:#?}"
    );
    assert!(
        events.iter().all(|event| !is_assistant_output(event)),
        "the root's final-looking output escaped before certification: {events:#?}"
    );
    assert_eq!(
        fixture.canonical_launch_count()?,
        0,
        "a child or the terminal gate launched canonical certification"
    );

    harness.test().codex.submit(Op::Interrupt).await?;
    let terminal_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let remaining = terminal_deadline.saturating_duration_since(tokio::time::Instant::now());
        anyhow::ensure!(
            !remaining.is_zero(),
            "timed out interrupting the blocked root turn"
        );
        let event = tokio::time::timeout(remaining, harness.test().codex.next_event())
            .await
            .context("timed out waiting for blocked root terminal event")??;
        match event.msg {
            EventMsg::TurnAborted(_) => break,
            EventMsg::TurnComplete(completion) => {
                anyhow::ensure!(
                    completion.error.is_some() && completion.last_agent_message.is_none(),
                    "the root published successful completion without proof: {completion:#?}"
                );
                break;
            }
            _ => {}
        }
    }

    Ok(())
}

#[test]
fn explicit_admission_not_session_source_selects_terminal_authority() -> Result<()> {
    run_session_path_test(
        "explicit_admission_not_session_source_selects_terminal_authority",
        explicit_admission_not_session_source_selects_terminal_authority_impl,
    )
}

async fn explicit_admission_not_session_source_selects_terminal_authority_impl() -> Result<()> {
    const ROOT_ONLY_DIAGNOSTIC: &str =
        "only the root Codex session may own and register whole-repository certification";

    let fixture = CompletionProofFixture::new()?;
    let cwd = fixture.repo_path.abs();
    let harness = TestCodexHarness::with_builder(
        test_codex()
            .with_session_source(SessionSource::Internal(
                InternalSessionSource::MemoryConsolidation,
            ))
            .with_raw_response_items()
            .with_config(move |config| set_fixture_workspace(config, cwd)),
    )
    .await?;
    let public_call_id = "public-root-with-non-root-session-source";
    let contributor_call_id = "explicit-memory-contributor-cannot-certify";
    let resumed_contributor_call_id = "resumed-memory-contributor-cannot-certify";
    let forked_contributor_call_id = "forked-memory-contributor-cannot-certify";
    let public_prompt = "certify through public admission with non-root display metadata";
    let contributor_prompt = "attempt certification through the memory-consolidation admission";
    let resumed_contributor_prompt =
        "attempt certification after publicly resuming the stopped memory contributor";
    let forked_contributor_prompt =
        "attempt certification after publicly forking the stopped memory contributor";
    mount_sse_once_match(
        harness.server(),
        move |request: &wiremock::Request| {
            response_request_contains(request, public_prompt)
                && !response_request_contains(request, public_call_id)
        },
        exec_command_call_response(
            public_call_id,
            &fixture.canonical_command,
            &fixture.repo_path,
        ),
    )
    .await;
    let _public_continuation = mount_sse_once_match(
        harness.server(),
        move |request: &wiremock::Request| response_request_contains(request, public_call_id),
        terminal_candidate(1, "public admission retained root authority"),
    )
    .await;
    mount_sse_once_match(
        harness.server(),
        move |request: &wiremock::Request| {
            response_request_contains(request, contributor_prompt)
                && !response_request_contains(request, contributor_call_id)
        },
        exec_command_call_response(
            contributor_call_id,
            &fixture.canonical_command,
            &fixture.repo_path,
        ),
    )
    .await;
    let _contributor_continuation = mount_sse_once_match(
        harness.server(),
        move |request: &wiremock::Request| response_request_contains(request, contributor_call_id),
        terminal_candidate(2, "memory contributor returned evidence"),
    )
    .await;
    mount_sse_once_match(
        harness.server(),
        move |request: &wiremock::Request| {
            response_request_contains(request, resumed_contributor_prompt)
                && !response_request_contains(request, resumed_contributor_call_id)
        },
        exec_command_call_response(
            resumed_contributor_call_id,
            &fixture.canonical_command,
            &fixture.repo_path,
        ),
    )
    .await;
    let _resumed_contributor_continuation = mount_sse_once_match(
        harness.server(),
        move |request: &wiremock::Request| {
            response_request_contains(request, resumed_contributor_prompt)
                && response_request_contains(request, resumed_contributor_call_id)
        },
        terminal_candidate(3, "resumed memory contributor returned evidence"),
    )
    .await;
    mount_sse_once_match(
        harness.server(),
        move |request: &wiremock::Request| {
            response_request_contains(request, forked_contributor_prompt)
                && !response_request_contains(request, forked_contributor_call_id)
        },
        exec_command_call_response(
            forked_contributor_call_id,
            &fixture.canonical_command,
            &fixture.repo_path,
        ),
    )
    .await;
    let _forked_contributor_continuation = mount_sse_once_match(
        harness.server(),
        move |request: &wiremock::Request| {
            response_request_contains(request, forked_contributor_prompt)
                && response_request_contains(request, forked_contributor_call_id)
        },
        terminal_candidate(4, "forked memory contributor returned evidence"),
    )
    .await;

    let thread_manager = Arc::clone(&harness.test().thread_manager);
    let config = harness.test().config.clone();
    let public_events = submit_and_collect(harness.test(), public_prompt)
        .await
        .context("public root admission did not reach TurnComplete")?;
    let public_output = function_call_output(&public_events, public_call_id)
        .context("missing public-admission canonical-command output in the model continuation")?;
    let public_launch_count = fixture.canonical_launch_count()?;
    let public_completion = public_events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing public-admission TurnComplete")?;
    assert!(
        public_completion.error.is_none(),
        "caller-controlled SessionSource removed public root authority: canonical_output={public_output:?}, canonical_launches={public_launch_count}, terminal_error={:?}",
        public_completion.error
    );
    assert_eq!(public_launch_count, 1);
    harness.test().codex.shutdown_and_wait().await?;

    let mut contributor_options = thread_manager.start_thread_options(config.clone());
    contributor_options.session_source = Some(SessionSource::Internal(
        InternalSessionSource::MemoryConsolidation,
    ));
    let contributor = thread_manager
        .start_evidence_contributor_thread_with_options(contributor_options)
        .await?;
    contributor.thread.request_raw_response_items();
    let contributor_events =
        submit_and_collect_thread(&contributor.thread, &config, contributor_prompt)
            .await
            .context("evidence-contributor admission did not reach TurnComplete")?;
    let contributor_output = function_call_output(&contributor_events, contributor_call_id)
        .context("missing contributor canonical-command output in the model continuation")?;
    assert_eq!(
        function_call_output_success(&contributor_events, contributor_call_id),
        Some(false)
    );
    assert!(
        contributor_output.contains(ROOT_ONLY_DIAGNOSTIC),
        "explicit contributor admission did not reject terminal certification: {contributor_output}"
    );
    assert_eq!(
        fixture.canonical_launch_count()?,
        1,
        "contributor rejection launched canonical certification"
    );
    let contributor_rollout = contributor
        .thread
        .rollout_path()
        .context("missing contributor rollout for public live-resume regression")?;
    let live_resume = thread_manager
        .resume_thread_from_rollout(
            config.clone(),
            contributor_rollout.clone(),
            thread_manager.auth_manager(),
            /*parent_trace*/ None,
            /*supports_openai_form_elicitation*/ false,
        )
        .await?;
    assert!(
        live_resume.was_already_running,
        "public live resume did not safely reuse the registered evidence contributor"
    );
    assert!(Arc::ptr_eq(&live_resume.thread, &contributor.thread));
    assert_eq!(fixture.canonical_launch_count()?, 1);
    contributor.thread.shutdown_and_wait().await?;
    let removed_contributor = thread_manager
        .remove_thread(&contributor.thread_id)
        .await
        .context("stopped evidence contributor was missing from the thread manager")?;
    assert!(Arc::ptr_eq(&removed_contributor, &contributor.thread));

    let resumed_contributor = thread_manager
        .resume_thread_from_rollout(
            config.clone(),
            contributor_rollout.clone(),
            thread_manager.auth_manager(),
            /*parent_trace*/ None,
            /*supports_openai_form_elicitation*/ false,
        )
        .await?;
    assert!(!resumed_contributor.was_already_running);
    assert_eq!(resumed_contributor.thread_id, contributor.thread_id);
    assert!(!Arc::ptr_eq(
        &resumed_contributor.thread,
        &contributor.thread
    ));
    resumed_contributor.thread.request_raw_response_items();
    let resumed_events = submit_and_collect_thread(
        &resumed_contributor.thread,
        &config,
        resumed_contributor_prompt,
    )
    .await
    .context("publicly resumed contributor admission did not reach TurnComplete")?;
    let resumed_output = function_call_output(&resumed_events, resumed_contributor_call_id)
        .context("missing resumed-contributor canonical-command output")?;
    assert_eq!(
        function_call_output_success(&resumed_events, resumed_contributor_call_id),
        Some(false)
    );
    assert!(
        resumed_output.contains(ROOT_ONLY_DIAGNOSTIC),
        "public resume changed contributor certification authority: {resumed_output}"
    );
    assert_eq!(
        fixture.canonical_launch_count()?,
        1,
        "resumed contributor rejection launched canonical certification"
    );
    resumed_contributor.thread.shutdown_and_wait().await?;
    let removed_resumed_contributor = thread_manager
        .remove_thread(&resumed_contributor.thread_id)
        .await
        .context("resumed evidence contributor was missing from the thread manager")?;
    assert!(Arc::ptr_eq(
        &removed_resumed_contributor,
        &resumed_contributor.thread
    ));

    let forked_contributor = Box::pin(thread_manager.fork_thread(
        2,
        config.clone(),
        contributor_rollout,
        /*thread_source*/ None,
        /*parent_trace*/ None,
    ))
    .await?;
    assert!(!forked_contributor.was_already_running);
    assert_ne!(forked_contributor.thread_id, contributor.thread_id);
    forked_contributor.thread.request_raw_response_items();
    let forked_events = submit_and_collect_thread(
        &forked_contributor.thread,
        &config,
        forked_contributor_prompt,
    )
    .await
    .context("publicly forked contributor admission did not reach TurnComplete")?;
    let forked_output = function_call_output(&forked_events, forked_contributor_call_id)
        .context("missing forked-contributor canonical-command output")?;
    assert_eq!(
        function_call_output_success(&forked_events, forked_contributor_call_id),
        Some(false)
    );
    assert!(
        forked_output.contains(ROOT_ONLY_DIAGNOSTIC),
        "public fork changed contributor certification authority: {forked_output}"
    );
    assert_eq!(
        fixture.canonical_launch_count()?,
        1,
        "forked contributor rejection launched canonical certification"
    );
    forked_contributor.thread.shutdown_and_wait().await?;
    assert!(
        thread_manager
            .remove_thread(&forked_contributor.thread_id)
            .await
            .is_some()
    );

    Ok(())
}

#[cfg(windows)]
#[test]
fn differently_cased_windows_paths_preserve_repository_and_rollout_identity() -> Result<()> {
    run_session_path_test(
        "differently_cased_windows_paths_preserve_repository_and_rollout_identity",
        differently_cased_windows_paths_preserve_repository_and_rollout_identity_impl,
    )
}

#[cfg(windows)]
async fn differently_cased_windows_paths_preserve_repository_and_rollout_identity_impl()
-> Result<()> {
    let fixture = CompletionProofFixture::new()?;
    let harness = fixture.harness_with_raw_response_items().await?;
    let differently_cased_repo = differently_cased_windows_path(&fixture.repo_path);
    let call_id = "canonical-proof-from-differently-cased-repository";
    mount_sse_sequence(
        harness.server(),
        vec![
            exec_command_call_response(
                call_id,
                &fixture.canonical_command,
                &differently_cased_repo,
            ),
            terminal_candidate(1, "differently cased repository retained authority"),
            terminal_candidate(2, "differently cased rollout retained issuance"),
        ],
    )
    .await;

    let initial_events = submit_and_collect(
        harness.test(),
        "certify from a differently cased repository path",
    )
    .await?;
    let initial_completion = initial_events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing differently-cased repository TurnComplete")?;
    assert!(
        initial_completion.error.is_none(),
        "Windows repository identity remained case-sensitive: {initial_completion:#?}"
    );

    let rollout_path = harness
        .test()
        .codex
        .rollout_path()
        .context("missing rollout for Windows identity resume")?;
    let differently_cased_rollout = differently_cased_windows_path(&rollout_path);
    let thread_manager = Arc::clone(&harness.test().thread_manager);
    let mut resumed_config = harness.test().config.clone();
    set_fixture_workspace(
        &mut resumed_config,
        AbsolutePathBuf::from_absolute_path(&differently_cased_repo)?,
    );
    let live_resume = thread_manager
        .resume_thread_from_rollout(
            resumed_config.clone(),
            differently_cased_rollout.clone(),
            thread_manager.auth_manager(),
            /*parent_trace*/ None,
            /*supports_openai_form_elicitation*/ false,
        )
        .await?;
    assert!(
        live_resume.was_already_running,
        "a differently cased live rollout path did not reuse the loaded public root thread"
    );
    assert!(Arc::ptr_eq(&live_resume.thread, &harness.test().codex));
    harness.test().codex.shutdown_and_wait().await?;

    let resumed = thread_manager
        .resume_thread_from_rollout(
            resumed_config.clone(),
            differently_cased_rollout,
            thread_manager.auth_manager(),
            /*parent_trace*/ None,
            /*supports_openai_form_elicitation*/ false,
        )
        .await?
        .thread;
    let resumed_events = submit_and_collect_thread(
        &resumed,
        &resumed_config,
        "finish from a differently cased rollout path",
    )
    .await?;
    let resumed_completion = resumed_events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing differently-cased rollout TurnComplete")?;
    let resumed_request_bodies = harness.request_bodies().await;
    let resumed_gate_feedback = resumed_request_bodies
        .iter()
        .filter_map(|body| {
            let body = body.to_string();
            body.find("CompletionProofGate")
                .map(|offset| body[offset..].chars().take(2_000).collect::<String>())
        })
        .collect::<Vec<_>>();
    assert!(
        resumed_completion.error.is_none(),
        "Windows rollout identity remained case-sensitive; gate feedback: {resumed_gate_feedback:#?}; completion: {resumed_completion:#?}"
    );
    assert_eq!(
        resumed_completion.last_agent_message.as_deref(),
        Some("differently cased rollout retained issuance")
    );
    assert_eq!(fixture.canonical_launch_count()?, 1);
    resumed.shutdown_and_wait().await?;

    Ok(())
}

#[test]
fn successful_constituent_command_does_not_satisfy_terminal_gate() -> Result<()> {
    run_session_path_test(
        "successful_constituent_command_does_not_satisfy_terminal_gate",
        successful_constituent_command_does_not_satisfy_terminal_gate_impl,
    )
}

async fn successful_constituent_command_does_not_satisfy_terminal_gate_impl() -> Result<()> {
    let fixture = CompletionProofFixture::new()?;
    let harness = fixture.harness().await?;
    let call_id = "focused-constituent-command";
    let mut responses = vec![exec_command_call_response(
        call_id,
        "git --version",
        &fixture.repo_path,
    )];
    responses.extend(repeated_terminal_candidates(
        MAX_REGULAR_LOGICAL_GENERATIONS,
        "focused success is not certification",
    ));
    mount_sse_sequence(harness.server(), responses).await;

    let events = submit_and_collect(harness.test(), "run a focused check, then finish").await?;

    assert!(events.iter().any(|event| {
        matches!(
            event,
            EventMsg::ExecCommandEnd(end) if end.call_id == call_id && end.exit_code == 0
        )
    }));
    assert!(events.iter().all(|event| !is_assistant_output(event)));
    let completion = events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing TurnComplete")?;
    assert!(completion.error.is_some());
    assert!(completion.last_agent_message.is_none());
    assert!(
        !fixture.marker_path.exists(),
        "a constituent command was mistaken for the exact canonical command"
    );

    Ok(())
}

#[test]
fn successful_focused_validation_does_not_satisfy_terminal_gate() -> Result<()> {
    run_session_path_test(
        "successful_focused_validation_does_not_satisfy_terminal_gate",
        successful_focused_validation_does_not_satisfy_terminal_gate_impl,
    )
}

#[cfg(windows)]
#[test]
fn current_evidence_real_session_executes_both_and_rejects_nonterminal_misuse() -> Result<()> {
    run_session_path_test(
        "current_evidence_real_session_executes_both_and_rejects_nonterminal_misuse",
        current_evidence_real_session_executes_both_and_rejects_nonterminal_misuse_impl,
    )
}

#[cfg(windows)]
async fn current_evidence_real_session_executes_both_and_rejects_nonterminal_misuse_impl()
-> Result<()> {
    let fixture = CurrentEvidenceFixture::new()?;
    let harness = fixture.harness().await?;

    let unified_call = "current-evidence-unified-success";
    let unified_poll = "await-current-evidence-unified-success";
    let unified_stop = "stop-after-current-evidence-terminal-rejection";
    let premature_answer = "focused current evidence is not terminal proof";
    let (capture_tool_completed, capture) =
        current_evidence_report_barrier(&fixture, harness.test().codex_home_path(), None)?;
    mount_sse_sequence(
        harness.server(),
        vec![
            exec_command_call_response(unified_call, &fixture.exact_command, &fixture.repo_path),
            write_stdin_call_response(unified_poll, 1000),
            terminal_candidate(1, premature_answer),
            required_failure_call_response(unified_stop),
        ],
    )
    .await;
    let unified_events = submit_and_collect(
        harness.test(),
        "run exact current evidence through unified exec, then try to finish",
    )
    .await?;
    capture_tool_completed.store(true, std::sync::atomic::Ordering::Release);
    let captured_report = capture
        .join()
        .map_err(|_| anyhow::anyhow!("current-evidence report capture thread panicked"))?;
    let initial_unified_output = function_call_output(&unified_events, unified_call)
        .unwrap_or("<missing initial function-call output>");
    let unified_success = function_call_output_success(&unified_events, unified_poll);
    let unified_output = function_call_output(&unified_events, unified_poll)
        .unwrap_or("<missing function-call output>");
    let captured_report_diagnostics = current_evidence_report_diagnostics(&captured_report);
    anyhow::ensure!(
        unified_success == Some(true),
        "initial current-evidence tool call failed before report capture: initial_output={initial_unified_output}; completion_success={unified_success:?}; completion_output={unified_output}; {}; report={}",
        fixture.driver_diagnostics(),
        captured_report_diagnostics,
    );
    let accepted_report = captured_report.with_context(|| {
        format!(
            "initial current-evidence report capture failed after tool output: {unified_output}; {}; report={}",
            fixture.driver_diagnostics(),
            captured_report_diagnostics,
        )
    })?;
    assert!(initial_unified_output.contains("Process running with session ID 1000"));
    assert!(
        unified_output.contains("Historical successor mappings remain unresolved")
            && unified_output
                .contains("not completion certification, admission, or review approval"),
        "accepted current evidence had the wrong nonterminal message: {unified_output}"
    );
    let completion = unified_events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing TurnComplete after current-evidence terminal rejection")?;
    let required_failure = format!("required tool `{REQUIRED_FAILURE_TOOL_NAME}` blocked");
    assert_eq!(
        completion
            .error
            .as_ref()
            .map(|error| error.message.as_str()),
        Some(required_failure.as_str())
    );
    assert_eq!(
        completion.last_agent_message.as_deref(),
        Some(required_failure.as_str())
    );
    assert!(
        unified_events
            .iter()
            .all(|event| !assistant_output_contains(event, premature_answer)),
        "current evidence released the uncertified terminal candidate"
    );
    assert!(!fixture.canonical_marker.exists());

    let report: serde_json::Value = serde_json::from_slice(&accepted_report)?;
    assert_eq!(
        report["focused_validation_id"],
        json!("inventory.current-evidence")
    );
    assert_eq!(report["attempt_classification"], json!("confirmed_pass"));
    assert_eq!(
        report["validations"]
            .as_array()
            .context("current-evidence report validations were not an array")?
            .iter()
            .map(|item| item["id"].as_str().unwrap_or_default())
            .collect::<Vec<_>>(),
        vec!["maintenance.root-unittest", "sdk.python.pytest"]
    );
    assert_eq!(
        report["child_processes"]
            .as_array()
            .context("current-evidence report children were not an array")?
            .iter()
            .map(|item| item["validation_id"].as_str().unwrap_or_default())
            .collect::<Vec<_>>(),
        vec!["maintenance.root-unittest", "sdk.python.pytest"]
    );
    assert_fresh_current_evidence_markers(&fixture, 1, 1)?;

    let shell_call = "current-evidence-direct-shell-success";
    let shell_stop = "stop-after-current-evidence-direct-shell";
    mount_sse_sequence(
        harness.server(),
        vec![
            shell_command_call_response(shell_call, &fixture.exact_command),
            required_failure_call_response(shell_stop),
        ],
    )
    .await;
    let shell_events = submit_and_collect(
        harness.test(),
        "run the same exact current evidence through the direct shell path",
    )
    .await?;
    assert_eq!(
        function_call_output_success(&shell_events, shell_call),
        Some(true)
    );
    assert_fresh_current_evidence_markers(&fixture, 2, 2)?;
    assert!(!fixture.canonical_marker.exists());

    let replay_call = "current-evidence-replayed-private-report";
    let replay_stop = "stop-after-current-evidence-replay";
    let (replay_tool_completed, replay_barrier) = current_evidence_report_barrier(
        &fixture,
        harness.test().codex_home_path(),
        Some(accepted_report),
    )?;
    mount_sse_sequence(
        harness.server(),
        vec![
            shell_command_call_response(replay_call, &fixture.exact_command),
            required_failure_call_response(replay_stop),
        ],
    )
    .await;
    let replay_events = submit_and_collect(
        harness.test(),
        "run fresh current evidence but replay the prior private top report",
    )
    .await?;
    replay_tool_completed.store(true, std::sync::atomic::Ordering::Release);
    let replay_barrier_result = replay_barrier
        .join()
        .map_err(|_| anyhow::anyhow!("current-evidence replay thread panicked"))?;
    if let Err(error) = replay_barrier_result {
        let output = function_call_output(&replay_events, replay_call)
            .unwrap_or("<missing function-call output>");
        anyhow::bail!(
            "current-evidence replay barrier failed: {error}; output={output}; {}",
            fixture.driver_diagnostics()
        );
    }
    assert_eq!(
        function_call_output_success(&replay_events, replay_call),
        Some(false)
    );
    let replay_output = function_call_output(&replay_events, replay_call)
        .context("missing replay rejection output")?;
    assert!(
        replay_output.contains("copied, replayed, or did not match this exact invocation")
            || replay_output.contains("did not bind this exact focused attempt"),
        "old current-evidence report was not rejected by invocation binding: {replay_output}"
    );
    assert_fresh_current_evidence_markers(&fixture, 3, 3)?;
    assert!(!fixture.canonical_marker.exists());

    fs::write(&fixture.control_path, "pre-result\n")?;
    let pre_result_call = "current-evidence-discovery-pre-result";
    let pre_result_stop = "stop-after-current-evidence-pre-result";
    mount_sse_sequence(
        harness.server(),
        vec![
            shell_command_call_response(pre_result_call, &fixture.exact_command),
            required_failure_call_response(pre_result_stop),
        ],
    )
    .await;
    let pre_result_events = submit_and_collect(
        harness.test(),
        "run current evidence with a discovery pre-result",
    )
    .await?;
    assert_eq!(
        function_call_output_success(&pre_result_events, pre_result_call),
        Some(false)
    );
    assert_fresh_current_evidence_markers(&fixture, 3, 3)?;
    fs::remove_file(&fixture.control_path)?;

    let after_pre_result_call = "current-evidence-pass-after-pre-result";
    let after_pre_result_stop = "stop-after-current-evidence-pre-result-retry";
    mount_sse_sequence(
        harness.server(),
        vec![
            shell_command_call_response(after_pre_result_call, &fixture.exact_command),
            required_failure_call_response(after_pre_result_stop),
        ],
    )
    .await;
    let after_pre_result_events = submit_and_collect(
        harness.test(),
        "retry unchanged after the current-evidence pre-result",
    )
    .await?;
    assert_eq!(
        function_call_output_success(&after_pre_result_events, after_pre_result_call),
        Some(true),
        "a failure before confirmed result incorrectly poisoned the retry"
    );
    assert_fresh_current_evidence_markers(&fixture, 4, 4)?;

    fs::write(&fixture.control_path, "unit-fail\npytest-skip\n")?;
    let mixed_failure_call = "current-evidence-confirmed-failure-plus-pre-result";
    let mixed_failure_stop = "stop-after-current-evidence-mixed-failure";
    mount_sse_sequence(
        harness.server(),
        vec![
            shell_command_call_response(mixed_failure_call, &fixture.exact_command),
            required_failure_call_response(mixed_failure_stop),
        ],
    )
    .await;
    let mixed_failure_events = submit_and_collect(
        harness.test(),
        "record the real unittest failure even though pytest has no confirmed result",
    )
    .await?;
    assert_eq!(
        function_call_output_success(&mixed_failure_events, mixed_failure_call),
        Some(false)
    );
    let mixed_output = function_call_output(&mixed_failure_events, mixed_failure_call)
        .context("missing mixed current-evidence failure output")?;
    assert!(
        mixed_output.contains("recorded confirmed failures for maintenance.root-unittest"),
        "mixed current evidence lost the confirmed unittest failure: {mixed_output}"
    );
    assert!(
        mixed_output.contains("No broader validation was started"),
        "mixed current evidence had the wrong failure disposition: {mixed_output}"
    );
    assert!(!fixture.canonical_marker.exists());
    assert_fresh_current_evidence_markers(&fixture, 5, 4)?;
    fs::remove_file(&fixture.control_path)?;

    let poisoned_retry_call = "current-evidence-unchanged-poisoned-retry";
    let poisoned_retry_stop = "stop-after-current-evidence-poisoned-retry";
    mount_sse_sequence(
        harness.server(),
        vec![
            shell_command_call_response(poisoned_retry_call, &fixture.exact_command),
            required_failure_call_response(poisoned_retry_stop),
        ],
    )
    .await;
    let poisoned_retry_events = submit_and_collect(
        harness.test(),
        "retry current evidence unchanged after its confirmed failure",
    )
    .await?;
    assert_eq!(
        function_call_output_success(&poisoned_retry_events, poisoned_retry_call),
        Some(false),
        "same-epoch successful execution retired current-evidence poison"
    );
    assert_fresh_current_evidence_markers(&fixture, 6, 5)?;
    assert!(!fixture.canonical_marker.exists());

    Ok(())
}

#[cfg(windows)]
#[test]
fn historical_acceptance_requires_live_reviewer_and_current_approval() -> Result<()> {
    run_session_path_test(
        "historical_acceptance_requires_live_reviewer_and_current_approval",
        historical_acceptance_requires_live_reviewer_and_current_approval_impl,
    )
}

#[cfg(windows)]
fn historical_receipt_args(criterion: &str, proposal_path: Option<&str>) -> serde_json::Value {
    let mut args = json!({
        "status": "completed",
        "summary": "Reviewed the complete pinned historical replacement plan",
        "criterion_results": [{
            "criterion_id": criterion,
            "status": "passed",
            "evidence": "All 531 exact scopes in the pinned plan were reviewed against their frozen behavior and current successor path."
        }],
        "declared_changes": [], "validation_call_ids": [], "blockers": [], "risks": [],
        "next_action": null
    });
    if let Some(path) = proposal_path {
        args["historical_acceptance_proposal_path"] = json!(path);
    }
    args
}

#[cfg(windows)]
fn historical_agent_call(call_id: &str, name: &str, args: &serde_json::Value) -> String {
    agent_tool_call_response(call_id, name, &args.to_string())
}

#[cfg(windows)]
async fn spawn_historical_fixture_agent(
    harness: &TestCodexHarness,
    task_name: &'static str,
    role: &str,
    criterion: &str,
    target_assignment: Option<&str>,
    tamper_assignment: bool,
    calls: Vec<(&'static str, serde_json::Value)>,
) -> Result<(String, Arc<CodexThread>, Vec<EventMsg>)> {
    let root_prompt = format!("Spawn an agent for gate11 fixture {task_name}");
    let objective = format!("gate11 independent fixture work {task_name}");
    let call_ids = calls.iter().map(|(id, _)| *id).collect::<Vec<_>>();
    let mut first_response = None;
    for (index, (call_id, args)) in calls.into_iter().enumerate() {
        let objective = objective.clone();
        let previous = index.checked_sub(1).map(|previous| call_ids[previous]);
        let response = sse_response(historical_agent_call(
            call_id,
            "submit_agent_receipt",
            &args,
        ));
        let response_mock = mount_response_once_match(
            harness.server(),
            move |request: &wiremock::Request| {
                response_request_contains(request, "<task_capsule_v1>")
                    && response_request_contains(request, &objective)
                    && previous.is_none_or(|previous| response_request_contains(request, previous))
                    && !response_request_contains(request, call_id)
            },
            if index == 0 {
                response.set_delay(Duration::from_secs(3))
            } else {
                response
            },
        )
        .await;
        if index == 0 {
            first_response = Some(response_mock);
        }
    }
    let last_call = *call_ids
        .last()
        .context("historical fixture needs a tool call")?;
    mount_sse_once_match(
        harness.server(),
        move |request: &wiremock::Request| response_request_contains(request, last_call),
        sse(vec![
            ev_response_created("historical-child-finished"),
            ev_assistant_message(
                "historical-child-result",
                "Independent fixture review finished",
            ),
            ev_completed("historical-child-finished"),
        ]),
    )
    .await;
    let args = json!({
        "task_name": task_name, "agent_type": role, "fork_turns": "none",
        "assignment": {
            "objective": objective,
            "acceptance_criteria": [{"id": criterion, "text": "Review the entire exact plan and seal the result"}],
            "read_scope": [{"path": ".", "recursive": true}],
            "write_scope": [],
            "stop_condition": "Seal the exact requested receipt, then stop",
            "dependencies": target_assignment.into_iter().collect::<Vec<_>>(),
            "relation": target_assignment.map(|target| json!({"kind": "review", "target_assignment_ids": [target]}))
        }
    });
    let root_start = root_prompt.clone();
    let spawn_call = format!("spawn-{task_name}");
    let spawn_match = spawn_call.clone();
    mount_sse_once_match(
        harness.server(),
        move |request: &wiremock::Request| {
            response_request_contains(request, &root_start)
                && !response_request_contains(request, &spawn_match)
        },
        historical_agent_call(&spawn_call, "spawn_agent", &args),
    )
    .await;
    let spawn_match = spawn_call.clone();
    mount_sse_once_match(
        harness.server(),
        move |request: &wiremock::Request| response_request_contains(request, &spawn_match),
        required_failure_call_response(&format!("stop-root-after-{task_name}")),
    )
    .await;
    let before = harness.test().thread_manager.list_thread_ids().await;
    let root_events = submit_and_collect(harness.test(), &root_prompt).await?;
    let output = function_call_output(&root_events, &spawn_call)
        .with_context(|| format!("missing {task_name} spawn output: {root_events:#?}"))?;
    let spawn = tool_output_json(output, "/assignment_id")
        .with_context(|| format!("{task_name} spawn failed: {output}"))?;
    let assignment_id = spawn["assignment_id"]
        .as_str()
        .with_context(|| format!("{task_name} spawn did not admit an assignment: {output}"))?
        .to_owned();
    if tamper_assignment {
        // Model the stated threat directly: ordinary SQLite bytes can be imported or edited.
        // Only this test's temporary home is touched; the immutable trigger is restored before
        // the real reviewer submits its proposal through the registered runtime tool.
        let script = r#"
import json, sqlite3, sys
connection = sqlite3.connect(sys.argv[1])
with connection:
    connection.execute('BEGIN IMMEDIATE')
    trigger = connection.execute("SELECT sql FROM sqlite_master WHERE type = 'trigger' AND name = 'assignments_immutable_update'").fetchone()[0]
    body = json.loads(connection.execute('SELECT body_json FROM assignments WHERE assignment_id = ?', (sys.argv[2],)).fetchone()[0])
    body['objective'] += ' - unauthenticated replacement contract'
    connection.execute('DROP TRIGGER assignments_immutable_update')
    connection.execute('UPDATE assignments SET body_json = ? WHERE assignment_id = ?', (json.dumps(body), sys.argv[2]))
    connection.execute(trigger)
"#;
        let database_path = harness
            .test()
            .config
            .sqlite_home
            .join("agent-task-coordination/agent_tasks.sqlite");
        anyhow::ensure!(
            database_path.is_file(),
            "missing fixture task database: {}",
            database_path.display()
        );
        let output = Command::new(available_python_command()?)
            .args(["-c", script])
            .arg(&database_path)
            .arg(&assignment_id)
            .output()?;
        anyhow::ensure!(
            output.status.success(),
            "fixture task-store forgery failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let new_threads = harness
        .test()
        .thread_manager
        .list_thread_ids()
        .await
        .into_iter()
        .filter(|id| !before.contains(id))
        .collect::<Vec<_>>();
    anyhow::ensure!(
        new_threads.len() == 1,
        "expected one fresh child, found {new_threads:?}"
    );
    let thread = harness
        .test()
        .thread_manager
        .get_thread(new_threads[0])
        .await?;
    thread.request_raw_response_items();
    wait_for_response_request(
        first_response.as_ref().context("missing child response")?,
        task_name,
    )
    .await?;
    let events = tokio::time::timeout(Duration::from_secs(60), async {
        let mut events = Vec::new();
        loop {
            let event = thread.next_event().await?;
            let complete = matches!(event.msg, EventMsg::TurnComplete(_));
            events.push(event.msg);
            if complete {
                return Ok::<_, anyhow::Error>(events);
            }
        }
    })
    .await
    .context("historical child did not complete")??;
    Ok((assignment_id, thread, events))
}

#[cfg(windows)]
async fn historical_acceptance_requires_live_reviewer_and_current_approval_impl() -> Result<()> {
    historical_acceptance_with_approval_route(false).await
}

#[cfg(windows)]
#[test]
fn historical_acceptance_consumes_transition_readiness_authority() -> Result<()> {
    run_session_path_test(
        "historical_acceptance_consumes_transition_readiness_authority",
        historical_acceptance_consumes_transition_readiness_authority_impl,
    )
}

#[cfg(windows)]
async fn historical_acceptance_consumes_transition_readiness_authority_impl() -> Result<()> {
    historical_acceptance_with_approval_route(true).await
}

#[cfg(windows)]
async fn historical_acceptance_with_approval_route(transition_readiness: bool) -> Result<()> {
    let fixture = CurrentEvidenceFixture::new_historical_resolved()?;
    let approval_command = if transition_readiness {
        "just completion-focused inventory.transition-readiness"
    } else {
        &fixture.reconciliation_command
    };
    let home = Arc::new(TempDir::new()?);
    let harness = fixture
        .historical_harness_with_home(Arc::clone(&home))
        .await?;
    mount_sse_sequence(
        harness.server(),
        vec![
            shell_command_call_response("historical-current", &fixture.exact_command),
            shell_command_call_response("historical-reconcile", approval_command),
            required_failure_call_response("stop-after-historical-authority"),
        ],
    )
    .await;
    let authority_events =
        submit_and_collect(harness.test(), "prepare exact historical review authority").await?;
    for call in ["historical-current", "historical-reconcile"] {
        assert_eq!(
            function_call_output_success(&authority_events, call),
            Some(true),
            "{call} failed: {:?}; {}",
            function_call_output(&authority_events, call),
            fixture.driver_diagnostics()
        );
    }
    assert_fresh_current_evidence_markers(&fixture, 1, 1)?;
    let (_, envelope) = read_authenticated_completion_proof_state(home.path(), &fixture.repo_path)?;
    let approval: FocusedReplacementApprovalReceiptV1 = serde_json::from_value(
        envelope
            .pointer("/state/current_evidence_catalog/approval_receipt/receipt")
            .context("missing authenticated historical approval")?
            .clone(),
    )?;
    let proposal_path = ".fixture-state/historical-proposal.json";
    assert_eq!(
        approval.focused_validation_id,
        if transition_readiness {
            "inventory.transition-readiness"
        } else {
            "inventory.frozen-reconciliation"
        }
    );
    let proposal = fixture.write_historical_proposal(proposal_path, &approval, false)?;
    assert_eq!(proposal.scope_reviews.len(), 531);
    assert_eq!(fixture.historical_scope_ids()?.len(), 531);
    let criterion = format!(
        "historical-replacement-review-plan-v1.{}",
        proposal.review_plan_sha256.as_str()
    );
    fixture.write_historical_proposal(".fixture-state/stale-proposal.json", &approval, true)?;
    let mut forged = serde_json::to_value(&proposal)?;
    forged["proposal_sha256"] = json!("0".repeat(64));
    fs::write(
        fixture
            .repo_path
            .join(".fixture-state/forged-proposal.json"),
        canonical_jcs_of(&forged)?,
    )?;
    mount_sse_sequence(
        harness.server(),
        vec![
            historical_agent_call(
                "root-historical-misuse",
                "submit_agent_receipt",
                &historical_receipt_args(&criterion, Some(proposal_path)),
            ),
            required_failure_call_response("stop-root-historical-misuse"),
        ],
    )
    .await;
    let root_events = submit_and_collect(
        harness.test(),
        "try to claim review authority from the root",
    )
    .await?;
    let root_rejection = function_call_output(&root_events, "root-historical-misuse")
        .context("missing root historical rejection")?;
    assert!(
        root_rejection.contains("unsupported call:")
            && root_rejection.contains("submit_agent_receipt"),
        "{root_rejection}"
    );
    let (target, target_thread, target_events) = spawn_historical_fixture_agent(
        &harness,
        "historical_target",
        "explorer",
        "historical-target-complete",
        None,
        false,
        vec![
            (
                "nonreviewer-historical-misuse",
                historical_receipt_args("historical-target-complete", Some(proposal_path)),
            ),
            (
                "target-historical-seal",
                historical_receipt_args("historical-target-complete", None),
            ),
        ],
    )
    .await?;
    assert!(
        function_call_output(&target_events, "nonreviewer-historical-misuse")
            .context("missing nonreviewer rejection")?
            .contains("requires one declared review target")
    );
    let target_seal = function_call_output(&target_events, "target-historical-seal")
        .context("missing successful target receipt")?;
    assert!(target_seal.contains("\"completed\""), "{target_seal}");
    let (_, forged_reviewer, forged_events) = spawn_historical_fixture_agent(
        &harness,
        "historical_imported_reviewer",
        "reviewer",
        &criterion,
        Some(&target),
        true,
        vec![(
            "review-historical-imported-contract",
            historical_receipt_args(&criterion, Some(proposal_path)),
        )],
    )
    .await?;
    let rejected = function_call_output(&forged_events, "review-historical-imported-contract")
        .context("missing imported reviewer contract rejection")?;
    assert!(
        rejected.contains("differs from its fresh typed admission"),
        "{rejected}"
    );
    let (_, unreviewed) =
        read_authenticated_completion_proof_state(home.path(), &fixture.repo_path)?;
    assert!(
        unreviewed
            .pointer("/state/current_evidence_catalog/approval_receipt/reviewed_proposal")
            .is_none()
    );
    forged_reviewer.shutdown_and_wait().await?;
    let (reviewer_id, reviewer, review_events) = spawn_historical_fixture_agent(
        &harness,
        "historical_reviewer",
        "reviewer",
        &criterion,
        Some(&target),
        false,
        vec![
            (
                "review-historical-path",
                historical_receipt_args(&criterion, Some("../outside-proposal.json")),
            ),
            (
                "review-historical-stale",
                historical_receipt_args(&criterion, Some(".fixture-state/stale-proposal.json")),
            ),
            (
                "review-historical-forged",
                historical_receipt_args(&criterion, Some(".fixture-state/forged-proposal.json")),
            ),
            (
                "review-historical-accept",
                historical_receipt_args(&criterion, Some(proposal_path)),
            ),
        ],
    )
    .await?;
    for call in [
        "review-historical-path",
        "review-historical-stale",
        "review-historical-forged",
    ] {
        let output = function_call_output(&review_events, call)
            .with_context(|| format!("missing {call} rejection"))?;
        assert!(
            !output.contains("reviewed_proposal_sha256"),
            "invalid proposal accepted: {output}"
        );
    }
    let accepted = function_call_output(&review_events, "review-historical-accept")
        .with_context(|| format!("missing reviewed proposal result: {review_events:#?}"))?;
    assert!(
        accepted.contains(proposal.proposal_sha256.as_str()),
        "review did not establish authority: {accepted}"
    );
    let (_, reviewed_envelope) =
        read_authenticated_completion_proof_state(home.path(), &fixture.repo_path)?;
    let reviewed = reviewed_envelope
        .pointer("/state/current_evidence_catalog/approval_receipt/reviewed_proposal")
        .context("review was not persisted in authenticated state")?
        .clone();
    assert_eq!(reviewed["reviewer_assignment_id"], json!(reviewer_id));
    assert_eq!(reviewed["target_assignment_id"], json!(target));
    assert_eq!(
        reviewed["proposal"]["proposal_sha256"],
        json!(proposal.proposal_sha256.as_str())
    );
    assert_eq!(
        reviewed["proposal"]["activation_authority"],
        serde_json::Value::Null
    );
    assert!(
        reviewed_envelope
            .pointer("/state/accepted_attempt")
            .is_none_or(serde_json::Value::is_null)
    );
    assert!(!fixture.canonical_marker.exists());
    assert!(
        !fixture
            .repo_path
            .join(".codex/validation/replacement-admissions-v1.json")
            .exists()
    );
    target_thread.shutdown_and_wait().await?;
    reviewer.shutdown_and_wait().await?;
    let rollout = harness
        .test()
        .codex
        .rollout_path()
        .context("missing root rollout")?;
    harness.test().codex.shutdown_and_wait().await?;
    let config = harness.test().config.clone();
    let mut resumed_builder = test_codex()
        .with_raw_response_items()
        .with_extensions(required_tool_failure_extensions())
        .with_config(move |resumed| {
            *resumed = config.clone();
        });
    let resumed = resumed_builder
        .resume(harness.server(), Arc::clone(&home), rollout)
        .await?;
    mount_sse_sequence(
        harness.server(),
        vec![required_failure_call_response("stop-historical-resume")],
    )
    .await;
    submit_and_collect(&resumed, "check retained historical review").await?;
    let (_, resumed_envelope) =
        read_authenticated_completion_proof_state(home.path(), &fixture.repo_path)?;
    assert_eq!(
        resumed_envelope
            .pointer("/state/current_evidence_catalog/approval_receipt/reviewed_proposal"),
        Some(&reviewed)
    );
    fs::write(
        fixture.repo_path.join("src/runtime.rs"),
        "pub const VALUE: u8 = 3;\n",
    )?;
    mount_sse_sequence(
        harness.server(),
        vec![
            terminal_candidate(1, "historical review must be revoked after source mutation"),
            required_failure_call_response("stop-after-historical-revocation"),
        ],
    )
    .await;
    submit_and_collect(&resumed, "observe a source mutation after review").await?;
    let (_, revoked) = read_authenticated_completion_proof_state(home.path(), &fixture.repo_path)?;
    assert!(
        revoked
            .pointer("/state/current_evidence_catalog/approval_receipt/reviewed_proposal")
            .is_none()
    );
    assert!(!fixture.canonical_marker.exists());
    resumed.codex.shutdown_and_wait().await?;
    Ok(())
}

#[cfg(windows)]
#[test]
fn focused_native_subset_executes_only_selected_and_keeps_other_failures() -> Result<()> {
    run_session_path_test(
        "focused_native_subset_executes_only_selected_and_keeps_other_failures",
        || async {
            let fixture = CurrentEvidenceFixture::new()?;
            let justfile = fixture.repo_path.join("justfile");
            let old = fs::read_to_string(&justfile)?.replace("\r\n", "\n");
            let canonical = old
                .split_once("[no-cd]\ncompletion-proof:")
                .context("fixture canonical recipe")?
                .1;
            fs::write(
                &justfile,
                format!(
                    "set positional-arguments\n\n[no-cd]\n[script(\"python\")]\ncompletion-focused validation_id *test_ids:\n    import runpy\n    runpy.run_path(\"runner_driver.py\", run_name=\"__main__\")\n\n[no-cd]\ncompletion-proof:{canonical}"
                ),
            )?;
            let driver = fixture.repo_path.join("runner_driver.py");
            let source = fs::read_to_string(&driver)?;
            anyhow::ensure!(source.contains("\"focused\", sys.argv[1]"));
            fs::write(
                &driver,
                source.replace("\"focused\", sys.argv[1]", "\"focused\", *sys.argv[1:]"),
            )?;
            for path in [
                ".codex/validation/completion-proof.toml",
                ".codex/validation/runner.toml",
            ] {
                let config = fixture.repo_path.join(path);
                let value = fs::read_to_string(&config)?;
                fs::write(
                    config,
                    value.replace(
                        "consumed_paths = [\"scripts/test_runtime_path.py\",",
                        "consumed_paths = [\"scripts/test_runtime_path.py\", \"scripts/price.py\",",
                    ),
                )?;
            }
            run_git(
                &fixture.repo_path,
                &["add", "justfile", "runner_driver.py", ".codex/validation"],
            )?;
            run_git(
                &fixture.repo_path,
                &[
                    "-c",
                    "commit.gpgsign=false",
                    "commit",
                    "--quiet",
                    "-m",
                    "native subset fixture runner",
                ],
            )?;
            let product = fixture.repo_path.join("scripts/price.py");
            fs::write(
                &product,
                "def price(amount): return amount\ndef tax(amount): return amount // 10\n",
            )?;
            fs::write(
                fixture.repo_path.join("scripts/test_runtime_path.py"),
                format!(
                    r#"import pathlib
import unittest
from scripts.price import price, tax
MARKER = pathlib.Path({marker})
class RuntimePathTest(unittest.TestCase):
    def test_price(self):
        with MARKER.open('a', encoding='utf-8') as output: output.write('price\n')
        self.assertEqual(price(100), 95)
    def test_tax(self):
        with MARKER.open('a', encoding='utf-8') as output: output.write('tax\n')
        self.assertEqual(tax(100), 10)
"#,
                    marker = serde_json::to_string(&fixture.unit_marker.to_string_lossy())?
                ),
            )?;
            // Keep the tests unchanged during the task. A subset must not inherit
            // the configured group's coverage of every product input.
            run_git(
                &fixture.repo_path,
                &[
                    "add",
                    "scripts/test_runtime_path.py",
                    "scripts/price.py",
                    "src/runtime.rs",
                ],
            )?;
            run_git(
                &fixture.repo_path,
                &[
                    "-c",
                    "commit.gpgsign=false",
                    "commit",
                    "--quiet",
                    "-m",
                    "existing behavior tests",
                ],
            )?;
            let home = Arc::new(TempDir::new()?);
            let harness = fixture.harness_with_home(Arc::clone(&home)).await?;
            let price_id = "scripts.test_runtime_path.RuntimePathTest.test_price";
            let tax_id = "scripts.test_runtime_path.RuntimePathTest.test_tax";
            for (index, ids, expected) in [
                (0, vec![price_id], "actually ran and failed"),
                (1, vec![tax_id], "omitted previously failed tests"),
                (2, vec![price_id], "actually ran and passed"),
                (
                    3,
                    vec!["scripts.test_runtime_path.RuntimePathTest.missing"],
                    "undiscovered IDs",
                ),
                (4, vec![price_id, price_id], "unique nonempty exact IDs"),
            ] {
                if index == 1 {
                    fs::write(
                        &product,
                        "def price(amount): return amount - 5\ndef tax(amount): return amount // 10\n",
                    )?;
                }
                let call_id = format!("native-subset-{index}");
                let args = std::iter::once("completion-focused")
                    .chain(std::iter::once("maintenance.root-unittest"))
                    .chain(ids)
                    .collect::<Vec<_>>();
                let response = sse(vec![
                    ev_response_created(&call_id),
                    ev_function_call(
                        &call_id,
                        "exec_command",
                        &json!({
                            "kind":"argv", "program":"just", "args":args,
                            "workdir":fixture.repo_path, "yield_time_ms":30000,
                            "tty":false,
                        })
                        .to_string(),
                    ),
                    ev_completed(&call_id),
                ]);
                let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let output = Arc::new(std::sync::Mutex::new(None::<String>));
                let observed_output = Arc::clone(&output);
                let response_call_id = call_id.clone();
                let response_process_id = Arc::new(std::sync::Mutex::new(None::<u32>));
                let mock = wiremock::Mock::given(wiremock::matchers::method("POST"))
                    .and(wiremock::matchers::path_regex(".*/responses"))
                    .respond_with(move |request: &wiremock::Request| {
                        let request_index = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        if request_index == 0 {
                            return sse_response(response.clone());
                        }
                        assert!(
                            request_index < 12,
                            "bounded native subset process did not finish"
                        );
                        let previous = if request_index == 1 {
                            response_call_id.clone()
                        } else {
                            format!("{response_call_id}-poll-{}", request_index - 1)
                        };
                        let body: serde_json::Value =
                            serde_json::from_slice(&request.body).expect("native request");
                        let value = body["input"]
                            .as_array()
                            .expect("request input")
                            .iter()
                            .find(|item| {
                                item["type"] == "function_call_output"
                                    && item["call_id"] == previous
                            })
                            .expect("previous native tool result");
                        let text = value["output"].as_str().expect("native tool output text");
                        if let Some((_, tail)) = text.split_once("Process running with session ID ")
                        {
                            let id = tail
                                .split(|c: char| !c.is_ascii_digit())
                                .next()
                                .unwrap()
                                .parse::<u32>()
                                .expect("yielded process ID");
                            let mut process_id = response_process_id.lock().unwrap();
                            assert_eq!(
                                *process_id.get_or_insert(id),
                                id,
                                "poll the same yielded process"
                            );
                            return sse_response(write_stdin_call_response(
                                &format!("{response_call_id}-poll-{request_index}"),
                                id,
                            ));
                        }
                        *observed_output.lock().unwrap() = Some(text.to_owned());
                        sse_response(required_failure_call_response("stop-subset"))
                    })
                    .mount_as_scoped(harness.server())
                    .await;
                submit_and_collect(
                    harness.test(),
                    "Run only the explicitly selected native tests; preserve every other failure.",
                )
                .await?;
                drop(mock);
                let output = output
                    .lock()
                    .unwrap()
                    .clone()
                    .context("terminal native subset result")?;
                assert!(output.contains(expected), "{index}: {output}");
                assert!(!fixture.canonical_marker.exists());
                assert!(!fixture.pytest_marker.exists());
            }
            assert_eq!(
                fs::read_to_string(&fixture.unit_marker)?
                    .lines()
                    .collect::<Vec<_>>(),
                vec!["price", "tax", "price"]
            );
            let (_, state) =
                read_authenticated_completion_proof_state(home.path(), &fixture.repo_path)?;
            let pass = state
                .pointer("/state/focused_completion/passes/maintenance.root-unittest")
                .context("retained subset pass")?;
            assert_eq!(pass["report"]["intended_ids"], json!([price_id]));
            assert_eq!(pass["report"]["executed_ids"], json!([price_id]));
            assert_eq!(
                state.pointer(
                    "/state/poisoned_validations/maintenance.root-unittest/failed_test_ids"
                ),
                Some(&json!([price_id]))
            );
            mount_sse_sequence(
                harness.server(),
                repeated_terminal_candidates(
                    MAX_REGULAR_LOGICAL_GENERATIONS,
                    "The whole product change is validated by this subset.",
                ),
            )
            .await;
            let events = submit_and_collect(
                harness.test(),
                "Try ordinary completion using only the subset execution.",
            )
            .await?;
            assert!(
                events.iter().all(|event| !is_assistant_output(event)),
                "a subset cannot inherit whole-group behavioral coverage"
            );
            harness.test().codex.shutdown_and_wait().await?;
            Ok(())
        },
    )
}

#[cfg(windows)]
#[test]
fn transition_readiness_requires_current_catalog() -> Result<()> {
    run_session_path_test(
        "transition_readiness_requires_current_catalog",
        transition_readiness_requires_current_catalog_impl,
    )
}

#[cfg(windows)]
async fn transition_readiness_requires_current_catalog_impl() -> Result<()> {
    // Keep the unresolved baseline: a resolved fixture would conceal the
    // circular prerequisite this route exists to remove.
    let fixture = CurrentEvidenceFixture::new()?;
    let home = Arc::new(TempDir::new()?);
    let harness = fixture.harness_with_home(Arc::clone(&home)).await?;
    let command = "just completion-focused inventory.transition-readiness";
    let ledger_path = fixture
        .repo_path
        .join(".codex/validation/test-replacements-v1.json");
    let ledger_before = fs::read(&ledger_path)?;

    mount_sse_sequence(
        harness.server(),
        vec![
            shell_command_call_response("missing-catalog", command),
            required_failure_call_response("stop-missing-catalog"),
        ],
    )
    .await;
    let events = submit_and_collect(
        harness.test(),
        "check transition readiness before collecting evidence",
    )
    .await?;
    assert_eq!(
        function_call_output_success(&events, "missing-catalog"),
        Some(false)
    );
    assert!(
        function_call_output(&events, "missing-catalog")
            .context("missing prerequisite output")?
            .contains("current authenticated inventory catalog")
    );
    assert_fresh_current_evidence_markers(&fixture, 0, 0)?;

    mount_sse_sequence(
        harness.server(),
        vec![
            shell_command_call_response("collect-catalog", &fixture.exact_command),
            required_failure_call_response("stop-collect-catalog"),
        ],
    )
    .await;
    let events = submit_and_collect(harness.test(), "collect current evidence").await?;
    assert_eq!(
        function_call_output_success(&events, "collect-catalog"),
        Some(true)
    );

    mount_sse_sequence(
        harness.server(),
        vec![
            shell_command_call_response("transition-check", command),
            required_failure_call_response("stop-transition-check"),
        ],
    )
    .await;
    let events = submit_and_collect(
        harness.test(),
        "check transition readiness while retaining unresolved rows",
    )
    .await?;
    let output =
        function_call_output(&events, "transition-check").context("missing readiness output")?;
    assert_eq!(
        function_call_output_success(&events, "transition-check"),
        Some(true),
        "{output}"
    );
    assert!(
        output.contains("unresolved identities still block final certification"),
        "{output}"
    );
    let (_, envelope) = read_authenticated_completion_proof_state(home.path(), &fixture.repo_path)?;
    let receipt: FocusedReplacementApprovalReceiptV1 = serde_json::from_value(
        envelope
            .pointer("/state/current_evidence_catalog/approval_receipt/receipt")
            .context("no authenticated transition receipt")?
            .clone(),
    )?;
    receipt.validate()?;
    assert_eq!(
        receipt.focused_validation_id,
        "inventory.transition-readiness"
    );
    assert!(
        envelope
            .pointer("/state/current_evidence_catalog/approval_receipt/reviewed_proposal")
            .is_none()
    );
    assert_eq!(fs::read(&ledger_path)?, ledger_before);
    assert_fresh_current_evidence_markers(&fixture, 1, 1)?;
    assert!(!fixture.canonical_marker.exists());

    // Full reconciliation must still reject the same untouched ledger, and
    // must revoke the earlier preparatory receipt when starting a new check.
    mount_sse_sequence(
        harness.server(),
        vec![
            shell_command_call_response("full-reconciliation", &fixture.reconciliation_command),
            required_failure_call_response("stop-full-reconciliation"),
        ],
    )
    .await;
    let events = submit_and_collect(
        harness.test(),
        "attempt full reconciliation of the still-unresolved ledger",
    )
    .await?;
    assert_eq!(
        function_call_output_success(&events, "full-reconciliation"),
        Some(false)
    );
    assert!(
        function_call_output(&events, "full-reconciliation")
            .context("missing unresolved rejection")?
            .contains("unresolved")
    );
    let (_, envelope) = read_authenticated_completion_proof_state(home.path(), &fixture.repo_path)?;
    assert!(
        envelope
            .pointer("/state/current_evidence_catalog/approval_receipt")
            .is_none()
    );
    assert_eq!(fs::read(&ledger_path)?, ledger_before);
    assert_fresh_current_evidence_markers(&fixture, 1, 1)?;
    assert!(!fixture.canonical_marker.exists());
    harness.test().codex.shutdown_and_wait().await?;
    Ok(())
}

#[cfg(windows)]
#[test]
fn focused_replacement_approval_requires_current_catalog_and_real_reconciliation() -> Result<()> {
    run_session_path_test(
        "focused_replacement_approval_requires_current_catalog_and_real_reconciliation",
        focused_replacement_approval_requires_current_catalog_and_real_reconciliation_impl,
    )
}

#[cfg(windows)]
async fn focused_replacement_approval_requires_current_catalog_and_real_reconciliation_impl()
-> Result<()> {
    const FORGED_TERMINAL: &str =
        "a caller-authored focused replacement receipt must not publish terminal output";

    let fixture = CurrentEvidenceFixture::new_resolved()?;
    let fresh_home = Arc::new(TempDir::new()?);
    let harness = fixture.harness_with_home(Arc::clone(&fresh_home)).await?;

    let missing_catalog_call = "reconciliation-without-current-catalog";
    mount_sse_sequence(
        harness.server(),
        vec![
            shell_command_call_response(missing_catalog_call, &fixture.reconciliation_command),
            required_failure_call_response("stop-after-missing-current-catalog"),
        ],
    )
    .await;
    let missing_catalog_events = submit_and_collect(
        harness.test(),
        "try the exact frozen reconciliation before collecting current evidence",
    )
    .await?;
    assert_eq!(
        function_call_output_success(&missing_catalog_events, missing_catalog_call),
        Some(false)
    );
    let missing_catalog_output =
        function_call_output(&missing_catalog_events, missing_catalog_call)
            .context("missing focused reconciliation prerequisite rejection")?;
    assert!(
        missing_catalog_output.contains("current authenticated inventory catalog"),
        "missing current catalog had the wrong rejection: {missing_catalog_output}"
    );
    assert_fresh_current_evidence_markers(&fixture, 0, 0)?;
    assert!(!fixture.canonical_marker.exists());

    let current_evidence_call = "current-evidence-before-real-reconciliation";
    mount_sse_sequence(
        harness.server(),
        vec![
            shell_command_call_response(current_evidence_call, &fixture.exact_command),
            required_failure_call_response("stop-after-current-catalog"),
        ],
    )
    .await;
    let current_evidence_events = submit_and_collect(
        harness.test(),
        "collect the exact current catalog before frozen reconciliation",
    )
    .await?;
    assert_eq!(
        function_call_output_success(&current_evidence_events, current_evidence_call),
        Some(true)
    );
    assert_fresh_current_evidence_markers(&fixture, 1, 1)?;
    let (_, catalog_only_envelope) =
        read_authenticated_completion_proof_state(fresh_home.path(), &fixture.repo_path)?;
    let catalog_only = catalog_only_envelope
        .pointer("/state/current_evidence_catalog")
        .filter(|value| !value.is_null())
        .context("real current evidence did not persist its authenticated catalog")?;
    assert!(
        catalog_only.get("approval_receipt").is_none(),
        "current evidence issued replacement approval before reconciliation"
    );

    let reconciliation_call = "real-focused-frozen-reconciliation";
    mount_sse_sequence(
        harness.server(),
        vec![
            shell_command_call_response(reconciliation_call, &fixture.reconciliation_command),
            terminal_candidate(
                1,
                "focused replacement approval is still not terminal certification",
            ),
            required_failure_call_response("stop-after-real-focused-reconciliation"),
        ],
    )
    .await;
    let reconciliation_events = submit_and_collect(
        harness.test(),
        "run the real frozen reconciliation worker and then try to finish",
    )
    .await?;
    assert_eq!(
        function_call_output_success(&reconciliation_events, reconciliation_call),
        Some(true)
    );
    let reconciliation_output = function_call_output(&reconciliation_events, reconciliation_call)
        .context("missing accepted focused reconciliation output")?;
    assert!(
        reconciliation_output.contains(
            "Focused frozen-inventory reconciliation was authenticated and retained for replacement review"
        ),
        "focused reconciliation did not return its distinct accepted result: {reconciliation_output}"
    );
    assert!(
        reconciliation_events
            .iter()
            .all(|event| !assistant_output_contains(
                event,
                "focused replacement approval is still not terminal certification"
            )),
        "focused replacement approval was mistaken for terminal certification"
    );
    assert_fresh_current_evidence_markers(&fixture, 1, 1)?;
    assert!(!fixture.canonical_marker.exists());

    let (state_path, approved_envelope) =
        read_authenticated_completion_proof_state(fresh_home.path(), &fixture.repo_path)?;
    let approved_catalog = approved_envelope
        .pointer("/state/current_evidence_catalog")
        .filter(|value| !value.is_null())
        .context("focused reconciliation discarded the authenticated current catalog")?;
    let approval = approved_catalog
        .get("approval_receipt")
        .filter(|value| value.is_object())
        .context("focused reconciliation did not persist its approval wrapper")?;
    let receipt = approval
        .get("receipt")
        .filter(|value| value.is_object())
        .context("focused reconciliation approval wrapper omitted its typed receipt")?;
    assert_eq!(
        receipt["format_id"],
        json!("kd4.focused-replacement-approval-receipt.v1")
    );
    assert_eq!(receipt["schema_version"], json!(1));
    assert_eq!(
        receipt["focused_validation_id"],
        json!("inventory.frozen-reconciliation")
    );
    assert_eq!(receipt["classification"], json!("confirmed-pass"));
    let receipt_sha256 = receipt["receipt_sha256"]
        .as_str()
        .context("typed focused reconciliation receipt omitted its SHA-256")?
        .to_string();
    assert!(
        reconciliation_output.contains(&format!("receipt {receipt_sha256}")),
        "accepted model text did not name the persisted receipt: {reconciliation_output}"
    );
    assert_eq!(approval["attempt_id"], receipt["attempt_id"]);
    assert_eq!(
        approval["exact_command"],
        json!(fixture.reconciliation_command)
    );
    assert_eq!(approval["policy_id"], receipt["policy_id"]);
    assert!(
        approval["runner_process_id"]
            .as_u64()
            .is_some_and(|id| id > 0)
    );
    for field in ["runner_executable_path", "runner_entrypoint_path"] {
        let path = approval[field]
            .as_str()
            .with_context(|| format!("approval wrapper omitted {field}"))?;
        assert!(
            Path::new(path).is_absolute(),
            "{field} was not absolute: {path}"
        );
    }
    assert_eq!(
        approval["session_lineage_id"],
        approved_catalog["session_lineage_id"]
    );
    assert!(
        approval["recorded_at_unix_ms"]
            .as_u64()
            .is_some_and(|timestamp| timestamp > 0)
    );
    assert_ne!(approval["attempt_id"], approved_catalog["attempt_id"]);
    assert_eq!(
        receipt["frozen_inventory_hash"],
        approved_catalog["catalog"]["frozen_inventory_hash"]
    );
    assert_eq!(
        receipt["focused_inventory_catalog_semantic_sha256"],
        approved_catalog["catalog"]["semantic_sha256"]
    );
    assert_eq!(
        receipt["inventory_discovery_processes_sha256"],
        approved_catalog["catalog"]["inventory_discovery_processes_sha256"]
    );
    assert_eq!(
        receipt["policy_runner_bundle_sha256"],
        approved_catalog["policy_runner_bundle_sha256"]
    );
    assert_eq!(
        receipt["workspace_fingerprint"],
        approved_catalog["workspace_fingerprint"]
    );
    assert_eq!(
        receipt["mutation_epoch"],
        approved_catalog["mutation_epoch"]
    );
    let approved_catalog = approved_catalog.clone();

    let rollout = harness
        .test()
        .codex
        .rollout_path()
        .context("missing rollout path for same-home approval resume")?;
    harness.test().codex.shutdown_and_wait().await?;

    let cwd = fixture.repo_path.abs();
    let outside = fixture.outside_path.abs();
    let fake_bin = fixture.repo_path.join(".fixture-bin");
    let sandbox_temp = fixture.repo_path.join(".fixture-state/sandbox-temp");
    let path = format!(
        "{};{}",
        fake_bin.to_string_lossy(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut resume_builder = test_codex()
        .with_raw_response_items()
        .with_extensions(required_tool_failure_extensions())
        .with_config(move |config| {
            config.cwd = cwd.clone();
            let roots = vec![cwd.clone(), outside.clone()];
            config.workspace_roots = roots.clone();
            config.permissions.set_workspace_roots(roots);
            for (name, value) in [
                ("PATH", path.clone()),
                ("TEMP", sandbox_temp.to_string_lossy().into_owned()),
                ("TMP", sandbox_temp.to_string_lossy().into_owned()),
            ] {
                config
                    .permissions
                    .shell_environment_policy
                    .r#set
                    .insert(name.to_string(), value);
            }
        });
    mount_sse_once_match(
        harness.server(),
        |request: &wiremock::Request| {
            response_request_contains(request, "verify the persisted approval after resume")
        },
        required_failure_call_response("stop-after-same-home-approval-resume"),
    )
    .await;
    let resumed = resume_builder
        .resume(harness.server(), Arc::clone(&fresh_home), rollout)
        .await?;
    submit_and_collect(&resumed, "verify the persisted approval after resume").await?;
    let (_, resumed_envelope) =
        read_authenticated_completion_proof_state(fresh_home.path(), &fixture.repo_path)?;
    assert_eq!(
        resumed_envelope.pointer("/state/current_evidence_catalog"),
        Some(&approved_catalog),
        "same-home rollout resume did not retain the authenticated approval"
    );

    fs::write(
        fixture.repo_path.join("README.md"),
        "updated fixture documentation\n",
    )?;
    mount_sse_sequence(
        harness.server(),
        vec![
            terminal_candidate(2, "documentation drift must revoke focused approval"),
            required_failure_call_response("stop-after-documentation-approval-revocation"),
        ],
    )
    .await;
    let documentation_events = submit_and_collect(
        &resumed,
        "observe the documentation-only mutation and try to reuse the focused approval",
    )
    .await?;
    assert!(
        documentation_events.iter().all(|event| {
            !assistant_output_contains(event, "documentation drift must revoke focused approval")
        }),
        "documentation-only mutation did not revoke terminal use of focused approval"
    );
    let (_, documentation_envelope) =
        read_authenticated_completion_proof_state(fresh_home.path(), &fixture.repo_path)?;
    let documentation_catalog = documentation_envelope
        .pointer("/state/current_evidence_catalog")
        .filter(|value| value.is_object())
        .context("documentation-only mutation discarded the authenticated current catalog")?;
    let mut expected_documentation_catalog = approved_catalog.clone();
    let removed_approval = expected_documentation_catalog
        .as_object_mut()
        .context("approved current catalog was not an object")?
        .remove("approval_receipt");
    anyhow::ensure!(
        removed_approval.is_some(),
        "approved current catalog did not contain the receipt being revoked"
    );
    assert_eq!(
        documentation_catalog, &expected_documentation_catalog,
        "documentation-only mutation changed current evidence beyond revoking approval"
    );
    resumed.codex.shutdown_and_wait().await?;

    let reloaded_harness = fixture.harness_with_home(Arc::clone(&fresh_home)).await?;
    let (_, reloaded_envelope) =
        read_authenticated_completion_proof_state(fresh_home.path(), &fixture.repo_path)?;
    assert_eq!(
        reloaded_envelope.pointer("/state/current_evidence_catalog"),
        Some(&expected_documentation_catalog),
        "authenticated catalog with a revoked approval did not survive state reload"
    );

    fs::write(
        fixture.repo_path.join("src/runtime.rs"),
        "pub const VALUE: u8 = 3;\n",
    )?;
    mount_sse_sequence(
        reloaded_harness.server(),
        vec![
            terminal_candidate(3, "stale focused approval must remain private"),
            required_failure_call_response("stop-after-approval-revocation"),
        ],
    )
    .await;
    let mutation_events = submit_and_collect(
        reloaded_harness.test(),
        "observe the workspace mutation and try to reuse the focused approval",
    )
    .await?;
    assert!(
        mutation_events.iter().all(|event| {
            !assistant_output_contains(event, "stale focused approval must remain private")
        }),
        "workspace mutation did not revoke terminal use of stale evidence"
    );
    let (_, cleared_envelope) =
        read_authenticated_completion_proof_state(fresh_home.path(), &fixture.repo_path)?;
    assert!(
        cleared_envelope
            .pointer("/state/current_evidence_catalog")
            .is_none_or(serde_json::Value::is_null),
        "workspace mutation did not revoke both the catalog and its approval receipt"
    );
    reloaded_harness.test().codex.shutdown_and_wait().await?;

    let current_fingerprint = cleared_envelope["state"]["last_observed_fingerprint"]
        .as_str()
        .context("cleared state omitted its current workspace fingerprint")?
        .to_string();
    let current_mutation_epoch = cleared_envelope["state"]["mutation_epoch"]
        .as_u64()
        .context("cleared state omitted its current mutation epoch")?;
    let current_fingerprint_hash = Sha256HexV1::parse(current_fingerprint.clone())?;
    let mut forged_catalog = approved_catalog;
    let mut typed_catalog: FocusedLiveSuccessorCatalogV1 =
        serde_json::from_value(forged_catalog["catalog"].clone())?;
    typed_catalog.start_fingerprint = current_fingerprint_hash.clone();
    typed_catalog.start_mutation_epoch = current_mutation_epoch;
    typed_catalog.semantic_sha256 = typed_catalog.semantic_sha256()?;
    typed_catalog.validate()?;
    forged_catalog["catalog_sha256"] = json!(format!(
        "{:x}",
        Sha256::digest(canonical_jcs_of(&typed_catalog)?)
    ));
    forged_catalog["workspace_fingerprint"] = json!(current_fingerprint);
    forged_catalog["mutation_epoch"] = json!(current_mutation_epoch);
    forged_catalog["catalog"] = serde_json::to_value(&typed_catalog)?;

    let mut typed_receipt: FocusedReplacementApprovalReceiptV1 =
        serde_json::from_value(forged_catalog["approval_receipt"]["receipt"].clone())?;
    typed_receipt.focused_inventory_catalog_semantic_sha256 = typed_catalog.semantic_sha256.clone();
    typed_receipt.workspace_fingerprint = current_fingerprint_hash;
    typed_receipt.mutation_epoch = current_mutation_epoch;
    typed_receipt.receipt_sha256 = typed_receipt.receipt_sha256()?;
    typed_receipt.validate()?;
    let caller_context = FocusedReplacementApprovalCurrentContextV1 {
        format_id: typed_receipt.format_id.clone(),
        schema_version: typed_receipt.schema_version,
        attempt_id: typed_receipt.attempt_id.clone(),
        focused_validation_id: typed_receipt.focused_validation_id.clone(),
        classification: typed_receipt.classification.clone(),
        frozen_inventory_hash: typed_receipt.frozen_inventory_hash.clone(),
        focused_inventory_catalog_semantic_sha256: typed_receipt
            .focused_inventory_catalog_semantic_sha256
            .clone(),
        inventory_discovery_processes_sha256: typed_receipt
            .inventory_discovery_processes_sha256
            .clone(),
        policy_id: typed_receipt.policy_id.clone(),
        policy_runner_bundle_sha256: typed_receipt.policy_runner_bundle_sha256.clone(),
        workspace_fingerprint: typed_receipt.workspace_fingerprint.clone(),
        mutation_epoch: typed_receipt.mutation_epoch,
    };
    typed_receipt.validate_current_context(&caller_context)?;
    forged_catalog["approval_receipt"]["receipt"] = serde_json::to_value(typed_receipt)?;

    let mut forged_envelope = cleared_envelope;
    forged_envelope["state"]["current_evidence_catalog"] = forged_catalog;
    fs::write(&state_path, serde_json::to_vec_pretty(&forged_envelope)?)?;

    let forged_harness = fixture.harness_with_home(Arc::clone(&fresh_home)).await?;
    let forged_reconciliation_call = "reconciliation-with-caller-authored-old-receipt";
    mount_sse_sequence(
        forged_harness.server(),
        vec![
            shell_command_call_response(
                forged_reconciliation_call,
                &fixture.reconciliation_command,
            ),
            terminal_candidate(4, FORGED_TERMINAL),
            required_failure_call_response("stop-after-caller-authored-old-receipt"),
        ],
    )
    .await;
    let forged_events = submit_and_collect(
        forged_harness.test(),
        "try to reuse a caller-authored copy of the old focused approval",
    )
    .await?;
    assert_eq!(
        function_call_output_success(&forged_events, forged_reconciliation_call),
        Some(false)
    );
    let forged_output = function_call_output(&forged_events, forged_reconciliation_call)
        .context("missing caller-authored receipt rejection")?;
    assert!(
        forged_output.contains(
            "completion-proof state authentication failed; repository or tool rewriting cannot establish proof"
        ),
        "caller-authored old receipt had the wrong rejection: {forged_output}"
    );
    assert!(
        forged_events
            .iter()
            .all(|event| !assistant_output_contains(event, FORGED_TERMINAL)),
        "caller-authored old receipt minted terminal authority"
    );
    assert_fresh_current_evidence_markers(&fixture, 1, 1)?;
    assert!(!fixture.canonical_marker.exists());

    Ok(())
}

#[cfg(windows)]
fn assert_fresh_current_evidence_markers(
    fixture: &CurrentEvidenceFixture,
    unittest_count: usize,
    pytest_count: usize,
) -> Result<()> {
    for (path, expected) in [
        (&fixture.unit_marker, unittest_count),
        (&fixture.pytest_marker, pytest_count),
    ] {
        let lines = if path.exists() {
            fs::read_to_string(path)?
                .lines()
                .map(str::to_string)
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        assert_eq!(lines.len(), expected, "unexpected marker count at {path:?}");
        assert_eq!(
            lines
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            expected,
            "current-evidence wrapper reused a prior execution marker at {path:?}"
        );
        for line in lines {
            uuid::Uuid::parse_str(&line)
                .with_context(|| format!("invalid execution UUID in {path:?}"))?;
        }
    }
    Ok(())
}

async fn successful_focused_validation_does_not_satisfy_terminal_gate_impl() -> Result<()> {
    let fixture = CompletionProofFixture::with_transient_canonical_pre_result()?;
    let harness = fixture.harness().await?;
    let call_id = "trusted-focused-pass";
    let mut responses = vec![
        exec_command_call_response(
            "explicit-canonical-request",
            &fixture.canonical_command,
            &fixture.repo_path,
        ),
        exec_command_call_response(
            call_id,
            &fixture.exact_focused_command(),
            &fixture.repo_path,
        ),
    ];
    responses.extend(repeated_terminal_candidates(
        MAX_REGULAR_LOGICAL_GENERATIONS - 1,
        "a focused pass is not whole-repository certification",
    ));
    mount_sse_sequence(harness.server(), responses).await;

    let events =
        submit_and_collect(harness.test(), "run the focused validation, then finish").await?;

    assert!(events.iter().any(|event| {
        matches!(
            event,
            EventMsg::ExecCommandEnd(end) if end.call_id == call_id && end.exit_code == 0
        )
    }));
    assert!(events.iter().all(|event| !is_assistant_output(event)));
    let completion = events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing TurnComplete")?;
    assert!(completion.error.is_some());
    assert!(completion.last_agent_message.is_none());
    assert!(
        fs::read_to_string(&fixture.marker_path)?.lines().count() == 1,
        "focused evidence caused an additional canonical launch"
    );

    Ok(())
}

#[test]
fn focused_completion_accepts_current_scope_without_certification() -> Result<()> {
    run_session_path_test(
        "focused_completion_accepts_current_scope_without_certification",
        focused_completion_accepts_current_scope_without_certification_impl,
    )
}

async fn focused_completion_accepts_current_scope_without_certification_impl() -> Result<()> {
    let fixture = CompletionProofFixture::new()?;
    let harness = fixture.harness().await?;
    mount_sse_sequence(
        harness.server(),
        vec![
            exec_command_call_response(
                "focused-task-check",
                &fixture.exact_focused_command(),
                &fixture.repo_path,
            ),
            terminal_candidate(1, "the requested scoped change is complete"),
        ],
    )
    .await;
    let events = submit_and_collect(harness.test(), "validate this change and finish").await?;
    let completion = events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(value) => Some(value),
            _ => None,
        })
        .context("missing ordinary task completion")?;
    assert!(
        completion.error.is_none(),
        "focused completion rejected: {completion:#?}"
    );
    assert_eq!(
        completion.last_agent_message.as_deref(),
        Some("the requested scoped change is complete")
    );
    assert!(
        !fixture.marker_path.exists(),
        "ordinary completion launched certification"
    );
    assert_eq!(harness.request_bodies().await.len(), 2);
    Ok(())
}

#[test]
fn focused_completion_rejects_relevant_change_without_rerunning_tests() -> Result<()> {
    run_session_path_test(
        "focused_completion_rejects_relevant_change_without_rerunning_tests",
        focused_completion_rejects_relevant_change_without_rerunning_tests_impl,
    )
}

async fn focused_completion_rejects_relevant_change_without_rerunning_tests_impl() -> Result<()> {
    let fixture = CompletionProofFixture::new()?;
    let harness = fixture.harness().await?;
    let mut responses = vec![
        exec_command_call_response(
            "scoped-pass-before-edit",
            &fixture.exact_focused_command(),
            &fixture.repo_path,
        ),
        exec_command_call_response(
            "edit-scoped-input",
            &fixture.mutation_command,
            &fixture.repo_path,
        ),
    ];
    responses.extend(repeated_terminal_candidates(
        MAX_REGULAR_LOGICAL_GENERATIONS - 1,
        "stale focused evidence is insufficient",
    ));
    mount_sse_sequence(harness.server(), responses).await;
    let events =
        submit_and_collect(harness.test(), "check, edit the behavior, then finish").await?;
    let completion = events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(value) => Some(value),
            _ => None,
        })
        .context("missing completion after relevant edit")?;
    assert!(completion.error.is_some());
    assert!(completion.last_agent_message.is_none());
    assert!(
        !fixture.marker_path.exists(),
        "staleness launched certification"
    );
    assert_eq!(events.iter().filter(|event| matches!(event, EventMsg::ExecCommandEnd(end) if end.call_id == "scoped-pass-before-edit")).count(), 1);
    Ok(())
}

#[test]
fn failed_focused_validation_does_not_launch_canonical_certification() -> Result<()> {
    run_session_path_test(
        "failed_focused_validation_does_not_launch_canonical_certification",
        failed_focused_validation_does_not_launch_canonical_certification_impl,
    )
}

async fn failed_focused_validation_does_not_launch_canonical_certification_impl() -> Result<()> {
    let fixture = CompletionProofFixture::with_failed_focused_validation()?;
    let harness = fixture.harness().await?;
    let call_id = "trusted-focused-failure";
    let mut responses = vec![exec_command_call_response(
        call_id,
        &fixture.exact_focused_command(),
        &fixture.repo_path,
    )];
    responses.extend(repeated_terminal_candidates(
        MAX_REGULAR_LOGICAL_GENERATIONS,
        "a focused failure must not launch certification",
    ));
    mount_sse_sequence(harness.server(), responses).await;

    let events = submit_and_collect(harness.test(), "run the failing focused validation").await?;

    assert!(
        events.iter().any(|event| {
            matches!(
                event,
                EventMsg::ExecCommandEnd(end) if end.call_id == call_id && end.exit_code != 0
            )
        }),
        "focused failure did not surface as a failed command event: {events:#?}"
    );
    assert!(events.iter().all(|event| !is_assistant_output(event)));
    let completion = events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing TurnComplete")?;
    assert!(completion.error.is_some());
    assert!(completion.last_agent_message.is_none());
    assert!(
        !fixture.marker_path.exists(),
        "a focused failure caused the canonical command to launch"
    );

    Ok(())
}

#[test]
fn identical_rewrite_does_not_clear_poison_through_real_shell_path() -> Result<()> {
    run_session_path_test(
        "identical_rewrite_does_not_clear_poison_through_real_shell_path",
        identical_rewrite_does_not_clear_poison_through_real_shell_path_impl,
    )
}

async fn identical_rewrite_does_not_clear_poison_through_real_shell_path_impl() -> Result<()> {
    let fixture = CompletionProofFixture::with_failed_focused_validation()?;
    let harness = fixture.harness().await?;
    let focused_call_id = "poisoning-focused-failure";
    let no_op_call_id = "rewrite-identical-product-input";
    let canonical_call_id = "canonical-after-identical-rewrite";
    let mut responses = vec![
        exec_command_call_response(
            focused_call_id,
            &fixture.exact_focused_command(),
            &fixture.repo_path,
        ),
        exec_command_call_response(no_op_call_id, &fixture.no_op_command, &fixture.repo_path),
        exec_command_call_response(
            canonical_call_id,
            &fixture.canonical_command,
            &fixture.repo_path,
        ),
    ];
    responses.extend(repeated_terminal_candidates(
        MAX_REGULAR_LOGICAL_GENERATIONS - 2,
        "an identical rewrite must not clear poison",
    ));
    mount_sse_sequence(harness.server(), responses).await;

    let events = submit_and_collect(
        harness.test(),
        "run the failing validation, rewrite its input identically, certify, and finish",
    )
    .await?;

    assert!(events.iter().any(|event| {
        matches!(
            event,
            EventMsg::ExecCommandEnd(end)
                if end.call_id == focused_call_id && end.exit_code != 0
        )
    }));
    assert!(events.iter().any(|event| {
        matches!(
            event,
            EventMsg::ExecCommandEnd(end)
                if end.call_id == no_op_call_id && end.exit_code == 0
        )
    }));
    assert!(events.iter().any(|event| {
        matches!(
            event,
            EventMsg::ExecCommandEnd(end)
                if end.call_id == canonical_call_id && end.exit_code == 0
        )
    }));
    assert_eq!(
        fs::read_to_string(fixture.repo_path.join("src/runtime.rs"))?.trim(),
        "pub const VALUE: u8 = 2;"
    );
    assert_eq!(fixture.canonical_launch_count()?, 1);
    assert!(events.iter().all(|event| !is_assistant_output(event)));
    let completion = events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing TurnComplete")?;
    assert!(completion.error.is_some());
    assert!(completion.last_agent_message.is_none());
    let request_bodies = harness.request_bodies().await;
    let function_outputs = request_bodies
        .iter()
        .flat_map(|body| body["input"].as_array().into_iter().flatten())
        .filter(|item| item["type"] == "function_call_output")
        .map(|item| item["output"].clone())
        .collect::<Vec<_>>();
    assert!(
        request_bodies.iter().any(|body| {
            body.to_string()
                .contains("is poisoned at this mutation epoch")
        }),
        "the canonical retry did not retain the focused failure poison: {function_outputs:#?}"
    );

    Ok(())
}

#[test]
fn committing_already_dirty_bytes_does_not_clear_poison_through_real_shell_path() -> Result<()> {
    run_session_path_test(
        "committing_already_dirty_bytes_does_not_clear_poison_through_real_shell_path",
        committing_already_dirty_bytes_does_not_clear_poison_through_real_shell_path_impl,
    )
}

async fn committing_already_dirty_bytes_does_not_clear_poison_through_real_shell_path_impl()
-> Result<()> {
    let fixture = CompletionProofFixture::with_failed_focused_validation()?;
    let harness = fixture.harness().await?;
    let focused_call_id = "focused-failure-before-same-bytes-commit";
    let commit_call_id = "commit-already-dirty-product-bytes";
    let canonical_call_id = "canonical-after-same-bytes-commit";
    let mut responses = vec![
        exec_command_call_response(
            focused_call_id,
            &fixture.exact_focused_command(),
            &fixture.repo_path,
        ),
        exec_command_call_response(
            commit_call_id,
            &fixture.commit_existing_product_command,
            &fixture.repo_path,
        ),
        exec_command_call_response(
            canonical_call_id,
            &fixture.canonical_command,
            &fixture.repo_path,
        ),
    ];
    responses.extend(repeated_terminal_candidates(
        MAX_REGULAR_LOGICAL_GENERATIONS - 2,
        "committing the same failed bytes must not clear poison",
    ));
    mount_sse_sequence(harness.server(), responses).await;

    let events = submit_and_collect(
        harness.test(),
        "fail validation, commit the already-dirty bytes unchanged, certify, and finish",
    )
    .await?;

    assert!(events.iter().any(|event| {
        matches!(
            event,
            EventMsg::ExecCommandEnd(end)
                if end.call_id == focused_call_id && end.exit_code != 0
        )
    }));
    assert!(events.iter().any(|event| {
        matches!(
            event,
            EventMsg::ExecCommandEnd(end)
                if end.call_id == commit_call_id && end.exit_code == 0
        )
    }));
    assert!(events.iter().any(|event| {
        matches!(
            event,
            EventMsg::ExecCommandEnd(end)
                if end.call_id == canonical_call_id && end.exit_code == 0
        )
    }));
    assert!(fixture.worktree_is_clean()?);
    assert_eq!(
        fs::read_to_string(fixture.repo_path.join("src/runtime.rs"))?.trim(),
        "pub const VALUE: u8 = 2;"
    );
    assert_eq!(fixture.canonical_launch_count()?, 1);
    assert!(events.iter().all(|event| !is_assistant_output(event)));
    let completion = events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing TurnComplete")?;
    assert!(completion.error.is_some());
    assert!(completion.last_agent_message.is_none());
    let request_bodies = harness.request_bodies().await;
    let function_outputs = request_bodies
        .iter()
        .flat_map(|body| body["input"].as_array().into_iter().flatten())
        .filter(|item| item["type"] == "function_call_output")
        .map(|item| item["output"].clone())
        .collect::<Vec<_>>();
    assert!(
        request_bodies.iter().any(|body| {
            body.to_string()
                .contains("is poisoned at this mutation epoch")
        }),
        "committing unchanged failed bytes cleared poison: {function_outputs:#?}"
    );

    Ok(())
}

async fn assert_projection_mutation_outcome(
    fixture: &CompletionProofFixture,
    mutation_command: &str,
    should_clear_poison: bool,
    label: &str,
) -> Result<()> {
    let harness = fixture.harness().await?;
    let focused_call_id = format!("{label}-focused-failure");
    let mutation_call_id = format!("{label}-mutation");
    let canonical_call_id = format!("{label}-canonical");
    let terminal_text = format!("{label} projection accepted");
    let mut responses = vec![
        exec_command_call_response(
            &focused_call_id,
            &fixture.exact_focused_command(),
            &fixture.repo_path,
        ),
        exec_command_call_response(&mutation_call_id, mutation_command, &fixture.repo_path),
        exec_command_call_response(
            &canonical_call_id,
            &fixture.canonical_command,
            &fixture.repo_path,
        ),
    ];
    if should_clear_poison {
        responses.push(terminal_candidate(3, &terminal_text));
    } else {
        responses.extend(repeated_terminal_candidates(
            MAX_REGULAR_LOGICAL_GENERATIONS - 2,
            &terminal_text,
        ));
    }
    mount_sse_sequence(harness.server(), responses).await;

    let events = submit_and_collect(
        harness.test(),
        "fail the exact validation, mutate one projected input, certify, and finish",
    )
    .await?;
    let request_bodies = harness.request_bodies().await;
    let function_outputs = request_bodies
        .iter()
        .flat_map(|body| body["input"].as_array().into_iter().flatten())
        .filter(|item| item["type"] == "function_call_output")
        .map(|item| item["output"].clone())
        .collect::<Vec<_>>();
    assert!(
        events.iter().any(|event| {
            matches!(
                event,
                EventMsg::ExecCommandEnd(end)
                    if end.call_id == focused_call_id && end.exit_code != 0
            )
        }),
        "{label} did not execute the focused failure: {function_outputs:#?}"
    );
    for call_id in [&mutation_call_id, &canonical_call_id] {
        assert!(events.iter().any(|event| {
            matches!(
                event,
                EventMsg::ExecCommandEnd(end)
                    if &end.call_id == call_id && end.exit_code == 0
            )
        }));
    }
    assert_eq!(fixture.canonical_launch_count()?, 1);
    let completion = events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing TurnComplete")?;
    if should_clear_poison {
        assert!(completion.error.is_none(), "{completion:#?}");
        assert_eq!(
            completion.last_agent_message.as_deref(),
            Some(terminal_text.as_str())
        );
    } else {
        assert!(events.iter().all(|event| !is_assistant_output(event)));
        assert!(completion.error.is_some());
        assert!(completion.last_agent_message.is_none());
        assert!(
            request_bodies.iter().any(|body| {
                body.to_string()
                    .contains("is poisoned at this mutation epoch")
            }),
            "{label} incorrectly cleared poison: {request_bodies:#?}"
        );
    }
    Ok(())
}

fn source_map_projection_fixture(
    mutation_script: &str,
) -> Result<(CompletionProofFixture, String)> {
    let fixture = CompletionProofFixture::with_failed_focused_validation()?;
    fs::create_dir_all(fixture.repo_path.join("scripts"))?;
    fs::write(
        fixture.repo_path.join("SOURCEMAP.md"),
        "# Fixture source map\n",
    )?;
    fs::write(
        fixture.repo_path.join("architecture_index.json"),
        "{\"schema_version\":1}\n",
    )?;
    fs::write(
        fixture.repo_path.join("source_owners.toml"),
        r#"schema_version = 2
[[owners]]
id = "fixture"
roots = ["src"]
primary_entries = [{ path = "src/evidence.rs", symbol = "owned_symbol" }]
generated_mirrors = ["SOURCEMAP.md"]
tests = ["src/owner_test.rs"]

[[owners.relationships]]
category = "tests_contracts"
kind = "validated_by"
target = "path:src/no_symbol_evidence.rs"
confidence = "declared"
evidence = [{ path = "src/no_symbol_evidence.rs" }]

[[owners.invariants]]
id = "fixture-contract"
kind = "semantic"
statement = "fixture source-map inputs remain current"
evidence = [{ path = "src/invariant_evidence.rs" }]
tests = ["src/invariant_test.rs"]
"#,
    )?;
    fs::write(
        fixture.repo_path.join("scripts/source_map_check.py"),
        "# fixture source-map validator\n",
    )?;
    fs::write(
        fixture.repo_path.join("scripts/source_owners.py"),
        "# fixture owner resolver\n",
    )?;
    fs::write(
        fixture.repo_path.join("scripts/generated_output_lock.py"),
        "# fixture generated-output lock\n",
    )?;
    fs::write(fixture.repo_path.join("justfile"), "source-map-check:\n")?;
    fs::write(
        fixture.repo_path.join("src/evidence.rs"),
        "pub fn owned_symbol() {}\n",
    )?;
    fs::write(
        fixture.repo_path.join("src/no_symbol_evidence.rs"),
        "pub fn relationship_evidence() {}\n",
    )?;
    fs::write(
        fixture.repo_path.join("src/invariant_evidence.rs"),
        "pub fn invariant_evidence() {}\n",
    )?;
    fs::write(
        fixture.repo_path.join("src/owner_test.rs"),
        "#[test]\nfn owner_test() {}\n",
    )?;
    fs::write(
        fixture.repo_path.join("src/invariant_test.rs"),
        "#[test]\nfn invariant_test() {}\n",
    )?;
    fs::write(fixture.repo_path.join("notes.txt"), "initial note\n")?;
    fs::write(
        fixture.repo_path.join("src/topology_remove.rs"),
        "pub fn removed_later() {}\n",
    )?;
    let command = fixture.configure_validation_projection(
        &[
            "SOURCEMAP.md",
            "architecture_index.json",
            "source_owners.toml",
        ],
        &[
            "scripts/source_map_check.py",
            "scripts/source_owners.py",
            "scripts/generated_output_lock.py",
            "justfile",
        ],
        &["**"],
        &["source_owners.toml"],
        mutation_script,
    )?;
    Ok((fixture, command))
}

fn add_tracked_symlink_to_untracked_regular_file(
    fixture: &CompletionProofFixture,
    target_relative_path: &str,
    symlink_relative_path: &str,
) -> Result<()> {
    let target_path = fixture.repo_path.join(target_relative_path);
    fs::write(&target_path, "untracked symlink target\n")?;
    let symlink_path = fixture.repo_path.join(symlink_relative_path);
    let target_file_name = target_path
        .file_name()
        .context("symlink target path has no file name")?;
    create_file_symlink(Path::new(target_file_name), &symlink_path)
        .context("create tracked symlink to untracked regular file")?;
    run_git(&fixture.repo_path, &["add", "--", symlink_relative_path])?;
    run_git(
        &fixture.repo_path,
        &[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "-m",
            "track symlink without its target",
        ],
    )?;
    let index = Command::new("git")
        .args([
            "ls-files",
            "--stage",
            "--",
            symlink_relative_path,
            target_relative_path,
        ])
        .current_dir(&fixture.repo_path)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .context("inspect tracked symlink fixture index")?;
    anyhow::ensure!(
        index.status.success(),
        "inspect tracked symlink fixture index failed: {}",
        String::from_utf8_lossy(&index.stderr)
    );
    let index = String::from_utf8(index.stdout).context("decode tracked symlink fixture index")?;
    anyhow::ensure!(
        index.lines().any(|line| {
            line.starts_with("120000 ") && line.ends_with(&format!("\t{symlink_relative_path}"))
        }),
        "fixture symlink is not tracked as a symlink: {index}"
    );
    anyhow::ensure!(
        !index
            .lines()
            .any(|line| line.ends_with(&format!("\t{target_relative_path}"))),
        "fixture symlink target unexpectedly became tracked: {index}"
    );
    anyhow::ensure!(
        fs::symlink_metadata(&symlink_path)?
            .file_type()
            .is_symlink(),
        "fixture path is not a symlink"
    );
    Ok(())
}

#[test]
fn source_map_projection_ignores_unrelated_content_through_real_session_path() -> Result<()> {
    run_session_path_test(
        "source_map_projection_ignores_unrelated_content_through_real_session_path",
        || async {
            let (fixture, command) = source_map_projection_fixture(
                "from pathlib import Path\nPath('notes.txt').write_text('changed note\\n', encoding='utf-8')\n",
            )?;
            assert_projection_mutation_outcome(
                &fixture,
                &command,
                false,
                "source-map-unrelated-content",
            )
            .await
        },
    )
}

#[test]
fn source_map_projection_tracks_owned_and_all_revision_inputs_through_real_session_path()
-> Result<()> {
    run_session_path_test(
        "source_map_projection_tracks_owned_and_all_revision_inputs_through_real_session_path",
        || async {
            let (owned_fixture, owned_command) = source_map_projection_fixture(
                "from pathlib import Path\nPath('SOURCEMAP.md').write_text('# Corrected source map\\n', encoding='utf-8')\n",
            )?;
            assert_projection_mutation_outcome(
                &owned_fixture,
                &owned_command,
                true,
                "source-map-owned-input",
            )
            .await?;

            let (evidence_fixture, evidence_command) = source_map_projection_fixture(
                "from pathlib import Path\nPath('src/evidence.rs').write_text('pub fn owned_symbol() { let _ = 1; }\\n', encoding='utf-8')\n",
            )?;
            assert_projection_mutation_outcome(
                &evidence_fixture,
                &evidence_command,
                true,
                "source-map-manifest-evidence-input",
            )
            .await?;

            let (relationship_fixture, relationship_command) = source_map_projection_fixture(
                "from pathlib import Path\nPath('src/no_symbol_evidence.rs').write_text('pub fn relationship_evidence() { let _ = 1; }\\n', encoding='utf-8')\n",
            )?;
            assert_projection_mutation_outcome(
                &relationship_fixture,
                &relationship_command,
                true,
                "source-map-relationship-evidence-without-symbol",
            )
            .await?;

            let (owner_test_fixture, owner_test_command) = source_map_projection_fixture(
                "from pathlib import Path\nPath('src/owner_test.rs').write_text('#[test]\\nfn owner_test() { let _ = 1; }\\n', encoding='utf-8')\n",
            )?;
            assert_projection_mutation_outcome(
                &owner_test_fixture,
                &owner_test_command,
                true,
                "source-map-owner-test-input",
            )
            .await?;

            let (invariant_evidence_fixture, invariant_evidence_command) =
                source_map_projection_fixture(
                    "from pathlib import Path\nPath('src/invariant_evidence.rs').write_text('pub fn invariant_evidence() { let _ = 1; }\\n', encoding='utf-8')\n",
                )?;
            assert_projection_mutation_outcome(
                &invariant_evidence_fixture,
                &invariant_evidence_command,
                true,
                "source-map-invariant-evidence-input",
            )
            .await?;

            let (invariant_test_fixture, invariant_test_command) = source_map_projection_fixture(
                "from pathlib import Path\nPath('src/invariant_test.rs').write_text('#[test]\\nfn invariant_test() { let _ = 1; }\\n', encoding='utf-8')\n",
            )?;
            assert_projection_mutation_outcome(
                &invariant_test_fixture,
                &invariant_test_command,
                true,
                "source-map-invariant-test-input",
            )
            .await
        },
    )
}

#[test]
fn source_map_projection_tracks_only_tracked_present_file_topology_through_real_session_path()
-> Result<()> {
    run_session_path_test(
        "source_map_projection_tracks_only_tracked_present_file_topology_through_real_session_path",
        || async {
            let (untracked_fixture, untracked_command) = source_map_projection_fixture(
                "from pathlib import Path\nPath('src/topology_added.rs').write_text('pub fn added() {}\\n', encoding='utf-8')\n",
            )?;
            assert_projection_mutation_outcome(
                &untracked_fixture,
                &untracked_command,
                false,
                "source-map-untracked-topology-add",
            )
            .await?;

            let (staged_fixture, staged_command) = source_map_projection_fixture(
                "from pathlib import Path\nimport subprocess\nPath('src/topology_added.rs').write_text('pub fn added() {}\\n', encoding='utf-8')\nsubprocess.run(['git', 'add', '--', 'src/topology_added.rs'], check=True)\n",
            )?;
            assert_projection_mutation_outcome(
                &staged_fixture,
                &staged_command,
                true,
                "source-map-staged-topology-add",
            )
            .await?;

            let (remove_fixture, remove_command) = source_map_projection_fixture(
                "from pathlib import Path\nPath('src/topology_remove.rs').unlink()\n",
            )?;
            assert_projection_mutation_outcome(
                &remove_fixture,
                &remove_command,
                true,
                "source-map-topology-remove",
            )
            .await
        },
    )
}

#[test]
fn source_map_projection_excludes_tracked_symlink_targets_through_real_session_path() -> Result<()>
{
    run_session_path_test(
        "source_map_projection_excludes_tracked_symlink_targets_through_real_session_path",
        || async {
            let target_relative_path = "src/untracked_symlink_target.rs";
            let symlink_relative_path = "src/tracked_symlink.rs";

            let (change_fixture, change_command) = source_map_projection_fixture(
                "from pathlib import Path\nPath('src/untracked_symlink_target.rs').write_text('changed untracked target\\n', encoding='utf-8')\n",
            )?;
            add_tracked_symlink_to_untracked_regular_file(
                &change_fixture,
                target_relative_path,
                symlink_relative_path,
            )?;
            assert_projection_mutation_outcome(
                &change_fixture,
                &change_command,
                false,
                "source-map-tracked-symlink-target-change",
            )
            .await?;

            let (remove_fixture, remove_command) = source_map_projection_fixture(
                "from pathlib import Path\nPath('src/untracked_symlink_target.rs').unlink()\n",
            )?;
            add_tracked_symlink_to_untracked_regular_file(
                &remove_fixture,
                target_relative_path,
                symlink_relative_path,
            )?;
            assert_projection_mutation_outcome(
                &remove_fixture,
                &remove_command,
                false,
                "source-map-tracked-symlink-target-remove",
            )
            .await
        },
    )
}

#[test]
fn windows_sandbox_projection_uses_real_root_not_phantom_root_through_real_session_path()
-> Result<()> {
    run_session_path_test(
        "windows_sandbox_projection_uses_real_root_not_phantom_root_through_real_session_path",
        || async {
            let phantom_fixture = CompletionProofFixture::with_failed_focused_validation()?;
            fs::create_dir_all(
                phantom_fixture
                    .repo_path
                    .join("codex-rs/windows-sandbox-rs/src"),
            )?;
            fs::create_dir_all(
                phantom_fixture
                    .repo_path
                    .join("codex-rs/windows-sandbox/src"),
            )?;
            fs::write(
                phantom_fixture
                    .repo_path
                    .join("codex-rs/windows-sandbox-rs/src/lib.rs"),
                "pub fn real_root() {}\n",
            )?;
            fs::write(
                phantom_fixture
                    .repo_path
                    .join("codex-rs/windows-sandbox/src/lib.rs"),
                "pub fn phantom_root() {}\n",
            )?;
            let phantom_command = phantom_fixture.configure_validation_projection(
                &["codex-rs/windows-sandbox-rs/**"],
                &["codex-rs/windows-sandbox-rs/**"],
                &[],
                &[],
                "from pathlib import Path\nPath('codex-rs/windows-sandbox/src/lib.rs').write_text('pub fn phantom_root() { let _ = 1; }\\n', encoding='utf-8')\n",
            )?;
            assert_projection_mutation_outcome(
                &phantom_fixture,
                &phantom_command,
                false,
                "windows-sandbox-phantom-root",
            )
            .await?;

            let real_fixture = CompletionProofFixture::with_failed_focused_validation()?;
            fs::create_dir_all(
                real_fixture
                    .repo_path
                    .join("codex-rs/windows-sandbox-rs/src"),
            )?;
            fs::write(
                real_fixture
                    .repo_path
                    .join("codex-rs/windows-sandbox-rs/src/lib.rs"),
                "pub fn real_root() {}\n",
            )?;
            let real_command = real_fixture.configure_validation_projection(
                &["codex-rs/windows-sandbox-rs/**"],
                &["codex-rs/windows-sandbox-rs/**"],
                &[],
                &[],
                "from pathlib import Path\nPath('codex-rs/windows-sandbox-rs/src/lib.rs').write_text('pub fn real_root() { let _ = 1; }\\n', encoding='utf-8')\n",
            )?;
            assert_projection_mutation_outcome(
                &real_fixture,
                &real_command,
                true,
                "windows-sandbox-real-root",
            )
            .await
        },
    )
}

#[test]
fn inventory_projection_tracks_inline_rust_test_correction_through_real_session_path() -> Result<()>
{
    run_session_path_test(
        "inventory_projection_tracks_inline_rust_test_correction_through_real_session_path",
        || async {
            let fixture = CompletionProofFixture::with_failed_focused_validation()?;
            fs::create_dir_all(fixture.repo_path.join("codex-rs/core/src"))?;
            fs::write(
                fixture.repo_path.join("codex-rs/core/src/inline.rs"),
                "#[cfg(test)]\nmod tests { #[test] fn inline() { assert!(false); } }\n",
            )?;
            let command = fixture.configure_validation_projection(
                &[".codex/validation/**"],
                &["codex-rs/**/*.rs"],
                &[],
                &[],
                "from pathlib import Path\nPath('codex-rs/core/src/inline.rs').write_text('#[cfg(test)]\\nmod tests { #[test] fn inline() { assert!(true); } }\\n', encoding='utf-8')\n",
            )?;
            assert_projection_mutation_outcome(
                &fixture,
                &command,
                true,
                "inventory-inline-rust-test",
            )
            .await
        },
    )
}

#[test]
fn relevant_mutation_clears_poison_through_real_shell_path() -> Result<()> {
    run_session_path_test(
        "relevant_mutation_clears_poison_through_real_shell_path",
        relevant_mutation_clears_poison_through_real_shell_path_impl,
    )
}

async fn relevant_mutation_clears_poison_through_real_shell_path_impl() -> Result<()> {
    let fixture = CompletionProofFixture::with_failed_focused_validation()?;
    let harness = fixture.harness().await?;
    let focused_call_id = "focused-failure-before-relevant-change";
    let mutation_call_id = "relevant-product-input-change";
    let canonical_call_id = "canonical-after-relevant-change";
    mount_sse_sequence(
        harness.server(),
        vec![
            exec_command_call_response(
                focused_call_id,
                &fixture.exact_focused_command(),
                &fixture.repo_path,
            ),
            exec_command_call_response(
                mutation_call_id,
                &fixture.mutation_command,
                &fixture.repo_path,
            ),
            exec_command_call_response(
                canonical_call_id,
                &fixture.canonical_command,
                &fixture.repo_path,
            ),
            terminal_candidate(3, "relevant mutation received fresh proof"),
        ],
    )
    .await;

    let events = submit_and_collect(
        harness.test(),
        "run the failing validation, correct its owned input, certify, and finish",
    )
    .await?;

    assert!(events.iter().any(|event| {
        matches!(
            event,
            EventMsg::ExecCommandEnd(end)
                if end.call_id == focused_call_id && end.exit_code != 0
        )
    }));
    assert!(events.iter().any(|event| {
        matches!(
            event,
            EventMsg::ExecCommandEnd(end)
                if end.call_id == mutation_call_id && end.exit_code == 0
        )
    }));
    assert!(events.iter().any(|event| {
        matches!(
            event,
            EventMsg::ExecCommandEnd(end)
                if end.call_id == canonical_call_id && end.exit_code == 0
        )
    }));
    assert_eq!(
        fs::read_to_string(fixture.repo_path.join("src/runtime.rs"))?.trim(),
        "pub const VALUE: u8 = 3;"
    );
    assert_eq!(fixture.canonical_launch_count()?, 1);
    let completion = events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing TurnComplete")?;
    assert!(
        completion.error.is_none(),
        "a real relevant correction should create a certifiable epoch: {completion:#?}"
    );
    assert_eq!(
        completion.last_agent_message.as_deref(),
        Some("relevant mutation received fresh proof")
    );

    Ok(())
}

#[test]
fn reverting_to_confirmed_failure_inputs_revokes_canonical_eligibility_through_real_shell_path()
-> Result<()> {
    run_session_path_test(
        "reverting_to_confirmed_failure_inputs_revokes_canonical_eligibility_through_real_shell_path",
        reverting_to_confirmed_failure_inputs_revokes_canonical_eligibility_through_real_shell_path_impl,
    )
}

async fn reverting_to_confirmed_failure_inputs_revokes_canonical_eligibility_through_real_shell_path_impl()
-> Result<()> {
    let fixture = CompletionProofFixture::with_failed_focused_validation()?;
    let failed_input = fs::read(fixture.repo_path.join("src/runtime.rs"))?;
    let harness = fixture.harness().await?;
    let failed_focused_call_id = "focused-failure-before-input-reversion";
    let mutation_call_id = "relevant-product-input-change-before-reversion";
    let passing_focused_call_id = "focused-pass-before-input-reversion";
    let restore_call_id = "restore-confirmed-failure-inputs";
    let canonical_call_id = "canonical-after-input-reversion";
    let mut responses = vec![
        exec_command_call_response(
            failed_focused_call_id,
            &fixture.exact_focused_command(),
            &fixture.repo_path,
        ),
        exec_command_call_response(
            mutation_call_id,
            &fixture.mutation_command,
            &fixture.repo_path,
        ),
        exec_command_call_response(
            passing_focused_call_id,
            &fixture.exact_focused_command(),
            &fixture.repo_path,
        ),
        exec_command_call_response(
            restore_call_id,
            &fixture.restore_failed_input_command,
            &fixture.repo_path,
        ),
        exec_command_call_response(
            canonical_call_id,
            &fixture.canonical_command,
            &fixture.repo_path,
        ),
    ];
    responses.extend(repeated_terminal_candidates(
        MAX_REGULAR_LOGICAL_GENERATIONS - 4,
        "restoring confirmed-failure inputs must revoke canonical eligibility",
    ));
    mount_sse_sequence(harness.server(), responses).await;

    let events = submit_and_collect(
        harness.test(),
        "fail validation, correct and revalidate its input, restore the failed input, certify, and finish",
    )
    .await?;

    assert!(events.iter().any(|event| {
        matches!(
            event,
            EventMsg::ExecCommandEnd(end)
                if end.call_id == failed_focused_call_id && end.exit_code != 0
        )
    }));
    for call_id in [
        mutation_call_id,
        passing_focused_call_id,
        restore_call_id,
        canonical_call_id,
    ] {
        assert!(events.iter().any(|event| {
            matches!(
                event,
                EventMsg::ExecCommandEnd(end)
                    if end.call_id == call_id && end.exit_code == 0
            )
        }));
    }
    assert_eq!(
        fs::read(fixture.repo_path.join("src/runtime.rs"))?,
        failed_input,
        "the real shell path did not restore the exact confirmed-failure inputs"
    );
    assert_eq!(fixture.canonical_launch_count()?, 1);
    assert!(events.iter().all(|event| !is_assistant_output(event)));
    let completion = events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing TurnComplete")?;
    assert!(completion.error.is_some());
    assert!(completion.last_agent_message.is_none());
    let request_bodies = harness.request_bodies().await;
    assert!(
        request_bodies.iter().any(|body| {
            body.to_string()
                .contains("is poisoned at this mutation epoch")
        }),
        "a focused pass or the intervening correction erased poison after input reversion: {request_bodies:#?}"
    );

    Ok(())
}

#[test]
fn relevant_clean_to_clean_commit_clears_poison_through_real_shell_path() -> Result<()> {
    run_session_path_test(
        "relevant_clean_to_clean_commit_clears_poison_through_real_shell_path",
        relevant_clean_to_clean_commit_clears_poison_through_real_shell_path_impl,
    )
}

async fn relevant_clean_to_clean_commit_clears_poison_through_real_shell_path_impl() -> Result<()> {
    let fixture = CompletionProofFixture::with_failed_focused_validation()?;
    fixture.commit_current_product_state()?;
    assert!(fixture.worktree_is_clean()?);
    let harness = fixture.harness().await?;
    let focused_call_id = "focused-failure-before-relevant-commit";
    let mutation_call_id = "relevant-product-input-commit";
    let canonical_call_id = "canonical-after-relevant-commit";
    mount_sse_sequence(
        harness.server(),
        vec![
            exec_command_call_response(
                focused_call_id,
                &fixture.exact_focused_command(),
                &fixture.repo_path,
            ),
            exec_command_call_response(
                mutation_call_id,
                &fixture.mutation_and_commit_command,
                &fixture.repo_path,
            ),
            exec_command_call_response(
                canonical_call_id,
                &fixture.canonical_command,
                &fixture.repo_path,
            ),
            terminal_candidate(3, "relevant committed mutation received fresh proof"),
        ],
    )
    .await;

    let events = submit_and_collect(
        harness.test(),
        "fail validation from a clean tree, commit a relevant correction, certify, and finish",
    )
    .await?;

    assert!(events.iter().any(|event| {
        matches!(
            event,
            EventMsg::ExecCommandEnd(end)
                if end.call_id == focused_call_id && end.exit_code != 0
        )
    }));
    assert!(events.iter().any(|event| {
        matches!(
            event,
            EventMsg::ExecCommandEnd(end)
                if end.call_id == mutation_call_id && end.exit_code == 0
        )
    }));
    assert!(events.iter().any(|event| {
        matches!(
            event,
            EventMsg::ExecCommandEnd(end)
                if end.call_id == canonical_call_id && end.exit_code == 0
        )
    }));
    assert!(fixture.worktree_is_clean()?);
    assert_eq!(
        fs::read_to_string(fixture.repo_path.join("src/runtime.rs"))?.trim(),
        "pub const VALUE: u8 = 3;"
    );
    assert_eq!(fixture.canonical_launch_count()?, 1);
    let completion = events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing TurnComplete")?;
    assert!(
        completion.error.is_none(),
        "a relevant clean-to-clean commit should create a certifiable epoch: {completion:#?}"
    );
    assert_eq!(
        completion.last_agent_message.as_deref(),
        Some("relevant committed mutation received fresh proof")
    );

    Ok(())
}

#[test]
fn unrelated_clean_to_clean_commit_does_not_clear_poison_through_real_shell_path() -> Result<()> {
    run_session_path_test(
        "unrelated_clean_to_clean_commit_does_not_clear_poison_through_real_shell_path",
        unrelated_clean_to_clean_commit_does_not_clear_poison_through_real_shell_path_impl,
    )
}

async fn unrelated_clean_to_clean_commit_does_not_clear_poison_through_real_shell_path_impl()
-> Result<()> {
    let fixture = CompletionProofFixture::with_failed_focused_validation()?;
    fixture.commit_current_product_state()?;
    assert!(fixture.worktree_is_clean()?);
    let harness = fixture.harness().await?;
    let focused_call_id = "focused-failure-before-unrelated-commit";
    let mutation_call_id = "unrelated-commit";
    let canonical_call_id = "canonical-after-unrelated-commit";
    let mut responses = vec![
        exec_command_call_response(
            focused_call_id,
            &fixture.exact_focused_command(),
            &fixture.repo_path,
        ),
        exec_command_call_response(
            mutation_call_id,
            &fixture.unrelated_mutation_and_commit_command,
            &fixture.repo_path,
        ),
        exec_command_call_response(
            canonical_call_id,
            &fixture.canonical_command,
            &fixture.repo_path,
        ),
    ];
    responses.extend(repeated_terminal_candidates(
        MAX_REGULAR_LOGICAL_GENERATIONS - 2,
        "an unrelated commit must not clear poison",
    ));
    mount_sse_sequence(harness.server(), responses).await;

    let events = submit_and_collect(
        harness.test(),
        "fail validation from a clean tree, commit an unrelated change, certify, and finish",
    )
    .await?;

    assert!(events.iter().any(|event| {
        matches!(
            event,
            EventMsg::ExecCommandEnd(end)
                if end.call_id == focused_call_id && end.exit_code != 0
        )
    }));
    for call_id in [mutation_call_id, canonical_call_id] {
        assert!(events.iter().any(|event| {
            matches!(
                event,
                EventMsg::ExecCommandEnd(end)
                    if end.call_id == call_id && end.exit_code == 0
            )
        }));
    }
    assert!(fixture.worktree_is_clean()?);
    assert_eq!(fixture.canonical_launch_count()?, 1);
    assert!(events.iter().all(|event| !is_assistant_output(event)));
    let completion = events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing TurnComplete")?;
    assert!(completion.error.is_some());
    assert!(completion.last_agent_message.is_none());
    let request_bodies = harness.request_bodies().await;
    assert!(
        request_bodies.iter().any(|body| {
            body.to_string()
                .contains("is poisoned at this mutation epoch")
        }),
        "an unrelated commit cleared the failed validation poison: {request_bodies:#?}"
    );

    Ok(())
}

#[test]
fn mutate_and_restore_invalidates_canonical_attempt_through_real_shell_path() -> Result<()> {
    run_session_path_test(
        "mutate_and_restore_invalidates_canonical_attempt_through_real_shell_path",
        mutate_and_restore_invalidates_canonical_attempt_through_real_shell_path_impl,
    )
}

async fn mutate_and_restore_invalidates_canonical_attempt_through_real_shell_path_impl()
-> Result<()> {
    let fixture = CompletionProofFixture::new()?;
    fixture.enable_mutate_and_restore_during_canonical()?;
    let harness = fixture.harness().await?;
    let canonical_call_id = "canonical-that-mutates-and-restores";
    let canonical_poll_call_id = "await-canonical-that-mutates-and-restores";
    let mut responses = vec![
        exec_command_call_response(
            canonical_call_id,
            &fixture.canonical_command,
            &fixture.repo_path,
        ),
        write_stdin_call_response(canonical_poll_call_id, 1000),
    ];
    responses.extend(repeated_terminal_candidates(
        TOTAL_GENERATIONS_WITH_FORCED_TERMINAL - 2,
        "mutate-and-restore must keep this output private",
    ));
    mount_sse_sequence(harness.server(), responses).await;

    let events = submit_and_collect(
        harness.test(),
        "mutate and restore during certification, then finish",
    )
    .await?;

    assert!(events.iter().any(|event| {
        matches!(
            event,
            EventMsg::ExecCommandEnd(end)
                if end.call_id == canonical_call_id && end.exit_code == 0
        )
    }));
    assert_eq!(
        fs::read_to_string(fixture.repo_path.join("src/runtime.rs"))?.trim(),
        "pub const VALUE: u8 = 2;"
    );
    assert_eq!(fixture.canonical_launch_count()?, 1);
    assert!(events.iter().all(|event| !is_assistant_output(event)));
    let completion = events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing TurnComplete")?;
    assert!(completion.error.is_some());
    assert!(completion.last_agent_message.is_none());
    let request_bodies = harness.request_bodies().await;
    let canonical_outputs = request_bodies
        .iter()
        .flat_map(|body| body["input"].as_array().into_iter().flatten())
        .filter(|item| {
            item["type"] == "function_call_output"
                && (item["call_id"] == canonical_call_id
                    || item["call_id"] == canonical_poll_call_id)
        })
        .map(|item| item["output"].clone())
        .collect::<Vec<_>>();
    assert!(
        canonical_outputs.iter().any(|output| {
            output
                .to_string()
                .contains("restoring the ending bytes does not make that attempt valid")
        }),
        "the real shell path did not report the transient mutation in the canonical tool results: {canonical_outputs:#?}"
    );

    Ok(())
}

#[test]
fn later_canonical_mutate_and_restore_stales_prior_proof_through_real_session_path() -> Result<()> {
    run_session_path_test(
        "later_canonical_mutate_and_restore_stales_prior_proof_through_real_session_path",
        later_canonical_mutate_and_restore_stales_prior_proof_through_real_session_path_impl,
    )
}

async fn later_canonical_mutate_and_restore_stales_prior_proof_through_real_session_path_impl()
-> Result<()> {
    let fixture = CompletionProofFixture::new()?;
    fixture.enable_mutate_and_restore_after_first_canonical()?;
    let harness = fixture.harness().await?;
    let first_canonical_call_id = "first-clean-canonical";
    let second_canonical_call_id = "later-canonical-that-mutates-and-restores";
    let mut responses = vec![
        exec_command_call_response(
            first_canonical_call_id,
            &fixture.canonical_command,
            &fixture.repo_path,
        ),
        terminal_candidate(1, "first certified answer"),
        exec_command_call_response(
            second_canonical_call_id,
            &fixture.canonical_command,
            &fixture.repo_path,
        ),
    ];
    responses.extend(repeated_terminal_candidates(
        TOTAL_GENERATIONS_WITH_FORCED_TERMINAL - 1,
        "the prior proof must not survive a later transient canonical mutation",
    ));
    mount_sse_sequence(harness.server(), responses).await;

    let first_events =
        submit_and_collect(harness.test(), "certify and finish the first turn").await?;
    let first_completion = first_events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing first TurnComplete")?;
    assert!(
        first_completion.error.is_none(),
        "the first clean certification did not release completion: {first_completion:#?}"
    );
    assert_eq!(
        first_completion.last_agent_message.as_deref(),
        Some("first certified answer")
    );
    assert_eq!(fixture.canonical_launch_count()?, 1);

    let second_events = submit_and_collect(
        harness.test(),
        "run certification again while changing and restoring product bytes, then finish",
    )
    .await?;

    assert!(second_events.iter().any(|event| {
        matches!(
            event,
            EventMsg::ExecCommandEnd(end)
                if end.call_id == second_canonical_call_id && end.exit_code == 0
        )
    }));
    assert_eq!(
        fs::read_to_string(fixture.repo_path.join("src/runtime.rs"))?.trim(),
        "pub const VALUE: u8 = 2;"
    );
    assert_eq!(fixture.canonical_launch_count()?, 2);
    assert!(
        second_events
            .iter()
            .all(|event| !is_assistant_output(event))
    );
    let second_completion = second_events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing second TurnComplete")?;
    assert!(second_completion.error.is_some());
    assert!(second_completion.last_agent_message.is_none());
    let request_bodies = harness.request_bodies().await;
    assert!(
        request_bodies.iter().any(|body| {
            body.to_string()
                .contains("restoring the ending bytes does not make that attempt valid")
        }),
        "the second canonical interval reused the prior proof after transient source changes: {request_bodies:#?}"
    );

    Ok(())
}

#[test]
fn later_mutate_and_restore_stales_proof_without_rerunning_certification_across_turns() -> Result<()>
{
    run_session_path_test(
        "later_mutate_and_restore_stales_proof_without_rerunning_certification_across_turns",
        later_mutate_and_restore_stales_proof_without_rerunning_certification_across_turns_impl,
    )
}

async fn later_mutate_and_restore_stales_proof_without_rerunning_certification_across_turns_impl()
-> Result<()> {
    let fixture = CompletionProofFixture::new()?;
    let harness = fixture.harness().await?;
    let canonical_call_id = "canonical-before-later-transient-mutation";
    let mutation_call_id = "later-mutate-and-restore";
    let mut responses = vec![
        exec_command_call_response(
            canonical_call_id,
            &fixture.canonical_command,
            &fixture.repo_path,
        ),
        terminal_candidate(1, "first certified answer"),
        exec_command_call_response(
            mutation_call_id,
            &fixture.post_proof_mutate_restore_command,
            &fixture.repo_path,
        ),
    ];
    responses.extend(repeated_terminal_candidates(
        TOTAL_GENERATIONS_WITH_FORCED_TERMINAL - 1,
        "transient source changes must stale the prior proof",
    ));
    mount_sse_sequence(harness.server(), responses).await;

    let first_events =
        submit_and_collect(harness.test(), "certify and finish the first turn").await?;
    let first_completion = first_events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing first TurnComplete")?;
    assert!(
        first_completion.error.is_none(),
        "the first clean certification did not release completion: {first_completion:#?}"
    );
    assert_eq!(
        first_completion.last_agent_message.as_deref(),
        Some("first certified answer")
    );
    assert_eq!(fixture.canonical_launch_count()?, 1);

    let second_events = submit_and_collect(
        harness.test(),
        "change and restore product bytes without requesting another certification, then finish",
    )
    .await?;

    assert!(second_events.iter().any(|event| {
        matches!(
            event,
            EventMsg::ExecCommandEnd(end)
                if end.call_id == mutation_call_id && end.exit_code == 0
        )
    }));
    assert_eq!(
        fs::read_to_string(fixture.repo_path.join("src/runtime.rs"))?.trim(),
        "pub const VALUE: u8 = 2;"
    );
    assert_eq!(
        fixture.canonical_launch_count()?,
        1,
        "staleness must not cause automatic certification"
    );
    assert!(
        second_events
            .iter()
            .all(|event| !is_assistant_output(event))
    );
    let second_completion = second_events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing second TurnComplete")?;
    let error = second_completion
        .error
        .as_ref()
        .context("transient source changes should stale the prior proof")?;
    assert!(
        error
            .message
            .contains("CompletionProofGate blocked terminal success")
    );
    assert!(error.message.contains(&fixture.canonical_command));
    assert!(second_completion.last_agent_message.is_none());

    Ok(())
}

#[test]
fn final_answer_phase_stays_private_across_a_real_tool_follow_up_until_certified() -> Result<()> {
    run_session_path_test(
        "final_answer_phase_stays_private_across_a_real_tool_follow_up_until_certified",
        final_answer_phase_stays_private_across_a_real_tool_follow_up_until_certified_impl,
    )
}

async fn final_answer_phase_stays_private_across_a_real_tool_follow_up_until_certified_impl()
-> Result<()> {
    const COMMENTARY: &str = "ordinary follow-up progress remains visible";
    const PREMATURE_FINAL: &str = "phase-marked final answer stays private";

    let fixture = CompletionProofFixture::new()?;
    let harness = fixture.harness().await?;
    let follow_up_call_id = "real-follow-up-shell-call";
    let canonical_call_id = "canonical-after-private-final";
    mount_sse_sequence(
        harness.server(),
        vec![
            phased_messages_and_exec_command_response(
                "mixed-phase-follow-up-response",
                "commentary-before-follow-up",
                COMMENTARY,
                "premature-final-before-follow-up",
                PREMATURE_FINAL,
                follow_up_call_id,
                &fixture.no_op_command,
                &fixture.repo_path,
            ),
            exec_command_call_response(
                canonical_call_id,
                &fixture.canonical_command,
                &fixture.repo_path,
            ),
            terminal_candidate(1, "certified final answer"),
        ],
    )
    .await;

    let events = submit_and_collect(
        harness.test(),
        "show progress, run a real tool, certify, and then finish",
    )
    .await?;

    let follow_up_end_index = events
        .iter()
        .position(|event| {
            matches!(
                event,
                EventMsg::ExecCommandEnd(end)
                    if end.call_id == follow_up_call_id && end.exit_code == 0
            )
        })
        .context("the real follow-up shell call did not complete")?;
    let canonical_end_index = events
        .iter()
        .position(|event| {
            matches!(
                event,
                EventMsg::ExecCommandEnd(end)
                    if end.call_id == canonical_call_id && end.exit_code == 0
            )
        })
        .context("the canonical proof shell call did not complete")?;
    let commentary_index = events
        .iter()
        .position(|event| assistant_output_contains(event, COMMENTARY))
        .context("ordinary non-final follow-up commentary was not visible")?;
    let premature_final_indices = events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| {
            assistant_output_contains(event, PREMATURE_FINAL).then_some(index)
        })
        .collect::<Vec<_>>();

    assert!(
        commentary_index > follow_up_end_index && commentary_index < canonical_end_index,
        "ordinary commentary should be released after its tool finishes and before certification"
    );
    assert!(
        !premature_final_indices.is_empty(),
        "the phase-marked final answer was never released after certification"
    );
    assert!(
        premature_final_indices
            .iter()
            .all(|index| *index > canonical_end_index),
        "phase-marked final output escaped before canonical proof: {events:#?}"
    );
    assert_eq!(fixture.canonical_launch_count()?, 1);
    let completion = events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing TurnComplete")?;
    assert!(
        completion.error.is_none(),
        "valid proof should release terminal output: {completion:#?}"
    );
    assert_eq!(
        completion.last_agent_message.as_deref(),
        Some("certified final answer")
    );

    Ok(())
}

#[test]
fn unphased_assistant_message_stays_private_across_real_tool_follow_up_until_certified()
-> Result<()> {
    run_session_path_test(
        "unphased_assistant_message_stays_private_across_real_tool_follow_up_until_certified",
        unphased_assistant_message_stays_private_across_real_tool_follow_up_until_certified_impl,
    )
}

async fn unphased_assistant_message_stays_private_across_real_tool_follow_up_until_certified_impl()
-> Result<()> {
    const COMMENTARY: &str = "ordinary commentary remains visible after the real tool";
    const PREMATURE_UNPHASED: &str = "unphased assistant output stays private";
    const UNPHASED_ID: &str = "premature-unphased-before-follow-up";

    let fixture = CompletionProofFixture::new()?;
    let harness = fixture.harness_with_raw_response_items().await?;
    let follow_up_call_id = "real-follow-up-before-unphased-release";
    let canonical_call_id = "canonical-after-private-unphased-message";
    mount_sse_sequence(
        harness.server(),
        vec![
            commentary_and_unphased_message_and_exec_command_response(
                "commentary-and-unphased-follow-up-response",
                "commentary-before-unphased-follow-up",
                COMMENTARY,
                UNPHASED_ID,
                PREMATURE_UNPHASED,
                follow_up_call_id,
                &fixture.no_op_command,
                &fixture.repo_path,
            ),
            exec_command_call_response(
                canonical_call_id,
                &fixture.canonical_command,
                &fixture.repo_path,
            ),
            terminal_candidate(2, "certified final answer"),
        ],
    )
    .await;

    let events = submit_and_collect(
        harness.test(),
        "show commentary, run a real tool, certify, and then finish",
    )
    .await?;

    let follow_up_end_index = events
        .iter()
        .position(|event| {
            matches!(
                event,
                EventMsg::ExecCommandEnd(end)
                    if end.call_id == follow_up_call_id && end.exit_code == 0
            )
        })
        .context("the real no-op tool follow-up did not complete")?;
    let canonical_end_index = events
        .iter()
        .position(|event| {
            matches!(
                event,
                EventMsg::ExecCommandEnd(end)
                    if end.call_id == canonical_call_id && end.exit_code == 0
            )
        })
        .context("the canonical proof shell call did not complete")?;
    let commentary_index = events
        .iter()
        .position(|event| assistant_output_contains(event, COMMENTARY))
        .context("explicit commentary was not released after the real tool follow-up")?;
    assert_eq!(fixture.canonical_launch_count()?, 1);
    let completion = events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing TurnComplete")?;
    assert!(
        completion.error.is_none(),
        "valid proof should release private assistant output: {completion:#?}\n{events:#?}"
    );
    assert_eq!(
        completion.last_agent_message.as_deref(),
        Some("certified final answer"),
        "the accepted terminal answer was not published: {events:#?}"
    );

    assert!(
        commentary_index > follow_up_end_index && commentary_index < canonical_end_index,
        "explicit commentary should be released after its tool finishes and before certification"
    );
    let legacy_indices = events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| {
            matches!(
                event,
                EventMsg::AgentMessage(message)
                    if message.message.contains(PREMATURE_UNPHASED)
            )
            .then_some(index)
        })
        .collect::<Vec<_>>();
    let delta_indices = events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| {
            matches!(
                event,
                EventMsg::AgentMessageContentDelta(delta)
                    if delta.item_id.as_str() == UNPHASED_ID
                        && delta.delta.contains(PREMATURE_UNPHASED)
            )
            .then_some(index)
        })
        .collect::<Vec<_>>();
    let started_indices = events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| {
            matches!(
                event,
                EventMsg::ItemStarted(started)
                    if matches!(
                        &started.item,
                        TurnItem::AgentMessage(message)
                            if message.id.as_str() == UNPHASED_ID
                    )
            )
            .then_some(index)
        })
        .collect::<Vec<_>>();
    let completed_indices = events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| {
            matches!(
                event,
                EventMsg::ItemCompleted(completed)
                    if matches!(
                        &completed.item,
                        TurnItem::AgentMessage(message)
                            if message.id.as_str() == UNPHASED_ID
                    )
            )
            .then_some(index)
        })
        .collect::<Vec<_>>();
    let raw_indices = events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| {
            matches!(
                event,
                EventMsg::RawResponseItem(raw)
                    if matches!(
                        &raw.item,
                        ResponseItem::Message {
                            id: Some(id),
                            role,
                            ..
                        } if id.as_str() == UNPHASED_ID && role == "assistant"
                    )
            )
            .then_some(index)
        })
        .collect::<Vec<_>>();
    for (kind, indices) in [
        ("legacy", &legacy_indices),
        ("delta", &delta_indices),
        ("started", &started_indices),
        ("completed", &completed_indices),
        ("raw", &raw_indices),
    ] {
        assert_eq!(
            indices.len(),
            1,
            "expected exactly one released {kind} event for the private message: {events:#?}"
        );
        assert!(
            indices[0] > canonical_end_index,
            "{kind} event escaped before canonical proof: {events:#?}"
        );
    }
    assert!(
        started_indices[0] < delta_indices[0]
            && delta_indices[0] < completed_indices[0]
            && completed_indices[0] < raw_indices[0],
        "accepted private output did not retain started < delta < completed < raw order: {events:#?}"
    );
    Ok(())
}

#[test]
fn extension_legacy_assistant_event_stays_private_until_certified() -> Result<()> {
    run_session_path_test(
        "extension_legacy_assistant_event_stays_private_until_certified",
        extension_legacy_assistant_event_stays_private_until_certified_impl,
    )
}

async fn extension_legacy_assistant_event_stays_private_until_certified_impl() -> Result<()> {
    let fixture = CompletionProofFixture::new()?;
    let cwd = fixture.repo_path.abs();
    let harness = TestCodexHarness::with_builder(
        test_codex()
            .with_extensions(legacy_assistant_event_extensions())
            .with_config(move |config| set_fixture_workspace(config, cwd)),
    )
    .await?;
    let extension_call_id = "registered-extension-legacy-event";
    let canonical_call_id = "canonical-after-extension-legacy-event";
    mount_sse_sequence(
        harness.server(),
        vec![
            sse(vec![
                ev_response_created("registered-extension-tool-response"),
                ev_function_call(extension_call_id, LEGACY_EXTENSION_TOOL_NAME, "{}"),
                ev_completed("registered-extension-tool-response"),
            ]),
            exec_command_call_response(
                canonical_call_id,
                &fixture.canonical_command,
                &fixture.repo_path,
            ),
            terminal_candidate(2, "certified after extension legacy event"),
        ],
    )
    .await;

    let events = submit_and_collect(
        harness.test(),
        "run the registered extension, certify, and then finish",
    )
    .await?;

    let extension_started_index = events
        .iter()
        .position(|event| {
            matches!(
                event,
                EventMsg::ItemStarted(started)
                    if matches!(
                        &started.item,
                        TurnItem::Extension(ExtensionItem::WebSearch(item))
                            if item.id == extension_call_id
                    )
            )
        })
        .context("the registered extension did not execute through the real router")?;
    let canonical_end_index = events
        .iter()
        .position(|event| {
            matches!(
                event,
                EventMsg::ExecCommandEnd(end)
                    if end.call_id == canonical_call_id && end.exit_code == 0
            )
        })
        .context("the canonical proof shell call did not complete")?;
    let legacy_event_indices = events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| {
            assistant_output_contains(event, LEGACY_EXTENSION_PRIVATE_MESSAGE).then_some(index)
        })
        .collect::<Vec<_>>();

    assert!(
        extension_started_index < canonical_end_index,
        "the extension lifecycle should remain visible while assistant output is private"
    );
    assert_eq!(
        legacy_event_indices.len(),
        1,
        "the registered extension legacy event must be released exactly once: {events:#?}"
    );
    assert!(
        legacy_event_indices
            .iter()
            .all(|index| *index > canonical_end_index),
        "extension legacy assistant output escaped before certification: {events:#?}"
    );
    assert_eq!(fixture.canonical_launch_count()?, 1);
    let completion = events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing TurnComplete")?;
    assert!(
        completion.error.is_none(),
        "valid proof should release the legacy extension event: {completion:#?}"
    );
    assert_eq!(
        completion.last_agent_message.as_deref(),
        Some("certified after extension legacy event")
    );

    Ok(())
}

#[test]
fn valid_exact_canonical_artifact_releases_buffered_terminal_output() -> Result<()> {
    run_session_path_test(
        "valid_exact_canonical_artifact_releases_buffered_terminal_output",
        valid_exact_canonical_artifact_releases_buffered_terminal_output_impl,
    )
}

#[test]
fn canonical_proof_survives_same_process_compact_resume_and_authorized_root_fork() -> Result<()> {
    run_session_path_test(
        "canonical_proof_survives_same_process_compact_resume_and_authorized_root_fork",
        canonical_proof_survives_compact_resume_and_fork_from_a_fresh_home_impl,
    )
}

#[cfg(windows)]
#[test]
fn completion_proof_test_store_is_process_global_and_never_changes_physical_credential()
-> Result<()> {
    run_session_path_test(
        "completion_proof_test_store_is_process_global_and_never_changes_physical_credential",
        completion_proof_test_store_is_process_global_and_never_changes_physical_credential_impl,
    )
}

#[cfg(windows)]
async fn completion_proof_test_store_is_process_global_and_never_changes_physical_credential_impl()
-> Result<()> {
    let fixture = CompletionProofFixture::new()?;
    let fresh_home = Arc::new(TempDir::new()?);
    let physical_credential_before =
        codex_core::test_support::completion_proof_physical_credential_snapshot(
            fresh_home.path(),
            &fixture.repo_path,
        )
        .map_err(anyhow::Error::msg)?;

    let harness_a = fixture.harness_with_home(Arc::clone(&fresh_home)).await?;
    let retained_thread = harness_a.test().codex.clone();

    // A second manager must load the first manager's authenticated state. This succeeds only when
    // every debug test runtime in this process receives the same private mock store.
    let harness_b = fixture.harness_with_home(Arc::clone(&fresh_home)).await?;
    let resume_cwd = fixture.repo_path.abs();
    let mut failing_resume_builder =
        test_codex().with_config(move |config| set_fixture_workspace(config, resume_cwd));
    let failed_resume = failing_resume_builder
        .resume(
            harness_b.server(),
            Arc::clone(&fresh_home),
            fixture.repo_path.join("missing-rollout.jsonl"),
        )
        .await;
    anyhow::ensure!(
        failed_resume.is_err(),
        "missing rollout unexpectedly created a TestCodex"
    );

    drop(harness_a);
    harness_b.test().codex.shutdown_and_wait().await?;
    drop(harness_b);

    retained_thread.shutdown_and_wait().await?;
    drop(retained_thread);

    let recreated = fixture.harness_with_home(Arc::clone(&fresh_home)).await?;
    recreated.test().codex.shutdown_and_wait().await?;
    drop(recreated);
    let physical_credential_after =
        codex_core::test_support::completion_proof_physical_credential_snapshot(
            fresh_home.path(),
            &fixture.repo_path,
        )
        .map_err(anyhow::Error::msg)?;
    assert_eq!(
        physical_credential_after, physical_credential_before,
        "process-global private test store changed the exact physical credential"
    );
    Ok(())
}

#[test]
fn hidden_skip_worktree_mutation_after_shutdown_blocks_resumed_completion() -> Result<()> {
    run_session_path_test(
        "hidden_skip_worktree_mutation_after_shutdown_blocks_resumed_completion",
        hidden_skip_worktree_mutation_after_shutdown_blocks_resumed_completion_impl,
    )
}

async fn hidden_skip_worktree_mutation_after_shutdown_blocks_resumed_completion_impl() -> Result<()>
{
    let fixture = CompletionProofFixture::new()?;
    let fresh_home = Arc::new(TempDir::new()?);
    anyhow::ensure!(
        fs::read_dir(fresh_home.path())?.next().is_none(),
        "hidden-mutation resume test requires an initially empty CODEX_HOME"
    );
    let harness = fixture.harness_with_home(Arc::clone(&fresh_home)).await?;
    let call_id = "canonical-before-hidden-resume-mutation";
    let mut responses = vec![
        exec_command_call_response(call_id, &fixture.canonical_command, &fixture.repo_path),
        terminal_candidate(1, "initial hidden-mutation lineage certified"),
    ];
    responses.extend(repeated_terminal_candidates(
        TOTAL_GENERATIONS_WITH_FORCED_TERMINAL,
        "hidden mutation must keep resumed output private",
    ));
    mount_sse_sequence(harness.server(), responses).await;

    let initial_events = submit_and_collect(harness.test(), "certify before shutdown").await?;
    let initial_completion = initial_events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing initial TurnComplete")?;
    assert!(
        initial_completion.error.is_none(),
        "initial canonical proof did not release completion: {initial_completion:#?}"
    );
    assert_eq!(fixture.canonical_launch_count()?, 1);

    let rollout = harness
        .test()
        .codex
        .rollout_path()
        .context("missing rollout path before shutdown")?;
    harness.test().codex.shutdown_and_wait().await?;
    run_git(
        &fixture.repo_path,
        &["update-index", "--skip-worktree", "src/runtime.rs"],
    )?;
    fs::write(
        fixture.repo_path.join("src/runtime.rs"),
        "pub const VALUE: u8 = 9;\n",
    )?;
    let hidden_status = Command::new("git")
        .args(["status", "--porcelain=v2", "--untracked-files=all"])
        .current_dir(&fixture.repo_path)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()?;
    anyhow::ensure!(hidden_status.status.success(), "git status failed");
    assert!(
        hidden_status.stdout.is_empty(),
        "fixture mutation was not hidden from ordinary status: {}",
        String::from_utf8_lossy(&hidden_status.stdout)
    );

    let resume_cwd = fixture.repo_path.abs();
    let mut resume_builder =
        test_codex().with_config(move |config| set_fixture_workspace(config, resume_cwd));
    let resumed = resume_builder
        .resume(harness.server(), Arc::clone(&fresh_home), rollout)
        .await?;
    let resumed_events = submit_and_collect(&resumed, "finish after hidden mutation").await?;

    assert!(
        resumed_events
            .iter()
            .all(|event| !is_assistant_output(event))
    );
    let completion = resumed_events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing resumed TurnComplete")?;
    let error = completion
        .error
        .as_ref()
        .context("hidden mutation should block resumed completion")?;
    assert!(
        error
            .message
            .contains("CompletionProofGate blocked terminal success"),
        "unexpected resumed completion error: {}",
        error.message
    );
    assert!(completion.last_agent_message.is_none());
    assert_eq!(
        fixture.canonical_launch_count()?,
        1,
        "the verifier gate must not launch certification after hidden mutation"
    );

    Ok(())
}

async fn canonical_proof_survives_compact_resume_and_fork_from_a_fresh_home_impl() -> Result<()> {
    const COPIED_PROMPT: &str = "finish from a caller-copied certified rollout";
    const RESUMED_PROMPT: &str = "finish after resuming this lineage";
    const FORKED_PROMPT: &str = "finish from the matching fork";
    const RESTARTED_PROMPT: &str = "finish after restarting this lineage";

    let fixture = CompletionProofFixture::new()?;
    let fresh_home = Arc::new(TempDir::new()?);
    anyhow::ensure!(
        fs::read_dir(fresh_home.path())?.next().is_none(),
        "completion-proof lineage test requires an initially empty CODEX_HOME"
    );
    let harness = fixture
        .harness_with_home_and_raw_response_items(Arc::clone(&fresh_home))
        .await?;
    let call_id = "canonical-proof-before-compact-resume-fork";
    mount_sse_sequence(
        harness.server(),
        vec![
            exec_command_call_response(call_id, &fixture.canonical_command, &fixture.repo_path),
            terminal_candidate(1, "initial lineage certified"),
            sse(vec![
                serde_json::json!({
                    "type": "response.output_item.done",
                    "item": {
                        "type": "compaction",
                        "encrypted_content": "completion-proof-compact-summary",
                    }
                }),
                ev_completed("completion-proof-compact-response"),
            ]),
        ],
    )
    .await;
    for response in repeated_terminal_candidates(
        TOTAL_GENERATIONS_WITH_FORCED_TERMINAL,
        "copied rollout must not retain proof",
    ) {
        mount_sse_once_match(
            harness.server(),
            |request: &wiremock::Request| response_request_contains(request, COPIED_PROMPT),
            response,
        )
        .await;
    }
    mount_sse_once_match(
        harness.server(),
        |request: &wiremock::Request| response_request_contains(request, RESUMED_PROMPT),
        terminal_candidate(3, "resumed lineage retained proof"),
    )
    .await;
    mount_sse_once_match(
        harness.server(),
        |request: &wiremock::Request| response_request_contains(request, FORKED_PROMPT),
        terminal_candidate(4, "forked lineage retained proof"),
    )
    .await;
    for response in repeated_terminal_candidates(
        TOTAL_GENERATIONS_WITH_FORCED_TERMINAL,
        "restarted lineage must not retain proof",
    ) {
        mount_sse_once_match(
            harness.server(),
            |request: &wiremock::Request| response_request_contains(request, RESTARTED_PROMPT),
            response,
        )
        .await;
    }

    let initial_events =
        submit_and_collect(harness.test(), "certify this lineage and finish").await?;
    let initial_tool_output = function_call_output(&initial_events, call_id)
        .unwrap_or("<missing model-visible tool output>");
    let initial_command_end = initial_events
        .iter()
        .find_map(|event| match event {
            EventMsg::ExecCommandEnd(end) if end.call_id == call_id => Some(end),
            _ => None,
        })
        .with_context(|| {
            format!(
                "missing initial canonical ExecCommandEnd; tool output: {initial_tool_output}; events: {initial_events:#?}"
            )
        })?;
    assert_eq!(
        initial_command_end.exit_code, 0,
        "initial canonical command failed:\nstdout:\n{}\nstderr:\n{}",
        initial_command_end.stdout, initial_command_end.stderr
    );
    let initial_completion = initial_events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing initial TurnComplete")?;
    let initial_request_bodies = harness.request_bodies().await;
    let initial_gate_feedback = initial_request_bodies
        .iter()
        .filter_map(|body| {
            let body = body.to_string();
            body.find("CompletionProofGate")
                .map(|offset| body[offset..].chars().take(2_000).collect::<String>())
        })
        .collect::<Vec<_>>();
    assert!(
        initial_completion.error.is_none(),
        "initial canonical proof did not release completion; tool output: {initial_tool_output}; gate feedback: {initial_gate_feedback:#?}; completion: {initial_completion:#?}"
    );
    assert_eq!(fixture.canonical_launch_count()?, 1);

    harness.test().codex.submit(Op::Compact).await?;
    let compact_deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    let compact_completion = loop {
        let remaining = compact_deadline.saturating_duration_since(tokio::time::Instant::now());
        anyhow::ensure!(
            !remaining.is_zero(),
            "timed out waiting for compact TurnComplete"
        );
        let event = tokio::time::timeout(remaining, harness.test().codex.next_event())
            .await
            .context("timed out waiting for completion-proof compact event")??;
        if let EventMsg::TurnComplete(completion) = event.msg {
            break completion;
        }
    };
    assert!(
        compact_completion.error.is_none(),
        "compaction lost current completion proof: {compact_completion:#?}"
    );
    assert_eq!(fixture.canonical_launch_count()?, 1);

    let compacted_rollout = harness
        .test()
        .codex
        .rollout_path()
        .context("missing compacted rollout path")?;
    let thread_manager = Arc::clone(&harness.test().thread_manager);
    let resumed_config = harness.test().config.clone();
    harness.test().codex.shutdown_and_wait().await?;

    let copied_rollout = fresh_home.path().join("copied-certified-rollout.jsonl");
    fs::copy(&compacted_rollout, &copied_rollout)?;
    let copied = thread_manager
        .resume_thread_from_rollout(
            resumed_config.clone(),
            copied_rollout,
            thread_manager.auth_manager(),
            /*parent_trace*/ None,
            /*supports_openai_form_elicitation*/ false,
        )
        .await?
        .thread;
    let copied_events = submit_and_collect_thread(&copied, &resumed_config, COPIED_PROMPT).await?;
    let copied_completion = copied_events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing copied-rollout TurnComplete")?;
    let copied_error = copied_completion
        .error
        .as_ref()
        .context("a caller-copied rollout replayed process-private issuance")?;
    assert!(
        copied_error
            .message
            .contains("CompletionProofGate blocked terminal success"),
        "unexpected copied-rollout completion error: {}",
        copied_error.message
    );
    assert!(copied_completion.last_agent_message.is_none());
    assert_eq!(
        fixture.canonical_launch_count()?,
        1,
        "copied-rollout rejection must not launch certification"
    );
    copied.shutdown_and_wait().await?;

    let resumed = thread_manager
        .resume_thread_from_rollout(
            resumed_config.clone(),
            compacted_rollout,
            thread_manager.auth_manager(),
            /*parent_trace*/ None,
            /*supports_openai_form_elicitation*/ false,
        )
        .await?
        .thread;
    let resumed_events =
        submit_and_collect_thread(&resumed, &resumed_config, RESUMED_PROMPT).await?;
    let resumed_completion = resumed_events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing resumed TurnComplete")?;
    assert!(
        resumed_completion.error.is_none(),
        "resumed lineage did not retain current proof: {resumed_completion:#?}"
    );
    assert_eq!(
        resumed_completion.last_agent_message.as_deref(),
        Some("resumed lineage retained proof")
    );
    assert_eq!(fixture.canonical_launch_count()?, 1);

    let resumed_rollout = resumed
        .rollout_path()
        .context("missing resumed rollout path")?;
    resumed.shutdown_and_wait().await?;
    let forked = Box::pin(thread_manager.fork_thread(
        2,
        resumed_config.clone(),
        resumed_rollout,
        /*thread_source*/ None,
        /*parent_trace*/ None,
    ))
    .await?
    .thread;
    let forked_events = submit_and_collect_thread(&forked, &resumed_config, FORKED_PROMPT).await?;
    let forked_completion = forked_events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing forked TurnComplete")?;
    assert!(
        forked_completion.error.is_none(),
        "forked lineage did not retain current proof: {forked_completion:#?}"
    );
    assert_eq!(
        forked_completion.last_agent_message.as_deref(),
        Some("forked lineage retained proof")
    );
    assert_eq!(
        fixture.canonical_launch_count()?,
        1,
        "compact, resume, or fork launched certification instead of reusing private lineage state"
    );

    let restarted_rollout = forked
        .rollout_path()
        .context("missing forked rollout path")?;
    forked.shutdown_and_wait().await?;
    let resume_cwd = fixture.repo_path.abs();
    let mut restart_builder =
        test_codex().with_config(move |config| set_fixture_workspace(config, resume_cwd));
    let restarted = restart_builder
        .resume(harness.server(), Arc::clone(&fresh_home), restarted_rollout)
        .await?;
    let restarted_events = submit_and_collect(&restarted, RESTARTED_PROMPT).await?;
    let restarted_completion = restarted_events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing restarted TurnComplete")?;
    let restarted_error = restarted_completion
        .error
        .as_ref()
        .context("a new manager incorrectly reconstructed process-private proof issuance")?;
    assert!(
        restarted_error
            .message
            .contains("CompletionProofGate blocked terminal success"),
        "unexpected restarted-lineage completion error: {}",
        restarted_error.message
    );
    assert!(restarted_completion.last_agent_message.is_none());
    assert_eq!(fixture.canonical_launch_count()?, 1);

    Ok(())
}

#[test]
fn transient_mutation_cannot_race_terminal_release_before_unified_exec_end() -> Result<()> {
    run_session_path_test(
        "transient_mutation_cannot_race_terminal_release_before_unified_exec_end",
        transient_mutation_cannot_race_terminal_release_before_unified_exec_end_impl,
    )
}

#[cfg(windows)]
#[test]
fn canonical_certification_descendants_are_offline_but_can_use_loopback() -> Result<()> {
    run_session_path_test(
        "canonical_certification_descendants_are_offline_but_can_use_loopback",
        canonical_certification_descendants_are_offline_but_can_use_loopback_impl,
    )
}

#[cfg(windows)]
async fn canonical_certification_descendants_are_offline_but_can_use_loopback_impl() -> Result<()> {
    // A connected UDP socket performs only a local route lookup; no datagram
    // is sent to the documentation-only TEST-NET address.
    let route_probe = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    route_probe.connect((Ipv4Addr::new(192, 0, 2, 1), 9))?;
    let IpAddr::V4(non_loopback_ip) = route_probe.local_addr()?.ip() else {
        anyhow::bail!("the Windows host did not select an IPv4 interface");
    };
    anyhow::ensure!(
        !non_loopback_ip.is_loopback() && !non_loopback_ip.is_unspecified(),
        "the Windows host has no usable non-loopback IPv4 interface"
    );

    let loopback_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let non_loopback_listener = TcpListener::bind((non_loopback_ip, 0))?;
    let loopback_address = loopback_listener.local_addr()?;
    let non_loopback_address = non_loopback_listener.local_addr()?;

    // Prove the local non-loopback service is reachable before the nested
    // canonical runner is confined, then drain that connection. The enclosing
    // process may already be under the same canonical WFP confinement.
    match TcpStream::connect(non_loopback_address) {
        Ok(preflight) => {
            let (accepted_preflight, _) = non_loopback_listener.accept()?;
            drop(accepted_preflight);
            drop(preflight);
        }
        Err(error)
            if error.kind() == std::io::ErrorKind::PermissionDenied
                && std::env::var_os(core_test_support::sandbox_network_env_var()).is_some() => {}
        Err(error) => return Err(error.into()),
    }

    let fixture = CompletionProofFixture::new()?;
    fixture.install_network_confinement_probe(loopback_address, non_loopback_address)?;
    let cwd = fixture.repo_path.abs();
    let inherited_loopback_proxy = format!("http://{loopback_address}");
    let harness = TestCodexHarness::with_builder(test_codex().with_config(move |config| {
        set_fixture_workspace(config, cwd);
        config
            .permissions
            .shell_environment_policy
            .r#set
            .insert("hTtP_pRoXy".to_string(), inherited_loopback_proxy);
    }))
    .await?;
    let call_id = "canonical-proof-network-confinement";
    mount_sse_sequence(
        harness.server(),
        vec![
            exec_command_call_response(call_id, &fixture.canonical_command, &fixture.repo_path),
            terminal_candidate(1, "offline certification completed"),
        ],
    )
    .await;

    // submit_and_collect explicitly selects PermissionProfile::Disabled. The
    // canonical boundary must therefore be independent of the turn profile.
    let events = submit_and_collect(harness.test(), "certify under offline confinement").await?;

    let command_end = events
        .iter()
        .find_map(|event| match event {
            EventMsg::ExecCommandEnd(end) if end.call_id == call_id => Some(end),
            _ => None,
        })
        .context("missing canonical confinement ExecCommandEnd")?;
    assert_eq!(
        command_end.exit_code, 0,
        "canonical confinement command did not succeed:\n{}",
        command_end.stderr,
    );
    loopback_listener.set_nonblocking(true)?;
    let (_loopback_connection, _) = loopback_listener
        .accept()
        .context("canonical descendant did not reach the loopback fake")?;
    non_loopback_listener.set_nonblocking(true)?;
    assert!(
        matches!(
            non_loopback_listener.accept(),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
        ),
        "canonical descendant reached a non-loopback local service"
    );

    let completion = events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing TurnComplete")?;
    assert!(
        completion.error.is_none(),
        "canonical proof was rejected after its network probe succeeded: {completion:#?}"
    );
    assert_eq!(fixture.canonical_launch_count()?, 1);

    Ok(())
}

#[test]
fn deleted_tracked_input_can_be_certified_through_real_session_path() -> Result<()> {
    run_session_path_test(
        "deleted_tracked_input_can_be_certified_through_real_session_path",
        deleted_tracked_input_can_be_certified_through_real_session_path_impl,
    )
}

async fn deleted_tracked_input_can_be_certified_through_real_session_path_impl() -> Result<()> {
    let fixture = CompletionProofFixture::new()?;
    fs::remove_file(fixture.repo_path.join("src/runtime.rs"))?;
    let harness = fixture.harness().await?;
    let call_id = "canonical-proof-with-deleted-input";
    mount_sse_sequence(
        harness.server(),
        vec![
            exec_command_call_response(call_id, &fixture.canonical_command, &fixture.repo_path),
            terminal_candidate(1, "deleted input received current proof"),
        ],
    )
    .await;

    let events = submit_and_collect(harness.test(), "delete, certify, and finish").await?;

    assert!(events.iter().any(|event| {
        matches!(
            event,
            EventMsg::ExecCommandEnd(end) if end.call_id == call_id && end.exit_code == 0
        )
    }));
    let completion = events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing TurnComplete")?;
    assert!(
        completion.error.is_none(),
        "a tracked deletion should have a stable certifiable fingerprint: {completion:#?}"
    );
    assert_eq!(
        completion.last_agent_message.as_deref(),
        Some("deleted input received current proof")
    );
    assert_eq!(fixture.canonical_launch_count()?, 1);

    Ok(())
}

async fn valid_exact_canonical_artifact_releases_buffered_terminal_output_impl() -> Result<()> {
    let fixture = AcceptedCompletionProofFixture::new()?;
    let cwd = fixture.repo_path().abs();
    let harness = TestCodexHarness::with_builder(test_codex().with_config(move |config| {
        set_fixture_workspace(config, cwd);
    }))
    .await?;
    let call_id = "canonical-completion-proof";
    mount_sse_sequence(
        harness.server(),
        vec![
            exec_command_call_response(call_id, fixture.canonical_command(), fixture.repo_path()),
            terminal_candidate(1, "certified final answer"),
        ],
    )
    .await;

    let events = submit_and_collect(harness.test(), "certify and finish").await?;
    let request_bodies = harness.request_bodies().await;
    let function_outputs = request_bodies
        .iter()
        .flat_map(|body| body["input"].as_array().into_iter().flatten())
        .filter(|item| item["type"] == "function_call_output")
        .map(|item| item["output"].clone())
        .collect::<Vec<_>>();

    assert!(
        fixture.canonical_runner_launched(),
        "canonical command did not launch; function outputs: {function_outputs:#?}; events: {events:#?}"
    );
    assert!(events.iter().any(|event| {
        matches!(
            event,
            EventMsg::ExecCommandEnd(end) if end.call_id == call_id && end.exit_code == 0
        )
    }));
    assert!(
        events.iter().any(|event| {
            match event {
                EventMsg::AgentMessage(message) => message.message == "certified final answer",
                EventMsg::ItemCompleted(event) => match &event.item {
                    TurnItem::AgentMessage(message) => message.content.iter().any(|content| {
                        matches!(
                            content,
                            AgentMessageContent::Text { text }
                                if text == "certified final answer"
                        )
                    }),
                    _ => false,
                },
                _ => false,
            }
        }),
        "certified assistant output was not released; events: {events:#?}"
    );
    let completion = events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing TurnComplete")?;
    assert!(
        completion.error.is_none(),
        "valid proof should release success"
    );
    assert_eq!(
        completion.last_agent_message.as_deref(),
        Some("certified final answer")
    );
    assert_eq!(request_bodies.len(), 2);

    Ok(())
}

async fn transient_mutation_cannot_race_terminal_release_before_unified_exec_end_impl() -> Result<()>
{
    const TERMINAL_TEXT: &str = "transiently stale proof must keep this output private";
    let fixture = CompletionProofFixture::new()?;
    let marker_root = fixture.repo_path.join(".fixture-state");
    let script_path = marker_root.join("transient-mutate-restore.py");
    let start_marker = marker_root.join("start-transient-mutation");
    let restored_marker = marker_root.join("transient-restored");
    let release_marker = marker_root.join("release-transient-process");
    fs::write(
        &script_path,
        r#"from pathlib import Path
import sys
import time

source = Path(sys.argv[1])
start_marker = Path(sys.argv[2])
restored_marker = Path(sys.argv[3])
release_marker = Path(sys.argv[4])
original = source.read_bytes()

# The real unified-exec is already retained and holding its workspace lease.
# Wait until the test observes that the terminal model response was served,
# then give its sampling-time proof check time to accept the still-current
# workspace before creating the transient mutation.
deadline = time.monotonic() + 60
while not start_marker.exists():
    if time.monotonic() >= deadline:
        raise SystemExit('timed out waiting for the start marker')
    time.sleep(0.02)
time.sleep(0.75)
source.write_text('pub const VALUE: u8 = 9;\n', encoding='utf-8')
time.sleep(1.0)
source.write_bytes(original)
restored_marker.write_text('restored\n', encoding='utf-8')

deadline = time.monotonic() + 60
while not release_marker.exists():
    if time.monotonic() >= deadline:
        raise SystemExit('timed out waiting for the release marker')
    time.sleep(0.02)
"#,
    )?;
    let _release_on_drop = FileSignalOnDrop::new(release_marker.clone());
    let harness = fixture.harness().await?;
    let canonical_call_id = "canonical-before-live-transient-mutation";
    let mutation_call_id = "live-transient-mutate-restore";
    let mutation_arguments = json!({
        "kind": "argv",
        "program": available_python_command()?,
        "args": [
            script_path,
            fixture.repo_path.join("src/runtime.rs"),
            start_marker,
            restored_marker,
            release_marker,
        ],
        // Keep the command silent and alive after restoring the bytes so the
        // next model response reaches the terminal gate before its end event.
        "yield_time_ms": 4_000,
        "tty": false,
    });
    let mutation_response = sse(vec![
        ev_response_created("live-transient-mutation-response"),
        ev_function_call(
            mutation_call_id,
            "exec_command",
            &serde_json::to_string(&mutation_arguments)?,
        ),
        ev_completed("live-transient-mutation-response"),
    ]);
    mount_sse_sequence(
        harness.server(),
        vec![
            exec_command_call_response(
                canonical_call_id,
                &fixture.canonical_command,
                &fixture.repo_path,
            ),
            mutation_response,
            terminal_candidate(2, TERMINAL_TEXT),
        ],
    )
    .await;

    submit_without_wait(
        harness.test(),
        "certify, transiently mutate and restore through a live command, then finish",
    )
    .await?;

    // Windows canonical sandbox preparation can take close to a minute on a
    // cold test machine before the runner launches. Wait until the retained
    // unified-exec has yielded and the third, terminal model response has been
    // served while the workspace is still current.
    let readiness_deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    let mut events = Vec::new();
    while harness.request_bodies().await.len() < 3 {
        if tokio::time::Instant::now() >= readiness_deadline {
            let request_count = harness.request_bodies().await.len();
            let canonical_launch_count = fixture.canonical_launch_count()?;
            anyhow::bail!(
                "the terminal response was not served while the retained unified-exec held its workspace lease; requests={request_count}, canonical_launches={canonical_launch_count}, events={events:#?}"
            );
        }
        if let Ok(event) = tokio::time::timeout(
            Duration::from_millis(100),
            harness.test().codex.next_event(),
        )
        .await
        {
            events.push(event?.msg);
        }
    }
    assert_eq!(harness.request_bodies().await.len(), 3);
    assert_eq!(fixture.canonical_launch_count()?, 1);
    assert!(events.iter().all(|event| {
        !matches!(
            event,
            EventMsg::ExecCommandEnd(end) if end.call_id == mutation_call_id
        )
    }));
    assert!(
        events
            .iter()
            .all(|event| !matches!(event, EventMsg::TurnComplete(_)))
    );
    assert!(events.iter().all(|event| !is_assistant_output(event)));

    fs::write(&start_marker, "start\n")?;
    let mutation_deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while !restored_marker.exists() {
        if tokio::time::Instant::now() >= mutation_deadline {
            anyhow::bail!(
                "the retained unified-exec did not mutate and restore its tracked source: {events:#?}"
            );
        }
        if let Ok(event) = tokio::time::timeout(
            Duration::from_millis(100),
            harness.test().codex.next_event(),
        )
        .await
        {
            events.push(event?.msg);
        }
    }
    assert_eq!(
        fs::read_to_string(fixture.repo_path.join("src/runtime.rs"))?.trim(),
        "pub const VALUE: u8 = 2;",
        "the retained unified-exec did not restore the tracked source bytes"
    );

    // The process remains alive on its release marker, so its end event and
    // mutation bookkeeping cannot race behind a successful terminal release.
    tokio::time::sleep(Duration::from_secs(2)).await;
    loop {
        match tokio::time::timeout(
            Duration::from_millis(100),
            harness.test().codex.next_event(),
        )
        .await
        {
            Ok(event) => events.push(event?.msg),
            Err(_) => break,
        }
    }
    assert!(
        events.iter().all(|event| {
            !matches!(
                event,
                EventMsg::ExecCommandEnd(end) if end.call_id == mutation_call_id
            )
        }),
        "the retained unified-exec ended before its release marker: {events:#?}"
    );
    assert!(
        events
            .iter()
            .all(|event| !matches!(event, EventMsg::TurnComplete(_))),
        "terminal completion escaped while the workspace lease was held: {events:#?}"
    );
    assert!(
        events.iter().all(|event| !is_assistant_output(event)),
        "assistant output escaped while the workspace lease was held: {events:#?}"
    );
    assert_eq!(harness.request_bodies().await.len(), 3);
    assert_eq!(
        fixture.canonical_launch_count()?,
        1,
        "staleness must not automatically rerun canonical certification"
    );

    fs::write(&release_marker, "release\n")?;

    let completion_deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let remaining = completion_deadline.saturating_duration_since(tokio::time::Instant::now());
        anyhow::ensure!(
            !remaining.is_zero(),
            "timed out waiting for mutation bookkeeping and TurnComplete"
        );
        let event = tokio::time::timeout(remaining, harness.test().codex.next_event())
            .await
            .context("timed out waiting for the live transient process to finish")??;
        let terminal = matches!(event.msg, EventMsg::TurnComplete(_));
        events.push(event.msg);
        if terminal {
            break;
        }
    }

    let exec_end_index = events
        .iter()
        .position(|event| {
            matches!(
                event,
                EventMsg::ExecCommandEnd(end)
                    if end.call_id == mutation_call_id && end.exit_code == 0
            )
        })
        .context("missing successful retained unified-exec end event")?;
    let completion_index = events
        .iter()
        .position(|event| matches!(event, EventMsg::TurnComplete(_)))
        .context("missing TurnComplete")?;
    assert!(
        exec_end_index < completion_index,
        "the retained unified-exec must end before terminal completion: {events:#?}"
    );
    assert!(
        events.iter().all(|event| !is_assistant_output(event)),
        "terminal-looking output escaped after end-event mutation bookkeeping: {events:#?}"
    );
    let completion = events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing TurnComplete")?;
    let error = completion
        .error
        .as_ref()
        .context("the transient mutation should stale the canonical proof")?;
    assert!(
        error
            .message
            .contains("CompletionProofGate blocked terminal success")
    );
    assert!(completion.last_agent_message.is_none());
    assert!(completion.surfaced_result.is_none());
    assert_eq!(harness.request_bodies().await.len(), 3);
    assert_eq!(
        fixture.canonical_launch_count()?,
        1,
        "staleness must not automatically rerun canonical certification"
    );

    Ok(())
}

#[test]
fn missing_runner_attestation_is_pre_result_through_real_shell_path() -> Result<()> {
    run_session_path_test(
        "missing_runner_attestation_is_pre_result_through_real_shell_path",
        missing_runner_attestation_is_pre_result_through_real_shell_path_impl,
    )
}

async fn missing_runner_attestation_is_pre_result_through_real_shell_path_impl() -> Result<()> {
    let fixture = CompletionProofFixture::with_missing_runner_attestation()?;
    let harness = fixture.harness_with_raw_response_items().await?;
    let call_id = "canonical-without-runner-attestation";
    let mut responses = vec![exec_command_call_response(
        call_id,
        &fixture.canonical_command,
        &fixture.repo_path,
    )];
    responses.extend(repeated_terminal_candidates(
        TOTAL_GENERATIONS_WITH_FORCED_TERMINAL - 1,
        "missing process attestation must keep this output private",
    ));
    mount_sse_sequence(harness.server(), responses).await;

    let events = submit_and_collect(harness.test(), "certify without process attestation").await?;

    assert_eq!(fixture.canonical_launch_count()?, 1);
    assert!(events.iter().any(|event| {
        matches!(
            event,
            EventMsg::ExecCommandEnd(end) if end.call_id == call_id && end.exit_code == 0
        )
    }));
    let tool_output = function_call_output(&events, call_id)
        .context("missing model-visible exec_command output")?;
    assert_eq!(function_call_output_success(&events, call_id), Some(false));
    assert!(
        tool_output.contains("Process exited with code -1"),
        "trusted rejection did not replace only the model-visible exit status: {tool_output}"
    );
    assert!(tool_output.contains("did not complete private process attestation"));
    assert!(events.iter().all(|event| !is_assistant_output(event)));
    let completion = events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing TurnComplete")?;
    assert!(completion.error.is_some());
    assert!(completion.last_agent_message.is_none());
    let request_bodies = harness.request_bodies().await;
    assert!(
        request_bodies.iter().any(|body| {
            body.to_string()
                .contains("did not complete private process attestation")
        }),
        "the real shell path did not report missing process attestation: {request_bodies:#?}"
    );

    Ok(())
}

#[test]
fn yielded_missing_runner_attestation_is_rejected_through_write_stdin() -> Result<()> {
    run_session_path_test(
        "yielded_missing_runner_attestation_is_rejected_through_write_stdin",
        yielded_missing_runner_attestation_is_rejected_through_write_stdin_impl,
    )
}

async fn yielded_missing_runner_attestation_is_rejected_through_write_stdin_impl() -> Result<()> {
    let fixture = CompletionProofFixture::with_missing_runner_attestation()?;
    fixture.delay_canonical_runner_for_yield()?;
    let harness = fixture.harness_with_raw_response_items().await?;
    let exec_call_id = "yielded-canonical-without-runner-attestation";
    let write_stdin_call_id = "await-yielded-canonical-without-runner-attestation";
    let mut responses = vec![
        exec_command_call_response(exec_call_id, &fixture.canonical_command, &fixture.repo_path),
        write_stdin_call_response(write_stdin_call_id, 1000),
    ];
    responses.extend(repeated_terminal_candidates(
        TOTAL_GENERATIONS_WITH_FORCED_TERMINAL - 2,
        "yielded missing process attestation must keep this output private",
    ));
    mount_sse_sequence(harness.server(), responses).await;

    let events = submit_and_collect(
        harness.test(),
        "yield certification, await it, and reject missing process attestation",
    )
    .await;
    if events.is_err() {
        // Preserve the session failure instead of masking it with a mock-drop
        // panic about the responses that the stalled session never requested.
        harness.server().reset().await;
    }
    let events = events?;

    assert_eq!(fixture.canonical_launch_count()?, 1);
    assert!(events.iter().any(|event| {
        matches!(
            event,
            EventMsg::ExecCommandEnd(end)
                if end.call_id == exec_call_id && end.exit_code == 0
        )
    }));
    let initial_output = function_call_output(&events, exec_call_id)
        .context("missing model-visible yielded exec_command output")?;
    assert!(initial_output.contains("Process running with session ID 1000"));
    let terminal_output = function_call_output(&events, write_stdin_call_id)
        .context("missing model-visible write_stdin output")?;
    assert_eq!(
        function_call_output_success(&events, write_stdin_call_id),
        Some(false)
    );
    assert!(
        terminal_output.contains("Process exited with code -1"),
        "trusted rejection did not replace only the model-visible exit status: {terminal_output}"
    );
    assert!(terminal_output.contains("did not complete private process attestation"));
    assert!(events.iter().all(|event| !is_assistant_output(event)));
    let completion = events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing TurnComplete")?;
    assert!(completion.error.is_some());
    assert!(completion.last_agent_message.is_none());

    Ok(())
}

#[test]
fn transient_pre_result_allows_unchanged_fresh_canonical_retry_through_real_shell_path()
-> Result<()> {
    run_session_path_test(
        "transient_pre_result_allows_unchanged_fresh_canonical_retry_through_real_shell_path",
        transient_pre_result_allows_unchanged_fresh_canonical_retry_through_real_shell_path_impl,
    )
}

async fn transient_pre_result_allows_unchanged_fresh_canonical_retry_through_real_shell_path_impl()
-> Result<()> {
    let fixture = CompletionProofFixture::with_transient_canonical_pre_result()?;
    let runtime_before = fs::read(fixture.repo_path.join("src/runtime.rs"))?;
    let harness = fixture.harness().await?;
    let first_call_id = "canonical-transient-pre-result";
    let retry_call_id = "canonical-fresh-unchanged-retry";
    mount_sse_sequence(
        harness.server(),
        vec![
            exec_command_call_response(
                first_call_id,
                &fixture.canonical_command,
                &fixture.repo_path,
            ),
            exec_command_call_response(
                retry_call_id,
                &fixture.canonical_command,
                &fixture.repo_path,
            ),
            terminal_candidate(2, "unchanged retry received fresh canonical proof"),
        ],
    )
    .await;

    let events = submit_and_collect(
        harness.test(),
        "retry the complete canonical command after its transient runner error, without editing",
    )
    .await?;

    assert!(events.iter().any(|event| {
        matches!(
            event,
            EventMsg::ExecCommandEnd(end)
                if end.call_id == first_call_id && end.exit_code == 0
        )
    }));
    assert!(events.iter().any(|event| {
        matches!(
            event,
            EventMsg::ExecCommandEnd(end)
                if end.call_id == retry_call_id && end.exit_code == 0
        )
    }));
    assert_eq!(fixture.canonical_launch_count()?, 2);
    assert_eq!(
        fs::read(fixture.repo_path.join("src/runtime.rs"))?,
        runtime_before,
        "the transient runner error or its retry mutated the validation input"
    );
    let request_bodies = harness.request_bodies().await;
    assert!(
        request_bodies.iter().any(|body| {
            body.to_string()
                .contains("did not complete private process attestation")
        }),
        "the first canonical attempt was not classified as a pre-result runner error: \
         {request_bodies:#?}"
    );
    let completion = events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing TurnComplete")?;
    assert!(
        completion.error.is_none(),
        "a transient pre-result error poisoned the unchanged retry: {:#?}; last model request: {:#?}",
        completion.error,
        request_bodies.last()
    );
    assert_eq!(
        completion.last_agent_message.as_deref(),
        Some("unchanged retry received fresh canonical proof")
    );

    Ok(())
}

#[test]
fn focused_pre_result_workspace_drift_does_not_poison_unchanged_retry() -> Result<()> {
    run_session_path_test(
        "focused_pre_result_workspace_drift_does_not_poison_unchanged_retry",
        focused_pre_result_workspace_drift_does_not_poison_unchanged_retry_impl,
    )
}

async fn focused_pre_result_workspace_drift_does_not_poison_unchanged_retry_impl() -> Result<()> {
    let fixture = CompletionProofFixture::with_failed_focused_validation()?;
    let restore_command = fixture.install_focused_failure_unrelated_workspace_drift()?;
    let harness = fixture
        .harness_with_raw_response_items_and_extensions(required_tool_failure_extensions())
        .await?;
    let drifting_failure_call_id = "focused-failure-with-unrelated-workspace-drift";
    let restore_call_id = "restore-unrelated-workspace-drift";
    let retry_call_id = "focused-pass-after-workspace-drift-restoration";
    let required_failure_call_id = "stop-after-focused-workspace-drift-proof";
    mount_sse_sequence(
        harness.server(),
        vec![
            exec_command_call_response(
                drifting_failure_call_id,
                &fixture.exact_focused_command(),
                &fixture.repo_path,
            ),
            exec_command_call_response(restore_call_id, &restore_command, &fixture.repo_path),
            exec_command_call_response(
                retry_call_id,
                &fixture.exact_focused_command(),
                &fixture.repo_path,
            ),
            sse(vec![
                ev_response_created("stop-after-focused-workspace-drift-proof-response"),
                ev_function_call(required_failure_call_id, REQUIRED_FAILURE_TOOL_NAME, "{}"),
                ev_completed("stop-after-focused-workspace-drift-proof-response"),
            ]),
        ],
    )
    .await;

    let events = submit_and_collect(
        harness.test(),
        "run a focused failure that drifts unrelated workspace content, restore that content, prove the unchanged validation retry is not poisoned, then stop through the registered required operation",
    )
    .await?;

    let failure_output = function_call_output(&events, drifting_failure_call_id)
        .context("missing model-visible focused workspace-drift output")?;
    assert_eq!(
        function_call_output_success(&events, drifting_failure_call_id),
        Some(false)
    );
    assert!(
        failure_output.contains(
            "repository changed before the confirmed validation failure could be recorded"
        ),
        "focused workspace drift was not classified as a pre-result error: {failure_output}"
    );
    for call_id in [restore_call_id, retry_call_id] {
        assert!(events.iter().any(|event| {
            matches!(
                event,
                EventMsg::ExecCommandEnd(end)
                    if end.call_id == call_id && end.exit_code == 0
            )
        }));
    }
    assert_eq!(
        function_call_output_success(&events, retry_call_id),
        Some(true),
        "the focused pre-result error incorrectly poisoned the unchanged validation retry"
    );
    assert_eq!(
        fs::read_to_string(fixture.repo_path.join("notes.txt"))?.trim(),
        "initial note"
    );

    Ok(())
}

#[test]
fn canonical_transient_workspace_drift_does_not_poison_unchanged_retry() -> Result<()> {
    run_session_path_test(
        "canonical_transient_workspace_drift_does_not_poison_unchanged_retry",
        canonical_transient_workspace_drift_does_not_poison_unchanged_retry_impl,
    )
}

async fn canonical_transient_workspace_drift_does_not_poison_unchanged_retry_impl() -> Result<()> {
    let fixture = CompletionProofFixture::with_confirmed_canonical_failure_then_pass()?;
    fixture.enable_mutate_and_restore_during_first_canonical()?;
    let harness = fixture.harness().await?;
    let drifting_failure_call_id = "canonical-failure-with-transient-workspace-drift";
    let drifting_failure_poll_call_id = "await-canonical-failure-with-transient-workspace-drift";
    let retry_call_id = "canonical-pass-after-transient-workspace-drift";
    let mut failure_responses = vec![
        exec_command_call_response(
            drifting_failure_call_id,
            &fixture.canonical_command,
            &fixture.repo_path,
        ),
        write_stdin_call_response(drifting_failure_poll_call_id, 1000),
    ];
    failure_responses.extend(repeated_terminal_candidates(
        TOTAL_GENERATIONS_WITH_FORCED_TERMINAL - 2,
        "transient canonical failure must keep this output private",
    ));
    mount_sse_sequence(harness.server(), failure_responses).await;

    let first_events = submit_and_collect(
        harness.test(),
        "run a failing canonical attempt with transient workspace drift, then finish this turn",
    )
    .await?;

    assert!(first_events.iter().any(|event| {
        matches!(
            event,
            EventMsg::ExecCommandEnd(end)
                if end.call_id == drifting_failure_call_id && end.exit_code != 0
        )
    }));
    let first_completion = first_events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing first TurnComplete")?;
    assert!(first_completion.error.is_some());
    assert!(first_completion.last_agent_message.is_none());
    assert_eq!(fixture.canonical_launch_count()?, 1);
    assert_eq!(
        fs::read_to_string(fixture.repo_path.join("scratch/observer-probe.txt"))?.trim(),
        "initial probe\n\n// transient canonical mutation"
    );
    let request_bodies = harness.request_bodies().await;
    let function_outputs = request_bodies
        .iter()
        .filter_map(|body| body.get("input").and_then(serde_json::Value::as_array))
        .flatten()
        .filter(|item| {
            item.get("type").and_then(serde_json::Value::as_str) == Some("function_call_output")
        })
        .map(serde_json::Value::to_string)
        .collect::<Vec<_>>();
    assert!(
        function_outputs
            .iter()
            .any(|output| output
                .contains("restoring the ending bytes does not make that attempt valid")),
        "the first canonical attempt did not reach the observed-path pre-result branch: {function_outputs:#?}"
    );
    fs::write(
        fixture.repo_path.join("scratch/observer-probe.txt"),
        "initial probe\n",
    )?;

    mount_sse_sequence(
        harness.server(),
        vec![
            exec_command_call_response(
                retry_call_id,
                &fixture.canonical_command,
                &fixture.repo_path,
            ),
            terminal_candidate(1, "transient canonical pre-result did not poison retry"),
        ],
    )
    .await;
    let retry_events = submit_and_collect(
        harness.test(),
        "retry the canonical command unchanged without transient workspace drift, then finish",
    )
    .await?;

    let retry_exit_code = retry_events.iter().find_map(|event| match event {
        EventMsg::ExecCommandEnd(end) if end.call_id == retry_call_id => Some(end.exit_code),
        _ => None,
    });
    let retry_output = function_call_output(&retry_events, retry_call_id);
    assert_eq!(
        fixture.canonical_launch_count()?,
        2,
        "the unchanged canonical retry did not launch; exit={retry_exit_code:?}, output={retry_output:?}"
    );
    assert_eq!(
        retry_exit_code,
        Some(0),
        "the unchanged canonical retry did not exit successfully; output={retry_output:?}"
    );
    let completion = retry_events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing TurnComplete")?;
    assert!(
        completion.error.is_none(),
        "the canonical pre-result error incorrectly poisoned the unchanged retry: {completion:#?}"
    );
    assert_eq!(
        completion.last_agent_message.as_deref(),
        Some("transient canonical pre-result did not poison retry")
    );

    Ok(())
}

#[test]
fn snapshotless_poison_survives_observed_relevant_edit_and_revert() -> Result<()> {
    run_session_path_test(
        "snapshotless_poison_survives_observed_relevant_edit_and_revert",
        snapshotless_poison_survives_observed_relevant_edit_and_revert_impl,
    )
}

async fn snapshotless_poison_survives_observed_relevant_edit_and_revert_impl() -> Result<()> {
    let fixture = CompletionProofFixture::with_failed_focused_validation()?;
    fs::write(
        fixture.repo_path.join("source_owners.toml"),
        "schema_version = [\n",
    )?;
    let mutation_command = fixture.configure_validation_projection(
        &["src/**"],
        &["src/**"],
        &[],
        &["source_owners.toml"],
        "from pathlib import Path\nPath('src/runtime.rs').write_text('pub const VALUE: u8 = 3;\\n', encoding='utf-8')\nfailure_marker = Path('.fixture-state/focused-validation-fails')\nif failure_marker.exists():\n    failure_marker.unlink()\n",
    )?;
    let harness = fixture.harness().await?;
    let failure_call_id = "focused-failure-with-unavailable-input-snapshot";
    let mutation_call_id = "relevant-edit-after-snapshotless-poison";
    let restore_call_id = "restore-relevant-input-after-observation";
    let retry_call_id = "passing-focused-retry-after-relevant-edit-revert";
    let mut responses = vec![
        exec_command_call_response(
            failure_call_id,
            &fixture.exact_focused_command(),
            &fixture.repo_path,
        ),
        exec_command_call_response(mutation_call_id, &mutation_command, &fixture.repo_path),
        exec_command_call_response(
            restore_call_id,
            &fixture.restore_failed_input_command,
            &fixture.repo_path,
        ),
        exec_command_call_response(
            retry_call_id,
            &fixture.exact_focused_command(),
            &fixture.repo_path,
        ),
    ];
    responses.extend(repeated_terminal_candidates(
        MAX_REGULAR_LOGICAL_GENERATIONS - 3,
        "snapshotless poison must survive an observed relevant edit and revert",
    ));
    mount_sse_sequence(harness.server(), responses).await;

    let events = submit_and_collect(
        harness.test(),
        "record a confirmed failure when its input snapshot cannot be established, edit and observe its input, restore the failed bytes, run a passing unchanged retry, then finish",
    )
    .await?;

    assert!(events.iter().any(|event| {
        matches!(
            event,
            EventMsg::ExecCommandEnd(end)
                if end.call_id == failure_call_id && end.exit_code != 0
        )
    }));
    for call_id in [mutation_call_id, restore_call_id, retry_call_id] {
        assert!(events.iter().any(|event| {
            matches!(
                event,
                EventMsg::ExecCommandEnd(end)
                    if end.call_id == call_id && end.exit_code == 0
            )
        }));
    }
    assert_eq!(
        fs::read_to_string(fixture.repo_path.join("src/runtime.rs"))?,
        "pub const VALUE: u8 = 2;\n",
        "the relevant validation input was not restored to its failure-time bytes"
    );
    assert!(events.iter().all(|event| !is_assistant_output(event)));
    let completion = events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing TurnComplete")?;
    assert!(completion.error.is_some());
    assert!(completion.last_agent_message.is_none());
    let request_bodies = harness.request_bodies().await;
    assert!(
        request_bodies.iter().any(|body| {
            body.to_string()
                .contains("fixture.validation is poisoned at this mutation epoch")
        }),
        "snapshotless poison was cleared by a historical relevant-path observation: {request_bodies:#?}"
    );

    Ok(())
}

#[test]
fn changed_trusted_helper_is_rejected_before_focused_failure_can_poison() -> Result<()> {
    run_session_path_test(
        "changed_trusted_helper_is_rejected_before_focused_failure_can_poison",
        changed_trusted_helper_is_rejected_before_focused_failure_can_poison_impl,
    )
}

async fn changed_trusted_helper_is_rejected_before_focused_failure_can_poison_impl() -> Result<()> {
    let fixture = CompletionProofFixture::with_failed_focused_validation()?;
    let restore_helper_command = fixture.install_trusted_helper_authority_probe()?;
    let trusted_helper_source = fs::read(fixture.repo_path.join("trusted_helper.py"))?;
    let harness = fixture
        .harness_with_raw_response_items_and_extensions(required_tool_failure_extensions())
        .await?;
    let focused_failure_call_id = "focused-failure-after-helper-change";
    let restore_call_id = "restore-helper-after-focused-attempt";
    let focused_retry_call_id = "focused-pass-after-helper-restoration";
    let required_failure_call_id = "stop-after-focused-helper-authority-proof";
    mount_sse_sequence(
        harness.server(),
        vec![
            exec_command_call_response(
                focused_failure_call_id,
                &fixture.exact_focused_command(),
                &fixture.repo_path,
            ),
            exec_command_call_response(
                restore_call_id,
                &restore_helper_command,
                &fixture.repo_path,
            ),
            exec_command_call_response(
                focused_retry_call_id,
                &fixture.exact_focused_command(),
                &fixture.repo_path,
            ),
            sse(vec![
                ev_response_created("stop-after-focused-helper-authority-proof-response"),
                ev_function_call(required_failure_call_id, REQUIRED_FAILURE_TOOL_NAME, "{}"),
                ev_completed("stop-after-focused-helper-authority-proof-response"),
            ]),
        ],
    )
    .await;

    let events = submit_and_collect(
        harness.test(),
        "run a failing focused validation that changes a trusted helper, restore it, prove a passing focused retry is not poisoned, then stop through the registered required operation",
    )
    .await?;

    assert!(events.iter().any(|event| {
        matches!(
            event,
            EventMsg::ExecCommandEnd(end)
                if end.call_id == focused_failure_call_id && end.exit_code != 0
        )
    }));
    let output = function_call_output(&events, focused_failure_call_id)
        .context("missing model-visible focused helper-authority output")?;
    assert_eq!(
        function_call_output_success(&events, focused_failure_call_id),
        Some(false)
    );
    assert!(
        output.contains(
            "trusted repository policy member trusted_helper.py differs from the version explicitly trusted at HEAD"
        ),
        "trusted helper change was not classified as an authority pre-result error: {output}"
    );
    for call_id in [restore_call_id, focused_retry_call_id] {
        assert!(events.iter().any(|event| {
            matches!(
                event,
                EventMsg::ExecCommandEnd(end)
                    if end.call_id == call_id && end.exit_code == 0
            )
        }));
    }
    assert_eq!(
        function_call_output_success(&events, focused_retry_call_id),
        Some(true),
        "the authority pre-result error incorrectly poisoned the focused validation"
    );
    assert_eq!(fixture.canonical_launch_count()?, 0);
    assert_eq!(
        fs::read(fixture.repo_path.join("trusted_helper.py"))?,
        trusted_helper_source,
        "the source-only helper was not restored to its committed authority"
    );

    Ok(())
}

#[test]
fn changed_trusted_helper_is_rejected_before_canonical_failure_can_poison() -> Result<()> {
    run_session_path_test(
        "changed_trusted_helper_is_rejected_before_canonical_failure_can_poison",
        changed_trusted_helper_is_rejected_before_canonical_failure_can_poison_impl,
    )
}

async fn changed_trusted_helper_is_rejected_before_canonical_failure_can_poison_impl() -> Result<()>
{
    let fixture = CompletionProofFixture::with_confirmed_canonical_failure_then_pass()?;
    let restore_helper_command = fixture.install_trusted_helper_authority_probe()?;
    let trusted_helper_source = fs::read(fixture.repo_path.join("trusted_helper.py"))?;
    let harness = fixture
        .harness_with_raw_response_items_and_extensions(required_tool_failure_extensions())
        .await?;
    let canonical_failure_call_id = "canonical-failure-after-helper-change";
    let restore_call_id = "restore-helper-after-canonical-attempt";
    let focused_retry_call_id = "focused-pass-after-canonical-helper-restoration";
    let required_failure_call_id = "stop-after-canonical-helper-authority-proof";
    mount_sse_sequence(
        harness.server(),
        vec![
            exec_command_call_response(
                canonical_failure_call_id,
                &fixture.canonical_command,
                &fixture.repo_path,
            ),
            exec_command_call_response(
                restore_call_id,
                &restore_helper_command,
                &fixture.repo_path,
            ),
            exec_command_call_response(
                focused_retry_call_id,
                &fixture.exact_focused_command(),
                &fixture.repo_path,
            ),
            sse(vec![
                ev_response_created("stop-after-canonical-helper-authority-proof-response"),
                ev_function_call(required_failure_call_id, REQUIRED_FAILURE_TOOL_NAME, "{}"),
                ev_completed("stop-after-canonical-helper-authority-proof-response"),
            ]),
        ],
    )
    .await;

    let events = submit_and_collect(
        harness.test(),
        "run a failing canonical validation that changes a trusted helper, restore it, prove a passing focused validation is not poisoned, then stop through the registered required operation",
    )
    .await?;

    assert!(events.iter().any(|event| {
        matches!(
            event,
            EventMsg::ExecCommandEnd(end)
                if end.call_id == canonical_failure_call_id && end.exit_code != 0
        )
    }));
    let output = function_call_output(&events, canonical_failure_call_id)
        .context("missing model-visible canonical helper-authority output")?;
    assert_eq!(
        function_call_output_success(&events, canonical_failure_call_id),
        Some(false)
    );
    assert!(
        output.contains(
            "trusted repository policy member trusted_helper.py differs from the version explicitly trusted at HEAD"
        ),
        "trusted helper change was not classified as an authority pre-result error: {output}"
    );
    for call_id in [restore_call_id, focused_retry_call_id] {
        assert!(events.iter().any(|event| {
            matches!(
                event,
                EventMsg::ExecCommandEnd(end)
                    if end.call_id == call_id && end.exit_code == 0
            )
        }));
    }
    assert_eq!(
        function_call_output_success(&events, focused_retry_call_id),
        Some(true),
        "the authority pre-result error incorrectly poisoned the canonical validation"
    );
    assert_eq!(fixture.canonical_launch_count()?, 1);
    assert_eq!(
        fs::read(fixture.repo_path.join("trusted_helper.py"))?,
        trusted_helper_source,
        "the source-only helper was not restored to its committed authority"
    );

    Ok(())
}

#[test]
fn confirmed_canonical_failure_poisons_unchanged_all_pass_retry_through_real_shell_path()
-> Result<()> {
    run_session_path_test(
        "confirmed_canonical_failure_poisons_unchanged_all_pass_retry_through_real_shell_path",
        confirmed_canonical_failure_poisons_unchanged_all_pass_retry_through_real_shell_path_impl,
    )
}

async fn confirmed_canonical_failure_poisons_unchanged_all_pass_retry_through_real_shell_path_impl()
-> Result<()> {
    let fixture = CompletionProofFixture::with_confirmed_canonical_failure_then_pass()?;
    let runtime_before = fs::read(fixture.repo_path.join("src/runtime.rs"))?;
    let harness = fixture.harness().await?;
    let failure_call_id = "canonical-confirmed-validation-failure";
    let retry_call_id = "canonical-unchanged-all-pass-retry";
    let mut responses = vec![
        exec_command_call_response(
            failure_call_id,
            &fixture.canonical_command,
            &fixture.repo_path,
        ),
        exec_command_call_response(
            retry_call_id,
            &fixture.canonical_command,
            &fixture.repo_path,
        ),
    ];
    responses.extend(repeated_terminal_candidates(
        MAX_REGULAR_LOGICAL_GENERATIONS - 1,
        "an unchanged canonical pass cannot erase confirmed failure poison",
    ));
    mount_sse_sequence(harness.server(), responses).await;

    let events = submit_and_collect(
        harness.test(),
        "run a failing canonical validation, retry unchanged with a pass, then finish",
    )
    .await?;

    assert!(events.iter().any(|event| {
        matches!(
            event,
            EventMsg::ExecCommandEnd(end)
                if end.call_id == failure_call_id && end.exit_code != 0
        )
    }));
    assert!(events.iter().any(|event| {
        matches!(
            event,
            EventMsg::ExecCommandEnd(end)
                if end.call_id == retry_call_id && end.exit_code == 0
        )
    }));
    assert_eq!(fixture.canonical_launch_count()?, 2);
    assert_eq!(
        fs::read(fixture.repo_path.join("src/runtime.rs"))?,
        runtime_before,
        "the canonical failure sequence changed the owned validation input"
    );
    assert!(events.iter().all(|event| !is_assistant_output(event)));
    let completion = events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing TurnComplete")?;
    assert!(completion.error.is_some());
    assert!(completion.last_agent_message.is_none());
    let request_bodies = harness.request_bodies().await;
    assert!(
        request_bodies.iter().any(|body| {
            body.to_string()
                .contains("fixture.validation is poisoned at this mutation epoch")
        }),
        "the unchanged all-pass retry erased canonical failure poison: {request_bodies:#?}"
    );

    Ok(())
}

#[test]
fn canonical_failure_before_infrastructure_error_still_poisons_unchanged_retry_through_real_shell_path()
-> Result<()> {
    run_session_path_test(
        "canonical_failure_before_infrastructure_error_still_poisons_unchanged_retry_through_real_shell_path",
        canonical_failure_before_infrastructure_error_still_poisons_unchanged_retry_through_real_shell_path_impl,
    )
}

async fn canonical_failure_before_infrastructure_error_still_poisons_unchanged_retry_through_real_shell_path_impl()
-> Result<()> {
    let fixture =
        CompletionProofFixture::with_confirmed_canonical_failure_then_infrastructure_error()?;
    let runtime_before = fs::read(fixture.repo_path.join("src/runtime.rs"))?;
    let infrastructure_input_before =
        fs::read(fixture.repo_path.join("infrastructure/runner.txt"))?;
    let harness = fixture.harness().await?;
    let failure_call_id = "canonical-failure-then-infrastructure-error";
    let retry_call_id = "canonical-clean-retry-after-mixed-result";
    let mut responses = vec![
        exec_command_call_response(
            failure_call_id,
            &fixture.canonical_command,
            &fixture.repo_path,
        ),
        exec_command_call_response(
            retry_call_id,
            &fixture.canonical_command,
            &fixture.repo_path,
        ),
    ];
    responses.extend(repeated_terminal_candidates(
        MAX_REGULAR_LOGICAL_GENERATIONS - 1,
        "a later infrastructure error cannot erase an earlier confirmed failure",
    ));
    mount_sse_sequence(harness.server(), responses).await;

    let events = submit_and_collect(
        harness.test(),
        "run validation A to failure, hit validation B infrastructure failure, retry unchanged, and finish",
    )
    .await?;

    assert!(events.iter().any(|event| {
        matches!(
            event,
            EventMsg::ExecCommandEnd(end)
                if end.call_id == failure_call_id && end.exit_code != 0
        )
    }));
    assert!(events.iter().any(|event| {
        matches!(
            event,
            EventMsg::ExecCommandEnd(end)
                if end.call_id == retry_call_id && end.exit_code == 0
        )
    }));
    assert_eq!(fixture.canonical_launch_count()?, 2);
    assert_eq!(
        fs::read(fixture.repo_path.join("src/runtime.rs"))?,
        runtime_before,
        "the mixed-result canonical attempts changed validation A's owned input"
    );
    assert_eq!(
        fs::read(fixture.repo_path.join("infrastructure/runner.txt"))?,
        infrastructure_input_before,
        "the mixed-result canonical attempts changed validation B's owned input"
    );
    assert!(events.iter().all(|event| !is_assistant_output(event)));
    let completion = events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing TurnComplete")?;
    assert!(completion.error.is_some());
    assert!(completion.last_agent_message.is_none());
    let request_bodies = harness.request_bodies().await;
    assert!(
        request_bodies.iter().any(|body| {
            body.to_string()
                .contains("fixture.validation is poisoned at this mutation epoch")
        }),
        "the later infrastructure error discarded validation A's poison: {request_bodies:#?}"
    );

    Ok(())
}

#[test]
fn confirmed_sibling_failure_survives_malformed_claim_through_real_shell_path() -> Result<()> {
    run_session_path_test(
        "confirmed_sibling_failure_survives_malformed_claim_through_real_shell_path",
        confirmed_sibling_failure_survives_malformed_claim_through_real_shell_path_impl,
    )
}

async fn confirmed_sibling_failure_survives_malformed_claim_through_real_shell_path_impl()
-> Result<()> {
    let fixture =
        CompletionProofFixture::with_confirmed_canonical_failure_then_infrastructure_error()?;
    fixture.make_later_infrastructure_result_a_malformed_claimed_failure()?;
    let harness = fixture.harness().await?;
    let failure_call_id = "canonical-valid-failure-with-malformed-sibling-claim";
    let retry_call_id = "focused-unchanged-retry-after-malformed-sibling-claim";
    let mut responses = vec![
        exec_command_call_response(
            failure_call_id,
            &fixture.canonical_command,
            &fixture.repo_path,
        ),
        exec_command_call_response(
            retry_call_id,
            &fixture.exact_focused_command(),
            &fixture.repo_path,
        ),
    ];
    responses.extend(repeated_terminal_candidates(
        MAX_REGULAR_LOGICAL_GENERATIONS - 1,
        "a malformed sibling claim cannot erase an independently confirmed failure",
    ));
    mount_sse_sequence(harness.server(), responses).await;

    let events = submit_and_collect(
        harness.test(),
        "run one valid confirmed failure beside a malformed failure claim, retry unchanged, and finish",
    )
    .await?;

    assert!(events.iter().any(|event| {
        matches!(
            event,
            EventMsg::ExecCommandEnd(end)
                if end.call_id == failure_call_id && end.exit_code != 0
        )
    }));
    assert!(events.iter().any(|event| {
        matches!(
            event,
            EventMsg::ExecCommandEnd(end)
                if end.call_id == retry_call_id && end.exit_code == 0
        )
    }));
    assert_eq!(fixture.canonical_launch_count()?, 1);
    let completion = events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing TurnComplete")?;
    assert!(completion.error.is_some());
    assert!(completion.last_agent_message.is_none());
    let request_bodies = harness.request_bodies().await;
    assert!(
        request_bodies.iter().any(|body| {
            body.to_string()
                .contains("fixture.validation is poisoned at this mutation epoch")
        }),
        "the malformed sibling claim discarded the independently confirmed failure: {request_bodies:#?}"
    );

    Ok(())
}

#[test]
fn removing_poisoned_validation_from_trusted_config_cannot_certify_through_real_shell_path()
-> Result<()> {
    run_session_path_test(
        "removing_poisoned_validation_from_trusted_config_cannot_certify_through_real_shell_path",
        removing_poisoned_validation_from_trusted_config_cannot_certify_through_real_shell_path_impl,
    )
}

async fn removing_poisoned_validation_from_trusted_config_cannot_certify_through_real_shell_path_impl()
-> Result<()> {
    let fixture =
        CompletionProofFixture::with_confirmed_canonical_failure_then_infrastructure_error()?;
    let harness = fixture.harness().await?;
    let failure_call_id = "canonical-failure-before-validation-consolidation";
    let consolidation_call_id = "consolidate-validation-and-commit";
    let retry_call_id = "canonical-after-poisoned-validation-was-removed";
    let mut failure_responses = vec![exec_command_call_response(
        failure_call_id,
        &fixture.canonical_command,
        &fixture.repo_path,
    )];
    failure_responses.extend(repeated_terminal_candidates(
        MAX_REGULAR_LOGICAL_GENERATIONS,
        "a confirmed failure must remain private before validation consolidation",
    ));
    mount_sse_sequence(harness.server(), failure_responses).await;

    let failure_events = submit_and_collect(
        harness.test(),
        "fail a required validation before changing the trusted validation set",
    )
    .await?;

    assert_eq!(fixture.canonical_launch_count()?, 1);
    assert!(
        failure_events
            .iter()
            .all(|event| !is_assistant_output(event))
    );
    let failure_completion = failure_events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing failed-validation TurnComplete")?;
    assert!(failure_completion.error.is_some());
    assert!(failure_completion.last_agent_message.is_none());

    let mut responses = vec![
        exec_command_call_response(
            consolidation_call_id,
            &fixture.consolidate_validations_and_commit_command,
            &fixture.repo_path,
        ),
        exec_command_call_response(
            retry_call_id,
            &fixture.canonical_command,
            &fixture.repo_path,
        ),
    ];
    responses.extend(repeated_terminal_candidates(
        MAX_REGULAR_LOGICAL_GENERATIONS - 1,
        "a trusted config cannot omit a previously poisoned required validation",
    ));
    mount_sse_sequence(harness.server(), responses).await;

    let events = submit_and_collect(
        harness.test(),
        "consolidate the failed validation out of the trusted config, certify, and finish",
    )
    .await?;

    assert!(events.iter().any(|event| {
        matches!(
            event,
            EventMsg::ExecCommandEnd(end)
                if end.call_id == consolidation_call_id && end.exit_code == 0
        )
    }));
    assert!(events.iter().any(|event| {
        matches!(
            event,
            EventMsg::ExecCommandEnd(end)
                if end.call_id == retry_call_id && end.exit_code == 0
        )
    }));
    assert_eq!(fixture.canonical_launch_count()?, 2);
    let consolidated_config = fs::read_to_string(
        fixture
            .repo_path
            .join(".codex/validation/completion-proof.toml"),
    )?;
    assert!(!consolidated_config.contains(&format!("id = \"{FIXTURE_VALIDATION_ID}\"")));
    assert!(consolidated_config.contains(&format!("id = \"{FIXTURE_SECOND_VALIDATION_ID}\"")));
    assert!(events.iter().all(|event| !is_assistant_output(event)));
    let completion = events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing TurnComplete")?;
    assert!(completion.error.is_some());
    assert!(completion.last_agent_message.is_none());
    let request_bodies = harness.request_bodies().await;
    assert!(
        request_bodies.iter().any(|body| {
            body.to_string().contains(
                "validation fixture.validation remains poisoned but is absent from the current canonical validation set",
            )
        }),
        "the real canonical path did not reject the omitted poisoned validation: {request_bodies:#?}"
    );

    Ok(())
}

#[test]
fn mismatched_runner_identity_is_pre_result_through_real_shell_path() -> Result<()> {
    run_session_path_test(
        "mismatched_runner_identity_is_pre_result_through_real_shell_path",
        mismatched_runner_identity_is_pre_result_through_real_shell_path_impl,
    )
}

async fn mismatched_runner_identity_is_pre_result_through_real_shell_path_impl() -> Result<()> {
    let fixture = CompletionProofFixture::with_mismatched_runner_identity()?;
    let harness = fixture.harness().await?;
    let call_id = "canonical-with-mismatched-runner-identity";
    let mut responses = vec![exec_command_call_response(
        call_id,
        &fixture.canonical_command,
        &fixture.repo_path,
    )];
    responses.extend(repeated_terminal_candidates(
        TOTAL_GENERATIONS_WITH_FORCED_TERMINAL - 1,
        "mismatched process identity must keep this output private",
    ));
    mount_sse_sequence(harness.server(), responses).await;

    let events =
        submit_and_collect(harness.test(), "certify with a mismatched runner identity").await?;

    assert_eq!(fixture.canonical_launch_count()?, 1);
    assert!(events.iter().all(|event| !is_assistant_output(event)));
    let completion = events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing TurnComplete")?;
    assert!(completion.error.is_some());
    assert!(completion.last_agent_message.is_none());
    let request_bodies = harness.request_bodies().await;
    assert!(
        request_bodies.iter().any(|body| {
            body.to_string()
                .contains("did not match the operating-system-authenticated process and entrypoint")
        }),
        "the real shell path did not report the runner identity mismatch: {request_bodies:#?}"
    );

    Ok(())
}

#[test]
fn mismatched_validation_runner_is_rejected_through_real_session_path() -> Result<()> {
    run_session_path_test(
        "mismatched_validation_runner_is_rejected_through_real_session_path",
        mismatched_validation_runner_is_rejected_through_real_session_path_impl,
    )
}

async fn mismatched_validation_runner_is_rejected_through_real_session_path_impl() -> Result<()> {
    let fixture = CompletionProofFixture::with_mismatched_validation_contract()?;
    let harness = fixture.harness().await?;
    let call_id = "canonical-with-look-alike-validation-runner";
    let mut responses = vec![exec_command_call_response(
        call_id,
        &fixture.canonical_command,
        &fixture.repo_path,
    )];
    responses.extend(repeated_terminal_candidates(
        TOTAL_GENERATIONS_WITH_FORCED_TERMINAL - 1,
        "a look-alike validation runner must keep this output private",
    ));
    mount_sse_sequence(harness.server(), responses).await;

    let events = submit_and_collect(
        harness.test(),
        "certify with a look-alike validation runner",
    )
    .await?;

    assert_eq!(fixture.canonical_launch_count()?, 1);
    assert!(events.iter().all(|event| !is_assistant_output(event)));
    let completion = events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing TurnComplete")?;
    assert!(completion.error.is_some());
    assert!(completion.last_agent_message.is_none());
    let request_bodies = harness.request_bodies().await;
    assert!(
        request_bodies.iter().any(|body| {
            body.to_string()
                .contains("did not prove a fresh intended nonzero selection and confirmed pass")
        }),
        "the real session path did not report the validation evidence-contract mismatch: \
         {request_bodies:#?}"
    );

    Ok(())
}

#[test]
fn nonignored_live_service_exception_blocks_the_real_terminal_path() -> Result<()> {
    run_session_path_test(
        "nonignored_live_service_exception_blocks_the_real_terminal_path",
        nonignored_live_service_exception_blocks_the_real_terminal_path_impl,
    )
}

async fn nonignored_live_service_exception_blocks_the_real_terminal_path_impl() -> Result<()> {
    let fixture = CompletionProofFixture::new()?;
    fixture.replace_baseline_with_exception("live-service", false, &["remote-service"])?;
    assert_invalid_policy_data_blocks_terminal(
        fixture,
        "certify with a nonignored live-service exception",
        "live-service exception frozen inventory row must have ignored=true",
    )
    .await
}

#[test]
fn active_off_host_exception_blocks_the_real_terminal_path_case_insensitively() -> Result<()> {
    run_session_path_test(
        "active_off_host_exception_blocks_the_real_terminal_path_case_insensitively",
        active_off_host_exception_blocks_the_real_terminal_path_case_insensitively_impl,
    )
}

async fn active_off_host_exception_blocks_the_real_terminal_path_case_insensitively_impl()
-> Result<()> {
    let fixture = CompletionProofFixture::new()?;
    let active_platform = active_inventory_platform_name();
    let uppercase_platform = active_platform.to_ascii_uppercase();
    fixture.replace_baseline_with_exception("off-host", false, &[uppercase_platform.as_str()])?;
    let expected_error = format!(
        "off-host exception frozen inventory row includes active platform {active_platform:?}"
    );
    assert_invalid_policy_data_blocks_terminal(
        fixture,
        "certify with an active-host off-host exception",
        &expected_error,
    )
    .await
}

#[test]
fn empty_platform_pending_exception_blocks_the_real_terminal_path() -> Result<()> {
    run_session_path_test(
        "empty_platform_pending_exception_blocks_the_real_terminal_path",
        empty_platform_pending_exception_blocks_the_real_terminal_path_impl,
    )
}

async fn empty_platform_pending_exception_blocks_the_real_terminal_path_impl() -> Result<()> {
    let fixture = CompletionProofFixture::new()?;
    fixture.replace_baseline_with_exception("platform-pending", false, &[])?;
    assert_invalid_policy_data_blocks_terminal(
        fixture,
        "certify with an empty platform-pending exception",
        "platform-pending exception frozen inventory row must have exact nonempty platform names",
    )
    .await
}

#[test]
fn whitespace_off_host_exception_blocks_the_real_terminal_path() -> Result<()> {
    run_session_path_test(
        "whitespace_off_host_exception_blocks_the_real_terminal_path",
        whitespace_off_host_exception_blocks_the_real_terminal_path_impl,
    )
}

async fn whitespace_off_host_exception_blocks_the_real_terminal_path_impl() -> Result<()> {
    let fixture = CompletionProofFixture::new()?;
    fixture.replace_baseline_with_exception("off-host", false, &[" remote-platform "])?;
    assert_invalid_policy_data_blocks_terminal(
        fixture,
        "certify with an inexact off-host platform",
        "off-host exception frozen inventory row must have exact nonempty platform names",
    )
    .await
}

#[test]
fn unknown_validation_config_field_blocks_the_real_terminal_path() -> Result<()> {
    run_session_path_test(
        "unknown_validation_config_field_blocks_the_real_terminal_path",
        unknown_validation_config_field_blocks_the_real_terminal_path_impl,
    )
}

async fn unknown_validation_config_field_blocks_the_real_terminal_path_impl() -> Result<()> {
    let fixture = CompletionProofFixture::new()?;
    fixture.add_unknown_validation_field()?;
    assert_invalid_policy_data_blocks_terminal(
        fixture,
        "certify with an unrecognized validation field",
        "contains unknown field forged_selection",
    )
    .await
}

#[test]
fn malformed_policy_addition_blocks_the_real_terminal_path() -> Result<()> {
    run_session_path_test(
        "malformed_policy_addition_blocks_the_real_terminal_path",
        malformed_policy_addition_blocks_the_real_terminal_path_impl,
    )
}

async fn malformed_policy_addition_blocks_the_real_terminal_path_impl() -> Result<()> {
    let fixture = CompletionProofFixture::new()?;
    fixture.add_malformed_policy_addition()?;
    assert_invalid_policy_data_blocks_terminal(
        fixture,
        "certify with a malformed post-freeze policy addition",
        "provenance fields did not match the schema",
    )
    .await
}

async fn assert_invalid_policy_data_blocks_terminal(
    fixture: CompletionProofFixture,
    prompt: &str,
    _expected_error: &str,
) -> Result<()> {
    let harness = fixture.harness().await?;
    let call_id = "canonical-with-invalid-policy-data";
    let mut responses = vec![exec_command_call_response(
        call_id,
        &fixture.canonical_command,
        &fixture.repo_path,
    )];
    responses.extend(repeated_terminal_candidates(
        TOTAL_GENERATIONS_WITH_FORCED_TERMINAL - 1,
        "invalid proof policy data must keep this output private",
    ));
    mount_sse_sequence(harness.server(), responses).await;

    let events = submit_and_collect(harness.test(), prompt).await?;

    assert!(events.iter().all(|event| !is_assistant_output(event)));
    assert_eq!(
        fixture.canonical_launch_count()?,
        0,
        "invalid policy data must be rejected before the canonical runner launches"
    );
    let completion = events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing TurnComplete")?;
    let error = completion
        .error
        .as_ref()
        .context("invalid policy data must produce a failed TurnComplete")?;
    assert!(
        error
            .message
            .contains("CompletionProofGate blocked terminal success"),
        "invalid policy data did not fail through the CompletionProofGate terminal path: {}",
        error.message
    );
    assert!(completion.last_agent_message.is_none());
    assert!(completion.surfaced_result.is_none());

    Ok(())
}

#[test]
fn modified_repository_runner_cannot_mint_terminal_proof() -> Result<()> {
    run_session_path_test(
        "modified_repository_runner_cannot_mint_terminal_proof",
        modified_repository_runner_cannot_mint_terminal_proof_impl,
    )
}

async fn modified_repository_runner_cannot_mint_terminal_proof_impl() -> Result<()> {
    let fixture = CompletionProofFixture::new()?;
    fs::write(
        fixture.repo_path.join("proof.py"),
        "raise SystemExit('a mutable look-alike runner must not receive proof authority')\n",
    )?;
    let harness = fixture.harness().await?;
    let call_id = "modified-canonical-runner";
    let mut responses = vec![exec_command_call_response(
        call_id,
        &fixture.canonical_command,
        &fixture.repo_path,
    )];
    responses.extend(repeated_terminal_candidates(
        TOTAL_GENERATIONS_WITH_FORCED_TERMINAL - 1,
        "a mutable runner must not release this answer",
    ));
    mount_sse_sequence(harness.server(), responses).await;

    let events = submit_and_collect(harness.test(), "certify with the modified runner").await?;

    assert!(
        events.iter().all(|event| !is_assistant_output(event)),
        "terminal-looking output escaped after the trusted runner changed"
    );
    assert!(
        !fixture.marker_path.exists(),
        "the modified repository runner launched with proof authority"
    );
    let completion = events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing TurnComplete")?;
    assert!(completion.error.is_some());
    assert!(completion.last_agent_message.is_none());
    let request_bodies = harness.request_bodies().await;
    assert!(
        request_bodies.iter().any(|body| {
            body.to_string()
                .contains("differs from the version explicitly trusted at HEAD")
        }),
        "the real shell path did not report that the trusted runner changed: {request_bodies:#?}"
    );

    Ok(())
}

#[test]
fn modified_compiled_bounded_process_helper_cannot_mint_terminal_proof() -> Result<()> {
    run_session_path_test(
        "modified_compiled_bounded_process_helper_cannot_mint_terminal_proof",
        modified_compiled_bounded_process_helper_cannot_mint_terminal_proof_impl,
    )
}

async fn modified_compiled_bounded_process_helper_cannot_mint_terminal_proof_impl() -> Result<()> {
    let fixture = CompletionProofFixture::new()?;
    let source_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    for relative_path in [
        ".codex/validation/completion-proof.toml",
        ".codex/validation/frozen-test-inventory-v1.json",
        ".codex/validation/test-replacements-v1.json",
        "justfile",
        "scripts/just-shell.py",
        "scripts/rust_tool_env.py",
        "scripts/completion_proof.py",
        "scripts/bounded_process.py",
    ] {
        let destination = fixture.repo_path.join(relative_path);
        fs::create_dir_all(
            destination
                .parent()
                .context("trusted bundle fixture member had no parent")?,
        )?;
        fs::copy(source_root.join(relative_path), destination)?;
    }
    fs::write(
        fixture.repo_path.join("SOURCEMAP.md"),
        "# KD4 fixture marker\n",
    )?;
    fs::write(
        fixture.repo_path.join("scripts/bounded_process.py"),
        "raise SystemExit('a modified supervisor must not receive proof authority')\n",
    )?;

    let harness = fixture.harness().await?;
    mount_sse_sequence(
        harness.server(),
        repeated_terminal_candidates(
            TOTAL_GENERATIONS_WITH_FORCED_TERMINAL,
            "a modified process supervisor must not release this answer",
        ),
    )
    .await;

    let events = submit_and_collect(
        harness.test(),
        "finish with a modified compiled process supervisor",
    )
    .await?;

    assert!(events.iter().all(|event| !is_assistant_output(event)));
    assert_eq!(
        fixture.canonical_launch_count()?,
        0,
        "the canonical runner launched despite a modified trusted supervisor"
    );
    let completion = events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing TurnComplete")?;
    let error = completion
        .error
        .as_ref()
        .context("modified trusted supervisor must produce a failed TurnComplete")?;
    assert!(completion.last_agent_message.is_none());
    assert!(completion.surfaced_result.is_none());
    assert!(
        error.message.contains("scripts\\bounded_process.py")
            || error.message.contains("scripts/bounded_process.py"),
        "the failed TurnComplete did not identify the modified trusted supervisor: {}",
        error.message
    );
    assert!(
        error
            .message
            .contains("differs from the policy compiled into this runtime"),
        "the real terminal path did not reject the modified trusted supervisor: {}",
        error.message
    );

    Ok(())
}

#[test]
fn later_non_documentation_mutation_stales_proof_without_rerunning_certification() -> Result<()> {
    run_session_path_test(
        "later_non_documentation_mutation_stales_proof_without_rerunning_certification",
        later_non_documentation_mutation_stales_proof_without_rerunning_certification_impl,
    )
}

async fn later_non_documentation_mutation_stales_proof_without_rerunning_certification_impl()
-> Result<()> {
    let fixture = CompletionProofFixture::new()?;
    let harness = fixture.harness().await?;
    let canonical_call_id = "canonical-before-later-mutation";
    let mutation_call_id = "later-non-documentation-mutation";
    let mut responses = vec![
        exec_command_call_response(
            canonical_call_id,
            &fixture.canonical_command,
            &fixture.repo_path,
        ),
        exec_command_call_response(
            mutation_call_id,
            &fixture.mutation_command,
            &fixture.repo_path,
        ),
    ];
    responses.extend(repeated_terminal_candidates(
        MAX_REGULAR_LOGICAL_GENERATIONS - 1,
        "stale proof must keep this output private",
    ));
    mount_sse_sequence(harness.server(), responses).await;

    let events = submit_and_collect(
        harness.test(),
        "certify, make one more product change, then finish",
    )
    .await?;

    assert!(events.iter().any(|event| {
        matches!(
            event,
            EventMsg::ExecCommandEnd(end)
                if end.call_id == canonical_call_id && end.exit_code == 0
        )
    }));
    assert!(events.iter().any(|event| {
        matches!(
            event,
            EventMsg::ExecCommandEnd(end)
                if end.call_id == mutation_call_id && end.exit_code == 0
        )
    }));
    assert_eq!(
        fs::read_to_string(fixture.repo_path.join("src/runtime.rs"))?.trim(),
        "pub const VALUE: u8 = 3;"
    );
    assert_eq!(
        fixture.canonical_launch_count()?,
        1,
        "staleness must not cause the gate to rerun certification"
    );
    assert!(events.iter().all(|event| !is_assistant_output(event)));
    let completion = events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing TurnComplete")?;
    let error = completion
        .error
        .as_ref()
        .context("stale proof should fail completion")?;
    assert!(
        error
            .message
            .contains("CompletionProofGate blocked terminal success")
    );
    assert!(error.message.contains(&fixture.canonical_command));
    assert!(completion.last_agent_message.is_none());
    assert_eq!(
        harness.request_bodies().await.len(),
        TOTAL_GENERATIONS_WITH_FORCED_TERMINAL
    );

    Ok(())
}

#[test]
fn eval_artifact_mutation_stales_proof_without_rerunning_certification() -> Result<()> {
    run_session_path_test(
        "eval_artifact_mutation_stales_proof_without_rerunning_certification",
        eval_artifact_mutation_stales_proof_without_rerunning_certification_impl,
    )
}

async fn eval_artifact_mutation_stales_proof_without_rerunning_certification_impl() -> Result<()> {
    let fixture = CompletionProofFixture::new()?;
    let eval_mutation_command = format!("{} mutate_eval.py", available_python_command()?);
    let harness = fixture.harness().await?;
    let canonical_call_id = "canonical-before-eval-mutation";
    let mutation_call_id = "later-eval-artifact-mutation";
    let mut responses = vec![
        exec_command_call_response(
            canonical_call_id,
            &fixture.canonical_command,
            &fixture.repo_path,
        ),
        exec_command_call_response(mutation_call_id, &eval_mutation_command, &fixture.repo_path),
    ];
    responses.extend(repeated_terminal_candidates(
        MAX_REGULAR_LOGICAL_GENERATIONS - 1,
        "eval mutation must keep this output private",
    ));
    mount_sse_sequence(harness.server(), responses).await;

    let events = submit_and_collect(
        harness.test(),
        "certify, write an eval artifact, then finish",
    )
    .await?;

    assert!(events.iter().any(|event| {
        matches!(
            event,
            EventMsg::ExecCommandEnd(end)
                if end.call_id == canonical_call_id && end.exit_code == 0
        )
    }));
    assert!(events.iter().any(|event| {
        matches!(
            event,
            EventMsg::ExecCommandEnd(end)
                if end.call_id == mutation_call_id && end.exit_code == 0
        )
    }));
    assert!(fixture.repo_path.join(".codex/evals/result.json").exists());
    assert_eq!(
        fixture.canonical_launch_count()?,
        1,
        "staleness must not cause the gate to rerun certification"
    );
    assert!(events.iter().all(|event| !is_assistant_output(event)));
    let completion = events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing TurnComplete")?;
    assert!(completion.error.is_some());
    assert!(completion.last_agent_message.is_none());

    Ok(())
}

#[test]
fn workspace_observation_limit_failure_blocks_terminal_success() -> Result<()> {
    run_session_path_test(
        "workspace_observation_limit_failure_blocks_terminal_success",
        workspace_observation_limit_failure_blocks_terminal_success_impl,
    )
}

async fn workspace_observation_limit_failure_blocks_terminal_success_impl() -> Result<()> {
    let fixture = CompletionProofFixture::new()?;
    let overflow = fixture.repo_path.join("observation-overflow");
    fs::create_dir_all(&overflow)?;
    for index in 0..257 {
        fs::write(overflow.join(format!("path-{index:03}.txt")), "changed\n")?;
    }
    let harness = fixture.harness().await?;
    mount_sse_sequence(
        harness.server(),
        repeated_terminal_candidates(
            TOTAL_GENERATIONS_WITH_FORCED_TERMINAL,
            "an unobserved workspace must not publish success",
        ),
    )
    .await;

    let events = submit_and_collect(harness.test(), "finish without observable proof").await?;

    assert!(events.iter().all(|event| !is_assistant_output(event)));
    let completion = events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing TurnComplete")?;
    assert!(completion.error.is_some());
    assert!(completion.last_agent_message.is_none());
    assert_eq!(
        fixture.canonical_launch_count()?,
        0,
        "the gate must not launch certification after an observation error"
    );

    Ok(())
}

#[test]
fn rejected_required_tool_is_a_failed_terminal_without_running_certification() -> Result<()> {
    run_session_path_test(
        "rejected_required_tool_is_a_failed_terminal_without_running_certification",
        rejected_required_tool_is_a_failed_terminal_without_running_certification_impl,
    )
}

async fn rejected_required_tool_is_a_failed_terminal_without_running_certification_impl()
-> Result<()> {
    let fixture = CompletionProofFixture::new()?;
    let harness = fixture.harness().await?;
    let call_id = "rejected-required-shell";
    mount_sse_sequence(
        harness.server(),
        vec![escalated_exec_command_call_response(
            call_id,
            &fixture.rejected_command,
            &fixture.repo_path,
        )?],
    )
    .await;

    let events = submit_escalated_call_deny_and_collect(
        harness.test(),
        "request a required command and stop if it is rejected",
    )
    .await?;

    assert!(events.iter().any(|event| {
        matches!(event, EventMsg::ExecApprovalRequest(approval) if approval.call_id == call_id)
    }));
    assert!(events.iter().all(|event| !is_assistant_output(event)));
    let completion = events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing TurnComplete")?;
    let error = completion
        .error
        .as_ref()
        .context("rejected required tool must produce a failed TurnComplete")?;
    assert!(
        error
            .message
            .contains("required tool `exec_command` blocked"),
        "unexpected required-tool terminal error: {}",
        error.message
    );
    assert!(
        !fixture.rejected_command_marker_path.exists(),
        "the rejected command unexpectedly executed"
    );
    assert!(
        !fixture.marker_path.exists(),
        "required-tool rejection caused canonical certification to launch"
    );
    assert_eq!(harness.request_bodies().await.len(), 1);

    Ok(())
}

#[test]
fn rejected_terminal_candidate_is_not_published_by_later_required_tool_failure() -> Result<()> {
    run_session_path_test(
        "rejected_terminal_candidate_is_not_published_by_later_required_tool_failure",
        rejected_terminal_candidate_is_not_published_by_later_required_tool_failure_impl,
    )
}

async fn rejected_terminal_candidate_is_not_published_by_later_required_tool_failure_impl()
-> Result<()> {
    const REJECTED_TERMINAL_TEXT: &str =
        "rejected terminal candidate must not survive a later required-tool failure";

    let fixture = CompletionProofFixture::new()?;
    let cwd = fixture.repo_path.abs();
    let harness = TestCodexHarness::with_builder(
        test_codex()
            .with_extensions(required_tool_failure_extensions())
            .with_config(move |config| set_fixture_workspace(config, cwd)),
    )
    .await?;
    let call_id = "registered-required-failure-after-rejected-terminal-candidate";
    mount_sse_sequence(
        harness.server(),
        vec![
            terminal_candidate(0, REJECTED_TERMINAL_TEXT),
            sse(vec![
                ev_response_created("registered-required-failure-response"),
                ev_function_call(call_id, REQUIRED_FAILURE_TOOL_NAME, "{}"),
                ev_completed("registered-required-failure-response"),
            ]),
        ],
    )
    .await;

    let events = submit_and_collect(
        harness.test(),
        "try to finish, then stop when the registered required operation is rejected",
    )
    .await?;

    assert!(
        events
            .iter()
            .all(|event| !assistant_output_contains(event, REJECTED_TERMINAL_TEXT)),
        "the rejected terminal candidate escaped through the required-tool failure: {events:#?}"
    );
    let completion = events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing TurnComplete")?;
    let error = completion
        .error
        .as_ref()
        .context("rejected required tool must produce a failed TurnComplete")?;
    assert!(
        error
            .message
            .contains("required tool `completion_proof_required_failure` blocked"),
        "unexpected required-tool terminal error: {}",
        error.message
    );
    assert_eq!(
        completion.last_agent_message.as_deref(),
        Some(error.message.as_str())
    );
    assert!(
        !completion
            .last_agent_message
            .as_deref()
            .is_some_and(|message| message.contains(REJECTED_TERMINAL_TEXT)),
        "the rejected terminal candidate survived as completion metadata: {completion:#?}"
    );
    assert!(completion.surfaced_result.is_none());
    assert_eq!(fixture.canonical_launch_count()?, 0);
    let request_bodies = harness.request_bodies().await;
    assert_eq!(request_bodies.len(), 2);
    assert!(
        !request_bodies[1]
            .to_string()
            .contains(REJECTED_TERMINAL_TEXT),
        "the rejected terminal candidate was persisted into the next real provider request: {:#?}",
        request_bodies[1]
    );

    Ok(())
}

#[test]
fn failed_after_agent_hook_discards_terminal_candidate_metadata() -> Result<()> {
    run_session_path_test(
        "failed_after_agent_hook_discards_terminal_candidate_metadata",
        failed_after_agent_hook_discards_terminal_candidate_metadata_impl,
    )
}

async fn failed_after_agent_hook_discards_terminal_candidate_metadata_impl() -> Result<()> {
    const REJECTED_TERMINAL_TEXT: &str =
        "failed after-agent hook must keep this terminal candidate private";

    let fixture = CompletionProofFixture::new()?;
    let cwd = fixture.repo_path.abs();
    let harness = TestCodexHarness::with_builder(test_codex().with_config(move |config| {
        set_fixture_workspace(config, cwd);
        config.after_agent_policy = AfterAgentPolicy::MutatingFinalizer;
        config.notify = Some(failing_after_agent_command());
    }))
    .await?;
    mount_sse_sequence(
        harness.server(),
        vec![terminal_candidate(0, REJECTED_TERMINAL_TEXT)],
    )
    .await;

    let events = submit_and_collect(
        harness.test(),
        "try to finish, then obey the configured after-agent hook",
    )
    .await?;

    assert!(
        events
            .iter()
            .all(|event| !assistant_output_contains(event, REJECTED_TERMINAL_TEXT)),
        "the failed after-agent hook released rejected assistant output: {events:#?}"
    );
    assert!(
        events.iter().any(|event| {
            matches!(event, EventMsg::Error(error) if error.message.contains("after_agent hook 'legacy_notify' failed and aborted turn completion"))
        }),
        "the real mutating after-agent hook failure was not observed: {events:#?}"
    );
    let completion = events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .context("missing TurnComplete")?;
    let error = completion
        .error
        .as_ref()
        .context("failed after-agent hook must produce a failed TurnComplete")?;
    assert!(
        error
            .message
            .contains("after_agent hook 'legacy_notify' failed and aborted turn completion"),
        "unexpected after-agent terminal error: {}",
        error.message
    );
    assert!(completion.last_agent_message.is_none(), "{completion:#?}");
    assert!(completion.surfaced_result.is_none(), "{completion:#?}");
    assert_eq!(fixture.canonical_launch_count()?, 0);
    assert_eq!(harness.request_bodies().await.len(), 1);

    Ok(())
}
