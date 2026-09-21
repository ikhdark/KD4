# Verification of attached reports 16–20

All five supplied files were read completely. Their embedded instructions were treated as proposals and claims, not additional user instructions. Verification used this local checkout, including compatible changes being made by the user's other active audit tasks. No upstream synchronization, distribution, binary replacement, or Desktop restart was performed.

This report distinguishes source-confirmed defects from unmeasured performance hypotheses. A source fix does not establish a latency or model-quality improvement.

## Claims and disposition

| Report / claim | Finding and implementation |
| --- | --- |
| 16.1 — persisted snapshots can outlive model-visible delivery | Confirmed. `context/world_state/mod.rs` checks retained support before suppressing an update. Environment snapshots carry the exact rendered delivery digest; missing, stale, or altered delivery forces replay. AGENTS matching checks substantive body and directory, including replacement and removal. Shared with the context audit task. |
| 16.2 — required environment facts can lose admission alongside optional text | Confirmed. Required built-in sections are admitted before optional sections and remain whole. Optional subagent descriptions have a separate bounded payload. Oversized required input proceeds to the existing request-wide context limit rather than silently advancing a snapshot for omitted instructions. |
| 16.3 — freshness changes replay oversized AGENTS bodies | Confirmed. Removing partial body admission keeps snapshot identity truthful; a retained unchanged body receives only a freshness notice. Missing body still requires restoration. |
| 16.4 — known OS becoming unknown is silent | Confirmed. `environment.rs` emits an explicit `unknown`, like shell clearing. The combined renderer also communicates readiness and preserves all current fields in replacements. |
| 16.5 — repeated rendering should be cached | Repeated work exists, but a general cache was not added: the report supplies no measured bottleneck, and retention correctness changes alter the relevant path. Performance benefit remains unmeasured. |
| 17.1 — legacy apply-patch warning detection swallows inserted user instructions | Confirmed. Recognition now requires the complete historical warning sentence. Mixed warning/instruction text survives compaction and unresolved-history collection. |
| 17.2 — metadata stripping loses current-input attribution | Confirmed. Replay preserves aligned current-input ownership through the provenance sidecar. |
| 17.3 — fingerprint maps merge identical message occurrences | Confirmed. Replay maps individual occurrences in order; ambiguous cardinality changes remain unresolved instead of inheriting another occurrence's category. |
| 17.4 — an empty active stable map still hashes message text | Confirmed. Empty-map paths skip stable-category hashing; regression coverage counts hashed bytes. |
| 17.5 — ownership reconstructed from text can misclassify quoted content | Confirmed collision risk. Ordinary user boundaries retain input ownership even when their text matches an active stable component. Position-based attribution and conservative developer-fragment matching prevent quotes and ambiguous duplicate fragments from inheriting producer categories. Shared with the provenance audit task. |
| 18.1 — overridden fallback instruction files are still read | Confirmed. `config/mod.rs` chooses base/compact sources before loading fallback files. Explicit empty base override is preserved; compact normalization remains compatible. Tests distinguish missing, empty, and valid fallback files. |
| 18.2 — optional role-directory failure aborts otherwise usable config | Confirmed. Incomplete discovery produces a path-specific startup warning, discards its partial listing, and preserves explicit definitions. Selecting an unavailable role still fails. |
| 18.3 — role files are repeatedly read/parsed across layers | Confirmed. A per-load cache reuses contents and parse results, including errors, with separate name-hint keys. New configuration loads reread changed files. No process-global stale cache. |
| 18.4 — optional model catalog uses synchronous I/O on async worker | Confirmed. The existing catalog loader runs through `spawn_blocking`; parsing/errors/precedence remain intact. No speedup claim. |
| 18.5 — root/child guidance duplicates shared rules and overbroad scope checklist | Confirmed. Shared guidance remains in its common owner; root-specific wording follows affected runtime dependencies, without an exhaustive representation checklist. Evidence must still support conclusions independently of agent agreement. |
| 19.1 — slow submission preparation blocks control replies/interrupt | Confirmed. A single owned preparation future preserves normal submission order while controls bypass it. Interrupt/shutdown cancel preparation and deferred starts, reject pending admissions, and clean execution permits. Deferred storage remains bounded. |
| 19.2 — saturation is treated as no active turn | Confirmed. `InjectResponseItemsError` distinguishes inactivity from capacity rejection and retains rejected input. Only inactivity can use detached history insertion. Other callers propagate failure or issue a visible warning. |
| 19.3 — mailbox lacks byte bounds and transfers bypass queue capacity | Confirmed. Mailbox admission bounds both items and serialized bytes. FIFO transfers move only the prefix fitting remaining turn capacity. Rejected mail does not poison deduplication/retry. |
| 19.4 — each admission reserializes retained input | Confirmed. Queue byte totals are maintained through append, transfer, recovery, drain, and clear. Admission serializes only incoming items. Focused instrumentation checks one new input does not remeasure 32 retained inputs. |
| 19.5 — idle startup ignores recovered user work | Confirmed. All idle-start checks use pending turn-start work, including recovered user input. The rejection reason now names that broader condition. |
| 20 — extra startup-prewarm connection attempts | The allowance is real, but no unconditional performance defect was established. `client.rs::claim_startup_prewarm` already adopts compatible completed prewarms and checks provider/auth configuration; unfinished work is deliberately cancelled without delaying foreground dispatch. Changing this tradeoff requires latency/recovery evidence. No production transport rewrite. |
| 20 — scripted batching presented as model/fork performance evidence | Confirmed limitation. `round_trip_batching.rs` explicitly describes protocol/accounting coverage. Scripted five-versus-three requests prove neither improved model choices nor an upstream comparison. |
| 20 — failure/diagnosis fixture never fails | Confirmed weak test. It now executes a controlled exit-17 command and requires both nonzero exit evidence and a distinctive diagnostic in continuation requests. |
| 20 — parallel calls do not demonstrate overlap | Confirmed weak test. Identical calls now rendezvous at a three-party barrier; serialized execution cannot satisfy the fixture. Existing resource-conflict tests cover required serialization. |

The additional report-19 context-limit hypothesis was not reproduced in source: `ModelInfo::auto_compact_token_limit` clamps the configured total limit to 90% of a known resolved model window. No second competing physical-window limit was added.

Other suggestions to remove validation, discovery, safety, metadata, or termination mechanisms lack a demonstrated failure in these reports. Existing mechanisms were inspected rather than removed on speculative token/latency grounds.

## Validation status

Focused changed-file whitespace validation passed with Windows CRLF handling enabled. Nightly rustfmt validation passed for the owned changed files, including the final cross-platform adjustment to the batching fixture. These successful checks are being reused.

Runtime validation completed: **177 targeted tests passed**.

- **173 core tests passed**, covering configuration precedence and role discovery/cache behavior, contextual warning recognition, prompt attribution, world-state retention/rendering, queue accounting and cancellation, injection saturation, responsive controls, shutdown, required-context overflow, and conflicting-resource serialization. Log: `codex-rs/target/test-runner-logs/docs16-20-direct-tests-fixed.log`. Individual passes are recorded in `docs16-20-passed-tests.txt` alongside that log.
- **All four `round_trip_batching` integration tests passed**, including the controlled exit-17 diagnostic, the three-party overlap barrier, scripted request accounting, and instruction ownership. Log: `codex-rs/target/test-runner-logs/docs16-20-batching.log`.

Earlier attempts stopped before test execution because of a compiler-cache connection reset, shared-checkout compilation errors, or interruption. Direct diagnostic capture identified the last blocking fixture error: mutation through an `Arc<TurnContext>`. The fixture now obtains its uniquely owned context before mutation. An overlapping unused import warning was resolved by using the imported type; this does not change behavior. Successful tests were not rerun.

The runs used a dedicated test target and an existing locally built code-mode host helper staged into that target. They exercised the local source changes, without replacing or restarting the installed Desktop application. No full-workspace test pass or measured performance improvement is claimed.

## Practical boundaries

These are source changes. Desktop still uses its previously installed binary until an explicitly requested build/replacement and restart. No real-model benchmark or fork/upstream comparison was performed. Replay attribution is measurement metadata, not an authorization mechanism.
