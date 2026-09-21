# Verification of the five attached reports

All five files were read completely: `New Text Document.txt` and numbered files
`(2)` through `(5)`, totaling 162,065 bytes. Their “CHANGE IT” directives and
implementation orders were treated as proposals to evaluate, not as additional
user instructions. This audit checks the current working checkout, including
pre-existing edits, rather than assuming the reports describe its current state.

The reports contain both reproducible correctness problems and suggestions to
replace intentional architecture or tune resource policy. Those are distinguished
below. Source inspection establishes a mechanism; it does not establish its
production frequency, token cost, performance ranking, or superiority over an
upstream version. No comparative performance results are claimed.

## Changes made

* **Configuration:** disabled project and root-hook contents are not read;
  executor canonicalization controls source identity; primary-checkout hooks can
  be discovered without a worktree-local `.codex` directory. Remote provider
  conversion now preserves AWS profile/region and standalone web-search support.
  The source protobuf was updated and Rust regenerated, including a reproducible
  generator allowance for the existing generated-code lint policy.
* **Patch correctness and diagnostics:** literal blank source lines cannot be
  discarded to make a failed edit succeed. Anchored EOF insertions must be at
  EOF. Missing excerpts retain the mismatch kind, source hash, and hunk identity;
  unavailable line locations use the documented zero sentinel. Diagnostic search
  preserves blank-line offsets, centers the excerpt on a discrepancy, and stops
  at comparison/byte budgets. Two old tests had accidental trailing blank source
  lines; their patch fixtures were corrected without restoring permissive matching.
* **Code Mode host:** the executable owns its Tokio runtime and bounds final
  teardown. Cell queues support immediate cancellation removal, local admission
  errors, and a reserved closure slot. Explicit cancellation causes survive both
  dispatch-observation branches. Delegate frames are prepared outside the shared
  pending-call mutex and liveness is rechecked before dispatch.
* **Tool contracts:** MCP contracts define their minimal `CallToolResult<T>`
  envelope locally, once per bundle, preserving the independent structured-content
  schema and showing content, error, and metadata access.
* **Coordination:** existing validation operations can settle after attempt
  sealing or replacement, without gaining proof eligibility. Validation records a
  fresh input epoch before launch, and completion proof requires matching input
  and terminal epochs. Capture failure can leave a truthful terminal process
  result with unavailable proof, using a savepoint to roll back failed capture.
  Unborn Git branches are supported, and strict Git capture does not traverse a
  diagnostic fallback tree that it would subsequently reject.

## Claim-by-claim disposition

“Retained policy” means the described mechanism exists, but the report does not
demonstrate a violated contract requiring its proposed replacement. These entries
are not being represented as implemented optimizations.

| Report / primary claim | Verification and disposition |
| --- | --- |
| 1.1 Queue congestion disconnects the host | Confirmed. Fixed cell-local overflow and closure contention. The outgoing transport still deliberately fails closed on a full queue. A connection-wide scheduler, response reservations, and protected request-admission lanes remain design proposals; they were not added. |
| 1.2 Async cleanup does not bound process teardown | Confirmed and fixed in the binary. Tokio documents that standard-input reads cannot be cancelled and ordinary runtime destruction can wait indefinitely. A real-process regression covers broken output with stdin still open. |
| 1.3 Cancellation retains queued payloads / can lose its cause | Confirmed and fixed for pending and active cell routes. Cancellation removes the queued owning message immediately and restores capacity, including when the future is dropped outside a runtime. |
| 1.4 Count-only buffering and serialization under a mutex | Confirmed. Moved serialization out of the pending-call lock. Aggregate byte-credit scheduling was not introduced: it is a separate resource-policy redesign, not a demonstrated ordinary-workload regression. Existing frame, call, cell, and session bounds remain. |
| 1.5 Output incompleteness is only prose | Outdated for oversized cell output. `OutputLoss` already crosses the wire, and the core adapter emits `output_complete: false`. The report's proposed retained-result/replay protocol is not implemented. Delegate transport errors still do not prove that an operation is safe to replay. |
| 2.1 Schema DAG expansion can exhaust rendering limits | Confirmed limitation. Retained the explicitly marked incomplete projection and finite traversal/output limits. Graph-first compilation and local subtree degradation remain proposals; no claim is made that the current renderer produces a usable declaration for every compact DAG. |
| 2.2 Remote execute retransmits supplied tool definitions | Confirmed wire shape. A versioned catalog/delta protocol is an optimization proposal; no workload measurement establishes the benefit or requires that protocol migration. Retained current per-execution authorization snapshots. |
| 2.3 Admission and consumed observations lack replay guarantees | Confirmed documented contract. The owning session already distinguishes work ownership from observation and warns against replaying uncertain side effects. Stable admission keys plus history-commit acknowledgments would change several ownership boundaries; no new replay guarantee was added. |
| 2.4 Reduce the default output allowance to 2,000 tokens | Not a verified defect or measured optimum. A ceiling is not actual consumption, and the report makes reduction conditional on recoverable evidence and task-level results it does not supply. Retained the existing default. |
| 2.5 Standalone MCP declarations reference an undefined envelope | Confirmed and fixed in both standalone and bundled declarations. |
| 3.1 Disabled project contents can fail startup | Confirmed and fixed. Content-read errors from trusted active sources still propagate; operational discovery errors still propagate. |
| 3.2 Mixed filesystem identity and repeated root probes | Mixed-filesystem canonicalization was confirmed and fixed. Overlapping metadata walks exist; a load-scoped discovery cache remains a performance proposal. |
| 3.3 Root hooks disappear when local `.codex` is absent | Confirmed and fixed through project-layer loading. Only hooks come from the primary checkout; unrelated root settings remain excluded and untrusted contents remain unread. |
| 3.4 Remote and local configuration acquisition is serialized | Confirmed ordering, not a measured latency defect. Retained acquisition/error precedence; concurrency restructuring was not necessary for the correctness repairs. |
| 3.5 Remote provider fields are silently defaulted | Confirmed and fixed in the protobuf and conversion. Non-default AWS/search values now survive a round trip; existing default-valued messages retain their behavior. An old receiver still cannot understand fields absent from its schema. |
| 4.1 Terminal attempts strand running validation callbacks | Confirmed and fixed at the existing-operation update boundary. Late success remains an execution fact, without a current proof epoch. No assertion is made that sealing itself kills a live external process. |
| 4.2 A post-execution scan can manufacture proof freshness | Confirmed and fixed conservatively with a fresh start observation and equal start/end epochs. The task evidence summary checks the same input identity. This is live-workspace observation, not a pinned snapshot or dependency-complete proof; mutate-and-revert during execution is outside that guarantee. |
| 4.3 Filesystem work holds SQLite's writer | Confirmed intentional publication ordering. A cross-process workspace capture sequencer, reservations, and asynchronous GC would be a substantial replacement. No such redesign was added or performance win claimed. |
| 4.4 Strict capture scans rejected fallback data / fails unborn HEAD / loses process settlement | Confirmed and fixed. Diagnostic and non-Git capture retain their existing fallback contract. Local Git-command deadlines were not added; the report supplies no hanging-Git reproduction in this checkout. |
| 4.5 Activity resets productivity and generates wakes | Confirmed policy. Removing activity from progress requires a replacement novelty definition and protection for legitimate long operations. No loop trace establishes a safe new heuristic; current wake/recovery policy was retained. |
| 5.1 Failed blank-line matching can become another edit | Confirmed and fixed, including EOF placement. Genuine blank replacements and valid EOF insertion still succeed. |
| 5.2 Repeated anchors are rejected before complete-block disambiguation | Confirmed intentional acceptance policy, explicitly tested. Retained conservative ambiguity rejection rather than changing which edits the tool accepts without an established targeting contract for repeated anchors. |
| 5.3 Standalone patch execution can leave a committed prefix | Confirmed documented nontransactional behavior, with exact/inexact deltas. The native handler already verifies before execution and re-reads during actual mutation. A shared reusable prepared-plan/commit architecture was not introduced, and multi-file atomicity is not claimed. |
| 5.4 Missing excerpts erase structured facts / delivery failures invite unsafe replay | Fixed the actual structured-fact loss. The omitted consumer already tracks committed deltas and withholds automatic retry after mutations or uncertain writes. The standalone flush diagnostic also explicitly says the patch was applied. No new mutation/delivery protocol was needed to repair that demonstrated information loss. |
| 5.5 Diagnostic location loses blank offsets, omits late discrepancies, and has unbounded search work | Confirmed and fixed. Budget exhaustion yields an unavailable excerpt without changing the mutation decision or discarding failure identity. |

## Other assertions in the reports

* Snapshot pagination does re-hash the entire immutable snapshot for every page.
  The 64 GiB figure is logical work for the stated 64 MiB / 64 KiB example, not
  measured disk traffic. Integrity checks remain; no unsafe pathname-only cache
  was introduced.
* Complete task retrieval retains validation history, capsules, observations,
  wake events, and handoff data. Persistence size is not evidence of billed model
  context. Compact status projections and notification batching remain proposals.
* Receipt publication can reject a repeated submission after sealing. Durable
  operation-key idempotency and output replay were not added without the required
  consumer-side commit contract.
* Review/verification gates, broad epochs, and activity classifications exist.
  Their cost does not establish that they are redundant under the applicable
  task policy. No new mandatory workflow or weaker acceptance gate was added.
* Patch parser copies and prospective diff generation exist. The native consumer
  confirms verification followed by execution; removing the execution-time read
  would also remove freshness checking across approval and intervening edits.
* Claims about token savings, whole-agent completion, unnecessary model turns,
  official-versus-fork performance, and the frequency of any failure remain
  unverified without the missing traces and controlled comparisons.

## Source anchors

The affected paths and relevant retained contracts are:

* [Host lifecycle](../codex-rs/code-mode-host/src/main.rs),
  [callback ownership and routing](../codex-rs/code-mode-host/src/peer.rs),
  [host admission and transport](../codex-rs/code-mode-host/src/lib.rs).
* [Schema projection](../codex-rs/code-mode-protocol/src/description/schema_ts.rs),
  [tool declarations](../codex-rs/code-mode-protocol/src/description/metadata.rs),
  [session ownership contract](../codex-rs/code-mode-protocol/src/session.rs),
  [wire results](../codex-rs/code-mode-protocol/src/host/payload.rs),
  [core output projection](../codex-rs/core/src/tools/code_mode/mod.rs).
* [Configuration acquisition](../codex-rs/config/src/loader/mod.rs),
  [remote conversion](../codex-rs/config/src/thread_config/remote.rs),
  [source protobuf](../codex-rs/config/src/thread_config/proto/codex.thread_config.v1.proto).
* [Coordination lifecycle](../codex-rs/agent-task-store/src/local.rs),
  [workspace capture](../codex-rs/agent-task-store/src/workspace.rs),
  [evidence summaries](../codex-rs/agent-task-store/src/model.rs).
* [Patch matching and diagnostics](../codex-rs/apply-patch/src/lib.rs),
  [matching tiers](../codex-rs/apply-patch/src/seek_sequence.rs),
  [native patch recovery](../codex-rs/core/src/tools/runtimes/apply_patch.rs),
  [standalone result delivery](../codex-rs/apply-patch/src/standalone_executable.rs).

The shutdown dependency contract is documented by Tokio's
[standard input documentation](https://docs.rs/tokio/latest/tokio/io/fn.stdin.html)
and [runtime shutdown implementation](https://docs.rs/tokio/latest/src/tokio/runtime/runtime.rs.html).

## Validation and activation

* Configuration selection: 28 passed.
* Coordination library: 112 passed; one intentionally child-only helper skipped.
* Patch and protocol libraries: 187 initially passed; the two corrected fixture
  failures subsequently passed. Already passing tests were not rerun.
* Host library and stdio integration: 36 passed, including the real-process
  shutdown regression.
* Additional capture-savepoint regression: 1 passed.
* Standalone patch CLI and scenarios: 42 passed.
* Total: 408 distinct tests passed. These were focused checks, not the entire
  repository suite.
* Scoped diff whitespace checks and nightly Rust formatting checks passed.
  Initial stable-formatter warnings about nightly options were resolved by using
  the installed nightly formatter. Git's LF/CRLF notices reflected checkout
  line-ending conversion.

Test binaries were built locally. The installed Desktop binary was not replaced,
and Desktop was not restarted. Existing independent checkout edits were preserved.
