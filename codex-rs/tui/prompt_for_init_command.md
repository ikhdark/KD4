Generate a file named `AGENTS.md` in the current working directory that serves
as a contributor guide for this repository.

Before writing:

1. Check whether `AGENTS.md` already exists in the current working directory.
   - If it exists, do not overwrite or modify it.
   - Report that initialization was skipped because the file already exists.

2. Inspect the repository to gather project-specific evidence. Check relevant
   sources such as:
   - top-level directories and important subdirectories;
   - package manifests and workspace definitions;
   - build scripts, task runners, and development commands;
   - test directories and test configuration;
   - formatter, linter, and static-analysis configuration;
   - `README`, contribution, and architecture documentation;
   - CI workflow files;
   - existing nested `AGENTS.md` files;
   - a representative sample of recent Git commits when determining commit
     conventions.

Do not invent commands, directory layouts, naming rules, coverage requirements,
pull request policies, or other conventions.

Include a rule only when it is supported by repository files, configuration,
documentation, or a clear recurring pattern in project history. When reliable
project-specific guidance cannot be established, omit that guidance rather than
adding generic advice.

Your goal is to create a clear, concise, repository-specific contributor guide
with descriptive headings and actionable explanations.

## Document requirements

- Title the document `Repository Guidelines`.
- Use Markdown headings for structure.
- Keep the document concise; approximately 200–400 words is preferred for a
  typical repository, but use slightly more when necessary to describe a large
  workspace accurately.
- Keep explanations short, direct, and specific to this repository.
- Include concrete examples where useful, such as verified commands, paths, or
  naming patterns.
- Maintain a professional, instructional tone.
- Avoid copying large sections of existing documentation.
- Do not repeat instructions already covered more precisely by a nested
  `AGENTS.md`.
- Do not contradict more deeply scoped repository instructions.

## Suggested sections

Adapt the structure to the repository. Add sections that are useful and omit
sections that are unsupported or irrelevant.

### Project Structure & Module Organization

Describe the major repository areas and where source code, tests, assets,
generated files, and important configuration live.

For a monorepo, explain the main workspace boundaries without listing every
package.

### Build, Test, and Development Commands

List the most important verified commands for building, testing, linting,
formatting, and running the project locally.

Briefly explain what each command does and, when relevant, where it should be
run.

Do not include commands merely because they are common for the language or
framework.

### Coding Style & Naming Conventions

Summarize conventions established by the existing code and configuration,
including:

- indentation and formatting;
- language-specific style;
- file, module, type, function, and test naming;
- configured formatters, linters, or static-analysis tools.

Avoid describing subjective preferences that are not established by the
repository.

### Testing Guidelines

Identify the test frameworks, test locations, naming patterns, and the verified
commands used to run relevant tests.

Include coverage requirements only when an explicit repository policy or
configuration establishes them.

### Commit & Pull Request Guidelines

Summarize clear recurring commit-message conventions found across a
representative sample of recent history.

Do not infer a convention from one or two unusual commits.

Describe pull request expectations only when they are established by repository
documentation, templates, workflows, or consistent project practice.

### Additional Guidance

Add other short sections only when supported and useful, such as:

- Security & Configuration;
- Architecture Overview;
- Generated Files;
- Release Process;
- Agent-Specific Instructions.

## Final verification

After writing `AGENTS.md`, review it against the repository evidence you
inspected.

Confirm that:

- every command is present in or supported by the repository;
- referenced paths exist;
- stated tools and conventions are actually configured or consistently used;
- no unsupported requirements were invented;
- the guide does not conflict with nested `AGENTS.md` files;
- the document remains concise and actionable.