---
name: skill-creator
description: Guide for creating effective skills. This skill should be used when users want to create a new skill (or update an existing skill) that extends Codex's capabilities with specialized knowledge, workflows, or tool integrations.
metadata:
  short-description: Create or update a skill
---

# Skill Creator

Create or update task-specific skills containing useful, non-obvious guidance. Assume Codex is capable: keep only information that changes its decisions, avoiding generic explanations and repeated rules.

## Structure and scope

A skill requires `SKILL.md` with YAML `name` and `description`, followed by its instructions. The description supplies the capability and invocation triggers; the body is loaded after selection. Preserve supported optional fields such as `license`, `allowed-tools`, and `metadata`; do not invent unsupported fields.

Use these optional resources only when needed:

- `scripts/`: deterministic or frequently reused executable work.
- `references/`: schemas, detailed procedures, and variant-specific guidance loaded on demand.
- `assets/`: templates, fonts, images, or other output resources, not routine context.
- `agents/openai.yaml`: UI metadata; read [openai_yaml.md](references/openai_yaml.md) before generating or updating it.

Match specificity to fragility: use outcomes and decision criteria when several approaches work, preferred scripts/pseudocode for a usual path, and narrow sequences only when deviation risks correctness. Keep core workflow in the body and substantial examples/variant details in references without duplicating them. Link each reference directly from `SKILL.md` with its read condition; give references over 100 lines a contents list and very large references useful search terms. Keep the body under 500 lines. Do not add unrelated README, installation, quick-reference, or changelog files.

## Creation and update workflow

Follow these steps, skipping those already satisfied or inapplicable to an update.

1. Establish realistic requests and triggers from user examples or examples validated with them. Ask only for unclear usage patterns, with the most important questions first.
2. Identify reusable scripts, references, and assets from those requests. For example, repeated PDF rotation warrants a helper; repeated schema discovery warrants a reference. Avoid unused directories and placeholders.
3. For a new skill, always run the initializer below. Do not reinitialize an existing skill.
4. Implement the resources and concise imperative instructions. Put trigger information in the frontmatter, not a redundant body section. Added scripts must be executed and checked for their intended output; a representative sample suffices for similar scripts.
5. Validate, then iterate using real tasks and demonstrated failures. Use the independent forward-testing procedure below after substantial or particularly tricky changes when available.

### Naming and destination

Use lowercase letters, digits, and hyphens; normalize titles to hyphen-case and keep names under 64 characters. Prefer short verb-led names, adding a tool/domain namespace when it improves discovery. The folder must match the skill name.

Honor the user's destination. Otherwise use `$CODEX_HOME/skills`, or `~/.codex/skills` when unset; ask only if a material ambiguity remains.

### Initialization and metadata

```bash
scripts/init_skill.py <skill-name> --path <output-directory> [--resources scripts,references,assets] [--examples]
```

The initializer creates the directory, frontmatter/body starter, and `agents/openai.yaml`; optional resources/examples require their flags. Replace or delete generated placeholders and unused examples.

Generate `display_name`, `short_description`, and `default_prompt` from the skill and pass them with `--interface key=value`. To regenerate metadata:

```bash
scripts/generate_openai_yaml.py <path/to/skill-folder> --interface key=value
```

Validate that UI metadata still matches on updates. Include other optional interface fields only if the user supplied them. Field definitions and examples live in [openai_yaml.md](references/openai_yaml.md).

### Validation

```bash
scripts/quick_validate.py <path/to/skill-folder>
```

Fix reported frontmatter/naming issues and rerun affected validation. Structural validation does not prove the workflow works; examine outputs from realistic uses and improve only the demonstrated weak points.

## Independent forward-testing

When available, prefer independent testing for substantial or complex skills. Assign a realistic task using `Use $skill-name at /path/to/skill-name to solve problem y`, not a request to pretend to test the skill. Give the skill and minimal raw artifacts; do not leak the intended answer, diagnosis, proposed fix, or prior conclusions. Success should depend on the skill and transferable reasoning.

Use fresh agent context, review actual outputs and artifacts, and prevent earlier test artifacts from contaminating later runs. Rebuild context from source artifacts after revisions. If a test may take substantial time, need further authorization, or affect production, show the proposed task and request approval and suggested changes first. Success that depends on leaked context does not validate the skill.
