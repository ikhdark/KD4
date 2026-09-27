# Finalized upstream crates

This directory is frozen: no local modifications unless required by an explicitly requested upstream change.

## Workspace location

This folder lives at the repository root, beside `codex-rs`. Its 15 crates remain members of the `codex-rs` Cargo workspace. Each crate manifest sets `package.workspace = "../../codex-rs"`; these relocation-only entries are the only new crate changes required by the root move.

The upstream fidelity statements below describe the restored source snapshots; crate manifests additionally carry that workspace locator. Rust source files are unchanged. Root-level Rust/Prettier exclusions protect this tree, and the local publisher fingerprints this directory together with `codex-rs` so moving it cannot hide changes from build freshness checks.

| Local directory | Upstream path | Baseline |
| --- | --- | --- |
| terminal-detection | codex-rs/terminal-detection | openai/codex `985cf47a4eb6084b2ff6b30ebdb1216acda85bb4` |
| response-debug-context | codex-rs/response-debug-context | openai/codex `985cf47a4eb6084b2ff6b30ebdb1216acda85bb4`, with error-enum compatibility below |
| responses-api-proxy | codex-rs/responses-api-proxy | openai/codex `985cf47a4eb6084b2ff6b30ebdb1216acda85bb4`, with build compatibility below |
| stdio-to-uds | codex-rs/stdio-to-uds | openai/codex `985cf47a4eb6084b2ff6b30ebdb1216acda85bb4` |
| aws-auth | codex-rs/aws-auth | openai/codex `9cbd4c0371a68b8877eab58633ceb51f22d45ef0` |
| codex-home | codex-rs/codex-home | openai/codex `985cf47a4eb6084b2ff6b30ebdb1216acda85bb4`, with instruction-API compatibility below |

All four terminal-detection crate files are restored verbatim from this revision, including its tests and Bazel metadata. Cargo workspace paths and CLI/TUI compatibility are maintained outside the frozen crate.

The pre-restoration working files and their SHA-256 manifest are preserved under the repository Git directory at `codex-backups/terminal-detection-20260926-211252-967`.

## Finalized compatibility snapshots

The five additional crates were restored and validated before relocation. Their initial compatibility adaptations are fixed parts of these snapshots, not permission for future local edits:

- `response-debug-context`: match the fork's existing `ApiError` and `TransportError` variants. Unsupported upstream-only variants are omitted; `PreDispatch`, `IncompleteResponse`, and `ProviderFailure` retain their existing diagnostic classifications. The rate-limit test uses the supported `RateLimit` variant. Header extraction follows upstream.
- `responses-api-proxy`: the existing reqwest 0.13 workspace calls its TLS feature `rustls`, and the existing ctor version requires `#[ctor::ctor(unsafe)]`. No proxy behavior changes are included in these build adaptations. Upstream's standalone HTTP client, forwarding, dump behavior, and multi-platform npm launcher are restored; the fork-only body/concurrency limits and dump enhancements are not retained. External npm staging still restricts the fork distribution to Windows.
- `codex-home`: import `UserInstructions` as `Instructions` and `LoadUserInstructionsFuture` as `LoadInstructionsFuture`; populate the fork's required absolute source path instead of `Some(path)`. Tests use the same payload adaptation. Upstream loading, refresh cache, warning deduplication, and lossy UTF-8 decoding are unchanged.
- `aws-auth` uses the latest compatible upstream snapshot before newer Bedrock/network-policy API migrations. All five files match that snapshot without local adaptations. This does not introduce the latest upstream AWS authentication architecture.
- `stdio-to-uds` matches its recorded upstream snapshot without local adaptations, including upstream EOF/connection-lifetime behavior.

The additional pre-restoration files, SHA-256 manifest, pre-change workspace manifests, and hashes of the three excluded utility crates are preserved under the repository Git directory at `codex-backups/frozen-crates-20260926-214048-602`.

`utils/der`, `utils/home-dir`, and `utils/template` are explicitly excluded from this restoration and remain in their original locations.

## Consolidated upstream-owned crates

The nine crates below were consolidated into this directory without changing their contents. The following notes preserve their earlier restoration provenance; the strict frozen policy in AGENTS.md governs all crates here.

### Baseline

Compared against upstream `main` at commit
`985cf47a4eb6084b2ff6b30ebdb1216acda85bb4` on 2026-09-26.
All files in the following crates match that revision, allowing Git's LF/CRLF
checkout conversion. Local deviations were replaced by the upstream versions
as explicitly requested; compatibility changes belong outside these crates.

| Local directory | Upstream directory | Reason to retain upstream ownership |
| --- | --- | --- |
| `cloud-tasks-mock-client` | `codex-rs/cloud-tasks-mock-client` | Mock cloud backend and sample responses, not fork runtime policy. |
| `codex-backend-openapi-models` | `codex-rs/codex-backend-openapi-models` | Backend wire contracts; use upstream definitions rather than custom schema changes. |
| `codex-experimental-api-macros` | `codex-rs/codex-experimental-api-macros` | Shared experimental-API derive machinery; feature choices belong in its consumers. |
| `keyring-store` | `codex-rs/keyring-store` | OS credential storage and native error classification. |
| `process-hardening` | `codex-rs/process-hardening` | Security-sensitive platform startup hardening; avoid incidental divergence. |
| `rustls-provider` | `codex-rs/utils/rustls-provider` | Shared TLS provider initialization and upstream regression tests. |

### Compatible upstream snapshots

The following crates are also protected here, but retain the compatibility
adaptations documented in [the auth/provider restoration notes](../docs/upstream-auth-compatibility.md).
They are not exact mirrors of the baseline above. Moving them here preserves
their restored contents; it does not perform another upstream update.

| Local directory | Upstream directory | Upstream source |
| --- | --- | --- |
| `login` | `codex-rs/login` | `e1b7b1acb3ccf6ba9295762679414934c894b914` |
| `backend-client` | `codex-rs/backend-client` | `506a328dab110591d3c1449a15217596e7e9cd61`; HTTP client and request tests from `888be42a20c5a727214d898c4e335ac2e27161af` |
| `model-provider-info` | `codex-rs/model-provider-info` | `2e8c3756f95789c215d9ea9a5ade6ec377934b3f` |

### Workspace integration

These remain active Cargo workspace members. Their dependency keys and package
names are unchanged; the parent workspace manifest supplies the relocated paths.
For the six exact mirrors above, crate-local files, including upstream Bazel
metadata, are kept upstream-exact. The three compatible snapshots retain their
documented exceptions and Cargo-only layout.
The workspace formatter excludes this directory to avoid incidental rewrites.

Restoring the models also restores upstream's signed 32-bit `reset_at` fields
and its `edu_plus` / `edu_pro` plan variants. The fork's backend adapter keeps
mapping these unsupported account tiers to `Unknown`, as before this sync.

The pre-sync local files are preserved in the repository's Git directory under
`codex-backups/upstream-owned-20260926-201640`, including a SHA-256 manifest.
This local recovery backup is not part of the source tree or distributed builds.

At that restoration, other top-level source directories were not moved: they contain differences
from this upstream revision or are fork-only. Being low-level, generated, or
rarely edited does not establish that those differences should be discarded.

For an explicitly requested upstream update, compare each directory against its
original upstream path above. Review compatibility with the fork's workspace
dependencies and consumers; do not blindly overwrite it with a newer revision.
