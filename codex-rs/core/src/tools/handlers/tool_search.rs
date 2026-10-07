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
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
#[cfg(test)]
use std::sync::atomic::Ordering;
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
// task entities are not negative capability evidence. A single indexed term
// qualifies a multiword query only within an explicit canonical source scope.
const MIN_TOOL_ACTIVATION_RELEVANCE: f32 = 0.5;

#[cfg(test)]
static LOADABLE_TOOL_SERIALIZATION_COUNT: AtomicUsize = AtomicUsize::new(0);

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
    name_indexes: Arc<[ToolSearchNameIndex]>,
    exact_name_index: Arc<HashMap<String, Vec<ToolSearchDocumentId>>>,
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
    fn relevance(&self, query: &str, id: ToolSearchDocumentId, source_scoped: bool) -> f32 {
        let terms = ToolSearchTokenizer.tokenize(query).into_iter().collect::<HashSet<_>>();
        let indexed = terms.iter().filter(|term| self.postings.contains_key(*term))
            .collect::<Vec<_>>();
        let minimum_matches = if terms.len() > 1 && !(source_scoped && indexed.len() == 1) { 2 } else { 1 };
        if indexed.len() < minimum_matches {
            return 0.0;
        }
        let matches = indexed.iter().filter(|term| {
            self.callable_terms[id.0].contains(term.as_str())
        }).count();
        if matches < minimum_matches { return 0.0; }
        matches as f32 / indexed.len() as f32
    }

    fn new(search_infos: &[ToolSearchInfo]) -> Self {
        const K1: f32 = 1.2;
        const B: f32 = 0.75;
        const FALLBACK_AVERAGE_DOCUMENT_LENGTH: f32 = 256.0;

        let tokenizer = ToolSearchTokenizer;
        let tokenized_documents = search_infos
            .iter()
            .map(|search_info| tokenizer.tokenize(&search_info.entry.search_text))
            .collect::<Vec<_>>();
        let average_document_length = {
            let total_document_length = tokenized_documents.iter().map(Vec::len).sum::<usize>();
            let average = if tokenized_documents.is_empty() {
                0.0
            } else {
                total_document_length as f64 / tokenized_documents.len() as f64
            };
            let average = average as f32;
            if average > 0.0 {
                average
            } else {
                FALLBACK_AVERAGE_DOCUMENT_LENGTH
            }
        };

        let mut postings = HashMap::<String, Vec<(ToolSearchDocumentId, f32)>>::new();
        let mut term_frequencies = HashMap::<String, usize>::new();
        for (index, tokens) in tokenized_documents.into_iter().enumerate() {
            let document_length = tokens.len() as f32;
            for token in tokens {
                *term_frequencies.entry(token).or_default() += 1;
            }
            for (token, term_frequency) in term_frequencies.drain() {
                let term_frequency = term_frequency as f32;
                let weight = term_frequency * (K1 + 1.0)
                    / (term_frequency
                        + K1 * (1.0 - B + B * document_length / average_document_length));
                postings
                    .entry(token)
                    .or_default()
                    .push((ToolSearchDocumentId(index), weight));
            }
        }

        Self {
            postings,
            callable_terms: search_infos.iter().map(|info|
                tokenizer.tokenize(&info.entry.callable_search_text()).into_iter().collect()).collect(),
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
        let required = required_query_terms(query);
        let mut scores = HashMap::<ToolSearchDocumentId, f32>::new();
        for token in tokens {
            let Some(postings) = self.postings.get(&token) else {
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
            if !matches_source(id.info(search_infos), source) { continue; }
            if !required.iter().all(|term| self.postings.get(term)
                .is_some_and(|postings| postings.iter().any(|(candidate, _)| *candidate == id)))
            {
                continue;
            }
            let candidate = RankedToolSearchDocument { id, score };
            // Eligibility must precede bounded admission: otherwise short weak
            // documents can evict a lower-scoring eligible capability.
            let eligible = exact_matches.contains(&id)
                || self.relevance(query, id, source.is_some()) >= MIN_TOOL_ACTIVATION_RELEVANCE;
            let heap = if eligible { &mut by_source } else { &mut weak_by_source };
            let best = heap.entry(tool_search_info_diversity_key(id.info(search_infos)))
                .or_default();
            if best.len() < limit {
                best.push(Reverse(candidate));
            } else if best.peek().is_some_and(|worst| candidate > worst.0) {
                best.pop();
                best.push(Reverse(candidate));
            }
        }

        let mut ranked = [by_source, weak_by_source].map(|heap| {
            let mut best = heap.into_values().flatten().map(|candidate| candidate.0).collect::<Vec<_>>();
            best.sort_unstable_by(|left, right| right.cmp(left));
            diversify_search_result_ids(search_infos, best.into_iter().map(|candidate| candidate.id).collect(), limit)
        });
        (std::mem::take(&mut ranked[0]), std::mem::take(&mut ranked[1]))
    }
}

fn required_query_terms(query: &str) -> Vec<String> {
    query.split_whitespace().filter_map(|term| term.strip_prefix('+'))
        .flat_map(|term| ToolSearchTokenizer.tokenize(term)).collect()
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

    #[cfg(test)]
    fn has_entry_name(&self, normalized_query: &str) -> bool {
        self.entry_names.contains(normalized_query)
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
    #[cfg(test)]
    fingerprint_compute_count: AtomicUsize,
    #[cfg(test)]
    handler_build_count: AtomicUsize,
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
    pub(crate) fn get_or_build(&self, search_infos: Vec<ToolSearchInfo>) -> Arc<ToolSearchHandler> {
        let search_infos: Arc<[ToolSearchInfo]> = search_infos.into();

        // The small LRU stores the authoritative immutable inventory, so an
        // unchanged hit can be recognized without rebuilding its serialized
        // fingerprint. Fingerprinting is reserved for actual misses and the
        // per-key single-flight table.
        if let Some(handler) = self.cached_handler_for_inventory(&search_infos) {
            return handler;
        }

        #[cfg(test)]
        self.fingerprint_compute_count
            .fetch_add(1, Ordering::Relaxed);
        let inventory_fingerprint = tool_search_inventory_fingerprint(&search_infos);

        loop {
            let (flight, build_leader) = {
                let mut state = self.state();
                if let Some(handler) = take_cached_handler(&mut state.cached, &search_infos) {
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
                    Some(handler) if handler.search_infos.as_ref() == search_infos.as_ref() => {
                        return handler;
                    }
                    Some(_) | None => continue,
                }
            }

            #[cfg(test)]
            self.handler_build_count.fetch_add(1, Ordering::Relaxed);
            let built = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                Arc::new(ToolSearchHandler::new_with_fingerprint(Arc::clone(
                    &search_infos,
                )))
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
    ) -> Option<Arc<ToolSearchHandler>> {
        let mut state = self.state();
        let handler = take_cached_handler(&mut state.cached, search_infos)?;
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

    #[cfg(test)]
    fn cached_len(&self) -> usize {
        self.state().cached.len()
    }

    #[cfg(test)]
    pub(crate) fn search_infos_for_test(&self) -> Vec<ToolSearchInfo> {
        self.state()
            .cached
            .back()
            .map(|handler| handler.search_infos.to_vec())
            .unwrap_or_default()
    }

    #[cfg(test)]
    fn fingerprint_compute_count(&self) -> usize {
        self.fingerprint_compute_count.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    fn handler_build_count(&self) -> usize {
        self.handler_build_count.load(Ordering::Relaxed)
    }
}

impl Default for ToolSearchHandlerCache {
    fn default() -> Self {
        Self {
            state: Mutex::new(ToolSearchHandlerCacheState::default()),
            #[cfg(test)]
            fingerprint_compute_count: AtomicUsize::new(0),
            #[cfg(test)]
            handler_build_count: AtomicUsize::new(0),
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
) -> Option<Arc<ToolSearchHandler>> {
    let index = cached
        .iter()
        .position(|handler| handler.search_infos.as_ref() == search_infos)?;
    let handler = cached.remove(index)?;
    cached.push_back(Arc::clone(&handler));
    Some(handler)
}

impl ToolSearchHandler {
    #[cfg(test)]
    #[instrument(
        level = "trace",
        skip_all,
        fields(search_info_count = search_infos.len())
    )]
    pub(crate) fn new(search_infos: Vec<ToolSearchInfo>) -> Self {
        Self::new_with_fingerprint(search_infos.into())
    }

    fn new_with_fingerprint(search_infos: Arc<[ToolSearchInfo]>) -> Self {
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

        Self {
            search_infos,
            name_indexes,
            exact_name_index: Arc::new(exact_name_index),
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

        let limit = self.effective_limit(&args.query, args.limit)?;
        let cancelled =
            || FunctionCallError::RespondToModel("tool search was cancelled".to_string());
        if cancellation_token.is_cancelled() {
            return Err(cancelled());
        }
        // Reject invalid requests before scheduling CPU work. The shared index
        // and cache stay off the async worker even when a search is contended.
        validate_tool_search_query(&args.query, limit)?;
        let handler = self.clone();
        let span = tracing::Span::current();
        let mut search = AbortOnDropHandle::new(tokio::task::spawn_blocking(move || {
            span.in_scope(|| handler.search(&args.query, limit))
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
        let returned_names = result.tools.iter().flat_map(loadable_tool_names).collect::<HashSet<_>>();
        if let Some(ambiguity) = &result.exact_name_ambiguity
            && source == crate::tools::context::ToolCallSource::Direct
            && crate::tools::effective_tool_mode(&turn)
                != codex_protocol::openai_models::ToolMode::CodeModeOnly
        {
            // Native tool-search outputs accept definitions only. Keep the
            // disambiguation receipt in the same call's existing history owner.
            supplemental_contexts.extend([
                codex_protocol::models::ResponseItem::Message {
                    id: Some(codex_protocol::ResponseItemId::with_suffix(
                        "msg_turn_advice_tool_search_ambiguous", &call_id,
                    )),
                    role: "developer".into(),
                    content: vec![codex_protocol::models::ContentItem::InputText {
                        text: format!("Ambiguous exact tool name; use a qualified alternative: {ambiguity}"),
                    }],
                    phase: None,
                    internal_chat_message_metadata_passthrough: None,
                },
            ]);
        }
        for item in &mut supplemental_contexts {
            item.set_turn_id_if_missing(&turn.sub_id);
        }
        if source == crate::tools::context::ToolCallSource::Direct {
            turn.queue_tool_search_context(&call_id, &result.activation_tools, supplemental_contexts).await;
        }
        Ok(boxed_tool_output(ToolSearchOutput {
            tools: result.serialized_tools.clone(),
            omitted_result_count: result.omitted_result_count,
            activated_omitted_tools: result.activation_tools.iter()
                .filter(|name| !returned_names.contains(*name) || result.supplemental_tools.contains(name))
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
            #[cfg(test)]
            LOADABLE_TOOL_SERIALIZATION_COUNT.fetch_add(1, Ordering::Relaxed);
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
    fn identity_query_key(&self, query: &str, limit: usize) -> Result<ToolSearchQueryKey, FunctionCallError> {
        let mut key = validate_tool_search_query(query, limit)?;
        if !self.exact_name_index.contains_key(&key.query) {
            let folded = key.query.to_lowercase();
            let mut alternatives = self.exact_name_index.keys()
                .filter(|name| name.to_lowercase() == folded)
                .cloned().collect::<Vec<_>>();
            alternatives.sort();
            match alternatives.as_slice() {
                [name] => key.query = name.clone(),
                [] => key.query = folded,
                _ => return Err(FunctionCallError::RespondToModel(format!(
                    "Ambiguous case-folded tool identity; use exact casing: {}",
                    alternatives.join(", ")
                ))),
            }
        }
        Ok(key)
    }

    fn effective_limit(&self, query: &str, requested: Option<usize>) -> Result<usize, FunctionCallError> {
        let key = self.identity_query_key(query, requested.unwrap_or(TOOL_SEARCH_DEFAULT_LIMIT))?;
        if requested.is_some() { return Ok(key.limit); }
        let names = self.exact_name_index.get(&key.query).into_iter().flatten()
            .filter(|id| matches_source(id.info(&self.search_infos), key.source.as_deref()))
            .flat_map(|id| {
                let selected = id.name_index(&self.name_indexes).output_names_for(&key.query);
                loadable_tool_names(id.info(&self.search_infos).entry.output.as_ref()).into_iter()
                    .filter(move |name| selected.is_some_and(|selected| selected.contains(&name.name)))
            }).collect::<HashSet<_>>();
        Ok(if names.len() == 1 { 1 } else { key.limit })
    }

    fn search(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Arc<ToolSearchResult>, FunctionCallError> {
        let key = self.identity_query_key(query, limit)?;
        if key.source.is_some() && !self.search_infos.iter().any(|info| matches_source(info, key.source.as_deref())) {
            let scopes = self.search_infos.iter().map(crate::tools::handlers::tool_search_spec::canonical_source)
                .collect::<std::collections::BTreeSet<_>>();
            let mut bytes = 0;
            let alternatives = scopes.iter().take(16).filter_map(|scope| {
                bytes += scope.len() + 7;
                (bytes <= 1024).then(|| format!("source:{scope}"))
            }).collect::<Vec<_>>();
            return Err(FunctionCallError::RespondToModel(format!(
                "No callable source `{}` exists in the current inventory. Use canonical scope tokens, not display names. Available: {} ({} omitted). Shared namespaces may contain multiple connectors.",
                key.source.as_deref().unwrap_or_default(),
                alternatives.join(", "), scopes.len() - alternatives.len(),
            )));
        }
        let query_terms = ToolSearchTokenizer.tokenize(&key.query);
        let missing_names = query_terms.iter().filter(|term|
            !self.exact_name_index.contains_key(&key.query)
                && term.contains('_') && !self.exact_name_index.contains_key(*term))
            .cloned().collect::<Vec<_>>();
        // Bare identities (including explicit multi-name requests) remain strict.
        // Mixed capability queries may contain project/account/file identifiers.
        if !missing_names.is_empty()
            && (key.query.split_whitespace().count() == 1
                || query_terms.iter().all(|term| term.contains('_')))
        {
            return Err(FunctionCallError::RespondToModel(format!(
                "No deferred tool by these exact names exists in the current inventory: {}. Use plain-language capability terms to search for alternatives.",
                missing_names.iter().map(|name| name.as_str()).collect::<Vec<_>>().join(", "),
            )));
        }
        if self.search_infos.is_empty() {
            return Ok(Arc::new(ToolSearchResult {
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
        let exact_terms = if self.exact_name_index.contains_key(&key.query) {
            vec![&key.query]
        } else {
            query_terms.iter().collect()
        };
        let exact_matches = exact_terms.into_iter()
            .filter_map(|term| self.exact_name_index.get(term))
            .flatten().copied()
            .filter(|id| matches_source(id.info(&self.search_infos), key.source.as_deref()))
            .filter(|id| required.iter().all(|term|
                self.exact_name_index.get(term).is_some_and(|ids| ids.contains(id))
                || self.search_index.postings.get(term).is_some_and(|postings|
                    postings.iter().any(|(candidate, _)| candidate == id))))
            .filter(|id| seen_exact.insert(*id)).collect::<Vec<_>>();
        if exact_matches.len() == 1 && limit == 1 {
            let mut result =
                self.search_output_tools(exact_matches.iter().copied(), Some(&key.query), limit, key.source.is_some())?;
            if result.omitted_result_count == 0 && !result.tools.is_empty() {
                result.unmatched_identifiers = missing_names;
                let result = Arc::new(result);
                self.cache_search_result(key, &result);
                return Ok(result);
            }
        }
        let exact_match_count = exact_matches.len();
        let candidate_limit = tool_search_candidate_limit(limit, self.search_infos.len());
        let (candidates, weak_candidates) =
            self.search_index
                .top_matches(&key.query, candidate_limit, &self.search_infos, key.source.as_deref(), &exact_matches);
        let candidate_count = candidates.len();
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
        let selection = results.into_iter().chain(remaining).take(candidate_limit).chain(weak_candidates);
        let mut result = self.search_output_tools(selection, Some(&key.query), limit, key.source.is_some())?;
        result.unmatched_identifiers = missing_names;
        // Derive ambiguity before the caller's result limit can hide alternatives.
        // Only the complete query is an exact-name lookup; a multi-name request
        // must not be mislabeled as a collision.
        let names = self.exact_name_index.get(&key.query).into_iter().flatten()
            .filter(|id| matches_source(id.info(&self.search_infos), key.source.as_deref()))
            .flat_map(|id| {
                let selected = id.name_index(&self.name_indexes).output_names_for(&key.query);
                loadable_tool_names(id.info(&self.search_infos).entry.output.as_ref()).into_iter()
                    .filter(move |name| selected.is_some_and(|names| names.contains(&name.name)))
            }).collect::<std::collections::BTreeSet<_>>();
        if names.len() > 1 {
            let mut bytes = 0;
            let alternatives = names.iter().take(8).map(ToString::to_string).take_while(|name| {
                bytes += name.len();
                bytes <= 2048
            }).collect::<Vec<_>>();
            result.exact_name_ambiguity = Some(serde_json::json!({
                "match_count": names.len(),
                "omitted_alternative_count": names.len() - alternatives.len(),
                "qualified_alternatives": alternatives,
            }));
        }
        let result = Arc::new(result);
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
                "tool search completed"
            );
        }
        // The inventory and presentation budget are immutable for this handler.
        // Cache partial receipts as well as complete ones; activation is repeated
        // for each invocation, so a cache hit cannot suppress capability delivery.
        self.cache_search_result(key, &result);
        Ok(result)
    }

    fn search_output_tools(
        &self,
        results: impl IntoIterator<Item = ToolSearchDocumentId>,
        exact_query: Option<&str>,
        limit: usize,
        source_scoped: bool,
    ) -> Result<ToolSearchResult, FunctionCallError> {
        let mut retained = ToolSearchResultBuilder::new();
        let mut activation_tools = Vec::new();
        let mut supplemental_tools = Vec::new();
        let mut unactivated_matches = Vec::new();
        let mut unactivated_bytes = 0;
        let mut omitted_result_count = 0usize;
        let mut selected = HashSet::new();
        for result_id in results {
            let result = &result_id.info(&self.search_infos).entry;
            let relevant = exact_query.is_none_or(|query| {
                std::iter::once(query).chain(ToolSearchTokenizer.tokenize(query).iter().map(String::as_str)).any(|term|
                    self.exact_name_index.get(term).is_some_and(|ids| ids.contains(&result_id)))
                    || self.search_index.relevance(query, result_id, source_scoped) >= MIN_TOOL_ACTIVATION_RELEVANCE
            });
            if !relevant {
                for name in loadable_tool_names(result.output.as_ref()) {
                    let name = name.to_string();
                    let bytes = serde_json::to_vec(&name)
                        .map_err(|error| FunctionCallError::Fatal(error.to_string()))?.len() + 1;
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
                if let Some(names) = result_id.name_index(&self.name_indexes).output_names_for(query) {
                    return Some(names.clone());
                }
                let tokens = ToolSearchTokenizer.tokenize(query);
                let names = std::iter::once(query).chain(tokens.iter().map(String::as_str)).filter_map(|term|
                    result_id.name_index(&self.name_indexes).output_names_for(term))
                    .flatten().cloned().collect::<HashSet<_>>();
                (!names.is_empty()).then_some(names)
            });
            // Select callable identities before charging the receipt budget.
            // A namespace is a container, not one callable.
            let candidates: Box<dyn Iterator<Item = LoadableToolSpec> + '_> = match result.output.as_ref() {
                LoadableToolSpec::Function(_) => {
                    Box::new(std::iter::once_with(|| result.output.as_ref().clone()))
                }
                LoadableToolSpec::Namespace(namespace) => {
                    let order = if let Some(query) = exact_query.filter(|_| namespace.tools.len() > 1 && exact_output_names.is_none()) {
                        // Rank callable descriptions before applying the callable
                        // limit. Share the authoritative output Arc; do not clone
                        // every schema merely to build ranking documents.
                        let members = namespace.tools.iter().map(|member| {
                            let ResponsesApiNamespaceTool::Function(tool) = member;
                            let mut info = result_id.info(&self.search_infos).clone();
                            info.entry.search_text = codex_tools::namespace_member_search_text(
                                &namespace.name, &namespace.description, tool);
                            info
                        }).collect::<Vec<_>>();
                        let mut index = ToolSearchIndex::new(&members);
                        // Each ranking document represents one callable, not
                        // the namespace Arc shared by these lightweight entries.
                        index.callable_terms = namespace.tools.iter().map(|member| {
                            let ResponsesApiNamespaceTool::Function(tool) = member;
                            ToolSearchTokenizer.tokenize(&codex_tools::namespace_member_search_text("", "", tool))
                                .into_iter().collect()
                        }).collect();
                        index.top_matches(query, members.len(), &members,
                            source_scoped.then_some(namespace.name.as_str()), &[]).0
                            .into_iter().map(|id| id.0).collect::<Vec<_>>()
                    } else {
                        (0..namespace.tools.len()).collect()
                    };
                    Box::new(order.into_iter().map(|index| &namespace.tools[index])
                        .filter(move |tool| {
                            let ResponsesApiNamespaceTool::Function(tool) = tool;
                            exact_output_names.as_ref().is_none_or(|names| names.contains(&tool.name))
                        })
                        .map(|tool| {
                            LoadableToolSpec::Namespace(ResponsesApiNamespace {
                                name: namespace.name.clone(),
                                description: namespace.description.clone(),
                                tools: vec![tool.clone()],
                            })
                        }))
                },
            };
            for candidate in candidates {
                if selected.len() == limit {
                    break;
                }
                let names = loadable_tool_names(&candidate);
                if names.iter().all(|name| selected.contains(name)) {
                    continue;
                }
                selected.extend(names.iter().cloned());
                activation_tools.extend(names.iter().cloned());
                let candidate = result.normalize_output(candidate);
                if !retained.try_push(&candidate) {
                    supplemental_tools.extend(names);
                    let local_names = match &candidate {
                        LoadableToolSpec::Function(tool) => HashSet::from([tool.name.clone()]),
                        LoadableToolSpec::Namespace(namespace) => namespace
                            .tools
                            .iter()
                            .map(|tool| {
                                let ResponsesApiNamespaceTool::Function(tool) = tool;
                                tool.name.clone()
                            })
                            .collect(),
                    };
                    if !compact_exact_match_recovery(&candidate, &local_names)
                        .is_some_and(|recovery| retained.try_push(&recovery))
                    {
                        // The incomplete receipt reports omitted definitions;
                        // the next request advertises the authoritative contract.
                        omitted_result_count += 1;
                    }
                }
            }
            if selected.len() == limit {
                break;
            }
        }
        let (tools, encoded_tools_len) = retained.finish();
        activation_tools.extend(tools.iter().flat_map(loadable_tool_names));
        activation_tools.sort_unstable();
        activation_tools.dedup();
        let serialized_tools = serialize_loadable_tools(&tools);
        Ok(ToolSearchResult {
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

    fn result_cache(&self) -> std::sync::MutexGuard<'_, VecDeque<ToolSearchCacheEntry>> {
        match self.result_cache.lock() {
            Ok(cache) => cache,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    #[cfg(test)]
    fn result_cache_len(&self) -> usize {
        self.result_cache().len()
    }
}

fn compact_exact_match_recovery(
    output: &LoadableToolSpec,
    exact_names: &HashSet<String>,
) -> Option<LoadableToolSpec> {
    match output {
        LoadableToolSpec::Function(tool) if exact_names.contains(&tool.name) => Some(
            LoadableToolSpec::Function(compact_recovery_tool(tool, &tool.name)),
        ),
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
    query
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
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
        if let Some(identity) = term.strip_prefix("source:") {
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
            "query must not be empty".to_string(),
        ));
    }

    Ok(ToolSearchQueryKey { query, limit, source })
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
    let mut schema_fits =
        |tool: &ResponsesApiTool| serde_json::to_writer(&mut writer, &tool.output_schema.as_ref().map(codex_tools::ToolOutputSchema::to_value)).is_ok();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::step_context::StepContext;
    use crate::session::tests::make_session_and_context_with_rx;
    use crate::tools::context::ToolCallSource;
    use crate::tools::handlers::DynamicToolHandler;
    use crate::tools::handlers::McpHandler;
    use crate::turn_diff_tracker::TurnDiffTracker;
    use codex_mcp::ToolInfo;
    use codex_protocol::dynamic_tools::DynamicToolFunctionSpec;
    use codex_protocol::dynamic_tools::DynamicToolNamespaceSpec;

    fn executor_search_info<T>(handler: T) -> ToolSearchInfo
    where
        T: ToolExecutor<ToolInvocation>,
    {
        handler
            .search_info()
            .expect("handler should return search info")
    }
    use codex_tools::ResponsesApiNamespace;
    use codex_tools::ResponsesApiNamespaceTool;
    use codex_tools::ResponsesApiTool;
    use codex_tools::ToolSearchEntry;
    use codex_tools::ToolSearchSourceInfo;
    use pretty_assertions::assert_eq;
    use rmcp::model::Tool;
    use std::sync::Arc;
    use std::sync::Barrier;
    use std::thread;
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn native_and_hybrid_search_advice_expires_after_success_and_new_turn() {
        use codex_protocol::models::{ContentItem, ResponseItem};
        use codex_protocol::ResponseItemId;
        use codex_protocol::openai_models::{InputModality, ToolMode};
        let is_advice = |item: &ResponseItem| matches!(item,
            ResponseItem::Message { id: Some(id), role, .. }
                if role == "developer" && id.as_str().starts_with("msg_turn_advice_"));
        for mode in [ToolMode::Direct, ToolMode::CodeMode] {
            let handler = ToolSearchHandler::new(vec![
                search_info("calendar lookup", Some("calendar"), "calendar", "find_event"),
                search_info("events lookup", Some("events"), "events", "find_event"),
            ]);
            let (session, mut turn, _events) = make_session_and_context_with_rx().await;
            Arc::get_mut(&mut turn).unwrap().model_info.tool_mode = Some(mode);
            let mut history = crate::context_manager::ContextManager::new();
            for (id, query) in [("ambiguous", "find_event"), ("qualified", "mcp__calendar__find_event")] {
                let payload = ToolPayload::ToolSearch {
                    arguments: codex_protocol::models::SearchToolCallParams {
                        query: query.into(), limit: Some(1),
                    },
                };
                handler.handle(ToolInvocation {
                    session: Arc::clone(&session),
                    step_context: StepContext::for_test(Arc::clone(&turn)),
                    cancellation_token: CancellationToken::new(),
                    tracker: Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
                    call_id: id.into(), tool_name: ToolName::plain(TOOL_SEARCH_TOOL_NAME),
                    source: ToolCallSource::Direct, payload,
                }).await.unwrap();
                let advice = turn.take_post_tool_contexts(id).await;
                if id == "ambiguous" {
                    assert!(!advice.is_empty());
                    assert!(advice.iter().all(&is_advice));
                    assert!(advice.iter().all(|item| item.turn_id() == Some(turn.sub_id.as_str())));
                } else {
                    assert!(advice.is_empty());
                }
                history.record_items(advice.iter(), codex_utils_output_truncation::TruncationPolicy::Tokens(10_000));
            }
            let mut next = ResponseItem::Message {
                id: Some(ResponseItemId::new("msg")), role: "user".into(),
                content: vec![ContentItem::InputText {text:"Unrelated task".into()}],
                phase: None, internal_chat_message_metadata_passthrough: None,
            };
            next.set_turn_id_if_missing("next-task");
            history.record_items([&next], codex_utils_output_truncation::TruncationPolicy::Tokens(10_000));
            let sampled = history.clone().prepare_for_sampling_prompt_with_completed_tool_projection(
                &[InputModality::Text], crate::stable_context::StableContextTarget::Sampling,
                None, &crate::git_workspace::GitWorkspaceCache::new(),
            );
            assert!(!sampled.items().iter().any(&is_advice));
            assert!(history.raw_items().iter().any(&is_advice));
        }
    }


    #[tokio::test]
    async fn exact_function_search_returns_and_activates_only_the_requested_function() {
        let mut info = search_info("calendar", None, "calendar", "create_event");
        let LoadableToolSpec::Namespace(namespace) = Arc::make_mut(&mut info.entry.output) else {
            panic!("expected namespace");
        };
        let ResponsesApiNamespaceTool::Function(create) = &namespace.tools[0];
        let mut delete = create.clone();
        delete.name = "delete_event".to_string();
        namespace
            .tools
            .push(ResponsesApiNamespaceTool::Function(delete));
        info.entry.tool_names.push("delete_event".to_string());
        let full_namespace = serde_json::to_value(&info.entry.output).expect("namespace");
        let mut exact_namespace = full_namespace.clone();
        exact_namespace["tools"]
            .as_array_mut()
            .expect("functions")
            .truncate(1);
        let handler = ToolSearchHandler::new(vec![info]);

        for (query, expected, names) in [
            (
                "create_event",
                exact_namespace.clone(),
                vec!["create_event"],
            ),
            (
                "mcp__calendar__create_event",
                exact_namespace,
                vec!["create_event"],
            ),
            (
                "calendar",
                full_namespace,
                vec!["create_event", "delete_event"],
            ),
        ] {
            let (session, mut turn, _events) = make_session_and_context_with_rx().await;
            Arc::get_mut(&mut turn)
                .expect("unique fixture turn")
                .model_info
                .tool_mode = Some(codex_protocol::openai_models::ToolMode::CodeModeOnly);
            turn.refresh_deferred_tool_capabilities(Arc::new(
                ["create_event", "delete_event"]
                    .into_iter()
                    .map(|name| {
                        (
                            ToolName::namespaced("mcp__calendar", name),
                            "calendar-v1".to_string(),
                        )
                    })
                    .collect(),
            ));
            let payload = ToolPayload::ToolSearch {
                arguments: codex_protocol::models::SearchToolCallParams {
                    query: query.to_string(),
                    limit: Some(2),
                },
            };
            let output = handler
                .handle(ToolInvocation {
                    session: Arc::clone(&session),
                    step_context: StepContext::for_test(Arc::clone(&turn)),
                    cancellation_token: CancellationToken::new(),
                    tracker: Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
                    call_id: "search".to_string(),
                    tool_name: ToolName::plain(TOOL_SEARCH_TOOL_NAME),
                    source: ToolCallSource::Direct,
                    payload: payload.clone(),
                })
                .await
                .expect("search invocation");

            assert!(turn.activated_deferred_tools().is_empty());
            session.record_tool_completion_ordered(&turn, "search", &[output.to_response_item("search", &payload).into()]).await.unwrap();            let codex_protocol::models::ResponseInputItem::ToolSearchOutput { tools, .. } =
                output.to_response_item("search", &payload)
            else {
                panic!("expected search response");
            };
            assert_eq!(tools, vec![expected], "query: {query}");
            let history = session.clone_history().await;
            let schema_text = history.raw_items().iter().find_map(|item| match item {
                codex_protocol::models::ResponseItem::Message { role, content, .. }
                    if role == "developer" =>
                {
                    content.iter().find_map(|content| match content {
                        codex_protocol::models::ContentItem::InputText { text } => {
                            text.strip_prefix("Activated tool schemas (callable through exec):\n")
                        }
                        _ => None,
                    })
                }
                _ => None,
            });
            assert!(
                schema_text.is_none(),
                "complete schemas must not be republished"
            );
            assert_eq!(
                turn.activated_deferred_tools(),
                names
                    .into_iter()
                    .map(|name| ToolName::namespaced("mcp__calendar", name))
                    .collect(),
                "query: {query}"
            );
        }
    }

    #[tokio::test]
    async fn code_mode_only_keeps_omitted_or_compacted_definitions_lazy() {
        for compacted in [false, true] {
            let normal = search_info("publication", None, "normal", "small");
            let normal_definition = serde_json::to_value(&normal.entry.output).unwrap();
            let mut oversized = search_info("publication", None, "large", "oversized");
            let LoadableToolSpec::Namespace(namespace) = Arc::make_mut(&mut oversized.entry.output) else {
                panic!("expected namespace");
            };
            if compacted {
                namespace.description = "x".repeat(MAX_TOOL_SEARCH_RESULT_BYTES);
            } else {
                let ResponsesApiNamespaceTool::Function(tool) = &mut namespace.tools[0];
                tool.description = "x".repeat(MAX_TOOL_SEARCH_RESULT_BYTES);
            }
            let handler = ToolSearchHandler::new(vec![normal, oversized]);
            let (session, mut turn, _events) = make_session_and_context_with_rx().await;
            Arc::get_mut(&mut turn).unwrap().model_info.tool_mode =
                Some(codex_protocol::openai_models::ToolMode::CodeModeOnly);
            let names = [
                ToolName::namespaced("mcp__normal", "small"),
                ToolName::namespaced("mcp__large", "oversized"),
            ];
            turn.refresh_deferred_tool_capabilities(Arc::new(
                names
                    .iter()
                    .cloned()
                    .map(|name| (name, "v1".into()))
                    .collect(),
            ));
            let payload = ToolPayload::ToolSearch {
                arguments: codex_protocol::models::SearchToolCallParams {
                    query: "mcp__normal__small mcp__large__oversized".into(),
                    limit: Some(2),
                },
            };
            // Prior activation and repeated searches must not cause schema
            // publication outside the bounded result.
            turn.activate_deferred_tools(names.iter().cloned());
            for _ in 0..2 {
                let output = handler
                    .handle(ToolInvocation {
                        session: Arc::clone(&session),
                        step_context: StepContext::for_test(Arc::clone(&turn)),
                        cancellation_token: CancellationToken::new(),
                        tracker: Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
                        call_id: "mixed-search".into(),
                        tool_name: ToolName::plain(TOOL_SEARCH_TOOL_NAME),
                        source: ToolCallSource::Direct,
                        payload: payload.clone(),
                    })
                    .await
                    .unwrap();
                let nested = output.code_mode_result(&payload);
                assert_eq!(nested["activated_omitted_tools"], serde_json::json!(["mcp__large.oversized"]));
                let codex_protocol::models::ResponseInputItem::ToolSearchOutput {
                    tools,
                    omitted_result_count,
                    ..
                } = output.to_response_item("mixed-search", &payload)
                else {
                    panic!("expected search output");
                };
                assert!(tools.contains(&normal_definition));
                assert_eq!(tools.len(), if compacted { 2 } else { 1 });
                assert_eq!(
                    omitted_result_count.unwrap_or_default(),
                    usize::from(!compacted)
                );
                assert_eq!(
                    turn.activated_deferred_tools(),
                    names.iter().cloned().collect()
                );
                let history = session.clone_history().await;
                let publications = history
                    .raw_items()
                    .iter()
                    .filter_map(|item| {
                        let codex_protocol::models::ResponseItem::Message { role, content, .. } =
                            item
                        else {
                            return None;
                        };
                        if role != "developer" {
                            return None;
                        }
                        content.iter().find_map(|content| match content {
                            codex_protocol::models::ContentItem::InputText { text } => text
                                .strip_prefix("Activated tool schemas (callable through exec):\n"),
                            _ => None,
                        })
                    })
                    .collect::<Vec<_>>();
                assert_eq!(
                    publications.len(),
                    0,
                    "schemas must remain in the callable catalog, not unbounded history"
                );
            }
        }
    }

    #[tokio::test]
    async fn lazy_schema_activation_does_not_require_supplemental_history_persistence() {
        let mut info = search_info("fault", None, "fault", "oversized");
        let LoadableToolSpec::Namespace(namespace) = Arc::make_mut(&mut info.entry.output) else {
            panic!("expected namespace");
        };
        let ResponsesApiNamespaceTool::Function(tool) = &mut namespace.tools[0];
        tool.description = "x".repeat(MAX_TOOL_SEARCH_RESULT_BYTES);
        let handler = ToolSearchHandler::new(vec![info]);
        let (session, mut turn, _events) = make_session_and_context_with_rx().await;
        Arc::get_mut(&mut turn).unwrap().model_info.tool_mode =
            Some(codex_protocol::openai_models::ToolMode::CodeModeOnly);
        turn.refresh_deferred_tool_capabilities(Arc::new(
            [(ToolName::namespaced("mcp__fault", "oversized"), "v1".into())].into_iter().collect(),
        ));
        session.close_durable_history_commit_gate_for_test();
        let result = handler.handle(ToolInvocation {
            session,
            step_context: StepContext::for_test(Arc::clone(&turn)),
            cancellation_token: CancellationToken::new(),
            tracker: Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
            call_id: "fault-search".into(),
            tool_name: ToolName::plain(TOOL_SEARCH_TOOL_NAME),
            source: ToolCallSource::CodeMode { cell_id: "test-cell".into(), parent_call_id: None, runtime_tool_call_id: "nested-search".into(), nested_deadline: None, cancellation_cause: None },
            payload: ToolPayload::ToolSearch {
                arguments: codex_protocol::models::SearchToolCallParams {
                    query: "mcp__fault__oversized".into(), limit: Some(1),
                },
            },
        }).await;
        assert!(result.is_ok());
        assert!(turn.activated_deferred_tools().contains(&ToolName::namespaced("mcp__fault", "oversized")));
    }

    #[test]
    fn tool_search_tokenizer_splits_unicode_words_and_normalizes_case() {
        assert_eq!(
            ToolSearchTokenizer.tokenize("Launch CALENDAR-events for José"),
            vec!["launch", "calendar", "events", "for", "josé"]
        );
    }

    #[tokio::test]
    async fn verified10_catalog_growth_preserves_executed_alias_target() {
        #[derive(Default)]
        struct Delegate(std::sync::Mutex<Vec<ToolName>>);
        impl codex_code_mode::CodeModeSessionDelegate for Delegate {
            fn invoke_tool<'a>(&'a self, call: codex_code_mode::CodeModeNestedToolCall,
                _cancel: codex_code_mode::NestedCancellation) -> codex_code_mode::ToolInvocationFuture<'a>
            {
                Box::pin(async move {
                    self.0.lock().unwrap().push(call.tool_name.clone());
                    Ok(serde_json::json!({"tool":call.tool_name.name}))
                })
            }
            fn notify<'a>(&'a self, _call: String, _cell: codex_code_mode::CellId, _text: String,
                _cancel: CancellationToken) -> codex_code_mode::NotificationFuture<'a>
            { Box::pin(async { Ok(()) }) }
            fn cell_closed(&self, _cell: &codex_code_mode::CellId) {}
        }
        let delegate = Arc::new(Delegate::default());
        let runtime = codex_code_mode::InProcessCodeModeSession::with_delegate(delegate.clone());
        let specs = ["read_file", "read-file"].map(|name| ToolSpec::Function(ResponsesApiTool {
            name: name.into(), description: name.into(), strict: false, defer_loading: None,
            parameters: codex_tools::JsonSchema::default(), output_schema: None,
        }));
        for count in [1, 2] {
            runtime.execute(codex_code_mode::ExecuteRequest {
                state_path: None, tool_call_id: format!("catalog-{count}"),
                enabled_tools: codex_tools::collect_code_mode_tool_definitions(&specs[..count]).into(),
                source: "text(await tools.read_file({}));".into(), yield_time_ms: None,
                max_output_tokens: Some(1000), default_tool_timeout_ms: None,
            }).await.unwrap().initial_response().await.unwrap();
        }
        assert_eq!(*delegate.0.lock().unwrap(), vec![ToolName::plain("read_file"), ToolName::plain("read_file")]);
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn oversized_search_resolves_and_invokes_in_one_cell_without_schema_history() {
        struct Delegate {
            handler: ToolSearchHandler,
            invocation: ToolInvocation,
            calls: std::sync::atomic::AtomicUsize,
        }
        impl codex_code_mode::CodeModeSessionDelegate for Delegate {
            fn invoke_tool<'a>(&'a self, call: codex_code_mode::CodeModeNestedToolCall,
                _cancel: codex_code_mode::NestedCancellation) -> codex_code_mode::ToolInvocationFuture<'a>
            {
                Box::pin(async move {
                    self.calls.fetch_add(1, Ordering::Relaxed);
                    if call.tool_name == ToolName::plain(TOOL_SEARCH_TOOL_NAME) {
                        let mut invocation = self.invocation.clone();
                        invocation.payload = ToolPayload::ToolSearch {
                            arguments: serde_json::from_value(call.input.unwrap()).unwrap(),
                        };
                        let payload = invocation.payload.clone();
                        let result = self.handler.handle(invocation).await.map_err(|error| error.to_string())?;
                        Ok(result.code_mode_result(&payload))
                    } else {
                        assert_eq!(call.tool_name, ToolName::namespaced("mcp__large", "oversized"));
                        assert!(self.invocation.step_context.turn.activated_deferred_tools().contains(&call.tool_name));
                        assert_eq!(call.input, Some(serde_json::json!({})));
                        Ok(serde_json::json!({"invoked": true}))
                    }
                })
            }
            fn notify<'a>(&'a self, _call: String, _cell: codex_code_mode::CellId, _text: String,
                _cancel: CancellationToken) -> codex_code_mode::NotificationFuture<'a>
            { Box::pin(async { Ok(()) }) }
            fn cell_closed(&self, _cell: &codex_code_mode::CellId) {}
        }
        let mut info = search_info("publication", None, "large", "oversized");
        let LoadableToolSpec::Namespace(namespace) = Arc::make_mut(&mut info.entry.output) else { panic!() };
        let ResponsesApiNamespaceTool::Function(tool) = &mut namespace.tools[0];
        tool.description = format!("{} authoritative-contract-tail", "x".repeat(MAX_TOOL_SEARCH_RESULT_BYTES));
        let spec = ToolSpec::Namespace(namespace.clone());
        let handler = ToolSearchHandler::new(vec![info]);
        let specs = [handler.spec(), spec];
        let enabled_tools = codex_tools::collect_code_mode_tool_definitions(specs.iter()).into();
        let (session, mut turn, _events) = make_session_and_context_with_rx().await;
        Arc::get_mut(&mut turn).unwrap().model_info.tool_mode = Some(codex_protocol::openai_models::ToolMode::CodeModeOnly);
        turn.refresh_deferred_tool_capabilities(Arc::new(
            [(ToolName::namespaced("mcp__large", "oversized"), "v1".into())].into_iter().collect()));
        let delegate = Arc::new(Delegate {
            handler, calls: std::sync::atomic::AtomicUsize::new(0),
            invocation: ToolInvocation {
                session: Arc::clone(&session), step_context: StepContext::for_test(turn),
                cancellation_token: CancellationToken::new(),
                tracker: Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
                call_id: "oversized-search".into(), tool_name: ToolName::plain(TOOL_SEARCH_TOOL_NAME),
                source: ToolCallSource::CodeMode { cell_id: "test-cell".into(), parent_call_id: None, runtime_tool_call_id: "nested-search".into(), nested_deadline: None, cancellation_cause: None }, payload: ToolPayload::Function { arguments: "{}".into() },
            },
        });
        let runtime = codex_code_mode::InProcessCodeModeSession::with_delegate(delegate.clone());
        let started = std::time::Instant::now();
        let result = runtime.execute(codex_code_mode::ExecuteRequest {
            state_path: None, tool_call_id: "one-cell".into(), enabled_tools,
            source: r#"
const search = await tools.tool_search({query:'mcp__large__oversized',limit:1});
if (!search.activated_omitted_tools.includes('mcp__large.oversized')) throw Error('missing lazy receipt');
const tool = resolve_tool('mcp__large.oversized');
if (!tool.description.includes('authoritative-contract-tail')) throw Error('incomplete callable contract');
const result = await tool({});
if (!result.invoked) throw Error('invocation failed');
text('one-cell-complete');
"#.into(), yield_time_ms: None, max_output_tokens: Some(1000), default_tool_timeout_ms: None,
        }).await.unwrap().initial_response().await.unwrap();
        eprintln!("oversized search/resolve/invoke wall time: {:?}; nested calls: 2", started.elapsed());
        let codex_code_mode::RuntimeResponse::Result { error_text, content_items, .. } = result else { panic!("cell did not finish") };
        assert_eq!(error_text, None);
        assert!(format!("{content_items:?}").contains("one-cell-complete"));
        assert_eq!(delegate.calls.load(Ordering::Relaxed), 2, "resolution must not add a dispatch or model round trip");
        assert!(!format!("{:?}", session.clone_history().await.raw_items()).contains("authoritative-contract-tail"));
    }

    #[test]
    fn snake_case_query_reports_missing_names_without_activating_similar_tools() {
        assert_eq!(
            ToolSearchTokenizer.tokenize("get_file_contents"),
            vec!["get_file_contents"]
        );
        let handler = ToolSearchHandler::new(vec![
            search_info(
                "github get_profile get profile",
                Some("GitHub"),
                "github",
                "get_profile",
            ),
            search_info(
                "github fetch_file fetch file get file contents from a repository",
                Some("GitHub"),
                "github",
                "fetch_file",
            ),
        ]);

        let missing = handler.search("github get_file_contents", 1).unwrap();
        assert_eq!(missing.unmatched_identifiers, ["get_file_contents"]);
        assert!(missing.activation_tools.is_empty());
        let result = handler.search("+github file contents", 1).expect("semantic search");

        assert_eq!(
            result.activation_tools,
            vec![ToolName::namespaced("mcp__github", "fetch_file")]
        );
        // An exact name is not split, so shared words do not pad its results.
        let exact = handler
            .search("get_profile", TOOL_SEARCH_DEFAULT_LIMIT)
            .expect("exact search");
        assert_eq!(
            exact.activation_tools,
            vec![ToolName::namespaced("mcp__github", "get_profile")]
        );
        let named = handler.search("please get_profile", 1).unwrap();
        assert_eq!(named.activation_tools, exact.activation_tools);
        assert!(handler.search("+gmail file contents", 8).unwrap().activation_tools.is_empty());
        assert!(ToolSearchHandler::new(Vec::new()).search("send_message_to_thread", 8)
            .unwrap_err().to_string().contains("send_message_to_thread"));
    }

    #[tokio::test]
    async fn oversized_namespace_member_keeps_rank_and_activates_with_partial_receipt() {
        let mut info = search_info("scheduling mcp__calendar", None, "calendar", "oversized");
        let LoadableToolSpec::Namespace(namespace) = Arc::make_mut(&mut info.entry.output) else {
            panic!("expected namespace");
        };
        let ResponsesApiNamespaceTool::Function(oversized) = &mut namespace.tools[0];
        let mut available = oversized.clone();
        available.name = "available".to_string();
        available.description = "available tool".to_string();
        oversized.description = "x".repeat(MAX_TOOL_SEARCH_RESULT_BYTES);
        namespace
            .tools
            .push(ResponsesApiNamespaceTool::Function(available));
        info.entry.tool_names.push("available".to_string());
        let handler = ToolSearchHandler::new(vec![info]);

        for query in ["oversized", "mcp__calendar__oversized", "mcp__calendar__oversized", "oversized"] {
            let (session, turn, _events) = make_session_and_context_with_rx().await;
            turn.refresh_deferred_tool_capabilities(Arc::new(
                ["oversized", "available"]
                    .into_iter()
                    .map(|name| {
                        (
                            ToolName::namespaced("mcp__calendar", name),
                            "v1".to_string(),
                        )
                    })
                    .collect(),
            ));
            let payload = ToolPayload::ToolSearch {
                arguments: codex_protocol::models::SearchToolCallParams {
                    query: query.to_string(),
                    limit: Some(1),
                },
            };
            let output = handler
                .handle(ToolInvocation {
                    session: Arc::clone(&session),
                    step_context: StepContext::for_test(Arc::clone(&turn)),
                    cancellation_token: CancellationToken::new(),
                    tracker: Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
                    call_id: "search".to_string(),
                    tool_name: ToolName::plain(TOOL_SEARCH_TOOL_NAME),
                    source: ToolCallSource::Direct,
                    payload: payload.clone(),
                })
                .await
                .expect("search invocation");

            assert!(turn.activated_deferred_tools().is_empty());
            session.record_tool_completion_ordered(&turn, "search", &[output.to_response_item("search", &payload).into()]).await.unwrap();            let codex_protocol::models::ResponseInputItem::ToolSearchOutput {
                tools,
                status,
                omitted_result_count,
                ..
            } = output.to_response_item("search", &payload)
            else {
                panic!("expected search response");
            };
            assert!(tools.is_empty(), "query: {query}");
            assert_eq!(status, "incomplete");
            assert_eq!(omitted_result_count, Some(1));
            let result = output.code_mode_result(&payload);
            assert_eq!(result["activated_omitted_tools"], serde_json::json!(["mcp__calendar.oversized"]));
            assert_eq!(result["status"], "incomplete");
            assert_eq!(result["tools"], serde_json::json!([]));
            assert!(
                serde_json::to_vec(&tools).expect("serialized tools").len()
                    <= MAX_TOOL_SEARCH_RESULT_BYTES
            );
            assert_eq!(
                turn.activated_deferred_tools(),
                [ToolName::namespaced("mcp__calendar", "oversized")]
                    .into_iter()
                    .collect(),
            );
        }
    }

    #[tokio::test]
    async fn unicode_exact_search_activates_compact_schema_without_losing_its_contract() {
        let name = "Créer_Événement";
        let mut info = search_info("unrelated ranking terms", None, "calendar", name);
        let LoadableToolSpec::Namespace(namespace) = Arc::make_mut(&mut info.entry.output) else {
            panic!("expected namespace");
        };
        let ResponsesApiNamespaceTool::Function(tool) = &mut namespace.tools[0];
        tool.description = "Create a calendar event with an attendee description.".to_string();
        let description = tool.description.clone();
        let expected_schema = serde_json::json!({
            "type": "object",
            "properties": {
                "description": {"type": "string", "minLength": 1},
                "status": {"const": {"description": "literal data must survive"}},
                "attendees": {"type": "array", "items": {"$ref": "#/$defs/attendee"}}
            },
            "$defs": {"attendee": {"type": "string", "pattern": "@"}},
            "required": ["description", "status", "attendees"],
            "additionalProperties": false
        });
        let mut verbose_schema = expected_schema.clone();
        verbose_schema["description"] =
            serde_json::json!("annotation ".repeat(MAX_TOOL_SEARCH_RESULT_BYTES));
        verbose_schema["properties"]["description"]["description"] =
            serde_json::json!("attendee description");
        verbose_schema["properties"]["attendees"]["items"]["description"] =
            serde_json::json!("item annotation");
        verbose_schema["$defs"]["attendee"]["description"] =
            serde_json::json!("definition annotation");
        tool.output_schema = Some(verbose_schema.into());
        assert_eq!(
            compact_recovery_tool(tool, &tool.name).output_schema,
            Some(expected_schema.into())
        );
        let handler = ToolSearchHandler::new(vec![info]);

        for query in ["Créer_Événement", "CRÉER_ÉVÉNEMENT"] {
            let (session, turn, _events) = make_session_and_context_with_rx().await;
            let tool_name = ToolName::namespaced("mcp__calendar", name);
            turn.refresh_deferred_tool_capabilities(Arc::new(
                [(tool_name.clone(), "calendar-v1".to_string())]
                    .into_iter()
                    .collect(),
            ));
            let payload = ToolPayload::ToolSearch {
                arguments: codex_protocol::models::SearchToolCallParams {
                    query: query.to_string(),
                    limit: Some(1),
                },
            };
            let output = handler
                .handle(ToolInvocation {
                    session: Arc::clone(&session),
                    step_context: StepContext::for_test(Arc::clone(&turn)),
                    cancellation_token: CancellationToken::new(),
                    tracker: Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
                    call_id: "unicode-search".to_string(),
                    tool_name: ToolName::plain(TOOL_SEARCH_TOOL_NAME),
                    source: ToolCallSource::Direct,
                    payload: payload.clone(),
                })
                .await
                .expect("search invocation");

            assert!(turn.activated_deferred_tools().is_empty());
            session.record_tool_completion_ordered(&turn, "unicode-search", &[output.to_response_item("unicode-search", &payload).into()]).await.unwrap();            let codex_protocol::models::ResponseInputItem::ToolSearchOutput { tools, .. } =
                output.to_response_item("unicode-search", &payload)
            else {
                panic!("expected search output");
            };
            assert_eq!(tools.len(), 1, "{query}");
            assert_eq!(tools[0]["tools"][0]["name"], name);
            assert!(
                tools[0]["tools"][0]["description"]
                    .as_str()
                    .unwrap()
                    .starts_with(&description)
            );
            assert!(tools[0]["tools"][0].get("output_schema").is_none());
            assert!(serde_json::to_vec(&tools).unwrap().len() <= MAX_TOOL_SEARCH_RESULT_BYTES);
            assert_eq!(
                turn.activated_deferred_tools(),
                [tool_name].into_iter().collect()
            );
        }
    }

    #[tokio::test]
    async fn cancelled_router_build_does_not_block_runtime_or_publish_capabilities() {
        let (session, mut turn) = crate::session::tests::make_session_and_context().await;
        let session = Arc::new(session);
        turn.model_info.supports_search_tool = true;
        turn.dynamic_tools = vec![codex_protocol::dynamic_tools::DynamicToolSpec::Function(
            DynamicToolFunctionSpec {
                name: "deferred_probe".to_string(),
                description: "Probe router cancellation".to_string(),
                input_schema: serde_json::json!({"type": "object", "properties": {}}),
                defer_loading: true,
            },
        )];
        let turn = Arc::new(turn);
        let step = session
            .capture_step_context(Arc::clone(&turn))
            .await
            .unwrap();
        let cache = Arc::clone(&session.services.tool_search_handler_cache);
        let (locked_tx, locked_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let blocker = thread::spawn(move || {
            let _guard = cache.state.lock().unwrap();
            locked_tx.send(()).unwrap();
            let _ = release_rx.recv_timeout(std::time::Duration::from_secs(5));
        });
        locked_rx.await.unwrap();

        let cancellation = CancellationToken::new();
        let build = crate::session::turn::built_tools(&session, &step, &[], &cancellation);
        tokio::pin!(build);
        let waiting = tokio::time::timeout(std::time::Duration::from_millis(100), &mut build)
            .await
            .is_err();
        cancellation.cancel();
        let result = if waiting {
            Some(tokio::time::timeout(std::time::Duration::from_secs(1), build).await)
        } else {
            None
        };
        let _ = release_tx.send(());
        blocker.join().unwrap();

        assert!(
            waiting,
            "router construction must yield while the cache is held"
        );
        assert!(matches!(
            result,
            Some(Ok(Err(codex_protocol::error::CodexErr::TurnAborted)))
        ));
        let tool = ToolName::plain("deferred_probe");
        turn.activate_deferred_tools([tool.clone()]);
        assert!(turn.activated_deferred_tools().is_empty());

        // A later, uncancelled build must still publish the same tool normally.
        let router =
            crate::session::turn::built_tools(&session, &step, &[], &CancellationToken::new())
                .await
                .expect("router builds after cancellation");
        assert!(
            router
                .deferred_tool_capability_revisions()
                .contains_key(&tool)
        );
        turn.activate_deferred_tools([tool.clone()]);
        assert_eq!(
            turn.activated_deferred_tools(),
            [tool].into_iter().collect()
        );
    }

    #[tokio::test]
    async fn cancelled_search_does_not_wait_for_the_index_or_activate_tools() {
        let handler = ToolSearchHandler::new(vec![search_info(
            "calendar",
            None,
            "calendar",
            "create_event",
        )]);
        let (session, turn, _events) = make_session_and_context_with_rx().await;
        turn.refresh_deferred_tool_capabilities(Arc::new(HashMap::from([(
            ToolName::namespaced("mcp__calendar", "create_event"),
            "calendar-v1".to_string(),
        )])));

        let cache = Arc::clone(&handler.result_cache);
        let (locked_tx, locked_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let blocker = thread::spawn(move || {
            let _guard = cache.lock().unwrap();
            locked_tx.send(()).unwrap();
            // Bound a regression that blocks the current-thread runtime itself.
            let _ = release_rx.recv_timeout(std::time::Duration::from_secs(5));
        });
        locked_rx.await.unwrap();

        let cancellation = CancellationToken::new();
        let invocation = ToolInvocation {
            session,
            step_context: StepContext::for_test(Arc::clone(&turn)),
            cancellation_token: cancellation.clone(),
            tracker: Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
            call_id: "cancelled-search".to_string(),
            tool_name: ToolName::plain(TOOL_SEARCH_TOOL_NAME),
            source: ToolCallSource::Direct,
            payload: ToolPayload::ToolSearch {
                arguments: codex_protocol::models::SearchToolCallParams {
                    query: "create_event".to_string(),
                    limit: Some(1),
                },
            },
        };
        let mut search = handler.handle(invocation);
        let first_poll = futures::poll!(&mut search);
        cancellation.cancel();
        let result = if first_poll.is_pending() {
            Some(tokio::time::timeout(std::time::Duration::from_secs(1), search).await)
        } else {
            None
        };
        let _ = release_tx.send(());
        blocker.join().unwrap();

        assert!(
            first_poll.is_pending(),
            "search must yield while its cache is held"
        );
        assert!(matches!(
            result,
            Some(Ok(Err(FunctionCallError::RespondToModel(message))))
                if message == "tool search was cancelled"
        ));
        assert!(turn.activated_deferred_tools().is_empty());
    }

    #[tokio::test]
    async fn colliding_token_hashes_do_not_return_or_activate_unrelated_tools() {
        // These distinct terms collided when postings used only a u32 hash.
        assert_eq!(
            <u32 as bm25::TokenEmbedder>::embed("term10409"),
            <u32 as bm25::TokenEmbedder>::embed("term10482"),
        );
        let first = search_info("term10409", None, "first", "run");
        let second = search_info("term10482", None, "second", "run");
        let expected_first = serde_json::to_value(&first.entry.output).unwrap();
        let expected_second = serde_json::to_value(&second.entry.output).unwrap();
        let handler = ToolSearchHandler::new(vec![first, second]);
        for (query, expected, namespace) in [
            ("term10409", expected_first, "mcp__first"),
            ("term10482", expected_second, "mcp__second"),
        ] {
            let (session, turn, _events) = make_session_and_context_with_rx().await;
            turn.refresh_deferred_tool_capabilities(Arc::new(
                ["mcp__first", "mcp__second"]
                    .into_iter()
                    .map(|name| (ToolName::namespaced(name, "run"), "v1".to_string()))
                    .collect(),
            ));
            let payload = ToolPayload::ToolSearch {
                arguments: codex_protocol::models::SearchToolCallParams {
                    query: query.to_string(),
                    limit: Some(2),
                },
            };
            let output = handler
                .handle(ToolInvocation {
                    session: Arc::clone(&session),
                    step_context: StepContext::for_test(Arc::clone(&turn)),
                    cancellation_token: CancellationToken::new(),
                    tracker: Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
                    call_id: "search".to_string(),
                    tool_name: ToolName::plain(TOOL_SEARCH_TOOL_NAME),
                    source: ToolCallSource::Direct,
                    payload: payload.clone(),
                })
                .await
                .expect("search invocation");

            assert!(turn.activated_deferred_tools().is_empty());
            session.record_tool_completion_ordered(&turn, "search", &[output.to_response_item("search", &payload).into()]).await.unwrap();            let codex_protocol::models::ResponseInputItem::ToolSearchOutput { tools, .. } =
                output.to_response_item("search", &payload)
            else {
                panic!("expected search response");
            };
            assert_eq!(tools, vec![expected], "query: {query}");
            assert_eq!(
                turn.activated_deferred_tools(),
                [ToolName::namespaced(namespace, "run")]
                    .into_iter()
                    .collect(),
                "query: {query}",
            );
        }
    }

    #[test]
    fn cached_results_reuse_serialized_tool_specs() {
        let handler = ToolSearchHandler::new(vec![search_info(
            "calendar",
            None,
            "calendar",
            "create_event",
        )]);
        LOADABLE_TOOL_SERIALIZATION_COUNT.store(0, Ordering::Relaxed);

        let first = handler.search("calendar", 10).expect("first search");
        let second = handler.search("calendar", 10).expect("cached search");
        assert!(Arc::ptr_eq(&first, &second));
        let output = ToolSearchOutput {
            tools: second.serialized_tools.clone(),
            omitted_result_count: 0,
            activated_omitted_tools: Vec::new(),
            unactivated_matches: Vec::new(),
            unmatched_identifiers: Vec::new(),
            exact_name_ambiguity: None,
        };
        let payload = ToolPayload::ToolSearch {
            arguments: codex_protocol::models::SearchToolCallParams {
                query: "calendar".to_string(),
                limit: None,
            },
        };
        let _ = crate::tools::context::ToolOutput::to_response_item(&output, "call-1", &payload);
        let _ = crate::tools::context::ToolOutput::code_mode_result(&output, &payload);

        assert_eq!(second.serialized_tools.len(), 1);
        assert_eq!(second.serialized_tools[0]["type"], "namespace");
        assert_eq!(second.serialized_tools[0]["name"], "mcp__calendar");
        assert_eq!(LOADABLE_TOOL_SERIALIZATION_COUNT.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn verified10_parameter_matches_survive_namespace_selection() {
        for count in [1, 2] {
            let spec = ToolSpec::Namespace(ResponsesApiNamespace {
                name: "mail".into(), description: "Operations".into(),
                tools: (0..count).map(|index| ResponsesApiNamespaceTool::Function(ResponsesApiTool {
                    name: format!("operation{index}"), description: "Operation".into(), strict: false, defer_loading: None,
                    parameters: codex_tools::parse_tool_input_schema(&serde_json::json!({
                        "type":"object", "properties": if index == 0 {
                            serde_json::json!({"recipient":{"type":"string", "description":"thread destination"}})
                        } else { serde_json::json!({"calendar":{"type":"string"}}) }
                    })).unwrap(), output_schema: None,
                })).collect(),
            });
            let handler = ToolSearchHandler::new(vec![ToolSearchInfo::from_tool_spec(&spec, None).unwrap()]);
            assert_eq!(handler.search("recipient thread", 1).unwrap().activation_tools,
                vec![ToolName::namespaced("mail", "operation0")]);
        }
    }

    #[test]
    fn eligible_candidates_survive_high_ranking_weak_matches() {
        let mut infos = (0..12).map(|index| search_info(
            if index % 2 == 0 { "weather" } else { "forecast" },
            None, "climate", &format!("operation{index}"))).collect::<Vec<_>>();
        infos.push(search_info(&format!("weather forecast {}", "padding ".repeat(2000)),
            None, "climate", "forecast_operation"));
        let handler = ToolSearchHandler::new(infos);
        let result = handler.search("weather forecast", 1).unwrap();
        assert_eq!(result.activation_tools, vec![ToolName::namespaced("mcp__climate", "forecast_operation")]);
        let (_, weak) = handler.search_index.top_matches("weather forecast", 3, &handler.search_infos, None, &[]);
        assert_eq!(weak.len(), 3);
    }

    #[test]
    fn connector_boilerplate_locates_but_does_not_activate_unrelated_calls() {
        let mut unrelated = tool_info("mail", "list_labels", "List label colors");
        unrelated.namespace_description = Some("Archive messages and manage labels".into());
        let unrelated = executor_search_info(McpHandler::new(unrelated).unwrap());
        let handler = ToolSearchHandler::new(vec![unrelated]);
        let weak = handler.search("archive messages", 8).unwrap();
        assert!(weak.activation_tools.is_empty());
        assert_eq!(weak.unactivated_matches, vec![ToolName::namespaced("mcp__mail", "list_labels").to_string()]);
        assert_eq!(handler.search("list_labels", 1).unwrap().activation_tools.len(), 1);
        assert_eq!(handler.search("label colors", 1).unwrap().activation_tools.len(), 1);
    }

    #[test]
    fn scoped_unknown_entities_preserve_single_term_capabilities_only() {
        let handler = ToolSearchHandler::new(vec![
            search_info("weather", None, "climate", "lookup"),
            search_info("scan pet spritesheet", None, "pets", "validate_pet"),
            search_info("repository file inventory scan", None, "repo", "inventory"),
        ]);
        assert_eq!(handler.search("weather", 1).unwrap().activation_tools.len(), 1);
        assert!(handler.search("weather Zelphara", 1).unwrap().activation_tools.is_empty());
        assert_eq!(handler.search("weather Zelphara source:mcp__climate", 1).unwrap().activation_tools,
            vec![ToolName::namespaced("mcp__climate", "lookup")]);
        for query in ["weather Zelphara source:mcp__pets", "repository file inventory scan source:mcp__pets",
            "weather +Zelphara source:mcp__climate", "Zelphara source:mcp__climate"] {
            assert!(handler.search(query, 8).unwrap().activation_tools.is_empty(), "{query}");
        }
        let error = handler.search("weather source:climate", 1).unwrap_err().to_string();
        assert!(error.contains("source:mcp__climate"));
    }

    #[test]
    fn verified10_task_identifiers_do_not_hide_capabilities() {
        let handler = ToolSearchHandler::new(vec![search_info("archive messages", None, "mail", "archive_messages")]);
        let plain = handler.search("archive messages", 1).unwrap();
        let mixed = handler.search("archive messages project_alpha", 1).unwrap();
        assert!(!plain.activation_tools.is_empty());
        assert_eq!(plain.activation_tools, mixed.activation_tools);
        assert_eq!(mixed.unmatched_identifiers, vec!["project_alpha"]);
        assert!(Arc::ptr_eq(&mixed, &handler.search("archive messages project_alpha", 1).unwrap()));
        assert!(handler.search("invented_tool", 1).is_err());
        assert!(handler.search("mail.invented_tool", 1).is_err());
        assert!(handler.search("archive_messages invented_tool", 2).is_err());
    }

    #[test]
    fn weak_matches_return_names_without_activation_and_exact_names_still_activate() {
        let handler = ToolSearchHandler::new(vec![
            search_info("scan pet spritesheet", Some("Pets"), "pets", "_validate_pet_spritesheet"),
            search_info("repository file inventory scan", None, "repo", "inventory"),
        ]);
        let result = handler.search("repository file inventory scan", 8).unwrap();
        assert_eq!(result.activation_tools, vec![ToolName::namespaced("mcp__repo", "inventory")]);
        assert_eq!(result.unactivated_matches, vec![
            ToolName::namespaced("mcp__pets", "_validate_pet_spritesheet").to_string(),
        ]);
        assert_eq!(result.serialized_tools.len(), 1);
        let exact = handler.search("_validate_pet_spritesheet", 1).unwrap();
        assert_eq!(exact.activation_tools, vec![
            ToolName::namespaced("mcp__pets", "_validate_pet_spritesheet"),
        ]);
        assert!(exact.unactivated_matches.is_empty());
    }

    #[test]
    fn search_ranks_matching_terms_with_lightweight_tokenizer() {
        let handler = ToolSearchHandler::new(vec![
            search_info(
                "reset account password credentials",
                Some("accounts"),
                "accounts",
                "reset_credentials",
            ),
            search_info(
                "inspect network proxy connections",
                Some("network"),
                "network",
                "inspect_proxy",
            ),
        ]);

        let result = handler
            .search("PASSWORD-reset", 1)
            .expect("matching terms should produce a result");

        let [LoadableToolSpec::Namespace(namespace)] = result.tools.as_slice() else {
            panic!("search should return one namespace");
        };
        assert_eq!(namespace.name, "mcp__accounts");
    }

    #[test]
    fn cache_reuses_handler_for_identical_search_infos_and_rebuilds_for_changes() {
        let cache = ToolSearchHandlerCache::default();
        let search_infos = vec![executor_search_info(
            McpHandler::new(tool_info("calendar", "create_event", "Create events"))
                .expect("MCP tool should convert"),
        )];

        let first = cache.get_or_build(search_infos.clone());
        let second = cache.get_or_build(search_infos.clone());
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(cache.fingerprint_compute_count(), 1);

        let mut changed_search_infos = search_infos.clone();
        changed_search_infos[0]
            .entry
            .search_text
            .push_str(" changed");
        let changed = cache.get_or_build(changed_search_infos);
        assert!(!Arc::ptr_eq(&first, &changed));

        let mut changed_source_infos = search_infos.clone();
        changed_source_infos[0]
            .source_info
            .as_mut()
            .expect("MCP search info should include source metadata")
            .name
            .push_str(" changed");
        let changed_source = cache.get_or_build(changed_source_infos);
        assert!(!Arc::ptr_eq(&first, &changed_source));

        let mut changed_output_infos = search_infos;
        match Arc::make_mut(&mut changed_output_infos[0].entry.output) {
            LoadableToolSpec::Function(tool) => tool.description.push_str(" changed"),
            LoadableToolSpec::Namespace(namespace) => namespace.description.push_str(" changed"),
        }
        let changed_output = cache.get_or_build(changed_output_infos);
        assert!(!Arc::ptr_eq(&first, &changed_output));
    }

    #[test]
    fn cache_singleflights_concurrent_identical_inventory_builds() {
        const THREAD_COUNT: usize = 8;
        let cache = Arc::new(ToolSearchHandlerCache::default());
        let search_infos = vec![executor_search_info(
            McpHandler::new(tool_info("calendar", "create_event", "Create events"))
                .expect("MCP tool should convert"),
        )];
        let barrier = Arc::new(Barrier::new(THREAD_COUNT));
        // Materialize every worker before joining so they can all cross the shared barrier.
        #[allow(clippy::needless_collect)]
        let threads = (0..THREAD_COUNT)
            .map(|_| {
                let cache = Arc::clone(&cache);
                let search_infos = search_infos.clone();
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    cache.get_or_build(search_infos)
                })
            })
            .collect::<Vec<_>>();
        let handlers = threads
            .into_iter()
            .map(|thread| thread.join().expect("cache build thread should finish"))
            .collect::<Vec<_>>();

        assert!(
            handlers
                .iter()
                .all(|handler| Arc::ptr_eq(&handlers[0], handler))
        );
        assert_eq!(cache.handler_build_count(), 1);
    }

    #[test]
    fn handler_precomputes_normalized_entry_and_output_names() {
        let handler = ToolSearchHandler::new(vec![search_info(
            "calendar lookup",
            Some("calendar"),
            "calendar",
            "  FIND_EVENT  ",
        )]);

        let names = &handler.name_indexes[0];
        assert!(names.has_entry_name("FIND_EVENT"));
        assert_eq!(
            names.output_names_for("FIND_EVENT"),
            Some(&HashSet::from(["  FIND_EVENT  ".to_string()]))
        );
    }

    #[test]
    fn reverse_name_index_preserves_ambiguous_and_qualified_matches() {
        let handler = ToolSearchHandler::new(vec![
            search_info(
                "calendar lookup",
                Some("calendar"),
                "calendar",
                "find_event",
            ),
            search_info("events lookup", Some("events"), "events", "find_event"),
        ]);
        assert_eq!(
            handler.exact_name_index.get("find_event"),
            Some(&vec![ToolSearchDocumentId(0), ToolSearchDocumentId(1)])
        );
        assert_eq!(
            handler.exact_name_index.get("mcp__calendar__find_event"),
            Some(&vec![ToolSearchDocumentId(0)])
        );
        let exact = handler
            .search("  MCP__CALENDAR__FIND_EVENT  ", 1)
            .expect("exact lookup");
        assert_eq!(
            exact.activation_tools,
            vec![ToolName::namespaced("mcp__calendar", "find_event")]
        );
        assert_eq!(exact.tools.len(), 1);
        let ambiguous = handler.search("find_event", 2).expect("ambiguous lookup");
        assert_eq!(
            ambiguous.activation_tools,
            vec![
                ToolName::namespaced("mcp__calendar", "find_event"),
                ToolName::namespaced("mcp__events", "find_event")
            ]
        );
        let limited = handler.search("find_event", 1).expect("limited lookup");
        assert_eq!(limited.activation_tools.len(), 1);
        assert_eq!(limited.exact_name_ambiguity, Some(serde_json::json!({
            "match_count": 2,
            "omitted_alternative_count": 0,
            "qualified_alternatives": ["mcp__calendar__find_event", "mcp__events__find_event"],
        })));
        assert!(exact.exact_name_ambiguity.is_none());
    }

    #[test]
    fn exact_case_and_callable_aliases_resolve_the_same_contract() {
        let infos = ["Read", "read", "read-file", "read_file"].map(|name| {
            search_info("read files", None, "files", name)
        });
        let handler = ToolSearchHandler::new(infos.to_vec());
        for info in &infos {
            let name = loadable_tool_names(info.entry.output.as_ref())[0].clone();
            let alias = codex_tools::code_mode_name_for_tool_name(&name);
            for query in [name.name.clone(), alias, format!("{}.{}", name.namespace.as_deref().unwrap(), name.name)] {
                assert_eq!(handler.search(&query, 1).unwrap().activation_tools, vec![name.clone()]);
            }
        }
        assert!(handler.search("READ", 1).is_err());
    }

    #[tokio::test]
    async fn parallel_search_context_is_published_in_call_order_only() {
        let mut histories = Vec::new();
        for reverse in [false, true] {
            let (mut session, mut turn, _) = make_session_and_context_with_rx().await;
            Arc::get_mut(&mut turn).unwrap().sub_id = "ordered-search-regression".into();
            let path = crate::session::tests::attach_thread_persistence(Arc::get_mut(&mut session).unwrap()).await;
            turn.refresh_deferred_tool_capabilities(Arc::new(
                ["one", "two"].into_iter().map(|ns| (ToolName::namespaced(format!("mcp__{ns}"), "read"), "v1".into())).collect()
            ));
            let handler = ToolSearchHandler::new(vec![
                search_info("read", None, "one", "read"),
                search_info("read", None, "two", "read"),
            ]);
            let payload = ToolPayload::ToolSearch {
                arguments: codex_protocol::models::SearchToolCallParams { query: "read".into(), limit: Some(1) },
            };
            let first_finished = tokio::sync::Notify::new();
            let run = async |id: &'static str, delayed: bool| {
                if delayed {
                    first_finished.notified().await;
                }
                let output = handler.handle(ToolInvocation {
                    session: Arc::clone(&session), step_context: StepContext::for_test(Arc::clone(&turn)),
                    cancellation_token: CancellationToken::new(),
                    tracker: Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
                    call_id: id.into(), tool_name: ToolName::plain(TOOL_SEARCH_TOOL_NAME),
                    source: ToolCallSource::Direct, payload: payload.clone(),
                }).await.unwrap();
                let mut item: codex_protocol::models::ResponseItem = output.to_response_item(id, &payload).into();
                item.set_id(Some(codex_protocol::ResponseItemId::with_suffix("tso", id)));
                if !delayed {
                    first_finished.notify_one();
                }
                item
            };
            let (first, second) = tokio::join!(run("first", reverse), run("second", !reverse));
            assert!(session.clone_history().await.raw_items().is_empty());
            assert!(turn.activated_deferred_tools().is_empty());
            for (id, item) in [("first", first), ("second", second)] {
                session.record_tool_completion_ordered(&turn, id, &[item]).await.unwrap();
            }
            session.flush_rollout().await.unwrap();
            let (persisted, _, errors) = crate::rollout::RolloutRecorder::load_rollout_items(&path).await.unwrap();
            assert_eq!(errors, 0);
            let persisted = persisted.into_iter().filter_map(|item| match item {
                codex_protocol::protocol::RolloutItem::ResponseItem(item) => Some(item),
                _ => None,
            }).collect::<Vec<_>>();
            // Stable fixture identities allow comparison of complete histories,
            // including metadata and repeated schema payloads.
            let history = session.clone_history().await;
            assert_eq!(persisted.as_slice(), history.raw_items());
            histories.push((history.raw_items().to_vec(), turn.deferred_tool_activation_revision(), turn.activated_deferred_tools()));
            session.live_thread().unwrap().shutdown().await.unwrap();
        }
        assert_eq!(histories[0], histories[1]);
        assert_eq!(histories[0].0.len(), 4);
    }

    #[tokio::test]
    async fn aborted_search_does_not_publish_staged_context_or_activations() {
        let (session, turn, _) = make_session_and_context_with_rx().await;
        turn.refresh_deferred_tool_capabilities(Arc::new(
            ["one", "two"].into_iter().map(|ns| (ToolName::namespaced(format!("mcp__{ns}"), "read"), "v1".into())).collect()
        ));
        let handler = ToolSearchHandler::new(vec![
            search_info("read", None, "one", "read"),
            search_info("read", None, "two", "read"),
        ]);
        let payload = ToolPayload::ToolSearch {
            arguments: codex_protocol::models::SearchToolCallParams { query: "read".into(), limit: Some(1) },
        };
        let output = handler.handle(ToolInvocation {
            session: Arc::clone(&session), step_context: StepContext::for_test(Arc::clone(&turn)),
            cancellation_token: CancellationToken::new(),
            tracker: Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
            call_id: "aborted".into(), tool_name: ToolName::plain(TOOL_SEARCH_TOOL_NAME),
            source: ToolCallSource::Direct, payload: payload.clone(),
        }).await.unwrap();
        let mut item: codex_protocol::models::ResponseItem = output.to_response_item("aborted", &payload).into();
        let codex_protocol::models::ResponseItem::ToolSearchOutput { status, tools, .. } = &mut item else {
            panic!("expected tool search output");
        };
        *status = "aborted".into();
        tools.clear();
        session.record_tool_completion_ordered(&turn, "aborted", &[item]).await.unwrap();
        assert!(turn.activated_deferred_tools().is_empty());
        assert!(turn.pending_post_tool_contexts.lock().await.is_empty());
        assert_eq!(session.clone_history().await.raw_items().len(), 1);
    }

    #[test]
    fn cache_retains_four_inventory_entries_in_lru_order() {
        let cache = ToolSearchHandlerCache::default();
        let inventories = (0..5)
            .map(|idx| {
                vec![executor_search_info(
                    McpHandler::new(tool_info(
                        "calendar",
                        &format!("tool_{idx}"),
                        "Calendar tool",
                    ))
                    .expect("MCP tool should convert"),
                )]
            })
            .collect::<Vec<_>>();
        let handlers = inventories[..4]
            .iter()
            .cloned()
            .map(|search_infos| cache.get_or_build(search_infos))
            .collect::<Vec<_>>();

        let refreshed_first = cache.get_or_build(inventories[0].clone());
        assert!(Arc::ptr_eq(&handlers[0], &refreshed_first));

        cache.get_or_build(inventories[4].clone());
        let rebuilt_second = cache.get_or_build(inventories[1].clone());
        assert!(!Arc::ptr_eq(&handlers[1], &rebuilt_second));

        let retained_first = cache.get_or_build(inventories[0].clone());
        assert!(Arc::ptr_eq(&handlers[0], &retained_first));
        assert_eq!(cache.cached_len(), MAX_TOOL_SEARCH_HANDLER_CACHE);
    }

    #[test]
    fn search_reuses_normalized_query_results_and_keys_by_limit() {
        let search_infos = vec![executor_search_info(
            McpHandler::new(tool_info("calendar", "create_event", "Create events"))
                .expect("MCP tool should convert"),
        )];
        let handler = ToolSearchHandler::new(search_infos);

        let first = handler
            .search("  Calendar   Events  ", TOOL_SEARCH_DEFAULT_LIMIT)
            .expect("search should succeed");
        let second = handler
            .search("calendar events", TOOL_SEARCH_DEFAULT_LIMIT)
            .expect("normalized query cache should succeed");
        let limited = handler
            .search("calendar events", 1)
            .expect("different limit should create a distinct cache entry");

        assert_eq!(first, second);
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(limited, first);
        assert_eq!(first.omitted_result_count, 0);
        assert_eq!(handler.result_cache_len(), 2);
    }

    #[test]
    fn search_result_cache_is_bounded_and_lru() {
        let search_infos = vec![executor_search_info(
            McpHandler::new(tool_info("calendar", "create_event", "Create events"))
                .expect("MCP tool should convert"),
        )];
        let handler = ToolSearchHandler::new(search_infos);

        for idx in 0..MAX_TOOL_SEARCH_RESULT_CACHE {
            handler
                .search(&format!("unmatched-query-{idx}"), TOOL_SEARCH_DEFAULT_LIMIT)
                .expect("search should succeed");
        }
        handler
            .search("unmatched-query-0", TOOL_SEARCH_DEFAULT_LIMIT)
            .expect("cache hit should refresh the oldest entry");
        handler
            .search(
                &format!("unmatched-query-{MAX_TOOL_SEARCH_RESULT_CACHE}"),
                TOOL_SEARCH_DEFAULT_LIMIT,
            )
            .expect("search should evict the least recently used entry");

        let cached = handler.result_cache();
        assert_eq!(cached.len(), MAX_TOOL_SEARCH_RESULT_CACHE);
        assert!(
            cached
                .iter()
                .any(|entry| entry.key.query == "unmatched-query-0")
        );
        assert!(
            !cached
                .iter()
                .any(|entry| entry.key.query == "unmatched-query-1")
        );
    }

    #[tokio::test]
    async fn natural_language_ranks_late_namespace_member_before_limit() {
        let mut info = search_info("calendar", None, "calendar", "create_event");
        let LoadableToolSpec::Namespace(namespace) = Arc::make_mut(&mut info.entry.output) else {
            panic!("expected namespace");
        };
        let ResponsesApiNamespaceTool::Function(template) = namespace.tools[0].clone();
        namespace.tools = [
            ("create_event", "Create a calendar event"),
            ("export_archive", "Export archived invoices for accounting"),
        ].into_iter().map(|(name, description)| ResponsesApiNamespaceTool::Function(ResponsesApiTool {
            name: name.into(), description: description.into(), ..template.clone()
        })).collect();
        let namespace = namespace.clone();
        let handler = ToolSearchHandler::new(vec![
            ToolSearchInfo::from_tool_spec(&ToolSpec::Namespace(namespace), None).unwrap(),
        ]);
        let result = handler.search("export archived invoices", 1).unwrap();
        assert_eq!(result.activation_tools.len(), 1);
        assert!(result.activation_tools[0].name.ends_with("export_archive"));
    }

    #[tokio::test]
    async fn namespace_limit_counts_callables_and_caches_selected_result() {
        let mut info = search_info("calendar", None, "calendar", "create_event");
        let LoadableToolSpec::Namespace(namespace) = Arc::make_mut(&mut info.entry.output) else {
            panic!("expected namespace");
        };
        let ResponsesApiNamespaceTool::Function(template) = namespace.tools[0].clone();
        namespace.tools = (0..4096)
            .map(|index| {
                ResponsesApiNamespaceTool::Function(ResponsesApiTool {
                    name: format!("operation_{index:054}"),
                    ..template.clone()
                })
            })
            .collect();
        let names = loadable_tool_names(&info.entry.output);
        let LoadableToolSpec::Namespace(namespace) = info.entry.to_loadable_spec() else {
            panic!("expected namespace");
        };
        let handler = ToolSearchHandler::new(vec![
            ToolSearchInfo::from_tool_spec(&ToolSpec::Namespace(namespace), None).unwrap(),
        ]);
        let (session, turn, _events) = make_session_and_context_with_rx().await;
        turn.refresh_deferred_tool_capabilities(Arc::new(
            names
                .iter()
                .cloned()
                .map(|name| (name, "calendar-v1".to_string()))
                .collect(),
        ));
        let payload = ToolPayload::ToolSearch {
            arguments: codex_protocol::models::SearchToolCallParams {
                query: names[0].to_string(),
                limit: Some(1),
            },
        };

        let output = handler
            .handle(ToolInvocation {
                session: Arc::clone(&session),
                step_context: StepContext::for_test(Arc::clone(&turn)),
                cancellation_token: CancellationToken::new(),
                tracker: Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
                call_id: "large-activation-search".to_string(),
                tool_name: ToolName::plain(TOOL_SEARCH_TOOL_NAME),
                source: ToolCallSource::Direct,
                payload: payload.clone(),
            })
            .await
            .expect("search must still activate oversized exact matches");

            assert!(turn.activated_deferred_tools().is_empty());
            session.record_tool_completion_ordered(&turn, "large-activation-search", &[output.to_response_item("large-activation-search", &payload).into()]).await.unwrap();        let codex_protocol::models::ResponseInputItem::ToolSearchOutput { tools, .. } =
            output.to_response_item("large-activation-search", &payload)
        else {
            panic!("expected search output");
        };
        assert_eq!(tools.len(), 1);
        let activated = turn.activated_deferred_tools();
        assert_eq!(activated.len(), 1);
        assert!(activated.contains(&names[0]));
        assert_eq!(handler.result_cache_len(), 1);
    }

    #[test]
    fn search_cache_counts_output_schemas_skipped_by_serialization() {
        let mut info = search_info("calendar", None, "calendar", "create_event");
        let LoadableToolSpec::Namespace(namespace) = Arc::make_mut(&mut info.entry.output) else {
            panic!("expected namespace");
        };
        let ResponsesApiNamespaceTool::Function(tool) = &mut namespace.tools[0];
        tool.output_schema = Some(serde_json::json!({
            "const": "x".repeat(MAX_TOOL_SEARCH_CACHE_ENTRY_BYTES)
        }).into());
        let handler = ToolSearchHandler::new(vec![info]);

        let result = handler.search("calendar", 1).unwrap();

        assert_eq!(result.tools.len(), 1);
        assert_eq!(
            result.activation_tools,
            vec![ToolName::namespaced("mcp__calendar", "create_event")]
        );
        assert_eq!(handler.result_cache_len(), 0);
    }

    #[test]
    fn candidate_limit_overfetches_and_saturates_at_inventory_size() {
        assert_eq!(tool_search_candidate_limit(3, 100), 9);
        assert_eq!(tool_search_candidate_limit(10, 5), 5);
        assert_eq!(tool_search_candidate_limit(usize::MAX, 7), 7);
    }

    #[test]
    fn result_builder_counts_compact_json_exactly_and_coalesces_namespaces() {
        let first = search_info("calendar", None, "calendar", "créer")
            .entry
            .output;
        let second = search_info("calendar", None, "calendar", "list_予定")
            .entry
            .output;
        let third = search_info("mail", None, "mail", "send").entry.output;
        let mut builder = ToolSearchResultBuilder::new();

        assert!(builder.try_push(&first));
        assert!(builder.try_push(&second));
        assert!(builder.try_push(&third));
        let (tools, encoded_tools_len) = builder.finish();

        assert_eq!(encoded_tools_len, serde_json::to_vec(&tools).unwrap().len());
        assert_eq!(tools.len(), 2);
        let LoadableToolSpec::Namespace(calendar) = &tools[0] else {
            panic!("first result should retain the calendar namespace");
        };
        assert_eq!(calendar.tools.len(), 2);
    }

    #[test]
    fn serialized_length_counter_obeys_the_exact_byte_boundary() {
        let tool = search_info("calendar", None, "calendar", "créer")
            .entry
            .output;
        let exact_len = serde_json::to_vec(&tool).unwrap().len();

        assert_eq!(serialized_len_with_limit(&tool, exact_len), Some(exact_len));
        assert_eq!(serialized_len_with_limit(&tool, exact_len - 1), None);
    }

    #[test]
    fn search_rejects_oversized_queries_and_limits() {
        let handler = ToolSearchHandler::new(vec![search_info(
            "calendar",
            None,
            "calendar",
            "create_event",
        )]);

        let oversized_query = "q".repeat(MAX_TOOL_SEARCH_QUERY_BYTES + 1);
        let query_error = handler
            .search(&oversized_query, TOOL_SEARCH_DEFAULT_LIMIT)
            .expect_err("oversized query should fail");
        assert!(
            query_error
                .to_string()
                .contains("query must not exceed 4096 bytes")
        );

        let limit_error = handler
            .search("calendar", MAX_TOOL_SEARCH_LIMIT + 1)
            .expect_err("oversized limit should fail");
        assert!(limit_error.to_string().contains("limit must not exceed 64"));
    }

    #[test]
    fn search_breaks_score_ties_before_the_candidate_cutoff() {
        for _ in 0..32 {
            let search_infos = (0..20)
                .map(|idx| {
                    search_info_with_source(
                        "shared capability",
                        &format!("source-{idx:02}"),
                        &format!("tool-{idx:02}"),
                    )
                })
                .collect();
            let handler = ToolSearchHandler::new(search_infos);

            let tools = handler
                .search("shared capability", 1)
                .expect("tied-score search should succeed");
            let [LoadableToolSpec::Namespace(namespace)] = tools.tools.as_slice() else {
                panic!("search should return one namespace");
            };

            assert_eq!(namespace.name, "mcp__source-00");
            assert_eq!(tools.omitted_result_count, 0);
        }
    }

    #[test]
    fn exact_name_search_recovers_a_definition_that_exceeds_the_result_budget() {
        let mut search_info = search_info("calendar", None, "calendar", "create_event");
        let LoadableToolSpec::Namespace(namespace) = Arc::make_mut(&mut search_info.entry.output) else {
            panic!("test search info should be a namespace");
        };
        namespace.description = "x".repeat(MAX_TOOL_SEARCH_RESULT_BYTES);
        let [ResponsesApiNamespaceTool::Function(source_tool)] = namespace.tools.as_mut_slice()
        else {
            panic!("test search info should contain one function");
        };
        source_tool.description = format!(
            "Create a calendar event. {} Requires an existing calendar.",
            "é".repeat(350)
        );
        let expected_description = source_tool.description.clone();
        source_tool.parameters = codex_tools::JsonSchema::object(
            std::collections::BTreeMap::from([(
                "title".to_string(),
                codex_tools::JsonSchema {
                    enum_values: Some(vec![serde_json::json!(2), serde_json::json!(4)]),
                    minimum: Some(1.into()),
                    maximum: Some(8.into()),
                    exclusive_minimum: Some(0.into()),
                    exclusive_maximum: Some(9.into()),
                    multiple_of: Some(2.into()),
                    ..codex_tools::JsonSchema::integer(Some(
                        "Duration in minutes; requires an existing calendar.".to_string(),
                    ))
                },
            )]),
            Some(vec!["title".to_string()]),
            Some(false.into()),
        );
        let expected_parameters = source_tool.parameters.clone();
        let handler = ToolSearchHandler::new(vec![search_info]);

        let tools = handler
            .search("create_event", TOOL_SEARCH_DEFAULT_LIMIT)
            .expect("exact-name oversized result should recover within the budget");

        let [LoadableToolSpec::Namespace(namespace)] = tools.tools.as_slice() else {
            panic!("exact-name recovery should retain the matching namespace");
        };
        let [ResponsesApiNamespaceTool::Function(tool)] = namespace.tools.as_slice() else {
            panic!("exact-name recovery should retain only the matching function");
        };
        assert_eq!(tool.name, "create_event");
        assert!(tool.description.starts_with("Create a calendar event."));
        assert!(tool.description.starts_with(&expected_description));
        assert!(tool.description.contains("verbose schema details"));
        assert_eq!(tool.parameters, expected_parameters);
        assert!(
            tool.parameters
                .properties
                .as_ref()
                .is_some_and(|properties| properties.contains_key("title"))
        );
        assert_eq!(tool.parameters.required, Some(vec!["title".to_string()]));
        assert_eq!(tool.parameters.additional_properties, Some(false.into()));
        let title = tool
            .parameters
            .properties
            .as_ref()
            .and_then(|properties| properties.get("title"))
            .expect("title schema");
        assert_eq!(title.minimum, Some(1.into()));
        assert_eq!(title.maximum, Some(8.into()));
        assert_eq!(
            title.enum_values,
            Some(vec![serde_json::json!(2), serde_json::json!(4)])
        );
        assert_eq!(title.exclusive_minimum, Some(0.into()));
        assert_eq!(title.exclusive_maximum, Some(9.into()));
        assert_eq!(title.multiple_of, Some(2.into()));
        assert_eq!(
            title.description.as_deref(),
            Some("Duration in minutes; requires an existing calendar.")
        );
        assert_eq!(tools.omitted_result_count, 0);
        assert!(
            serde_json::to_vec(&tools.tools)
                .expect("recovered result should serialize")
                .len()
                <= MAX_TOOL_SEARCH_RESULT_BYTES
        );
        assert_eq!(handler.result_cache_len(), 1);

        let cached = handler
            .search("create_event", TOOL_SEARCH_DEFAULT_LIMIT)
            .expect("cached exact-name recovery should succeed");
        assert_eq!(cached, tools);
    }

    #[test]
    fn audit_tool_search_contract_compaction_preserves_strictness_and_output_schema() {
        let output_schema = serde_json::to_value(codex_tools::JsonSchema::string(None))
            .expect("output schema should serialize");
        let source = ResponsesApiTool {
            name: "strict_tool".to_string(),
            description: format!(
                "{} Timeout is in milliseconds; never retry a completed mutation.",
                "context ".repeat(100)
            ),
            strict: true,
            defer_loading: Some(true),
            parameters: codex_tools::JsonSchema::object(
                Default::default(),
                Some(Vec::new()),
                Some(false.into()),
            ),
            output_schema: Some(output_schema.clone().into()),
        };

        let compact = compact_recovery_tool(&source, "strict_tool");

        assert!(compact.strict);
        assert!(compact.description.starts_with(&source.description));
        assert_eq!(compact.output_schema, Some(output_schema.into()));
        assert_eq!(compact.parameters.required, Some(Vec::new()));
        assert_eq!(compact.parameters.additional_properties, Some(false.into()));
    }

    #[test]
    fn search_preserves_complete_definition_at_six_kib_limit() {
        let mut info = search_info("calendar", None, "calendar", "create_event");
        let original_bytes = serde_json::to_vec(&[info.entry.output.as_ref()])
            .expect("fixture should serialize")
            .len();
        let LoadableToolSpec::Namespace(namespace) = Arc::make_mut(&mut info.entry.output) else {
            panic!("namespace fixture");
        };
        let [ResponsesApiNamespaceTool::Function(tool)] = namespace.tools.as_mut_slice() else {
            panic!("single tool fixture");
        };
        tool.description
            .push_str(&"x".repeat(6 * 1024 - original_bytes));
        let expected = serde_json::to_value([info.entry.output.as_ref()])
            .expect("complete definition should serialize");

        let result = ToolSearchHandler::new(vec![info])
            .search("create_event", TOOL_SEARCH_DEFAULT_LIMIT)
            .expect("six-KiB definition should fit without compaction");

        assert_eq!(result.encoded_tools_len, 6 * 1024);
        assert_eq!(result.omitted_result_count, 0);
        assert_eq!(serde_json::to_value(&result.tools).unwrap(), expected);
        assert_eq!(
            result.activation_tools,
            vec![ToolName::namespaced("mcp__calendar", "create_event")]
        );
    }

    #[test]
    fn oversized_tool_description_uses_omission_instead_of_a_partial_contract() {
        let mut info = search_info("calendar", None, "calendar", "create_event");
        let LoadableToolSpec::Namespace(namespace) = Arc::make_mut(&mut info.entry.output) else {
            panic!("namespace fixture");
        };
        let [ResponsesApiNamespaceTool::Function(tool)] = namespace.tools.as_mut_slice() else {
            panic!("single tool fixture");
        };
        tool.description = format!(
            "{} Never retry a completed mutation.",
            "x".repeat(MAX_TOOL_SEARCH_RESULT_BYTES)
        );
        let result = ToolSearchHandler::new(vec![info])
            .search("create_event", TOOL_SEARCH_DEFAULT_LIMIT)
            .unwrap();
        assert!(result.tools.is_empty());
        assert_eq!(result.omitted_result_count, 1);
        assert_eq!(
            result.activation_tools,
            vec![ToolName::namespaced("mcp__calendar", "create_event")]
        );
        assert!(result.encoded_tools_len <= MAX_TOOL_SEARCH_RESULT_BYTES);
    }

    #[test]
    fn token_backfire_oversized_exact_match_stays_activatable() {
        let mut search_info = search_info("calendar", None, "calendar", "create_event");
        let LoadableToolSpec::Namespace(namespace) = Arc::make_mut(&mut search_info.entry.output) else {
            panic!("test search info should be a namespace");
        };
        namespace.description = "x".repeat(MAX_TOOL_SEARCH_RESULT_BYTES);
        let [ResponsesApiNamespaceTool::Function(source_tool)] = namespace.tools.as_mut_slice()
        else {
            panic!("test search info should contain one function");
        };
        source_tool.strict = true;
        source_tool.parameters = codex_tools::JsonSchema::object(
            std::collections::BTreeMap::from([(
                "payload".to_string(),
                codex_tools::JsonSchema {
                    enum_values: Some(vec![serde_json::json!(
                        "x".repeat(MAX_TOOL_SEARCH_RESULT_BYTES)
                    )]),
                    ..codex_tools::JsonSchema::string(None)
                },
            )]),
            Some(vec!["payload".to_string()]),
            Some(false.into()),
        );

        let tools = ToolSearchHandler::new(vec![search_info])
            .search("create_event", TOOL_SEARCH_DEFAULT_LIMIT)
            .expect("oversized exact-name search should keep the tool activatable");

        assert!(tools.tools.is_empty());
        assert_eq!(tools.omitted_result_count, 1);
        assert!(tools.encoded_tools_len <= MAX_TOOL_SEARCH_RESULT_BYTES);
        assert_eq!(
            tools.activation_tools,
            vec![ToolName::namespaced("mcp__calendar", "create_event")]
        );
    }

    #[test]
    fn qualified_namespace_name_is_an_exact_search_match() {
        let handler = ToolSearchHandler::new(vec![search_info(
            "calendar",
            None,
            "calendar",
            "create_event",
        )]);

        let tools = handler
            .search("mcp__calendar__create_event", TOOL_SEARCH_DEFAULT_LIMIT)
            .expect("qualified exact-name search should succeed");

        assert_eq!(tools.tools.len(), 1);
        assert_eq!(
            tools.activation_tools,
            vec![ToolName::namespaced("mcp__calendar", "create_event")]
        );
    }

    #[test]
    fn search_compacts_container_descriptions_without_changing_selection() {
        let mut first = search_info("first", None, "first", "run");
        let mut second = search_info("second", None, "second", "run");
        for search_info in [&mut first, &mut second] {
            let LoadableToolSpec::Namespace(namespace) = Arc::make_mut(&mut search_info.entry.output) else {
                panic!("test search info should be a namespace");
            };
            namespace.description = "x".repeat(MAX_TOOL_SEARCH_RESULT_BYTES / 2);
        }
        let handler = ToolSearchHandler::new(vec![first, second]);
        let results = [ToolSearchDocumentId(0), ToolSearchDocumentId(1)];

        let tools = handler
            .search_output_tools(results, None, TOOL_SEARCH_DEFAULT_LIMIT, false)
            .expect("search results should serialize within the budget");

        assert_eq!(tools.tools.len(), 2);
        assert_eq!(tools.omitted_result_count, 0);
        assert!(
            serde_json::to_vec(&tools.tools)
                .expect("bounded search result should serialize")
                .len()
                <= MAX_TOOL_SEARCH_RESULT_BYTES
        );
    }

    #[test]
    fn semantic_search_preserves_best_match_when_receipt_requires_compaction() {
        let mut oversized = search_info("capability", None, "oversized", "run");
        let later = search_info("capability extra terms", None, "later", "run");
        let LoadableToolSpec::Namespace(namespace) = Arc::make_mut(&mut oversized.entry.output) else {
            panic!("test search info should be a namespace");
        };
        namespace.description = "x".repeat(MAX_TOOL_SEARCH_RESULT_BYTES);
        let handler = ToolSearchHandler::new(vec![oversized, later]);
        let tools = handler
            .search("capability", 1)
            .expect("later search result should fit within the budget");

        assert_eq!(tools.tools.len(), 1);
        assert_eq!(tools.omitted_result_count, 0);
        assert_eq!(
            tools.activation_tools,
            vec![ToolName::namespaced("mcp__oversized", "run")]
        );
    }

    #[test]
    fn mixed_search_results_coalesce_mcp_namespaces() {
        let dynamic_namespace = DynamicToolNamespaceSpec {
            name: "codex_app".to_string(),
            description: "Tools in the codex_app namespace.".to_string(),
            tools: Vec::new(),
        };
        let dynamic_tools = [DynamicToolFunctionSpec {
            name: "automation_update".to_string(),
            description: "Create, update, view, or delete recurring automations.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "mode": { "type": "string" },
                },
                "required": ["mode"],
                "additionalProperties": false,
            }),
            defer_loading: true,
        }];
        let mcp_tools = [
            tool_info("calendar", "create_event", "Create events"),
            tool_info("calendar", "list_events", "List events"),
        ];
        let mut search_infos = mcp_tools
            .iter()
            .map(|tool| {
                executor_search_info(
                    McpHandler::new(tool.clone()).expect("MCP tool should convert"),
                )
            })
            .collect::<Vec<_>>();
        search_infos.extend(dynamic_tools.iter().map(|tool| {
            executor_search_info(
                DynamicToolHandler::new_in_namespace(&dynamic_namespace, tool)
                    .expect("dynamic tool should convert"),
            )
        }));
        let handler = ToolSearchHandler::new(search_infos);
        let results = [
            ToolSearchDocumentId(0),
            ToolSearchDocumentId(2),
            ToolSearchDocumentId(1),
        ];

        let tools = handler
            .search_output_tools(results, None, TOOL_SEARCH_DEFAULT_LIMIT, false)
            .expect("mixed search output should serialize");

        assert_eq!(
            tools.tools,
            vec![
                LoadableToolSpec::Namespace(ResponsesApiNamespace {
                    name: "mcp__calendar".to_string(),
                    description: "Tools in the mcp__calendar namespace.".to_string(),
                    tools: vec![
                        ResponsesApiNamespaceTool::Function(ResponsesApiTool {
                            name: "create_event".to_string(),
                            description: "Create events desktop tool".to_string(),
                            strict: false,
                            defer_loading: Some(true),
                            parameters: codex_tools::JsonSchema::object(
                                Default::default(),
                                /*required*/ None,
                                Some(false.into()),
                            ),
                            output_schema: None,
                        }),
                        ResponsesApiNamespaceTool::Function(ResponsesApiTool {
                            name: "list_events".to_string(),
                            description: "List events desktop tool".to_string(),
                            strict: false,
                            defer_loading: Some(true),
                            parameters: codex_tools::JsonSchema::object(
                                Default::default(),
                                /*required*/ None,
                                Some(false.into()),
                            ),
                            output_schema: None,
                        }),
                    ],
                }),
                LoadableToolSpec::Namespace(ResponsesApiNamespace {
                    name: "codex_app".to_string(),
                    description: "Tools in the codex_app namespace.".to_string(),
                    tools: vec![ResponsesApiNamespaceTool::Function(ResponsesApiTool {
                        name: "automation_update".to_string(),
                        description: "Create, update, view, or delete recurring automations."
                            .to_string(),
                        strict: false,
                        defer_loading: Some(true),
                        parameters: codex_tools::JsonSchema::object(
                            std::collections::BTreeMap::from([(
                                "mode".to_string(),
                                codex_tools::JsonSchema::string(/*description*/ None),
                            )]),
                            Some(vec!["mode".to_string()]),
                            Some(false.into()),
                        ),
                        output_schema: None,
                    })],
                }),
            ],
        );
    }

    #[test]
    fn diversify_search_results_round_robins_by_source() {
        let calendar_create = search_info_with_source("calendar-create", "calendar", "create");
        let calendar_list = search_info_with_source("calendar-list", "calendar", "list");
        let calendar_delete = search_info_with_source("calendar-delete", "calendar", "delete");
        let docs_search = search_info_with_source("docs-search", "docs", "search");
        let calendar_update = search_info_with_source("calendar-update", "calendar", "update");

        let results = vec![
            calendar_create,
            calendar_list,
            calendar_delete,
            docs_search,
            calendar_update,
        ];
        let diversified = diversify_search_result_ids(
            &results,
            (0..results.len()).map(ToolSearchDocumentId).collect(),
            3,
        );
        let diversified_names = diversified
            .iter()
            .map(|id| id.info(&results).entry.search_text.as_str())
            .collect::<Vec<_>>();

        assert_eq!(
            diversified_names,
            vec!["calendar-create", "docs-search", "calendar-list"],
        );
    }

    #[test]
    fn diversify_search_results_falls_back_to_namespace_identity() {
        let alpha_first = search_info("alpha-first", None, "alpha", "first");
        let alpha_second = search_info("alpha-second", None, "alpha", "second");
        let beta_first = search_info("beta-first", None, "beta", "first");

        let results = vec![alpha_first, alpha_second, beta_first];
        let diversified = diversify_search_result_ids(
            &results,
            (0..results.len()).map(ToolSearchDocumentId).collect(),
            2,
        );
        let diversified_names = diversified
            .iter()
            .map(|id| id.info(&results).entry.search_text.as_str())
            .collect::<Vec<_>>();

        assert_eq!(diversified_names, vec!["alpha-first", "beta-first"]);
    }

    #[test]
    fn search_overfetches_then_returns_diverse_sources() {
        let handler = ToolSearchHandler::new(vec![
            search_info_with_source("shared capability", "alpha", "first"),
            search_info_with_source("shared capability", "alpha", "second"),
            search_info_with_source("shared capability", "alpha", "third"),
            search_info_with_source("shared capability", "beta", "first"),
            search_info_with_source("shared capability", "gamma", "first"),
        ]);

        let tools = handler
            .search("shared capability", 3)
            .expect("search should return diverse results");
        let namespaces = tools
            .tools
            .iter()
            .map(|tool| match tool {
                LoadableToolSpec::Namespace(namespace) => namespace.name.as_str(),
                LoadableToolSpec::Function(tool) => tool.name.as_str(),
            })
            .collect::<Vec<_>>();

        assert_eq!(namespaces, vec!["mcp__alpha", "mcp__beta", "mcp__gamma"]);
    }

    #[test]
    fn search_promotes_exact_normalized_tool_names_before_ranked_results() {
        let handler = ToolSearchHandler::new(vec![
            search_info("unrelated terms", None, "exact", "Target_Tool"),
            search_info("target_tool target_tool", None, "ranked", "other_tool"),
        ]);

        let tools = handler
            .search("  TARGET_TOOL  ", 2)
            .expect("search should promote the exact normalized tool name");
        let namespaces = tools
            .tools
            .iter()
            .map(|tool| match tool {
                LoadableToolSpec::Namespace(namespace) => namespace.name.as_str(),
                LoadableToolSpec::Function(tool) => tool.name.as_str(),
            })
            .collect::<Vec<_>>();

        assert_eq!(namespaces, vec!["mcp__exact", "mcp__ranked"]);
    }

    #[test]
    fn exact_name_promotion_preserves_ranked_order_without_duplicates() {
        let search_infos = vec![
            search_info("exact", None, "exact", "target_tool"),
            search_info("ranked-first", None, "ranked-first", "first"),
            search_info("ranked-second", None, "ranked-second", "second"),
        ];

        let results = promote_exact_name_matches(
            &search_infos,
            &[ToolSearchDocumentId(0)],
            &[
                ToolSearchDocumentId(1),
                ToolSearchDocumentId(0),
                ToolSearchDocumentId(2),
            ],
            3,
        );
        let search_texts = results
            .iter()
            .map(|result_id| result_id.info(&search_infos).entry.search_text.as_str())
            .collect::<Vec<_>>();

        assert_eq!(search_texts, vec!["exact", "ranked-first", "ranked-second"]);
    }

    #[test]
    fn exact_name_promotion_does_not_crowd_out_other_sources() {
        let mut search_infos = vec![
            search_info_with_source("search shared capability", "beta", "beta_tool"),
            search_info_with_source("search shared capability", "gamma", "gamma_tool"),
        ];
        search_infos.extend((0..20).map(|idx| {
            search_info_with_source(
                &format!("search shared capability {idx}"),
                "alpha",
                "search",
            )
        }));
        let handler = ToolSearchHandler::new(search_infos);

        let tools = handler
            .search("search", 3)
            .expect("exact-name search should preserve source diversity");
        let namespaces = tools
            .tools
            .iter()
            .map(|tool| match tool {
                LoadableToolSpec::Namespace(namespace) => namespace.name.as_str(),
                LoadableToolSpec::Function(tool) => tool.name.as_str(),
            })
            .collect::<Vec<_>>();

        assert_eq!(namespaces, vec!["mcp__alpha", "mcp__beta", "mcp__gamma"]);
    }

    #[test]
    fn discovery_recovers_small_sources_before_shortlist_truncation() {
        let mut infos = (0..100).map(|index| search_info_with_source(
            "shared capability", "large", &format!("operation_{index}"),
        )).collect::<Vec<_>>();
        infos.push(search_info_with_source("shared capability", "small", "lookup"));
        let result = ToolSearchHandler::new(infos).search("shared capability", 2).unwrap();
        assert_eq!(result.activation_tools.len(), 2);
        assert!(result.activation_tools.contains(&ToolName::namespaced("mcp__small", "lookup")));
    }

    #[test]
    fn capability_evidence_ignores_entities_without_activating_incidental_hits() {
        let handler = ToolSearchHandler::new(vec![
            search_info_with_source("create calendar event", "calendar", "create_event"),
            search_info_with_source("scan pet spritesheet", "pets", "validate_pet"),
            search_info_with_source("repository file inventory scan", "repo", "inventory"),
        ]);
        let result = handler.search("create calendar event for Zelphara on 20490317", 8).unwrap();
        assert_eq!(result.activation_tools, vec![ToolName::namespaced("mcp__calendar", "create_event")]);
        for query in ["scan Zelphara 20490317", "repository file inventory scan Zelphara"] {
            let result = handler.search(query, 8).unwrap();
            assert!(!result.activation_tools.contains(&ToolName::namespaced("mcp__pets", "validate_pet")));
        }
        assert!(handler.search("create calendar event +Zelphara", 8).unwrap().activation_tools.is_empty());
    }

    #[test]
    fn canonical_source_scope_cannot_be_satisfied_by_metadata() {
        let handler = ToolSearchHandler::new(vec![
            search_info_with_source("create calendar event", "alpha", "create_event"),
            search_info_with_source("create calendar event alpha", "beta", "create_event"),
        ]);
        let scoped = handler.search("create calendar event source:mcp__alpha", 8).unwrap();
        assert_eq!(scoped.activation_tools, vec![ToolName::namespaced("mcp__alpha", "create_event")]);
        assert_eq!(handler.search("create calendar event", 8).unwrap().activation_tools.len(), 2);
        assert!(handler.search("create source:alpha", 8).is_err());
        assert!(handler.search("create source:mcp__alpha source:mcp__beta", 8).is_err());
        assert!(handler.search("create source:", 8).is_err());
        assert!(Arc::ptr_eq(&scoped, &handler.search("create calendar event source:mcp__alpha", 8).unwrap()));
    }

    #[test]
    fn omitted_limit_narrows_only_a_unique_entire_callable_identity() {
        let handler = ToolSearchHandler::new(vec![
            search_info_with_source("shared capability target", "alpha", "target"),
            search_info_with_source("shared capability target", "beta", "other"),
        ]);
        assert_eq!(handler.effective_limit("target", None).unwrap(), 1);
        assert_eq!(handler.effective_limit("mcp__alpha.target", None).unwrap(), 1);
        assert_eq!(handler.effective_limit("target", Some(8)).unwrap(), 8);
        assert_eq!(handler.effective_limit("target capability", None).unwrap(), TOOL_SEARCH_DEFAULT_LIMIT);
        assert_eq!(handler.search("mcp__alpha.target", 1).unwrap().activation_tools,
            vec![ToolName::namespaced("mcp__alpha", "target")]);
        let ambiguous = ToolSearchHandler::new(vec![
            search_info_with_source("target", "alpha", "target"),
            search_info_with_source("target", "beta", "target"),
        ]);
        assert_eq!(ambiguous.effective_limit("target", None).unwrap(), TOOL_SEARCH_DEFAULT_LIMIT);
        assert_eq!(ambiguous.effective_limit("target source:mcp__alpha", None).unwrap(), 1);
    }

    #[test]
    fn native_and_mcp_names_carry_natural_language_capabilities() {
        for name in ["export_calendar_events", "exportCalendarEvents"] {
            let mut tool = tool_info("calendar", name, "");
            tool.tool.description = None;
            let mcp = McpHandler::new(tool).unwrap();
            let spec = mcp.spec();
            for info in [mcp.search_info().unwrap(), ToolSearchInfo::from_tool_spec(&spec, None).unwrap()] {
                let handler = ToolSearchHandler::new(vec![info]);
                assert_eq!(handler.search("export calendar events", 1).unwrap().activation_tools,
                    vec![mcp.tool_name()]);
            }
        }
    }

    fn search_info_with_source(
        search_text: &str,
        source_name: &str,
        tool_name: &str,
    ) -> ToolSearchInfo {
        search_info(search_text, Some(source_name), source_name, tool_name)
    }

    fn search_info(
        search_text: &str,
        source_name: Option<&str>,
        namespace_name: &str,
        tool_name: &str,
    ) -> ToolSearchInfo {
        ToolSearchInfo {
            entry: ToolSearchEntry {
                search_text: search_text.to_string(),
                tool_names: vec![tool_name.to_string()],
                output: Arc::new(LoadableToolSpec::Namespace(ResponsesApiNamespace {
                    name: format!("mcp__{namespace_name}"),
                    description: format!("Tools in the {namespace_name} namespace."),
                    tools: vec![ResponsesApiNamespaceTool::Function(ResponsesApiTool {
                        name: tool_name.to_string(),
                        description: search_text.to_string(),
                        strict: false,
                        defer_loading: Some(true),
                        parameters: codex_tools::JsonSchema::object(
                            Default::default(),
                            /*required*/ None,
                            Some(false.into()),
                        ),
                        output_schema: None,
                    })],
                })),
                normalize_on_selection: false,
            },
            source_info: source_name.map(|source_name| ToolSearchSourceInfo {
                name: source_name.to_string(),
                description: None,
            }),
        }
    }

    fn tool_info(server_name: &str, tool_name: &str, description_prefix: &str) -> ToolInfo {
        ToolInfo {
            server_name: server_name.to_string(),
            supports_parallel_tool_calls: false,
            server_origin: None,
            callable_name: tool_name.to_string(),
            callable_namespace: format!("mcp__{server_name}"),
            namespace_description: None,
            tool: Tool::new(
                tool_name.to_string(),
                format!("{description_prefix} desktop tool"),
                Arc::new(rmcp::model::object(serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false,
                }))),
            ),
            connector_id: None,
            connector_name: None,
            plugin_display_names: Vec::new(),
        }
    }
}
