//! Shared raw tool cache for the host-owned Codex Apps MCP server.
//!
//! Cache entries are process-local live state scoped by the active Codex auth
//! key. Disk is best-effort cold-start persistence; entries do not reread disk
//! after creation.

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Instant;

use anyhow::Context;
use arc_swap::ArcSwapOption;
use codex_config::canonicalize_chatgpt_base_url;
use codex_login::CodexAuth;
use codex_protocol::mcp::McpServerInfo;
use codex_utils_string::sha1_hex;
use serde::Deserialize;
use serde::Serialize;
use tracing::instrument;

use crate::runtime::emit_duration;
use crate::tools::MCP_TOOLS_CACHE_WRITE_DURATION_METRIC;
use crate::tools::ToolInfo;

const MCP_TOOLS_CACHE_PUBLISH_DURATION_METRIC: &str = "codex.mcp.tools.cache_publish.duration_ms";
const CODEX_APPS_TOOLS_CACHE_CAPACITY: usize = 32;
pub(crate) const DEFAULT_CODEX_APPS_MCP_PRODUCT_SKU: &str = "codex";

/// Everything that identifies a Codex Apps catalog.
///
/// Debug bearer-token overrides bypass the shared cache, so shared entries only
/// need the CodexAuth-backed identity plus the effective endpoint and product.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CodexAppsToolsCacheKey {
    pub(crate) account_id: Option<String>,
    pub(crate) chatgpt_user_id: Option<String>,
    pub(crate) is_workspace_account: bool,
    pub(crate) chatgpt_base_url: String,
    pub(crate) product_sku: String,
}

/// Builds the Codex Apps cache key from auth and catalog-routing inputs.
pub fn codex_apps_tools_cache_key(
    auth: Option<&CodexAuth>,
    chatgpt_base_url: &str,
    apps_mcp_product_sku: Option<&str>,
) -> CodexAppsToolsCacheKey {
    CodexAppsToolsCacheKey {
        account_id: auth.and_then(CodexAuth::get_account_id),
        chatgpt_user_id: auth.and_then(CodexAuth::get_chatgpt_user_id),
        is_workspace_account: auth.is_some_and(CodexAuth::is_workspace_account),
        chatgpt_base_url: canonicalize_chatgpt_base_url(chatgpt_base_url),
        product_sku: apps_mcp_product_sku
            .unwrap_or(DEFAULT_CODEX_APPS_MCP_PRODUCT_SKU)
            .to_string(),
    }
}

/// Process-scoped registry for shared Codex Apps raw tool snapshots.
///
/// Two clients share an entry only when they would read the same Codex Apps
/// catalog. New entries may seed from disk; live entries read from memory only.
#[derive(Clone, Default)]
pub struct CodexAppsToolsCache {
    entries: Arc<Mutex<CodexAppsToolsCacheEntries>>,
}

#[derive(Default)]
struct CodexAppsToolsCacheEntries {
    by_identity: HashMap<CodexAppsToolsCacheIdentity, RetainedCodexAppsToolsCacheEntry>,
    next_access: u64,
}

struct RetainedCodexAppsToolsCacheEntry {
    entry: Arc<CodexAppsToolsCacheEntry>,
    last_access: u64,
}

impl CodexAppsToolsCacheEntries {
    fn context_entry(
        &mut self,
        identity: CodexAppsToolsCacheIdentity,
    ) -> Arc<CodexAppsToolsCacheEntry> {
        self.next_access = self.next_access.saturating_add(1);
        let last_access = self.next_access;
        let retained = self.by_identity.entry(identity.clone()).or_insert_with(|| {
            RetainedCodexAppsToolsCacheEntry {
                entry: Arc::new(CodexAppsToolsCacheEntry::new(identity)),
                last_access,
            }
        });
        retained.last_access = last_access;
        let entry = Arc::clone(&retained.entry);
        self.prune_idle_entries();
        entry
    }

    fn prune_idle_entries(&mut self) {
        while self.by_identity.len() > CODEX_APPS_TOOLS_CACHE_CAPACITY {
            let Some(identity) = self
                .by_identity
                .iter()
                .filter(|(_, retained)| Arc::strong_count(&retained.entry) == 1)
                .min_by_key(|(_, retained)| retained.last_access)
                .map(|(identity, _)| identity.clone())
            else {
                break;
            };
            self.by_identity.remove(&identity);
        }
    }
}

static SHARED_CODEX_APPS_TOOLS_CACHE: LazyLock<CodexAppsToolsCache> =
    LazyLock::new(CodexAppsToolsCache::default);

/// One authoritative Codex Apps tools snapshot.
///
/// Disk-seeded snapshots have no `published_at` value and are treated as
/// startup data. A successful live fetch publishes the tools and timestamp
/// atomically so derived callers cannot retain an older projection.
#[derive(Clone)]
pub struct CodexAppsToolsSnapshot {
    tools: Vec<ToolInfo>,
    published_at: Option<Instant>,
}

impl CodexAppsToolsSnapshot {
    pub fn tools(&self) -> &[ToolInfo] {
        &self.tools
    }

    pub fn codex_apps_ready(&self) -> bool {
        self.published_at.is_some()
    }

    pub fn is_fresh_for(&self, ttl: std::time::Duration) -> bool {
        self.published_at
            .is_some_and(|published_at| published_at.elapsed() < ttl)
    }
}

/// Handle to one shared Codex Apps tools cache entry.
///
/// The connection manager creates this from the auth key, then tool
/// reads and refreshes for that managed client use the same entry.
#[derive(Clone)]
pub(crate) struct CodexAppsToolsCacheContext {
    entry: Arc<CodexAppsToolsCacheEntry>,
}

impl CodexAppsToolsCacheContext {
    pub(crate) fn tools_cache_path(&self) -> PathBuf {
        self.entry
            .identity
            .cache_path_in(CODEX_APPS_TOOLS_CACHE_DIR)
    }

    pub(crate) fn server_info_cache_path(&self) -> PathBuf {
        self.entry
            .identity
            .cache_path_in(CODEX_APPS_SERVER_INFO_CACHE_DIR)
    }

    pub(crate) fn current_tools(&self) -> Option<Vec<ToolInfo>> {
        self.entry
            .current_snapshot
            .load_full()
            .map(|snapshot| snapshot.tools.clone())
    }

    pub(crate) fn has_current_tools(&self) -> bool {
        self.entry.current_snapshot.load_full().is_some()
    }

    pub(crate) fn begin_fetch(
        &self,
        source: CodexAppsToolsFetchSource,
    ) -> CodexAppsToolsFetchTicket {
        CodexAppsToolsFetchTicket {
            generation: self
                .entry
                .next_fetch_generation
                .fetch_add(1, Ordering::Relaxed)
                + 1,
            source,
        }
    }

    pub(crate) fn publish_if_newest_accepted(
        &self,
        ticket: CodexAppsToolsFetchTicket,
        server_info: &McpServerInfo,
        tools: Vec<ToolInfo>,
    ) -> Vec<ToolInfo> {
        let publish_start = Instant::now();
        let mut last_accepted_generation = lock_unpoisoned(&self.entry.last_accepted_generation);
        if ticket.generation <= *last_accepted_generation {
            emit_duration(
                MCP_TOOLS_CACHE_PUBLISH_DURATION_METRIC,
                publish_start.elapsed(),
                &[("source", ticket.source.as_str()), ("result", "stale")],
            );
            return self.current_tools().unwrap_or(tools);
        }

        *last_accepted_generation = ticket.generation;
        self.entry
            .current_snapshot
            .store(Some(Arc::new(CodexAppsToolsSnapshot {
                tools: tools.clone(),
                published_at: Some(Instant::now()),
            })));
        persist_codex_apps_cache(self, server_info, &tools);
        emit_duration(
            MCP_TOOLS_CACHE_PUBLISH_DURATION_METRIC,
            publish_start.elapsed(),
            &[("source", ticket.source.as_str()), ("result", "published")],
        );
        tools
    }

    #[cfg(test)]
    pub(crate) fn store_current_tools_for_test(&self, tools: Vec<ToolInfo>) {
        self.entry
            .current_snapshot
            .store(Some(Arc::new(CodexAppsToolsSnapshot {
                tools,
                published_at: None,
            })));
    }
}

impl CodexAppsToolsCache {
    /// Returns the process-wide live Codex Apps tools registry.
    pub fn shared() -> Self {
        SHARED_CODEX_APPS_TOOLS_CACHE.clone()
    }

    /// Returns the latest shared Codex Apps tools without starting or refreshing
    /// an MCP connection. A new cache entry may be seeded from disk.
    pub fn current_tools(
        &self,
        codex_home: PathBuf,
        auth_key: CodexAppsToolsCacheKey,
    ) -> Option<Vec<ToolInfo>> {
        self.context(codex_home, auth_key).current_tools()
    }

    /// Returns the current raw tools and their live-publication state without
    /// starting or refreshing an MCP connection.
    pub fn current_snapshot(
        &self,
        codex_home: PathBuf,
        auth_key: CodexAppsToolsCacheKey,
    ) -> Option<CodexAppsToolsSnapshot> {
        self.context(codex_home, auth_key)
            .entry
            .current_snapshot
            .load_full()
            .map(|snapshot| snapshot.as_ref().clone())
    }

    pub(crate) fn context(
        &self,
        codex_home: PathBuf,
        auth_key: CodexAppsToolsCacheKey,
    ) -> CodexAppsToolsCacheContext {
        let identity = CodexAppsToolsCacheIdentity {
            codex_home,
            auth_key,
        };
        let entry = lock_unpoisoned(&self.entries).context_entry(identity);
        CodexAppsToolsCacheContext { entry }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum CodexAppsToolsFetchSource {
    Startup,
    HardRefresh,
}

impl CodexAppsToolsFetchSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Startup => "startup",
            Self::HardRefresh => "hard_refresh",
        }
    }
}

pub(crate) struct CodexAppsToolsFetchTicket {
    generation: u64,
    source: CodexAppsToolsFetchSource,
}

struct CodexAppsToolsCacheEntry {
    identity: CodexAppsToolsCacheIdentity,
    current_snapshot: ArcSwapOption<CodexAppsToolsSnapshot>,
    next_fetch_generation: AtomicU64,
    last_accepted_generation: Mutex<u64>,
}

impl CodexAppsToolsCacheEntry {
    fn new(identity: CodexAppsToolsCacheIdentity) -> Self {
        let current_snapshot = load_cached_codex_apps_tools_for_identity(&identity).map(|tools| {
            Arc::new(CodexAppsToolsSnapshot {
                tools,
                published_at: None,
            })
        });
        Self {
            identity,
            current_snapshot: ArcSwapOption::from(current_snapshot),
            next_fetch_generation: AtomicU64::new(0),
            last_accepted_generation: Mutex::new(0),
        }
    }
}

/// Everything that decides whether two Codex Apps clients can share tools.
///
/// The catalog key says whose catalog and which endpoint/product we are
/// reading. `codex_home` keeps the persisted cache under the right home.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CodexAppsToolsCacheIdentity {
    codex_home: PathBuf,
    auth_key: CodexAppsToolsCacheKey,
}

impl CodexAppsToolsCacheIdentity {
    fn cache_path_in(&self, cache_dir: &str) -> PathBuf {
        // `codex_home` is already the parent directory. Keep it out of the
        // filename hash so non-UTF-8 Unix paths cannot collapse distinct auth
        // keys onto the same disk cache file.
        let identity_json = serde_json::to_string(&self.auth_key).unwrap_or_default();
        let identity_hash = sha1_hex(identity_json.as_bytes());
        self.codex_home
            .join(cache_dir)
            .join(format!("{identity_hash}.json"))
    }
}

#[cfg(test)]
fn write_cached_codex_apps_tools_for_test(
    cache_context: &CodexAppsToolsCacheContext,
    server_info: &McpServerInfo,
    tools: &[ToolInfo],
) {
    cache_context
        .entry
        .current_snapshot
        .store(Some(Arc::new(CodexAppsToolsSnapshot {
            tools: tools.to_vec(),
            published_at: Some(Instant::now()),
        })));
    persist_codex_apps_cache(cache_context, server_info, tools);
}

pub(crate) fn load_startup_cached_codex_apps_server_info(
    cache_context: &CodexAppsToolsCacheContext,
) -> Option<McpServerInfo> {
    load_cached_codex_apps_server_info(cache_context)
}

#[cfg(test)]
fn read_cached_codex_apps_tools(
    cache_context: &CodexAppsToolsCacheContext,
) -> Option<Vec<ToolInfo>> {
    load_cached_codex_apps_tools_for_identity(&cache_context.entry.identity)
}

#[instrument(level = "trace", skip_all)]
fn load_cached_codex_apps_tools_for_identity(
    identity: &CodexAppsToolsCacheIdentity,
) -> Option<Vec<ToolInfo>> {
    let cache_path = identity.cache_path_in(CODEX_APPS_TOOLS_CACHE_DIR);
    let bytes = std::fs::read(cache_path).ok()?;
    let cache: CodexAppsToolsDiskCache = serde_json::from_slice(&bytes).ok()?;
    (cache.schema_version == CODEX_APPS_TOOLS_CACHE_SCHEMA_VERSION).then_some(cache.tools)
}

fn write_cached_codex_apps_tools(
    cache_context: &CodexAppsToolsCacheContext,
    tools: &[ToolInfo],
) -> anyhow::Result<()> {
    let cache_path = cache_context.tools_cache_path();
    let bytes = serde_json::to_vec_pretty(&CodexAppsToolsDiskCache {
        schema_version: CODEX_APPS_TOOLS_CACHE_SCHEMA_VERSION,
        tools: tools.to_vec(),
    })
    .context("failed to serialize Codex Apps tools cache")?;
    write_codex_apps_cache_file(&cache_path, "tools", bytes)
}

#[instrument(level = "trace", skip_all)]
fn load_cached_codex_apps_server_info(
    cache_context: &CodexAppsToolsCacheContext,
) -> Option<McpServerInfo> {
    let bytes = std::fs::read(cache_context.server_info_cache_path()).ok()?;
    let cache: CodexAppsServerInfoDiskCache = serde_json::from_slice(&bytes).ok()?;
    (cache.schema_version == CODEX_APPS_SERVER_INFO_CACHE_SCHEMA_VERSION)
        .then_some(cache.server_info)
}

fn write_cached_codex_apps_server_info(
    cache_context: &CodexAppsToolsCacheContext,
    server_info: &McpServerInfo,
) -> anyhow::Result<()> {
    let cache_path = cache_context.server_info_cache_path();
    let bytes = serde_json::to_vec_pretty(&CodexAppsServerInfoDiskCache {
        schema_version: CODEX_APPS_SERVER_INFO_CACHE_SCHEMA_VERSION,
        server_info: server_info.clone(),
    })
    .context("failed to serialize Codex Apps server info cache")?;
    write_codex_apps_cache_file(&cache_path, "server info", bytes)
}

fn write_codex_apps_cache_file(
    cache_path: &Path,
    cache_name: &str,
    bytes: Vec<u8>,
) -> anyhow::Result<()> {
    if let Some(parent) = cache_path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create Codex Apps {cache_name} cache directory `{}`",
                parent.display()
            )
        })?;
    }
    std::fs::write(cache_path, bytes).with_context(|| {
        format!(
            "failed to write Codex Apps {cache_name} cache `{}`",
            cache_path.display()
        )
    })?;
    Ok(())
}

fn persist_codex_apps_cache(
    cache_context: &CodexAppsToolsCacheContext,
    server_info: &McpServerInfo,
    tools: &[ToolInfo],
) {
    let cache_write_start = Instant::now();
    let tools_result = write_cached_codex_apps_tools(cache_context, tools);
    if let Err(err) = &tools_result {
        tracing::warn!("failed to write Codex Apps tools cache: {err:#}");
    }
    let server_info_result = write_cached_codex_apps_server_info(cache_context, server_info);
    if let Err(err) = &server_info_result {
        tracing::warn!("failed to write Codex Apps server info cache: {err:#}");
    }
    let status = if tools_result.is_ok() && server_info_result.is_ok() {
        "success"
    } else {
        "failure"
    };
    emit_duration(
        MCP_TOOLS_CACHE_WRITE_DURATION_METRIC,
        cache_write_start.elapsed(),
        &[("status", status)],
    );
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CodexAppsToolsDiskCache {
    schema_version: u8,
    tools: Vec<ToolInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CodexAppsServerInfoDiskCache {
    schema_version: u8,
    server_info: McpServerInfo,
}

const CODEX_APPS_TOOLS_CACHE_DIR: &str = "cache/codex_apps_tools";
const CODEX_APPS_TOOLS_CACHE_SCHEMA_VERSION: u8 = 4;

const CODEX_APPS_SERVER_INFO_CACHE_DIR: &str = "cache/codex_apps_server_info";
const CODEX_APPS_SERVER_INFO_CACHE_SCHEMA_VERSION: u8 = 1;

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
#[path = "codex_apps_cache_tests.rs"]
mod tests;
