# Agent Pattern Reference

Use this reference only when the user or active instructions explicitly ask for
subagents, delegation, or parallel agent work.

## KD4 Guardrails

- Default to a single responsible agent.
- Give each subagent a bounded task and expected output.
- Do not allow recursive delegation unless explicitly needed.
- Reload shared tracker or run state before assignment and integration.
- Avoid overlapping edits. One owner integrates final changes and updates
  shared state.
- Require each agent to report inspected scope and evidence.

## Overlap Gate

Before parallel work, compare owner paths, shared types or protocols,
configuration, generated artifacts, ordering dependencies, and validation
resources. Different filenames do not prove independence.

- Parallelize work only when no shared write owner or unfinished dependency was
  found.
- Coordinate work with an explicit owner split and integration order when a
  shared contract or validation resource remains.
- Keep work blocked when it depends on an unfinished or competing owner.
- Inspect further instead of guessing when evidence is insufficient.

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
- shared-state or overlap constraints;
- stop condition.

## Integration Rule

The final owner is responsible for reconciling results against the real
worktree. Subagent conclusions are inputs, not proof by themselves.
