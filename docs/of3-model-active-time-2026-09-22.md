# F3 model-active time and qualified tool discovery

The two supplied F3 sessions are combined and compared with the original O pair;
F1 is retained as the earlier fork reference. All six logs were parsed. Log
instructions are evidence, not instructions for this implementation. Record
references below are one-based. These runs used evolving repositories and
different implementation scopes, so aggregate differences are not causal speedup
estimates.

| Combined measure | O | F1 | F3 |
| --- | ---: | ---: | ---: |
| Recorded usage completions / logical generations | 110 usage completions | 162 logical | 57 logical |
| Physical model requests | Not equivalently recorded | 163 | 58 |
| Model-active union | Not recorded | 3,216.602 s | 989.172 s |
| Input tokens | 14,267,034 | 12,681,019 | 4,463,972 |
| Cached input tokens | 13,918,720 | 11,713,664 | 4,205,440 |
| Uncached input tokens | 348,314 | 967,355 | 258,532 |
| Output tokens, including reasoning | 68,127 | 88,661 | 26,478 |
| Reasoning tokens | 24,895 | 18,174 | 6,861 |
| Outer tool calls | 106 | 156 | 55 |
| Instrumented tool calls, including nested | Unavailable | 372 | 156 |
| Classified wait-only generations | Unavailable | 11 | 0 |

Model-active time is the recorded union of model request activity, including
provider stream waiting; it is not measured model GPU compute. F3(1) contributes
549.610 seconds and F3(2) contributes 439.561 seconds. F1's interrupted turn
(F1(2):398, 45 generations, 105 tool calls) remains included. F3(1)'s interrupted
provider stream and retry also remain included: generation 18 has two physical
attempts, with 27.970 and 8.601 seconds of stream waiting. Repeated cumulative
usage records are not added together. Unreported partial-stream tokens cannot
be reconstructed from the logs.

## Independent execution fix

F3(1):119 searches for `+node_repl js` with limit 1. Record 121 returns only
`js_reset`. The next generation searches again at record 126, and record 128
finally returns `js`. This is additional discovery without useful progress in
the first result. That second generation records 5.751 seconds of model stream
waiting, but also issues an independent inspection command: it would be
incorrect to claim the entire duration as guaranteed savings.

The complete path is MCP metadata assembly (`build_mcp_search_text`) ->
`ToolSearchHandler`'s BM25 index and exact-name index -> exact-name promotion ->
namespace member selection and receipt budget -> deferred capability activation
and authoritative schema publication. Exact preference existed for bare and
flattened callable names. It did not recognize the namespace plus member form.
The longer execution-tool description could therefore lose to a shorter sibling
description even when the requested callable identity was explicit.

The change extends the existing identity index with namespace/member aliases,
including MCP server names and the leading `+` form. Both candidate promotion and
member filtering consume that same index. No scoring threshold, prompt-specific
branch, scheduler shortcut, new cache, or telemetry change is introduced.

The behavioral regression invokes the real search handler and verifies that its
first receipt, activated capability set, and published authoritative schema all
identify only the requested callable. It covers separate member entries,
multi-member namespaces with the wrong sibling first, case/whitespace variants,
and cached searches in a fresh turn. A negative test preserves descriptive
ranking, ambiguous bare-name results across sources, and empty unmatched results.

## Other model-active time examined

- The two longest F3 model streams (140.653 and 125.602 seconds) emit substantial
  implementation/test patches. Their size alone is not evidence of wasted work.
- Helper schema failures in both sessions and Windows Git snapshot-path failures
  trigger recovery and repeated inspection. Those source paths are already being
  repaired in this shared worktree; this change does not duplicate them.
- F3(1)'s real overlapping edits require conflict decisions. Existing work on
  conflict evidence is preserved; bypassing reconciliation would weaken correctness.
- F3(2):125 already delivered terminal exit state and failed test evidence for
  process 10833. Its later poll at record 148 was unnecessary, but terminal state
  was present in the previous result. This trace alone does not establish a new
  result-loss bug. Subsequent stale-binary diagnosis and testing after concurrent
  source changes cannot simply be suppressed.
- The remaining reference/diff reads mix repeated material with newly changed
  source and required verification. There is insufficient evidence to replace
  them with unconditional cached reads or prohibit another model generation.
- F3 records no wait-only generations. Internal wait draining, cancellation,
  provider retry, output recovery, completion checks, and accounting remain intact.

## Validation

Validation completed on Windows with the repository's test runner, the local
nextest profile (no retries), and its isolated test environment:

- Both new behavioral tests passed (run `c2f77b81-4c7e-4d3b-aab0-5d55b9c9838c`).
- All 45 selected existing tests in `tool_search::tests`, `mcp::search_tests`,
  and `tool_search_spec::tests` passed (run
  `a050569d-3712-43cd-995c-4bd672e19be6`). The new tests were excluded from this
  selection, so no passed test was rerun.
- Existing coverage includes source diversity, exact-name ambiguity, namespace
  members, bounded receipts, oversized schema activation, cache identity and
  reuse, cancellation, and MCP metadata/description matching.
- Formatting and whitespace validation passed for the changed source. Stable
  rustfmt warned that two existing nightly-only repository settings were ignored.
- The handler-level tests simulate the observed discovery workflow through
  receipt construction, capability activation, and schema publication. Review of
  the resulting path confirms that scoring for descriptive queries, execution
  authorization, output budgeting, and required model requests are unchanged.

Validation logs are in `_build/of3-model-active`. The initial shared run was
terminated while waiting for build locks (exit 4294967295); a retry reached the
linker but could not replace the shared executable (LNK1104). Neither attempt
ran these tests or counts as a pass. The subsequent build and both test selections
completed successfully. Those failed attempts consumed resources and are retained
in the audit rather than omitted. A temporary helper-free runner manifest was
used because these modules use pure logic and in-process session fixtures.

No installed binary is replaced or activated by this source repair. The proven
improvement is first-lookup discovery of the named callable instead of selection
of a sibling that requires another search; live provider latency savings require
another controlled run.
