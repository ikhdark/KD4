use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use codex_features::Feature;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::WebSearchMode;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ImageDetail;
use codex_protocol::models::PermissionProfile;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::InputModality;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::TaskCompletionGate;
use codex_protocol::protocol::TaskCompletionStatus;
use codex_protocol::user_input::UserInput;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::approx_token_count;
use codex_utils_output_truncation::truncate_text;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use crate::agent::role::apply_role_to_config;
use crate::codex_delegate::run_codex_thread_one_shot;
use crate::compact::COMPACT_IMAGE_OMISSION_MARKER;
use crate::config::Config;
use crate::config::Constrained;
use crate::context::CompletionReviewRepair;
use crate::context::ContextualUserFragment;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;

const REVIEW_DEADLINE: Duration = Duration::from_secs(90);
const REVIEW_CLEANUP_DEADLINE: Duration = Duration::from_secs(5);
const MAX_FALLBACK_USER_ENTRIES: usize = 40;
const MAX_USER_ENTRY_TOKENS: usize = 2_000;
const MAX_USER_HISTORY_TOKENS: usize = 6_000;
const MAX_TASK_SUMMARY_TOKENS: usize = 2_000;
const MAX_EVIDENCE_BLOCK_TOKENS: usize = 8_000;
const MAX_RENDERED_REQUEST_TOKENS: usize = 8_999;
const MAX_REVIEW_OUTPUT_TOKENS: usize = 2_000;
const MAX_REVIEW_FINDINGS: usize = 8;
const MAX_REPAIR_PAYLOAD_TOKENS: usize = 900;
const REPAIR_ENVELOPE_RESERVE_TOKENS: usize = 100;
const MAX_RENDERED_REPAIR_TOKENS: usize = 999;
const MAX_REVIEW_USER_IMAGES: usize = crate::compact::MAX_RETAINED_USER_IMAGES;
const MAX_REVIEW_USER_IMAGE_BYTES: usize = crate::compact::MAX_RETAINED_USER_IMAGE_BYTES;

const REVIEWER_BASE_INSTRUCTIONS: &str = r#"You are the independent KD4 completion reviewer. Work read-only. Review the closed request against the repository and decide whether the candidate completion fully satisfies every applicable user requirement. The declarative evidence gate is useful but not authoritative for requirement coverage. Report only concrete, task-relevant gaps; do not propose broad cleanup, redesign, or new scope. Return only the required structured JSON."#;

const REVIEW_REQUEST_PREFIX: &str = r#"KD4_COMPLETION_REVIEW_REQUEST_V1

Independently verify completeness. A `clean` verdict means the candidate satisfies all supplied user requirements and the repository evidence reveals no task-relevant omission. A `repair_needed` verdict must list findings in critical-to-low severity order. Each finding must identify the smallest correction and one focused proof command. Do not treat reviewer infrastructure or uncertainty as a task-state blocker.

<review_evidence>
"#;
const REVIEW_REQUEST_SUFFIX: &str = "\n</review_evidence>";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum CompletionReviewPhase {
    #[default]
    NotReviewed,
    Reviewed,
    RepairStarted,
}

#[derive(Debug, Default)]
pub(crate) struct CompletionReviewState {
    phase: CompletionReviewPhase,
}

#[derive(Debug, Default)]
pub(crate) struct CompletionReviewCoordinatorOutcome {
    pub(crate) repair_injected: bool,
    pub(crate) advisory: Option<String>,
    pub(crate) partial_reasons: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReviewFailureCategory {
    Timeout,
    Capacity,
    SpawnModel,
    MalformedOutput,
    OversizedOutput,
    Cleanup,
    RepairInjection,
    InputImagesOmitted,
}

impl ReviewFailureCategory {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::Capacity => "capacity",
            Self::SpawnModel => "spawn_model",
            Self::MalformedOutput => "malformed_output",
            Self::OversizedOutput => "oversized_output",
            Self::Cleanup => "cleanup",
            Self::RepairInjection => "repair_injection",
            Self::InputImagesOmitted => "input_images_omitted",
        }
    }

    const fn partial_reason(self) -> &'static str {
        match self {
            Self::Timeout => "completion reviewer timed out",
            Self::Capacity => "completion reviewer private capacity was unavailable",
            Self::SpawnModel => "completion reviewer could not start or complete",
            Self::MalformedOutput => "completion reviewer returned malformed structured output",
            Self::OversizedOutput => "completion reviewer output exceeded the 2,000-token limit",
            Self::Cleanup => "completion reviewer cleanup did not complete",
            Self::RepairInjection => "completion repair instruction could not be injected",
            Self::InputImagesOmitted => {
                "completion reviewer could not inspect every user-supplied image requirement"
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ReviewImage {
    image_url: String,
    detail: Option<ImageDetail>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ReviewUserEntry {
    text: String,
    images: Vec<ReviewImage>,
}

#[derive(Debug)]
struct CompletionReviewRequest {
    inputs: Vec<UserInput>,
    failures: Vec<ReviewFailureCategory>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ReviewVerdict {
    Clean,
    RepairNeeded,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum FindingSeverity {
    Critical,
    High,
    Medium,
    Low,
}

impl FindingSeverity {
    const fn rank(self) -> u8 {
        match self {
            Self::Critical => 0,
            Self::High => 1,
            Self::Medium => 2,
            Self::Low => 3,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Critical => "critical",
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct CompletionReviewFinding {
    severity: FindingSeverity,
    summary: String,
    evidence: String,
    smallest_correction: String,
    proof_command: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct CompletionReviewOutput {
    verdict: ReviewVerdict,
    findings: Vec<CompletionReviewFinding>,
}

#[derive(Debug)]
struct ReviewerExecution {
    output: Option<CompletionReviewOutput>,
    failures: Vec<ReviewFailureCategory>,
}

impl ReviewerExecution {
    fn failed(category: ReviewFailureCategory) -> Self {
        Self {
            output: None,
            failures: vec![category],
        }
    }
}

pub(crate) async fn coordinate_completion_review(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    cancellation_token: &CancellationToken,
    initial_host_mutation_revision: Option<u64>,
    candidate_completion: Option<&str>,
    state: &mut CompletionReviewState,
) -> CodexResult<CompletionReviewCoordinatorOutcome> {
    if cancellation_token.is_cancelled() {
        return Err(CodexErr::TurnAborted);
    }
    if state.phase != CompletionReviewPhase::NotReviewed
        || turn_context.session_source.is_non_root_agent()
        || turn_context.collaboration_mode.mode != ModeKind::Default
        || turn_context.final_output_json_schema.is_some()
        || !sess.services.task_evidence.allows_kd4_completion()
    {
        return Ok(CompletionReviewCoordinatorOutcome::default());
    }
    let Some(initial_revision) = initial_host_mutation_revision else {
        return Ok(CompletionReviewCoordinatorOutcome::default());
    };
    let Some(current_revision) = sess.services.task_evidence.host_mutation_revision().await else {
        return Ok(CompletionReviewCoordinatorOutcome::default());
    };
    if current_revision <= initial_revision {
        return Ok(CompletionReviewCoordinatorOutcome {
            advisory: sess.services.task_evidence.finalization_advisory().await,
            ..Default::default()
        });
    }

    if !turn_context
        .config
        .features
        .enabled(Feature::TaskCompletionReviewer)
    {
        return Ok(CompletionReviewCoordinatorOutcome {
            advisory: sess.services.task_evidence.finalization_advisory().await,
            ..Default::default()
        });
    }

    state.phase = CompletionReviewPhase::Reviewed;
    let gate = sess.services.task_evidence.completion_gate().await;
    let evidence_summary = match gate.as_ref() {
        Some(gate) => {
            sess.services
                .task_evidence
                .completion_review_evidence_summary(gate)
                .await
        }
        None => "Evidence gate: unavailable".to_string(),
    };
    let CompletionReviewRequest {
        inputs,
        failures: request_failures,
    } = build_review_request(sess, turn_context, &evidence_summary, candidate_completion).await;

    let mut execution = match sess.try_acquire_completion_review_slot() {
        Some(_permit) => {
            run_reviewer_with_deadline(sess, turn_context, inputs, cancellation_token).await?
        }
        None => ReviewerExecution::failed(ReviewFailureCategory::Capacity),
    };
    for failure in request_failures {
        push_failure(&mut execution.failures, failure);
    }
    let gate_needs_repair = gate
        .as_ref()
        .is_some_and(|gate| gate.status != TaskCompletionStatus::Passed);
    let reviewer_needs_repair = execution
        .output
        .as_ref()
        .is_some_and(|output| output.verdict == ReviewVerdict::RepairNeeded);
    let should_repair = gate_needs_repair || reviewer_needs_repair;

    let mut repair_injected = false;
    if should_repair {
        state.phase = CompletionReviewPhase::RepairStarted;
        match build_repair_item(execution.output.as_ref(), gate.as_ref()) {
            Some(item) => {
                sess.record_response_item_and_emit_turn_item(turn_context, item)
                    .await;
                repair_injected = true;
            }
            None => push_failure(
                &mut execution.failures,
                ReviewFailureCategory::RepairInjection,
            ),
        }
    }

    let finding_summary = execution
        .output
        .as_ref()
        .map(|output| {
            output
                .findings
                .iter()
                .map(|finding| {
                    format!("[{}] {}", finding.severity.as_str(), finding.summary.trim())
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let audit_outcome = execution
        .output
        .as_ref()
        .map_or("infrastructure_failure", |output| match output.verdict {
            ReviewVerdict::Clean => "clean",
            ReviewVerdict::RepairNeeded => "repair_needed",
        });
    let failure_category = execution.failures.first().copied();
    if !sess
        .services
        .task_evidence
        .record_completion_review_audit(
            &turn_context.sub_id,
            audit_outcome,
            failure_category.map(ReviewFailureCategory::as_str),
            finding_summary,
            repair_injected,
        )
        .await
    {
        push_failure(&mut execution.failures, ReviewFailureCategory::Cleanup);
    }

    let partial_reasons = execution
        .failures
        .iter()
        .map(|failure| failure.partial_reason().to_string())
        .collect();
    Ok(CompletionReviewCoordinatorOutcome {
        repair_injected,
        advisory: None,
        partial_reasons,
    })
}

fn push_failure(failures: &mut Vec<ReviewFailureCategory>, failure: ReviewFailureCategory) {
    if !failures.contains(&failure) {
        failures.push(failure);
    }
}

async fn run_reviewer_with_deadline(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    inputs: Vec<UserInput>,
    parent_cancellation: &CancellationToken,
) -> CodexResult<ReviewerExecution> {
    let review_cancellation = CancellationToken::new();
    let mut run = Box::pin(run_reviewer_once(
        Arc::clone(sess),
        Arc::clone(turn_context),
        inputs,
        review_cancellation.clone(),
    ));
    tokio::select! {
        biased;
        _ = parent_cancellation.cancelled() => {
            review_cancellation.cancel();
            let _ = timeout(REVIEW_CLEANUP_DEADLINE, &mut run).await;
            Err(CodexErr::TurnAborted)
        }
        result = &mut run => Ok(result),
        _ = tokio::time::sleep(REVIEW_DEADLINE) => {
            review_cancellation.cancel();
            let mut execution = ReviewerExecution::failed(ReviewFailureCategory::Timeout);
            if timeout(REVIEW_CLEANUP_DEADLINE, &mut run).await.is_err() {
                push_failure(&mut execution.failures, ReviewFailureCategory::Cleanup);
            }
            Ok(execution)
        }
    }
}

async fn run_reviewer_once(
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
    inputs: Vec<UserInput>,
    cancellation_token: CancellationToken,
) -> ReviewerExecution {
    let requires_images = inputs
        .iter()
        .any(|input| matches!(input, UserInput::Image { .. }));
    let subconfig = match build_reviewer_config(turn_context.as_ref(), requires_images).await {
        Ok(config) => config,
        Err(()) => return ReviewerExecution::failed(ReviewFailureCategory::SpawnModel),
    };
    let io = match run_codex_thread_one_shot(
        subconfig,
        Arc::clone(&sess.services.auth_manager),
        Arc::clone(&sess.services.models_manager),
        inputs,
        Arc::clone(&sess),
        Arc::clone(&turn_context),
        cancellation_token,
        SubAgentSource::Review,
        Some(completion_review_output_schema()),
        /*initial_history*/ None,
    )
    .await
    {
        Ok(io) => io,
        Err(_) => return ReviewerExecution::failed(ReviewFailureCategory::SpawnModel),
    };
    let termination = io.session_loop_termination.clone();
    let mut reviewer_turn_id = None;
    let raw_output = loop {
        let event = match io.next_event().await {
            Ok(event) => event,
            Err(_) => {
                termination.await;
                return ReviewerExecution::failed(ReviewFailureCategory::SpawnModel);
            }
        };
        match event.msg {
            EventMsg::TurnStarted(started) => {
                reviewer_turn_id.get_or_insert(started.turn_id);
            }
            EventMsg::TurnComplete(completed)
                if reviewer_turn_id.as_deref() == Some(completed.turn_id.as_str()) =>
            {
                break completed.last_agent_message;
            }
            EventMsg::TurnAborted(aborted)
                if reviewer_turn_id.as_deref() == aborted.turn_id.as_deref() =>
            {
                termination.await;
                return ReviewerExecution::failed(ReviewFailureCategory::SpawnModel);
            }
            _ => {}
        }
    };
    termination.await;
    let Some(raw_output) = raw_output else {
        return ReviewerExecution::failed(ReviewFailureCategory::MalformedOutput);
    };
    if approx_token_count(&raw_output) > MAX_REVIEW_OUTPUT_TOKENS {
        return ReviewerExecution::failed(ReviewFailureCategory::OversizedOutput);
    }
    match parse_review_output(&raw_output) {
        Some(output) => ReviewerExecution {
            output: Some(output),
            failures: Vec::new(),
        },
        None => ReviewerExecution::failed(ReviewFailureCategory::MalformedOutput),
    }
}

async fn build_reviewer_config(
    turn_context: &TurnContext,
    requires_images: bool,
) -> Result<Config, ()> {
    let mut config = turn_context.config.as_ref().clone();
    let inherited_model_provider = config.model_provider.clone();
    apply_role_to_config(&mut config, Some("kd4_reviewer"))
        .await
        .map_err(|_| ())?;
    config.model_provider = inherited_model_provider;
    if requires_images {
        config.model = Some(turn_context.model_info.slug.clone());
    }

    config.ephemeral = true;
    config.notify = None;
    config.base_instructions = Some(REVIEWER_BASE_INSTRUCTIONS.to_string());
    config.developer_instructions = None;
    config.include_permissions_instructions = false;
    config.include_apps_instructions = false;
    config.include_collaboration_mode_instructions = false;
    config.include_skill_instructions = false;
    config.include_environment_context = false;
    config.orchestrator_skills_enabled = false;
    config.orchestrator_mcp_enabled = false;
    config.memories.use_memories = false;
    config.memories.dedicated_tools = false;
    config.permissions.approval_policy = Constrained::allow_only(AskForApproval::Never);
    config
        .permissions
        .set_permission_profile(PermissionProfile::read_only())
        .map_err(|_| ())?;
    config
        .web_search_mode
        .set(WebSearchMode::Disabled)
        .map_err(|_| ())?;
    config.mcp_servers.set(HashMap::new()).map_err(|_| ())?;
    for feature in [
        Feature::SpawnCsv,
        Feature::Collab,
        Feature::MultiAgentV2,
        Feature::Apps,
        Feature::EnableMcpApps,
        Feature::Plugins,
        Feature::WebSearchRequest,
        Feature::WebSearchCached,
        Feature::CodeMode,
        Feature::CodeModeHost,
        Feature::CodeModeOnly,
        Feature::CodexHooks,
    ] {
        config.features.disable(feature).map_err(|_| ())?;
        if config.features.enabled(feature) {
            return Err(());
        }
    }
    Ok(config)
}

fn completion_review_output_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "verdict": {
                "type": "string",
                "enum": ["clean", "repair_needed"]
            },
            "findings": {
                "type": "array",
                "maxItems": MAX_REVIEW_FINDINGS,
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "severity": {
                            "type": "string",
                            "enum": ["critical", "high", "medium", "low"]
                        },
                        "summary": { "type": "string" },
                        "evidence": { "type": "string" },
                        "smallest_correction": { "type": "string" },
                        "proof_command": { "type": "string" }
                    },
                    "required": [
                        "severity",
                        "summary",
                        "evidence",
                        "smallest_correction",
                        "proof_command"
                    ]
                }
            }
        },
        "required": ["verdict", "findings"]
    })
}

fn parse_review_output(raw: &str) -> Option<CompletionReviewOutput> {
    let output: CompletionReviewOutput = serde_json::from_str(raw).ok()?;
    if output.findings.len() > MAX_REVIEW_FINDINGS
        || output.findings.iter().any(|finding| {
            finding.summary.trim().is_empty()
                || finding.evidence.trim().is_empty()
                || finding.smallest_correction.trim().is_empty()
                || finding.proof_command.trim().is_empty()
        })
        || output
            .findings
            .windows(2)
            .any(|pair| pair[0].severity.rank() > pair[1].severity.rank())
        || (output.verdict == ReviewVerdict::Clean && !output.findings.is_empty())
        || (output.verdict == ReviewVerdict::RepairNeeded && output.findings.is_empty())
    {
        return None;
    }
    Some(output)
}

async fn build_review_request(
    sess: &Session,
    turn_context: &TurnContext,
    evidence_summary: &str,
    candidate_completion: Option<&str>,
) -> CompletionReviewRequest {
    let history = sess.clone_history().await;
    let boundary = sess.last_passed_root_completion_turn_id().await;
    let user_entries = extract_user_entries(history.raw_items(), boundary.as_deref());
    let user_history = render_bounded_user_history(
        user_entries
            .iter()
            .map(review_entry_text)
            .collect::<Vec<_>>(),
    );
    let task_summary = fit_serialized_text(
        &format!(
            "Repository root: {}\n{}\nCandidate completion:\n{}",
            turn_context.config.cwd.as_path().display(),
            evidence_summary,
            candidate_completion.unwrap_or("<none>")
        ),
        MAX_TASK_SUMMARY_TOKENS,
    );
    let evidence_block = fit_serialized_text(
        &format!(
            "User-only requirements:\n{}\n\nTask evidence and candidate:\n{}",
            user_history, task_summary
        ),
        MAX_EVIDENCE_BLOCK_TOKENS,
    );
    let request = fit_request(evidence_block);
    build_review_inputs(
        request,
        &user_entries,
        turn_context
            .model_info
            .input_modalities
            .contains(&InputModality::Image),
    )
}

fn extract_user_entries(
    items: &[ResponseItem],
    boundary_turn_id: Option<&str>,
) -> Vec<ReviewUserEntry> {
    let boundary_index = boundary_turn_id.and_then(|turn_id| {
        items
            .iter()
            .rposition(|item| item.turn_id() == Some(turn_id))
    });
    let start = boundary_index.map_or(0, |index| index.saturating_add(1));
    let mut entries = items[start..]
        .iter()
        .filter_map(|item| {
            let Some(TurnItem::UserMessage(user)) = crate::event_mapping::parse_turn_item(item)
            else {
                return None;
            };
            let mut text = Vec::new();
            let mut images = Vec::new();
            for input in user.content {
                match input {
                    UserInput::Text { text: input, .. } if !input.trim().is_empty() => {
                        text.push(input);
                    }
                    UserInput::Image { image_url, detail } => {
                        images.push(ReviewImage { image_url, detail });
                    }
                    _ => {}
                }
            }
            (!text.is_empty() || !images.is_empty()).then(|| ReviewUserEntry {
                text: text.join("\n"),
                images,
            })
        })
        .collect::<Vec<_>>();
    if boundary_index.is_none() && entries.len() > MAX_FALLBACK_USER_ENTRIES {
        entries.drain(..entries.len() - MAX_FALLBACK_USER_ENTRIES);
    }
    entries
}

fn review_entry_text(entry: &ReviewUserEntry) -> String {
    if entry.text.trim().is_empty() && !entry.images.is_empty() {
        "<image-only user requirement; inspect the attached review image>".to_string()
    } else {
        fit_serialized_text(&entry.text, MAX_USER_ENTRY_TOKENS)
    }
}

fn build_review_inputs(
    request: String,
    entries: &[ReviewUserEntry],
    supports_images: bool,
) -> CompletionReviewRequest {
    build_review_inputs_with_limits(
        request,
        entries,
        supports_images,
        MAX_REVIEW_USER_IMAGES,
        MAX_REVIEW_USER_IMAGE_BYTES,
    )
}

fn build_review_inputs_with_limits(
    request: String,
    entries: &[ReviewUserEntry],
    supports_images: bool,
    max_images: usize,
    max_image_bytes: usize,
) -> CompletionReviewRequest {
    let mut inputs = vec![UserInput::Text {
        text: request,
        text_elements: Vec::new(),
    }];
    let mut failures = Vec::new();
    if entries
        .iter()
        .any(|entry| entry.text.contains(COMPACT_IMAGE_OMISSION_MARKER))
    {
        push_failure(&mut failures, ReviewFailureCategory::InputImagesOmitted);
    }

    let mut retained_count = 0usize;
    let mut retained_bytes = 0usize;
    let mut review_image_number = 0usize;
    for (entry_index, entry) in entries.iter().enumerate() {
        for image in &entry.images {
            if !supports_images {
                push_failure(&mut failures, ReviewFailureCategory::InputImagesOmitted);
                continue;
            }
            let next_bytes = retained_bytes.saturating_add(image.image_url.len());
            if retained_count >= max_images || next_bytes > max_image_bytes {
                push_failure(&mut failures, ReviewFailureCategory::InputImagesOmitted);
                continue;
            }
            retained_count = retained_count.saturating_add(1);
            retained_bytes = next_bytes;
            review_image_number = review_image_number.saturating_add(1);
            inputs.push(UserInput::Text {
                text: format!(
                    "Review image {review_image_number} for user requirement entry {}.",
                    entry_index + 1
                ),
                text_elements: Vec::new(),
            });
            inputs.push(UserInput::Image {
                image_url: image.image_url.clone(),
                detail: image.detail,
            });
        }
    }

    CompletionReviewRequest { inputs, failures }
}

fn render_bounded_user_history(entries: Vec<String>) -> String {
    if entries.is_empty() {
        return "<none>".to_string();
    }
    let mut selected = BTreeMap::<usize, String>::new();
    insert_history_entry(&mut selected, 0, &entries[0]);
    let latest = entries.len() - 1;
    if latest != 0 {
        insert_history_entry(&mut selected, latest, &entries[latest]);
    }
    for index in (1..latest).rev() {
        insert_history_entry(&mut selected, index, &entries[index]);
    }
    render_history_entries(&selected)
}

fn insert_history_entry(selected: &mut BTreeMap<usize, String>, index: usize, text: &str) {
    if selected.contains_key(&index) {
        return;
    }
    let text = fit_history_entry(index, text);
    if text.is_empty() {
        return;
    }
    selected.insert(index, text.clone());
    if serialized_text_tokens(&render_history_entries(selected)) <= MAX_USER_HISTORY_TOKENS {
        return;
    }
    selected.remove(&index);

    let mut low = 1;
    let mut high = approx_token_count(&text).max(1);
    let mut best = None;
    while low <= high {
        let middle = low + (high - low) / 2;
        let candidate = truncate_text(&text, TruncationPolicy::Tokens(middle));
        selected.insert(index, candidate.clone());
        if serialized_text_tokens(&render_history_entries(selected)) <= MAX_USER_HISTORY_TOKENS {
            best = Some(candidate);
            low = middle.saturating_add(1);
        } else {
            high = middle.saturating_sub(1);
        }
        selected.remove(&index);
    }
    if let Some(best) = best.filter(|text| !text.trim().is_empty()) {
        selected.insert(index, best);
    }
}

fn fit_history_entry(index: usize, text: &str) -> String {
    if serialized_text_tokens(&render_history_entry(index, text)) <= MAX_USER_ENTRY_TOKENS {
        return text.to_string();
    }
    let mut low = 1;
    let mut high = approx_token_count(text).max(1);
    let mut best = String::new();
    while low <= high {
        let middle = low + (high - low) / 2;
        let candidate = truncate_text(text, TruncationPolicy::Tokens(middle));
        if serialized_text_tokens(&render_history_entry(index, &candidate)) <= MAX_USER_ENTRY_TOKENS
        {
            best = candidate;
            low = middle.saturating_add(1);
        } else {
            high = middle.saturating_sub(1);
        }
    }
    best
}

fn render_history_entry(index: usize, text: &str) -> String {
    format!(
        "User requirement entry {} (JSON string): {}",
        index + 1,
        serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_string())
    )
}

fn render_history_entries(entries: &BTreeMap<usize, String>) -> String {
    entries
        .iter()
        .map(|(index, text)| render_history_entry(*index, text))
        .collect::<Vec<_>>()
        .join("\n")
}

fn fit_request(mut evidence_block: String) -> String {
    evidence_block = fit_serialized_text(&evidence_block, MAX_EVIDENCE_BLOCK_TOKENS);
    loop {
        let request = format!("{REVIEW_REQUEST_PREFIX}{evidence_block}{REVIEW_REQUEST_SUFFIX}");
        let tokens = serialized_text_tokens(&request);
        if tokens <= MAX_RENDERED_REQUEST_TOKENS {
            return request;
        }
        let overage = tokens.saturating_sub(MAX_RENDERED_REQUEST_TOKENS);
        let next_budget =
            serialized_text_tokens(&evidence_block).saturating_sub(overage.saturating_add(1));
        let next = fit_serialized_text(&evidence_block, next_budget);
        if next == evidence_block {
            return format!("{REVIEW_REQUEST_PREFIX}{REVIEW_REQUEST_SUFFIX}");
        }
        evidence_block = next;
    }
}

fn fit_serialized_text(text: &str, max_tokens: usize) -> String {
    if max_tokens == 0 {
        return String::new();
    }
    if serialized_text_tokens(text) <= max_tokens {
        return text.to_string();
    }
    let mut low = 1;
    let mut high = approx_token_count(text).max(1);
    let mut best = String::new();
    while low <= high {
        let middle = low + (high - low) / 2;
        let candidate = truncate_text(text, TruncationPolicy::Tokens(middle));
        if serialized_text_tokens(&candidate) <= max_tokens {
            best = candidate;
            low = middle.saturating_add(1);
        } else {
            high = middle.saturating_sub(1);
        }
    }
    best
}

fn serialized_text_tokens(text: &str) -> usize {
    let rendered = serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_string());
    approx_token_count(&rendered)
}

fn build_repair_item(
    reviewer: Option<&CompletionReviewOutput>,
    gate: Option<&TaskCompletionGate>,
) -> Option<ResponseItem> {
    let mut lines = Vec::new();
    if let Some(reviewer) = reviewer {
        for finding in &reviewer.findings {
            lines.push(format!(
                "[{}] {}\nSmallest correction: {}\nProof command: {}",
                finding.severity.as_str(),
                finding.summary.trim(),
                finding.smallest_correction.trim(),
                finding.proof_command.trim()
            ));
        }
    }
    if let Some(gate) = gate
        && gate.status != TaskCompletionStatus::Passed
    {
        lines.extend(
            gate.reasons
                .iter()
                .map(|reason| format!("Evidence gap: {reason}")),
        );
    }
    if lines.is_empty() {
        return None;
    }
    let mut payload = fit_serialized_text(&lines.join("\n\n"), MAX_REPAIR_PAYLOAD_TOKENS);
    let empty_item = ContextualUserFragment::into(CompletionReviewRepair::new(""));
    if serialized_response_item_tokens(&empty_item) > REPAIR_ENVELOPE_RESERVE_TOKENS {
        return None;
    }
    loop {
        let item = ContextualUserFragment::into(CompletionReviewRepair::new(payload.clone()));
        let tokens = serialized_response_item_tokens(&item);
        if tokens <= MAX_RENDERED_REPAIR_TOKENS {
            return Some(item);
        }
        let overage = tokens.saturating_sub(MAX_RENDERED_REPAIR_TOKENS);
        let next_budget =
            serialized_text_tokens(&payload).saturating_sub(overage.saturating_add(1));
        let next = fit_serialized_text(&payload, next_budget);
        if next.is_empty() || next == payload {
            return None;
        }
        payload = next;
    }
}

fn serialized_response_item_tokens(item: &ResponseItem) -> usize {
    serde_json::to_string(item)
        .map(|rendered| approx_token_count(&rendered))
        .unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::models::ContentItem;

    fn user_message(text: &str, turn_id: &str) -> ResponseItem {
        user_message_with_content(
            vec![ContentItem::InputText {
                text: text.to_string(),
            }],
            turn_id,
        )
    }

    fn user_message_with_content(content: Vec<ContentItem>, turn_id: &str) -> ResponseItem {
        let mut item = ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content,
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        };
        item.set_turn_id_if_missing(turn_id);
        item
    }

    fn finding(severity: FindingSeverity) -> CompletionReviewFinding {
        CompletionReviewFinding {
            severity,
            summary: "missing requirement".to_string(),
            evidence: "the changed code omits it".to_string(),
            smallest_correction: "implement the omitted branch".to_string(),
            proof_command: "cargo test -p codex-core focused_test".to_string(),
        }
    }

    #[test]
    fn structured_output_requires_consistent_verdict_and_severity_order() {
        let valid = serde_json::to_string(&CompletionReviewOutput {
            verdict: ReviewVerdict::RepairNeeded,
            findings: vec![
                finding(FindingSeverity::High),
                finding(FindingSeverity::Low),
            ],
        })
        .expect("serialize");
        assert!(parse_review_output(&valid).is_some());

        let reversed = serde_json::to_string(&CompletionReviewOutput {
            verdict: ReviewVerdict::RepairNeeded,
            findings: vec![
                finding(FindingSeverity::Low),
                finding(FindingSeverity::High),
            ],
        })
        .expect("serialize");
        assert!(parse_review_output(&reversed).is_none());
        assert!(
            parse_review_output(r#"{"verdict":"clean","findings":[{"severity":"low","summary":"x","evidence":"x","smallest_correction":"x","proof_command":"x"}]}"#)
                .is_none()
        );
    }

    #[test]
    fn request_and_history_caps_include_serialization_expansion() {
        let entries = (0..45)
            .map(|index| format!("entry-{index}: {}", "\\\"\n".repeat(4_000)))
            .collect::<Vec<_>>();
        let rendered = render_bounded_user_history(entries);
        assert!(serialized_text_tokens(&rendered) <= MAX_USER_HISTORY_TOKENS);
        assert!(rendered.contains("entry-0"));
        assert!(rendered.contains("entry-44"));
        assert!(
            rendered
                .lines()
                .all(|entry| { serialized_text_tokens(entry) <= MAX_USER_ENTRY_TOKENS })
        );

        let request = fit_request(format!("{}{}", "\\\"\n".repeat(20_000), "latest"));
        assert!(serialized_text_tokens(&request) < 9_000);
        assert!(request.contains("latest"));
    }

    #[test]
    fn per_entry_and_task_caps_use_prefix_suffix_omission() {
        let text = format!("FIRST {} LAST", "middle ".repeat(20_000));
        let fitted = fit_serialized_text(&text, MAX_USER_ENTRY_TOKENS);
        assert!(serialized_text_tokens(&fitted) <= MAX_USER_ENTRY_TOKENS);
        assert!(fitted.contains("FIRST"));
        assert!(fitted.contains("LAST"));
        assert_ne!(fitted, text);

        let task = fit_serialized_text(&text, MAX_TASK_SUMMARY_TOKENS);
        assert!(serialized_text_tokens(&task) <= MAX_TASK_SUMMARY_TOKENS);
    }

    #[test]
    fn user_history_starts_after_last_passed_root_turn_and_falls_back_to_latest_forty() {
        let items = vec![
            user_message("before", "turn-1"),
            user_message("boundary", "turn-2"),
            user_message("after", "turn-3"),
        ];
        assert_eq!(
            extract_user_entries(&items, Some("turn-2"))
                .into_iter()
                .map(|entry| entry.text)
                .collect::<Vec<_>>(),
            vec!["after".to_string()],
        );

        let fallback = (0..45)
            .map(|index| user_message(&format!("entry-{index}"), &format!("turn-{index}")))
            .collect::<Vec<_>>();
        let extracted = extract_user_entries(&fallback, Some("missing-turn"));
        assert_eq!(extracted.len(), MAX_FALLBACK_USER_ENTRIES);
        assert_eq!(
            extracted.first().map(|entry| entry.text.as_str()),
            Some("entry-5")
        );
        assert_eq!(
            extracted.last().map(|entry| entry.text.as_str()),
            Some("entry-44")
        );
    }

    #[test]
    fn image_requirements_are_extracted_and_attached_in_order_without_text_leakage() {
        let items = vec![
            user_message_with_content(
                vec![ContentItem::InputImage {
                    image_url: "data:image/png;base64,before".to_string(),
                    detail: Some(ImageDetail::Low),
                }],
                "turn-before",
            ),
            user_message("boundary", "turn-boundary"),
            user_message_with_content(
                vec![
                    ContentItem::InputText {
                        text: r#"<image name=[Image #1] path="C:\private\review.png">"#.to_string(),
                    },
                    ContentItem::InputImage {
                        image_url: "data:image/png;base64,first".to_string(),
                        detail: Some(ImageDetail::High),
                    },
                    ContentItem::InputText {
                        text: "</image>".to_string(),
                    },
                    ContentItem::InputText {
                        text: "match this layout".to_string(),
                    },
                ],
                "turn-mixed",
            ),
            user_message_with_content(
                vec![ContentItem::InputImage {
                    image_url: "data:image/png;base64,second".to_string(),
                    detail: Some(ImageDetail::Original),
                }],
                "turn-image-only",
            ),
        ];

        let entries = extract_user_entries(&items, Some("turn-boundary"));
        let request = build_review_inputs("safe request".to_string(), &entries, true);

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].text, "match this layout");
        assert_eq!(entries[0].images[0].detail, Some(ImageDetail::High));
        assert!(entries[1].text.is_empty());
        assert_eq!(entries[1].images[0].detail, Some(ImageDetail::Original));
        assert_eq!(request.inputs.len(), 5);
        assert!(matches!(
            &request.inputs[2],
            UserInput::Image { image_url, detail: Some(ImageDetail::High) }
                if image_url.ends_with("first")
        ));
        assert!(matches!(
            &request.inputs[4],
            UserInput::Image { image_url, detail: Some(ImageDetail::Original) }
                if image_url.ends_with("second")
        ));
        for input in &request.inputs {
            if let UserInput::Text { text, .. } = input {
                assert!(!text.contains("data:image"));
                assert!(!text.contains("private\\review.png"));
            }
        }
        assert!(request.failures.is_empty());
    }

    #[test]
    fn image_caps_modality_and_compaction_loss_are_reported_as_partial() {
        let entries = vec![ReviewUserEntry {
            text: format!("requirement\n{COMPACT_IMAGE_OMISSION_MARKER}"),
            images: vec![
                ReviewImage {
                    image_url: "one".to_string(),
                    detail: None,
                },
                ReviewImage {
                    image_url: "two".to_string(),
                    detail: None,
                },
            ],
        }];

        let capped =
            build_review_inputs_with_limits("request".to_string(), &entries, true, 1, usize::MAX);
        assert_eq!(
            capped
                .inputs
                .iter()
                .filter(|input| matches!(input, UserInput::Image { .. }))
                .count(),
            1
        );
        assert_eq!(
            capped.failures,
            vec![ReviewFailureCategory::InputImagesOmitted]
        );

        let unsupported = build_review_inputs("request".to_string(), &entries, false);
        assert!(
            !unsupported
                .inputs
                .iter()
                .any(|input| matches!(input, UserInput::Image { .. }))
        );
        assert_eq!(
            unsupported.failures,
            vec![ReviewFailureCategory::InputImagesOmitted]
        );
    }

    #[test]
    fn repair_fragment_reserves_envelope_and_stays_below_limit() {
        let output = CompletionReviewOutput {
            verdict: ReviewVerdict::RepairNeeded,
            findings: (0..MAX_REVIEW_FINDINGS)
                .map(|_| CompletionReviewFinding {
                    smallest_correction: "correction ".repeat(600),
                    ..finding(FindingSeverity::High)
                })
                .collect(),
        };
        let item = build_repair_item(Some(&output), None).expect("repair item");
        assert!(serialized_response_item_tokens(&item) < 1_000);
        assert!(
            serialized_response_item_tokens(&ContextualUserFragment::into(
                CompletionReviewRepair::new("")
            )) <= REPAIR_ENVELOPE_RESERVE_TOKENS
        );
    }

    #[test]
    fn turn_local_state_cannot_be_rearmed() {
        let mut state = CompletionReviewState::default();
        assert_eq!(state.phase, CompletionReviewPhase::NotReviewed);
        state.phase = CompletionReviewPhase::Reviewed;
        state.phase = CompletionReviewPhase::RepairStarted;
        assert_ne!(state.phase, CompletionReviewPhase::NotReviewed);
    }

    #[tokio::test]
    async fn parent_abort_wins_before_reviewer_invocation() {
        let (sess, turn_context) = crate::session::tests::make_session_and_context().await;
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        let result = run_reviewer_with_deadline(
            &Arc::new(sess),
            &Arc::new(turn_context),
            vec![UserInput::Text {
                text: "request".to_string(),
                text_elements: Vec::new(),
            }],
            &cancellation,
        )
        .await;

        assert!(matches!(result, Err(CodexErr::TurnAborted)));
    }

    #[tokio::test]
    async fn non_root_agent_is_ineligible_without_starting_review() {
        let (sess, mut turn_context) = crate::session::tests::make_session_and_context().await;
        turn_context.session_source = codex_protocol::protocol::SessionSource::SubAgent(
            SubAgentSource::Other("eligibility-test".to_string()),
        );
        let mut state = CompletionReviewState::default();

        let outcome = coordinate_completion_review(
            &Arc::new(sess),
            &Arc::new(turn_context),
            &CancellationToken::new(),
            Some(0),
            None,
            &mut state,
        )
        .await
        .expect("non-root eligibility");

        assert!(!outcome.repair_injected);
        assert!(outcome.advisory.is_none());
        assert!(outcome.partial_reasons.is_empty());
        assert_eq!(state.phase, CompletionReviewPhase::NotReviewed);
    }
}
