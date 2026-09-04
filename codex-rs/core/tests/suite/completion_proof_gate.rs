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

fn active_inventory_platform_name() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    }
}

fn set_fixture_workspace(config: &mut Config, cwd: AbsolutePathBuf) {
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

struct CompletionProofFixture {
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
        TestCodexHarness::with_builder(test_codex().with_config(move |config| {
            set_fixture_workspace(config, cwd);
        }))
        .await
    }

    async fn harness_with_raw_response_items(&self) -> Result<TestCodexHarness> {
        let cwd = self.repo_path.abs();
        TestCodexHarness::with_builder(
            test_codex()
                .with_raw_response_items()
                .with_config(move |config| set_fixture_workspace(config, cwd)),
        )
        .await
    }

    async fn harness_with_home(&self, home: Arc<TempDir>) -> Result<TestCodexHarness> {
        let cwd = self.repo_path.abs();
        TestCodexHarness::with_builder(
            test_codex()
                .with_home(home)
                .with_config(move |config| set_fixture_workspace(config, cwd)),
        )
        .await
    }

    async fn harness_with_home_and_raw_response_items(
        &self,
        home: Arc<TempDir>,
    ) -> Result<TestCodexHarness> {
        let cwd = self.repo_path.abs();
        TestCodexHarness::with_builder(
            test_codex()
                .with_home(home)
                .with_raw_response_items()
                .with_config(move |config| set_fixture_workspace(config, cwd)),
        )
        .await
    }

    async fn multi_agent_harness(&self) -> Result<TestCodexHarness> {
        let cwd = self.repo_path.abs();
        TestCodexHarness::with_builder(test_codex().with_config(move |config| {
            set_fixture_workspace(config, cwd);
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
            .context("timed out waiting for completion-proof session event")??;
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
    let public_prompt = "certify through public admission with non-root display metadata";
    let contributor_prompt = "attempt certification through the memory-consolidation admission";
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
    let contributor_thread = thread_manager
        .start_evidence_contributor_thread_with_options(contributor_options)
        .await?
        .thread;
    contributor_thread.request_raw_response_items();
    let contributor_events =
        submit_and_collect_thread(&contributor_thread, &config, contributor_prompt)
            .await
            .context("evidence-contributor admission did not reach TurnComplete")?;
    let contributor_output = function_call_output(&contributor_events, contributor_call_id)
        .context("missing contributor canonical-command output in the model continuation")?;
    assert!(
        contributor_output.contains("only the root Codex session may own and register"),
        "explicit contributor admission did not reject terminal certification: {contributor_output}"
    );
    assert_eq!(
        fixture.canonical_launch_count()?,
        1,
        "contributor rejection launched canonical certification"
    );
    let contributor_rollout = contributor_thread
        .rollout_path()
        .context("missing contributor rollout for public live-resume regression")?;
    let public_resume_error = match thread_manager
        .resume_thread_from_rollout(
            config.clone(),
            contributor_rollout,
            thread_manager.auth_manager(),
            /*parent_trace*/ None,
            /*supports_openai_form_elicitation*/ false,
        )
        .await
    {
        Ok(_) => anyhow::bail!(
            "public resume reused a live evidence-contributor thread instead of consuming root authority"
        ),
        Err(err) => err,
    };
    assert!(
        public_resume_error
            .to_string()
            .contains("different completion-proof authority"),
        "unexpected public live-resume error: {public_resume_error}"
    );
    contributor_thread.shutdown_and_wait().await?;

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

async fn successful_focused_validation_does_not_satisfy_terminal_gate_impl() -> Result<()> {
    let fixture = CompletionProofFixture::new()?;
    let harness = fixture.harness().await?;
    let call_id = "trusted-focused-pass";
    let mut responses = vec![exec_command_call_response(
        call_id,
        &fixture.exact_focused_command(),
        &fixture.repo_path,
    )];
    responses.extend(repeated_terminal_candidates(
        MAX_REGULAR_LOGICAL_GENERATIONS,
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
        !fixture.marker_path.exists(),
        "a focused pass caused the canonical command to launch"
    );

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
        "[[owner]]\nid = \"fixture\"\n[[owner.symbols]]\nsymbol = \"owned_symbol\"\npath = \"src/evidence.rs\"\n",
    )?;
    fs::write(
        fixture.repo_path.join("scripts/source_map_check.py"),
        "# fixture source-map validator\n",
    )?;
    fs::write(
        fixture.repo_path.join("scripts/source_owners.py"),
        "# fixture owner resolver\n",
    )?;
    fs::write(fixture.repo_path.join("justfile"), "source-map-check:\n")?;
    fs::write(
        fixture.repo_path.join("src/evidence.rs"),
        "pub fn owned_symbol() {}\n",
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
            "justfile",
        ],
        &["**"],
        &["source_owners.toml"],
        mutation_script,
    )?;
    Ok((fixture, command))
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
fn source_map_projection_tracks_owned_and_manifest_evidence_inputs_through_real_session_path()
-> Result<()> {
    run_session_path_test(
        "source_map_projection_tracks_owned_and_manifest_evidence_inputs_through_real_session_path",
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
            .await
        },
    )
}

#[test]
fn source_map_projection_tracks_topology_add_and_remove_through_real_session_path() -> Result<()> {
    run_session_path_test(
        "source_map_projection_tracks_topology_add_and_remove_through_real_session_path",
        || async {
            let (add_fixture, add_command) = source_map_projection_fixture(
                "from pathlib import Path\nPath('src/topology_added.rs').write_text('pub fn added() {}\\n', encoding='utf-8')\n",
            )?;
            assert_projection_mutation_outcome(
                &add_fixture,
                &add_command,
                true,
                "source-map-topology-add",
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
    let mut responses = vec![exec_command_call_response(
        canonical_call_id,
        &fixture.canonical_command,
        &fixture.repo_path,
    )];
    responses.extend(repeated_terminal_candidates(
        TOTAL_GENERATIONS_WITH_FORCED_TERMINAL - 1,
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
    assert!(
        request_bodies.iter().any(|body| {
            body.to_string()
                .contains("restoring the ending bytes does not make that attempt valid")
        }),
        "the real shell path did not report the transient mutation: {request_bodies:#?}"
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
    .await?;

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
