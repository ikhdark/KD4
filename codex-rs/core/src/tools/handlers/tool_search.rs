use crate::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::ToolSearchOutput;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::tool_search_spec::create_tool_search_tool;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use bm25::Tokenizer;
use codex_tools::LoadableToolSpec;
use codex_tools::ResponsesApiNamespace;
use codex_tools::ResponsesApiNamespaceTool;
use codex_tools::ResponsesApiTool;
use codex_tools::TOOL_SEARCH_DEFAULT_LIMIT;
use codex_tools::TOOL_SEARCH_TOOL_NAME;
use codex_tools::ToolName;
use codex_tools::ToolSearchInfo;
use codex_tools::ToolSpec;
use sha2::Digest;
use sha2::Sha256;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::io::Write;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use tokio_util::task::AbortOnDropHandle;
use tracing::instrument;
use unicode_segmentation::UnicodeSegmentation;

const MAX_TOOL_SEARCH_HANDLER_CACHE: usize = 4;
const MAX_TOOL_SEARCH_RESULT_CACHE: usize = 32;
const MAX_TOOL_SEARCH_CACHE_ENTRY_BYTES: usize = 256 * 1024;
// Tool-search outputs stay in model-visible history. Keep ordinary serialized
// results near a 1,536-token projection (using the core's 4 bytes/token estimate).
const MAX_TOOL_SEARCH_RESULT_BYTES: usize = 6 * 1024;
const MAX_TOOL_SEARCH_QUERY_BYTES: usize = 4 * 1024;
const MAX_TOOL_SEARCH_LIMIT: usize = 64;
const TOOL_SEARCH_CANDIDATE_MULTIPLIER: usize = 3;
// BM25 ranks results but has no corpus-independent absolute cutoff. Require
// at least half of the indexed query terms before exposing a schema. Unknown
// task entities are not negative capability evidence. One recognized term
// qualifies even when the query also names an unknown task entity.
const MIN_TOOL_ACTIVATION_RELEVANCE: f32 = 0.5;

#[derive(Debug, Default)]
struct ToolSearchTokenizer;

impl Tokenizer for ToolSearchTokenizer {
    fn tokenize(&self, input_text: &str) -> Vec<String> {
        input_text.unicode_words().map(str::to_lowercase).collect()
    }
}

#[derive(Clone)]
pub struct ToolSearchHandler {
    search_infos: Arc<[ToolSearchInfo]>,
    cache_context: serde_json::Value,
    name_indexes: Arc<[ToolSearchNameIndex]>,
    exact_name_index: Arc<HashMap<String, Vec<ToolSearchDocumentId>>>,
    folded_name_index: Arc<HashMap<String, Vec<String>>>,
    namespace_member_indexes: Arc<HashMap<ToolSearchDocumentId, (Vec<ToolSearchInfo>, ToolSearchIndex)>>,
    spec: ToolSpec,
    search_index: Arc<ToolSearchIndex>,
    result_cache: Arc<Mutex<VecDeque<ToolSearchCacheEntry>>>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct ToolSearchDocumentId(usize);

impl ToolSearchDocumentId {
    fn info(self, search_infos: &[ToolSearchInfo]) -> &ToolSearchInfo {
        &search_infos[self.0]
    }

    fn name_index(self, name_indexes: &[ToolSearchNameIndex]) -> &ToolSearchNameIndex {
        &name_indexes[self.0]
    }
}

struct ToolSearchIndex {
    postings: HashMap<String, Vec<(ToolSearchDocumentId, f32)>>,
    callable_terms: Vec<HashSet<String>>,
    document_count: usize,
}

struct ToolSearchQueryEvidence<'a> {
    indexed_terms: Vec<&'a str>,
    minimum_matches: usize,
}

#[derive(Clone, Copy, Debug)]
struct RankedToolSearchDocument {
    id: ToolSearchDocumentId,
    score: f32,
}

impl PartialEq for RankedToolSearchDocument {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && self.score.total_cmp(&other.score).is_eq()
    }
}

impl Eq for RankedToolSearchDocument {}

impl PartialOrd for RankedToolSearchDocument {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for RankedToolSearchDocument {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.score
            .total_cmp(&other.score)
            // Earlier inventory entries win score ties.
            .then_with(|| other.id.cmp(&self.id))
    }
}

impl ToolSearchIndex {
    fn query_evidence<'a>(
        &self,
        tokens: &'a [String],
        search_infos: &[ToolSearchInfo],
        source: Option<&str>,
    ) -> ToolSearchQueryEvidence<'a> {
        let terms = tokens.iter().map(String::as_str).collect::<HashSet<_>>();
        let indexed_terms = terms
            .iter()
            .copied()
            .filter(|term| {
                self.postings.get(*term).is_some_and(|postings| {
                    source.is_none()
                        || postings
                            .iter()
                            .any(|(id, _)| matches_source(id.info(search_infos), source))
                })
            })
            .collect::<Vec<_>>();
        // Unknown project/person names are not missing capabilities. Count
        // recognized inventory terms instead, globally so a source scope still
        // cannot erase a known capability belonging to another source.
        let minimum_matches = terms
            .iter()
            .filter(|term| self.postings.contains_key(**term))
            .count()
            .clamp(1, 2);
        ToolSearchQueryEvidence {
            indexed_terms,
            minimum_matches,
        }
    }

    fn relevance(&self, evidence: &ToolSearchQueryEvidence<'_>, id: ToolSearchDocumentId) -> f32 {
        if evidence.indexed_terms.len() < evidence.minimum_matches {
            return 0.0;
        }
        let matches = evidence
            .indexed_terms
            .iter()
            .filter(|term| self.callable_terms[id.0].contains(**term))
            .count();
        if matches < evidence.minimum_matches {
            return 0.0;
        }
        matches as f32 / evidence.indexed_terms.len() as f32
    }

    fn new(search_infos: &[ToolSearchInfo]) -> Self {
        Self::new_with_callable_texts(
            search_infos,
            search_infos
                .iter()
                .map(|info| info.entry.callable_search_text()),
        )
    }

    fn new_with_callable_texts(
        search_infos: &[ToolSearchInfo],
        callable_texts: impl Iterator<Item = String>,
    ) -> Self {
        const K1: f32 = 1.2;
        const B: f32 = 0.75;
        const FALLBACK_AVERAGE_DOCUMENT_LENGTH: f32 = 256.0;

        let tokenizer = ToolSearchTokenizer;
        // Reuse the callable tokenization for both length normalization and
        // activation. Connector prose and output hints must not penalize the
        // operation merely by making the retrieval document longer.
        let mut callable_lengths = Vec::with_capacity(search_infos.len());
        let callable_terms = callable_texts
            .map(|text| {
                let tokens = tokenizer.tokenize(&text);
                callable_lengths.push(tokens.len());
                tokens.into_iter().collect::<HashSet<_>>()
            })
            .collect::<Vec<_>>();
        let average_document_length = {
            let total_document_length = callable_lengths.iter().sum::<usize>();
            let average = if callable_lengths.is_empty() {
                0.0
            } else {
                total_document_length as f64 / callable_lengths.len() as f64
            };
            let average = average as f32;
            if average > 0.0 {
                average
            } else {
                FALLBACK_AVERAGE_DOCUMENT_LENGTH
            }
        };

        let mut postings = HashMap::<String, Vec<(ToolSearchDocumentId, f32)>>::new();
        let mut term_frequencies = HashMap::<std::borrow::Cow<'_, str>, usize>::new();
        for (index, info) in search_infos.iter().enumerate() {
            let document_length = callable_lengths[index] as f32;
            // Borrow common lowercase tokens; only new vocabulary needs an
            // owned posting key. Keep Unicode lowercasing identical to queries.
            for word in info.entry.search_text.unicode_words() {
                let token =
                    if word.is_ascii() && !word.bytes().any(|byte| byte.is_ascii_uppercase()) {
                        std::borrow::Cow::Borrowed(word)
                    } else {
                        std::borrow::Cow::Owned(word.to_lowercase())
                    };
                *term_frequencies.entry(token).or_default() += 1;
            }
            for (token, term_frequency) in term_frequencies.drain() {
                let term_frequency = term_frequency as f32;
                let weight = term_frequency * (K1 + 1.0)
                    / (term_frequency
                        + K1 * (1.0 - B + B * document_length / average_document_length));
                let posting = (ToolSearchDocumentId(index), weight);
                if index == 0 {
                    // The first document has no existing vocabulary; avoid
                    // hashing each token twice on tiny/cold inventories.
                    postings.insert(token.into_owned(), vec![posting]);
                } else if let Some(entries) = postings.get_mut(token.as_ref()) {
                    entries.push(posting);
                } else {
                    postings.insert(token.into_owned(), vec![posting]);
                }
            }
        }

        Self {
            postings,
            callable_terms,
            document_count: search_infos.len(),
        }
    }

    fn top_matches(
        &self,
        query: &str,
        limit: usize,
        search_infos: &[ToolSearchInfo],
        source: Option<&str>,
        exact_matches: &[ToolSearchDocumentId],
    ) -> (Vec<ToolSearchDocumentId>, Vec<ToolSearchDocumentId>) {
        if limit == 0 || self.document_count == 0 {
            return (Vec::new(), Vec::new());
        }

        let tokenizer = ToolSearchTokenizer;
        let tokens = tokenizer.tokenize(query);
        let evidence = self.query_evidence(&tokens, search_infos, source);
        let required = required_query_terms(query);
        let mut scores = HashMap::<ToolSearchDocumentId, f32>::new();
        for token in &tokens {
            let Some(postings) = self.postings.get(token) else {
                continue;
            };
            let document_frequency = postings.len() as f32;
            let inverse_document_frequency = (1.0
                + (self.document_count as f32 - document_frequency + 0.5)
                    / (document_frequency + 0.5))
                .ln();
            for (id, document_weight) in postings {
                *scores.entry(*id).or_default() += inverse_document_frequency * document_weight;
            }
        }

        // Bound each source's admission before global truncation can erase a
        // smaller source. Total retained candidates never exceeds scored docs.
        let mut by_source = HashMap::<_, BinaryHeap<Reverse<RankedToolSearchDocument>>>::new();
        let mut weak_by_source = HashMap::<_, BinaryHeap<Reverse<RankedToolSearchDocument>>>::new();
        for (id, score) in scores {
            if !matches_source(id.info(search_infos), source) {
                continue;
            }
            if !required.iter().all(|term| {
                self.postings.get(term).is_some_and(|postings| {
                    postings
                        .binary_search_by_key(&id, |(candidate, _)| *candidate)
                        .is_ok()
                })
            }) {
                continue;
            }
            let candidate = RankedToolSearchDocument { id, score };
            // Eligibility must precede bounded admission: otherwise short weak
            // documents can evict a lower-scoring eligible capability.
            let eligible = exact_matches.contains(&id)
                || self.relevance(&evidence, id) >= MIN_TOOL_ACTIVATION_RELEVANCE;
            let heap = if eligible {
                &mut by_source
            } else {
                &mut weak_by_source
            };
            let best = heap
                .entry(tool_search_info_diversity_key(id.info(search_infos)))
                .or_default();
            if best.len() < limit {
                best.push(Reverse(candidate));
            } else if best.peek().is_some_and(|worst| candidate > worst.0) {
                best.pop();
                best.push(Reverse(candidate));
            }
        }

        let mut ranked = [by_source, weak_by_source].map(|heap| {
            let mut best = heap
                .into_values()
                .flatten()
                .map(|candidate| candidate.0)
                .collect::<Vec<_>>();
            best.sort_unstable_by(|left, right| right.cmp(left));
            diversify_search_result_ids(
                search_infos,
                best.into_iter().map(|candidate| candidate.id).collect(),
                limit,
            )
        });
        (
            std::mem::take(&mut ranked[0]),
            std::mem::take(&mut ranked[1]),
        )
    }
}

fn required_query_terms(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .filter_map(|term| term.strip_prefix('+'))
        .flat_map(|term| ToolSearchTokenizer.tokenize(term))
        .collect()
}

struct ToolSearchNameIndex {
    entry_names: HashSet<String>,
    output_names: HashMap<String, HashSet<String>>,
}

impl ToolSearchNameIndex {
    fn new(search_info: &ToolSearchInfo) -> Self {
        let mut entry_names = search_info
            .entry
            .tool_names
            .iter()
            .map(|name| normalize_tool_search_query(name))
            .collect::<HashSet<_>>();
        let mut output_names = HashMap::<String, HashSet<String>>::new();
        match search_info.entry.output.as_ref() {
            LoadableToolSpec::Function(tool) => {
                index_output_name(
                    &mut entry_names,
                    &mut output_names,
                    &ToolName::plain(tool.name.clone()),
                );
            }
            LoadableToolSpec::Namespace(namespace) => {
                for tool in &namespace.tools {
                    let ResponsesApiNamespaceTool::Function(tool) = tool;
                    index_output_name(
                        &mut entry_names,
                        &mut output_names,
                        &ToolName::namespaced(namespace.name.clone(), tool.name.clone()),
                    );
                }
            }
        }
        Self {
            entry_names,
            output_names,
        }
    }


    fn output_names_for(&self, normalized_query: &str) -> Option<&HashSet<String>> {
        self.output_names.get(normalized_query)
    }
}

fn index_output_name(
    entry_names: &mut HashSet<String>,
    output_names: &mut HashMap<String, HashSet<String>>,
    tool_name: &ToolName,
) {
    let aliases = [
        tool_name.name.clone(),
        codex_tools::code_mode_name_for_tool_name(tool_name),
        tool_name.to_string(),
        match &tool_name.namespace {
            Some(namespace) => format!("{namespace}.{}", tool_name.name),
            None => tool_name.name.clone(),
        },
    ];
    for alias in aliases {
        let normalized = normalize_tool_search_query(&alias);
        entry_names.insert(normalized.clone());
        output_names
            .entry(normalized)
            .or_default()
            .insert(tool_name.name.clone());
    }
}

pub(crate) struct ToolSearchHandlerCache {
    state: Mutex<ToolSearchHandlerCacheState>,
}

#[derive(Default)]
struct ToolSearchHandlerCacheState {
    cached: VecDeque<Arc<ToolSearchHandler>>,
    in_flight: HashMap<[u8; 32], Arc<ToolSearchBuildFlight>>,
}

#[derive(Default)]
struct ToolSearchBuildFlight {
    state: Mutex<ToolSearchBuildFlightState>,
    ready: Condvar,
}

#[derive(Default)]
enum ToolSearchBuildFlightState {
    #[default]
    Building,
    Ready(Arc<ToolSearchHandler>),
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ToolSearchQueryKey {
    query: String,
    limit: usize,
    source: Option<String>,
}

struct ToolSearchCacheEntry {
    key: ToolSearchQueryKey,
    result: Arc<ToolSearchResult>,
}

#[derive(Clone, Debug, PartialEq)]
struct ToolSearchResult {
    enumeration_complete: bool,
    tools: Vec<LoadableToolSpec>,
    serialized_tools: Vec<serde_json::Value>,
    activation_tools: Vec<ToolName>,
    supplemental_tools: Vec<ToolName>,
    unactivated_matches: Vec<String>,
    unmatched_identifiers: Vec<String>,
    omitted_result_count: usize,
    encoded_tools_len: usize,
    exact_name_ambiguity: Option<serde_json::Value>,
}

impl Default for ToolSearchResult {
    fn default() -> Self {
        Self {
            enumeration_complete: true,
            tools: Vec::new(),
            serialized_tools: Vec::new(),
            activation_tools: Vec::new(),
            supplemental_tools: Vec::new(),
            unactivated_matches: Vec::new(),
            unmatched_identifiers: Vec::new(),
            omitted_result_count: 0,
            encoded_tools_len: 2,
            exact_name_ambiguity: None,
        }
    }
}

struct ToolSearchResultBuilder {
    tools: Vec<LoadableToolSpec>,
    namespace_indexes: HashMap<String, usize>,
    // Maintain the exact compact JSON size incrementally; an empty array is two bytes.
    encoded_len: usize,
}

impl ToolSearchResultBuilder {
    fn new() -> Self {
        Self {
            tools: Vec::new(),
            namespace_indexes: HashMap::new(),
            encoded_len: 2,
        }
    }

    fn try_push(&mut self, candidate: &LoadableToolSpec) -> bool {
        match candidate {
            LoadableToolSpec::Function(_) => {
                let separator = usize::from(!self.tools.is_empty());
                let Some(remaining) = MAX_TOOL_SEARCH_RESULT_BYTES
                    .checked_sub(self.encoded_len)
                    .and_then(|remaining| remaining.checked_sub(separator))
                else {
                    return false;
                };
                let Some(encoded_len) = serialized_len_with_limit(candidate, remaining) else {
                    return false;
                };
                self.tools.push(candidate.clone());
                self.encoded_len += separator + encoded_len;
                true
            }
            LoadableToolSpec::Namespace(namespace) => {
                let Some(&existing_index) = self.namespace_indexes.get(&namespace.name) else {
                    let separator = usize::from(!self.tools.is_empty());
                    let Some(remaining) = MAX_TOOL_SEARCH_RESULT_BYTES
                        .checked_sub(self.encoded_len)
                        .and_then(|remaining| remaining.checked_sub(separator))
                    else {
                        return false;
                    };
                    let Some(encoded_len) = serialized_len_with_limit(candidate, remaining) else {
                        return false;
                    };
                    let index = self.tools.len();
                    self.tools.push(candidate.clone());
                    self.namespace_indexes.insert(namespace.name.clone(), index);
                    self.encoded_len += separator + encoded_len;
                    return true;
                };

                let LoadableToolSpec::Namespace(existing) = &self.tools[existing_index] else {
                    unreachable!("namespace index must point to a namespace");
                };
                let mut next_len = self.encoded_len;
                let mut has_tools = !existing.tools.is_empty();
                for tool in &namespace.tools {
                    let separator = usize::from(has_tools);
                    let Some(remaining) = MAX_TOOL_SEARCH_RESULT_BYTES
                        .checked_sub(next_len)
                        .and_then(|remaining| remaining.checked_sub(separator))
                    else {
                        return false;
                    };
                    let Some(encoded_len) = serialized_len_with_limit(tool, remaining) else {
                        return false;
                    };
                    next_len += separator + encoded_len;
                    has_tools = true;
                }
                let LoadableToolSpec::Namespace(existing) = &mut self.tools[existing_index] else {
                    unreachable!("namespace index must point to a namespace");
                };
                existing.tools.extend(namespace.tools.iter().cloned());
                self.encoded_len = next_len;
                true
            }
        }
    }

    fn finish(self) -> (Vec<LoadableToolSpec>, usize) {
        (self.tools, self.encoded_len)
    }
}

struct ByteBudgetWriter {
    remaining: usize,
    written: usize,
}

impl ByteBudgetWriter {
    fn new(limit: usize) -> Self {
        Self {
            remaining: limit,
            written: 0,
        }
    }
}

impl Write for ByteBudgetWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if buf.len() > self.remaining {
            return Err(std::io::Error::new(
                std::io::ErrorKind::FileTooLarge,
                "tool search serialization exceeds its byte budget",
            ));
        }
        self.remaining -= buf.len();
        self.written += buf.len();
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn serialized_len_with_limit<T: serde::Serialize>(value: &T, limit: usize) -> Option<usize> {
    let mut writer = ByteBudgetWriter::new(limit);
    serde_json::to_writer(&mut writer, value).ok()?;
    Some(writer.written)
}

impl ToolSearchHandlerCache {
    #[instrument(level = "trace", skip_all, fields(search_info_count = search_infos.len()))]
    pub(crate) fn get_or_build(
        &self,
        search_infos: Vec<ToolSearchInfo>,
        cache_context: serde_json::Value,
    ) -> Arc<ToolSearchHandler> {
        let search_infos: Arc<[ToolSearchInfo]> = search_infos.into();

        // The small LRU stores the authoritative immutable inventory, so an
        // unchanged hit can be recognized without rebuilding its serialized
        // fingerprint. Fingerprinting is reserved for actual misses and the
        // per-key single-flight table.
        if let Some(handler) = self.cached_handler_for_inventory(&search_infos, &cache_context) {
            return handler;
        }

        let mut hasher = Sha256::new();
        hasher.update(tool_search_inventory_fingerprint(&search_infos));
        hasher.update(cache_context.to_string().as_bytes());
        let inventory_fingerprint: [u8; 32] = hasher.finalize().into();

        loop {
            let (flight, build_leader) = {
                let mut state = self.state();
                if let Some(handler) =
                    take_cached_handler(&mut state.cached, &search_infos, &cache_context)
                {
                    tracing::trace!(
                        cache_hit = true,
                        cached_inventory_count = state.cached.len(),
                        "tool search handler cache resolved after fingerprinting"
                    );
                    return handler;
                }
                if let Some(flight) = state.in_flight.get(&inventory_fingerprint) {
                    (Arc::clone(flight), false)
                } else {
                    let flight = Arc::new(ToolSearchBuildFlight::default());
                    state
                        .in_flight
                        .insert(inventory_fingerprint, Arc::clone(&flight));
                    (flight, true)
                }
            };

            if !build_leader {
                match flight.wait() {
                    Some(handler)
                        if handler.search_infos.as_ref() == search_infos.as_ref()
                            && handler.cache_context == cache_context =>
                    {
                        return handler;
                    }
                    Some(_) | None => continue,
                }
            }

            let built = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                Arc::new(ToolSearchHandler::new_with_fingerprint(
                    Arc::clone(&search_infos),
                    cache_context.clone(),
                ))
            }));
            match built {
                Ok(handler) => {
                    let (cached_inventory_count, evicted_inventory_count) = {
                        let mut state = self.state();
                        state.in_flight.remove(&inventory_fingerprint);
                        state.cached.push_back(Arc::clone(&handler));
                        let mut evicted_inventory_count = 0usize;
                        while state.cached.len() > MAX_TOOL_SEARCH_HANDLER_CACHE {
                            state.cached.pop_front();
                            evicted_inventory_count += 1;
                        }
                        (state.cached.len(), evicted_inventory_count)
                    };
                    flight.complete(Arc::clone(&handler));
                    tracing::trace!(
                        cache_hit = false,
                        cached_inventory_count,
                        evicted_inventory_count,
                        "tool search handler cache resolved"
                    );
                    return handler;
                }
                Err(payload) => {
                    self.state().in_flight.remove(&inventory_fingerprint);
                    flight.fail();
                    std::panic::resume_unwind(payload);
                }
            }
        }
    }

    fn cached_handler_for_inventory(
        &self,
        search_infos: &[ToolSearchInfo],
        cache_context: &serde_json::Value,
    ) -> Option<Arc<ToolSearchHandler>> {
        let mut state = self.state();
        let handler = take_cached_handler(&mut state.cached, search_infos, cache_context)?;
        tracing::trace!(
            cache_hit = true,
            cached_inventory_count = state.cached.len(),
            "tool search handler cache resolved"
        );
        Some(handler)
    }

    fn state(&self) -> std::sync::MutexGuard<'_, ToolSearchHandlerCacheState> {
        match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        }
    }




}

impl Default for ToolSearchHandlerCache {
    fn default() -> Self {
        Self {
            state: Mutex::new(ToolSearchHandlerCacheState::default()),
        }
    }
}

impl ToolSearchBuildFlight {
    // This waits on an OS thread. Production router construction runs inside
    // spawn_blocking in session::turn; keep future callers off async workers.
    fn wait(&self) -> Option<Arc<ToolSearchHandler>> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            match &*state {
                ToolSearchBuildFlightState::Building => {
                    state = self
                        .ready
                        .wait(state)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                }
                ToolSearchBuildFlightState::Ready(handler) => {
                    return Some(Arc::clone(handler));
                }
                ToolSearchBuildFlightState::Failed => return None,
            }
        }
    }

    fn complete(&self, handler: Arc<ToolSearchHandler>) {
        *self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            ToolSearchBuildFlightState::Ready(handler);
        self.ready.notify_all();
    }

    fn fail(&self) {
        *self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            ToolSearchBuildFlightState::Failed;
        self.ready.notify_all();
    }
}

fn take_cached_handler(
    cached: &mut VecDeque<Arc<ToolSearchHandler>>,
    search_infos: &[ToolSearchInfo],
    cache_context: &serde_json::Value,
) -> Option<Arc<ToolSearchHandler>> {
    let index = cached
        .iter()
        .position(|handler| {
            handler.search_infos.as_ref() == search_infos
                && &handler.cache_context == cache_context
        })?;
    let handler = cached.remove(index)?;
    cached.push_back(Arc::clone(&handler));
    Some(handler)
}

impl ToolSearchHandler {

    pub(crate) fn search_infos(&self) -> &[ToolSearchInfo] {
        &self.search_infos
    }

    fn new_with_fingerprint(
        search_infos: Arc<[ToolSearchInfo]>,
        cache_context: serde_json::Value,
    ) -> Self {
        let name_indexes: Arc<[ToolSearchNameIndex]> =
            search_infos.iter().map(ToolSearchNameIndex::new).collect();
        let mut exact_name_index = HashMap::<String, Vec<ToolSearchDocumentId>>::new();
        for (index, names) in name_indexes.iter().enumerate() {
            for name in &names.entry_names {
                exact_name_index
                    .entry(name.clone())
                    .or_default()
                    .push(ToolSearchDocumentId(index));
            }
        }
        let spec = create_tool_search_tool(TOOL_SEARCH_DEFAULT_LIMIT);
        let search_index = Arc::new(ToolSearchIndex::new(&search_infos));
        let mut folded_name_index = HashMap::<String, Vec<String>>::new();
        for name in exact_name_index.keys() {
            folded_name_index.entry(name.to_lowercase()).or_default().push(name.clone());
        }
        for names in folded_name_index.values_mut() {
            names.sort();
        }
        let namespace_member_indexes = search_infos.iter().enumerate().filter_map(|(id, info)| {
            let LoadableToolSpec::Namespace(namespace) = info.entry.output.as_ref() else { return None; };
            let members = namespace.tools.iter().map(|member| {
                let ResponsesApiNamespaceTool::Function(tool) = member;
                let mut member = info.clone();
                member.entry.search_text = codex_tools::namespace_member_search_text(
                    &namespace.name, &namespace.description, tool);
                member
            }).collect::<Vec<_>>();
            let index = ToolSearchIndex::new_with_callable_texts(&members, namespace.tools.iter().map(|member| {
                let ResponsesApiNamespaceTool::Function(tool) = member;
                codex_tools::ToolSearchEntry::callable_function_search_text(tool)
            }));
            Some((ToolSearchDocumentId(id), (members, index)))
        }).collect();

        Self {
            search_infos,
            cache_context,
            name_indexes,
            exact_name_index: Arc::new(exact_name_index),
            folded_name_index: Arc::new(folded_name_index),
            namespace_member_indexes: Arc::new(namespace_member_indexes),
            spec,
            search_index,
            result_cache: Arc::new(Mutex::new(VecDeque::new())),
        }
    }
}

fn tool_search_inventory_fingerprint(search_infos: &[ToolSearchInfo]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    for search_info in search_infos {
        update_fingerprint_field(&mut hasher, search_info.entry.search_text.as_bytes());
        if let Some(title) = &search_info.entry.callable_title {
            hasher.update([1]);
            update_fingerprint_field(&mut hasher, title.as_bytes());
        } else {
            hasher.update([0]);
        }
        for tool_name in &search_info.entry.tool_names {
            update_fingerprint_field(&mut hasher, tool_name.as_bytes());
        }
        if let Some(source_info) = &search_info.source_info {
            hasher.update([1]);
            update_fingerprint_field(&mut hasher, source_info.name.as_bytes());
            if let Some(description) = &source_info.description {
                hasher.update([1]);
                update_fingerprint_field(&mut hasher, description.as_bytes());
            } else {
                hasher.update([0]);
            }
        } else {
            hasher.update([0]);
        }
        if let Ok(encoded) = serde_json::to_vec(&search_info.entry.output) {
            update_fingerprint_field(&mut hasher, &encoded);
        }
        hasher.update([0xff]);
    }
    hasher.finalize().into()
}

fn update_fingerprint_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_le_bytes());
    hasher.update(value);
}

impl ToolExecutor<ToolInvocation> for ToolSearchHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(TOOL_SEARCH_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(self.handle_call(invocation))
    }
}

impl ToolSearchHandler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session: _,
            payload,
            step_context,
            cancellation_token,
            call_id,
            source,
            ..
        } = invocation;
        let turn = Arc::clone(&step_context.turn);

        let args = match payload {
            ToolPayload::ToolSearch { arguments } => arguments,
            _ => {
                return Err(FunctionCallError::Fatal(format!(
                    "{TOOL_SEARCH_TOOL_NAME} handler received unsupported payload"
                )));
            }
        };

        let key = self.resolved_query_key(&args.query, args.limit)?;
        let cancelled =
            || FunctionCallError::RespondToModel("tool search was cancelled".to_string());
        if cancellation_token.is_cancelled() {
            return Err(cancelled());
        }
        // Reject invalid requests before scheduling CPU work. The shared index
        // and cache stay off the async worker even when a search is contended.
        let handler = self.clone();
        let span = tracing::Span::current();
        let mut search = AbortOnDropHandle::new(tokio::task::spawn_blocking(move || {
            span.in_scope(|| handler.search_resolved(key))
        }));
        let result = tokio::select! {
            biased;
            _ = cancellation_token.cancelled() => {
                return Err(cancelled());
            }
            result = &mut search => result.map_err(|error| {
                FunctionCallError::Fatal(format!("tool search task failed: {error}"))
            })??,
        };
        if cancellation_token.is_cancelled() {
            return Err(cancelled());
        }
        // The runtime catalog already retains authoritative callable contracts.
        // Omitted/compacted contracts resolve locally in the same cell; never
        // bypass the search budget by republishing them as developer history.
        if source != crate::tools::context::ToolCallSource::Direct {
            // Nested searches must enable calls in the same executing cell.
            // Their result already carries all diagnostics, with no side writes.
            turn.activate_deferred_tools(result.activation_tools.iter().cloned());
        }

        let mut supplemental_contexts = Vec::new();
        if (!result.unactivated_matches.is_empty() || !result.unmatched_identifiers.is_empty())
            && source == crate::tools::context::ToolCallSource::Direct
            && crate::tools::effective_tool_mode(&turn)
                != codex_protocol::openai_models::ToolMode::CodeModeOnly
        {
            supplemental_contexts.extend([
                codex_protocol::models::ResponseItem::Message {
                    id: Some(codex_protocol::ResponseItemId::with_suffix(
                        "msg_turn_advice_tool_search_low", &call_id,
                    )),
                    role: "developer".to_string(),
                    content: vec![codex_protocol::models::ContentItem::InputText {
                        text: format!(
                            "Low-relevance tool names only (not activated; refine the query): {}. Unmatched query identifiers (not tool names or provider filters): {}",
                            serde_json::to_string(&result.unactivated_matches)
                                .map_err(|error| FunctionCallError::Fatal(error.to_string()))?,
                            serde_json::to_string(&result.unmatched_identifiers)
                                .map_err(|error| FunctionCallError::Fatal(error.to_string()))?,
                        ),
                    }],
                    phase: None,
                    internal_chat_message_metadata_passthrough: None,
                },
            ]);
        }
        let returned_names = result
            .tools
            .iter()
            .flat_map(loadable_tool_names)
            .collect::<HashSet<_>>();
        if let Some(ambiguity) = &result.exact_name_ambiguity
            && source == crate::tools::context::ToolCallSource::Direct
            && crate::tools::effective_tool_mode(&turn)
                != codex_protocol::openai_models::ToolMode::CodeModeOnly
        {
            // Native tool-search outputs accept definitions only. Keep the
            // disambiguation receipt in the same call's existing history owner.
            supplemental_contexts.extend([codex_protocol::models::ResponseItem::Message {
                id: Some(codex_protocol::ResponseItemId::with_suffix(
                    "msg_turn_advice_tool_search_ambiguous",
                    &call_id,
                )),
                role: "developer".into(),
                content: vec![codex_protocol::models::ContentItem::InputText {
                    text: format!(
                        "Ambiguous exact tool name; use a qualified alternative: {ambiguity}"
                    ),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            }]);
        }
        for item in &mut supplemental_contexts {
            item.set_turn_id_if_missing(&turn.sub_id);
        }
        if source == crate::tools::context::ToolCallSource::Direct {
            turn.queue_tool_search_context(
                &call_id,
                &result.activation_tools,
                supplemental_contexts,
            )
            .await;
        }
        Ok(boxed_tool_output(ToolSearchOutput {
            search_complete: self.search_complete(&result),
            tools: result.serialized_tools.clone(),
            omitted_result_count: result.omitted_result_count,
            activated_omitted_tools: result
                .activation_tools
                .iter()
                .filter(|name| {
                    !returned_names.contains(*name) || result.supplemental_tools.contains(name)
                })
                .map(|name| match &name.namespace {
                    Some(namespace) => format!("{namespace}.{}", name.name),
                    None => name.name.clone(),
                })
                .collect(),
            unactivated_matches: result.unactivated_matches.clone(),
            unmatched_identifiers: result.unmatched_identifiers.clone(),
            exact_name_ambiguity: result.exact_name_ambiguity.clone(),
        }))
    }
}

fn serialize_loadable_tools(tools: &[LoadableToolSpec]) -> Vec<serde_json::Value> {
    tools
        .iter()
        .map(|tool| {
            serde_json::to_value(tool).unwrap_or_else(|err| {
                serde_json::Value::String(format!("failed to serialize tool_search output: {err}"))
            })
        })
        .collect()
}

fn loadable_tool_names(spec: &LoadableToolSpec) -> Vec<ToolName> {
    spec.callable_tool_names()
}

impl CoreToolRuntime for ToolSearchHandler {}

impl ToolSearchHandler {
    fn identity_query_key(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<ToolSearchQueryKey, FunctionCallError> {
        let mut key = validate_tool_search_query(query, limit)?;
        if !self.exact_name_index.contains_key(&key.query) {
            let folded = key.query.to_lowercase();
            let alternatives = self.folded_name_index.get(&folded).map(Vec::as_slice).unwrap_or_default();
            match alternatives {
                [name] => key.query = name.clone(),
                // Preserve exact identities within batches and mixed queries;
                // lowercasing them can select a different callable.
                [] if key
                    .query
                    .split_whitespace()
                    .any(|term| self.exact_name_index.contains_key(term)) => {}
                [] => key.query = folded,
                _ => {
                    return Err(FunctionCallError::RespondToModel(format!(
                        "Ambiguous case-folded tool identity; use exact casing: {}",
                        alternatives.join(", ")
                    )));
                }
            }
        }
        Ok(key)
    }

    fn identity_query_terms(&self, query: &str) -> Vec<String> {
        query
            .split_whitespace()
            .filter(|term| !term.starts_with('+'))
            .flat_map(|term| {
                if self.exact_name_index.contains_key(term) {
                    vec![term.to_string()]
                } else {
                    ToolSearchTokenizer.tokenize(term)
                }
            })
            .collect()
    }

    fn resolved_query_key(
        &self,
        query: &str,
        requested: Option<usize>,
    ) -> Result<ToolSearchQueryKey, FunctionCallError> {
        let mut key =
            self.identity_query_key(query, requested.unwrap_or(TOOL_SEARCH_DEFAULT_LIMIT))?;
        if requested.is_some() {
            return Ok(key);
        }
        let names = self
            .exact_name_index
            .get(&key.query)
            .into_iter()
            .flatten()
            .filter(|id| matches_source(id.info(&self.search_infos), key.source.as_deref()))
            .flat_map(|id| {
                let selected = id
                    .name_index(&self.name_indexes)
                    .output_names_for(&key.query);
                loadable_tool_names(id.info(&self.search_infos).entry.output.as_ref())
                    .into_iter()
                    .filter(move |name| {
                        selected.is_some_and(|selected| selected.contains(&name.name))
                    })
            })
            .collect::<HashSet<_>>();
        if names.len() == 1 {
            key.limit = 1;
        }
        Ok(key)
    }



    fn search_resolved(
        &self,
        key: ToolSearchQueryKey,
    ) -> Result<Arc<ToolSearchResult>, FunctionCallError> {
        let limit = key.limit;
        if key.source.is_some()
            && !self
                .search_infos
                .iter()
                .any(|info| matches_source(info, key.source.as_deref()))
        {
            let scopes = self
                .search_infos
                .iter()
                .map(crate::tools::handlers::tool_search_spec::canonical_source)
                .collect::<std::collections::BTreeSet<_>>();
            let mut bytes = 0;
            let alternatives = scopes
                .iter()
                .take(16)
                .filter_map(|scope| {
                    bytes += scope.len() + 7;
                    (bytes <= 1024).then(|| format!("source:{scope}"))
                })
                .collect::<Vec<_>>();
            return Err(FunctionCallError::RespondToModel(format!(
                "No callable source `{}` exists in the current inventory. Use canonical scope tokens, not display names. Available: {} ({} omitted). Shared namespaces may contain multiple connectors.",
                key.source.as_deref().unwrap_or_default(),
                alternatives.join(", "),
                scopes.len() - alternatives.len(),
            )));
        }
        let query_terms = ToolSearchTokenizer.tokenize(&key.query);
        let missing_names = if self.exact_name_index.contains_key(&key.query) {
            Vec::new()
        } else {
            key.query
                .split_whitespace()
                .filter(|term| !term.starts_with('+'))
                .filter(|term| !self.exact_name_index.contains_key(*term))
                .flat_map(|term| ToolSearchTokenizer.tokenize(term))
                .filter(|term| term.contains('_') && !self.exact_name_index.contains_key(term))
                .collect::<Vec<_>>()
        };
        // A partially available batch of names can return its exact members
        // without making the model repeat each lookup. Missing names remain
        // diagnostics, never fuzzy substitutes or activation candidates.
        let partial_name_lookup = !missing_names.is_empty()
            && key.query.split_whitespace().count() > 1
            && query_terms.iter().all(|term| term.contains('_'))
            && key.query.split_whitespace().any(|name| {
                self.exact_name_index.get(name).is_some_and(|ids| {
                    ids.iter().any(|id| {
                        matches_source(id.info(&self.search_infos), key.source.as_deref())
                    })
                })
            });
        // Wholly missing name lookups remain strict. Mixed capability queries
        // may contain project/account/file identifiers.
        if !missing_names.is_empty()
            && !partial_name_lookup
            && (key.query.split_whitespace().count() == 1
                || query_terms.iter().all(|term| term.contains('_')))
        {
            return Err(FunctionCallError::RespondToModel(format!(
                "No deferred tool by these exact names exists in the current inventory: {}. Use plain-language capability terms to search for alternatives.",
                missing_names
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(", "),
            )));
        }
        if self.search_infos.is_empty() {
            return Ok(self.finish_search(key, ToolSearchResult {
                unmatched_identifiers: missing_names,
                ..Default::default()
            }));
        }

        if let Some(result) = self.cached_search_result(&key) {
            if tracing::enabled!(tracing::Level::TRACE) {
                tracing::trace!(
                    normalized_query_bytes = key.query.len(),
                    effective_limit = limit,
                    cache_hit = true,
                    output_tool_count = result.tools.len(),
                    output_source_count = loadable_tool_spec_diversity_count(&result.tools),
                    omitted_result_count = result.omitted_result_count,
                    "tool search completed"
                );
            }
            return Ok(result);
        }

        let required = required_query_terms(&key.query);
        let mut seen_exact = HashSet::new();
        let explicit_names = key.query.split_whitespace().collect::<Vec<_>>();
        let explicit_multi_name = explicit_names.len() > 1
            && explicit_names.iter().all(|term| {
                let matches = self
                    .exact_name_index
                    .get(*term)
                    .into_iter()
                    .flatten()
                    .filter(|id| matches_source(id.info(&self.search_infos), key.source.as_deref()))
                    .collect::<Vec<_>>();
                matches.len() == 1
                    && matches[0]
                        .name_index(&self.name_indexes)
                        .output_names_for(term)
                        .is_some_and(|names| names.len() == 1)
            });
        let identity_terms = self.identity_query_terms(&key.query);
        let exact_terms = if self.exact_name_index.contains_key(&key.query) {
            vec![key.query.as_str()]
        } else if explicit_multi_name || partial_name_lookup {
            explicit_names
                .into_iter()
                .chain(identity_terms.iter().map(String::as_str))
                .collect()
        } else {
            Vec::new()
        };
        let exact_matches = exact_terms
            .into_iter()
            .filter_map(|term| self.exact_name_index.get(term))
            .flatten()
            .copied()
            .filter(|id| matches_source(id.info(&self.search_infos), key.source.as_deref()))
            .filter(|id| {
                required.iter().all(|term| {
                    self.exact_name_index
                        .get(term)
                        .is_some_and(|ids| ids.contains(id))
                        || self
                            .search_index
                            .postings
                            .get(term)
                            .is_some_and(|postings| {
                                postings
                                    .binary_search_by_key(id, |(candidate, _)| *candidate)
                                    .is_ok()
                            })
                })
            })
            .filter(|id| seen_exact.insert(*id))
            .collect::<Vec<_>>();
        if partial_name_lookup || explicit_multi_name {
            // Name-only requests select exact identities, not ranked substitutes
            // that happen to share their case-folded capability terms.
            let mut result = self.search_output_tools(
                exact_matches.iter().copied(),
                Some(&key.query),
                limit,
                key.source.as_deref(),
            )?;
            result.unmatched_identifiers = missing_names;
            return Ok(self.finish_search(key, result));
        }
        if exact_matches.len() == 1 && limit == 1 {
            let mut result = self.search_output_tools(
                exact_matches.iter().copied(),
                Some(&key.query),
                limit,
                key.source.as_deref(),
            )?;
            if result.omitted_result_count == 0 && !result.tools.is_empty() {
                result.unmatched_identifiers = missing_names;
                return Ok(self.finish_search(key, result));
            }
        }
        let exact_match_count = exact_matches.len();
        let candidate_limit = tool_search_candidate_limit(limit, self.search_infos.len());
        let (candidates, weak_candidates) = self.search_index.top_matches(
            &key.query,
            candidate_limit,
            &self.search_infos,
            key.source.as_deref(),
            &exact_matches,
        );
        let candidate_count = candidates.len();
        // Reaching the bounded candidate window does not prove exhaustion.
        let candidates_complete = candidate_count < candidate_limit
            || candidate_limit == self.search_infos.len();
        let trace_enabled = tracing::enabled!(tracing::Level::TRACE);
        let candidate_source_count = trace_enabled.then(|| {
            tool_search_info_diversity_count(
                candidates.iter().map(|id| id.info(&self.search_infos)),
            )
        });
        let results =
            promote_exact_name_matches(&self.search_infos, &exact_matches, &candidates, limit);
        let result_count = results.len();
        let result_source_count = trace_enabled.then(|| {
            tool_search_info_diversity_count(results.iter().map(|id| id.info(&self.search_infos)))
        });
        // Preserve the initial diversity selection, but retain ranked candidates
        // to refill slots whose definitions cannot fit the output budget.
        let mut seen = results.iter().copied().collect::<HashSet<_>>();
        let remaining = exact_matches
            .into_iter()
            .chain(candidates)
            .filter(|id| seen.insert(*id));
        let selection = results
            .into_iter()
            .chain(remaining)
            .take(candidate_limit)
            .chain(weak_candidates);
        let mut result =
            self.search_output_tools(selection, Some(&key.query), limit, key.source.as_deref())?;
        result.enumeration_complete &= candidates_complete;
        result.unmatched_identifiers = missing_names;
        if let (Some(candidate_source_count), Some(result_source_count)) =
            (candidate_source_count, result_source_count)
        {
            tracing::trace!(
                normalized_query_bytes = key.query.len(),
                effective_limit = limit,
                cache_hit = false,
                exact_match_count,
                candidate_limit,
                candidate_count,
                candidate_source_count,
                result_count,
                result_source_count,
                output_tool_count = result.tools.len(),
                output_source_count = loadable_tool_spec_diversity_count(&result.tools),
                omitted_result_count = result.omitted_result_count,
                "tool search ranking completed"
            );
        }
        // Incomplete receipts remain retryable, never a deduplication barrier.
        Ok(self.finish_search(key, result))
    }

    fn finish_search(&self, key: ToolSearchQueryKey, mut result: ToolSearchResult) -> Arc<ToolSearchResult> {
        // Resolve every named identity from the full inventory, not the limited
        // result page. Separate unambiguous batch members from true collisions.
        let mut ambiguous = std::collections::BTreeSet::new();
        let terms = if self.exact_name_index.contains_key(&key.query) {
            vec![key.query.as_str()]
        } else if key.query.split_whitespace().all(|term| {
            self.exact_name_index.contains_key(term)
                || (term.contains('_') && !term.starts_with('+'))
        }) {
            key.query.split_whitespace().collect()
        } else { Vec::new() };
        for term in terms {
            let names = self.exact_name_index.get(term).into_iter().flatten()
                .filter(|id| matches_source(id.info(&self.search_infos), key.source.as_deref()))
                .flat_map(|id| {
                    let selected = id.name_index(&self.name_indexes).output_names_for(term);
                    loadable_tool_names(id.info(&self.search_infos).entry.output.as_ref()).into_iter()
                        .filter(move |name| selected.is_some_and(|names| names.contains(&name.name)))
                }).collect::<std::collections::BTreeSet<_>>();
            if names.len() > 1 { ambiguous.extend(names); }
        }
        if !ambiguous.is_empty() {
            let mut bytes = 0;
            let alternatives = ambiguous.iter().take(8).map(ToString::to_string).take_while(|name| {
                bytes += name.len(); bytes <= 2048
            }).collect::<Vec<_>>();
            result.exact_name_ambiguity = Some(serde_json::json!({
                "match_count": ambiguous.len(), "omitted_alternative_count": ambiguous.len() - alternatives.len(),
                "qualified_alternatives": alternatives,
            }));
        }
        let result = Arc::new(result);
        tracing::trace!(normalized_query_bytes = key.query.len(), effective_limit = key.limit,
            cache_hit = false, output_tool_count = result.tools.len(),
            output_source_count = loadable_tool_spec_diversity_count(&result.tools),
            omitted_result_count = result.omitted_result_count, "tool search completed");
        self.cache_search_result(key, &result);
        result
    }

    fn search_output_tools(
        &self,
        results: impl IntoIterator<Item = ToolSearchDocumentId>,
        exact_query: Option<&str>,
        limit: usize,
        source: Option<&str>,
    ) -> Result<ToolSearchResult, FunctionCallError> {
        let mut retained = ToolSearchResultBuilder::new();
        let mut activation_tools = Vec::new();
        let mut supplemental_tools = Vec::new();
        let mut unactivated_matches = Vec::new();
        let mut unactivated_bytes = 0;
        let mut omitted_result_count = 0usize;
        let mut enumeration_complete = true;
        let mut selected = HashSet::new();
        let query_tokens = exact_query
            .map(|query| ToolSearchTokenizer.tokenize(query))
            .unwrap_or_default();
        let identity_terms = exact_query
            .map(|query| self.identity_query_terms(query))
            .unwrap_or_default();
        let evidence = self
            .search_index
            .query_evidence(&query_tokens, &self.search_infos, source);
        let mut results = results.into_iter().peekable();
        'results: while let Some(result_id) = results.next() {
            let result = &result_id.info(&self.search_infos).entry;
            let relevant = exact_query.is_none_or(|query| {
                let identity_list = query.split_whitespace().all(|term| {
                    self.exact_name_index.contains_key(term)
                        || (term.contains('_') && !term.starts_with('+'))
                });
                std::iter::once(query)
                    .chain(query.split_whitespace().filter(|_| identity_list))
                    .chain(identity_terms.iter().map(String::as_str).filter(|_| identity_list))
                    .any(|term| {
                        self.exact_name_index
                            .get(term)
                            .is_some_and(|ids| ids.contains(&result_id))
                    })
                    || self.search_index.relevance(&evidence, result_id)
                        >= MIN_TOOL_ACTIVATION_RELEVANCE
            });
            if !relevant {
                for name in loadable_tool_names(result.output.as_ref()) {
                    let name = name.to_string();
                    let bytes = serde_json::to_vec(&name)
                        .map_err(|error| FunctionCallError::Fatal(error.to_string()))?
                        .len()
                        + 1;
                    if unactivated_matches.len() < limit
                        && unactivated_bytes + bytes <= MAX_TOOL_SEARCH_RESULT_BYTES / 4
                        && !unactivated_matches.contains(&name)
                    {
                        unactivated_bytes += bytes;
                        unactivated_matches.push(name);
                    }
                }
                continue;
            }
            let exact_output_names = exact_query.and_then(|query| {
                if let Some(names) = result_id
                    .name_index(&self.name_indexes)
                    .output_names_for(query)
                {
                    return Some(names.clone());
                }
                let names = std::iter::once(query)
                    .chain(query.split_whitespace())
                    .chain(identity_terms.iter().map(String::as_str))
                    .filter_map(|term| {
                        result_id
                            .name_index(&self.name_indexes)
                            .output_names_for(term)
                    })
                    .flatten()
                    .cloned()
                    .collect::<HashSet<_>>();
                (!names.is_empty()).then_some(names)
            });
            // Select callable identities before charging the receipt budget.
            // A namespace is a container, not one callable.
            let candidates: Box<dyn Iterator<Item = LoadableToolSpec> + '_> = match result
                .output
                .as_ref()
            {
                LoadableToolSpec::Function(_) => {
                    Box::new(std::iter::once_with(|| result.output.as_ref().clone()))
                }
                LoadableToolSpec::Namespace(namespace) => {
                    let order = if let Some(query) = exact_query
                        .filter(|_| namespace.tools.len() > 1 && exact_output_names.is_none())
                    {
                        // Rank callable descriptions before applying the callable
                        // limit. Share the authoritative output Arc; do not clone
                        // every schema merely to build ranking documents.
                        let (members, index) = &self.namespace_member_indexes[&result_id];
                        index
                            .top_matches(
                                query,
                                members.len(),
                                members,
                                source.map(|_| namespace.name.as_str()),
                                &[],
                            )
                            .0
                            .into_iter()
                            .map(|id| id.0)
                            .collect::<Vec<_>>()
                    } else {
                        (0..namespace.tools.len()).collect()
                    };
                    Box::new(
                        order
                            .into_iter()
                            .map(|index| &namespace.tools[index])
                            .filter(move |tool| {
                                let ResponsesApiNamespaceTool::Function(tool) = tool;
                                exact_output_names
                                    .as_ref()
                                    .is_none_or(|names| names.contains(&tool.name))
                            })
                            .map(|tool| {
                                LoadableToolSpec::Namespace(ResponsesApiNamespace {
                                    name: namespace.name.clone(),
                                    description: namespace.description.clone(),
                                    tools: vec![tool.clone()],
                                })
                            }),
                    )
                }
            };
            let mut candidates = candidates.peekable();
            while let Some(candidate) = candidates.next() {
                let names = loadable_tool_names(&candidate);
                if names.iter().all(|name| selected.contains(name)) {
                    continue;
                }
                selected.extend(names.iter().cloned());
                activation_tools.extend(names.iter().cloned());
                let candidate = result.normalize_output(candidate);
                if !retained.try_push(&candidate) {
                    supplemental_tools.extend(names);
                    let recovery = match &candidate {
                        // Flat recovery retains the complete serialized call
                        // contract and adds prose, so it cannot fit after this
                        // same candidate exhausted the unchanged byte budget.
                        LoadableToolSpec::Function(_) => None,
                        LoadableToolSpec::Namespace(namespace) => {
                            let local_names = namespace.tools.iter().map(|tool| {
                                let ResponsesApiNamespaceTool::Function(tool) = tool;
                                tool.name.clone()
                            }).collect();
                            compact_exact_match_recovery(&candidate, &local_names)
                        }
                    };
                    if !recovery.is_some_and(|recovery| retained.try_push(&recovery))
                    {
                        // The incomplete receipt reports omitted definitions;
                        // the next request advertises the authoritative contract.
                        omitted_result_count += 1;
                    }
                }
                if selected.len() == limit {
                    enumeration_complete = candidates.peek().is_none() && results.peek().is_none();
                    break 'results;
                }
            }
        }
        let (tools, encoded_tools_len) = retained.finish();
        activation_tools.extend(tools.iter().flat_map(loadable_tool_names));
        activation_tools.sort_unstable();
        activation_tools.dedup();
        let serialized_tools = serialize_loadable_tools(&tools);
        Ok(ToolSearchResult {
            enumeration_complete,
            tools,
            serialized_tools,
            activation_tools,
            supplemental_tools,
            unactivated_matches,
            unmatched_identifiers: Vec::new(),
            omitted_result_count,
            encoded_tools_len,
            exact_name_ambiguity: None,
        })
    }

    fn cached_search_result(&self, key: &ToolSearchQueryKey) -> Option<Arc<ToolSearchResult>> {
        let mut cache = self.result_cache();
        let index = cache.iter().position(|entry| &entry.key == key)?;
        let entry = cache.remove(index)?;
        let result = entry.result.clone();
        cache.push_back(entry);
        Some(result)
    }

    fn cache_search_result(&self, key: ToolSearchQueryKey, result: &Arc<ToolSearchResult>) {
        if !self.search_complete(result) {
            return;
        }
        if !tool_search_cache_entry_fits_budget(&key, result) {
            tracing::trace!(
                normalized_query_bytes = key.query.len(),
                output_tool_count = result.tools.len(),
                cache_entry_byte_limit = MAX_TOOL_SEARCH_CACHE_ENTRY_BYTES,
                "skipped oversized tool search cache entry"
            );
            return;
        }

        let mut cache = self.result_cache();
        if let Some(index) = cache.iter().position(|entry| entry.key == key) {
            cache.remove(index);
        }
        cache.push_back(ToolSearchCacheEntry {
            key,
            result: Arc::clone(result),
        });
        while cache.len() > MAX_TOOL_SEARCH_RESULT_CACHE {
            cache.pop_front();
        }
    }

    fn search_complete(&self, result: &ToolSearchResult) -> bool {
        self.cache_context["enumeration_complete"] == true
            && result.enumeration_complete
            && result.omitted_result_count == 0
            && result.supplemental_tools.is_empty()
            && result.unmatched_identifiers.is_empty()
            && result.unactivated_matches.is_empty()
            && result.exact_name_ambiguity.is_none()
    }

    fn result_cache(&self) -> std::sync::MutexGuard<'_, VecDeque<ToolSearchCacheEntry>> {
        match self.result_cache.lock() {
            Ok(cache) => cache,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

}

fn compact_exact_match_recovery(
    output: &LoadableToolSpec,
    exact_names: &HashSet<String>,
) -> Option<LoadableToolSpec> {
    match output {
        LoadableToolSpec::Namespace(namespace) => {
            let tools = namespace
                .tools
                .iter()
                .filter_map(|tool| match tool {
                    ResponsesApiNamespaceTool::Function(tool)
                        if exact_names.contains(&tool.name) =>
                    {
                        Some(ResponsesApiNamespaceTool::Function(compact_recovery_tool(
                            tool,
                            &format!("{}.{}", namespace.name, tool.name),
                        )))
                    }
                    ResponsesApiNamespaceTool::Function(_) => None,
                })
                .collect::<Vec<_>>();
            (!tools.is_empty()).then(|| {
                LoadableToolSpec::Namespace(ResponsesApiNamespace {
                    name: namespace.name.clone(),
                    description: format!(
                        "Compact recovery for an exact tool match in `{}`; the full schema exceeded the tool-search response budget.",
                        namespace.name
                    ),
                    tools,
                })
            })
        }
        LoadableToolSpec::Function(_) => None,
    }
}

fn compact_recovery_tool(tool: &ResponsesApiTool, qualified_name: &str) -> ResponsesApiTool {
    ResponsesApiTool {
        name: tool.name.clone(),
        description: format!(
            "{}\n\nCompact exact-match definition for `{qualified_name}`; verbose schema details were removed to fit the tool-search response budget.",
            tool.description
        ),
        strict: tool.strict,
        defer_loading: Some(true),
        // Descriptions can specify units, prerequisites, or combinations that
        // JSON Schema cannot express. Budget recovery must not change the call
        // contract; if it still does not fit, the omission path supplies a locator.
        parameters: tool.parameters.clone(),
        output_schema: tool.output_schema.clone().map(|schema| {
            let mut schema = schema.into_value();
            strip_output_schema_descriptions(&mut schema);
            schema.into()
        }),
    }
}

fn strip_output_schema_descriptions(schema: &mut serde_json::Value) {
    let Some(schema) = schema.as_object_mut() else {
        return;
    };
    schema.remove("description");
    // Visit schema positions only: a property named `description` or an
    // object inside `const`, `enum`, or `default` is part of the contract.
    for keyword in [
        "properties",
        "patternProperties",
        "$defs",
        "definitions",
        "dependentSchemas",
        "dependencies",
    ] {
        if let Some(children) = schema
            .get_mut(keyword)
            .and_then(serde_json::Value::as_object_mut)
        {
            for child in children.values_mut() {
                strip_output_schema_descriptions(child);
            }
        }
    }
    for keyword in [
        "items",
        "prefixItems",
        "additionalItems",
        "additionalProperties",
        "unevaluatedItems",
        "unevaluatedProperties",
        "contains",
        "propertyNames",
        "not",
        "if",
        "then",
        "else",
        "anyOf",
        "oneOf",
        "allOf",
    ] {
        if let Some(child) = schema.get_mut(keyword) {
            if let Some(children) = child.as_array_mut() {
                for child in children {
                    strip_output_schema_descriptions(child);
                }
            } else {
                strip_output_schema_descriptions(child);
            }
        }
    }
}

fn normalize_tool_search_query(query: &str) -> String {
    query.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn validate_tool_search_query(
    query: &str,
    limit: usize,
) -> Result<ToolSearchQueryKey, FunctionCallError> {
    if query.len() > MAX_TOOL_SEARCH_QUERY_BYTES {
        return Err(FunctionCallError::RespondToModel(format!(
            "query must not exceed {MAX_TOOL_SEARCH_QUERY_BYTES} bytes"
        )));
    }
    if limit == 0 {
        return Err(FunctionCallError::RespondToModel(
            "limit must be greater than zero".to_string(),
        ));
    }
    if limit > MAX_TOOL_SEARCH_LIMIT {
        return Err(FunctionCallError::RespondToModel(format!(
            "limit must not exceed {MAX_TOOL_SEARCH_LIMIT}"
        )));
    }

    let mut source = None;
    let mut terms = Vec::new();
    for term in query.split_whitespace() {
        // A source scope already requires exact membership. Accept the
        // redundant required-term prefix without searching for `source` in
        // provider metadata or silently dropping the requested boundary.
        if let Some(identity) = term
            .strip_prefix("source:")
            .or_else(|| term.strip_prefix("+source:"))
        {
            if identity.is_empty() || source.is_some() {
                return Err(FunctionCallError::RespondToModel(
                    "Use at most one nonempty source:<canonical namespace> scope.".to_string(),
                ));
            }
            source = Some(identity.to_string());
        } else {
            terms.push(term);
        }
    }
    let query = normalize_tool_search_query(&terms.join(" "));
    if query.is_empty() {
        return Err(FunctionCallError::RespondToModel(
            if source.is_some() {
                "A source scope needs a capability term or exact tool name, for example `spawn_agent source:agents`. In Code Mode, filter ALL_TOOL_NAMES locally to inspect names without a search. No search ran."
            } else {
                "query must not be empty"
            }.to_string(),
        ));
    }

    Ok(ToolSearchQueryKey {
        query,
        limit,
        source,
    })
}

fn matches_source(info: &ToolSearchInfo, source: Option<&str>) -> bool {
    source.is_none_or(|source| match info.entry.output.as_ref() {
        LoadableToolSpec::Namespace(namespace) => namespace.name == source,
        LoadableToolSpec::Function(tool) => tool.name == source,
    })
}

fn tool_search_candidate_limit(effective_limit: usize, inventory_size: usize) -> usize {
    effective_limit
        .saturating_mul(TOOL_SEARCH_CANDIDATE_MULTIPLIER)
        .min(inventory_size)
}

fn tool_search_cache_entry_fits_budget(
    key: &ToolSearchQueryKey,
    result: &ToolSearchResult,
) -> bool {
    // Both representations are retained. Activation names can exceed the response
    // budget when even a compact exact match is too large to return.
    let Some(remaining) = result
        .encoded_tools_len
        .checked_mul(2)
        .and_then(|bytes| bytes.checked_add(key.query.len()))
        .and_then(|bytes| MAX_TOOL_SEARCH_CACHE_ENTRY_BYTES.checked_sub(bytes))
    else {
        return false;
    };
    let mut writer = ByteBudgetWriter::new(remaining);
    if serde_json::to_writer(&mut writer, &result.activation_tools).is_err()
        || serde_json::to_writer(&mut writer, &result.supplemental_tools).is_err()
        || serde_json::to_writer(&mut writer, &result.unactivated_matches).is_err()
        || serde_json::to_writer(&mut writer, &result.unmatched_identifiers).is_err()
    {
        return false;
    }

    // Output schemas are owned by the typed tools but skipped by their serializer.
    let mut schema_fits = |tool: &ResponsesApiTool| {
        serde_json::to_writer(
            &mut writer,
            &tool
                .output_schema
                .as_ref()
                .map(codex_tools::ToolOutputSchema::to_value),
        )
        .is_ok()
    };
    result.tools.iter().all(|tool| match tool {
        LoadableToolSpec::Function(tool) => schema_fits(tool),
        LoadableToolSpec::Namespace(namespace) => namespace.tools.iter().all(|tool| {
            let ResponsesApiNamespaceTool::Function(tool) = tool;
            schema_fits(tool)
        }),
    })
}

fn promote_exact_name_matches(
    search_infos: &[ToolSearchInfo],
    exact_matches: &[ToolSearchDocumentId],
    ranked_results: &[ToolSearchDocumentId],
    limit: usize,
) -> Vec<ToolSearchDocumentId> {
    let mut results = Vec::with_capacity(exact_matches.len().saturating_add(ranked_results.len()));
    let mut seen = HashSet::<ToolSearchDocumentId>::new();

    for result in exact_matches.iter().chain(ranked_results).copied() {
        if seen.insert(result) {
            results.push(result);
        }
    }

    diversify_search_result_ids(search_infos, results, limit)
}

fn diversify_search_result_ids(
    search_infos: &[ToolSearchInfo],
    results: Vec<ToolSearchDocumentId>,
    limit: usize,
) -> Vec<ToolSearchDocumentId> {
    if results.len() <= limit {
        return results;
    }

    let mut remaining = results;
    let mut diversified = Vec::with_capacity(limit);
    let mut seen_this_pass = HashSet::new();

    while !remaining.is_empty() && diversified.len() < limit {
        let mut deferred = Vec::new();
        let mut added_this_pass = false;

        for result_id in remaining {
            if diversified.len() >= limit {
                break;
            }
            let result = result_id.info(search_infos);
            if seen_this_pass.insert(tool_search_info_diversity_key(result)) {
                diversified.push(result_id);
                added_this_pass = true;
            } else {
                deferred.push(result_id);
            }
        }

        if !added_this_pass {
            diversified.extend(deferred.into_iter().take(limit - diversified.len()));
            break;
        }

        remaining = deferred;
        seen_this_pass.clear();
    }

    diversified
}

fn tool_search_info_diversity_count<'a>(
    results: impl IntoIterator<Item = &'a ToolSearchInfo>,
) -> usize {
    results
        .into_iter()
        .map(tool_search_info_diversity_key)
        .collect::<HashSet<_>>()
        .len()
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum ToolSearchDiversityKey<'a> {
    Source(&'a str),
    Function(&'a str),
    Namespace(&'a str),
}

fn tool_search_info_diversity_key(search_info: &ToolSearchInfo) -> ToolSearchDiversityKey<'_> {
    search_info
        .source_info
        .as_ref()
        .map(|source| ToolSearchDiversityKey::Source(source.name.as_str()))
        .unwrap_or_else(|| loadable_tool_spec_diversity_key(&search_info.entry.output))
}

fn loadable_tool_spec_diversity_count(specs: &[LoadableToolSpec]) -> usize {
    specs
        .iter()
        .map(loadable_tool_spec_diversity_key)
        .collect::<HashSet<_>>()
        .len()
}

fn loadable_tool_spec_diversity_key(spec: &LoadableToolSpec) -> ToolSearchDiversityKey<'_> {
    match spec {
        LoadableToolSpec::Function(tool) => ToolSearchDiversityKey::Function(tool.name.as_str()),
        LoadableToolSpec::Namespace(namespace) => {
            ToolSearchDiversityKey::Namespace(namespace.name.as_str())
        }
    }
}

