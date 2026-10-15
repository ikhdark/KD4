# Rollout activity capture

Enable optional intermediate activity records in the fork configuration:

```toml
[features]
rollout_activity = true
```

The feature is off by default and requires a binary built with this feature.
It affects new events in sessions using that configuration, including resumed
sessions; it cannot recover events omitted by older runs.

Captured events use the existing JSONL `event_msg` representation:

- `reasoning_content_delta`, `reasoning_raw_content_delta`,
  `agent_reasoning_section_break`
- `plan_delta`, `agent_message_content_delta`
- `patch_apply_updated`, `turn_diff`
- `mcp_tool_call_progress`
- `exec_command_output_delta`, `terminal_interaction`
- `turn_phase_changed`, `safety_buffering`

These observations supplement, rather than replace, completed response items,
tool results, durable plan snapshots and turn timing. They do not enter model
history or change frontend rendering. Explicit non-persisting delivery and
ephemeral sessions stay non-persisting. Only model/backend content actually
emitted can be captured; this does not expose unavailable private reasoning.

Activity uses the existing ordered writer, timestamps, and terminal durability
barrier. There is no per-delta flush or new background queue. As with other
ordered records, an unflushed tail can be lost on process termination. Enabled
capture increases retained memory, disk traffic, and rollout size; a slow store
can add backpressure. Production latency and storage growth are not bounded by
this feature. Disable it when the additional detail is not needed.

Treat these logs as sensitive: they may contain partial source edits, command
output bytes, terminal stdin, and provider-supplied reasoning text. Output
chunks retain their protocol base64 encoding. Access and retention follow the
existing rollout directory policy.

For request-attempt and execution diagnostics beyond the emitted activity,
the existing `CODEX_ROLLOUT_TRACE_ROOT` environment variable enables separate
trace bundles. This feature neither enables those bundles nor copies them into
the rollout. Completed summaries, plans and tool results already persist
without either diagnostic option.
