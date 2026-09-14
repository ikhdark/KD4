use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::atomic::compiler_fence;

use age::decrypt;
use age::encrypt;
use age::scrypt::Identity as ScryptIdentity;
use age::scrypt::Recipient as ScryptRecipient;
use age::secrecy::ExposeSecret;
use age::secrecy::SecretString;
use anyhow::Context;
use anyhow::Result;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use codex_keyring_store::DefaultKeyringStore;
use codex_keyring_store::KeyringStore;
use rand::TryRngCore;
use rand::rngs::OsRng;
use serde::Deserialize;
use serde::Serialize;
use tracing::warn;

use super::SecretListEntry;
use super::SecretName;
use super::SecretScope;
use super::compute_keyring_account;
use super::keyring_service;

const SECRETS_VERSION: u8 = 1;
const LOCAL_SECRETS_FILENAME: &str = "local.age";
const CODEX_AUTH_SECRETS_FILENAME: &str = "codex_auth.age";
const MCP_OAUTH_SECRETS_FILENAME: &str = "mcp_oauth.age";

/// Selects the local encrypted file used by a `LocalSecretsBackend`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LocalSecretsNamespace {
    /// General managed secrets stored in `local.age`.
    #[default]
    ManagedSecrets,
    /// Codex authentication credentials used by the CLI, TUI, app server, and other clients.
    CodexAuth,
    /// OAuth credentials for external MCP servers.
    McpOAuth,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
struct SecretsFile {
    version: u8,
    secrets: BTreeMap<String, String>,
}

impl SecretsFile {
    fn new_empty() -> Self {
        Self {
            version: SECRETS_VERSION,
            secrets: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct LocalSecretsBackend {
    codex_home: PathBuf,
    keyring_store: Arc<dyn KeyringStore>,
    namespace: LocalSecretsNamespace,
}

impl LocalSecretsBackend {
    pub fn new_default(codex_home: PathBuf) -> Self {
        let keyring_store: Arc<dyn KeyringStore> = Arc::new(DefaultKeyringStore);
        Self::new(codex_home, keyring_store)
    }

    pub fn new(codex_home: PathBuf, keyring_store: Arc<dyn KeyringStore>) -> Self {
        Self::new_with_namespace(
            codex_home,
            keyring_store,
            LocalSecretsNamespace::ManagedSecrets,
        )
    }

    pub fn new_with_namespace(
        codex_home: PathBuf,
        keyring_store: Arc<dyn KeyringStore>,
        namespace: LocalSecretsNamespace,
    ) -> Self {
        Self {
            codex_home,
            keyring_store,
            namespace,
        }
    }

    pub fn set(&self, scope: &SecretScope, name: &SecretName, value: &str) -> Result<()> {
        anyhow::ensure!(!value.is_empty(), "secret value must not be empty");
        let canonical_key = scope.canonical_key(name);
        let _lock = self.lock_store()?;
        let (mut file, passphrase) = self.load_file_with_passphrase()?;
        if file
            .secrets
            .get(&canonical_key)
            .is_some_and(|existing| existing == value)
        {
            return Ok(());
        }
        let passphrase = match passphrase {
            Some(passphrase) => passphrase,
            None => self.load_or_create_passphrase()?,
        };
        file.secrets.insert(canonical_key, value.to_string());
        self.save_file(&file, &passphrase)
    }

    pub fn get(&self, scope: &SecretScope, name: &SecretName) -> Result<Option<String>> {
        let canonical_key = scope.canonical_key(name);
        let file = self.load_file()?;
        Ok(file.secrets.get(&canonical_key).cloned())
    }

    pub fn delete(&self, scope: &SecretScope, name: &SecretName) -> Result<bool> {
        let canonical_key = scope.canonical_key(name);
        let _lock = self.lock_store()?;
        let (mut file, passphrase) = self.load_file_with_passphrase()?;
        let removed = file.secrets.remove(&canonical_key).is_some();
        if removed {
            let passphrase = passphrase.context("existing secrets file has no passphrase")?;
            self.save_file(&file, &passphrase)?;
        }
        Ok(removed)
    }

    pub fn list(&self, scope_filter: Option<&SecretScope>) -> Result<Vec<SecretListEntry>> {
        let file = self.load_file()?;
        let mut entries = Vec::new();
        for canonical_key in file.secrets.keys() {
            let Some(entry) = parse_canonical_key(canonical_key) else {
                warn!("skipping invalid canonical secret key: {canonical_key}");
                continue;
            };
            if let Some(scope) = scope_filter
                && entry.scope != *scope
            {
                continue;
            }
            entries.push(entry);
        }
        Ok(entries)
    }

    fn secrets_dir(&self) -> PathBuf {
        self.codex_home.join("secrets")
    }

    fn secrets_path(&self) -> PathBuf {
        let filename = match self.namespace {
            LocalSecretsNamespace::ManagedSecrets => LOCAL_SECRETS_FILENAME,
            LocalSecretsNamespace::CodexAuth => CODEX_AUTH_SECRETS_FILENAME,
            LocalSecretsNamespace::McpOAuth => MCP_OAUTH_SECRETS_FILENAME,
        };
        self.secrets_dir().join(filename)
    }

    // One lock covers all namespaces because they share the keyring account.
    // Hold it through the entire read/modify/replace transaction and bootstrap.
    fn lock_store(&self) -> Result<codex_file_system::AtomicWriteLock> {
        codex_file_system::acquire_atomic_write_lock(&self.secrets_dir().join("store"))
            .context("failed to lock local secrets store")
    }

    fn load_file(&self) -> Result<SecretsFile> {
        self.load_file_with_passphrase().map(|(file, _)| file)
    }

    fn load_file_with_passphrase(&self) -> Result<(SecretsFile, Option<SecretString>)> {
        let path = self.secrets_path();
        let ciphertext = match fs::read(&path) {
            Ok(ciphertext) => ciphertext,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok((SecretsFile::new_empty(), None));
            }
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("failed to read secrets file at {}", path.display()));
            }
        };
        let passphrase = self.load_passphrase()?.context(
            "secrets file exists but its keyring key is missing; restore the original secrets key",
        )?;
        let plaintext = decrypt_with_passphrase(&ciphertext, &passphrase)?;
        let mut parsed: SecretsFile = serde_json::from_slice(&plaintext).with_context(|| {
            format!(
                "failed to deserialize decrypted secrets file at {}",
                path.display()
            )
        })?;
        if parsed.version == 0 {
            parsed.version = SECRETS_VERSION;
        }
        anyhow::ensure!(
            parsed.version <= SECRETS_VERSION,
            "secrets file version {} is newer than supported version {}",
            parsed.version,
            SECRETS_VERSION
        );
        Ok((parsed, Some(passphrase)))
    }

    fn save_file(&self, file: &SecretsFile, passphrase: &SecretString) -> Result<()> {
        let dir = self.secrets_dir();
        fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create secrets dir {}", dir.display()))?;

        let plaintext = serde_json::to_vec(file).context("failed to serialize secrets file")?;
        let ciphertext = encrypt_with_passphrase(&plaintext, passphrase)?;
        let path = self.secrets_path();
        write_file_atomically(&path, &ciphertext)?;
        Ok(())
    }

    fn load_passphrase(&self) -> Result<Option<SecretString>> {
        let account = compute_keyring_account(&self.codex_home);
        let loaded = self
            .keyring_store
            .load(keyring_service(), &account)
            .map_err(|err| anyhow::anyhow!(err.message()))
            .with_context(|| format!("failed to load secrets key from keyring for {account}"))?;
        Ok(loaded.map(SecretString::from))
    }

    // Caller holds the shared store lock.
    fn load_or_create_passphrase(&self) -> Result<SecretString> {
        match self.load_passphrase()? {
            Some(existing) => Ok(existing),
            None => {
                for filename in [
                    LOCAL_SECRETS_FILENAME,
                    CODEX_AUTH_SECRETS_FILENAME,
                    MCP_OAUTH_SECRETS_FILENAME,
                ] {
                    let path = self.secrets_dir().join(filename);
                    anyhow::ensure!(
                        !path.try_exists().with_context(|| format!(
                            "failed to inspect secrets file at {}",
                            path.display()
                        ))?,
                        "secrets file exists but its keyring key is missing; restore the original secrets key"
                    );
                }
                let account = compute_keyring_account(&self.codex_home);
                // Generate a high-entropy key and persist it in the OS keyring.
                // This keeps secrets out of plaintext config while remaining
                // fully local/offline for the MVP.
                let generated = generate_passphrase()?;
                self.keyring_store
                    .save(keyring_service(), &account, generated.expose_secret())
                    .map_err(|err| anyhow::anyhow!(err.message()))
                    .context("failed to persist secrets key in keyring")?;
                Ok(generated)
            }
        }
    }
}

fn write_file_atomically(path: &Path, contents: &[u8]) -> Result<()> {
    write_file_atomically_with_replace(path, contents, |from, to| fs::rename(from, to))
}

fn write_file_atomically_with_replace<F>(path: &Path, contents: &[u8], replace: F) -> Result<()>
where
    F: FnOnce(&Path, &Path) -> std::io::Result<()>,
{
    write_file_atomically_with_io(
        path,
        contents,
        |file, contents| {
            file.write_all(contents)?;
            file.sync_all()
        },
        replace,
    )
}

fn write_file_atomically_with_io<W, R>(
    path: &Path,
    contents: &[u8],
    write_and_sync: W,
    replace: R,
) -> Result<()>
where
    W: FnOnce(&mut fs::File, &[u8]) -> std::io::Result<()>,
    R: FnOnce(&Path, &Path) -> std::io::Result<()>,
{
    let dir = path.parent().with_context(|| {
        format!(
            "failed to compute parent directory for secrets file at {}",
            path.display()
        )
    })?;
    let filename = path.file_name().with_context(|| {
        format!(
            "failed to compute filename for secrets file at {}",
            path.display()
        )
    })?;
    let mut tmp_file = tempfile::Builder::new()
        .prefix(&format!(".{}.tmp-", filename.to_string_lossy()))
        .tempfile_in(dir)
        .with_context(|| format!("failed to create temp secrets file in {}", dir.display()))?;
    write_and_sync(tmp_file.as_file_mut(), contents).with_context(|| {
        format!(
            "failed to write or sync temp secrets file at {}",
            tmp_file.path().display()
        )
    })?;
    // Close the Windows file handle before replacement, retaining cleanup ownership
    // on every error path until the target has been atomically replaced.
    let tmp_path = tmp_file.into_temp_path();
    replace(&tmp_path, path).with_context(|| {
        format!(
            "failed to atomically replace secrets file at {} with {}",
            path.display(),
            tmp_path.display()
        )
    })
}

fn generate_passphrase() -> Result<SecretString> {
    let mut bytes = [0_u8; 32];
    let mut rng = OsRng;
    rng.try_fill_bytes(&mut bytes)
        .context("failed to generate random secrets key")?;
    // Base64 keeps the keyring payload ASCII-safe without reducing entropy.
    let encoded = BASE64_STANDARD.encode(bytes);
    wipe_bytes(&mut bytes);
    Ok(SecretString::from(encoded))
}

fn wipe_bytes(bytes: &mut [u8]) {
    for byte in bytes {
        // Volatile writes make it much harder for the compiler to elide the wipe.
        // SAFETY: `byte` is a valid mutable reference into `bytes`.
        unsafe { std::ptr::write_volatile(byte, 0) };
    }
    compiler_fence(Ordering::SeqCst);
}

fn encrypt_with_passphrase(plaintext: &[u8], passphrase: &SecretString) -> Result<Vec<u8>> {
    let recipient = ScryptRecipient::new(passphrase.clone());
    encrypt(&recipient, plaintext).context("failed to encrypt secrets file")
}

fn decrypt_with_passphrase(ciphertext: &[u8], passphrase: &SecretString) -> Result<Vec<u8>> {
    let identity = ScryptIdentity::new(passphrase.clone());
    decrypt(&identity, ciphertext).context("failed to decrypt secrets file")
}

fn parse_canonical_key(canonical_key: &str) -> Option<SecretListEntry> {
    if let Some(name) = canonical_key.strip_prefix("global/") {
        return Some(SecretListEntry {
            scope: SecretScope::Global,
            name: SecretName::new(name).ok()?,
        });
    }
    let (environment_id, name) = canonical_key.strip_prefix("env/")?.rsplit_once('/')?;
    Some(SecretListEntry {
        scope: SecretScope::environment(environment_id).ok()?,
        name: SecretName::new(name).ok()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_keyring_store::tests::MockKeyringStore;
    use keyring::Error as KeyringError;
    use pretty_assertions::assert_eq;

    #[test]
    fn missing_key_never_bootstraps_over_existing_ciphertext_in_any_namespace() -> Result<()> {
        for namespace in [
            LocalSecretsNamespace::ManagedSecrets,
            LocalSecretsNamespace::CodexAuth,
            LocalSecretsNamespace::McpOAuth,
        ] {
            let home = tempfile::tempdir()?;
            let keyring = Arc::new(MockKeyringStore::default());
            let existing = LocalSecretsBackend::new_with_namespace(
                home.path().into(),
                keyring.clone(),
                namespace,
            );
            fs::create_dir_all(existing.secrets_dir())?;
            fs::write(existing.secrets_path(), b"existing ciphertext")?;
            let name = SecretName::new("TEST")?;
            let error = existing
                .get(&SecretScope::Global, &name)
                .expect_err("missing key must fail");
            assert!(error.to_string().contains("keyring key is missing"));
            let writer = LocalSecretsBackend::new(home.path().into(), keyring.clone());
            let error = writer
                .set(&SecretScope::Global, &name, "new value")
                .expect_err("must preserve other namespaces' key identity");
            assert!(error.to_string().contains("keyring key is missing"));
            assert_eq!(
                keyring.load(keyring_service(), &compute_keyring_account(home.path()))?,
                None
            );
            assert_eq!(fs::read(existing.secrets_path())?, b"existing ciphertext");
        }
        Ok(())
    }

    #[test]
    fn reads_distinguish_absence_from_io_failure_without_creating_a_key() -> Result<()> {
        let home = tempfile::tempdir()?;
        let keyring = Arc::new(MockKeyringStore::default());
        let backend = LocalSecretsBackend::new(home.path().into(), keyring.clone());
        let name = SecretName::new("TEST")?;
        assert_eq!(backend.get(&SecretScope::Global, &name)?, None);
        fs::create_dir_all(backend.secrets_path())?;
        let error = backend
            .get(&SecretScope::Global, &name)
            .expect_err("a directory is not an empty store");
        assert!(error.to_string().contains("failed to read secrets file"));
        assert_eq!(
            keyring.load(keyring_service(), &compute_keyring_account(home.path()))?,
            None
        );
        Ok(())
    }

    #[test]
    fn unchanged_set_preserves_ciphertext_and_environment_scopes_round_trip() -> Result<()> {
        let home = tempfile::tempdir()?;
        let keyring = Arc::new(MockKeyringStore::default());
        let backend = LocalSecretsBackend::new(home.path().into(), keyring);
        let scope = SecretScope::environment("tenant/project/api")?;
        let name = SecretName::new("TEST")?;
        backend.set(&scope, &name, "value")?;
        let ciphertext = fs::read(backend.secrets_path())?;
        backend.set(&scope, &name, "value")?;
        assert_eq!(fs::read(backend.secrets_path())?, ciphertext);
        assert_eq!(
            backend.list(Some(&scope))?,
            vec![SecretListEntry {
                scope: scope.clone(),
                name: name.clone()
            }]
        );
        assert_eq!(backend.get(&scope, &name)?, Some("value".into()));
        assert!(backend.delete(&scope, &name)?);
        assert_eq!(backend.get(&scope, &name)?, None);
        Ok(())
    }

    #[test]
    fn concurrent_backend_instances_preserve_both_updates_and_shared_key() -> Result<()> {
        // Exercise both a shared file transaction and cross-namespace bootstrap.
        for second_namespace in [
            LocalSecretsNamespace::ManagedSecrets,
            LocalSecretsNamespace::McpOAuth,
        ] {
            let home = tempfile::tempdir()?;
            let keyring = Arc::new(MockKeyringStore::default());
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let mut workers = Vec::new();
            for (name, namespace) in [
                ("FIRST", LocalSecretsNamespace::ManagedSecrets),
                ("SECOND", second_namespace),
            ] {
                let backend = LocalSecretsBackend::new_with_namespace(
                    home.path().into(),
                    keyring.clone(),
                    namespace,
                );
                let barrier = barrier.clone();
                workers.push(std::thread::spawn(move || -> Result<()> {
                    barrier.wait();
                    backend.set(&SecretScope::Global, &SecretName::new(name)?, name)
                }));
            }
            for worker in workers {
                worker.join().expect("writer thread")?;
            }
            for (name, namespace) in [
                ("FIRST", LocalSecretsNamespace::ManagedSecrets),
                ("SECOND", second_namespace),
            ] {
                let backend = LocalSecretsBackend::new_with_namespace(
                    home.path().into(),
                    keyring.clone(),
                    namespace,
                );
                assert_eq!(
                    backend.get(&SecretScope::Global, &SecretName::new(name)?)?,
                    Some(name.into())
                );
            }
        }
        Ok(())
    }

    #[test]
    fn load_file_rejects_newer_schema_versions() -> Result<()> {
        let codex_home = tempfile::tempdir().expect("tempdir");
        let keyring = Arc::new(MockKeyringStore::default());
        let backend = LocalSecretsBackend::new(codex_home.path().to_path_buf(), keyring);

        let file = SecretsFile {
            version: SECRETS_VERSION + 1,
            secrets: BTreeMap::new(),
        };
        let _lock = backend.lock_store()?;
        let passphrase = backend.load_or_create_passphrase()?;
        backend.save_file(&file, &passphrase)?;

        let error = backend
            .load_file()
            .expect_err("must reject newer schema version");
        assert!(
            error.to_string().contains("newer than supported version"),
            "unexpected error: {error:#}"
        );
        Ok(())
    }

    #[test]
    fn set_fails_when_keyring_is_unavailable() -> Result<()> {
        let codex_home = tempfile::tempdir().expect("tempdir");
        let keyring = Arc::new(MockKeyringStore::default());
        let account = compute_keyring_account(codex_home.path());
        keyring.set_error(
            keyring_service(),
            &account,
            KeyringError::Invalid("error".into(), "load".into()),
        );

        let backend = LocalSecretsBackend::new(codex_home.path().to_path_buf(), keyring);
        let scope = SecretScope::Global;
        let name = SecretName::new("TEST_SECRET")?;
        let error = backend
            .set(&scope, &name, "secret-value")
            .expect_err("must fail when keyring load fails");
        assert!(
            error
                .to_string()
                .contains("failed to load secrets key from keyring"),
            "unexpected error: {error:#}"
        );
        Ok(())
    }

    #[test]
    fn save_file_does_not_leave_temp_files() -> Result<()> {
        let codex_home = tempfile::tempdir().expect("tempdir");
        let keyring = Arc::new(MockKeyringStore::default());
        let backend = LocalSecretsBackend::new(codex_home.path().to_path_buf(), keyring.clone());

        let scope = SecretScope::Global;
        let name = SecretName::new("TEST_SECRET")?;
        backend.set(&scope, &name, "one")?;
        backend.set(&scope, &name, "two")?;

        let secrets_dir = backend.secrets_dir();
        let entries = fs::read_dir(&secrets_dir)
            .with_context(|| format!("failed to read {}", secrets_dir.display()))?
            .collect::<std::io::Result<Vec<_>>>()
            .with_context(|| format!("failed to enumerate {}", secrets_dir.display()))?;

        let filenames: Vec<String> = entries
            .into_iter()
            .filter_map(|entry| entry.file_name().to_str().map(ToString::to_string))
            .collect();
        let mut filenames = filenames;
        filenames.sort();
        assert_eq!(
            filenames,
            vec![
                ".store.lock".to_string(),
                LOCAL_SECRETS_FILENAME.to_string()
            ]
        );
        let reopened = LocalSecretsBackend::new(codex_home.path().to_path_buf(), keyring);
        assert_eq!(reopened.get(&scope, &name)?, Some("two".to_string()));
        Ok(())
    }

    #[test]
    fn replacement_failure_preserves_existing_bytes() -> Result<()> {
        let codex_home = tempfile::tempdir().expect("tempdir");

        let path = codex_home.path().join(LOCAL_SECRETS_FILENAME);
        let original_ciphertext = b"original bytes";
        let replacement_ciphertext = b"replacement bytes";
        fs::write(&path, original_ciphertext)?;

        let mut observed_prepared_temp = false;
        let error = write_file_atomically_with_replace(
            &path,
            replacement_ciphertext,
            |tmp_path, destination| {
                observed_prepared_temp = true;
                assert_eq!(destination, path);
                assert_eq!(fs::read(tmp_path)?, replacement_ciphertext);
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "injected replacement failure",
                ))
            },
        )
        .expect_err("replacement must fail after the temp file is prepared");
        assert!(observed_prepared_temp);
        assert!(
            error
                .chain()
                .any(|cause| cause.to_string() == "injected replacement failure"),
            "unexpected error: {error:#}"
        );

        assert_eq!(fs::read(&path)?, original_ciphertext);
        let filenames = fs::read_dir(codex_home.path())?
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<std::io::Result<Vec<_>>>()?;
        assert_eq!(
            filenames,
            vec![std::ffi::OsString::from(LOCAL_SECRETS_FILENAME)]
        );
        Ok(())
    }

    #[test]
    fn preparation_failure_preserves_store_and_removes_temporary_files() -> Result<()> {
        let codex_home = tempfile::tempdir().expect("tempdir");
        let path = codex_home.path().join(LOCAL_SECRETS_FILENAME);
        let original_ciphertext = b"original bytes";
        let replacement_ciphertext = b"replacement bytes";
        fs::write(&path, original_ciphertext)?;

        for fail_during_sync in [false, true] {
            let mut replacement_attempted = false;
            let error = write_file_atomically_with_io(
                &path,
                replacement_ciphertext,
                |file, contents| {
                    let written_len = if fail_during_sync {
                        contents.len()
                    } else {
                        contents.len() / 2
                    };
                    file.write_all(&contents[..written_len])?;
                    assert_eq!(file.metadata()?.len(), written_len as u64);
                    Err(std::io::Error::other(if fail_during_sync {
                        "injected sync failure"
                    } else {
                        "injected write failure"
                    }))
                },
                |from, to| {
                    replacement_attempted = true;
                    fs::rename(from, to)
                },
            )
            .expect_err("preparation must fail before replacement");
            assert!(error.chain().any(|cause| {
                cause.to_string()
                    == if fail_during_sync {
                        "injected sync failure"
                    } else {
                        "injected write failure"
                    }
            }));
            assert!(!replacement_attempted);
            assert_eq!(fs::read(&path)?, original_ciphertext);
            let filenames = fs::read_dir(codex_home.path())?
                .map(|entry| entry.map(|entry| entry.file_name()))
                .collect::<std::io::Result<Vec<_>>>()?;
            assert_eq!(
                filenames,
                vec![std::ffi::OsString::from(LOCAL_SECRETS_FILENAME)]
            );
        }
        Ok(())
    }

    #[test]
    fn stale_temp_file_does_not_corrupt_live_store() -> Result<()> {
        let codex_home = tempfile::tempdir().expect("tempdir");
        let keyring = Arc::new(MockKeyringStore::default());
        let backend = LocalSecretsBackend::new(codex_home.path().to_path_buf(), keyring.clone());
        let scope = SecretScope::Global;
        let name = SecretName::new("TEST_SECRET")?;
        backend.set(&scope, &name, "one")?;

        let stale_path = backend
            .secrets_dir()
            .join(format!(".{LOCAL_SECRETS_FILENAME}.tmp-stale"));
        let stale_contents = b"incomplete encrypted replacement";
        fs::write(&stale_path, stale_contents)?;
        assert_eq!(backend.get(&scope, &name)?, Some("one".to_string()));

        backend.set(&scope, &name, "two")?;
        let reopened = LocalSecretsBackend::new(codex_home.path().to_path_buf(), keyring);
        assert_eq!(reopened.get(&scope, &name)?, Some("two".to_string()));
        assert_eq!(fs::read(stale_path)?, stale_contents);
        Ok(())
    }

    #[test]
    fn local_namespaces_write_separate_files() -> Result<()> {
        let codex_home = tempfile::tempdir().expect("tempdir");
        let keyring = Arc::new(MockKeyringStore::default());
        let codex_auth_backend = LocalSecretsBackend::new_with_namespace(
            codex_home.path().to_path_buf(),
            keyring.clone(),
            LocalSecretsNamespace::CodexAuth,
        );
        let mcp_backend = LocalSecretsBackend::new_with_namespace(
            codex_home.path().to_path_buf(),
            keyring,
            LocalSecretsNamespace::McpOAuth,
        );
        let scope = SecretScope::Global;
        let name = SecretName::new("TEST_SECRET")?;

        codex_auth_backend.set(&scope, &name, "codex-auth-value")?;
        mcp_backend.set(&scope, &name, "mcp-value")?;

        assert_eq!(
            codex_auth_backend.get(&scope, &name)?,
            Some("codex-auth-value".to_string())
        );
        assert_eq!(
            mcp_backend.get(&scope, &name)?,
            Some("mcp-value".to_string())
        );
        assert!(
            codex_home
                .path()
                .join("secrets")
                .join("codex_auth.age")
                .exists()
        );
        assert!(
            codex_home
                .path()
                .join("secrets")
                .join("mcp_oauth.age")
                .exists()
        );
        assert!(!codex_home.path().join("secrets").join("local.age").exists());
        Ok(())
    }
}
