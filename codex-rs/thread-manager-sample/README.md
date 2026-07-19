# ThreadManager Sample

Small one-shot binary that starts a Codex thread with `ThreadManager` from
`codex-core-api`, submits a single user turn, and emits mapped server
notifications as newline-delimited JSON on stdout. Assistant text is represented
by JSONL delta and item notifications; the binary does not print a separate final
assistant message. Nonfatal warnings and recovery notices are written to stderr.

```sh
cargo run -p codex-thread-manager-sample -- "Say hello"
```

Use `--model` to override the configured default model:

```sh
cargo run -p codex-thread-manager-sample -- --model gpt-5.2 "Say hello"
```

The prompt can also be piped through stdin:

```sh
printf 'Say hello\n' | cargo run -p codex-thread-manager-sample
```

## Configuration

This sample constructs a synthetic minimal configuration instead of loading the
normal Codex CLI configuration. It uses read-only permissions without approvals,
configures no MCP servers, disables web search and environment/app/skill/
collaboration instruction injection, keeps thread history ephemeral, and disables
analytics.

Authentication uses existing Codex login state. The sample does not enable
`CODEX_API_KEY` as an environment authentication override; change that only if
standalone environment-key execution becomes part of the sample contract.
