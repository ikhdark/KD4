//! Regression tests and an opt-in benchmark using real encryption and isolated files.
#![cfg(test)]
use codex_keyring_store::{CredentialStoreError, KeyringStore};
use codex_secrets::{LocalSecretsBackend, LocalSecretsReadCache, SecretName, SecretScope};
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[derive(Debug, Default)]
struct MemoryKeyring(Mutex<Option<String>>);
impl KeyringStore for MemoryKeyring {
    fn load(&self, _: &str, _: &str) -> Result<Option<String>, CredentialStoreError> {
        Ok(self.0.lock().unwrap().clone())
    }
    fn save(&self, _: &str, _: &str, value: &str) -> Result<(), CredentialStoreError> {
        *self.0.lock().unwrap() = Some(value.into());
        Ok(())
    }
    fn delete(&self, _: &str, _: &str) -> Result<bool, CredentialStoreError> {
        Ok(self.0.lock().unwrap().take().is_some())
    }
}

#[test]
#[ignore]
fn encrypted_lookup_probe() -> anyhow::Result<()> {
    let home = tempfile::TempDir::new()?;
    let backend = LocalSecretsBackend::new(home.path().into(), Arc::new(MemoryKeyring::default()));
    let name = SecretName::new("MCP_TEST_TOKEN")?;
    backend.set(
        &SecretScope::Global,
        &name,
        "fixture-token-not-a-real-secret",
    )?;
    for run in 0..3 {
        let cache = LocalSecretsReadCache::default();
        let start = Instant::now();
        for _ in 0..4 {
            assert_eq!(
                backend
                    .get_with_read_cache(&SecretScope::Global, &name, &cache)?
                    .as_deref(),
                Some("fixture-token-not-a-real-secret")
            );
        }
        println!(
            "AUDIT secrets run={run} lookups=4 elapsed_ms={:.3}",
            start.elapsed().as_secs_f64() * 1000.0
        );
    }
    Ok(())
}

#[test]
fn cached_reads_validate_contents_key_and_namespace() -> anyhow::Result<()> {
    let home = tempfile::TempDir::new()?;
    let keyring = Arc::new(MemoryKeyring::default());
    let backend = LocalSecretsBackend::new(home.path().into(), keyring.clone());
    let name = SecretName::new("MCP_TEST_TOKEN")?;
    let cache = Arc::new(LocalSecretsReadCache::default());
    let weak = Arc::downgrade(&cache);
    backend.set(&SecretScope::Global, &name, "before")?;
    let path = home.path().join("secrets/local.age");
    let old_ciphertext = std::fs::read(&path)?;
    assert_eq!(
        backend
            .get_with_read_cache(&SecretScope::Global, &name, &cache)?
            .as_deref(),
        Some("before")
    );
    assert_eq!(
        backend.get_with_read_cache(&SecretScope::Global, &SecretName::new("ABSENT")?, &cache)?,
        None
    );
    // An independent writer changes the file while this read operation is alive.
    let other = LocalSecretsBackend::new(home.path().into(), keyring.clone());
    other.set(&SecretScope::Global, &name, "after!")?;
    assert_eq!(
        backend
            .get_with_read_cache(&SecretScope::Global, &name, &cache)?
            .as_deref(),
        Some("after!")
    );
    std::fs::write(&path, old_ciphertext)?;
    assert_eq!(
        backend
            .get_with_read_cache(&SecretScope::Global, &name, &cache)?
            .as_deref(),
        Some("before")
    );
    let other_namespace = LocalSecretsBackend::new_with_namespace(
        home.path().into(),
        keyring.clone(),
        codex_secrets::LocalSecretsNamespace::McpOAuth,
    );
    assert_eq!(
        other_namespace.get_with_read_cache(&SecretScope::Global, &name, &cache)?,
        None
    );
    *keyring.0.lock().unwrap() = Some("a-different-key".into());
    assert!(
        backend
            .get_with_read_cache(&SecretScope::Global, &name, &cache)
            .is_err()
    );
    std::fs::write(&path, b"not an age file")?;
    assert!(
        backend
            .get_with_read_cache(&SecretScope::Global, &name, &cache)
            .is_err()
    );
    drop(cache);
    assert!(weak.upgrade().is_none());
    Ok(())
}
