use crate::function_tool::FunctionCallError;
use crate::tools::command_output_artifact::ReadToolOutputError;
use crate::tools::command_output_artifact::ReadToolOutputResult;
use crate::tools::command_output_artifact::ToolOutputSelector;
#[cfg(test)]
use crate::tools::command_output_artifact::ToolOutputSelectorResult;
use crate::tools::command_output_artifact::ToolOutputSelectorStatus;
use crate::tools::command_output_artifact::read_tool_output_selectors_with_ceiling_and_reuse;
use crate::tools::command_output_artifact::read_tool_output_selectors_with_reuse;
use crate::tools::context::ToolCallSource;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::context::semantic_evidence_for_command_output;
use crate::tools::handlers::read_tool_output_spec::READ_TOOL_OUTPUT_TOOL_NAME;
use crate::tools::handlers::read_tool_output_spec::create_read_tool_output_tool;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::protocol::DeterministicContinuationClass;
use codex_protocol::protocol::DeterministicContinuationHostAction;
use codex_protocol::protocol::TurnTimingDeterministicContinuationReceipt;
use codex_tools::CanonicalToolResult;
use codex_tools::JsonToolOutput;
use codex_tools::ToolName;
use codex_tools::ToolOutput;
use codex_tools::ToolOutputProjectionMetadata;
use codex_tools::ToolSpec;
use serde::Deserialize;
use serde_json::Value;
use std::path::Path;

const DEFAULT_MAX_BYTES: usize = 16_384;
const DEFAULT_LINE_COUNT: usize = 200;
const MAX_LEGACY_RANGES: usize = 16;
const MAX_AGGREGATE_LINES: usize = 2_000;
// A nested result is serialized into a code-mode cell and then into the outer
// exec result. Reserve enough space for that outer envelope so a fitting exact
// recovery cannot be recursively truncated into another artifact.
const CODE_MODE_RECOVERY_WRAPPER_RESERVE_TOKENS: usize = 4_000;
const CODE_MODE_RECOVERY_TOKEN_CEILING: usize =
    codex_utils_output_truncation::DEFAULT_SUCCESS_OUTPUT_TOKENS
        .saturating_sub(CODE_MODE_RECOVERY_WRAPPER_RESERVE_TOKENS);

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadToolOutputArgs {
    artifact_id: String,
    #[serde(default)]
    selectors: Option<Vec<ToolOutputSelector>>,
    #[serde(default)]
    start_line: Option<usize>,
    #[serde(default)]
    end_line: Option<usize>,
    #[serde(default)]
    ranges: Vec<ReadToolOutputRangeArgs>,
    #[serde(default)]
    max_bytes: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadToolOutputRangeArgs {
    start_line: usize,
    end_line: usize,
}

pub struct ReadToolOutputHandler;

struct ReadToolOutputToolOutput {
    inner: JsonToolOutput,
    exact_recovery: Option<(TurnTimingDeterministicContinuationReceipt, Value)>,
    semantic_evidence: Vec<String>,
}

impl ToolOutput for ReadToolOutputToolOutput {
    fn log_preview(&self) -> String {
        self.inner.log_preview()
    }

    fn success_for_logging(&self) -> bool {
        self.inner.success_for_logging()
    }

    fn sampling_request_signal(&self) -> Option<Value> {
        Some(serde_json::json!({
            "kind": "semantic_evidence",
            "semantic_evidence": self.semantic_evidence,
        }))
    }

    fn deterministic_continuation_receipts(
        &self,
    ) -> Vec<TurnTimingDeterministicContinuationReceipt> {
        self.exact_recovery
            .as_ref()
            .map(|(receipt, _)| vec![receipt.clone()])
            .unwrap_or_default()
    }

    fn deterministic_continuation_content(&self) -> Vec<Value> {
        self.exact_recovery
            .as_ref()
            .map(|(_, value)| vec![value.clone()])
            .unwrap_or_default()
    }

    fn projection_metadata(&self) -> Option<ToolOutputProjectionMetadata> {
        self.inner.projection_metadata()
    }

    fn canonical_result(&self, payload: &ToolPayload) -> Option<CanonicalToolResult> {
        self.inner.canonical_result(payload)
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        self.inner.to_response_item(call_id, payload)
    }

    fn code_mode_result(&self, payload: &ToolPayload) -> Value {
        self.inner.code_mode_result(payload)
    }
}

impl ToolExecutor<ToolInvocation> for ReadToolOutputHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(READ_TOOL_OUTPUT_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        create_read_tool_output_tool()
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(handle_read_tool_output(invocation))
    }
}

impl CoreToolRuntime for ReadToolOutputHandler {}

async fn handle_read_tool_output(
    invocation: ToolInvocation,
) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
    let ToolPayload::Function { ref arguments } = invocation.payload else {
        return Err(FunctionCallError::RespondToModel(
            "read_tool_output received unsupported payload".to_string(),
        ));
    };
    let args: ReadToolOutputArgs = serde_json::from_str(arguments).map_err(|err| {
        FunctionCallError::RespondToModel(format!(
            "failed to parse read_tool_output arguments: {err}"
        ))
    })?;
    // Keep validating the legacy knob for compatibility, but never use it to
    // clip a selected value. The selector engine owns its exact response fit.
    let _legacy_max_bytes = resolved_max_bytes(args.max_bytes)?;
    let selectors = resolved_selectors(&args)?;
    let code_mode_recovery = matches!(&invocation.source, ToolCallSource::CodeMode { .. });
    let action_bounds_hash = crate::tool_history::sha256(
        serde_json::to_string(&selectors)
            .unwrap_or_default()
            .as_bytes(),
    );
    let (output, reused) = execute_recovery_transaction(
        invocation.turn.config.codex_home.as_path(),
        &invocation.session.thread_id.to_string(),
        &args.artifact_id,
        selectors,
        code_mode_recovery,
    )
    .await
    .map_err(|err| FunctionCallError::RespondToModel(err.for_model()))?;
    if !reused {
        invocation
            .turn
            .turn_timing_state
            .record_tool_output_artifact_reread();
    }
    invocation
        .turn
        .turn_timing_state
        .record_tool_output_recovery(recovery_retruncation_count(&output));

    let exact_recovery_receipt =
        exact_code_mode_recovery_receipt(code_mode_recovery, &output, action_bounds_hash);
    let semantic_evidence = read_tool_output_semantic_evidence(&output);
    let output = serde_json::to_value(output).map_err(|err| {
        FunctionCallError::RespondToModel(format!("failed to serialize recovery result: {err}"))
    })?;
    let exact_recovery = exact_recovery_receipt.map(|receipt| (receipt, output.clone()));
    Ok(boxed_tool_output(ReadToolOutputToolOutput {
        inner: JsonToolOutput::new(output),
        exact_recovery,
        semantic_evidence,
    }))
}

fn read_tool_output_semantic_evidence(output: &ReadToolOutputResult) -> Vec<String> {
    let recovered_text = output
        .results
        .iter()
        .filter(|result| result.status == ToolOutputSelectorStatus::Ok)
        .filter_map(|result| result.text.as_deref())
        .collect::<Vec<_>>()
        .join("\n");
    if !recovered_text.is_empty() {
        let mut evidence = semantic_evidence_for_command_output(recovered_text.as_bytes());
        let supplemental_results = output
            .results
            .iter()
            .filter(|result| {
                result.status != ToolOutputSelectorStatus::Ok
                    || !result.complete
                    || result.text.is_none()
            })
            .collect::<Vec<_>>();
        if !output.complete
            || !output.unavailable_ranges.is_empty()
            || !supplemental_results.is_empty()
        {
            let supplemental = serde_json::to_vec(&serde_json::json!({
                "complete": output.complete,
                "unavailable_ranges": output.unavailable_ranges,
                "results": supplemental_results,
            }))
            .unwrap_or_default();
            evidence.push(format!(
                "artifact-recovery-status-v1:{}",
                crate::tool_history::sha256(&supplemental)
            ));
        }
        return evidence;
    }
    let recovered_complete_artifact = output.complete
        && output.unavailable_ranges.is_empty()
        && output.results.iter().any(|result| {
            result.status == ToolOutputSelectorStatus::Ok
                && result.complete
                && result
                    .canonical_range
                    .is_some_and(|range| range.start == 0 && range.end == output.canonical_bytes)
        });
    if recovered_complete_artifact {
        return vec![format!("canonical-output-v1:{}", output.canonical_sha256)];
    }
    let projection = serde_json::to_vec(&serde_json::json!({
        "canonical_sha256": output.canonical_sha256,
        "complete": output.complete,
        "unavailable_ranges": output.unavailable_ranges,
        "results": output.results,
    }))
    .unwrap_or_default();
    vec![format!(
        "artifact-projection-v1:{}",
        crate::tool_history::sha256(&projection)
    )]
}

pub(crate) async fn execute_recovery_transaction(
    codex_home: &Path,
    thread_id: &str,
    artifact_id: &str,
    selectors: Vec<ToolOutputSelector>,
    code_mode_recovery: bool,
) -> Result<(ReadToolOutputResult, bool), ReadToolOutputError> {
    if code_mode_recovery {
        read_tool_output_selectors_with_ceiling_and_reuse(
            codex_home,
            thread_id,
            artifact_id,
            selectors,
            CODE_MODE_RECOVERY_TOKEN_CEILING,
        )
        .await
    } else {
        read_tool_output_selectors_with_reuse(codex_home, thread_id, artifact_id, selectors).await
    }
}

fn exact_code_mode_recovery_receipt(
    code_mode_recovery: bool,
    output: &crate::tools::command_output_artifact::ReadToolOutputResult,
    action_bounds_hash: String,
) -> Option<TurnTimingDeterministicContinuationReceipt> {
    (code_mode_recovery
        && output.complete
        && output.unavailable_ranges.is_empty()
        && !output.results.is_empty()
        && output
            .results
            .iter()
            .all(|result| result.status == ToolOutputSelectorStatus::Ok && result.complete))
    .then(|| TurnTimingDeterministicContinuationReceipt {
        class: DeterministicContinuationClass::ArtifactRange,
        wire_identity: String::new(),
        resource_identity_hash: crate::tool_history::sha256(output.artifact_id.as_bytes()),
        state_revision: output.canonical_sha256.clone(),
        host_action: DeterministicContinuationHostAction::DrainArtifactRanges,
        action_bounds_hash,
        suppressed_continuation_count: 1,
    })
}

fn recovery_retruncation_count(
    _output: &crate::tools::command_output_artifact::ReadToolOutputResult,
) -> u32 {
    // Typed overflow is a truthful transaction outcome, not loss of evidence
    // that was already complete. Direct recovery bypasses recursive spilling,
    // and code-mode fit is decided before the carrier is serialized.
    0
}

fn resolved_selectors(
    args: &ReadToolOutputArgs,
) -> Result<Vec<ToolOutputSelector>, FunctionCallError> {
    if let Some(selectors) = &args.selectors {
        if selectors.is_empty() {
            return Err(FunctionCallError::RespondToModel(
                "selectors must contain at least one selector".to_string(),
            ));
        }
        if selectors.len() > 64 {
            return Err(FunctionCallError::RespondToModel(
                "selectors may contain at most 64 entries".to_string(),
            ));
        }
        if args.start_line.is_some() || args.end_line.is_some() || !args.ranges.is_empty() {
            return Err(FunctionCallError::RespondToModel(
                "selectors cannot be combined with legacy line arguments".to_string(),
            ));
        }
        return Ok(selectors.clone());
    }
    if !args.ranges.is_empty() {
        if args.start_line.is_some() || args.end_line.is_some() {
            return Err(FunctionCallError::RespondToModel(
                "ranges is mutually exclusive with start_line/end_line".to_string(),
            ));
        }
        if args.ranges.len() > MAX_LEGACY_RANGES {
            return Err(FunctionCallError::RespondToModel(format!(
                "ranges may contain at most {MAX_LEGACY_RANGES} entries"
            )));
        }
        let mut ranges = args
            .ranges
            .iter()
            .map(|range| {
                if range.start_line == 0 || range.end_line < range.start_line {
                    Err(FunctionCallError::RespondToModel(
                        "each range requires 1-based start_line <= end_line".to_string(),
                    ))
                } else {
                    Ok(ToolOutputSelector::Lines {
                        start: range.start_line,
                        end: range.end_line,
                    })
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        ranges.sort_unstable_by_key(|selector| match selector {
            ToolOutputSelector::Lines { start, end } => (*start, *end),
            _ => unreachable!("legacy ranges always normalize to line selectors"),
        });
        let mut normalized: Vec<ToolOutputSelector> = Vec::with_capacity(ranges.len());
        for selector in ranges {
            let ToolOutputSelector::Lines { start, end } = selector else {
                unreachable!("legacy ranges always normalize to line selectors");
            };
            match normalized.last_mut() {
                Some(ToolOutputSelector::Lines {
                    start: _,
                    end: previous_end,
                }) if start <= previous_end.saturating_add(1) => {
                    *previous_end = (*previous_end).max(end);
                }
                _ => normalized.push(ToolOutputSelector::Lines { start, end }),
            }
        }
        let aggregate_lines = normalized.iter().try_fold(0_usize, |total, selector| {
            let ToolOutputSelector::Lines { start, end } = selector else {
                unreachable!("legacy ranges always normalize to line selectors");
            };
            total.checked_add(end - start + 1)
        });
        if aggregate_lines.is_none_or(|lines| lines > MAX_AGGREGATE_LINES) {
            return Err(FunctionCallError::RespondToModel(format!(
                "ranges may request at most {MAX_AGGREGATE_LINES} aggregate lines"
            )));
        }
        return Ok(normalized);
    }
    let (start, end) = resolved_line_range(args)?;
    Ok(vec![ToolOutputSelector::Lines { start, end }])
}

fn resolved_line_range(args: &ReadToolOutputArgs) -> Result<(usize, usize), FunctionCallError> {
    let start_line = args.start_line.unwrap_or(1);
    let end_line = match args.end_line {
        Some(end_line) => end_line,
        None => start_line
            .checked_add(DEFAULT_LINE_COUNT - 1)
            .ok_or_else(|| {
                FunctionCallError::RespondToModel("start_line is too large".to_string())
            })?,
    };
    Ok((start_line, end_line))
}

fn resolved_max_bytes(max_bytes: Option<usize>) -> Result<usize, FunctionCallError> {
    match max_bytes {
        Some(max_bytes) if max_bytes == 0 || max_bytes > DEFAULT_MAX_BYTES => {
            Err(FunctionCallError::RespondToModel(format!(
                "max_bytes must be between 1 and {DEFAULT_MAX_BYTES}"
            )))
        }
        Some(max_bytes) => Ok(max_bytes),
        None => Ok(DEFAULT_MAX_BYTES),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selector_result(status: ToolOutputSelectorStatus) -> ToolOutputSelectorResult {
        ToolOutputSelectorResult {
            selector: ToolOutputSelector::Lines { start: 1, end: 1 },
            status,
            complete: status == ToolOutputSelectorStatus::Ok,
            exact_bytes: None,
            canonical_range: None,
            text: None,
            value: None,
            data_base64: None,
            subdivision_plan: None,
            child_selectors: Vec::new(),
            continuation: None,
            message: None,
        }
    }

    #[test]
    fn typed_overflow_is_not_counted_as_retruncation() {
        let output = crate::tools::command_output_artifact::ReadToolOutputResult {
            artifact_id: "artifact".to_string(),
            canonical_sha256: "digest".to_string(),
            canonical_bytes: 1,
            retained_bytes: 1,
            complete: false,
            unavailable_ranges: Vec::new(),
            results: vec![
                selector_result(ToolOutputSelectorStatus::Ok),
                selector_result(ToolOutputSelectorStatus::SelectorTooLarge),
                selector_result(ToolOutputSelectorStatus::AggregateOmitted),
                selector_result(ToolOutputSelectorStatus::NotFound),
            ],
        };

        assert_eq!(recovery_retruncation_count(&output), 0);
    }

    #[test]
    fn exact_code_mode_recovery_is_carried_by_owner_receipt() {
        let output = crate::tools::command_output_artifact::ReadToolOutputResult {
            artifact_id: "01900000-0000-7000-8000-000000000000".to_string(),
            canonical_sha256: "canonical-revision".to_string(),
            canonical_bytes: 5,
            retained_bytes: 5,
            complete: true,
            unavailable_ranges: Vec::new(),
            results: vec![selector_result(ToolOutputSelectorStatus::Ok)],
        };

        let receipt =
            exact_code_mode_recovery_receipt(true, &output, "selector-bounds".to_string())
                .expect("exact nested recovery receipt");

        assert_eq!(receipt.state_revision, "canonical-revision");
        assert_eq!(receipt.action_bounds_hash, "selector-bounds");
        assert_eq!(receipt.suppressed_continuation_count, 1);
        assert!(
            exact_code_mode_recovery_receipt(false, &output, "selector-bounds".to_string(),)
                .is_none()
        );

        let incomplete = crate::tools::command_output_artifact::ReadToolOutputResult {
            results: vec![selector_result(ToolOutputSelectorStatus::SelectorTooLarge)],
            ..output
        };
        assert!(
            exact_code_mode_recovery_receipt(true, &incomplete, "selector-bounds".to_string(),)
                .is_none()
        );
    }

    #[test]
    fn complete_artifact_recovery_reuses_the_producers_canonical_identity() {
        let mut result = selector_result(ToolOutputSelectorStatus::Ok);
        result.canonical_range = Some(codex_tools::CanonicalByteRange::new(0, 5));
        let output = ReadToolOutputResult {
            artifact_id: "artifact".to_string(),
            canonical_sha256: "canonical-revision".to_string(),
            canonical_bytes: 5,
            retained_bytes: 5,
            complete: true,
            unavailable_ranges: Vec::new(),
            results: vec![result],
        };

        assert_eq!(
            read_tool_output_semantic_evidence(&output),
            vec!["canonical-output-v1:canonical-revision"]
        );
    }

    #[test]
    fn recovered_text_reuses_the_command_fact_identity() {
        let mut result = selector_result(ToolOutputSelectorStatus::Ok);
        result.text = Some("src/lib.rs:10:let stable = compute();".to_string());
        let output = ReadToolOutputResult {
            artifact_id: "artifact".to_string(),
            canonical_sha256: "canonical-revision".to_string(),
            canonical_bytes: 40,
            retained_bytes: 40,
            complete: true,
            unavailable_ranges: Vec::new(),
            results: vec![result],
        };

        assert_eq!(
            read_tool_output_semantic_evidence(&output),
            semantic_evidence_for_command_output(
                b"diff --git a/src/lib.rs b/src/lib.rs\n@@ -9,0 +10 @@\n+let stable = compute();"
            )
        );
    }

    #[test]
    fn recovered_text_preserves_incomplete_selector_status() {
        let mut recovered = selector_result(ToolOutputSelectorStatus::Ok);
        recovered.text = Some("src/lib.rs:10:let stable = compute();".to_string());
        let complete = ReadToolOutputResult {
            artifact_id: "artifact".to_string(),
            canonical_sha256: "canonical-revision".to_string(),
            canonical_bytes: 40,
            retained_bytes: 40,
            complete: true,
            unavailable_ranges: Vec::new(),
            results: vec![recovered.clone()],
        };
        let incomplete = ReadToolOutputResult {
            results: vec![
                recovered,
                selector_result(ToolOutputSelectorStatus::AggregateOmitted),
            ],
            ..complete.clone()
        };

        assert_ne!(
            read_tool_output_semantic_evidence(&incomplete),
            read_tool_output_semantic_evidence(&complete)
        );
    }

    #[test]
    fn artifact_recovery_leaves_space_for_the_outer_exec_envelope() {
        assert_eq!(
            CODE_MODE_RECOVERY_TOKEN_CEILING + CODE_MODE_RECOVERY_WRAPPER_RESERVE_TOKENS,
            codex_utils_output_truncation::DEFAULT_SUCCESS_OUTPUT_TOKENS,
        );
        assert_eq!(CODE_MODE_RECOVERY_TOKEN_CEILING, 6_000);
    }

    #[test]
    fn default_range_is_exactly_two_hundred_lines() {
        let args = ReadToolOutputArgs {
            artifact_id: uuid::Uuid::now_v7().to_string(),
            selectors: None,
            start_line: Some(17),
            end_line: None,
            ranges: Vec::new(),
            max_bytes: None,
        };
        assert_eq!(resolved_line_range(&args).unwrap(), (17, 216));
    }

    #[test]
    fn max_bytes_is_legacy_validated_but_not_a_clipping_contract() {
        assert_eq!(resolved_max_bytes(None).unwrap(), 16_384);
        assert_eq!(resolved_max_bytes(Some(1)).unwrap(), 1);
        assert_eq!(resolved_max_bytes(Some(16_384)).unwrap(), 16_384);
        for invalid in [0, 16_385, usize::MAX] {
            assert!(resolved_max_bytes(Some(invalid)).is_err());
        }
    }

    #[test]
    fn three_exact_ranges_become_one_bounded_owner_batch() {
        let args = ReadToolOutputArgs {
            artifact_id: uuid::Uuid::now_v7().to_string(),
            selectors: None,
            start_line: None,
            end_line: None,
            ranges: vec![
                ReadToolOutputRangeArgs {
                    start_line: 2,
                    end_line: 4,
                },
                ReadToolOutputRangeArgs {
                    start_line: 11,
                    end_line: 13,
                },
                ReadToolOutputRangeArgs {
                    start_line: 21,
                    end_line: 25,
                },
            ],
            max_bytes: None,
        };

        assert_eq!(
            resolved_selectors(&args).unwrap(),
            vec![
                ToolOutputSelector::Lines { start: 2, end: 4 },
                ToolOutputSelector::Lines { start: 11, end: 13 },
                ToolOutputSelector::Lines { start: 21, end: 25 },
            ]
        );
    }

    #[test]
    fn legacy_ranges_are_sorted_merged_and_capped_at_sixteen() {
        let mut args = ReadToolOutputArgs {
            artifact_id: uuid::Uuid::now_v7().to_string(),
            selectors: None,
            start_line: None,
            end_line: None,
            ranges: vec![
                ReadToolOutputRangeArgs {
                    start_line: 10,
                    end_line: 12,
                },
                ReadToolOutputRangeArgs {
                    start_line: 2,
                    end_line: 4,
                },
                ReadToolOutputRangeArgs {
                    start_line: 4,
                    end_line: 6,
                },
                ReadToolOutputRangeArgs {
                    start_line: 7,
                    end_line: 9,
                },
            ],
            max_bytes: None,
        };

        assert_eq!(
            resolved_selectors(&args).unwrap(),
            vec![ToolOutputSelector::Lines { start: 2, end: 12 }]
        );

        args.ranges = (1..=MAX_LEGACY_RANGES)
            .map(|line| ReadToolOutputRangeArgs {
                start_line: line * 2,
                end_line: line * 2,
            })
            .collect();
        assert_eq!(resolved_selectors(&args).unwrap().len(), MAX_LEGACY_RANGES);

        args.ranges = (1..=MAX_LEGACY_RANGES + 1)
            .map(|line| ReadToolOutputRangeArgs {
                start_line: line * 2,
                end_line: line * 2,
            })
            .collect();
        assert!(resolved_selectors(&args).is_err());

        args.ranges = vec![
            ReadToolOutputRangeArgs {
                start_line: 1,
                end_line: 1_000,
            },
            ReadToolOutputRangeArgs {
                start_line: 2_000,
                end_line: 3_000,
            },
        ];
        assert!(resolved_selectors(&args).is_err());
    }
}
