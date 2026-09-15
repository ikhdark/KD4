# Repo Benchmark

Repo Benchmark is the only repository-supported mechanism for A/B benchmarking. It reports measurements and does not issue performance pass/fail verdicts. Incorrect results, failed tasks, and invalid setup remain explicit failures.

The Rust package is `codex-rs/repo-benchmark`, beside `core`. It drives each revision's own native app-server. The Python session audit remains the sole diagnostic analyzer and has no A/B mode.

## Choose a measurement

Use the [session audit](../../../scripts/kd4_turn_latency_audit.py) for a recorded
session and Repo Benchmark for comparisons between revisions. The audit shares
definitions with [timing analysis](../../../scripts/kd4_timing_analysis.py) and
[first-action analysis](../../../scripts/kd4_first_useful_action_analysis.py).
These files and the benchmark are routed through the `repo-benchmark` source
owner in [source_owners.toml](../../../source_owners.toml).

| Question | Existing evidence | Limits |
|---|---|---|
| Where did a turn spend time? | Audit phase unions, exclusive durations, first actions, tool lifecycles | Requires supported, complete telemetry; overlapping phases are not additive |
| Where did session startup spend time? | Audit `--startup-log` phase summaries and validity diagnostics | Requires captured JSON trace logs; ends at first real model send, may include user idle time |
| Did a revision improve the task? | Repo Benchmark paired measurements and independent task verifiers | Scripted work measures harness scenarios; sparse live samples do not establish model variance |
| How much work happened outside execution? | Report `outside_<phase>_ms` comparisons | Does not measure logging/instrumentation overhead inside execution |
| How quickly did text become visible? | TUI chunking and rendering correctness tests | This benchmark does not measure terminal paint or perceived streaming latency |
| How costly are builds, sandboxing, extensions, or a specific transport? | Their focused correctness tests and available subsystem telemetry | No isolated cost estimate follows from file size, test timeout, or test serialization |

For a change, identify the affected observable behavior, use its focused tests,
and choose an existing scenario that actually exercises it. Record the expected
effect and selected metric before comparing revisions. Keep model, reasoning,
permissions, instrumentation, and workload comparable. An unavailable metric is
not zero, and correctness failures must remain visible even if successful
attempts look faster.

A session audit is the smallest diagnostic run and makes no model calls:

```text
python scripts/kd4_turn_latency_audit.py C:\path\to\rollout.jsonl --summary-json
```

It is not a smaller A/B experiment. Fast/full benchmark time limits and their
coverage are described below.

## Startup diagnostics

The existing app-server logger accepts `LOG_FORMAT=json`. To capture startup
snapshots, set `RUST_LOG=error,codex_core::session::turn=trace` in the environment
of the app-server process and preserve its stderr. Use the ordinary client to
start a session and send a turn, then analyze the captured log:

```text
python scripts/kd4_turn_latency_audit.py --startup-log C:\path\to\app-server.stderr.jsonl --tokens off --json
python scripts/kd4_turn_latency_audit.py C:\path\to\rollout.jsonl --startup-log C:\path\to\app-server.stderr.jsonl
```

`startupTiming` is an optional diagnostic section in full JSON, bounded JSON,
and text output. Startup snapshot schema 1 is independent of turn timing schema
versions. The analyzer checks `profile_valid`, transition/clock/saturation
diagnostics, numeric fields, and phase/overlap bounds before summarizing durations.
It retains prewarm status, exclusion reasons, and the source log's byte count and
checksum. Repeated frozen snapshots are counted once by correlation ID and startup
timestamp; conflicting snapshots exclude that profile. Truncated/non-JSON log
lines are counted as parse errors. No valid captured snapshot means durations are
unavailable. Coverage describes captured profiles only; logging may be incomplete.

The measured interval begins during session construction and ends at the first
real model-send boundary. It excludes process launch and Node wrapper startup,
can include time waiting for user input, and is not a cold/warm classification.
Phase unions and their overlap fields must not be summed or subtracted from turn
wall time. A `ready` prewarm status alone does not prove a latency improvement.
Trace collection is opt-in and can add overhead; the normal benchmark does not
enable this logging or turn these diagnostics into A/B metrics.

## Interpreting and recovering a run

For example, a hypothetical paired elapsed-time change of -12 ms with an interval
from -35 to +18 ms is compatible with both improvement and regression. An interval
entirely below zero supports a decrease for that sampled workload and setup; it
does not establish a meaningful user benefit or isolate an individual feature.
If the report has too few independent pairs for an interval, treat the difference
as descriptive. Check failures, excluded pairs, and sample identities before
acting on a mean or tail estimate. Do not pool different tasks as repetitions.

- Missing startup timing: use JSON stderr from a process with the trace filter
  enabled and a session that reached its first real model send. Rollouts alone do
  not contain these startup snapshots.
- Missing/invalid timing or token data: inspect audit coverage and exclusion
  reasons. Unsupported reference telemetry remains unavailable.
- Preparation rejects a revision, dependency, or identity: inspect that failure
  and prepare again after resolving it. Preparation uses committed revisions;
  working-tree edits do not enter the candidate automatically.
- Analysis fails after execution: preserved native evidence remains available.
  Use the result's analysis-only rerun command after resolving the analyzer error;
  this makes no model calls. A changed frozen analyzer requires fresh preparation.
- A run is interrupted: recover its attempt checkpoints through analysis-only
  rerun. Failed and unrun attempts are not successful measurements.

Imported `accepted` reports are local, ignored artifacts, not committed baselines.
Keep their prepared inputs and evidence when a future comparison must be
reproducible; a report name alone does not establish compatible provenance.

## Run

```text
just repo-benchmark
just repo-benchmark -fast
just repo-benchmark -full
just repo-benchmark prepare -full
just repo-benchmark prepare -full --reference C:\path\to\candidate
just repo-benchmark compare --prepared C:\absolute\path\prepared.json -full
just repo-benchmark import --result C:\absolute\path\result.json
just repo-benchmark rerun --result C:\absolute\path\result.json --attempt ATTEMPT_ID
just repo-benchmark rerun --result C:\absolute\path\result.json --analysis-only
```

`run`, `prepare`, `compare`, `import`, and `rerun` are operations of this one command. Fast is the default. The exact single-hyphen flags `-fast` and `-full` cannot be combined. Prepared comparisons retain their selected mode; changing the mode requires preparation again.

| Mode | Scripted execution ceiling | Real-model work and ceiling |
|---|---:|---|
| Fast | 30 minutes | Rust task × three variants; 30 minutes |
| Full | 30 minutes | Rust, TypeScript, Python tasks × three variants; 90 minutes |

Each real-model attempt has a ten-minute ceiling. Execution is sequential: scripted work, cleanup, then real-model work. Variants and independent attempts run one at a time. These are maximum allowances; finite schedules stop immediately when finished. Unused time never creates extra repetitions or model calls. Preparation, builds, resets, independent verification, cleanup, and analysis are recorded outside execution budgets. Independent verification has its own two-minute limit.

## Revisions and controls

A defaults to committed local `main`. B defaults to locally available `upstream/main`; `--reference` selects another checkout's committed HEAD. No ancestry relationship is required and preparation never fetches. `--fork-ref` explicitly selects another committed fork revision. An older fork revision without the inventoried runtime controls fails preparation rather than silently incorporating working-tree edits.

| Variant | Native implementation |
|---|---|
| `fork_off` | A with the inventoried KD4 controls disabled |
| `fork_on` | A with implemented controls enabled and deliberately disabled features still disabled |
| `reference` | B's own app-server; displayed as `reference (upstream)` for the default |

`fork_off` against `reference` measures fork drift. `fork_on` against `fork_off` measures the controlled feature effect. `fork_on` against `reference` measures the overall result. Fixed instrumentation and other fork differences remain in both fork variants, so drift is not parity.

The feature inventory is the selected fork commit's `kd4_features.toml`, including phase reasoning and `features.kd4_runtime`. Reports separate declared controls, enabled settings, evidence of exercise, and fixed differences. Enabling a feature alone does not prove that a workload exercised it. Preparation freezes declarations; it does not claim that feature test gates ran against the selected revision.

Preparation pins all source, executable, fixture, analyzer, configuration, and environment identities. No benchmark sources or Cargo registrations are injected into candidate checkouts. Native builds use each revision's own dependencies and lockfile, six Cargo jobs, release optimization, thin LTO, four codegen units, disabled incremental compilation, and the same pinned toolchain. Separate persistent target directories and available sccache support verified reuse.

For revisions using upstream's custom V8 release, preparation follows that revision's dependency setup: download the matching archive and Rust bindings from the official Codex release, verify the release manifest against the revision's tracked checksum, then verify both artifacts against that manifest. Their URLs, hashes, and effective build paths are preserved in build provenance and checked before reuse. This supplies build dependencies without changing the selected revision or its Cargo manifests.

## Tasks, diagnostics, and evidence

The scripted segment covers requests/history/cache behavior, direct/nested/parallel tools, retained processes, cancellation, continuation, and tool completion after follow-up and restart/resume. Its fixed schedule is three independent clusters × 14 workloads × three variants: 126 attempts, one measured observation per workload/variant in each cluster and no warmup attempts. Each attempt starts a fresh native process, so the old in-process warmup/repetition counts did not warm subsequent processes and made the native schedule infeasible. The first 42 attempts cover every workload/variant combination before another cluster starts; across three clusters each variant runs first, second and third once per workload. Both modes use this schedule and the unchanged shared 30-minute ceiling. Completing within that ceiling still requires evidence from the actual run. Three clusters provide descriptive measurements only. Intervals require at least five independent paired clusters; failed or unrun pairs reduce coverage further. This is an explicit reporting floor, not a guarantee of statistical power. The complete schedule is frozen in preparation; old manifests whose schedule differs must be prepared again. This segment never computes token diagnostics.

Real-model task 1 fixes a Rust bug in a synthetic repository with generated discovery-noise files. Task 2 adds a TypeScript feature across files in a similar synthetic repository. These fixtures measure focused edits amid search noise, not production repository complexity. Task 3 performs a behavior-preserving Python refactor in a pinned KD4 Git snapshot. Focused tests and protected independent verifiers check the requested behavior, meaningful regression tests, and final workspace changes. Task snapshots exclude working-tree build output and untracked content; shared instructions and scripts are separately recorded identical additions.

All variants receive the same supplied root AGENTS, scripts, task root, prompts, permissions, shell, executable search paths, and dependencies. Homes and sessions are isolated. The exact approved base configuration is used without remaining user/project configuration or service-tier overrides. Authentication is supplied separately and excluded from preserved configuration.

Preparation records how each arm's fixed configuration and feature overrides differ from the current checkout's explicit `.codex/config.toml`. Reports show project-only keys, benchmark-only keys, and keys with different values, together with the captured file's hash. Values are not retained in this comparison. This is not the layered daily effective configuration: home settings and defaults are outside its scope. Missing project configuration or older preparation manifests are reported as unavailable. Code-mode host differences across builds are also called out because they affect nested-tool comparability.

Native tool dispatch retry and reentry counts appear in the diagnostic summary and comparison tables only when all captured turns have the required counters and no tool timing overflow. Unknown counts remain unavailable; partial observations are retained separately. Request retention compares the uncapped `counters.modelRequestCount` with retained `modelRequests`; a mismatch withdraws complete request, continuation and token totals. Provider usage with contradictory cached, uncached, output or total counts cannot establish complete token coverage. These checks consume existing native evidence and do not generate extra model calls.

Raw native evidence is saved before the frozen Python analyzer runs. Startup failures, unfinished turns, rejected tools, retries, verification failures, and missing telemetry remain visible. Real-model token data comes from native telemetry, with provider counts distinguished from estimates and unsupported reference categories marked unavailable. No recording proxy is used. Expensive analysis and workspace verification occur outside measured execution.

Reports lead with completion and failures, distinguishing changes outside the permitted task scope from incorrect behavior. Report schema 2 compares named metrics independently: elapsed time, tool and turn counts, first-output and first-action timing, request volume, complete live token usage, discovery work, and outside-execution durations. Missing or partial evidence excludes the attempt for that metric rather than supplying zero. Statistics retain matched pairs, run clusters, distributions, sample identities, and 10,000-resample bootstrap intervals when supported. Markdown suppresses tail comparisons below 20 observations in either arm; JSON retains the interpolated sample quantiles. Behavior counts remain descriptive without significance tests. One live attempt per task and variant is an observation, not evidence of repeat-to-repeat model variance. Different tasks are not repeated samples of one task.

Scripted request count and compact UTF-8 JSON request-body bytes are measured from the captured provider requests through the frozen analyzer. Bytes include tools and input and are a request-volume proxy, not tokenizer output, transport bytes, or provider cache hits. Scripted token regressions are not measured directly. Single live attempts cannot establish behavioral regression rates or variance. The live fixtures do not cover long-session compaction, approval decisions, user corrections, or broad production refactors. Discovery counts describe recognized tool-call events, not distinct files read, retained edits, or instruction compliance.

Every started attempt retains its inputs, native evidence, logs, verifier result, identities, and specific rerun command. Analysis failure preserves the raw experiment. Analysis-only reruns and report import make no model calls; live reruns preserve the original and can produce different behavior.

Generated run artifacts live under the prepared paths beneath `codex-rs/target/repo-benchmark`. Imported reports go to this documentation directory's ignored `accepted` subdirectory. Historical reports retain their original provenance. Old prepared comparisons must be prepared again.

## Behavior measurements

The feature inventory reports how many entries change runtime settings, retain
identical settings, cannot be ablated, or lack configuration evidence. Per-key
settings expose shared controls; features sharing a switch cannot have their
individual effects isolated by that comparison. Declared verification references
are shown separately from observed exercise and do not certify a passing gate.

Incomplete token accounting leaves top-level totals and cache share unavailable.
`observedTotals` retains partial provider counts and output-only observations;
these are not complete attempt costs. Request classification summaries count all
request records before display truncation, with overlapping tags kept separate.

Rust and Python quantiles use linear interpolation at `(n-1)*q`. First-action
summaries include mean, population standard deviation, range, and per-metric
coverage, with timing schema counts and separate superseded/unterminated turns.
Exclusion rates name their denominators. Reports also show excluded pair rates
per metric and completion caveats for incomplete segments, so surviving samples
cannot silently stand for the entire scheduled experiment.

Audit schema 20 carries `behaviorMetrics` with independent `behaviorSchemaVersion: 2`.
The fixed vector includes discovery events, searches, reads, broad searches,
excess repeated searches, executed validations, model retries/fallbacks, and
provider total tokens, plus validation duration, suppressed validation output,
governor interventions, planning generations and iterations, and interaction
wait counts. It also carries canonical/model-visible tool-output token estimates
and recovery-call/retruncated-section counts from the runtime. Compare both token
totals alongside both recovery counts: smaller projections alone do not establish
less work. These are sums of runtime projection and recovery operations, including
recorded nested operations, rather than model-visible calls. Output tokens are
estimates, not provider usage or billed tokens, and remain available for scripted
attempts. The reserved recursive-spill counter is omitted because its constant
zero is not a measurement. These counters measure activity, not quality, mistakes, or user
corrections. Validation duration sums command wall time in nanoseconds.
Discovery totals use the full event stream before display
truncation. A batch remains one observed tool-call event. Repeated searches count
only subsequent identical decoded commands within the same turn; scopes and
options remain significant, and a digest protects private query identity.

Missing, partial, or saturated telemetry produces `null` with a reason, never an
inferred zero. Benchmark reports sum each metric across distinct captured rollout
sessions only when every session supplies that metric under the supported schema.
Scripted token metrics remain disabled. The `discovery_*` and `behavior_*`
comparisons preserve raw sample IDs, distributions, and paired differences in
counts, nanoseconds, or tokens; they have no confidence interval, quality gate, or causal claim.
Structured tool outcomes take precedence over output-text status heuristics.
The bounded JSON keeps metric values and availability reasons, omits explanatory
prose and per-interval prompt categories, and uses `sameAs: "all"` when a population
duplicates the aggregate. Complete details remain in the full JSON report.

Terminal turns without start events or with unresolved tools cannot establish
complete behavior measurements. Replayed terminal records contribute counters
once. A counter at its integer maximum is unavailable even when the general
saturation flag is zero. Non-progress inference requires explicit `true` for
unchanged state and explicit `false` for a changed next action; missing values
establish neither. Governor eligibility is not recorded, and interaction counts
require matching permission policies for comparison.

The frozen Python audit recomputes behavior from preserved evidence during normal
analysis and analysis-only reruns. `runnerDiagnostics.toolActivity` counts unique
native item lifecycles by `(threadId, turnId, itemId)`, retains per-thread/turn
breakdowns, and distinguishes observed, started, completed, and pending items.
Duplicate notifications do not add work or reopen completed items. Completion
includes failed/interrupted terminal items; it does not mean success. Notification
durations require an observed start and completion; a completion alone has no
measured duration.

`observed_tool_items` and `observed_tool_items_<kind>` compare captured command,
MCP, dynamic, generic, collaboration, web-search, and file-change items. These
counts include child-thread notifications that were captured, but do not prove
complete child coverage or count model decisions. A native item can represent
multiple nested dispatches. The legacy `tool_executions` metric covers completed
root-thread items in the native turn loop. Missing evidence remains unavailable;
an observed terminal turn with no native tool items establishes zero observed
items only. Repeated edits and searches alone do not establish mistakes or wasted
work.

Live `cache_hit_rate` is cached input divided by input, a fraction in [0, 1]. It
requires complete provider usage, valid cache counts, and nonzero input. It remains
unavailable for scripted attempts, partial usage, or zero input. Input, cached
input, output, and reasoning totals retain their separate accounting. Token and
item comparisons exclude failed, timed-out, unverified, and warmup attempts through
the existing completion filter. They describe observed effort for verified tasks;
they do not prove a causal behavior regression or feature exercise. Compare like
model and reasoning configurations. Full diagnostic JSON retains details omitted
from Markdown, alongside each attempt's rerun command and frozen input identities.

Rollout-trace recording remains opt-in: enabling additional trace recording on
the timed path would require assessing overhead and compatibility across all
selected revisions. The Python audit remains authoritative for benchmark metrics.

## Focused validation

Use `cargo test --locked -p repo-benchmark --jobs 6`, the affected feature/core gates, the Python audit/timing tests, and `just source-map-check`. Do not run the full repository suite. Completion requires a full-mode execution covering scripted work and all nine live attempts with preserved evidence; fast selection is checked without another paid matrix.

The approved implementation plan and fixed paths are recorded verbatim in the installed Stay On Track reminder record. Its checkpoints apply before implementation, verification, and completion. Builds or required execution that remain blocked must be reported as blocked, not as completed validation.

Execution checkpoints are stored per attempt in `attempt.json`; the complete
`result.json` is written when the run starts and finishes. An analysis-only rerun
of an interrupted run recovers those checkpoints, preserving partial and failed
attempts without repeatedly rewriting earlier event traces.
Mutable checkpoints include their checksum in the same atomically published JSON
record, so an interrupted replacement cannot split the payload from its checksum.
Native JSON, rollouts, and captured logs have identities bound into each attempt;
reanalysis and import reject changed evidence. A process-held file lock at the
prepared workspace prevents concurrent invocations from resetting the same tree.

Workspace resets verify snapshot bytes and restore changed or missing files,
remove added files and old Git state, and retain unchanged files in place. The
final source digest and saved changes exclude `.git`, `target`, `node_modules`,
and `__pycache__`; these are source-state measurements, not build-output hashes.
The pinned KD4 fixture still contains the full committed source tree.
