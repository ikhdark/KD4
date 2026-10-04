use crate::FunctionCallError;
use crate::tools::command_output_artifact::RECOVERY_AGGREGATE_TOKEN_CEILING;
use crate::tools::command_output_artifact::ReadToolOutputError;
use crate::tools::command_output_artifact::ReadToolOutputResult;
use crate::tools::command_output_artifact::ToolOutputSelector;
use crate::tools::command_output_artifact::ToolOutputSelectorResult;
use crate::tools::command_output_artifact::ToolOutputSelectorStatus;
use crate::tools::command_output_artifact::ToolOutputSnapshot;
use crate::tools::command_output_artifact::load_tool_output_snapshot;
#[cfg(test)]
use crate::tools::command_output_artifact::read_tool_output_selectors_with_ceiling_and_reuse;
#[cfg(test)]
use crate::tools::command_output_artifact::read_tool_output_selectors_with_reuse;
use crate::tools::context::ToolCallSource;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::handlers::read_tool_output_spec::READ_TOOL_OUTPUT_MAX_BYTES;
use crate::tools::handlers::read_tool_output_spec::READ_TOOL_OUTPUT_MAX_LEGACY_RANGES;
use crate::tools::handlers::read_tool_output_spec::READ_TOOL_OUTPUT_MAX_SELECTORS;
use crate::tools::handlers::read_tool_output_spec::READ_TOOL_OUTPUT_SCRIPT_MAX_BYTES;
use crate::tools::handlers::read_tool_output_spec::READ_TOOL_OUTPUT_TOOL_NAME;
use crate::tools::handlers::read_tool_output_spec::create_read_tool_output_tool;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use base64::Engine as _;
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
use codex_utils_string::TokenCountEstimate;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;
use std::collections::HashSet;
#[cfg(test)]
use std::path::Path;
use tokio_util::sync::CancellationToken;

const DEFAULT_LINE_COUNT: usize = 200;
const MAX_AGGREGATE_LINES: usize = 2_000;
// A nested result is serialized into a code-mode cell and then into the outer
// exec result. Reserve enough space for that outer envelope so a fitting exact
// recovery cannot be recursively truncated into another artifact.
const CODE_MODE_RECOVERY_WRAPPER_RESERVE_TOKENS: usize = 1_000;
const CODE_MODE_RECOVERY_TOKEN_CEILING: usize =
    codex_utils_output_truncation::DEFAULT_SUCCESS_OUTPUT_TOKENS
        .saturating_sub(CODE_MODE_RECOVERY_WRAPPER_RESERVE_TOKENS);

#[derive(Debug)]
pub(crate) struct DrainedRecoveryTransaction {
    pub(crate) output: ReadToolOutputResult,
    drained_continuation_pages: u32,
    continuation_stop: Option<RecoveryContinuationStopV1>,
}

fn recovery_envelope(
    output: &ReadToolOutputResult,
    stop: Option<&RecoveryContinuationStopV1>,
) -> serde_json::Result<Value> {
    let mut value = serde_json::to_value(output)?;
    if let (Some(stop), Some(object)) = (stop, value.as_object_mut()) {
        object.insert("continuation_stop".to_string(), serde_json::to_value(stop)?);
    }
    Ok(value)
}

fn recovery_envelope_fits(
    output: &ReadToolOutputResult,
    stop: Option<&RecoveryContinuationStopV1>,
    ceiling: usize,
) -> bool {
    recovery_envelope(output, stop)
        .and_then(|value| serde_json::to_string(&value))
        .is_ok_and(|text| codex_utils_string::approx_token_count(&text) <= ceiling)
}

struct RecoveryCheckpoint {
    index: usize,
    selector: ToolOutputSelector,
    length: usize,
    complete: bool,
    continuation: Option<ToolOutputSelector>,
    owner_complete: bool,
    owner_cost: RecoveryResultCost,
}

#[expect(
    clippy::expect_used,
    reason = "Recovery selectors and envelopes have infallible JSON serialization"
)]
fn recovery_size(value: &impl Serialize) -> TokenCountEstimate {
    TokenCountEstimate::new(&serde_json::to_string(value).expect("recovery value serializes"))
}

fn continuation_size(selector: Option<&ToolOutputSelector>) -> TokenCountEstimate {
    selector.map_or_else(TokenCountEstimate::default, |selector| {
        TokenCountEstimate::new(",\"continuation\":").add_delimited(recovery_size(selector))
    })
}

#[derive(Clone, Copy)]
struct RecoveryResultCost {
    serialized: TokenCountEstimate,
    payload_tokens: usize,
}

impl RecoveryResultCost {
    fn new(result: &ToolOutputSelectorResult) -> Self {
        let payload_tokens = if result.status == ToolOutputSelectorStatus::Ok {
            result
                .text
                .as_ref()
                .or(result.data_base64.as_ref())
                .map(recovery_size)
                .or_else(|| result.value.as_ref().map(recovery_size))
                .map_or(0, TokenCountEstimate::tokens)
        } else {
            0
        };
        Self {
            serialized: recovery_size(result),
            payload_tokens,
        }
    }
}

struct ReconstructedSelection {
    result: ToolOutputSelectorResult,
    cost: RecoveryResultCost,
    source_length: usize,
    direct_index: Option<usize>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ContinuationStopReason {
    Budget,
    Cancelled,
    IdentityDrift,
    IncompleteOwnerResult,
    InvalidSelector,
    SelectorNotFound,
    PageReadError,
    RepeatedSelector,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct RecoveryContinuationStopV1 {
    version: u8,
    reason: ContinuationStopReason,
    selector: Option<ToolOutputSelector>,
    resumable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    page_selectors: Vec<ToolOutputSelector>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ContinuationStep {
    Complete,
    Follow {
        result_index: usize,
        selector: ToolOutputSelector,
    },
    Stop(ContinuationStopReason),
}

struct RecoveryContinuationState {
    checkpoints: Vec<RecoveryCheckpoint>,
    initial_result_count: usize,
    output: ReadToolOutputResult,
    followed_selectors: Vec<(usize, ToolOutputSelector)>,
    result_owners: Vec<usize>,
    drained_continuation_pages: u32,
    token_ceiling: usize,
    continuation_stop: Option<RecoveryContinuationStopV1>,
    result_costs: Vec<RecoveryResultCost>,
    reconstructed: Vec<Option<ReconstructedSelection>>,
    covered_until: Vec<u64>,
    envelope_costs: [TokenCountEstimate; 2],
}

impl RecoveryContinuationState {
    fn new(mut output: ReadToolOutputResult, token_ceiling: usize) -> Self {
        let results = std::mem::take(&mut output.results);
        let complete = output.complete;
        output.complete = false;
        let incomplete_cost = recovery_size(&output);
        output.complete = true;
        let complete_cost = recovery_size(&output);
        output.complete = complete;
        output.results = results;
        let mut state = Self {
            checkpoints: Vec::new(),
            initial_result_count: output.results.len(),
            result_owners: (0..output.results.len()).collect(),
            result_costs: output.results.iter().map(RecoveryResultCost::new).collect(),
            reconstructed: (0..output.results.len()).map(|_| None).collect(),
            covered_until: output
                .results
                .iter()
                .map(|result| result.canonical_range.map_or(0, |range| range.start))
                .collect(),
            envelope_costs: [incomplete_cost, complete_cost],
            output,
            followed_selectors: Vec::new(),
            drained_continuation_pages: 0,
            token_ceiling,
            continuation_stop: None,
        };
        state.refresh_reconstruction();
        state
    }

    fn record_stop(
        &mut self,
        reason: ContinuationStopReason,
        selector: Option<ToolOutputSelector>,
    ) {
        let resumable = matches!(
            reason,
            ContinuationStopReason::Budget | ContinuationStopReason::Cancelled
        );
        let selector = selector.map(|selector| {
            if resumable {
                self.remaining_owner_selector(selector)
            } else {
                selector
            }
        });
        self.continuation_stop = Some(RecoveryContinuationStopV1 {
            page_selectors: Vec::new(),
            version: 1,
            reason,
            selector,
            resumable,
            message: Some(match reason {
                ContinuationStopReason::Budget => "Recovery reached its output budget. Continue with the unconsumed selector in a new call.",
                ContinuationStopReason::Cancelled => "Recovery was cancelled. Already recovered pages are retained; the unconsumed selector can be retried.",
                ContinuationStopReason::IdentityDrift => "The artifact identity or continuation changed. Re-identify the artifact and start a new recovery; do not combine pages from different identities.",
                ContinuationStopReason::IncompleteOwnerResult => "The artifact has unavailable ranges or returned an incomplete continuation result. Inspect the retained evidence and obtain the missing source before claiming complete recovery.",
                ContinuationStopReason::InvalidSelector => "The selector is invalid. Correct it using the selector result message and the read_tool_output schema.",
                ContinuationStopReason::SelectorNotFound => "The selector did not find the requested content. Inspect the artifact structure or revise the selector.",
                ContinuationStopReason::RepeatedSelector => "The continuation repeated an already consumed selector. Stop this traversal and choose a different selector.",
                ContinuationStopReason::PageReadError => "The continuation page could not be read.",
            }.to_string()),
        });
    }

    fn record_page_read_error(
        &mut self,
        error: &ReadToolOutputError,
        selector: ToolOutputSelector,
    ) {
        let resumable = matches!(error, ReadToolOutputError::StillWriting);
        let selector = if resumable {
            self.remaining_owner_selector(selector)
        } else {
            selector
        };
        self.continuation_stop = Some(RecoveryContinuationStopV1 {
            page_selectors: Vec::new(),
            version: 1,
            reason: ContinuationStopReason::PageReadError,
            selector: Some(selector),
            resumable,
            message: Some(error.for_model()),
        });
    }

    // Internal draining follows bounded child ranges. Across model calls the
    // continuation must also carry the owner's remaining extent: retrying only
    // one child would complete that child and silently lose every later page.
    fn remaining_owner_selector(&self, selector: ToolOutputSelector) -> ToolOutputSelector {
        let ToolOutputSelector::Bytes { start, end } = &selector else {
            return selector;
        };
        let remaining_end = self.output.results.iter().find_map(|owner| {
            if owner.continuation.as_ref() != Some(&selector) {
                return None;
            }
            let range = owner.canonical_range?;
            (range.start <= *start && *end <= range.end).then_some(range.end)
        });
        match remaining_end {
            Some(end) => ToolOutputSelector::Bytes { start: *start, end },
            None => selector,
        }
    }

    fn first_pending_selector(&self) -> Option<ToolOutputSelector> {
        self.output.results.iter().find_map(|result| {
            if selector_stop_reason(result.status).is_some() {
                Some(result.selector.clone())
            } else {
                result.continuation.clone()
            }
        })
    }

    fn next_step(&self) -> ContinuationStep {
        for (result_index, result) in self.output.results.iter().enumerate() {
            if let Some(reason) = selector_stop_reason(result.status) {
                return ContinuationStep::Stop(reason);
            }
            let Some(selector) = result.continuation.as_ref() else {
                continue;
            };
            // A search continuation is another requested page, not an unfinished
            // fragment of the requested page. Preserve the caller's max_results.
            if result.status == ToolOutputSelectorStatus::Ok
                && selector.is_search()
            {
                continue;
            }
            if let ToolOutputSelector::Bytes { start, end } = selector
                && self.output.unavailable_ranges.iter().any(|range| {
                    *start < range.end && range.start < *end
                })
            {
                return ContinuationStep::Stop(ContinuationStopReason::IncompleteOwnerResult);
            }
            if self
                .followed_selectors
                .contains(&(self.result_owners[result_index], selector.clone()))
            {
                return ContinuationStep::Stop(ContinuationStopReason::RepeatedSelector);
            }
            return ContinuationStep::Follow {
                result_index,
                selector: selector.clone(),
            };
        }
        ContinuationStep::Complete
    }

    fn accept_page(
        &mut self,
        result_index: usize,
        selector: &ToolOutputSelector,
        page: ReadToolOutputResult,
    ) -> Result<(), ContinuationStopReason> {
        let Some(predecessor) = self.output.results.get(result_index) else {
            return Err(ContinuationStopReason::IncompleteOwnerResult);
        };
        if predecessor.continuation.as_ref() != Some(selector)
            || page.artifact_id != self.output.artifact_id
            || page.canonical_sha256 != self.output.canonical_sha256
        {
            return Err(ContinuationStopReason::IdentityDrift);
        }
        if page.results.len() != 1
            || &page.results[0].selector != selector
        {
            return Err(ContinuationStopReason::IncompleteOwnerResult);
        }
        if let Some(reason) = selector_stop_reason(page.results[0].status) {
            return Err(reason);
        }

        let previous_length = self.output.results.len();
        let previous_complete = self.output.complete;
        let previous_cost = self.result_costs[result_index];
        let predecessor = &mut self.output.results[result_index];
        let next_continuation = next_owner_continuation(predecessor, selector);
        let previous_continuation =
            std::mem::replace(&mut predecessor.continuation, next_continuation);
        let predecessor_was_complete = predecessor.complete;
        if predecessor.status == ToolOutputSelectorStatus::Ok {
            predecessor.complete = predecessor.continuation.is_none();
        }
        self.result_costs[result_index].serialized = previous_cost
            .serialized
            .subtract_delimited(continuation_size(previous_continuation.as_ref()))
            .subtract_delimited(recovery_size(&predecessor_was_complete))
            .add_delimited(continuation_size(predecessor.continuation.as_ref()))
            .add_delimited(recovery_size(&predecessor.complete));
        self.result_costs
            .extend(page.results.iter().map(RecoveryResultCost::new));
        // Keep already-drained pages in traversal order. Inserting every page
        // immediately after the owner reverses multi-page continuations.
        self.result_owners.extend(std::iter::repeat_n(
            self.result_owners[result_index],
            page.results.len(),
        ));
        self.output.results.extend(page.results);
        self.output.complete = self.output.results.iter().all(|result| {
                result.status == ToolOutputSelectorStatus::Ok
                    && result.complete
                    && result.continuation.is_none()
            });
        self.refresh_reconstruction();
        if self.projected_size(None).tokens() > self.token_ceiling {
            // Roll back only this page rather than copying every accumulated
            // page for each budget check.
            self.output.results.truncate(previous_length);
            self.result_owners.truncate(previous_length);
            self.output.complete = previous_complete;
            let predecessor = &mut self.output.results[result_index];
            predecessor.continuation = previous_continuation;
            predecessor.complete = predecessor_was_complete;
            self.rollback_costs(previous_length, result_index, previous_cost);
            return Err(ContinuationStopReason::Budget);
        }

        self.followed_selectors
            .push((self.result_owners[result_index], selector.clone()));
        self.checkpoints.push(RecoveryCheckpoint {
            index: result_index,
            selector: selector.clone(),
            length: previous_length,
            complete: previous_complete,
            continuation: previous_continuation,
            owner_complete: predecessor_was_complete,
            owner_cost: previous_cost,
        });
        self.drained_continuation_pages = self.drained_continuation_pages.saturating_add(1);
        Ok(())
    }

    #[cfg(test)]
    fn reconstructed_output(&self) -> ReadToolOutputResult {
        // Replace overflow descriptors only after exact, gap-free coverage is
        // proven. Appended byte pages are transport fragments, not extra selections.
        if self.output.unavailable_ranges.is_empty()
            && self.output.results[..self.initial_result_count]
                .iter()
                .any(|owner| owner.status != ToolOutputSelectorStatus::Ok)
        {
            let mut reconstructed = self.output.clone();
            let mut recovered_owners = Vec::new();
            for owner in &mut reconstructed.results[..self.initial_result_count] {
                if owner.status == ToolOutputSelectorStatus::Ok {
                    continue;
                }
                if let Some(exact) = reconstruct_selection(owner, &self.output.results) {
                    recovered_owners.push((exact.selector.clone(), exact.canonical_range));
                    *owner = exact;
                }
            }
            if !recovered_owners.is_empty() {
                // Only discard fragments represented by a reconstructed owner.
                // Other continuation pages and explicit search pages remain intact.
                let mut index = 0;
                reconstructed.results.retain(|page| {
                    let keep = index < self.initial_result_count
                        || !recovered_owners.iter().any(|(selector, range)| {
                            page.selector == *selector
                                || (matches!(page.selector, ToolOutputSelector::Bytes { .. })
                                    && range.zip(page.canonical_range).is_some_and(
                                        |(owner, page)| {
                                            owner.start <= page.start && page.end <= owner.end
                                        },
                                    ))
                        });
                    index += 1;
                    keep
                });
                reconstructed.complete = reconstructed.results.iter().all(|r| {
                    r.status == ToolOutputSelectorStatus::Ok
                        && r.complete
                        && r.continuation.is_none()
                });
                return reconstructed;
            }
        }
        self.output.clone()
    }

    // Charge unrelated selections and already accepted payload before reading
    // another page. The active owner's overflow descriptor and fragment
    // envelopes will be replaced by its exact selection.
    fn page_token_ceiling(&self, result_index: usize) -> usize {
        let owner = self.result_owners[result_index];
        let occupied = self
            .result_costs
            .iter()
            .enumerate()
            .map(|(index, cost)| {
                if self.result_owners[index] != owner {
                    cost.serialized.tokens()
                } else {
                    cost.payload_tokens
                }
            })
            .sum::<usize>();
        self.token_ceiling.saturating_sub(occupied)
    }

    fn cached_page(&self, selector: &ToolOutputSelector) -> Option<ReadToolOutputResult> {
        let mut owner = self.output.results.first()?.clone();
        owner.selector = selector.clone();
        owner.canonical_range = match selector {
            ToolOutputSelector::Bytes { start, end } => {
                Some(codex_tools::CanonicalByteRange::new(*start, *end))
            }
            _ => None,
        };
        if !selection_has_coverage(&owner, &self.output.results) {
            return None;
        }
        let exact = reconstruct_selection(&owner, &self.output.results)?;
        Some(ReadToolOutputResult {
            artifact_id: self.output.artifact_id.clone(),
            canonical_sha256: self.output.canonical_sha256.clone(),
            canonical_bytes: self.output.canonical_bytes,
            retained_bytes: self.output.retained_bytes,
            unavailable_ranges: self.output.unavailable_ranges.clone(),
            results: vec![exact],
            complete: true,
        })
    }

    fn finish(mut self) -> DrainedRecoveryTransaction {
        loop {
            if self
                .projected_size(self.continuation_stop.as_ref())
                .tokens()
                <= self.token_ceiling
            {
                self.materialize_reconstruction();
                break;
            }
            let Some(checkpoint) = self.checkpoints.pop() else {
                // Error text is optional metadata; recovered source bytes remain exact.
                if let Some(stop) = self.continuation_stop.as_mut() {
                    stop.message = None;
                }
                break;
            };
            self.output.results.truncate(checkpoint.length);
            self.result_owners.truncate(checkpoint.length);
            self.output.complete = checkpoint.complete;
            self.output.results[checkpoint.index].continuation = checkpoint.continuation;
            self.output.results[checkpoint.index].complete = checkpoint.owner_complete;
            self.rollback_costs(checkpoint.length, checkpoint.index, checkpoint.owner_cost);
            self.followed_selectors.pop();
            self.drained_continuation_pages = self.drained_continuation_pages.saturating_sub(1);
            self.record_stop(ContinuationStopReason::Budget, Some(checkpoint.selector));
        }
        merge_adjacent_recovery_pages(&mut self.output.results);
        DrainedRecoveryTransaction {
            output: self.output,
            drained_continuation_pages: self.drained_continuation_pages,
            continuation_stop: self.continuation_stop,
        }
    }

    fn rollback_costs(&mut self, length: usize, index: usize, cost: RecoveryResultCost) {
        self.result_costs.truncate(length);
        self.result_costs[index] = cost;
        for cached in &mut self.reconstructed {
            if cached
                .as_ref()
                .is_some_and(|cached| cached.source_length > length)
            {
                *cached = None;
            }
        }
        for (index, cursor) in self.covered_until.iter_mut().enumerate() {
            *cursor = self.output.results[index]
                .canonical_range
                .map_or(0, |range| range.start);
        }
        self.refresh_reconstruction();
    }

    fn refresh_reconstruction(&mut self) {
        for (index, owner) in self.output.results[..self.initial_result_count]
            .iter()
            .enumerate()
        {
            if owner.status == ToolOutputSelectorStatus::Ok {
                continue;
            }
            let direct_index = self.output.results.iter().position(|page| {
                page.selector == owner.selector
                    && page.status == ToolOutputSelectorStatus::Ok
                    && page.complete
                    && page.continuation.is_none()
            });
            if self.reconstructed[index]
                .as_ref()
                .is_some_and(|cached| cached.direct_index == direct_index)
                || !selection_has_coverage_from(
                    owner,
                    &self.output.results,
                    &mut self.covered_until[index],
                )
            {
                continue;
            }
            if let Some(result) = reconstruct_selection(owner, &self.output.results) {
                self.reconstructed[index] = Some(ReconstructedSelection {
                    cost: RecoveryResultCost::new(&result),
                    result,
                    source_length: self.output.results.len(),
                    direct_index,
                });
            }
        }
    }

    fn fragment_is_reconstructed(&self, index: usize) -> bool {
        if index < self.initial_result_count {
            return false;
        }
        let page = &self.output.results[index];
        self.reconstructed.iter().flatten().any(|cached| {
            let owner = &cached.result;
            page.selector == owner.selector
                || (matches!(page.selector, ToolOutputSelector::Bytes { .. })
                    && owner.canonical_range.zip(page.canonical_range).is_some_and(
                        |(owner, page)| owner.start <= page.start && page.end <= owner.end,
                    ))
        })
    }

    fn projected_size(&self, stop: Option<&RecoveryContinuationStopV1>) -> TokenCountEstimate {
        let mut size = TokenCountEstimate::default();
        let mut count = 0;
        let mut complete = true;
        for (index, raw) in self.output.results.iter().enumerate() {
            if self.fragment_is_reconstructed(index) {
                continue;
            }
            let (result, cost) = self
                .reconstructed
                .get(index)
                .and_then(Option::as_ref)
                .map_or((raw, &self.result_costs[index]), |cached| {
                    (&cached.result, &cached.cost)
                });
            if count > 0 {
                size = size.add_delimited(TokenCountEstimate::new(","));
            }
            size = size.add_delimited(cost.serialized);
            count += 1;
            complete &= result.status == ToolOutputSelectorStatus::Ok
                && result.complete
                && result.continuation.is_none();
        }
        if !self.reconstructed.iter().any(Option::is_some) {
            complete = self.output.complete;
        }
        size = size.add_delimited(self.envelope_costs[usize::from(complete)]);
        if let Some(stop) = stop {
            size = size
                .add_delimited(TokenCountEstimate::new(",\"continuation_stop\":"))
                .add_delimited(recovery_size(stop));
        }
        size
    }

    fn materialize_reconstruction(&mut self) {
        if !self.reconstructed.iter().any(Option::is_some) {
            return;
        }
        let keep = (0..self.output.results.len())
            .map(|index| !self.fragment_is_reconstructed(index))
            .collect::<Vec<_>>();
        self.output.results = std::mem::take(&mut self.output.results)
            .into_iter()
            .enumerate()
            .filter_map(|(index, result)| {
                keep[index].then(|| {
                    self.reconstructed
                        .get_mut(index)
                        .and_then(Option::take)
                        .map_or(result, |cached| cached.result)
                })
            })
            .collect();
        self.output.complete = self.output.results.iter().all(|result| {
            result.status == ToolOutputSelectorStatus::Ok
                && result.complete
                && result.continuation.is_none()
        });
    }
}

fn merge_adjacent_recovery_pages(results: &mut Vec<ToolOutputSelectorResult>) {
    let mut merged: Vec<ToolOutputSelectorResult> = Vec::with_capacity(results.len());
    for page in std::mem::take(results) {
        if let Some(previous) = merged.last_mut()
            && previous.status == ToolOutputSelectorStatus::Ok
            && page.status == ToolOutputSelectorStatus::Ok
            && previous.complete && page.complete
            && previous.continuation.is_none() && page.continuation.is_none()
            && matches!(previous.selector, ToolOutputSelector::Bytes { .. })
            && matches!(page.selector, ToolOutputSelector::Bytes { .. })
            && let (Some(left), Some(right)) = (previous.canonical_range, page.canonical_range)
            && left.end == right.start
            && let (Some(text), Some(next)) = (&mut previous.text, &page.text)
        {
            text.push_str(next);
            previous.canonical_range = Some(codex_tools::CanonicalByteRange::new(left.start, right.end));
            previous.selector = ToolOutputSelector::Bytes { start: left.start, end: right.end };
            previous.exact_bytes = Some(right.end - left.start);
            previous.child_selectors.clear();
            previous.subdivision_plan = None;
            previous.message = None;
        } else {
            merged.push(page);
        }
    }
    *results = merged;
}

// Check only range metadata until coverage is complete. Incomplete owners must
// not repeatedly decode/copy all previously accepted fragments.
fn selection_has_coverage(
    owner: &ToolOutputSelectorResult,
    pages: &[ToolOutputSelectorResult],
) -> bool {
    let mut cursor = owner.canonical_range.map_or(0, |range| range.start);
    selection_has_coverage_from(owner, pages, &mut cursor)
}

fn selection_has_coverage_from(
    owner: &ToolOutputSelectorResult,
    pages: &[ToolOutputSelectorResult],
    cursor: &mut u64,
) -> bool {
    if pages.iter().any(|page| {
        page.selector == owner.selector
            && page.status == ToolOutputSelectorStatus::Ok
            && page.complete
            && page.continuation.is_none()
    }) {
        return true;
    }
    let Some(range) = owner.canonical_range else {
        return false;
    };
    while *cursor < range.end {
        let next = pages
            .iter()
            .filter(|page| {
                page.status == ToolOutputSelectorStatus::Ok
                    && page.complete
                    && page.continuation.is_none()
                    && (page.text.is_some() || page.data_base64.is_some())
            })
            .filter_map(|page| page.canonical_range)
            .filter(|page| page.start <= *cursor && *cursor < page.end)
            .map(|page| page.end)
            .max();
        let Some(next) = next else {
            return false;
        };
        *cursor = next;
    }
    true
}

fn reconstruct_selection(
    owner: &ToolOutputSelectorResult,
    pages: &[ToolOutputSelectorResult],
) -> Option<ToolOutputSelectorResult> {
    if let Some(page) = pages.iter().find(|page| {
        page.selector == owner.selector
            && page.status == ToolOutputSelectorStatus::Ok
            && page.complete
            && page.continuation.is_none()
    }) {
        return Some(page.clone());
    }
    let range = owner.canonical_range?;
    let mut pages = pages
        .iter()
        .filter(|page| {
            page.status == ToolOutputSelectorStatus::Ok
                && page.complete
                && page.continuation.is_none()
                // JSON values and search metadata are not canonical byte
                // fragments, even when their selected ranges overlap.
                && (page.text.is_some() || page.data_base64.is_some())
                && page
                    .canonical_range
                    .is_some_and(|r| r.start < range.end && range.start < r.end)
        })
        .collect::<Vec<_>>();
    pages.sort_by_key(|page| page.canonical_range.map(|r| r.start));
    let mut cursor = range.start;
    let mut bytes = Vec::new();
    for page in pages {
        let page_range = page.canonical_range?;
        if page_range.end <= cursor {
            continue;
        }
        if page_range.start > cursor {
            return None;
        }
        let fragment = if let Some(text) = &page.text {
            text.as_bytes().to_vec()
        } else {
            base64::engine::general_purpose::STANDARD
                .decode(page.data_base64.as_ref()?)
                .ok()?
        };
        if fragment.len() as u64 != page_range.end.checked_sub(page_range.start)? {
            return None;
        }
        let end = page_range.end.min(range.end);
        bytes.extend_from_slice(
            &fragment[usize::try_from(cursor - page_range.start).ok()?
                ..usize::try_from(end - page_range.start).ok()?],
        );
        cursor = end;
    }
    if cursor != range.end || bytes.len() as u64 != range.end.checked_sub(range.start)? {
        return None;
    }
    let mut result = owner.clone();
    result.status = ToolOutputSelectorStatus::Ok;
    result.complete = true;
    result.exact_bytes = Some(bytes.len() as u64);
    result.continuation = None;
    result.child_selectors.clear();
    result.subdivision_plan = None;
    result.message = None;
    result.text = None;
    result.value = None;
    result.data_base64 = None;
    if matches!(owner.selector, ToolOutputSelector::JsonPointer { .. }) {
        result.value = Some(serde_json::from_slice(&bytes).ok()?);
    } else if let Ok(text) = String::from_utf8(bytes.clone()) {
        result.text = Some(text);
    } else {
        result.data_base64 = Some(base64::engine::general_purpose::STANDARD.encode(bytes));
    }
    Some(result)
}

fn selector_stop_reason(status: ToolOutputSelectorStatus) -> Option<ContinuationStopReason> {
    match status {
        ToolOutputSelectorStatus::Invalid => Some(ContinuationStopReason::InvalidSelector),
        ToolOutputSelectorStatus::NotFound => Some(ContinuationStopReason::SelectorNotFound),
        ToolOutputSelectorStatus::Ok
        | ToolOutputSelectorStatus::SelectorTooLarge
        | ToolOutputSelectorStatus::AggregateOmitted => None,
    }
}

fn next_owner_continuation(
    predecessor: &ToolOutputSelectorResult,
    consumed: &ToolOutputSelector,
) -> Option<ToolOutputSelector> {
    if let (
        Some(plan),
        ToolOutputSelector::Bytes {
            end: consumed_end, ..
        },
    ) = (predecessor.subdivision_plan.as_ref(), consumed)
        && *consumed_end < plan.range.end
    {
        return Some(ToolOutputSelector::Bytes {
            start: *consumed_end,
            end: consumed_end
                .saturating_add(plan.chunk_bytes.max(1))
                .min(plan.range.end),
        });
    }

    predecessor
        .child_selectors
        .iter()
        .position(|child| child == consumed)
        .and_then(|index| predecessor.child_selectors.get(index.saturating_add(1)))
        .cloned()
        .or_else(|| {
            // Large or nonuniform ranges intentionally have no uniform plan.
            // Re-select their suffix to compute its next bounded UTF-8 page;
            // exhausting the first advertised child is not owner completion.
            let ToolOutputSelector::Bytes { end, .. } = consumed else {
                return None;
            };
            let range = predecessor.canonical_range?;
            (*end < range.end).then_some(ToolOutputSelector::Bytes {
                start: *end,
                end: range.end,
            })
        })
}

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
    ranges: Option<Vec<ReadToolOutputRangeArgs>>,
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
    exact_recovery: Option<TurnTimingDeterministicContinuationReceipt>,
}

impl ToolOutput for ReadToolOutputToolOutput {
    fn log_preview(&self) -> String {
        self.inner.log_preview()
    }

    fn success_for_logging(&self) -> bool {
        self.inner.success_for_logging()
    }

    fn sampling_request_signal(&self) -> Option<Value> {
        self.inner.sampling_request_signal()
    }

    fn deterministic_continuation_receipts(
        &self,
    ) -> Vec<TurnTimingDeterministicContinuationReceipt> {
        self.exact_recovery
            .as_ref()
            .map(|receipt| vec![receipt.clone()])
            .unwrap_or_default()
    }

    fn deterministic_continuation_content(&self) -> Vec<Value> {
        self.exact_recovery
            .as_ref()
            .map(|_| vec![self.inner.value().clone()])
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

impl CoreToolRuntime for ReadToolOutputHandler {
    fn cancellation_cleanup_policy(&self) -> crate::tools::registry::ToolCleanupPolicy {
        crate::tools::registry::ToolCleanupPolicy::InterruptibleRead
    }

    fn terminal_failure_reuse(&self) -> crate::tools::registry::TerminalFailureReuse {
        crate::tools::registry::TerminalFailureReuse::RequestRevisionAndJsonSyntax
    }
}

async fn handle_read_tool_output(
    invocation: ToolInvocation,
) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
    let ToolPayload::Function { ref arguments } = invocation.payload else {
        return Err(FunctionCallError::RespondToModel(
            "read_tool_output received unsupported payload".to_string(),
        ));
    };
    let args = parse_read_tool_output_args(arguments)?;
    let selectors = resolved_selectors(&args)?;
    let code_mode_recovery = matches!(&invocation.source, ToolCallSource::CodeMode { .. });
    let max_bytes = if code_mode_recovery
        && args.max_bytes.is_some_and(|bytes| bytes > READ_TOOL_OUTPUT_MAX_BYTES)
    {
        args.max_bytes.unwrap_or_default().min(READ_TOOL_OUTPUT_SCRIPT_MAX_BYTES)
    } else {
        resolved_max_bytes(args.max_bytes)?
    };
    let mut action_bounds_digest = Sha256::new();
    serde_json::to_writer(&mut action_bounds_digest, &selectors).map_err(|err| {
        FunctionCallError::RespondToModel(format!("failed to serialize recovery selectors: {err}"))
    })?;
    let action_bounds_hash = format!("{:x}", action_bounds_digest.finalize());
    // JavaScript consumes exact data before choosing what to print. Coupling
    // this selection to its display budget makes a small summary require extra
    // recovery calls. Larger exact script selections are explicit opt-ins;
    // direct reads and the default retain their existing display-sized caps.
    let token_ceiling = if code_mode_recovery && max_bytes > READ_TOOL_OUTPUT_MAX_BYTES {
        // Approximate token costs use UTF-8 bytes / 4. Leave ample space for
        // escaping, continuation metadata and the retry-avoidance margin.
        (READ_TOOL_OUTPUT_SCRIPT_MAX_BYTES - 128 * 1024) / 4
    } else if code_mode_recovery {
        CODE_MODE_RECOVERY_TOKEN_CEILING
    } else {
        RECOVERY_AGGREGATE_TOKEN_CEILING
    };
    let snapshot = load_tool_output_snapshot(
        invocation.step_context.turn.config.codex_home.as_path(),
        &invocation.session.thread_id.to_string(),
        &args.artifact_id,
    ).await.map_err(|err| FunctionCallError::RespondToModel(err.for_model()))?;
    let transaction = drain_recovery_snapshot_with_byte_limit(
        &snapshot, selectors, token_ceiling, max_bytes, &invocation.cancellation_token,
    ).await
    .map_err(|err| FunctionCallError::RespondToModel(err.for_model()))?;
    let DrainedRecoveryTransaction {
        output,
        drained_continuation_pages,
        continuation_stop,
    } = transaction;
    invocation.step_context.turn.turn_timing_state.record_tool_output_artifact_reread();
    // Typed overflow reports an incomplete selection without losing retained
    // evidence. Recovery bypasses recursive spilling, so no retruncation occurs.
    // This is an explicit tool invocation, even when wrapped in model-authored
    // JS. Nesting alone does not prove recovery avoided a model handoff.
    // Automatic continuation pages are tracked separately by the transaction.
    invocation
        .step_context
        .turn
        .turn_timing_state
        .record_tool_output_recovery(/*retruncation_count*/ 0);

    let exact_recovery_receipt = exact_code_mode_recovery_receipt(
        code_mode_recovery,
        &output,
        action_bounds_hash,
        drained_continuation_pages,
    );
    let successful = output.results.iter().any(|result| result.status == ToolOutputSelectorStatus::Ok);
    let evidence = output.delivered_evidence().map(|identity| serde_json::json!({
        "source": "artifact",
        "scope": output.artifact_id,
        "identity": identity,
    }));
    if successful {
        let selectors = output.delivered_ranges().into_iter()
            .map(|(start, end)| serde_json::json!({"kind": "bytes", "start": start, "end": end}))
            .collect();
        invocation.session
            .record_tool_history_recovery(args.artifact_id.clone(), invocation.call_id.clone(), selectors).await;
    }
    let output = recovery_envelope(&output, continuation_stop.as_ref()).map_err(|err| {
        FunctionCallError::RespondToModel(format!("failed to serialize recovery result: {err}"))
    })?;
    Ok(boxed_tool_output(ReadToolOutputToolOutput {
        inner: recovery_tool_output(output, successful, evidence),
        exact_recovery: exact_recovery_receipt,
    }))
}

fn recovery_tool_output(output: Value, successful: bool, evidence: Option<Value>) -> JsonToolOutput {
    let projected = codex_code_mode::model_visible_tool_result(
        &ToolName::plain(READ_TOOL_OUTPUT_TOOL_NAME), &output,
    );
    let mut result = JsonToolOutput::with_success(output, Some(successful));
    if let Some(projected) = projected {
        result = result.with_model_value(projected);
    }
    if successful {
        if let Some(evidence) = evidence {
            result = result.with_sampling_request_signal(
                crate::tools::context::semantic_evidence_sampling_signal(evidence),
            );
        }
    } else {
        let statuses = result.value()["results"].as_array().into_iter().flatten()
            .filter_map(|result| result["status"].as_str())
            .collect::<std::collections::BTreeSet<_>>();
        let mut signal = crate::tools::context::semantic_failure_sampling_signal(serde_json::json!({
            "artifact": result.value()["canonical_sha256"],
            "statuses": statuses,
        }));
        // Rejection is diagnostic, not successful source coverage.
        if let Some(signal) = signal.as_object_mut() {
            signal.remove("semantic_evidence");
        }
        result = result.with_sampling_request_signal(signal);
    }
    result
}

fn parse_read_tool_output_args(arguments: &str) -> Result<ReadToolOutputArgs, FunctionCallError> {
    parse_arguments(arguments).map_err(|err| match err {
        FunctionCallError::RespondToModel(message) => {
            let detail = message
                .strip_prefix("failed to parse function arguments: ")
                .unwrap_or(&message);
            FunctionCallError::RespondToModel(format!(
                "failed to parse read_tool_output arguments: {detail}. Consult the advertised read_tool_output schema."
            ))
        }
        err => err,
    })
}

#[cfg(test)]
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

#[cfg(test)]
pub(crate) async fn execute_recovery_transaction_with_continuations(
    codex_home: &Path,
    thread_id: &str,
    artifact_id: &str,
    selectors: Vec<ToolOutputSelector>,
    code_mode_recovery: bool,
    cancellation_token: &CancellationToken,
) -> Result<DrainedRecoveryTransaction, ReadToolOutputError> {
    let token_ceiling = if code_mode_recovery {
        CODE_MODE_RECOVERY_TOKEN_CEILING
    } else {
        RECOVERY_AGGREGATE_TOKEN_CEILING
    };
    let snapshot = load_tool_output_snapshot(codex_home, thread_id, artifact_id).await?;
    drain_recovery_snapshot(&snapshot, selectors, token_ceiling, cancellation_token).await
}

async fn drain_recovery_snapshot_with_byte_limit(
    snapshot: &std::sync::Arc<ToolOutputSnapshot>,
    selectors: Vec<ToolOutputSelector>,
    mut token_ceiling: usize,
    max_bytes: usize,
    cancellation_token: &CancellationToken,
) -> Result<DrainedRecoveryTransaction, ReadToolOutputError> {
    loop {
        let result = drain_recovery_snapshot(
            snapshot, selectors.clone(), token_ceiling, cancellation_token,
        ).await?;
        let delivered_bytes: u64 = result.output.delivered_ranges().iter()
            .map(|(start, end)| end - start).sum();
        let envelope_bytes = recovery_envelope(&result.output, result.continuation_stop.as_ref())
            .and_then(|value| serde_json::to_vec(&value))
            .map_err(|error| ReadToolOutputError::Io(error.to_string()))?.len();
        if delivered_bytes <= max_bytes as u64
            && envelope_bytes <= READ_TOOL_OUTPUT_SCRIPT_MAX_BYTES
        {
            return Ok(result);
        }
        // Refit against the same authenticated snapshot, never rerun or reread
        // the producer. Preserve the selector engine's exact ranges/continuation.
        let smaller = (token_ceiling.saturating_mul(max_bytes)
            / usize::try_from(delivered_bytes).unwrap_or(usize::MAX).max(1))
            .min(token_ceiling.saturating_mul(READ_TOOL_OUTPUT_SCRIPT_MAX_BYTES)
                / envelope_bytes.max(1));
        if smaller >= token_ceiling || smaller < 256 {
            return Err(ReadToolOutputError::InvalidRange(
                "max_bytes is too small for an exact recovery page; increase it or request a smaller selector".to_string(),
            ));
        }
        token_ceiling = smaller;
    }
}

async fn drain_recovery_snapshot(
    snapshot: &std::sync::Arc<ToolOutputSnapshot>,
    selectors: Vec<ToolOutputSelector>,
    token_ceiling: usize,
    cancellation_token: &CancellationToken,
) -> Result<DrainedRecoveryTransaction, ReadToolOutputError> {
    let token_ceiling = token_ceiling.saturating_add(
        crate::tools::command_output_artifact::RECOVERY_RETRY_AVOIDANCE_TOKEN_MARGIN,
    );
    let complete_output = snapshot.select(selectors.clone(), token_ceiling).await?;
    if complete_output.complete
        && recovery_envelope_fits(&complete_output, None, token_ceiling)
    {
        return Ok(RecoveryContinuationState::new(complete_output, token_ceiling).finish());
    }
    // Reserve stop metadata, including caller-supplied selectors. Byte continuations
    // fit within the fixed allowance; arbitrary error text is checked at finalization.
    let reserve = selectors
        .iter()
        .filter_map(|selector| serde_json::to_string(selector).ok())
        .map(|text| codex_utils_string::approx_token_count(&text))
        .max()
        .unwrap_or_default()
        .saturating_add(256);
    let output = snapshot
        .select(selectors, token_ceiling.saturating_sub(reserve))
        .await?;
    // Loading this transaction did perform an artifact read. Following its
    // pages reuses that same identity-checked observation, without more I/O.
    let mut state = RecoveryContinuationState::new(output, token_ceiling);
    loop {
        let (result_index, selector) = match state.next_step() {
            ContinuationStep::Complete => break,
            ContinuationStep::Stop(reason) => {
                let selector = state.first_pending_selector();
                state.record_stop(reason, selector);
                break;
            }
            ContinuationStep::Follow {
                result_index,
                selector,
            } => (result_index, selector),
        };
        if cancellation_token.is_cancelled() {
            state.record_stop(ContinuationStopReason::Cancelled, Some(selector));
            break;
        }
        let page = if let Some(page) = state.cached_page(&selector) {
            Ok(page)
        } else {
            let ceiling = state.page_token_ceiling(result_index);
            if ceiling == 0 {
                state.record_stop(ContinuationStopReason::Budget, Some(selector));
                break;
            }
            snapshot
                .select(vec![selector.clone()], ceiling)
                .await
        };
        if cancellation_token.is_cancelled() {
            state.record_stop(ContinuationStopReason::Cancelled, Some(selector));
            break;
        }
        let page = match page {
            Ok(page) => page,
            Err(error) => {
                state.record_page_read_error(&error, selector);
                break;
            }
        };
        if let Err(reason) = state.accept_page(result_index, &selector, page) {
            state.record_stop(reason, Some(selector));
            break;
        }
    }
    // Only incomplete reads need a page directory. Reserving it before draining
    // would consume the retry-avoidance margin on nearly complete selections.
    if token_ceiling >= 2_000
        && state.continuation_stop.as_ref().is_some_and(|stop| {
            stop.reason == ContinuationStopReason::Budget
        })
    {
        state.token_ceiling = token_ceiling.saturating_sub(256);
    }
    let mut result = state.finish();
    if let Some(stop) = result.continuation_stop.as_mut()
        && stop.reason == ContinuationStopReason::Budget
        && let Some(ToolOutputSelector::Bytes { start, end }) = &stop.selector
    {
        stop.page_selectors = snapshot.page_selectors(*start, *end, token_ceiling.saturating_sub(1_000));
    }
    while !recovery_envelope_fits(&result.output, result.continuation_stop.as_ref(), token_ceiling) {
        let Some(stop) = result.continuation_stop.as_mut() else { break };
        if stop.page_selectors.pop().is_none() { break; }
    }
    if !recovery_envelope_fits(
        &result.output,
        result.continuation_stop.as_ref(),
        token_ceiling,
    ) {
        return Err(ReadToolOutputError::InvalidRange(
            "Recovery metadata exceeds the output budget; request fewer or smaller selectors."
                .to_string(),
        ));
    }
    Ok(result)
}

#[cfg(test)]
fn recovery_result_fits_token_ceiling(output: &ReadToolOutputResult, token_ceiling: usize) -> bool {
    serde_json::to_string(output)
        .is_ok_and(|rendered| codex_utils_string::approx_token_count(&rendered) <= token_ceiling)
}

fn exact_code_mode_recovery_receipt(
    code_mode_recovery: bool,
    output: &crate::tools::command_output_artifact::ReadToolOutputResult,
    action_bounds_hash: String,
    suppressed_continuation_count: u32,
) -> Option<TurnTimingDeterministicContinuationReceipt> {
    (code_mode_recovery
        && suppressed_continuation_count > 0
        && output.unavailable_ranges.is_empty()
        && !output.results.is_empty()
        && output.results.iter().all(|result| {
            matches!(
                result.status,
                ToolOutputSelectorStatus::Ok
                    | ToolOutputSelectorStatus::SelectorTooLarge
                    | ToolOutputSelectorStatus::AggregateOmitted
            )
        }))
    .then(|| TurnTimingDeterministicContinuationReceipt {
        class: DeterministicContinuationClass::ArtifactRange,
        wire_identity: String::new(),
        resource_identity_hash: crate::tool_history::sha256(output.artifact_id.as_bytes()),
        state_revision: output.canonical_sha256.clone(),
        host_action: DeterministicContinuationHostAction::DrainArtifactRanges,
        action_bounds_hash,
        suppressed_continuation_count,
    })
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
        if selectors.len() > READ_TOOL_OUTPUT_MAX_SELECTORS {
            return Err(FunctionCallError::RespondToModel(format!(
                "selectors may contain at most {READ_TOOL_OUTPUT_MAX_SELECTORS} entries"
            )));
        }
        if args.start_line.is_some() || args.end_line.is_some() || args.ranges.is_some() {
            return Err(FunctionCallError::RespondToModel(
                "selectors cannot be combined with legacy line arguments".to_string(),
            ));
        }
        return Ok(selectors.clone());
    }
    if let Some(ranges) = &args.ranges {
        if args.start_line.is_some() || args.end_line.is_some() {
            return Err(FunctionCallError::RespondToModel(
                "ranges is mutually exclusive with start_line/end_line".to_string(),
            ));
        }
        if ranges.is_empty() {
            return Err(FunctionCallError::RespondToModel(
                "ranges must contain at least one range".to_string(),
            ));
        }
        if ranges.len() > READ_TOOL_OUTPUT_MAX_LEGACY_RANGES {
            return Err(FunctionCallError::RespondToModel(format!(
                "ranges may contain at most {READ_TOOL_OUTPUT_MAX_LEGACY_RANGES} entries"
            )));
        }
        let ranges = ranges
            .iter()
            .map(|range| {
                if range.start_line == 0 || range.end_line < range.start_line {
                    Err(FunctionCallError::RespondToModel(
                        "each range requires 1-based start_line <= end_line".to_string(),
                    ))
                } else {
                    Ok((range.start_line, range.end_line))
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        let normalized = crate::tools::command_output_artifact::normalize_line_ranges(ranges);
        let aggregate_lines = normalized.iter().try_fold(0_usize, |total, (start, end)| {
            total.checked_add(end - start + 1)
        });
        if aggregate_lines.is_none_or(|lines| lines > MAX_AGGREGATE_LINES) {
            return Err(FunctionCallError::RespondToModel(format!(
                "ranges may request at most {MAX_AGGREGATE_LINES} aggregate lines"
            )));
        }
        return Ok(normalized
            .into_iter()
            .map(|(start, end)| ToolOutputSelector::Lines { start, end })
            .collect());
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
    if start_line == 0 || end_line < start_line {
        return Err(FunctionCallError::RespondToModel(
            "line ranges require 1-based start_line <= end_line".to_string(),
        ));
    }
    Ok((start_line, end_line))
}

fn resolved_max_bytes(max_bytes: Option<usize>) -> Result<usize, FunctionCallError> {
    match max_bytes {
        Some(0) => {
            Err(FunctionCallError::RespondToModel(format!(
                "max_bytes must be between 1 and {READ_TOOL_OUTPUT_MAX_BYTES}"
            )))
        }
        Some(max_bytes) => Ok(max_bytes.min(READ_TOOL_OUTPUT_MAX_BYTES)),
        None => Ok(READ_TOOL_OUTPUT_MAX_BYTES),
    }
}

#[cfg(test)]
#[path = "read_tool_output_tests.rs"]
mod tests;
#[cfg(test)]
#[path = "read_tool_output_paging_tests.rs"]
mod paging_regressions;
