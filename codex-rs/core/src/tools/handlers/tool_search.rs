use crate::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::ToolSearchOutput;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::tool_search_spec::create_tool_search_tool;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use bm25::TokenEmbedder;
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
use tracing::instrument;
use unicode_segmentation::UnicodeSegmentation;

const MAX_TOOL_SEARCH_HANDLER_CACHE: usize = 4;
const MAX_TOOL_SEARCH_RESULT_CACHE: usize = 32;
const MAX_TOOL_SEARCH_CACHE_ENTRY_BYTES: usize = 256 * 1024;
// Tool-search outputs stay in model-visible history. Keep each serialized
// result near a 768-token projection (using the core's 4 bytes/token estimate)
// while exact-name recovery preserves a callable schema for oversized tools.
const MAX_TOOL_SEARCH_RESULT_BYTES: usize = 3 * 1024;
const MAX_TOOL_SEARCH_QUERY_BYTES: usize = 4 * 1024;
const MAX_TOOL_SEARCH_LIMIT: usize = 64;
const TOOL_SEARCH_CANDIDATE_MULTIPLIER: usize = 3;

#[derive(Debug, Default)]
struct ToolSearchTokenizer;

impl Tokenizer for ToolSearchTokenizer {
    fn tokenize(&self, input_text: &str) -> Vec<String> {
        input_text.unicode_words().map(str::to_lowercase).collect()
    }
}

pub struct ToolSearchHandler {
    search_infos: Arc<[ToolSearchInfo]>,
    name_indexes: Vec<ToolSearchNameIndex>,
    spec: ToolSpec,
    search_index: ToolSearchIndex,
    result_cache: Mutex<VecDeque<ToolSearchCacheEntry>>,
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
    postings: HashMap<u32, Vec<(ToolSearchDocumentId, f32)>>,
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
    fn new(search_infos: &[ToolSearchInfo]) -> Self {
        const K1: f32 = 1.2;
        const B: f32 = 0.75;
        const FALLBACK_AVERAGE_DOCUMENT_LENGTH: f32 = 256.0;

        let tokenizer = ToolSearchTokenizer;
        let tokenized_documents = search_infos
            .iter()
            .map(|search_info| {
                tokenizer
                    .tokenize(&search_info.entry.search_text)
                    .into_iter()
                    .map(|token| <u32 as TokenEmbedder>::embed(&token))
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        let average_document_length = {
            let total_document_length = tokenized_documents.iter().map(Vec::len).sum::<usize>();
            let average = total_document_length as f64 / tokenized_documents.len() as f64;
            let average = average as f32;
            if average > 0.0 {
                average
            } else {
                FALLBACK_AVERAGE_DOCUMENT_LENGTH
            }
        };

        let mut postings = HashMap::<u32, Vec<(ToolSearchDocumentId, f32)>>::new();
        for (index, tokens) in tokenized_documents.into_iter().enumerate() {
            let document_length = tokens.len() as f32;
            let mut term_frequencies = HashMap::<u32, usize>::new();
            for token in tokens {
                *term_frequencies.entry(token).or_default() += 1;
            }
            for (token, term_frequency) in term_frequencies {
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
            document_count: search_infos.len(),
        }
    }

    fn top_matches(&self, query: &str, limit: usize) -> Vec<ToolSearchDocumentId> {
        if limit == 0 || self.document_count == 0 {
            return Vec::new();
        }

        let tokenizer = ToolSearchTokenizer;
        let mut scores = HashMap::<ToolSearchDocumentId, f32>::new();
        for token in tokenizer.tokenize(query) {
            let token = <u32 as TokenEmbedder>::embed(&token);
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

        let mut best = BinaryHeap::<Reverse<RankedToolSearchDocument>>::with_capacity(limit);
        for (id, score) in scores {
            let candidate = RankedToolSearchDocument { id, score };
            if best.len() < limit {
                best.push(Reverse(candidate));
            } else if best.peek().is_some_and(|worst| candidate > worst.0) {
                best.pop();
                best.push(Reverse(candidate));
            }
        }

        let mut best = best
            .into_iter()
            .map(|candidate| candidate.0)
            .collect::<Vec<_>>();
        best.sort_unstable_by(|left, right| right.cmp(left));
        best.into_iter().map(|candidate| candidate.id).collect()
    }
}

struct ToolSearchNameIndex {
    entry_names: HashSet<String>,
    output_names: HashMap<String, HashSet<String>>,
}

impl ToolSearchNameIndex {
    fn new(search_info: &ToolSearchInfo) -> Self {
        let entry_names = search_info
            .entry
            .tool_names
            .iter()
            .map(|name| normalize_tool_search_query(name))
            .collect();
        let mut output_names = HashMap::<String, HashSet<String>>::new();
        match &search_info.entry.output {
            LoadableToolSpec::Function(tool) => {
                output_names
                    .entry(normalize_tool_search_query(&tool.name))
                    .or_default()
                    .insert(tool.name.clone());
            }
            LoadableToolSpec::Namespace(namespace) => {
                for tool in &namespace.tools {
                    let ResponsesApiNamespaceTool::Function(tool) = tool;
                    output_names
                        .entry(normalize_tool_search_query(&tool.name))
                        .or_default()
                        .insert(tool.name.clone());
                }
            }
        }
        Self {
            entry_names,
            output_names,
        }
    }

    fn has_entry_name(&self, normalized_query: &str) -> bool {
        self.entry_names.contains(normalized_query)
    }

    fn output_names_for(&self, normalized_query: &str) -> Option<&HashSet<String>> {
        self.output_names.get(normalized_query)
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
}

struct ToolSearchCacheEntry {
    key: ToolSearchQueryKey,
    result: Arc<ToolSearchResult>,
}

#[derive(Clone, Debug, PartialEq)]
struct ToolSearchResult {
    tools: Vec<LoadableToolSpec>,
    omitted_result_count: usize,
    encoded_tools_len: usize,
}

impl Default for ToolSearchResult {
    fn default() -> Self {
        Self {
            tools: Vec::new(),
            omitted_result_count: 0,
            encoded_tools_len: 2,
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
        let name_indexes = search_infos.iter().map(ToolSearchNameIndex::new).collect();
        let has_unnamed_tools = search_infos
            .iter()
            .any(|search_info| search_info.source_info.is_none());
        let search_source_infos = search_infos
            .iter()
            .filter_map(|search_info| search_info.source_info.clone())
            .collect::<Vec<_>>();
        let spec = create_tool_search_tool(
            &search_source_infos,
            has_unnamed_tools,
            TOOL_SEARCH_DEFAULT_LIMIT,
        );
        let search_index = ToolSearchIndex::new(&search_infos);

        Self {
            search_infos,
            name_indexes,
            spec,
            search_index,
            result_cache: Mutex::new(VecDeque::new()),
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
            payload,
            step_context,
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

        let limit = args.limit.unwrap_or(TOOL_SEARCH_DEFAULT_LIMIT);
        let result = self.search(&args.query, limit)?;
        turn.activate_deferred_tools(result.tools.iter().flat_map(loadable_tool_names));

        Ok(boxed_tool_output(ToolSearchOutput {
            tools: result.tools.clone(),
            omitted_result_count: result.omitted_result_count,
        }))
    }
}

fn loadable_tool_names(spec: &LoadableToolSpec) -> Vec<ToolName> {
    spec.callable_tool_names()
}

impl CoreToolRuntime for ToolSearchHandler {}

impl ToolSearchHandler {
    fn search(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Arc<ToolSearchResult>, FunctionCallError> {
        let key = validate_tool_search_query(query, limit)?;
        if self.search_infos.is_empty() {
            return Ok(Arc::new(ToolSearchResult::default()));
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

        let exact_matches = self
            .name_indexes
            .iter()
            .enumerate()
            .filter_map(|(index, names)| {
                names
                    .has_entry_name(&key.query)
                    .then_some(ToolSearchDocumentId(index))
            })
            .collect::<Vec<_>>();
        let exact_match_count = exact_matches.len();
        let candidate_limit = tool_search_candidate_limit(limit, self.search_infos.len());
        let candidates = self.search_index.top_matches(&key.query, candidate_limit);
        let candidate_count = candidates.len();
        let trace_enabled = tracing::enabled!(tracing::Level::TRACE);
        let candidate_source_count = trace_enabled.then(|| {
            tool_search_info_diversity_count(
                candidates.iter().map(|id| id.info(&self.search_infos)),
            )
        });
        let results =
            promote_exact_name_matches(&self.search_infos, exact_matches, candidates, limit);
        let result_count = results.len();
        let result_source_count = trace_enabled.then(|| {
            tool_search_info_diversity_count(results.iter().map(|id| id.info(&self.search_infos)))
        });
        let result = Arc::new(self.search_output_tools(results, Some(&key.query))?);
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
        self.cache_search_result(key, &result);
        Ok(result)
    }

    fn search_output_tools(
        &self,
        results: impl IntoIterator<Item = ToolSearchDocumentId>,
        exact_query: Option<&str>,
    ) -> Result<ToolSearchResult, FunctionCallError> {
        let mut retained = ToolSearchResultBuilder::new();
        let mut omitted_result_count = 0usize;
        for result_id in results {
            let result = &result_id.info(&self.search_infos).entry;
            let exact_output_names = exact_query.and_then(|query| {
                result_id
                    .name_index(&self.name_indexes)
                    .output_names_for(query)
            });
            if retained.try_push(&result.output) {
            } else if let Some(recovery) = exact_output_names
                .and_then(|names| compact_exact_match_recovery(&result.output, names))
            {
                if retained.try_push(&recovery) {
                } else {
                    omitted_result_count = omitted_result_count.saturating_add(1);
                }
            } else {
                omitted_result_count = omitted_result_count.saturating_add(1);
            }
        }
        let (tools, encoded_tools_len) = retained.finish();
        Ok(ToolSearchResult {
            tools,
            omitted_result_count,
            encoded_tools_len,
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
            "Compact exact-match definition for `{qualified_name}`; verbose schema details were removed to fit the tool-search response budget."
        ),
        strict: tool.strict,
        defer_loading: Some(true),
        parameters: compact_recovery_schema(&tool.parameters),
        output_schema: tool.output_schema.clone(),
    }
}

fn compact_recovery_schema(schema: &codex_tools::JsonSchema) -> codex_tools::JsonSchema {
    let mut compact = schema.clone();
    strip_schema_descriptions(&mut compact);
    compact
}

fn strip_schema_descriptions(schema: &mut codex_tools::JsonSchema) {
    schema.description = None;
    if let Some(items) = schema.items.as_mut() {
        strip_schema_descriptions(items);
    }
    if let Some(properties) = schema.properties.as_mut() {
        for property in properties.values_mut() {
            strip_schema_descriptions(property);
        }
    }
    if let Some(codex_tools::AdditionalProperties::Schema(additional)) =
        schema.additional_properties.as_mut()
    {
        strip_schema_descriptions(additional);
    }
    for variants in [
        schema.any_of.as_mut(),
        schema.one_of.as_mut(),
        schema.all_of.as_mut(),
    ]
    .into_iter()
    .flatten()
    {
        for variant in variants {
            strip_schema_descriptions(variant);
        }
    }
    for definitions in [schema.defs.as_mut(), schema.definitions.as_mut()]
        .into_iter()
        .flatten()
    {
        for definition in definitions.values_mut() {
            strip_schema_descriptions(definition);
        }
    }
}

fn normalize_tool_search_query(query: &str) -> String {
    query
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
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

    let query = normalize_tool_search_query(query);
    if query.is_empty() {
        return Err(FunctionCallError::RespondToModel(
            "query must not be empty".to_string(),
        ));
    }

    Ok(ToolSearchQueryKey { query, limit })
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
    key.query
        .len()
        .checked_add(result.encoded_tools_len)
        .is_some_and(|encoded_len| encoded_len <= MAX_TOOL_SEARCH_CACHE_ENTRY_BYTES)
}

#[cfg(test)]
fn diversify_search_results(results: Vec<&ToolSearchInfo>, limit: usize) -> Vec<&ToolSearchInfo> {
    if results.len() <= limit {
        return results;
    }

    let mut remaining = results;
    let mut diversified = Vec::with_capacity(limit);
    let mut seen_this_pass = HashSet::new();

    while !remaining.is_empty() && diversified.len() < limit {
        let mut deferred = Vec::new();
        let mut added_this_pass = false;

        for result in remaining {
            if diversified.len() >= limit {
                break;
            }
            if seen_this_pass.insert(tool_search_info_diversity_key(result)) {
                diversified.push(result);
                added_this_pass = true;
            } else {
                deferred.push(result);
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

fn promote_exact_name_matches(
    search_infos: &[ToolSearchInfo],
    exact_matches: Vec<ToolSearchDocumentId>,
    ranked_results: Vec<ToolSearchDocumentId>,
    limit: usize,
) -> Vec<ToolSearchDocumentId> {
    let mut results = Vec::with_capacity(exact_matches.len().saturating_add(ranked_results.len()));
    let mut seen = HashSet::<ToolSearchDocumentId>::new();

    for result in exact_matches.into_iter().chain(ranked_results) {
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
    use crate::tools::handlers::DynamicToolHandler;
    use crate::tools::handlers::McpHandler;
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

    #[test]
    fn tool_search_tokenizer_splits_unicode_words_and_normalizes_case() {
        assert_eq!(
            ToolSearchTokenizer.tokenize("Launch CALENDAR-events for José"),
            vec!["launch", "calendar", "events", "for", "josé"]
        );
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
        match &mut changed_output_infos[0].entry.output {
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
        assert!(names.has_entry_name("find_event"));
        assert_eq!(
            names.output_names_for("find_event"),
            Some(&HashSet::from(["  FIND_EVENT  ".to_string()]))
        );
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
        let LoadableToolSpec::Namespace(namespace) = &mut search_info.entry.output else {
            panic!("test search info should be a namespace");
        };
        namespace.description = "x".repeat(MAX_TOOL_SEARCH_RESULT_BYTES);
        let [ResponsesApiNamespaceTool::Function(source_tool)] = namespace.tools.as_mut_slice()
        else {
            panic!("test search info should contain one function");
        };
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
                    ..codex_tools::JsonSchema::integer(Some("Event title".to_string()))
                },
            )]),
            Some(vec!["title".to_string()]),
            Some(false.into()),
        );
        let expected_parameters = compact_recovery_schema(&source_tool.parameters);
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
        let output_schema =
            serde_json::to_value(codex_tools::JsonSchema::string(Some("result".to_string())))
                .expect("output schema should serialize");
        let source = ResponsesApiTool {
            name: "strict_tool".to_string(),
            description: "verbose".to_string(),
            strict: true,
            defer_loading: Some(true),
            parameters: codex_tools::JsonSchema::object(
                Default::default(),
                Some(Vec::new()),
                Some(false.into()),
            ),
            output_schema: Some(output_schema.clone()),
        };

        let compact = compact_recovery_tool(&source, "strict_tool");

        assert!(compact.strict);
        assert_eq!(compact.output_schema, Some(output_schema));
        assert_eq!(compact.parameters.required, Some(Vec::new()));
        assert_eq!(compact.parameters.additional_properties, Some(false.into()));
    }

    #[test]
    fn audit_tool_search_contract_omits_exact_match_when_safe_schema_exceeds_budget() {
        let mut search_info = search_info("calendar", None, "calendar", "create_event");
        let LoadableToolSpec::Namespace(namespace) = &mut search_info.entry.output else {
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
            .expect("oversized exact-name search should return an explicit omission");

        assert!(tools.tools.is_empty());
        assert_eq!(tools.omitted_result_count, 1);
    }

    #[test]
    fn search_skips_lower_ranked_definitions_that_exceed_the_result_budget() {
        let mut first = search_info("first", None, "first", "run");
        let mut second = search_info("second", None, "second", "run");
        for search_info in [&mut first, &mut second] {
            let LoadableToolSpec::Namespace(namespace) = &mut search_info.entry.output else {
                panic!("test search info should be a namespace");
            };
            namespace.description = "x".repeat(MAX_TOOL_SEARCH_RESULT_BYTES / 2);
        }
        let handler = ToolSearchHandler::new(vec![first, second]);
        let results = [ToolSearchDocumentId(0), ToolSearchDocumentId(1)];

        let tools = handler
            .search_output_tools(results, None)
            .expect("search results should serialize within the budget");

        assert_eq!(tools.tools.len(), 1);
        assert_eq!(tools.omitted_result_count, 1);
        assert!(
            serde_json::to_vec(&tools.tools)
                .expect("bounded search result should serialize")
                .len()
                <= MAX_TOOL_SEARCH_RESULT_BYTES
        );
    }

    #[test]
    fn search_keeps_later_matches_after_an_oversized_definition() {
        let mut oversized = search_info("oversized", None, "oversized", "run");
        let later = search_info("later", None, "later", "run");
        let LoadableToolSpec::Namespace(namespace) = &mut oversized.entry.output else {
            panic!("test search info should be a namespace");
        };
        namespace.description = "x".repeat(MAX_TOOL_SEARCH_RESULT_BYTES);
        let expected = later.entry.output.clone();
        let handler = ToolSearchHandler::new(vec![oversized, later]);
        let results = [ToolSearchDocumentId(0), ToolSearchDocumentId(1)];

        let tools = handler
            .search_output_tools(results, None)
            .expect("later search result should fit within the budget");

        assert_eq!(tools.tools, vec![expected]);
        assert_eq!(tools.omitted_result_count, 1);
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
            .search_output_tools(results, None)
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
            &calendar_create,
            &calendar_list,
            &calendar_delete,
            &docs_search,
            &calendar_update,
        ];
        let diversified = diversify_search_results(results, 3);
        let diversified_names = diversified
            .iter()
            .map(|search_info| search_info.entry.search_text.as_str())
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

        let diversified =
            diversify_search_results(vec![&alpha_first, &alpha_second, &beta_first], 2);
        let diversified_names = diversified
            .iter()
            .map(|search_info| search_info.entry.search_text.as_str())
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
            vec![ToolSearchDocumentId(0)],
            vec![
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
                output: LoadableToolSpec::Namespace(ResponsesApiNamespace {
                    name: format!("mcp__{namespace_name}"),
                    description: format!("Tools in the {namespace_name} namespace."),
                    tools: vec![ResponsesApiNamespaceTool::Function(ResponsesApiTool {
                        name: tool_name.to_string(),
                        description: format!("{tool_name} tool"),
                        strict: false,
                        defer_loading: Some(true),
                        parameters: codex_tools::JsonSchema::object(
                            Default::default(),
                            /*required*/ None,
                            Some(false.into()),
                        ),
                        output_schema: None,
                    })],
                }),
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
