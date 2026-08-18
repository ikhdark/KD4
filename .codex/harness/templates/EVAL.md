# Eval

## Metadata

- Eval id:
- Task:
- Owner:
- Type: capability | regression | release-critical | manual-review
- Baseline:
- Date:

## Success Criteria

- [ ] <criterion>

## Grader

Choose the strongest practical grader:

- Command grader: deterministic command or test.
- Rule grader: regex, schema, or file-structure assertion.
- Manual grader: human judgment for ambiguous UX, security, or product calls.
- Model grader: only for open-ended outputs where deterministic checks are not
  enough.

## Capability Checks

| Check | Method | Expected | Result |
| --- | --- | --- | --- |
| <check> | <command/rule/manual/model> | <expected> | <pending> |

## Regression Checks

| Check | Baseline | Result |
| --- | --- | --- |
| <check> | <baseline> | <pending> |

## Benchmark Contract (Delete Unless Active)

- Exercised hot path:
- Quality contract and checks:
- Command and workload:
- Latency metric and threshold:
- Token metric and budget:
- Environment/build equivalence:

| Build or revision | Quality gate | Samples | Latency result | Token result | Contract status |
| --- | --- | ---: | ---: | ---: | --- |
| <baseline or established contract> |  |  |  |  | <pending> |
| <candidate> |  |  |  |  | <pending> |

Do not rank a candidate on latency or tokens unless its quality gate passes. On
a latency or token miss, record the measured failure and owner-level bottleneck
hypothesis. After a relevant implementation change, rerun affected quality
checks before rerunning these unchanged performance contracts.

- Selected quality-preserving implementation:
- Selection evidence versus the baseline or best prior candidate:
- Threshold still missed, if any:

## Run Log

| Attempt | Revision or fingerprint | Evidence | Provenance | Covered contract | Result |
| --- | --- | --- | --- | --- | --- |
| 1 | <revision> | <command or artifact> | <provenance kind> | <exact scope> | <pending> |

## Summary

- Required checks passed:
- Required checks failed:
- Skipped checks and reasons:
- Repeated-trial result (only when identical independent trials were run):
- Status: ready | needs-work | blocked
- Remaining risk:
