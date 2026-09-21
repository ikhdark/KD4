# Verification: folder (5), documents 21â€“25

All five supplied documents were read completely, including their limitations and
proposed validation. Embedded imperatives were treated as recommendations to
verify, not as additional user instructions. No agents were contacted or created.
The baseline is the existing modified checkout. Unrelated changes were preserved.

## Claim dispositions

| Document / claim | Current-source finding and disposition |
|---|---|
| 21.1 / 25.1 Warm-lane retention | Confirmed: `protected_warm_lane_names` ranked suffix before recency while allocation preferred recency. Retention now uses last-use time first with deterministic name/suffix ties. Existing age, activity, containment, and capacity safeguards remain. |
| 21.2 Full/changed Python validation mismatch | Confirmed in `root_maintenance.py`. Full and changed discovery now share test-target conversion; changed lint/tests recognize the owned roots and preserve path spelling. Explicit routes remain additive. |
| 21.3 Repeated bounded inventory never advances | Confirmed in `source_inventory.py`. Pending batches resume a retained scan epoch without rereading completed source observations. The output identifies retained evidence; `--refresh` starts a new epoch and rehashes current sources. Consumer evidence is still content-checked. Pagination remains independent. |
| 21.4 / 25.5 Foreground lane maintenance | Confirmed ordering, but no measured responsiveness failure or storage-safe alternative was supplied. Retained the existing interval-gated, coordinated maintenance and explicit prune commands. A new janitor/storage-debt scheduler is a performance redesign, not needed for the reproduced retention failure. No latency improvement is claimed. |
| 21.5 Gate completion ignores binary identity | Confirmed. Completion checks now bind the test name to the selected binary identity as well as requiring exactly one successful receipt. The installed nextest was exercised with library, binary, and integration targets; parsing accepts its optional progress counter and `package::bin/name` spelling. Existing zero-selection, ignored-test, failure, retry, and bounded-log behavior remains. A JUnit migration is a separate interface replacement; terminal parsing is not described as cryptographic or adversarial proof. |
| 22.1 No-op publish restarts Desktop | Confirmed. The convenience recipe uses conditional restart, with the existing live-runtime proof deciding healthy no-ops. Explicit force restart remains. Dry-run does not probe/restart the live runtime. |
| 22.2 Conflicting/malformed timing evidence and independent milestones | Confirmed in both diagnostic paths. Terminal snapshots have conflict quarantine, malformed profiles are excluded from arithmetic, and independent output milestones no longer require a tool-action pair. Unaffected observations remain usable and conflicts block an unqualified conclusion. |
| 22.3 / 25.2 Child lifetime and unbounded cleanup | Confirmed. Shared process ownership uses Windows jobs assigned before execution, and process groups elsewhere. Cleanup is bounded and failure is terminal. Lanes with unconfirmed cleanup are quarantined against allocation/pruning. Native process regressions exercise surviving descendants and sibling cancellation. |
| 22.4 Publisher build/stamp/consumption race | Confirmed missing ownership. A target-specific build lease spans reuse/build, stamping, and publication consumption. This reuses existing bundle staging and avoids an additional artifact copy. Cooperating producers are serialized; arbitrary external writers do not honor this lease. |
| 22.5 Whole-transcript capture and unbounded default diagnostics | Confirmed. Capture hashes while spooling, spilling beyond a bounded in-memory threshold. Default text uses the bounded runner projection with a byte cap. Exact analysis still retains correlation/scalar/detail state; this is not a claim of constant-memory analysis or a complete incremental-reducer rewrite. |
| 23.1 Package/archive/sidecar transaction gaps and checksum lost updates | Confirmed. Archives read the private package. An outer cooperating-writer transaction retains rename backups of previous package/archive/sidecar outputs until all requested outputs succeed, without copying the old package up front. The shared checksum merge is locked, including direct callers. This is exception rollback, not simultaneous visibility of multiple paths to arbitrary readers. |
| 23.2 Mutable build handoff and cross-target prebuilt version evidence | Confirmed: the CLI retains the package build lease through staging, and direct builders also acquire it. Staged executable digests must match input digests captured before copying. Cross-target prebuilt entrypoints are rejected for distributable archives because their requested release version cannot be established; source builds and local package staging remain available. Required LICENSE/NOTICE files are checked before build work. |
| 23.3 Same-path tool replacement bypasses reuse | Confirmed. Recipe evidence includes executable contents, configured linkers and wrappers. Package targets are separated by tool-content identity so a changed tool cannot reuse the preceding target's Cargo outputs. This does not establish complete hermetic builds. |
| 23.4 Failed dependency acquisition waits for sibling | Confirmed. Parallel workers share cancellation, and Cargo commands own their descendants. Downloads check cancellation and a total deadline; local ripgrep architecture is checked before compilation. Healthy build/download overlap remains. |
| 23.5 Repeated archive encoding | Confirmed. Identical formats/compression settings are encoded once per invocation and copied independently into destination staging. No persistent archive cache or final-output hardlinks were added. |
| 24.1 Inventory writes corrupt checkpoints/aliases/immutable artifacts | Confirmed. Existing atomic-output support now handles bytes and immutable publication; writes flush/sync a complete temporary before commit. Replacing the report directory entry preserves other hardlinks. Immutable conflicting content remains an error. |
| 24.2 npm cancellation bypasses rollback | Confirmed. Activation is serialized with a durable journal, cancellation rollback, and idempotent recovery. Recovery validates owned filenames and content before restoring/removing files. |
| 24.3 npm subprocess ownership and missing middle diagnostics | Confirmed. Staging commands use owned processes and shared operation cancellation/deadlines, including worker and lock boundaries. Full named logs survive bounded excerpts. |
| 24.4 Compact output bounded against pretty JSON | Confirmed. Both bounding stages use the selected wire encoding, including the complete envelope and newline. |
| 24.5 Repeated owner-symbol interpretation and query reread | Confirmed. Identical symbol/mode checks are memoized per capture; query receives the fingerprints already captured during validation. No timestamp-only or persistent source cache was added. |
| 25.3 Feature checker bypasses lane ownership | Confirmed. Feature export and runtime verification acquire/reuse an operation lease and pass its target explicitly. The CLI acquires the lease lazily and keeps it across both phases; helper/gate batching remains. |
| 25.4 Validation may update Cargo.lock | Confirmed missing policy. Metadata, helpers, nextest selection/execution, and feature-default export now supply runner-owned `--locked`; no unlocked retry was added. |

The reports' claims about no measured upstream advantage, token savings, or
production frequency are correct limitations. Their preserved safeguardsâ€”exact
selection, helper batching, source/content freshness, independent executable
copies, bounded diagnostic excerpts, and publication recoveryâ€”remain relevant.
The diagnostic WebSocket fixture was not promoted to acceptance evidence.

## Validation

Recorded results:

- **744 distinct Python test IDs passed; 2 skipped; no unresolved failures.**
  The skips require a repo-local skills directory that is not materialized.
- The broader run covered inventory, ownership, build-lane storage/policy,
  Rust test routing, feature checking, timing diagnostics, rollout snapshots,
  package Cargo/layout/CLI, npm staging, and publisher fixtures. It included
  a real feature-default Cargo export.
- A small native Rust fixture ran three passing tests in separate library,
  binary, and integration targets. The initial receipt assertion exposed
  nextest's optional progress counter; its saved output was re-parsed after
  correcting the parser, **without rerunning those Rust tests**.
- Six isolated PowerShell decision cases passed: healthy no-op, missing/stale
  runtime, binary change, routing change, dry-run no-op, and explicit force.
  Runtime proof was mocked; no Desktop restart was executed.
- Ruff passed for the edited Python files. Publisher PowerShell parsing and
  the scoped Git whitespace check passed.
- The result ledger contains **zero repeated passing test IDs**. Retries
  excluded earlier passing IDs. Initially failed tests and newly added checks
  were run until their failures were resolved.

The ledger and logs are retained under `_build/report-21-25-validation/`:
`test-results.jsonl`, `regressions.log`, `suites.log`, `retry-1.log` through
`retry-3.log`, `nextest-identity.log`, `nextest-identity-replay.log`,
`restart-decision.log`, `ruff.log`, `ruff-retry.log`, and `diff-check.log`.
Initial failure logs are intentionally preserved alongside successful retries.

Validation reuses passing results from this session, as requested. This is
focused verification of the changed tooling, not a full Rust workspace test
run, production publication, or measured performance comparison. Independent
workspace edits were preserved, including compatible process-runner changes
that appeared while this work was in progress.

No installed binary was replaced, Desktop was not restarted, and no upstream
synchronization or distribution was performed.
