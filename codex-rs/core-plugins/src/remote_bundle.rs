use crate::plugin_bundle_archive::PluginBundleUnpackError;
use crate::plugin_bundle_archive::unpack_plugin_bundle_tar_gz;
use crate::remote::REMOTE_GLOBAL_MARKETPLACE_NAME;
use crate::store::PluginInstallResult;
use crate::store::PluginStore;
use crate::store::PluginStoreError;
use crate::store::error_context_sub_error_type;
use crate::store::validate_plugin_version_segment;
use codex_http_client::HttpError;
use codex_http_client::HttpResponse;
use codex_http_client::RouteAwareClientPool;
use codex_plugin::PluginId;
use codex_plugin::PluginIdError;
use codex_plugin::find_plugin_manifest_path;
use codex_utils_absolute_path::AbsolutePathBuf;
use http::StatusCode;
use serde_json::Value as JsonValue;
use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use url::Host;
use url::Url;

const REMOTE_PLUGIN_BUNDLE_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(60);
const REMOTE_PLUGIN_BUNDLE_MAX_DOWNLOAD_BYTES: u64 = 100 * 1024 * 1024;
const REMOTE_PLUGIN_BUNDLE_ERROR_BODY_MAX_BYTES: u64 = 8 * 1024;
const REMOTE_PLUGIN_BUNDLE_MAX_EXTRACTED_BYTES: u64 = 512 * 1024 * 1024;
const REMOTE_PLUGIN_INSTALL_STAGING_DIR: &str = "plugins/.remote-plugin-install-staging";
#[cfg(debug_assertions)]
const TEST_ALLOW_LOOPBACK_HTTP_REMOTE_PLUGIN_BUNDLES_ENV: &str =
    "CODEX_TEST_ALLOW_HTTP_REMOTE_PLUGIN_BUNDLE_DOWNLOADS";

#[derive(Debug, Clone)]
pub struct ValidatedRemotePluginBundle {
    pub plugin_id: PluginId,
    pub plugin_version: String,
    remote_plugin_id: String,
    app_manifest: Option<JsonValue>,
    bundle_download_url: String,
}

#[derive(Debug, thiserror::Error)]
pub enum RemotePluginBundleInstallError {
    #[error("failed to build remote plugin bundle HTTP client: {0}")]
    HttpClient(#[from] codex_login::BuildLoginHttpClientError),

    #[error("backend did not return a release version for remote plugin `{remote_plugin_id}`")]
    MissingReleaseVersion { remote_plugin_id: String },

    #[error(
        "backend returned an invalid release version for remote plugin `{remote_plugin_id}`: {message}"
    )]
    InvalidReleaseVersion {
        remote_plugin_id: String,
        message: String,
    },

    #[error("backend did not return a download URL for remote plugin `{remote_plugin_id}`")]
    MissingBundleDownloadUrl { remote_plugin_id: String },

    #[error(
        "backend returned an invalid download URL for remote plugin `{remote_plugin_id}`: {url}"
    )]
    InvalidBundleDownloadUrl {
        remote_plugin_id: String,
        url: String,
        #[source]
        source: url::ParseError,
    },

    #[error(
        "backend returned an unsupported download URL scheme for remote plugin `{remote_plugin_id}`: {scheme}"
    )]
    UnsupportedBundleDownloadUrlScheme {
        remote_plugin_id: String,
        scheme: String,
    },

    #[error(
        "backend returned an invalid local plugin id for remote plugin `{remote_plugin_id}`: {source}"
    )]
    InvalidPluginId {
        remote_plugin_id: String,
        #[source]
        source: PluginIdError,
    },

    #[error("failed to send remote plugin bundle download request to {url}: {source}")]
    DownloadRequest {
        url: String,
        #[source]
        source: codex_http_client::RouteAwareRequestError,
    },

    #[error("remote plugin bundle download from {url} failed with status {status}: {body}")]
    DownloadStatus {
        url: String,
        status: StatusCode,
        body: String,
    },

    #[error("failed to read remote plugin bundle download response from {url}: {source}")]
    DownloadBody {
        url: String,
        #[source]
        source: HttpError,
    },

    #[error("remote plugin bundle download from {url} exceeded maximum size of {max_bytes} bytes")]
    DownloadTooLarge { url: String, max_bytes: u64 },

    #[error("remote plugin bundle download from {url} redirected to unsupported URL {final_url}")]
    UnsupportedBundleDownloadFinalUrl { url: String, final_url: String },

    #[error(
        "remote plugin bundle extracted size would be {bytes} bytes, exceeding the maximum total size of {max_bytes} bytes"
    )]
    ExtractedBundleTooLarge { bytes: u64, max_bytes: u64 },

    #[error("{context}: {source}")]
    Io {
        context: &'static str,
        #[source]
        source: io::Error,
    },

    #[error("{0}")]
    InvalidBundle(String),

    #[error("{0}")]
    Store(#[from] PluginStoreError),
}

impl RemotePluginBundleInstallError {
    fn io(context: &'static str, source: io::Error) -> Self {
        Self::Io { context, source }
    }

    pub fn sub_error_type(&self) -> Option<String> {
        match self {
            Self::Io { context, .. } => Some(error_context_sub_error_type(context)),
            Self::Store(err) => err.sub_error_type(),
            Self::HttpClient(_)
            | Self::MissingReleaseVersion { .. }
            | Self::InvalidReleaseVersion { .. }
            | Self::MissingBundleDownloadUrl { .. }
            | Self::InvalidBundleDownloadUrl { .. }
            | Self::UnsupportedBundleDownloadUrlScheme { .. }
            | Self::InvalidPluginId { .. }
            | Self::DownloadRequest { .. }
            | Self::DownloadStatus { .. }
            | Self::DownloadBody { .. }
            | Self::DownloadTooLarge { .. }
            | Self::UnsupportedBundleDownloadFinalUrl { .. }
            | Self::ExtractedBundleTooLarge { .. }
            | Self::InvalidBundle(_) => None,
        }
    }
}

pub fn validate_remote_plugin_bundle(
    remote_plugin_id: &str,
    remote_marketplace_name: &str,
    plugin_name: &str,
    release_version: Option<&str>,
    bundle_download_url: Option<&str>,
    app_manifest: Option<JsonValue>,
) -> Result<ValidatedRemotePluginBundle, RemotePluginBundleInstallError> {
    let plugin_id = PluginId::new(plugin_name.to_string(), remote_marketplace_name.to_string())
        .map_err(|source| RemotePluginBundleInstallError::InvalidPluginId {
            remote_plugin_id: remote_plugin_id.to_string(),
            source,
        })?;
    let plugin_version = release_version
        .map(str::trim)
        .filter(|version| !version.is_empty())
        .ok_or_else(|| RemotePluginBundleInstallError::MissingReleaseVersion {
            remote_plugin_id: remote_plugin_id.to_string(),
        })?
        .to_string();
    validate_plugin_version_segment(&plugin_version).map_err(|message| {
        RemotePluginBundleInstallError::InvalidReleaseVersion {
            remote_plugin_id: remote_plugin_id.to_string(),
            message,
        }
    })?;
    let bundle_download_url = bundle_download_url
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .ok_or_else(
            || RemotePluginBundleInstallError::MissingBundleDownloadUrl {
                remote_plugin_id: remote_plugin_id.to_string(),
            },
        )?
        .to_string();
    let parsed_bundle_url = Url::parse(&bundle_download_url).map_err(|source| {
        RemotePluginBundleInstallError::InvalidBundleDownloadUrl {
            remote_plugin_id: remote_plugin_id.to_string(),
            url: bundle_download_url.clone(),
            source,
        }
    })?;
    if !is_allowed_bundle_download_url(
        &parsed_bundle_url,
        allow_test_loopback_http_bundle_downloads(),
    ) {
        return Err(
            RemotePluginBundleInstallError::UnsupportedBundleDownloadUrlScheme {
                remote_plugin_id: remote_plugin_id.to_string(),
                scheme: parsed_bundle_url.scheme().to_string(),
            },
        );
    }

    Ok(ValidatedRemotePluginBundle {
        plugin_id,
        plugin_version,
        remote_plugin_id: remote_plugin_id.to_string(),
        app_manifest,
        bundle_download_url,
    })
}

fn allow_test_loopback_http_bundle_downloads() -> bool {
    #[cfg(debug_assertions)]
    {
        if let Ok(value) = std::env::var(TEST_ALLOW_LOOPBACK_HTTP_REMOTE_PLUGIN_BUNDLES_ENV) {
            return value == "1";
        }
    }

    false
}

fn is_allowed_bundle_download_url(url: &Url, allow_loopback_http: bool) -> bool {
    match url.scheme() {
        "https" => true,
        "http" => allow_loopback_http && is_loopback_url(url),
        _ => false,
    }
}

fn is_loopback_url(url: &Url) -> bool {
    match url.host() {
        Some(Host::Ipv4(addr)) => addr.is_loopback(),
        Some(Host::Ipv6(addr)) => addr.is_loopback(),
        Some(Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
        None => false,
    }
}

pub async fn download_and_install_remote_plugin_bundle(
    codex_home: PathBuf,
    bundle: ValidatedRemotePluginBundle,
    http_clients: &RouteAwareClientPool,
) -> Result<PluginInstallResult, RemotePluginBundleInstallError> {
    let mutation = crate::remote::mark_remote_plugin_cache_mutation_in_flight(
        &codex_home,
        bundle.plugin_id.marketplace_name(),
        bundle.plugin_id.plugin_name(),
    )
    .await
    .map_err(|error| {
        RemotePluginBundleInstallError::InvalidBundle(format!(
            "failed to acquire remote plugin cache mutation: {error}"
        ))
    })?;
    download_and_install_remote_plugin_bundle_with_guard(codex_home, bundle, http_clients, mutation)
        .await
}

pub(crate) async fn download_and_install_remote_plugin_bundle_with_guard(
    codex_home: PathBuf,
    bundle: ValidatedRemotePluginBundle,
    http_clients: &RouteAwareClientPool,
    mutation: crate::remote::RemotePluginCacheMutationGuard,
) -> Result<PluginInstallResult, RemotePluginBundleInstallError> {
    let bundle_bytes = download_remote_plugin_bundle_with_limit(
        &bundle.bundle_download_url,
        /*max_bytes*/ REMOTE_PLUGIN_BUNDLE_MAX_DOWNLOAD_BYTES,
        http_clients,
    )
    .await?;
    tokio::task::spawn_blocking(move || {
        let _mutation = mutation;
        install_remote_plugin_bundle(codex_home, bundle, bundle_bytes, &_mutation)
    })
    .await
    .map_err(|err| {
        RemotePluginBundleInstallError::InvalidBundle(format!(
            "failed to join remote plugin bundle install task: {err}"
        ))
    })?
}

pub(crate) async fn download_and_extract_remote_plugin_bundle_to_path<T>(
    bundle: ValidatedRemotePluginBundle,
    destination: AbsolutePathBuf,
    http_clients: &RouteAwareClientPool,
    finalize: impl FnOnce(AbsolutePathBuf) -> T + Send + 'static,
) -> Result<T, RemotePluginBundleInstallError>
where
    T: Send + 'static,
{
    let bundle_bytes = download_remote_plugin_bundle_with_limit(
        &bundle.bundle_download_url,
        /*max_bytes*/ REMOTE_PLUGIN_BUNDLE_MAX_DOWNLOAD_BYTES,
        http_clients,
    )
    .await?;
    // Keep the download cancellable, but once extraction is admitted its owner must also
    // finish local registration even if the caller stops observing the blocking task.
    tokio::task::spawn_blocking(move || {
        let destination = extract_remote_plugin_bundle_to_path(bundle, bundle_bytes, destination)?;
        Ok(finalize(destination))
    })
    .await
    .map_err(|err| {
        RemotePluginBundleInstallError::InvalidBundle(format!(
            "failed to join remote plugin bundle extraction and finalization task: {err}"
        ))
    })?
}

async fn download_remote_plugin_bundle_with_limit(
    bundle_download_url: &str,
    max_bytes: u64,
    http_clients: &RouteAwareClientPool,
) -> Result<Vec<u8>, RemotePluginBundleInstallError> {
    let response = http_clients
        .get(bundle_download_url)
        .timeout(REMOTE_PLUGIN_BUNDLE_DOWNLOAD_TIMEOUT)
        .send()
        .await
        .map_err(|source| RemotePluginBundleInstallError::DownloadRequest {
            url: bundle_download_url.to_string(),
            source,
        })?;

    let final_url = response.url().clone();
    // The route-aware pool follows redirects one hop at a time, re-resolving the route before each
    // request. Reject unsupported final schemes before caching the response body.
    if !is_allowed_bundle_download_url(&final_url, allow_test_loopback_http_bundle_downloads()) {
        return Err(
            RemotePluginBundleInstallError::UnsupportedBundleDownloadFinalUrl {
                url: bundle_download_url.to_string(),
                final_url: final_url.to_string(),
            },
        );
    }

    let url = final_url.to_string();
    let status = response.status();
    if !status.is_success() {
        let mut response = response;
        let mut body = Vec::new();
        let mut body_truncated = false;
        let mut body_read_error = None;
        loop {
            let chunk = match response.chunk().await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break,
                Err(source) => {
                    body_read_error = Some(source);
                    break;
                }
            };
            let remaining = REMOTE_PLUGIN_BUNDLE_ERROR_BODY_MAX_BYTES as usize - body.len();
            if chunk.len() > remaining {
                body.extend_from_slice(&chunk[..remaining]);
                body_truncated = true;
                break;
            }
            body.extend_from_slice(&chunk);
        }

        let mut body = String::from_utf8_lossy(&body).into_owned();
        if body_truncated {
            body.push_str(&format!(
                "\n[response body truncated after {REMOTE_PLUGIN_BUNDLE_ERROR_BODY_MAX_BYTES} bytes]"
            ));
        }
        if let Some(source) = body_read_error {
            body.push_str(&format!("\n[failed to read response body: {source}]"));
        }
        return Err(RemotePluginBundleInstallError::DownloadStatus { url, status, body });
    }

    read_response_body_with_limit(response, &url, max_bytes).await
}

async fn read_response_body_with_limit(
    mut response: HttpResponse,
    url: &str,
    max_bytes: u64,
) -> Result<Vec<u8>, RemotePluginBundleInstallError> {
    if let Some(content_length) = response.content_length() {
        enforce_download_size_limit(url, content_length, max_bytes)?;
    }

    let mut body = Vec::new();
    while let Some(chunk) =
        response
            .chunk()
            .await
            .map_err(|source| RemotePluginBundleInstallError::DownloadBody {
                url: url.to_string(),
                source,
            })?
    {
        let next_len = body.len() as u64 + chunk.len() as u64;
        enforce_download_size_limit(url, next_len, max_bytes)?;
        body.extend_from_slice(&chunk);
    }

    Ok(body)
}

fn enforce_download_size_limit(
    url: &str,
    bytes: u64,
    max_bytes: u64,
) -> Result<(), RemotePluginBundleInstallError> {
    if bytes > max_bytes {
        return Err(RemotePluginBundleInstallError::DownloadTooLarge {
            url: url.to_string(),
            max_bytes,
        });
    }
    Ok(())
}

fn install_remote_plugin_bundle(
    codex_home: PathBuf,
    bundle: ValidatedRemotePluginBundle,
    bundle_bytes: Vec<u8>,
    mutation: &crate::remote::RemotePluginCacheMutationGuard,
) -> Result<PluginInstallResult, RemotePluginBundleInstallError> {
    install_remote_plugin_bundle_with_metadata_writer(
        codex_home,
        bundle,
        bundle_bytes,
        PluginStore::write_remote_plugin_id,
        mutation,
    )
}

fn install_remote_plugin_bundle_with_metadata_writer(
    codex_home: PathBuf,
    bundle: ValidatedRemotePluginBundle,
    bundle_bytes: Vec<u8>,
    write_metadata: impl FnOnce(&PluginStore, &PluginId, &str) -> Result<(), PluginStoreError>,
    mutation: &crate::remote::RemotePluginCacheMutationGuard,
) -> Result<PluginInstallResult, RemotePluginBundleInstallError> {
    let staging_root = codex_home.join(REMOTE_PLUGIN_INSTALL_STAGING_DIR);
    fs::create_dir_all(&staging_root).map_err(|source| {
        RemotePluginBundleInstallError::io(
            "failed to create remote plugin bundle staging directory",
            source,
        )
    })?;
    let extract_dir = tempfile::Builder::new()
        .prefix("remote-plugin-bundle-")
        .tempdir_in(&staging_root)
        .map_err(|source| {
            RemotePluginBundleInstallError::io(
                "failed to create remote plugin bundle extraction directory",
                source,
            )
        })?;

    extract_plugin_bundle_tar_gz(&bundle_bytes, extract_dir.path())?;
    let plugin_root = find_extracted_plugin_root(extract_dir.path())?;
    prepare_extracted_remote_plugin_root(&plugin_root, &bundle)?;
    let plugin_root = AbsolutePathBuf::try_from(plugin_root).map_err(|err| {
        RemotePluginBundleInstallError::InvalidBundle(format!(
            "failed to resolve extracted remote plugin bundle root: {err}"
        ))
    })?;

    let store = PluginStore::try_new(codex_home)?;
    let remote_plugin_id = bundle.remote_plugin_id;
    let pending_install = store
        .begin_install_with_version_and_publication_guard(
            plugin_root,
            bundle.plugin_id,
            bundle.plugin_version,
            || mutation.lock_for_publication(),
        )
        .map_err(RemotePluginBundleInstallError::from)?;
    write_metadata(
        &store,
        &pending_install.result().plugin_id,
        &remote_plugin_id,
    )?;
    Ok(pending_install.commit())
}

fn extract_remote_plugin_bundle_to_path(
    bundle: ValidatedRemotePluginBundle,
    bundle_bytes: Vec<u8>,
    destination: AbsolutePathBuf,
) -> Result<AbsolutePathBuf, RemotePluginBundleInstallError> {
    if destination.as_path().exists() {
        return Err(RemotePluginBundleInstallError::InvalidBundle(format!(
            "plugin checkout destination already exists: {}",
            destination.display()
        )));
    }

    let parent = destination.as_path().parent().ok_or_else(|| {
        RemotePluginBundleInstallError::InvalidBundle(format!(
            "plugin checkout destination has no parent: {}",
            destination.display()
        ))
    })?;
    fs::create_dir_all(parent).map_err(|source| {
        RemotePluginBundleInstallError::io("failed to create plugin checkout directory", source)
    })?;

    let extract_dir = tempfile::Builder::new()
        .prefix("remote-plugin-checkout-")
        .tempdir_in(parent)
        .map_err(|source| {
            RemotePluginBundleInstallError::io(
                "failed to create remote plugin bundle extraction directory",
                source,
            )
        })?;

    extract_plugin_bundle_tar_gz(&bundle_bytes, extract_dir.path())?;
    let plugin_root = find_extracted_plugin_root(extract_dir.path())?;
    let manifest = crate::manifest::load_plugin_manifest(&plugin_root).ok_or_else(|| {
        RemotePluginBundleInstallError::InvalidBundle(
            "remote plugin bundle did not contain a valid plugin.json".to_string(),
        )
    })?;
    if manifest.name != bundle.plugin_id.plugin_name() {
        return Err(RemotePluginBundleInstallError::InvalidBundle(format!(
            "plugin.json name `{}` does not match remote plugin name `{}`",
            manifest.name,
            bundle.plugin_id.plugin_name()
        )));
    }

    let staged_path = extract_dir.path();
    fs::rename(staged_path, destination.as_path()).map_err(|source| {
        RemotePluginBundleInstallError::io(
            "failed to activate checked out plugin directory",
            source,
        )
    })?;

    Ok(destination)
}

fn prepare_extracted_remote_plugin_root(
    plugin_root: &Path,
    bundle: &ValidatedRemotePluginBundle,
) -> Result<(), RemotePluginBundleInstallError> {
    if bundle.plugin_id.marketplace_name() != REMOTE_GLOBAL_MARKETPLACE_NAME {
        return Ok(());
    }

    overwrite_plugin_manifest_version(plugin_root, &bundle.plugin_version)?;
    if let Some(app_manifest) = &bundle.app_manifest {
        overwrite_plugin_app_manifest(plugin_root, app_manifest)?;
    }
    Ok(())
}

fn overwrite_plugin_manifest_version(
    plugin_root: &Path,
    plugin_version: &str,
) -> Result<(), RemotePluginBundleInstallError> {
    let manifest_path = find_plugin_manifest_path(plugin_root).ok_or_else(|| {
        RemotePluginBundleInstallError::InvalidBundle(
            "remote plugin bundle did not contain a valid plugin.json".to_string(),
        )
    })?;
    let contents = fs::read_to_string(&manifest_path).map_err(|source| {
        RemotePluginBundleInstallError::io("failed to read remote plugin manifest", source)
    })?;
    let mut manifest: JsonValue = serde_json::from_str(&contents).map_err(|err| {
        RemotePluginBundleInstallError::InvalidBundle(format!(
            "failed to parse remote plugin manifest: {err}"
        ))
    })?;
    let Some(manifest_object) = manifest.as_object_mut() else {
        return Err(RemotePluginBundleInstallError::InvalidBundle(
            "remote plugin manifest must be a JSON object".to_string(),
        ));
    };
    manifest_object.insert(
        "version".to_string(),
        JsonValue::String(plugin_version.to_string()),
    );
    write_json_file(
        &manifest_path,
        &manifest,
        "failed to write remote plugin manifest",
    )
}

fn overwrite_plugin_app_manifest(
    plugin_root: &Path,
    app_manifest: &JsonValue,
) -> Result<(), RemotePluginBundleInstallError> {
    let app_manifest_path = crate::manifest::load_plugin_manifest(plugin_root)
        .and_then(|manifest| manifest.paths.apps.map(|path| path.to_path_buf()))
        .unwrap_or_else(|| plugin_root.join(".app.json"));
    write_json_file(
        &app_manifest_path,
        app_manifest,
        "failed to write remote plugin app manifest",
    )
}

fn write_json_file(
    path: &Path,
    value: &JsonValue,
    context: &'static str,
) -> Result<(), RemotePluginBundleInstallError> {
    let parent = path.parent().ok_or_else(|| {
        RemotePluginBundleInstallError::InvalidBundle(format!(
            "remote plugin output path has no parent: {}",
            path.display()
        ))
    })?;
    fs::create_dir_all(parent)
        .map_err(|source| RemotePluginBundleInstallError::io(context, source))?;
    let mut contents = serde_json::to_vec_pretty(value).map_err(|err| {
        RemotePluginBundleInstallError::InvalidBundle(format!(
            "failed to serialize remote plugin JSON override: {err}"
        ))
    })?;
    contents.push(b'\n');
    fs::write(path, contents).map_err(|source| RemotePluginBundleInstallError::io(context, source))
}

fn extract_plugin_bundle_tar_gz(
    bytes: &[u8],
    destination: &Path,
) -> Result<(), RemotePluginBundleInstallError> {
    extract_plugin_bundle_tar_gz_with_limits(
        bytes,
        destination,
        REMOTE_PLUGIN_BUNDLE_MAX_EXTRACTED_BYTES,
    )
}

fn extract_plugin_bundle_tar_gz_with_limits(
    bytes: &[u8],
    destination: &Path,
    max_total_bytes: u64,
) -> Result<(), RemotePluginBundleInstallError> {
    unpack_plugin_bundle_tar_gz(bytes, destination, max_total_bytes).map_err(|err| match err {
        PluginBundleUnpackError::ExtractedBundleTooLarge { bytes, max_bytes } => {
            RemotePluginBundleInstallError::ExtractedBundleTooLarge { bytes, max_bytes }
        }
        PluginBundleUnpackError::Io { context, source } => {
            RemotePluginBundleInstallError::io(context, source)
        }
        PluginBundleUnpackError::InvalidBundle(message) => {
            RemotePluginBundleInstallError::InvalidBundle(message)
        }
    })
}

fn find_extracted_plugin_root(
    extraction_root: &Path,
) -> Result<PathBuf, RemotePluginBundleInstallError> {
    if is_standard_plugin_root(extraction_root) {
        return Ok(extraction_root.to_path_buf());
    }

    Err(RemotePluginBundleInstallError::InvalidBundle(
        "remote plugin bundle did not contain a standard plugin root with plugin.json".to_string(),
    ))
}

fn is_standard_plugin_root(path: &Path) -> bool {
    find_plugin_manifest_path(path).is_some()
}
