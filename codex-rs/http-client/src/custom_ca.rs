//! Custom CA handling shared by Codex outbound HTTP and websocket clients.
//!
//! Codex constructs outbound reqwest clients and secure websocket connections in a few crates, but
//! they all need the same trust-store policy when enterprise proxies or gateways intercept TLS.
//! This module centralizes that policy so callers can start from an ordinary
//! `reqwest::ClientBuilder` or rustls client config, layer in custom CA support, and either get
//! back a configured transport or a user-facing error that explains how to fix a misconfigured CA
//! bundle.
//!
//! The module intentionally has a narrow responsibility:
//!
//! - read CA material from `CODEX_CA_CERTIFICATE`, falling back to `SSL_CERT_FILE`
//! - normalize PEM variants that show up in real deployments, including OpenSSL-style
//!   `TRUSTED CERTIFICATE` labels and bundles that also contain CRLs
//! - return user-facing errors that explain how to fix misconfigured CA files
//!
//! Its production contract is narrow: produce a transport configuration whose root store contains
//! every parseable certificate block from the configured PEM bundle, or fail early with a precise
//! error before the caller starts network traffic.
//!
//! In this module's test setup, a hermetic test is one whose result depends only on the CA file
//! and environment variables that the test chose for itself. That matters here because the normal
//! reqwest client-construction path is not hermetic enough for environment-sensitive tests:
//!
//! - child processes inherit CA-related environment variables by default, which lets developer
//!   shell state or CI configuration affect a test unless the test scrubs those variables first
//!
//! The tests in this crate therefore stay split across two layers:
//!
//! - unit tests in this module cover env-selection logic without constructing a real client
//! - subprocess integration tests under `tests/` cover real client construction through
//!   [`build_reqwest_client_for_subprocess_tests`], which disables reqwest proxy autodetection so
//!   the tests can observe custom-CA success and failure directly, including one TLS handshake
//!   through a local HTTPS server
//! - those subprocess tests also scrub inherited CA environment variables before launch so their
//!   result depends only on the test fixtures and env vars set by the test itself

use std::collections::HashMap;
use std::env;
use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::Mutex;

use codex_utils_der::first_der_item;
use codex_utils_rustls_provider::ensure_rustls_crypto_provider;
use rustls::ClientConfig;
use rustls::RootCertStore;
use rustls_pki_types::CertificateDer;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::pem::SectionKind;
use rustls_pki_types::pem::{self};
use sha2::Digest;
use sha2::Sha256;
use thiserror::Error;
use tracing::info;
use tracing::warn;

pub const CODEX_CA_CERT_ENV: &str = "CODEX_CA_CERTIFICATE";
pub const SSL_CERT_FILE_ENV: &str = "SSL_CERT_FILE";
const CA_CERT_HINT: &str = "If you set CODEX_CA_CERTIFICATE or SSL_CERT_FILE, ensure it points to a PEM file containing one or more CERTIFICATE blocks, or unset it to use system roots.";
type PemSection = (SectionKind, Vec<u8>);

static NATIVE_ROOT_STORE: LazyLock<RootCertStore> = LazyLock::new(|| {
    let mut root_store = RootCertStore::empty();
    let rustls_native_certs::CertificateResult { certs, errors, .. } =
        rustls_native_certs::load_native_certs();
    if !errors.is_empty() {
        warn!(
            native_root_error_count = errors.len(),
            "encountered errors while loading native root certificates"
        );
    }
    let _ = root_store.add_parsable_certificates(certs);
    root_store
});
static RUSTLS_CLIENT_CONFIGS: LazyLock<Mutex<HashMap<CaBundleCacheKey, Arc<ClientConfig>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static REQWEST_CA_CERTIFICATES: LazyLock<
    Mutex<HashMap<CaBundleCacheKey, Arc<Vec<reqwest::Certificate>>>>,
> = LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CaBundleCacheKey {
    source_env: Option<&'static str>,
    path: Option<PathBuf>,
    pem_sha256: Option<[u8; 32]>,
}

impl CaBundleCacheKey {
    fn new(bundle: Option<&ConfiguredCaBundle>, pem_data: Option<&[u8]>) -> Self {
        Self {
            source_env: bundle.map(|bundle| bundle.source_env),
            path: bundle.map(|bundle| bundle.path.clone()),
            pem_sha256: pem_data.map(|pem_data| Sha256::digest(pem_data).into()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CustomCaPolicy {
    HonorProcessEnvironment,
    ExplicitRootSet,
}
static DEFAULT_RUSTLS_CLIENT_CONFIG: LazyLock<Arc<ClientConfig>> = LazyLock::new(|| {
    ensure_rustls_crypto_provider();
    Arc::new(
        ClientConfig::builder()
            .with_root_certificates(native_root_store().clone())
            .with_no_client_auth(),
    )
});

fn native_root_store() -> &'static RootCertStore {
    &NATIVE_ROOT_STORE
}

fn default_rustls_client_config() -> Arc<ClientConfig> {
    Arc::clone(&DEFAULT_RUSTLS_CLIENT_CONFIG)
}

/// Describes why a transport using shared custom CA support could not be constructed.
///
/// These failure modes apply to both reqwest client construction and websocket TLS
/// configuration. A build can fail because the configured CA file could not be read, could not be
/// parsed as certificates, contained certs that the target TLS stack refused to register, or
/// because the final reqwest client builder failed. Callers that do not care about the
/// distinction can rely on the `From<BuildCustomCaTransportError> for io::Error` conversion.
#[derive(Debug, Error)]
pub enum BuildCustomCaTransportError {
    /// Reading the selected CA file from disk failed before any PEM parsing could happen.
    #[error(
        "Failed to read CA certificate file {} selected by {}: {source}. {hint}",
        path.display(),
        source_env,
        hint = CA_CERT_HINT
    )]
    ReadCaFile {
        source_env: &'static str,
        path: PathBuf,
        source: io::Error,
    },

    /// The selected CA file was readable, but did not produce usable certificate material.
    #[error(
        "Failed to load CA certificates from {} selected by {}: {detail}. {hint}",
        path.display(),
        source_env,
        hint = CA_CERT_HINT
    )]
    InvalidCaFile {
        source_env: &'static str,
        path: PathBuf,
        detail: String,
    },

    /// One parsed certificate block could not be registered with the reqwest client builder.
    #[error(
        "Failed to parse certificate #{certificate_index} from {} selected by {}: {source}. {hint}",
        path.display(),
        source_env,
        hint = CA_CERT_HINT
    )]
    RegisterCertificate {
        source_env: &'static str,
        path: PathBuf,
        certificate_index: usize,
        source: reqwest::Error,
    },

    /// Reqwest rejected the final client configuration after a custom CA bundle was loaded.
    #[error(
        "Failed to build HTTP client while using CA bundle from {} ({}): {source}",
        source_env,
        path.display()
    )]
    BuildClientWithCustomCa {
        source_env: &'static str,
        path: PathBuf,
        #[source]
        source: reqwest::Error,
    },

    /// Reqwest rejected the final client configuration while using only system roots.
    #[error("Failed to build HTTP client while using system root certificates: {0}")]
    BuildClientWithSystemRoots(#[source] reqwest::Error),

    /// Reqwest rejected the final client configuration while using caller-supplied roots.
    #[error("Failed to build HTTP client while using the explicit root certificate set: {0}")]
    BuildClientWithExplicitRoots(#[source] reqwest::Error),

    /// One parsed certificate block could not be registered with the websocket TLS root store.
    #[error(
        "Failed to register certificate #{certificate_index} from {} selected by {} in rustls root store: {source}. {hint}",
        path.display(),
        source_env,
        hint = CA_CERT_HINT
    )]
    RegisterRustlsCertificate {
        source_env: &'static str,
        path: PathBuf,
        certificate_index: usize,
        source: rustls::Error,
    },
}

impl From<BuildCustomCaTransportError> for io::Error {
    fn from(error: BuildCustomCaTransportError) -> Self {
        match error {
            BuildCustomCaTransportError::ReadCaFile { ref source, .. } => {
                io::Error::new(source.kind(), error)
            }
            BuildCustomCaTransportError::InvalidCaFile { .. }
            | BuildCustomCaTransportError::RegisterCertificate { .. }
            | BuildCustomCaTransportError::RegisterRustlsCertificate { .. } => {
                io::Error::new(io::ErrorKind::InvalidData, error)
            }
            BuildCustomCaTransportError::BuildClientWithCustomCa { .. }
            | BuildCustomCaTransportError::BuildClientWithSystemRoots(_)
            | BuildCustomCaTransportError::BuildClientWithExplicitRoots(_) => {
                io::Error::other(error)
            }
        }
    }
}

/// Builds a reqwest client that honors Codex custom CA environment variables.
///
/// Callers supply the baseline builder configuration they need, and this helper layers in custom
/// CA handling before finally constructing the client. `CODEX_CA_CERTIFICATE` takes precedence
/// over `SSL_CERT_FILE`, and empty values for either are treated as unset so callers do not
/// accidentally turn `VAR=""` into a bogus path lookup.
///
/// Callers that build a raw `reqwest::Client` directly bypass this policy entirely. That is an
/// easy mistake to make when adding a new outbound Codex HTTP path, and the resulting bug only
/// shows up in environments where a proxy or gateway requires a custom root CA.
///
/// # Errors
///
/// Returns a [`BuildCustomCaTransportError`] when the configured CA file is unreadable,
/// malformed, or contains a certificate block that `reqwest` cannot register as a root.
pub fn build_reqwest_client_with_custom_ca(
    builder: reqwest::ClientBuilder,
) -> Result<reqwest::Client, BuildCustomCaTransportError> {
    build_reqwest_client_with_env(&ProcessEnv, builder)
}

pub(crate) fn build_reqwest_client_with_custom_ca_policy(
    builder: reqwest::ClientBuilder,
    custom_ca_policy: CustomCaPolicy,
) -> Result<reqwest::Client, BuildCustomCaTransportError> {
    build_reqwest_client_with_env_and_policy(&ProcessEnv, builder, custom_ca_policy)
}

/// Builds a blocking reqwest client that honors Codex custom CA environment variables.
///
/// This is the blocking sibling of [`build_reqwest_client_with_custom_ca`]. Callers supply their
/// baseline builder configuration, and this helper preserves it while applying the same custom CA
/// selection, parsing, and error policy as asynchronous HTTP clients.
pub fn build_blocking_reqwest_client_with_custom_ca(
    builder: reqwest::blocking::ClientBuilder,
) -> Result<reqwest::blocking::Client, BuildCustomCaTransportError> {
    build_blocking_reqwest_client_with_env(&ProcessEnv, builder)
}

pub(crate) fn build_blocking_reqwest_client_with_custom_ca_policy(
    builder: reqwest::blocking::ClientBuilder,
    custom_ca_policy: CustomCaPolicy,
) -> Result<reqwest::blocking::Client, BuildCustomCaTransportError> {
    build_blocking_reqwest_client_with_env_and_policy(&ProcessEnv, builder, custom_ca_policy)
}

/// Builds a rustls client config when a Codex custom CA bundle is configured.
///
/// This is the websocket-facing sibling of [`build_reqwest_client_with_custom_ca`]. When
/// `CODEX_CA_CERTIFICATE` or `SSL_CERT_FILE` selects a CA bundle, the returned config starts from
/// the platform native roots and then adds the configured custom CA certificates. When no custom
/// CA env var is set, this returns `Ok(None)` so websocket callers can keep using their ordinary
/// default connector path.
///
/// Callers that let tungstenite build its default TLS connector directly bypass this policy
/// entirely. That bug only shows up in environments where secure websocket traffic needs the same
/// enterprise root CA bundle as HTTPS traffic.
pub fn maybe_build_rustls_client_config_with_custom_ca()
-> Result<Option<Arc<ClientConfig>>, BuildCustomCaTransportError> {
    let Some(bundle) = ProcessEnv.configured_ca_bundle() else {
        return Ok(None);
    };
    cached_rustls_client_config(Some(&bundle)).map(Some)
}

/// Builds a rustls client config using native roots and any configured Codex custom CA bundle.
///
/// Unlike [`maybe_build_rustls_client_config_with_custom_ca`], this always returns a config. Use
/// this when the caller must perform TLS itself instead of delegating default configuration to a
/// transport library.
pub fn build_rustls_client_config_with_custom_ca()
-> Result<Arc<ClientConfig>, BuildCustomCaTransportError> {
    let bundle = ProcessEnv.configured_ca_bundle();
    cached_rustls_client_config(bundle.as_ref())
}

/// Builds a reqwest client for spawned subprocess tests that exercise CA behavior.
///
/// This is the test-only client-construction path used by the subprocess coverage in `tests/`.
/// The module-level docs explain the hermeticity problem in full; this helper only addresses the
/// reqwest proxy-discovery panic side of that problem by disabling proxy autodetection. The tests
/// still scrub inherited CA environment variables themselves. Normal production callers should use
/// [`build_reqwest_client_with_custom_ca`] so test-only proxy behavior does not leak into
/// ordinary client construction.
pub fn build_reqwest_client_for_subprocess_tests(
    builder: reqwest::ClientBuilder,
) -> Result<reqwest::Client, BuildCustomCaTransportError> {
    build_reqwest_client_with_env(&ProcessEnv, builder.no_proxy())
}

#[cfg(test)]
fn maybe_build_rustls_client_config_with_env(
    env_source: &dyn EnvSource,
) -> Result<Option<Arc<ClientConfig>>, BuildCustomCaTransportError> {
    let Some(bundle) = env_source.configured_ca_bundle() else {
        return Ok(None);
    };

    build_rustls_client_config(Some(&bundle)).map(Some)
}

fn cached_rustls_client_config(
    bundle: Option<&ConfiguredCaBundle>,
) -> Result<Arc<ClientConfig>, BuildCustomCaTransportError> {
    let pem_data = bundle.map(ConfiguredCaBundle::read_pem_data).transpose()?;
    let key = CaBundleCacheKey::new(bundle, pem_data.as_deref());
    let mut configs = RUSTLS_CLIENT_CONFIGS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(config) = configs.get(&key) {
        return Ok(config.clone());
    }

    let config = build_rustls_client_config_from_pem_data(bundle, pem_data.as_deref())?;
    configs.retain(|cached_key, _| cached_key.source_env != key.source_env);
    configs.insert(key, config.clone());
    Ok(config)
}

fn cached_reqwest_certificates(
    bundle: &ConfiguredCaBundle,
) -> Result<Arc<Vec<reqwest::Certificate>>, BuildCustomCaTransportError> {
    let pem_data = bundle.read_pem_data()?;
    let key = CaBundleCacheKey::new(Some(bundle), Some(&pem_data));
    let mut cached = REQWEST_CA_CERTIFICATES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(certificates) = cached.get(&key) {
        return Ok(certificates.clone());
    }

    let certificates = bundle
        .load_certificates_from_pem_data(&pem_data)?
        .into_iter()
        .enumerate()
        .map(|(idx, cert)| {
            reqwest::Certificate::from_der(cert.as_ref()).map_err(|source| {
                warn!(
                    source_env = bundle.source_env,
                    ca_path = %bundle.path.display(),
                    certificate_index = idx + 1,
                    error = %source,
                    "failed to register CA certificate"
                );
                BuildCustomCaTransportError::RegisterCertificate {
                    source_env: bundle.source_env,
                    path: bundle.path.clone(),
                    certificate_index: idx + 1,
                    source,
                }
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let certificates = Arc::new(certificates);
    cached.retain(|cached_key, _| cached_key.source_env != key.source_env);
    cached.insert(key, certificates.clone());
    Ok(certificates)
}

#[cfg(test)]
fn build_rustls_client_config(
    bundle: Option<&ConfiguredCaBundle>,
) -> Result<Arc<ClientConfig>, BuildCustomCaTransportError> {
    let pem_data = bundle.map(ConfiguredCaBundle::read_pem_data).transpose()?;
    build_rustls_client_config_from_pem_data(bundle, pem_data.as_deref())
}

fn build_rustls_client_config_from_pem_data(
    bundle: Option<&ConfiguredCaBundle>,
    pem_data: Option<&[u8]>,
) -> Result<Arc<ClientConfig>, BuildCustomCaTransportError> {
    let Some(bundle) = bundle else {
        return Ok(default_rustls_client_config());
    };
    let Some(pem_data) = pem_data else {
        return Err(BuildCustomCaTransportError::InvalidCaFile {
            source_env: bundle.source_env,
            path: bundle.path.clone(),
            detail: "configured CA bundle PEM data was not loaded".to_string(),
        });
    };

    ensure_rustls_crypto_provider();

    // Start from the platform roots so websocket callers keep the same baseline trust behavior
    // they would get from tungstenite's default rustls connector, then layer in the Codex custom
    // CA bundle on top when configured.
    let mut root_store = native_root_store().clone();

    let certificates = bundle.load_certificates_from_pem_data(pem_data)?;
    for (idx, cert) in certificates.into_iter().enumerate() {
        if let Err(source) = root_store.add(cert) {
            warn!(
                source_env = bundle.source_env,
                ca_path = %bundle.path.display(),
                certificate_index = idx + 1,
                error = %source,
                "failed to register CA certificate in rustls root store"
            );
            return Err(BuildCustomCaTransportError::RegisterRustlsCertificate {
                source_env: bundle.source_env,
                path: bundle.path.clone(),
                certificate_index: idx + 1,
                source,
            });
        }
    }

    Ok(Arc::new(
        ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth(),
    ))
}

/// Builds a reqwest client using an injected environment source and reqwest builder.
///
/// This exists so tests can exercise precedence behavior deterministically without mutating the
/// real process environment. It selects the CA bundle, delegates file parsing to
/// the shared content-keyed certificate cache, preserves the caller's chosen `reqwest` builder
/// configuration, forces rustls when a custom CA is configured, and finally registers each parsed
/// certificate with that builder.
fn build_reqwest_client_with_env(
    env_source: &dyn EnvSource,
    builder: reqwest::ClientBuilder,
) -> Result<reqwest::Client, BuildCustomCaTransportError> {
    build_reqwest_client_with_env_and_policy(
        env_source,
        builder,
        CustomCaPolicy::HonorProcessEnvironment,
    )
}

fn build_reqwest_client_with_env_and_policy(
    env_source: &dyn EnvSource,
    mut builder: reqwest::ClientBuilder,
    custom_ca_policy: CustomCaPolicy,
) -> Result<reqwest::Client, BuildCustomCaTransportError> {
    if let Some(bundle) = matches!(custom_ca_policy, CustomCaPolicy::HonorProcessEnvironment)
        .then(|| env_source.configured_ca_bundle())
        .flatten()
    {
        ensure_rustls_crypto_provider();
        info!(
            source_env = bundle.source_env,
            ca_path = %bundle.path.display(),
            "building HTTP client with rustls backend for custom CA bundle"
        );
        builder = builder.use_rustls_tls();

        let certificates = cached_reqwest_certificates(&bundle)?;
        for certificate in certificates.iter().cloned() {
            builder = builder.add_root_certificate(certificate);
        }
        return match builder.build() {
            Ok(client) => Ok(client),
            Err(source) => {
                warn!(
                    source_env = bundle.source_env,
                    ca_path = %bundle.path.display(),
                    error = %source,
                    "failed to build client after loading custom CA bundle"
                );
                Err(BuildCustomCaTransportError::BuildClientWithCustomCa {
                    source_env: bundle.source_env,
                    path: bundle.path.clone(),
                    source,
                })
            }
        };
    }

    match custom_ca_policy {
        CustomCaPolicy::HonorProcessEnvironment => info!(
            codex_ca_certificate_configured = false,
            ssl_cert_file_configured = false,
            "using system root certificates because no CA override environment variable was selected"
        ),
        CustomCaPolicy::ExplicitRootSet => info!(
            "using the caller's explicit root certificate set without process custom CA augmentation"
        ),
    }

    match builder.build() {
        Ok(client) => Ok(client),
        Err(source) => match custom_ca_policy {
            CustomCaPolicy::HonorProcessEnvironment => {
                warn!(
                    error = %source,
                    "failed to build client while using system root certificates"
                );
                Err(BuildCustomCaTransportError::BuildClientWithSystemRoots(
                    source,
                ))
            }
            CustomCaPolicy::ExplicitRootSet => {
                warn!(
                    error = %source,
                    "failed to build client while using the explicit root certificate set"
                );
                Err(BuildCustomCaTransportError::BuildClientWithExplicitRoots(
                    source,
                ))
            }
        },
    }
}

fn build_blocking_reqwest_client_with_env(
    env_source: &dyn EnvSource,
    builder: reqwest::blocking::ClientBuilder,
) -> Result<reqwest::blocking::Client, BuildCustomCaTransportError> {
    build_blocking_reqwest_client_with_env_and_policy(
        env_source,
        builder,
        CustomCaPolicy::HonorProcessEnvironment,
    )
}

fn build_blocking_reqwest_client_with_env_and_policy(
    env_source: &dyn EnvSource,
    mut builder: reqwest::blocking::ClientBuilder,
    custom_ca_policy: CustomCaPolicy,
) -> Result<reqwest::blocking::Client, BuildCustomCaTransportError> {
    if let Some(bundle) = matches!(custom_ca_policy, CustomCaPolicy::HonorProcessEnvironment)
        .then(|| env_source.configured_ca_bundle())
        .flatten()
    {
        ensure_rustls_crypto_provider();
        info!(
            source_env = bundle.source_env,
            ca_path = %bundle.path.display(),
            "building blocking HTTP client with rustls backend for custom CA bundle"
        );
        builder = builder.use_rustls_tls();

        let certificates = cached_reqwest_certificates(&bundle)?;
        for certificate in certificates.iter().cloned() {
            builder = builder.add_root_certificate(certificate);
        }

        return builder.build().map_err(|source| {
            warn!(
                source_env = bundle.source_env,
                ca_path = %bundle.path.display(),
                error = %source,
                "failed to build blocking client after loading custom CA bundle"
            );
            BuildCustomCaTransportError::BuildClientWithCustomCa {
                source_env: bundle.source_env,
                path: bundle.path,
                source,
            }
        });
    }

    match custom_ca_policy {
        CustomCaPolicy::HonorProcessEnvironment => info!(
            codex_ca_certificate_configured = false,
            ssl_cert_file_configured = false,
            "using system root certificates because no CA override environment variable was selected"
        ),
        CustomCaPolicy::ExplicitRootSet => info!(
            "using the caller's explicit root certificate set without process custom CA augmentation"
        ),
    }
    builder.build().map_err(|source| match custom_ca_policy {
        CustomCaPolicy::HonorProcessEnvironment => {
            warn!(
                error = %source,
                "failed to build blocking client while using system root certificates"
            );
            BuildCustomCaTransportError::BuildClientWithSystemRoots(source)
        }
        CustomCaPolicy::ExplicitRootSet => {
            warn!(
                error = %source,
                "failed to build blocking client while using the explicit root certificate set"
            );
            BuildCustomCaTransportError::BuildClientWithExplicitRoots(source)
        }
    })
}

/// Abstracts environment access so tests can cover precedence rules without mutating process-wide
/// variables.
trait EnvSource {
    /// Returns the environment variable value for `key`, if this source considers it set.
    ///
    /// Implementations should return `None` for absent values and may also collapse unreadable
    /// process-environment states into `None`, because the custom CA logic treats both cases as
    /// "no override configured". Callers build precedence and empty-string handling on top of this
    /// method, so implementations should not trim or normalize the returned string.
    fn var(&self, key: &str) -> Option<String>;

    /// Returns a non-empty environment variable value interpreted as a filesystem path.
    ///
    /// Empty strings are treated as unset because presence here acts as a boolean "custom CA
    /// override requested" signal. This keeps the precedence logic from treating `VAR=""` as an
    /// attempt to open the current working directory or some other platform-specific oddity once
    /// it is converted into a path.
    fn non_empty_path(&self, key: &str) -> Option<PathBuf> {
        self.var(key)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    }

    /// Returns the configured CA bundle and which environment variable selected it.
    ///
    /// `CODEX_CA_CERTIFICATE` wins over `SSL_CERT_FILE` because it is the Codex-specific override.
    /// Keeping the winning variable name with the path lets later logging explain not only which
    /// file was used but also why that file was chosen.
    fn configured_ca_bundle(&self) -> Option<ConfiguredCaBundle> {
        self.non_empty_path(CODEX_CA_CERT_ENV)
            .map(|path| ConfiguredCaBundle {
                source_env: CODEX_CA_CERT_ENV,
                path,
            })
            .or_else(|| {
                self.non_empty_path(SSL_CERT_FILE_ENV)
                    .map(|path| ConfiguredCaBundle {
                        source_env: SSL_CERT_FILE_ENV,
                        path,
                    })
            })
    }
}

/// Reads CA configuration from the real process environment.
///
/// This is the production `EnvSource` implementation used by
/// [`build_reqwest_client_with_custom_ca`]. Tests substitute in-memory env maps so they can
/// exercise precedence and empty-value behavior without mutating process-global variables.
struct ProcessEnv;

impl EnvSource for ProcessEnv {
    fn var(&self, key: &str) -> Option<String> {
        env::var(key).ok()
    }
}

/// Identifies the CA bundle selected for a client and the policy decision that selected it.
///
/// This is the concrete output of the environment-precedence logic. Callers use `source_env` for
/// logging and diagnostics, while `path` is the bundle that will actually be loaded.
struct ConfiguredCaBundle {
    /// The environment variable that won the precedence check for this bundle.
    source_env: &'static str,
    /// The filesystem path that should be read as PEM certificate input.
    path: PathBuf,
}

impl ConfiguredCaBundle {
    fn load_certificates_from_pem_data(
        &self,
        pem_data: &[u8],
    ) -> Result<Vec<CertificateDer<'static>>, BuildCustomCaTransportError> {
        match self.parse_certificates(pem_data) {
            Ok(certificates) => {
                info!(
                    source_env = self.source_env,
                    ca_path = %self.path.display(),
                    certificate_count = certificates.len(),
                    "loaded certificates from custom CA bundle"
                );
                Ok(certificates)
            }
            Err(error) => {
                warn!(
                    source_env = self.source_env,
                    ca_path = %self.path.display(),
                    error = %error,
                    "failed to load custom CA bundle"
                );
                Err(error)
            }
        }
    }

    /// Loads every certificate block from a PEM file intended for Codex CA overrides.
    ///
    /// This accepts a few common real-world variants so Codex behaves like other CA-aware tooling:
    /// leading comments are preserved, `TRUSTED CERTIFICATE` labels are normalized to standard
    /// certificate labels, and embedded CRLs are ignored when they are well-formed enough for the
    /// section iterator to classify them.
    fn parse_certificates(
        &self,
        pem_data: &[u8],
    ) -> Result<Vec<CertificateDer<'static>>, BuildCustomCaTransportError> {
        let normalized_pem = NormalizedPem::from_pem_data(self.source_env, &self.path, pem_data);

        let mut certificates = Vec::new();
        let mut logged_crl_presence = false;
        for section_result in normalized_pem.sections() {
            // Known limitation: if `rustls-pki-types` fails while parsing a malformed CRL section,
            // that error is reported here before we can classify the block as ignorable. A bundle
            // containing valid certificates plus a malformed `X509 CRL` therefore still fails to
            // load today, even though well-formed CRLs are ignored.
            let (section_kind, der) = match section_result {
                Ok(section) => section,
                Err(error) => return Err(self.pem_parse_error(&error)),
            };
            match section_kind {
                SectionKind::Certificate => {
                    // Standard CERTIFICATE blocks already decode to the exact DER bytes reqwest
                    // wants. Only OpenSSL TRUSTED CERTIFICATE blocks need trimming to drop any
                    // trailing X509_AUX trust metadata before registration.
                    let cert_der = normalized_pem.certificate_der(&der).ok_or_else(|| {
                        self.invalid_ca_file(
                            "failed to extract certificate data from TRUSTED CERTIFICATE: invalid DER length",
                        )
                    })?;
                    certificates.push(CertificateDer::from(cert_der.to_vec()));
                }
                SectionKind::Crl if !logged_crl_presence => {
                    info!(
                        source_env = self.source_env,
                        ca_path = %self.path.display(),
                        "ignoring X509 CRL entries found in custom CA bundle"
                    );
                    logged_crl_presence = true;
                }
                _ => {}
            }
        }

        if certificates.is_empty() {
            return Err(self.pem_parse_error(&pem::Error::NoItemsFound));
        }

        Ok(certificates)
    }

    /// Reads the CA bundle bytes while preserving the original filesystem error kind.
    ///
    /// The caller wants a user-facing error that includes the bundle path and remediation hint, but
    /// higher-level surfaces still benefit from distinguishing "not found" from other I/O
    /// failures. This helper keeps both pieces together.
    fn read_pem_data(&self) -> Result<Vec<u8>, BuildCustomCaTransportError> {
        fs::read(&self.path).map_err(|source| BuildCustomCaTransportError::ReadCaFile {
            source_env: self.source_env,
            path: self.path.clone(),
            source,
        })
    }

    /// Rewrites PEM parsing failures into user-facing configuration errors.
    ///
    /// The underlying parser knows whether the file was empty, malformed, or contained unsupported
    /// PEM content, but callers need a message that also points them back to the relevant
    /// environment variables and the expected remediation.
    fn pem_parse_error(&self, error: &pem::Error) -> BuildCustomCaTransportError {
        let detail = match error {
            pem::Error::NoItemsFound => "no certificates found in PEM file".to_string(),
            _ => format!("failed to parse PEM file: {error}"),
        };

        self.invalid_ca_file(detail)
    }

    /// Creates an invalid-CA error tied to this file path.
    ///
    /// Most parse-time failures in this module eventually collapse to "the configured CA bundle is
    /// not usable", but the detailed reason still matters for operator debugging. Centralizing that
    /// formatting keeps the path and hint text consistent across the different parser branches.
    fn invalid_ca_file(&self, detail: impl std::fmt::Display) -> BuildCustomCaTransportError {
        BuildCustomCaTransportError::InvalidCaFile {
            source_env: self.source_env,
            path: self.path.clone(),
            detail: detail.to_string(),
        }
    }
}

/// The PEM text shape after OpenSSL compatibility normalization.
///
/// `Standard` means the input already used ordinary PEM certificate labels. `TrustedCertificate`
/// means the input used OpenSSL's `TRUSTED CERTIFICATE` labels, so callers must also be prepared
/// to trim trailing `X509_AUX` bytes from decoded certificate sections.
enum NormalizedPem {
    /// PEM contents that already used ordinary `CERTIFICATE` labels.
    Standard(String),
    /// PEM contents rewritten from OpenSSL `TRUSTED CERTIFICATE` labels to `CERTIFICATE`.
    TrustedCertificate(String),
}

impl NormalizedPem {
    /// Normalizes PEM text from a CA bundle into the label shape this module expects.
    ///
    /// Codex only needs certificate DER bytes to seed `reqwest`'s root store, but operators may
    /// point it at CA files that came from OpenSSL tooling rather than from a minimal certificate
    /// bundle. OpenSSL's `TRUSTED CERTIFICATE` form is one such variant: it is still certificate
    /// material, but it uses a different PEM label and may carry auxiliary trust metadata that
    /// this crate does not consume. This constructor rewrites only the PEM labels so the mixed-
    /// section parser can keep treating the file as certificate input. The rustls ecosystem does
    /// not currently accept `TRUSTED CERTIFICATE` as a standard certificate label upstream, so
    /// this remains a local compatibility shim rather than behavior delegated to
    /// `rustls-pki-types`.
    ///
    /// See also:
    /// - rustls/pemfile issue #52, closed as not planned, documenting that
    ///   `BEGIN TRUSTED CERTIFICATE` blocks are ignored upstream:
    ///   <https://github.com/rustls/pemfile/issues/52>
    /// - OpenSSL `x509 -trustout`, which emits `TRUSTED CERTIFICATE` PEM blocks:
    ///   <https://docs.openssl.org/master/man1/openssl-x509/>
    /// - OpenSSL PEM readers, which document that plain `PEM_read_bio_X509()` discards auxiliary
    ///   trust settings:
    ///   <https://docs.openssl.org/master/man3/PEM_read_bio_PrivateKey/>
    /// - `openssl s_server`, a real OpenSSL-based server/test tool that operates in this
    ///   ecosystem:
    ///   <https://docs.openssl.org/master/man1/openssl-s_server/>
    fn from_pem_data(source_env: &'static str, path: &Path, pem_data: &[u8]) -> Self {
        let pem = String::from_utf8_lossy(pem_data);
        if pem.contains("TRUSTED CERTIFICATE") {
            info!(
                source_env,
                ca_path = %path.display(),
                "normalizing OpenSSL TRUSTED CERTIFICATE labels in custom CA bundle"
            );
            Self::TrustedCertificate(
                pem.replace("BEGIN TRUSTED CERTIFICATE", "BEGIN CERTIFICATE")
                    .replace("END TRUSTED CERTIFICATE", "END CERTIFICATE"),
            )
        } else {
            Self::Standard(pem.into_owned())
        }
    }

    /// Returns the normalized PEM contents regardless of the label shape that produced them.
    fn contents(&self) -> &str {
        match self {
            Self::Standard(contents) | Self::TrustedCertificate(contents) => contents,
        }
    }

    /// Iterates over every recognized PEM section in this normalized PEM text.
    ///
    /// `rustls-pki-types` exposes mixed-section parsing through a `PemObject` implementation on the
    /// `(SectionKind, Vec<u8>)` tuple. Keeping that type-directed API here lets callers iterate in
    /// terms of normalized sections rather than trait plumbing.
    fn sections(&self) -> impl Iterator<Item = Result<PemSection, pem::Error>> + '_ {
        PemSection::pem_slice_iter(self.contents().as_bytes())
    }

    /// Returns the certificate DER bytes for one parsed PEM certificate section.
    ///
    /// Standard PEM certificates already decode to the exact DER bytes `reqwest` wants. OpenSSL
    /// `TRUSTED CERTIFICATE` sections may append `X509_AUX` bytes after the certificate, so those
    /// sections need to be trimmed down to their first DER object before registration.
    fn certificate_der<'a>(&self, der: &'a [u8]) -> Option<&'a [u8]> {
        match self {
            Self::Standard(_) => Some(der),
            Self::TrustedCertificate(_) => first_der_item(der),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;
    use std::fs::FileTimes;
    use std::fs::OpenOptions;
    use std::path::PathBuf;
    use std::sync::Arc;

    use pretty_assertions::assert_eq;
    use tempfile::TempDir;

    use super::BuildCustomCaTransportError;
    use super::CODEX_CA_CERT_ENV;
    use super::CaBundleCacheKey;
    use super::ConfiguredCaBundle;
    use super::CustomCaPolicy;
    use super::EnvSource;
    use super::RUSTLS_CLIENT_CONFIGS;
    use super::SSL_CERT_FILE_ENV;
    use super::build_blocking_reqwest_client_with_env;
    use super::build_blocking_reqwest_client_with_env_and_policy;
    use super::build_reqwest_client_with_env_and_policy;
    use super::build_rustls_client_config;
    use super::cached_reqwest_certificates;
    use super::cached_rustls_client_config;
    use super::maybe_build_rustls_client_config_with_env;
    use super::native_root_store;

    const TEST_CERT: &str = include_str!("../tests/fixtures/test-ca.pem");

    struct MapEnv {
        values: HashMap<String, String>,
    }

    impl EnvSource for MapEnv {
        fn var(&self, key: &str) -> Option<String> {
            self.values.get(key).cloned()
        }
    }

    fn map_env(pairs: &[(&str, &str)]) -> MapEnv {
        MapEnv {
            values: pairs
                .iter()
                .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
                .collect(),
        }
    }

    fn write_cert_file(temp_dir: &TempDir, name: &str, contents: &str) -> PathBuf {
        let path = temp_dir.path().join(name);
        fs::write(&path, contents).unwrap_or_else(|error| {
            panic!("write cert fixture failed for {}: {error}", path.display())
        });
        path
    }

    #[test]
    fn ca_path_prefers_codex_env() {
        let env = map_env(&[
            (CODEX_CA_CERT_ENV, "/tmp/codex.pem"),
            (SSL_CERT_FILE_ENV, "/tmp/fallback.pem"),
        ]);

        assert_eq!(
            env.configured_ca_bundle().map(|bundle| bundle.path),
            Some(PathBuf::from("/tmp/codex.pem"))
        );
    }

    #[test]
    fn ca_path_falls_back_to_ssl_cert_file() {
        let env = map_env(&[(SSL_CERT_FILE_ENV, "/tmp/fallback.pem")]);

        assert_eq!(
            env.configured_ca_bundle().map(|bundle| bundle.path),
            Some(PathBuf::from("/tmp/fallback.pem"))
        );
    }

    #[test]
    fn ca_path_ignores_empty_values() {
        let env = map_env(&[
            (CODEX_CA_CERT_ENV, ""),
            (SSL_CERT_FILE_ENV, "/tmp/fallback.pem"),
        ]);

        assert_eq!(
            env.configured_ca_bundle().map(|bundle| bundle.path),
            Some(PathBuf::from("/tmp/fallback.pem"))
        );
    }

    #[test]
    fn rustls_config_uses_custom_ca_bundle_when_configured() {
        let temp_dir = TempDir::new().expect("tempdir");
        let cert_path = write_cert_file(&temp_dir, "ca.pem", TEST_CERT);
        let env = map_env(&[(CODEX_CA_CERT_ENV, cert_path.to_string_lossy().as_ref())]);

        let config = maybe_build_rustls_client_config_with_env(&env)
            .expect("rustls config")
            .expect("custom CA config should be present");

        assert!(config.enable_sni);
    }

    #[test]
    fn rustls_native_root_store_is_reused() {
        let first = native_root_store();
        let second = native_root_store();

        assert!(std::ptr::eq(first, second));
    }

    #[test]
    fn rustls_client_config_is_reused_for_same_ca_state() {
        let first = cached_rustls_client_config(None).expect("build first rustls config");
        let second = cached_rustls_client_config(None).expect("reuse rustls config");

        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn rustls_client_config_cache_detects_same_metadata_ca_rotation() {
        const ROTATION_TEST_SOURCE: &str = "CODEX_CA_CERTIFICATE_ROTATION_TEST";
        let temp_dir = TempDir::new().expect("tempdir");
        let first_pem = format!("# rotation A\n{TEST_CERT}");
        let second_pem = format!("# rotation B\n{TEST_CERT}");
        assert_eq!(first_pem.len(), second_pem.len());
        let cert_path = write_cert_file(&temp_dir, "rotating-ca.pem", &first_pem);
        let original_metadata = fs::metadata(&cert_path).expect("first CA metadata");
        let original_modified = original_metadata.modified().expect("first CA mtime");
        let bundle = ConfiguredCaBundle {
            source_env: ROTATION_TEST_SOURCE,
            path: cert_path.clone(),
        };

        let first = cached_rustls_client_config(Some(&bundle)).expect("first rustls config");
        let first_key = CaBundleCacheKey::new(Some(&bundle), Some(first_pem.as_bytes()));
        fs::write(&cert_path, second_pem).expect("rotate CA contents");
        OpenOptions::new()
            .write(true)
            .open(&cert_path)
            .expect("open rotated CA")
            .set_times(FileTimes::new().set_modified(original_modified))
            .expect("restore CA mtime");
        let rotated_metadata = fs::metadata(&cert_path).expect("rotated CA metadata");
        assert_eq!(rotated_metadata.len(), original_metadata.len());
        assert_eq!(
            rotated_metadata.modified().expect("rotated CA mtime"),
            original_modified
        );

        let second = cached_rustls_client_config(Some(&bundle)).expect("rotated rustls config");
        let reused = cached_rustls_client_config(Some(&bundle)).expect("reused rustls config");

        assert!(!Arc::ptr_eq(&first, &second));
        assert!(Arc::ptr_eq(&second, &reused));
        let configs = RUSTLS_CLIENT_CONFIGS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(!configs.contains_key(&first_key));
        assert_eq!(
            configs
                .keys()
                .filter(|key| key.source_env == Some(ROTATION_TEST_SOURCE))
                .count(),
            1
        );
    }

    #[test]
    fn reqwest_certificate_cache_reuses_content_and_detects_same_metadata_rotation() {
        const ROTATION_TEST_SOURCE: &str = "CODEX_REQWEST_CA_CERTIFICATE_ROTATION_TEST";
        let temp_dir = TempDir::new().expect("tempdir");
        let first_pem = format!("# rotation A\n{TEST_CERT}");
        let second_pem = format!("# rotation B\n{TEST_CERT}");
        assert_eq!(first_pem.len(), second_pem.len());
        let cert_path = write_cert_file(&temp_dir, "rotating-reqwest-ca.pem", &first_pem);
        let original_metadata = fs::metadata(&cert_path).expect("first CA metadata");
        let original_modified = original_metadata.modified().expect("first CA mtime");
        let bundle = ConfiguredCaBundle {
            source_env: ROTATION_TEST_SOURCE,
            path: cert_path.clone(),
        };

        let first = cached_reqwest_certificates(&bundle).expect("first parsed certificates");
        let reused = cached_reqwest_certificates(&bundle).expect("reused parsed certificates");
        assert!(Arc::ptr_eq(&first, &reused));

        fs::write(&cert_path, second_pem).expect("rotate CA contents");
        OpenOptions::new()
            .write(true)
            .open(&cert_path)
            .expect("open rotated CA")
            .set_times(FileTimes::new().set_modified(original_modified))
            .expect("restore CA mtime");
        let rotated_metadata = fs::metadata(&cert_path).expect("rotated CA metadata");
        assert_eq!(rotated_metadata.len(), original_metadata.len());
        assert_eq!(
            rotated_metadata.modified().expect("rotated CA mtime"),
            original_modified
        );

        let rotated = cached_reqwest_certificates(&bundle).expect("rotated parsed certificates");
        assert!(!Arc::ptr_eq(&first, &rotated));
        assert!(Arc::ptr_eq(
            &rotated,
            &cached_reqwest_certificates(&bundle).expect("reused rotated certificates")
        ));
    }

    #[test]
    fn default_rustls_client_config_is_reused() {
        let first = build_rustls_client_config(None).expect("first default rustls config");
        let second = build_rustls_client_config(None).expect("second default rustls config");

        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn rustls_config_reports_invalid_ca_file() {
        let temp_dir = TempDir::new().expect("tempdir");
        let cert_path = write_cert_file(&temp_dir, "empty.pem", "");
        let env = map_env(&[(CODEX_CA_CERT_ENV, cert_path.to_string_lossy().as_ref())]);

        let error = maybe_build_rustls_client_config_with_env(&env).expect_err("invalid CA");

        assert!(matches!(
            error,
            BuildCustomCaTransportError::InvalidCaFile { .. }
        ));
    }

    #[test]
    fn blocking_client_reports_invalid_ca_file() {
        let temp_dir = TempDir::new().expect("tempdir");
        let cert_path = write_cert_file(&temp_dir, "empty.pem", "");
        let env = map_env(&[(CODEX_CA_CERT_ENV, cert_path.to_string_lossy().as_ref())]);

        let error = build_blocking_reqwest_client_with_env(
            &env,
            reqwest::blocking::Client::builder().no_proxy(),
        )
        .expect_err("invalid CA");

        assert!(matches!(
            error,
            BuildCustomCaTransportError::InvalidCaFile { .. }
        ));
    }

    #[test]
    fn explicit_async_roots_ignore_process_custom_ca_file() {
        let env = map_env(&[(CODEX_CA_CERT_ENV, "missing-process-ca.pem")]);
        let certificate = reqwest::Certificate::from_pem(TEST_CERT.as_bytes())
            .expect("valid explicit CA certificate");

        let client = build_reqwest_client_with_env_and_policy(
            &env,
            reqwest::Client::builder()
                .no_proxy()
                .tls_certs_only(vec![certificate]),
            CustomCaPolicy::ExplicitRootSet,
        );

        assert!(client.is_ok());
    }

    #[test]
    fn explicit_blocking_roots_ignore_process_custom_ca_file() {
        let env = map_env(&[(CODEX_CA_CERT_ENV, "missing-process-ca.pem")]);
        let certificate = reqwest::Certificate::from_pem(TEST_CERT.as_bytes())
            .expect("valid explicit CA certificate");

        let client = build_blocking_reqwest_client_with_env_and_policy(
            &env,
            reqwest::blocking::Client::builder()
                .no_proxy()
                .tls_certs_only(vec![certificate]),
            CustomCaPolicy::ExplicitRootSet,
        );

        assert!(client.is_ok());
    }
}
