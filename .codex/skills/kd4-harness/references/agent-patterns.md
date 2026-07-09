# Agent Pattern Reference

Use this reference only when the user or active instructions explicitly ask for
subagents, delegation, or parallel agent work.

## KD4 Guardrails

- Default to a single responsible agent.
- Give each subagent a bounded task and expected output.
- Do not allow recursive delegation unless explicitly needed.
- Avoid overlapping edits. One owner integrates final changes.
- Require each agent to report inspected scope and evidence.

## Patterns

Pipeline:
Use when work has clear stages, such as research, implementation, then review.
Each output becomes the next input.

Fanout/fanin:
Use when independent areas can be inspected in parallel. One owner collects
findings and chooses the final change.

Expert pool:
Use when the task spans different surfaces, such as Rust runtime, scripts,
docs, and validation.

Producer/reviewer:
Use when one agent implements and another checks for regressions, missing tests,
or wiring gaps.

Supervisor:
Use when the task has many moving pieces. The supervisor tracks constraints,
integrates outputs, and owns the final evidence.

## Assignment Template

Each assignment should include:

- scope;
- files or directories to inspect;
- expected output;
- validation expectation;
- stop condition.

## Integration Rule

The final owner is responsible for reconciling results against the real
worktree. Subagent conclusions are inputs, not proof by themselves.
