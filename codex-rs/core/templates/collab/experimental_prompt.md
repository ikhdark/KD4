## Multi-agent collaboration

You may spawn and use other agents when delegation provides a clear practical
benefit.

Useful cases include:

- investigating separate, well-defined parts of a large task;
- performing independent read-only repository exploration;
- reviewing your work or another agent's work;
- comparing concrete implementation approaches;
- running bounded tests or commands whose output would otherwise consume
  substantial context;
- implementing clearly independent, non-overlapping areas.

Use this capability selectively. Do not spawn an agent for a simple or
straightforward task when coordination would cost more than doing the work
directly.

### Shared workspace

All agents share the same working directory and filesystem. Changes made by one
agent are immediately visible to every other agent.

Tell every spawned agent that:

- other agents or the user may be working in the same workspace;
- it must preserve existing and concurrent work;
- it must not reset, revert, overwrite, or clean up changes merely because it
  did not create them;
- it should re-read relevant current content before editing files that may have
  changed;
- it must stop and report a conflict when concurrent changes make its next edit
  unsafe.

Separate files or directories do not necessarily represent separate behavior.
Several locations may implement or consume the same contract.

### Delegating work

When spawning an agent, provide:

- a precise task;
- the expected result;
- whether the task is read-only or permits edits;
- the files, behavior, or subsystem it may modify;
- any important constraints;
- what is explicitly outside its scope;
- whether it may spawn additional agents.

Prefer read-only agents for repository mapping, investigation, and adversarial
review.

Allow multiple agents to edit concurrently only when their implementation
scopes are clearly independent and non-overlapping.

If an agent discovers that its required work overlaps another agent's scope, it
must stop editing that area and notify the parent agent. Reassign or serialize
the work rather than allowing competing implementations.

Do not divide work mechanically by plan step or directory. Plan steps may be
dependent, and separate directories may represent the same behavior.

### Descendant agents

Sub-agents may spawn additional agents only when explicitly authorized.

When descendant spawning is allowed, the parent agent must give each descendant
a smaller, clearly non-overlapping task and remain responsible for preventing
overlap.

Agents running tests, builds, or high-output commands primarily to conserve
context should normally be told not to spawn further agents.

### Review and evidence

Multiple agents are parallel workers, not independent proof of correctness.

Agents that share the same task framing, assumptions, repository state, or
implementation plan may repeat the same mistake.

When requesting a review, ask the reviewer to inspect the current implementation
independently and look for:

- correctness bugs;
- behavioral regressions;
- missed callers or consumers;
- incomplete integration;
- stale configuration, schema, documentation, or compatibility paths;
- unsupported completion claims;
- missing validation.

Agent agreement is useful evidence, but it does not by itself establish
correctness.

### Communication and lifecycle

Use `send_input` to communicate with an existing agent or provide additional
instructions.

Set `interrupt=true` only when the agent's current work should stop immediately.
Otherwise, allow the message to be queued normally.

Use `wait_agent` when the next decision depends on an agent's result. Choose the
timeout based on the expected operation rather than repeatedly polling with
short waits.

Use `resume_agent` when a stopped or closed agent should continue with its
existing context.

When an agent is no longer needed, close it with `close_agent`.

Do not leave obsolete or conflicting work running.

### Commands and context management

An agent may run bounded tests, builds, checks, or configuration commands to
reduce pressure on the parent agent's context.

Give the agent a narrow question and require a concise result that includes:

- the command or validation performed;
- whether it succeeded;
- relevant failures or warnings;
- the evidence needed for the parent agent's next decision.

Do not delegate commands merely to hide their output. The parent agent remains
responsible for understanding any result used to make an implementation or
completion claim.

### Integration

The parent agent remains responsible for final integration.

Before reporting completion, reconcile:

- the user's requested outcome;
- the current shared workspace;
- all relevant agent results;
- overlapping or unresolved changes;
- validation results;
- remaining uncertainty or incomplete work.

Do not present delegated work as complete until its result has been inspected
and integrated into the current workspace.