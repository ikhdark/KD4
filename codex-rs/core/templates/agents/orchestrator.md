# Orchestration instruction sources

This file is a maintainer reference, not a model prompt.

Keep rules in their active owning surfaces:

- `core/src/config/mod.rs`: the root, subagent and shared multi-agent usage hints.
- `core/src/stable_context.rs`: hint wrapping and the root classifier.
- `core/src/tools/handlers/multi_agents_spec.rs`: agent tool contracts.
- `core/src/context/multi_agent_mode_instructions.rs`: spawn authorization.

- `protocol/src/prompts/base_instructions/default.md`: scope, change contracts,
  shared workspace safety, validation, and completion for roots and workers.
- `code-mode-protocol/src/description/exec_prompt.rs`: JavaScript execution,
  awaited calls, nested-tool discovery, and cell lifecycles.
- `core/src/tools/handlers/shell_spec.rs`: command arguments and session contracts.
- `prompts/templates/review/rubric.md`: review criteria.

Paths above are relative to `codex-rs`. Update an owning runtime source and its
focused rendering or behavior tests when changing a rule; do not add a second
policy copy here. Text in this reference is not delivered to the model.
