# Repository instructions

## Repository identity and runtime boundary

* This is the user's local fork of [`openai/codex`](https://github.com/openai/codex).
  Upstream synchronization or distribution requires a request that explicitly names it.
* This is a local project for the user's own use and is not intended for public release or distribution. Its main goal is to improve and optimize Codex.
* Treat the active repository root as the checkout location; do not hard-code a workstation-specific checkout path.
* `C:\Users\kuh\Desktop\LOCAL-KD` is the fork home and `C:\Users\kuh\.codex` is the official upstream home. The published fork Desktop must use `CODEX_HOME=C:\Users\kuh\Desktop\LOCAL-KD`.
* This repository contains the Rust CLI and app-server, not the native Windows shell. Source changes become Desktop-visible only after rebuilding and replacing or updating the local binary, then restarting Desktop. Perform those activation steps only when the request includes them.

## Scope and workspace

* A no-change result is valid and preferred when the requested capability already exists adequately. Before adding a mechanism, identify the concrete missing capability and explain, using relevant source evidence, why existing abstractions are insufficient. Prefer reuse, consolidation, or deletion over adding parallel machinery.
* When edits overlap, preserve independent changes and combine compatible behavior against the requested contract. Verify the combined runtime path; ask only when conflicting intended behavior cannot be resolved from current evidence.
* Partial wiring of implemented code is forbidden. End-to-end wiring is mandatory.

## Validation

* Use the smallest existing check that proves the changed contract. Inspection or a direct assertion is sufficient when execution is unnecessary. Run a full suite only when explicitly required by the user or repository.
* Every test must assert the expected behavior and fail for a plausible incorrect implementation. Reuse existing coverage; add or repair tests only when a concrete changed-behavior gap prevents sufficient validation.
* Let a valid, progressing validation run finish. Diagnose relevant failures, repair them in one batch, and rerun only affected checks. Report unrelated failures without expanding the task.
* Reuse passed validation until an overlapping source or dependency change makes it stale. Never rerun an unchanged failing check. A freshness notice alone does not require revalidation; if stale evidence is essential, use the cheapest scoped check, otherwise report the claim as unverified.