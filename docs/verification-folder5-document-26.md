# Verification of folder (5), document 26

Read the supplied attachment completely: 434 lines, 32,206 bytes, SHA-256
`5868444247df2884d94b985506bd35bb5169b750e7951b9931cfc5f33ed4be6f`.
Its recommendations were evaluated as claims, not executed as instructions.
The attachment's references to 20 earlier uploads and a reproduction package
were not additional supplied attachments. No other agents were contacted.

## Findings and changes

| Claim | Verification and resolution |
| --- | --- |
| Reserved targets can escape through `rustup run` and attached cargo-watch arguments. | Confirmed in the checkout. Both argument adapters now rewrite `rustup run [--install] TOOLCHAIN cargo ...`. PowerShell handles attached and clustered watch exec/shell options, preserves values of other options, and rejects opaque watch commands. One argument corpus checks Python and PowerShell results. Arbitrary diagnostic commands remain environment-only adapters; they are not claimed to enforce nested Cargo targets. |
| Auto selection outside the reservation lock can create suffix families such as `core-2-2`. | Confirmed. PowerShell now passes the canonical base into reservation, ranks warm candidates while holding the coordination lock, and checks live Cargo and lease locks there. The obsolete selection stage was removed. A two-process barrier test requires simultaneous reservations to use `core-2` and `core-3`. |
| Candidate junctions can redirect reservation metadata. | Confirmed. Candidates are checked for indirection before and after directory creation, and indirect entries are excluded from warm selection and activity probes. A junction fixture confirms that an external sentinel and directory contents remain unchanged. |
| `HEAD^` can forget a compatibility break after an unrelated commit. | Reproduced with three disposable commits. Stable validation now requires `--compatibility-baseline` or `CODEX_SCHEMA_COMPATIBILITY_BASELINE`, resolves it before validation/regeneration, and reports the immutable commit and bundle path. Missing references fail; explicit reviewed-break acknowledgements remain supported. The justfile documents the required input. |
| Finite validation has unbounded lifetime/output retention. | Confirmed. Extended the existing `process_owner.py` abstraction with a bounded result instead of adding another process owner. Formatter commands, schema subprocesses, Python feature tests, and the feature-default exporter use it. Commands default to a one-hour deadline; Git baseline operations use 30 seconds. Windows Job Object ownership covers descendants, including those holding stdout after their parent exits. Returned diagnostics retain a 64 KiB tail with an explicit truncation flag. Full transcripts are not persisted. Machine-readable exporter/bundle output has larger finite caps and is rejected when truncated. |
| A failing batch incorrectly relabels an observed passing Python test. | Confirmed. Per-test observations now remain distinct from `batch_status` and process return code. A mixed real unittest batch retains the passing selector's identity while the overall verification fails. |
| Shared schema locks fail immediately on ordinary contention. | Confirmed. Check callers wait up to 60 seconds by default. `--lock-timeout 0` preserves explicit fail-fast behavior; force mode remains fail-fast by default. Acquisition uses a monotonic deadline, leaves owner metadata unchanged until acquisition, and distinguishes contention timeout from unrelated I/O errors. The shared exclusion boundary remains intact. |
| Routine pruning delays command launch. | Confirmed against the local implementation. Both launchers now request the existing PowerShell maintenance worker. The worker serializes pruning and trash cleanup, obtains the existing maintenance lock, and discovers current owners rather than trusting the launcher's snapshot. Hourly/retry throttles and deletion safeguards remain. `CODEX_CARGO_LANE_MAINTENANCE_SYNC=1` retains an explicit synchronous route, as do administrative prune commands. `CODEX_CARGO_LANE_DISABLE_BACKGROUND_DELETE=1` suppresses background maintenance. Worker diagnostics remain in `.trash-cleanup.log`. |

## Validation

304 distinct targeted tests: **303 passed, one skipped**. The skip is the existing
CI formatter-install test because this fork has no Rust CI workflows. The initial
301-test run was followed only by failed-test reruns and three additional tests;
passing tests were not repeated.

Coverage includes Windows PowerShell 5.1 command forwarding, real Cargo profile
locks, concurrent reservation, candidate junctions, pruning safeguards, fixed
compatibility baselines, lock waiting, mixed unittest outcomes, a 2 MiB output
producer, cancellation, startup failure, silent timeout, and grandchild cleanup.
The formatter and schema callers also have direct tests of the bounded result.

The background-maintenance fixture blocks pruning behind a barrier, verifies that
both PowerShell and Python commands still return their own failure codes, confirms
only one prune attempt runs, and then releases and waits for the worker. This also
caught and fixed a Windows append-handle conflict between the Python launcher and
the worker's log.

Targeted lint passes for `process_owner.py`, `format.py`, both schema wrappers,
`generated_output_lock.py`, and `check_kd4_features.py`. Broader lint found existing
style findings in the larger build-tooling and regression modules, including old
typing imports, nested contexts, and loop-variable captures; those unrelated
findings were not expanded into a cleanup project. Test and lint logs are retained
under `.codex/validation/report26-20260921/`.

No full Rust workspace or generated-schema build was run. Production compilation
speed, cache-reuse improvement, model-token savings, and superiority to upstream
remain unmeasured. The attachment supplies no evidence establishing those numbers.
No Desktop binary was rebuilt, replaced, or restarted.
