# Apps (Connectors) tool discovery

Searches apps/connectors tool metadata with BM25 and makes matching tool
definitions available on the next model call.

The following apps/connectors are available for tool discovery:

{{app_descriptions}}

When the user's request requires functionality from one of these
apps/connectors, first check whether a suitable tool is already active. If not,
use `tool_search` to search for the concrete action or capability needed.

For the apps/connectors listed above, always use `tool_search` rather than
`list_mcp_resources` or `list_mcp_resource_templates` to discover callable
tools.

Do not use `tool_search` for unrelated public web research, local repository
inspection, or tools that are already active.

After matching tools are loaded, invoke the appropriate tool on the next model
call. A successful tool search only exposes tools; it does not perform the
user's requested action.