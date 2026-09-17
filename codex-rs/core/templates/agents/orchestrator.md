# Orchestration instruction sources

This file is a maintainer reference, not a model prompt. Runtime assembly is in
`core/src/context/multi_agent_mode_instructions.rs`; it includes
`core/templates/agents/root_orchestration.md` for root coordination.

Keep rules in their active owning surfaces:

- `protocol/src/prompts/base_instructions/default.md`: scope, change contracts,
  shared workspace safety, validation, and completion for roots and workers.
- `core/src/context/task_model_guidance.rs`: evidence provenance and task-state
  tracking; `core/src/session/turn.rs` selects whether to inject this fragment.
- `code-mode-protocol/src/description/exec_prompt.rs`: JavaScript execution,
  awaited calls, nested-tool discovery, and cell lifecycles.
- `core/src/tools/handlers/shell_spec.rs`: command arguments and session contracts.
- `prompts/templates/review/rubric.md`: review criteria.

Paths above are relative to `codex-rs`. Update an owning runtime source and its
focused rendering or behavior tests when changing a rule; do not add a second
policy copy here. Text in this reference is not delivered to the model.
