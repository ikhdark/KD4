//! Consolidated native front doors. Original runtimes remain the execution owners.
use std::collections::BTreeMap;
use std::sync::Arc;

use codex_tools::ResponsesApiNamespace;
use codex_tools::ResponsesApiNamespaceTool;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolExecutor;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;

use crate::FunctionCallError;
use crate::agent::task_capabilities::TypedToolClass;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::RegisteredTool;
use crate::tools::registry::ToolExposure;

type Operation = (&'static str, &'static str);
type Group = (
    &'static str,
    &'static str,
    TypedToolClass,
    &'static [Operation],
);

// Group related operations, never different authority classes or namespaces.
// Interactive answers/grants, encrypted messages, command launch, source acquisition,
// retained recovery and structured edits retain their dedicated front doors.
const GROUPS: &[Group] = &[
    (
        "inspect_files",
        "List paths, wait for local file state, and inspect checksums, line endings, freshness, classification and prior read coverage.",
        TypedToolClass::ReadSearch,
        &[
            ("list", "list_files"),
            ("wait", "wait_for_file"),
            ("paths", "canonical_paths"),
            ("checksum", "checksum_file"),
            ("line_endings", "line_ending_diagnostics"),
            ("freshness", "check_snapshot_freshness"),
            ("read_history", "read_status"),
            ("classify", "classify_paths"),
        ],
    ),
    (
        "inspect_environment",
        "Inspect backend capabilities, executable resolution and host process inventory.",
        TypedToolClass::ReadSearch,
        &[
            ("capabilities", "backend_capabilities"),
            ("executable", "lookup_executable"),
            ("processes", "list_processes"),
        ],
    ),
    (
        "inspect_git",
        "Inspect Git differences/history, working-tree status, conflicts and line attribution.",
        TypedToolClass::ReadSearch,
        &[
            ("diff", "git_diff"),
            ("status", "git_status"),
            ("conflicts", "git_conflicts"),
            ("blame", "git_blame"),
        ],
    ),
    (
        "inspect_artifacts",
        "Compare retained artifact bytes or align authenticated source lines.",
        TypedToolClass::ReadSearch,
        &[
            ("compare", "compare_artifacts"),
            ("align_lines", "line_map"),
        ],
    ),
    (
        "mcp_resources",
        "List authorized MCP resources or URI templates and read a known resource.",
        TypedToolClass::ReadSearch,
        &[
            ("list", "list_mcp_resources"),
            ("read", "read_mcp_resource"),
        ],
    ),
    (
        "manage_process",
        "Interact with one owned process or wait for a group of owned processes.",
        TypedToolClass::Shell,
        &[("interact", "write_stdin"), ("wait", "wait_processes")],
    ),
    (
        "agent_team",
        "List or wait for agents and manage cooperation locks.",
        TypedToolClass::AgentCommunication,
        &[
            ("list", "list_agents"),
            ("wait", "wait_agent"),
            ("lock", "agent_lock"),
        ],
    ),
    (
        "agent_task",
        "Read task/evidence state, publish results, report batch-job items, submit receipts, set gates and declare edit intentions.",
        TypedToolClass::OwnTask,
        &[
            ("get", "get_agent_task"),
            ("partial_result", "get_agent_partial_result"),
            ("group_findings", "group_agent_findings"),
            ("set_gate", "set_agent_gate"),
            ("publish", "publish_agent_partial_result"),
            ("submit_receipt", "submit_agent_receipt"),
            ("job_result", "report_agent_job_result"),
            ("edit_intents", "declare_edit_intents"),
        ],
    ),
    (
        "agent_control",
        "Spawn individual or CSV batch agents, interrupt, amend or abandon agent tasks and waive soft gates.",
        TypedToolClass::RootTaskControl,
        &[
            ("spawn", "spawn_agent"),
            ("spawn_csv", "spawn_agents_on_csv"),
            ("interrupt", "interrupt_agent"),
            ("amend", "amend_agent_task"),
            ("waive_gate", "waive_agent_gate"),
            ("abandon", "abandon_agent_task"),
        ],
    ),
];

pub(crate) fn consolidate_native_tools(runtimes: &mut Vec<RegisteredTool>, deferred: bool) {
    for &(name, description, class, operations) in GROUPS {
        let mut namespaces = BTreeMap::<Option<String>, Vec<(Operation, usize)>>::new();
        for (index, tool) in runtimes.iter().enumerate() {
            if tool.authorization_class() != class
                || matches!(
                    tool.exposure(),
                    ToolExposure::Hidden | ToolExposure::Compatibility
                )
            {
                continue;
            }
            if let Some(&operation) = operations
                .iter()
                .find(|(_, old)| *old == tool.tool_name().name)
            {
                namespaces
                    .entry(tool.tool_name().namespace.clone())
                    .or_default()
                    .push((operation, index));
            }
        }
        for (namespace, members) in namespaces {
            if members.len() < 2 {
                continue;
            }
            let identity = ToolName::new(namespace, name.to_string());
            if runtimes.iter().any(|tool| tool.tool_name() == &identity) {
                continue;
            }
            let direct_only = members
                .iter()
                .all(|(_, i)| runtimes[*i].exposure() == ToolExposure::DirectModelOnly);
            if !direct_only
                && members
                    .iter()
                    .any(|(_, i)| runtimes[*i].exposure() == ToolExposure::DirectModelOnly)
            {
                continue;
            }
            let selected = members
                .iter()
                .map(|((operation, _), i)| (*operation, &runtimes[*i]))
                .collect::<Vec<_>>();
            // Unsupported schema identities keep their original API instead of
            // weakening a contract or silently dropping an operation.
            let Some(group) = NativeToolGroup::new(identity, description, &selected) else {
                continue;
            };
            let exposure = if direct_only {
                ToolExposure::DirectModelOnly
            } else if deferred && class != TypedToolClass::Shell {
                ToolExposure::Deferred
            } else {
                ToolExposure::Direct
            };
            for (_, index) in members {
                runtimes[index].set_exposure(if direct_only {
                    ToolExposure::Hidden
                } else {
                    ToolExposure::Compatibility
                });
            }
            runtimes.push(RegisteredTool::with_exposure(
                Arc::new(group),
                exposure,
                class,
            ));
        }
    }
}

struct NativeToolGroup {
    name: ToolName,
    spec: ToolSpec,
    code_mode_schema: Value,
    operations: BTreeMap<String, (ToolName, Arc<dyn CoreToolRuntime>)>,
    parallel: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    operation: String,
    arguments: serde_json::Map<String, Value>,
}

impl NativeToolGroup {
    fn new(name: ToolName, description: &str, members: &[(&str, &RegisteredTool)]) -> Option<Self> {
        let mut direct = Vec::new();
        let mut nested = Vec::new();
        let mut outputs = Vec::new();
        let mut operations = BTreeMap::new();
        let mut parallel = true;
        let mut namespace_description: &str = "";
        for &(operation, tool) in members {
            let function = match tool.spec() {
                ToolSpec::Function(function) => function,
                ToolSpec::Namespace(namespace) => {
                    // Group descriptions belong on the callable, not on its
                    // shared namespace. Preserve genuine conflicts for the
                    // existing registry quarantine instead of inventing one.
                    if !namespace.description.trim().is_empty() {
                        if !namespace_description.is_empty()
                            && namespace_description != namespace.description
                        {
                            return None;
                        }
                        namespace_description = namespace.description.as_str();
                    }
                    match namespace.tools.as_slice() {
                        [ResponsesApiNamespaceTool::Function(function)] => function,
                        _ => return None,
                    }
                }
                _ => return None,
            };
            direct.push((
                operation,
                function.description.as_str(),
                serde_json::to_value(&function.parameters).ok()?,
            ));
            nested.push((
                operation,
                function.description.as_str(),
                tool.effective_input_schema()?.clone(),
            ));
            if let Some(schema) = &function.output_schema {
                outputs.push(schema.to_value());
            }
            operations.insert(
                operation.to_string(),
                (tool.tool_name().clone(), Arc::clone(tool.runtime())),
            );
            parallel &= tool.runtime().supports_parallel_tool_calls();
        }
        let function = ResponsesApiTool {
            name: name.name.clone(),
            description: format!(
                "{description} Select operation and pass its unchanged input object as arguments. Only operations available in this session are listed. Each operation retains its own permission, backend, limits, cancellation, persistence and result contract; grouping grants no authority. Legacy exact tool calls remain compatible."
            ),
            strict: false,
            defer_loading: None,
            parameters: serde_json::from_value(group_schema(direct)?).ok()?,
            // No new result envelope: retain the union of declared original
            // shapes, or unknown when any operation has no output contract.
            // Use anyOf, not oneOf: valid operation outputs may overlap.
            output_schema: if outputs.len() == members.len() {
                merge_output_schemas(outputs).map(Into::into)
            } else {
                None
            },
        };
        let spec = if let Some(namespace) = &name.namespace {
            ToolSpec::Namespace(ResponsesApiNamespace {
                name: namespace.clone(),
                description: namespace_description.to_string(),
                tools: vec![ResponsesApiNamespaceTool::Function(function)],
            })
        } else {
            ToolSpec::Function(function)
        };
        Some(Self {
            name,
            spec,
            code_mode_schema: group_schema(nested)?,
            operations,
            parallel,
        })
    }
}

fn group_schema(members: Vec<(&str, &str, Value)>) -> Option<Value> {
    let mut variants = Vec::new();
    let mut names = Vec::new();
    for (index, (operation, description, mut arguments)) in members.into_iter().enumerate() {
        relocate_schema(
            &mut arguments,
            &format!("#/oneOf/{index}/properties/arguments"),
        )?;
        let fields = arguments.as_object_mut()?;
        let existing = fields.get("description").and_then(Value::as_str);
        fields.insert(
            "description".into(),
            json!(match existing {
                Some(existing) => format!("{description}\n{existing}"),
                None => description.to_string(),
            }),
        );
        names.push(json!(operation));
        variants.push(json!({
            "type": "object",
            "properties": {"operation": {"type": "string", "enum": [operation]}, "arguments": arguments},
            "required": ["operation", "arguments"], "additionalProperties": false,
        }));
    }
    Some(json!({
        "type": "object",
        "properties": {"operation": {"type": "string", "enum": names}, "arguments": {"type": "object"}},
        "required": ["operation", "arguments"], "additionalProperties": false, "oneOf": variants,
    }))
}

fn merge_output_schemas(schemas: Vec<Value>) -> Option<Value> {
    let mut variants = Vec::new();
    for schema in schemas {
        if !variants.contains(&schema) {
            variants.push(schema);
        }
    }
    if variants.len() == 1 {
        return variants.pop();
    }
    for (index, schema) in variants.iter_mut().enumerate() {
        relocate_schema(schema, &format!("#/anyOf/{index}"))?;
    }
    (!variants.is_empty()).then(|| json!({"anyOf": variants}))
}

// Rebase schema-position references only. Enum/default values are caller data,
// not schema trees; never walk or rewrite them.
fn relocate_schema(schema: &mut Value, prefix: &str) -> Option<()> {
    let Some(fields) = schema.as_object_mut() else {
        return schema.is_boolean().then_some(());
    };
    if fields.contains_key("$id") || fields.contains_key("$anchor") {
        return None;
    }
    if let Some(reference) = fields.get_mut("$ref") {
        let original = reference.as_str()?;
        if original != "#" && !original.starts_with("#/") {
            return None;
        }
        *reference = json!(format!("{prefix}{}", &original[1..]));
    }
    for key in ["properties", "$defs", "definitions"] {
        if let Some(children) = fields.get_mut(key) {
            for child in children.as_object_mut()?.values_mut() {
                relocate_schema(child, prefix)?;
            }
        }
    }
    for key in ["anyOf", "oneOf", "allOf"] {
        if let Some(children) = fields.get_mut(key) {
            for child in children.as_array_mut()? {
                relocate_schema(child, prefix)?;
            }
        }
    }
    for key in ["items", "additionalProperties"] {
        if let Some(child) = fields.get_mut(key) {
            relocate_schema(child, prefix)?;
        }
    }
    Some(())
}

impl ToolExecutor<ToolInvocation> for NativeToolGroup {
    fn tool_name(&self) -> ToolName {
        self.name.clone()
    }
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }
    fn supports_parallel_tool_calls(&self) -> bool {
        self.parallel
    }

    fn handle(&self, _invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async {
            Err(FunctionCallError::RespondToModel(
                "native tool groups must resolve through the registered operation runtime".into(),
            ))
        })
    }
}

impl CoreToolRuntime for NativeToolGroup {
    fn operation_routes(&self) -> BTreeMap<String, ToolName> {
        self.operations
            .iter()
            .map(|(operation, (name, _))| (operation.clone(), name.clone()))
            .collect()
    }

    fn code_mode_input_schema(&self) -> Option<Value> {
        Some(self.code_mode_schema.clone())
    }

    fn resolve_call(
        &self,
        payload: &ToolPayload,
    ) -> Result<Option<(ToolName, ToolPayload)>, FunctionCallError> {
        let ToolPayload::Function { arguments } = payload else {
            return Err(FunctionCallError::RespondToModel(
                "native tool groups require JSON function arguments".into(),
            ));
        };
        let args: Arguments = parse_arguments(arguments)?;
        let (name, _) = self.operations.get(&args.operation).ok_or_else(|| {
            FunctionCallError::RespondToModel(format!(
                "operation {:?} is unavailable for {}; available operations: {}",
                args.operation,
                self.name,
                self.operations
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })?;
        Ok(Some((
            name.clone(),
            ToolPayload::Function {
                arguments: Value::Object(args.arguments).to_string(),
            },
        )))
    }

    fn prepare_invocation<'a>(
        &'a self,
        mut invocation: ToolInvocation,
    ) -> futures::future::BoxFuture<'a, Result<ToolInvocation, FunctionCallError>> {
        Box::pin(async move {
            if let Some((name, payload)) = self.resolve_call(&invocation.payload)? {
                invocation.tool_name = name;
                invocation.payload = payload;
            }
            let runtime = self
                .operations
                .values()
                .find(|(name, _)| name == &invocation.tool_name)
                .map(|(_, runtime)| runtime)
                .ok_or_else(|| {
                    FunctionCallError::RespondToModel("native operation is unavailable".into())
                })?;
            runtime.prepare_invocation(invocation).await
        })
    }
}
