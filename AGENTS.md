# Repository guidance

Treat harness prompts and instruction architecture as critical components: apply these rules strictly and review changes carefully for unintended behavior. Diagnose whether an issue requires a prompt fix, a harness implementation fix, or both; do not use prompt changes to mask implementation defects. Keep harness prompts aligned with your changes, following [the instruction architecture](codex-rs/core/instruction-architecture.md) for ownership, delivery, and maintenance. Replace outdated guidance; add instructions, prompt sections, or guidance files only when needed to improve the agent's understanding. Keep each instruction in its authoritative owner and remove duplication. Choose the best-supported version of the prompt, not automatically your own, and avoid changes made solely to produce a diff. Keep `codex-rs/protocol/src/prompts/base_instructions/default.md` concise and repository-neutral rather than copying other prompts into it.

This is Windows only project.
This is a local project.
Do not publish or republish without permission.

Upstream is https://github.com/openai/codex

## Session analysis

- Merge overlapping edits or use the most optimal implementation.
- Fork sessions: `C:\Users\kuh\Desktop\LOCAL-KD\sessions`.
- Use KDR (`C:\Users\kuh\Desktop\KDR\kdr.cmd`) for benchmarks.
- Prefer the most recent relevant sessions for diagnosis over older sessions.
- If selection, instrumentation or correctness
  evidence is missing, extend the existing harness or KDR before benchmarking. 
- Do not use reasoning level/effort/priority to explain or justify metric results.
- If KDR is missing what you need to prove your benchmarks, add the instruments to KDR and run your benchmarks.
- Put SHORT summaries of your audits/benchmarks in "C:\Users\kuh\Desktop\kd4\.codex\audits" in a folder with a name related to that specfic task.
- Implementations require a narrow benchmark proving they are improvements. If benchmarking is not what the implementation needs, show that the failure was reproduced and fixed instead. Documentation changes do not require either.
- Keep your verbose level low and straight to the point. Instead of pointless progress updates, spend that time finishing the task.
- Fix all verified issues within the authorized scope, including review and audit findings; do not stop at reporting them unless explicitly restricted to read-only/report-only work. Treat harness issues as fix requests by default.