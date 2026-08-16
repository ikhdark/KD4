# Investigation evaluation corpus

This directory owns the frozen, local-only evaluation corpus used to measure
investigation behavior. Its manifest, validators, scorer, and fixtures own the
current local evaluation contract.
The corpus measures model behavior; it does not grant completion authority and
it does not implement investigation checkpoints, hypotheses, stopping, or
refutation.

## Frozen fixtures

Each fixture patch adds isolated files under `investigation_cases/` to an empty
Git index. Corpus validation rejects modifications to existing files, so the
fixtures do not depend on any repository commit remaining reachable after a
history rewrite.

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
- `patches/` contains self-contained new-file fixture patches.
- `validate_cases.py` validates manifest shape, category coverage, referenced
  files, fixture isolation, and patch applicability against an empty index.
- `score_results.py` validates locally recorded results and reports aggregate
  metrics.

The seeded fixtures are intentionally isolated under `investigation_cases/` in
disposable repositories. They do not modify production code and are never
applied to the working checkout.

## Validate the corpus

From the KD4 repository root:

```text
python scripts/investigation_eval/validate_cases.py
```

Use `--show-fingerprints` to print the exact fingerprint that each result
wrapper must record. The fingerprint binds the manifest record, prompt, and
patch bytes.

## Record a run

For each case:

1. Create an empty disposable directory and run `git init` in it.
2. Apply the referenced patch from the KD4 checkout when one is present.
3. Read the referenced prompt verbatim.
4. Run the recorded KD4 binary and fixed settings:

   ```text
   codex exec --ephemeral --ignore-user-config --model gpt-5.6-sol \
     -c model_reasoning_effort="max" --sandbox read-only --json \
     -C <disposable-repository> <prompt>
   ```

5. Preserve the complete JSONL event stream without alteration in the result's
   `raw_events` array. The ignored local result may contain machine-specific
   paths; do not commit it.
6. Classify the final answer into the stable corpus finding kinds without
   changing its meaning.
7. Save the wrapper as
   `.codex/evals/investigation/<run-name>/<case-id>.json`.
8. Remove the disposable repository.

The result wrapper is:

```json
{
  "case_id": "stable-case-id",
  "case_fingerprint": "lowercase SHA-256 from validate_cases.py --show-fingerprints",
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
  "raw_events": [
    {
      "type": "item.completed",
      "item": {
        "id": "item_0",
        "type": "agent_message",
        "text": "verbatim final model response"
      }
    }
  ]
}
```

`reported_findings` is a human classification of the preserved final output,
not a replacement for it. A finding may be mapped to an expected stable kind
only when the final output identifies the same violated invariant and causal
path. Every structured locator must occur in the final output, and
`final_output` must exactly equal the last completed agent-message event.
Uncertain language must remain `uncertain` or `deferred`.

The scorer derives tool-call and repeated-equivalent-action counts from the
preserved event stream. Do not add hand-entered metric totals: semantic
premature-completion judgements and monetary costs are not scored without an
independently generated evidence format.

## Score a run

```text
python scripts/investigation_eval/score_results.py \
  --results .codex/evals/investigation/baseline \
  --binary <exact-codex-executable-used-for-the-run>
```

The scorer hashes `--binary` itself and requires every result wrapper to match
that digest. A wrapper-provided digest alone is never accepted as provenance.

The scorer reports confirmed-finding recall, precision, clean-control false
positives, deferred or uncertain findings, and event-derived tool calls and
repeated equivalent actions.
