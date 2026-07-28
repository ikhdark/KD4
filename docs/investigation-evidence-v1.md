# Investigation evidence metadata version 1

This document is the semantic source of truth for the common evidence envelope
emitted by external evidence providers. The machine-readable shape is
[`schemas/investigation-evidence-v1.schema.json`](schemas/investigation-evidence-v1.schema.json).

The providers remain independent:

- Each provider owns its operation-specific evidence and limitations.
- Implementing this envelope does not grant a provider task-completion,
  orchestration, or runtime-proof authority.
- KD4 may retain valid provider evidence, but it does not move task state,
  hypothesis state, completion authority, or cross-provider orchestration into
  a provider.

## Common result shape

Every evidence-bearing structured provider result includes `evidenceMeta`
version 1. Provider-specific fields remain alongside it.

```json
{
  "evidenceMeta": {
    "schemaVersion": 1,
    "producer": "provider-id",
    "operation": "provider operation name",
    "evidenceBearing": true,
    "payloadCompleteness": "complete | partial | unknown",
    "truncated": false,
    "approximate": false,
    "limitations": [],
    "snapshot": null
  }
}
```

## Field semantics

### `schemaVersion`

Version `1` identifies this contract. A consumer must not interpret an unknown
version as version 1 evidence.

### `producer`

The evidence producer is a non-blank, provider-owned stable identifier. KD4
treats it as opaque and does not maintain a provider-name allowlist.

### `operation`

The provider-owned name of the operation that produced the result.

### `evidenceBearing`

- `true` means the result contains evidence that KD4 may retain.
- `false` means the result is administrative or setup output, such as root
  selection.
- KD4 must not create an external evidence receipt for a result whose value is
  `false`.

### `payloadCompleteness`

- `complete` means the provider delivered its full declared result for this
  invocation.
- `partial` means a known cap, truncation, failed side channel, or omitted
  record reduced the declared result.
- `unknown` means the provider cannot establish completeness.

When a provider knows that records were omitted but cannot establish whether
delivery ended normally, `unknown` takes precedence and `truncated` remains
`true`.

Payload completeness never means that:

- the repository was exhaustively inspected;
- a test proves correctness;
- a hypothesis is confirmed; or
- all callers or execution paths were found.

### `truncated`

`true` only when provider output or provider-declared records were omitted
because of a size, count, depth, or transport bound. Approximate analysis
without an actual omission uses `approximate: true`; it is not automatically
truncated.

### `approximate`

`true` when the result depends on heuristic, identifier-based, historical,
fuzzy, or otherwise non-authoritative relationships.

### `limitations`

This array contains concrete material limitations. Providers do not add generic
boilerplate when no material limitation applies.

### `snapshot`

A provider-owned stable identity for the evidence source, or `null` when no
snapshot applies.

## Structured error results

A provider may return schema-valid, redacted structured error information. KD4
records it as evidence only when `evidenceMeta` is present and valid. A provider
must never place unredacted data in `structuredContent` when its text path is
redacted.

## Non-goals

Version 1 does not define:

- a shared implementation library across providers;
- a cross-provider finding schema;
- hypothesis state;
- completion semantics; or
- cross-provider orchestration.
