# Harness Audit Reference

Use this reference when auditing, scoring, hardening, or optimizing the KD4
harness skill or its durable task artifacts.

## Evidence Pass

Inspect the current skill contract, bundled references, UI metadata, `.codex`
source/state policy, and any targeted run artifact. Check repository policy only
where the harness routes to it. Do not treat ignored logs or old run state as
current policy.

## Categories

- Trigger precision: frontmatter activates for durable workflow work without
  becoming the default implementation skill.
- Reference integrity: every required path exists, every bundled reference is
  directly routed, and deleted templates or docs are not required.
- Context efficiency: instructions use progressive disclosure and do not copy
  root or `.codex` policy.
- Source/state boundary: reviewable skill source stays separate from ignored
  runs, logs, screenshots, caches, and runtime evidence.
- Artifact usability: each artifact has a distinct purpose, minimal fields, and
  a concrete continuation or proof value.
- Validation alignment: recorded checks are the nearest sufficient proof and
  final claims do not exceed them.
- Authority and safety: dirty work, approval, sandbox, execution, publish,
  generated-output, and subagent boundaries remain owned by active policy.
- Runtime clarity: desktop-visible work preserves publish, restart, process,
  and visible-proof state when relevant.

## Findings

Lead with evidence-backed findings ordered by impact. For each finding, name the
affected file or artifact, explain the practical failure mode, and propose the
smallest corrective action. Score categories only when the user asks for a
score.

Create `HARNESS_AUDIT.md` only when the audit must survive the current task. Use
the sections `Scope`, `Findings`, `Top actions`, `Validation`, and `Decision`;
otherwise report the audit directly.
