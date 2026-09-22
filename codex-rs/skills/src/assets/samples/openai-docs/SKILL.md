---
name: "openai-docs"
description: "Use when the user asks how to build with OpenAI products or APIs, asks about Codex itself or choosing Codex surfaces, needs up-to-date official documentation with citations, help choosing the latest model for a use case, or model upgrade and prompt-upgrade guidance; use OpenAI docs MCP tools for non-Codex docs questions, use the Codex manual helper first for broad Codex self-knowledge, and restrict fallback browsing to official OpenAI domains."
---

# OpenAI Docs

Provide current, cited OpenAI product/API guidance and scoped model or prompt upgrades. For a generic software task that merely mentions Codex, continue that task directly.

## Select the source route

| Request | First source and follow-up |
|---|---|
| Codex behavior, setup, customization, troubleshooting, surfaces, or local-state synthesis | Read [Codex self-knowledge](references/codex-self-knowledge.md) and follow its manual-first route, including reuse, freshness, and fallback conditions. |
| Other OpenAI documentation | Search `mcp__openaiDeveloperDocs__search_openai_docs` with 2–6 essential terms, then fetch the relevant page/section with `mcp__openaiDeveloperDocs__fetch_openai_doc`. Narrow noisy searches; try a known plausible official URL with fetch before relying on web content. Use `list_openai_docs` only for discovery without a clear query. |
| API schemas, parameters, or required fields | Also verify with `mcp__openaiDeveloperDocs__get_openapi_spec` when available. |
| Model selection, latest/current/default model | Fetch [latest-model.md](https://developers.openai.com/api/docs/guides/latest-model.md); use [bundled latest-model](references/latest-model.md) only if unavailable. |
| Model or prompt upgrade | Preserve a named target. For unspecified/latest/current/default upgrades, run `node scripts/resolve-latest-model-info.js`, then fetch its migration and prompting guide URLs. Prefer explicit links from the latest-model page over guessed URLs. |

For non-Codex lookups, use official-domain web search only after Docs MCP is unavailable or unhelpful. For failed direct upgrade-guide fetches, use MCP/search to recover the same guide, then bundled references if necessary. Restrict fallback browsing to `developers.openai.com` and `platform.openai.com`. Disclose fallback use, cite actual sources, and keep quotations within policy limits.

Never substitute a newer model for an explicit target; mention newer guidance only as optional. Do not invent pricing, availability, account access, API parameters, or behavior. If sources disagree, explain and cite the difference; if public docs conflict with verified callable session behavior, disclose that before broad claims or edits. When sources cannot establish the answer, state the bounded uncertainty and a useful next step.

## Credentials and integration boundaries

Use `openai-platform-api-key` when available before authenticated API calls or live API tests. Offline implementation, examples, review, and conceptual work do not require credentials; continue that work when live access is unavailable.

A documentation lookup does not authorize installing an MCP server, configuration changes, escalation, or restarting Codex. Do not make those prerequisites for answering. Follow the active permission policy and existing user authorization.

## Upgrade scope

Prefer narrow, behavior-preserving model/prompt changes and prompt-only improvements when sufficient. Update active OpenAI API defaults and related prompts only when safe.

Leave historical docs, examples, eval baselines, fixtures, provider comparisons/registries, pricing tables, alias defaults, low-cost fallbacks, and ambiguous older usages unchanged unless requested. SDK, tooling, IDE, plugin, shell, auth, and provider-environment migrations require their own scope. If an upgrade needs API rewiring, schemas, tool handlers, or other implementation beyond model strings/prompts, explain the required scope and proceed only when authorized.

Read only needed references: [upgrade-guide.md](references/upgrade-guide.md) for unavailable remote upgrade guidance, [prompting-guide.md](references/prompting-guide.md) for prompt rewrites/fallback, and the Codex route above for product self-knowledge. Do not load all references.
