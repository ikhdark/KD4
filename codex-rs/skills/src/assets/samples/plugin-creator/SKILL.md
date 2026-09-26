---
name: plugin-creator
description: Create and scaffold plugin directories for Codex with a required `.codex-plugin/plugin.json`, optional plugin folders/files, valid manifest defaults, and personal-marketplace entries by default. Use when Codex needs to create a new personal plugin, add optional plugin structure, generate or update marketplace entries for plugin ordering and availability metadata, or update an existing local plugin during development with the CLI-driven cachebuster and reinstall flow.
---

# Plugin Creator

Create or update a plugin using the existing scaffold and validation helpers. Run commands from this skill's root. Preserve the requested destination and existing metadata.

## Create or update

| Work | Procedure |
|---|---|
| New standalone plugin | `python3 scripts/create_basic_plugin.py <plugin-name>` creates `~/plugins/<plugin-name>`. |
| New personal marketplace plugin | Add `--with-marketplace`; the default marketplace is `~/.agents/plugins/marketplace.json`. |
| Explicit repo/team destination | Supply both `--path <parent-plugin-directory>` and `--marketplace-path <marketplace-json-path>`, plus `--with-marketplace`. |
| Existing local plugin | Read [installing-and-updating.md](references/installing-and-updating.md), then use `python3 scripts/update_plugin_cachebuster.py <plugin-path>` and its reinstall flow. Prefer the default cachebuster unless an override was requested; do not hand-edit marketplace configuration for updates. |

Use the equivalent user-profile paths on Windows. Request only needed companion resources with `--with-skills`, `--with-hooks`, `--with-scripts`, `--with-assets`, `--with-mcp`, or `--with-apps`.

The helper normalizes names to lowercase hyphen-case (spaces, underscores, and punctuation become hyphens; repeated hyphens collapse), at most 64 characters. The outer folder and manifest name must match. Keep `.codex-plugin/plugin.json`, valid defaults, and no unfinished placeholders. Edit metadata when the request supplies it. Include `apps` or `mcpServers` only with their companion files; omit unsupported manifest fields, including `hooks`. Use `--force` only for intentional replacement.

## Marketplace contract

- Default to the personal marketplace; repo/team placement is opt-in. Its default file is discovered implicitly: do not instruct `codex plugin marketplace add` for this path.
- For an explicit non-default marketplace, ensure it is installed before reinstall instructions; use `codex plugin marketplace add <path-to-marketplace-root>` when missing.
- Use `--marketplace-name` only to seed a different new marketplace when `personal` is already taken/installed. Never rename an existing file through this option; its top-level name must match.
- Read names with `scripts/read_marketplace_name.py [--marketplace-path <marketplace.json>]`. With no argument it reads the personal marketplace.
- Preserve existing `interface.displayName`. It belongs at marketplace-root `interface`, not inside `plugins[]`.
- Append entries unless reordering was requested; array order is Codex render order. Keep `source.path` relative to the marketplace root as `./plugins/<plugin-name>`.
- Always write `policy.installation`, `policy.authentication`, and `category`. Defaults are `AVAILABLE` and `ON_INSTALL`; use other allowed values only when requested. Installation allows `NOT_AVAILABLE|AVAILABLE|INSTALLED_BY_DEFAULT`; authentication allows `ON_INSTALL|ON_USE`. Omit `policy.products` unless explicit product gating was requested.
- If writing requires approval under the active policy, obtain it. If the user elects to run the write, give the exact scaffold command and continue from validation or subsequent edits.

For a new marketplace, use this shape; see [plugin-json-spec.md](references/plugin-json-spec.md) for the complete manifest and entry schema:

```json
{
  "name": "personal",
  "interface": { "displayName": "Personal" },
  "plugins": [{
    "name": "plugin-name",
    "source": { "source": "local", "path": "./plugins/plugin-name" },
    "policy": { "installation": "AVAILABLE", "authentication": "ON_INSTALL" },
    "category": "Productivity"
  }]
}
```

## Validate and hand off

Before returning a generated plugin, run:

```bash
python3 scripts/validate_plugin.py <plugin-path>
```

After editing this skill, run `python3 ../skill-creator/scripts/quick_validate.py .`.

When a marketplace entry was created or updated, end with "To view this in the Codex app:" and Markdown links labeled `View <normalized plugin name>` and `Share <normalized plugin name>`. Use `codex://plugins/<normalized plugin name>?marketplacePath=<absolute marketplace.json path>`, adding `&mode=share` for Share. Substitute actual values and URL-encode the path segment/query value. Do not add `pluginName` or `hostId`, and omit these links when no marketplace entry changed.
