use chrono::DateTime;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use std::collections::HashMap;
use std::fmt::Debug;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use tracing::warn;

use super::BedrockApiKeyAuth;
use crate::token_data::TokenData;
use codex_agent_identity::AgentIdentityJwtClaims;
use codex_agent_identity::parse_unverified_agent_identity_jwt;
use codex_config::types::AuthCredentialsStoreMode;
pub use codex_config::types::AuthKeyringBackendKind;
use codex_keyring_store::DefaultKeyringStore;
use codex_keyring_store::KeyringStore;
use codex_protocol::account::PlanType as AccountPlanType;
use codex_protocol::auth::AuthMode;
use codex_secrets::LocalSecretsNamespace;
use codex_secrets::SecretName;
use codex_secrets::SecretScope;
use codex_secrets::SecretsBackendKind;
use codex_secrets::SecretsManager;
use once_cell::sync::Lazy;

/// Expected structure for $CODEX_HOME/auth.json.
#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
pub struct AuthDotJson {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_context: Option<OAuthContext>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_mode: Option<AuthMode>,

    #[serde(rename = "OPENAI_API_KEY")]
    pub openai_api_key: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<TokenData>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_refresh: Option<DateTime<Utc>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_identity: Option<AgentIdentityStorage>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub personal_access_token: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bedrock_api_key: Option<BedrockApiKeyAuth>,
}

/// Explicit non-default OAuth provenance. Legacy records retain the default
/// issuer/client interpretation and the existing test endpoint overrides.
#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct OAuthContext {
    pub issuer: String,
    pub client_id: String,
}

impl OAuthContext {
    pub fn endpoint(&self, path: &str) -> std::io::Result<String> {
        let issuer = url::Url::parse(&self.issuer).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid stored OAuth issuer",
            )
        })?;
        if !matches!(issuer.scheme(), "https" | "http")
            || issuer.host_str().is_none()
            || !issuer.username().is_empty()
            || issuer.password().is_some()
            || issuer.query().is_some()
            || issuer.fragment().is_some()
            || self.client_id.trim().is_empty()
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid stored OAuth context",
            ));
        }
        Ok(format!("{}{path}", self.issuer.trim_end_matches('/')))
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(untagged)]
pub enum AgentIdentityStorage {
    Jwt(String),
    Record(AgentIdentityAuthRecord),
}

impl AgentIdentityStorage {
    pub fn has_auth_material(&self) -> bool {
        match self {
            Self::Jwt(jwt) => !jwt.trim().is_empty(),
            Self::Record(record) => {
                !record.agent_runtime_id.trim().is_empty()
                    && !record.agent_private_key.trim().is_empty()
            }
        }
    }

    pub(crate) fn as_record(&self) -> Option<&AgentIdentityAuthRecord> {
        match self {
            Self::Jwt(_) => None,
            Self::Record(record) => Some(record),
        }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct AgentIdentityAuthRecord {
    /// Auth-service base URL that issued this signing identity. Legacy records
    /// remain readable, but managed bootstrap must register again before reuse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issuer_origin: Option<String>,
    pub agent_runtime_id: String,
    pub agent_private_key: String,
    pub account_id: String,
    pub chatgpt_user_id: String,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_non_empty_string",
        serialize_with = "serialize_optional_string_as_empty"
    )]
    pub email: Option<String>,
    pub plan_type: AccountPlanType,
    pub chatgpt_account_is_fedramp: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
}

fn deserialize_optional_non_empty_string<'de, D>(
    deserializer: D,
) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer).map(|value| value.filter(|value| !value.is_empty()))
}

fn serialize_optional_string_as_empty<S>(
    value: &Option<String>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    value.as_deref().unwrap_or_default().serialize(serializer)
}

impl AgentIdentityAuthRecord {
    pub(crate) fn from_agent_identity_jwt(jwt: &str) -> std::io::Result<Self> {
        let claims = parse_unverified_agent_identity_jwt(jwt).map_err(std::io::Error::other)?;

        Ok(claims.into())
    }
}

impl From<AgentIdentityJwtClaims> for AgentIdentityAuthRecord {
    fn from(claims: AgentIdentityJwtClaims) -> Self {
        Self {
            agent_runtime_id: claims.agent_runtime_id,
            issuer_origin: None,
            agent_private_key: claims.agent_private_key,
            account_id: claims.account_id,
            chatgpt_user_id: claims.chatgpt_user_id,
            email: claims.email,
            plan_type: claims.plan_type.into(),
            chatgpt_account_is_fedramp: claims.chatgpt_account_is_fedramp,
            task_id: None,
        }
    }
}

pub(super) fn get_auth_file(codex_home: &Path) -> PathBuf {
    codex_home.join("auth.json")
}

pub(super) fn delete_file_if_exists(codex_home: &Path) -> std::io::Result<bool> {
    let auth_file = get_auth_file(codex_home);
    match std::fs::remove_file(&auth_file) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err),
    }
}

pub(super) trait AuthStorageBackend: Debug + Send + Sync {
    fn lifecycle_id(&self) -> Option<String> {
        None
    }
    fn load(&self) -> std::io::Result<Option<AuthDotJson>>;
    fn save(&self, auth: &AuthDotJson) -> std::io::Result<()>;
    fn delete(&self) -> std::io::Result<bool>;
    fn delete_if_lifecycle_unchanged(&self, expected: Option<&AuthDotJson>) -> std::io::Result<bool> {
        if self.load()?.as_ref() != expected {
            return Err(auth_changed());
        }
        self.delete()
    }
    fn save_if_unchanged(
        &self,
        expected: Option<&AuthDotJson>,
        auth: &AuthDotJson,
    ) -> std::io::Result<()> {
        if self.load()?.as_ref() != expected {
            return Err(auth_changed());
        }
        self.save(auth)
    }
    fn prepare_replacement(&self) -> std::io::Result<()> {
        Ok(())
    }
    fn replace_if_unchanged(&self, auth: &AuthDotJson) -> std::io::Result<()> {
        self.save(auth)
    }
}

#[derive(Debug, thiserror::Error)]
#[error("authentication changed before credential commit")]
struct AuthCommitChanged;

fn auth_changed() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::PermissionDenied, AuthCommitChanged)
}

pub(super) fn is_auth_changed(error: &std::io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(<dyn std::error::Error + std::marker::Send + std::marker::Sync + 'static>::is::<AuthCommitChanged>)
}

// Lock order: lifecycle transaction -> backend writer. No HTTP or manager/cache
// lock is held here. Contention is explicit, never an unbounded runtime wait.
#[derive(Debug)]
struct TransactionalAuthStorage {
    inner: Arc<dyn AuthStorageBackend>,
    home: PathBuf,
    ephemeral: bool,
    observed: Mutex<Option<String>>,
}

static EPHEMERAL_REVISIONS: Lazy<Mutex<HashMap<String, String>>> = Lazy::new(|| Mutex::new(HashMap::new()));

impl TransactionalAuthStorage {
    fn transaction<T>(
        &self,
        action: impl FnOnce(&mut String) -> std::io::Result<T>,
    ) -> std::io::Result<T> {
        if self.ephemeral {
            let key = compute_store_key(&self.home)?;
            let mut revisions = EPHEMERAL_REVISIONS
                .lock()
                .map_err(|_| std::io::Error::other("auth lifecycle lock poisoned"))?;
            return action(revisions.entry(key).or_default());
        }
        let path = self.home.join("auth-lifecycle");
        let _lock = codex_file_system::try_acquire_atomic_write_lock(&path)?.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "credential transaction is busy",
            )
        })?;
        let mut revision = match std::fs::read_to_string(&path) {
            Ok(value) => value,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(err) => return Err(err),
        };
        action(&mut revision)
    }

    fn advance(&self, revision: &mut String) -> std::io::Result<()> {
        let next = format!("{:032x}", rand::random::<u128>());
        if !self.ephemeral {
            codex_file_system::write_atomically(&self.home.join("auth-lifecycle"), &next)?;
        }
        *revision = next.clone();
        *self
            .observed
            .lock()
            .map_err(|_| std::io::Error::other("auth snapshot lock poisoned"))? = Some(next);
        Ok(())
    }
}

impl AuthStorageBackend for TransactionalAuthStorage {
    fn lifecycle_id(&self) -> Option<String> {
        self.observed
            .lock()
            .ok()
            .and_then(|revision| revision.clone())
    }
    fn prepare_replacement(&self) -> std::io::Result<()> {
        self.transaction(|revision| {
            *self
                .observed
                .lock()
                .map_err(|_| std::io::Error::other("auth snapshot lock poisoned"))? =
                Some(revision.clone());
            Ok(())
        })
    }

    fn replace_if_unchanged(&self, auth: &AuthDotJson) -> std::io::Result<()> {
        self.transaction(|revision| {
            let observed = self
                .observed
                .lock()
                .map_err(|_| std::io::Error::other("auth snapshot lock poisoned"))?
                .clone();
            if observed.as_deref() != Some(revision.as_str()) {
                return Err(auth_changed());
            }
            self.advance(revision)?;
            self.inner.save(auth)
        })
    }

    fn load(&self) -> std::io::Result<Option<AuthDotJson>> {
        self.transaction(|revision| {
            let auth = self.inner.load()?;
            *self
                .observed
                .lock()
                .map_err(|_| std::io::Error::other("auth snapshot lock poisoned"))? =
                Some(revision.clone());
            Ok(auth)
        })
    }

    fn save(&self, auth: &AuthDotJson) -> std::io::Result<()> {
        self.transaction(|revision| {
            // Invalidate old work even if a backend partially writes then fails.
            self.advance(revision)?;
            self.inner.save(auth)
        })
    }

    fn delete(&self) -> std::io::Result<bool> {
        self.transaction(|revision| {
            self.advance(revision)?;
            self.inner.delete()
        })
    }

    fn delete_if_lifecycle_unchanged(&self, expected: Option<&AuthDotJson>) -> std::io::Result<bool> {
        self.transaction(|revision| {
            let observed = self
                .observed
                .lock()
                .map_err(|_| std::io::Error::other("auth snapshot lock poisoned"))?
                .clone();
            // Managed refreshes keep their lifecycle and must still be
            // retired. Runtime external credentials also have direct
            // process-local writers, so fence their payload separately.
            if observed.as_deref() != Some(revision.as_str())
                || (self.ephemeral && self.inner.load()?.as_ref() != expected)
            {
                return Err(auth_changed());
            }
            self.advance(revision)?;
            self.inner.delete()
        })
    }

    fn save_if_unchanged(
        &self,
        expected: Option<&AuthDotJson>,
        auth: &AuthDotJson,
    ) -> std::io::Result<()> {
        self.transaction(|revision| {
            let observed = self
                .observed
                .lock()
                .map_err(|_| std::io::Error::other("auth snapshot lock poisoned"))?
                .clone();
            if observed.as_deref() != Some(revision.as_str())
                || self.inner.load()?.as_ref() != expected
            {
                return Err(auth_changed());
            }
            // A conditional update retains its lifecycle; payload comparison
            // fences overlapping refresh/bootstrap updates within that lifecycle.
            self.inner.save(auth)
        })
    }
}

#[derive(Clone, Debug)]
pub(super) struct FileAuthStorage {
    codex_home: PathBuf,
}

impl FileAuthStorage {
    pub(super) fn new(codex_home: PathBuf) -> Self {
        Self { codex_home }
    }

    /// Attempt to read and parse the `auth.json` file in the given `CODEX_HOME` directory.
    /// Returns the full AuthDotJson structure.
    pub(super) fn try_read_auth_json(&self, auth_file: &Path) -> std::io::Result<AuthDotJson> {
        let mut file = File::open(auth_file)?;
        let mut contents = String::new();
        file.read_to_string(&mut contents)?;
        let auth_dot_json: AuthDotJson = serde_json::from_str(&contents)?;

        Ok(auth_dot_json)
    }
}

impl AuthStorageBackend for FileAuthStorage {
    fn load(&self) -> std::io::Result<Option<AuthDotJson>> {
        let auth_file = get_auth_file(&self.codex_home);
        let auth_dot_json = match self.try_read_auth_json(&auth_file) {
            Ok(auth) => auth,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err),
        };
        Ok(Some(auth_dot_json))
    }

    fn save(&self, auth_dot_json: &AuthDotJson) -> std::io::Result<()> {
        let auth_file = get_auth_file(&self.codex_home);

        if let Some(parent) = auth_file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json_data = serde_json::to_string_pretty(auth_dot_json)?;
        let paths = codex_file_system::resolve_symlink_write_paths(&auth_file)?;
        codex_file_system::write_bytes_atomically_preserving_permissions(
            &paths.write_path, json_data.as_bytes(),
        )
    }

    fn delete(&self) -> std::io::Result<bool> {
        delete_file_if_exists(&self.codex_home)
    }
}

static CODEX_AUTH_SECRET_NAME: Lazy<SecretName> =
    Lazy::new(|| match SecretName::new("CODEX_AUTH") {
        Ok(name) => name,
        Err(err) => unreachable!("CODEX_AUTH should be a valid secret name: {err}"),
    });
const KEYRING_SERVICE: &str = "Codex Auth";

// turns codex_home path into a stable, short key string
fn compute_store_key(codex_home: &Path) -> std::io::Result<String> {
    let canonical = codex_home
        .canonicalize()
        .unwrap_or_else(|_| codex_home.to_path_buf());
    let path_str = canonical.to_string_lossy();
    let mut hasher = Sha256::new();
    hasher.update(path_str.as_bytes());
    let hex = hex::encode(hasher.finalize());
    let truncated = hex.get(..16).unwrap_or(&hex);
    Ok(format!("cli|{truncated}"))
}

#[derive(Clone, Debug)]
struct DirectKeyringAuthStorage {
    codex_home: PathBuf,
    keyring_store: Arc<dyn KeyringStore>,
}

impl DirectKeyringAuthStorage {
    fn new(codex_home: PathBuf, keyring_store: Arc<dyn KeyringStore>) -> Self {
        Self {
            codex_home,
            keyring_store,
        }
    }

    fn load_from_keyring(&self, key: &str) -> std::io::Result<Option<AuthDotJson>> {
        match self.keyring_store.load(KEYRING_SERVICE, key) {
            Ok(Some(serialized)) => serde_json::from_str(&serialized).map(Some).map_err(|err| {
                std::io::Error::other(format!(
                    "failed to deserialize CLI auth from keyring: {err}"
                ))
            }),
            Ok(None) => Ok(None),
            Err(error) => {
                let error = std::io::Error::from(error);
                Err(std::io::Error::new(
                    error.kind(),
                    format!("failed to load CLI auth from keyring: {error}"),
                ))
            }
        }
    }

    fn save_to_keyring(&self, key: &str, value: &str) -> std::io::Result<()> {
        match self.keyring_store.save(KEYRING_SERVICE, key, value) {
            Ok(()) => Ok(()),
            Err(error) => {
                let message = format!(
                    "failed to write OAuth tokens to keyring: {}",
                    error.message()
                );
                warn!("{message}");
                let error = std::io::Error::from(error);
                Err(std::io::Error::new(error.kind(), message))
            }
        }
    }
}

impl AuthStorageBackend for DirectKeyringAuthStorage {
    fn load(&self) -> std::io::Result<Option<AuthDotJson>> {
        let key = compute_store_key(&self.codex_home)?;
        self.load_from_keyring(&key)
    }

    fn save(&self, auth: &AuthDotJson) -> std::io::Result<()> {
        let key = compute_store_key(&self.codex_home)?;
        // Simpler error mapping per style: prefer method reference over closure
        let serialized = serde_json::to_string(auth).map_err(std::io::Error::other)?;
        self.save_to_keyring(&key, &serialized)?;
        delete_file_if_exists(&self.codex_home)?;
        Ok(())
    }

    fn delete(&self) -> std::io::Result<bool> {
        let key = compute_store_key(&self.codex_home)?;
        let keyring_removed = self
            .keyring_store
            .delete(KEYRING_SERVICE, &key)
            .map_err(|err| {
                std::io::Error::other(format!("failed to delete auth from keyring: {err}"))
            })?;
        let file_removed = delete_file_if_exists(&self.codex_home)?;
        Ok(keyring_removed || file_removed)
    }
}

#[derive(Clone)]
struct SecretsKeyringAuthStorage {
    codex_home: PathBuf,
    direct_storage: DirectKeyringAuthStorage,
    secrets_manager: SecretsManager,
}

impl Debug for SecretsKeyringAuthStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretsKeyringAuthStorage")
            .field("codex_home", &self.codex_home)
            .finish_non_exhaustive()
    }
}

impl SecretsKeyringAuthStorage {
    fn new(codex_home: PathBuf, keyring_store: Arc<dyn KeyringStore>) -> Self {
        let direct_storage =
            DirectKeyringAuthStorage::new(codex_home.clone(), Arc::clone(&keyring_store));
        let secrets_manager = SecretsManager::new_with_keyring_store_and_namespace(
            codex_home.clone(),
            SecretsBackendKind::Local,
            keyring_store,
            LocalSecretsNamespace::CodexAuth,
        );
        Self {
            codex_home,
            direct_storage,
            secrets_manager,
        }
    }
}

impl AuthStorageBackend for SecretsKeyringAuthStorage {
    fn load(&self) -> std::io::Result<Option<AuthDotJson>> {
        match self
            .secrets_manager
            .get(&SecretScope::Global, &CODEX_AUTH_SECRET_NAME)
            .map_err(|err| {
                std::io::Error::new(
                    err.downcast_ref::<std::io::Error>()
                        .map_or(std::io::ErrorKind::Other, std::io::Error::kind),
                    format!("failed to load CLI auth from encrypted auth storage: {err}"),
                )
            })? {
            Some(serialized) => serde_json::from_str(&serialized).map(Some).map_err(|err| {
                std::io::Error::other(format!(
                    "failed to deserialize CLI auth from encrypted auth storage: {err}"
                ))
            }),
            None => Ok(None),
        }
    }

    fn save(&self, auth: &AuthDotJson) -> std::io::Result<()> {
        let serialized = serde_json::to_string(auth).map_err(std::io::Error::other)?;
        self.secrets_manager
            .set(&SecretScope::Global, &CODEX_AUTH_SECRET_NAME, &serialized)
            .map_err(|err| {
                let message =
                    format!("failed to write OAuth tokens to encrypted auth storage: {err}");
                warn!("{message}");
                let kind = err
                    .downcast_ref::<std::io::Error>()
                    .map_or(std::io::ErrorKind::Other, std::io::Error::kind);
                std::io::Error::new(kind, message)
            })?;
        delete_file_if_exists(&self.codex_home)?;
        Ok(())
    }

    fn delete(&self) -> std::io::Result<bool> {
        let keyring_removed = self
            .secrets_manager
            .delete(&SecretScope::Global, &CODEX_AUTH_SECRET_NAME)
            .map_err(|err| {
                std::io::Error::other(format!(
                    "failed to delete auth from encrypted auth storage: {err}"
                ))
            })?;
        let file_removed = delete_file_if_exists(&self.codex_home)?;
        let direct_removed = self.direct_storage.delete()?;
        Ok(keyring_removed || file_removed || direct_removed)
    }
}

#[derive(Clone, Debug)]
struct AutoAuthStorage {
    keyring_storage: Arc<dyn AuthStorageBackend>,
    file_storage: Arc<FileAuthStorage>,
}

impl AutoAuthStorage {
    fn new(
        codex_home: PathBuf,
        keyring_store: Arc<dyn KeyringStore>,
        keyring_backend_kind: AuthKeyringBackendKind,
    ) -> Self {
        Self {
            keyring_storage: create_keyring_auth_storage(
                codex_home.clone(),
                keyring_store,
                keyring_backend_kind,
            ),
            file_storage: Arc::new(FileAuthStorage::new(codex_home)),
        }
    }
}

impl AuthStorageBackend for AutoAuthStorage {
    fn load(&self) -> std::io::Result<Option<AuthDotJson>> {
        match self.keyring_storage.load() {
            Ok(Some(auth)) => Ok(Some(auth)),
            Ok(None) => self.file_storage.load(),
            // An unreadable preferred authority is not evidence that fallback
            // credentials belong to the currently selected account.
            Err(err) => Err(err),
        }
    }

    fn save(&self, auth: &AuthDotJson) -> std::io::Result<()> {
        match self.keyring_storage.save(auth) {
            Ok(()) => Ok(()),
            // Contention is not backend unavailability: a fallback would hide
            // the new credential behind the existing encrypted value.
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => Err(err),
            Err(err) => {
                match self.keyring_storage.load() {
                    Ok(None) => {}
                    Ok(Some(_)) | Err(_) => return Err(err),
                }
                warn!("failed to save auth to keyring, falling back to file storage: {err}");
                self.file_storage.save(auth)
            }
        }
    }

    fn delete(&self) -> std::io::Result<bool> {
        // Keyring storage will delete from disk as well
        self.keyring_storage.delete()
    }
}

// A global in-memory store for mapping codex_home -> AuthDotJson.
static EPHEMERAL_AUTH_STORE: Lazy<Mutex<HashMap<String, AuthDotJson>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));



#[derive(Clone, Debug)]
struct EphemeralAuthStorage {
    codex_home: PathBuf,
}

impl EphemeralAuthStorage {
    fn new(codex_home: PathBuf) -> Self {
        Self { codex_home }
    }

    fn with_store<F, T>(&self, action: F) -> std::io::Result<T>
    where
        F: FnOnce(&mut HashMap<String, AuthDotJson>, String) -> std::io::Result<T>,
    {
        let key = compute_store_key(&self.codex_home)?;
        let mut store = EPHEMERAL_AUTH_STORE
            .lock()
            .map_err(|_| std::io::Error::other("failed to lock ephemeral auth storage"))?;
        action(&mut store, key)
    }
}

impl AuthStorageBackend for EphemeralAuthStorage {
    fn load(&self) -> std::io::Result<Option<AuthDotJson>> {
        self.with_store(|store, key| Ok(store.get(&key).cloned()))
    }

    fn save(&self, auth: &AuthDotJson) -> std::io::Result<()> {
        self.with_store(|store, key| {
            store.insert(key, auth.clone());
            Ok(())
        })
    }

    fn delete(&self) -> std::io::Result<bool> {
        self.with_store(|store, key| Ok(store.remove(&key).is_some()))
    }
}

/// Prepare the store key and acquire the store before entering a caller's short memory transaction.
pub(super) fn with_ephemeral_auth_entry<T>(
    codex_home: &Path,
    action: impl FnOnce(std::collections::hash_map::Entry<'_, String, AuthDotJson>) -> std::io::Result<T>,
) -> std::io::Result<T> {
    EphemeralAuthStorage::new(codex_home.to_path_buf())
        .with_store(|store, key| action(store.entry(key)))
}

pub(super) fn create_auth_storage(
    codex_home: PathBuf,
    mode: AuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
) -> Arc<dyn AuthStorageBackend> {
    let keyring_store: Arc<dyn KeyringStore> = Arc::new(DefaultKeyringStore);
    create_auth_storage_with_store(codex_home, mode, keyring_store, keyring_backend_kind)
}

fn create_auth_storage_with_store(
    codex_home: PathBuf,
    mode: AuthCredentialsStoreMode,
    keyring_store: Arc<dyn KeyringStore>,
    keyring_backend_kind: AuthKeyringBackendKind,
) -> Arc<dyn AuthStorageBackend> {
    let home = codex_home.clone();
    let inner: Arc<dyn AuthStorageBackend> = match mode {
        AuthCredentialsStoreMode::File => Arc::new(FileAuthStorage::new(codex_home)),
        AuthCredentialsStoreMode::Keyring => {
            create_keyring_auth_storage(codex_home, keyring_store, keyring_backend_kind)
        }
        AuthCredentialsStoreMode::Auto => Arc::new(AutoAuthStorage::new(
            codex_home,
            keyring_store,
            keyring_backend_kind,
        )),
        AuthCredentialsStoreMode::Ephemeral => Arc::new(EphemeralAuthStorage::new(codex_home)),
    };
    Arc::new(TransactionalAuthStorage {
        inner,
        home,
        ephemeral: mode == AuthCredentialsStoreMode::Ephemeral,
        observed: Mutex::new(None),
    })
}

fn create_keyring_auth_storage(
    codex_home: PathBuf,
    keyring_store: Arc<dyn KeyringStore>,
    keyring_backend_kind: AuthKeyringBackendKind,
) -> Arc<dyn AuthStorageBackend> {
    match keyring_backend_kind {
        AuthKeyringBackendKind::Direct => {
            Arc::new(DirectKeyringAuthStorage::new(codex_home, keyring_store))
        }
        AuthKeyringBackendKind::Secrets => {
            Arc::new(SecretsKeyringAuthStorage::new(codex_home, keyring_store))
        }
    }
}

