use codex_extension_api::FunctionCallError;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolExecutor;
use codex_extension_api::ToolExecutorFuture;
use codex_extension_api::ToolName;
use codex_extension_api::ToolSpec;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;

use crate::catalog::SkillCatalogEntry;
use crate::render::truncate_catalog_skill_description;
use crate::render::truncate_utf8_to_bytes;

use super::MAX_HANDLE_BYTES;
use super::SkillToolAuthority;
use super::SkillToolContext;
use super::external_json_output;
use super::is_bounded_handle;
use super::parse_args;
use super::skill_function_tool;
use super::skill_tool_name;

const TOOL_NAME: &str = "list";
const MAX_WARNINGS: usize = 4;
const MAX_WARNING_BYTES: usize = 256;
const MAX_LIST_RESPONSE_BYTES: usize = 8_000;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    authority: SkillToolAuthority,
    cursor: Option<String>,
}

#[derive(Debug, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[schemars(deny_unknown_fields)]
struct ListedSkill {
    authority: SkillToolAuthority,
    package: String,
    name: String,
    description: String,
    main_resource: String,
}

#[derive(Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[schemars(deny_unknown_fields)]
struct ListResponse {
    skills: Vec<ListedSkill>,
    warnings: Vec<String>,
    next_cursor: Option<String>,
}

#[derive(Clone)]
pub(super) struct ListTool {
    pub(super) context: SkillToolContext,
}

impl ToolExecutor<ToolCall> for ListTool {
    fn tool_name(&self) -> ToolName {
        skill_tool_name(TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        skill_function_tool::<ListArgs, ListResponse>(
            TOOL_NAME,
            "List a page of enabled skills owned by the requested authority. Only orchestrator-owned skills are currently supported. Returns opaque package and main-resource handles for skills.read. Pass next_cursor back as cursor to continue; restart from the first page if stale. An explicit first-page request retries failed discovery.",
        )
    }

    fn handle(&self, call: ToolCall) -> ToolExecutorFuture<'_> {
        Box::pin(async move {
            let args: ListArgs = parse_args(&call)?;
            let budget = call.response_byte_budget(MAX_LIST_RESPONSE_BYTES);
            if args.cursor.is_none() {
                self.context
                    .thread_state
                    .retry_failed_orchestrator_catalog();
            }
            let authority = args.authority.into_authority();
            let catalog = self.context.catalog(&call.turn_id, args.authority).await;
            let skills: Vec<_> = catalog
                .entries
                .into_iter()
                .filter(|entry| entry.enabled && entry.authority == authority)
                .filter_map(listed_skill)
                .collect();
            let warnings = bounded_warnings(catalog.warnings);
            let fingerprint = super::read::value_fingerprint(&(&skills, &warnings));
            let start = match args.cursor.as_deref() {
                None => 0,
                Some(cursor) => {
                    let (hash, offset) = cursor.split_once(':').ok_or_else(invalid_cursor)?;
                    if u64::from_str_radix(hash, 16).ok() != Some(fingerprint) {
                        return Err(FunctionCallError::RespondToModel(
                            "skills.list cursor is stale; restart from the first page".to_string(),
                        ));
                    }
                    offset
                        .parse::<usize>()
                        .ok()
                        .filter(|offset| *offset < skills.len())
                        .ok_or_else(invalid_cursor)?
                }
            };
            let response = page_response(skills, warnings, start, fingerprint, budget)?;

            external_json_output(&response)
        })
    }
}

fn invalid_cursor() -> FunctionCallError {
    FunctionCallError::RespondToModel("skills.list cursor is invalid".to_string())
}

fn page_response(
    skills: Vec<ListedSkill>,
    warnings: Vec<String>,
    start: usize,
    fingerprint: u64,
    budget: usize,
) -> Result<ListResponse, FunctionCallError> {
    let count = skills.len();
    let mut response = ListResponse {
        skills: Vec::new(),
        warnings,
        next_cursor: None,
    };
    // Make room for complete handles before including advisory warnings.
    while super::read::serialized_len(&response)? > budget && !response.warnings.is_empty() {
        response.warnings.pop();
    }
    for (index, skill) in skills.into_iter().enumerate().skip(start) {
        let next_cursor = (index + 1 < count).then(|| format!("{fingerprint:016x}:{}", index + 1));
        let previous_cursor = response.next_cursor.clone();
        response.next_cursor = next_cursor;
        response.skills.push(skill);
        if super::read::serialized_len(&response)? > budget {
            if response.skills.len() > 1 {
                response.skills.pop();
                response.next_cursor = previous_cursor;
                break;
            }
            // A single long description must not prevent discovery of its intact handles.
            if let Some(skill) = response.skills.last_mut() {
                skill.description.clear();
            }
            while super::read::serialized_len(&response)? > budget && !response.warnings.is_empty()
            {
                response.warnings.pop();
            }
            if super::read::serialized_len(&response)? > budget {
                return Err(FunctionCallError::RespondToModel("skills.list response budget leaves no room for a complete skill; increase the output budget".to_string()));
            }
        }
    }
    if super::read::serialized_len(&response)? > budget {
        return Err(FunctionCallError::RespondToModel(
            "skills.list response budget is too small".to_string(),
        ));
    }
    Ok(response)
}

fn listed_skill(entry: SkillCatalogEntry) -> Option<ListedSkill> {
    let authority = SkillToolAuthority::from_authority(&entry.authority)?;
    if !is_bounded_handle(&entry.id.0, MAX_HANDLE_BYTES)
        || !is_bounded_handle(entry.main_prompt.as_str(), MAX_HANDLE_BYTES)
    {
        return None;
    }

    Some(ListedSkill {
        authority,
        package: entry.id.0,
        name: entry.name,
        description: truncate_catalog_skill_description(
            entry
                .short_description
                .as_deref()
                .unwrap_or(&entry.description),
        )
        .into_owned(),
        main_resource: entry.main_prompt.as_str().to_string(),
    })
}

fn bounded_warnings(warnings: Vec<String>) -> Vec<String> {
    warnings
        .into_iter()
        .take(MAX_WARNINGS)
        .map(|warning| {
            let (warning, _) = truncate_utf8_to_bytes(&warning, MAX_WARNING_BYTES);
            warning
        })
        .collect()
}
