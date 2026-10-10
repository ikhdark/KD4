use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::atomic::compiler_fence;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

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
use codex_keyring_store::KeyringStore;
use rand::TryRngCore;
use rand::rngs::OsRng;
use serde::Deserialize;
use serde::Serialize;
use tracing::warn;

use super::SecretListEntry;
use super::SecretName;
use super::SecretScope;
use super::SecretsBackend;
use super::compute_keyring_account;
use super::keyring_service;

const SECRETS_VERSION: u8 = 1;
const LOCAL_SECRETS_FILENAME: &str = "local.age";
const CODEX_AUTH_SECRETS_FILENAME: &str = "codex_auth.age";
const MCP_OAUTH_SECRETS_FILENAME: &str = "mcp_oauth.age";
const SECRETS_LOCK_FILENAME: &str = ".lock";

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

    /// Returns an error containing `io::ErrorKind::WouldBlock` if another writer
    /// is updating any local namespace in this Codex home.
    pub fn set(&self, scope: &SecretScope, name: &SecretName, value: &str) -> Result<()> {
        anyhow::ensure!(!value.is_empty(), "secret value must not be empty");
        let _lock = self.lock_secrets()?;
        let canonical_key = scope.canonical_key(name);
        let mut file = self.load_file()?;
        file.secrets.insert(canonical_key, value.to_string());
        self.save_file(&file)
    }

    pub fn get(&self, scope: &SecretScope, name: &SecretName) -> Result<Option<String>> {
        let canonical_key = scope.canonical_key(name);
        let file = self.load_file()?;
        Ok(file.secrets.get(&canonical_key).cloned())
    }

    /// Uses the same nonblocking, home-wide write lock as [`Self::set`].
    pub fn delete(&self, scope: &SecretScope, name: &SecretName) -> Result<bool> {
        let _lock = self.lock_secrets()?;
        let canonical_key = scope.canonical_key(name);
        let mut file = self.load_file()?;
        let removed = file.secrets.remove(&canonical_key).is_some();
        if removed {
            self.save_file(&file)?;
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

    /// Serialize all namespace writers: their files share one keyring passphrase.
    /// Never wait behind a writer blocked in the keyring or filesystem. Keep the
    /// lock file after release so other processes always lock the same object.
    fn lock_secrets(&self) -> Result<fs::File> {
        let dir = self.secrets_dir();
        fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create secrets dir {}", dir.display()))?;
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join(SECRETS_LOCK_FILENAME))
            .context("failed to open secrets write lock")?;
        lock.try_lock()
            .map_err(std::io::Error::from)
            .context("secrets store is unavailable for writing")?;
        Ok(lock)
    }

    fn secrets_path(&self) -> PathBuf {
        let filename = match self.namespace {
            LocalSecretsNamespace::ManagedSecrets => LOCAL_SECRETS_FILENAME,
            LocalSecretsNamespace::CodexAuth => CODEX_AUTH_SECRETS_FILENAME,
            LocalSecretsNamespace::McpOAuth => MCP_OAUTH_SECRETS_FILENAME,
        };
        self.secrets_dir().join(filename)
    }

    fn load_file(&self) -> Result<SecretsFile> {
        let path = self.secrets_path();
        let ciphertext = match fs::read(&path) {
            Ok(ciphertext) => ciphertext,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(SecretsFile::new_empty());
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to read secrets file at {}", path.display()));
            }
        };
        // Reads must never create a replacement key for existing ciphertext.
        // Only a writer holding the home-wide lock may initialize the keyring.
        let passphrase = self
            .load_passphrase()?
            .context("secrets key is missing from keyring")?;
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
        Ok(parsed)
    }

    fn save_file(&self, file: &SecretsFile) -> Result<()> {
        let dir = self.secrets_dir();
        fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create secrets dir {}", dir.display()))?;

        let passphrase = self.load_or_create_passphrase()?;
        let plaintext = serde_json::to_vec(file).context("failed to serialize secrets file")?;
        let ciphertext = encrypt_with_passphrase(&plaintext, &passphrase)?;
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

    fn load_or_create_passphrase(&self) -> Result<SecretString> {
        match self.load_passphrase()? {
            Some(existing) => Ok(existing),
            None => {
                // Generate a high-entropy key and persist it in the OS keyring.
                // This keeps secrets out of plaintext config while remaining
                // fully local/offline for the MVP.
                let generated = generate_passphrase()?;
                let account = compute_keyring_account(&self.codex_home);
                self.keyring_store
                    .save(keyring_service(), &account, generated.expose_secret())
                    .map_err(|err| anyhow::anyhow!(err.message()))
                    .context("failed to persist secrets key in keyring")?;
                Ok(generated)
            }
        }
    }
}

impl SecretsBackend for LocalSecretsBackend {
    fn set(&self, scope: &SecretScope, name: &SecretName, value: &str) -> Result<()> {
        LocalSecretsBackend::set(self, scope, name, value)
    }

    fn get(&self, scope: &SecretScope, name: &SecretName) -> Result<Option<String>> {
        LocalSecretsBackend::get(self, scope, name)
    }

    fn delete(&self, scope: &SecretScope, name: &SecretName) -> Result<bool> {
        LocalSecretsBackend::delete(self, scope, name)
    }

    fn list(&self, scope_filter: Option<&SecretScope>) -> Result<Vec<SecretListEntry>> {
        LocalSecretsBackend::list(self, scope_filter)
    }
}

fn write_file_atomically(path: &Path, contents: &[u8]) -> Result<()> {
    let dir = path.parent().with_context(|| {
        format!(
            "failed to compute parent directory for secrets file at {}",
            path.display()
        )
    })?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let filename = path.file_name().with_context(|| {
        format!(
            "failed to compute filename for secrets file at {}",
            path.display()
        )
    })?;
    let tmp_path = dir.join(format!(
        ".{}.tmp-{}-{nonce}",
        filename.to_string_lossy(),
        std::process::id()
    ));

    {
        let mut tmp_file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&tmp_path)
            .with_context(|| {
                format!(
                    "failed to create temp secrets file at {}",
                    tmp_path.display()
                )
            })?;
        tmp_file.write_all(contents).with_context(|| {
            format!(
                "failed to write temp secrets file at {}",
                tmp_path.display()
            )
        })?;
        tmp_file.sync_all().with_context(|| {
            format!("failed to sync temp secrets file at {}", tmp_path.display())
        })?;
    }

    match fs::rename(&tmp_path, path) {
        Ok(()) => Ok(()),
        Err(initial_error) => {
            // Never delete the committed file to retry a failed atomic replace.
            // Preserve it and clean up only this attempted write.
            let _ = fs::remove_file(&tmp_path);
            Err(initial_error).with_context(|| {
                format!(
                    "failed to atomically replace secrets file at {} with {}",
                    path.display(),
                    tmp_path.display()
                )
            })
        }
    }
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
    let (scope_kind, remainder) = canonical_key.split_once('/')?;
    match scope_kind {
        "global" => {
            let name = SecretName::new(remainder).ok()?;
            Some(SecretListEntry {
                scope: SecretScope::Global,
                name,
            })
        }
        "env" => {
            // Environment IDs are opaque; only secret names exclude slashes.
            let (environment_id, name) = remainder.rsplit_once('/')?;
            let name = SecretName::new(name).ok()?;
            let scope = SecretScope::environment(environment_id.to_string()).ok()?;
            Some(SecretListEntry { scope, name })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_keyring_store::tests::MockKeyringStore;
    use keyring::Error as KeyringError;
    use pretty_assertions::assert_eq;

    #[test]
    fn canonical_keys_preserve_opaque_environment_ids_and_reject_malformed_names() {
        for (key, scope) in [
            ("global/TOKEN", SecretScope::Global),
            ("env/simple/TOKEN", SecretScope::environment("simple").unwrap()),
            (
                "env/team/project/TOKEN",
                SecretScope::environment("team/project").unwrap(),
            ),
            ("env//team//TOKEN", SecretScope::environment("/team/").unwrap()),
        ] {
            assert_eq!(
                parse_canonical_key(key),
                Some(SecretListEntry {
                    scope,
                    name: SecretName::new("TOKEN").unwrap(),
                }),
                "{key}"
            );
        }
        for key in [
            "global",
            "global/",
            "global/TOKEN/EXTRA",
            "env/TOKEN",
            "env//TOKEN",
            "env/team/",
            "env/team/lowercase",
            "unknown/TOKEN",
        ] {
            assert_eq!(parse_canonical_key(key), None, "{key}");
        }
    }

    #[test]
    fn list_distinguishes_missing_store_from_read_errors() -> Result<()> {
        let codex_home = tempfile::tempdir().expect("tempdir");
        let keyring = Arc::new(MockKeyringStore::default());
        let missing = LocalSecretsBackend::new(codex_home.path().join("missing"), keyring.clone());
        assert!(missing.list(None)?.is_empty());

        let invalid = LocalSecretsBackend::new(codex_home.path().join("invalid\0home"), keyring);
        let error = invalid
            .list(None)
            .expect_err("an unreadable store must not be reported as empty");
        assert!(error.to_string().contains("failed to read secrets file at"));
        assert_eq!(
            error
                .downcast_ref::<std::io::Error>()
                .map(std::io::Error::kind),
            Some(std::io::ErrorKind::InvalidInput)
        );
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
        backend.save_file(&file)?;

        let error = backend
            .list(None)
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
        // A failed keyring operation must release the writer lock.
        drop(backend.lock_secrets()?);
        Ok(())
    }

    #[test]
    fn save_file_does_not_leave_temp_files() -> Result<()> {
        let codex_home = tempfile::tempdir().expect("tempdir");
        let keyring = Arc::new(MockKeyringStore::default());
        let backend = LocalSecretsBackend::new(codex_home.path().to_path_buf(), keyring);

        let scope = SecretScope::Global;
        let name = SecretName::new("TEST_SECRET")?;
        backend.set(&scope, &name, "one")?;
        backend.set(&scope, &name, "two")?;

        #[cfg(target_os = "windows")]
        {
            use std::os::windows::fs::OpenOptionsExt;

            let path = backend.secrets_path();
            let original = fs::read(&path)?;
            // Allow reads but deny replacement and deletion while the file is open.
            let reader = fs::OpenOptions::new()
                .read(true)
                .share_mode(1) // FILE_SHARE_READ
                .open(&path)?;
            let result = backend.set(&scope, &name, "three");
            drop(reader);

            let error = result.expect_err("a locked secrets file must not be replaced");
            // The failure must come from the replace step, not from an earlier read.
            assert!(
                error
                    .to_string()
                    .contains("failed to atomically replace secrets file at"),
                "unexpected error: {error:#}"
            );
            assert_eq!(fs::read(&path)?, original);
        }

        let secrets_dir = backend.secrets_dir();
        let entries = fs::read_dir(&secrets_dir)
            .with_context(|| format!("failed to read {}", secrets_dir.display()))?
            .collect::<std::io::Result<Vec<_>>>()
            .with_context(|| format!("failed to enumerate {}", secrets_dir.display()))?;

        let mut filenames: Vec<String> = entries
            .into_iter()
            .filter_map(|entry| entry.file_name().to_str().map(ToString::to_string))
            .collect();
        filenames.sort();
        assert_eq!(
            filenames,
            vec![SECRETS_LOCK_FILENAME.to_string(), LOCAL_SECRETS_FILENAME.to_string()]
        );
        assert_eq!(backend.get(&scope, &name)?, Some("two".to_string()));
        Ok(())
    }

    #[test]
    fn concurrent_namespace_initialization_preserves_both_secrets() -> Result<()> {
        assert_concurrent_writes_preserve_both_secrets(LocalSecretsNamespace::McpOAuth)
    }

    #[test]
    fn concurrent_same_namespace_writes_preserve_both_secrets() -> Result<()> {
        assert_concurrent_writes_preserve_both_secrets(LocalSecretsNamespace::CodexAuth)
    }

    fn assert_concurrent_writes_preserve_both_secrets(
        second_namespace: LocalSecretsNamespace,
    ) -> Result<()> {
        use codex_keyring_store::CredentialStoreError;
        use std::sync::Mutex;
        use std::sync::atomic::AtomicBool;
        use std::sync::mpsc;
        use std::time::Duration;

        #[derive(Debug)]
        struct PausedFirstLoad {
            inner: MockKeyringStore,
            first: AtomicBool,
            entered: mpsc::Sender<()>,
            resume: Mutex<mpsc::Receiver<()>>,
        }

        impl KeyringStore for PausedFirstLoad {
            fn load(&self, service: &str, account: &str) -> Result<Option<String>, CredentialStoreError> {
                let value = self.inner.load(service, account)?;
                if self.first.swap(false, Ordering::SeqCst) {
                    assert_eq!(value, None, "first writer must observe an absent key");
                    self.entered.send(()).expect("notify test");
                    self.resume
                        .lock()
                        .expect("resume mutex")
                        .recv_timeout(Duration::from_secs(60))
                        .expect("test must release blocked keyring load");
                }
                Ok(value)
            }

            fn save(&self, service: &str, account: &str, value: &str) -> Result<(), CredentialStoreError> {
                self.inner.save(service, account, value)
            }

            fn delete(&self, service: &str, account: &str) -> Result<bool, CredentialStoreError> {
                self.inner.delete(service, account)
            }
        }

        let home = tempfile::tempdir()?;
        let (entered_tx, entered_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let keyring = Arc::new(PausedFirstLoad {
            inner: MockKeyringStore::default(),
            first: AtomicBool::new(true),
            entered: entered_tx,
            resume: Mutex::new(resume_rx),
        });
        let first = LocalSecretsBackend::new_with_namespace(
            home.path().to_path_buf(),
            keyring.clone(),
            LocalSecretsNamespace::CodexAuth,
        );
        let second = LocalSecretsBackend::new_with_namespace(
            home.path().to_path_buf(),
            keyring,
            second_namespace,
        );
        let name = SecretName::new("FIRST_TOKEN")?;
        let second_name = SecretName::new("SECOND_TOKEN")?;
        let first_writer = {
            let backend = first.clone();
            let name = name.clone();
            std::thread::spawn(move || backend.set(&SecretScope::Global, &name, "first"))
        };
        let entered = entered_rx.recv_timeout(Duration::from_secs(10));
        if entered.is_err() {
            let _ = resume_tx.send(());
            let result = first_writer.join();
            panic!("first writer did not reach keyring load: {entered:?}; {result:?}");
        }

        let (finished_tx, finished_rx) = mpsc::channel();
        let second_writer = {
            let backend = second.clone();
            let name = second_name.clone();
            std::thread::spawn(move || {
                let result = backend.set(&SecretScope::Global, &name, "second");
                let _ = finished_tx.send(());
                result
            })
        };
        // An unavailable keyring must not make another writer wait indefinitely.
        // Explicit contention is acceptable, but two successes must remain readable.
        let finished = finished_rx.recv_timeout(Duration::from_secs(10));
        let _ = resume_tx.send(());
        let first_result = first_writer.join().expect("first writer must not panic");
        let second_result = second_writer.join().expect("second writer must not panic");
        finished.expect("second writer must finish or report contention while keyring is blocked");
        first_result?;
        if let Err(error) = second_result {
            assert_eq!(
                error.downcast_ref::<std::io::Error>().map(std::io::Error::kind),
                Some(std::io::ErrorKind::WouldBlock),
                "unexpected concurrent-write error: {error:#}"
            );
            second.set(&SecretScope::Global, &second_name, "second")?;
        }
        assert_eq!(first.get(&SecretScope::Global, &name)?, Some("first".to_string()));
        assert_eq!(second.get(&SecretScope::Global, &second_name)?, Some("second".to_string()));
        Ok(())
    }

    #[test]
    fn missing_key_reads_do_not_initialize_or_replace_the_keyring() -> Result<()> {
        let home = tempfile::tempdir()?;
        let keyring = Arc::new(MockKeyringStore::default());
        let backend = LocalSecretsBackend::new(home.path().to_path_buf(), keyring.clone());
        let name = SecretName::new("TOKEN")?;
        backend.set(&SecretScope::Global, &name, "retained")?;
        let ciphertext = fs::read(backend.secrets_path())?;
        let account = compute_keyring_account(home.path());
        assert!(keyring.delete(keyring_service(), &account)?);

        for error in [
            backend.get(&SecretScope::Global, &name).unwrap_err(),
            backend.list(None).unwrap_err(),
        ] {
            assert!(error.to_string().contains("secrets key is missing from keyring"));
            assert_eq!(keyring.load(keyring_service(), &account)?, None);
        }
        assert_eq!(fs::read(backend.secrets_path())?, ciphertext);
        Ok(())
    }

    #[test]
    fn write_lock_is_shared_across_processes_and_home_path_aliases() -> Result<()> {
        const CHILD_HOME: &str = "CODEX_SECRETS_WRITE_LOCK_TEST_HOME";
        if let Some(home) = std::env::var_os(CHILD_HOME) {
            let backend = LocalSecretsBackend::new(
                PathBuf::from(home),
                Arc::new(MockKeyringStore::default()),
            );
            let name = SecretName::new("CHILD_TOKEN")?;
            let error = backend.set(&SecretScope::Global, &name, "blocked").unwrap_err();
            assert_eq!(
                error.downcast_ref::<std::io::Error>().map(std::io::Error::kind),
                Some(std::io::ErrorKind::WouldBlock)
            );
            println!("child observed secrets write contention");
            return Ok(());
        }

        let home = tempfile::tempdir()?;
        let keyring = Arc::new(MockKeyringStore::default());
        let backend = LocalSecretsBackend::new(home.path().to_path_buf(), keyring.clone());
        let name = SecretName::new("TOKEN")?;
        backend.set(&SecretScope::Global, &name, "retained")?;
        let lock = backend.lock_secrets()?;
        let alias = LocalSecretsBackend::new(home.path().join("."), keyring);
        assert_eq!(compute_keyring_account(home.path()), compute_keyring_account(&home.path().join(".")));
        assert_eq!(alias.get(&SecretScope::Global, &name)?, Some("retained".to_string()));
        assert_eq!(alias.list(None)?.len(), 1);
        let error = alias.delete(&SecretScope::Global, &name).unwrap_err();
        assert_eq!(
            error.downcast_ref::<std::io::Error>().map(std::io::Error::kind),
            Some(std::io::ErrorKind::WouldBlock)
        );

        let mut child = std::process::Command::new(std::env::current_exe()?)
            .args(["--exact", "local::tests::write_lock_is_shared_across_processes_and_home_path_aliases", "--nocapture"])
            .env(CHILD_HOME, home.path().join("."))
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut timed_out = false;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {}
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(error.into());
                }
            }
            if std::time::Instant::now() >= deadline {
                timed_out = true;
                // Reap the child even if it exits between the poll and kill.
                let _ = child.kill();
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let output = child.wait_with_output()?;
        drop(lock);
        assert!(!timed_out, "contending process must not wait for the lock");
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert!(String::from_utf8_lossy(&output.stdout).contains("child observed secrets write contention"));
        alias.set(&SecretScope::Global, &name, "after release")?;
        assert_eq!(backend.get(&SecretScope::Global, &name)?, Some("after release".to_string()));
        assert!(alias.delete(&SecretScope::Global, &name)?);
        assert_eq!(backend.get(&SecretScope::Global, &name)?, None);
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
