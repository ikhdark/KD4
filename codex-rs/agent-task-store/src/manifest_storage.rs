use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use sqlx::Row;
use sqlx::Sqlite;
use sqlx::SqlitePool;
use sqlx::Transaction;

use crate::StoreError;
use crate::StoreResult;
use crate::WorkspaceManifestEntry;

pub(crate) const INLINE_V1: &str = "inline_v1";
pub(crate) const CONTENT_ADDRESSED_V1: &str = "content_addressed_v1";
const REFERENCE_STORAGE_TAG: &str = "content_addressed";
const REFERENCE_TAG_VERSION: u32 = 1;
const PAYLOAD_FORMAT_VERSION: u32 = 1;
const MANIFEST_V1_DOMAIN: &[u8] = b"kd4-workspace-manifest-v1\0";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CanonicalManifest {
    pub bytes: Vec<u8>,
    pub manifest_hash: String,
    pub entry_count: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StoredManifest {
    pub stored_json: String,
    pub storage_kind: &'static str,
    pub reference_hash: Option<String>,
    pub work: PersistenceWork,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ManifestReferenceV1 {
    storage: String,
    tag_version: u32,
    payload_format_version: u32,
    manifest_id: String,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct PersistenceWork {
    pub unique_payloads: u64,
    pub payload_reuses: u64,
    pub manifest_bytes: u64,
    pub reference_bytes: u64,
    pub sqlite_statements: u64,
}

pub(crate) fn canonical_manifest(
    entries: &[WorkspaceManifestEntry],
) -> StoreResult<CanonicalManifest> {
    let bytes = canonical_v1_bytes(entries)?;
    Ok(CanonicalManifest {
        manifest_hash: manifest_v1_id(&bytes),
        entry_count: entries.len().try_into().unwrap_or(u64::MAX),
        bytes,
    })
}

pub(crate) fn encode_inline_manifest(
    entries: &[WorkspaceManifestEntry],
) -> StoreResult<StoredManifest> {
    let stored_json = serde_json::to_string(entries)?;
    Ok(StoredManifest {
        storage_kind: INLINE_V1,
        reference_hash: None,
        work: PersistenceWork {
            manifest_bytes: stored_json.len() as u64,
            ..PersistenceWork::default()
        },
        stored_json,
    })
}

pub(crate) async fn encode_manifest_reference(
    transaction: &mut Transaction<'_, Sqlite>,
    workspace_id: &str,
    entries: &[WorkspaceManifestEntry],
) -> StoreResult<StoredManifest> {
    let canonical = canonical_manifest(entries)?;
    encode_canonical_manifest_reference(transaction, workspace_id, &canonical).await
}

pub(crate) async fn encode_manifest(
    transaction: &mut Transaction<'_, Sqlite>,
    workspace_id: &str,
    entries: &[WorkspaceManifestEntry],
    reference_writes_enabled: bool,
) -> StoreResult<StoredManifest> {
    if reference_writes_enabled {
        encode_manifest_reference(transaction, workspace_id, entries).await
    } else {
        encode_inline_manifest(entries)
    }
}

pub(crate) async fn encode_canonical_manifest(
    transaction: &mut Transaction<'_, Sqlite>,
    workspace_id: &str,
    canonical: &CanonicalManifest,
    reference_writes_enabled: bool,
) -> StoreResult<StoredManifest> {
    if reference_writes_enabled {
        encode_canonical_manifest_reference(transaction, workspace_id, canonical).await
    } else {
        validate_canonical_manifest(canonical)?;
        let entries: Vec<WorkspaceManifestEntry> = serde_json::from_slice(&canonical.bytes)
            .map_err(|error| {
                StoreError::CorruptData(format!("invalid prepared manifest: {error}"))
            })?;
        encode_inline_manifest(&entries)
    }
}

pub(crate) async fn encode_canonical_manifest_reference(
    transaction: &mut Transaction<'_, Sqlite>,
    workspace_id: &str,
    canonical: &CanonicalManifest,
) -> StoreResult<StoredManifest> {
    validate_canonical_manifest(canonical)?;
    let existing = sqlx::query(
        "SELECT payload_format_version, canonical_manifest_bytes, entry_count, payload_byte_count
         FROM workspace_manifest_payloads
         WHERE workspace_id = ? AND manifest_id = ?",
    )
    .bind(workspace_id)
    .bind(&canonical.manifest_hash)
    .fetch_optional(&mut **transaction)
    .await?;

    let (unique_payloads, payload_reuses, sqlite_statements) = if let Some(existing) = existing {
        let version = existing.get::<i64, _>("payload_format_version");
        let bytes = existing.get::<Vec<u8>, _>("canonical_manifest_bytes");
        let entry_count = existing.get::<i64, _>("entry_count");
        let payload_byte_count = existing.get::<i64, _>("payload_byte_count");
        if version != i64::from(PAYLOAD_FORMAT_VERSION)
            || bytes != canonical.bytes
            || entry_count != canonical.entry_count as i64
            || payload_byte_count != canonical.bytes.len() as i64
        {
            return Err(StoreError::CorruptData(format!(
                "workspace manifest payload identity collision for {}",
                canonical.manifest_hash
            )));
        }
        (0, 1, 1)
    } else {
        sqlx::query(
            "INSERT INTO workspace_manifest_payloads (
                workspace_id, manifest_id, payload_format_version,
                canonical_manifest_bytes, entry_count, payload_byte_count, created_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(workspace_id)
        .bind(&canonical.manifest_hash)
        .bind(i64::from(PAYLOAD_FORMAT_VERSION))
        .bind(&canonical.bytes)
        .bind(canonical.entry_count as i64)
        .bind(canonical.bytes.len() as i64)
        .bind(serde_json::to_string(&Utc::now())?)
        .execute(&mut **transaction)
        .await?;
        (1, 0, 2)
    };

    let stored_json = serde_json::to_string(&ManifestReferenceV1 {
        storage: REFERENCE_STORAGE_TAG.to_string(),
        tag_version: REFERENCE_TAG_VERSION,
        payload_format_version: PAYLOAD_FORMAT_VERSION,
        manifest_id: canonical.manifest_hash.clone(),
    })?;
    Ok(StoredManifest {
        storage_kind: CONTENT_ADDRESSED_V1,
        reference_hash: Some(canonical.manifest_hash.clone()),
        work: PersistenceWork {
            unique_payloads,
            payload_reuses,
            manifest_bytes: if unique_payloads == 1 {
                canonical.bytes.len() as u64
            } else {
                0
            },
            reference_bytes: stored_json.len() as u64,
            sqlite_statements,
        },
        stored_json,
    })
}

/// The only decoder for `expected_manifest_json`. Reference reads are storage
/// compatibility behavior and are deliberately independent of cache/write gates.
pub(crate) async fn decode_manifest(
    pool: &SqlitePool,
    workspace_id: &str,
    stored: &str,
    storage_kind: &str,
    reference_hash: Option<&str>,
) -> StoreResult<Vec<WorkspaceManifestEntry>> {
    match storage_kind {
        INLINE_V1 => {
            if reference_hash.is_some() {
                return Err(StoreError::CorruptData(
                    "inline workspace manifest unexpectedly has a reference hash".to_string(),
                ));
            }
            decode_inline(stored)
        }
        CONTENT_ADDRESSED_V1 => {
            let reference: ManifestReferenceV1 = serde_json::from_str(stored).map_err(|error| {
                StoreError::CorruptData(format!("invalid workspace manifest reference: {error}"))
            })?;
            if reference.storage != REFERENCE_STORAGE_TAG {
                return Err(StoreError::CorruptData(
                    "workspace manifest reference storage tag is unsupported".to_string(),
                ));
            }
            if reference.tag_version != REFERENCE_TAG_VERSION {
                return Err(StoreError::CorruptData(format!(
                    "unsupported workspace manifest reference version {}",
                    reference.tag_version
                )));
            }
            if reference.payload_format_version != PAYLOAD_FORMAT_VERSION {
                return Err(StoreError::CorruptData(format!(
                    "unsupported workspace manifest payload version {}",
                    reference.payload_format_version
                )));
            }
            let reference_hash = reference_hash.ok_or_else(|| {
                StoreError::CorruptData("workspace manifest reference hash is missing".to_string())
            })?;
            if reference.manifest_id != reference_hash {
                return Err(StoreError::CorruptData(
                    "workspace manifest reference identity mismatch".to_string(),
                ));
            }
            decode_reference(pool, workspace_id, reference_hash).await
        }
        value => Err(StoreError::CorruptData(format!(
            "unsupported workspace manifest storage kind {value}"
        ))),
    }
}

fn decode_inline(stored: &str) -> StoreResult<Vec<WorkspaceManifestEntry>> {
    let value: serde_json::Value = serde_json::from_str(stored).map_err(|error| {
        StoreError::CorruptData(format!(
            "invalid persisted workspace manifest JSON: {error}"
        ))
    })?;
    match value {
        serde_json::Value::Array(_) => {
            let entries: Vec<WorkspaceManifestEntry> =
                serde_json::from_value(value).map_err(|error| {
                    StoreError::CorruptData(format!("invalid inline manifest: {error}"))
                })?;
            canonical_v1_bytes(&entries)?;
            Ok(entries)
        }
        _ => Err(StoreError::CorruptData(
            "inline workspace manifest must be an array".to_string(),
        )),
    }
}

async fn decode_reference(
    pool: &SqlitePool,
    workspace_id: &str,
    reference_hash: &str,
) -> StoreResult<Vec<WorkspaceManifestEntry>> {
    let row = sqlx::query(
        "SELECT payload_format_version, canonical_manifest_bytes, entry_count, payload_byte_count
         FROM workspace_manifest_payloads
         WHERE workspace_id = ? AND manifest_id = ?",
    )
    .bind(workspace_id)
    .bind(reference_hash)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| {
        StoreError::CorruptData(format!(
            "workspace manifest payload {reference_hash} is missing"
        ))
    })?;
    let version = row.get::<i64, _>("payload_format_version");
    if version != i64::from(PAYLOAD_FORMAT_VERSION) {
        return Err(StoreError::CorruptData(format!(
            "unsupported workspace manifest payload version {version}"
        )));
    }
    let bytes = row.get::<Vec<u8>, _>("canonical_manifest_bytes");
    let entry_count = row.get::<i64, _>("entry_count");
    let payload_byte_count = row.get::<i64, _>("payload_byte_count");
    if manifest_v1_id(&bytes) != reference_hash {
        return Err(StoreError::CorruptData(
            "workspace manifest payload identity mismatch".to_string(),
        ));
    }
    let entries: Vec<WorkspaceManifestEntry> = serde_json::from_slice(&bytes).map_err(|error| {
        StoreError::CorruptData(format!("invalid canonical workspace manifest: {error}"))
    })?;
    if entry_count < 0
        || payload_byte_count < 0
        || entry_count as usize != entries.len()
        || payload_byte_count as usize != bytes.len()
        || canonical_v1_bytes(&entries)? != bytes
    {
        return Err(StoreError::CorruptData(
            "workspace manifest payload encoding is noncanonical".to_string(),
        ));
    }
    Ok(entries)
}

pub(crate) fn manifest_v1_id_for_entries(
    entries: &[WorkspaceManifestEntry],
) -> StoreResult<String> {
    Ok(canonical_manifest(entries)?.manifest_hash)
}

pub(crate) fn canonical_v1_bytes(entries: &[WorkspaceManifestEntry]) -> StoreResult<Vec<u8>> {
    let mut previous: Option<&str> = None;
    for entry in entries {
        if entry.path.is_empty()
            || entry.path.contains('\\')
            || entry.path.starts_with('/')
            || entry
                .path
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == "..")
        {
            return Err(StoreError::CorruptData(format!(
                "noncanonical workspace manifest path {:?}",
                entry.path
            )));
        }
        if previous.is_some_and(|path| path >= entry.path.as_str()) {
            return Err(StoreError::CorruptData(
                "workspace manifest paths are duplicated or unsorted".to_string(),
            ));
        }
        match (entry.existed, entry.content_hash.as_deref()) {
            (false, None) => {}
            (true, Some(hash))
                if hash.len() == 64
                    && hash
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')) => {}
            _ => {
                return Err(StoreError::CorruptData(format!(
                    "workspace manifest entry has inconsistent existence/hash state: {}",
                    entry.path
                )));
            }
        }
        previous = Some(&entry.path);
    }
    Ok(serde_json::to_vec(entries)?)
}

fn validate_canonical_manifest(canonical: &CanonicalManifest) -> StoreResult<()> {
    if manifest_v1_id(&canonical.bytes) != canonical.manifest_hash {
        return Err(StoreError::CorruptData(
            "prepared workspace manifest hash does not match canonical bytes".to_string(),
        ));
    }
    let entries: Vec<WorkspaceManifestEntry> = serde_json::from_slice(&canonical.bytes)
        .map_err(|error| StoreError::CorruptData(format!("invalid prepared manifest: {error}")))?;
    if entries.len() as u64 != canonical.entry_count
        || canonical_v1_bytes(&entries)? != canonical.bytes
    {
        return Err(StoreError::CorruptData(
            "prepared workspace manifest bytes are not canonical".to_string(),
        ));
    }
    Ok(())
}

fn manifest_v1_id(canonical: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(MANIFEST_V1_DOMAIN);
    digest.update(canonical);
    format!("{:x}", digest.finalize())
}
