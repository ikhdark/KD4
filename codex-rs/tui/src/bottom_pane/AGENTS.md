# TUI bottom pane (state machines)

When changing paste-burst or chat-composer state machines:

- Keep `chat_composer.rs`/`paste_burst.rs` docs as readable top-down behavior
  explanations and align implementation/docstrings unless divergence is explicit.
- Verify docs mention only real APIs and behavior, especially Enter/newline and
  `disable_paste_burst` semantics.
