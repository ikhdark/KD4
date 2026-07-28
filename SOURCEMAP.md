# KD4 Repository Source Map

KD4 is the user's local fork of
[`openai/codex`](https://github.com/openai/codex), with its home repository at
[`ikhdark/KD4`](https://github.com/ikhdark/KD4). This file maps repository
ownership, runtime entrypoints, package boundaries, contracts, generated
artifacts, validation routes, and the local install path.

Read [`AGENTS.md`](AGENTS.md) and the nearest scoped `AGENTS.md` before changing
files. This map is the cross-cutting source of truth when ownership is unclear,
a behavior crosses packages or languages, or a source change must be traced to
an SDK, schema, package, installed binary, or Codex Desktop.

- Product documentation: [OpenAI Codex documentation](https://developers.openai.com/codex)
- Local installation and build guidance: [`docs/install.md`](docs/install.md)
- Contribution guidance: [`docs/contributing.md`](docs/contributing.md)
- Configuration guidance: [`docs/config.md`](docs/config.md)
- License: [`LICENSE`](LICENSE)

<!-- Begin ToC -->

- [Maintenance contract](#maintenance-contract)
- [How to use this map](#how-to-use-this-map)
- [Runtime architecture](#runtime-architecture)
- [Top-level ownership](#top-level-ownership)
- [Instruction scopes](#instruction-scopes)
- [Runtime and executable entrypoints](#runtime-and-executable-entrypoints)
- [Rust package inventory](#rust-package-inventory)
- [Non-Rust project inventory](#non-rust-project-inventory)
- [Core runtime routing](#core-runtime-routing)
- [Extension boundary](#extension-boundary)
- [Persistence and stored state](#persistence-and-stored-state)
- [Contracts and generated artifacts](#contracts-and-generated-artifacts)
- [Build, package, publish, and install paths](#build-package-publish-and-install-paths)
- [Validation routes](#validation-routes)
- [Documentation and policy](#documentation-and-policy)
- [Cross-cutting change routes](#cross-cutting-change-routes)

<!-- End ToC -->

## Maintenance contract

`SOURCEMAP.md` is a required repository contract, not an optional overview.
Update it in the same change whenever the repository materially changes.

A change is material to this map when it does any of the following:

- adds, removes, renames, or repurposes a tracked top-level entry;
- adds, removes, or moves a Rust package, JavaScript/Python project manifest, or
  scoped `AGENTS.md`;
- changes a primary executable, SDK, daemon, generator, or runtime entrypoint;
- moves responsibility between crates, packages, scripts, or instruction scopes;
- changes ownership of a public protocol, configuration, stored-state,
  serialization, generated-artifact, or migration contract;
- changes the build, test, package, publish, install, or Desktop runtime-proof
  chain; or
- makes an ownership table or cross-cutting route incomplete or misleading.

An internal implementation change that remains inside an accurately mapped
owner and leaves all routes and contracts unchanged is not material to this
file.

The top-level, instruction, Rust package, and non-Rust project tables below are
machine-checked against tracked repository files. `just source-map-check`
validates those inventories, ASCII content, and this table of contents. The
check intentionally fails when structural drift requires a map decision. Do not
silence drift by adding a path alone: update the applicable ownership,
entrypoint, contract, and validation descriptions so the map remains useful.

## How to use this map

1. Read the root and nearest scoped `AGENTS.md`.
2. Start from the user-visible entrypoint or the package inventory.
3. Follow the relevant runtime, contract, state, and generated-output owners.
4. Use the cross-cutting route to identify consumers outside the first package.
5. Return to the applicable policy file for its exact validation and completion
   gate.

For a clear crate-local or script-local task, use the closest owner instructions
directly. Use this map when the route crosses boundaries or when a new boundary
must be placed.

## Runtime architecture

```mermaid
flowchart LR
    NPM["npm wrapper<br/>codex-cli"] --> CLI["Rust multitool CLI<br/>codex-rs/cli"]
    CLI --> TUI["Interactive TUI"]
    CLI --> EXEC["Headless exec"]
    CLI --> APP["App server"]
    CLI --> MCP["MCP server"]
    TUI --> CORE["Core session and turn runtime"]
    EXEC --> CORE
    APP --> CORE
    MCP --> CORE
    CORE --> TOOLS["Tools, shell, approvals, sandbox"]
    CORE --> MODELS["Model providers and backend clients"]
    CORE --> STATE["State DB, rollouts, thread store"]
    CORE --> EXT["Plugins, skills, MCP, extensions"]
    CORE --> PROTOCOL["Shared protocol and app-server protocol"]
    PROTOCOL --> SDK["TypeScript and Python SDKs"]
    PROTOCOL --> DESKTOP["Codex Desktop client"]
    CLI --> PUBLISH["Local build and publish"]
    PUBLISH --> LOCAL["LOCAL-KD/codex.exe"]
    LOCAL --> DESKTOP
```

The native Windows Desktop shell is not in this repository. Desktop-visible
completion requires the local binary publish and Desktop restart chain described
below.

## Top-level ownership

| Path | Owns |
| --- | --- |
| `.codex/` | Repo-local Codex configuration, environment setup, durable harness material, fork-local skills, and workspace policy |
| `.devcontainer/` | Development-container image, bootstrap, and container-local Codex installation inputs |
| `.vscode/` | Checked-in editor and workspace defaults |
| `codex-cli/` | npm-facing `@openai/codex` wrapper, native binary discovery, and npm package inputs |
| `codex-rs/` | Primary Rust workspace and nearly all CLI, runtime, app-server, TUI, tool, protocol, state, plugin, extension, and sandbox behavior |
| `docs/` | User, contributor, configuration, authentication, sandbox, execution-policy, command, and skill documentation |
| `scripts/` | Build lanes, local publish, package assembly, installers, schema helpers, repository checks, and maintenance tooling |
| `sdk/` | TypeScript SDK, Python SDK, and Python runtime package |
| `third_party/` | Checked-in integration or vendored inputs updated only through their owning workflow |
| `tools/` | Repository tooling outside the main Rust workspace |
| `.codespellignore`, `.codespellrc` | Spelling-check configuration and accepted vocabulary |
| `.gitattributes`, `.gitignore` | Git content and ignore behavior |
| `.markdownlint-cli2.yaml`, `.prettierignore`, `.prettierrc.toml` | Markdown and Prettier formatting policy |
| `.npmrc` | npm and pnpm behavior used by the JavaScript workspace |
| `AGENTS.md`, `SOURCEMAP.md` | Repository-wide editing policy and this cross-cutting ownership contract |
| `CHANGELOG.md`, `LICENSE`, `NOTICE` | Release history and legal notices |
| `flake.nix`, `flake.lock` | Nix development/build environment and its locked inputs |
| `justfile`, `kd4_features.toml` | Preferred command router and KD4 feature inventory |
| `package.json`, `pnpm-lock.yaml`, `pnpm-workspace.yaml` | Root maintenance commands, JavaScript dependency state, and workspace membership |

## Instruction scopes

| Path | Applies to |
| --- | --- |
| `AGENTS.md` | Entire repository; canonical shared policy plus KD4 project context |
| `.codex/AGENTS.md` | Repo-local Codex configuration, environments, skills, and harness routing |
| `codex-rs/AGENTS.md` | Rust workspace routing, safety constraints, build lanes, and validation |
| `codex-rs/core/AGENTS.md` | Core session, turn, context, tool, evidence, and model-runtime behavior |
| `codex-rs/prompts/AGENTS.md` | Model-visible prompt text, templates, and snapshot expectations |
| `codex-rs/protocol/AGENTS.md` | Shared event, item, configuration, permission, and compatibility types |
| `codex-rs/shell-command/AGENTS.md` | Shell parsing, environment, execution, and platform compatibility |
| `codex-rs/tui/src/bottom_pane/AGENTS.md` | Bottom-pane interaction, composer, overlays, rendering, and snapshots |
| `scripts/AGENTS.md` | Repository scripts, local publish, checks, packaging helpers, and validation |
| `scripts/codex_package/AGENTS.md` | Canonical CLI and app-server package directory/archive builder |
| `scripts/install/AGENTS.md` | Standalone shell and PowerShell installer contracts |

## Runtime and executable entrypoints

| Surface | Primary entrypoint | Follow-on owners |
| --- | --- | --- |
| npm `codex` launcher | `codex-cli/bin/codex.js` | `codex-cli/package.json`, staged native packages, platform binary discovery |
| Rust multitool CLI | `codex-rs/cli/src/main.rs` | CLI dispatch, login/auth, plugin/marketplace commands, TUI, exec, app-server, MCP, sandbox setup |
| CLI library support | `codex-rs/cli/src/lib.rs` | shared build info and exit-status helpers |
| Interactive TUI | `codex-rs/tui/src/main.rs` | `codex-rs/tui/src/lib.rs`, app/session routing, chat widget, bottom pane, core/protocol |
| Headless execution | `codex-rs/exec/src/main.rs` | `codex-rs/exec`, core, protocol, JSONL/event output |
| App server | `codex-rs/app-server/src/main.rs` | app-server library, protocol, transport, daemon, core |
| App-server exec transport | `codex-rs/app-server/src/bin/exec_server.rs` | app-server transport and process execution wiring |
| App-server test client | `codex-rs/app-server-test-client/src/main.rs` | app-server protocol/transport smoke paths |
| MCP server | `codex-rs/mcp-server/src/main.rs` | MCP server library, core, RMCP client, protocol |
| Code-mode host | `codex-rs/code-mode-host/src/main.rs` | code-mode runtime and protocol |
| Responses API proxy | `codex-rs/responses-api-proxy/src/main.rs` | Rust proxy library and `codex-rs/responses-api-proxy/npm/bin/codex-responses-api-proxy.js` |
| File search CLI | `codex-rs/file-search/src/main.rs` | file-search library and TUI/core consumers |
| Source search CLI | `codex-rs/file-search/src/source_search_main.rs` | structured source-content search |
| Patch application helper | `codex-rs/apply-patch/src/main.rs` | apply-patch parser/library and core tool wiring |
| Linux sandbox helper | `codex-rs/linux-sandbox/src/main.rs` | sandboxing policy and Bubblewrap helper |
| Windows sandbox setup | `codex-rs/windows-sandbox-rs/src/bin/setup_main/main.rs` | Windows sandbox installation and policy |
| Windows command runner | `codex-rs/windows-sandbox-rs/src/bin/command_runner/main.rs` | sandboxed Windows process execution |
| Rust local validation binary | `codex-rs/verify-local/src/main.rs` | standalone Rust verifier; the preferred scripted router is `scripts/verify_local.py` with `scripts/verify_local_rules.toml` |
| State log client | `codex-rs/state/src/bin/logs_client.rs` | state DB log queries and `just log` |
| Config schema writer | `codex-rs/core/src/bin/config_schema.rs` | config/core/features inputs and `codex-rs/core/config.schema.json` |
| App-server schema writers | `codex-rs/app-server-protocol/src/bin/export.rs`, `codex-rs/app-server-protocol/src/bin/write_schema_fixtures.rs` | app-server protocol schema tree and fixtures |
| Hook schema writer | `codex-rs/hooks/src/bin/write_hooks_schema_fixtures.rs` | hook type/schema sources and generated hook schemas |
| TypeScript SDK API | `sdk/typescript/src/index.ts` | SDK implementation/tests and app-server contracts |
| Python SDK API | `sdk/python/src/openai_codex/__init__.py` | Python client, generated models, tests, and app-server contracts |

## Rust package inventory

| Domain | Package roots |
| --- | --- |
| Workspace and repository tooling | `codex-rs`, `codex-rs/verify-local`, `tools/argument-comment-lint` |
| CLI, authentication, home, and install context | `codex-rs/arg0`, `codex-rs/aws-auth`, `codex-rs/cli`, `codex-rs/codex-home`, `codex-rs/install-context`, `codex-rs/keyring-store`, `codex-rs/login`, `codex-rs/secrets` |
| Interactive and headless clients | `codex-rs/tui`, `codex-rs/exec` |
| Core runtime, configuration, context, and prompts | `codex-rs/collaboration-mode-templates`, `codex-rs/config`, `codex-rs/context-fragments`, `codex-rs/core`, `codex-rs/core/tests/common`, `codex-rs/core-api`, `codex-rs/features`, `codex-rs/prompts` |
| App server and shared protocol | `codex-rs/app-server`, `codex-rs/app-server/tests/common`, `codex-rs/app-server-client`, `codex-rs/app-server-daemon`, `codex-rs/app-server-protocol`, `codex-rs/app-server-test-client`, `codex-rs/app-server-transport`, `codex-rs/protocol` |
| Code mode | `codex-rs/code-mode`, `codex-rs/code-mode-host`, `codex-rs/code-mode-protocol` |
| Tools, shell, exec policy, and hooks | `codex-rs/apply-patch`, `codex-rs/exec-server`, `codex-rs/exec-server-protocol`, `codex-rs/execpolicy`, `codex-rs/execpolicy-legacy`, `codex-rs/file-search`, `codex-rs/file-system`, `codex-rs/file-watcher`, `codex-rs/hooks`, `codex-rs/shell-command`, `codex-rs/shell-escalation`, `codex-rs/terminal-detection`, `codex-rs/tools` |
| Sandbox, network policy, and process hardening | `codex-rs/bwrap`, `codex-rs/linux-sandbox`, `codex-rs/network-proxy`, `codex-rs/process-hardening`, `codex-rs/sandboxing`, `codex-rs/windows-sandbox-rs` |
| State, threads, rollouts, history, and memories | `codex-rs/agent-task-store`, `codex-rs/memories/read`, `codex-rs/memories/write`, `codex-rs/message-history`, `codex-rs/rollout`, `codex-rs/rollout-trace`, `codex-rs/state`, `codex-rs/thread-store` |
| Models, backend clients, and network transports | `codex-rs/backend-client`, `codex-rs/chatgpt`, `codex-rs/codex-api`, `codex-rs/codex-backend-openapi-models`, `codex-rs/codex-client`, `codex-rs/http-client`, `codex-rs/lmstudio`, `codex-rs/model-provider`, `codex-rs/model-provider-info`, `codex-rs/models-manager`, `codex-rs/ollama`, `codex-rs/realtime-webrtc`, `codex-rs/responses-api-proxy`, `codex-rs/websocket-client` |
| Plugins, skills, connectors, and MCP | `codex-rs/codex-mcp`, `codex-rs/connectors`, `codex-rs/core-plugins`, `codex-rs/core-skills`, `codex-rs/mcp-server`, `codex-rs/mcp-server/tests/common`, `codex-rs/plugin`, `codex-rs/rmcp-client`, `codex-rs/skills` |
| Extension API and built-in extensions | `codex-rs/ext/connectors`, `codex-rs/ext/extension-api`, `codex-rs/ext/goal`, `codex-rs/ext/guardian`, `codex-rs/ext/image-generation`, `codex-rs/ext/items`, `codex-rs/ext/mcp`, `codex-rs/ext/memories`, `codex-rs/ext/skills`, `codex-rs/ext/web-search` |
| Cloud and external agents | `codex-rs/agent-graph-store`, `codex-rs/agent-identity`, `codex-rs/cloud-config`, `codex-rs/cloud-tasks`, `codex-rs/cloud-tasks-client`, `codex-rs/cloud-tasks-mock-client`, `codex-rs/external-agent-migration`, `codex-rs/external-agent-sessions` |
| Telemetry, feedback, and diagnostics | `codex-rs/analytics`, `codex-rs/feedback`, `codex-rs/otel`, `codex-rs/response-debug-context` |
| Support crates, samples, and narrow binaries | `codex-rs/ansi-escape`, `codex-rs/async-utils`, `codex-rs/codex-experimental-api-macros`, `codex-rs/git-utils`, `codex-rs/stdio-to-uds`, `codex-rs/test-binary-support`, `codex-rs/thread-manager-sample`, `codex-rs/uds`, `codex-rs/v8-poc` |
| Shared utility crates | `codex-rs/utils/absolute-path`, `codex-rs/utils/approval-presets`, `codex-rs/utils/cache`, `codex-rs/utils/cargo-bin`, `codex-rs/utils/cli`, `codex-rs/utils/elapsed`, `codex-rs/utils/fuzzy-match`, `codex-rs/utils/home-dir`, `codex-rs/utils/image`, `codex-rs/utils/json-to-toml`, `codex-rs/utils/oss`, `codex-rs/utils/output-truncation`, `codex-rs/utils/path-uri`, `codex-rs/utils/path-utils`, `codex-rs/utils/plugins`, `codex-rs/utils/pty`, `codex-rs/utils/readiness`, `codex-rs/utils/rustls-provider`, `codex-rs/utils/sandbox-summary`, `codex-rs/utils/sleep-inhibitor`, `codex-rs/utils/stream-parser`, `codex-rs/utils/string`, `codex-rs/utils/template` |

## Non-Rust project inventory

| Manifest | Owns |
| --- | --- |
| `package.json` | Root formatting, Python maintenance dispatch, dependency policy, and JavaScript toolchain pins |
| `.devcontainer/codex-install/package.json` | Dev-container Codex installation helper |
| `codex-cli/package.json` | Published npm CLI wrapper |
| `codex-rs/responses-api-proxy/npm/package.json` | npm wrapper for the Responses API proxy |
| `sdk/typescript/package.json` | TypeScript SDK package |
| `scripts/pyproject.toml` | Python script linting, formatting, and test environment |
| `sdk/python/pyproject.toml` | Python SDK package |
| `sdk/python-runtime/pyproject.toml` | Python runtime support package |

## Core runtime routing

| Concern | Start here | Follow-on owners |
| --- | --- | --- |
| Command selection and top-level dispatch | `codex-rs/cli/src/main.rs` | command-specific CLI modules, then TUI, exec, app-server, MCP, auth, plugins, or sandbox setup |
| Session and turn lifecycle | `codex-rs/core/src/session`, `codex-rs/core/src/tasks` | core state, rollout, protocol events, tools, model client, extension lifecycle |
| Multi-agent execution | `codex-rs/core/src/agent`, `codex-rs/core/src/agent_communication.rs` | agent identity/task/graph stores, protocol events, app-server and TUI consumers |
| Model-visible context | `codex-rs/core/src/context`, `codex-rs/core/src/context_manager`, `codex-rs/prompts` | context fragments, skills/plugins/apps instructions, compaction, prompt snapshots |
| Model requests and retries | `codex-rs/core/src/client.rs` | model-provider, backend/client crates, auth, telemetry, response debug context |
| Tool planning and dispatch | `codex-rs/core/src/tools`, `codex-rs/tools` | built-in handlers, extension tools, MCP calls, approvals, shell and sandbox owners |
| Retained command output | `codex-rs/core/src/tools/command_output_artifact.rs` | unified exec and shell producers, `ExecCommandToolOutput` model/code-mode projection, opaque current-thread `read_tool_output` handler/spec, generic retention and receipt-scoped protected evidence-artifact lifecycle |
| Task and external evidence ledger | `codex-rs/core/src/task_evidence.rs` | session initialization, KD4 completion-only plan/validation callers, direct MCP handler receipt capture, thread-scoped output artifacts for oversized canonical payloads |
| Shell execution and approvals | `codex-rs/core/src/exec.rs`, `codex-rs/core/src/exec_policy.rs` | shell-command, shell-escalation, execpolicy, sandboxing, platform sandboxes |
| Configuration resolution | `codex-rs/config`, `codex-rs/core/src/config`, `codex-rs/features` | profiles, permissions, requirements, hooks, MCP, schema generator, consuming runtime |
| Interactive presentation | `codex-rs/tui/src/app.rs`, `codex-rs/tui/src/chatwidget.rs` | app-server session bridge, bottom pane, history cells, protocol conversion, snapshots |
| App-server request lifecycle | `codex-rs/app-server/src/lib.rs` | request processors, thread/turn state, app-server protocol, core runtime, transport |
| Shared events and types | `codex-rs/protocol/src/lib.rs` | core, TUI, exec, app-server mappers, stored rollout compatibility |

## Extension boundary

`codex-rs/ext/extension-api` is the typed host-extension boundary. Prefer an
existing contributor contract and the immutable registry in
`codex-rs/ext/extension-api/src/registry.rs` before adding a fork-only hook in
core.

| Contribution | Owns |
| --- | --- |
| `McpServerContributor` | Runtime MCP server resolution from host and thread configuration |
| `ContextContributor` | Thread, turn, and rendered world-state prompt fragments |
| `ThreadLifecycleContributor` | Thread start, resume, idle, and stop extension state |
| `TurnLifecycleContributor` | Turn start, stop, abort, and error lifecycle |
| `TurnInputContributor` | Turn-local model-visible contextual input |
| `ConfigContributor` | Notifications after effective thread configuration changes |
| `TokenUsageContributor` | Model token-usage checkpoints |
| `ToolContributor` | Native extension-owned tool executors |
| `ToolLifecycleContributor` | Accepted tool-call start and terminal observation |
| `TurnItemContributor` | Ordered post-processing of parsed turn items |
| `ApprovalReviewContributor` | Extension-owned approval review decisions |

Built-in implementations live under `codex-rs/ext/*`; host installation and
dispatch cross `codex-rs/core-plugins`, `codex-rs/core-skills`, and core
session/tool/context owners. `codex-rs/hooks` is a separate external-command
hook engine and policy surface, not a substitute for the typed extension API.

## Persistence and stored state

| State | Primary owner | Consumers and compatibility boundary |
| --- | --- | --- |
| In-memory session and turn state | `codex-rs/core/src/session`, `codex-rs/core/src/state` | core tasks, TUI, app-server, extension stores |
| SQLite state and migrations | `codex-rs/state/src`, especially `migrations.rs` and `runtime` | thread lists, goals, logs, memories, agent jobs, recovery |
| Thread indexing and lookup | `codex-rs/thread-store` | CLI/TUI resume paths and app-server thread APIs |
| Rollout recording | `codex-rs/rollout` | persisted JSONL session history and replay/resume consumers |
| Rollout tracing | `codex-rs/rollout-trace` | diagnostics and execution tracing |
| Prompt/message history | `codex-rs/message-history` | core context reconstruction and client history |
| Memories | `codex-rs/memories/read`, `codex-rs/memories/write`, `codex-rs/ext/memories` | state DB, prompt context, tools, lifecycle callbacks |
| Agent graph and tasks | `codex-rs/agent-graph-store`, `codex-rs/agent-task-store` | core multi-agent coordinator, protocol/app-server status |

Treat migrations, rollout records, serialized protocol items, stored thread
metadata, and resume behavior as one compatibility surface. A change to one
owner must trace every reader and writer before completion.

## Contracts and generated artifacts

| Contract or output | Source owner | Update and validation path |
| --- | --- | --- |
| App-server request/notification schema | `codex-rs/app-server`, `codex-rs/app-server-protocol`, `codex-rs/protocol` | Focused protocol/app-server tests plus `just app-server-schema-check`; intentional regeneration uses the force/generator recipe |
| App-server schema tree | `codex-rs/app-server-protocol/schema` | Generated output; never hand-edit; inspect the generator-produced diff |
| Config schema | `codex-rs/config`, `codex-rs/features`, `codex-rs/core` | Focused config/core tests plus `just config-schema-check`; output is `codex-rs/core/config.schema.json` |
| Thread-config protobuf binding | `codex-rs/config/src/thread_config/proto/codex.thread_config.v1.proto` | `just generate-config-proto-check`; intentional regeneration uses `just generate-config-proto` |
| Hook schemas | `codex-rs/hooks/src` | Focused hook tests; intentional regeneration uses `just write-hooks-schema` and produces `codex-rs/hooks/schema/generated` |
| npm package layout | `codex-cli`, `scripts/stage_npm_packages.py`, `scripts/codex_package` | Staging/package tests, archive inspection, and the owning dry-run |
| Cargo package membership and dependency state | Rust package manifests, `codex-rs/Cargo.toml`, `codex-rs/Cargo.lock` | Cargo owns the lock update; never hand-edit generated dependency state |
| JavaScript workspace and dependency state | root/package manifests and `pnpm-workspace.yaml` | pnpm owns `pnpm-lock.yaml`; use the configured package-manager workflow |
| Rust snapshots and schema fixtures | Owning crate tests or generator | Regenerate through the owning command, then review focused diffs |
| Build outputs and vendored trees | `codex-rs/target`, `node_modules`, `codex-rs/vendor`, `third_party` | Do not hand-edit; rebuild, reinstall, or run the owning update workflow |

## Build, package, publish, and install paths

| Flow | Owner and entrypoint | Required downstream proof |
| --- | --- | --- |
| Rust workspace build/test | `codex-rs/Cargo.toml`, root `justfile`, crate manifests | focused crate check/test; use isolated lanes when parallel Rust work exists |
| npm CLI wrapper staging | `codex-cli/bin/codex.js`, `codex-cli/package.json`, `scripts/stage_npm_packages.py` | wrapper lint, staging/package tests, platform layout inspection |
| Canonical package archives | `scripts/codex_package` | package-local tests and archive/content checks |
| Standalone installers | `scripts/install/install.sh`, `scripts/install/install.ps1` | installer tests, digest/layout/locking/PATH/migration behavior |
| TypeScript SDK | `sdk/typescript` | `just sdk-ts-check` and package-facing type/tests |
| Python SDK | `sdk/python` | focused `uv run pytest` and `uv run ruff check .` |
| Python runtime package | `sdk/python-runtime` | focused runtime-package tests and lint |
| Nix environment | `flake.nix`, `flake.lock` | Nix evaluation/build appropriate to the changed input |
| Windows local publish | `scripts/publish-local-codex.ps1`, `just publish-local-codex-*` | dry-run, release build, doctor, backup/rollback guards, installed hash/version |
| Desktop-visible completion | local publish output plus app-server/CLI runtime | publish final, restart Desktop, prove process path and binary hash/version, inspect initialize/model metadata, capture visible evidence |

The expected installed target is
`C:\Users\kuh\Desktop\LOCAL-KD\codex.exe`. Source edits do not hot-apply to the
installed Desktop app. Unless the task explicitly includes local publish and
restart, report that `just publish-local-codex-final` and a Desktop restart
remain required.

## Validation routes

| Changed surface | Smallest owning proof |
| --- | --- |
| Source map or structural inventory | `python -m unittest scripts.test_source_map_check` and `just source-map-check` |
| Root or Python maintenance scripts | closest `python -m unittest scripts.test_<name>` plus syntax/lint appropriate to the script |
| Focused Rust crate | `just test-fast -p <crate>` or the nearest focused recipe/filter |
| App-server protocol/schema | focused crate tests plus `just app-server-schema-check` |
| Config schema | focused config/core tests plus `just config-schema-check` |
| Thread-config protobuf | `just generate-config-proto-check` |
| Hooks/schema | focused hook tests; run `just write-hooks-schema` only for intentional regeneration |
| TypeScript SDK | `just sdk-ts-check` |
| Python SDK | focused `uv run pytest` and `uv run ruff check .` |
| Package/archive flow | package-local tests followed by the relevant staging or dry-run proof |
| Local publish wiring | `just publish-local-codex-dry-run`; installed replacement requires `just publish-local-codex-final` |
| Installed provider external-evidence path | `python scripts/investigation_evidence_smoke.py --run`; uses staged local KDS/KDWG/Repo Atlas plugins, disposable Git repositories and Codex homes, and a loopback model endpoint |
| Repository-scoped final validation | `just verify-local <args>` or `scripts/verify_local.py` through its documented bounded flow |

Green tooling alone does not prove runtime behavior. Use the focused failing
test or approved runtime gate for the behavior being changed.

## Documentation and policy

| Need | Start here |
| --- | --- |
| Repository source routing and material-change inventory | `SOURCEMAP.md`; validate with `just source-map-check` |
| Repository-wide editing policy | `AGENTS.md` |
| Rust workspace policy | `codex-rs/AGENTS.md` |
| Script ownership and validation | `scripts/AGENTS.md`, then `scripts/README.md` |
| Repo-local Codex setup and durable harness | `.codex/AGENTS.md`, `.codex/README.md`, `.codex/harness/README.md` |
| Installation and local build | `docs/install.md` |
| Getting started and execution | `docs/getting-started.md`, `docs/exec.md`, `docs/slash_commands.md` |
| Configuration | `docs/config.md`, `docs/example-config.md` |
| Authentication | `docs/authentication.md` |
| Sandbox and execution policy | `docs/sandbox.md`, `docs/execpolicy.md` |
| Skills and agent guidance | `docs/skills.md`, `docs/agents_md.md` |
| Package-specific implementation background | nearest crate/package `README.md` |

Operational rules belong in the closest `AGENTS.md`. Package architecture,
usage, and examples belong in the nearest README or checked-in documentation.
This map owns cross-cutting navigation and structural inventory.

## Cross-cutting change routes

| Change | Trace this path |
| --- | --- |
| CLI command or flag | `codex-rs/cli` -> command owner -> core/protocol/config when shared -> docs/completions/tests -> npm wrapper only if launch behavior changes |
| TUI behavior | `codex-rs/tui` -> core and protocol contracts -> app-server bridge when used -> snapshots/focused TUI tests |
| Headless exec behavior | `codex-rs/exec` -> core/protocol -> JSONL/event compatibility -> focused exec tests |
| Desktop-visible behavior | app-server/core/protocol -> schema if the wire contract changes -> local release build -> publish -> Desktop restart/runtime proof |
| App-server API | app-server request processor -> app-server-protocol types/mappers -> generated schema -> SDK/Desktop consumers -> focused schema/runtime checks |
| Config field or default | config/features -> core consumer -> CLI/TUI/app-server exposure -> config schema/protobuf if applicable -> docs/tests |
| Tool or dynamic-tool behavior | core tool plan/router -> `codex-rs/tools` or extension/MCP owner -> lifecycle/approval/output mapping -> protocol/UI consumers |
| Shell, approval, or sandbox behavior | core exec/exec-policy -> shell-command/execpolicy/sandboxing -> platform sandbox helper -> security tests and user-facing policy |
| Model/provider behavior | model-provider/model catalogs/backend clients -> core request/retry path -> auth/telemetry -> diagnostics/tests |
| Prompt or model-visible context | prompts/context/context-manager/context-fragments -> plugin/skill/app/extension contributors -> compaction/history -> snapshots/tests |
| Plugin, skill, MCP, connector, or extension behavior | manifest/discovery owner -> core-plugins/core-skills/registry -> core session/tool/context dispatch -> CLI/TUI/app-server presentation |
| Hook behavior | config declaration -> hooks registry/engine/event type -> core lifecycle call site -> generated hook schema -> focused tests |
| Stored thread/session behavior | core session state -> state/thread-store/rollout/history/memories -> migrations/serialization -> app-server/TUI/CLI resume consumers |
| Multi-agent behavior | core agent coordinator -> identity/task/graph stores -> protocol/app-server status -> TUI/client presentation and persistence |
| npm packaging or install behavior | codex-cli -> stage script/codex_package -> platform package/archive -> installer/release workflow |
| SDK/API surface | app-server protocol/schema -> TypeScript and/or Python SDK -> generated models/types -> focused SDK tests |
| Dependency or build-system change | owning manifest -> lock state -> workspace/recipe consumers -> focused build/test/package proof |
| New top-level area, package, or instruction scope | add the owner and policy boundary -> update the machine-checked inventory in this file -> add routing/validation -> run `just source-map-check` |
