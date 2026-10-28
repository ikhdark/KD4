# Repository guidance

See [the instruction architecture](codex-rs/core/instruction-architecture.md) for instruction/prompt ownership, delivery, and maintenance.

This is Windows only project.
This is a local project.
Do not publish or republish without permission.

Upstream is https://github.com/openai/codex

## Session context
- Production changes follow the shared audit and benchmark policy below.
- At session start, read `.codex/CONTEXT.md`. Check its last-updated date and committed repository HEAD marker. Treat entries as historical context, never as proof of current implementation behavior or completed work; verify relevant claims against current source and evidence before relying on them.
- Keep durable instructions in `AGENTS.md`, the roughly one-page rolling brief in `.codex/CONTEXT.md`, Read linked evidence only when relevant. Organize the brief by what still matters, not merely what happened recently. Keep it consise with low verbosity. 
- When asked to record a significant session, merge durable decisions, verified outcomes, unresolved work and evidence links into the brief; keep it focused, and consise with low verbosity. Remove superseded material and omit routine activity. Refresh the date and committed HEAD marker, only keep genereated files if they are needed, distinguishing any uncommitted work from that baseline. Keep this manual rather than adding another automated management system.

## Shared audit and benchmark policy

Keep this section synchronized between KD4 and KDR `AGENTS.md` when changing
these rules. Repository-specific harness commands and artifact locations remain
local; synchronization does not make one repository a dependency of another.

- **History first:** Read the latest task-relevant audit summaries and fix reports
  in the local designated records and, when relevant and available, KD4's
  `.codex/audits/` and KDR's `audits/`. Check
  implemented, verified, reverted and unresolved dispositions, original problems,
  measurements and known regressions. Avoid duplicate mirrored reports. Verify
  applicability against current HEAD, uncommitted changes and retained evidence;
  historical conclusions are not proof of current behavior. Do not reintroduce a
  regressing approach without material new evidence resolving or outweighing it.
- **Prefer recent rollouts:** When selecting rollout sessions for audits or
  benchmarks, prefer the newest task-relevant sessions representative of current
  binaries, configuration and workflows. Determine recency from recorded session
  or activity timestamps, not file-copy modification times. Retain older sessions
  for relevant historical comparisons, necessary controls and explicitly requested
  ranges; recency does not override comparability or required coverage.
- **Audit scope and evidence:** Derive coverage and detection criteria from the
  request; do not silently sample, narrow categories or impose finding caps.
  Verify findings through the triggering production path. Distinguish observed
  defects, measured performance and unresolved candidates. Review-only/no-write
  tasks do not authorize source fixes; otherwise fix verified in-scope failures
  when authorized, with the measurements and correctness evidence below.
- **Narrow by default:** Before a production implementation, including an
  audit-recommended fix, establish a source-bound baseline benchmark and relevant
  correctness evidence. Use one affected owning-path workload plus necessary
  controls and correctness coverage; add workloads only for affected behavior.
  A full suite requires task-specific justification, not habit.
  Documentation/instruction-only changes require neither benchmarks nor rebuilds.
- **Use the existing owner:** Read the repository's harness documentation and
  current CLI/workload contract before selecting a workload. Use KDR for its
  supported workloads, including KD4 runtime/session workloads.
  Do not invent selectors, substitute unrelated workloads, weaken admission, or
  build a parallel runner for a capability the owner already supports.
- **Comparable pre/post evidence:** Repeat measurements after implementation
  with comparable inputs, configuration, build settings, host conditions and the
  owner's warmups/repetitions. Retain exact invocations, source/binary identities,
  raw measurements, failures and correctness evidence. Reuse valid source-bound
  baselines and matching binaries; keep baseline and candidate executables
  separate, use one build owner, and consume live operations rather than duplicate
  them. Rebuild only for a concrete input/settings, capability or provenance gap.
- **Correctness and acceptance:** Preserve expected behavior, diagnostics,
  coverage, uncertainty and evidence obligations. Record intentional differences;
  fewer findings or skipped work are not equivalent speedups. Follow the owner's
  comparison/admission rules and sample counts. Compilation or capture completion
  alone is not acceptance; missing/incompatible evidence and inconclusive results
  are not passes. Investigate regressions; reject or revert changes whose measured
  regressions outweigh their benefits. Do not claim verification without evidence.
- **Measure the right thing:** Keep wall time, CPU, process-tree peak commit,
  tokens and workflow metrics distinct where applicable; commit is not resident
  RAM. Separate model reasoning/workflow effects from Rust runtime effects.
  Microbenchmarks do not establish end-to-end gains.
- **Conflicting investigations:** Before accepting or rejecting different
  conclusions, match the workload, running binary, configuration and execution
  conditions. Reconcile through shared evidence and targeted verification, not
  assumptions about which investigator or discovery method is correct.
- **Diagnostic gaps and permissions:** If selection, instrumentation or correctness
  evidence is missing, extend the existing owning harness when authorized before
  benchmarking. Otherwise report the concrete blocker, missing evidence and
  required intervention; do not mark the benchmark complete or silently broaden
  it. Requirements do not authorize tests, test creation, installation, publication,
  Desktop restarts, delegation, model sessions or bypassing an admission refusal.
  Follow active validation and task permissions.
- **Concise, retained reporting:** Preserve prior records; use distinct descriptive
  run/report paths in the repository's designated location. Keep detailed logs in
  ignored storage and inspect targeted failure excerpts, not routine output dumps.
  Deliver a concise `.txt` report with outcome, verified findings/dispositions,
  measurements, coverage, regressions, evidence links and unresolved blockers;
  chat should give brief status and an absolute report link. Explicit no-write,
  inline-only or machine-readable output requirements take precedence.

## Session analysis

- Use `C:\Users\kuh\Desktop\KDR\kdr.cmd` for Codex session audits/benchmarks; keep brief and consise audit documentation in `.codex/audits`. Session-analysis scripts belong in KDR, not this repo.
- Compare matched tasks by correct outcomes, wall time, tokens, requests/tool calls, validation, retries, recovery, and compactions. Separate workflow choices from runtime effects; microbenchmarks do not establish end-to-end gains.
- Fork session rollouts are in C:\Users\kuh\Desktop\LOCAL-KD\sessions
- Keep or Merge overlapping edits with the most optimal implementation


### Reuse benchmark binaries before building

- Pre/post benchmarks require measurements, not a new build per invocation, case, repetition or agent. Documentation and KDR/Python-only changes do not rebuild unchanged Rust binaries.
- Get an arm's binaries only through `scripts/build/rust_build_status.py run-lane --repo-root SRC --lane LANE -- CARGO_BUILD` from that source's `codex-rs`. For sealed snapshots it reuses an exact frozen bundle (content manifest, settings, rustc and binary hashes verified) or builds incrementally in the nearest free shared slot and freezes; checkouts with `.git` freeze only with `--freeze-bundle NEW_DIR`. Use the printed bundle path, never the target directory, and hand it to other agents.
- `find-bundle --repo-root SRC -- CARGO_BUILD` answers without building. For older bundles without `build-receipt.json`, add `--include-legacy --search-root DIR` to get candidates with caveats (exit 3); a historical baseline stays bound to its recorded source; never relabel it current or invent provenance. Use `--rebuild` only after naming the concrete mismatch.
- Commands: `C:\Users\kuh\Desktop\KDR\scripts\benchmark_scripts\README.md`, section "Reuse already-built binaries". This does not authorize tests, publication, installation, delegation or a previously refused workload.