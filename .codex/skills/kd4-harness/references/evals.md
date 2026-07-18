# Evals Reference

Use this reference when a harness change needs explicit success criteria,
regression criteria, or repeatable grading.

## When To Create An Eval

Create an eval artifact for:

- changes to harness workflow policy;
- new or changed skills;
- automation scripts or generators;
- desktop-visible workflows;
- bug fixes where the failing behavior can recur.

Skip eval artifacts for tiny documentation edits when `git diff --check` and a
focused read-back are enough.

## Eval Types

Capability eval:
Proves the new behavior can happen.

Regression eval:
Proves an existing workflow still behaves the same.

Release-critical eval:
Requires repeated passing evidence, such as `pass^3`, for safety-sensitive or
desktop-visible paths.

Manual-review eval:
Captures a human decision when deterministic checks cannot judge the outcome.

## Graders

Prefer in this order:

1. Command grader: focused test, build, script check, schema check, or dry-run.
2. Rule grader: exact file, regex, schema, or JSON assertion.
3. Manual grader: named reviewer judgment for ambiguous outcomes.
4. Model grader: open-ended judgment with a written rubric.

## Metrics

- `pass@1`: first attempt succeeds.
- `pass@3`: at least one of three controlled attempts succeeds.
- `pass^3`: three controlled attempts all succeed.

Use `pass^3` only where repeatability matters enough to justify the cost.
