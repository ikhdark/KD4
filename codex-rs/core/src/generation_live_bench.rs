//! Temporary, opt-in runtime candidates for the integrated live-model benchmark.
//! Default builds do not include this module or its call sites.
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::ResponseItem;
use serde_json::Value;
use serde_json::json;
use std::io::Write;
use std::sync::Arc;

pub(crate) fn active(finding: u32) -> bool {
    std::env::var("KD4_GENERATION_BENCH_CANDIDATE").ok().as_deref()
        == Some(finding.to_string().as_str())
}

// Promoted changes stay enabled in current-head experiments unless that exact
// finding is explicitly selected as the baseline under measurement.
pub(crate) fn production_enabled(finding: u32) -> bool {
    std::env::var("KD4_GENERATION_BENCH_BASELINE").ok().as_deref()
        != Some(finding.to_string().as_str())
}

pub(crate) fn record(finding: u32, kind: &str) {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(path) = std::env::var_os("KD4_GENERATION_BENCH_LOG")
        && let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path)
    {
        let _ = writeln!(file, "{}", json!({"finding":finding,"kind":kind}));
    }
}

pub(crate) fn checkpoint_notes(items: Arc<[ResponseItem]>) -> Arc<[ResponseItem]> {
    if !active(18) { return items; }
    let mut result: Option<Vec<ResponseItem>> = None;
    for (index,item) in items.iter().enumerate() {
        let ResponseItem::FunctionCall {name,call_id,arguments,..} = item else {continue};
        if name != "context_checkpoint" || arguments.len() < 2048 {continue;}
        let Ok(mut args) = serde_json::from_str::<Value>(arguments) else {continue};
        let (Some(summary),Some(active_work),Some(completed)) = (args["summary"].as_str(),args["active_work"].as_str(),args["completed_call_ids"].as_array()) else {continue};
        if summary.trim().len()+active_work.trim().len() < 2048 {continue;}
        let tail = &items[index+1..];
        let Some((output_index,output)) = tail.iter().enumerate().find_map(|(i,item)| {
            let ResponseItem::FunctionCallOutput {call_id:id,output,..} = item else {return None};
            if id != call_id || output.success == Some(false) {return None;}
            let FunctionCallOutputBody::Text(text) = &output.body else {return None};
            Some((i,text))
        }) else {continue};
        let Ok(receipt) = serde_json::from_str::<Value>(output) else {continue};
        if receipt["changed"] != true || receipt["checkpoint_item_persisted"] != true
            || receipt["canonical_history_preserved"] != true {continue;}
        let matches = tail[..output_index].iter().any(|item| {
            let ResponseItem::Message {role,content,..} = item else {return false};
            role == "developer" && content.iter().any(|part| {
                let ContentItem::InputText {text} = part else {return false};
                let Some(body) = text.strip_prefix("<completed_phase_checkpoint>\n").and_then(|s|s.strip_suffix("\n</completed_phase_checkpoint>")) else {return false};
                let Ok(checkpoint) = serde_json::from_str::<Value>(body) else {return false};
                checkpoint["summary"].as_str() == Some(summary.trim())
                    && checkpoint["active_work"].as_str() == Some(active_work.trim())
                    && checkpoint["receipts"].as_object().is_some_and(|receipts| !receipts.is_empty()
                        && receipt["checkpointed_call_count"].as_u64() == Some(receipts.len() as u64)
                        && receipts.keys().all(|id|completed.iter().any(|v|v.as_str()==Some(id))))
            })
        });
        if !matches {continue;}
        args["summary"] = json!("Working notes retained in the following checkpoint.");
        args["active_work"] = json!("");
        let mut replacement = item.clone();
        let ResponseItem::FunctionCall {arguments,..} = &mut replacement else {unreachable!()};
        *arguments = args.to_string();
        if codex_utils_output_truncation::model_token_count(&serde_json::to_string(&replacement).unwrap())
            < codex_utils_output_truncation::model_token_count(&serde_json::to_string(item).unwrap())
        {
            result.get_or_insert_with(||items.to_vec())[index] = replacement;
            record(18,"large_checkpoint_notes_projected");
        }
    }
    result.map_or(items,Arc::from)
}
