# Codex Rust do-not-code map

## Purpose and decision rule

This file defines the small part of `codex-rs` that should remain aligned with
upstream because editing it has no credible upside for this fork's agent,
model, harness, navigation, learning, tool-use, latency, or daily-development
goals. `Protected` is intentionally a high bar. Runtime importance alone does
not make a path protected; behavior and improvement surfaces remain editable.

The upstream comparison point for this audit is OpenAI Codex `main` commit
`646f7c0a91b8e327d263335da68ae8ef212895ce`. The commit was fetched into
`upstream/main` and its SHA was independently confirmed with `git ls-remote`.

The classifications mean:

- **Protected - upstream mirror:** never carry fork edits here. Replace the
  complete path with the pinned upstream tree, including upstream additions and
  deletion of fork-only files.
- **Protected - workflow managed:** never hand-edit. These paths are controlled
  by Cargo or a build command and are not independent upstream-mirror targets.
- **Mixed:** the parent is an improvement surface, but the named generated child
  must be changed through its owner. Do not blanket-protect or blanket-sync the
  parent.
- **Editable:** a credible fork improvement exists, even if the path is risky or
  foundational. Risk alone is not a reason to classify code as protected.

## Protected source: exact upstream mirror

These four paths, and only these four paths, are exact upstream-mirror targets.
Compatibility repairs belong in their editable callers; do not patch the mirror
to make an older caller compile.

| Path | Why it is protected |
| --- | --- |
| `codex-rs/backend-client/` | Backend account, cloud-task, config, and rate-limit HTTP plumbing. It is necessary runtime infrastructure, not the model request or harness behavior owner. |
| `codex-rs/codex-backend-openapi-models/` | Generated OpenAPI model files plus upstream-maintained wrapper and curated export metadata; its sole internal consumer is `backend-client`, and it owns no fork behavior. |
| `codex-rs/login/` | Authentication acquisition, refresh, storage, PKCE/device login, and callback plumbing. This is a deliberately stable runtime boundary; auth callers adapt to upstream rather than forking it. |
| `codex-rs/vendor/` | Third-party Bubblewrap source plus Codex-owned build integration metadata. Keep the complete upstream-owned unit together; never maintain fork behavior inside it. |

`login` and `backend-client` have wide reverse-dependency fanout. That makes
their synchronization expensive, but it does not turn their plumbing into a
harness improvement surface.

## Mixed areas: editable parent, generated child

No mixed parent contains an independently upstream-mirrored source island. The
parents below remain editable. Only the named output is workflow-managed, and
it must be regenerated from the fork's editable owner rather than copied from
upstream by itself.

| Editable parent | Workflow-managed child | Owning route |
| --- | --- | --- |
| `codex-rs/app-server-protocol/` | `schema/` | `just app-server-schema-check`; intentional regeneration uses `just app-server-schema-regenerate <owner>`. |
| `codex-rs/cli/` | checked-in `*.snap` test snapshots | Regenerate through the owning snapshot test and review the resulting diff. |
| `codex-rs/config/` | `src/thread_config/proto/codex.thread_config.v1.rs` | `just generate-config-proto-check`; intentional regeneration uses `just generate-config-proto`. |
| `codex-rs/core/` | `config.schema.json` and checked-in `*.snap` test snapshots | Use `just config-schema-check` or the owning snapshot test; intentional schema regeneration uses `just config-schema-regenerate <owner>`. |
| `codex-rs/exec-server/` | `src/proto/codex.exec_server.relay.v1.rs` | The file is marked prost-generated from the adjacent protobuf contract, but this checkout exposes no regeneration recipe. Never hand-edit it; a change must first add or restore a reproducible owner command and check. |
| `codex-rs/hooks/` | `schema/generated/` | Run focused hook tests; intentional regeneration uses `just write-hooks-schema`. |
| `codex-rs/tui/` | checked-in `*.snap` test snapshots | Regenerate through the owning snapshot test and review the resulting UI diff. |

## Editable areas

The editable set intentionally includes all behavior and policy surfaces:

- session, turn, context, prompt, compaction, memory, multi-agent, goal, state,
  rollout, history, and evidence behavior;
- model providers, model requests, retries, streaming, diagnostics, telemetry,
  caching, WebSockets, MCP, plugins, skills, connectors, and extensions;
- tools, code mode, shell commands, patching, file search,
  filesystem access, approvals, execution policy, sandboxing, and process
  lifecycle;
- CLI, exec, app-server, protocol, TUI, configuration, feature flags, tests,
  build performance, packaging context, and local-fork documentation.

In particular, `codex-rs/code-mode/` and `codex-rs/shell-command/` are editable
because they directly affect daily harness behavior. Highly shared owners such
as `core`, `protocol`, `app-server`, and `tui` are also editable; they require
stronger validation, not a do-not-code label.

`codex-rs/aws-auth/` is also editable. Its credential resolution, request
signing, and retry classification directly affect the Bedrock model provider's
reliability and latency, which is credible harness improvement upside even
though the crate is security-sensitive.

## Generated, vendored, and local build output

- `codex-rs/Cargo.lock` is Cargo-owned. Dependency changes may update it, but it
  must never be hand-edited or replaced wholesale with upstream because this
  fork has different workspace members and dependencies.
- `codex-rs/target/` and every current `codex-rs/target-*/` directory are local
  build outputs. Never hand-edit, upstream-sync, or delete them while Rust jobs
  may be active.
- The generated children in the mixed-area table are allowed to change only as
  outputs of their documented owner. They are not exact upstream-mirror paths.
- `codex-rs/vendor/` is different: it is an exact upstream-mirror source path
  because third-party vendored code has no fork-local behavior owner.
- `codex-rs/workspace-coordinator/` is currently an empty, untracked scaffold.
  It remains editable because workspace coordination is a credible future
  harness feature; its emptiness is not a reason to protect it.

## Complete top-level classification

This inventory covers checked-in immediate entries under `codex-rs`. Ignored
build outputs and machine-local directories are not enumerated; they remain
workflow-managed regardless of their local names.

### Protected - exact upstream mirror

- `codex-rs/backend-client/`
- `codex-rs/codex-backend-openapi-models/`
- `codex-rs/login/`
- `codex-rs/vendor/`

### Protected - workflow managed, not upstream mirrored

- `codex-rs/Cargo.lock`
- `codex-rs/target/`

### Mixed

- `codex-rs/app-server-protocol/` - only `schema/` is generator-owned.
- `codex-rs/cli/` - only checked-in test snapshots are workflow-managed.
- `codex-rs/config/` - only the generated thread-config Rust binding is workflow-managed.
- `codex-rs/core/` - only the config schema and checked-in test snapshots are workflow-managed.
- `codex-rs/exec-server/` - only the generated relay protobuf Rust binding is workflow-managed.
- `codex-rs/hooks/` - only `schema/generated/` is generator-owned.
- `codex-rs/tui/` - only checked-in test snapshots are workflow-managed.

### Editable

- `codex-rs/.cargo/`
- `codex-rs/.config/`
- `codex-rs/.gitignore`
- `codex-rs/agent-graph-store/`
- `codex-rs/agent-identity/`
- `codex-rs/agent-task-store/`
- `codex-rs/AGENTS.md`
- `codex-rs/analytics/`
- `codex-rs/ansi-escape/`
- `codex-rs/app-server/`
- `codex-rs/app-server-client/`
- `codex-rs/app-server-daemon/`
- `codex-rs/app-server-test-client/`
- `codex-rs/app-server-transport/`
- `codex-rs/apply-patch/`
- `codex-rs/arg0/`
- `codex-rs/async-utils/`
- `codex-rs/aws-auth/`
- `codex-rs/build_info.rs`
- `codex-rs/bwrap/`
- `codex-rs/Cargo.toml`
- `codex-rs/chatgpt/`
- `codex-rs/clippy.toml`
- `codex-rs/cloud-config/`
- `codex-rs/cloud-tasks/`
- `codex-rs/cloud-tasks-client/`
- `codex-rs/cloud-tasks-mock-client/`
- `codex-rs/code-mode/`
- `codex-rs/code-mode-host/`
- `codex-rs/code-mode-protocol/`
- `codex-rs/codex-api/`
- `codex-rs/codex-client/`
- `codex-rs/codex-experimental-api-macros/`
- `codex-rs/codex-home/`
- `codex-rs/codex-mcp/`
- `codex-rs/collaboration-mode-templates/`
- `codex-rs/config.md`
- `codex-rs/connectors/`
- `codex-rs/context-fragments/`
- `codex-rs/core-api/`
- `codex-rs/core-plugins/`
- `codex-rs/core-skills/`
- `codex-rs/default.nix`
- `codex-rs/deny.toml`
- `codex-rs/docs/`
- `codex-rs/exec/`
- `codex-rs/exec-server-protocol/`
- `codex-rs/execpolicy/`
- `codex-rs/execpolicy-legacy/`
- `codex-rs/ext/`
- `codex-rs/external-agent-migration/`
- `codex-rs/external-agent-sessions/`
- `codex-rs/features/`
- `codex-rs/feedback/`
- `codex-rs/file-search/`
- `codex-rs/file-system/`
- `codex-rs/file-watcher/`
- `codex-rs/git-utils/`
- `codex-rs/http-client/`
- `codex-rs/install-context/`
- `codex-rs/keyring-store/`
- `codex-rs/linux-sandbox/`
- `codex-rs/lmstudio/`
- `codex-rs/mcp-server/`
- `codex-rs/memories/`
- `codex-rs/message-history/`
- `codex-rs/model-provider/`
- `codex-rs/model-provider-info/`
- `codex-rs/models-manager/`
- `codex-rs/network-proxy/`
- `codex-rs/ollama/`
- `codex-rs/otel/`
- `codex-rs/plugin/`
- `codex-rs/process-hardening/`
- `codex-rs/prompts/`
- `codex-rs/protocol/`
- `codex-rs/README.md`
- `codex-rs/realtime-webrtc/`
- `codex-rs/response-debug-context/`
- `codex-rs/responses-api-proxy/`
- `codex-rs/rmcp-client/`
- `codex-rs/rollout/`
- `codex-rs/rollout-trace/`
- `codex-rs/rust-toolchain.toml`
- `codex-rs/rustfmt.toml`
- `codex-rs/sandboxing/`
- `codex-rs/scripts/`
- `codex-rs/secrets/`
- `codex-rs/shell-command/`
- `codex-rs/shell-escalation/`
- `codex-rs/skills/`
- `codex-rs/state/`
- `codex-rs/stdio-to-uds/`
- `codex-rs/terminal-detection/`
- `codex-rs/test-binary-support/`
- `codex-rs/thread-manager-sample/`
- `codex-rs/thread-store/`
- `codex-rs/tools/`
- `codex-rs/uds/`
- `codex-rs/utils/`
- `codex-rs/v8-poc/`
- `codex-rs/websocket-client/`
- `codex-rs/windows-sandbox-rs/`
- `codex-rs/workspace-coordinator/`

## Historical upstream synchronization record (2026-08-12)

This section is frozen evidence for the synchronization described below, not a
claim about the current checkout. New synchronization evidence belongs in a
dated audit artifact rather than in the current classification above.

Completed against OpenAI Codex `main` commit
[`646f7c0a91b8e327d263335da68ae8ef212895ce`](https://github.com/openai/codex/tree/646f7c0a91b8e327d263335da68ae8ef212895ce/codex-rs).
Both `upstream/main` and a fresh `git ls-remote` resolved to that SHA at the
end of the work.

- The complete upstream trees for `backend-client/`,
  `codex-backend-openapi-models/`, `login/`, and `vendor/` were restored with
  upstream additions and deletions intact. A final Git-clean-filtered blob
  audit compared 128 upstream files with 128 local files and found no missing,
  different, or extra files.
- The immediate `codex-rs` inventory contains 123 entries, each listed exactly
  once: 4 exact upstream mirrors, 8 workflow-managed entries, 7 mixed parents,
  and 104 editable entries. The audit found no missing or document-only entry.
- Narrow compatibility repairs were made only in editable dependents. They
  adapt the fork to upstream HTTP-client factories, concrete auth-route and
  managed-auth types, originator identity, backend rate-limit fields, and the
  generated app-server protocol contract. The protected trees were not patched
  to retain fork APIs.
- `just app-server-schema-regenerate "upstream-sync"` regenerated the complete
  owner-managed schema tree, and final `just app-server-schema-check` passed
  both schema fixture checks.
- `cargo fmt --all -- --check` passed. Focused and fanout `cargo check --tests`
  coverage passed for every directly changed Rust crate. Full owner suites
  passed for `codex-http-client`, `codex-websocket-client`, `codex-login`,
  `codex-backend-client`, `codex-cloud-config`, `codex-model-provider`, and
  `codex-protocol`: 633 tests total. Nine focused core, TUI, app-server
  rate-limit, and initialize/originator tests also passed.
- The source-map unit suite passed all 11 tests and `just source-map-check`
  passed when run with a temporary alternate index containing the new
  `do-not-code.md`. The real index was intentionally left untouched; until the
  user tracks this new root file, the ordinary checker correctly reports that
  its ownership row is not backed by a tracked path.
- `just release-build-fast` completed the local `codex-cli` release build in
  22 minutes 37 seconds. The uninstalled artifact is
  `codex-rs/target/lanes/release-cli/release/codex.exe` (380,007,936 bytes,
  `codex-cli 0.0.0`) with SHA-256
  `683509927EEF3362A98E2DA914EC5372CE7D37C72B2D10CD4B18DD5EBD83E3D6`.

No file was staged, committed, installed, or published. The pre-existing user
change in `.codex/config.toml` was left untouched.
