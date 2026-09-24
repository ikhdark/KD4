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
use crate::tools::context::semantic_evidence_for_command_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::handlers::read_tool_output_spec::READ_TOOL_OUTPUT_MAX_BYTES;
use crate::tools::handlers::read_tool_output_spec::READ_TOOL_OUTPUT_MAX_LEGACY_RANGES;
use crate::tools::handlers::read_tool_output_spec::READ_TOOL_OUTPUT_MAX_SELECTORS;
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
    reused: bool,
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
    reused: bool,
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
    fn new(mut output: ReadToolOutputResult, reused: bool, token_ceiling: usize) -> Self {
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
            reused,
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
        if !self.output.unavailable_ranges.is_empty() {
            return ContinuationStep::Stop(ContinuationStopReason::IncompleteOwnerResult);
        }
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
                && matches!(selector, ToolOutputSelector::Search { .. })
            {
                continue;
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
        page_reused: bool,
    ) -> Result<(), ContinuationStopReason> {
        self.reused &= page_reused;
        let Some(predecessor) = self.output.results.get(result_index) else {
            return Err(ContinuationStopReason::IncompleteOwnerResult);
        };
        if predecessor.continuation.as_ref() != Some(selector)
            || page.artifact_id != self.output.artifact_id
            || page.canonical_sha256 != self.output.canonical_sha256
        {
            return Err(ContinuationStopReason::IdentityDrift);
        }
        if !page.unavailable_ranges.is_empty()
            || page.results.len() != 1
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
        self.output.complete = self.output.unavailable_ranges.is_empty()
            && self.output.results.iter().all(|result| {
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
        DrainedRecoveryTransaction {
            output: self.output,
            reused: self.reused,
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
        if !self.output.unavailable_ranges.is_empty() {
            return;
        }
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

impl CoreToolRuntime for ReadToolOutputHandler {}

async fn handle_read_tool_output(
    invocation: ToolInvocation,
) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
    let ToolPayload::Function { ref arguments } = invocation.payload else {
        return Err(FunctionCallError::RespondToModel(
            "read_tool_output received unsupported payload".to_string(),
        ));
    };
    let args = parse_read_tool_output_args(arguments)?;
    // Keep validating the legacy knob for compatibility, but never use it to
    // clip a selected value. The selector engine owns its exact response fit.
    let _legacy_max_bytes = resolved_max_bytes(args.max_bytes)?;
    let selectors = resolved_selectors(&args)?;
    let code_mode_recovery = matches!(&invocation.source, ToolCallSource::CodeMode { .. });
    let mut action_bounds_digest = Sha256::new();
    serde_json::to_writer(&mut action_bounds_digest, &selectors).map_err(|err| {
        FunctionCallError::RespondToModel(format!("failed to serialize recovery selectors: {err}"))
    })?;
    let action_bounds_hash = format!("{:x}", action_bounds_digest.finalize());
    let transaction = execute_recovery_transaction_with_continuations(
        invocation.step_context.turn.config.codex_home.as_path(),
        &invocation.session.thread_id.to_string(),
        &args.artifact_id,
        selectors,
        code_mode_recovery,
        &invocation.cancellation_token,
    )
    .await
    .map_err(|err| FunctionCallError::RespondToModel(err.for_model()))?;
    let DrainedRecoveryTransaction {
        output,
        reused,
        drained_continuation_pages,
        continuation_stop,
    } = transaction;
    if !reused {
        invocation
            .step_context
            .turn
            .turn_timing_state
            .record_tool_output_artifact_reread();
    }
    // Typed overflow reports an incomplete selection without losing retained
    // evidence. Recovery bypasses recursive spilling, so no retruncation occurs.
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
    let semantic_evidence = read_tool_output_semantic_evidence(&output, continuation_stop.as_ref());
    let output = recovery_envelope(&output, continuation_stop.as_ref()).map_err(|err| {
        FunctionCallError::RespondToModel(format!("failed to serialize recovery result: {err}"))
    })?;
    Ok(boxed_tool_output(ReadToolOutputToolOutput {
        inner: JsonToolOutput::new(output),
        exact_recovery: exact_recovery_receipt,
        semantic_evidence,
    }))
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

fn read_tool_output_semantic_evidence(
    output: &ReadToolOutputResult,
    continuation_stop: Option<&RecoveryContinuationStopV1>,
) -> Vec<String> {
    let mut recovered_fragments = output
        .results
        .iter()
        .filter(|result| result.status == ToolOutputSelectorStatus::Ok)
        .filter_map(|result| result.text.as_deref().map(|text| (result, text)))
        .peekable();
    if recovered_fragments.peek().is_some() {
        let mut evidence = Vec::new();
        let mut seen_facts = HashSet::new();
        for (result, recovered_fragment) in recovered_fragments {
            let fragment_facts =
                semantic_evidence_for_command_output(recovered_fragment.as_bytes());
            let provenance = serde_json::to_vec(&serde_json::json!({
                "canonical_sha256": output.canonical_sha256,
                "selector": result.selector,
                "canonical_range": result.canonical_range,
                "facts": fragment_facts,
            }))
            .unwrap_or_default();
            for fact in fragment_facts {
                if !seen_facts.contains(&fact) {
                    seen_facts.insert(fact.clone());
                    evidence.push(fact);
                }
            }
            evidence.push(format!(
                "artifact-recovery-fragment-v1:{}",
                crate::tool_history::sha256(&provenance)
            ));
        }
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
                "continuation_stop": continuation_stop,
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
        "continuation_stop": continuation_stop,
    }))
    .unwrap_or_default();
    vec![format!(
        "artifact-projection-v1:{}",
        crate::tool_history::sha256(&projection)
    )]
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

async fn drain_recovery_snapshot(
    snapshot: &std::sync::Arc<ToolOutputSnapshot>,
    selectors: Vec<ToolOutputSelector>,
    token_ceiling: usize,
    cancellation_token: &CancellationToken,
) -> Result<DrainedRecoveryTransaction, ReadToolOutputError> {
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
    let mut state = RecoveryContinuationState::new(output, false, token_ceiling);
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
            Ok((page, true))
        } else {
            let ceiling = state.page_token_ceiling(result_index);
            if ceiling == 0 {
                state.record_stop(ContinuationStopReason::Budget, Some(selector));
                break;
            }
            snapshot
                .select(vec![selector.clone()], ceiling)
                .await
                .map(|page| (page, true))
        };
        if cancellation_token.is_cancelled() {
            state.record_stop(ContinuationStopReason::Cancelled, Some(selector));
            break;
        }
        let (page, page_reused) = match page {
            Ok(page) => page,
            Err(error) => {
                state.record_page_read_error(&error, selector);
                break;
            }
        };
        if let Err(reason) = state.accept_page(result_index, &selector, page, page_reused) {
            state.record_stop(reason, Some(selector));
            break;
        }
    }
    let result = state.finish();
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
        Some(max_bytes) if max_bytes == 0 || max_bytes > READ_TOOL_OUTPUT_MAX_BYTES => {
            Err(FunctionCallError::RespondToModel(format!(
                "max_bytes must be between 1 and {READ_TOOL_OUTPUT_MAX_BYTES}"
            )))
        }
        Some(max_bytes) => Ok(max_bytes),
        None => Ok(READ_TOOL_OUTPUT_MAX_BYTES),
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    #[serial_test::serial(command_output_artifact)]
    async fn recovery_pages_use_one_validated_snapshot_without_reopening_the_artifact() {
        use crate::tools::command_output_artifact::create_canonical_output_artifact;

        let home = tempfile::tempdir().unwrap();
        let text = "recovery evidence\n".repeat(8_000);
        let canonical = CanonicalToolResult::text(&text);
        let artifact = create_canonical_output_artifact(home.path(), "snapshot", &canonical).await;
        let id = artifact.artifact_id().unwrap();
        let snapshot = load_tool_output_snapshot(home.path(), "snapshot", &id)
            .await
            .unwrap();
        let path = home
            .path()
            .join("tool-output/snapshot")
            .join(format!("{id}.log"));
        std::fs::remove_file(&path).unwrap();

        let cancelled = CancellationToken::new();
        cancelled.cancel();
        let stopped = drain_recovery_snapshot(
            &snapshot,
            vec![ToolOutputSelector::Bytes {
                start: 0,
                end: text.len() as u64,
            }],
            CODE_MODE_RECOVERY_TOKEN_CEILING,
            &cancelled,
        )
        .await
        .unwrap();
        assert_eq!(stopped.drained_continuation_pages, 0);
        assert!(!stopped.output.complete);
        assert_eq!(
            stopped.continuation_stop.unwrap().reason,
            ContinuationStopReason::Cancelled,
        );

        // Removing the backing file makes any hidden reopen fail. The production
        // continuation loop must still deliver authenticated pages from its
        // initial observation, and stop at the normal output ceiling.
        let recovered = drain_recovery_snapshot(
            &snapshot,
            vec![ToolOutputSelector::Bytes {
                start: 0,
                end: text.len() as u64,
            }],
            CODE_MODE_RECOVERY_TOKEN_CEILING,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(recovered.drained_continuation_pages > 0);
        assert!(
            !recovered.reused,
            "the initial disk read must remain accounted for"
        );
        assert!(!recovered.output.complete);
        assert_eq!(recovered.output.canonical_sha256, canonical.sha256);
        assert_eq!(
            recovered.continuation_stop.as_ref().unwrap().reason,
            ContinuationStopReason::Budget
        );
        let pages = recovered
            .output
            .results
            .iter()
            .filter(|part| part.text.is_some())
            .collect::<Vec<_>>();
        assert!(
            !pages.is_empty(),
            "recovery must return useful evidence, not just an overflow descriptor"
        );
        for page in pages {
            let range = page.canonical_range.unwrap();
            assert_eq!(
                page.text.as_deref().unwrap(),
                &text[range.start as usize..range.end as usize]
            );
        }

        // Reuse is scoped to this observation, never a cache that masks expiry
        // on the next invocation. Required disk validation still takes place.
        assert!(matches!(
            execute_recovery_transaction_with_continuations(
                home.path(),
                "snapshot",
                &id,
                vec![ToolOutputSelector::Lines { start: 1, end: 1 }],
                true,
                &CancellationToken::new(),
            )
            .await,
            Err(ReadToolOutputError::Expired),
        ));
    }

    #[tokio::test]
    #[serial_test::serial(command_output_artifact)]
    async fn new_recovery_transaction_revalidates_same_length_modified_bytes() {
        use crate::tools::command_output_artifact::create_canonical_output_artifact;

        let home = tempfile::tempdir().unwrap();
        let canonical = CanonicalToolResult::text("original evidence\n");
        let artifact = create_canonical_output_artifact(home.path(), "snapshot", &canonical).await;
        let id = artifact.artifact_id().unwrap();
        let selectors = vec![ToolOutputSelector::Lines { start: 1, end: 1 }];
        let first = execute_recovery_transaction_with_continuations(
            home.path(),
            "snapshot",
            &id,
            selectors.clone(),
            true,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(first.output.complete);
        assert_eq!(
            first.output.results[0].text.as_deref(),
            Some("original evidence\n")
        );

        std::fs::write(
            home.path()
                .join("tool-output/snapshot")
                .join(format!("{id}.log")),
            "modified evidence\n",
        )
        .unwrap();
        let error = execute_recovery_transaction_with_continuations(
            home.path(),
            "snapshot",
            &id,
            selectors,
            true,
            &CancellationToken::new(),
        )
        .await
        .err()
        .expect("new calls must authenticate disk contents again");
        assert_eq!(
            error,
            ReadToolOutputError::Io("artifact SHA identity does not match metadata".to_string())
        );
    }

    use super::*;
    use crate::tools::command_output_artifact::ByteSubdivisionPlan;

    #[tokio::test]
    async fn recovery_handler_output_schema_covers_exact_search_and_rejected_selectors() {
        let (session, turn) = crate::session::tests::make_session_and_context().await;
        let session = std::sync::Arc::new(session);
        let turn = std::sync::Arc::new(turn);
        let artifact = crate::tools::command_output_artifact::create_canonical_output_artifact(
            &turn.config.codex_home,
            &session.thread_id.to_string(),
            &CanonicalToolResult::json(serde_json::json!({"present": "value"})),
        )
        .await;
        let artifact_id = artifact
            .artifact_id()
            .expect("retained artifact")
            .to_string();
        let spec = ReadToolOutputHandler.spec();
        let ToolSpec::Function(spec) = spec else {
            panic!("recovery uses a function spec");
        };
        let validator =
            jsonschema::validator_for(spec.output_schema.as_ref().expect("output schema"))
                .expect("valid output schema");
        for (selector, expected_status, expected_reason) in [
            (
                serde_json::json!({"kind": "json_pointer", "pointer": "invalid"}),
                "invalid",
                Some("invalid_selector"),
            ),
            (
                serde_json::json!({"kind": "json_pointer", "pointer": "/present~2"}),
                "invalid",
                Some("invalid_selector"),
            ),
            (
                serde_json::json!({"kind": "json_pointer", "pointer": "/present~"}),
                "invalid",
                Some("invalid_selector"),
            ),
            (
                serde_json::json!({"kind": "json_pointer", "pointer": "/missing"}),
                "not_found",
                Some("selector_not_found"),
            ),
            (
                serde_json::json!({"kind": "lines", "start": 0, "end": 0}),
                "invalid",
                Some("invalid_selector"),
            ),
            (
                serde_json::json!({"kind": "json_pointer", "pointer": "/present"}),
                "ok",
                None,
            ),
            (
                serde_json::json!({"kind": "search", "query": "present"}),
                "ok",
                None,
            ),
        ] {
            let payload = ToolPayload::Function {
                arguments: serde_json::json!({
                    "artifact_id": artifact_id,
                    "selectors": [selector],
                })
                .to_string(),
            };
            let result = ReadToolOutputHandler
                .handle(ToolInvocation {
                    session: std::sync::Arc::clone(&session),
                    step_context: crate::session::step_context::StepContext::for_test(
                        std::sync::Arc::clone(&turn),
                    ),
                    cancellation_token: Default::default(),
                    tracker: std::sync::Arc::new(tokio::sync::Mutex::new(
                        crate::turn_diff_tracker::TurnDiffTracker::new(),
                    )),
                    call_id: "selector-recovery".to_string(),
                    tool_name: ToolName::plain("read_tool_output"),
                    source: ToolCallSource::Direct,
                    payload: payload.clone(),
                })
                .await
                .expect("handler output")
                .code_mode_result(&payload);
            assert!(
                validator.is_valid(&result),
                "schema rejected handler result: {result}"
            );
            assert_eq!(
                result["results"][0]["status"], expected_status,
                "{selector}"
            );
            if let Some(reason) = expected_reason {
                assert_eq!(result["continuation_stop"]["reason"], reason);
                assert_eq!(result["continuation_stop"]["selector"], selector);
                assert_eq!(result["continuation_stop"]["resumable"], false);
                assert!(
                    !result["continuation_stop"]["message"]
                        .as_str()
                        .unwrap()
                        .is_empty()
                );
                assert_eq!(result["complete"], false);
                let mut malformed = result.clone();
                malformed["continuation_stop"]["reason"] = serde_json::json!("unknown");
                assert!(!validator.is_valid(&malformed));
            } else {
                assert_eq!(result["complete"], true);
                assert!(result.get("continuation_stop").is_none());
                if selector["kind"] == "search" {
                    assert_eq!(result["results"][0]["value"]["total_matches"], 1);
                    assert_eq!(
                        result["results"][0]["value"]["hydrated_ranges"][0]["text"],
                        r#"{"present":"value"}"#
                    );
                    let mut malformed = result.clone();
                    malformed["results"][0]["value"]["hydrated_ranges"] =
                        serde_json::json!("missing ranges");
                    assert!(!validator.is_valid(&malformed));
                } else {
                    assert_eq!(result["results"][0]["value"], "value");
                }
            }
            let mut malformed = result;
            malformed["complete"] = serde_json::json!("true");
            assert!(!validator.is_valid(&malformed));
        }
    }

    use codex_tools::CanonicalByteRange;

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

    fn page_selector(start: u64) -> ToolOutputSelector {
        ToolOutputSelector::Bytes {
            start,
            end: start + 10,
        }
    }

    fn continuation_result(
        selector: ToolOutputSelector,
        continuation: Option<ToolOutputSelector>,
        text: &str,
    ) -> ToolOutputSelectorResult {
        ToolOutputSelectorResult {
            selector,
            status: ToolOutputSelectorStatus::Ok,
            complete: continuation.is_none(),
            exact_bytes: Some(text.len() as u64),
            canonical_range: None,
            text: Some(text.to_string()),
            value: None,
            data_base64: None,
            subdivision_plan: None,
            child_selectors: Vec::new(),
            continuation,
            message: None,
        }
    }

    fn recovery_output(results: Vec<ToolOutputSelectorResult>) -> ReadToolOutputResult {
        ReadToolOutputResult {
            artifact_id: "01900000-0000-7000-8000-000000000000".to_string(),
            canonical_sha256: "canonical-revision".to_string(),
            canonical_bytes: 100,
            retained_bytes: 100,
            complete: results
                .iter()
                .all(|result| result.status == ToolOutputSelectorStatus::Ok && result.complete),
            unavailable_ranges: Vec::new(),
            results,
        }
    }

    #[test]
    fn many_page_incremental_recovery_matches_reconstruction_and_budget_rollback() {
        for mode in 0..4 {
            let text = "escaped \"\\\n\t 中😀 word_123 ".repeat(160);
            let bytes = match mode {
                2 => serde_json::to_vec(&serde_json::json!({"data": text})).unwrap(),
                3 => [text.as_bytes(), &[0xff, 0xfe]].concat(),
                _ => text.into_bytes(),
            };
            let pages = bytes
                .chunks(64)
                .enumerate()
                .map(|(index, bytes)| {
                    let start = (index * 64) as u64;
                    let end = start + bytes.len() as u64;
                    let mut page = selector_result(ToolOutputSelectorStatus::Ok);
                    page.selector = ToolOutputSelector::Bytes { start, end };
                    page.canonical_range = Some(CanonicalByteRange::new(start, end));
                    page.exact_bytes = Some(bytes.len() as u64);
                    if let Ok(text) = std::str::from_utf8(bytes) {
                        page.text = Some(text.into());
                    } else {
                        page.data_base64 =
                            Some(base64::engine::general_purpose::STANDARD.encode(bytes));
                    }
                    page
                })
                .collect::<Vec<_>>();
            assert!(pages.len() > 64);
            let mut owner = selector_result(if mode == 0 {
                ToolOutputSelectorStatus::Ok
            } else {
                ToolOutputSelectorStatus::SelectorTooLarge
            });
            owner.complete = false;
            owner.continuation = Some(pages[0].selector.clone());
            if mode != 0 {
                owner.selector = if mode == 2 {
                    ToolOutputSelector::JsonPointer {
                        pointer: "/data".into(),
                    }
                } else {
                    ToolOutputSelector::Bytes {
                        start: 0,
                        end: bytes.len() as u64,
                    }
                };
                owner.canonical_range = Some(CanonicalByteRange::new(0, bytes.len() as u64));
                owner.subdivision_plan =
                    Some(crate::tools::command_output_artifact::ByteSubdivisionPlan {
                        range: owner.canonical_range.unwrap(),
                        chunk_bytes: 64,
                        chunk_count: pages.len() as u64,
                        selector_kind: "bytes".into(),
                    });
            }
            let initial = recovery_output(vec![owner]);
            for ceiling in [usize::MAX, 3500, 3501] {
                let mut state = RecoveryContinuationState::new(initial.clone(), false, ceiling);
                let mut reference = RecoveryContinuationState::new(initial.clone(), false, ceiling);
                let mut checkpoints = Vec::new();
                for (index, fragment) in pages.iter().enumerate() {
                    let step = reference.next_step();
                    assert_eq!(state.next_step(), step);
                    let ContinuationStep::Follow {
                        result_index,
                        selector,
                    } = step
                    else {
                        panic!("expected continuation {index}");
                    };
                    let mut page = fragment.clone();
                    if mode == 0 && index + 1 < pages.len() {
                        page.complete = false;
                        page.continuation = Some(pages[index + 1].selector.clone());
                    }
                    let before = reference.output.clone();
                    let predecessor = &mut reference.output.results[result_index];
                    predecessor.continuation = next_owner_continuation(predecessor, &selector);
                    if predecessor.status == ToolOutputSelectorStatus::Ok {
                        predecessor.complete = predecessor.continuation.is_none();
                    }
                    reference.output.results.push(page.clone());
                    reference.output.complete = reference.output.results.iter().all(|result| {
                        result.status == ToolOutputSelectorStatus::Ok
                            && result.complete
                            && result.continuation.is_none()
                    });
                    let expected = reference.reconstructed_output();
                    let fits = recovery_result_fits_token_ceiling(&expected, ceiling);
                    let accepted = state.accept_page(
                        result_index,
                        &selector,
                        recovery_output(vec![page]),
                        true,
                    );
                    assert_eq!(
                        accepted,
                        if fits {
                            Ok(())
                        } else {
                            Err(ContinuationStopReason::Budget)
                        }
                    );
                    if !fits {
                        reference.output = before;
                        reference
                            .record_stop(ContinuationStopReason::Budget, Some(selector.clone()));
                        state.record_stop(ContinuationStopReason::Budget, Some(selector));
                        break;
                    }
                    reference
                        .result_owners
                        .push(reference.result_owners[result_index]);
                    checkpoints.push((before, selector));
                    assert_eq!(
                        state.projected_size(None).tokens(),
                        recovery_size(&expected).tokens()
                    );
                    assert_eq!(state.reconstructed_output(), expected);
                    // The cache must agree with the old per-selection serialization,
                    // including the per-fragment rounding used for page admission.
                    for owner in 0..state.initial_result_count {
                        let occupied = state
                            .output
                            .results
                            .iter()
                            .enumerate()
                            .map(|(index, result)| {
                                if state.result_owners[index] != owner {
                                    recovery_size(result).tokens()
                                } else {
                                    RecoveryResultCost::new(result).payload_tokens
                                }
                            })
                            .sum::<usize>();
                        assert_eq!(
                            state.page_token_ceiling(owner),
                            ceiling.saturating_sub(occupied)
                        );
                    }
                }
                if ceiling == usize::MAX {
                    assert_eq!(checkpoints.len(), pages.len());
                    assert_eq!(state.next_step(), ContinuationStep::Complete);
                } else {
                    assert!(checkpoints.len() > 8 && checkpoints.len() < pages.len());
                    assert_eq!(
                        state.continuation_stop.as_ref().unwrap().reason,
                        ContinuationStopReason::Budget
                    );
                }
                // Replay the old final-envelope rollback independently of the cache.
                let expected = loop {
                    let reconstructed = reference.reconstructed_output();
                    if recovery_envelope_fits(
                        &reconstructed,
                        reference.continuation_stop.as_ref(),
                        ceiling,
                    ) {
                        break reconstructed;
                    }
                    let (previous, selector) = checkpoints
                        .pop()
                        .expect("fixture leaves room for stop metadata");
                    reference.output = previous;
                    reference.record_stop(ContinuationStopReason::Budget, Some(selector));
                };
                let actual = state.finish();
                assert_eq!(
                    actual.drained_continuation_pages as usize,
                    checkpoints.len()
                );
                assert_eq!(actual.continuation_stop, reference.continuation_stop);
                assert_eq!(
                    serde_json::to_vec(&actual.output).unwrap(),
                    serde_json::to_vec(&expected).unwrap()
                );
                if ceiling == usize::MAX && mode != 0 {
                    let result = &actual.output.results[0];
                    let recovered = if let Some(text) = &result.text {
                        text.as_bytes().to_vec()
                    } else if let Some(value) = &result.value {
                        serde_json::to_vec(value).unwrap()
                    } else {
                        base64::engine::general_purpose::STANDARD
                            .decode(result.data_base64.as_ref().unwrap())
                            .unwrap()
                    };
                    assert_eq!(recovered, bytes);
                    assert!(actual.output.complete);
                    assert!(actual.continuation_stop.is_none());
                }
            }
        }
    }

    #[test]
    fn search_page_limit_does_not_automatically_fetch_later_matches() {
        let selector = ToolOutputSelector::Search {
            query: "needle".into(),
            start_byte: 0,
            max_results: 1,
            context_lines: 0,
        };
        let next = ToolOutputSelector::Search {
            query: "needle".into(),
            start_byte: 100,
            max_results: 1,
            context_lines: 0,
        };
        let state = RecoveryContinuationState::new(
            recovery_output(vec![continuation_result(
                selector,
                Some(next.clone()),
                "one match",
            )]),
            false,
            usize::MAX,
        );
        assert_eq!(state.next_step(), ContinuationStep::Complete);
        let output = state.finish();
        assert_eq!(output.drained_continuation_pages, 0);
        assert_eq!(output.output.results.len(), 1);
        assert_eq!(output.output.results[0].continuation, Some(next));
        assert!(!output.output.complete);
    }

    #[test]
    fn overlapping_json_selection_does_not_block_byte_reconstruction() {
        let mut owner = selector_result(ToolOutputSelectorStatus::SelectorTooLarge);
        owner.selector = ToolOutputSelector::Bytes { start: 0, end: 4 };
        owner.canonical_range = Some(CanonicalByteRange::new(0, 4));
        let mut json = selector_result(ToolOutputSelectorStatus::Ok);
        json.selector = ToolOutputSelector::JsonPointer {
            pointer: "/flag".into(),
        };
        json.canonical_range = owner.canonical_range;
        json.value = Some(serde_json::json!(true));
        let mut first =
            continuation_result(ToolOutputSelector::Bytes { start: 0, end: 2 }, None, "tr");
        first.canonical_range = Some(CanonicalByteRange::new(0, 2));
        let mut last =
            continuation_result(ToolOutputSelector::Bytes { start: 2, end: 4 }, None, "ue");
        last.canonical_range = Some(CanonicalByteRange::new(2, 4));
        owner.continuation = Some(first.selector.clone());
        let mut state = RecoveryContinuationState::new(
            recovery_output(vec![owner.clone(), json.clone()]),
            false,
            usize::MAX,
        );
        state
            .accept_page(
                0,
                &first.selector.clone(),
                recovery_output(vec![first]),
                false,
            )
            .unwrap();
        state
            .accept_page(
                0,
                &last.selector.clone(),
                recovery_output(vec![last]),
                false,
            )
            .unwrap();
        assert_eq!(state.next_step(), ContinuationStep::Complete);
        let transaction = state.finish();
        assert!(transaction.output.complete);
        assert_eq!(transaction.output.results.len(), 2);
        assert_eq!(transaction.output.results[0].selector, owner.selector);
        assert_eq!(transaction.output.results[0].text.as_deref(), Some("true"));
        assert_eq!(transaction.output.results[1], json);
    }

    #[test]
    fn overflow_reconstruction_requires_exact_coverage_and_decodes_original_json() {
        let bytes = br#"{"ok":true}"#;
        let mut owner = selector_result(ToolOutputSelectorStatus::SelectorTooLarge);
        owner.selector = ToolOutputSelector::JsonPointer {
            pointer: "/result".into(),
        };
        owner.canonical_range = Some(CanonicalByteRange { start: 10, end: 21 });
        let mut first = continuation_result(
            ToolOutputSelector::Bytes { start: 10, end: 15 },
            None,
            std::str::from_utf8(&bytes[..5]).unwrap(),
        );
        first.canonical_range = Some(CanonicalByteRange { start: 10, end: 15 });
        let mut last = continuation_result(
            ToolOutputSelector::Bytes { start: 15, end: 21 },
            None,
            std::str::from_utf8(&bytes[5..]).unwrap(),
        );
        last.canonical_range = Some(CanonicalByteRange { start: 15, end: 21 });
        assert!(reconstruct_selection(&owner, &[first.clone()]).is_none());
        let exact = reconstruct_selection(&owner, &[last.clone(), first.clone()]).unwrap();
        assert_eq!(exact.status, ToolOutputSelectorStatus::Ok);
        assert!(exact.complete);
        assert_eq!(exact.value, Some(serde_json::json!({"ok":true})));
        assert_eq!(
            reconstruct_selection(&owner, &[first.clone(), first.clone(), last.clone()])
                .unwrap()
                .value,
            exact.value
        );

        owner.continuation = Some(first.selector.clone());
        let mut state =
            RecoveryContinuationState::new(recovery_output(vec![owner.clone()]), false, usize::MAX);
        let first_selector = first.selector.clone();
        let last_selector = last.selector.clone();
        state
            .accept_page(0, &first_selector, recovery_output(vec![first]), false)
            .unwrap();
        assert_eq!(
            state.next_step(),
            ContinuationStep::Follow {
                result_index: 0,
                selector: last_selector.clone(),
            }
        );
        state
            .accept_page(0, &last_selector, recovery_output(vec![last]), false)
            .unwrap();
        assert_eq!(state.next_step(), ContinuationStep::Complete);
        let transaction = state.finish();
        assert_eq!(transaction.drained_continuation_pages, 2);
        assert!(transaction.output.complete);
        assert_eq!(transaction.output.results.len(), 1);
        assert_eq!(transaction.output.results[0].selector, owner.selector);
        assert_eq!(
            transaction.output.results[0].value,
            Some(serde_json::json!({"ok":true}))
        );
    }

    #[test]
    fn terminal_recovery_results_complete_after_the_validation_pass() {
        let mut ok = continuation_result(page_selector(0), None, "ok");
        ok.status = ToolOutputSelectorStatus::Ok;
        let mut oversized = continuation_result(page_selector(10), None, "oversized");
        oversized.status = ToolOutputSelectorStatus::SelectorTooLarge;
        let mut omitted = continuation_result(page_selector(20), None, "omitted");
        omitted.status = ToolOutputSelectorStatus::AggregateOmitted;
        let state = RecoveryContinuationState::new(
            recovery_output(vec![ok, oversized, omitted]),
            false,
            usize::MAX,
        );

        assert_eq!(state.next_step(), ContinuationStep::Complete);
    }

    #[test]
    fn stop_envelope_rolls_back_pages_without_clipping_source() {
        let selector = page_selector(0);
        let next = page_selector(1);
        let mut owner = selector_result(ToolOutputSelectorStatus::Ok);
        owner.continuation = Some(selector.clone());
        owner.complete = false;
        let initial = recovery_output(vec![owner]);
        let mut state = RecoveryContinuationState::new(initial.clone(), false, usize::MAX);
        let page = continuation_result(
            selector.clone(),
            Some(next.clone()),
            &"exact source".repeat(100),
        );
        state
            .accept_page(0, &selector, recovery_output(vec![page]), false)
            .unwrap();
        let payload_tokens = codex_utils_string::approx_token_count(
            &serde_json::to_string(&state.reconstructed_output()).unwrap(),
        );
        state.token_ceiling = payload_tokens;
        state.record_stop(ContinuationStopReason::Budget, Some(next));
        assert!(!recovery_envelope_fits(
            &state.reconstructed_output(),
            state.continuation_stop.as_ref(),
            payload_tokens
        ));
        let result = state.finish();
        assert!(recovery_envelope_fits(
            &result.output,
            result.continuation_stop.as_ref(),
            payload_tokens
        ));
        assert_eq!(result.output, initial);
        assert_eq!(result.drained_continuation_pages, 0);
        let stop = result.continuation_stop.unwrap();
        assert_eq!(stop.reason, ContinuationStopReason::Budget);
        assert!(stop.resumable);
        assert_eq!(stop.selector, Some(selector));
    }

    #[test]
    fn final_page_is_budgeted_as_reconstructed_selection() {
        let selector = ToolOutputSelector::Bytes { start: 0, end: 10 };
        let mut owner = selector_result(ToolOutputSelectorStatus::SelectorTooLarge);
        owner.selector = ToolOutputSelector::Lines { start: 1, end: 1 };
        owner.canonical_range = Some(CanonicalByteRange { start: 0, end: 10 });
        owner.continuation = Some(selector.clone());
        let mut page = continuation_result(selector.clone(), None, "abcdefghij");
        page.canonical_range = owner.canonical_range;
        let exact = reconstruct_selection(&owner, &[page.clone()]).unwrap();
        let ceiling = codex_utils_string::approx_token_count(
            &serde_json::to_string(&recovery_output(vec![exact])).unwrap(),
        );
        assert!(!recovery_result_fits_token_ceiling(
            &recovery_output(vec![owner.clone(), page.clone()]),
            ceiling,
        ));
        let mut state =
            RecoveryContinuationState::new(recovery_output(vec![owner]), false, ceiling);
        assert_eq!(
            state.accept_page(0, &selector, recovery_output(vec![page]), false),
            Ok(())
        );
        let result = state.finish();
        assert!(result.output.complete);
        assert_eq!(result.output.results.len(), 1);
        assert_eq!(result.output.results[0].text.as_deref(), Some("abcdefghij"));
        assert!(result.continuation_stop.is_none());
    }

    #[test]
    fn overlapping_owners_reuse_a_range_without_becoming_a_cycle() {
        let selector = ToolOutputSelector::Bytes { start: 0, end: 10 };
        let mut owner = selector_result(ToolOutputSelectorStatus::SelectorTooLarge);
        owner.selector = ToolOutputSelector::Lines { start: 1, end: 1 };
        owner.canonical_range = Some(CanonicalByteRange { start: 0, end: 10 });
        owner.continuation = Some(selector.clone());
        let mut second = owner.clone();
        second.selector = ToolOutputSelector::Bytes { start: 0, end: 10 };
        let mut page = continuation_result(selector.clone(), None, "abcdefghij");
        page.canonical_range = owner.canonical_range;
        let mut state =
            RecoveryContinuationState::new(recovery_output(vec![owner, second]), false, usize::MAX);
        state
            .accept_page(0, &selector, recovery_output(vec![page]), false)
            .unwrap();
        assert_eq!(
            state.next_step(),
            ContinuationStep::Follow {
                result_index: 1,
                selector: selector.clone()
            }
        );
        let cached = state
            .cached_page(&selector)
            .expect("authenticated range is already available");
        state.accept_page(1, &selector, cached, true).unwrap();
        assert_eq!(state.next_step(), ContinuationStep::Complete);
        let result = state.finish();
        assert!(result.output.complete);
        assert_eq!(result.output.results.len(), 2);
        assert!(
            result
                .output
                .results
                .iter()
                .all(|r| r.text.as_deref() == Some("abcdefghij"))
        );
        assert!(result.continuation_stop.is_none());
    }

    #[test]
    fn exact_continuation_pages_are_drained_in_selector_order() {
        let first_selector = page_selector(0);
        let second_selector = page_selector(10);
        let initial = recovery_output(vec![continuation_result(
            first_selector.clone(),
            Some(second_selector.clone()),
            "first page",
        )]);
        let page = recovery_output(vec![continuation_result(
            second_selector.clone(),
            None,
            "second page",
        )]);
        let retained_text = initial.results[0].text.as_ref().unwrap().as_ptr();
        let mut state = RecoveryContinuationState::new(initial, false, usize::MAX);

        assert_eq!(
            state.next_step(),
            ContinuationStep::Follow {
                result_index: 0,
                selector: second_selector.clone(),
            }
        );
        assert_eq!(state.accept_page(0, &second_selector, page, true), Ok(()));
        assert_eq!(
            state.output.results[0].text.as_ref().unwrap().as_ptr(),
            retained_text,
            "accepting a page must retain previously recovered text without copying it",
        );
        assert_eq!(state.next_step(), ContinuationStep::Complete);

        let transaction = state.finish();
        assert_eq!(transaction.drained_continuation_pages, 1);
        assert!(!transaction.reused);
        assert!(transaction.output.complete);
        assert_eq!(
            transaction
                .output
                .results
                .iter()
                .map(|result| result.selector.clone())
                .collect::<Vec<_>>(),
            vec![first_selector, second_selector]
        );
        assert!(
            transaction
                .output
                .results
                .iter()
                .all(|result| result.continuation.is_none())
        );
    }

    #[test]
    fn final_page_is_admitted_using_exact_reconstruction_and_shared_coverage() {
        let child = ToolOutputSelector::Bytes { start: 0, end: 12 };
        let mut owner = selector_result(ToolOutputSelectorStatus::SelectorTooLarge);
        owner.selector = ToolOutputSelector::Lines { start: 1, end: 1 };
        owner.canonical_range = Some(CanonicalByteRange::new(0, 12));
        owner.continuation = Some(child.clone());
        owner.message = Some("large descriptor metadata ".repeat(100));
        let mut page = continuation_result(child.clone(), None, "exact source");
        page.canonical_range = owner.canonical_range;
        let exact = reconstruct_selection(&owner, &[page.clone()]).unwrap();
        let expected = recovery_output(vec![exact.clone(), exact]);
        let ceiling =
            codex_utils_string::approx_token_count(&serde_json::to_string(&expected).unwrap()) + 10;
        let mut state = RecoveryContinuationState::new(
            recovery_output(vec![owner.clone(), owner]),
            false,
            ceiling,
        );
        assert_eq!(
            state.accept_page(0, &child, recovery_output(vec![page]), false),
            Ok(())
        );
        // A different owner may request the same authenticated range.
        assert_eq!(
            state.next_step(),
            ContinuationStep::Follow {
                result_index: 1,
                selector: child.clone()
            }
        );
        let cached = state
            .cached_page(&child)
            .expect("reuse identity-checked bytes");
        assert_eq!(state.accept_page(1, &child, cached, true), Ok(()));
        let transaction = state.finish();
        assert!(transaction.output.complete);
        assert_eq!(transaction.output.results.len(), 2);
        assert!(
            transaction
                .output
                .results
                .iter()
                .all(|result| result.text.as_deref() == Some("exact source"))
        );
        assert!(recovery_result_fits_token_ceiling(
            &transaction.output,
            ceiling
        ));
    }

    #[test]
    fn recovery_reuses_partially_overlapping_ranges_and_reserves_unrelated_output() {
        let mut owner = selector_result(ToolOutputSelectorStatus::SelectorTooLarge);
        owner.selector = ToolOutputSelector::Bytes { start: 2, end: 8 };
        owner.canonical_range = Some(CanonicalByteRange::new(2, 8));
        let mut page = continuation_result(
            ToolOutputSelector::Bytes { start: 0, end: 10 },
            None,
            "0123456789",
        );
        page.canonical_range = Some(CanonicalByteRange::new(0, 10));
        assert_eq!(
            reconstruct_selection(&owner, &[page.clone()])
                .unwrap()
                .text
                .as_deref(),
            Some("234567")
        );
        let state = RecoveryContinuationState::new(recovery_output(vec![owner, page]), false, 1000);
        assert!(state.page_token_ceiling(0) < 1000);
        assert!(state.page_token_ceiling(0) > 0);
    }

    #[test]
    fn continuation_budget_stop_preserves_first_unconsumed_selector() {
        let first_selector = page_selector(0);
        let second_selector = page_selector(10);
        let initial = recovery_output(vec![continuation_result(
            first_selector,
            Some(second_selector.clone()),
            "first page",
        )]);
        let initial_tokens = codex_utils_string::approx_token_count(
            &serde_json::to_string(&initial).expect("serialize initial page"),
        );
        let oversized_page = recovery_output(vec![continuation_result(
            second_selector.clone(),
            None,
            &"x".repeat(10_000),
        )]);
        let mut state = RecoveryContinuationState::new(initial.clone(), true, initial_tokens + 1);

        assert_eq!(
            state.accept_page(0, &second_selector, oversized_page, false),
            Err(ContinuationStopReason::Budget)
        );
        let transaction = state.finish();
        assert_eq!(transaction.output, initial);
        assert_eq!(transaction.drained_continuation_pages, 0);
        assert!(!transaction.reused);
        assert_eq!(
            transaction.output.results[0].continuation,
            Some(second_selector)
        );
    }

    #[test]
    fn continuation_identity_drift_stops_without_mutating_the_aggregate() {
        let second_selector = page_selector(10);
        let initial = recovery_output(vec![continuation_result(
            page_selector(0),
            Some(second_selector.clone()),
            "first page",
        )]);
        let mut drifted_page = recovery_output(vec![continuation_result(
            second_selector.clone(),
            None,
            "second page",
        )]);
        drifted_page.canonical_sha256 = "different-revision".to_string();
        let mut state = RecoveryContinuationState::new(initial.clone(), false, usize::MAX);

        assert_eq!(
            state.accept_page(0, &second_selector, drifted_page, false),
            Err(ContinuationStopReason::IdentityDrift)
        );
        assert_eq!(state.finish().output, initial);
    }

    #[test]
    fn aggregate_omission_may_retry_its_owner_advertised_selector_once() {
        let selector = ToolOutputSelector::Lines { start: 1, end: 10 };
        let mut omitted = selector_result(ToolOutputSelectorStatus::AggregateOmitted);
        omitted.selector = selector.clone();
        omitted.continuation = Some(selector.clone());
        let initial = recovery_output(vec![omitted]);
        let page = recovery_output(vec![continuation_result(
            selector.clone(),
            None,
            "exact retry",
        )]);
        let mut state = RecoveryContinuationState::new(initial, false, usize::MAX);

        assert_eq!(
            state.next_step(),
            ContinuationStep::Follow {
                result_index: 0,
                selector: selector.clone(),
            }
        );
        assert_eq!(state.accept_page(0, &selector, page, false), Ok(()));
        assert_eq!(state.next_step(), ContinuationStep::Complete);
    }

    #[test]
    fn repeated_owner_continuation_is_never_followed_twice() {
        let second_selector = page_selector(10);
        let initial = recovery_output(vec![continuation_result(
            page_selector(0),
            Some(second_selector.clone()),
            "first page",
        )]);
        let repeated_page = recovery_output(vec![continuation_result(
            second_selector.clone(),
            Some(second_selector.clone()),
            "second page",
        )]);
        let mut state = RecoveryContinuationState::new(initial, false, usize::MAX);

        assert_eq!(
            state.accept_page(0, &second_selector, repeated_page, false),
            Ok(())
        );
        assert_eq!(
            state.next_step(),
            ContinuationStep::Stop(ContinuationStopReason::RepeatedSelector)
        );
        let transaction = state.finish();
        assert_eq!(transaction.drained_continuation_pages, 1);
        assert_eq!(
            transaction.output.results[1].continuation,
            Some(second_selector)
        );
    }

    #[test]
    fn selector_overflow_drains_the_owner_subdivision_plan_and_preserves_its_contract() {
        let parent_selector = ToolOutputSelector::Lines { start: 1, end: 100 };
        let child_selector = ToolOutputSelector::Bytes { start: 0, end: 10 };
        let second_child_selector = ToolOutputSelector::Bytes { start: 10, end: 20 };
        let mut overflow = selector_result(ToolOutputSelectorStatus::SelectorTooLarge);
        overflow.selector = parent_selector;
        overflow.canonical_range = Some(CanonicalByteRange { start: 0, end: 20 });
        overflow.subdivision_plan = Some(ByteSubdivisionPlan {
            range: CanonicalByteRange { start: 0, end: 20 },
            chunk_bytes: 10,
            chunk_count: 2,
            selector_kind: "bytes".to_string(),
        });
        overflow.child_selectors = vec![child_selector.clone()];
        overflow.continuation = Some(child_selector.clone());
        let initial = recovery_output(vec![overflow]);
        let first_page = recovery_output(vec![continuation_result(
            child_selector.clone(),
            None,
            "exact child",
        )]);
        let second_page = recovery_output(vec![continuation_result(
            second_child_selector.clone(),
            None,
            "second exact child",
        )]);
        let mut state = RecoveryContinuationState::new(initial, false, usize::MAX);

        assert_eq!(
            state.next_step(),
            ContinuationStep::Follow {
                result_index: 0,
                selector: child_selector.clone(),
            }
        );
        assert_eq!(
            state.accept_page(0, &child_selector, first_page, false),
            Ok(())
        );
        assert_eq!(
            state.next_step(),
            ContinuationStep::Follow {
                result_index: 0,
                selector: second_child_selector.clone(),
            }
        );
        assert_eq!(
            state.accept_page(0, &second_child_selector, second_page, false),
            Ok(())
        );
        assert_eq!(state.next_step(), ContinuationStep::Complete);
        let transaction = state.finish();
        assert!(!transaction.output.complete);
        assert_eq!(
            transaction.output.results[0].status,
            ToolOutputSelectorStatus::SelectorTooLarge
        );
        assert_eq!(
            transaction.output.results[0].child_selectors,
            vec![child_selector]
        );
        assert!(transaction.output.results[0].continuation.is_none());
        assert_eq!(transaction.drained_continuation_pages, 2);
        assert_eq!(
            transaction.output.results[2].selector,
            second_child_selector
        );
    }

    #[test]
    fn typed_overflow_preserves_artifact_completeness() {
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

        let delivered = serde_json::to_value(output).expect("recovery result");
        assert_eq!(delivered["retained_artifact_complete"], true);
        assert_eq!(delivered["delivered_selection_complete"], false);
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
            exact_code_mode_recovery_receipt(true, &output, "selector-bounds".to_string(), 2)
                .expect("exact nested recovery receipt");

        assert_eq!(receipt.state_revision, "canonical-revision");
        assert_eq!(receipt.action_bounds_hash, "selector-bounds");
        assert_eq!(receipt.suppressed_continuation_count, 2);
        assert!(
            exact_code_mode_recovery_receipt(false, &output, "selector-bounds".to_string(), 2,)
                .is_none()
        );
        assert!(
            exact_code_mode_recovery_receipt(true, &output, "selector-bounds".to_string(), 0,)
                .is_none()
        );
        let value = serde_json::to_value(output).expect("recovery output");
        let mut tool_output = ReadToolOutputToolOutput {
            inner: JsonToolOutput::new(value.clone()),
            exact_recovery: Some(receipt.clone()),
            semantic_evidence: Vec::new(),
        };
        assert_eq!(
            tool_output.deterministic_continuation_receipts(),
            vec![receipt]
        );
        assert_eq!(
            tool_output.deterministic_continuation_content(),
            vec![value]
        );
        tool_output.exact_recovery = None;
        assert!(tool_output.deterministic_continuation_receipts().is_empty());
        assert!(tool_output.deterministic_continuation_content().is_empty());
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
            read_tool_output_semantic_evidence(&output, None),
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

        let expected = semantic_evidence_for_command_output(
            b"diff --git a/src/lib.rs b/src/lib.rs\n@@ -9,0 +10 @@\n+let stable = compute();",
        );
        let evidence = read_tool_output_semantic_evidence(&output, None);

        assert!(expected.iter().all(|fact| evidence.contains(fact)));
        assert_eq!(
            evidence
                .iter()
                .filter(|fact| fact.starts_with("artifact-recovery-fragment-v1:"))
                .count(),
            1
        );
        // Repeated selections keep their provenance but must not duplicate or
        // reorder the semantic facts already emitted for the first fragment.
        let provenance = evidence
            .iter()
            .find(|fact| fact.starts_with("artifact-recovery-fragment-v1:"))
            .expect("fragment provenance")
            .clone();
        let mut repeated = output;
        repeated.results.push(repeated.results[0].clone());
        let mut expected_repeated = evidence;
        expected_repeated.push(provenance);
        assert_eq!(
            read_tool_output_semantic_evidence(&repeated, None),
            expected_repeated
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
            read_tool_output_semantic_evidence(&incomplete, None),
            read_tool_output_semantic_evidence(&complete, None)
        );
    }

    #[test]
    fn disjoint_recovered_fragments_do_not_create_synthetic_semantic_facts() {
        let fragments = [
            "diff --git a/src/lib.rs b/src/lib.rs",
            "@@ -9,0 +10 @@\n+let stable = compute();",
        ];
        let mut results = Vec::new();
        for fragment in fragments {
            let mut result = selector_result(ToolOutputSelectorStatus::Ok);
            result.text = Some(fragment.to_string());
            results.push(result);
        }
        let output = ReadToolOutputResult {
            artifact_id: "artifact".to_string(),
            canonical_sha256: "canonical-revision".to_string(),
            canonical_bytes: 80,
            retained_bytes: 80,
            complete: true,
            unavailable_ranges: Vec::new(),
            results,
        };
        let expected = fragments
            .iter()
            .flat_map(|fragment| semantic_evidence_for_command_output(fragment.as_bytes()))
            .collect::<Vec<_>>();

        let evidence = read_tool_output_semantic_evidence(&output, None);
        assert!(expected.iter().all(|fact| evidence.contains(fact)));
        assert_eq!(
            evidence
                .iter()
                .filter(|fact| fact.starts_with("artifact-recovery-fragment-v1:"))
                .count(),
            fragments.len()
        );
        assert_ne!(
            evidence,
            semantic_evidence_for_command_output(fragments.join("\n").as_bytes())
        );
    }

    #[test]
    fn recovered_fragment_identity_includes_its_selector() {
        let mut first = selector_result(ToolOutputSelectorStatus::Ok);
        first.selector = ToolOutputSelector::Lines { start: 1, end: 1 };
        first.text = Some("same recovered fact".to_string());
        let mut second = first.clone();
        second.selector = ToolOutputSelector::Lines { start: 2, end: 2 };
        let output = recovery_output(vec![first, second]);

        let provenance = read_tool_output_semantic_evidence(&output, None)
            .into_iter()
            .filter(|fact| fact.starts_with("artifact-recovery-fragment-v1:"))
            .collect::<Vec<_>>();

        assert_eq!(provenance.len(), 2);
        assert_ne!(provenance[0], provenance[1]);
    }

    #[test]
    fn continuation_stop_is_typed_and_preserves_the_unconsumed_selector() {
        let selector = page_selector(10);
        let initial = recovery_output(vec![continuation_result(
            page_selector(0),
            Some(selector.clone()),
            "first page",
        )]);
        let mut state = RecoveryContinuationState::new(initial, true, usize::MAX);
        state.record_stop(ContinuationStopReason::Budget, Some(selector.clone()));

        let stop = state
            .finish()
            .continuation_stop
            .expect("typed stop receipt");
        assert_eq!(stop.reason, ContinuationStopReason::Budget);
        assert_eq!(stop.selector, Some(selector));
        assert!(stop.resumable);
        assert_eq!(
            stop.message.as_deref(),
            Some(
                "Recovery reached its output budget. Continue with the unconsumed selector in a new call."
            )
        );
        assert_eq!(
            serde_json::to_value(stop).expect("serialize stop")["reason"],
            "budget"
        );
    }

    #[test]
    fn resumable_byte_stop_preserves_the_remaining_owner_suffix() {
        let child = ToolOutputSelector::Bytes { start: 7, end: 11 };
        let mut owner = selector_result(ToolOutputSelectorStatus::SelectorTooLarge);
        owner.selector = ToolOutputSelector::Bytes { start: 3, end: 31 };
        owner.canonical_range = Some(CanonicalByteRange::new(3, 31));
        owner.continuation = Some(child.clone());
        owner.child_selectors = vec![child.clone()];
        assert_eq!(
            next_owner_continuation(&owner, &child),
            Some(ToolOutputSelector::Bytes { start: 11, end: 31 }),
            "an absent uniform subdivision plan cannot erase the remaining suffix"
        );
        for reason in [
            ContinuationStopReason::Budget,
            ContinuationStopReason::Cancelled,
        ] {
            let mut state = RecoveryContinuationState::new(
                recovery_output(vec![owner.clone()]),
                false,
                usize::MAX,
            );
            state.record_stop(reason, Some(child.clone()));
            let stop = state.finish().continuation_stop.unwrap();
            assert!(stop.resumable);
            assert_eq!(
                stop.selector,
                Some(ToolOutputSelector::Bytes { start: 7, end: 31 })
            );
        }
        let mut state =
            RecoveryContinuationState::new(recovery_output(vec![owner]), false, usize::MAX);
        state.record_page_read_error(&ReadToolOutputError::StillWriting, child);
        assert_eq!(
            state.finish().continuation_stop.unwrap().selector,
            Some(ToolOutputSelector::Bytes { start: 7, end: 31 })
        );
    }

    #[test]
    fn continuation_page_error_preserves_retryability_and_cause() {
        let selector = page_selector(10);
        let cases = [
            (ReadToolOutputError::InvalidArtifactId, false),
            (
                ReadToolOutputError::InvalidRange("invalid continuation range".to_string()),
                false,
            ),
            (ReadToolOutputError::Expired, false),
            (ReadToolOutputError::StillWriting, true),
            (
                ReadToolOutputError::Io("artifact storage unavailable".to_string()),
                false,
            ),
        ];

        for (error, resumable) in cases {
            let expected_message = error.for_model();
            let mut state = RecoveryContinuationState::new(
                recovery_output(vec![selector_result(ToolOutputSelectorStatus::Ok)]),
                false,
                usize::MAX,
            );
            state.record_page_read_error(&error, selector.clone());
            let stop = state
                .finish()
                .continuation_stop
                .expect("page error stop receipt");
            assert_eq!(stop.reason, ContinuationStopReason::PageReadError);
            assert_eq!(stop.selector, Some(selector.clone()));
            assert_eq!(stop.resumable, resumable);
            assert_eq!(stop.message.as_deref(), Some(expected_message.as_str()));
        }
    }

    #[test]
    fn default_range_is_exactly_two_hundred_lines() {
        let args = ReadToolOutputArgs {
            artifact_id: uuid::Uuid::now_v7().to_string(),
            selectors: None,
            start_line: Some(17),
            end_line: None,
            ranges: None,
            max_bytes: None,
        };
        assert_eq!(resolved_line_range(&args).unwrap(), (17, 216));
    }

    #[test]
    fn legacy_single_range_uses_the_canonical_line_invariants() {
        for (start_line, end_line) in [(0, Some(1)), (3, Some(2))] {
            let args = ReadToolOutputArgs {
                artifact_id: uuid::Uuid::now_v7().to_string(),
                selectors: None,
                start_line: Some(start_line),
                end_line,
                ranges: None,
                max_bytes: None,
            };
            assert!(resolved_selectors(&args).is_err());
        }

        let args = ReadToolOutputArgs {
            artifact_id: uuid::Uuid::now_v7().to_string(),
            selectors: None,
            start_line: Some(3),
            end_line: Some(3),
            ranges: None,
            max_bytes: None,
        };
        assert_eq!(
            resolved_selectors(&args).unwrap(),
            vec![ToolOutputSelector::Lines { start: 3, end: 3 }]
        );
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
    fn read_tool_output_schema_matches_runtime_bounds() {
        let tool = serde_json::to_value(create_read_tool_output_tool())
            .expect("serialize read_tool_output tool");
        let validator = jsonschema::validator_for(&tool["parameters"])
            .expect("compile read_tool_output schema");
        let line_selector = serde_json::json!({
            "kind": "lines",
            "start": 1,
            "end": 1,
        });
        let selector_args = |count: usize| {
            serde_json::json!({
                "artifact_id": "artifact",
                "selectors": vec![line_selector.clone(); count],
            })
        };
        let range_args = |count: usize| {
            serde_json::json!({
                "artifact_id": "artifact",
                "ranges": (1..=count)
                    .map(|line| serde_json::json!({
                        "start_line": line,
                        "end_line": line,
                    }))
                    .collect::<Vec<_>>(),
            })
        };
        let runtime_accepts = |value: &Value| {
            parse_read_tool_output_args(&value.to_string())
                .ok()
                .is_some_and(|args| {
                    resolved_max_bytes(args.max_bytes).is_ok() && resolved_selectors(&args).is_ok()
                })
        };
        let cases = [
            (selector_args(1), true),
            (selector_args(READ_TOOL_OUTPUT_MAX_SELECTORS), true),
            (selector_args(0), false),
            (selector_args(READ_TOOL_OUTPUT_MAX_SELECTORS + 1), false),
            (range_args(1), true),
            (range_args(64), true),
            (range_args(0), false),
            (range_args(65), false),
        ];

        for (arguments, expected) in cases {
            assert_eq!(
                validator.is_valid(&arguments),
                expected,
                "schema verdict for {arguments}"
            );
            assert_eq!(
                runtime_accepts(&arguments),
                expected,
                "runtime verdict for {arguments}"
            );
        }

        // Old callers may still supply max_bytes, but new calls must use the
        // selector bounds advertised by the schema instead of a clipping knob.
        for (max_bytes, runtime_expected) in [
            (0, false),
            (1, true),
            (READ_TOOL_OUTPUT_MAX_BYTES, true),
            (READ_TOOL_OUTPUT_MAX_BYTES + 1, false),
        ] {
            let mut arguments = selector_args(1);
            arguments["max_bytes"] = serde_json::json!(max_bytes);
            assert!(
                !validator.is_valid(&arguments),
                "legacy field is not advertised"
            );
            assert_eq!(
                runtime_accepts(&arguments),
                runtime_expected,
                "legacy runtime verdict for {arguments}"
            );
        }
    }

    #[test]
    fn invalid_recovery_selectors_defer_shape_to_advertised_schema() {
        let error = parse_read_tool_output_args(
            r#"{"artifact_id":"artifact","selector":{"kind":"line","start":1,"end":2}}"#,
        )
        .expect_err("singular selector and line kind must be rejected");
        let FunctionCallError::RespondToModel(message) = error else {
            panic!("parse failures must be returned to the model");
        };

        assert!(message.contains("advertised read_tool_output schema"));
        assert!(!message.contains(r#""selectors""#));
        assert!(!message.contains(r#"{"artifact_id""#));
    }

    #[test]
    fn three_exact_ranges_become_one_bounded_owner_batch() {
        let args = ReadToolOutputArgs {
            artifact_id: uuid::Uuid::now_v7().to_string(),
            selectors: None,
            start_line: None,
            end_line: None,
            ranges: Some(vec![
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
            ]),
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
            ranges: Some(vec![
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
            ]),
            max_bytes: None,
        };

        assert_eq!(
            resolved_selectors(&args).unwrap(),
            vec![ToolOutputSelector::Lines { start: 2, end: 12 }]
        );

        args.ranges = Some(
            (1..=READ_TOOL_OUTPUT_MAX_LEGACY_RANGES)
                .map(|line| ReadToolOutputRangeArgs {
                    start_line: line * 2,
                    end_line: line * 2,
                })
                .collect(),
        );
        assert_eq!(
            resolved_selectors(&args).unwrap().len(),
            READ_TOOL_OUTPUT_MAX_LEGACY_RANGES
        );

        args.ranges = Some(
            (1..=READ_TOOL_OUTPUT_MAX_LEGACY_RANGES + 1)
                .map(|line| ReadToolOutputRangeArgs {
                    start_line: line * 2,
                    end_line: line * 2,
                })
                .collect(),
        );
        assert!(resolved_selectors(&args).is_err());

        args.ranges = Some(vec![
            ReadToolOutputRangeArgs {
                start_line: 1,
                end_line: 1_000,
            },
            ReadToolOutputRangeArgs {
                start_line: 2_000,
                end_line: 3_000,
            },
        ]);
        assert!(resolved_selectors(&args).is_err());
    }
}
