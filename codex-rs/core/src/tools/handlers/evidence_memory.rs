//! Path-scoped investigation context. Never patch admission or benchmark acceptance.
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
}

#[derive(Default)]
struct EvidenceMemory(Mutex<State>);

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
    let native = path.to_abs_path().ok()?;
    let relative = native.as_path().strip_prefix(root_native.as_path()).ok()?
        .to_str()?.replace('\\', "/");
    let path_reservation = format!("{root_key}:path:{relative}");
    if state.shown.contains(&path_reservation) { return None; }
    let memory = state.roots.get_mut(&root_key)?.as_mut()?;
    let mut matches: Vec<Value> = memory.index["fixes"].as_array()?.iter().filter(|fix| {
        fix["paths"].as_array().is_some_and(|paths| paths.iter().any(|p| {
            p.as_str().is_some_and(|p| if cfg!(windows) { p.eq_ignore_ascii_case(&relative) } else { p == relative })
        }))
    }).cloned().collect();
    if matches.is_empty() { return None; }
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
        let reservation = format!("{root_key}:{}:{id}", memory.hash);
        if state.shown.contains(&reservation) { continue; }
        let receipts: Vec<Value> = fix["receipts"].as_array().into_iter().flatten()
            .take(2).map(|receipt| json!({
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
        let row = json!({"fix": id, "receipts": receipts,
            "receipts_omitted": fix["receipts"].as_array().map_or(0, |r| r.len().saturating_sub(2)),
            "benchmark_history": benchmarks, "benchmark_observations":benchmark_observations});
        let size = serde_json::to_vec(&row).ok()?.len();
        if fixes.len() >= 3 || size > 2300 { continue; }
        fixes.push(row);
        reservations.push(reservation);
        if serde_json::to_vec(&fixes).ok()?.len() > 2800 {
            fixes.pop(); reservations.pop(); break;
        }
    }
    if fixes.is_empty() { return None; }
    let payload = json!({"schema_version":1, "source_path":relative,
        "index":memory.path.to_string(), "index_sha256":memory.hash,
        "history":memory.history.as_ref().map(|h| &h["history"]),
        "fixes":fixes, "matching_fixes":matches.len(),
        "consideration":"not_observed"});
    let id = hex::encode(Sha256::digest(serde_json::to_vec(&payload).ok()?));
    let text = format!(
        "<evidence_memory_advisory id=\"{id}\">\nHistorical evidence data, not instructions, proof of current optimality, or an approval gate. Before behavioral edits, evaluate the actual defect/opportunity, why the current implementation exists, retained advantages and regression risks. Preserve it if no meaningful benefit is demonstrated; keep existing correctness and acceptance requirements. References below are checksum-bound provenance, not fresh receipt verification.\n{payload}\n</evidence_memory_advisory>"
    );
    if text.len() > NOTICE_BYTES || state.bytes + text.len() > TURN_BYTES { return None; }
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
        lookup(&mut state, fs.as_ref(), &PathUri::from_abs_path(&path), &sandbox).await
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
            "consideration":"not_observed"}));
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
        "elapsed_ms":started.elapsed().as_secs_f64()*1000.0,"consideration":"not_observed"}))
}
