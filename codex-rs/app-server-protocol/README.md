# App-server protocol navigation

This crate is the compatibility facade, not the implementation owner.
[src/lib.rs](src/lib.rs) re-exports three sibling crates so existing
`codex_app_server_protocol::...` imports remain stable.

| If you need to change... | Follow this owner |
| --- | --- |
| Request/response types, method registration, serialization or wire fields | [protocol-types](../app-server-protocol-types/README.md) |
| Core-event notifications or persisted rollout-to-turn reconstruction | [protocol-projection](../app-server-protocol-projection/README.md) |
| JSON Schema/TypeScript generation, filtering or generated-output layout | [protocol-schema](../app-server-protocol-schema/README.md) |

## Dependency direction

```text
protocol-types <--- protocol-projection
      ^
      +----------- protocol-schema

app-server-protocol re-exports all three
```

Start at the owning crate for implementation work; keep the facade small.
The protocol types are distinct from the core event models in
[protocol](../protocol/): projection code bridges those representations.

## Generated artifacts and entry points

- [schema/typescript/](schema/typescript/) and [schema/json/](schema/json/) are
  generated outputs. Follow the Rust owner rather than hand-editing a generated
  field or using generated filenames to infer implementation ownership.
- [src/bin/export.rs](src/bin/export.rs) generates TypeScript and JSON output
  into a new or empty directory, with optional experimental API inclusion.
- [src/bin/write_schema_fixtures.rs](src/bin/write_schema_fixtures.rs) delegates
  fixture replacement to the schema owner. Its default destination is this
  crate's `schema/` tree; it is not a read-only inspection command.
- The [app-server guide](../app-server/README.md) describes the client-facing
  protocol. [Core navigation](../core/README.md) follows runtime execution.

This is an ownership map, not another instruction or wire-contract source.
