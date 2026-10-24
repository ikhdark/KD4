use super::*;
use crate::oauth::{delete_oauth_tokens_from_keyring_and_file, load_oauth_tokens_coordinated, save_oauth_tokens_coordinated};
use codex_config::types::OAuthCredentialsStoreMode;
use codex_keyring_store::{CredentialStoreError, KeyringStore};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[derive(Clone, Debug, Default)]
struct ControlledKeyring {
    inner: MockKeyringStore,
    fail_next_save: Arc<AtomicBool>,
    fail_load_at: Arc<AtomicUsize>,
    load_count: Arc<AtomicUsize>,
    gate: Arc<Mutex<Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>>>,
    break_authority_after_save: Arc<Mutex<Option<std::path::PathBuf>>>,
}

impl ControlledKeyring {
    fn gate_next_operation(&self) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        *self.gate.lock().unwrap() = Some((entered_tx, release_rx));
        (entered_rx, release_tx)
    }
    fn enter(&self) {
        let gate = self.gate.lock().unwrap().take();
        if let Some((entered, release)) = gate {
            entered.send(()).unwrap();
            release.recv_timeout(Duration::from_secs(10)).expect("fixture must release keyring owner");
        }
    }
    fn failure() -> CredentialStoreError {
        CredentialStoreError::new(keyring::Error::PlatformFailure(Box::new(std::io::Error::other("injected backend failure"))))
    }
}

impl KeyringStore for ControlledKeyring {
    fn load(&self, service: &str, account: &str) -> std::result::Result<Option<String>, CredentialStoreError> {
        self.enter();
        let call = self.load_count.fetch_add(1, Ordering::SeqCst) + 1;
        if self.fail_load_at.load(Ordering::SeqCst) == call { return Err(Self::failure()); }
        self.inner.load(service, account)
    }
    fn save(&self, service: &str, account: &str, value: &str) -> std::result::Result<(), CredentialStoreError> {
        self.enter();
        if self.fail_next_save.swap(false, Ordering::SeqCst) { return Err(Self::failure()); }
        self.inner.save(service, account, value)?;
        if let Some(path) = self.break_authority_after_save.lock().unwrap().take() {
            std::fs::remove_file(&path).unwrap();
            std::fs::create_dir(&path).unwrap();
        }
        Ok(())
    }
    fn delete(&self, service: &str, account: &str) -> std::result::Result<bool, CredentialStoreError> {
        self.inner.delete(service, account)
    }
}

fn generation(value: &str) -> StoredOAuthTokens {
    let mut tokens = sample_tokens();
    tokens.token_response.0.set_access_token(AccessToken::new(value.into()));
    tokens
}

fn save(store: &ControlledKeyring, home: &std::path::Path, backend: AuthKeyringBackendKind, mode: OAuthCredentialsStoreMode, tokens: &StoredOAuthTokens) -> Result<()> {
    save_oauth_tokens_coordinated(store, home, backend, &tokens.server_name, tokens, mode)
}

#[test]
fn audit20261023_acknowledged_fallback_wins_over_a_readable_stale_secure_value() -> Result<()> {
    for backend in [AuthKeyringBackendKind::Direct, AuthKeyringBackendKind::Secrets] {
        let home = tempfile::tempdir()?;
        let store = ControlledKeyring::default();
        let old = generation("secure-v1");
        let new = generation("fallback-v2");
        save(&store, home.path(), backend, OAuthCredentialsStoreMode::Auto, &old)?;
        match backend {
            AuthKeyringBackendKind::Direct => { store.fail_next_save.store(true, Ordering::SeqCst); }
            AuthKeyringBackendKind::Secrets => {
                // The first load decrypts V1; the second precedes publishing V2.
                store.load_count.store(0, Ordering::SeqCst);
                store.fail_load_at.store(2, Ordering::SeqCst);
            }
        }
        save(&store, home.path(), backend, OAuthCredentialsStoreMode::Auto, &new)?;
        store.fail_load_at.store(0, Ordering::SeqCst);
        let still_secure = load_oauth_tokens_from_keyring(&store, home.path(), backend, &old.server_name, &old.url)?.expect("fixture must preserve readable secure V1");
        assert_eq!(still_secure.token_response.0.access_token().secret(), "secure-v1");
        let loaded = load_oauth_tokens_coordinated(&store.clone(), home.path(), &new.server_name, &new.url, OAuthCredentialsStoreMode::Auto, backend)?.unwrap();
        assert_eq!(loaded.token_response.0.access_token().secret(), "fallback-v2");
        assert!(load_oauth_tokens_coordinated(&store, home.path(), &new.server_name, &new.url, OAuthCredentialsStoreMode::Keyring, backend)?.is_none());
    }
    Ok(())
}

#[test]
fn audit20261023_secure_cleanup_cannot_delete_a_later_file_commit() -> Result<()> {
    for backend in [AuthKeyringBackendKind::Direct, AuthKeyringBackendKind::Secrets] {
        let home = tempfile::tempdir()?;
        let store = ControlledKeyring::default();
        save(&store, home.path(), backend, OAuthCredentialsStoreMode::Auto, &generation("v1"))?;
        // A legacy alias must be removed by A without exposing B's later commit.
        save_oauth_tokens_to_file(home.path(), &generation("legacy"))?;
        let (entered, release) = store.gate_next_operation();
        let a_home = home.path().to_owned();
        let a_store = store.clone();
        let a = std::thread::spawn(move || save(&a_store, &a_home, backend, OAuthCredentialsStoreMode::Keyring, &generation("secure-v2")));
        entered.recv_timeout(Duration::from_secs(2))?;
        let (contended_tx, contended_rx) = mpsc::channel();
        let b_home = home.path().to_owned();
        let b_store = store.clone();
        let b = std::thread::spawn(move || tracing::subscriber::with_default(LockContentionSubscriber { contended_tx }, || save(&b_store, &b_home, backend, OAuthCredentialsStoreMode::File, &generation("file-v3"))));
        let contention = contended_rx.recv_timeout(Duration::from_secs(2));
        let _ = release.send(());
        a.join().expect("secure writer panicked")?;
        b.join().expect("file writer panicked")?;
        contention.context("independent logical writer must contend before secure cleanup")?;
        let loaded = load_oauth_tokens_coordinated(&store, home.path(), "test-server", "https://example.test", OAuthCredentialsStoreMode::Auto, backend)?.unwrap();
        assert_eq!(loaded.token_response.0.access_token().secret(), "file-v3");
        assert_eq!(load_oauth_tokens_from_file(home.path(), "test-server", "https://example.test")?.unwrap().token_response.0.access_token().secret(), "file-v3");
    }
    Ok(())
}

#[test]
fn audit20261023_deletion_tombstones_prevent_stale_backend_resurrection() -> Result<()> {
    for backend in [AuthKeyringBackendKind::Direct, AuthKeyringBackendKind::Secrets] {
        let home = tempfile::tempdir()?;
        let store = ControlledKeyring::default();
        let tokens = generation("old");
        save(&store, home.path(), backend, OAuthCredentialsStoreMode::Auto, &tokens)?;
        assert!(delete_oauth_tokens_from_keyring_and_file(&store, home.path(), OAuthCredentialsStoreMode::Auto, backend, &tokens.server_name, &tokens.url)?);
        save_oauth_tokens_with_keyring(&store, home.path(), backend, &tokens.server_name, &tokens)?;
        save_oauth_tokens_to_file(home.path(), &tokens)?;
        for mode in [OAuthCredentialsStoreMode::Auto, OAuthCredentialsStoreMode::File, OAuthCredentialsStoreMode::Keyring] {
            assert!(load_oauth_tokens_coordinated(&store, home.path(), &tokens.server_name, &tokens.url, mode, backend)?.is_none());
        }
    }
    Ok(())
}

#[test]
fn audit20261023_authority_publication_failure_never_downgrades_to_plaintext() -> Result<()> {
    let home = tempfile::tempdir()?;
    let store = ControlledKeyring::default();
    let tokens = generation("v1");
    save(&store, home.path(), AuthKeyringBackendKind::Direct, OAuthCredentialsStoreMode::Auto, &tokens)?;
    *store.break_authority_after_save.lock().unwrap() = Some(home.path().join("mcp-oauth-authority.json"));
    assert!(save(&store, home.path(), AuthKeyringBackendKind::Direct, OAuthCredentialsStoreMode::Auto, &generation("v2")).is_err());
    assert!(!fallback_file_path(home.path()).exists());
    assert!(load_oauth_tokens_coordinated(&store, home.path(), &tokens.server_name, &tokens.url, OAuthCredentialsStoreMode::Auto, AuthKeyringBackendKind::Direct).is_err());
    Ok(())
}
