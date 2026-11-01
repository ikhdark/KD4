# Updating Existing Local Plugins

Use this reference for an existing local plugin whose marketplace entry already points at the
source being edited. For initial plugin or marketplace creation, use the scaffold flow in
[SKILL.md](../SKILL.md). Commands below are relative to this skill's root.

## Update loop

1. Resolve the marketplace before changing or reinstalling the plugin. Reuse an already confirmed
   local source binding; otherwise use `codex plugin list` and inspect the selected entry.
   If it is non-local or points at a different source, stop and resolve that mismatch first.
   This flow does not rewrite marketplace entries or hand-edit `config.toml`.

2. Read that marketplace's name:

   ```bash
   python3 scripts/read_marketplace_name.py
   ```

   With no argument, the helper reads the personal marketplace at
   `~/.agents/plugins/marketplace.json` (the equivalent user-profile path on Windows).
   That marketplace is discovered implicitly: do not add it with
   `codex plugin marketplace add`, or use `codex plugin marketplace list` to decide whether
   its file exists.

   For another local marketplace, pass `--marketplace-path <path-to-marketplace.json>`.
   Confirm it is configured; if missing and installation is authorized, first run
   `codex plugin marketplace add <path-to-marketplace-root>`.

3. Update the plugin manifest's cachebuster:

   ```bash
   python3 scripts/update_plugin_cachebuster.py <plugin-path>
   ```

   By default the helper uses a UTC timestamp down to seconds. Use
   `--cachebuster local-20260519-184516` only when explicitly requested or when an external
   workflow requires a specific token. Preserve the version prefix before `+` and replace
   its suffix with `+codex.<cachebuster>`; do not append cachebusters or increment numeric
   versions merely to trigger reinstall.

   Examples:

   - `0.1.0` → `0.1.0+codex.local-20260519-184516`
   - `0.1.0+codex.old-token` → `0.1.0+codex.local-20260519-184516`
   - `1.2.3-beta.1+codex.prev` → `1.2.3-beta.1+codex.local-20260519-184516`
   - `dev-build+other-tag` → `dev-build+codex.local-20260519-184516`

4. Reinstall once, using the name returned for the confirmed marketplace:

   ```bash
   codex plugin add <plugin-name>@<marketplace-name>
   ```

5. Tell the user to start a new thread to pick up the updated plugin's skills and MCP tools.
