# Protocol schema ownership

This crate generates TypeScript bindings and JSON Schemas from
[protocol-types](../app-server-protocol-types/README.md).
[lib.rs](src/lib.rs) exposes generation APIs through the
[compatibility facade](../app-server-protocol/README.md).

| Concern | Source |
| --- | --- |
| Type registration, generation and output transformation | [export.rs](src/export.rs) |
| Generated TypeScript filtering, scanning and formatting | [export/typescript.rs](src/export/typescript.rs) |
| JSON normalization, experimental filtering and annotations | [export/json_schema.rs](src/export/json_schema.rs) |
| Namespaced and flat-v2 bundle construction | [export/schema_bundle.rs](src/export/schema_bundle.rs) |
| Staging and replacing vendored fixture subtrees | [schema_fixtures.rs](src/schema_fixtures.rs) |
| CLI arguments and destination admission | [export binary](../app-server-protocol/src/bin/export.rs), [fixture writer](../app-server-protocol/src/bin/write_schema_fixtures.rs) |

`GenerateTsOptions` and the experimental-API switches select output composition.
The optional formatter is an explicit executable argument; generation does not
require installing one. Fixture writing stages both generators before replacing
the `typescript/` and `json/` subtrees, and retains recovery data if publication
fails. That replacement is local generated-file publication, not product
installation or permission to republish KD4.

Generated artifacts remain in
[app-server-protocol/schema/](../app-server-protocol/schema/). Change their Rust
type/export owner rather than patching generated output directly.
