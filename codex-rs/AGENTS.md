# codex-rs Rust workspace policy

This file applies inside `codex-rs`. It inherits the repository-wide local-build
policy from the root `AGENTS.md`.

Keep this parent file compact. Put crate-specific rules in nested `AGENTS.md`
files only when they are important enough to load automatically for that subtree.
For background architecture, examples, and reference material, use README files.

## Scoped Policy

Known scoped instruction files include:

- `core/AGENTS.md`: core runtime, model context, tool execution, evidence, and
  session flow.
- `shell-command/AGENTS.md`: shell command execution and environment handling.

Do not assume other nested `AGENTS.md` files exist unless they are present in the
working tree.

## Fast Routing

Use this section first, then read `SOURCE_MAP.md` only when ownership remains
ambiguous or the task touches a cross-cutting surface.

- CLI, login, auth, plugins from the CLI, marketplace, daemon, and completions:
  `cli`, `login`, `aws-auth`, `keyring-store`, `secrets`.
- Desktop/app-server API, thread/turn lifecycle, streaming, schemas, and
  SDK-facing contracts: `app-server`, `app-server-protocol`, `app-server-client`,
  plus `protocol` for shared CLI/TUI/app-server wire types.
- App-server transport, daemon, local smoke harness, remote-control IO, and
  workflow-client behavior: `app-server-transport`, `app-server-daemon`,
  `app-server-test-client`.
- Core runtime, model context, tool execution, approvals, sandbox policy,
  compaction, rollout recording, exec policy, and session orchestration: `core`,
  `core-api`, `exec-server`, `execpolicy`, `execpolicy-legacy`.
- Code-mode host and protocol integration: `code-mode`, `code-mode-host`,
  `code-mode-protocol`.
- Config loading, profiles, permissions, requirements, MCP config, hooks,
  keymaps, and feature-gated config structs: `config`, plus `features` for
  feature keys/defaults.
- TUI rendering, input handling, status, chat widgets, snapshots, and Ratatui
  style: `tui`.
- Built-in tools, shell execution/escalation, filesystem access, file search/watch,
  and terminal detection: `tools`, `shell-command`, `shell-escalation`,
  `file-system`, `file-search`, `file-watcher`, `terminal-detection`.
- Persistence, thread lists, session metadata, state DB, rollout files, history,
  goals, memories, and logs: `state`, `thread-store`, `rollout`,
  `rollout-trace`, `message-history`, `memories/*`.
- Telemetry, runtime metrics, request diagnostics, feedback, and response debug
  context: `otel`, `analytics`, `feedback`, `response-debug-context`, plus
  `core` or `exec` when request wiring or JSONL turn events change.
- Model/provider catalogs, backend clients, generated OpenAPI models, ChatGPT
  bridge code, and local model integrations: `model-provider`,
  `model-provider-info`, `models-manager`, `codex-client`, `codex-api`,
  `backend-client`, `codex-backend-openapi-models`, `chatgpt`,
  `responses-api-proxy`, `lmstudio`, `ollama`, `realtime-webrtc`.
- Connectors, MCP, extensions, skills, templates, and plugin behavior:
  `connectors`, `codex-mcp`, `rmcp-client`, `mcp-server`, `ext/*`, `plugin`,
  `core-plugins`, `core-skills`, `skills`, `collaboration-mode-templates`,
  `context-fragments`.
- Model-visible prompts and template behavior: `prompts`.
- Network proxying, process hardening, Codex home/install context, patch
  application, CLI arg0 dispatch, and sandbox helpers: `network-proxy`,
  `process-hardening`, `codex-home`, `install-context`, `apply-patch`, `arg0`,
  `sandboxing`, `linux-sandbox`, `windows-sandbox-rs`.
- Cloud task, remote task, agent identity, graph state, and external-agent
  migration/session behavior: `cloud-config`, `cloud-tasks`,
  `cloud-tasks-client`, `cloud-tasks-mock-client`, `agent-identity`,
  `agent-graph-store`, `external-agent-migration`, `external-agent-sessions`.
- Build metadata, Bazel, Cargo workspace, toolchain, dependency pins, and
  Bubblewrap helper build inputs: root files under `codex-rs`, `bwrap`, and
  checked-in patch inputs.
- Support crates, sample binaries, UDS helpers, and narrow shared utilities:
  `test-binary-support`, `thread-manager-sample`, `stdio-to-uds`, `uds`,
  `ansi-escape`, `async-utils`, `git-utils`, `codex-experimental-api-macros`,
  and `utils/*`.

For review, recommendation, agreement, reasons, or `what would you fix` requests,
stay non-mutating unless the user explicitly asks for edits.

## Workspace Rules

- Prefer existing repo helpers, crate boundaries, and local patterns before adding
  new abstractions.
- For broad claims such as "all", "every", "complete", or "repo-wide", perform
  and cite an appropriate closure sweep. If the sweep is partial, state the exact
  inspected scope and remaining risk instead of implying broader coverage.
- During a specific implementation, ignore unrelated dirty-worktree changes,
  untracked files, generated artifacts, and failures outside the accepted task
  scope. If code changes overlap with existing local edits or competing code
  paths, stop and compare the versions. Keep or add the better version for the
  requested behavior, integrate compatible improvements where practical, and
  continue without reverting unrelated work.
- Prefer `SOURCE_MAP.md`, crate-local README files, Cargo workspace metadata, and
  manifest entrypoints before hard-coded path heuristics.
- If the user says no tests, do not run Rust/Cargo/nextest test commands. Use
  relevant non-test checks and report skipped test commands.
- Do not launch multiple normal Cargo/`just test`/`just fix` commands concurrently
  against the shared `codex-rs/target`. Use `just test-lane`, `just cargo-lane`,
  or another isolated lane when another Rust build is active or parallel work is
  needed.
- Do not delete package caches, lane caches, or `target` directories while Rust
  jobs may be running. Use `just rust-build-doctor`, `just target-disk`, and
  `just target-prune` for target cleanup.
- Keep generated schema, snapshot, lockfile, vendor, and Bazel metadata edits tied
  to the owning source change and generator. Do not hand-edit generated outputs
  unless the owning workflow explicitly requires it.
- If Rust dependencies change, refresh Bazel lock state with the repo recipe and
  include the lockfile update in the same change.
- If adding `include_str!`, `include_bytes!`, `sqlx::migrate!`, or similar
  compile-time file reads, update the crate `BUILD.bazel` data inputs as needed.
- Preserve public CLI flags, app-server APIs, config loading, sandbox behavior,
  stored sessions, rollout compatibility, and installed-binary behavior unless the
  user explicitly asks to change that surface.
- Rust changes that affect Codex Desktop behavior are not desktop-visible until
  the local publish/restart proof chain from the root `AGENTS.md` succeeds.

## Tool Use

- Before non-trivial tool work, send one brief preamble grouping the next actions.
- Use `rg` or `rg --files` for plain text and file-name discovery when
  available.
- Prefer installed purpose-built local tools when they are present and clearly
  fit the task: prefer `fd` for file discovery, `jq`/`yq` for JSON/YAML
  inspection, `delta` for readable Git diffs, `ast-grep` for syntax-aware search,
  `just` recipes before lower level build/test/publish commands, `cargo
  nextest`/`cargo shear`/`cargo audit`/`cargo deny` for Rust test and
  dependency-health workflows when applicable, `taplo`/`dprint` for configured
  formatting or validation, and `hyperfine`/`tokei` for performance and sizing
  measurements.
- Use source-discovery tools when available for model/runtime discovery.
- Use `apply_patch` for manual edits.
- Do not add new dependencies, install global tools, or change environment setup
  unless necessary and explained; never publish or commit unless asked.
- Do not re-read large files unnecessarily after a successful patch; inspect
  targeted diffs instead.

## Validation

Choose the smallest useful proof for the touched surface:

- Focused Rust crate changes: use `just test-fast -p <crate>` or the closest
  focused recipe/test filter.
- Config schema changes: run focused config/core validation and regenerate with
  `just write-config-schema`.
- App-server protocol/schema changes: run focused app-server/app-server-protocol
  validation and regenerate with `just write-app-server-schema` when the wire
  contract changed.
- Dependency changes: run the relevant Rust check/test plus Bazel lock update/check
  recipes.
- Formatting-only or docs-only changes: use `git diff --check` on touched files;
  do not claim runtime behavior changed.

## Test Conventions

- New Rust test modules should usually live in sibling `*_tests.rs` files and be
  wired with `#[path = "..."] mod tests;`.
- Prefer comparing whole objects with `assert_eq!` over asserting fields one by
  one. Use `pretty_assertions::assert_eq` where existing tests do.
- Avoid mutating process environment in tests when a passed dependency or flag is
  practical.
- Prefer `codex_utils_cargo_bin::cargo_bin` and `find_resource!` for first-party
  binaries/resources so tests work under Cargo and Bazel.
- For core end-to-end tests, prefer `core_test_support::responses` helpers and
  structured request assertions over manual JSON digging.
