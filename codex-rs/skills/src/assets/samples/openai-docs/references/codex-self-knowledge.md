# Codex self-knowledge

Use for Codex behavior, setup, customization, troubleshooting, local state, and surface selection. Merely mentioning plugins, skills, hooks, MCP, or automation in a software task does not trigger this route.

## Manual-first source route

For broad Codex synthesis, use the manual before Docs MCP:

1. Reuse fresh same-thread manual and outline paths. Otherwise run `node <skill-dir>/scripts/fetch-codex-manual.mjs` in a normal writable session. Skip only for explicit read-only policy, unavailable shell execution, or no allowed temp cache; a guessed failure is insufficient.
2. Use its returned `codex-manual.md` and `codex-manual.outline.md` paths. The outline gives page/heading ranges; read only relevant sections and search those files for product facts.
3. Refresh when older than about a day, unusable, from another thread/uncertain provenance, or plausibly stale for missing current information. If asked whether the manual is current, run the helper when caching is allowed and report its returned status/paths.
4. Stop expanding sources for claims established by the manual; cite its source pages/known anchors and continue the broader task.
5. For helper failure/unavailability, missing or stale claims, or required page-specific citations, use one narrow `mcp__openaiDeveloperDocs__search_openai_docs` and fetch a relevant hit with `mcp__openaiDeveloperDocs__fetch_openai_doc`. Only then use official-domain web fallback if MCP is unavailable/unhelpful.

For an undocumented term, first search obvious adjacent manual concepts and explain the closest documented mapping. If the exact term remains material or likely current, use the narrow MCP gap-fill above. Unresolved model slugs, modes, entitlements, account access, or rollout labels do not justify leaving public official sources: state bounded uncertainty or route to support/admin/feedback.

Published product answers use the manual, permitted Docs MCP/official web fallback, and verified current-session capabilities. Disclose conflicts between public docs and callable capabilities, preferring verified behavior for that environment. Outside knowledge bases are not substitutes for this source route.

## Helper/cache mechanics

Resolve `<skill-dir>` to this skill's directory. The helper checks `$TMPDIR/openai-docs-cache`, `%TEMP%\\openai-docs-cache`, `%TMP%\\openai-docs-cache`, `/private/tmp/openai-docs-cache`, then `/tmp/openai-docs-cache`. Workspace-only write access does not establish permission to write a temp cache. Use `--cache-dir <cache-dir>` when needed.

The helper uses curl if native fetch is unavailable or proxy environment variables are present; no shell-specific proxy prefix is needed. On Windows it checks TEMP/TMP automatically. After successful retrieval, use the returned paths rather than searching the skill folder for product facts.

## Diagnostics

Check bundle availability, plugin installed/enabled state, connector authorization, MCP setup, workspace/admin policy, per-surface availability, and refresh/restart or new-task expectations, then support/feedback. API-key access alone does not imply ChatGPT, cloud-task, or connector access.

### Surface Map

When Codex nouns or durable-instruction surfaces overlap, recommend the smallest surface that matches the scope:

- Prompt or thread context -> one-off task constraints.
- `AGENTS.md` -> durable repo conventions, commands, verification steps, and review expectations; closer nested files apply under their subtree.
- Project `.codex/config.toml` -> trusted-repo Codex settings such as sandbox, MCP, hooks, model, or reasoning defaults.
- Global config or global guidance -> personal defaults across repos.
- Skill -> reusable task workflow with references or scripts.
- Plugin -> installable bundle with skills plus commands, tools, MCP config, hooks, assets, apps, or marketplace metadata.
- MCP server or app connector -> live external data/actions or authorized private app/workspace data. Use connectors for private Google Docs, Calendar, Slack, GitHub, Notion, and similar data instead of web search or model memory.
- Automation -> scheduled checks, reminders, monitors, or follow-up work; use a thread heartbeat when continuity in an existing thread matters.
- Hook -> lifecycle enforcement around tool calls, commands, or file edits.

Split mixed-scope requests instead of forcing one answer. Example: "always do X, but only for this PR" defaults to prompt/thread context for the current run; use `AGENTS.md` or project config only if it should persist, hooks only for mechanical enforcement, and automations only for scheduled or follow-up work.

Use this quick product map when needed: CLI is terminal-first local repo work; IDE extension is editor-attached coding; Codex app is desktop planning, review, and interactive work; cloud/web is hosted parallel/offloaded work; Browser Use/in-app browser is Codex-controlled web testing; Chrome extension uses the user's Chrome profile; Computer Use controls desktop apps and OS UI. Keep `config.toml` defaults, `requirements.toml` constraints, and managed/admin policy separate.

### Boundaries And Output

- Sandbox or network denials need scoped escalation with a clear justification. Destructive commands, writes outside the workspace, or broad access changes require explicit approval.
- Memory can provide user preference or context, but explicit prompt instructions win and memory is not a source for current external facts.
- For affirmative surface-selection answers, use this shape: recommendation, why, what to avoid, and the manual/source evidence used.
- When page-specific Codex citations are actually needed, these anchors often fit: `concepts/customization#agents-guidance` for `AGENTS.md`, `concepts/customization#skills` for skills, `plugins/build#plugin-structure` for plugins, `concepts/customization#mcp` for MCP, `config-advanced#hooks` for hooks, `app/automations#thread-automations` for thread automations, and `config-reference#configtoml` for config.
