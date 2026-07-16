# KD4 Repository Source Map

KD4 is the user's local fork of
[`openai/codex`](https://github.com/openai/codex), with its home repository at
[`ikhdark/KD4`](https://github.com/ikhdark/KD4). This file maps ownership,
entrypoints, contracts, and validation surfaces across the checkout.

This map is navigation, not editing policy. Read [`AGENTS.md`](AGENTS.md) and
the nearest scoped `AGENTS.md` before changing files. Use this map when ownership
is unclear, a task crosses directories, or a change must be traced from an
entrypoint to generated, package, SDK, or desktop-visible outputs.

- Product documentation: [OpenAI Codex documentation](https://developers.openai.com/codex)
- Local installation and build guidance: [`docs/install.md`](docs/install.md)
- Contribution guidance: [`docs/contributing.md`](docs/contributing.md)
- Configuration guidance: [`docs/config.md`](docs/config.md)
- License: [`LICENSE`](LICENSE)

<!-- Begin ToC -->

- [How to use this map](#how-to-use-this-map)
- [Primary runtime entrypoints](#primary-runtime-entrypoints)
- [Top-level ownership](#top-level-ownership)
- [Rust workspace routing](#rust-workspace-routing)
- [Build, package, and publish paths](#build-package-and-publish-paths)
- [KD4 extension boundary](#kd4-extension-boundary)
- [Contracts and generated artifacts](#contracts-and-generated-artifacts)
- [Documentation and policy](#documentation-and-policy)
- [Cross-cutting change routes](#cross-cutting-change-routes)

<!-- End ToC -->

## How to use this map

1. Start with the root and nearest scoped `AGENTS.md` files.
2. Use the primary entrypoint table when the user-facing path is known.
3. Use the ownership tables when the change spans packages or languages.
4. Identify contract and generated-output owners before editing schemas,
   lockfiles, package artifacts, or publish/install paths.
5. Return to the applicable `AGENTS.md` for the exact validation and final
   reporting rules.

For a clear crate-local or script-local task, skip this map and use the closest
owner documentation directly.

## Primary runtime entrypoints

| Surface | Primary entrypoint | Follow-on owners |
| --- | --- | --- |
| Codex multitool CLI | `codex-rs/cli/src/main.rs` | `codex-rs/cli/src/lib.rs`, `codex-rs/core`, `codex-rs/tui`, `codex-rs/exec` |
| Standalone TUI | `codex-rs/tui/src/main.rs` | `codex-rs/tui/src/lib.rs`, `codex-rs/core`, `codex-rs/protocol` |
| Core agent runtime | `codex-rs/core/src/lib.rs` | sessions, tools, config, models, approvals, sandboxing, rollout, and state crates |
| App server | `codex-rs/app-server/src/main.rs` | `codex-rs/app-server/src/lib.rs`, `codex-rs/app-server-protocol`, `codex-rs/protocol` |
| MCP server | `codex-rs/mcp-server/src/main.rs` | `codex-rs/mcp-server/src/lib.rs`, `codex-rs/core`, `codex-rs/rmcp-client` |
| npm CLI wrapper | `codex-cli/bin/codex.js` | `codex-cli/package.json`, native package staging, installed binary discovery |
| TypeScript SDK | `sdk/typescript/src/index.ts` | `sdk/typescript/src`, app-server contracts |
| Python SDK | `sdk/python/src/openai_codex/__init__.py` | `sdk/python/src/openai_codex`, generated API models, app-server contracts |

## Top-level ownership

| Path | Owns |
| --- | --- |
| `.codex/` | Repo-local Codex policy and routing, worktree environment setup, fork-local skills, and ignored task, verification, and runtime state |
| `.github/` | CI, release, repository checks, issue templates, and automation |
| `codex-cli/` | npm-facing `@openai/codex` wrapper, native binary discovery, and package staging inputs |
| `codex-rs/` | Primary Rust workspace and nearly all CLI, runtime, app-server, TUI, tool, protocol, state, plugin, and sandbox behavior |
| `docs/` | User, contributor, configuration, authentication, sandbox, command, and skill documentation |
| `scripts/` | Build lanes, local publish, package assembly, installers, schema helpers, repository checks, and maintenance tooling |
| `sdk/` | TypeScript, Python, and Python runtime SDK/package surfaces |
| `third_party/` | Checked-in or vendored integration inputs; edit only through the owning workflow |
| `tools/` | Repository tooling outside the main Rust workspace |
| `justfile` | Preferred command router for focused build, test, schema, dependency, and local publish workflows |
| `package.json`, `pnpm-workspace.yaml` | Root formatting/maintenance commands and JavaScript workspace membership |

## Rust workspace routing

[`codex-rs/Cargo.toml`](codex-rs/Cargo.toml) is authoritative for workspace
membership. [`codex-rs/AGENTS.md`](codex-rs/AGENTS.md) owns detailed Rust routing
and validation.

| Concern | Primary crates or directories |
| --- | --- |
| CLI, login, auth, completions | `cli`, `login`, `aws-auth`, `keyring-store`, `secrets` |
| TUI and interactive presentation | `tui` |
| Headless execution and policy | `exec`, `exec-server`, `execpolicy`, `execpolicy-legacy` |
| Core runtime and orchestration | `core`, `core-api`, `context-fragments`, `prompts` |
| Config and features | `config`, `features`, `codex-home`, `install-context` |
| App-server lifecycle and transports | `app-server`, `app-server-protocol`, `app-server-client`, `app-server-transport`, `app-server-daemon`, `app-server-test-client` |
| Shared wire and event types | `protocol` |
| Built-in tools and shell execution | `tools`, `shell-command`, `shell-escalation`, `file-system`, `file-search`, `file-watcher`, `terminal-detection` |
| Approvals, patching, and sandboxing | `apply-patch`, `sandboxing`, `linux-sandbox`, `windows-sandbox-rs`, `process-hardening`, `network-proxy` |
| State and persistence | `state`, `thread-store`, `rollout`, `rollout-trace`, `message-history`, `memories/*` |
| Models and backend clients | `model-provider`, `model-provider-info`, `models-manager`, `codex-client`, `codex-api`, `backend-client`, `chatgpt`, `ollama`, `lmstudio` |
| Plugins, skills, MCP, and extensions | `plugin`, `core-plugins`, `core-skills`, `skills`, `codex-mcp`, `mcp-server`, `rmcp-client`, `connectors`, `ext/*` |
| Telemetry and diagnostics | `otel`, `analytics`, `feedback`, `response-debug-context` |
| Cloud and external agents | `cloud-config`, `cloud-tasks*`, `agent-identity`, `agent-graph-store`, `external-agent-*` |
| Shared support utilities | `utils/*`, `git-utils`, `async-utils`, `test-binary-support` |

## Build, package, and publish paths

| Flow | Owner and entrypoint |
| --- | --- |
| Rust build and tests | `codex-rs/Cargo.toml`, root `justfile`, crate-local tests, `codex-rs/AGENTS.md` |
| npm package staging | `scripts/stage_npm_packages.py`, `codex-cli/scripts/build_npm_package.py`, `codex-cli/` |
| Canonical package archives | `scripts/codex_package/` and `scripts/codex_package/AGENTS.md` |
| Platform installers | `scripts/install/install.sh`, `scripts/install/install.ps1`, `scripts/install/AGENTS.md` |
| Windows local runtime publish | `scripts/publish-local-codex.ps1` and `just publish-local-codex-*` recipes |
| TypeScript SDK package | `sdk/typescript/package.json`, `sdk/typescript/src`, `sdk/typescript/tests` |
| Python SDK package | `sdk/python/pyproject.toml`, `sdk/python/src`, `sdk/python/tests` |
| Python runtime package | `sdk/python-runtime/pyproject.toml`, `sdk/python-runtime/src` |
| CI and release | `.github/workflows/`, especially repository checks and Rust release workflows |

The installed Windows runtime target for this fork is
`C:\Users\kuh\Desktop\LOCAL-KD\codex.exe`. Source changes do not become visible
in Codex Desktop until the owning local publish and restart chain succeeds.

## KD4 extension boundary

Prefer the existing `codex-rs/ext/extension-api` registry before adding a
fork-only host hook. The accepted migration seams are:

| KD4 capability | Existing seam | Owning phase |
| --- | --- | --- |
| Repository-native tools such as `repo_query` | `ToolContributor` | Repository intelligence |
| Task evidence observation around tool execution | `ToolLifecycleContributor` | Completion evidence |
| Task Capsule context injection | `ContextContributor` | Durable task state |

Completion evaluation and app-server initialization receipts do not currently
have an equivalent contributor contract. Add those seams only in their owning
phase, after the required protocol/state shape is known. Do not introduce a
general KD4 hook framework up front.

## Contracts and generated artifacts

| Contract or output | Source owner | Update path |
| --- | --- | --- |
| App-server protocol/schema | `codex-rs/app-server`, `codex-rs/app-server-protocol`, `codex-rs/protocol` | Focused app-server tests plus `just app-server-schema-check`; intentional regeneration uses the owning force/generator recipe |
| Config schema | `codex-rs/config`, `codex-rs/features`, `codex-rs/core` | Focused config/core tests plus `just config-schema-check`; generated output is `codex-rs/core/config.schema.json` |
| npm package layout | `codex-cli`, `scripts/stage_npm_packages.py`, `scripts/codex_package/` | Package staging tests and dry-run/package inspection |
| Cargo dependency state | `codex-rs/Cargo.toml`, crate manifests, and `codex-rs/Cargo.lock` | Owning Cargo dependency and lock update commands; never hand-edit generated lock state |
| JavaScript dependency state | `package.json`, workspace package manifests | `pnpm-lock.yaml` through the owning package-manager workflow |
| Rust snapshots and schema fixtures | Owning crate tests or schema generator | Regenerate through the owning test or generator, then inspect focused diffs |
| Build outputs and vendored trees | `codex-rs/target`, `node_modules`, `codex-rs/vendor`, `third_party` | Do not hand-edit; rebuild, reinstall, or run the owning update workflow |

## Documentation and policy

| Need | Start here |
| --- | --- |
| Repository source routing | `SOURCEMAP.md`; validate with `just source-map-check` |
| Repository-wide editing policy | `AGENTS.md` |
| Rust workspace policy and crate routing | `codex-rs/AGENTS.md` |
| Script ownership and validation | `scripts/AGENTS.md`, then `scripts/README.md` |
| Repo-local Codex setup and durable task context | `.codex/AGENTS.md`, `.codex/README.md`, `.codex/skills/kd4-harness/SKILL.md` |
| Installation and local build | `docs/install.md` |
| Configuration | `docs/config.md`, `docs/example-config.md` |
| Authentication | `docs/authentication.md` |
| Sandbox and execution policy | `docs/sandbox.md`, `docs/execpolicy.md` |
| Skills and agent guidance | `docs/skills.md`, `docs/agents_md.md` |
| Package-specific background | The nearest crate/package `README.md` |

Operational rules belong in the closest `AGENTS.md`. Background, architecture,
and usage belong in the nearest package README or checked-in documentation. Keep
this source map focused on navigation and ownership so agents can load it only
when the task needs cross-cutting context.

## Cross-cutting change routes

| Change | Trace this path |
| --- | --- |
| CLI command or flag | `codex-rs/cli` -> command owner (`tui`, `exec`, auth, cloud, or app server) -> `core`/`protocol` when shared |
| TUI behavior | `codex-rs/tui` -> `core` and `protocol` contracts -> snapshots/focused TUI tests |
| Desktop-visible behavior | `app-server`/`core`/`protocol` -> schema when needed -> local binary build -> publish -> desktop restart/runtime proof |
| Config field or default | `config`/`features` -> consuming runtime path -> config schema -> focused config/core validation |
| Tool, shell, approval, or sandbox behavior | scoped `core`, `tools`, `shell-*`, `sandboxing`, or platform sandbox owner -> focused safety tests |
| Model/provider behavior | `model-provider*`, `models-manager`, backend/client crates -> `core` request path -> diagnostics/tests |
| Prompt or model-visible context | `prompts`, `core` prompt owners, `context-fragments` -> scoped prompt/core policy -> snapshots/tests |
| Plugin, skill, MCP, or extension behavior | `plugin`, `core-plugins`, `core-skills`, `skills`, `mcp-server`, `connectors`, `ext/*` -> registry/dispatch callers |
| Repo-local Codex policy, skill, or durable task context | `.codex/AGENTS.md` -> `.codex/README.md` -> targeted `.codex/skills` owner; `.codex/harness/runs` remains ignored local state |
| Stored thread/session behavior | `state`, `thread-store`, `rollout*`, `message-history`, `memories/*` -> app-server/TUI consumers |
| npm packaging or install behavior | `codex-cli` -> `scripts/stage_npm_packages.py`/`scripts/codex_package` -> installer/release workflow |
| SDK/API surface | app-server protocol/schema -> `sdk/typescript` and/or `sdk/python` -> generated artifacts and focused SDK tests |
| Dependency or build-system change | owning Cargo/package manifest -> Cargo lock state -> focused build/test/package proof |
