# Repository instructions

## Repository identity and runtime boundary

* This is the user's local fork of [`openai/codex`](https://github.com/openai/codex).
  Upstream synchronization or distribution requires a request that explicitly names it.
* This is a local project for the user's own use and is not intended for public release or distribution. Its main goal is to improve and optimize Codex.
* Treat the active repository root as the checkout location; do not hard-code a workstation-specific checkout path.
* `C:\Users\kuh\Desktop\LOCAL-KD` is the fork home and `C:\Users\kuh\.codex` is the official upstream home. The published fork Desktop must use `CODEX_HOME=C:\Users\kuh\Desktop\LOCAL-KD`.
* This repository contains the Rust CLI and app-server, not the native Windows shell.

## Scope and workspace

* Prefer existing owners and abstractions; identify a verified missing capability before adding machinery. Preserve overlapping user work and verify the combined runtime path end to end.

## Session logs

* Analyze rollout JSONL (`LOCAL-KD\sessions`) in a script: count and hash bulk records (tool manifests, inventories, per-call timing arrays), compare timing aggregates, and emit only distinct, decision-relevant text. Complete scripted coverage satisfies a request to read logs fully; do not page every record into context.
