//! On-demand reconciliation. Durable observations are not current control grants.
use super::Session;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RolloutItem;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::Path;

pub(crate) const LOCATOR: &str = "context:resume-state";

fn checkpoint_records(item: &ResponseItem, index: usize, records: &mut BTreeMap<String, Value>) {
    let Some(inventory) = crate::unified_exec::process_inventory::recorded_inventory(item) else {
        return;
    };
    for entry in inventory["entries"].as_array().into_iter().flatten() {
        let Some(id) = entry["session_id"].as_u64() else {
            continue;
        };
        let incarnation = entry["incarnation"].as_str().unwrap_or("unknown");
        let key = format!("checkpoint:{id}:{incarnation}");
        if entry["status"] != "exited"
            && records.get(&key).is_some_and(|record| record["terminal_receipt"].is_object())
        {
            continue;
        }
        records.insert(key, json!({
            "kind": "durable_process_checkpoint", "rollout_item_index": index,
            "session_id": id, "incarnation": entry["incarnation"], "call_id": entry["call_id"],
            "recorded_status": entry["status"],
            "terminal_receipt": (entry["status"] == "exited").then(|| json!({
                "source": "durable_process_checkpoint", "exit_code": entry["exit_code"],
                "raw_output_artifact_id": entry["raw_output_artifact_id"], "output_drained": null,
            })),
            "raw_output_artifact_id": entry["raw_output_artifact_id"],
        }));
    }
}

fn command_records(items: &[RolloutItem]) -> BTreeMap<String, Value> {
    let mut records = BTreeMap::new();
    for (index, item) in items.iter().enumerate() {
        match item {
            RolloutItem::ResponseItem(item) => checkpoint_records(item, index, &mut records),
            RolloutItem::Compacted(item) => {
                for item in item.replacement_history.iter().flatten() {
                    checkpoint_records(item, index, &mut records);
                }
            }
            _ => {}
        }
        let item = match item {
            RolloutItem::EventMsg(EventMsg::ItemStarted(event)) => &event.item,
            RolloutItem::EventMsg(EventMsg::ItemCompleted(event)) => &event.item,
            _ => continue,
        };
        let TurnItem::CommandExecution(command) = item else {
            continue;
        };
        let metadata = command.output_metadata.as_ref();
        let key = format!("command:{}:{}:{}", command.id,
            command.process_id.as_deref().unwrap_or("unknown"),
            metadata.and_then(|metadata| metadata.incarnation.as_deref()).unwrap_or("unknown"));
        let previous = records.get(&key).and_then(|record: &Value| record.get("terminal_receipt"))
            .filter(|receipt| receipt.is_object()).cloned();
        if previous.is_some() && metadata.is_none_or(|metadata| !metadata.process_exited) {
            continue;
        }
        records.insert(key, json!({
            "kind": "durable_command", "call_id": command.id,
            "rollout_item_index": index, "execution_id": command.execution_id,
            "parent_call_id": command.parent_call_id, "parent_cell_id": command.parent_cell_id,
            "environment": command.environment,
            "session_id": command.process_id.as_deref().and_then(|id| id.parse::<u32>().ok()),
            "incarnation": metadata.and_then(|metadata| metadata.incarnation.as_deref()),
            "recorded_status": command.status,
            "terminal_receipt": metadata.filter(|metadata| metadata.process_exited).map(|metadata|
                json!({"source": "durable_command_lifecycle", "exit_code": command.exit_code,
                    "output_drained": metadata.output_drained,
                    "search_no_match": metadata.search_no_match,
                    "failure_cause": metadata.failure_cause,
                    "raw_output_artifact_id": metadata.raw_output_artifact_id})).or(previous),
            "raw_output_artifact_id": metadata.and_then(|metadata| metadata.raw_output_artifact_id.as_deref()),
        }));
    }
    records
}

fn reconcile(record: &mut Value, owners: &[Value]) {
    let owner = record["session_id"].as_u64().and_then(|id| {
        owners
            .iter()
            .find(|owner| owner["session_id"].as_u64() == Some(id))
    });
    let expected = record["incarnation"]
        .as_str()
        .and_then(|id| uuid::Uuid::parse_str(id).ok());
    let observed = owner
        .and_then(|owner| owner["incarnation"].as_str())
        .and_then(|id| uuid::Uuid::parse_str(id).ok());
    let verification = match (expected, observed) {
        (Some(expected), Some(observed)) if expected == observed => "matched",
        (Some(_), Some(_)) => "stale_incarnation_rejected",
        (None, _) => "recorded_incarnation_unavailable",
        _ => "owner_unavailable",
    };
    record["owner_verification"] = verification.into();
    record["current_status"] = if verification == "matched" {
        owner
            .map(|owner| owner["status"].clone())
            .unwrap_or(Value::Null)
    } else {
        Value::Null
    };
    record["outcome"] = if record["terminal_receipt"].is_object() {
        "durable_terminal_receipt"
    } else if record["current_status"] == "exited" {
        "owner_observed_exit"
    } else if record["current_status"] == "running" {
        "verified_live"
    } else {
        "unresolved"
    }
    .into();
}

impl Session {
    /// Historical facts only; this never reconstructs control rights.
    pub(crate) async fn retained_process_result(
        &self,
        process_id: u32,
        incarnation: uuid::Uuid,
    ) -> Option<Value> {
        if let Some(result) = self.services.command_execution
            .completed_process_result(process_id, incarnation).await
        {
            return Some(result);
        }
        let records = self.durable_process_records().await?;
        Self::durable_process_result(&records, process_id, incarnation)
    }

    /// A grouped wait shares this single read/parse instead of reloading the
    /// same durable rollout once for every retired handle.
    pub(crate) async fn durable_process_records(&self) -> Option<std::sync::Arc<Vec<Value>>> {
        let path = self.current_rollout_path().await.ok()??;
        let (items, _, _) = codex_rollout::RolloutRecorder::load_rollout_items(&path).await.ok()?;
        tokio::task::spawn_blocking(move || std::sync::Arc::new(
            command_records(&items).into_values()
                .filter(|record| record["terminal_receipt"].is_object()).collect()
        )).await.ok()
    }

    pub(crate) fn durable_process_result(
        records: &[Value],
        process_id: u32,
        incarnation: uuid::Uuid,
    ) -> Option<Value> {
        let record = records.iter().filter(|record| {
            record["session_id"].as_u64() == Some(u64::from(process_id))
                && record["incarnation"].as_str().and_then(|id| uuid::Uuid::parse_str(id).ok()) == Some(incarnation)
        }).max_by_key(|record| (
            record["terminal_receipt"]["source"] == "durable_command_lifecycle",
            record["rollout_item_index"].as_u64(),
        ))?;
        let receipt = &record["terminal_receipt"];
        let detailed = receipt["source"] == "durable_command_lifecycle";
        let success = (detailed && (receipt["exit_code"].is_i64() || receipt["failure_cause"].is_string()))
            .then(|| receipt["failure_cause"].is_null()
            && (receipt["exit_code"] == 0 || receipt["search_no_match"] == true));
        let artifact_id = receipt["raw_output_artifact_id"].clone();
        let mut result = json!({
            "completion_recovered": true, "polled_session_id": process_id,
            "incarnation": incarnation,
            "completion_provenance": {
                "source": receipt["source"], "rollout_item_index": record["rollout_item_index"],
                "call_id": record["call_id"], "execution_id": record["execution_id"],
                "parent_call_id": record["parent_call_id"], "parent_cell_id": record["parent_cell_id"],
            },
            "execution_state": "exited", "process_exited": true,
            "exit_code": receipt["exit_code"], "output_drained": receipt["output_drained"],
            "success": success, "search_no_match": receipt["search_no_match"] == true,
            "error": receipt["failure_cause"], "terminal_facts_available": detailed,
            "output": "", "output_complete": false, "output_reduced": true,
            "raw_output_artifact_retention_limit_hit": null, "retained_artifact_complete": false,
            "raw_output_artifact_id": artifact_id,
            "output_reference": {
                "owner_thread_id": null, "artifact_id": artifact_id,
                "capture_complete": false, "missing_bytes": null, "retained_bytes": null,
                "retrieval_availability": "unknown",
                "missing_bytes_reason": "Historical exit/output-drain evidence does not establish complete retained capture or current artifact availability.",
            },
            "notice": "Durable terminal evidence reused; no live handle was queried. Recover output through the original artifact owner; exit does not prove complete capture or delivery.",
        });
        if let Some(id) = artifact_id.as_str() {
            let recovery = json!({"tool": "read_tool_output", "arguments": {
                "artifact_id": id, "selectors": [{"kind": "tail", "lines": 200, "max_bytes": 4096}]
            }});
            result["output_reference"]["recovery"] = recovery.clone();
            result["recovery"] = recovery;
        } else {
            result.as_object_mut()?.remove("raw_output_artifact_id");
        }
        Some(result)
    }

    /// Reuses the durable rollout, process store and artifact recovery owners.
    /// No collectors, interaction gates, process controls or launches are used.
    pub(crate) async fn read_resume_inventory(
        &self,
        home: &Path,
        active_turn: &str,
    ) -> Result<String, String> {
        let path = self
            .current_rollout_path()
            .await
            .map_err(|error| error.to_string())?
            .ok_or("resume inventory requires a durable session rollout")?;
        let (items, _, parse_errors) = codex_rollout::RolloutRecorder::load_rollout_items(&path)
            .await
            .map_err(|error| error.to_string())?;
        let active_turn = active_turn.to_string();
        let (mut records, unresolved_invocations) = tokio::task::spawn_blocking(move || {
            let records = command_records(&items);
            // In-flight calls in this turn still have their original owner.
            // Do not classify the inventory call itself as a lost result.
            let boundary = items.iter().position(|item| matches!(item,
                RolloutItem::TurnContext(context) if context.turn_id.as_deref() == Some(&active_turn)))
                .unwrap_or(items.len());
            let unresolved = super::rollout_reconstruction::unsettled_tool_recovery_entries(&items[..boundary])
                .collect::<Vec<_>>();
            (records, unresolved)
        }).await.map_err(|error| error.to_string())?;

        // Release each owner before consulting the next. This is explicitly a
        // series of point-in-time observations, not a cross-owner transaction.
        let mut owners = self
            .services
            .unified_exec_manager
            .process_inventory_entries()
            .await;
        for owner in &mut owners {
            if let Some(fields) = owner.as_object_mut() {
                fields.remove("command_preview");
                fields.remove("command_preview_truncated");
            }
            owner["verified_live"] =
                json!(owner["status"] == "running" && owner["incarnation"].is_string());
        }
        for record in records.values_mut() {
            reconcile(record, &owners);
        }
        let history = self.clone_history().await;
        let mut references = history
            .tool_history_state_ref()
            .artifact_references()
            .into_iter()
            .map(|(id, (bytes, sha))| (id, Some((bytes, sha))))
            .collect::<BTreeMap<_, _>>();
        for record in records.values().chain(owners.iter()) {
            if let Some(id) = record["raw_output_artifact_id"].as_str() {
                references.entry(id.to_string()).or_insert(None);
            }
        }
        let mut evidence = Vec::new();
        for (id, expected) in references {
            let (mut audit, _) =
                crate::tools::command_output_artifact::audit_tool_output_reference(
                    home,
                    &self.thread_id.to_string(),
                    &id,
                    16_384,
                )
                .await;
            audit["recorded_reference"] = history
                .tool_history_state_ref()
                .retention_recovery_obligation(&id);
            audit["identity_matches_record"] = expected
                .map(|(bytes, sha)| {
                    json!(
                        audit["identity"]["canonical_bytes"].as_u64() == Some(bytes)
                            && audit["identity"]["canonical_sha256"].as_str() == Some(sha.as_str())
                    )
                })
                .unwrap_or(Value::Null);
            evidence.push(audit);
        }
        let records = records.into_values().collect::<Vec<_>>();
        let inventory = json!({
            "kind": "resume_state_inventory", "version": 1,
            "observed_at": chrono::Utc::now().to_rfc3339(),
            "scope": "retained durable command lifecycle/process checkpoints and unmatched prior-turn invocations, current process owner, and history/command artifact references; omitted historical records and unrecorded effects remain unknown",
            "observation": "non_atomic_point_in_time; no output consumed, content examined, control granted, termination inferred or work restarted",
            "rollout_parse_errors": parse_errors, "rollout_parse_complete": parse_errors == 0,
            "counts": {"durable_operations": records.len(),
                "durable_commands": records.iter().filter(|record| record["kind"] == "durable_command").count(),
                "verified_live_handles": owners.iter().filter(|owner| owner["verified_live"] == true).count(),
                "durable_terminal_receipts": records.iter().filter(|record| record["terminal_receipt"].is_object()).count(),
                "unresolved_commands": records.iter().filter(|record| record["outcome"] == "unresolved").count(),
                "unresolved_invocations": unresolved_invocations.len(), "evidence_references": evidence.len()},
            "operations": records, "current_process_owner": owners,
            "retained_evidence": evidence, "unresolved_invocations": unresolved_invocations,
            "recovery": "Use original owner-issued handles/capabilities and retained evidence recipes. Missing/stale ownership is not exit, failure, non-execution or permission to retry. Live Code Mode cells are not reconstructed from script text; use their originating exec/wait owner.",
        });
        serde_json::to_string_pretty(&inventory).map_err(|error| error.to_string())
    }
}
