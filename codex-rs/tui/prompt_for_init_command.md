Generate `AGENTS.md` in the current working directory as a repository-specific contributor guide.

First check whether that file exists. If it does, do not overwrite or modify it: report that initialization was skipped and stop before inspecting the repository.

Otherwise, start with supplied context, root documentation, and manifests. Inspect additional directories, scripts, test/lint configuration, CI, nested AGENTS files, or representative recent commits only to establish a missing or conflicting rule. Do not enumerate the entire repository or install dependencies/run builds, tests, or linters solely to validate this guide.

Title the document `Repository Guidelines`. Use descriptive Markdown headings and concise, professional instructions, normally 200–400 words; use more only when the workspace needs it. Include verified commands, paths, and useful examples without copying large documentation sections.

Choose supported sections only:
- Project structure: major areas and workspace boundaries, not every package.
- Build/test/development: important commands, their purpose, and working directory.
- Style/naming: configured formatters/linters and established conventions.
- Testing: frameworks, locations, naming, and commands; coverage requirements only with explicit policy.
- Commits/PRs: recurring history conventions and documented expectations; never infer a rule from one or two unusual commits.
- Other useful repository-specific architecture, security, configuration, generated-file, release, or agent guidance.

Do not invent commands, layouts, conventions, or policies; omit unsupported guidance and generic advice. Do not duplicate or contradict more precise nested AGENTS instructions.

Before finishing, review the guide against the evidence already inspected: commands and paths must be real, tools/conventions supported, nested instructions respected, and the result concise and actionable. Reuse unchanged evidence.
