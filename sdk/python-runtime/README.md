# Codex CLI Runtime for Python SDK

Windows-specific runtime package consumed by `openai-codex` when no explicit
`CodexConfig.codex_bin` is supplied. This local KD4 checkout is not a published
SDK distribution.

The packaging tools stage a canonical Windows runtime tree (`codex-package.json`,
`bin/codex.exe`, resources, and helper paths) so the SDK can pin an exact CLI
version without checking binaries into the repo. Supported wheel targets are
Windows x64 and ARM64. Staging is local preparation, not permission to publish.

`openai-codex-cli-bin` is intentionally wheel-only. Do not build or publish an
sdist for this package.
