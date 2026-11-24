//! Bounded regression context for reads and committed edits, never admission.
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use codex_file_system::{ExecutorFileSystem, FileMetadata, FileSystemSandboxContext, FindUpErrorPolicy};
use codex_protocol::models::{ContentItem, ResponseItem};
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use crate::tools::context::{ToolCallSource, ToolInvocation};
use crate::tools::runtimes::apply_patch::ApplyPatchRequest;
use crate::session::turn_context::TurnContext;

const INDEX_BYTES: usize = 256 * 1024;
const HISTORY_BYTES: usize = 512 * 1024;
const NOTICE_BYTES: usize = 4096;
const TURN_BYTES: usize = 8192;
const EVIDENCE_RECHECK: Duration = Duration::from_secs(5);
const LEDGER_NOTICE_INTRO: &str = "Recorded KDR verification ledger, not fresh source reconciliation. For each repository being analyzed, set `$RepoRoot` (Unix: `repo_root`) to that repository's absolute filesystem root, not a bare name, relative path, or file URI. The ledger URI below is inside that root under .local/evidence/fixes/ or .codex/fix-evidence/. From KDR run `.\\kdr.cmd fix-ledger status --repo $RepoRoot --unresolved --summary` in PowerShell (bare `kdr` is not assumed on PATH) or `sh ./kdr.sh fix-ledger status --repo \"$repo_root\" --unresolved --summary` on Unix. From another directory, invoke the discovered absolute kdr.cmd path with PowerShell's `&`. Status reconciles relevant drift without running behavioral checks. Work on task-relevant unresolved findings. Preserve verified fixes unless relevant changes or new behavioral evidence invalidate their proof. Overlap warnings never refuse edits, lock sessions or restrict normal writes. Only affected proof needs KDR reverification; closure requires passing KDR evidence for the declared contract. Missing coverage is unknown.\n";

fn bounded_ledger_snapshot(repositories: &[Value], budget: usize) -> Option<String> {
    let full = json!({"repositories": repositories}).to_string();
    if full.len() <= budget {
        return Some(full);
    }

    // Reserve the recovery locators and counts before spending bytes on details.
    // An unusually long locator must not suppress the other repositories.
    let mut notice = json!({"repositories": [], "repositories_omitted": repositories.len()});
    let mut included = Vec::new();
    for repository in repositories {
        let mut envelope = repository.clone();
        if let Some(entries) = repository["unresolved"].as_array() {
            let omitted = repository["unresolved_omitted"].as_u64().unwrap_or(0);
            envelope["unresolved"] = json!([]);
            envelope["unresolved_omitted"] = json!(omitted + entries.len() as u64);
        }
        if let Some(gaps) = envelope.as_object_mut()?.remove("evidence_gaps")
            && !gaps.as_array().is_some_and(Vec::is_empty)
        {
            envelope["evidence_gaps_omitted"] =
                gaps.as_array().map_or(Value::Null, |rows| json!(rows.len()));
        }

        notice["repositories"].as_array_mut()?.push(envelope);
        notice["repositories_omitted"] = json!(repositories.len() - included.len() - 1);
        if notice.to_string().len() <= budget {
            included.push(repository);
        } else {
            notice["repositories"].as_array_mut()?.pop();
            notice["repositories_omitted"] = json!(repositories.len() - included.len());
        }
    }

    for (index, repository) in included.into_iter().enumerate() {
        let Some(entries) = repository["unresolved"].as_array() else { continue; };
        for entry in entries {
            if entry.to_string().len() > budget {
                continue;
            }
            let envelope = &mut notice["repositories"][index];
            let previous = envelope["unresolved_omitted"].as_u64()?;
            envelope["unresolved"].as_array_mut()?.push(entry.clone());
            envelope["unresolved_omitted"] = json!(previous - 1);
            if notice.to_string().len() > budget {
                let envelope = &mut notice["repositories"][index];
                envelope["unresolved"].as_array_mut()?.pop();
                envelope["unresolved_omitted"] = json!(previous);
            }
        }
    }
    let bounded = notice.to_string();
    (bounded.len() <= budget).then_some(bounded)
}

/// A recorded work queue, not a fresh behavioral pass or a source-write gate.
/// Captured through the same world-state owner on new turns and resumes.
pub(crate) async fn ledger_context(step: &crate::session::step_context::StepContext)
    -> Option<codex_extension_api::WorldStateSectionContribution>
{
    use codex_extension_api::{PreviousWorldStateSection, RenderedWorldStateFragment, WorldStateSectionContribution};
    if !step.turn.file_system_sandbox_policy().has_full_disk_read_access() { return None; }
    const SECTION_ID: &str = "verification_ledger";
    let capture = async {
        let mut repositories = Vec::new();
        for environment in step.environments.turn_environments.iter().take(8) {
            if environment.environment.is_remote() { continue; }
            let fs = environment.environment.get_filesystem();
            let sandbox = step.turn.file_system_sandbox_context(None, environment.cwd());
            let cwd = environment.cwd().clone();
            let root = codex_file_system::find_nearest_ancestor_with_markers(fs.as_ref(), &cwd,
                vec![".git".into(), ".local/evidence/fixes/ledger-view.json".into(), ".codex/fix-evidence/ledger-view.json".into()],
                FindUpErrorPolicy::Propagate, Some(&sandbox)).await?;
            let Some(root) = root else { continue; };
            let paths = [root.join(".local/evidence/fixes/ledger-view.json").map_err(std::io::Error::other)?,
                root.join(".codex/fix-evidence/ledger-view.json").map_err(std::io::Error::other)?];
            let Some(path) = fs.first_regular_file(&paths, Some(&sandbox)).await? else { continue; };
            let Some((view, _)) = read_json(fs.as_ref(), &path, INDEX_BYTES, &sandbox).await else {
                repositories.push(json!({"ledger":path.to_string(),"coverage":"unavailable"})); continue;
            };
            let Some(fixes) = view["fixes"].as_array() else {
                repositories.push(json!({"ledger":path.to_string(),"coverage":"unavailable"})); continue;
            };
            let unresolved: Vec<Value> = fixes.iter().filter(|f| f["status"] != "verified_closed" && f["status"] != "superseded")
                .take(8).map(|f| json!({"id":f["id"],"status":f["status"],"missing_conditions":f["missing_conditions"]})).collect();
            repositories.push(json!({"ledger":path.to_string(),"revision":view["revision"],
                "recorded_at_ns":view["recorded_at_ns"],"unresolved":unresolved,
                "unresolved_omitted":fixes.iter().filter(|f| f["status"] != "verified_closed" && f["status"] != "superseded").count().saturating_sub(8),
                "closed_count":fixes.iter().filter(|f| f["status"] == "verified_closed").count(),
                "evidence_gaps":view["evidence_gaps"]}));
        }
        if repositories.is_empty() { return Ok(None); }
        let Some(snapshot) = bounded_ledger_snapshot(&repositories, NOTICE_BYTES - LEDGER_NOTICE_INTRO.len()) else {
            return Ok(Some(WorldStateSectionContribution::unavailable(SECTION_ID)));
        };
        let body = format!("{LEDGER_NOTICE_INTRO}{snapshot}");
        // Recording freshness is not a changed work queue. Keep it in the
        // delivered notice, but do not replay instructions solely for a refresh.
        for repository in &mut repositories {
            if let Some(fields) = repository.as_object_mut() {
                fields.remove("recorded_at_ns");
            }
        }
        let identity = json!({"repositories":repositories});
        let rendered = identity.clone();
        Ok::<_, std::io::Error>(Some(WorldStateSectionContribution::new(SECTION_ID, identity, move |previous| {
            if matches!(previous, PreviousWorldStateSection::Known(old) if old == &rendered) { return None; }
            Some(RenderedWorldStateFragment::new("developer", ("<verification_ledger>", "</verification_ledger>"), body.clone()))
        })))
    };
    match tokio::time::timeout(Duration::from_millis(50), capture).await {
        Ok(Ok(section)) => section,
        // Uncertain discovery and deadline exhaustion cannot withdraw the accepted queue.
        Ok(Err(_)) | Err(_) => Some(WorldStateSectionContribution::unavailable(SECTION_ID)),
    }
}

/// Optional durable observations. No source mutation, permission changes or
/// session locks; errors remain diagnostics on the already-completed edit.
async fn record_ledger_overlap(req: &ApplyPatchRequest, turn: &TurnContext, change: &Change) -> Option<Value> {
    let fs = req.turn_environment.environment.get_filesystem();
    let sandbox = turn.file_system_sandbox_context(None, req.turn_environment.cwd());
    let directory = change.before_path.parent()?;
    let root = codex_file_system::find_nearest_ancestor_with_markers(fs.as_ref(), &directory,
        vec![".git".into(), ".local/evidence/fixes/ledger-view.json".into(), ".codex/fix-evidence/ledger-view.json".into()],
        FindUpErrorPolicy::Ignore, Some(&sandbox)).await.ok().flatten()?;
    let paths = [root.join(".local/evidence/fixes/ledger-view.json").ok()?, root.join(".codex/fix-evidence/ledger-view.json").ok()?];
    let path = fs.first_regular_file(&paths, Some(&sandbox)).await.ok().flatten()?;
    let (view, _) = read_json(fs.as_ref(), &path, INDEX_BYTES, &sandbox).await?;
    let root_native = root.to_abs_path().ok()?;
    let relative = change.before_path.to_abs_path().ok()?.as_path().strip_prefix(root_native.as_path()).ok()?.to_string_lossy().replace('\\', "/");
    let after_relative = change.path.to_abs_path().ok()?.as_path().strip_prefix(root_native.as_path()).ok()?.to_string_lossy().replace('\\', "/");
    let moved = !same_path(&relative, &after_relative);
    let deadline = Instant::now() + Duration::from_millis(15);
    let before = codex_tool_artifacts::source::code_sections::function_bodies(&change.before_path.to_string(), &change.before, deadline);
    let after = codex_tool_artifacts::source::code_sections::function_bodies(&change.path.to_string(), &change.after, deadline);
    let mut affected = Vec::new();
    let mut overlaps = Vec::new();
    for fix in view["fixes"].as_array()? {
        let mut conditions = Vec::new();
        for condition in fix["contract"].as_array().into_iter().flatten() {
            for scope in condition["scopes"].as_array().into_iter().flatten() {
                if !scope["path"].as_str().is_some_and(|p| same_path(p, &relative)) { continue; }
                let changed = if moved { Some(true) }
                else if let Some(symbol) = scope["symbol"].as_str() {
                    match (BehaviorChange::body(&before, symbol), BehaviorChange::body(&after, symbol)) {
                        (Some(old), Some(new)) => Some(old != new), (Some(_), None) => Some(true), _ => None,
                    }
                } else if let Some(text) = scope["text"].as_str() {
                    Some(change.before.matches(text).count() == 1 && change.after.matches(text).count() != 1)
                } else { Some(change.before != change.after || change.before_path != change.path) };
                overlaps.push(json!({"fix":fix["id"],"condition":condition["id"],"scope":scope,
                    "assessment":match changed { Some(true)=>"changes",Some(false)=>"preserves",None=>"unknown" }}));
                if changed == Some(true) && !conditions.contains(&condition["id"]) { conditions.push(condition["id"].clone()); }
            }
        }
        if !conditions.is_empty() { affected.push(json!({"fix":fix["id"],"conditions":conditions})); }
    }
    if overlaps.is_empty() { return None; }
    let observed_at_ns = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos();
    let observation = json!({"schema_version":1,"kind":"fix_overlap","advisory_only":true,
        "ledger_revision":view["revision"],"observed_at_ns":observed_at_ns.to_string(),
        "before_path":relative,"after_path":change.path.to_string(),
        "before_sha256":hex::encode(Sha256::digest(change.before.as_bytes())),
        "after_sha256":hex::encode(Sha256::digest(change.after.as_bytes())),
        "overlaps":overlaps,"affected":affected,"warning_delivery":"patch_diagnostic; provider delivery unmeasured"});
    let bytes = serde_json::to_vec(&observation).ok()?;
    if bytes.len() > 256*1024 { return Some(json!({"kind":"fix_ledger_overlap","advisory_only":true,"persistence":"unavailable_oversized"})); }
    let destination = path.parent()?.join("overlaps").ok()?;
    let event_path = destination.join(&format!("{}.json",uuid::Uuid::new_v4())).ok()?;
    let saved = async {
        fs.create_directory(&destination, codex_file_system::CreateDirectoryOptions { recursive:true }, Some(&sandbox)).await?;
        match fs.mutate_file_if_matches(&event_path, None, Some(bytes), Some(&sandbox)).await? {
            codex_file_system::FileMutationOutcome::Committed => Ok(()),
            other => Err(std::io::Error::other(format!("overlap publication incomplete: {other:?}"))),
        }
    }.await;
    Some(json!({"kind":"fix_ledger_overlap","advisory_only":true,"overlap":observation,
        "persistence":if saved.is_ok() { "recorded" } else { "unavailable" },"path":event_path.to_string(),
        "error":saved.err().map(|error|error.to_string()),"source_edit_refused":false}))
}

struct Memory {
    index: Value,
    path: PathUri,
    hash: String,
    history: Option<Value>,
    unavailable: Vec<&'static str>,
}

#[derive(Default)]
struct RootMemory {
    index: EvidenceFile,
    ledger: EvidenceFile,
    history: EvidenceFile,
    view: Option<Memory>,
    revision: u64,
    needs_rebuild: bool,
}

#[derive(Default)]
struct EvidenceFile {
    stamp: Option<(PathUri, FileMetadata)>,
    checked: Option<Instant>,
    last_good: Option<(Value, String)>,
    last_path: Option<PathUri>,
    available: bool,
    unavailable: bool,
}

#[derive(Default)]
struct State {
    directories: HashMap<String, Option<PathUri>>,
    roots: HashMap<String, RootMemory>,
    shown: HashSet<String>,
    reservation_notices: HashMap<String, String>,
    bytes: usize,
    budget_exhausted: bool,
    reads: Vec<Value>,
    reads_omitted: usize,
    notices: HashMap<String, Value>,
}

#[derive(Default)]
struct EvidenceMemory(Mutex<State>);

/// Requalify source-read reservations at the accepted-history lifecycle boundary.
/// Keep retained notices, patch assessments and the bounded per-turn byte budget.
pub(crate) async fn history_replaced(turn: &TurnContext, items: &[ResponseItem]) {
    let present: HashSet<String> = items.iter().filter_map(|item| {
        let ResponseItem::Message { role, content, .. } = item else { return None; };
        if role != "developer" { return None; }
        content.iter().find_map(|part| {
            let ContentItem::InputText { text } = part else { return None; };
            text.strip_prefix("<evidence_memory_advisory id=\"")?
                .split_once('"').map(|(id, _)| id.to_owned())
        })
    }).collect();
    let memory = turn.extension_data.get_or_init(EvidenceMemory::default);
    let mut state = memory.0.lock().await;
    let evicted: Vec<String> = state.reservation_notices.iter().filter(|(_, id)|
        !present.contains(*id) && state.notices.get(*id)
            .is_some_and(|notice| notice["payload"]["phase"] == "source_read"))
        .map(|(key, _)| key.clone()).collect();
    tracing::debug!(present = present.len(), reservations = state.reservation_notices.len(), evicted = evicted.len(), bytes = state.bytes, "evidence advisory history replacement");
    for key in evicted { state.shown.remove(&key); state.reservation_notices.remove(&key); }
}

pub(super) struct Change {
    before_path: PathUri,
    path: PathUri,
    before: String,
    after: String,
}

/// Capture committed bytes before releasing the mutation owner, never reread.
pub(super) fn patch_changes(delta: &codex_apply_patch::AppliedPatchDelta) -> Option<Vec<Change>> {
    use codex_apply_patch::AppliedPatchFileChange;
    if !delta.is_exact() { return None; }
    let mut changes: Vec<Change> = Vec::new();
    let mut bytes = 0usize;
    for change in delta.changes() {
        let (before, after) = match &change.change {
            AppliedPatchFileChange::Add { content, overwritten_content } =>
                (overwritten_content.as_deref().unwrap_or(""), content.as_str()),
            AppliedPatchFileChange::Update { old_content, new_content, .. } =>
                (old_content.as_str(), new_content.as_str()),
            AppliedPatchFileChange::Delete { content } => (content.as_str(), ""),
        };
        bytes = bytes.saturating_add(before.len()).saturating_add(after.len());
        let overwritten = match &change.change {
            AppliedPatchFileChange::Update { overwritten_move_content, .. } => overwritten_move_content.as_ref(),
            _ => None,
        };
        bytes = bytes.saturating_add(overwritten.map_or(0, |s| s.len()));
        if bytes > 256 * 1024 || changes.len() + 1 + usize::from(overwritten.is_some()) > 8 { return None; }
        let endpoint = delta.endpoint_change(change);
        let before_path = endpoint.path_uri().clone();
        let path = endpoint.move_path_uri().unwrap_or(endpoint.path_uri()).clone();
        let original = changes.iter().position(|c| c.path == before_path)
            .map(|index| changes.remove(index));
        if let AppliedPatchFileChange::Update { overwritten_move_content: Some(content), .. } = &change.change {
            changes.push(Change { before_path:path.clone(), path:path.clone(),
                before:content.clone(), after:after.to_owned() });
        }
        changes.push(Change { before_path:original.as_ref().map_or(before_path, |c| c.before_path.clone()),
            path, before:original.map_or_else(|| before.to_owned(), |c| c.before), after:after.to_owned() });
    }
    changes.retain(|c| c.before != c.after || c.before_path != c.path);
    Some(changes)
}

fn same_path(a: &str, b: &str) -> bool {
    if cfg!(windows) { a.eq_ignore_ascii_case(b) } else { a == b }
}

fn bounded(value: &Value, limit: usize) -> Value {
    value.as_str().map(|s| Value::String(s.chars().take(limit).collect())).unwrap_or(Value::Null)
}

struct BehaviorChange<'a> {
    change: &'a Change,
    before_path: String,
    path: String,
    before: Option<Vec<(String, &'a str)>>,
    after: Option<Vec<(String, &'a str)>>,
}

impl BehaviorChange<'_> {
    fn body<'a>(bodies: &'a Option<Vec<(String, &str)>>, symbol: &str) -> Option<&'a str> {
        let rows = bodies.as_ref()?;
        let mut matches = rows.iter().filter(|(name, _)| name == symbol);
        let (_, body) = matches.next()?;
        matches.next().is_none().then_some(*body)
    }
}

fn constraints(fix: &Value, relative: &str, symbol: Option<&str>, change: Option<&BehaviorChange<'_>>) -> Vec<Value> {
    let mut result: Vec<Value> = fix["behavioral_constraints"].as_array().into_iter().flatten().filter_map(|constraint| {
        for key in ["id", "broken_behavior", "correction", "must_remain_true"] {
            constraint[key].as_str().filter(|s| !s.is_empty())?;
        }
        let scopes = constraint["scopes"].as_array()?;
        let mut assessments = Vec::new();
        for scope in scopes.iter().take(16) {
            let path_match = scope["path"].as_str().is_some_and(|path|
                same_path(path, relative) || change.is_some_and(|c| same_path(path, &c.before_path)));
            let target = scope["symbol"].as_str();
            let symbol_match = target.is_some_and(|s| change.is_some_and(|c|
                BehaviorChange::body(&c.before, s).is_some() || BehaviorChange::body(&c.after, s).is_some()));
            let anchors = scope["anchors"].as_array();
            let anchor_match = anchors.is_some_and(|a| a.iter().take(8).filter_map(Value::as_str)
                .filter(|s| !s.is_empty() && s.len() <= 256).any(|s| change.is_some_and(|c|
                    c.change.before.contains(s) || c.change.after.contains(s))));
            if !path_match && !symbol_match && !anchor_match { continue; }
            if change.is_none() && symbol.is_some() && target.is_some() && symbol != target { continue; }
            let (assessment, basis) = if let Some(c) = change {
                let old = target.and_then(|s| BehaviorChange::body(&c.before, s));
                let new = target.and_then(|s| BehaviorChange::body(&c.after, s));
                // Anchors are registered behavior fingerprints, not assertions
                // that arbitrary text proves correct behavior.
                let lost = anchors.is_some_and(|a| a.iter().take(8).filter_map(Value::as_str)
                    .filter(|s| !s.is_empty() && s.len() <= 256).any(|s|
                        old.unwrap_or(&c.change.before).contains(s) && !new.unwrap_or(&c.change.after).contains(s)));
                if lost || (old.is_some() && new.is_none() && c.after.is_some()) {
                    ("potentially_invalidates", "protected anchor or function removed")
                } else if c.before_path != c.path {
                    ("potentially_invalidates", "owner moved; linkage not established")
                } else if scope["text"].as_str().is_some_and(|text|
                    c.change.before.matches(text).count() == 1 && c.change.after.matches(text).count() == 1)
                {
                    ("preserves", "registered exact text unchanged; callers and dependencies not proven")
                } else if old.is_some() && old == new {
                    ("preserves", "registered function bytes unchanged; callers and dependencies not proven")
                } else if old.is_some() && new.is_some() {
                    ("changes", "registered function changed; behavioral effect requires assessment")
                } else {
                    ("potentially_invalidates", "path/behavior overlap; exact function comparison unavailable")
                }
            } else { ("context", "protected behavior for the selected source") };
            assessments.push(json!({"path":bounded(&scope["path"],240), "symbol":bounded(&scope["symbol"],160),
                "assessment":assessment, "basis":basis}));
        }
        if assessments.is_empty() { return None; }
        assessments.sort_by_key(|s| match s["assessment"].as_str() {
            Some("potentially_invalidates") => 0, Some("changes") => 1, _ => 2,
        });
        let needs_verification = assessments.iter().any(|s| matches!(s["assessment"].as_str(), Some("changes" | "potentially_invalidates")));
        let verification: Vec<Value> = if needs_verification {
            constraint["verification"].as_array().into_iter().flatten().filter(|v|
                v["receipt_sha256"].as_str().is_some_and(|hash| fix["receipts"].as_array()
                    .is_some_and(|refs| refs.iter().any(|r| r["sha256"] == hash))))
                .take(2).map(|v| {
                    let mut target = json!({"kind":bounded(&v["kind"],32),
                        "receipt_sha256":v["receipt_sha256"], "selector":bounded(&v["selector"],240)});
                    if !v["recorded_proof_valid"].is_null() {
                        target["recorded_proof_valid"] = v["recorded_proof_valid"].clone();
                    }
                    target
                }).collect()
        } else { Vec::new() };
        Some(json!({"id":bounded(&constraint["id"],120),
            "broken_behavior":bounded(&constraint["broken_behavior"],240),
            "correction":bounded(&constraint["correction"],240),
            "causal_lesson_coverage":constraint["causal_lesson_coverage"].as_str().unwrap_or("authored_not_independently_verified"),
            "must_remain_true":bounded(&constraint["must_remain_true"],320),
            "scopes":assessments.into_iter().take(2).collect::<Vec<_>>(),
            "targeted_verification":verification,
            "verification_needed":needs_verification}))
    }).collect();
    result.sort_by_key(|c| !c["verification_needed"].as_bool().unwrap_or(false));
    result.truncate(2);
    result
}

async fn read_json(
    fs: &dyn ExecutorFileSystem,
    path: &PathUri,
    limit: usize,
    sandbox: &FileSystemSandboxContext,
) -> Option<(Value, String)> {
    let bytes = fs.read_file_bounded(path, limit, Some(sandbox)).await.ok()??;
    let value = serde_json::from_slice(bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&bytes)).ok()?;
    Some((value, hex::encode(Sha256::digest(&bytes))))
}

impl EvidenceFile {
    fn current(&self) -> Option<&(Value, String)> {
        self.available.then_some(self.last_good.as_ref()).flatten()
    }

    async fn refresh(
        &mut self,
        fs: &dyn ExecutorFileSystem,
        paths: &[PathUri],
        limit: usize,
        sandbox: &FileSystemSandboxContext,
        validate: impl Fn(&Value) -> bool,
        ignore_recording_time: bool,
        expected: bool,
    ) -> bool {
        let mut stamp = None;
        let mut inaccessible = false;
        for path in paths {
            match fs.get_metadata(path, Some(sandbox)).await {
                Ok(metadata) if metadata.is_file => {
                    stamp = Some((path.clone(), metadata));
                    break;
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => { inaccessible = true; self.last_path = Some(path.clone()); break; }
            }
        }
        // The filesystem stamp is a cheap hint, not a content identity. Recheck
        // periodically even when size and millisecond timestamps are unchanged.
        if !inaccessible && self.stamp == stamp
            && self.checked.is_some_and(|checked| checked.elapsed() < EVIDENCE_RECHECK)
        {
            return false;
        }
        let same_path = self.stamp.as_ref().map(|(path, _)| path)
            == stamp.as_ref().map(|(path, _)| path);
        let was_available = self.available;
        let was_unavailable = self.unavailable;
        // Read before publishing any new observation; the caller may cancel
        // the whole optional lookup at its deadline.
        let fresh = if let Some((path, _)) = &stamp {
            tracing::trace!(path = %path, "refreshing bounded evidence owner");
            read_json(fs, path, limit, sandbox).await
        } else { None };
        let mut changed = false;
        self.available = false;
        self.unavailable = inaccessible || (stamp.is_none() && !paths.is_empty()
            && (expected || self.last_good.is_some()));
        if let Some((path, _)) = &stamp {
            self.last_path = Some(path.clone());
            match fresh {
                Some((mut value, hash)) if validate(&value) => {
                    if ignore_recording_time {
                        // Store the semantic input; a new recording timestamp
                        // alone does not change the work queue or its notices.
                        if let Some(view) = value.as_object_mut() {
                            view.remove("recorded_at_ns");
                        }
                    }
                    if !same_path || !self.last_good.as_ref().is_some_and(|(old, _)| old == &value) {
                        self.last_good = Some((value, hash));
                        changed = true;
                    }
                    self.available = true;
                }
                _ => self.unavailable = true,
            }
        }
        self.stamp = stamp;
        self.checked = Some(Instant::now());
        changed || self.available != was_available || self.unavailable != was_unavailable
    }
}

async fn lookup(
    state: &mut State,
    fs: &dyn ExecutorFileSystem,
    path: &PathUri,
    sandbox: &FileSystemSandboxContext,
    symbol: Option<&str>,
    change: Option<&Change>,
) -> Option<(String, Vec<String>, String, Value)> {
    if state.bytes >= TURN_BYTES { return None; }
    let directory = path.parent()?;
    let key = directory.to_string();
    if !state.directories.contains_key(&key) {
        if state.directories.len() >= 64 { return None; }
        let root = codex_file_system::find_nearest_ancestor_with_markers(
            fs, &directory, vec![".git".into(), ".local/evidence/fixes/index.json".into(), ".codex/fix-evidence/index.json".into()],
            FindUpErrorPolicy::Ignore, Some(sandbox),
        ).await.ok()?;
        state.directories.insert(key.clone(), root);
    }
    let root = state.directories.get(&key)?.as_ref()?.clone();
    let root_key = root.to_string();
    if state.roots.len() >= 8 && !state.roots.contains_key(&root_key) { return None; }
    let root_memory = state.roots.entry(root_key.clone()).or_default();
    let index_paths = [root.join(".local/evidence/fixes/index.json").ok()?,
        root.join(".codex/fix-evidence/index.json").ok()?];
    let ledger_paths = [root.join(".local/evidence/fixes/ledger-view.json").ok()?,
        root.join(".codex/fix-evidence/ledger-view.json").ok()?];
    root_memory.needs_rebuild |= root_memory.index.refresh(fs, &index_paths, INDEX_BYTES, sandbox,
        |v| v["schema_version"] == 1 && v["fixes"].is_array(), false, false).await;
    root_memory.needs_rebuild |= root_memory.ledger.refresh(fs, &ledger_paths, INDEX_BYTES, sandbox,
        |v| v["fixes"].is_array(), true, false).await;
    let history_path = root_memory.index.current()
        .and_then(|(index, _)| index["benchmark_advisories"].as_str())
        .and_then(|link| root_memory.index.last_path.as_ref()?.parent()?.join(link).ok());
    root_memory.needs_rebuild |= root_memory.history.refresh(fs, history_path.as_slice(), HISTORY_BYTES, sandbox,
        |v| v["schema_version"] == 1, false, true).await;
    if root_memory.needs_rebuild {
        let (mut index, mut hash) = root_memory.index.current().cloned()
            .unwrap_or_else(|| (json!({"schema_version":1,"fixes":[]}), String::new()));
        if let Some((view, view_hash)) = root_memory.ledger.current() {
            for fix in view["fixes"].as_array().into_iter().flatten() {
                if fix["status"] == "superseded" {
                    if let Some(fixes) = index["fixes"].as_array_mut() { fixes.retain(|row| row["id"] != fix["id"]); }
                    continue;
                }
                let mut receipts = Vec::new();
                let active: Vec<&Value> = fix["contract"].as_array().into_iter().flatten()
                    .filter(|c| c["id"].as_str().is_some_and(|id| fix["retired_conditions"].get(id).is_none())).collect();
                let rows: Vec<Value> = active.iter().map(|condition| {
                    let verification: Vec<Value> = condition["id"].as_str().filter(|id| !id.is_empty() && id.len() <= 120)
                        .into_iter().flat_map(|id| fix["proofs"].as_array().into_iter().flatten().filter_map(move |proof| {
                            if proof["condition_id"].as_str() != Some(id) { return None; }
                            let valid = match proof["valid"].as_i64()? { 0 => false, 1 => true, _ => return None };
                            let reference = &proof["receipt"];
                            let hash = reference["sha256"].as_str().filter(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))?;
                            let path = reference["receipt"].as_str().filter(|s| !s.is_empty() && s.len() <= 240)?;
                            let selector = reference["selector"].as_str().filter(|s| s.len() <= 240 && *s == format!("/checks/{id}"))?;
                            Some((json!({"kind":"ledger_proof", "path":path, "sha256":hash}),
                                json!({"kind":"ledger_proof", "receipt_sha256":hash, "selector":selector,
                                    "recorded_proof_valid":valid})))
                        })).take(2).map(|(receipt, target)| { receipts.push(receipt); target }).collect();
                    json!({"id":condition["id"],"broken_behavior":"not recorded",
                        "correction":"not recorded", "causal_lesson_coverage":"not_recorded",
                        "must_remain_true":condition["behavior"],"scopes":condition["scopes"],
                        "verification":verification})
                }).collect();
                let paths: Vec<Value> = active.iter()
                    .flat_map(|c| c["scopes"].as_array().into_iter().flatten().map(|s|s["path"].clone())).collect();
                if let Some(fixes) = index["fixes"].as_array_mut() {
                    let incoming = json!({"id":fix["id"],"paths":paths,"receipts":receipts,
                        "behavioral_constraints":rows,"ledger_status":fix["status"],"ledger_revision":view["revision"]});
                    if let Some(existing) = fixes.iter_mut().find(|row| row["id"] == fix["id"]) {
                        // Registry lessons and ledger proof are complementary. Keep
                        // authored causal detail, union references, select one fix row.
                        for key in ["paths", "receipts"] {
                            if !existing[key].is_array() { existing[key] = json!([]); }
                            for value in incoming[key].as_array().into_iter().flatten() {
                                let values = existing[key].as_array_mut()?;
                                if !values.contains(value) { values.push(value.clone()); }
                            }
                        }
                        if !existing["behavioral_constraints"].is_array() { existing["behavioral_constraints"] = json!([]); }
                        let constraints = existing["behavioral_constraints"].as_array_mut()?;
                        constraints.retain(|row| row["id"].as_str().is_none_or(|id| fix["retired_conditions"].get(id).is_none()));
                        for row in &rows {
                            if let Some(authored) = constraints.iter_mut().find(|c| c["id"] == row["id"]) {
                                if !authored["verification"].is_array() { authored["verification"] = json!([]); }
                                for target in row["verification"].as_array().into_iter().flatten() {
                                    let targets = authored["verification"].as_array_mut()?;
                                    if !targets.contains(target) { targets.push(target.clone()); }
                                }
                            } else { constraints.push(row.clone()); }
                        }
                        existing["ledger_status"] = fix["status"].clone();
                        existing["ledger_revision"] = view["revision"].clone();
                    } else { fixes.push(incoming); }
                }
            }
            hash = hex::encode(Sha256::digest(format!("{hash}:{view_hash}").as_bytes()));
        }
        let unavailable = [
            ("index", root_memory.index.unavailable),
            ("ledger", root_memory.ledger.unavailable),
            ("history", root_memory.history.unavailable),
        ].into_iter().filter_map(|(name, missing)| missing.then_some(name)).collect();
        root_memory.view = root_memory.index.last_path.as_ref().or(root_memory.ledger.last_path.as_ref())
            .map(|path| Memory { index, path: path.clone(), hash, unavailable,
                history:root_memory.history.current().map(|(value, _)| value.clone()) });
        root_memory.revision = root_memory.revision.saturating_add(1);
        root_memory.needs_rebuild = false;
    }
    let memory = root_memory.view.as_ref()?;
    let evidence_revision = root_memory.revision;
    let root_native = root.to_abs_path().ok()?;
    let native = change.map_or(path, |c| &c.path).to_abs_path().ok()?;
    let relative = native.as_path().strip_prefix(root_native.as_path()).ok()?
        .to_str()?.replace('\\', "/");
    let revision = change.map(|c| hex::encode(Sha256::digest(format!("{}\0{}\0{}", c.before_path, c.before, c.after).as_bytes()))).unwrap_or_default();
    let path_reservation = format!("{root_key}:path:{evidence_revision}:{relative}:{}:{revision}",symbol.unwrap_or(""));
    if state.shown.contains(&path_reservation) { return None; }
    let deadline = Instant::now() + Duration::from_millis(15);
    let behavior = change.map(|c| BehaviorChange {
        change:c, before_path:c.before_path.to_abs_path().ok().and_then(|p|
            p.as_path().strip_prefix(root_native.as_path()).ok().map(|p| p.to_string_lossy().replace('\\',"/"))).unwrap_or_default(),
        path:relative.clone(),
        before:codex_tool_artifacts::source::code_sections::function_bodies(&c.before_path.to_string(), &c.before, deadline),
        after:codex_tool_artifacts::source::code_sections::function_bodies(&c.path.to_string(), &c.after, deadline),
    });
    let mut matches: Vec<Value> = memory.index["fixes"].as_array()?.iter().filter(|fix| {
        !constraints(fix, &relative, symbol, behavior.as_ref()).is_empty()
            || fix["paths"].as_array().is_some_and(|paths| paths.iter().any(|p|
                p.as_str().is_some_and(|p| same_path(p, &relative)
                    || behavior.as_ref().is_some_and(|c| same_path(p, &c.before_path)))))
    }).cloned().collect();
    let matching_fixes = matches.len();
    let has_constraints = matches.iter().any(|fix|
        !constraints(fix, &relative, symbol, behavior.as_ref()).is_empty());
    // A modeled conflict must not lose its budget to unrelated path-only
    // history. Keep the full index linked and count that unknown coverage.
    if change.is_some() && has_constraints {
        matches.retain(|fix| !constraints(fix, &relative, symbol, behavior.as_ref()).is_empty());
    }
    let unmodeled_path_matches = matching_fixes - matches.len();
    // Deferred owners need not be registered fixes. Keep this read-only
    // context separate from preservation constraints and patch assessments.
    let source_key = native.as_path().to_str()?.replace('\\', "/");
    let source_key = if cfg!(windows) { source_key.trim_start_matches("//?/").to_ascii_lowercase() } else { source_key };
    let deferred: Vec<Value> = if change.is_none() {
        memory.history.as_ref().and_then(|h| h["deferred_findings"]["by_source_path"][&source_key].as_array())
            .into_iter().flatten().filter(|r| r["status"] == "deferred_unfinished"
                && matches!(r["current_status"].as_str(), Some("open" | "blocked")))
            .map(|r| json!({"id":bounded(&r["id"],120),"audit":bounded(&r["audit"],160),
                "status":"deferred_unfinished","path":r["path"],"sha256":r["sha256"],
                "scope":bounded(&r["scope"],160),"reason":bounded(&r["reason"],240),
                "next_action":bounded(&r["next_action"],240)})).collect()
    } else { Vec::new() };
    if matches.is_empty() && deferred.is_empty() && memory.unavailable.is_empty() { return None; }
    let priority = |fix: &Value| {
        if !fix["ledger_status"].is_null() { return -2; }
        if !constraints(fix, &relative, symbol, behavior.as_ref()).is_empty() { return -1; }
        fix["receipts"].as_array().into_iter().flatten().filter_map(|r| r["sha256"].as_str())
            .flat_map(|hash| memory.history.as_ref().and_then(|h| h["by_receipt_sha256"][hash].as_array()).into_iter().flatten())
            .map(|row| match row["status"].as_str().unwrap_or("") {
                "rejected_correctness" | "rejected_wall_clock_regression" | "rejected_historical_wall_clock_regression" => 0,
                "improved_no_observed_regression" => 1,
                _ => 2,
            }).min().unwrap_or(3)
    };
    matches.sort_by_key(priority);
    if matches.len() > 3
        && let Some(index) = matches.iter().position(|fix| priority(fix) == 1)
        && index > 2
    { matches.swap(2, index); }
    let mut fixes = Vec::new();
    let mut reservations = Vec::new();
    for fix in &matches {
        let Some(id) = fix["id"].as_str().filter(|id| id.len() <= 240) else { continue; };
        let reservation = format!("{root_key}:{evidence_revision}:{}:{id}:{relative}:{}:{revision}", memory.hash, symbol.unwrap_or(""));
        if state.shown.contains(&reservation) { continue; }
        let selected_constraints = constraints(fix, &relative, symbol, behavior.as_ref());
        let selected_ids: HashSet<&str> = selected_constraints.iter().filter_map(|c| c["id"].as_str()).collect();
        let target_hashes: HashSet<&str> = fix["behavioral_constraints"].as_array().into_iter().flatten()
            .filter(|c| c["id"].as_str().is_some_and(|id| selected_ids.contains(id)))
            .flat_map(|c| c["verification"].as_array().into_iter().flatten())
            .filter_map(|v| v["receipt_sha256"].as_str()).collect();
        let mut references: Vec<&Value> = fix["receipts"].as_array().into_iter().flatten().collect();
        references.sort_by_key(|r| !r["sha256"].as_str().is_some_and(|hash| target_hashes.contains(hash)));
        let receipts: Vec<Value> = references.into_iter()
            .take(if has_constraints { 1 } else { 2 }).map(|receipt| json!({
                "kind": receipt["kind"], "path": receipt["path"], "sha256": receipt["sha256"],
            })).collect();
        let mut seen = HashSet::new();
        let mut benchmarks = Vec::new();
        for receipt in fix["receipts"].as_array().into_iter().flatten() {
            let Some(hash) = receipt["sha256"].as_str() else { continue; };
            for row in memory.history.as_ref().and_then(|h| h["by_receipt_sha256"][hash].as_array())
                .into_iter().flatten()
            {
                let Some(observation) = row["observation"].as_str() else { continue; };
                if seen.insert(observation.to_string()) {
                    benchmarks.push(json!({"observation": observation, "status": row["status"],
                        "workload": row["workload"], "scope": row["scope"],
                        "metrics": row["metrics"].as_array().map(|m| m.iter().take(2).cloned().collect::<Vec<_>>())}));
                }
            }
        }
        benchmarks.sort_by_key(|row| match row["status"].as_str().unwrap_or("") {
            "rejected_correctness" | "rejected_wall_clock_regression" | "rejected_historical_wall_clock_regression" => 0,
            "improved_no_observed_regression" => 1,
            _ => 2,
        });
        let benchmark_observations = benchmarks.len();
        // Retain an advantage as well as a warning when both are known.
        if benchmarks.len() > 2
            && let Some(index) = benchmarks.iter().position(|r| r["status"] == "improved_no_observed_regression")
            && index > 1
        { benchmarks.swap(1, index); }
        benchmarks.truncate(2);
        if has_constraints {
            for row in &mut benchmarks { row["metrics"] = json!([]); }
        }
        // Edit receipts focus on conflicts, not repeat the read's metric body.
        if change.is_some() { benchmarks.clear(); }
        let protected = constraints(fix, &relative, symbol, behavior.as_ref());
        let mut row = json!({"fix": id, "receipts": receipts,
            "receipts_omitted": fix["receipts"].as_array().map_or(0, |r| r.len().saturating_sub(receipts.len())),
            "behavioral_constraints":protected,
            "constraint_coverage":if protected.is_empty() { "not_recorded" } else { "registered_scopes_only" },
            "conflict_assessment":if change.is_some() && protected.is_empty() {
                "unknown: path overlap without recorded behavioral constraints"
            } else { "see registered scopes" },
            "benchmark_history": benchmarks, "benchmark_observations":benchmark_observations});
        if !fix["ledger_status"].is_null() {
            row["recorded_ledger_status"] = fix["ledger_status"].clone();
            row["ledger_revision"] = fix["ledger_revision"].clone();
            row["work_policy"] = json!("Closed fixes are preservation context, not investigation assignments. Reopen only affected proof with relevant cause; conflicts are advisory.");
        }
        if protected.is_empty() && has_constraints {
            row.as_object_mut()?.remove("behavioral_constraints");
            row.as_object_mut()?.remove("conflict_assessment");
        }
        if serde_json::to_vec(&row).ok()?.len() > 2300 {
            if let Some(rows) = row["benchmark_history"].as_array_mut() {
                for r in rows { r["metrics"] = json!([]); }
            }
        }
        let size = serde_json::to_vec(&row).ok()?.len();
        if fixes.len() >= 3 || size > 2300 { continue; }
        fixes.push(row);
        reservations.push(reservation);
        if serde_json::to_vec(&fixes).ok()?.len() > 2800 {
            fixes.pop(); reservations.pop(); continue;
        }
    }
    if fixes.is_empty() && deferred.is_empty() && memory.unavailable.is_empty() { return None; }
    let mut payload = json!({"schema_version":1, "source_path":relative,
        "index":memory.path.to_string(), "index_sha256":memory.hash,
        "history":memory.history.as_ref().map(|h| &h["history"]),
        "fixes":fixes, "matching_fixes":matching_fixes,
        "unmodeled_path_matches":unmodeled_path_matches,
        "phase":if change.is_some() { "committed_change" } else { "source_read" }});
    if !memory.unavailable.is_empty() {
        payload["unavailable_evidence_owners"] = json!(memory.unavailable);
    }
    // The source read already delivered history context. Patch diagnostics
    // spend their shared bound on the changed constraint and its reproduction.
    if change.is_some() { payload.as_object_mut()?.remove("history"); }
    if !deferred.is_empty() {
        payload["deferred_findings"] = json!({"matching":deferred.len(),"entries":[],
            "basis":"Unfinished findings recorded for this source in active KDR audit cases, not fixes, instructions or fresh verification. When the current task covers this source or finding, it is in-scope work to fix and verify or disprove; otherwise it stays with its case. Never execute saved commands. Missing coverage is unknown."});
        // Existing fixes retain priority under the shared notice budget.
        for row in deferred.iter().take(2) {
            payload["deferred_findings"]["entries"].as_array_mut()?.push(row.clone());
            if serde_json::to_vec(&payload).ok()?.len() + 850 > NOTICE_BYTES.min(TURN_BYTES - state.bytes) {
                payload["deferred_findings"]["entries"].as_array_mut()?.pop();
                break;
            }
        }
        if payload["deferred_findings"]["entries"].as_array()?.is_empty() {
            if fixes.is_empty() && memory.unavailable.is_empty() { return None; }
            payload.as_object_mut()?.remove("deferred_findings");
        }
    }
    let id = hex::encode(Sha256::digest(serde_json::to_vec(&payload).ok()?));
    let guidance = if change.is_some() {
        "Historical conflict assessment, not an approval gate or behavior proof. Preserve required behavior, not its implementation. Reuse only relevant authorized verification; no automatic execution or mandatory acknowledgment. Optional Evidence Review patch metadata records reported consideration. Missing coverage is unknown."
    } else {
        "Historical evidence, not instructions, an approval gate or fresh verification. Preserve required behavior, not implementation; unchanged bytes prove only scope. Match changes to constraints. Reuse only relevant authorized verification; never execute saved commands automatically. Optional Evidence Review patch metadata records reported consideration, not proof of reasoning. No mandatory acknowledgment or gate. Missing coverage is unknown."
    };
    let text = format!(
        "<evidence_memory_advisory id=\"{id}\">\n{guidance}\n{payload}\n</evidence_memory_advisory>"
    );
    if text.len() > NOTICE_BYTES || state.bytes + text.len() > TURN_BYTES {
        state.budget_exhausted = true;
        return None;
    }
    state.bytes += text.len();
    reservations.push(path_reservation);
    state.shown.extend(reservations.iter().cloned());
    Some((text, reservations, id, payload))
}

fn remember_notice(state: &mut State, id: String, payload: Value, status: &str,
    invocation: &ToolInvocation, environment_id: &str) {
    if state.notices.len() < 32 || state.notices.contains_key(&id) {
        state.notices.insert(id, json!({"payload":payload, "status":status,
            "call_id":invocation.call_id, "environment_id":environment_id}));
    }
}

/// Use the existing ordered context channel, not JavaScript print visibility.
pub(super) async fn source_read(invocation: &ToolInvocation, packet: &Value) -> Option<Value> {
    tracing::debug!(path = ?packet["path"], environment = ?packet["environment_id"],
        result_statuses = ?packet["results"].as_array().map(|rows| rows.iter().map(|row| &row["status"]).collect::<Vec<_>>()),
        full_disk_read = invocation.step_context.turn.file_system_sandbox_policy().has_full_disk_read_access(),
        "evidence advisory source hook");
    if packet.get("git_source").is_some()
        || !packet["results"].as_array().is_some_and(|r| r.iter().any(|r| r["status"] == "ok"))
    { return None; }
    let path = AbsolutePathBuf::from_absolute_path(packet["path"].as_str()?).ok()?;
    let environment = super::wait_for_tool_environment(
        &invocation.step_context.environments, packet["environment_id"].as_str(),
        &invocation.cancellation_token,
    ).await.ok()??;
    if environment.environment.is_remote()
        || !invocation.step_context.turn.file_system_sandbox_policy().has_full_disk_read_access()
    { return None; }
    let turn = &invocation.step_context.turn;
    let fs = environment.environment.get_filesystem();
    let sandbox = turn.file_system_sandbox_context(None, environment.cwd());
    let ledger = turn.extension_data.get_or_init(EvidenceMemory::default);
    let started = Instant::now();
    let current_history = invocation.session.clone_history().await;
    let present_ids: HashSet<String> = current_history.raw_items().iter().filter_map(|item| {
        let ResponseItem::Message { role, content, .. } = item else { return None; };
        if role != "developer" { return None; }
        let [ContentItem::InputText { text }] = content.as_slice() else { return None; };
        text.strip_prefix("<evidence_memory_advisory id=\"")?
            .split_once('"').map(|(id, _)| id.to_owned())
    }).collect();
    let pending_calls: HashSet<String> = turn.pending_post_tool_contexts.lock().await.keys().cloned().collect();
    // No new tasks, processes, model calls or waits on optional evidence.
    let found = tokio::time::timeout(Duration::from_millis(50), async {
        let mut state = ledger.0.lock().await;
        // Same TurnContext can survive compaction. Release only reservations
        // whose accepted notice was evicted; queued contexts remain reserved.
        let evicted: Vec<String> = state.reservation_notices.iter().filter(|(_, id)|
            !present_ids.contains(*id) && state.notices.get(*id).is_some_and(|notice|
                matches!(notice["status"].as_str(), Some("context_recorded" | "context_reused"))
                    || (notice["status"] == "context_queued"
                        && !pending_calls.contains(notice["call_id"].as_str().unwrap_or("")))))
            .map(|(key, _)| key.clone()).collect();
        for key in evicted { state.shown.remove(&key); state.reservation_notices.remove(&key); }
        // Acquisition is observable; model reading/understanding is not. Keep
        // only bounded provenance, never source bodies or a second file hash.
        if let Some(hash) = packet["source_sha256"].as_str() {
            if state.reads.len() < 128 {
                let ranges: Vec<Value> = packet["results"].as_array().into_iter().flatten()
                    .filter_map(|row| row.get("canonical_range")).take(4).cloned().collect();
                state.reads.push(json!({"path":PathUri::from_abs_path(&path).to_string(),
                    "source_sha256":hash, "call_id":invocation.call_id,
                    "environment_id":environment.environment_id, "selected_ranges":ranges,
                    "coverage":"native_snapshot_acquisition; selected ranges only; not proof of model reading"}));
            } else { state.reads_omitted += 1; }
        }
        let symbol = packet["selector_bindings"].as_array().filter(|rows| rows.len() == 1)
            .and_then(|rows| rows[0]["qualified_name"].as_str());
        let found = lookup(&mut state, fs.as_ref(), &PathUri::from_abs_path(&path), &sandbox, symbol, None).await;
        tracing::debug!(present = present_ids.len(), shown = state.shown.len(), reservations = state.reservation_notices.len(), bytes = state.bytes, found = found.is_some(), "evidence advisory source lookup");
        found
    }).await.ok().flatten();
    let (text, reservations, id, payload) = found?;
    {
        let mut state = ledger.0.lock().await;
        for key in &reservations { state.reservation_notices.insert(key.clone(), id.clone()); }
    }
    let bytes = text.len();
    // Consult accepted current history, not a permanent per-thread seen set:
    // compaction/removal must allow a later turn to deliver the evidence again.
    let history = invocation.session.clone_history().await;
    let latest = history.raw_items().iter().rev().find_map(|item| {
        let ResponseItem::Message { role, content, .. } = item else { return None; };
        if role != "developer" { return None; }
        let [ContentItem::InputText { text: present }] = content.as_slice() else { return None; };
        if !present.starts_with("<evidence_memory_advisory ") { return None; }
        let previous: Value = serde_json::from_str(present.lines().nth(2)?).ok()?;
        (previous["phase"] == "source_read" && previous["index"] == payload["index"]
            && previous["source_path"] == payload["source_path"]).then_some(present)
    });
    if latest == Some(&text) {
        // The notice is already available; keep this turn's reservations but
        // don't spend its injection allowance on a body we did not inject.
        let mut state = ledger.0.lock().await;
        state.bytes = state.bytes.saturating_sub(bytes);
        remember_notice(&mut state, id.clone(), payload, "context_reused", invocation,
            &environment.environment_id);
        return Some(json!({"schema_version":1,"id":id,"status":"context_reused",
            "bytes":0,"existing_bytes":bytes,"elapsed_ms":started.elapsed().as_secs_f64()*1000.0,
            "phase":"source_read"}));
    }
    // A genuinely new source-read delivery after compaction must not collide
    // with the same turn's durable append retry key for the evicted notice.
    // Retrying this invocation still uses the same identity.
    let item = ResponseItem::Message { id:Some(codex_protocol::ResponseItemId::with_suffix("evidence", &invocation.call_id)), role:"developer".into(),
        content:vec![ContentItem::InputText { text }], phase:None,
        internal_chat_message_metadata_passthrough:None };
    let status = if matches!(invocation.source, ToolCallSource::Direct) {
        turn.queue_post_tool_contexts(&invocation.call_id, vec![item]).await;
        "context_queued"
    } else if invocation.session.record_conversation_items_ordered(turn, &[item]).await.is_ok() {
        "context_recorded"
    } else {
        let mut state = ledger.0.lock().await;
        for key in reservations { state.shown.remove(&key); }
        state.bytes = state.bytes.saturating_sub(bytes);
        "context_unavailable"
    };
    tracing::debug!(advisory_id = %id, status, call_id = %invocation.call_id, "evidence advisory context publication");
    {
        let mut state = ledger.0.lock().await;
        remember_notice(&mut state, id.clone(), payload, status, invocation,
            &environment.environment_id);
    }
    Some(json!({"schema_version":1,"id":id,"status":status,"bytes":bytes,
        "elapsed_ms":started.elapsed().as_secs_f64()*1000.0,"phase":"source_read"}))
}

/// Post-commit assessment shares the read ledger and existing patch receipt.
/// It runs only after the mutation gate is released and never executes evidence.
pub(super) async fn changed_behavior(
    req: &ApplyPatchRequest, turn: &TurnContext, changes: Option<Vec<Change>>,
) -> Vec<Value> {
    if req.turn_environment.environment.is_remote()
        || !turn.file_system_sandbox_policy().has_full_disk_read_access() { return Vec::new(); }
    let Some(changes) = changes else { return vec![json!({
        "kind":"regression_assessment_skipped", "advisory_only":true,
        "reason":"inexact_delta_or_256KiB_eight_change_limit", "verification":"unknown"})]; };
    let fs = req.turn_environment.environment.get_filesystem();
    let sandbox = turn.file_system_sandbox_context(None, req.turn_environment.cwd());
    let ledger = turn.extension_data.get_or_init(EvidenceMemory::default);
    let mut result = Vec::new();
    // Independent of advisory display/deduplication budgets. Lost observations
    // stay explicit; KDR also reconciles scopes for shell/external writes.
    for change in &changes {
        match tokio::time::timeout(Duration::from_millis(100), record_ledger_overlap(req, turn, change)).await {
            Ok(Some(observation)) => result.push(observation),
            Err(_) => result.push(json!({"kind":"fix_ledger_overlap","advisory_only":true,"persistence":"unknown_timeout"})),
            _ => {}
        }
    }
    let completed = tokio::time::timeout(Duration::from_millis(50), async {
        let mut state = ledger.0.lock().await;
        state.budget_exhausted = false;
        for change in &changes {
            if req.cancellation_token.is_cancelled() { break; }
            if let Some((text, _, id, _)) = lookup(&mut state, fs.as_ref(), &change.before_path,
                &sandbox, None, Some(change)).await {
                result.push(json!({"kind":"previous_fix_conflict", "advisory_only":true,
                    "id":id, "context":text, "verification_executed":false}));
            }
        }
        if state.budget_exhausted { result.push(json!({"kind":"regression_assessment_skipped",
            "advisory_only":true,"reason":"bounded_notice_or_turn_budget_exhausted","verification":"unknown"})); }
    }).await;
    if completed.is_err() { result.push(json!({"kind":"regression_assessment_skipped",
        "advisory_only":true,"reason":"optional_50ms_budget_exhausted","verification":"unknown"})); }
    result
}

/// Optional self-report and independently observed provenance, captured before
/// dedicated patch dispatch. No filesystem access, gate or extra model request.
pub(super) async fn before_patch(invocation: &ToolInvocation, raw: Option<&str>) -> Value {
    let mut result = json!({"kind":"prior_evidence_review", "schema_version":1,
        "advisory_only":true, "status":"not_reported", "consideration":"unknown",
        "observation_boundary":"before_patch_dispatch", "provider_delivery":"not_measured",
        "applicability":"agent_reported_not_semantically_verified",
        "coverage":"same-turn local full-read read_file snapshots only; other tools, prior turns and omitted reads remain unknown"});
    let Some(raw) = raw else { return result; };
    result["status"] = json!("invalid_report");
    if raw.len() > 4096 {
        result["reason"] = json!("review_metadata_exceeds_4096_bytes");
        return result;
    }
    let Ok(report) = serde_json::from_str::<Value>(raw) else {
        result["reason"] = json!("review_metadata_is_not_json");
        return result;
    };
    let Some(id) = report["advisory_id"].as_str().filter(|id| !id.is_empty() && id.len() <= 128) else {
        result["reason"] = json!("bounded_advisory_id_required");
        return result;
    };
    let Some(assessments) = report["assessments"].as_array().filter(|rows| !rows.is_empty() && rows.len() <= 8) else {
        result["reason"] = json!("one_to_eight_assessments_required");
        return result;
    };
    let mut seen = HashSet::new();
    let valid = assessments.iter().all(|row| {
        row["fix"].as_str().is_some_and(|fix| !fix.is_empty() && fix.len() <= 128 && seen.insert(fix))
            && matches!(row["decision"].as_str(), Some("preserve" | "modify" | "reject"))
            && row["rationale"].as_str().is_some_and(|reason| !reason.trim().is_empty() && reason.len() <= 1024)
    });
    if !valid {
        result["reason"] = json!("distinct_fix_ids_decisions_and_nonempty_bounded_rationales_required");
        return result;
    }
    result["status"] = json!("reported");
    result["consideration"] = json!("agent_reported_not_independently_proven");
    result["advisory_id"] = json!(id);
    result["context_observation"] = json!("not_observed");
    result["assessments"] = json!(assessments.iter().map(|row| json!({
        "fix":row["fix"], "decision":row["decision"], "rationale":row["rationale"],
        "registered_fix":"not_observed", "native_receipt_reads":[],
    })).collect::<Vec<_>>());
    let ledger = invocation.step_context.turn.extension_data.get_or_init(EvidenceMemory::default);
    let observed = tokio::time::timeout(Duration::from_millis(50), async {
        let state = ledger.0.lock().await;
        result["read_observations_omitted"] = json!(state.reads_omitted);
        let Some(notice) = state.notices.get(id) else { return; };
        result["context_observation"] = notice["status"].clone();
        result["context_read_call_id"] = notice["call_id"].clone();
        let payload = &notice["payload"];
        result["index"] = payload["index"].clone();
        result["index_sha256"] = payload["index_sha256"].clone();
        // Resolve against the delivered advisory, not the current root view:
        // that older observation stays attributable after a refresh.
        let Some(directory) = payload["index"].as_str()
            .and_then(|uri| PathUri::parse(uri).ok())
            .and_then(|path| path.parent())
        else { return; };
        for row in result["assessments"].as_array_mut().into_iter().flatten() {
            let fix = row["fix"].clone();
            row["advisory_fix_observed"] = json!(payload["fixes"].as_array().into_iter().flatten()
                .any(|entry| entry["fix"] == fix));
            let Some(selected) = payload["fixes"].as_array().into_iter().flatten()
                .find(|entry| entry["fix"] == fix) else { continue; };
            row["registered_fix"] = json!("in_selected_advisory_not_fresh_integrity_check");
            let receipts: Vec<&Value> = selected["receipts"].as_array().into_iter().flatten().collect();
            row["receipts_omitted"] = json!(receipts.len().saturating_sub(4));
            row["native_receipt_reads"] = json!(receipts.into_iter().take(4).map(|receipt| {
                let path = receipt["path"].as_str().and_then(|path| directory.join(path).ok());
                let read = path.as_ref().and_then(|path| state.reads.iter().rev().find(|read|
                    read["environment_id"] == notice["environment_id"]
                        && read["source_sha256"] == receipt["sha256"]
                        && read["path"].as_str().is_some_and(|read| same_path(read, &path.to_string()))));
                json!({"sha256":receipt["sha256"], "path":receipt["path"],
                    "status":if read.is_some() { "native_read_observed" } else { "not_observed" },
                    "observation":read})
            }).collect::<Vec<_>>());
        }
    }).await;
    if observed.is_err() {
        result["observation_status"] = json!("unknown_optional_50ms_budget_exhausted");
    }
    // Canonical input remains bound to the durable operation. Keep even a
    // valid but verbose diagnostic bounded without hiding omitted assessments.
    let mut omitted = 0;
    while result.to_string().len() > 16 * 1024 {
        if result["assessments"].as_array_mut().and_then(Vec::pop).is_none() { break; }
        omitted += 1;
        result["assessments_omitted"] = json!(omitted);
    }
    result
}
