//! Enforceable source-write contracts and bounded, content-bound source manifests.
//! Legacy prose remains readable evidence, never mutation authority.
use crate::{Assignment, AssignmentAdmissionOrigin, AttemptAmendment, CapabilityProfile,
    RepoScope, StoreError, StoreResult, WorkspaceManifestEntry};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(untagged)]
pub enum WriteProhibition {
    Typed(WriteProhibitionKind),
    Legacy(String),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WriteProhibitionKind {
    Paths { scopes: Vec<RepoScope> },
    Create,
    Delete,
    Move,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceWriteOperation { Create, Update, Delete, Move }

pub fn normalize_write_prohibitions(root: &Path, values: &[WriteProhibition])
    -> StoreResult<Vec<WriteProhibition>>
{
    values.iter().map(|value| Ok(match value {
        WriteProhibition::Typed(WriteProhibitionKind::Paths { scopes }) => {
            if scopes.is_empty() {
                return Err(StoreError::InvalidAssignment("paths prohibition requires explicit scopes".into()));
            }
            WriteProhibition::Typed(WriteProhibitionKind::Paths {
                scopes: crate::normalize_repo_scopes(root, scopes)?,
            })
        }
        other => other.clone(),
    })).collect()
}

/// Legacy and report-only contracts cannot acquire source-write authority.
pub fn require_write_contract(assignment: &Assignment, amendment: Option<&AttemptAmendment>)
    -> StoreResult<()>
{
    if !matches!(assignment.capability_profile,
        CapabilityProfile::ScopedSourceWrite | CapabilityProfile::IntegratorSourceWrite)
    {
        if !assignment.write_scope.is_empty() {
            return Err(StoreError::InvalidAssignment("read-only assignments cannot declare write scopes".into()));
        }
        return Ok(());
    }
    let invalid = |reason: &str| StoreError::InvalidAssignment(reason.into());
    if assignment.admission_origin != AssignmentAdmissionOrigin::Typed
        || assignment.write_scope.is_empty()
    {
        return Err(invalid("writable workers require an explicit typed assignment and write_scope; create a replacement assignment"));
    }
    if assignment.prohibited_changes.iter().any(|value| matches!(value, WriteProhibition::Legacy(_))) {
        return Err(invalid("free-text prohibited_changes cannot authorize a writable worker; use typed operation/path prohibitions"));
    }
    let criteria = assignment.effective_criteria(amendment);
    for criterion in criteria {
        criterion.validate_evidence_binding(&assignment.required_evidence)?;
    }
    if criteria.is_empty() || assignment.required_evidence.is_empty()
        || criteria.iter().any(|criterion| criterion.evidence_policy.as_ref().is_none_or(|policy|
            policy.kind != crate::CriterionEvidenceRequirement::SourceInspection
            || policy.required_evidence.as_ref().is_none_or(|required|
                !assignment.required_evidence.contains(required))))
        || assignment.required_evidence.iter().any(|required| !criteria.iter().any(|criterion|
            criterion.evidence_policy.as_ref().and_then(|policy| policy.required_evidence.as_ref()) == Some(required)))
    {
        return Err(invalid("every writable criterion and required_evidence entry needs an explicit owned source-inspection policy; root owns execution and integration checks"));
    }
    Ok(())
}

pub fn authorize_source_write(assignment: &Assignment, amendment: Option<&AttemptAmendment>,
    path: &str, operation: SourceWriteOperation) -> StoreResult<()>
{
    require_write_contract(assignment, amendment)?;

    if !matches!(assignment.capability_profile,
        CapabilityProfile::ScopedSourceWrite | CapabilityProfile::IntegratorSourceWrite)
        || !assignment.write_scope.iter().any(|scope| scope.covers_path(path))
        || path.split('/').any(|part| part.eq_ignore_ascii_case(".git"))
    {
        return Err(StoreError::InvalidScope(format!("source write outside declared authority: {path}")));
    }
    for prohibition in &assignment.prohibited_changes {
        let denied = match prohibition {
            WriteProhibition::Typed(WriteProhibitionKind::Paths { scopes }) =>
                scopes.iter().any(|scope| scope.covers_path(path)),
            WriteProhibition::Typed(WriteProhibitionKind::Create) => operation == SourceWriteOperation::Create,
            WriteProhibition::Typed(WriteProhibitionKind::Delete) => operation == SourceWriteOperation::Delete,
            WriteProhibition::Typed(WriteProhibitionKind::Move) => operation == SourceWriteOperation::Move,
            // Historical prose is evidence, not a mechanically enforced policy.
            WriteProhibition::Legacy(_) => true,
        };
        if denied {
            return Err(StoreError::InvalidScope(format!("declared prohibition rejects {operation:?}: {path}")));
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SourceManifest {
    pub schema_version: u32,
    pub repository_id: String,
    pub workspace_id: String,
    pub scopes: Vec<RepoScope>,
    pub entries: Vec<WorkspaceManifestEntry>,
    pub sha256: String,
    pub captured_at: DateTime<Utc>,
}

impl SourceManifest {
    pub fn verify(&self) -> StoreResult<()> {
        let bytes = serde_json::to_vec(&(self.schema_version, &self.repository_id,
            &self.workspace_id, &self.scopes, &self.entries))?;
        if self.schema_version != 1 || hex::encode(Sha256::digest(bytes)) != self.sha256 {
            return Err(StoreError::InvalidTaskCapsule("source manifest identity mismatch".into()));
        }
        Ok(())
    }
}

/// Complete within the declared scopes or an error; never a partial manifest.
/// File count, file size and cumulative bytes bound both capture and memory.
pub fn capture_source_manifest(root: &Path, scopes: &[RepoScope]) -> StoreResult<SourceManifest> {
    const FILE_LIMIT: usize = 4096;
    const FILE_BYTES: u64 = 8 * 1024 * 1024;
    const TOTAL_BYTES: u64 = 64 * 1024 * 1024;
    let root = std::fs::canonicalize(root)?;
    let scopes = crate::normalize_repo_scopes(&root, scopes)?;
    let mut pending = scopes.iter().map(|scope| (scope.path.clone(), scope.recursive, 0_usize)).collect::<Vec<_>>();
    let mut entries = BTreeMap::new();
    let mut total = 0_u64;
    let mut buffer = [0_u8; 65536];
    while let Some((path, recursive, depth)) = pending.pop() {
        if depth > 64 || pending.len() + entries.len() >= FILE_LIMIT
            || path.split('/').any(|part| part.eq_ignore_ascii_case(".git"))
        {
            return Err(StoreError::InvalidScope("source manifest exceeds bounded scope; declare narrower scopes excluding repository administration".into()));
        }
        if entries.contains_key(&path) { continue; }
        let target = root.join(&path);
        let metadata = match std::fs::symlink_metadata(&target) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                entries.insert(path.clone(), WorkspaceManifestEntry { path, content_hash: None, existed: false });
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        if metadata.file_type().is_symlink() {
            return Err(StoreError::InvalidScope(format!("source manifests require non-symlink paths: {path}")));
        }
        if metadata.is_dir() {
            if !recursive { return Err(StoreError::InvalidScope(format!("directory source scope must be recursive: {path}"))); }
            for child in std::fs::read_dir(&target)? {
                let child = child?;
                if child.file_type()?.is_symlink() {
                    return Err(StoreError::InvalidScope("source manifest encountered a symlink entry".into()));
                }
                let child = crate::normalize_repo_path(&root, &child.path().strip_prefix(&root)
                    .map_err(|error| StoreError::InvalidScope(error.to_string()))?.to_string_lossy())?;
                pending.push((child, true, depth + 1));
                if pending.len() + entries.len() >= FILE_LIMIT {
                    return Err(StoreError::InvalidScope("source manifest exceeds bounded file count".into()));
                }
            }
            continue;
        }
        let (mut file, before) = codex_file_system::open_confined_file_with_metadata(&root, &target)?;
        if !before.is_file() || before.len() > FILE_BYTES {
            return Err(StoreError::InvalidScope(format!("source manifest file unsupported or exceeds 8 MiB: {path}")));
        }
        let mut digest = Sha256::new();
        let mut count = 0_u64;
        loop {
            let n = file.read(&mut buffer)?;
            if n == 0 { break; }
            count += n as u64;
            total += n as u64;
            if count > FILE_BYTES || total > TOTAL_BYTES {
                return Err(StoreError::InvalidScope("source manifest exceeds bounded byte budget".into()));
            }
            digest.update(&buffer[..n]);
        }
        let after = file.metadata()?;
        let named = std::fs::metadata(&target)?;
        if before.len() != count || after.len() != count || named.len() != count
            || before.modified()? != after.modified()? || named.modified()? != after.modified()?
            || crate::normalize_repo_path(&root, &path)? != path
        {
            return Err(StoreError::InvalidScope(format!("source changed during manifest capture: {path}")));
        }
        entries.insert(path.clone(), WorkspaceManifestEntry {
            path, content_hash: Some(hex::encode(digest.finalize())), existed: true,
        });
    }
    let mut manifest = SourceManifest { schema_version: 1,
        repository_id: crate::repository_lineage_id(&root)?,
        workspace_id: crate::repository_workspace_id(&root)?,
        scopes, entries: entries.into_values().collect(), sha256: String::new(), captured_at: Utc::now() };
    manifest.sha256 = hex::encode(Sha256::digest(serde_json::to_vec(&(manifest.schema_version,
        &manifest.repository_id, &manifest.workspace_id, &manifest.scopes, &manifest.entries))?));
    Ok(manifest)
}

pub fn dispatch_manifest(assignment: &Assignment, amendment: Option<&AttemptAmendment>)
    -> StoreResult<SourceManifest>
{
    let manifest = if let Some(amendment) = amendment {
        amendment.source_manifest.clone().ok_or_else(||
            StoreError::InvalidTaskCapsule("correction attempt has no host source baseline; create a replacement assignment".into()))?
    } else {
        let capsule: crate::TaskCapsuleV1 = serde_json::from_str(assignment.task_capsule.as_deref()
            .ok_or_else(|| StoreError::InvalidTaskCapsule("writable dispatch requires its immutable source capsule".into()))?)?;
        let manifest = capsule.source_manifest.ok_or_else(||
            StoreError::InvalidTaskCapsule("historical dispatch has no source manifest; create a replacement typed assignment".into()))?;
        if capsule.assignment_id != assignment.assignment_id || capsule.workspace_manifest_hash != manifest.sha256
            || capsule.source_manifest_error.is_some() {
            return Err(StoreError::InvalidTaskCapsule("dispatch capsule identity mismatch".into()));
        }
        manifest
    };
    manifest.verify()?;
    if manifest.repository_id != assignment.repository_id || manifest.workspace_id != assignment.workspace_id
        || manifest.scopes != assignment.write_scope
    {
        return Err(StoreError::InvalidTaskCapsule("dispatch source manifest is not bound to this assignment".into()));
    }
    Ok(manifest)
}

/// Canonicalize absolute patch endpoints, including absent files, without lexical
/// prefix guesses or treating a URI's display spelling as a repository path.
pub fn normalize_absolute_source_path(root: &Path, path: &Path) -> StoreResult<String> {
    if !path.is_absolute() {
        return Err(StoreError::InvalidScope("patch endpoint must be absolute".into()));
    }
    let root = std::fs::canonicalize(root)?;
    let mut ancestor = path;
    let mut suffix = Vec::new();
    while !ancestor.try_exists()? {
        suffix.push(ancestor.file_name().ok_or_else(||
            StoreError::InvalidScope("patch endpoint has no existing ancestor".into()))?.to_os_string());
        ancestor = ancestor.parent().ok_or_else(|| StoreError::InvalidScope("missing endpoint parent".into()))?;
    }
    let mut canonical = std::fs::canonicalize(ancestor)?;
    for component in suffix.into_iter().rev() { canonical.push(component); }
    let relative = canonical.strip_prefix(&root)
        .map_err(|_| StoreError::InvalidScope("patch endpoint is outside its assigned repository".into()))?;
    crate::normalize_repo_path(&root, &relative.to_string_lossy())
}
