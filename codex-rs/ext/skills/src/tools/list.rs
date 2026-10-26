use codex_extension_api::ConversationHistoryRequirement;
use codex_extension_api::FunctionCallError;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolExecutor;
use codex_extension_api::ToolExecutorFuture;
use codex_extension_api::ToolName;
use codex_extension_api::ToolPayload;
use codex_extension_api::ToolSpec;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use std::borrow::Cow;

use crate::catalog::SkillCatalogEntry;
use crate::catalog::SkillDiscoveryContinuation;
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
    /// Exact, case-sensitive skill name. Mutually exclusive with query.
    name: Option<String>,
    /// ASCII-case-insensitive substring of a skill's name or description.
    query: Option<String>,
}

#[derive(Clone, Debug, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[schemars(deny_unknown_fields)]
struct ListedSkill<'a> {
    authority: SkillToolAuthority,
    package: &'a str,
    name: &'a str,
    description: Cow<'a, str>,
    main_resource: &'a str,
}

#[derive(Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[schemars(deny_unknown_fields)]
struct ListResponse<'a> {
    skills: Vec<ListedSkill<'a>>,
    warnings: Vec<String>,
    warnings_omitted: usize,
    next_cursor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    search_complete: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    discovery_blocked: Option<bool>,
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
            "List a page of enabled skills owned by the requested authority. Only orchestrator-owned skills are currently supported. Use name for an exact name or query for a substring search (not both, at most 256 bytes). search_complete=false means no-match is not definitive. discovery_blocked=true means discovery cannot advance until the MCP resource generation changes; known skills remain readable. Returns opaque package and main-resource handles for skills.read. Pass next_cursor back as cursor with the same filters to continue; restart from the first page if stale. An explicit first-page request retries failed discovery, but not blocked pagination.",
        )
    }

    fn conversation_history_requirement(
        &self,
        _payload: &ToolPayload,
    ) -> ConversationHistoryRequirement {
        ConversationHistoryRequirement::None
    }

    fn handle(&self, call: ToolCall) -> ToolExecutorFuture<'_> {
        Box::pin(async move {
            let args: ListArgs = parse_args(&call)?;
            if (args.name.is_some() && args.query.is_some())
                || [&args.name, &args.query].into_iter().flatten().any(|value| value.trim().is_empty() || value.len() > 256)
            {
                return Err(FunctionCallError::RespondToModel(
                    "skills.list accepts either name or query, each nonempty and at most 256 bytes".to_string(),
                ));
            }
            let query = args.query.as_deref().map(str::to_ascii_lowercase);
            let search = args.name.is_some() || query.is_some();
            let budget = call.response_byte_budget(MAX_LIST_RESPONSE_BYTES);
            if args.cursor.is_none() {
                self.context
                    .thread_state
                    .retry_failed_orchestrator_catalog();
            }
            let authority = args.authority.into_authority();
            let mut catalog = self.context.catalog(&call.turn_id, args.authority).await;
            let mut skills = matching_entries(&catalog, &args, query.as_deref(), &authority);
            let start = match args.cursor.as_deref() {
                None => 0,
                Some(cursor) => {
                    let (hash, offset) = cursor.split_once(':').ok_or_else(invalid_cursor)?;
                    let offset = offset
                        .parse::<usize>()
                        .ok()
                        .filter(|offset| *offset <= skills.len())
                        .ok_or_else(invalid_cursor)?;
                    if u64::from_str_radix(hash, 16).ok()
                        != Some(list_fingerprint(&skills[..offset], &args))
                    {
                        return Err(FunctionCallError::RespondToModel(
                            "skills.list cursor is stale; restart from the first page".to_string(),
                        ));
                    }
                    offset
                }
            };
            if catalog.continuation.is_some()
                && !catalog.discovery_blocked()
                && (args.cursor.is_none() || start == skills.len())
            {
                drop(skills);
                catalog = self
                    .context
                    .continue_catalog(&call.turn_id, catalog.entries.len())
                    .await;
                skills = matching_entries(&catalog, &args, query.as_deref(), &authority);
            }
            let incomplete = catalog.continuation.is_some();
            // Warnings can represent failed provider discovery; never turn
            // that absence into a definitive targeted-search miss.
            let search_complete = search.then_some(
                !incomplete
                    && catalog.warnings.is_empty()
                    && catalog.entries.iter().filter(|entry| entry.enabled && entry.authority == authority).all(|entry| {
                        is_bounded_handle(&entry.id.0, MAX_HANDLE_BYTES)
                            && is_bounded_handle(entry.main_prompt.as_str(), MAX_HANDLE_BYTES)
                    }),
            );
            let (warnings, warnings_omitted) = bounded_warnings(&catalog.warnings);
            let response = page_response(
                &skills,
                warnings,
                warnings_omitted,
                start,
                catalog.continuation.as_ref(),
                budget,
                &args,
                search_complete,
            )?;

            external_json_output(&response)
        })
    }
}

fn invalid_cursor() -> FunctionCallError {
    FunctionCallError::RespondToModel("skills.list cursor is invalid".to_string())
}

fn matching_entries<'a>(
    catalog: &'a crate::catalog::SkillCatalog,
    args: &ListArgs,
    query: Option<&str>,
    authority: &crate::catalog::SkillAuthority,
) -> Vec<&'a SkillCatalogEntry> {
    let contains = |text: &str, query: &str| text.as_bytes().windows(query.len())
        .any(|part| part.eq_ignore_ascii_case(query.as_bytes()));
    catalog.entries.iter()
        .filter(|entry| entry.enabled && &entry.authority == authority)
        .filter(|entry| args.name.as_ref().is_none_or(|name| entry.name == *name))
        .filter(|entry| query.is_none_or(|query| contains(&entry.name, query)
            || contains(&entry.description, query)
            || entry.short_description.as_deref().is_some_and(|text| contains(text, query))))
        .filter(|entry| is_listable(entry))
        .collect()
}

fn page_response<'a>(
    skills: &[&'a SkillCatalogEntry],
    warnings: Vec<String>,
    warnings_omitted: usize,
    start: usize,
    continuation: Option<&SkillDiscoveryContinuation>,
    budget: usize,
    args: &ListArgs,
    search_complete: Option<bool>,
) -> Result<ListResponse<'a>, FunctionCallError> {
    let count = skills.len();
    let incomplete = continuation.is_some_and(|progress| !progress.blocked);
    let mut response = ListResponse {
        skills: Vec::new(),
        warnings,
        warnings_omitted,
        next_cursor: incomplete.then(|| {
            format!(
                "{:016x}:{start}",
                0_u64
            )
        }),
        search_complete,
        discovery_blocked: continuation.filter(|progress| progress.blocked).map(|_| true),
    };
    // Make room for complete handles before including advisory warnings.
    while super::read::serialized_len(&response)? > budget && !response.warnings.is_empty() {
        response.warnings.pop();
        response.warnings_omitted += 1;
    }
    for (index, skill) in skills.iter().enumerate().skip(start) {
        let next_cursor = (index + 1 < count || incomplete).then(|| {
            format!(
                "{:016x}:{}",
                0_u64,
                index + 1
            )
        });
        let previous_cursor = response.next_cursor.clone();
        response.next_cursor = next_cursor;
        response.skills.push(listed_skill(skill));
        if super::read::serialized_len(&response)? > budget {
            if response.skills.len() > 1 {
                response.skills.pop();
                response.next_cursor = previous_cursor;
                break;
            }
            // A single long description must not prevent discovery of its intact handles.
            if let Some(skill) = response.skills.last_mut() {
                skill.description = Cow::Borrowed("");
            }
            while super::read::serialized_len(&response)? > budget && !response.warnings.is_empty()
            {
                response.warnings.pop();
                response.warnings_omitted += 1;
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
    let end = start + response.skills.len();
    response.next_cursor = (end < count || incomplete)
        .then(|| format!("{:016x}:{end}", list_fingerprint(&skills[..end], args)));
    Ok(response)
}

fn list_fingerprint(skills: &[&SkillCatalogEntry], args: &ListArgs) -> u64 {
    // Use the original slice hash, including its length prefix. This
    // borrowed projection is built once for cursor validation/publication, not
    // once for every tentative row during response fitting.
    let listed = skills.iter().map(|entry| listed_skill(entry)).collect::<Vec<_>>();
    super::read::value_fingerprint(&(args.name.as_deref(), args.query.as_deref(), listed.as_slice()))
}

fn is_listable(entry: &SkillCatalogEntry) -> bool {
    SkillToolAuthority::from_authority(&entry.authority).is_some()
        && is_bounded_handle(&entry.id.0, MAX_HANDLE_BYTES)
        && is_bounded_handle(entry.main_prompt.as_str(), MAX_HANDLE_BYTES)
}

fn listed_skill(entry: &SkillCatalogEntry) -> ListedSkill<'_> {
    ListedSkill {
        authority: SkillToolAuthority::Orchestrator,
        package: &entry.id.0,
        name: &entry.name,
        description: truncate_catalog_skill_description(
            entry
                .short_description
                .as_deref()
                .unwrap_or(&entry.description),
        ),
        main_resource: entry.main_prompt.as_str(),
    }
}

fn bounded_warnings(warnings: &[String]) -> (Vec<String>, usize) {
    let omitted = warnings.len().saturating_sub(MAX_WARNINGS);
    let warnings = warnings
        .iter()
        .take(MAX_WARNINGS)
        .map(|warning| {
            if warning.len() <= MAX_WARNING_BYTES {
                warning.clone()
            } else {
                let (prefix, _) = truncate_utf8_to_bytes(&warning, MAX_WARNING_BYTES - 3);
                format!("{prefix}...")
            }
        })
        .collect();
    (warnings, omitted)
}
