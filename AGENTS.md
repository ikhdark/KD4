# Repository instructions

## Repository identity and runtime boundary

* This is the user's local fork of [`openai/codex`](https://github.com/openai/codex).
  Upstream synchronization or distribution requires a request that explicitly names it.
* This is a local project for the user's own use and is not intended for public release or distribution. Its main goal is to improve and optimize Codex.
* Treat the active repository root as the checkout location; do not hard-code a workstation-specific checkout path.
* `C:\Users\kuh\Desktop\LOCAL-KD` is the fork home and `C:\Users\kuh\.codex` is the official upstream home. The published fork Desktop must use `CODEX_HOME=C:\Users\kuh\Desktop\LOCAL-KD`.
* This repository contains the Rust CLI and app-server, not the native Windows shell. Source changes become Desktop-visible only after rebuilding and replacing or updating the local binary, then restarting Desktop. Perform those activation steps only when the request includes them.

## Scope and workspace

* Prefer existing owners and abstractions; identify a verified missing capability before adding machinery. Preserve overlapping user work and verify the combined runtime path end to end.

## Validation

* Use the smallest check that proves the changed contract; a full suite requires an explicit request. Reuse passing evidence until relevant inputs change. Let progressing checks finish, batch relevant repairs, and rerun only affected checks. Report unrelated failures without expanding scope.
