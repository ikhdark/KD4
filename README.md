# KD4

KD4 is a Windows-only, local fork of [OpenAI Codex](https://github.com/openai/codex).
This checkout contains the Rust harness, CLI and app server, Code Mode runtime,
SDKs, and local build/install tooling. It is not the upstream distribution.
Publishing or republishing requires explicit permission.

## Documentation

| Area | Guide |
| --- | --- |
| Repository instructions and evidence policy | [AGENTS.md](AGENTS.md) |
| Build lanes, frozen bundles and formatting | [Repository scripts](scripts/README.md) |
| Windows installation, local publishing and PGO | [Install scripts](scripts/install/README.md) |
| CLI npm wrapper and package staging | [Wrapper](codex-cli/README.npm.md), [packaging scripts](codex-cli/scripts/README.md) |
| Python SDK | [Overview](sdk/python/README.md), [getting started](sdk/python/docs/getting-started.md), [API reference](sdk/python/docs/api-reference.md), [FAQ](sdk/python/docs/faq.md) |
| TypeScript SDK | [SDK guide](sdk/typescript/README.md) |
| Python CLI runtime package | [Runtime packaging](sdk/python-runtime/README.md) |
| Configuration | [Layer loader](codex-rs/config/src/loader/README.md), [schema generation](codex-rs/core/src/config/schema.md) |
| Harness instructions and tool delivery | [Instruction architecture](codex-rs/core/instruction-architecture.md) |
| Turn timing | [Critical-path timing](codex-rs/core/critical-path-timing.md), [response-start timing](codex-rs/core/response-start-timing.md) |
| App-server queue | [Thread queue](codex-rs/app-server/THREAD_QUEUE.md) |
| Execution service | [Exec server](codex-rs/exec-server/README.md) |
| Session persistence | [Rollout storage](codex-rs/rollout/README.md) |
| Process and transport utilities | [Process hardening](codex-rs/process-hardening/README.md), [PTY](codex-rs/utils/pty/README.md), [stdio-to-UDS](codex-rs/stdio-to-uds/README.md) |
| Parsing and rendering utilities | [Stream parser](codex-rs/utils/stream-parser/README.md), [templates](codex-rs/utils/template/README.md) |
| Extensions and bundled skills | [Extension API](codex-rs/ext/extension-api/notes.md), [skill samples](codex-rs/skills/src/assets/samples/) |
| Argument-comment lint | [Dylint guide](tools/argument-comment-lint/README.md) |
| Vendored integration notes | [Notify patch](third_party/notify/KD4.md), [Tungstenite](third_party/tungstenite/README.md), [V8](third_party/v8/README.md) |
| Local reports and captured evidence | [Workspace navigation](.codex/README.md), [audit library](.codex/audits/README.md) |

## Local development

Run commands from the checkout root unless a guide says otherwise. The root
[justfile](justfile) runs Cargo recipes from `codex-rs`, where
[rust-toolchain.toml](codex-rs/rust-toolchain.toml) and
[.cargo/config.toml](codex-rs/.cargo/config.toml) select the toolchain and build
flags. A standalone `--manifest-path` does not select those working-directory
settings.

With the required tools already available:

```powershell
just --list
just check-lane codex-core
just cargo-lane cli cargo build -p codex-cli -p codex-code-mode-host
```

Use the target directory emitted by the lane owner; lane names include source
identity and are not literal target paths. See the scripts guide for exact
frozen-build receipts and snapshot reuse. `dev` is the normal development
profile; `release` and `pgo` are documented in the install guide. Windows
builds use native CPU tuning and are not portable distribution binaries.

Node/pnpm requirements are declared in [package.json](package.json); Python
requirements belong to each script/SDK project's manifest. Reuse configured
toolchains and environments rather than provisioning the machine as a side
effect of documentation or source inspection.

## Workflow boundaries

[AGENTS.md](AGENTS.md) owns repository policy and links the shared KDR evidence,
acceptance and benchmark workflows. A documented command or registered runner
does not grant permission to execute it. Inspect delegated workloads and follow
the active validation mode, including restrictions on test execution and creation.

Local publishing is a separate, explicitly authorized operation: it can replace
the installed bundle, change Desktop routing, and restart processes. Ordinary
builds and documentation updates do not publish.

Saved-session analysis belongs in the sibling KDR project. Historical reports,
source snapshots, receipts, and third-party licenses are retained records, not
current task instructions or material to rewrite during a documentation refresh.
