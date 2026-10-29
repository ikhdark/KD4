use super::REMOTE_CREATED_BY_ME_MARKETPLACE_NAME;
use super::REMOTE_GLOBAL_MARKETPLACE_NAME;
use super::REMOTE_WORKSPACE_MARKETPLACE_NAME;
use super::REMOTE_WORKSPACE_SHARED_WITH_ME_MARKETPLACE_NAME;
use super::REMOTE_WORKSPACE_SHARED_WITH_ME_PRIVATE_MARKETPLACE_NAME;
use super::REMOTE_WORKSPACE_SHARED_WITH_ME_UNLISTED_MARKETPLACE_NAME;
use super::RemotePluginCatalogError;
use super::RemotePluginScope;
use super::RemotePluginServiceConfig;
use super::ensure_chatgpt_auth;
use super::fetch_installed_plugins_for_scope_with_download_url;
use super::remote_plugin_canonical_marketplace_name;
use crate::store::PLUGINS_CACHE_DIR;
use crate::store::PluginStore;
use crate::store::PluginStoreError;
use crate::store::is_reserved_plugin_cache_entry;
use codex_login::CodexAuth;
use codex_plugin::PluginId;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::OnceLock;
use tracing::info;
use tracing::warn;

static REMOTE_PLUGIN_CACHE_MUTATIONS_IN_FLIGHT: OnceLock<Mutex<RemotePluginCacheMutations>> =
    OnceLock::new();

#[derive(Default)]
struct RemotePluginCacheMutations {
    in_flight: HashMap<RemotePluginCacheMutationKey, usize>,
    generations: HashMap<PathBuf, u64>,
    independent_in_flight: HashMap<PathBuf, usize>,
    syncs: HashMap<RemoteInstalledPluginBundleSyncKey, RemoteInstalledPluginBundleSyncState>,
}

#[derive(Default)]
struct RemoteInstalledPluginBundleSyncState {
    authority: Option<RemoteInstalledPluginBundleSyncAuthority>,
    epoch: u64,
    in_flight: bool,
    requested: Option<RemoteInstalledPluginBundleSyncRequest>,
}

#[derive(PartialEq, Eq)]
struct RemoteInstalledPluginBundleSyncAuthority {
    chatgpt_base_url: String,
    account_id: Option<String>,
    chatgpt_user_id: Option<String>,
    is_workspace_account: bool,
}

struct RemoteInstalledPluginBundleSyncRequest {
    codex_home: PathBuf,
    config: RemotePluginServiceConfig,
    auth: CodexAuth,
    epoch: u64,
    on_local_cache_changed: Option<Arc<dyn Fn() + Send + Sync + 'static>>,
    completion: Option<tokio::sync::oneshot::Sender<RemoteInstalledPluginBundleSyncResult>>,
    runtime: tokio::runtime::Handle,
}

type RemoteInstalledPluginBundleSyncResult =
    Result<RemoteInstalledPluginBundleSyncOutcome, RemoteInstalledPluginBundleSyncError>;

struct RemoteInstalledPluginBundleSyncGuard(RemoteInstalledPluginBundleSyncKey);

impl Drop for RemoteInstalledPluginBundleSyncGuard {
    fn drop(&mut self) {
        let mutations = REMOTE_PLUGIN_CACHE_MUTATIONS_IN_FLIGHT.get_or_init(Default::default);
        let key = self.0.clone();
        let release = move || {
            let requested = {
                let mut mutations = lock_cache_mutations();
                let Some(state) = mutations.syncs.get_mut(&key) else {
                    return;
                };
                state.in_flight = false;
                take_ready_sync_request(&mut mutations, &key)
            };
            if let Some(requested) = requested {
                spawn_remote_installed_plugin_bundle_sync(requested, key);
            }
        };
        // A last registration may be released by async cancellation while an
        // unrelated publication owns the mutex. Do not wait for disk I/O there.
        if let Ok(mut mutations) = mutations.try_lock() {
            if let Some(state) = mutations.syncs.get_mut(&self.0) {
                state.in_flight = false;
            }
            let requested = take_ready_sync_request(&mut mutations, &self.0);
            drop(mutations);
            if let Some(requested) = requested {
                spawn_remote_installed_plugin_bundle_sync(requested, self.0.clone());
            }
        } else if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn_blocking(release);
        } else {
            release();
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RemoteInstalledPluginBundleSyncOutcome {
    pub installed_plugin_ids: Vec<String>,
    pub removed_cache_plugin_ids: Vec<String>,
    pub failed_remote_plugin_ids: Vec<String>,
}

impl RemoteInstalledPluginBundleSyncOutcome {
    pub fn changed_local_cache(&self) -> bool {
        !self.installed_plugin_ids.is_empty() || !self.removed_cache_plugin_ids.is_empty()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RemoteInstalledPluginBundleSyncError {
    #[error("{source}")]
    Partial {
        outcome: RemoteInstalledPluginBundleSyncOutcome,
        source: Box<RemoteInstalledPluginBundleSyncError>,
    },

    #[error("{0}")]
    Catalog(#[from] RemotePluginCatalogError),

    #[error("{0}")]
    Store(#[from] PluginStoreError),

    #[error("failed to join stale remote plugin cache cleanup task: {0}")]
    Join(#[from] tokio::task::JoinError),

    #[error("failed to remove stale remote plugin cache entries: {0}")]
    CacheRemove(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RemoteInstalledPluginBundleSyncKey {
    plugin_cache_root: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RemotePluginCacheMutationKey {
    plugin_cache_root: PathBuf,
    marketplace_name: String,
    plugin_name: String,
}

pub struct RemotePluginCacheMutationGuard {
    key: RemotePluginCacheMutationKey,
    sync_registration: Option<Arc<RemoteInstalledPluginBundleSyncGuard>>,
    invalidates_snapshot: bool,
    snapshot_generation: Option<u64>,
}

pub(crate) fn maybe_start_remote_installed_plugin_bundle_sync(
    codex_home: PathBuf,
    config: RemotePluginServiceConfig,
    auth: Option<CodexAuth>,
    on_local_cache_changed: Option<Arc<dyn Fn() + Send + Sync + 'static>>,
) {
    let Some(auth) = auth else {
        invalidate_remote_installed_plugin_bundle_sync(&codex_home);
        return;
    };
    enqueue_remote_installed_plugin_bundle_sync(RemoteInstalledPluginBundleSyncRequest {
        codex_home,
        config,
        auth,
        epoch: 0,
        on_local_cache_changed,
        completion: None,
        runtime: tokio::runtime::Handle::current(),
    });
}

fn enqueue_remote_installed_plugin_bundle_sync(mut request: RemoteInstalledPluginBundleSyncRequest) {
    let key = RemoteInstalledPluginBundleSyncKey {
        plugin_cache_root: remote_plugin_cache_root(&request.codex_home),
    };
    let authority = RemoteInstalledPluginBundleSyncAuthority {
        chatgpt_base_url: request.config.chatgpt_base_url.clone(),
        account_id: request.auth.get_account_id(),
        chatgpt_user_id: request.auth.get_chatgpt_user_id(),
        is_workspace_account: request.auth.is_workspace_account(),
    };
    {
        let mut mutations_guard = lock_cache_mutations();
        // Borrow the guarded fields disjointly so the entry stays live.
        let mutations = &mut *mutations_guard;
        let state = mutations.syncs.entry(key.clone()).or_default();
        if state.authority.as_ref() != Some(&authority) {
            state.authority = Some(authority);
            state.epoch = state.epoch.wrapping_add(1);
            let generation = mutations
                .generations
                .entry(key.plugin_cache_root.clone())
                .or_default();
            *generation = generation.wrapping_add(1);
        }
        request.epoch = state.epoch;
        if request.on_local_cache_changed.is_none()
            && let Some(previous) = state.requested.as_ref()
            && previous.epoch == request.epoch
        {
            request.on_local_cache_changed = previous.on_local_cache_changed.clone();
        }
        // One physical owner per home; a changed account/config supersedes
        // its publication authority immediately, not just its callback.
        state.requested = Some(request);
        let Some(ready) = take_ready_sync_request(mutations, &key) else {
            return;
        };
        request = ready;
    }
    spawn_remote_installed_plugin_bundle_sync(request, key);
}

fn take_ready_sync_request(
    mutations: &mut RemotePluginCacheMutations,
    key: &RemoteInstalledPluginBundleSyncKey,
) -> Option<RemoteInstalledPluginBundleSyncRequest> {
    if mutations.independent_in_flight.contains_key(&key.plugin_cache_root) {
        return None;
    }
    let state = mutations.syncs.get_mut(key)?;
    if state.in_flight {
        return None;
    }
    let request = state.requested.take()?;
    state.in_flight = true;
    Some(request)
}

fn spawn_remote_installed_plugin_bundle_sync(
    mut request: RemoteInstalledPluginBundleSyncRequest,
    key: RemoteInstalledPluginBundleSyncKey,
) {
    let guard = Arc::new(RemoteInstalledPluginBundleSyncGuard(key));
    let runtime = request.runtime.clone();
    runtime.spawn(async move {
        let _guard = guard;
        let snapshot = {
            let mutations = lock_cache_mutations();
            mutations
                .syncs
                .get(&_guard.0)
                .filter(|state| state.epoch == request.epoch)
                .map(|_| {
                    mutations
                        .generations
                        .get(&_guard.0.plugin_cache_root)
                        .copied()
                        .unwrap_or_default()
                })
        };
        let sync = async {
            match snapshot {
                Some(generation) => {
                    sync_remote_installed_plugin_bundles_with_registration(
                        request.codex_home,
                        &request.config,
                        Some(&request.auth),
                        generation,
                        Arc::clone(&_guard),
                    )
                    .await
                }
                None => Ok(RemoteInstalledPluginBundleSyncOutcome::default()),
            }
        };
        let result = if let Some(completion) = request.completion.as_mut() {
            // A cancelled one-shot still cancels its reversible async work.
            // Already-admitted blocking workers retain the registration until
            // their publication/rollback or cleanup has actually finished.
            tokio::select! {
                result = sync => result,
                _ = completion.closed() => return,
            }
        } else {
            sync.await
        };
        let outcome = match &result {
            Ok(outcome) | Err(RemoteInstalledPluginBundleSyncError::Partial { outcome, .. }) => {
                Some(outcome)
            }
            Err(_) => None,
        };
        if outcome.is_some_and(RemoteInstalledPluginBundleSyncOutcome::changed_local_cache)
            && let Some(on_local_cache_changed) = request.on_local_cache_changed
        {
            on_local_cache_changed();
        }
        match &result {
            Ok(outcome) => {
                info!(
                    installed_plugin_ids = ?outcome.installed_plugin_ids,
                    removed_cache_plugin_ids = ?outcome.removed_cache_plugin_ids,
                    failed_remote_plugin_ids = ?outcome.failed_remote_plugin_ids,
                    "completed remote installed plugin bundle sync"
                );
            }
            Err(err) => {
                warn!(
                    error = %err,
                    "remote installed plugin bundle sync failed"
                );
            }
        }
        if let Some(completion) = request.completion {
            let _ = completion.send(result);
        }
    });
}

pub async fn sync_remote_installed_plugin_bundles_once(
    codex_home: PathBuf,
    config: &RemotePluginServiceConfig,
    auth: Option<&CodexAuth>,
) -> Result<RemoteInstalledPluginBundleSyncOutcome, RemoteInstalledPluginBundleSyncError> {
    let auth = ensure_chatgpt_auth(auth)?.clone();
    let (completion, result) = tokio::sync::oneshot::channel();
    enqueue_remote_installed_plugin_bundle_sync(RemoteInstalledPluginBundleSyncRequest {
        codex_home,
        config: config.clone(),
        auth,
        epoch: 0,
        on_local_cache_changed: None,
        completion: Some(completion),
        runtime: tokio::runtime::Handle::current(),
    });
    result.await.map_err(|_| {
        PluginStoreError::Invalid(
            "remote plugin bundle sync was superseded before execution".to_string(),
        )
    })?
}

async fn sync_remote_installed_plugin_bundles_with_registration(
    codex_home: PathBuf,
    config: &RemotePluginServiceConfig,
    auth: Option<&CodexAuth>,
    generation: u64,
    sync_registration: Arc<RemoteInstalledPluginBundleSyncGuard>,
) -> Result<RemoteInstalledPluginBundleSyncOutcome, RemoteInstalledPluginBundleSyncError> {
    let mut outcome = RemoteInstalledPluginBundleSyncOutcome::default();
    let result = sync_remote_installed_plugin_bundles_inner(
        codex_home,
        config,
        auth,
        &mut outcome,
        generation,
        sync_registration,
    )
    .await;
    outcome.installed_plugin_ids.sort();
    outcome.installed_plugin_ids.dedup();
    outcome.removed_cache_plugin_ids.sort();
    match result {
        Ok(()) => Ok(outcome),
        Err(source) if outcome.changed_local_cache() => {
            Err(RemoteInstalledPluginBundleSyncError::Partial {
                outcome,
                source: Box::new(source),
            })
        }
        Err(err) => Err(err),
    }
}

async fn sync_remote_installed_plugin_bundles_inner(
    codex_home: PathBuf,
    config: &RemotePluginServiceConfig,
    auth: Option<&CodexAuth>,
    outcome: &mut RemoteInstalledPluginBundleSyncOutcome,
    generation: u64,
    sync_registration: Arc<RemoteInstalledPluginBundleSyncGuard>,
) -> Result<(), RemoteInstalledPluginBundleSyncError> {
    let auth = ensure_chatgpt_auth(auth)?;
    let global = async {
        let scope = RemotePluginScope::Global;
        let installed_plugins = fetch_installed_plugins_for_scope_with_download_url(
            config, auth, scope, /*include_download_urls*/ true,
        )
        .await?;
        Ok::<_, RemotePluginCatalogError>((scope, installed_plugins))
    };
    let workspace = async {
        let scope = RemotePluginScope::Workspace;
        let installed_plugins = fetch_installed_plugins_for_scope_with_download_url(
            config, auth, scope, /*include_download_urls*/ true,
        )
        .await?;
        Ok::<_, RemotePluginCatalogError>((scope, installed_plugins))
    };
    let user = async {
        let scope = RemotePluginScope::User;
        let installed_plugins = fetch_installed_plugins_for_scope_with_download_url(
            config, auth, scope, /*include_download_urls*/ true,
        )
        .await?;
        Ok::<_, RemotePluginCatalogError>((scope, installed_plugins))
    };

    let (global, workspace, user) = tokio::try_join!(global, workspace, user)?;
    let store = PluginStore::try_new(codex_home.clone())?;
    let mut installed_plugin_names_by_marketplace =
        BTreeMap::<String, BTreeSet<String>>::from_iter([
            (REMOTE_GLOBAL_MARKETPLACE_NAME.to_string(), BTreeSet::new()),
            (
                REMOTE_CREATED_BY_ME_MARKETPLACE_NAME.to_string(),
                BTreeSet::new(),
            ),
            (
                REMOTE_WORKSPACE_MARKETPLACE_NAME.to_string(),
                BTreeSet::new(),
            ),
            (
                REMOTE_WORKSPACE_SHARED_WITH_ME_MARKETPLACE_NAME.to_string(),
                BTreeSet::new(),
            ),
            (
                REMOTE_WORKSPACE_SHARED_WITH_ME_PRIVATE_MARKETPLACE_NAME.to_string(),
                BTreeSet::new(),
            ),
            (
                REMOTE_WORKSPACE_SHARED_WITH_ME_UNLISTED_MARKETPLACE_NAME.to_string(),
                BTreeSet::new(),
            ),
        ]);
    let mut failed_remote_plugin_ids = BTreeSet::new();

    for (_scope, installed_plugins) in [global, workspace, user] {
        for installed_plugin in installed_plugins {
            let plugin = installed_plugin.plugin;
            let marketplace_name = remote_plugin_canonical_marketplace_name(&plugin)?.to_string();
            installed_plugin_names_by_marketplace
                .entry(marketplace_name.clone())
                .or_default()
                .insert(plugin.name.clone());
            let plugin_id = match PluginId::new(plugin.name.clone(), marketplace_name.clone()) {
                Ok(plugin_id) => plugin_id,
                Err(err) => {
                    warn!(
                        remote_plugin_id = %plugin.id,
                        plugin = %plugin.name,
                        marketplace = %marketplace_name,
                        error = %err,
                        "skipping remote installed plugin with invalid local cache id"
                    );
                    failed_remote_plugin_ids.insert(plugin.id);
                    continue;
                }
            };
            let release_version = plugin
                .release
                .version
                .as_deref()
                .map(str::trim)
                .filter(|version| !version.is_empty());
            let cached_identity = {
                let store = store.clone();
                let plugin_id = plugin_id.clone();
                let remote_plugin_id = plugin.id.clone();
                let release_version = release_version.map(str::to_string);
                let registration = sync_registration.clone();
                tokio::task::spawn_blocking(move || {
                    let _registration = registration;
                    let _entry_lock = match crate::store::try_lock_plugin_cache_entry(
                        store.plugin_base_root(&plugin_id).as_path(),
                    ) {
                        Ok(lock) => lock,
                        Err(_) => return None,
                    };
                    let _publication = match lock_snapshot_publication(
                        store.codex_home().as_path(),
                        generation,
                    ) {
                        Ok(publication) => publication,
                        Err(_) => return Some(Ok(())),
                    };
                    if !release_version.as_deref().is_some_and(|expected| {
                        store.active_plugin_version(&plugin_id).as_deref() == Some(expected)
                    }) {
                        return None;
                    }
                    match store.remote_plugin_id(&plugin_id) {
                        Ok(Some(existing)) if existing != remote_plugin_id => None,
                        Ok(Some(_)) => Some(Ok(())),
                        Ok(None) => {
                            Some(store.write_remote_plugin_id(&plugin_id, &remote_plugin_id))
                        }
                        Err(err) => Some(Err(err)),
                    }
                })
                .await?
            };
            if let Some(identity_result) = cached_identity {
                if let Err(err) = identity_result {
                    warn!(
                        remote_plugin_id = %plugin.id,
                        plugin = %plugin.name,
                        marketplace = %marketplace_name,
                        error = %err,
                        "failed to persist identity for cached remote installed plugin"
                    );
                    failed_remote_plugin_ids.insert(plugin.id);
                }
                continue;
            }

            let bundle = match crate::remote_bundle::validate_remote_plugin_bundle(
                &plugin.id,
                &marketplace_name,
                &plugin.name,
                release_version,
                plugin.release.bundle_download_url.as_deref(),
                plugin.release.app_manifest.clone(),
            ) {
                Ok(bundle) => bundle,
                Err(err) => {
                    warn!(
                        remote_plugin_id = %plugin.id,
                        plugin = %plugin.name,
                        marketplace = %marketplace_name,
                        error = %err,
                        "skipping remote installed plugin bundle download"
                    );
                    failed_remote_plugin_ids.insert(plugin.id);
                    continue;
                }
            };

            let mut mutation = acquire_remote_plugin_cache_mutation(
                &codex_home,
                &marketplace_name,
                &plugin.name,
                /*invalidates_snapshot*/ false,
            )
            .await?;
            mutation.sync_registration = Some(sync_registration.clone());
            mutation.snapshot_generation = Some(generation);
            match crate::remote_bundle::download_and_install_remote_plugin_bundle_with_guard(
                codex_home.clone(),
                bundle,
                &config.http_clients,
                mutation,
            )
            .await
            {
                Ok(result) => {
                    outcome.installed_plugin_ids.push(result.plugin_id.as_key());
                }
                Err(err) => {
                    warn!(
                        remote_plugin_id = %plugin.id,
                        plugin = %plugin.name,
                        marketplace = %marketplace_name,
                        error = %err,
                        "failed to download remote installed plugin bundle"
                    );
                    failed_remote_plugin_ids.insert(plugin.id);
                }
            }
        }
    }

    outcome.failed_remote_plugin_ids = failed_remote_plugin_ids.into_iter().collect();
    let (removed, result) = tokio::task::spawn_blocking(move || {
        let _registration = sync_registration;
        let mut removed = Vec::new();
        let result = remove_stale_remote_plugin_caches_since(
            codex_home.as_path(),
            &installed_plugin_names_by_marketplace,
            generation,
            &mut removed,
        );
        (removed, result)
    })
    .await?;
    outcome.removed_cache_plugin_ids = removed;
    result.map_err(RemoteInstalledPluginBundleSyncError::CacheRemove)
}

pub async fn mark_remote_plugin_cache_mutation_in_flight(
    codex_home: &Path,
    marketplace_name: &str,
    plugin_name: &str,
) -> Result<RemotePluginCacheMutationGuard, tokio::task::JoinError> {
    acquire_remote_plugin_cache_mutation(
        codex_home,
        marketplace_name,
        plugin_name,
        /*invalidates_snapshot*/ true,
    )
    .await
}

async fn acquire_remote_plugin_cache_mutation(
    codex_home: &Path,
    marketplace_name: &str,
    plugin_name: &str,
    invalidates_snapshot: bool,
) -> Result<RemotePluginCacheMutationGuard, tokio::task::JoinError> {
    let codex_home = codex_home.to_path_buf();
    let marketplace_name = marketplace_name.to_string();
    let plugin_name = plugin_name.to_string();
    tokio::task::spawn_blocking(move || {
        mark_remote_plugin_cache_mutation_in_flight_inner(
            &codex_home,
            &marketplace_name,
            &plugin_name,
            invalidates_snapshot,
        )
    })
    .await
}

fn mark_remote_plugin_cache_mutation_in_flight_inner(
    codex_home: &Path,
    marketplace_name: &str,
    plugin_name: &str,
    invalidates_snapshot: bool,
) -> RemotePluginCacheMutationGuard {
    let key = RemotePluginCacheMutationKey {
        plugin_cache_root: remote_plugin_cache_root(codex_home),
        marketplace_name: marketplace_name.to_string(),
        plugin_name: plugin_name.to_string(),
    };
    let mutations = REMOTE_PLUGIN_CACHE_MUTATIONS_IN_FLIGHT
        .get_or_init(|| Mutex::new(RemotePluginCacheMutations::default()));
    let mut mutations = match mutations.lock() {
        Ok(mutations) => mutations,
        Err(err) => err.into_inner(),
    };
    // A sync's own install is part of its catalog snapshot. Only independent
    // mutations invalidate that snapshot; both still participate in exclusion.
    if invalidates_snapshot {
        let generation = mutations
            .generations
            .entry(key.plugin_cache_root.clone())
            .or_default();
        *generation = generation.wrapping_add(1);
        *mutations
            .independent_in_flight
            .entry(key.plugin_cache_root.clone())
            .or_default() += 1;
    }
    *mutations.in_flight.entry(key.clone()).or_default() += 1;
    RemotePluginCacheMutationGuard {
        key,
        sync_registration: None,
        invalidates_snapshot,
        snapshot_generation: None,
    }
}

pub(crate) struct RemotePluginCachePublicationGuard {
    _mutations: MutexGuard<'static, RemotePluginCacheMutations>,
}

impl RemotePluginCacheMutationGuard {
    pub(crate) fn lock_for_publication(
        &self,
    ) -> Result<RemotePluginCachePublicationGuard, PluginStoreError> {
        let mutations = lock_cache_mutations();
        if let Some(generation) = self.snapshot_generation
            && !snapshot_is_current(&mutations, &self.key.plugin_cache_root, generation)
        {
            return Err(PluginStoreError::Invalid(
                "remote plugin bundle snapshot was superseded before publication".to_string(),
            ));
        }
        Ok(RemotePluginCachePublicationGuard {
            _mutations: mutations,
        })
    }
}

impl Drop for RemotePluginCacheMutationGuard {
    fn drop(&mut self) {
        let Some(mutations) = REMOTE_PLUGIN_CACHE_MUTATIONS_IN_FLIGHT.get() else {
            return;
        };
        if let Ok(mut mutations) = mutations.try_lock() {
            let requested = release_remote_plugin_cache_mutation(
                &mut mutations,
                &self.key,
                self.invalidates_snapshot,
            );
            drop(mutations);
            if let Some(requested) = requested {
                spawn_remote_installed_plugin_bundle_sync(
                    requested,
                    RemoteInstalledPluginBundleSyncKey {
                        plugin_cache_root: self.key.plugin_cache_root.clone(),
                    },
                );
            }
            return;
        }
        // Publication/cleanup may briefly hold this lock across a rename. A
        // cancelled download must not wait for filesystem I/O on an async thread.
        let key = self.key.clone();
        let invalidates_snapshot = self.invalidates_snapshot;
        let registration = self.sync_registration.take();
        let release = move || {
            let _registration = registration;
            let mut mutations = mutations
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let requested =
                release_remote_plugin_cache_mutation(&mut mutations, &key, invalidates_snapshot);
            drop(mutations);
            if let Some(requested) = requested {
                spawn_remote_installed_plugin_bundle_sync(
                    requested,
                    RemoteInstalledPluginBundleSyncKey {
                        plugin_cache_root: key.plugin_cache_root,
                    },
                );
            }
        };
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn_blocking(release);
        } else {
            release();
        }
    }
}

fn release_remote_plugin_cache_mutation(
    mutations: &mut RemotePluginCacheMutations,
    key: &RemotePluginCacheMutationKey,
    invalidates_snapshot: bool,
) -> Option<RemoteInstalledPluginBundleSyncRequest> {
    if invalidates_snapshot {
        let generation = mutations
            .generations
            .entry(key.plugin_cache_root.clone())
            .or_default();
        *generation = generation.wrapping_add(1);
        if let Some(count) = mutations.independent_in_flight.get_mut(&key.plugin_cache_root) {
            *count -= 1;
            if *count == 0 {
                mutations.independent_in_flight.remove(&key.plugin_cache_root);
            }
        }
    }
    if let Some(count) = mutations.in_flight.get_mut(key) {
        *count -= 1;
        if *count == 0 {
            mutations.in_flight.remove(key);
        }
    }
    take_ready_sync_request(
        mutations,
        &RemoteInstalledPluginBundleSyncKey {
            plugin_cache_root: key.plugin_cache_root.clone(),
        },
    )
}

fn lock_cache_mutations() -> MutexGuard<'static, RemotePluginCacheMutations> {
    REMOTE_PLUGIN_CACHE_MUTATIONS_IN_FLIGHT
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn snapshot_is_current(mutations: &RemotePluginCacheMutations, root: &Path, generation: u64) -> bool {
    mutations.generations.get(root).copied().unwrap_or_default() == generation
        && !mutations.independent_in_flight.contains_key(root)
}

fn lock_snapshot_publication(
    codex_home: &Path,
    generation: u64,
) -> Result<RemotePluginCachePublicationGuard, PluginStoreError> {
    let mutations = lock_cache_mutations();
    if !snapshot_is_current(&mutations, &remote_plugin_cache_root(codex_home), generation) {
        return Err(PluginStoreError::Invalid(
            "remote plugin bundle snapshot was superseded before publication".to_string(),
        ));
    }
    Ok(RemotePluginCachePublicationGuard {
        _mutations: mutations,
    })
}

fn remove_stale_remote_plugin_caches_since(
    codex_home: &Path,
    installed_plugin_names_by_marketplace: &BTreeMap<String, BTreeSet<String>>,
    generation: u64,
    removed_cache_plugin_ids: &mut Vec<String>,
) -> Result<(), String> {
    let root = remote_plugin_cache_root(codex_home);
    if !snapshot_is_current(&lock_cache_mutations(), &root, generation) {
        return Ok(());
    }
    for marketplace_name in [
        REMOTE_GLOBAL_MARKETPLACE_NAME,
        REMOTE_CREATED_BY_ME_MARKETPLACE_NAME,
        REMOTE_WORKSPACE_MARKETPLACE_NAME,
        REMOTE_WORKSPACE_SHARED_WITH_ME_MARKETPLACE_NAME,
        REMOTE_WORKSPACE_SHARED_WITH_ME_PRIVATE_MARKETPLACE_NAME,
        REMOTE_WORKSPACE_SHARED_WITH_ME_UNLISTED_MARKETPLACE_NAME,
    ] {
        let marketplace_root = codex_home.join(PLUGINS_CACHE_DIR).join(marketplace_name);
        if !marketplace_root.exists() {
            continue;
        }
        let installed_plugin_names = installed_plugin_names_by_marketplace
            .get(marketplace_name)
            .cloned()
            .unwrap_or_default();
        for entry in fs::read_dir(&marketplace_root).map_err(|err| {
            format!(
                "failed to read remote plugin cache directory {}: {err}",
                marketplace_root.display()
            )
        })? {
            let entry = entry.map_err(|err| {
                format!(
                    "failed to enumerate remote plugin cache directory {}: {err}",
                    marketplace_root.display()
                )
            })?;
            let plugin_name = entry.file_name().into_string().map_err(|file_name| {
                format!(
                    "remote plugin cache entry under {} is not valid UTF-8: {:?}",
                    marketplace_root.display(),
                    file_name
                )
            })?;
            // A mutation guard names only its plugin, while the store stages that plugin's
            // transaction in a sibling entry; deleting it could strand the previous version.
            if is_reserved_plugin_cache_entry(&plugin_name)
                || installed_plugin_names.contains(&plugin_name)
            {
                continue;
            }
            // Retire a cache entry under the same authority as publication, then
            // recursively delete outside it. Account changes never wait for a
            // whole cache tree removal, and stale epochs cannot move newer roots.
            let retired = tempfile::Builder::new()
                .prefix(".plugin-stale-")
                .tempdir_in(&marketplace_root)
                .map_err(|err| {
                    format!("failed to stage stale remote plugin cache removal: {err}")
                })?;
            let mutations = lock_cache_mutations();
            if !snapshot_is_current(&mutations, &root, generation) {
                return Ok(());
            }
            if mutations
                .in_flight
                .contains_key(&RemotePluginCacheMutationKey {
                    plugin_cache_root: root.clone(),
                    marketplace_name: marketplace_name.to_string(),
                    plugin_name: plugin_name.clone(),
                })
            {
                continue;
            }

            let cache_path = entry.path();
            let _entry_lock = match crate::store::try_lock_plugin_cache_entry(&cache_path) {
                Ok(lock) => lock,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(error) => {
                    return Err(format!(
                        "failed to own stale plugin cache entry {}: {error}",
                        cache_path.display()
                    ));
                }
            };
            fs::rename(&cache_path, retired.path().join("stale-plugin-root")).map_err(|err| {
                format!(
                    "failed to retire stale remote plugin cache entry {}: {err}",
                    cache_path.display()
                )
            })?;
            drop(mutations);
            let plugin_key = PluginId::new(plugin_name.clone(), marketplace_name.to_string())
                .map(|plugin_id| plugin_id.as_key())
                .unwrap_or_else(|_| format!("{plugin_name}@{marketplace_name}"));
            removed_cache_plugin_ids.push(plugin_key);
            let retired_path = retired.path().to_path_buf();
            retired.close().map_err(|err| {
                format!(
                    "failed to remove retired remote plugin cache entry at {}: {err}",
                    retired_path.display()
                )
            })?;
        }
    }

    removed_cache_plugin_ids.sort();
    Ok(())
}

fn remote_plugin_cache_root(codex_home: &Path) -> PathBuf {
    codex_home.join(PLUGINS_CACHE_DIR)
}

pub(crate) fn invalidate_remote_installed_plugin_bundle_sync(codex_home: &Path) {
    let root = remote_plugin_cache_root(codex_home);
    let mut mutations = lock_cache_mutations();
    let generation = mutations.generations.entry(root.clone()).or_default();
    *generation = generation.wrapping_add(1);
    if let Some(state) = mutations.syncs.get_mut(&RemoteInstalledPluginBundleSyncKey {
        plugin_cache_root: root,
    }) {
        state.epoch = state.epoch.wrapping_add(1);
        state.authority = None;
        state.requested = None;
    }
}
