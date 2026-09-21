use std::cell::OnceCell;
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
            "Read a page from an enabled skill using known authority, package, and resource handles. Use skills.list only when a required handle is missing. Handles remain opaque and are routed to their owner. Pass next_cursor back as cursor to continue. Restart from the first page if the cursor is reported stale.",
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
                    if catalog.continuation.is_some() {
                        "skill package has not been discovered yet; use skills.list and follow next_cursor to continue incomplete discovery"
                    } else {
                        "skill package is not available from the requested authority"
                    }
                    .to_string(),
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
                    FunctionCallError::RespondToModel(err.model_message().to_string())
                })?;
            let fingerprint = OnceCell::new();
            let start = parse_pagination_cursor(
                args.cursor.as_deref(),
                result.contents.as_str(),
                &fingerprint,
            )?;
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
                &fingerprint,
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
    fingerprint: &OnceCell<u64>,
) -> Result<ReadResponse, FunctionCallError> {
    let response = |end, next_cursor| ReadResponse {
        resource: resource.to_string(),
        contents: contents[start..end].to_string(),
        next_cursor,
    };
    if contents.len() - start <= max_response_bytes {
        let complete = response(contents.len(), None);
        if serialized_len(&complete)? <= max_response_bytes {
            return Ok(complete);
        }
    }

    let fingerprint = *fingerprint.get_or_init(|| value_fingerprint(contents));
    let mut lower = start;
    let mut upper =
        contents.floor_char_boundary(start.saturating_add(max_response_bytes).min(contents.len()));
    let mut best = None;
    while lower < upper {
        // Probe strictly above lower so a multibyte character cannot stall the search.
        let end = contents.ceil_char_boundary(lower.midpoint(upper).saturating_add(1));
        let candidate = response(end, Some(pagination_cursor(fingerprint, end)));
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

fn pagination_cursor(fingerprint: u64, offset: usize) -> String {
    format!("{fingerprint:016x}:{offset}")
}

fn parse_pagination_cursor(
    cursor: Option<&str>,
    contents: &str,
    cached_fingerprint: &OnceCell<u64>,
) -> Result<usize, FunctionCallError> {
    let Some(cursor) = cursor else {
        return Ok(0);
    };
    let invalid = || FunctionCallError::RespondToModel("skills.read cursor is invalid".to_string());
    let (fingerprint, offset) = cursor.split_once(':').ok_or_else(invalid)?;
    if u64::from_str_radix(fingerprint, 16).ok()
        != Some(*cached_fingerprint.get_or_init(|| value_fingerprint(contents)))
    {
        return Err(FunctionCallError::RespondToModel(
            "skills.read cursor is stale; restart from the first page".to_string(),
        ));
    }
    offset.parse::<usize>().map_err(|_| invalid())
}

pub(crate) fn value_fingerprint(value: &(impl Hash + ?Sized)) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

pub(super) fn serialized_len(value: &impl Serialize) -> Result<usize, FunctionCallError> {
    serde_json::to_vec(value)
        .map(|value| value.len())
        .map_err(|err| FunctionCallError::Fatal(err.to_string()))
}
