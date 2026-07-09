# Harness Checklist

## Intake

- [ ] User objective is concrete enough to act on
- [ ] Task lane chosen from root `AGENTS.md`
- [ ] Wiring Guard/KDWG-triggered implementation work treated as harnessed when
      applicable
- [ ] Nearest scoped `AGENTS.md` inspected when editing files
- [ ] Existing dirty-worktree changes identified and preserved

## Implementation

- [ ] `kd4-crosscheck-and-finish` applied for implementation or repo-behavior
      work
- [ ] Wiring Guard/KDWG intent declared before implementation-shaped edits when
      active
- [ ] Owner files inspected
- [ ] Relevant call path inspected
- [ ] Relevant config, schema, or generated artifact ownership checked
- [ ] Nearest tests or validation route identified
- [ ] Manual edits made with `apply_patch`
- [ ] No unrelated cleanup mixed into the patch

## Safety And Runtime

- [ ] Approval, sandbox, test-gating, patch, and execution-safety behavior left
      untouched unless explicitly in scope
- [ ] Public CLI flags, app-server APIs, config loading, protocol contracts, and
      stored session behavior considered when touched
- [ ] Desktop-visible changes include publish and restart proof, or the final
      answer says that work remains

## Validation

- [ ] Focused validation ran
- [ ] Wiring Guard/KDWG check ran when active and applicable, or
      `--no-wiring-targets` was explicitly justified
- [ ] Validation output reviewed
- [ ] Skipped checks are named with reasons
- [ ] Claims in the final answer match the evidence

## Finish

- [ ] Important changed files named
- [ ] Remaining risk stated
- [ ] No commit, push, or publish performed unless the user asked for it
