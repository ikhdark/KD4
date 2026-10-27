//! This file handles all logic related to managing MCP OAuth credentials.
//! Credentials are stored with Windows Credential Manager through the keyring crate.
//! https://crates.io/crates/keyring
//!
//! If the keyring is not available or fails, we fall back to CODEX_HOME/.credentials.json which is consistent with other coding CLI agents.

mod store_lock;


use anyhow::Context;
use anyhow::Error;
use anyhow::Result;
use codex_config::types::AuthKeyringBackendKind;
use codex_config::types::OAuthCredentialsStoreMode;
use codex_secrets::LocalSecretsBackend;
use codex_secrets::LocalSecretsNamespace;
use codex_secrets::SecretName;
use codex_secrets::SecretScope;
use oauth2::AccessToken;
use oauth2::RefreshToken;
use oauth2::Scope;
use oauth2::TokenResponse;
use oauth2::basic::BasicTokenType;
use rmcp::transport::auth::OAuthTokenResponse;
use rmcp::transport::auth::VendorExtraTokenFields;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::map::Map as JsonMap;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use tempfile::NamedTempFile;
use tracing::warn;

use self::store_lock::OAuthStore;
use self::store_lock::OAuthStoreLock;
use self::store_lock::OAuthStoreLockFailure;

use codex_keyring_store::DefaultKeyringStore;
use codex_keyring_store::KeyringStore;
use rmcp::transport::auth::CredentialStore;
use rmcp::transport::auth::InMemoryCredentialStore;
use tokio::sync::Mutex;

const KEYRING_SERVICE: &str = "Codex MCP Credentials";
const MCP_OAUTH_SECRET_PREFIX: &str = "MCP_OAUTH";
const REFRESH_SKEW_MILLIS: u64 = 30_000;

/// Credential durability is separate from whether the server operation ran.
#[derive(Debug, thiserror::Error)]
pub enum OAuthPersistenceError {
    #[error("MCP OAuth credential persistence failed; durable authentication was not confirmed")]
    Failed(#[source] anyhow::Error),
    #[error("MCP OAuth credential durability is unconfirmed; an admitted write may still complete")]
    Pending,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
enum CredentialBackend { File, Direct, Secrets }

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
struct CredentialAuthority {
    backend: Option<CredentialBackend>,
    fingerprint: Option<String>,
    #[serde(default)]
    revision: u64,
}

/// A loaded client's claim on committed authority. Clones share successful
/// rotations, but an explicit login/logout never updates an older claim.
#[derive(Clone)]
pub(crate) struct OAuthRequestAuthority {
    codex_home: PathBuf,
    server_name: String,
    url: String,
    expected: Arc<std::sync::Mutex<Option<CredentialAuthority>>>,
}

#[derive(Debug, thiserror::Error)]
#[error("MCP OAuth authority changed; reconnect after login or logout")]
struct OAuthAuthorityChanged;

impl OAuthRequestAuthority {
    fn check_with_lock_held(&self, state: &CredentialAuthorities, key: &str) -> Result<()> {
        let expected = self.expected.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.entries.get(key) != expected.as_ref() {
            return Err(OAuthAuthorityChanged.into());
        }
        Ok(())
    }

    fn accept_with_lock_held(&self, authority: CredentialAuthority) {
        *self.expected.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(authority);
    }

    pub(crate) async fn check(&self) -> Result<()> {
        let authority = self.clone();
        tokio::task::spawn_blocking(move || {
            let _transaction = OAuthStoreLock::acquire_in(
                &authority.codex_home, OAuthStore::Authority, OAUTH_PERSISTENCE_TIMEOUT,
            )?;
            let state = read_credential_authorities(&authority.codex_home)?;
            authority.check_with_lock_held(
                &state,
                &compute_store_key(&authority.server_name, &authority.url)?,
            )
        })
        .await
        .context("MCP OAuth authority admission task failed")?
    }
}

#[derive(Serialize, Deserialize)]
struct CredentialAuthorities {
    version: u32,
    entries: BTreeMap<String, CredentialAuthority>,
}

fn read_credential_authorities(home: &Path) -> Result<CredentialAuthorities> {
    match fs::read(home.join("mcp-oauth-authority.json")) {
        Ok(bytes) => {
            let state: CredentialAuthorities = serde_json::from_slice(&bytes).context("invalid OAuth credential authority")?;
            anyhow::ensure!(state.version == 1, "unsupported OAuth credential authority version");
            Ok(state)
        }
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(CredentialAuthorities { version: 1, entries: BTreeMap::new() }),
        Err(err) => Err(err).context("read OAuth credential authority"),
    }
}

// This is a commitment to credential contents, never an expiry-based ordering.
// Exclude RMCP's relative expires_in, which is refreshed on every load, and use
// the common durable fields represented by both supported storage formats.
fn credential_fingerprint(tokens: &StoredOAuthTokens) -> Result<String> {
    let response = &tokens.token_response.0;
    let payload = serde_json::json!({
        "server_name": tokens.server_name, "url": tokens.url, "client_id": tokens.client_id,
        "access_token": response.access_token().secret(),
        "refresh_token": response.refresh_token().map(|value| value.secret()),
        "scopes": response.scopes().map(|values| values.iter().map(|scope| scope.to_string()).collect::<Vec<_>>()).unwrap_or_default(),
        "expires_at": tokens.expires_at,
    });
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(&payload)?)))
}

fn commit_credential_authority(home: &Path, state: &CredentialAuthorities) -> Result<()> {
    fs::create_dir_all(home)?;
    let mut staged = NamedTempFile::new_in(home)?;
    staged.write_all(&serde_json::to_vec(state)?)?;
    staged.as_file().sync_all()?;
    staged.persist(home.join("mcp-oauth-authority.json")).map_err(|err| err.error)?;
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StoredOAuthTokens {
    pub server_name: String,
    pub url: String,
    pub client_id: String,
    pub token_response: WrappedOAuthTokenResponse,
    #[serde(default)]
    pub expires_at: Option<u64>,
}

/// Wrap OAuthTokenResponse to allow for partial equality comparison.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WrappedOAuthTokenResponse(pub OAuthTokenResponse);

impl PartialEq for WrappedOAuthTokenResponse {
    fn eq(&self, other: &Self) -> bool {
        match (serde_json::to_string(self), serde_json::to_string(other)) {
            (Ok(s1), Ok(s2)) => s1 == s2,
            _ => false,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StoredOAuthTokenStatus {
    Missing,
    Usable,
    AuthorizationRequired,
}

pub(crate) fn load_oauth_tokens(
    codex_home: &Path,
    server_name: &str,
    url: &str,
    store_mode: OAuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
) -> Result<Option<StoredOAuthTokens>> {
    load_oauth_tokens_with_authority(codex_home, server_name, url, store_mode, keyring_backend_kind)
        .map(|(tokens, _)| tokens)
}

pub(crate) fn load_oauth_tokens_with_authority(
    codex_home: &Path,
    server_name: &str,
    url: &str,
    store_mode: OAuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
) -> Result<(Option<StoredOAuthTokens>, OAuthRequestAuthority)> {
    let keyring_store = DefaultKeyringStore;
    load_oauth_tokens_coordinated(&keyring_store, codex_home, server_name, url, store_mode, keyring_backend_kind)
}

fn load_oauth_tokens_coordinated<K: KeyringStore + Clone + 'static>(
    keyring_store: &K, codex_home: &Path, server_name: &str, url: &str,
    store_mode: OAuthCredentialsStoreMode, keyring_backend_kind: AuthKeyringBackendKind,
) -> Result<(Option<StoredOAuthTokens>, OAuthRequestAuthority)> {
    // Global lock order: authority -> one aggregate store -> Secrets internal lock.
    let _transaction = OAuthStoreLock::acquire(codex_home, OAuthStore::Authority)?;
    let authority = read_credential_authorities(codex_home)?;
    let request_authority = OAuthRequestAuthority {
        codex_home: codex_home.to_path_buf(),
        server_name: server_name.to_string(),
        url: url.to_string(),
        expected: Arc::new(std::sync::Mutex::new(
            authority.entries.get(&compute_store_key(server_name, url)?).cloned(),
        )),
    };
    if let Some(committed) = authority.entries.get(&compute_store_key(server_name, url)?) {
        let Some(backend) = committed.backend else { return Ok((None, request_authority)); };
        if matches!(store_mode, OAuthCredentialsStoreMode::File) && backend != CredentialBackend::File
            || matches!(store_mode, OAuthCredentialsStoreMode::Keyring) && backend == CredentialBackend::File {
            return Ok((None, request_authority));
        }
        let tokens = match backend {
            CredentialBackend::File => load_oauth_tokens_from_file(codex_home, server_name, url)?,
            CredentialBackend::Direct => {
                anyhow::ensure!(matches!(keyring_backend_kind, AuthKeyringBackendKind::Direct), "OAuth credential authority belongs to a different secure backend");
                load_oauth_tokens_from_direct_keyring(keyring_store, server_name, url)?
            }
            CredentialBackend::Secrets => {
                anyhow::ensure!(matches!(keyring_backend_kind, AuthKeyringBackendKind::Secrets), "OAuth credential authority belongs to a different secure backend");
                load_oauth_tokens_from_secrets_keyring(keyring_store, codex_home, server_name, url)?
            }
        }.context("committed OAuth credentials are unavailable")?;
        anyhow::ensure!(committed.fingerprint.as_deref() == Some(credential_fingerprint(&tokens)?.as_str()), "OAuth credentials do not match their committed authority");
        return Ok((Some(tokens), request_authority));
    }
    let tokens = match store_mode {
        OAuthCredentialsStoreMode::Auto => load_legacy_oauth_tokens_with_fallback(
            keyring_store,
            codex_home,
            keyring_backend_kind,
            server_name,
            url,
        ),
        OAuthCredentialsStoreMode::File => {
            load_oauth_tokens_from_file(codex_home, server_name, url)
        }
        OAuthCredentialsStoreMode::Keyring => load_oauth_tokens_from_keyring(
            keyring_store,
            codex_home,
            keyring_backend_kind,
            server_name,
            url,
        )
        .with_context(|| "failed to read OAuth tokens from keyring".to_string()),
    }?;
    Ok((tokens, request_authority))
}

pub(crate) fn oauth_token_status(
    codex_home: &Path,
    server_name: &str,
    url: &str,
    store_mode: OAuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
) -> Result<StoredOAuthTokenStatus> {
    Ok(
        match load_oauth_tokens(
            codex_home,
            server_name,
            url,
            store_mode,
            keyring_backend_kind,
        )?
        .as_ref()
        {
            None => StoredOAuthTokenStatus::Missing,
            Some(tokens) if oauth_tokens_are_usable(tokens) => StoredOAuthTokenStatus::Usable,
            Some(_) => StoredOAuthTokenStatus::AuthorizationRequired,
        },
    )
}

fn oauth_tokens_are_usable(tokens: &StoredOAuthTokens) -> bool {
    if tokens.client_id.trim().is_empty() {
        return false;
    }

    let token_response = &tokens.token_response.0;
    if token_needs_refresh(tokens.expires_at) {
        return token_response
            .refresh_token()
            .is_some_and(|token| !token.secret().trim().is_empty());
    }

    !token_response.access_token().secret().trim().is_empty()
}

fn refresh_expires_in_from_timestamp(tokens: &mut StoredOAuthTokens) {
    let Some(expires_at) = tokens.expires_at else {
        return;
    };

    match expires_in_from_timestamp(expires_at) {
        Some(seconds) => {
            let duration = Duration::from_secs(seconds);
            tokens.token_response.0.set_expires_in(Some(&duration));
        }
        None => {
            // RMCP treats a missing expiry as unknown and uses the access token
            // as-is. Treat a known-expired timestamp as an explicit zero so
            // startup refreshes the token before the first request.
            tokens
                .token_response
                .0
                .set_expires_in(Some(&Duration::ZERO));
        }
    }
}


fn load_legacy_oauth_tokens_with_fallback<K: KeyringStore + Clone + 'static>(
    keyring_store: &K, codex_home: &Path, keyring_backend_kind: AuthKeyringBackendKind, server_name: &str, url: &str,
) -> Result<Option<StoredOAuthTokens>> {
    match load_oauth_tokens_from_keyring(
        keyring_store,
        codex_home,
        keyring_backend_kind,
        server_name,
        url,
    ) {
        Ok(Some(tokens)) => Ok(Some(tokens)),
        Ok(None) => load_oauth_tokens_from_file(codex_home, server_name, url),
        // A store lock failure means the configured aggregate authority could be changing, or
        // that coordination itself is unavailable. It is not evidence that the keyring backend
        // is unavailable, so consulting File here could replay credentials hidden behind a
        // newer Secrets entry. This is the load-side counterpart of the save guard below.
        Err(error) if error.downcast_ref::<OAuthStoreLockFailure>().is_some() => Err(error),
        Err(error) => {
            warn!("failed to read OAuth tokens from keyring: {error}");
            load_oauth_tokens_from_file(codex_home, server_name, url)
                .with_context(|| format!("failed to read OAuth tokens from keyring: {error}"))
        }
    }
}

fn load_oauth_tokens_from_keyring<K: KeyringStore + Clone + 'static>(
    keyring_store: &K,
    codex_home: &Path,
    keyring_backend_kind: AuthKeyringBackendKind,
    server_name: &str,
    url: &str,
) -> Result<Option<StoredOAuthTokens>> {
    match keyring_backend_kind {
        AuthKeyringBackendKind::Direct => {
            load_oauth_tokens_from_direct_keyring(keyring_store, server_name, url)
        }
        AuthKeyringBackendKind::Secrets => {
            load_oauth_tokens_from_secrets_keyring(keyring_store, codex_home, server_name, url)
        }
    }
}

fn load_oauth_tokens_from_direct_keyring<K: KeyringStore>(
    keyring_store: &K,
    server_name: &str,
    url: &str,
) -> Result<Option<StoredOAuthTokens>> {
    let key = compute_store_key(server_name, url)?;
    match keyring_store.load(KEYRING_SERVICE, &key) {
        Ok(Some(serialized)) => {
            let mut tokens: StoredOAuthTokens = serde_json::from_str(&serialized)
                .context("failed to deserialize OAuth tokens from keyring")?;
            refresh_expires_in_from_timestamp(&mut tokens);
            Ok(Some(tokens))
        }
        Ok(None) => Ok(None),
        Err(error) => Err(Error::new(error.into_error())),
    }
}

fn load_oauth_tokens_from_secrets_keyring<K: KeyringStore + Clone + 'static>(
    keyring_store: &K,
    codex_home: &Path,
    server_name: &str,
    url: &str,
) -> Result<Option<StoredOAuthTokens>> {
    let _store_lock = OAuthStoreLock::acquire(codex_home, OAuthStore::Secrets)?;
    let backend = LocalSecretsBackend::new_with_namespace(
        codex_home.to_path_buf(),
        Arc::new(keyring_store.clone()),
        LocalSecretsNamespace::McpOAuth,
    );
    let secret_name = compute_secret_name(server_name, url)?;
    match backend
        .get(&SecretScope::Global, &secret_name)
        .context("failed to load MCP OAuth tokens from encrypted storage")?
    {
        Some(serialized) => {
            let mut tokens: StoredOAuthTokens = serde_json::from_str(&serialized)
                .context("failed to deserialize OAuth tokens from encrypted storage")?;
            refresh_expires_in_from_timestamp(&mut tokens);
            Ok(Some(tokens))
        }
        None => Ok(None),
    }
}

pub fn save_oauth_tokens(
    codex_home: &Path,
    server_name: &str,
    tokens: &StoredOAuthTokens,
    store_mode: OAuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
) -> Result<()> {
    let keyring_store = DefaultKeyringStore;
    save_oauth_tokens_coordinated(&keyring_store, codex_home, keyring_backend_kind, server_name, tokens, store_mode, None)
}

fn save_oauth_tokens_with_keyring<K: KeyringStore + Clone + 'static>(
    keyring_store: &K,
    codex_home: &Path,
    keyring_backend_kind: AuthKeyringBackendKind,
    server_name: &str,
    tokens: &StoredOAuthTokens,
) -> Result<()> {
    match keyring_backend_kind {
        AuthKeyringBackendKind::Direct => {
            save_oauth_tokens_to_direct_keyring(keyring_store, codex_home, server_name, tokens)
        }
        AuthKeyringBackendKind::Secrets => {
            save_oauth_tokens_to_secrets_keyring(keyring_store, codex_home, server_name, tokens)
        }
    }
}

fn save_oauth_tokens_to_direct_keyring<K: KeyringStore>(
    keyring_store: &K,
    codex_home: &Path,
    server_name: &str,
    tokens: &StoredOAuthTokens,
) -> Result<()> {
    let serialized = serde_json::to_string(tokens).context("failed to serialize OAuth tokens")?;

    let key = compute_store_key(server_name, &tokens.url)?;
    match keyring_store.save(KEYRING_SERVICE, &key, &serialized) {
        Ok(()) => {
            cleanup_plaintext_after_secure_save(codex_home, &key);
            Ok(())
        }
        Err(error) => {
            let message = format!(
                "failed to write OAuth tokens to keyring: {}",
                error.message()
            );
            warn!("{message}");
            Err(Error::new(error.into_error()).context(message))
        }
    }
}

/// Saves one credential while holding the Secrets aggregate-store lock across the mutation.
///
/// The Secrets lock is released before fallback File cleanup to preserve aggregate-lock ordering.
fn save_oauth_tokens_to_secrets_keyring<K: KeyringStore + Clone + 'static>(
    keyring_store: &K,
    codex_home: &Path,
    server_name: &str,
    tokens: &StoredOAuthTokens,
) -> Result<()> {
    let serialized = serde_json::to_string(tokens).context("failed to serialize OAuth tokens")?;
    {
        let _store_lock = OAuthStoreLock::acquire(codex_home, OAuthStore::Secrets)?;
        save_oauth_tokens_to_secrets_keyring_with_lock_held(
            keyring_store,
            codex_home,
            server_name,
            tokens,
            &serialized,
        )?;
    }

    let key = compute_store_key(server_name, &tokens.url)?;
    cleanup_plaintext_after_secure_save(codex_home, &key);
    Ok(())
}

fn cleanup_plaintext_after_secure_save(codex_home: &Path, key: &str) {
    if let Err(error) = delete_oauth_tokens_from_file(codex_home, key) {
        warn!("secure OAuth credentials were saved but plaintext fallback cleanup failed: {error}");
    }
}

/// Writes one credential to Secrets. The caller must hold the Secrets aggregate-store lock.
fn save_oauth_tokens_to_secrets_keyring_with_lock_held<K: KeyringStore + Clone + 'static>(
    keyring_store: &K,
    codex_home: &Path,
    server_name: &str,
    tokens: &StoredOAuthTokens,
    serialized: &str,
) -> Result<()> {
    let backend = LocalSecretsBackend::new_with_namespace(
        codex_home.to_path_buf(),
        Arc::new(keyring_store.clone()),
        LocalSecretsNamespace::McpOAuth,
    );
    let secret_name = compute_secret_name(server_name, &tokens.url)?;
    backend
        .set(&SecretScope::Global, &secret_name, serialized)
        .context("failed to write OAuth tokens to encrypted storage")
}


fn save_oauth_tokens_coordinated<K: KeyringStore + Clone + 'static>(
    keyring_store: &K, codex_home: &Path, keyring_backend_kind: AuthKeyringBackendKind,
    server_name: &str, tokens: &StoredOAuthTokens, mode: OAuthCredentialsStoreMode,
    expected: Option<&OAuthRequestAuthority>,
) -> Result<()> {
    anyhow::ensure!(server_name == tokens.server_name, "OAuth credential server identity does not match the save target");
    let _transaction = OAuthStoreLock::acquire(codex_home, OAuthStore::Authority)?;
    let mut state = read_credential_authorities(codex_home)?;
    let key = compute_store_key(server_name, &tokens.url)?;
    if let Some(expected) = expected {
        expected.check_with_lock_held(&state, &key)?;
    }
    let revision = state.entries.get(&key).map_or(0, |authority| authority.revision)
        .checked_add(1).context("MCP OAuth authority revision exhausted")?;
    let mut tokens = tokens.clone();
    tokens.expires_at = tokens.expires_at.or_else(|| compute_expires_at_millis(&tokens.token_response.0));
    let tokens = &tokens;
    let backend = if matches!(mode, OAuthCredentialsStoreMode::File) {
        save_oauth_tokens_to_file(codex_home, tokens)?;
        CredentialBackend::File
    } else { match save_oauth_tokens_with_keyring(
        keyring_store,
        codex_home,
        keyring_backend_kind,
        server_name,
        tokens,
    ) {
        Ok(()) => match keyring_backend_kind { AuthKeyringBackendKind::Direct => CredentialBackend::Direct, AuthKeyringBackendKind::Secrets => CredentialBackend::Secrets },
        // As on load, a store lock failure is a coordination failure rather than evidence that
        // the keyring backend is unavailable. Falling back could leave a newer File token hidden
        // behind a stale Secrets entry.
        Err(error)
            if error.downcast_ref::<OAuthStoreLockFailure>().is_some()
                || error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::WouldBlock) =>
        {
            return Err(error);
        }
        Err(error) => {
            if matches!(mode, OAuthCredentialsStoreMode::Keyring) { return Err(error); }
            let message = error.to_string();
            warn!("falling back to file storage for OAuth tokens: {message}");
            save_oauth_tokens_to_file(codex_home, tokens)
                .with_context(|| format!("failed to write OAuth tokens to keyring: {message}"))?;
            CredentialBackend::File
        }
    }};
    let authority = CredentialAuthority { backend: Some(backend), fingerprint: Some(credential_fingerprint(tokens)?), revision };
    state.entries.insert(key, authority.clone());
    // Publication failures are not secure-store failures and must never trigger
    // a retry to plaintext. Cleanup above is serialized with every logical save.
    commit_credential_authority(codex_home, &state)?;
    if let Some(expected) = expected {
        expected.accept_with_lock_held(authority);
    }
    Ok(())
}

pub fn delete_oauth_tokens(
    codex_home: &Path,
    server_name: &str,
    url: &str,
    store_mode: OAuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
) -> Result<bool> {
    let keyring_store = DefaultKeyringStore;
    delete_oauth_tokens_from_keyring_and_file(
        &keyring_store,
        codex_home,
        store_mode,
        keyring_backend_kind,
        server_name,
        url,
        None,
    )
}

fn delete_oauth_tokens_from_keyring_and_file<K: KeyringStore + Clone + 'static>(
    keyring_store: &K,
    codex_home: &Path,
    store_mode: OAuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
    server_name: &str,
    url: &str,
    expected: Option<&OAuthRequestAuthority>,
) -> Result<bool> {
    let _transaction = OAuthStoreLock::acquire(codex_home, OAuthStore::Authority)?;
    let mut authorities = read_credential_authorities(codex_home)?;
    let key = compute_store_key(server_name, url)?;
    if let Some(expected) = expected {
        expected.check_with_lock_held(&authorities, &key)?;
    }
    let revision = authorities.entries.get(&key).map_or(0, |authority| authority.revision)
        .checked_add(1).context("MCP OAuth authority revision exhausted")?;
    let keyring_result = delete_oauth_tokens_from_keyring(
        keyring_store,
        codex_home,
        keyring_backend_kind,
        server_name,
        url,
    );
    let keyring_removed = match keyring_result {
        Ok(removed) => removed,
        Err(error) => {
            let message = error.to_string();
            warn!("failed to delete OAuth tokens from keyring: {message}");
            match store_mode {
                OAuthCredentialsStoreMode::Auto | OAuthCredentialsStoreMode::Keyring => {
                    return Err(error).context("failed to delete OAuth tokens from keyring");
                }
                OAuthCredentialsStoreMode::File => false,
            }
        }
    };

    let file_removed = delete_oauth_tokens_from_file(codex_home, &key)?;
    let authority = CredentialAuthority { backend: None, fingerprint: None, revision };
    authorities.entries.insert(key, authority.clone());
    commit_credential_authority(codex_home, &authorities)?;
    if let Some(expected) = expected {
        expected.accept_with_lock_held(authority);
    }
    Ok(keyring_removed || file_removed)
}

fn delete_oauth_tokens_from_keyring<K: KeyringStore + Clone + 'static>(
    keyring_store: &K,
    codex_home: &Path,
    keyring_backend_kind: AuthKeyringBackendKind,
    server_name: &str,
    url: &str,
) -> Result<bool> {
    match keyring_backend_kind {
        AuthKeyringBackendKind::Direct => {
            delete_oauth_tokens_from_direct_keyring(keyring_store, server_name, url)
        }
        AuthKeyringBackendKind::Secrets => {
            let direct_removed =
                delete_oauth_tokens_from_direct_keyring(keyring_store, server_name, url)?;
            let secrets_removed = delete_oauth_tokens_from_secrets_keyring(
                keyring_store,
                codex_home,
                server_name,
                url,
            )?;
            Ok(direct_removed || secrets_removed)
        }
    }
}

fn delete_oauth_tokens_from_direct_keyring<K: KeyringStore>(
    keyring_store: &K,
    server_name: &str,
    url: &str,
) -> Result<bool> {
    let key = compute_store_key(server_name, url)?;
    keyring_store
        .delete(KEYRING_SERVICE, &key)
        .map_err(|error| Error::new(error.into_error()))
}

fn delete_oauth_tokens_from_secrets_keyring<K: KeyringStore + Clone + 'static>(
    keyring_store: &K,
    codex_home: &Path,
    server_name: &str,
    url: &str,
) -> Result<bool> {
    let _store_lock = OAuthStoreLock::acquire(codex_home, OAuthStore::Secrets)?;
    let backend = LocalSecretsBackend::new_with_namespace(
        codex_home.to_path_buf(),
        Arc::new(keyring_store.clone()),
        LocalSecretsNamespace::McpOAuth,
    );
    let secret_name = compute_secret_name(server_name, url)?;
    let secrets_removed = backend
        .delete(&SecretScope::Global, &secret_name)
        .context("failed to delete OAuth tokens from encrypted storage")?;
    Ok(secrets_removed)
}

#[derive(Clone)]
pub(crate) struct OAuthPersistor {
    inner: Arc<OAuthPersistorInner>,
}

struct OAuthPersistorInner {
    credential_store: InMemoryCredentialStore,
    persistence: OAuthPersistenceState,
    authority: OAuthRequestAuthority,
    retrying: std::sync::atomic::AtomicBool,
    retry_generation: std::sync::atomic::AtomicU64,
}

enum OAuthPersistenceOperation {
    Save {
        server_name: String,
        tokens: Box<StoredOAuthTokens>,
        store_mode: OAuthCredentialsStoreMode,
        keyring_backend_kind: AuthKeyringBackendKind,
    },
    Delete {
        server_name: String,
        url: String,
        store_mode: OAuthCredentialsStoreMode,
        keyring_backend_kind: AuthKeyringBackendKind,
    },
}

impl OAuthPersistenceOperation {
    fn execute(self, codex_home: &Path, authority: &OAuthRequestAuthority) -> Result<()> {
        match self {
            Self::Save {
                server_name,
                tokens,
                store_mode,
                keyring_backend_kind,
            } => save_oauth_tokens_coordinated(
                &DefaultKeyringStore, codex_home, keyring_backend_kind,
                &server_name, &tokens, store_mode, Some(authority),
            ),
            Self::Delete {
                server_name,
                url,
                store_mode,
                keyring_backend_kind,
            } => delete_oauth_tokens_from_keyring_and_file(
                &DefaultKeyringStore, codex_home, store_mode, keyring_backend_kind,
                &server_name, &url, Some(authority),
            )
            .map(drop),
        }
    }
}

type OAuthPersistenceExecutor = Arc<dyn Fn(OAuthPersistenceOperation) -> Result<()> + Send + Sync>;

struct OAuthPersistenceState {
    server_name: String,
    url: String,
    store_mode: OAuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
    operation_lock: Arc<Mutex<()>>,
    last_credentials: Arc<Mutex<Option<StoredOAuthTokens>>>,
    pending_write: Arc<std::sync::Mutex<Option<PendingOAuthWrite>>>,
    next_write_generation: std::sync::atomic::AtomicU64,
    executor: OAuthPersistenceExecutor,
}

struct PendingOAuthWrite {
    generation: u64,
    credentials: Option<StoredOAuthTokens>,
}

struct PendingOAuthWriteGuard {
    pending: Arc<std::sync::Mutex<Option<PendingOAuthWrite>>>,
    generation: u64,
}

impl Drop for PendingOAuthWriteGuard {
    fn drop(&mut self) {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if pending
            .as_ref()
            .is_some_and(|write| write.generation == self.generation)
        {
            *pending = None;
        }
    }
}

impl OAuthPersistenceState {
    fn new(
        server_name: String,
        url: String,
        store_mode: OAuthCredentialsStoreMode,
        keyring_backend_kind: AuthKeyringBackendKind,
        initial_credentials: Option<StoredOAuthTokens>,
        executor: OAuthPersistenceExecutor,
    ) -> Self {
        Self {
            server_name,
            url,
            store_mode,
            keyring_backend_kind,
            operation_lock: Arc::new(Mutex::new(())),
            last_credentials: Arc::new(Mutex::new(initial_credentials)),
            pending_write: Arc::default(),
            next_write_generation: std::sync::atomic::AtomicU64::new(0),
            executor,
        }
    }


    async fn persist_with_guard(
        &self,
        client_id: String,
        maybe_credentials: Option<OAuthTokenResponse>,
        operation_guard: tokio::sync::OwnedMutexGuard<()>,
        custody: OAuthPersistor,
    ) -> Result<()> {
        match maybe_credentials {
            Some(credentials) => {
                let previous = self.last_credentials.lock().await.clone();
                let new_token_response = WrappedOAuthTokenResponse(credentials.clone());
                let same_token = previous
                    .as_ref()
                    .map(|previous| previous.token_response == new_token_response)
                    .unwrap_or(false);
                let expires_at = if same_token {
                    previous.as_ref().and_then(|previous| previous.expires_at)
                } else {
                    compute_expires_at_millis(&credentials)
                };
                let stored = StoredOAuthTokens {
                    server_name: self.server_name.clone(),
                    url: self.url.clone(),
                    client_id,
                    token_response: new_token_response,
                    expires_at,
                };
                if !same_token
                    || !previous.as_ref().is_some_and(|previous| {
                        previous.server_name == stored.server_name
                            && previous.url == stored.url
                            && previous.client_id == stored.client_id
                            && previous.expires_at == stored.expires_at
                    })
                {
                    self.execute(
                        OAuthPersistenceOperation::Save {
                            server_name: self.server_name.clone(),
                            tokens: Box::new(stored),
                            store_mode: self.store_mode,
                            keyring_backend_kind: self.keyring_backend_kind,
                        },
                        operation_guard,
                        custody,
                    )
                    .await?;
                }
            }
            None => {
                if self.last_credentials.lock().await.is_some() {
                    self
                        .execute(
                            OAuthPersistenceOperation::Delete {
                                server_name: self.server_name.clone(),
                                url: self.url.clone(),
                                store_mode: self.store_mode,
                                keyring_backend_kind: self.keyring_backend_kind,
                            },
                            operation_guard,
                            custody,
                        )
                        .await?;
                }
            }
        }

        Ok(())
    }

    async fn execute(
        &self,
        operation: OAuthPersistenceOperation,
        operation_guard: tokio::sync::OwnedMutexGuard<()>,
        custody: OAuthPersistor,
    ) -> Result<()> {
        let executor = Arc::clone(&self.executor);
        let last_credentials = Arc::clone(&self.last_credentials);
        let next_credentials = match &operation {
            OAuthPersistenceOperation::Save { tokens, .. } => Some(*tokens.clone()),
            OAuthPersistenceOperation::Delete { .. } => None,
        };
        let generation = self
            .next_write_generation
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        *self
            .pending_write
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(PendingOAuthWrite {
            generation,
            credentials: next_credentials.clone(),
        });
        let pending_guard = PendingOAuthWriteGuard {
            pending: Arc::clone(&self.pending_write),
            generation,
        };
        tokio::task::spawn_blocking(move || {
            let _operation_guard = operation_guard;
            let _pending_guard = pending_guard;
            executor(operation).inspect_err(|error| {
                // The original observer may have timed out or coalesced. The
                // admitted owner must still surface a late durability failure.
                warn!(%error, "OAuth credential persistence failed");
                custody.retain_for_retry();
            })?;
            // The durable write and bookkeeping share ownership even if the waiter is dropped.
            *last_credentials.blocking_lock() = next_credentials;
            Ok(())
        })
        .await
        .context("OAuth credential persistence task failed")?
    }
}

impl OAuthPersistor {

    pub(crate) fn new(
        server_name: String,
        url: String,
        codex_home: PathBuf,
        credential_store: InMemoryCredentialStore,
        store_mode: OAuthCredentialsStoreMode,
        keyring_backend_kind: AuthKeyringBackendKind,
        initial_credentials: Option<StoredOAuthTokens>,
        authority: OAuthRequestAuthority,
    ) -> Self {
        let write_authority = authority.clone();
        let executor: OAuthPersistenceExecutor =
            Arc::new(move |operation| operation.execute(&codex_home, &write_authority));
        Self {
            inner: Arc::new(OAuthPersistorInner {
                credential_store,
                authority,
                retrying: std::sync::atomic::AtomicBool::new(false),
                retry_generation: std::sync::atomic::AtomicU64::new(0),
                persistence: OAuthPersistenceState::new(
                    server_name,
                    url,
                    store_mode,
                    keyring_backend_kind,
                    initial_credentials,
                    executor,
                ),
            }),
        }
    }

    /// Bound the caller's wait, not the lifetime of an admitted disk write.
    /// Before admission, credentials remain in the shared credential store for a
    /// later attempt. After admission, execute() owns the write lock and its
    /// bookkeeping in spawn_blocking even if this observer times out.
    pub(crate) async fn persist_if_needed(&self) -> Result<()> {
        match tokio::time::timeout(OAUTH_PERSISTENCE_TIMEOUT, self.persist_latest()).await {
            Ok(result) => result.map_err(|error| {
                self.retain_for_retry();
                OAuthPersistenceError::Failed(error).into()
            }),
            Err(_) => Err(OAuthPersistenceError::Pending.into()),
        }
    }

    /// Post-result observation may join an already admitted generation without
    /// awaiting the same slow write again. This is not a durability receipt:
    /// initialize/recovery keep persist_if_needed's completion boundary.
    pub(crate) async fn observe_after_operation(&self) -> Result<()> {
        let result = tokio::time::timeout(OAUTH_PERSISTENCE_TIMEOUT, async {
            let stored = self.inner.credential_store.load().await?;
            let already_admitted = {
                let pending = self
                    .inner
                    .persistence
                    .pending_write
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                pending
                    .as_ref()
                    .is_some_and(|write| match (&write.credentials, &stored) {
                        (Some(admitted), Some(current)) => {
                            current.token_response.as_ref().is_some_and(|token| {
                                admitted.client_id == current.client_id
                                    && admitted.token_response
                                        == WrappedOAuthTokenResponse(token.clone())
                            })
                        }
                        (None, None) => true,
                        (None, Some(current)) => current.token_response.is_none(),
                        _ => false,
                    })
            };
            if already_admitted {
                return Err(OAuthPersistenceError::Pending.into());
            }
            // Sample again after acquiring write order. A differing old save or
            // delete must finish before we decide that the latest state is saved.
            self.persist_latest().await
        })
        .await;
        match result {
            Ok(Err(error)) if error.downcast_ref::<OAuthPersistenceError>().is_some() => Err(error),
            Ok(result) => result.map_err(|error| {
                self.retain_for_retry();
                OAuthPersistenceError::Failed(error).into()
            }),
            Err(_) => Err(OAuthPersistenceError::Pending.into()),
        }
    }

    /// A failed startup may drop its client. Keep one persistence-only owner of
    /// replacement credentials until storage recovers or a newer login/logout
    /// fences it. Retries use the existing write admission and never replay MCP
    /// or OAuth network operations.
    fn retain_for_retry(&self) {
        use std::sync::atomic::Ordering;
        self.inner.retry_generation.fetch_add(1, Ordering::AcqRel);
        if self.inner.retrying.swap(true, Ordering::AcqRel) {
            return;
        }
        let runtime = self.clone();
        tokio::spawn(async move {
            let mut delay = Duration::from_secs(1);
            loop {
                tokio::time::sleep(delay).await;
                if let Err(error) = runtime.inner.authority.check().await {
                    if error.downcast_ref::<OAuthAuthorityChanged>().is_some() {
                        runtime.inner.retrying.store(false, Ordering::Release);
                        return;
                    }
                    delay = (delay * 2).min(Duration::from_secs(30));
                    continue;
                }
                let generation = runtime.inner.retry_generation.load(Ordering::Acquire);
                if runtime.persist_latest().await.is_ok() {
                    runtime.inner.retrying.store(false, Ordering::Release);
                    // A concurrent failed generation must not lose its retry
                    // while the successful owner was releasing write order.
                    if runtime.inner.retry_generation.load(Ordering::Acquire) != generation {
                        runtime.retain_for_retry();
                    }
                    return;
                }
                delay = (delay * 2).min(Duration::from_secs(30));
            }
        });
    }

    /// Persists the latest stored credentials if they have changed.
    /// Deletes the credentials if they are no longer present.
    async fn persist_latest(&self) -> Result<()> {
        let operation_guard = Arc::clone(&self.inner.persistence.operation_lock)
            .lock_owned()
            .await;
        // Sample after write admission, but never join a network refresh just
        // to persist the last committed credentials of a completed operation.
        let (client_id, maybe_credentials) = self
            .inner
            .credential_store
            .load()
            .await?
            .map(|stored| (stored.client_id, stored.token_response))
            .unwrap_or_default();

        self.inner
            .persistence
            .persist_with_guard(client_id, maybe_credentials, operation_guard, self.clone())
            .await
    }
}

const OAUTH_PERSISTENCE_TIMEOUT: Duration = Duration::from_secs(1);

const FALLBACK_FILENAME: &str = ".credentials.json";
const MCP_SERVER_TYPE: &str = "http";

type FallbackFile = BTreeMap<String, FallbackTokenEntry>;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FallbackTokenEntry {
    server_name: String,
    server_url: String,
    client_id: String,
    access_token: String,
    #[serde(default)]
    expires_at: Option<u64>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    scopes: Vec<String>,
}

fn load_oauth_tokens_from_file(
    codex_home: &Path,
    server_name: &str,
    url: &str,
) -> Result<Option<StoredOAuthTokens>> {
    let _store_lock = OAuthStoreLock::acquire(codex_home, OAuthStore::File)?;
    let Some(store) = read_fallback_file_unlocked(codex_home)? else {
        return Ok(None);
    };

    let key = compute_store_key(server_name, url)?;

    for entry in store.get(&key).into_iter().chain(store.values()) {
        let entry_key = compute_store_key(&entry.server_name, &entry.server_url)?;
        if entry_key != key {
            continue;
        }

        let mut token_response = OAuthTokenResponse::new(
            AccessToken::new(entry.access_token.clone()),
            BasicTokenType::Bearer,
            VendorExtraTokenFields::default(),
        );

        if let Some(refresh) = entry.refresh_token.clone() {
            token_response.set_refresh_token(Some(RefreshToken::new(refresh)));
        }

        let scopes = entry.scopes.clone();
        if !scopes.is_empty() {
            token_response.set_scopes(Some(scopes.into_iter().map(Scope::new).collect()));
        }

        let mut stored = StoredOAuthTokens {
            server_name: entry.server_name.clone(),
            url: entry.server_url.clone(),
            client_id: entry.client_id.clone(),
            token_response: WrappedOAuthTokenResponse(token_response),
            expires_at: entry.expires_at,
        };
        refresh_expires_in_from_timestamp(&mut stored);

        return Ok(Some(stored));
    }

    Ok(None)
}

/// Saves one credential while holding the File aggregate-store lock across the full
/// read-modify-write operation.
fn save_oauth_tokens_to_file(codex_home: &Path, tokens: &StoredOAuthTokens) -> Result<()> {
    let _store_lock = OAuthStoreLock::acquire(codex_home, OAuthStore::File)?;
    save_oauth_tokens_to_file_with_lock_held(codex_home, tokens)
}

/// Updates the fallback File. The caller must hold the File aggregate-store lock.
fn save_oauth_tokens_to_file_with_lock_held(
    codex_home: &Path,
    tokens: &StoredOAuthTokens,
) -> Result<()> {
    let key = compute_store_key(&tokens.server_name, &tokens.url)?;
    let mut store = read_fallback_file_unlocked(codex_home)?.unwrap_or_default();

    let token_response = &tokens.token_response.0;
    let expires_at = tokens
        .expires_at
        .or_else(|| compute_expires_at_millis(token_response));
    let refresh_token = token_response
        .refresh_token()
        .map(|token| token.secret().to_string());
    let scopes = token_response
        .scopes()
        .map(|s| s.iter().map(|s| s.to_string()).collect())
        .unwrap_or_default();
    let entry = FallbackTokenEntry {
        server_name: tokens.server_name.clone(),
        server_url: tokens.url.clone(),
        client_id: tokens.client_id.clone(),
        access_token: token_response.access_token().secret().to_string(),
        expires_at,
        refresh_token,
        scopes,
    };

    remove_fallback_identity(&mut store, &key)?;
    store.insert(key, entry);
    write_fallback_file(codex_home, &store)
}

fn delete_oauth_tokens_from_file(codex_home: &Path, key: &str) -> Result<bool> {
    let _store_lock = OAuthStoreLock::acquire(codex_home, OAuthStore::File)?;
    let mut store = match read_fallback_file_unlocked(codex_home)? {
        Some(store) => store,
        None => return Ok(false),
    };

    let removed = remove_fallback_identity(&mut store, key)?;

    if removed {
        write_fallback_file(codex_home, &store)?;
    }

    Ok(removed)
}

// Legacy files may store a credential under an alias. Mutations must remove
// every identity accepted by the loader, including plaintext secure-save cleanup.
fn remove_fallback_identity(store: &mut FallbackFile, key: &str) -> Result<bool> {
    let aliases = store
        .iter()
        .map(|(stored_key, entry)| {
            Ok((
                stored_key.clone(),
                stored_key == key
                    || compute_store_key(&entry.server_name, &entry.server_url)? == key,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut removed = false;
    for (alias, matches) in aliases {
        if matches {
            removed |= store.remove(&alias).is_some();
        }
    }
    Ok(removed)
}

pub(crate) fn compute_expires_at_millis(response: &OAuthTokenResponse) -> Option<u64> {
    let expires_in = response.expires_in()?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0));
    let expiry = now.checked_add(expires_in)?;
    let millis = expiry.as_millis();
    if millis > u128::from(u64::MAX) {
        Some(u64::MAX)
    } else {
        Some(millis as u64)
    }
}

fn expires_in_from_timestamp(expires_at: u64) -> Option<u64> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0));
    let now_ms = now.as_millis() as u64;

    if expires_at <= now_ms {
        None
    } else {
        Some((expires_at - now_ms) / 1000)
    }
}

fn token_needs_refresh(expires_at: Option<u64>) -> bool {
    let Some(expires_at) = expires_at else {
        return false;
    };

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_millis() as u64;

    now.saturating_add(REFRESH_SKEW_MILLIS) >= expires_at
}

fn compute_store_key(server_name: &str, server_url: &str) -> Result<String> {
    let mut payload = JsonMap::new();
    payload.insert(
        "type".to_string(),
        Value::String(MCP_SERVER_TYPE.to_string()),
    );
    payload.insert("url".to_string(), Value::String(server_url.to_string()));
    payload.insert("headers".to_string(), Value::Object(JsonMap::new()));

    let truncated = sha_256_prefix(&Value::Object(payload))?;
    Ok(format!("{server_name}|{truncated}"))
}

/// Derive a valid secret-store name from the MCP OAuth store key.
///
/// `compute_store_key` intentionally includes readable identity components and
/// a pipe separator, but `SecretName` only allows `A-Z`, `0-9`, and `_`.
/// Re-hashing keeps the secret key deterministic while satisfying that
/// restricted alphabet.
fn compute_secret_name(server_name: &str, server_url: &str) -> Result<SecretName> {
    let key = compute_store_key(server_name, server_url)?;
    let mut hasher = Sha256::new();
    hasher.update(key.as_bytes());
    let digest = hasher.finalize();
    let hex = hex::encode_upper(digest);
    SecretName::new(&format!("{MCP_OAUTH_SECRET_PREFIX}_{}", &hex[..32]))
}

fn fallback_file_path(codex_home: &Path) -> PathBuf {
    codex_home.join(FALLBACK_FILENAME)
}

fn read_fallback_file_unlocked(codex_home: &Path) -> Result<Option<FallbackFile>> {
    let path = fallback_file_path(codex_home);
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(err).context(format!(
                "failed to read credentials file at {}",
                path.display()
            ));
        }
    };

    match serde_json::from_str::<FallbackFile>(&contents) {
        Ok(store) => Ok(Some(store)),
        Err(e) => Err(e).context(format!(
            "failed to parse credentials file at {}",
            path.display()
        )),
    }
}

fn write_fallback_file(codex_home: &Path, store: &FallbackFile) -> Result<()> {
    let path = fallback_file_path(codex_home);

    if store.is_empty() {
        if path.exists() {
            fs::remove_file(path)?;
        }
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let serialized = serde_json::to_string(store)?;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("credentials path has no parent: {}", path.display()))?;
    let mut temporary = NamedTempFile::new_in(parent)?;

    temporary.write_all(serialized.as_bytes())?;
    temporary.flush()?;
    temporary.as_file().sync_all()?;
    temporary.persist(&path).map_err(|error| error.error)?;

    Ok(())
}

fn sha_256_prefix(value: &Value) -> Result<String> {
    let serialized =
        serde_json::to_string(&value).context("failed to serialize MCP OAuth key payload")?;
    let mut hasher = Sha256::new();
    hasher.update(serialized.as_bytes());
    let hex = hex::encode(hasher.finalize());
    let truncated = &hex[..16];
    Ok(truncated.to_string())
}

