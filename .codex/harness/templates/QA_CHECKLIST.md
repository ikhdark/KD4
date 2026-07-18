# QA Checklist

## Scope

- Reviewed files:
- Reviewed call paths:
- Reviewed tests or validation routes:

## Correctness

- [ ] User objective is satisfied
- [ ] Behavior is wired through the intended runtime path
- [ ] Stale or parallel code paths do not override the change
- [ ] Edge cases from the inspected owner area are covered

## Contracts

- [ ] Config schema ownership checked when config changed
- [ ] App-server or protocol schema ownership checked when contracts changed
- [ ] Generated artifacts updated only through owning workflow
- [ ] Public CLI or API behavior called out when changed

## Validation

- [ ] Focused checks ran
- [ ] Check results support the final claim
- [ ] Failures are explained and scoped

## Implementation Completion Gate

Use the authoritative status definitions in
[`../workflow.md`](../workflow.md#incomplete-implementation-finish-gate).

- [ ] Intended runtime path identified:
- [ ] Changed code is reached from that path:
- [ ] No new or task-relevant placeholder/stub markers in changed code or the
      intended runtime path, including `TODO`, `FIXME`, `todo!()`,
      `unimplemented!()`, `stub`, `temporary`, `fake`, `mock-only`, and panic
      placeholders
- [ ] New public functions, types, config fields, commands, or workflow entries
      are wired into expected callers
- [ ] Nearest sufficient validation ran, or skip/not-applicable reason recorded
- [ ] Completion gate status recorded: passed | partial | blocked
- [ ] Final implementation answer includes completion gate status, wiring proof,
      validation run, and remaining unverified risk

## Desktop Visibility

- [ ] Publish required:
- [ ] Restart required:
- [ ] Process path/hash checked:
- [ ] Visible runtime evidence captured:

## Findings

- <finding>
