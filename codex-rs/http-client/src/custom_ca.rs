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

use std::borrow::Cow;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::Mutex;

use codex_utils_der::trusted_certificate_for_server_auth;
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

static NATIVE_ROOT_STORE: LazyLock<RootCertStore> = LazyLock::new(load_native_root_store);

fn load_native_root_store() -> RootCertStore {
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
}
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
    native_roots_sha256: Option<[u8; 32]>,
}

impl CaBundleCacheKey {
    fn new(bundle: Option<&ConfiguredCaBundle>, pem_data: Option<&[u8]>) -> Self {
        Self {
            source_env: bundle.map(|bundle| bundle.source_env),
            path: bundle.map(|bundle| bundle.path.clone()),
            pem_sha256: pem_data.map(|pem_data| Sha256::digest(pem_data).into()),
            native_roots_sha256: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CustomCaPolicy {
    HonorProcessEnvironment,
    ExplicitRootSet,
}

pub(crate) fn parse_explicit_root_certificates(
    pem: &[u8],
) -> io::Result<Vec<reqwest::Certificate>> {
    let certificates = reqwest::Certificate::from_pem_bundle(pem)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if certificates.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "no certificates found in explicit root PEM",
        ));
    }
    Ok(certificates)
}

fn native_root_store() -> Cow<'static, RootCertStore> {
    // The native loader replaces OS roots with these mutable sources. Never put their
    // contents into the permanent platform snapshot; include the resulting roots in cache identity.
    if env::var_os(SSL_CERT_FILE_ENV).is_some() || env::var_os("SSL_CERT_DIR").is_some() {
        Cow::Owned(load_native_root_store())
    } else {
        Cow::Borrowed(&NATIVE_ROOT_STORE)
    }
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
/// An empty SSL_CERT_FILE is rejected; unset it to select system roots.
///
/// Callers that let tungstenite build its default TLS connector directly bypass this policy
/// entirely. That bug only shows up in environments where secure websocket traffic needs the same
/// enterprise root CA bundle as HTTPS traffic.
pub fn maybe_build_rustls_client_config_with_custom_ca()
-> Result<Option<Arc<ClientConfig>>, BuildCustomCaTransportError> {
    validate_rustls_ca_environment(&ProcessEnv)?;
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
/// An empty SSL_CERT_FILE is rejected; unset it to select system roots.
pub fn build_rustls_client_config_with_custom_ca()
-> Result<Arc<ClientConfig>, BuildCustomCaTransportError> {
    validate_rustls_ca_environment(&ProcessEnv)?;
    let bundle = ProcessEnv.configured_ca_bundle();
    cached_rustls_client_config(bundle.as_ref())
}



fn cached_rustls_client_config(
    bundle: Option<&ConfiguredCaBundle>,
) -> Result<Arc<ClientConfig>, BuildCustomCaTransportError> {
    let pem_data = bundle.map(ConfiguredCaBundle::read_pem_data).transpose()?;
    let roots = native_root_store();
    let mut key = CaBundleCacheKey::new(bundle, pem_data.as_deref());
    if matches!(&roots, Cow::Owned(_)) {
        let mut hasher = Sha256::new();
        for root in &roots.roots {
            hasher.update([u8::from(root.name_constraints.is_some())]);
            for field in [
                root.subject.as_ref(),
                root.subject_public_key_info.as_ref(),
                root.name_constraints
                    .as_ref()
                    .map_or(&[][..], |value| value.as_ref()),
            ] {
                hasher.update(field.len().to_le_bytes());
                hasher.update(field);
            }
        }
        key.native_roots_sha256 = Some(hasher.finalize().into());
    }
    let mut configs = RUSTLS_CLIENT_CONFIGS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(config) = configs.get(&key) {
        return Ok(config.clone());
    }

    let config = build_rustls_client_config_from_pem_data(bundle, pem_data.as_deref(), &roots)?;
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


fn build_rustls_client_config_from_pem_data(
    bundle: Option<&ConfiguredCaBundle>,
    pem_data: Option<&[u8]>,
    roots: &RootCertStore,
) -> Result<Arc<ClientConfig>, BuildCustomCaTransportError> {
    ensure_rustls_crypto_provider();
    let Some(bundle) = bundle else {
        return Ok(Arc::new(
            ClientConfig::builder()
                .with_root_certificates(roots.clone())
                .with_no_client_auth(),
        ));
    };
    let Some(pem_data) = pem_data else {
        return Err(BuildCustomCaTransportError::InvalidCaFile {
            source_env: bundle.source_env,
            path: bundle.path.clone(),
            detail: "configured CA bundle PEM data was not loaded".to_string(),
        });
    };

    // Start from the platform roots so websocket callers keep the same baseline trust behavior
    // they would get from tungstenite's default rustls connector, then layer in the Codex custom
    // CA bundle on top when configured.
    let mut root_store = roots.clone();

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
/// The injected source controls precedence without changing the real process environment.
/// This selects the CA bundle, delegates file parsing to
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

/// Abstracts environment access without mutating process-wide variables.
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

/// Rejects an empty SSL_CERT_FILE when no Codex-specific CA bundle is configured.
fn validate_rustls_ca_environment(
    env_source: &dyn EnvSource,
) -> Result<(), BuildCustomCaTransportError> {
    if env_source.non_empty_path(CODEX_CA_CERT_ENV).is_none()
        && env_source.var(SSL_CERT_FILE_ENV).as_deref() == Some("") {
        return Err(BuildCustomCaTransportError::InvalidCaFile {
            source_env: SSL_CERT_FILE_ENV,
            path: PathBuf::new(),
            detail: "SSL_CERT_FILE is empty; unset SSL_CERT_FILE to use system roots".into(),
        });
    }
    Ok(())
}

/// Reads CA configuration from the real process environment.
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
    /// leading comments are preserved, unrestricted `TRUSTED CERTIFICATE` labels are normalized,
    /// nonempty AUX policies are rejected, and embedded CRLs are ignored when well-formed enough for the
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
            let (section_kind, der, trusted) = match section_result {
                Ok(section) => section,
                Err(error) => return Err(self.pem_parse_error(&error)),
            };
            match section_kind {
                SectionKind::Certificate => {
                    let cert_der = if trusted {
                        trusted_certificate_for_server_auth(&der)
                            .map_err(|detail| self.invalid_ca_file(detail))?
                    } else {
                        der.as_slice()
                    };
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

/// PEM blocks retain their original label so auxiliary data is trimmed only for trusted certs.
struct NormalizedPem {
    blocks: Vec<(bool, String)>,
}

impl NormalizedPem {
    fn from_pem_data(source_env: &'static str, path: &Path, pem_data: &[u8]) -> Self {
        let pem = String::from_utf8_lossy(pem_data);
        let mut blocks = Vec::new();
        let mut contents = String::new();
        let mut trusted = false;
        for line in pem.lines() {
            if line.starts_with("-----BEGIN ") {
                if !contents.is_empty() {
                    blocks.push((trusted, std::mem::take(&mut contents)));
                }
                trusted = line.trim_end() == "-----BEGIN TRUSTED CERTIFICATE-----";
            }
            let line = if trusted {
                match line.trim_end() {
                    "-----BEGIN TRUSTED CERTIFICATE-----" => "-----BEGIN CERTIFICATE-----",
                    "-----END TRUSTED CERTIFICATE-----" => "-----END CERTIFICATE-----",
                    _ => line,
                }
            } else {
                line
            };
            contents.push_str(line);
            contents.push('\n');
        }
        if !contents.is_empty() {
            blocks.push((trusted, contents));
        }
        if blocks.iter().any(|(trusted, _)| *trusted) {
            info!(source_env, ca_path = %path.display(),
                "normalizing OpenSSL TRUSTED CERTIFICATE labels in custom CA bundle");
        }
        Self { blocks }
    }

    fn sections(
        &self,
    ) -> impl Iterator<Item = Result<(SectionKind, Vec<u8>, bool), pem::Error>> + '_ {
        self.blocks.iter().flat_map(|(trusted, contents)| {
            PemSection::pem_slice_iter(contents.as_bytes())
                .map(move |section| section.map(|(kind, der)| (kind, der, *trusted)))
        })
    }
}

