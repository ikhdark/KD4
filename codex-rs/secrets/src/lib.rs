use std::fmt;
use std::path::Path;

use anyhow::Result;
use codex_git_utils::get_git_repo_root;
use sha2::Digest;
use sha2::Sha256;

mod local;
mod sanitizer;

pub use local::LocalSecretsBackend;
pub use local::LocalSecretsNamespace;
pub use sanitizer::redact_secrets;

const KEYRING_SERVICE: &str = "codex";

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SecretName(String);

impl SecretName {
    pub fn new(raw: &str) -> Result<Self> {
        let trimmed = raw.trim();
        anyhow::ensure!(!trimmed.is_empty(), "secret name must not be empty");
        anyhow::ensure!(
            trimmed
                .chars()
                .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_'),
            "secret name must contain only A-Z, 0-9, or _"
        );
        Ok(Self(trimmed.to_string()))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Display for SecretName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SecretScope {
    Global,
    Environment(String),
}

impl SecretScope {
    pub fn environment(environment_id: impl Into<String>) -> Result<Self> {
        let env_id = environment_id.into();
        let trimmed = env_id.trim();
        anyhow::ensure!(!trimmed.is_empty(), "environment id must not be empty");
        Ok(Self::Environment(trimmed.to_string()))
    }

    pub fn canonical_key(&self, name: &SecretName) -> String {
        // Stable, env-safe identifier used as the on-disk map key.
        match self {
            Self::Global => format!("global/{}", name.as_str()),
            Self::Environment(environment_id) => {
                format!("env/{environment_id}/{}", name.as_str())
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretListEntry {
    pub scope: SecretScope,
    pub name: SecretName,
}

pub fn environment_id_from_cwd(cwd: &Path) -> String {
    if let Some(repo_root) = get_git_repo_root(cwd)
        && let Some(name) = repo_root.file_name()
    {
        let name = name.to_string_lossy().trim().to_string();
        if !name.is_empty() {
            return name;
        }
    }

    let canonical = cwd
        .canonicalize()
        .unwrap_or_else(|_| cwd.to_path_buf())
        .to_string_lossy()
        .into_owned();
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    let digest = hasher.finalize();
    let hex = format!("{digest:x}");
    let short = hex.get(..12).unwrap_or(hex.as_str());
    format!("cwd-{short}")
}

/// Computes the OS keyring account name used to store the local secrets passphrase.
pub fn compute_keyring_account(codex_home: &Path) -> String {
    let canonical = codex_home
        .canonicalize()
        .unwrap_or_else(|_| codex_home.to_path_buf())
        .to_string_lossy()
        .into_owned();
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    let digest = hasher.finalize();
    let hex = format!("{digest:x}");
    let short = hex.get(..16).unwrap_or(hex.as_str());
    format!("secrets|{short}")
}

pub(crate) fn keyring_service() -> &'static str {
    KEYRING_SERVICE
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_keyring_store::tests::MockKeyringStore;
    use pretty_assertions::assert_eq;
    use std::sync::Arc;

    #[test]
    fn environment_id_fallback_has_cwd_prefix() {
        let dir = tempfile::tempdir().expect("tempdir");
        let env_id = environment_id_from_cwd(dir.path());
        let canonical = dir
            .path()
            .canonicalize()
            .expect("tempdir canonical path should exist")
            .to_string_lossy()
            .into_owned();
        let mut hasher = Sha256::new();
        hasher.update(canonical.as_bytes());
        let digest = hasher.finalize();
        let hex = format!("{digest:x}");
        let short = hex.get(..12).expect("digest has at least 12 chars");
        assert_eq!(env_id, format!("cwd-{short}"));
    }

    #[test]
    fn local_backend_round_trips_secrets() -> Result<()> {
        let codex_home = tempfile::tempdir().expect("tempdir");
        let keyring = Arc::new(MockKeyringStore::default());
        let backend = LocalSecretsBackend::new(codex_home.path().to_path_buf(), keyring);
        let scope = SecretScope::Global;
        let name = SecretName::new("GITHUB_TOKEN")?;

        backend.set(&scope, &name, "token-1")?;
        assert_eq!(backend.get(&scope, &name)?, Some("token-1".to_string()));

        let listed = backend.list(/*scope_filter*/ None)?;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, name);

        assert!(backend.delete(&scope, &name)?);
        assert_eq!(backend.get(&scope, &name)?, None);
        Ok(())
    }

    #[test]
    fn local_backend_is_the_only_secrets_behavior_owner() {
        let obsolete_facade = ["struct Secrets", "Manager"].concat();
        assert!(!include_str!("lib.rs").contains(&obsolete_facade));
    }
}
