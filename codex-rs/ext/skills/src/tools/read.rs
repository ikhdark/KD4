use std::collections::hash_map::DefaultHasher;
use std::hash::Hash;
use std::hash::Hasher;

use codex_extension_api::FunctionCallError;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolExecutor;
use codex_extension_api::ToolExecutorFuture;
use codex_extension_api::ToolName;
use codex_extension_api::ToolSpec;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;

use crate::catalog::SkillPackageId;
use crate::catalog::SkillResourceId;
use crate::provider::SkillReadRequest;

use super::MAX_HANDLE_BYTES;
use super::SkillToolAuthority;
use super::SkillToolContext;
use super::external_json_output;
use super::parse_args;
use super::skill_function_tool;
use super::skill_tool_name;
use super::validate_handle;

const TOOL_NAME: &str = "read";
const MAX_SKILL_RESPONSE_BYTES: usize = 512 * 1024;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    authority: SkillToolAuthority,
    package: String,
    resource: String,
    cursor: Option<String>,
}

#[derive(Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[schemars(deny_unknown_fields)]
struct ReadResponse {
    resource: String,
    contents: String,
    next_cursor: Option<String>,
}

#[derive(Clone)]
pub(super) struct ReadTool {
    pub(super) context: SkillToolContext,
}

impl ToolExecutor<ToolCall> for ReadTool {
    fn tool_name(&self) -> ToolName {
        skill_tool_name(TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        skill_function_tool::<ReadArgs, ReadResponse>(
            TOOL_NAME,
            "Read one page from an enabled skill. Pass the exact authority and package returned by skills.list; resource identifiers remain opaque and are routed to that authority. Pass next_cursor back as cursor to continue the same cached resource.",
        )
    }

    fn handle(&self, call: ToolCall) -> ToolExecutorFuture<'_> {
        Box::pin(async move {
            let args: ReadArgs = parse_args(&call)?;
            let response_byte_budget = call.response_byte_budget(MAX_SKILL_RESPONSE_BYTES);
            let authority = args.authority.into_authority();
            validate_handle("package", &args.package, MAX_HANDLE_BYTES)?;
            validate_handle("resource", &args.resource, MAX_HANDLE_BYTES)?;

            let catalog = self.context.catalog(&call.turn_id, args.authority).await;
            let package_is_available = catalog.entries.iter().any(|entry| {
                entry.enabled && entry.authority == authority && entry.id.0 == args.package
            });
            if !package_is_available {
                return Err(FunctionCallError::RespondToModel(
                    "skill package is not available from the requested authority".to_string(),
                ));
            }

            let requested_resource = SkillResourceId::new(args.resource);
            let result = self
                .context
                .thread_state
                .read_skill(
                    &self.context.providers,
                    SkillReadRequest {
                        authority,
                        package: SkillPackageId(args.package),
                        resource: requested_resource.clone(),
                        host_snapshot: None,
                        mcp_resources: self.context.mcp_resources.clone(),
                    },
                )
                .await
                .map_err(|err| {
                    tracing::warn!(
                        error = %err,
                        turn_id = %call.turn_id,
                        call_id = %call.call_id,
                        resource = requested_resource.as_str(),
                        "skills.read provider request failed"
                    );
                    FunctionCallError::RespondToModel("failed to read skill resource".to_string())
                })?;
            if result.resource != requested_resource {
                return Err(FunctionCallError::Fatal(
                    "skill provider returned a different resource".to_string(),
                ));
            }

            let start = parse_pagination_cursor(args.cursor.as_deref(), result.contents.as_str())?;
            if start > result.contents.len() || !result.contents.is_char_boundary(start) {
                return Err(FunctionCallError::RespondToModel(
                    "skills.read cursor is invalid".to_string(),
                ));
            }
            let response = page_response(
                result.resource.as_str(),
                &result.contents,
                start,
                response_byte_budget,
            )?;

            external_json_output(&response)
        })
    }
}

fn page_response(
    resource: &str,
    contents: &str,
    start: usize,
    max_response_bytes: usize,
) -> Result<ReadResponse, FunctionCallError> {
    let response = |end, next_cursor| ReadResponse {
        resource: resource.to_string(),
        contents: contents[start..end].to_string(),
        next_cursor,
    };
    let complete = response(contents.len(), None);
    if serialized_len(&complete)? <= max_response_bytes {
        return Ok(complete);
    }

    let mut lower = start;
    let mut upper = contents.len();
    let mut best = None;
    while lower < upper {
        // Probe strictly above lower so a multibyte character cannot stall the search.
        let end = contents.ceil_char_boundary(lower.midpoint(upper).saturating_add(1));
        let candidate = response(end, Some(pagination_cursor(contents, end)));
        if serialized_len(&candidate)? <= max_response_bytes {
            lower = end;
            best = Some(candidate);
        } else {
            upper = contents.floor_char_boundary(end.saturating_sub(1));
        }
    }
    best.ok_or_else(|| {
        FunctionCallError::RespondToModel(
            "skills.read response budget leaves no room for contents".to_string(),
        )
    })
}

fn pagination_cursor(contents: &str, offset: usize) -> String {
    format!("{:016x}:{offset}", value_fingerprint(contents))
}

fn parse_pagination_cursor(
    cursor: Option<&str>,
    contents: &str,
) -> Result<usize, FunctionCallError> {
    let Some(cursor) = cursor else {
        return Ok(0);
    };
    let invalid = || FunctionCallError::RespondToModel("skills.read cursor is invalid".to_string());
    let (fingerprint, offset) = cursor.split_once(':').ok_or_else(invalid)?;
    if u64::from_str_radix(fingerprint, 16).ok() != Some(value_fingerprint(contents)) {
        return Err(FunctionCallError::RespondToModel(
            "skills.read cursor is stale; restart from the first page".to_string(),
        ));
    }
    offset.parse::<usize>().map_err(|_| invalid())
}

fn value_fingerprint(value: &(impl Hash + ?Sized)) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

fn serialized_len(value: &impl Serialize) -> Result<usize, FunctionCallError> {
    serde_json::to_vec(value)
        .map(|value| value.len())
        .map_err(|err| FunctionCallError::Fatal(err.to_string()))
}
