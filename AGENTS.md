# Repository guidance

Treat harness prompts and instruction architecture as critical components: apply these rules strictly and review changes carefully for unintended behavior. Diagnose whether an issue requires a prompt fix, a harness implementation fix, or both; do not use prompt changes to mask implementation defects. Keep harness prompts aligned with your changes, following [the instruction architecture](codex-rs/core/instruction-architecture.md) for ownership, delivery, and maintenance. Replace outdated guidance; add instructions, prompt sections, or guidance files only when needed to improve the agent's understanding. Keep each instruction in its authoritative owner and remove duplication. Choose the best-supported version of the prompt, not automatically your own, and avoid changes made solely to produce a diff. Keep `codex-rs/protocol/src/prompts/base_instructions/default.md` concise and repository-neutral rather than copying other prompts into it.

This is Windows only project.
This is a local project.
Do not publish or republish without permission.

Upstream is https://github.com/openai/codex

## Session analysis

- Merge overlapping edits or use the most optimal implementation.
- Fork sessions: `C:\Users\kuh\Desktop\LOCAL-KD\sessions`.
- Use KDR (`C:\Users\kuh\Desktop\KDR\kdr.cmd`) for benchmarks; keep the same `--config` across capture, comparison and history commands so they share one journal.
- Before claiming an optimization win, consult KDR's `benchmark-history` and run `tools diagnose compare-receipts --baseline BEFORE.json --candidate AFTER.json` on the retained matched receipts; select `--candidate-variant candidate` when the candidate arm is inside a paired receipt. Comparisons automatically retain and consult shared history. Preserve its regression vetoes; missing evidence or noise is not a win. Use `--no-history` only for explicitly read-only work, never to bypass a historical veto. See `C:\Users\kuh\Desktop\KDR\docs\DIAGNOSTICS.md` for context evidence and the shared journal contract.
- Prefer the most recent relevant sessions for diagnosis over older sessions.
- Before behavioral edits, surface a prior benchmark/defect receipt for the affected code, or explicitly record that none is registered. Run KDR's historical-reversal --repo REPO --patch PROPOSAL before applying a saved proposal; inspect receipt links, inverse-change matches across commit hashes, and skipped coverage. Maintain checksum-bound links in REPO/.codex/fix-evidence.json (format/limits in KDR's README). Advisories are not permission or correctness verdicts.
- If selection, instrumentation or correctness
  evidence is missing, extend the existing harness or KDR before benchmarking. 
- Do not use reasoning level/effort/priority to explain or justify metric results.
- If KDR is missing what you need to prove your benchmarks, add the instruments to KDR and run your benchmarks.
- Put SHORT summaries of your audits/benchmarks in "C:\Users\kuh\Desktop\kd4\.codex\audits" in a folder with a name related to that specfic task.

- Keep your verbose level low and straight to the point. Instead of pointless progress updates, spend that time finishing the task.
- Fix all verified issues within the authorized scope, including review and audit findings; do not stop at reporting them unless explicitly restricted to read-only/report-only work. Treat harness issues as fix requests by default.

Implementation requirements: Implementations must demonstrate their intended benefit with evidence proportional to the change while preserving correctness and avoiding regressions. Performance optimizations require narrow KDR baseline-versus-candidate benchmarks under matched workloads and configurations, retaining workload, metric, repetitions, and measured delta in benchmark evidence. Correctness and reliability fixes require evidence that the targeted failure is addressed, using reproduction, causal analysis demonstrating that the failure mechanism is eliminated, or other appropriate verification. Performance regression checks are required only when relevant to the affected path. A small, explicitly justified performance or resource cost may be acceptable only when it is necessary to achieve a material correctness or reliability improvement.

Regression definition: A regression is any introduced failure, loss of existing functionality, reduced reliability, new workflow blocker, or repeatable degradation in relevant performance or resource usage beyond measurement noise attributable to the implementation. Regressions cannot be justified by improvements elsewhere and must be resolved before acceptance, except for a small cost explicitly accepted under the rule above for material correctness or reliability improvements. Do not accept any implementation attempts that regress correctness or wall-clock/latency. Rollback your regressed implemenation.

Unverified risks: Investigate suspected regressions only when supported by a concrete, plausible causal mechanism. Unverified risks block acceptance only when they could materially affect correctness, reliability, workflow, or relevant performance. Resolve these through targeted verification or revision. Unsupported speculation must not block acceptance, and unresolved risks must not block unrelated work. Lack of evidence is not proof of safety.

Run benchmark verification using dev-profile binaries rather than release-profile binaries.

Reporting: Keep agent output brief. Report correctness and performance outcomes separately. Summarize the key results, including any material accepted cost or remaining relevant limitation, and avoid dumping detailed evidence already available in tool output. Never present passing correctness checks as proof of performance improvement. Documentation-only changes are exempt.