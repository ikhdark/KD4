# Local validation concurrency sample (2026-09-27)

Windows x64, Ryzen 7 9850X3D (8 cores / 16 logical processors). These are
bounded local measurements, not a universal optimum or a clean-build benchmark.
The shared checkout had concurrent edits/builds; exclude recompiling gate runs
from warm-gate comparisons. All measured executions completed successfully.

| Workers | Incremental compile, seconds | Warm Nextest gate, seconds | In-process libtest, seconds |
| --- | --- | --- | --- |
| 2 | 3.42, 3.36 | 8.85 | 3.00, 2.95 |
| 4 | 4.18, 3.39 | 7.40, 7.50 | 2.02, 2.06 |
| 6 | 3.48, 3.47 | 7.34, 7.36 | 1.83, 1.90 |

## Workloads and decision

- Compile: `cargo build --lib -p codex-tools -p codex-config -p codex-utils-output-truncation`
  in the reserved `core-tests` target directory. Warm dependencies first, then
  touch the three crate roots without changing bytes and restore their mtimes
  after each run. Trial order: 2, 4, 6, 6, 4, 2.
- Nextest: `run-gate verified-runtime-contracts capability-kd4-turn-execution`
  through `scripts/rust_test_runner.py`, local profile, 20 core library cases
  plus three transport cases. Warm first; use the same forward/reverse order.
  One 2-worker run rebuilt after a concurrent edit (50.43 seconds, including
  42.44 seconds reported by Cargo); it is not a warm timing sample.
- Libtest: run the same 20 exact core library cases in the already-built test
  executable, with the declared code-mode helper and an 8 MiB worker stack.
  Vary `RUST_TEST_THREADS` in the same order; assert selection before execution.
- Keep Cargo jobs at **2**; use **6** for Nextest and libtest. Environment/CLI
  overrides still win. Nextest's fixed-port and legacy-session groups stay at
  **1**, process-heavy tests at **2**, and Windows core at **4**. Six versus four
  Nextest workers is a small difference, since this sample is core-group-capped;
  do not infer a broad six-worker speedup from it. Use the grouped Nextest path
  for process-heavy/fixed-port gates; libtest has no Nextest group enforcement.

Raw local logs and one-off experiment drivers are under
`codex-rs/target/validation-{parallelism,gate-parallelism,libtest-parallelism}*`
(ignored build artifacts). The initial nine-case turn-execution-only gate also
passed at all three settings. No full suite or clean release build was measured.
