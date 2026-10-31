use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

use codex_tools::JsonSchema;
use codex_tools::JsonToolOutput;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;
use serde_json::json;

use crate::FunctionCallError;
use crate::tool_history::ReadStatusQuery;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::checksum_file::checksum_source;
use crate::tools::handlers::parse_arguments;
use crate::tools::handlers::wait_for_tool_environment;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;

pub(crate) struct CheckSnapshotFreshnessHandler;

const MAX_CHECKS: usize = 32;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckInput {
    path: String,
    environment_id: Option<String>,
    source_sha256: Option<String>,
    snapshot_id: Option<String>,
}

#[derive(Clone)]
struct Observation {
    sha256: String,
    bytes_hashed: u64,
    source_identity: String,
    check_started_at: String,
    observed_at: String,
}

// Invocation-local only. Serial acquisition bounds filesystem contention and
// keeps cancellation/task lifetime with the existing checksum owner. Different
// expected identities still share one authorized read of the same bound path.
type Observations = BTreeMap<(String, String), Result<Observation, &'static str>>;

impl CheckInput {
    fn normalize(&mut self) -> Result<(), &'static str> {
        if self.path.trim().is_empty() || self.source_sha256.is_some() == self.snapshot_id.is_some()
        {
            return Err("provide a nonempty path and exactly one of source_sha256 or snapshot_id");
        }
        for hash in [&mut self.source_sha256, &mut self.snapshot_id]
            .into_iter()
            .flatten()
        {
            if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err("snapshot identity must be a 64-character hexadecimal SHA-256");
            }
            hash.make_ascii_lowercase();
        }
        Ok(())
    }
}

impl ToolExecutor<ToolInvocation> for CheckSnapshotFreshnessHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("check_snapshot_freshness")
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        let fields = BTreeMap::from([
            ("path".into(), JsonSchema::string(Some("Original file path in the selected environment.".into()))),
            ("environment_id".into(), JsonSchema::string(Some("Original execution environment; omit for the selected default.".into()))),
            ("source_sha256".into(), JsonSchema::string(Some("Whole raw-file SHA-256, 64 hex characters; not a rendered-text or decoded-representation hash. Mutually exclusive with snapshot_id.".into()))),
            ("snapshot_id".into(), JsonSchema::string(Some("Exact snapshot_id returned by read_status for this path and environment. Missing history remains unknown.".into()))),
        ]);
        let mut properties = fields.clone();
        properties.insert("checks".into(), JsonSchema {
            min_items: Some(1), max_items: Some(MAX_CHECKS as u64),
            ..JsonSchema::array(JsonSchema::object(fields, Some(vec!["path".into()]), Some(false.into())),
                Some("1–32 independent checks, in input order. Mutually exclusive with all single-file fields. Invalid identities return unknown without losing successful siblings.".into()))
        });
        let outcome = json!({
            "type":"object", "additionalProperties":false,
            "properties":{
                "path":{"type":"string"}, "environment_id":{"type":"string"},
                "canonical_uri":{"type":"string"},
                "snapshot_id":{"type":"string"}, "source_sha256":{"type":"string"},
                "status":{"type":"string","enum":["matches","changed","unknown"]},
                "reason":{"type":"string"}, "input_index":{"type":"integer","minimum":0},
                "checked_sha256":{"type":"string"}, "bytes_hashed":{"type":"integer","minimum":0},
                "source_identity":{"type":"string"},
                "check_started_at":{"type":"string"}, "observed_at":{"type":"string"}
            },
            "required":["path","status","reason","check_started_at","observed_at"]
        });
        ToolSpec::Function(ResponsesApiTool {
            name: "check_snapshot_freshness".into(),
            description: "Check current raw file bytes against an existing snapshot identity without redisplaying or retaining content. Supply single-file fields or checks (1–32); each item needs exactly one of source_sha256 or snapshot_id. Independent ordered matches/changed/unknown outcomes share one checksum acquisition per resolved environment/path within this call, even with different expected hashes. Acquisition is serial and bounded, not an atomic workspace snapshot. Source identity and UTC observation intervals are recorded; duplicate results retain the original interval, not a later freshness claim. Reuses checksum_file's authorized revision-guarded streaming SHA-256, including large/binary files: size/mtime alone never prove freshness. Unavailable/denied/unstable sources, missing/ambiguous attribution and unsupported backends remain unknown. HTML snapshot IDs are unknown, as is a mismatching hash that may identify rendered HTML or decoded UTF-16 text rather than raw bytes. Host/Git locators are not current filesystem sources. A match is point-in-time content equality, not acquisition/semantic coverage or a continuing guarantee. Never reruns a producer or refreshes historical snapshots.".into(),
            strict: false, defer_loading: None,
            parameters: JsonSchema::object(properties, None, Some(false.into())),
            output_schema: Some(json!({"oneOf":[outcome, {
                "type":"object","additionalProperties":false,
                "properties":{
                    "results":{"type":"array","minItems":1,"maxItems":MAX_CHECKS,"items":outcome},
                    "coherent_snapshot_verified":{"const":false}
                    ,"checksum_acquisitions":{"type":"integer","minimum":0,"maximum":MAX_CHECKS}
                },
                "required":["results","coherent_snapshot_verified","checksum_acquisitions"]
            }]}).into()),
        })
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let ToolPayload::Function { ref arguments } = invocation.payload else {
                return Err(FunctionCallError::RespondToModel(
                    "check_snapshot_freshness requires function arguments".into(),
                ));
            };
            let mut raw: serde_json::Value = parse_arguments(arguments)?;
            let checks = raw
                .as_object_mut()
                .and_then(|fields| fields.remove("checks"));
            let batch = checks.is_some();
            let inputs = if let Some(checks) = checks {
                let checks = checks
                    .as_array()
                    .filter(|checks| (1..=MAX_CHECKS).contains(&checks.len()))
                    .ok_or_else(|| {
                        FunctionCallError::RespondToModel("checks requires 1–32 items".into())
                    })?;
                if raw.as_object().is_none_or(|fields| !fields.is_empty()) {
                    return Err(FunctionCallError::RespondToModel(
                        "checks cannot be combined with single-file fields".into(),
                    ));
                }
                checks.clone()
            } else {
                vec![raw]
            };
            let mut observations = Observations::new();
            let mut results = Vec::with_capacity(inputs.len());
            for (index, input) in inputs.into_iter().enumerate() {
                let parsed = serde_json::from_value::<CheckInput>(input.clone())
                    .map_err(|_| "invalid_check_fields")
                    .and_then(|mut args| {
                        args.normalize()?;
                        Ok(args)
                    });
                let mut value = match parsed {
                    Ok(args) => observe(&invocation, &args, &mut observations).await,
                    Err(reason) if batch => json!({
                        "path":input["path"].as_str().unwrap_or(""), "status":"unknown", "reason":reason,
                        "check_started_at":chrono::Utc::now().to_rfc3339(),
                        "observed_at":chrono::Utc::now().to_rfc3339(),
                    }),
                    Err(reason) => return Err(FunctionCallError::RespondToModel(reason.into())),
                };
                if batch {
                    value["input_index"] = json!(index);
                }
                results.push(value);
            }
            let value = if batch {
                json!({"results":results,"coherent_snapshot_verified":false,"checksum_acquisitions":observations.len()})
            } else {
                results.remove(0)
            };
            let result = JsonToolOutput::new(value);
            crate::tools::context::ensure_exact_model_delivery(&result, &invocation)?;
            Ok(boxed_tool_output(result))
        })
    }
}

async fn observe(
    invocation: &ToolInvocation,
    args: &CheckInput,
    observations: &mut Observations,
) -> serde_json::Value {
    let mut value = json!({
        "path":args.path, "status":"unknown", "reason":"source_unavailable",
        "check_started_at":chrono::Utc::now().to_rfc3339(),
        "observed_at":chrono::Utc::now().to_rfc3339(),
    });
    if let Some(id) = &args.snapshot_id {
        value["snapshot_id"] = json!(id);
    }
    if let Some(hash) = &args.source_sha256 {
        value["source_sha256"] = json!(hash);
    }
    let result = if invocation.cancellation_token.is_cancelled() {
        Err("cancelled")
    } else if args.path.starts_with("skill:")
        || args.path.starts_with("context:")
        || args.path.starts_with("git:")
    {
        Err("unsupported_source")
    } else {
        check(invocation, args, &mut value, observations).await
    };
    if let Err(reason) = result {
        value["reason"] = json!(reason);
        value["observed_at"] = json!(chrono::Utc::now().to_rfc3339());
    }
    value
}

async fn check(
    invocation: &ToolInvocation,
    args: &CheckInput,
    value: &mut serde_json::Value,
    observations: &mut Observations,
) -> Result<(), &'static str> {
    let environment = wait_for_tool_environment(
        &invocation.step_context.environments,
        args.environment_id.as_deref(),
        &invocation.cancellation_token,
    )
    .await
    .map_err(|_| "environment_unavailable")?
    .ok_or("environment_unavailable")?;
    value["environment_id"] = json!(environment.environment_id);
    let path = environment
        .cwd()
        .join(&args.path)
        .map_err(|_| "source_unavailable")?;
    value["canonical_uri"] = json!(path.to_string());
    let paths = [PathBuf::from(path.inferred_native_path_string())];
    let expected = if let Some(id) = &args.snapshot_id {
        let history = invocation.session.clone_history().await;
        let status = history.read_status_page(
            &paths,
            Some(&environment.environment_id),
            &ReadStatusQuery {
                snapshot_id: Some(id.clone()),
                ..Default::default()
            },
        );
        let snapshot = status["paths"][0]["snapshots"]
            .as_array()
            .filter(|snapshots| snapshots.len() == 1)
            .and_then(|snapshots| snapshots.first())
            .ok_or("snapshot_unavailable")?;
        if snapshot["source_attribution"] != "recorded"
            || snapshot["environment_id"] != environment.environment_id
            || snapshot["canonical_uri"] != path.to_string()
        {
            return Err("source_attribution_unknown");
        }
        if html_path(&args.path) {
            return Err("representation_unavailable");
        }
        snapshot["source_sha256"]
            .as_str()
            .ok_or("snapshot_unavailable")?
            .to_owned()
    } else {
        args.source_sha256.clone().ok_or("snapshot_unavailable")?
    };
    value["source_sha256"] = json!(expected);
    let key = (environment.environment_id.clone(), path.to_string());
    if !observations.contains_key(&key) {
        let started = chrono::Utc::now().to_rfc3339();
        let observation =
            checksum_source(invocation, &args.path, Some(&environment.environment_id))
                .await
                .map(|checksum| Observation {
                    sha256: checksum.sha256,
                    bytes_hashed: checksum.bytes_hashed,
                    source_identity: checksum.source_identity,
                    check_started_at: started,
                    observed_at: chrono::Utc::now().to_rfc3339(),
                })
                .map_err(|failure| failure.reason);
        observations.insert(key.clone(), observation);
    }
    let checksum = observations
        .get(&key)
        .ok_or("source_unavailable")?
        .as_ref()
        .map_err(|reason| *reason)?;
    if invocation.cancellation_token.is_cancelled() {
        return Err("cancelled");
    }
    let matches = checksum.sha256 == expected;
    // Rendered HTML and decoded UTF-16 canonical hashes are not raw-file
    // identities. A mismatch cannot establish a change in those representations.
    if !matches && args.snapshot_id.is_none() {
        if html_path(&args.path) {
            return Err("representation_unavailable");
        }
        let status = invocation.session.clone_history().await.read_status_page(
            &paths,
            Some(&environment.environment_id),
            &ReadStatusQuery {
                canonical_sha256: Some(expected.clone()),
                ..Default::default()
            },
        );
        if status["paths"][0]["snapshots"]
            .as_array()
            .is_some_and(|snapshots| {
                snapshots
                    .iter()
                    .any(|snapshot| snapshot["source_sha256"] != expected)
            })
        {
            return Err("representation_unavailable");
        }
    }
    value["status"] = json!(if matches { "matches" } else { "changed" });
    value["reason"] = json!(if matches {
        "content_hash_matches"
    } else {
        "content_hash_changed"
    });
    value["checked_sha256"] = json!(checksum.sha256);
    value["bytes_hashed"] = json!(checksum.bytes_hashed);
    value["source_identity"] = json!(checksum.source_identity);
    value["check_started_at"] = json!(checksum.check_started_at);
    value["observed_at"] = json!(checksum.observed_at);
    Ok(())
}

/// Default reads of HTML outside the workspace may identify rendered text.
fn html_path(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("html") || extension.eq_ignore_ascii_case("htm")
        })
}

impl CoreToolRuntime for CheckSnapshotFreshnessHandler {
    fn cancellation_cleanup_policy(&self) -> crate::tools::registry::ToolCleanupPolicy {
        crate::tools::registry::ToolCleanupPolicy::InterruptibleRead
    }
}
