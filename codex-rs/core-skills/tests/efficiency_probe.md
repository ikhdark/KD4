# Core-skills targeted measurements — 2026-09-27

## Scope and method

- Windows, Rust 1.98.1, Cargo's unoptimized test profile; not the installed Desktop binary.
- `efficiency_probe.rs` calls the real public skill loader and service APIs against real temporary files. Its filesystem wrapper counts provider calls, not every internal OS syscall.
- Three warmups, then 30 timed samples per local scenario. Plugin A/B order alternates. Each plugin sample starts with a fresh preload collection; OS file caches are warm.
- The delayed-filesystem scenario adds 5 ms before each manifest metadata request and takes 10 samples. This is a sensitivity experiment, **not a measured remote environment**.
- The two cache probes run 20 independent real-file scenarios each. The race uses notification barriers, not probabilistic sleeps; its timeout only prevents a hang.
- The report uses the conventional median (average of the central pair), recomputed from retained microsecond samples. The harness's printed `median_ms` is the upper central sample. P95 uses nearest rank.
- Production source was restored after the temporary empty-root A/B patch. No installation, publication, or Desktop restart was performed.

## Results

The initial pass was noisy. A restored-baseline control was run after the candidate; it used the original loader again. Do not combine the two timing regimes into a single advertised speedup.

| Scenario | Initial baseline median | Restored baseline median / P95 | Reuse or guard median / P95 |
| --- | ---: | ---: | ---: |
| Plugin skills, two load phases, no preload versus shared preload | 143.652 ms | 41.637 / 55.499 ms | 21.041 / 28.925 ms in the restored control |
| Four empty/missing roots, local filesystem | 25.070 ms | 4.235 / 5.183 ms | 0.501 / 0.656 ms in the guard build |
| Same roots, synthetic 5 ms manifest-metadata delay | 570.899 ms | 493.865 / 507.619 ms | 0.507 / 0.664 ms in the guard build |

The initial plugin preload median was 80.802 ms (P95 393.468 ms), versus the initial no-preload median of 143.652 ms (P95 479.329 ms). Both runs showed the same exact operation-count reductions.

### 1. Plugin preload reuse: concrete but bounded local benefit

Fixture: eight plugin roots, four skills per root, an 8,192-byte body per skill. The two passes model the skill-reading portion of plugin discovery followed by skill collection; they do not measure a complete `skills/list` request.

| Provider work per two-pass sample | Without preload | Shared preload |
| --- | ---: | ---: |
| Walks | 16 | 8 |
| Canonicalizations | 96 | 48 |
| File reads | 64 | 32 |
| Bytes read | 527,296 | 263,648 |

Every sample asserted that both outcomes contained 32 skills, that their complete skill metadata matched, and that the reused second pass performed zero additional provider operations. The mechanism is supported already; the listing caller needs to retain and pass the matching preload instead of throwing that work away.

Interpretation: approximately **20.6 ms saved** for this fixture in the restored control, with file reads halved. This is a catalog-load optimization, not evidence of faster model generation or a large saving on every turn. The full listing caller was source-traced, not end-to-end benchmarked.

### 2. Forced refresh: stale model-facing snapshots in 20/20 trials

Sequence: warm `snapshot_for_config` with description `before`; write `after`; call `snapshot_for_cwd(force_reload=true)`; request `snapshot_for_config` again.

- The forced list result contained `after` in 20/20 trials.
- The next model-facing snapshot still contained `before` in 20/20 trials.
- Explicit `clear_cache()` made the next config snapshot return `after` in 20/20 controls.

The desired-freshness assertion failed. This establishes a cache-coherence defect, not a timing improvement. No Desktop watcher was running to mask the service-level behavior. A candidate cache-unification fix was not implemented or timed.

### 3. In-flight invalidation: stale overwrite in 20/20 trials

Sequence: pause an older load after it has captured real `before` bytes; write `after`; clear caches; complete a newer load and assert it publishes `after`; release the old load; inspect the next cached result.

- The older load overwrote the newer cached snapshot in 20/20 trials.
- Both the newer-load controls and subsequent explicit-clear controls returned `after` in 20/20 trials.

The desired publication-order assertion failed. This proves the generation-check gap under a controlled overlap. It does not estimate how often that overlap happens during daily use, and a candidate generation fix was not implemented or timed.

### 4. Empty roots: small local gain, latency-sensitive gain

Fixture: four roots at the same depth, two existing empty directories and two missing directories.

- Original loader: four walks, four root canonicalizations, **80 manifest metadata calls**.
- Guard prototype: the same four walks and canonicalizations, **zero metadata calls**.
- Both versions returned zero skills and zero errors for this fixture.

The prototype inserted this guard immediately after collecting `resolved_skills` in `load_skills_under_root`, before namespace resolution:

```rust
if resolved_skills.is_empty() {
    return;
}
```

Interpretation: use the conservative local control, approximately **3.7 ms saved for four roots**, not the initially observed 24.6 ms. This is low priority as a normal local-turn speed optimization. It becomes material when metadata requests have network-like latency; the synthetic delayed scenario saved about 493 ms. Error-preservation and broader loader compatibility were not separately exercised for shipping this prototype; it was removed.

## Priority after measurement

1. Fix cache correctness to avoid using obsolete skill metadata; both defects were reproducible.
2. Reuse the existing plugin preload where the caller has already loaded the same roots/configuration.
3. Keep the empty-root change to the small early return; do not build a new caching/discovery subsystem for a few local milliseconds.

No full-turn duration, model-request count, model behavior, task-token usage, prompt-cache hit rate, or production-release timing was measured. Filesystem snapshot reuse is not the model's prompt-token cache.

## Reproduction and retained evidence

From `codex-rs`, run the isolated target through the existing lane owner:

```powershell
python ../scripts/rust_build_status.py run-lane --lane codex-core-skills -- cargo test --locked --offline -p codex-core-skills --test efficiency_probe -- --ignored --nocapture --test-threads=1
```

The complete baseline target currently exits 101 because its two correctness assertions expose the existing cache bugs. Its two timing tests pass. To run only the timing scenarios, insert `measure_` after `--test efficiency_probe`.

For the empty-root candidate only, apply the three-line guard above temporarily, set `SKILLS_EMPTY_ROOT_CANDIDATE=1` in that command's process, and select `measure_empty_roots`. The environment variable changes the expected call-count assertion; it does not itself optimize the loader. Restore the source and unset the flag afterward.

Retained local artifacts under `codex-rs/target/tmp/core-skills-efficiency-20260927/`:

- `baseline.log` / `baseline-build.json`: initial setup build, stopped by a harness format-string typo; repaired before measurements.
- `baseline-run.log` / `baseline-run-build.json`: two timing tests passed; both 20-trial cache probes failed their desired-freshness assertions.
- `candidate.log` / `candidate-build.json`: empty-root guard timing test passed.
- `restored-control.log` / `restored-control-build.json`: both timing tests passed with the original loader restored.
- `measurements.json`: all raw timed samples, recomputed medians, operation counts, correctness-probe counts, and source hashes.

Stable rustfmt emitted warnings that repository nightly-only formatting options were unavailable; compilation and measurements proceeded after the harness typo was corrected.

Another task added an executor-server benchmark module during the run. Inspection showed a `#[cfg(test)]` module declaration, not a change to the executor filesystem used by this integration target. Those independent edits were preserved. Shared-host activity and the observed timing drift prevent treating these microbenchmarks as isolated production latency measurements. Tooling also emitted freshness invalidations during concurrent activity: results here describe the captured builds and logs, not a blanket validation of every current workspace dependency.

Restored loader SHA-256: `57c7df7bc7cff5c2ca30659578115e61009cc6a507bd7058c85cb78557225d24`.
Service SHA-256: `185c5f0addc15e49cb4099a1808435fc4e317f7b51fbda749a69ab461aa6890d`.
Harness SHA-256: `4e92144a0615fe2243d87d716a044da71e97c48a5ba4c5b0cec2c3859be9c507`.
