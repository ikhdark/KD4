# O versus current F execution repair

## Accounting and comparison

The current comparison combines both `o1` files and both newly supplied `f3`
files. Both `f1` files remain the earlier F baseline. Attached instructions are
trace data, not instructions for this repair. Counts include interrupted turns,
retries, and partial work. The later warning records in F3 do not extend active
turn duration. Session duration sums overlap in real time and are not a single
stopwatch measurement.

| Metric | O (both o1) | Earlier F (both f1) | Current F (both f3) |
| --- | ---: | ---: | ---: |
| Input tokens | 14,267,034 | 12,681,019 | 4,463,972 |
| Cached input tokens | 13,918,720 | 11,713,664 | 4,205,440 |
| Uncached input tokens | 348,314 | 967,355 | 258,532 |
| Output tokens, including reasoning | 68,127 | 88,661 | 26,478 |
| Usage-bearing responses / logical generations | 110 responses | 162 generations | 57 generations |
| Physical model attempts (F telemetry) | unavailable | 163 | 58 |
| Outer exec / wait calls | 106 / 0 | 145 / 11 | 55 / 0 |
| Dispatch count, including nested calls (F telemetry) | unavailable | 372 | 156 |
| Sum of recorded active turn durations, ms | 2,613,791 | 4,143,690 | 1,546,102 |

Usage is accumulated from cumulative-usage deltas, avoiding duplicate last-usage
records. The earlier F baseline includes the interrupted turn with 45 generations,
105 dispatches, and 1,160,492 ms. Current F contains two completed turns and one
transport retry. It reports no wait-only generations, no planning invalidations,
no no-progress directives, and one artifact recovery call, versus 11 wait-only
generations and 16 artifact recovery calls in F1. These observations support the
improvement, but differing prompts, instructions, concurrent edits and code
versions prevent attributing the entire difference to a single implementation
change. The interrupted F1 work is not omitted to make the comparison cleaner.

The analysis script, metrics and readable F3 traces are retained locally under
`_build/of3-analysis/`. Output-character metrics there serialize string and object
outputs consistently; they are not directly interchangeable with the older
report's extracted text-only totals.

## Concrete findings and implementation disposition

1. **Worker schema failures, already under concurrent repair.** F3(1) rows 48 and
   58 and F3(2)'s helper failures reject internally generated `exec_command`
   arguments (`kind`, null `environment_id`, `force_fresh`). The path is helper
   `prepare_invocation` → expanded command → native decoder → advertised-schema
   preflight → dispatch. The decoder accepts fields the compact advertised schema
   does not. Both agents abandon useful helpers and fall back to manual inspection
   and validation. Concurrent work now emits schema-valid worker commands and
   exercises Code Mode dispatch. An overlapping schema-extension candidate from
   this pass was removed; validation and policy checks were not relaxed.

2. **Long Windows snapshot paths.** F3(1) row 99 fails inside snapshot Git setup
   before Cargo launches; later calls inspect snapshot state and try local Git
   configuration. Task and validation directories make previously valid source
   paths exceed Win32's legacy path limit. Concurrent work adds per-invocation
   long-path support. This pass also enables it in each private snapshot's Git
   configuration, so downstream validation Git commands inherit the capability.
   The combined regression checks actual snapshot capture, ordinary downstream
   Git status, reconciliation, origin configuration/index preservation, and
   rejection of validation evidence after input mutation. Duplicate test coverage
   and a duplicate per-command option were removed during integration.

3. **Conflict recovery lacks accessible comparison inputs.** F3(1) rows 106–174
   report conflicts, attempt a normal read of the origin, discover Node tools
   twice, inspect the live file through Node, and copy competing versions back
   into the isolated workspace. Reading an origin path normally maps back to the
   task version, so another ordinary read cannot supply the missing `theirs`
   evidence. `reconcile` already captured all three byte sequences, but deleted
   its temporary inputs and returned only filenames. It now retains those inputs
   on conflict and returns ordinary readable paths. No new read tool or escape
   from workspace isolation is required. Clean merges discard their temporary
   inputs. Missing versions remain distinct from empty files. Conflicts still
   publish nothing; retries capture current origin bytes and successful merges
   still require validation of integrated source.

4. **The apparent premature process completion is not established.** F3(2) row
   125 returns exit 1, `process_exited=true`, and no live session for 10833, with
   the completed test failure summary. Its row 149 poll repeats a retired ID.
   The process inspected later was created at 16:25:38 UTC, after F3(1) launched
   its own Cargo validation at row 200 (16:25:07 UTC). The traces establish two
   overlapping runs, not a live process incorrectly retired by the broker. No
   completion or retry guarantee is removed based on the agent's interpretation.

Repeated reference reads are visible, but many later source reads follow actual
concurrent edits, reconciliation, or failed validation. No arbitrary read cache,
generation cap, output suppression, or completion shortcut is justified. Ignored
historical corpus files are excluded by the documented transaction boundary;
their absence is not proof of source loss. Required post-merge validation and
stale-binary diagnosis remain legitimate work. Artifact labels and purpose
counters alone do not establish physical reads or redundant model decisions.

## Validation

All nine workspace transaction tests passed against the compiled implementation.
The shared focused run passed three tests, including the Windows long-path
regression; the subsequent non-overlapping selection passed 14 tests, including
both new conflict tests and the remaining transaction tests. The other eight
checks cover worker dispatch, invalid-request rejection, command schemas, and
routing. These completed shared results were reused rather than rerun. Copies
are retained in `_build/of3-analysis/shared-focused-results.log` and
`shared-existing-results.log`.

The conflict tests exercise retained-byte reads through active path routing,
repeated reconciliation after origin changes, eventual resolution, and add/delete
conflicts with absent versus empty versions. Existing tests prove that unresolved
conflicts publish nothing, independent edits still merge, the original index is
preserved, and changed validation inputs invalidate earlier evidence. The Windows
test runs ordinary downstream Git against the long-path snapshot, then exercises
reconciliation and rejects a subsequently modified validation snapshot. These
checks exercise repository execution rather than telemetry counters.

The first runner invocation was interrupted during compilation with status
4294967295 and no compiler diagnostic; other shared jobs ended simultaneously.
The retry reached linking but failed with LNK1104 because the shared test
executable was in use by the successful test run. Both attempts consumed build
resources and remain recorded in `conflict-focused.log` and
`conflict-focused-retry.log`; neither is counted as passing validation. The
successful executable was built after the transaction implementation and tests
were last modified and includes both new conflict tests. No production source
repair was needed for those build-environment failures.

Final path review preserves conflict failure signaling, workspace isolation,
current-origin rechecks, and post-integration validation. Successful automatic
merges still discard temporary inputs; retained conflict evidence remains
historical across retries. Scheduling, context projection, progress/convergence
decisions, transport retries, output recovery, and process retirement were not
changed. Their counters and the trace of the completed process do not establish
an additional implementation failure in these runs.

This pass does not rebuild or activate the installed Desktop, synchronize
upstream, or run a paid model benchmark. Mechanism tests can establish that
conflict evidence is available without reconstructing it from the live checkout;
they do not establish a fixed number of future model generations saved.
