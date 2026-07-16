# Evals Reference

Use this reference when harness behavior needs explicit capability or
regression criteria that must survive the current turn.

## Decide Whether To Persist An Eval

Create `EVAL.md` when the user requests an eval, the workflow change is complex
or repeatable, a known failure can recur, or later work needs durable acceptance
criteria. Skip the artifact when deterministic validation and a focused
read-back are sufficient.

For a skill change, always run structural validation. Forward-test only when
active instructions permit subagents and realistic behavior cannot be covered
deterministically.

## Eval Shape

Record:

- capability or regression objective;
- baseline or failure being protected;
- exact success criteria;
- grader and expected result;
- attempt evidence and result;
- remaining risk.

## Grader Order

Prefer the strongest practical grader in this order:

1. Command grader: focused validator, test, build, schema check, or dry-run.
2. Rule grader: exact path, link, regex, schema, or structured-data assertion.
3. Manual grader: named human judgment for ambiguous behavior.
4. Model grader: open-ended judgment with a written rubric when deterministic
   checks cannot decide.

Use `pass@1` for a first-attempt capability result, `pass@3` for at least one of
three controlled successes, and `pass^3` only when three consecutive successes
are worth the cost. Do not inflate repeat counts for documentation-only changes.
