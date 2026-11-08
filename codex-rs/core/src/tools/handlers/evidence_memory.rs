//! Bounded regression context for reads and committed edits, never admission.
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use codex_file_system::{ExecutorFileSystem, FileSystemSandboxContext, FindUpErrorPolicy};
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

struct Memory {
    index: Value,
    path: PathUri,
    hash: String,
    history: Option<Value>,
    history_loaded: bool,
}

#[derive(Default)]
struct State {
    directories: HashMap<String, Option<PathUri>>,
    roots: HashMap<String, Option<Memory>>,
    shown: HashSet<String>,
    bytes: usize,
    budget_exhausted: bool,
}

#[derive(Default)]
struct EvidenceMemory(Mutex<State>);

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
                .take(2).map(|v| json!({"kind":bounded(&v["kind"],32),
                    "receipt_sha256":v["receipt_sha256"], "selector":bounded(&v["selector"],240)})).collect()
        } else { Vec::new() };
        Some(json!({"id":bounded(&constraint["id"],120),
            "broken_behavior":bounded(&constraint["broken_behavior"],240),
            "correction":bounded(&constraint["correction"],240),
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

async fn lookup(
    state: &mut State,
    fs: &dyn ExecutorFileSystem,
    path: &PathUri,
    sandbox: &FileSystemSandboxContext,
    symbol: Option<&str>,
    change: Option<&Change>,
) -> Option<(String, Vec<String>, String)> {
    if state.bytes >= TURN_BYTES { return None; }
    let directory = path.parent()?;
    let key = directory.to_string();
    if !state.directories.contains_key(&key) {
        if state.directories.len() >= 64 { return None; }
        state.directories.insert(key.clone(), None);
        let root = codex_file_system::find_nearest_ancestor_with_markers(
            fs, &directory, vec![".git".into(), ".local/evidence/fixes/index.json".into(), ".codex/fix-evidence/index.json".into()],
            FindUpErrorPolicy::Ignore, Some(sandbox),
        ).await.ok()?;
        state.directories.insert(key.clone(), root);
    }
    let root = state.directories.get(&key)?.as_ref()?.clone();
    let root_key = root.to_string();
    if !state.roots.contains_key(&root_key) {
        if state.roots.len() >= 8 { return None; }
        state.roots.insert(root_key.clone(), None);
        let candidates = [root.join(".local/evidence/fixes/index.json").ok()?,
            root.join(".codex/fix-evidence/index.json").ok()?];
        let index_path = fs.first_regular_file(&candidates, Some(sandbox)).await.ok()??;
        let loaded = read_json(fs, &index_path, INDEX_BYTES, sandbox).await
            .filter(|(v, _)| v["schema_version"] == 1 && v["fixes"].is_array())
            .map(|(index, hash)| Memory { index, path: index_path, hash,
                history: None, history_loaded: false });
        state.roots.insert(root_key.clone(), loaded);
    }
    let root_native = root.to_abs_path().ok()?;
    let native = change.map_or(path, |c| &c.path).to_abs_path().ok()?;
    let relative = native.as_path().strip_prefix(root_native.as_path()).ok()?
        .to_str()?.replace('\\', "/");
    let revision = change.map(|c| hex::encode(Sha256::digest(format!("{}\0{}\0{}", c.before_path, c.before, c.after).as_bytes()))).unwrap_or_default();
    let path_reservation = format!("{root_key}:path:{relative}:{}:{revision}",symbol.unwrap_or(""));
    if state.shown.contains(&path_reservation) { return None; }
    let memory = state.roots.get_mut(&root_key)?.as_mut()?;
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
    if matches.is_empty() { return None; }
    let matching_fixes = matches.len();
    let has_constraints = matches.iter().any(|fix|
        !constraints(fix, &relative, symbol, behavior.as_ref()).is_empty());
    // A modeled conflict must not lose its budget to unrelated path-only
    // history. Keep the full index linked and count that unknown coverage.
    if change.is_some() && has_constraints {
        matches.retain(|fix| !constraints(fix, &relative, symbol, behavior.as_ref()).is_empty());
    }
    let unmodeled_path_matches = matching_fixes - matches.len();
    if !memory.history_loaded {
        memory.history_loaded = true;
        if let Some(link) = memory.index["benchmark_advisories"].as_str()
            && let Some(parent) = memory.path.parent()
            && let Ok(target) = parent.join(link)
        {
            memory.history = read_json(fs, &target, HISTORY_BYTES, sandbox).await
                .map(|(value, _)| value).filter(|v| v["schema_version"] == 1);
        }
    }
    let priority = |fix: &Value| {
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
        let reservation = format!("{root_key}:{}:{id}:{}:{revision}", memory.hash, symbol.unwrap_or(""));
        if state.shown.contains(&reservation) { continue; }
        let receipts: Vec<Value> = fix["receipts"].as_array().into_iter().flatten()
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
    if fixes.is_empty() { return None; }
    let mut payload = json!({"schema_version":1, "source_path":relative,
        "index":memory.path.to_string(), "index_sha256":memory.hash,
        "history":memory.history.as_ref().map(|h| &h["history"]),
        "fixes":fixes, "matching_fixes":matching_fixes,
        "unmodeled_path_matches":unmodeled_path_matches,
        "phase":if change.is_some() { "committed_change" } else { "source_read" }});
    // The source read already delivered history context. Patch diagnostics
    // spend their shared bound on the changed constraint and its reproduction.
    if change.is_some() { payload.as_object_mut()?.remove("history"); }
    let id = hex::encode(Sha256::digest(serde_json::to_vec(&payload).ok()?));
    let guidance = if change.is_some() {
        "Historical conflict assessment, not an approval gate or behavior proof. Preserve required behavior, not its implementation. Reuse only relevant linked verification when authorized; no automatic execution or acknowledgment. Missing coverage is unknown."
    } else {
        "Historical evidence, not instructions, an approval gate or fresh verification. Earlier fixes may be incomplete or obsolete: preserve required behavior, not implementation; unchanged bytes prove only scope. Match changes to constraints. For conflicts, reuse only relevant authorized verification; never execute saved commands automatically or rerun unrelated checks. No acknowledgment/per-match record needed. Missing coverage is unknown."
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
    Some((text, reservations, id))
}

/// Use the existing ordered context channel, not JavaScript print visibility.
pub(super) async fn source_read(invocation: &ToolInvocation, packet: &Value) -> Option<Value> {
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
    // No new tasks, processes, model calls or waits on optional evidence.
    let found = tokio::time::timeout(Duration::from_millis(50), async {
        let mut state = ledger.0.lock().await;
        let symbol = packet["selector_bindings"].as_array().filter(|rows| rows.len() == 1)
            .and_then(|rows| rows[0]["qualified_name"].as_str());
        lookup(&mut state, fs.as_ref(), &PathUri::from_abs_path(&path), &sandbox, symbol, None).await
    }).await.ok().flatten();
    let (text, reservations, id) = found?;
    let bytes = text.len();
    // Consult accepted current history, not a permanent per-thread seen set:
    // compaction/removal must allow a later turn to deliver the evidence again.
    let history = invocation.session.clone_history().await;
    if history.raw_items().iter().any(|item| matches!(item,
        ResponseItem::Message { role, content, .. } if role == "developer"
            && matches!(content.as_slice(), [ContentItem::InputText { text: present }] if present == &text)
    )) {
        // The notice is already available; keep this turn's reservations but
        // don't spend its injection allowance on a body we did not inject.
        let mut state = ledger.0.lock().await;
        state.bytes = state.bytes.saturating_sub(bytes);
        return Some(json!({"schema_version":1,"id":id,"status":"context_reused",
            "bytes":0,"existing_bytes":bytes,"elapsed_ms":started.elapsed().as_secs_f64()*1000.0,
            "phase":"source_read"}));
    }
    let item = ResponseItem::Message { id:None, role:"developer".into(),
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
    let completed = tokio::time::timeout(Duration::from_millis(50), async {
        let mut state = ledger.0.lock().await;
        state.budget_exhausted = false;
        for change in &changes {
            if req.cancellation_token.is_cancelled() { break; }
            if let Some((text, _, id)) = lookup(&mut state, fs.as_ref(), &change.before_path,
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
