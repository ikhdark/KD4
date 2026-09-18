# Repo Benchmark

Repo Benchmark is the only repository-supported mechanism for A/B benchmarking. It reports
measurements and does not issue performance pass/fail verdicts. Incorrect results, failed tasks, and
invalid setup remain explicit failures.

The Rust package is `codex-rs/repo-benchmark`, beside `core`. It drives each revision's own native
app-server. The Python session audit remains the sole diagnostic analyzer and has no A/B mode.

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
| How costly are local builds? | Local workflow snapshots and opt-in Cargo lane phase timing below | Child wall time includes compilation, cache access, tests, and any child-side waits |
| How costly are sandboxing, extensions, or a specific transport? | Their focused correctness tests and available subsystem telemetry | No isolated cost estimate follows from file size, test timeout, or test serialization |

For a change, identify the affected observable behavior, use its focused tests,
and choose an existing scenario that actually exercises it. Record the expected
effect and selected metric before comparing revisions. Keep model, reasoning,
permissions, instrumentation, and workload comparable. An unavailable metric is
not zero, and correctness failures must remain visible even if successful
attempts look faster.

`retained_process` retains behavioral evidence but contributes no comparable
metrics: the fork and upstream clamp the requested yield interval differently.
Cancellation workloads still require a stopped child and no late file writes.

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

## Tool latency diagnostics

The session audit's `runnerDiagnostics.toolActivity.durations` summarizes captured
native item lifecycles by kind, including command execution, MCP calls, edits,
and collaboration calls. `rgSearch` selects only single `rg` commands; shell
pipelines and compound commands cannot attribute their duration to the search.
Failed searches and the normal no-match exit status still contribute elapsed time.

```text
python scripts/kd4_turn_latency_audit.py --runner-evidence C:\path\to\native-evidence.json --tokens off --summary-json
```

Each summary reports observed and measured counts, missing/invalid durations,
the measured total, and extrema. `totalMs` is unavailable if any observed item
lacks a usable start/completion pair. `observedTotalMs` retains partial evidence.
Duplicate notifications count once by thread, turn, and item ID. Durations use
notification timestamps: they include transport/scheduling delay and may overlap.
Their sum is neither exclusive process time nor the elapsed union. This provides
diagnostics for captured MCP calls without claiming an isolated MCP server cost.
The section is present in full JSON, bounded JSON, and text output. Bounded JSON
omits zero-valued tool phase entries with an explicit `omittedZeroPhases` count;
full JSON retains the complete phase vocabulary and population details.

## Local workflow measurements

[kd4_perf_snapshot.py](../../../scripts/kd4_perf_snapshot.py) already measures
local workflow commands. Its quick profile covers Python startup, Git status,
and feature validation; the phase0 catalogue also includes CLI builds, a focused
core test, app-server initialization tests, and a Desktop publish dry run.
Select only the scenario needed for the change:

```text
python scripts/kd4_perf_snapshot.py --profile quick --iterations 2 --output C:\path\to\quick.json
python scripts/kd4_perf_snapshot.py --scenario local-cli-build --iterations 2 --output C:\path\to\build.json
```

The snapshot records revision, dirty paths, platform, Python version, CPU count,
and installed binary identity (hashing is opt-in). Its `cold_ms` field means the
first invocation and `warm_p50_ms` means subsequent invocations; these labels do
not establish OS, compiler, or provider cache state. It does not measure actual
Desktop installation, end-user paint latency, or a complete edit-to-runtime loop.
Record background builds and other competing work when interpreting differences.

For a breakdown inside the existing lane runner, pass a new output filename:

```text
python scripts/rust_build_status.py run-lane --lane measurement --timing-json C:\path\to\lane.json -- cargo check --manifest-path codex-rs/Cargo.toml -p codex-utils-absolute-path
```

The schema-1 record separates reservation, command setup, throttled maintenance,
child-command execution, and reservation release. `reservation` includes lock
coordination and lane selection; it is not an isolated lock-poll or process-enumeration
cost. Requested and resolved lanes expose fallback to another lane. Measurements
use a monotonic clock and include failed phases; unentered phases remain null.
Failures retain their exit status or error type, and timing does not change the
child's exit code. Output files are created exclusively to preserve prior samples.
The record excludes Python/PowerShell startup and final report writing. On an
exception, unwinding is included in the active phase. Compare the same command,
toolchain, wrappers, cache policy, and lane state; no regression threshold is
inferred from one run.

## Measurement coverage index

This index distinguishes available measurements from subsystem cost experiments.
Use the owning implementation's focused tests for correctness before measuring.

| Area | Evidence and limits |
|---|---|
| TUI pacing and rendering | [Chunking policy](../../../codex-rs/tui/src/streaming/chunking.rs) tests exercise thresholds and hysteresis. [Commit ticks](../../../codex-rs/tui/src/streaming/commit_tick.rs) trace mode transitions with queue depth/age. Neither proves that the constants are optimal or measures terminal paint, highlighting, wrapping, diffs, tables, history insertion, or resume-picker latency. |
| Startup and prewarm | Startup diagnostics above consume validity, schema, phase, overlap, and prewarm-status fields. They do not separate Node/process launch or prove prewarm benefit. |
| Build tools and storage | Workflow snapshots and lane timing above measure command-level costs. Dedicated cache-hit history, lock-poll cost, disk growth trends, and instrumentation overhead remain separate questions. [Changed-file test routing](../../../scripts/root_maintenance.py) already includes build performance tests. |
| Transport and providers | Scripted SSE workloads do not establish WebSocket/fallback, HTTP proxy, local-provider, backend, or UDS performance. [TUI runtime metrics](../../../codex-rs/tui/src/chatwidget/turn_runtime.rs) are merged and displayed; their existence does not supply an A/B transport comparison. |
| Sandbox and policy | A danger-full-access workload cannot establish sandbox, hardening, policy-check, or approval overhead. Turn timing and approval/wait counters describe observed work under the recorded permission policy. |
| Search and filesystem | Search notification durations above and the Git-status workflow scenario provide bounded coverage. They do not establish watcher, indexing, filesystem, deep-history, or large-dirty-workspace performance. |
| Hooks, MCP, and extensions | Native item durations cover observed external tool calls; [metric names](../../../codex-rs/otel/src/metrics/names.rs) also include hook duration. Neither establishes extension-specific overhead or prompt-token attribution. |
| Packaging and activation | The publish dry-run scenario measures preparation checks. It does not measure replacing binaries, archive generation, installation, restart, or comparisons between Codex homes. |
| Source size and architecture | File size, timeout values, and serialized tests do not demonstrate a runtime or build bottleneck. Use a measured scenario before imposing size budgets or refactoring. |
| Protocol and generated schemas | Generated timing fields document transport shape. Runtime profiles and the Python analyzer supply measurement semantics; schema size and serialization/validation cost require separate observations. |
| Authentication and cloud | A local scripted run does not measure keyring, login, AWS, account-tier effects, cloud task services, or migration cost. |
| Observability | Metric declarations, emitted measurements, and local analysis are different coverage levels. [Task metrics](../../../codex-rs/core/src/tasks/mod.rs) emit turn memory; their [tests](../../../codex-rs/core/src/tasks/mod_tests.rs) check it. Export overhead, cardinality, CPU profiling, and trace-to-report joins are not measured here. |
| Experiment design | Reports preserve failures, exclusions, sample IDs, provenance, and outside-execution durations. [Workload verifiers](../../../codex-rs/repo-benchmark/src/workloads.rs) retain hashes for protected files. All-pass results can compare latency but cannot distinguish success rate; all-fail results cannot establish time to a verified solution. |

OTEL metric names are not interchangeable with turn-profile fields. For example,
`codex.startup.phase.duration_ms` uses milliseconds while startup trace summaries
retain nanoseconds; `codex.tool.call.duration_ms` and native notification durations
have different boundaries. `codex.turn.ttft.duration_ms` is not terminal paint
latency. Keep units, collection boundaries, validity, and coverage alongside any
comparison. The tools above consume saved files and do not require an OTEL collector.

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
just repo-benchmark scan -fast
just repo-benchmark scan -full
just repo-benchmark prepare -full
just repo-benchmark prepare -full --reference C:\path\to\candidate
just repo-benchmark compare --prepared C:\absolute\path\prepared.json -full
just repo-benchmark import --result C:\absolute\path\result.json
just repo-benchmark rerun --result C:\absolute\path\result.json --attempt ATTEMPT_ID
just repo-benchmark rerun --result C:\absolute\path\result.json --analysis-only
```

`run`, `prepare`, `compare`, `import`, and `rerun` are operations of this one command. Fast is the
default. The exact single-hyphen flags `-fast` and `-full` cannot be combined. Prepared comparisons
retain their selected mode; changing the mode requires preparation again.

Every run compares `fork_on` with `reference` (upstream by default).
`scan` is an alias for `run`. There is no disabled-feature fork variant or
variant-selection switch. Prepared manifest schema 3 freezes this comparison;
older manifests must be prepared again before comparison, rerun, or import.

Both modes schedule 84 scripted attempts (14 workloads × three clusters ×
two variants). Fast schedules two live attempts; full schedules six. Reports
contain the overall comparison and both variants' configuration. Individual
feature effects are not isolated.

| Mode | Scripted execution ceiling | Real-model work and ceiling |
|---|---:|---|
| Fast | 30 minutes | Rust task Ã— two variants; 50 minutes |
| Full | 30 minutes | Rust, TypeScript, Python tasks Ã— two variants; 150 minutes |

Each real-model attempt has a twenty-minute ceiling. Execution is sequential: scripted work,
cleanup, then real-model work. Variants and independent attempts run one at a time. These are
maximum allowances; finite schedules stop immediately when finished. Unused time never creates extra
repetitions or model calls. Preparation, builds, resets, independent verification, cleanup, and
analysis are recorded outside execution budgets. Independent verification has its own two-minute
limit.

## Revisions and controls

A defaults to committed local `main`. B defaults to the newest locally available stable upstream
release tag (`rust-vMAJOR.MINOR.PATCH`, ordered numerically). Prerelease tags and unreleased
`upstream/main` commits do not advance the baseline. Preparation never fetches; upstream release
tags must already be available locally. Missing stable release tags produce an error. `--reference`
explicitly selects another checkout's committed HEAD. No ancestry relationship is required.
`--fork-ref` explicitly selects another committed fork revision. An older fork revision without the
inventoried runtime controls fails preparation rather than silently incorporating working-tree
edits.

| Variant | Native implementation |
|---|---|
| `fork_on` | A with implemented controls enabled and deliberately disabled features still disabled |
| `reference` | B's own app-server; displayed as `reference (upstream)` for the default |

`fork_on` against `reference` measures the overall result, including runtime controls,
instrumentation, and other fixed fork differences.

The feature inventory is the selected fork commit's `kd4_features.toml`, including phase reasoning
and `features.kd4_runtime`. Reports separate declared controls, enabled settings, evidence of
exercise, and fixed differences. Enabling a feature alone does not prove that a workload exercised
it. Preparation freezes declarations; it does not claim that feature test gates ran against the
selected revision.

Preparation pins all source, executable, fixture, analyzer, configuration, and environment
identities. No benchmark sources or Cargo registrations are injected into candidate checkouts.
Native builds use each revision's own dependencies and lockfile, twelve Cargo jobs, release
optimization, thin LTO, four codegen units, disabled incremental compilation, and the same pinned
toolchain. Each build keeps its final executables in a separate persistent target directory. Cargo's
intermediate build directory is shared across revisions within each fork/reference lane, so
unchanged dependencies can be reused when the selected revision changes. Cargo owns fingerprint
invalidation and build locking; available sccache provides additional reuse. The first build
populates the shared intermediates. Intermediate paths are recorded as optional provenance and are
not required to verify a completed build.

Upstream is not rebuilt for every benchmark. After the first build, preparations reuse the verified
native artifacts under `codex-rs/target/repo-benchmark/builds/reference` and print that Cargo was
skipped. A newer local stable release selects a new baseline and builds it once. Changes to the
fork, benchmark mode, schedule, or harness alone do not invalidate upstream's build. Build inputs
and toolchain identities must still match; changed build inputs or a deleted cache require a build,
and failed artifact verification stops preparation. Keep prior prepared source directories alongside
the build cache because their pinned lockfile and configuration remain part of verification.
Original build duration is retained separately from each preparation's cache verification time.

For revisions using upstream's custom V8 release, preparation follows that revision's dependency
setup: download the matching archive and Rust bindings from the official Codex release, verify the
release manifest against the revision's tracked checksum, then verify both artifacts against that
manifest. Their URLs, hashes, and effective build paths are preserved in build provenance and
checked before reuse. This supplies build dependencies without changing the selected revision or its
Cargo manifests.

## Tasks, diagnostics, and evidence

The scripted segment covers requests/history/cache behavior, direct/nested/parallel tools, retained
processes, cancellation, continuation, and tool completion after follow-up and restart/resume. Its
fixed schedule is three independent clusters across 14 workloads and two variants: 84 attempts, one
measured observation per workload/variant in each cluster and no warmup attempts. Each attempt
starts a fresh native process, so the old in-process warmup/repetition counts did not warm
subsequent processes and made the native schedule infeasible. The first cluster covers every
selected workload/variant combination. The schedule alternates which variant runs first across
workloads and clusters; each variant occupies both positions for every workload. Both modes use this
schedule and the unchanged shared 30-minute ceiling. Completing within that ceiling still requires
evidence from the actual run. Three clusters provide descriptive measurements only. Intervals
require at least five independent paired clusters; failed or unrun pairs reduce coverage further.
This is an explicit reporting floor, not a guarantee of statistical power. The complete schedule is
frozen in preparation; old manifests whose schedule differs must be prepared again. This segment
never computes token diagnostics.

Real-model task 1 fixes a Rust bug in a synthetic repository with generated discovery-noise files.
Task 2 adds a TypeScript feature across files in a similar synthetic repository. These fixtures
measure focused edits amid search noise, not production repository complexity. Task 3 performs a
behavior-preserving Python refactor in a pinned KD4 Git snapshot. Focused tests and protected
independent verifiers check the requested behavior, meaningful regression tests, and final workspace
changes. Task snapshots exclude working-tree build output and untracked content; shared instructions
and scripts are separately recorded identical additions.

All variants receive the same supplied root AGENTS, scripts, task root, prompts, permissions, shell,
executable search paths, and dependencies. Homes and sessions are isolated. The exact approved base
configuration is used without remaining user/project configuration or service-tier overrides.
Authentication is supplied separately and excluded from preserved configuration.

Tool discovery accepts both top-level declarations and Responses Lite's in-band
`additional_tools`. The exclusive-tool scenario overlaps an `apply_patch` mutation
with a command, then applies the final mutation and reads the resulting files in
a separate tool call before independent verification on every variant. This also
handles native invalidation of tool evidence after a mutation. Restart/resume
restores the exact prepared configuration bytes before
relaunch, clearing native workspace-trust persistence while preserving sessions.
Windows configuration checks accept equivalent canonical paths and schema-version
1 migration metadata; additional settings remain rejected. Rust verification uses
a short temporary build path so deeply nested evidence paths do not break MSVC.

Preparation records how each arm's fixed configuration and feature overrides differ from the current
checkout's explicit `.codex/config.toml`. Reports show project-only keys, benchmark-only keys, and
keys with different values, together with the captured file's hash. Values are not retained in this
comparison. This is not the layered daily effective configuration: home settings and defaults are
outside its scope. Missing project configuration or older preparation manifests are reported as
unavailable. Code-mode host differences across builds are also called out because they affect
nested-tool comparability.

Native tool dispatch retry and reentry counts appear in the diagnostic summary and comparison tables
only when all captured turns have the required counters and no tool timing overflow. Unknown counts
remain unavailable; partial observations are retained separately. Request retention compares the
uncapped `counters.modelRequestCount` with retained `modelRequests`; a mismatch withdraws complete
request, continuation and token totals. Provider usage with contradictory cached, uncached, output
or total counts cannot establish complete token coverage. These checks consume existing native
evidence and do not generate extra model calls.

Raw native evidence is saved before the frozen Python analyzer runs. Startup failures, unfinished
turns, rejected tools, retries, verification failures, and missing telemetry remain visible.
Real-model token data comes from native telemetry, with provider counts distinguished from estimates
and unsupported reference categories marked unavailable. No recording proxy is used. Expensive
analysis and workspace verification occur outside measured execution.

Reports lead with completion and failures, distinguishing changes outside the permitted task scope
from incorrect behavior. Report schema 3 compares named metrics independently: elapsed time, tool
and turn counts, first-output and first-action timing, request volume, complete live token usage,
discovery work, and outside-execution durations. Missing or partial evidence excludes the attempt
for that metric rather than supplying zero. Statistics retain matched pairs, run clusters,
distributions, sample identities, and 10,000-resample bootstrap intervals when supported. Markdown
suppresses tail comparisons below 20 observations in either arm; JSON retains the interpolated
sample quantiles. Behavior counts remain descriptive without significance tests. One live attempt
per task and variant is an observation, not evidence of repeat-to-repeat model variance. Different
tasks are not repeated samples of one task.

Scripted request count and compact UTF-8 JSON request-body bytes are measured from the captured
provider requests through the frozen analyzer. Bytes include tools and input and are a
request-volume proxy, not tokenizer output, transport bytes, or provider cache hits. Scripted token
regressions are not measured directly. Single live attempts cannot establish behavioral regression
rates or variance. The live fixtures do not cover long-session compaction, approval decisions, user
corrections, or broad production refactors. Discovery counts describe recognized tool-call events,
not distinct files read, retained edits, or instruction compliance.

Every started attempt retains its inputs, native evidence, logs, verifier result, identities, and
specific rerun command. Analysis failure preserves the raw experiment. Analysis-only reruns and
report import make no model calls; live reruns preserve the original and can produce different
behavior.

Generated run artifacts live under the prepared paths beneath `codex-rs/target/repo-benchmark`.
Imported reports go to this documentation directory's ignored `accepted` subdirectory. Historical
reports retain their original provenance. Old prepared comparisons must be prepared again.

## Behavior measurements

The feature inventory reports configured and observed effective settings for
both variants. Settings and declared verification references are shown
separately from observed exercise and do not certify a passing gate. The
overall comparison does not isolate individual feature effects.

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

## Audit AJâ€“AY: verified changes and measurement limits

The behavior vector uses the existing `investigation-evidence-v1` envelope.
Its snapshot hashes the captured rollout byte identities, independently of file
locations. Completeness describes the exported measurements: unavailable values
remain null with reasons. Discovery recognition is explicitly approximate and
versioned; it is not an execution classifier. Display truncation does not change
the vector or its snapshot.

Artifact creations, reuses, omitted sections and recovery-associated generations
are additive runtime counts, summed across captured complete turns. Omitted
sections use the runtime's 64-bit range; other counts use 32 bits. Saturated,
absent or malformed counters are unavailable. Recovery association means a
tool-result generation followed a recovery read; it does not prove the read
caused the generation. These counts do not establish artifact expiry, storage
pressure, distinct files recovered, or unnecessary work. Collection reuses the
existing offline audit and adds no runtime instrumentation.

`configuration.sha256` hashes the captured `config/read` effective values, with
stable JSON ordering, excluding layer provenance. Missing snapshots remain
unknown. This captures benchmark startup configuration, not later per-thread or
per-request overrides. Reports flag differing hashes and missing coverage.
Native timing, generation lineage, token coverage and nested dispatch identity
include the thread as well as the turn; directory audits distinguish rollout
files. Repeated terminal notifications contribute once within that identity.
Summed thread durations are not elapsed task time.

| Section | Source evidence and disposition |
| --- | --- |
| AJ | `nextest.toml` local/fast profiles disable fail-fast. `rust_test_runner.py::require_core_lib_filter` rejects unfiltered core library runs. Validation duration is command wall time, without compile/lane-wait decomposition; failure-fixing efficiency cannot be inferred from it. No optional runtime telemetry was added. |
| AK | Projection/recovery counters now reach comparisons together, including omitted sections and recovery-associated generations. `read_tool_output.rs` supports line ranges and resumable continuation: its 2,000-line cap is per read, not proof that execution must be repeated. Token counts are estimates, not exact billed compression. Reserved recursive spill is excluded. |
| AL | `command_shape.rs` supplies structured invocations and `command_search.rs` classifies rg narrowing; the latter is not a separate search tool. Audit discovery remains a labelled heuristic of call text. Adding a general dispatch taxonomy requires a separate contract; unmatched text cannot establish a zero unknown-operation rate. |
| AM | `generationPurposeLatency`, per-request purpose/effort and token diagnostics already exist. Purpose is not interchangeable with configured reasoning phase. No phase tuning or causal conclusion follows from these observations. |
| AN | Captured configuration hashes and report warnings expose a confounder. Benchmark isolated configuration differs from `.codex/config.toml`; local historical configuration is not reconstructed from today's file. |
| AO | The existing evidence envelope now accompanies the vector, including completeness, approximation, limitations and byte-snapshot identity. No parallel schema or analyzer was introduced. |
| AP | Existing artifact creation/reuse counters are exported. Retention limits, search budgets and expiry behavior remain unchanged; there is no evidence here that tuning them improves outcomes. |
| AQ | Runtime `parentCallId` provides nested counts separately from direct calls. Thread/turn/call identity now prevents collisions in captured timing and generation links. Source regex labels are not executed nested-call counts; missing or overflowed runtime coverage remains a limitation. |
| AR | Native items already use thread-scoped identity; timing and usage now do too. Captured children are not proof of complete task coverage. Durations are summed, and parent wait time is not labelled human delay or useful work. |
| AS | Reason, phase, status and context-size fields live in `analytics/src/facts.rs::CodexCompactionEvent`; their existence does not establish their availability in captured rollouts. Existing compaction time does not prove context pressure, summary quality or harmful rediscovery. No threshold was changed. |
| AT | Interruptions, prompts and feedback do not reliably distinguish correction from cancellation or task growth. No correction classifier, feedback upload or causal quality score was added. |
| AU | Runtime `TaskKind` is Regular/Review/Compact; hook and subagent activity are separate concepts. Comparisons already group by workload and segment. Session totals do not claim homogeneous user implementation tasks or infer intent from task text. |
| AV | Comparisons preserve units, sample counts, raw values, medians and missing measurements. New output counters retain their separate components; no composite score or threshold was added. |
| AW | Existing `analysis_only` recomputes frozen evidence without model calls. Execution budgets are time limits, not spending guarantees. Scheduling live runs and adding a token budget are proposals, not verified defects fixed by this audit. |
| AX | Changes extend the canonical offline analyzer and existing report path. No dashboard, LLM judge, live CI gate, new framework or fixture corpus was added. |
| AY | Discovery/governor vectors already reach `Observation` and `summarize`; the output counters and metadata follow that path. Regression coverage includes the frozen Python audit and Rust report comparisons. Live repetitions and automatic session/scheduled collection remain separate future work. |

## Focused validation

Use `cargo test --locked -p repo-benchmark --jobs 6`, the affected feature/core gates, the Python
audit/timing tests, and `just source-map-check`. Do not run the full repository suite. Completion
requires a full-mode execution covering scripted work and all nine live attempts with preserved
evidence; fast selection is checked without another paid matrix.

The approved implementation plan and fixed paths are recorded verbatim in the installed Stay On
Track reminder record. Its checkpoints apply before implementation, verification, and completion.
Builds or required execution that remain blocked must be reported as blocked, not as completed
validation.

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

The live attempt allowance is 20 minutes. Fast and full segment ceilings are 50 and 150 minutes,
covering fork-on and upstream with 25% scheduling headroom. Replay uses the budgets frozen in each
prepared manifest; these defaults apply to new preparations.

Cancellation verification checks both PID creation time and actual exit state. A published retained
process can intentionally survive turn interruption; such an attempt remains failed and does not
become a passing pair merely because the turn reported interrupted.
