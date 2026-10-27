//! Shared raw tool cache for the host-owned Codex Apps MCP server.
//!
//! Cache entries are process-local live state scoped by the active Codex auth
//! key. Disk is best-effort cold-start persistence; entries do not reread disk
//! after creation.

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::sync::Once;
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
    /// External header credentials do not establish an authenticated account.
    #[serde(skip)]
    pub(crate) cacheable: bool,
    pub(crate) account_id: Option<String>,
    pub(crate) chatgpt_user_id: Option<String>,
    pub(crate) is_workspace_account: bool,
    pub(crate) chatgpt_base_url: String,
    pub(crate) mcp_endpoint: String,
    pub(crate) product_sku: String,
}

/// Builds the Codex Apps cache key from auth and catalog-routing inputs.
pub fn codex_apps_tools_cache_key(
    auth: Option<&CodexAuth>,
    chatgpt_base_url: &str,
    apps_mcp_product_sku: Option<&str>,
) -> CodexAppsToolsCacheKey {
    CodexAppsToolsCacheKey {
        cacheable: auth.is_some_and(|auth| !matches!(auth, CodexAuth::Headers(_))),
        account_id: auth.and_then(CodexAuth::get_account_id),
        chatgpt_user_id: auth.and_then(CodexAuth::get_chatgpt_user_id),
        is_workspace_account: auth.is_some_and(CodexAuth::is_workspace_account),
        chatgpt_base_url: canonicalize_chatgpt_base_url(chatgpt_base_url),
        mcp_endpoint: crate::mcp::codex_apps_mcp_url_for_base_url(chatgpt_base_url),
        product_sku: apps_mcp_product_sku
            .unwrap_or(DEFAULT_CODEX_APPS_MCP_PRODUCT_SKU)
            .to_string(),
    }
}

impl CodexAppsToolsCacheKey {
    pub(crate) fn for_server(mut self, config: &codex_config::McpServerConfig) -> Self {
        if let codex_config::McpServerTransportConfig::StreamableHttp {
            url, http_headers, ..
        } = &config.transport
        {
            self.mcp_endpoint = url::Url::parse(url)
                .map(|url| url.to_string())
                .unwrap_or_else(|_| url.clone());
            self.product_sku = http_headers
                .as_ref()
                .and_then(|headers| {
                    headers
                        .iter()
                        .find(|(name, _)| name.eq_ignore_ascii_case("X-OpenAI-Product-Sku"))
                })
                .map(|(_, value)| value.clone())
                .unwrap_or_default();
        }
        self
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
/// startup data. A successful live fetch publishes tools, timestamp, and content
/// revision atomically, including when its awaiting caller has been cancelled.
#[derive(Clone)]
pub struct CodexAppsToolsSnapshot {
    tools: Vec<ToolInfo>,
    published_at: Option<Instant>,
    content_revision: u64,
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
    pub(crate) fn current_snapshot(&self) -> Option<Arc<CodexAppsToolsSnapshot>> {
        self.entry.current_snapshot.load_full()
    }

    pub(crate) fn content_revision(&self) -> u64 {
        self.current_snapshot()
            .map_or(0, |snapshot| snapshot.content_revision)
    }

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

    #[expect(
        clippy::expect_used,
        reason = "A panicked publication worker may have partially committed a generation; do not return fabricated success"
    )]
    pub(crate) async fn publish_if_newest_accepted(
        &self,
        ticket: CodexAppsToolsFetchTicket,
        server_info: &McpServerInfo,
        tools: Vec<ToolInfo>,
    ) -> Vec<ToolInfo> {
        let context = self.clone();
        let server_info = server_info.clone();
        // Keep acceptance, live publication, and disk ordering in one owned job.
        // Dropping the request cannot detach persistence from its generation.
        tokio::task::spawn_blocking(move || {
            context.publish_if_newest_accepted_blocking(ticket, &server_info, tools)
        })
        .await
        .expect("Codex Apps cache publication worker failed")
    }

    fn publish_if_newest_accepted_blocking(
        &self,
        ticket: CodexAppsToolsFetchTicket,
        server_info: &McpServerInfo,
        mut tools: Vec<ToolInfo>,
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
        sort_raw_tools(&mut tools);
        let previous = self.current_snapshot();
        let content_revision = previous
            .as_ref()
            .map_or(0, |snapshot| snapshot.content_revision)
            + u64::from(
                previous
                    .as_ref()
                    .is_none_or(|snapshot| snapshot.tools != tools),
            );
        self.entry
            .current_snapshot
            .store(Some(Arc::new(CodexAppsToolsSnapshot {
                tools: tools.clone(),
                published_at: Some(Instant::now()),
                content_revision,
            })));
        persist_codex_apps_cache(self, server_info, &tools);
        emit_duration(
            MCP_TOOLS_CACHE_PUBLISH_DURATION_METRIC,
            publish_start.elapsed(),
            &[("source", ticket.source.as_str()), ("result", "published")],
        );
        tools
    }

}

impl CodexAppsToolsCache {
    /// Returns the process-wide live Codex Apps tools registry.
    pub fn shared() -> Self {
        SHARED_CODEX_APPS_TOOLS_CACHE.clone()
    }

    /// Returns the latest shared Codex Apps tools without starting or refreshing
    /// an MCP connection. A new cache entry may be seeded from disk.
    pub async fn current_tools(
        &self,
        codex_home: PathBuf,
        auth_key: CodexAppsToolsCacheKey,
    ) -> Option<Vec<ToolInfo>> {
        if !auth_key.cacheable {
            return None;
        }
        self.context(codex_home, auth_key).await.current_tools()
    }

    /// Returns the current raw tools and their live-publication state without
    /// starting or refreshing an MCP connection.
    pub async fn current_snapshot(
        &self,
        codex_home: PathBuf,
        auth_key: CodexAppsToolsCacheKey,
    ) -> Option<Arc<CodexAppsToolsSnapshot>> {
        if !auth_key.cacheable {
            return None;
        }
        self.context(codex_home, auth_key)
            .await
            .entry
            .current_snapshot
            .load_full()
    }

    #[expect(
        clippy::expect_used,
        reason = "A panicked initialization worker cannot provide the shared cache identity required by callers"
    )]
    pub(crate) async fn context(
        &self,
        codex_home: PathBuf,
        auth_key: CodexAppsToolsCacheKey,
    ) -> CodexAppsToolsCacheContext {
        let cache = self.clone();
        // Cold disk seeding and registry acquisition both belong on a worker,
        // but unrelated warm entries must not wait behind that disk I/O.
        tokio::task::spawn_blocking(move || cache.context_blocking(codex_home, auth_key))
            .await
            .expect("Codex Apps cache initialization worker failed")
    }

    fn context_blocking(
        &self,
        codex_home: PathBuf,
        auth_key: CodexAppsToolsCacheKey,
    ) -> CodexAppsToolsCacheContext {
        self.context_blocking_with_seed(
            codex_home,
            auth_key,
            load_cached_codex_apps_tools_for_identity,
        )
    }

    fn context_blocking_with_seed(
        &self,
        codex_home: PathBuf,
        auth_key: CodexAppsToolsCacheKey,
        load: impl FnOnce(&CodexAppsToolsCacheIdentity) -> Option<Vec<ToolInfo>>,
    ) -> CodexAppsToolsCacheContext {
        let identity = CodexAppsToolsCacheIdentity {
            codex_home,
            auth_key,
        };
        if !identity.auth_key.cacheable {
            // Keep a connection's live discovery, but never share or seed an
            // ambiguous identity across clients or processes.
            return CodexAppsToolsCacheContext {
                entry: Arc::new(CodexAppsToolsCacheEntry::new(identity)),
            };
        }
        let entry = lock_unpoisoned(&self.entries).context_entry(identity);
        entry.seed_with(|| load(&entry.identity));
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
    disk_seed: Once,
    current_snapshot: ArcSwapOption<CodexAppsToolsSnapshot>,
    next_fetch_generation: AtomicU64,
    last_accepted_generation: Mutex<u64>,
    persisted: Mutex<PersistedCodexAppsCache>,
}

#[derive(Default)]
struct PersistedCodexAppsCache {
    tools_revision: Option<u64>,
    server_info: Option<McpServerInfo>,
}

impl CodexAppsToolsCacheEntry {
    fn new(identity: CodexAppsToolsCacheIdentity) -> Self {
        Self {
            identity,
            disk_seed: Once::new(),
            current_snapshot: ArcSwapOption::empty(),
            next_fetch_generation: AtomicU64::new(0),
            last_accepted_generation: Mutex::new(0),
            persisted: Mutex::new(PersistedCodexAppsCache::default()),
        }
    }

    fn seed_with(&self, load: impl FnOnce() -> Option<Vec<ToolInfo>>) {
        self.disk_seed.call_once(|| {
            // Only same-identity cold callers wait for this read. The registry
            // has already released its lock and the retained Arc prevents pruning.
            let tools = load();
            let generation = lock_unpoisoned(&self.last_accepted_generation);
            // Publication shares this short critical section: an old disk read
            // must never overwrite a live catalog that won while it was loading.
            if *generation == 0 && self.current_snapshot.load_full().is_none() {
                self.current_snapshot.store(tools.map(|tools| {
                    Arc::new(CodexAppsToolsSnapshot {
                        tools,
                        published_at: None,
                        content_revision: 0,
                    })
                }));
            }
        });
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


pub(crate) fn load_startup_cached_codex_apps_server_info(
    cache_context: &CodexAppsToolsCacheContext,
) -> Option<McpServerInfo> {
    load_cached_codex_apps_server_info(cache_context)
}


#[instrument(level = "trace", skip_all)]
fn load_cached_codex_apps_tools_for_identity(
    identity: &CodexAppsToolsCacheIdentity,
) -> Option<Vec<ToolInfo>> {
    let cache_path = identity.cache_path_in(CODEX_APPS_TOOLS_CACHE_DIR);
    let bytes = std::fs::read(cache_path).ok()?;
    let mut cache: CodexAppsToolsDiskCache = serde_json::from_slice(&bytes).ok()?;
    sort_raw_tools(&mut cache.tools);
    (cache.schema_version == CODEX_APPS_TOOLS_CACHE_SCHEMA_VERSION).then_some(cache.tools)
}

fn sort_raw_tools(tools: &mut [ToolInfo]) {
    // Match model-facing identity without rewriting names or discarding metadata.
    // Stable sorting preserves first-wins handling of duplicate raw identities.
    tools.sort_by(|left, right| {
        left.server_name
            .cmp(&right.server_name)
            .then_with(|| left.callable_namespace.cmp(&right.callable_namespace))
            .then_with(|| left.connector_id.cmp(&right.connector_id))
            .then_with(|| left.callable_name.cmp(&right.callable_name))
            .then_with(|| left.tool.name.cmp(&right.tool.name))
    });
}

fn write_cached_codex_apps_tools(
    cache_context: &CodexAppsToolsCacheContext,
    tools: &[ToolInfo],
) -> anyhow::Result<()> {
    let cache_path = cache_context.tools_cache_path();
    let bytes = serde_json::to_vec_pretty(&CodexAppsToolsDiskCache {
        schema_version: CODEX_APPS_TOOLS_CACHE_SCHEMA_VERSION,
        tools,
    })
    .context("failed to serialize Codex Apps tools cache")?;
    write_codex_apps_cache_file(&cache_path, "tools", bytes)
}

#[instrument(level = "trace", skip_all)]
fn load_cached_codex_apps_server_info(
    cache_context: &CodexAppsToolsCacheContext,
) -> Option<McpServerInfo> {
    if !cache_context.entry.identity.auth_key.cacheable {
        return None;
    }
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
        server_info,
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
    let parent = cache_path.parent().context("cache path has no parent")?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(&bytes)?;
    temporary.persist(cache_path).with_context(|| {
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
    if !cache_context.entry.identity.auth_key.cacheable {
        return;
    }
    let cache_write_start = Instant::now();
    let mut persisted = lock_unpoisoned(&cache_context.entry.persisted);
    let revision = cache_context.content_revision();
    let tools_result = if persisted.tools_revision == Some(revision) {
        Ok(())
    } else {
        write_cached_codex_apps_tools(cache_context, tools).inspect(|()| {
            persisted.tools_revision = Some(revision);
        })
    };
    if let Err(err) = &tools_result {
        tracing::warn!("failed to write Codex Apps tools cache: {err:#}");
    }
    let server_info_result = if persisted.server_info.as_ref() == Some(server_info) {
        Ok(())
    } else {
        write_cached_codex_apps_server_info(cache_context, server_info).inspect(|()| {
            persisted.server_info = Some(server_info.clone());
        })
    };
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
struct CodexAppsToolsDiskCache<T = Vec<ToolInfo>> {
    schema_version: u8,
    tools: T,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CodexAppsServerInfoDiskCache<T = McpServerInfo> {
    schema_version: u8,
    server_info: T,
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

