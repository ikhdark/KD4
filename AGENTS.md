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
* Add no new frameworks, redesigns, or cleanup projects unless a confirmed failure requires them. Add acceptance checks only when required to validate the requested behavior or a confirmed failure.

## Validation

* Every test must assert an expected result and fail for a plausible incorrect implementation of the behavior or logic under test.
* Repair weak tests covering the changed behavior or blocking its validation. Report unrelated weaknesses encountered without starting a broader test audit.
* When validation or tests report errors, warnings, or failures, let the current run finish and diagnose all reported issues before making repair edits. Apply related fixes in consolidated batches and rerun affected tests or validation checks that have not yet passed. Repeat only if failures remain or new evidence requires it.
* Do not rerun tests or validation checks that have already passed in your current validation session.