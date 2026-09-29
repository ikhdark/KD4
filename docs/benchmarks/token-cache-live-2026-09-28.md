# Integrated live-model token/cache run — 2026-09-28

## Result: functional pass, efficiency regression

Ran one complete baseline/candidate pair against **gpt-6-astra**, reasoning
effort **high**, using the existing fork login. Both variants produced the exact
expected file, executed the command producer once and MCP tool once, and preserved
the evidence source and unrelated sentinel file. All six candidates were exercised.

**Do not promote this bundle as-is.** It lowered uncached input but increased
output, handoffs, and elapsed time. The live results do not support the original
goal of reducing input, total tokens, and output together without regression.

| Provider usage / observed metric | Baseline | Candidate | Change |
| --- | ---: | ---: | ---: |
| Uncached input tokens | 195,705 | 164,201 | −31,504 / **−16.10%** |
| Cached input tokens | 0 | 29,696 | +29,696 |
| Total input tokens | 195,705 | 193,897 | −1,808 / −0.92% |
| Output tokens | 788 | 1,216 | +428 / **+54.31%** |
| Total tokens | 196,493 | 195,113 | −1,380 / **−0.70%** |
| Model requests | 11 | 14 | +3 |
| Complete-turn elapsed time | 62.855 s | 73.823 s | +10.968 s / **+17.45%** |
| Task success | pass | pass | unchanged |

These are actual `response.completed.response.usage` counters, summed across
every completed request in each turn. Uncached input is input minus cached input.
Missing usage is rejected rather than replaced with tokenizer estimates. All 25
requests in this pair returned complete usage. No monetary price is inferred.

## Where the extra requests came from

The saved candidate trace contains two failed discovery cells followed by a
diagnostic cell before the requested search succeeds:

1. Searched `ALL_TOOL_NAMES` for `tool_search_tool`, then attempted to invoke the
   result of `resolve_tool`: `TypeError: t is not a function`.
2. Tried to invoke `tools[t.name]`: `TypeError: Cannot read properties of undefined
   (reading 'name')`.
3. Listed matching actual names and received `mcp__bench__mirror` and `tool_search`.
4. Successfully invoked `tools.tool_search` and continued.

The baseline used `tools.tool_search` immediately. This is evidence against the
assumption behind **#2** that direct schemas alone replace the eager nested
contracts without model recovery cost. Provider-facing names and JavaScript
callable names are not interchangeable. The bundle also generated a longer final
write/check cell; not all additional output can be attributed to the two errors.
The repeated-search receipt (#5) and MCP mirror projection (#18) were subsequently
consumed successfully in the live turn.

Next experiment should isolate #2 or retain its nested alias/call-shape guidance,
then compare against the other five candidates. That experiment has **not** been
run; these results are not evidence for its outcome.

## Integration boundary and scope

- This is live inference, not replay or a scripted mock model. A freshly built
  core integration binary runs real code-mode dispatch, file operations,
  subprocess/session handling, output recovery, and a local synthetic MCP server.
- The test-only adapter transforms requests **before** forwarding them to the
  actual provider. #15 uses an explicit real command-output budget; #17/#5/#2/
  #10/#18 use the same bounded projections exercised by the offline candidate E2E.
  The prompt explicitly requests tagged result wrappers so the adapter can
  recognize them. This is not a general projection for arbitrary tool output.
- The configured model's live catalog defaults to **code-mode-only**. #2 is not
  applicable there because direct schemas are not duplicated. Both measured
  variants explicitly override the test catalog to **mixed code mode** to test
  all six together. The adapter handles Responses Lite `additional_tools` items
  as well as ordinary top-level tools, and refuses to strip sole nested contracts.
- The live model chooses its code and recovery actions. A ten-checkpoint prompt
  constrains the workload; this is not a broad task-success or autonomy evaluation.
- The task uses synthetic files in temporary workspaces. Authentication stays in
  the runner, is sent only to the built-in HTTPS provider origin, and is excluded
  from saved request headers/logs. Redirects are disabled. No installed Desktop
  binary, login file, or user configuration was replaced or restarted.

## Limits and guardrails

This is one sequential A/B pair, not a randomized or statistically representative
sample. Cache state is provider-controlled; the candidate ran second. The cached
input increase is observed, **not causally established** as an optimization effect.
Temporary paths, generated IDs, watcher timing, model choices, and server load
vary. The elapsed-time comparison includes the adapter's buffered SSE transport
and is not a Desktop UX latency benchmark. Cancellation behavior is not validated.

Each turn is capped at 16 provider requests and ten minutes, with a 90-second
HTTP timeout. Further requests stop after observed cumulative usage reaches
350,000 input or 12,000 output tokens; these thresholds cannot cap an already
in-flight response. Provider errors stop forwarding without automatic upstream
retry. A narrowly scoped nextest override permits the two bounded live turns;
default suite timeouts are unchanged.

## Validation and retained failures

- `live-preflight-5`: offline paired E2E, HTTP runtime bridge, usage parsing,
  missing/invalid-usage rejection, Responses Lite handling, and single-source
  schema preservation passed. Explicit live-opt-in rejection was also checked:
  it fails before creating an output directory.
- `live-3`: **1 test passed**, 121 unselected; 140.779 s test execution. Source
  hashes were unchanged during the run and matched the final inspected sources.
- Ruff checks/formatting, Python compilation, Rust formatting, and scoped diff
  checks passed. No full suite was run.
- `live-1`: local runtime handoff failure; zero generation requests reached the
  provider. Repaired and covered by a local transport-bridge test.
- `live-2`: six genuine requests completed before the mock fixture's 30-second
  event deadline interrupted the baseline. The harness now uses its own bounded
  live event loop. This partial run consumed **76,619 measured tokens**: 76,206
  input (7,936 cached, 68,270 uncached) and 413 output. It is excluded from the
  paired comparison, not hidden as a free retry.
- Initial compilation failures in the new harness were repaired before successful
  validation. An unchanged offline preflight was accidentally repeated after a
  rejected patch; it made no live requests.

Combined recorded usage across the interrupted run and completed pair is
**468,225 tokens**. No repeat was made to seek a more favorable live result.

## Reproduce and inspect

From the checkout root, with the fork's existing `CODEX_HOME` set and a new output
directory (this explicitly authorizes real model usage):

```powershell
python scripts/benchmark_token_cache.py live-e2e --allow-live --output codex-rs/target/token-cache-review/new-live
```

The runner reads the model and reasoning effort from the active fork config;
it does not modify them. Current live auth support is the existing ChatGPT
`auth.json` shape, not every provider or credential store.

Evidence: `codex-rs/target/token-cache-review/live-3/` contains `manifest.json`,
`records.jsonl`, `baseline-attempts.jsonl`, `candidate-attempts.jsonl`, synthetic
request JSONL, and `runner.log`. These are ignored local artifacts. The manifest
is a scoped source-hash check, not a hermetic dependency fingerprint.

Sources: `codex-rs/core/tests/suite/code_mode_token_cache_live.rs`, shared fixture
`code_mode_token_cache_e2e.rs`, and `scripts/benchmark_token_cache.py`.
Production adoption, a #2 ablation, and broader live non-regression remain unperformed.
