use crate::function_tool::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::ToolSearchOutput;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::tool_search_spec::create_tool_search_tool;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use bm25::Document;
use bm25::Language;
use bm25::SearchEngine;
use bm25::SearchEngineBuilder;
use codex_tools::LoadableToolSpec;
use codex_tools::TOOL_SEARCH_DEFAULT_LIMIT;
use codex_tools::TOOL_SEARCH_TOOL_NAME;
use codex_tools::ToolName;
use codex_tools::ToolSearchEntry;
use codex_tools::ToolSearchInfo;
use codex_tools::ToolSpec;
use codex_tools::coalesce_loadable_tool_specs;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::io::Write;
use std::sync::Arc;
use std::sync::Mutex;
use tracing::instrument;

const MAX_TOOL_SEARCH_HANDLER_CACHE: usize = 4;
const MAX_TOOL_SEARCH_RESULT_CACHE: usize = 32;
const MAX_TOOL_SEARCH_CACHE_ENTRY_BYTES: usize = 256 * 1024;
const MAX_TOOL_SEARCH_QUERY_BYTES: usize = 4 * 1024;
const MAX_TOOL_SEARCH_LIMIT: usize = 64;
const TOOL_SEARCH_CANDIDATE_MULTIPLIER: usize = 3;

pub struct ToolSearchHandler {
    search_infos: Vec<ToolSearchInfo>,
    spec: ToolSpec,
    search_engine: SearchEngine<usize>,
    result_cache: Mutex<VecDeque<ToolSearchCacheEntry>>,
}

#[derive(Default)]
pub(crate) struct ToolSearchHandlerCache {
    cached: Mutex<VecDeque<Arc<ToolSearchHandler>>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ToolSearchQueryKey {
    query: String,
    limit: usize,
}

#[derive(Clone)]
struct ToolSearchCacheEntry {
    key: ToolSearchQueryKey,
    tools: Vec<LoadableToolSpec>,
}

struct CacheSizeWriter {
    remaining: usize,
}

impl CacheSizeWriter {
    fn new(limit: usize) -> Self {
        Self { remaining: limit }
    }
}

impl Write for CacheSizeWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if buf.len() > self.remaining {
            return Err(std::io::Error::new(
                std::io::ErrorKind::FileTooLarge,
                "tool search cache entry exceeds its byte budget",
            ));
        }
        self.remaining -= buf.len();
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl ToolSearchHandlerCache {
    #[instrument(level = "trace", skip_all, fields(search_info_count = search_infos.len()))]
    pub(crate) fn get_or_build(&self, search_infos: Vec<ToolSearchInfo>) -> Arc<ToolSearchHandler> {
        {
            let mut cached = self.cached();
            if let Some(index) = cached
                .iter()
                .position(|handler| handler.search_infos == search_infos)
                && let Some(handler) = cached.remove(index)
            {
                cached.push_back(Arc::clone(&handler));
                tracing::trace!(
                    cache_hit = true,
                    cached_inventory_count = cached.len(),
                    "tool search handler cache resolved"
                );
                return handler;
            }
        }

        let handler = Arc::new(ToolSearchHandler::new(search_infos));
        let mut cached = self.cached();
        if let Some(index) = cached
            .iter()
            .position(|cached_handler| cached_handler.search_infos == handler.search_infos)
            && let Some(cached_handler) = cached.remove(index)
        {
            cached.push_back(Arc::clone(&cached_handler));
            tracing::trace!(
                cache_hit = true,
                cached_inventory_count = cached.len(),
                "tool search handler cache resolved after concurrent build"
            );
            return cached_handler;
        }

        cached.push_back(Arc::clone(&handler));
        let mut evicted_inventory_count = 0usize;
        while cached.len() > MAX_TOOL_SEARCH_HANDLER_CACHE {
            cached.pop_front();
            evicted_inventory_count += 1;
        }
        tracing::trace!(
            cache_hit = false,
            cached_inventory_count = cached.len(),
            evicted_inventory_count,
            "tool search handler cache resolved"
        );
        handler
    }

    fn cached(&self) -> std::sync::MutexGuard<'_, VecDeque<Arc<ToolSearchHandler>>> {
        match self.cached.lock() {
            Ok(cached) => cached,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

impl ToolSearchHandler {
    #[instrument(
        level = "trace",
        skip_all,
        fields(search_info_count = search_infos.len())
    )]
    pub(crate) fn new(search_infos: Vec<ToolSearchInfo>) -> Self {
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
        let documents: Vec<Document<usize>> = search_infos
            .iter()
            .map(|search_info| search_info.entry.search_text.clone())
            .enumerate()
            .map(|(idx, search_text)| Document::new(idx, search_text))
            .collect();
        let search_engine =
            SearchEngineBuilder::<usize>::with_documents(Language::English, documents).build();

        Self {
            search_infos,
            spec,
            search_engine,
            result_cache: Mutex::new(VecDeque::new()),
        }
    }
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
        let ToolInvocation { payload, .. } = invocation;

        let args = match payload {
            ToolPayload::ToolSearch { arguments } => arguments,
            _ => {
                return Err(FunctionCallError::Fatal(format!(
                    "{TOOL_SEARCH_TOOL_NAME} handler received unsupported payload"
                )));
            }
        };

        let limit = args.limit.unwrap_or(TOOL_SEARCH_DEFAULT_LIMIT);
        let tools = self.search(&args.query, limit)?;

        Ok(boxed_tool_output(ToolSearchOutput { tools }))
    }
}

impl CoreToolRuntime for ToolSearchHandler {}

impl ToolSearchHandler {
    fn search(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<LoadableToolSpec>, FunctionCallError> {
        let key = validate_tool_search_query(query, limit)?;
        if self.search_infos.is_empty() {
            return Ok(Vec::new());
        }

        if let Some(tools) = self.cached_search_result(&key) {
            tracing::trace!(
                normalized_query_bytes = key.query.len(),
                effective_limit = limit,
                cache_hit = true,
                output_tool_count = tools.len(),
                output_source_count = loadable_tool_spec_diversity_count(&tools),
                "tool search completed"
            );
            return Ok(tools);
        }

        let candidate_limit = tool_search_candidate_limit(limit, self.search_infos.len());
        let mut ranked_results = self.search_engine.search(&key.query, candidate_limit);
        debug_assert!(ranked_results.len() <= candidate_limit);
        ranked_results.sort_by(|left, right| {
            right
                .score
                .total_cmp(&left.score)
                .then_with(|| left.document.id.cmp(&right.document.id))
        });
        let candidates = ranked_results
            .into_iter()
            .take(candidate_limit)
            .map(|result| result.document.id)
            .filter_map(|id| self.search_infos.get(id))
            .collect::<Vec<_>>();
        let candidate_count = candidates.len();
        let candidate_source_count = tool_search_info_diversity_count(candidates.iter().copied());
        let results = diversify_search_results(candidates, limit);
        let result_count = results.len();
        let result_source_count = tool_search_info_diversity_count(results.iter().copied());
        let tools =
            self.search_output_tools(results.iter().map(|search_info| &search_info.entry))?;
        tracing::trace!(
            normalized_query_bytes = key.query.len(),
            effective_limit = limit,
            cache_hit = false,
            candidate_limit,
            candidate_count,
            candidate_source_count,
            result_count,
            result_source_count,
            output_tool_count = tools.len(),
            output_source_count = loadable_tool_spec_diversity_count(&tools),
            "tool search completed"
        );
        self.cache_search_result(key, &tools);
        Ok(tools)
    }

    fn search_output_tools<'a>(
        &self,
        results: impl IntoIterator<Item = &'a ToolSearchEntry>,
    ) -> Result<Vec<LoadableToolSpec>, FunctionCallError> {
        Ok(coalesce_loadable_tool_specs(
            results.into_iter().map(|entry| entry.output.clone()),
        ))
    }

    fn cached_search_result(&self, key: &ToolSearchQueryKey) -> Option<Vec<LoadableToolSpec>> {
        let mut cache = self.result_cache();
        let index = cache.iter().position(|entry| &entry.key == key)?;
        let entry = cache.remove(index)?;
        let tools = entry.tools.clone();
        cache.push_back(entry);
        Some(tools)
    }

    fn cache_search_result(&self, key: ToolSearchQueryKey, tools: &[LoadableToolSpec]) {
        if !tool_search_cache_entry_fits_budget(&key, tools) {
            tracing::trace!(
                normalized_query_bytes = key.query.len(),
                output_tool_count = tools.len(),
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
            tools: tools.to_vec(),
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
    tools: &[LoadableToolSpec],
) -> bool {
    let Some(tool_budget) = MAX_TOOL_SEARCH_CACHE_ENTRY_BYTES.checked_sub(key.query.len()) else {
        return false;
    };
    let mut writer = CacheSizeWriter::new(tool_budget);
    serde_json::to_writer(&mut writer, tools).is_ok()
}

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
    use codex_tools::ResponsesApiNamespace;
    use codex_tools::ResponsesApiNamespaceTool;
    use codex_tools::ResponsesApiTool;
    use codex_tools::ToolSearchEntry;
    use codex_tools::ToolSearchSourceInfo;
    use pretty_assertions::assert_eq;
    use rmcp::model::Tool;
    use std::sync::Arc;

    #[test]
    fn cache_reuses_handler_for_identical_search_infos_and_rebuilds_for_changes() {
        let cache = ToolSearchHandlerCache::default();
        let search_infos = vec![
            McpHandler::new(tool_info("calendar", "create_event", "Create events"))
                .expect("MCP tool should convert")
                .search_info()
                .expect("MCP handler should return search info"),
        ];

        let first = cache.get_or_build(search_infos.clone());
        let second = cache.get_or_build(search_infos.clone());
        assert!(Arc::ptr_eq(&first, &second));

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
    fn cache_retains_four_inventory_entries_in_lru_order() {
        let cache = ToolSearchHandlerCache::default();
        let inventories = (0..5)
            .map(|idx| {
                vec![
                    McpHandler::new(tool_info(
                        "calendar",
                        &format!("tool_{idx}"),
                        "Calendar tool",
                    ))
                    .expect("MCP tool should convert")
                    .search_info()
                    .expect("MCP handler should return search info"),
                ]
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
        assert_eq!(cache.cached().len(), MAX_TOOL_SEARCH_HANDLER_CACHE);
    }

    #[test]
    fn search_reuses_normalized_query_results_and_keys_by_limit() {
        let search_infos = vec![
            McpHandler::new(tool_info("calendar", "create_event", "Create events"))
                .expect("MCP tool should convert")
                .search_info()
                .expect("MCP handler should return search info"),
        ];
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
        assert_eq!(limited, first);
        assert_eq!(handler.result_cache_len(), 2);
    }

    #[test]
    fn search_result_cache_is_bounded_and_lru() {
        let search_infos = vec![
            McpHandler::new(tool_info("calendar", "create_event", "Create events"))
                .expect("MCP tool should convert")
                .search_info()
                .expect("MCP handler should return search info"),
        ];
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
    fn search_bounds_the_ranked_candidate_window() {
        let search_infos = (0..20)
            .map(|idx| {
                search_info_with_source(
                    &format!("shared capability {idx}"),
                    &format!("source-{idx}"),
                    &format!("tool-{idx}"),
                )
            })
            .collect();
        let handler = ToolSearchHandler::new(search_infos);

        let tools = handler
            .search("shared capability", 1)
            .expect("bounded search should succeed");

        assert_eq!(tools.len(), 1);
    }

    #[test]
    fn search_skips_oversized_cache_entries_before_cloning_them() {
        let mut search_info = search_info("calendar", None, "calendar", "create_event");
        let LoadableToolSpec::Namespace(namespace) = &mut search_info.entry.output else {
            panic!("test search info should be a namespace");
        };
        namespace.description = "x".repeat(MAX_TOOL_SEARCH_CACHE_ENTRY_BYTES);
        let handler = ToolSearchHandler::new(vec![search_info]);

        let tools = handler
            .search("calendar", TOOL_SEARCH_DEFAULT_LIMIT)
            .expect("oversized result should still be returned");

        assert_eq!(tools.len(), 1);
        assert_eq!(handler.result_cache_len(), 0);
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
                McpHandler::new(tool.clone())
                    .expect("MCP tool should convert")
                    .search_info()
                    .expect("MCP handler should return search info")
            })
            .collect::<Vec<_>>();
        search_infos.extend(dynamic_tools.iter().map(|tool| {
            DynamicToolHandler::new_in_namespace(&dynamic_namespace, tool)
                .expect("dynamic tool should convert")
                .search_info()
                .expect("dynamic handler should return search info")
        }));
        let handler = ToolSearchHandler::new(search_infos);
        let results = [
            &handler.search_infos[0].entry,
            &handler.search_infos[2].entry,
            &handler.search_infos[1].entry,
        ];

        let tools = handler
            .search_output_tools(results)
            .expect("mixed search output should serialize");

        assert_eq!(
            tools,
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
