# Repo Benchmark

Repo Benchmark is the only repository-supported mechanism for A/B benchmarking. It reports measurements and does not issue performance pass/fail verdicts. Incorrect results, failed tasks, and invalid setup remain explicit failures.

The Rust package is `codex-rs/repo-benchmark`, beside `core`. It drives each revision's own native app-server. The Python session audit remains the sole diagnostic analyzer and has no A/B mode.

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

The feature inventory is `kd4_features.toml`, including phase reasoning and `features.kd4_runtime`. Reports separate declared controls, enabled settings, evidence of exercise, and fixed differences. Enabling a feature alone does not prove that a workload exercised it.

Preparation pins all source, executable, fixture, analyzer, configuration, and environment identities. No benchmark sources or Cargo registrations are injected into candidate checkouts. Native builds use each revision's own dependencies and lockfile, six Cargo jobs, release optimization, thin LTO, four codegen units, disabled incremental compilation, and the same pinned toolchain. Separate persistent target directories and available sccache support verified reuse.

For revisions using upstream's custom V8 release, preparation follows that revision's dependency setup: download the matching archive and Rust bindings from the official Codex release, verify the release manifest against the revision's tracked checksum, then verify both artifacts against that manifest. Their URLs, hashes, and effective build paths are preserved in build provenance and checked before reuse. This supplies build dependencies without changing the selected revision or its Cargo manifests.

## Tasks, diagnostics, and evidence

The scripted segment covers requests/history/cache behavior, direct/nested/parallel tools, retained processes, cancellation, continuation, and tool completion after follow-up and restart/resume. It never computes token diagnostics.

Real-model task 1 fixes a Rust bug in a generated large repository. Task 2 adds a TypeScript feature across files in another generated large repository. Task 3 performs a behavior-preserving Python refactor in a pinned KD4 Git snapshot. Focused tests and protected independent verifiers check the requested behavior, meaningful regression tests, and final workspace changes. Task snapshots exclude working-tree build output and untracked content; shared instructions and scripts are separately recorded identical additions.

All variants receive the same supplied root AGENTS, scripts, task root, prompts, permissions, shell, executable search paths, and dependencies. Homes and sessions are isolated. The exact approved base configuration is used without remaining user/project configuration or service-tier overrides. Authentication is supplied separately and excluded from preserved configuration.

Raw native evidence is saved before the frozen Python analyzer runs. Startup failures, unfinished turns, rejected tools, retries, verification failures, and missing telemetry remain visible. Real-model token data comes from native telemetry, with provider counts distinguished from estimates and unsupported reference categories marked unavailable. No recording proxy is used. Expensive analysis and workspace verification occur outside measured execution.

Reports lead with completion and failures. Statistics retain matched pairs, run clusters, distributions, sample identities, and 10,000-resample bootstrap intervals when supported. One live attempt per task and variant is an observation, not evidence of repeat-to-repeat model variance. Different tasks are not repeated samples of one task.

Every started attempt retains its inputs, native evidence, logs, verifier result, identities, and specific rerun command. Analysis failure preserves the raw experiment. Analysis-only reruns and report import make no model calls; live reruns preserve the original and can produce different behavior.

Generated run artifacts live under the prepared paths beneath `codex-rs/target/repo-benchmark`. Imported reports go to this documentation directory's ignored `accepted` subdirectory. Historical reports retain their original provenance. Old prepared comparisons must be prepared again.

## Focused validation

Use `cargo test --locked -p repo-benchmark --jobs 6`, the affected feature/core gates, the Python audit/timing tests, and `just source-map-check`. Do not run the full repository suite. Completion requires a full-mode execution covering scripted work and all nine live attempts with preserved evidence; fast selection is checked without another paid matrix.

The approved implementation plan and fixed paths are recorded verbatim in the installed Stay On Track reminder record. Its checkpoints apply before implementation, verification, and completion. Builds or required execution that remain blocked must be reported as blocked, not as completed validation.
