//! Parser-backed, closure-scoped source evidence used by `locate_task`.
//!
//! The persisted layer intentionally contains compact syntax IR and fingerprints,
//! never source bodies. Query-time source slices are built from captured bytes.

use anyhow::Context;
use anyhow::Result;
use ignore::WalkBuilder;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use tempfile::NamedTempFile;
use tree_sitter::Language;
use tree_sitter::Node;
use tree_sitter::Parser;
use unicode_casefold::UnicodeCaseFold;

pub const LOCATE_TASK_SCHEMA_VERSION: u32 = 1;
pub const ROUTING_MANIFEST_SCHEMA_VERSION: u32 = 1;
pub const LOCATE_TASK_MAX_FILES: usize = 2_000;
pub const LOCATE_TASK_MAX_SOURCE_BYTES: usize = 16 * 1024 * 1024;
pub const LOCATE_TASK_MAX_RENDERED_BYTES: usize = 8 * 1024;
const MAX_NEIGHBORHOOD_BYTES: usize = 5 * 1024;
const MAX_NEIGHBORHOOD_LINES: usize = 120;
const CACHE_SCHEMA_VERSION: u32 = 1;
const PARSER_VERSIONS: &str =
    "tree-sitter-rust/0.24.2;tree-sitter-javascript/0.25.0;tree-sitter-typescript/0.23.2";

static CACHE_GUARDS: OnceLock<Mutex<BTreeMap<PathBuf, Weak<Mutex<()>>>>> = OnceLock::new();

#[cfg(test)]
type BeforeFinalVerifyHook = Box<dyn FnOnce() + Send>;

#[cfg(test)]
static BEFORE_FINAL_VERIFY_HOOKS: OnceLock<Mutex<BTreeMap<PathBuf, BeforeFinalVerifyHook>>> =
    OnceLock::new();

#[derive(Debug, Clone)]
pub struct LocateTaskRequest<'a> {
    pub repository_root: &'a Path,
    pub cache_root: &'a Path,
    pub manifest_path: &'a Path,
    pub environment_id: Option<&'a str>,
    pub task: &'a str,
    pub path_anchor: Option<&'a str>,
    pub symbol_anchor: Option<&'a str>,
    pub max_files: usize,
    pub max_source_bytes: usize,
    pub force_fresh: bool,
}

#[derive(Debug, Clone)]
pub struct LocateTaskOutput {
    pub rendered: String,
    pub supporting_reads: Vec<SupportingRead>,
    pub snapshot_id: String,
    pub files_inspected: usize,
    pub files_reparsed: usize,
    pub rendered_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupportingRead {
    pub path: String,
    pub content_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExactSpan {
    pub start_line: usize,
    pub end_line: usize,
    pub start_byte: usize,
    pub end_byte: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Definition {
    pub name: String,
    pub kind: String,
    pub span: ExactSpan,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImportBinding {
    pub local: String,
    pub imported: String,
    pub source: String,
    pub span: ExactSpan,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExportBinding {
    pub local: String,
    pub exported: String,
    pub source: Option<String>,
    pub span: ExactSpan,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Reference {
    pub name: String,
    pub span: ExactSpan,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CallSite {
    pub callee: String,
    pub span: ExactSpan,
    pub direct: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TestDeclaration {
    pub name: String,
    pub span: ExactSpan,
    pub framework: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModuleEdge {
    pub specifier: String,
    pub span: ExactSpan,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ParseDiagnostic {
    pub message: String,
    pub span: ExactSpan,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourceFile {
    pub path: String,
    pub language: String,
    pub definitions: Vec<Definition>,
    pub imports: Vec<ImportBinding>,
    pub exports: Vec<ExportBinding>,
    pub references: Vec<Reference>,
    pub calls: Vec<CallSite>,
    pub tests: Vec<TestDeclaration>,
    pub module_edges: Vec<ModuleEdge>,
    pub diagnostics: Vec<ParseDiagnostic>,
    pub line_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingManifest {
    pub schema_version: u32,
    #[serde(default)]
    pub owners: Vec<OwnerDeclaration>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OwnerDeclaration {
    pub id: String,
    #[serde(default)]
    pub concern_ids: Vec<String>,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub phrases: Vec<String>,
    #[serde(default)]
    pub ambiguous_with: Vec<String>,
    #[serde(default)]
    pub roots: Vec<String>,
    #[serde(default)]
    pub primary_entries: Vec<EntryDeclaration>,
    #[serde(default)]
    pub instructions: Vec<String>,
    #[serde(default)]
    pub consumers: Vec<String>,
    #[serde(default)]
    pub contracts: Vec<String>,
    #[serde(default)]
    pub generated_mirrors: Vec<String>,
    #[serde(default)]
    pub tests: Vec<String>,
    #[serde(default)]
    pub validation: Vec<ValidationDeclaration>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntryDeclaration {
    pub path: String,
    pub symbol: String,
    #[serde(default)]
    pub ambiguous: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationDeclaration {
    pub id: String,
    pub cwd: String,
    pub argv: Vec<String>,
    pub role: String,
}

#[derive(Debug, Clone)]
pub struct ManifestValidation {
    pub manifest: Option<RoutingManifest>,
    pub manifest_hash: String,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedFile {
    fingerprint: String,
    parser_version: String,
    source_file: SourceFile,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct CacheLayer {
    schema_version: u32,
    parser_versions: String,
    manifest_hash: String,
    repository_identity: String,
    files: BTreeMap<String, CachedFile>,
}

enum ManifestReadState {
    Present(Vec<u8>),
    Missing,
    Unverified,
}

#[derive(Debug, Clone, Serialize)]
struct EnvironmentSummary {
    id: Option<String>,
    kind: &'static str,
    canonical_root: String,
}

#[derive(Debug, Clone, Serialize)]
struct RepositorySummary {
    identity: String,
    root: String,
}

#[derive(Debug, Clone, Serialize)]
struct SnapshotSummary {
    snapshot_id: String,
    canonical_root_identity: String,
    repository_identity: String,
    manifest_hash: String,
    parser_provider_versions: String,
    reconciled_scope: Vec<String>,
    contributing_file_count: usize,
    contributing_file_set_digest: String,
    changed_contributors: Vec<String>,
    dirty_overlay_digest: String,
    reconciliation_mode: String,
    completeness: String,
}

#[derive(Debug, Clone, Serialize)]
struct RoutingSummary {
    status: String,
    owner_id: Option<String>,
    reason: String,
    score: f64,
    provenance: String,
}

#[derive(Debug, Clone, Serialize)]
struct PrimarySummary {
    path: Option<String>,
    symbol: Option<String>,
    kind: Option<String>,
    span: Option<ExactSpan>,
    resolution: String,
    confidence: f64,
    provenance: String,
}

#[derive(Debug, Clone, Serialize)]
struct SourceNeighborhood {
    path: String,
    span: ExactSpan,
    text: String,
    provenance: String,
}

#[derive(Debug, Clone, Serialize)]
struct InstructionEvidence {
    path: String,
    fingerprint: String,
    excerpt: Option<String>,
    read_operation: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct RelationshipEvidence {
    path: String,
    span: ExactSpan,
    role: String,
    resolution: String,
    confidence: f64,
    provenance: String,
}

#[derive(Debug, Clone, Serialize)]
struct DeclaredEvidence {
    path: String,
    role: String,
    provenance: String,
}

#[derive(Debug, Clone, Serialize)]
struct TestEvidence {
    path: String,
    name: Option<String>,
    span: Option<ExactSpan>,
    resolution: String,
    provenance: String,
}

#[derive(Debug, Clone, Serialize)]
struct ValidationEvidence {
    id: String,
    cwd: String,
    argv: Vec<String>,
    role: String,
    executed: bool,
}

#[derive(Debug, Clone, Serialize)]
struct AlternativeEvidence {
    owner_id: String,
    reason: String,
    score: f64,
    provenance: String,
}

#[derive(Debug, Clone, Serialize)]
struct TruncationEvidence {
    collection: String,
    returned_count: usize,
    known_count: usize,
    reason: String,
    followup: String,
}

#[derive(Debug, Clone, Serialize)]
struct LocateTaskResult {
    schema_version: u32,
    environment: EnvironmentSummary,
    repository: RepositorySummary,
    snapshot: SnapshotSummary,
    routing: RoutingSummary,
    primary: PrimarySummary,
    source_neighborhoods: Vec<SourceNeighborhood>,
    instructions: Vec<InstructionEvidence>,
    relationships: Vec<RelationshipEvidence>,
    contracts: Vec<DeclaredEvidence>,
    tests: Vec<TestEvidence>,
    validation: Vec<ValidationEvidence>,
    alternatives: Vec<AlternativeEvidence>,
    unresolved: Vec<String>,
    truncation: Vec<TruncationEvidence>,
    followups: Vec<String>,
}

#[derive(Debug, Clone)]
struct RouteDecision<'a> {
    owner: Option<&'a OwnerDeclaration>,
    status: String,
    reason: String,
    score: f64,
    provenance: String,
    alternatives: Vec<AlternativeEvidence>,
    unresolved: Vec<String>,
}

pub fn validate_routing_manifest(
    repository_root: &Path,
    manifest_path: &Path,
) -> ManifestValidation {
    validate_routing_manifest_with_cache(repository_root, manifest_path, None).0
}

fn validate_routing_manifest_with_cache(
    repository_root: &Path,
    manifest_path: &Path,
    mut cache: Option<&mut CacheLayer>,
) -> (ManifestValidation, usize, ManifestReadState) {
    let mut reparsed = 0usize;
    let bytes = match fs::read(manifest_path) {
        Ok(bytes) => bytes,
        Err(error) => {
            let state = if error.kind() == std::io::ErrorKind::NotFound {
                ManifestReadState::Missing
            } else {
                ManifestReadState::Unverified
            };
            return (
                ManifestValidation {
                    manifest: None,
                    manifest_hash: sha256_bytes(&[]),
                    errors: vec![format!("routing_manifest_invalid: {error}")],
                },
                reparsed,
                state,
            );
        }
    };
    let manifest_hash = sha256_bytes(&bytes);
    let manifest_text = String::from_utf8_lossy(&bytes);
    let manifest = match toml::from_str::<RoutingManifest>(&manifest_text) {
        Ok(manifest) => manifest,
        Err(error) => {
            return (
                ManifestValidation {
                    manifest: None,
                    manifest_hash,
                    errors: vec![format!("routing_manifest_invalid: {error}")],
                },
                reparsed,
                ManifestReadState::Present(bytes),
            );
        }
    };
    let mut errors = Vec::new();
    if manifest.schema_version != ROUTING_MANIFEST_SCHEMA_VERSION {
        errors.push(format!(
            "routing_manifest_invalid: schema_version {} is unsupported",
            manifest.schema_version
        ));
    }
    let canonical_root = match repository_root.canonicalize() {
        Ok(root) => root,
        Err(error) => {
            errors.push(format!(
                "routing_manifest_invalid: repository root: {error}"
            ));
            repository_root.to_path_buf()
        }
    };
    let mut owner_ids = BTreeSet::new();
    let mut phrases: BTreeMap<String, Vec<&OwnerDeclaration>> = BTreeMap::new();
    let mut entries: BTreeMap<String, Vec<(&OwnerDeclaration, &EntryDeclaration)>> =
        BTreeMap::new();
    for owner in &manifest.owners {
        if owner.id.trim().is_empty() || !owner_ids.insert(owner.id.clone()) {
            errors.push(format!(
                "routing_manifest_invalid: duplicate or empty owner id `{}`",
                owner.id
            ));
        }
        for phrase in owner.aliases.iter().chain(&owner.phrases) {
            let normalized = normalize(phrase);
            if !normalized.is_empty() {
                phrases.entry(normalized).or_default().push(owner);
            }
        }
        for entry in &owner.primary_entries {
            entries
                .entry(entry.symbol.clone())
                .or_default()
                .push((owner, entry));
            reparsed += validate_entry_symbol(
                &canonical_root,
                owner,
                entry,
                cache.as_deref_mut(),
                &mut errors,
            );
        }
        for path in owner
            .roots
            .iter()
            .chain(&owner.instructions)
            .chain(&owner.consumers)
            .chain(&owner.contracts)
            .chain(&owner.generated_mirrors)
            .chain(&owner.tests)
            .chain(owner.primary_entries.iter().map(|entry| &entry.path))
        {
            validate_declared_path(&canonical_root, path, &owner.id, &mut errors);
        }
        for validation in &owner.validation {
            validate_declared_path(&canonical_root, &validation.cwd, &owner.id, &mut errors);
            if validation.id.trim().is_empty()
                || validation.argv.is_empty()
                || validation.argv[0].trim().is_empty()
            {
                errors.push(format!(
                    "routing_manifest_invalid: owner `{}` has invalid validation `{}`",
                    owner.id, validation.id
                ));
            }
        }
    }
    for (phrase, owners) in phrases {
        if owners.len() > 1 {
            for owner in &owners {
                let peers = owners.iter().filter(|peer| peer.id != owner.id);
                if peers
                    .clone()
                    .any(|peer| !owner.ambiguous_with.contains(&peer.id))
                {
                    errors.push(format!(
                        "routing_manifest_invalid: phrase `{phrase}` collides without explicit ambiguity"
                    ));
                    break;
                }
            }
        }
    }
    for (symbol, owners) in entries {
        if owners.len() > 1 && owners.iter().any(|(_, entry)| !entry.ambiguous) {
            errors.push(format!(
                "routing_manifest_invalid: entry symbol `{symbol}` is not explicitly ambiguous"
            ));
        }
    }
    errors.sort();
    errors.dedup();
    (
        ManifestValidation {
            manifest: Some(manifest),
            manifest_hash,
            errors,
        },
        reparsed,
        ManifestReadState::Present(bytes),
    )
}

fn validate_entry_symbol(
    canonical_root: &Path,
    owner: &OwnerDeclaration,
    entry: &EntryDeclaration,
    cache: Option<&mut CacheLayer>,
    errors: &mut Vec<String>,
) -> usize {
    let Ok(absolute) = confined_join(canonical_root, &entry.path) else {
        return 0;
    };
    let Ok(bytes) = fs::read(&absolute) else {
        return 0;
    };
    let fingerprint = sha256_bytes(&bytes);
    let parser_version = parser_version_for(&entry.path);
    let cached_source = cache.as_deref().and_then(|cache| {
        cache.files.get(&entry.path).and_then(|cached| {
            (cached.fingerprint == fingerprint && cached.parser_version == parser_version)
                .then(|| cached.source_file.clone())
        })
    });
    let (source, reparsed) = if let Some(source) = cached_source {
        (source, 0)
    } else {
        let source = parse_source_file(&entry.path, &bytes);
        if let Some(cache) = cache {
            cache.files.insert(
                entry.path.clone(),
                CachedFile {
                    fingerprint,
                    parser_version: parser_version.to_string(),
                    source_file: source.clone(),
                },
            );
        }
        (source, 1)
    };
    let found = source
        .definitions
        .iter()
        .any(|definition| definition.name == entry.symbol)
        || source
            .imports
            .iter()
            .any(|binding| binding.local == entry.symbol || binding.imported == entry.symbol)
        || source
            .exports
            .iter()
            .any(|binding| binding.local == entry.symbol || binding.exported == entry.symbol);
    if !found {
        errors.push(format!(
            "routing_manifest_invalid: owner `{}` entry symbol `{}` was not found in `{}`",
            owner.id, entry.symbol, entry.path
        ));
    }
    reparsed
}

pub fn locate_task(request: &LocateTaskRequest<'_>) -> Result<LocateTaskOutput> {
    locate_task_inner(request, None)
}

pub fn locate_task_cancellable(
    request: &LocateTaskRequest<'_>,
    cancelled: &AtomicBool,
) -> Result<LocateTaskOutput> {
    locate_task_inner(request, Some(cancelled))
}

fn locate_task_inner(
    request: &LocateTaskRequest<'_>,
    cancelled: Option<&AtomicBool>,
) -> Result<LocateTaskOutput> {
    check_cancelled(cancelled)?;
    let root = request
        .repository_root
        .canonicalize()
        .with_context(|| format!("canonicalize {}", request.repository_root.display()))?;
    let cache_key = cache_path(request.cache_root, &root);
    let guard = {
        let guards = CACHE_GUARDS.get_or_init(|| Mutex::new(BTreeMap::new()));
        let mut guards = guards
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guards.retain(|_, guard| guard.strong_count() > 0);
        if let Some(guard) = guards.get(&cache_key).and_then(Weak::upgrade) {
            guard
        } else {
            let guard = Arc::new(Mutex::new(()));
            guards.insert(cache_key.clone(), Arc::downgrade(&guard));
            guard
        }
    };
    let _guard = lock_cache_guard(&guard, cancelled)?;
    match locate_once(request, &root, &cache_key, cancelled) {
        Ok(output) => Ok(output),
        Err(error) if error.to_string().contains("source_changed_during_query") => {
            locate_once(request, &root, &cache_key, cancelled)
        }
        Err(error) => Err(error),
    }
}

fn check_cancelled(cancelled: Option<&AtomicBool>) -> Result<()> {
    if cancelled.is_some_and(|flag| flag.load(Ordering::Acquire)) {
        anyhow::bail!("locate_task_cancelled");
    }
    Ok(())
}

fn lock_cache_guard<'a>(
    guard: &'a Mutex<()>,
    cancelled: Option<&AtomicBool>,
) -> Result<std::sync::MutexGuard<'a, ()>> {
    loop {
        match guard.try_lock() {
            Ok(locked) => return Ok(locked),
            Err(std::sync::TryLockError::Poisoned(error)) => return Ok(error.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => {
                check_cancelled(cancelled)?;
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    }
}

fn locate_once(
    request: &LocateTaskRequest<'_>,
    root: &Path,
    cache_path: &Path,
    cancelled: Option<&AtomicBool>,
) -> Result<LocateTaskOutput> {
    check_cancelled(cancelled)?;
    let repository_identity = repository_identity(root);
    let mut cache = if request.force_fresh {
        CacheLayer::default()
    } else {
        load_cache(cache_path, &repository_identity)
    };
    let cache_before_reconciliation = cache.clone();
    let (validation, manifest_reparsed, manifest_state) =
        validate_routing_manifest_with_cache(root, request.manifest_path, Some(&mut cache));
    let closure_manifest = validation.manifest.as_ref();
    let authoritative_manifest = validation
        .errors
        .is_empty()
        .then_some(closure_manifest)
        .flatten();
    let mut route = route_task(
        authoritative_manifest,
        request.task,
        request.path_anchor,
        request.symbol_anchor,
    );
    if cache.manifest_hash != validation.manifest_hash {
        cache.manifest_hash = validation.manifest_hash.clone();
    }
    let max_files = request.max_files.clamp(1, LOCATE_TASK_MAX_FILES);
    let max_bytes = request
        .max_source_bytes
        .clamp(1, LOCATE_TASK_MAX_SOURCE_BYTES);
    let (candidate_paths, omitted_units) = closure_paths(
        root,
        closure_manifest,
        route.owner,
        request.path_anchor,
        request.symbol_anchor.is_some() || authoritative_manifest.is_none(),
        max_files,
        max_bytes,
    )?;
    let mut source_files = Vec::new();
    let mut captured = BTreeMap::<String, Vec<u8>>::new();
    let mut fingerprints = BTreeMap::<String, String>::new();
    let mut changed_contributors = Vec::new();
    let mut reparsed = manifest_reparsed;
    for relative in &candidate_paths {
        check_cancelled(cancelled)?;
        let absolute = confined_join(root, relative)?;
        let bytes = match fs::read(&absolute) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("read {}", absolute.display()));
            }
        };
        let fingerprint = sha256_bytes(&bytes);
        fingerprints.insert(relative.clone(), fingerprint.clone());
        captured.insert(relative.clone(), bytes.clone());
        if !supported_path(relative) {
            continue;
        }
        let cached = cache.files.get(relative).filter(|cached| {
            cached.fingerprint == fingerprint
                && cached.parser_version == parser_version_for(relative)
        });
        let parsed = if let Some(cached) = cached {
            cached.source_file.clone()
        } else {
            if cache.files.contains_key(relative) {
                changed_contributors.push(relative.clone());
            }
            reparsed += 1;
            parse_source_file(relative, &bytes)
        };
        cache.files.insert(
            relative.clone(),
            CachedFile {
                fingerprint,
                parser_version: parser_version_for(relative).to_string(),
                source_file: parsed.clone(),
            },
        );
        source_files.push(parsed);
    }
    route = reconcile_symbol_anchor(
        authoritative_manifest,
        route,
        &source_files,
        request.path_anchor,
        request.symbol_anchor,
    );
    let instruction_paths = instruction_paths(root, route.owner);
    for path in &instruction_paths {
        capture_contributor(root, path, &mut captured, &mut fingerprints)?;
    }
    let expected_missing_manifest = match manifest_state {
        ManifestReadState::Present(bytes) => {
            let path = repository_relative_manifest_path(
                request.repository_root,
                root,
                request.manifest_path,
            )?;
            let fingerprint = sha256_bytes(&bytes);
            if fingerprint != validation.manifest_hash {
                anyhow::bail!("source_changed_during_query:{path}")
            }
            captured.insert(path.clone(), bytes);
            fingerprints.insert(path, fingerprint);
            None
        }
        ManifestReadState::Missing => Some(repository_relative_manifest_path(
            request.repository_root,
            root,
            request.manifest_path,
        )?),
        ManifestReadState::Unverified => None,
    };
    cache.schema_version = CACHE_SCHEMA_VERSION;
    cache.parser_versions = PARSER_VERSIONS.to_string();
    cache.repository_identity = repository_identity.clone();

    let contributor_digest = contributor_digest(&fingerprints);
    let dirty_overlay_digest = dirty_overlay_digest(&cache_before_reconciliation, &fingerprints);
    let scope = route
        .owner
        .map(|owner| owner.roots.clone())
        .unwrap_or_else(|| {
            request
                .path_anchor
                .into_iter()
                .map(str::to_string)
                .collect()
        });
    let root_identity = root.to_string_lossy();
    let reconciled_scope = scope.join("\n");
    let snapshot_id = sha256_join(&[
        &root_identity,
        &repository_identity,
        &validation.manifest_hash,
        PARSER_VERSIONS,
        &reconciled_scope,
        &contributor_digest,
        &dirty_overlay_digest,
    ]);
    let files_inspected = fingerprints.len();
    let files_reparsed = reparsed;
    let mut result = build_result(
        request,
        root,
        &validation,
        route,
        BuildResultInput {
            source_files,
            captured: &captured,
            fingerprints: &fingerprints,
            repository_identity,
            snapshot_id: snapshot_id.clone(),
            contributor_digest,
            dirty_overlay_digest,
            changed_contributors,
            omitted_units,
            instruction_paths,
            reparsed,
        },
    );
    check_cancelled(cancelled)?;
    let rendered = render_bounded(&mut result)?;
    #[cfg(test)]
    run_before_final_verify_hook(root);
    verify_captured(root, &fingerprints)?;
    if let Some(path) = expected_missing_manifest {
        verify_absent(root, &path)?;
    }
    check_cancelled(cancelled)?;
    persist_cache(cache_path, &cache);
    let rendered_bytes = rendered.len();
    let supporting_reads = fingerprints
        .into_iter()
        .map(|(path, content_hash)| SupportingRead { path, content_hash })
        .collect();
    Ok(LocateTaskOutput {
        rendered,
        supporting_reads,
        snapshot_id,
        files_inspected,
        files_reparsed,
        rendered_bytes,
    })
}

fn route_task<'a>(
    manifest: Option<&'a RoutingManifest>,
    task: &str,
    path_anchor: Option<&str>,
    symbol_anchor: Option<&str>,
) -> RouteDecision<'a> {
    let Some(manifest) = manifest else {
        return unresolved_route("routing_manifest_invalid");
    };
    let path_owner = path_anchor.and_then(|path| owner_for_path(manifest, path));
    let symbol_owners = symbol_anchor
        .map(|symbol| owners_for_symbol(manifest, symbol))
        .unwrap_or_default();
    if symbol_owners.len() > 1 {
        return RouteDecision {
            owner: None,
            status: "symbol_ambiguity".to_string(),
            reason: "duplicate exact symbol declarations".to_string(),
            score: 1.0,
            provenance: "manifest_declared".to_string(),
            alternatives: symbol_owners
                .into_iter()
                .map(|owner| alternative(owner, "exact symbol candidate", 1.0, "manifest_declared"))
                .collect(),
            unresolved: vec!["symbol_ambiguity".to_string()],
        };
    }
    let symbol_owner = symbol_owners.first().copied();
    if let (Some(path_owner), Some(symbol_owner)) = (path_owner, symbol_owner)
        && path_owner.id != symbol_owner.id
    {
        return RouteDecision {
            owner: None,
            status: "anchor_conflict".to_string(),
            reason: "path and symbol resolve to different owners".to_string(),
            score: 1.0,
            provenance: "anchor_exact".to_string(),
            alternatives: vec![
                alternative(path_owner, "exact path anchor", 1.0, "anchor_exact"),
                alternative(symbol_owner, "exact symbol anchor", 1.0, "anchor_exact"),
            ],
            unresolved: vec!["anchor_conflict".to_string()],
        };
    }
    if let Some(owner) = path_owner.or(symbol_owner) {
        return RouteDecision {
            owner: Some(owner),
            status: "selected".to_string(),
            reason: if path_owner.is_some() {
                "exact path anchor".to_string()
            } else {
                "exact symbol anchor".to_string()
            },
            score: 1.0,
            provenance: "anchor_exact".to_string(),
            alternatives: Vec::new(),
            unresolved: Vec::new(),
        };
    }
    let normalized_task = normalize(task);
    let task_tokens = distinctive_tokens(&normalized_task);
    let mut scores = Vec::<(&OwnerDeclaration, f64, String, String)>::new();
    for owner in &manifest.owners {
        let mut best = (
            0.0,
            "no distinctive match".to_string(),
            "lexical_fallback".to_string(),
        );
        for phrase in owner.aliases.iter().chain(&owner.phrases) {
            let normalized_phrase = normalize(phrase);
            if normalized_phrase == normalized_task
                || contains_phrase(&normalized_task, &normalized_phrase)
            {
                best = (
                    1.0,
                    format!("exact phrase `{phrase}`"),
                    "manifest_declared".to_string(),
                );
                break;
            }
            let tokens = distinctive_tokens(&normalized_phrase);
            if tokens.is_empty() {
                continue;
            }
            let matched = tokens.intersection(&task_tokens).count();
            let coverage = matched as f64 / tokens.len() as f64;
            if coverage > best.0 {
                best = (
                    coverage,
                    format!(
                        "{matched}/{} distinctive tokens from `{phrase}`",
                        tokens.len()
                    ),
                    if coverage == 1.0 {
                        "manifest_declared".to_string()
                    } else {
                        "lexical_fallback".to_string()
                    },
                );
            }
        }
        scores.push((owner, best.0, best.1, best.2));
    }
    scores.sort_by(|left, right| {
        right
            .1
            .total_cmp(&left.1)
            .then_with(|| left.0.id.cmp(&right.0.id))
    });
    let alternatives = scores
        .iter()
        .map(|(owner, score, reason, provenance)| alternative(owner, reason, *score, provenance))
        .collect::<Vec<_>>();
    let Some((leader, leader_score, reason, provenance)) = scores.first() else {
        return unresolved_route("owner_ambiguity");
    };
    let next = scores.get(1).map(|item| item.1).unwrap_or(0.0);
    if *leader_score >= 0.75 && *leader_score - next >= 0.20 {
        RouteDecision {
            owner: Some(leader),
            status: "selected".to_string(),
            reason: reason.clone(),
            score: *leader_score,
            provenance: provenance.clone(),
            alternatives: alternatives.into_iter().skip(1).collect(),
            unresolved: Vec::new(),
        }
    } else {
        RouteDecision {
            owner: None,
            status: "owner_ambiguity".to_string(),
            reason: "routing coverage or margin is insufficient".to_string(),
            score: *leader_score,
            provenance: "unresolved".to_string(),
            alternatives,
            unresolved: vec!["owner_ambiguity".to_string()],
        }
    }
}

struct BuildResultInput<'a> {
    source_files: Vec<SourceFile>,
    captured: &'a BTreeMap<String, Vec<u8>>,
    fingerprints: &'a BTreeMap<String, String>,
    repository_identity: String,
    snapshot_id: String,
    contributor_digest: String,
    dirty_overlay_digest: String,
    changed_contributors: Vec<String>,
    omitted_units: Vec<String>,
    instruction_paths: BTreeSet<String>,
    reparsed: usize,
}

fn build_result(
    request: &LocateTaskRequest<'_>,
    root: &Path,
    validation: &ManifestValidation,
    mut route: RouteDecision<'_>,
    input: BuildResultInput<'_>,
) -> LocateTaskResult {
    let BuildResultInput {
        source_files,
        captured,
        fingerprints,
        repository_identity,
        snapshot_id,
        contributor_digest,
        dirty_overlay_digest,
        mut changed_contributors,
        omitted_units,
        instruction_paths,
        reparsed,
    } = input;
    let mut truncation = Vec::new();
    cap_collection(
        &mut route.alternatives,
        3,
        "alternatives",
        "locate_task with an exact path_anchor or symbol_anchor",
        &mut truncation,
    );
    let changed_contributor_count = changed_contributors.len();
    changed_contributors.sort();
    changed_contributors.truncate(8);
    if changed_contributor_count > changed_contributors.len() {
        truncation.push(TruncationEvidence {
            collection: "changed_contributors".to_string(),
            returned_count: changed_contributors.len(),
            known_count: changed_contributor_count,
            reason: "collection_cap".to_string(),
            followup: "rerun locate_task with force_fresh after source changes settle".to_string(),
        });
    }
    let owner = route.owner;
    let (primary_path, primary_definition, primary_resolution) = select_primary(
        &source_files,
        owner,
        request.path_anchor,
        request.symbol_anchor,
        request.task,
    );
    let primary = PrimarySummary {
        path: primary_path.clone(),
        symbol: primary_definition
            .as_ref()
            .map(|definition| definition.name.clone()),
        kind: primary_definition
            .as_ref()
            .map(|definition| definition.kind.clone()),
        span: primary_definition
            .as_ref()
            .map(|definition| definition.span.clone()),
        resolution: primary_resolution.clone(),
        confidence: match primary_resolution.as_str() {
            "anchor_exact" | "syntax_identity_unique" => 1.0,
            "syntax_match_unique" => 0.8,
            "lexical_fallback" => 0.5,
            _ => 0.0,
        },
        provenance: primary_resolution,
    };
    let source_neighborhoods = primary_path
        .as_deref()
        .zip(primary_definition.as_ref())
        .and_then(|(path, definition)| captured.get(path).map(|bytes| (path, definition, bytes)))
        .map(|(path, definition, bytes)| neighborhood(path, definition, bytes, request.task))
        .into_iter()
        .collect();
    let instructions = instruction_chain(&instruction_paths, fingerprints, captured);
    let mut relationships = relationships(
        &source_files,
        primary_definition.as_ref(),
        primary_path.as_deref(),
    );
    cap_collection(
        &mut relationships,
        8,
        "relationships",
        "locate_task with an exact symbol_anchor for the omitted caller",
        &mut truncation,
    );
    let mut contracts = owner
        .into_iter()
        .flat_map(|owner| owner.contracts.iter().chain(&owner.generated_mirrors))
        .map(|path| DeclaredEvidence {
            path: path.clone(),
            role: if owner.is_some_and(|owner| owner.generated_mirrors.contains(path)) {
                "generated_mirror".to_string()
            } else {
                "contract".to_string()
            },
            provenance: "manifest_declared".to_string(),
        })
        .collect();
    cap_collection(
        &mut contracts,
        6,
        "contracts",
        "locate_task with a path_anchor for the omitted contract or mirror",
        &mut truncation,
    );
    let mut tests = collect_tests(owner, &source_files);
    cap_collection(
        &mut tests,
        6,
        "tests",
        "search_source for the owner path with a focused test-name query",
        &mut truncation,
    );
    let validation_evidence = owner
        .into_iter()
        .flat_map(|owner| &owner.validation)
        .map(|entry| ValidationEvidence {
            id: entry.id.clone(),
            cwd: entry.cwd.clone(),
            argv: entry.argv.clone(),
            role: entry.role.clone(),
            executed: false,
        })
        .collect();
    let mut unresolved = route.unresolved.clone();
    unresolved.extend(validation.errors.clone());
    for file in &source_files {
        if !file.diagnostics.is_empty() {
            unresolved.push(format!("parse_diagnostic:{}", file.path));
        }
    }
    unresolved.sort();
    unresolved.dedup();
    if !omitted_units.is_empty() {
        truncation.push(TruncationEvidence {
            collection: "closure".to_string(),
            returned_count: source_files.len(),
            known_count: source_files.len() + omitted_units.len(),
            reason: "closure_budget_exceeded".to_string(),
            followup: request
                .path_anchor
                .map(|path| {
                    format!("locate_task with path_anchor `{path}` and a narrower owning root")
                })
                .unwrap_or_else(|| {
                    "locate_task with an exact path_anchor inside the omitted root".to_string()
                }),
        });
    }
    let has_parse_diagnostics = source_files
        .iter()
        .any(|source_file| !source_file.diagnostics.is_empty());
    let completeness =
        if truncation.is_empty() && validation.errors.is_empty() && !has_parse_diagnostics {
            "complete"
        } else {
            "partial"
        };
    let mut followups = Vec::new();
    if primary.path.is_some() && primary.span.is_some() {
        followups.push("read_file_span only if an exact missing detail is required".to_string());
    } else if route.status.contains("ambiguity") || route.status == "anchor_conflict" {
        followups.push("use narrowed search_source or anchored locate_task".to_string());
    } else {
        followups.push("use anchored locate_task with an exact path or symbol".to_string());
    }
    if !omitted_units.is_empty() {
        followups.push(format!(
            "omitted closure units: {}",
            omitted_units.join(", ")
        ));
    }
    LocateTaskResult {
        schema_version: LOCATE_TASK_SCHEMA_VERSION,
        environment: EnvironmentSummary {
            id: request.environment_id.map(str::to_string),
            kind: "local",
            canonical_root: normalize_path(root),
        },
        repository: RepositorySummary {
            identity: repository_identity.clone(),
            root: normalize_path(root),
        },
        snapshot: SnapshotSummary {
            snapshot_id,
            canonical_root_identity: sha256_bytes(normalize_path(root).as_bytes()),
            repository_identity,
            manifest_hash: validation.manifest_hash.clone(),
            parser_provider_versions: PARSER_VERSIONS.to_string(),
            reconciled_scope: owner.map(|owner| owner.roots.clone()).unwrap_or_default(),
            contributing_file_count: fingerprints.len(),
            contributing_file_set_digest: contributor_digest,
            changed_contributors,
            dirty_overlay_digest,
            reconciliation_mode: if reparsed == 0 {
                "closure_warm".to_string()
            } else {
                "closure_incremental".to_string()
            },
            completeness: completeness.to_string(),
        },
        routing: RoutingSummary {
            status: route.status,
            owner_id: owner.map(|owner| owner.id.clone()),
            reason: route.reason,
            score: route.score,
            provenance: route.provenance,
        },
        primary,
        source_neighborhoods,
        instructions,
        relationships,
        contracts,
        tests,
        validation: validation_evidence,
        alternatives: route.alternatives,
        unresolved,
        truncation,
        followups,
    }
}

fn reconcile_symbol_anchor<'a>(
    manifest: Option<&'a RoutingManifest>,
    mut route: RouteDecision<'a>,
    files: &[SourceFile],
    path_anchor: Option<&str>,
    symbol_anchor: Option<&str>,
) -> RouteDecision<'a> {
    let (Some(manifest), Some(symbol)) = (manifest, symbol_anchor) else {
        return route;
    };
    let normalized_path = path_anchor.map(normalize_relative);
    let mut candidates = files
        .iter()
        .filter(|file| {
            normalized_path
                .as_ref()
                .is_none_or(|path| eq_path(&file.path, path))
        })
        .flat_map(|file| {
            file.definitions
                .iter()
                .filter(move |definition| definition.name == symbol)
                .map(move |definition| (file, definition))
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        left.0
            .path
            .cmp(&right.0.path)
            .then_with(|| left.1.span.start_byte.cmp(&right.1.span.start_byte))
    });
    if candidates.len() > 1 {
        route.owner = None;
        route.status = "symbol_ambiguity".to_string();
        route.reason = "duplicate exact syntax definitions".to_string();
        route.score = 1.0;
        route.provenance = "syntax_match_unique".to_string();
        route.alternatives.clear();
        route.unresolved = candidates
            .iter()
            .take(3)
            .map(|(file, definition)| {
                format!(
                    "exact candidate {}:{}-{}",
                    file.path, definition.span.start_line, definition.span.end_line
                )
            })
            .collect();
        return route;
    }
    let Some((file, _definition)) = candidates.first() else {
        route
            .unresolved
            .push(format!("exact symbol anchor `{symbol}` was not found"));
        return route;
    };
    let symbol_owner = owner_for_path(manifest, &file.path);
    let path_owner = path_anchor.and_then(|path| owner_for_path(manifest, path));
    if let (Some(path_owner), Some(symbol_owner)) = (path_owner, symbol_owner)
        && path_owner.id != symbol_owner.id
    {
        route.owner = None;
        route.status = "anchor_conflict".to_string();
        route.reason = "path and exact syntax symbol resolve to different owners".to_string();
        route.score = 1.0;
        route.provenance = "anchor_exact".to_string();
        route.alternatives = vec![
            alternative(path_owner, "exact path anchor", 1.0, "anchor_exact"),
            alternative(symbol_owner, "exact symbol anchor", 1.0, "anchor_exact"),
        ];
        return route;
    }
    if let Some(owner) = symbol_owner {
        route.owner = Some(owner);
        route.status = "selected".to_string();
        route.reason = "unique exact syntax symbol anchor".to_string();
        route.score = 1.0;
        route.provenance = "anchor_exact".to_string();
        route.alternatives.clear();
    }
    route
}

fn select_primary(
    files: &[SourceFile],
    owner: Option<&OwnerDeclaration>,
    path_anchor: Option<&str>,
    symbol_anchor: Option<&str>,
    task: &str,
) -> (Option<String>, Option<Definition>, String) {
    if let Some(path) = path_anchor {
        let normalized = normalize_relative(path);
        if let Some(file) = files.iter().find(|file| eq_path(&file.path, &normalized)) {
            if let Some(symbol) = symbol_anchor {
                let matches = file
                    .definitions
                    .iter()
                    .filter(|definition| definition.name == symbol)
                    .cloned()
                    .collect::<Vec<_>>();
                if matches.len() == 1 {
                    return (
                        Some(file.path.clone()),
                        matches.into_iter().next(),
                        "anchor_exact".to_string(),
                    );
                }
            }
            if file.definitions.len() == 1 {
                return (
                    Some(file.path.clone()),
                    file.definitions.first().cloned(),
                    "syntax_identity_unique".to_string(),
                );
            }
        }
    }
    if let Some(symbol) = symbol_anchor {
        let matches = files
            .iter()
            .flat_map(|file| {
                file.definitions
                    .iter()
                    .filter(move |definition| definition.name == symbol)
                    .map(move |definition| (file.path.clone(), definition.clone()))
            })
            .collect::<Vec<_>>();
        if let [(path, definition)] = matches.as_slice() {
            return (
                Some(path.clone()),
                Some(definition.clone()),
                "anchor_exact".to_string(),
            );
        }
        return (None, None, "unresolved".to_string());
    }
    if let Some(owner) = owner {
        for entry in &owner.primary_entries {
            if let Some(file) = files.iter().find(|file| eq_path(&file.path, &entry.path))
                && let Some(definition) = file
                    .definitions
                    .iter()
                    .find(|definition| definition.name == entry.symbol)
            {
                return (
                    Some(file.path.clone()),
                    Some(definition.clone()),
                    "manifest_declared".to_string(),
                );
            }
        }
    }
    let terms = distinctive_tokens(&normalize(task));
    let mut candidates = files
        .iter()
        .flat_map(|file| {
            let terms = &terms;
            file.definitions.iter().filter_map(move |definition| {
                let name_tokens = distinctive_tokens(&normalize(&definition.name));
                let score = name_tokens.intersection(terms).count();
                (score > 0).then(|| (score, file.path.clone(), definition.clone()))
            })
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    if candidates
        .first()
        .is_some_and(|first| candidates.get(1).is_none_or(|next| first.0 > next.0))
    {
        let (_, path, definition) = candidates.remove(0);
        return (
            Some(path),
            Some(definition),
            "syntax_match_unique".to_string(),
        );
    }
    (None, None, "unresolved".to_string())
}

fn relationships(
    files: &[SourceFile],
    primary: Option<&Definition>,
    primary_path: Option<&str>,
) -> Vec<RelationshipEvidence> {
    let Some(primary) = primary else {
        return Vec::new();
    };
    let definition_count = files
        .iter()
        .flat_map(|file| &file.definitions)
        .filter(|definition| definition.name == primary.name)
        .count();
    let mut relationships = files
        .iter()
        .flat_map(|file| {
            file.calls
                .iter()
                .filter(|call| call.callee == primary.name)
                .map(move |call| RelationshipEvidence {
                    path: file.path.clone(),
                    span: call.span.clone(),
                    role: if primary_path.is_some_and(|path| eq_path(path, &file.path)) {
                        "intra_file_call".to_string()
                    } else {
                        "caller".to_string()
                    },
                    resolution: if call.direct && definition_count == 1 {
                        "syntax_identity_unique".to_string()
                    } else {
                        "unresolved".to_string()
                    },
                    confidence: if call.direct && definition_count == 1 {
                        1.0
                    } else {
                        0.0
                    },
                    provenance: "tree_sitter_call_expression".to_string(),
                })
        })
        .collect::<Vec<_>>();
    relationships.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.span.start_line.cmp(&right.span.start_line))
    });
    relationships
}

fn collect_tests(owner: Option<&OwnerDeclaration>, files: &[SourceFile]) -> Vec<TestEvidence> {
    let mut tests = owner
        .into_iter()
        .flat_map(|owner| &owner.tests)
        .map(|path| TestEvidence {
            path: path.clone(),
            name: None,
            span: None,
            resolution: "manifest_declared".to_string(),
            provenance: "manifest_declared".to_string(),
        })
        .collect::<Vec<_>>();
    for file in files {
        for test in &file.tests {
            tests.push(TestEvidence {
                path: file.path.clone(),
                name: Some(test.name.clone()),
                span: Some(test.span.clone()),
                resolution: "syntax_identity_unique".to_string(),
                provenance: format!("tree_sitter_{}", test.framework),
            });
        }
    }
    tests.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.name.cmp(&right.name))
    });
    tests.dedup_by(|left, right| left.path == right.path && left.name == right.name);
    tests
}

fn cap_collection<T>(
    values: &mut Vec<T>,
    cap: usize,
    collection: &str,
    followup: &str,
    truncation: &mut Vec<TruncationEvidence>,
) {
    let known_count = values.len();
    if known_count <= cap {
        return;
    }
    values.truncate(cap);
    truncation.push(TruncationEvidence {
        collection: collection.to_string(),
        returned_count: values.len(),
        known_count,
        reason: "collection_cap".to_string(),
        followup: followup.to_string(),
    });
}

fn instruction_paths(root: &Path, owner: Option<&OwnerDeclaration>) -> BTreeSet<String> {
    let mut paths = BTreeSet::from(["AGENTS.md".to_string()]);
    if let Some(owner) = owner {
        paths.extend(owner.instructions.iter().cloned());
        for owner_root in &owner.roots {
            let mut current = PathBuf::from(owner_root);
            if current.extension().is_some() {
                current.pop();
            }
            loop {
                let candidate = normalize_relative(&current.join("AGENTS.md").to_string_lossy());
                if root.join(&candidate).is_file() {
                    paths.insert(candidate);
                }
                if !current.pop() {
                    break;
                }
            }
        }
    }
    paths
}

fn instruction_chain(
    paths: &BTreeSet<String>,
    fingerprints: &BTreeMap<String, String>,
    captured: &BTreeMap<String, Vec<u8>>,
) -> Vec<InstructionEvidence> {
    paths
        .iter()
        .filter_map(|path| {
            let bytes = captured.get(path)?;
            let fingerprint = fingerprints.get(path)?.clone();
            let text = String::from_utf8_lossy(bytes);
            let excerpt = text
                .lines()
                .filter(|line| {
                    line.contains("source")
                        || line.contains("validation")
                        || line.contains("generated")
                })
                .take(6)
                .collect::<Vec<_>>()
                .join("\n");
            Some(InstructionEvidence {
                path: path.clone(),
                fingerprint,
                excerpt: (!excerpt.is_empty()).then_some(excerpt),
                read_operation: Some(format!("read_file_span path={path}")),
            })
        })
        .collect()
}

fn neighborhood(
    path: &str,
    definition: &Definition,
    bytes: &[u8],
    task: &str,
) -> SourceNeighborhood {
    let text = String::from_utf8_lossy(bytes);
    let lines = text.lines().collect::<Vec<_>>();
    let mut wanted = BTreeSet::new();
    let start = definition.span.start_line.saturating_sub(3).max(1);
    let opening_end = (definition.span.start_line + 12).min(lines.len());
    wanted.extend(start..=opening_end);
    let terms = distinctive_tokens(&normalize(task));
    for (index, line) in lines.iter().enumerate() {
        let normalized = normalize(line);
        if terms.iter().any(|term| normalized.contains(term))
            || [
                "return", "match", "dispatch", "insert", "remove", "write", "send",
            ]
            .iter()
            .any(|term| normalized.contains(term))
        {
            let line_number = index + 1;
            if line_number >= definition.span.start_line && line_number <= definition.span.end_line
            {
                wanted.extend(
                    line_number.saturating_sub(1).max(1)..=(line_number + 1).min(lines.len()),
                );
            }
        }
    }
    wanted.extend(
        definition.span.end_line.saturating_sub(2).max(1)
            ..=definition.span.end_line.min(lines.len()),
    );
    let mut rendered = String::new();
    let mut selected = Vec::new();
    for line_number in wanted.into_iter().take(MAX_NEIGHBORHOOD_LINES) {
        let line = lines.get(line_number - 1).copied().unwrap_or_default();
        let candidate = format!("{line_number}: {line}\n");
        if rendered.len() + candidate.len() > MAX_NEIGHBORHOOD_BYTES {
            break;
        }
        rendered.push_str(&candidate);
        selected.push(line_number);
    }
    let span = ExactSpan {
        start_line: selected
            .first()
            .copied()
            .unwrap_or(definition.span.start_line),
        end_line: selected.last().copied().unwrap_or(definition.span.end_line),
        start_byte: definition.span.start_byte,
        end_byte: definition.span.end_byte,
    };
    SourceNeighborhood {
        path: path.to_string(),
        span,
        text: rendered,
        provenance: "captured_source_bytes".to_string(),
    }
}

fn parse_source_file(path: &str, bytes: &[u8]) -> SourceFile {
    let language_kind = language_kind(path);
    let language = parser_language(path);
    let mut source = SourceFile {
        path: path.to_string(),
        language: language_kind.to_string(),
        definitions: Vec::new(),
        imports: Vec::new(),
        exports: Vec::new(),
        references: Vec::new(),
        calls: Vec::new(),
        tests: Vec::new(),
        module_edges: Vec::new(),
        diagnostics: Vec::new(),
        line_count: bytes.iter().filter(|byte| **byte == b'\n').count() + 1,
    };
    let Some(language) = language else {
        source.diagnostics.push(ParseDiagnostic {
            message: "unsupported_language".to_string(),
            span: zero_span(),
        });
        return source;
    };
    let mut parser = Parser::new();
    if let Err(error) = parser.set_language(&language) {
        source.diagnostics.push(ParseDiagnostic {
            message: format!("parser_language_error:{error}"),
            span: zero_span(),
        });
        return source;
    }
    let Some(tree) = parser.parse(bytes, None) else {
        source.diagnostics.push(ParseDiagnostic {
            message: "parser_returned_no_tree".to_string(),
            span: zero_span(),
        });
        return source;
    };
    visit_node(tree.root_node(), bytes, language_kind, None, &mut source);
    sort_source_file(&mut source);
    source
}

fn visit_node(
    node: Node<'_>,
    bytes: &[u8],
    language: &str,
    container: Option<&str>,
    source: &mut SourceFile,
) {
    if node.is_error() || node.is_missing() {
        source.diagnostics.push(ParseDiagnostic {
            message: if node.is_missing() {
                "missing_syntax"
            } else {
                "parse_error"
            }
            .to_string(),
            span: exact_span(node),
        });
    }
    let kind = node.kind();
    let mut next_container = container.map(str::to_string);
    if language == "rust" {
        match kind {
            "function_item" | "struct_item" | "enum_item" | "union_item" | "trait_item"
            | "type_item" | "const_item" | "static_item" | "mod_item" => {
                if let Some(name) = field_text(node, "name", bytes) {
                    source.definitions.push(Definition {
                        name: name.clone(),
                        kind: kind.trim_end_matches("_item").to_string(),
                        span: exact_span(node),
                        container: container.map(str::to_string),
                    });
                    if matches!(
                        kind,
                        "struct_item" | "enum_item" | "union_item" | "trait_item" | "mod_item"
                    ) {
                        next_container = Some(name.clone());
                    }
                    if kind == "function_item" && rust_test_attribute(node, bytes) {
                        source.tests.push(TestDeclaration {
                            name,
                            span: exact_span(node),
                            framework: "rust_test_attribute".to_string(),
                        });
                    }
                }
            }
            "impl_item" => {
                let name = field_text(node, "type", bytes).unwrap_or_else(|| "impl".to_string());
                source.definitions.push(Definition {
                    name: format!("impl {name}"),
                    kind: "impl".to_string(),
                    span: exact_span(node),
                    container: None,
                });
                next_container = Some(name);
            }
            "use_declaration" => record_rust_use(node, bytes, source),
            "call_expression" => record_call(node, bytes, source),
            "identifier" | "type_identifier" => record_reference(node, bytes, source),
            _ => {}
        }
    } else {
        match kind {
            "function_declaration" | "class_declaration" | "method_definition" => {
                if let Some(name) = field_text(node, "name", bytes) {
                    source.definitions.push(Definition {
                        name: name.clone(),
                        kind: kind.to_string(),
                        span: exact_span(node),
                        container: container.map(str::to_string),
                    });
                    if kind == "class_declaration" {
                        next_container = Some(name);
                    }
                }
            }
            "variable_declarator" => record_js_binding(node, bytes, source),
            "import_statement" => record_js_import(node, bytes, source),
            "export_statement" => record_js_export(node, bytes, source),
            "call_expression" => {
                record_call(node, bytes, source);
                record_js_test(node, bytes, source);
            }
            "identifier" | "property_identifier" => record_reference(node, bytes, source),
            _ => {}
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        visit_node(child, bytes, language, next_container.as_deref(), source);
    }
}

fn record_rust_use(node: Node<'_>, bytes: &[u8], source: &mut SourceFile) {
    let raw = node_text(node, bytes);
    let body = raw
        .trim()
        .trim_start_matches("use")
        .trim()
        .trim_end_matches(';')
        .trim();
    let local = body.rsplit("::").next().unwrap_or(body).trim().to_string();
    source.imports.push(ImportBinding {
        local: local.clone(),
        imported: local,
        source: body.to_string(),
        span: exact_span(node),
    });
    source.module_edges.push(ModuleEdge {
        specifier: body.to_string(),
        span: exact_span(node),
        kind: "rust_use".to_string(),
    });
}

fn record_js_binding(node: Node<'_>, bytes: &[u8], source: &mut SourceFile) {
    let Some(name) = field_text(node, "name", bytes) else {
        return;
    };
    let Some(value) = node.child_by_field_name("value") else {
        return;
    };
    if matches!(value.kind(), "arrow_function" | "function_expression") {
        source.definitions.push(Definition {
            name,
            kind: value.kind().to_string(),
            span: exact_span(node),
            container: None,
        });
    } else if value.kind() == "call_expression"
        && field_text(value, "function", bytes).as_deref() == Some("require")
        && let Some(specifier) = first_quoted(&node_text(value, bytes))
    {
        source.imports.push(ImportBinding {
            local: name,
            imported: "default".to_string(),
            source: specifier.clone(),
            span: exact_span(node),
        });
        source.module_edges.push(ModuleEdge {
            specifier,
            span: exact_span(node),
            kind: "commonjs_require".to_string(),
        });
    }
}

fn record_js_import(node: Node<'_>, bytes: &[u8], source: &mut SourceFile) {
    let raw = node_text(node, bytes);
    let Some(specifier) = first_quoted(&raw) else {
        return;
    };
    let mut identifiers = named_leaf_texts(node, bytes, "identifier");
    identifiers.retain(|identifier| !matches!(identifier.as_str(), "import" | "from" | "type"));
    for local in identifiers {
        source.imports.push(ImportBinding {
            local: local.clone(),
            imported: local,
            source: specifier.clone(),
            span: exact_span(node),
        });
    }
    source.module_edges.push(ModuleEdge {
        specifier,
        span: exact_span(node),
        kind: "es_import".to_string(),
    });
}

fn record_js_export(node: Node<'_>, bytes: &[u8], source: &mut SourceFile) {
    let raw = node_text(node, bytes);
    let specifier = first_quoted(&raw);
    for local in named_leaf_texts(node, bytes, "identifier") {
        source.exports.push(ExportBinding {
            local: local.clone(),
            exported: local,
            source: specifier.clone(),
            span: exact_span(node),
        });
    }
    if let Some(specifier) = specifier {
        source.module_edges.push(ModuleEdge {
            specifier,
            span: exact_span(node),
            kind: "es_reexport".to_string(),
        });
    }
}

fn record_call(node: Node<'_>, bytes: &[u8], source: &mut SourceFile) {
    let Some(function) = node.child_by_field_name("function") else {
        return;
    };
    let direct = matches!(function.kind(), "identifier" | "scoped_identifier");
    let callee = if direct {
        node_text(function, bytes)
            .rsplit("::")
            .next()
            .unwrap_or_default()
            .to_string()
    } else {
        node_text(function, bytes)
    };
    if !callee.is_empty() {
        source.calls.push(CallSite {
            callee,
            span: exact_span(node),
            direct,
        });
    }
}

fn record_js_test(node: Node<'_>, bytes: &[u8], source: &mut SourceFile) {
    let Some(function) = node.child_by_field_name("function") else {
        return;
    };
    let framework = node_text(function, bytes);
    if !matches!(framework.as_str(), "describe" | "test" | "it") {
        return;
    }
    let name = first_quoted(&node_text(node, bytes)).unwrap_or_else(|| framework.clone());
    source.tests.push(TestDeclaration {
        name,
        span: exact_span(node),
        framework,
    });
}

fn record_reference(node: Node<'_>, bytes: &[u8], source: &mut SourceFile) {
    if source.references.len() >= 512 {
        return;
    }
    let name = node_text(node, bytes);
    if !name.is_empty() {
        source.references.push(Reference {
            name,
            span: exact_span(node),
        });
    }
}

fn rust_test_attribute(node: Node<'_>, bytes: &[u8]) -> bool {
    let mut sibling = node.prev_named_sibling();
    while let Some(candidate) = sibling {
        if candidate.kind() != "attribute_item" {
            break;
        }
        if node_text(candidate, bytes).contains("test") {
            return true;
        }
        sibling = candidate.prev_named_sibling();
    }
    false
}

fn closure_paths(
    root: &Path,
    manifest: Option<&RoutingManifest>,
    owner: Option<&OwnerDeclaration>,
    path_anchor: Option<&str>,
    expand_for_symbol: bool,
    max_files: usize,
    max_bytes: usize,
) -> Result<(Vec<String>, Vec<String>)> {
    let mut units = Vec::<String>::new();
    if let Some(path) = path_anchor {
        units.push(normalize_relative(path));
    }
    if let Some(owner) = owner {
        units.extend(owner.primary_entries.iter().map(|entry| entry.path.clone()));
        units.extend(owner.roots.iter().cloned());
        units.extend(owner.consumers.iter().cloned());
        units.extend(owner.contracts.iter().cloned());
        units.extend(owner.generated_mirrors.iter().cloned());
        units.extend(owner.tests.iter().cloned());
        units.extend(owner.instructions.iter().cloned());
    }
    if expand_for_symbol && let Some(manifest) = manifest {
        for candidate in &manifest.owners {
            units.extend(
                candidate
                    .primary_entries
                    .iter()
                    .map(|entry| entry.path.clone()),
            );
            units.extend(candidate.roots.iter().cloned());
        }
    }
    units.push("AGENTS.md".to_string());
    let mut seen_units = BTreeSet::new();
    units.retain(|unit| seen_units.insert(normalize_relative(unit)));
    let mut admitted = Vec::new();
    let mut seen_paths = BTreeSet::new();
    let mut total_bytes = 0usize;
    let mut omitted = Vec::new();
    for unit in units {
        let relative_unit = normalize_relative(&unit);
        let absolute = match confined_join(root, &relative_unit) {
            Ok(path) => path,
            Err(_) => {
                omitted.push(relative_unit);
                continue;
            }
        };
        let mut unit_paths = if absolute.is_file() {
            vec![relative_unit.clone()]
        } else if absolute.is_dir() {
            let mut paths = WalkBuilder::new(&absolute)
                .hidden(false)
                .git_ignore(true)
                .git_exclude(true)
                .git_global(true)
                .build()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
                .filter_map(|entry| entry.path().strip_prefix(root).ok().map(normalize_path))
                .filter(|path| supported_path(path) || path.ends_with("AGENTS.md"))
                .collect::<Vec<_>>();
            paths.sort();
            paths
        } else {
            Vec::new()
        };
        unit_paths.retain(|path| seen_paths.insert(path.clone()));
        let unit_bytes = unit_paths
            .iter()
            .filter_map(|path| fs::metadata(root.join(path)).ok())
            .map(|metadata| usize::try_from(metadata.len()).unwrap_or(usize::MAX))
            .sum::<usize>();
        if admitted.len() + unit_paths.len() > max_files
            || total_bytes.saturating_add(unit_bytes) > max_bytes
        {
            omitted.push(relative_unit);
            continue;
        }
        total_bytes += unit_bytes;
        admitted.extend(unit_paths);
    }
    Ok((admitted, omitted))
}

fn render_bounded(result: &mut LocateTaskResult) -> Result<String> {
    let mut rendered = serde_json::to_string(result)?;
    if rendered.len() <= LOCATE_TASK_MAX_RENDERED_BYTES {
        return Ok(rendered);
    }
    let excerpt_count = result
        .instructions
        .iter()
        .filter(|instruction| instruction.excerpt.is_some())
        .count();
    if excerpt_count > 0 {
        for instruction in &mut result.instructions {
            instruction.excerpt = None;
        }
        record_truncation(
            result,
            "instruction_excerpts",
            0,
            excerpt_count,
            "render_budget_exceeded",
            "read_file_span for the returned instruction path",
        );
        rendered = serde_json::to_string(result)?;
    }
    if rendered.len() > LOCATE_TASK_MAX_RENDERED_BYTES && !result.relationships.is_empty() {
        let known_count = result.relationships.len();
        while rendered.len() > LOCATE_TASK_MAX_RENDERED_BYTES && !result.relationships.is_empty() {
            result.relationships.pop();
            record_truncation(
                result,
                "relationships",
                result.relationships.len(),
                known_count,
                "render_budget_exceeded",
                "locate_task with an exact symbol_anchor for the omitted caller",
            );
            rendered = serde_json::to_string(result)?;
        }
    }
    for index in 0..result.source_neighborhoods.len() {
        let known_count = result.source_neighborhoods[index].text.lines().count();
        while result.source_neighborhoods[index].text.len() > 256
            && rendered.len() > LOCATE_TASK_MAX_RENDERED_BYTES
        {
            let new_len = result.source_neighborhoods[index]
                .text
                .len()
                .saturating_sub(256);
            let boundary = floor_char_boundary(&result.source_neighborhoods[index].text, new_len);
            result.source_neighborhoods[index].text.truncate(boundary);
            let returned_count = result.source_neighborhoods[index].text.lines().count();
            record_truncation(
                result,
                "source_neighborhood_lines",
                returned_count,
                known_count,
                "render_budget_exceeded",
                "read_file_span for the returned primary path and exact span",
            );
            rendered = serde_json::to_string(result)?;
        }
    }
    let known_unresolved = result.unresolved.len();
    while rendered.len() > LOCATE_TASK_MAX_RENDERED_BYTES && result.unresolved.len() > 1 {
        result.unresolved.pop();
        record_truncation(
            result,
            "unresolved",
            result.unresolved.len(),
            known_unresolved,
            "render_budget_exceeded",
            "rerun anchored locate_task or narrowed search_source for omitted diagnostics",
        );
        rendered = serde_json::to_string(result)?;
    }
    if rendered.len() > LOCATE_TASK_MAX_RENDERED_BYTES {
        anyhow::bail!("mandatory locate_task metadata exceeds 8 KiB")
    }
    Ok(rendered)
}

fn record_truncation(
    result: &mut LocateTaskResult,
    collection: &str,
    returned_count: usize,
    known_count: usize,
    reason: &str,
    followup: &str,
) {
    if let Some(existing) = result
        .truncation
        .iter_mut()
        .find(|entry| entry.collection == collection)
    {
        existing.returned_count = returned_count;
        existing.known_count = existing.known_count.max(known_count);
        existing.reason = reason.to_string();
        existing.followup = followup.to_string();
    } else {
        result.truncation.push(TruncationEvidence {
            collection: collection.to_string(),
            returned_count,
            known_count,
            reason: reason.to_string(),
            followup: followup.to_string(),
        });
    }
}

fn validate_declared_path(root: &Path, path: &str, owner: &str, errors: &mut Vec<String>) {
    match confined_join(root, path) {
        Ok(candidate) if candidate.exists() => {}
        Ok(_) => errors.push(format!(
            "routing_manifest_invalid: owner `{owner}` declares missing path `{path}`"
        )),
        Err(error) => errors.push(format!(
            "routing_manifest_invalid: owner `{owner}` path `{path}`: {error}"
        )),
    }
}

fn repository_relative_manifest_path(
    requested_root: &Path,
    canonical_root: &Path,
    manifest_path: &Path,
) -> Result<String> {
    let relative = if manifest_path.is_absolute() {
        if let Ok(relative) = manifest_path.strip_prefix(requested_root) {
            relative.to_path_buf()
        } else {
            let canonical_manifest = manifest_path
                .canonicalize()
                .with_context(|| format!("resolve {}", manifest_path.display()))?;
            canonical_manifest
                .strip_prefix(canonical_root)
                .with_context(|| {
                    format!(
                        "routing manifest {} is outside repository root",
                        manifest_path.display()
                    )
                })?
                .to_path_buf()
        }
    } else {
        manifest_path.to_path_buf()
    };
    let relative = normalize_relative(&relative.to_string_lossy());
    confined_join(canonical_root, &relative)?;
    Ok(relative)
}

fn confined_join(root: &Path, relative: &str) -> Result<PathBuf> {
    let path = Path::new(relative);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::Prefix(_) | Component::RootDir
            )
        })
    {
        anyhow::bail!("path is not repository-relative")
    }
    let joined = root.join(path);
    if joined.exists() {
        let canonical = joined.canonicalize()?;
        if !canonical.starts_with(root) {
            anyhow::bail!("path escapes repository through a link")
        }
        Ok(canonical)
    } else {
        Ok(joined)
    }
}

fn capture_contributor(
    root: &Path,
    relative: &str,
    captured: &mut BTreeMap<String, Vec<u8>>,
    fingerprints: &mut BTreeMap<String, String>,
) -> Result<()> {
    if fingerprints.contains_key(relative) {
        return Ok(());
    }
    let absolute = confined_join(root, relative)?;
    let bytes = match fs::read(&absolute) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("read {}", absolute.display()));
        }
    };
    let fingerprint = sha256_bytes(&bytes);
    captured.insert(relative.to_string(), bytes);
    fingerprints.insert(relative.to_string(), fingerprint);
    Ok(())
}

fn load_cache(path: &Path, repository_identity: &str) -> CacheLayer {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(_) => return CacheLayer::default(),
    };
    let cache = match serde_json::from_slice::<CacheLayer>(&bytes) {
        Ok(cache) => cache,
        Err(_) => {
            quarantine_cache(path);
            return CacheLayer::default();
        }
    };
    if cache.schema_version != CACHE_SCHEMA_VERSION
        || cache.repository_identity != repository_identity
    {
        return CacheLayer::default();
    }
    if cache.parser_versions != PARSER_VERSIONS {
        return CacheLayer {
            schema_version: CACHE_SCHEMA_VERSION,
            parser_versions: PARSER_VERSIONS.to_string(),
            manifest_hash: cache.manifest_hash,
            repository_identity: repository_identity.to_string(),
            files: BTreeMap::new(),
        };
    }
    cache
}

fn persist_cache(path: &Path, cache: &CacheLayer) {
    let Some(parent) = path.parent() else {
        return;
    };
    if fs::create_dir_all(parent).is_err() {
        return;
    }
    let Ok(mut temporary) = NamedTempFile::new_in(parent) else {
        return;
    };
    let Ok(bytes) = serde_json::to_vec(cache) else {
        return;
    };
    if temporary.write_all(&bytes).is_err() || temporary.as_file().sync_all().is_err() {
        return;
    }
    let _ = temporary.persist(path);
}

fn quarantine_cache(path: &Path) {
    let quarantine = path.with_extension(format!("corrupt.{}", std::process::id()));
    let _ = fs::rename(path, quarantine);
}

fn cache_path(cache_root: &Path, root: &Path) -> PathBuf {
    let identity = sha256_bytes(normalize_path(root).as_bytes());
    cache_root
        .join("source-index")
        .join(format!("{identity}.json"))
}

fn repository_identity(root: &Path) -> String {
    let mut material = normalize_path(root);
    let dot_git = root.join(".git");
    if let Ok(metadata) = fs::metadata(&dot_git) {
        if metadata.is_file() {
            if let Ok(text) = fs::read_to_string(&dot_git) {
                material.push_str(&text);
            }
        } else if let Ok(head) = fs::read_to_string(dot_git.join("HEAD")) {
            material.push_str(&head);
            if let Some(reference) = head.trim().strip_prefix("ref: ")
                && let Ok(value) = fs::read_to_string(dot_git.join(reference))
            {
                material.push_str(&value);
            }
        }
    }
    sha256_bytes(material.as_bytes())
}

fn verify_captured(root: &Path, fingerprints: &BTreeMap<String, String>) -> Result<()> {
    for (path, expected) in fingerprints {
        let actual = fs::read(root.join(path))
            .map(|bytes| sha256_bytes(&bytes))
            .unwrap_or_default();
        if &actual != expected {
            anyhow::bail!("source_changed_during_query:{path}")
        }
    }
    Ok(())
}

fn verify_absent(root: &Path, path: &str) -> Result<()> {
    match fs::symlink_metadata(root.join(path)) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Ok(_) | Err(_) => anyhow::bail!("source_changed_during_query:{path}"),
    }
}

#[cfg(test)]
fn install_before_final_verify_hook(root: &Path, hook: impl FnOnce() + Send + 'static) {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let hooks = BEFORE_FINAL_VERIFY_HOOKS.get_or_init(|| Mutex::new(BTreeMap::new()));
    hooks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(root, Box::new(hook));
}

#[cfg(test)]
fn run_before_final_verify_hook(root: &Path) {
    let hook = BEFORE_FINAL_VERIFY_HOOKS.get().and_then(|hooks| {
        hooks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(root)
    });
    if let Some(hook) = hook {
        hook();
    }
}

fn contributor_digest(fingerprints: &BTreeMap<String, String>) -> String {
    let material = fingerprints
        .iter()
        .map(|(path, fingerprint)| format!("{}\0{}", normalize_relative(path), fingerprint))
        .collect::<Vec<_>>()
        .join("\n");
    sha256_bytes(material.as_bytes())
}

fn dirty_overlay_digest(cache: &CacheLayer, fingerprints: &BTreeMap<String, String>) -> String {
    let material = fingerprints
        .iter()
        .filter(|(path, fingerprint)| {
            cache
                .files
                .get(*path)
                .is_none_or(|cached| &cached.fingerprint != *fingerprint)
        })
        .map(|(path, fingerprint)| format!("{path}\0{fingerprint}"))
        .collect::<Vec<_>>()
        .join("\n");
    sha256_bytes(material.as_bytes())
}

fn sort_source_file(source: &mut SourceFile) {
    source
        .definitions
        .sort_by_key(|item| (item.span.start_byte, item.name.clone()));
    source
        .imports
        .sort_by_key(|item| (item.span.start_byte, item.local.clone()));
    source
        .exports
        .sort_by_key(|item| (item.span.start_byte, item.exported.clone()));
    source
        .references
        .sort_by_key(|item| (item.span.start_byte, item.name.clone()));
    source
        .calls
        .sort_by_key(|item| (item.span.start_byte, item.callee.clone()));
    source
        .tests
        .sort_by_key(|item| (item.span.start_byte, item.name.clone()));
    source
        .module_edges
        .sort_by_key(|item| (item.span.start_byte, item.specifier.clone()));
    source.diagnostics.sort_by_key(|item| item.span.start_byte);
}

fn owner_for_path<'a>(manifest: &'a RoutingManifest, path: &str) -> Option<&'a OwnerDeclaration> {
    let normalized = normalize_relative(path);
    let mut owners = manifest
        .owners
        .iter()
        .filter_map(|owner| {
            let exact = owner
                .primary_entries
                .iter()
                .any(|entry| eq_path(&normalized, &entry.path))
                || owner
                    .consumers
                    .iter()
                    .any(|candidate| eq_path(&normalized, candidate))
                || owner
                    .contracts
                    .iter()
                    .any(|candidate| eq_path(&normalized, candidate))
                || owner
                    .generated_mirrors
                    .iter()
                    .any(|candidate| eq_path(&normalized, candidate))
                || owner
                    .tests
                    .iter()
                    .any(|candidate| eq_path(&normalized, candidate));
            let root_specificity = owner
                .roots
                .iter()
                .filter(|root| path_starts_with(&normalized, root))
                .map(|root| normalize_relative(root).len())
                .max();
            exact
                .then_some(usize::MAX)
                .or(root_specificity)
                .map(|specificity| (owner, specificity))
        })
        .collect::<Vec<_>>();
    let best = owners.iter().map(|(_, specificity)| *specificity).max()?;
    owners.retain(|(_, specificity)| *specificity == best);
    owners.sort_by(|(left, _), (right, _)| left.id.cmp(&right.id));
    (owners.len() == 1).then(|| owners[0].0)
}

fn owners_for_symbol<'a>(manifest: &'a RoutingManifest, symbol: &str) -> Vec<&'a OwnerDeclaration> {
    let mut owners = manifest
        .owners
        .iter()
        .filter(|owner| {
            owner
                .primary_entries
                .iter()
                .any(|entry| entry.symbol == symbol)
        })
        .collect::<Vec<_>>();
    owners.sort_by(|left, right| left.id.cmp(&right.id));
    owners
}

fn unresolved_route(reason: &str) -> RouteDecision<'static> {
    RouteDecision {
        owner: None,
        status: reason.to_string(),
        reason: reason.to_string(),
        score: 0.0,
        provenance: "unresolved".to_string(),
        alternatives: Vec::new(),
        unresolved: vec![reason.to_string()],
    }
}

fn alternative(
    owner: &OwnerDeclaration,
    reason: &str,
    score: f64,
    provenance: &str,
) -> AlternativeEvidence {
    AlternativeEvidence {
        owner_id: owner.id.clone(),
        reason: reason.to_string(),
        score,
        provenance: provenance.to_string(),
    }
}

fn normalize(value: &str) -> String {
    let folded: String = value.case_fold().collect();
    let mut result = String::new();
    let mut separated = true;
    for character in folded.chars() {
        if character.is_alphanumeric() || character == '_' {
            result.push(character);
            separated = false;
        } else if !separated {
            result.push(' ');
            separated = true;
        }
    }
    result.trim().to_string()
}

fn distinctive_tokens(value: &str) -> BTreeSet<String> {
    const GENERIC: &[&str] = &[
        "add",
        "change",
        "code",
        "create",
        "fix",
        "implement",
        "improve",
        "issue",
        "new",
        "please",
        "source",
        "task",
        "test",
        "tool",
        "update",
        "use",
    ];
    value
        .split_whitespace()
        .filter(|token| token.len() > 2 && !GENERIC.contains(token))
        .map(str::to_string)
        .collect()
}

fn contains_phrase(task: &str, phrase: &str) -> bool {
    !phrase.is_empty() && format!(" {task} ").contains(&format!(" {phrase} "))
}

fn path_starts_with(path: &str, root: &str) -> bool {
    let path = normalize_relative(path);
    let root = normalize_relative(root).trim_end_matches('/').to_string();
    eq_path(&path, &root)
        || path
            .to_ascii_lowercase()
            .starts_with(&format!("{}/", root.to_ascii_lowercase()))
}

fn eq_path(left: &str, right: &str) -> bool {
    normalize_relative(left).eq_ignore_ascii_case(&normalize_relative(right))
}

fn normalize_relative(path: &str) -> String {
    path.replace('\\', "/")
        .trim_start_matches("./")
        .trim_matches('/')
        .to_string()
}

fn normalize_path(path: impl AsRef<Path>) -> String {
    path.as_ref().to_string_lossy().replace('\\', "/")
}

fn supported_path(path: &str) -> bool {
    matches!(
        Path::new(path)
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("rs" | "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts")
    )
}

fn language_kind(path: &str) -> &'static str {
    match Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "rs" => "rust",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "tsx",
        _ => "javascript",
    }
}

fn parser_language(path: &str) -> Option<Language> {
    match Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "rs" => Some(tree_sitter_rust::LANGUAGE.into()),
        "ts" | "mts" | "cts" => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
        "tsx" => Some(tree_sitter_typescript::LANGUAGE_TSX.into()),
        "js" | "jsx" | "mjs" | "cjs" => Some(tree_sitter_javascript::LANGUAGE.into()),
        _ => None,
    }
}

fn parser_version_for(path: &str) -> &'static str {
    match language_kind(path) {
        "rust" => "tree-sitter-rust/0.24.2",
        "typescript" | "tsx" => "tree-sitter-typescript/0.23.2",
        _ => "tree-sitter-javascript/0.25.0",
    }
}

fn field_text(node: Node<'_>, field: &str, bytes: &[u8]) -> Option<String> {
    node.child_by_field_name(field)
        .map(|child| node_text(child, bytes))
}

fn node_text(node: Node<'_>, bytes: &[u8]) -> String {
    node.utf8_text(bytes).unwrap_or_default().trim().to_string()
}

fn named_leaf_texts(node: Node<'_>, bytes: &[u8], kind: &str) -> Vec<String> {
    let mut result = Vec::new();
    collect_named_leaf_texts(node, bytes, kind, &mut result);
    result
}

fn collect_named_leaf_texts(node: Node<'_>, bytes: &[u8], kind: &str, result: &mut Vec<String>) {
    if node.kind() == kind {
        result.push(node_text(node, bytes));
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_named_leaf_texts(child, bytes, kind, result);
    }
}

fn exact_span(node: Node<'_>) -> ExactSpan {
    ExactSpan {
        start_line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
    }
}

fn zero_span() -> ExactSpan {
    ExactSpan {
        start_line: 1,
        end_line: 1,
        start_byte: 0,
        end_byte: 0,
    }
}

fn first_quoted(value: &str) -> Option<String> {
    for quote in ['\'', '"'] {
        if let Some(start) = value.find(quote) {
            let rest = &value[start + quote.len_utf8()..];
            if let Some(end) = rest.find(quote) {
                return Some(rest[..end].to_string());
            }
        }
    }
    None
}

fn sha256_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn sha256_join(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    format!("{:x}", hasher.finalize())
}

fn floor_char_boundary(value: &str, mut index: usize) -> usize {
    index = index.min(value.len());
    while index > 0 && !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

#[cfg(test)]
#[path = "task_locator_tests.rs"]
mod tests;
