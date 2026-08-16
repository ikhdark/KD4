use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

use crate::function_tool::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::ShellCommandHandler;
use crate::tools::handlers::ShellCommandHandlerOptions;
use crate::tools::handlers::parse_arguments;
use crate::tools::handlers::resolve_tool_environment;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_tools::JsonSchema;
use codex_tools::JsonToolOutput;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolOutputOutcome;
use codex_tools::ToolSpec;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;

const MAX_SOURCE_READS: usize = 32;
const MAX_SOURCE_BYTES: usize = 256 * 1024;
const MAX_ARCHITECTURE_RELATIONSHIPS: usize = 32;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LoadSkillArgs {
    locator: String,
}

pub struct LoadSkillHandler;

impl ToolExecutor<ToolInvocation> for LoadSkillHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("load_skill")
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(ResponsesApiTool {
            name: "load_skill".to_string(),
            description: "Load one advertised skill directly by its opaque skill: locator. Prefer this over searching the filesystem for SKILL.md.".to_string(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(
                BTreeMap::from([(
                    "locator".to_string(),
                    JsonSchema::string(Some("Opaque skill locator advertised in the active skill catalog.".to_string())),
                )]),
                Some(vec!["locator".to_string()]),
                Some(false.into()),
            ),
            output_schema: None,
        })
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let ToolPayload::Function { arguments } = &invocation.payload else {
                return Err(FunctionCallError::RespondToModel(
                    "load_skill received unsupported payload".to_string(),
                ));
            };
            let args: LoadSkillArgs = parse_arguments(arguments)?;
            let snapshot = &invocation.turn.turn_skills.snapshot;
            let skill = snapshot
                .resolve_catalog_locator(&args.locator)
                .cloned()
                .ok_or_else(|| {
                    FunctionCallError::RespondToModel(format!(
                        "unknown skill catalog locator `{}`",
                        args.locator
                    ))
                })?;
            let text = snapshot.read_skill_text(&skill).await.map_err(|err| {
                FunctionCallError::RespondToModel(format!(
                    "failed to load skill `{}`: {err}",
                    args.locator
                ))
            })?;
            Ok(boxed_tool_output(JsonToolOutput::new(json!({
                "locator": args.locator,
                "name": skill.name,
                "path": skill.path_to_skills_md,
                "text": text,
            }))))
        })
    }
}

impl CoreToolRuntime for LoadSkillHandler {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadSourceBatchArgs {
    reads: Vec<SourceRead>,
    #[serde(default)]
    environment_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceRead {
    path: String,
    #[serde(default)]
    start_line: Option<usize>,
    #[serde(default)]
    end_line: Option<usize>,
    /// Hash returned by an earlier read of the same selected projection. When
    /// it still matches, return only the receipt instead of replaying text.
    #[serde(default)]
    known_sha256: Option<String>,
}

#[derive(Debug, Serialize)]
struct SourceReadResult {
    path: String,
    start_line: usize,
    end_line: usize,
    total_lines: usize,
    sha256: String,
    content_identity: String,
    unchanged: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
}

pub struct ReadSourceBatchHandler;

impl ToolExecutor<ToolInvocation> for ReadSourceBatchHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("read_source_batch")
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        let read = JsonSchema::object(
            BTreeMap::from([
                (
                    "path".to_string(),
                    JsonSchema::string(Some(
                        "File path relative to the selected environment cwd, or an absolute path."
                            .to_string(),
                    )),
                ),
                (
                    "start_line".to_string(),
                    JsonSchema::integer(Some("Optional one-based first line.".to_string())),
                ),
                (
                    "end_line".to_string(),
                    JsonSchema::integer(Some("Optional inclusive last line.".to_string())),
                ),
                (
                    "known_sha256".to_string(),
                    JsonSchema::string(Some(
                        "Optional sha256 returned for this exact path and line projection. Matching content is acknowledged without replaying text."
                            .to_string(),
                    )),
                ),
            ]),
            Some(vec!["path".to_string()]),
            Some(false.into()),
        );
        ToolSpec::Function(ResponsesApiTool {
            name: "read_source_batch".to_string(),
            description: format!(
                "Read up to {MAX_SOURCE_READS} source files or bounded line ranges in one filesystem operation batch. Each result includes a stable task-local content_identity and sha256 for cache reuse. Prefer this to launching Python, PowerShell, cat, sed, or repeated single-file reads. The aggregate response is capped at {MAX_SOURCE_BYTES} UTF-8 bytes."
            ),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(
                BTreeMap::from([
                    (
                        "reads".to_string(),
                        JsonSchema::array(
                            read,
                            Some("Independent source reads to execute concurrently.".to_string()),
                        ),
                    ),
                    (
                        "environment_id".to_string(),
                        JsonSchema::string(Some(
                            "Optional environment id; omit for the primary environment."
                                .to_string(),
                        )),
                    ),
                ]),
                Some(vec!["reads".to_string()]),
                Some(false.into()),
            ),
            output_schema: None,
        })
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let ToolPayload::Function { arguments } = &invocation.payload else {
                return Err(FunctionCallError::RespondToModel(
                    "read_source_batch received unsupported payload".to_string(),
                ));
            };
            let args: ReadSourceBatchArgs = parse_arguments(arguments)?;
            if args.reads.is_empty() || args.reads.len() > MAX_SOURCE_READS {
                return Err(FunctionCallError::RespondToModel(format!(
                    "reads must contain between 1 and {MAX_SOURCE_READS} entries"
                )));
            }
            let Some(environment) = resolve_tool_environment(
                &invocation.step_context.environments,
                args.environment_id.as_deref(),
            )?
            else {
                return Err(FunctionCallError::RespondToModel(
                    "read_source_batch is unavailable without a filesystem environment".to_string(),
                ));
            };
            let sandbox = invocation
                .turn
                .file_system_sandbox_context(None, environment.cwd());
            let fs = environment.environment.get_filesystem();
            let cwd = environment.cwd().clone();
            let task_identity = invocation.session.thread_id.to_string();
            let futures = args.reads.into_iter().map(|read| {
                let fs = fs.clone();
                let sandbox = sandbox.clone();
                let cwd = cwd.clone();
                let task_identity = task_identity.clone();
                async move {
                    let path_uri = cwd.join(&read.path).map_err(|err| {
                        FunctionCallError::RespondToModel(format!(
                            "unable to resolve source path `{}`: {err}",
                            read.path
                        ))
                    })?;
                    let text =
                        fs.read_file_text(&path_uri, Some(&sandbox))
                            .await
                            .map_err(|err| {
                                FunctionCallError::RespondToModel(format!(
                                    "unable to read source `{}`: {err}",
                                    read.path
                                ))
                            })?;
                    select_lines(read, text, &task_identity)
                }
            });
            let results = futures::future::try_join_all(futures).await?;
            let total_bytes = results
                .iter()
                .map(|result| result.text.as_deref().map_or(0, str::len))
                .sum::<usize>();
            if total_bytes > MAX_SOURCE_BYTES {
                return Err(FunctionCallError::RespondToModel(format!(
                    "batched source result is {total_bytes} bytes; narrow the reads below the {MAX_SOURCE_BYTES}-byte aggregate limit"
                )));
            }
            Ok(boxed_tool_output(JsonToolOutput::new(json!({
                "total_bytes": total_bytes,
                "reads": results,
            }))))
        })
    }
}

impl CoreToolRuntime for ReadSourceBatchHandler {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArchitectureSliceArgs {
    owners: Vec<String>,
    #[serde(default = "default_architecture_relationship_limit")]
    max_relationships: usize,
    #[serde(default)]
    focus: Option<String>,
    #[serde(default)]
    index_path: Option<String>,
    #[serde(default)]
    environment_id: Option<String>,
}

const fn default_architecture_relationship_limit() -> usize {
    MAX_ARCHITECTURE_RELATIONSHIPS
}

pub struct ArchitectureSliceHandler;

impl ToolExecutor<ToolInvocation> for ArchitectureSliceHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("architecture_slice")
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(ResponsesApiTool {
            name: "architecture_slice".to_string(),
            description: format!(
                "Return a bounded, update_plan-compatible source-closure slice from the generated architecture index. Use this before the first edit to surface owner fanout across callers, configuration, registration, tests, generated artifacts, and invariants. Inspect exact source before mutation. At most {MAX_ARCHITECTURE_RELATIONSHIPS} relationships may be returned."
            ),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(
                BTreeMap::from([
                    (
                        "owners".to_string(),
                        JsonSchema::array(
                            JsonSchema::string(None),
                            Some("One or more exact source-owner ids from SOURCEMAP.md.".to_string()),
                        ),
                    ),
                    (
                        "max_relationships".to_string(),
                        JsonSchema::integer(Some(format!(
                            "Optional relationship budget from 1 to {MAX_ARCHITECTURE_RELATIONSHIPS}."
                        ))),
                    ),
                    (
                        "focus".to_string(),
                        JsonSchema::string(Some(
                            "Optional task description used to rank relationships within each architecture facet.".to_string(),
                        )),
                    ),
                    (
                        "index_path".to_string(),
                        JsonSchema::string(Some(
                            "Generated architecture-index path relative to the selected environment cwd; defaults to architecture_index.json.".to_string(),
                        )),
                    ),
                    (
                        "environment_id".to_string(),
                        JsonSchema::string(Some(
                            "Optional environment id; omit for the primary environment.".to_string(),
                        )),
                    ),
                ]),
                Some(vec!["owners".to_string()]),
                Some(false.into()),
            ),
            output_schema: None,
        })
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let ToolPayload::Function { arguments } = &invocation.payload else {
                return Err(FunctionCallError::RespondToModel(
                    "architecture_slice received unsupported payload".to_string(),
                ));
            };
            let args: ArchitectureSliceArgs = parse_arguments(arguments)?;
            if args.owners.is_empty() {
                return Err(FunctionCallError::RespondToModel(
                    "owners must contain at least one source-owner id".to_string(),
                ));
            }
            if !(1..=MAX_ARCHITECTURE_RELATIONSHIPS).contains(&args.max_relationships) {
                return Err(FunctionCallError::RespondToModel(format!(
                    "max_relationships must be between 1 and {MAX_ARCHITECTURE_RELATIONSHIPS}"
                )));
            }
            let Some(environment) = resolve_tool_environment(
                &invocation.step_context.environments,
                args.environment_id.as_deref(),
            )?
            else {
                return Err(FunctionCallError::RespondToModel(
                    "architecture_slice is unavailable without a filesystem environment"
                        .to_string(),
                ));
            };
            let sandbox = invocation
                .turn
                .file_system_sandbox_context(None, environment.cwd());
            let index_path = args
                .index_path
                .as_deref()
                .unwrap_or("architecture_index.json");
            let path_uri = environment.cwd().join(index_path).map_err(|err| {
                FunctionCallError::RespondToModel(format!(
                    "unable to resolve architecture index `{index_path}`: {err}"
                ))
            })?;
            let text = environment
                .environment
                .get_filesystem()
                .read_file_text(&path_uri, Some(&sandbox))
                .await
                .map_err(|err| {
                    FunctionCallError::RespondToModel(format!(
                        "unable to read architecture index `{index_path}`: {err}"
                    ))
                })?;
            let index: Value = serde_json::from_str(&text).map_err(|err| {
                FunctionCallError::RespondToModel(format!(
                    "architecture index `{index_path}` is not valid JSON: {err}"
                ))
            })?;
            let slice = architecture_slice_from_index(
                &index,
                &args.owners,
                args.max_relationships,
                args.focus.as_deref(),
                text.len(),
            )
            .map_err(FunctionCallError::RespondToModel)?;
            Ok(boxed_tool_output(JsonToolOutput::new(slice)))
        })
    }
}

impl CoreToolRuntime for ArchitectureSliceHandler {}

const ARCHITECTURE_FACETS: [&str; 7] = [
    "control_and_data_flow",
    "callers_and_consumers",
    "configuration_and_gates",
    "registration_and_entrypoints",
    "tests_and_contracts",
    "generated_artifacts",
    "invariants",
];

fn architecture_slice_from_index(
    index: &Value,
    requested_owners: &[String],
    max_relationships: usize,
    focus: Option<&str>,
    bytes_read: usize,
) -> Result<Value, String> {
    let owners = index
        .get("owners")
        .and_then(Value::as_array)
        .ok_or_else(|| "architecture index is missing owners".to_string())?;
    let owners_by_id = owners
        .iter()
        .filter_map(|owner| Some((owner.get("id")?.as_str()?, owner)))
        .collect::<BTreeMap<_, _>>();
    let selected_ids = requested_owners
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let unknown = selected_ids
        .iter()
        .filter(|owner| !owners_by_id.contains_key(**owner))
        .copied()
        .collect::<Vec<_>>();
    if !unknown.is_empty() {
        return Err(format!("unknown source-owner ids: {}", unknown.join(", ")));
    }

    let relevant = index
        .get("relationships")
        .and_then(Value::as_array)
        .ok_or_else(|| "architecture index is missing relationships".to_string())?
        .iter()
        .filter(|relationship| {
            ["source", "target"].iter().any(|field| {
                relationship
                    .get(field)
                    .and_then(Value::as_str)
                    .and_then(|value| value.strip_prefix("owner:"))
                    .is_some_and(|owner| selected_ids.contains(owner))
            })
        })
        .collect::<Vec<_>>();

    let mut facets = ARCHITECTURE_FACETS
        .iter()
        .map(|facet| (*facet, Vec::<Value>::new()))
        .collect::<BTreeMap<_, _>>();
    let mut coverage = ARCHITECTURE_FACETS
        .iter()
        .map(|facet| (*facet, BTreeSet::<String>::new()))
        .collect::<BTreeMap<_, _>>();

    for relationship in relevant {
        let category = relationship
            .get("category")
            .and_then(Value::as_str)
            .ok_or_else(|| "architecture relationship is missing category".to_string())?;
        let facet = architecture_facet_for_category(category)
            .ok_or_else(|| format!("unknown architecture relationship category `{category}`"))?;
        for field in ["source", "target"] {
            if let Some(owner) = relationship
                .get(field)
                .and_then(Value::as_str)
                .and_then(|value| value.strip_prefix("owner:"))
                .filter(|owner| selected_ids.contains(owner))
            {
                coverage.entry(facet).or_default().insert(owner.to_string());
            }
        }
        facets.entry(facet).or_default().push(json!({
            "kind": architecture_kind_for_relationship(
                relationship.get("kind").and_then(Value::as_str).unwrap_or_default()
            )?,
            "source": relationship.get("source").cloned().unwrap_or(Value::Null),
            "target": relationship.get("target").cloned().unwrap_or(Value::Null),
            "evidence": architecture_evidence_text(relationship.get("evidence")),
            "provenance": if relationship.get("confidence").and_then(Value::as_str)
                == Some("compiler_resolved") { "exact" } else { "declared" },
        }));
    }

    for owner_id in &selected_ids {
        let owner = owners_by_id[owner_id];
        append_owner_architecture_relationships(owner_id, owner, &mut facets, &mut coverage);
    }

    let focus_tokens = architecture_ranking_tokens(focus);
    for facet in ARCHITECTURE_FACETS {
        let relationships = facets.entry(facet).or_default();
        let mut seen = BTreeSet::new();
        relationships.retain(|relationship| seen.insert(relationship.to_string()));
        relationships.sort_by_key(|relationship| {
            architecture_relationship_rank_key(facet, relationship, &selected_ids, &focus_tokens)
        });
    }

    let relationship_total = facets.values().map(Vec::len).sum::<usize>();
    let omitted_relationships = relationship_total.saturating_sub(max_relationships);
    if omitted_relationships != 0 {
        let mut retained = ARCHITECTURE_FACETS
            .iter()
            .map(|facet| (*facet, Vec::<Value>::new()))
            .collect::<BTreeMap<_, _>>();
        let max_facet_len = facets.values().map(Vec::len).max().unwrap_or_default();
        let mut retained_count = 0;
        for rank in 0..max_facet_len {
            for facet in ARCHITECTURE_FACETS {
                if retained_count == max_relationships {
                    break;
                }
                if let Some(relationship) = facets[facet].get(rank) {
                    retained
                        .entry(facet)
                        .or_default()
                        .push(relationship.clone());
                    retained_count += 1;
                }
            }
        }
        facets = retained;
    }

    let mut material_unknowns = index
        .get("unresolved")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect::<Vec<_>>();
    let mut output = serde_json::Map::new();
    for facet in ARCHITECTURE_FACETS {
        let mut uncovered = Vec::new();
        let mut reasons = Vec::new();
        for owner_id in &selected_ids {
            if coverage[facet].contains(*owner_id) {
                continue;
            }
            let reason = owners_by_id[owner_id]
                .get("facet_exclusions")
                .and_then(|value| value.get(facet))
                .and_then(Value::as_str);
            if let Some(reason) = reason {
                reasons.push(format!("{owner_id}: {reason}"));
            } else {
                uncovered.push(*owner_id);
            }
        }
        if !uncovered.is_empty() {
            material_unknowns.push(format!(
                "{facet}: missing declarations for {}",
                uncovered.join(", ")
            ));
        }
        let relationships = facets.remove(facet).unwrap_or_default();
        let facet_value = if relationships.is_empty() && !reasons.is_empty() && uncovered.is_empty()
        {
            json!({
                "status": "not_applicable",
                "relationships": relationships,
                "not_applicable_reason": reasons.join("; "),
            })
        } else {
            json!({"status": "established", "relationships": relationships})
        };
        output.insert(facet.to_string(), facet_value);
    }

    let base_omissions = index
        .pointer("/omitted/relationships")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let total_omissions = base_omissions + omitted_relationships as u64;
    let snapshot = format!(
        "{}:{}",
        index
            .get("repository_revision")
            .and_then(Value::as_str)
            .unwrap_or("unknown"),
        index
            .get("manifest_sha256")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
    );
    output.insert("snapshot".to_string(), Value::String(snapshot));
    output.insert(
        "truncated".to_string(),
        Value::Bool(
            total_omissions != 0 || index.get("status").and_then(Value::as_str) != Some("complete"),
        ),
    );
    output.insert("omitted_relationships".to_string(), json!(total_omissions));
    output.insert("material_unknowns".to_string(), json!(material_unknowns));
    output.insert(
        "limitations".to_string(),
        json!([
            "Architecture-index relationships are declarative; inspect exact source before mutation.",
            "Relationships are ordered within each facet by facet-specific kind, focus-term overlap, provenance, and selected-owner directness."
        ]),
    );
    output.insert(
        "metrics".to_string(),
        json!({
            "tool_calls": 1,
            "files_read": 1,
            "bytes_read": bytes_read,
            "late_relationship_discoveries": 0,
        }),
    );
    Ok(Value::Object(output))
}

fn architecture_ranking_tokens(value: Option<&str>) -> BTreeSet<String> {
    const STOP_WORDS: [&str; 17] = [
        "a", "an", "and", "be", "by", "for", "from", "in", "instead", "its", "must", "of", "or",
        "own", "the", "to", "with",
    ];
    value
        .unwrap_or_default()
        .split(|character: char| !character.is_ascii_alphanumeric())
        .map(str::to_ascii_lowercase)
        .filter(|token| token.len() > 1 && !STOP_WORDS.contains(&token.as_str()))
        .collect()
}

fn architecture_relationship_rank_key(
    facet: &str,
    relationship: &Value,
    selected_ids: &BTreeSet<&str>,
    focus_tokens: &BTreeSet<String>,
) -> (
    Reverse<u8>,
    Reverse<usize>,
    Reverse<u8>,
    Reverse<usize>,
    String,
) {
    let kind = relationship
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let kind_priority = match (facet, kind) {
        ("control_and_data_flow", "control_flow") => 4,
        ("control_and_data_flow", "data_flow") => 3,
        ("callers_and_consumers", "caller") => 4,
        ("callers_and_consumers", "consumer") => 3,
        ("callers_and_consumers", "direct_builder") => 2,
        ("callers_and_consumers", "control_flow") => 1,
        ("configuration_and_gates", "config_gate") => 4,
        ("configuration_and_gates", "feature_gate") => 3,
        ("configuration_and_gates", "configuration") => 2,
        ("registration_and_entrypoints", "control_flow") => 5,
        ("registration_and_entrypoints", "registration") => 4,
        ("registration_and_entrypoints", "entrypoint") => 3,
        ("tests_and_contracts", "test") => 4,
        ("tests_and_contracts", "contract") => 3,
        ("generated_artifacts", "generated_by") => 4,
        ("generated_artifacts", "generated_consumer") => 3,
        ("invariants", "invariant") => 4,
        _ => 0,
    };
    let searchable = ["kind", "source", "target", "evidence"]
        .iter()
        .filter_map(|field| relationship.get(field).and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join(" ");
    let overlap = architecture_ranking_tokens(Some(&searchable))
        .intersection(focus_tokens)
        .count();
    let selected_endpoint_count = ["source", "target"]
        .iter()
        .filter_map(|field| relationship.get(field).and_then(Value::as_str))
        .filter_map(|endpoint| endpoint.strip_prefix("owner:"))
        .collect::<BTreeSet<_>>()
        .intersection(selected_ids)
        .count();
    let provenance_priority = match relationship.get("provenance").and_then(Value::as_str) {
        Some("exact") => 2,
        Some("declared") => 1,
        _ => 0,
    };
    (
        Reverse(kind_priority),
        Reverse(overlap),
        Reverse(provenance_priority),
        Reverse(selected_endpoint_count),
        ["source", "kind", "target", "evidence"]
            .iter()
            .filter_map(|field| relationship.get(field).and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\0"),
    )
}

fn architecture_facet_for_category(category: &str) -> Option<&'static str> {
    match category {
        "control_flow" => Some("control_and_data_flow"),
        "callers_consumers" => Some("callers_and_consumers"),
        "configuration" => Some("configuration_and_gates"),
        "runtime_registration" => Some("registration_and_entrypoints"),
        "tests_contracts" => Some("tests_and_contracts"),
        "generated_artifacts" => Some("generated_artifacts"),
        _ => None,
    }
}

fn architecture_kind_for_relationship(kind: &str) -> Result<&'static str, String> {
    match kind {
        "calls" | "constructs" => Ok("control_flow"),
        "consumed_by" => Ok("consumer"),
        "emits" | "persists" => Ok("data_flow"),
        "gated_by" => Ok("feature_gate"),
        "generates" => Ok("generated_by"),
        "reads_config" => Ok("config_gate"),
        "registers" => Ok("registration"),
        "validated_by" => Ok("test"),
        _ => Err(format!("unknown architecture relationship kind `{kind}`")),
    }
}

fn architecture_evidence_text(evidence: Option<&Value>) -> String {
    evidence
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let path = item.get("path")?.as_str()?;
            Some(match item.get("symbol").and_then(Value::as_str) {
                Some(symbol) => format!("{path}::{symbol}"),
                None => path.to_string(),
            })
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn append_owner_architecture_relationships(
    owner_id: &str,
    owner: &Value,
    facets: &mut BTreeMap<&str, Vec<Value>>,
    coverage: &mut BTreeMap<&str, BTreeSet<String>>,
) {
    let mut append = |facet: &'static str, relationship: Value| {
        facets.entry(facet).or_default().push(relationship);
        coverage
            .entry(facet)
            .or_default()
            .insert(owner_id.to_string());
    };
    for entry in owner
        .get("primary_entries")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let (Some(path), Some(symbol)) = (
            entry.get("path").and_then(Value::as_str),
            entry.get("symbol").and_then(Value::as_str),
        ) {
            append(
                "registration_and_entrypoints",
                json!({"kind":"entrypoint","source":format!("owner:{owner_id}"),"target":format!("path:{path}::{symbol}"),"evidence":format!("source_owners.toml::{owner_id}.primary_entries"),"provenance":"declared"}),
            );
        }
    }
    for (field, facet, kind, prefix) in [
        ("tests", "tests_and_contracts", "test", "path"),
        (
            "configuration",
            "tests_and_contracts",
            "contract",
            "contract",
        ),
        (
            "generated_artifacts",
            "generated_artifacts",
            "generated_consumer",
            "generated",
        ),
    ] {
        for target in owner
            .get(field)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            append(
                facet,
                json!({"kind":kind,"source":format!("owner:{owner_id}"),"target":format!("{prefix}:{target}"),"evidence":format!("source_owners.toml::{owner_id}.{field}"),"provenance":"declared"}),
            );
        }
    }
    for invariant in owner
        .get("invariants")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(id) = invariant.get("id").and_then(Value::as_str) {
            append(
                "invariants",
                json!({"kind":"invariant","source":format!("owner:{owner_id}"),"target":format!("contract:{id}"),"evidence":architecture_evidence_text(invariant.get("evidence")),"provenance":"declared"}),
            );
        }
    }
}

fn select_lines(
    read: SourceRead,
    text: String,
    task_identity: &str,
) -> Result<SourceReadResult, FunctionCallError> {
    let lines = text.lines().collect::<Vec<_>>();
    let total_lines = lines.len();
    let start = read.start_line.unwrap_or(1);
    let end = read.end_line.unwrap_or(total_lines.max(1));
    if start == 0 || end < start {
        return Err(FunctionCallError::RespondToModel(format!(
            "source read `{}` requires 1-based start_line <= end_line",
            read.path
        )));
    }
    let bounded_end = end.min(total_lines);
    let selected = if start > total_lines {
        String::new()
    } else {
        lines[start - 1..bounded_end].join("\n")
    };
    let sha256 = format!("{:x}", Sha256::digest(selected.as_bytes()));
    let start_identity = start.to_string();
    let end_identity = bounded_end.to_string();
    let total_identity = total_lines.to_string();
    let content_identity = stable_identity(
        "KD4_TASK_LOCAL_SOURCE_READ_V1",
        &[
            task_identity,
            &read.path,
            &start_identity,
            &end_identity,
            &total_identity,
            &sha256,
        ],
    );
    let unchanged = read.known_sha256.as_deref() == Some(sha256.as_str());
    Ok(SourceReadResult {
        path: read.path,
        start_line: start,
        end_line: bounded_end,
        total_lines,
        sha256,
        content_identity,
        unchanged,
        text: (!unchanged).then_some(selected),
    })
}

fn stable_identity(domain: &str, parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(domain.as_bytes());
    for part in parts {
        hasher.update([0]);
        hasher.update(part.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CargoTestArgs {
    #[serde(default)]
    package: Option<String>,
    #[serde(default)]
    test_filter: Option<String>,
    #[serde(default)]
    cargo_args: Vec<String>,
    #[serde(default)]
    harness_args: Vec<String>,
    #[serde(default)]
    workdir: Option<String>,
    #[serde(default = "default_cargo_test_timeout_ms")]
    timeout_ms: u64,
}

const fn default_cargo_test_timeout_ms() -> u64 {
    300_000
}

pub struct CargoTestHandler {
    shell_options: ShellCommandHandlerOptions,
}

impl CargoTestHandler {
    pub(crate) fn new(shell_options: ShellCommandHandlerOptions) -> Self {
        Self { shell_options }
    }
}

impl ToolExecutor<ToolInvocation> for CargoTestHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("cargo_test")
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(ResponsesApiTool {
            name: "cargo_test".to_string(),
            description: "Run a focused Cargo test with explicit evidence. The tool first executes the same selection with --list, reports selected_test_count (and the matched_tests compatibility alias), and returns a normalized failure_signature for unsuccessful or zero-test validation. Prefer this to treating a successful zero-test cargo invocation as validation.".to_string(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(
                BTreeMap::from([
                    ("package".to_string(), JsonSchema::string(Some("Optional Cargo package passed with -p.".to_string()))),
                    ("test_filter".to_string(), JsonSchema::string(Some("Optional libtest name filter.".to_string()))),
                    ("cargo_args".to_string(), JsonSchema::array(JsonSchema::string(None), Some("Additional Cargo arguments placed before the test filter.".to_string()))),
                    ("harness_args".to_string(), JsonSchema::array(JsonSchema::string(None), Some("Additional libtest arguments placed after --.".to_string()))),
                    ("workdir".to_string(), JsonSchema::string(Some("Working directory; defaults to the turn cwd.".to_string()))),
                    ("timeout_ms".to_string(), JsonSchema::integer(Some("Bounded runtime for each phase; defaults to 300000 ms.".to_string()))),
                ]),
                None,
                Some(false.into()),
            ),
            output_schema: None,
        })
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let ToolPayload::Function { arguments } = &invocation.payload else {
                return Err(FunctionCallError::RespondToModel(
                    "cargo_test received unsupported payload".to_string(),
                ));
            };
            let args: CargoTestArgs = parse_arguments(arguments)?;
            let base_args = cargo_test_args(&args, false);
            let probe_args = cargo_test_args(&args, true);
            let shell = ShellCommandHandler::new(self.shell_options);
            let (probe_invocation, probe_payload) =
                cargo_shell_invocation(&invocation, &args, probe_args, "list");
            let probe_output = shell.handle_call(probe_invocation).await?;
            let probe_outcome = probe_output.outcome_context().outcome;
            let probe_result = probe_output.code_mode_result(&probe_payload);
            if probe_outcome != ToolOutputOutcome::Success {
                let failure_signature = normalized_tool_validation_failure(&probe_result, false);
                return Ok(boxed_tool_output(JsonToolOutput::with_success(
                    json!({
                        "execution_outcome": "list_failed",
                        "command_was_executed": false,
                        "matched_tests": 0,
                        "selected_test_count": Value::Null,
                        "failure_signature": failure_signature,
                        "not_exercised": true,
                        "list_command": cargo_command_display(&probe_payload),
                        "list_output": probe_result,
                    }),
                    Some(false),
                )));
            }
            let matched_tests = count_listed_tests(&probe_result);
            if matched_tests == 0 {
                return Ok(boxed_tool_output(JsonToolOutput::skipped_with_disposition(
                    json!({
                        "execution_outcome": "not_executed_no_matching_tests",
                        "command_was_executed": false,
                        "matched_tests": 0,
                        "selected_test_count": 0,
                        "failure_signature": "validation-failure-v1:zero-tests-selected",
                        "not_exercised": true,
                        "list_command": cargo_command_display(&probe_payload),
                        "list_output": probe_result,
                    }),
                    codex_tools::ToolOutputSkipDisposition::NotApplicable,
                )));
            }
            let (run_invocation, run_payload) =
                cargo_shell_invocation(&invocation, &args, base_args, "run");
            let run_output = shell.handle_call(run_invocation).await?;
            let run_outcome = run_output.outcome_context().outcome;
            let success = run_outcome == ToolOutputOutcome::Success;
            let run_result = run_output.code_mode_result(&run_payload);
            let failure_signature =
                (!success).then(|| normalized_tool_validation_failure(&run_result, false));
            Ok(boxed_tool_output(JsonToolOutput::with_success(
                json!({
                    "execution_outcome": if success { "executed_success" } else { "executed_failure" },
                    "command_was_executed": true,
                    "matched_tests": matched_tests,
                    "selected_test_count": matched_tests,
                    "failure_signature": failure_signature,
                    "not_exercised": false,
                    "list_command": cargo_command_display(&probe_payload),
                    "command": cargo_command_display(&run_payload),
                    "output": run_result,
                }),
                Some(success),
            )))
        })
    }
}

fn normalized_tool_validation_failure(output: &Value, zero_tests_selected: bool) -> String {
    let serialized = serde_json::to_vec(output).unwrap_or_default();
    crate::tools::command_execution::normalized_validation_failure_signature(
        1,
        zero_tests_selected,
        &serialized,
    )
    .unwrap_or_default()
}

impl CoreToolRuntime for CargoTestHandler {}

fn cargo_test_args(args: &CargoTestArgs, list: bool) -> Vec<String> {
    let mut result = vec!["test".to_string()];
    if let Some(package) = args.package.as_ref() {
        result.extend(["-p".to_string(), package.clone()]);
    }
    result.extend(args.cargo_args.iter().cloned());
    if let Some(filter) = args.test_filter.as_ref() {
        result.push(filter.clone());
    }
    result.push("--".to_string());
    if list {
        result.push("--list".to_string());
    }
    result.extend(args.harness_args.iter().cloned());
    result
}

fn cargo_shell_invocation(
    invocation: &ToolInvocation,
    args: &CargoTestArgs,
    command_args: Vec<String>,
    phase: &str,
) -> (ToolInvocation, ToolPayload) {
    let payload = ToolPayload::Function {
        arguments: json!({
            "kind": "argv",
            "program": "cargo",
            "args": command_args,
            "workdir": args.workdir,
            "timeout_ms": args.timeout_ms,
        })
        .to_string(),
    };
    let mut nested = invocation.clone();
    nested.call_id = format!("{}:{phase}", invocation.call_id);
    nested.tool_name = ToolName::plain("shell_command");
    nested.payload = payload.clone();
    (nested, payload)
}

fn cargo_command_display(payload: &ToolPayload) -> Value {
    let ToolPayload::Function { arguments } = payload else {
        return Value::Null;
    };
    serde_json::from_str(arguments).unwrap_or(Value::Null)
}

fn count_listed_tests(value: &Value) -> usize {
    fn count_text(text: &str) -> usize {
        text.lines()
            .filter(|line| {
                let line = line.trim();
                line.ends_with(": test") || line.ends_with(": benchmark")
            })
            .count()
    }
    if let Some(output) = value.get("output").and_then(Value::as_str) {
        return count_text(output);
    }
    match value {
        Value::String(text) => count_text(text),
        Value::Array(items) => items.iter().map(count_listed_tests).sum(),
        Value::Object(fields) => fields.values().map(count_listed_tests).sum(),
        _ => 0,
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LintArgs {
    calls: Vec<LintCall>,
    symbol_handles: Vec<AuditSymbolHandle>,
    validation_commands: Vec<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AuditSymbolHandle {
    path: String,
    symbol: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LintCall {
    #[serde(default)]
    id: Option<String>,
    tool: String,
    #[serde(default)]
    arguments: Value,
    #[serde(default)]
    depends_on: Vec<String>,
}

pub struct OrchestrationLintHandler;

impl ToolExecutor<ToolInvocation> for OrchestrationLintHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("lint_tool_calls")
    }
    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }
    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(ResponsesApiTool {
            name: "lint_tool_calls".to_string(),
            description: "Lint a proposed engineering tool-call sequence before execution. Every finding carries a stable finding id, durable source symbol handles, a semantic conclusion, and executable validation commands. Reports duplicate calls, avoidable shell wrapping, unscoped searches, serial polling, and reads that should be batched. Optional stable ids and depends_on edges produce dependency-safe execution waves for functions.exec Promise.all batching.".to_string(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(
                BTreeMap::from([
                    ("calls".to_string(), JsonSchema::array(
                        JsonSchema::object(BTreeMap::from([
                            ("id".to_string(), JsonSchema::string(Some("Optional stable node id used by depends_on edges.".to_string()))),
                            ("tool".to_string(), JsonSchema::string(None)),
                            ("arguments".to_string(), JsonSchema::object(BTreeMap::new(), None, Some(true.into()))),
                            ("depends_on".to_string(), JsonSchema::array(JsonSchema::string(None), Some("Node ids that must finish before this call.".to_string()))),
                        ]), Some(vec!["tool".to_string()]), Some(false.into())),
                        Some("Ordered proposed calls.".to_string()),
                    )),
                    ("symbol_handles".to_string(), JsonSchema::array(
                        JsonSchema::object(BTreeMap::from([
                            ("path".to_string(), JsonSchema::string(Some("Repository-relative owning source path.".to_string()))),
                            ("symbol".to_string(), JsonSchema::string(Some("Stable compiler-neutral symbol locator.".to_string()))),
                        ]), Some(vec!["path".to_string(), "symbol".to_string()]), Some(false.into())),
                        Some("Durable source handles that ground every reported finding.".to_string()),
                    )),
                    ("validation_commands".to_string(), JsonSchema::array(
                        JsonSchema::array(JsonSchema::string(None), Some("One direct argv validation command.".to_string())),
                        Some("Direct argv commands that prove or disprove every reported finding.".to_string()),
                    )),
                ]),
                Some(vec!["calls".to_string(), "symbol_handles".to_string(), "validation_commands".to_string()]), Some(false.into()),
            ),
            output_schema: None,
        })
    }
    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let ToolPayload::Function { arguments } = &invocation.payload else {
                return Err(FunctionCallError::RespondToModel(
                    "lint_tool_calls received unsupported payload".to_string(),
                ));
            };
            let args: LintArgs = parse_arguments(arguments)?;
            if args.symbol_handles.is_empty()
                || args
                    .symbol_handles
                    .iter()
                    .any(|handle| handle.path.trim().is_empty() || handle.symbol.trim().is_empty())
            {
                return Err(FunctionCallError::RespondToModel(
                    "symbol_handles requires at least one non-empty path and symbol".to_string(),
                ));
            }
            if args.validation_commands.is_empty()
                || args
                    .validation_commands
                    .iter()
                    .any(|command| command.is_empty() || command.iter().any(String::is_empty))
            {
                return Err(FunctionCallError::RespondToModel(
                    "validation_commands requires at least one non-empty direct argv command"
                        .to_string(),
                ));
            }
            let findings = lint_calls(&args.calls, &args.symbol_handles, &args.validation_commands);
            let execution_waves = execution_waves(&args.calls)?;
            Ok(boxed_tool_output(JsonToolOutput::new(json!({
                "call_count": args.calls.len(),
                "finding_count": findings.len(),
                "findings": findings,
                "execution_waves": execution_waves,
            }))))
        })
    }
}

fn execution_waves(calls: &[LintCall]) -> Result<Vec<Vec<String>>, FunctionCallError> {
    if calls
        .iter()
        .all(|call| call.id.is_none() && call.depends_on.is_empty())
    {
        return Ok(Vec::new());
    }
    let mut remaining = BTreeMap::<String, Vec<String>>::new();
    for (index, call) in calls.iter().enumerate() {
        let Some(id) = call.id.as_ref().filter(|id| !id.trim().is_empty()) else {
            return Err(FunctionCallError::RespondToModel(format!(
                "call {index} requires a non-empty id when dependency batching is used"
            )));
        };
        if remaining
            .insert(id.clone(), call.depends_on.clone())
            .is_some()
        {
            return Err(FunctionCallError::RespondToModel(format!(
                "duplicate orchestration node id `{id}`"
            )));
        }
    }
    for (id, dependencies) in &remaining {
        for dependency in dependencies {
            if !remaining.contains_key(dependency) {
                return Err(FunctionCallError::RespondToModel(format!(
                    "orchestration node `{id}` depends on unknown node `{dependency}`"
                )));
            }
        }
    }
    let mut completed = std::collections::BTreeSet::new();
    let mut waves = Vec::new();
    while !remaining.is_empty() {
        let wave = remaining
            .iter()
            .filter(|(_, dependencies)| {
                dependencies
                    .iter()
                    .all(|dependency| completed.contains(dependency))
            })
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        if wave.is_empty() {
            return Err(FunctionCallError::RespondToModel(
                "orchestration dependency graph contains a cycle".to_string(),
            ));
        }
        for id in &wave {
            remaining.remove(id);
            completed.insert(id.clone());
        }
        waves.push(wave);
    }
    Ok(waves)
}
impl CoreToolRuntime for OrchestrationLintHandler {}

fn lint_calls(
    calls: &[LintCall],
    symbol_handles: &[AuditSymbolHandle],
    validation_commands: &[Vec<String>],
) -> Vec<Value> {
    let mut findings = Vec::new();
    let mut seen = std::collections::HashMap::<String, usize>::new();
    let source_reads = calls
        .iter()
        .filter(|call| call.tool == "read_source_batch")
        .count();
    for (index, call) in calls.iter().enumerate() {
        let fingerprint =
            serde_json::to_string(&(call.tool.as_str(), &call.arguments)).unwrap_or_default();
        if let Some(first) = seen.insert(fingerprint, index) {
            findings.push(lint_finding(
                "duplicate_call",
                Some(index),
                Some(first),
                "Equivalent call is repeated; reuse its output.",
                symbol_handles,
                validation_commands,
            ));
        }
        if call.tool == "exec_command"
            && call.arguments.get("kind").and_then(Value::as_str) != Some("argv")
            && (call.arguments.get("program").is_some()
                || call
                    .arguments
                    .get("cmd")
                    .and_then(Value::as_str)
                    .is_some_and(|cmd| {
                        ["git ", "rg ", "cargo ", "node ", "python "]
                            .iter()
                            .any(|prefix| cmd.starts_with(prefix))
                    }))
        {
            findings.push(lint_finding(
                "avoidable_shell_wrapper",
                Some(index),
                None,
                "Use exec_command kind=argv with separated program and args.",
                symbol_handles,
                validation_commands,
            ));
        }
        if call.tool == "exec_command"
            && call.arguments.get("program").and_then(Value::as_str) == Some("rg")
        {
            let scoped = call
                .arguments
                .get("args")
                .and_then(Value::as_array)
                .is_some_and(|args| args.len() >= 2);
            if !scoped {
                findings.push(lint_finding(
                    "unscoped_search",
                    Some(index),
                    None,
                    "Add an explicit repository path or bounded owner directory to rg.",
                    symbol_handles,
                    validation_commands,
                ));
            }
        }
        if call.tool == "write_stdin"
            && call
                .arguments
                .get("chars")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
            && call.arguments.get("until_exit_ms").is_none()
        {
            findings.push(lint_finding(
                "manual_poll",
                Some(index),
                None,
                "Set until_exit_ms to a bounded deadline instead of repeated status polling.",
                symbol_handles,
                validation_commands,
            ));
        }
    }
    if source_reads > 1 {
        findings.push(lint_finding(
            "serializable_batch",
            None,
            None,
            "Combine multiple read_source_batch calls into one reads array.",
            symbol_handles,
            validation_commands,
        ));
    }
    findings
}

fn lint_finding(
    kind: &str,
    call_index: Option<usize>,
    first_index: Option<usize>,
    semantic_conclusion: &str,
    symbol_handles: &[AuditSymbolHandle],
    validation_commands: &[Vec<String>],
) -> Value {
    let call_identity = call_index
        .map(|index| index.to_string())
        .unwrap_or_default();
    let first_identity = first_index
        .map(|index| index.to_string())
        .unwrap_or_default();
    let handles_identity = serde_json::to_string(symbol_handles).unwrap_or_default();
    let commands_identity = serde_json::to_string(validation_commands).unwrap_or_default();
    json!({
        "finding_id": stable_identity("KD4_TOOL_AUDIT_FINDING_V1", &[kind, &call_identity, &first_identity, semantic_conclusion, &handles_identity, &commands_identity]),
        "kind": kind,
        "call_index": call_index,
        "first_index": first_index,
        "message": semantic_conclusion,
        "semantic_conclusion": semantic_conclusion,
        "symbol_handles": symbol_handles,
        "validation_commands": validation_commands,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_selection_is_one_based_and_bounded() {
        let result = select_lines(
            SourceRead {
                path: "x".into(),
                start_line: Some(2),
                end_line: Some(9),
                known_sha256: None,
            },
            "a\nb\nc".into(),
            "task-a",
        )
        .unwrap();
        assert_eq!(result.text.as_deref(), Some("b\nc"));
        assert_eq!(result.end_line, 3);
        assert!(!result.unchanged);
    }

    #[test]
    fn matching_projection_hash_omits_repeated_source_text() {
        let first = select_lines(
            SourceRead {
                path: "x".into(),
                start_line: Some(2),
                end_line: Some(3),
                known_sha256: None,
            },
            "a\nb\nc".into(),
            "task-a",
        )
        .unwrap();
        let repeated = select_lines(
            SourceRead {
                path: "x".into(),
                start_line: Some(2),
                end_line: Some(3),
                known_sha256: Some(first.sha256.clone()),
            },
            "a\nb\nc".into(),
            "task-a",
        )
        .unwrap();

        assert!(repeated.unchanged);
        assert_eq!(repeated.sha256, first.sha256);
        assert_eq!(repeated.content_identity, first.content_identity);
        assert_eq!(repeated.text, None);
    }

    #[test]
    fn evidence_contract_source_identity_is_stable_and_task_local() {
        let read = || SourceRead {
            path: "src/owner.rs".into(),
            start_line: Some(1),
            end_line: Some(1),
            known_sha256: None,
        };
        let first = select_lines(read(), "owner".into(), "task-a").unwrap();
        let repeated = select_lines(read(), "owner".into(), "task-a").unwrap();
        let other_task = select_lines(read(), "owner".into(), "task-b").unwrap();

        assert_eq!(first.content_identity, repeated.content_identity);
        assert_ne!(first.content_identity, other_task.content_identity);
        assert_eq!(first.content_identity.len(), 64);
    }

    #[test]
    fn cargo_test_probe_preserves_the_exact_run_selection() {
        let args = CargoTestArgs {
            package: Some("codex-core".into()),
            test_filter: Some("focused_contract".into()),
            cargo_args: vec!["--lib".into()],
            harness_args: vec!["--nocapture".into()],
            workdir: None,
            timeout_ms: default_cargo_test_timeout_ms(),
        };
        assert_eq!(
            cargo_test_args(&args, false),
            vec![
                "test",
                "-p",
                "codex-core",
                "--lib",
                "focused_contract",
                "--",
                "--nocapture"
            ]
        );
        assert_eq!(
            cargo_test_args(&args, true),
            vec![
                "test",
                "-p",
                "codex-core",
                "--lib",
                "focused_contract",
                "--",
                "--list",
                "--nocapture"
            ]
        );
    }

    #[test]
    fn listed_test_count_uses_the_canonical_process_output_once() {
        let result = json!({
            "output": "alpha: test\nbeta: test\n2 tests, 0 benchmarks",
            "summary": "alpha: test\nbeta: test"
        });
        assert_eq!(count_listed_tests(&result), 2);
        assert_eq!(
            count_listed_tests(&json!({"output":"0 tests, 0 benchmarks"})),
            0
        );
    }

    #[test]
    fn architecture_slice_surfaces_declared_fanout_and_explicit_exclusions() {
        let index = json!({
            "repository_revision": "head-1",
            "manifest_sha256": "manifest-1",
            "status": "complete",
            "omitted": {"relationships": 0},
            "unresolved": [],
            "owners": [{
                "id": "owner-a",
                "primary_entries": [{"path":"src/main.rs","symbol":"main"}],
                "configuration": [],
                "generated_artifacts": [],
                "tests": ["tests/owner.rs"],
                "invariants": [{
                    "id":"stable-contract",
                    "evidence":[{"path":"src/lib.rs","symbol":"run"}]
                }],
                "facet_exclusions": {
                    "configuration_and_gates":"No configuration surface.",
                    "generated_artifacts":"No generated outputs."
                }
            }],
            "relationships": [
                {
                    "source":"owner:owner-a",
                    "target":"path:src/lib.rs",
                    "category":"control_flow",
                    "kind":"calls",
                    "confidence":"compiler_resolved",
                    "evidence":[{"path":"src/main.rs","symbol":"main"}]
                },
                {
                    "source":"owner:owner-a",
                    "target":"owner:consumer",
                    "category":"callers_consumers",
                    "kind":"consumed_by",
                    "confidence":"declared",
                    "evidence":[{"path":"src/lib.rs","symbol":"run"}]
                }
            ]
        });

        let slice = architecture_slice_from_index(&index, &["owner-a".into()], 32, None, 123)
            .expect("complete architecture slice");
        assert_eq!(slice["snapshot"], "head-1:manifest-1");
        assert_eq!(
            slice["callers_and_consumers"]["relationships"][0]["kind"],
            "consumer"
        );
        assert_eq!(slice["configuration_and_gates"]["status"], "not_applicable");
        assert_eq!(slice["material_unknowns"], json!([]));
        assert_eq!(slice["truncated"], false);
    }

    #[test]
    fn architecture_slice_reports_relationship_budget_omissions() {
        let index = json!({
            "repository_revision": "head-1",
            "manifest_sha256": "manifest-1",
            "status": "complete",
            "omitted": {"relationships": 0},
            "unresolved": [],
            "owners": [{
                "id":"owner-a",
                "primary_entries": [],
                "configuration": [],
                "generated_artifacts": [],
                "tests": [],
                "invariants": [],
                "facet_exclusions": {}
            }],
            "relationships": [
                {"source":"owner:owner-a","target":"path:a","category":"control_flow","kind":"calls","confidence":"declared","evidence":[]},
                {"source":"owner:owner-a","target":"path:b","category":"control_flow","kind":"calls","confidence":"declared","evidence":[]}
            ]
        });

        let slice = architecture_slice_from_index(&index, &["owner-a".into()], 1, None, 10)
            .expect("bounded architecture slice");
        assert_eq!(slice["omitted_relationships"], 1);
        assert_eq!(slice["truncated"], true);
    }

    #[test]
    fn architecture_slice_ranks_focus_within_a_facet() {
        let index = json!({
            "repository_revision": "head-1",
            "manifest_sha256": "manifest-1",
            "status": "complete",
            "omitted": {"relationships": 0},
            "unresolved": [],
            "owners": [{
                "id":"owner-a",
                "primary_entries": [],
                "configuration": [],
                "generated_artifacts": [],
                "tests": [],
                "invariants": [],
                "facet_exclusions": {}
            }],
            "relationships": [
                {"source":"owner:owner-a","target":"path:secondary_queue.rs","category":"control_flow","kind":"calls","confidence":"declared","evidence":[]},
                {"source":"owner:owner-a","target":"path:critical_cache.rs","category":"control_flow","kind":"calls","confidence":"declared","evidence":[]}
            ]
        });

        let slice = architecture_slice_from_index(
            &index,
            &["owner-a".into()],
            32,
            Some("repair critical cache"),
            10,
        )
        .expect("ranked architecture slice");
        assert_eq!(
            slice["control_and_data_flow"]["relationships"][0]["target"],
            "path:critical_cache.rs"
        );
    }

    #[test]
    fn architecture_slice_balances_a_small_budget_across_facets() {
        let index = json!({
            "repository_revision": "head-1",
            "manifest_sha256": "manifest-1",
            "status": "complete",
            "omitted": {"relationships": 0},
            "unresolved": [],
            "owners": [{
                "id":"owner-a",
                "primary_entries": [],
                "configuration": [],
                "generated_artifacts": [],
                "tests": [],
                "invariants": [],
                "facet_exclusions": {}
            }],
            "relationships": [
                {"source":"owner:owner-a","target":"path:control_a.rs","category":"control_flow","kind":"calls","confidence":"declared","evidence":[]},
                {"source":"owner:owner-a","target":"path:control_b.rs","category":"control_flow","kind":"calls","confidence":"declared","evidence":[]},
                {"source":"owner:owner-a","target":"config:feature.toml","category":"configuration","kind":"reads_config","confidence":"declared","evidence":[]}
            ]
        });

        let slice = architecture_slice_from_index(&index, &["owner-a".into()], 2, None, 10)
            .expect("facet-balanced architecture slice");
        assert_eq!(
            slice["control_and_data_flow"]["relationships"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            slice["configuration_and_gates"]["relationships"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(slice["omitted_relationships"], 1);
    }

    #[test]
    fn lint_detects_duplicate_shell_and_polling_costs() {
        let calls = vec![
            LintCall {
                id: None,
                tool: "exec_command".into(),
                arguments: json!({"cmd":"git status"}),
                depends_on: Vec::new(),
            },
            LintCall {
                id: None,
                tool: "exec_command".into(),
                arguments: json!({"cmd":"git status"}),
                depends_on: Vec::new(),
            },
            LintCall {
                id: None,
                tool: "write_stdin".into(),
                arguments: json!({"session_id": 1}),
                depends_on: Vec::new(),
            },
        ];
        let symbol_handles = vec![AuditSymbolHandle {
            path: "src/tools.rs".into(),
            symbol: "ToolRouter::dispatch".into(),
        }];
        let validation_commands = vec![vec!["cargo".into(), "test".into(), "focused".into()]];
        let findings = lint_calls(&calls, &symbol_handles, &validation_commands);
        assert!(findings.iter().any(|f| f["kind"] == "duplicate_call"));
        assert!(
            findings
                .iter()
                .any(|f| f["kind"] == "avoidable_shell_wrapper")
        );
        assert!(findings.iter().any(|f| f["kind"] == "manual_poll"));
        assert!(findings.iter().all(|finding| {
            finding["finding_id"]
                .as_str()
                .is_some_and(|id| id.len() == 64)
                && finding["semantic_conclusion"].as_str().is_some()
                && finding["symbol_handles"][0]["symbol"] == "ToolRouter::dispatch"
                && finding["validation_commands"][0][0] == "cargo"
        }));
    }

    #[test]
    fn dependency_graph_is_returned_as_parallel_waves() {
        let calls = vec![
            LintCall {
                id: Some("inspect-a".into()),
                tool: "read_source_batch".into(),
                arguments: json!({}),
                depends_on: Vec::new(),
            },
            LintCall {
                id: Some("inspect-b".into()),
                tool: "exec_command".into(),
                arguments: json!({}),
                depends_on: Vec::new(),
            },
            LintCall {
                id: Some("validate".into()),
                tool: "exec_command".into(),
                arguments: json!({}),
                depends_on: vec!["inspect-a".into(), "inspect-b".into()],
            },
        ];
        assert_eq!(
            execution_waves(&calls).unwrap(),
            vec![
                vec!["inspect-a".to_string(), "inspect-b".to_string()],
                vec!["validate".to_string()]
            ]
        );
    }
}
