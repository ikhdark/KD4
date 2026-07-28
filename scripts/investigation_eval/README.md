# Investigation evaluation corpus

This directory owns the frozen, local-only evaluation corpus used to measure
the investigation evidence work described in
`Investigation_Harness_Rewritten.md`. The corpus measures model behavior; it
does not grant completion authority and it does not implement investigation
checkpoints, hypotheses, stopping, or refutation.

## Frozen baseline

The first corpus is frozen at KD4 commit
`d5d9f02dbcf010f9b6247aeae4247b614b8527c0`.

The before-change baseline uses:

- KD4 binary version: `codex-cli 0.0.0`;
- model: `gpt-5.6-sol`;
- reasoning effort: `max`;
- sandbox: `read-only`;
- session persistence: `--ephemeral`;
- user configuration: ignored after authentication with
  `--ignore-user-config`.

Each result records the SHA-256 of the exact KD4 binary used and the fixed
execution settings. Absolute paths, credentials, raw usage identifiers, and
machine-specific model output are local-only and must not be committed.

## Corpus layout

- `cases.jsonl` is the frozen manifest.
- `prompts/` contains one repository-relative audit prompt per case.
- `patches/` contains small reversible fixture patches applied to the recorded
  base commit.
- `validate_cases.py` validates manifest shape, category coverage, referenced
  files, base commits, and patch applicability.
- `score_results.py` validates locally recorded results and reports aggregate
  metrics.

The seeded fixtures are intentionally isolated under `investigation_cases/` in
disposable worktrees. They do not modify production code and are never applied
to the working checkout.

## Validate the corpus

From the KD4 repository root:

```text
python scripts/investigation_eval/validate_cases.py
```

## Record a run

For each case:

1. Create a disposable Git worktree at the case's `base_commit`.
2. Apply the referenced patch when one is present.
3. Read the referenced prompt verbatim.
4. Run the recorded KD4 binary and fixed settings:

   ```text
   codex exec --ephemeral --ignore-user-config --model gpt-5.6-sol \
     -c model_reasoning_effort="max" --sandbox read-only --json \
     -C <disposable-worktree> <prompt>
   ```

5. Preserve the complete JSONL event stream without alteration in the result's
   `raw_events` array. The ignored local result may contain machine-specific
   paths; do not commit it.
6. Classify the final answer into the stable corpus finding kinds without
   changing its meaning.
7. Save the wrapper as
   `.codex/evals/investigation/<run-name>/<case-id>.json`.
8. Remove the disposable worktree.

The result wrapper is:

```json
{
  "case_id": "stable-case-id",
  "base_commit": "full commit sha",
  "completed_at": "RFC 3339 timestamp",
  "model": {
    "name": "gpt-5.6-sol",
    "reasoning_effort": "max",
    "codex_version": "codex-cli 0.0.0",
    "binary_sha256": "lowercase SHA-256"
  },
  "execution": {
    "sandbox": "read-only",
    "session_persistence": "ephemeral",
    "user_configuration": "ignored"
  },
  "final_output": "verbatim final model response",
  "reported_findings": [
    {
      "kind": "stable finding category",
      "status": "confirmed | deferred | uncertain",
      "locators": ["repository-relative path or symbol"]
    }
  ],
  "metrics": {
    "tool_calls": 0,
    "repeated_equivalent_actions": 0,
    "premature_completion": false,
    "model_cost": null,
    "tool_cost": null
  },
  "raw_events": []
}
```

`reported_findings` is a human classification of the preserved final output,
not a replacement for it. A finding may be mapped to an expected stable kind
only when the final output identifies the same violated invariant and causal
path. Uncertain language must remain `uncertain` or `deferred`.

## Score a run

```text
python scripts/investigation_eval/score_results.py \
  --results .codex/evals/investigation/baseline
```

The scorer defaults to the frozen baseline binary hash. For an after-change run,
pass the SHA-256 of the exact resulting binary recorded in every wrapper:

```text
python scripts/investigation_eval/score_results.py \
  --results .codex/evals/investigation/after \
  --binary-sha256 <resulting-binary-sha256>
```

The scorer reports confirmed-finding recall, precision, clean-control false
positives, deferred or uncertain findings, tool calls, repeated equivalent
actions, premature completion, and model/tool cost when present. Missing costs
remain unavailable rather than being treated as zero.
