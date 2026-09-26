use std::fmt;
use std::path::Path;

use anyhow::Result;
use sha2::Digest;
use sha2::Sha256;

mod local;

pub use local::LocalSecretsBackend;
pub use local::LocalSecretsNamespace;

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
}

impl SecretScope {
    pub fn canonical_key(&self, name: &SecretName) -> String {
        // Stable, env-safe identifier used as the on-disk map key.
        match self {
            Self::Global => format!("global/{}", name.as_str()),
        }
    }
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
