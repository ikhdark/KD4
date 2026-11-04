# Protocol type ownership

This crate owns app-server wire types and method registration.
[lib.rs](src/lib.rs) keeps their public re-exports; the
[compatibility facade](../app-server-protocol/README.md) exposes them to existing
consumers. It does not own live session handling or rollout replay.

| Concern | Source |
| --- | --- |
| JSON-RPC envelopes and request IDs | [rpc.rs](src/rpc.rs) |
| Shared auth/search types and registration facade | [protocol/common.rs](src/protocol/common.rs) |
| Client-initiated requests, serialization scopes and response exports | [common/client_requests.rs](src/protocol/common/client_requests.rs) |
| Server-initiated requests and response exports | [common/server_requests.rs](src/protocol/common/server_requests.rs) |
| Client/server notification registries and delivery metadata | [common/notifications.rs](src/protocol/common/notifications.rs) |
| Legacy API types | [protocol/v1.rs](src/protocol/v1.rs) |
| Domain-specific v2 types | [protocol/v2/mod.rs](src/protocol/v2/mod.rs), then the named sibling modules |
| Thread listing, search, read and history pagination | [protocol/v2/thread_queries.rs](src/protocol/v2/thread_queries.rs) |
| Thread lifecycle, settings, status and history mutations | [protocol/v2/thread.rs](src/protocol/v2/thread.rs) |
| Shared item construction | [protocol/item_builders.rs](src/protocol/item_builders.rs) |
| Server build/capability metadata | [protocol/server_info.rs](src/protocol/server_info.rs) |
| Experimental API and raw schema support | [experimental_api.rs](src/experimental_api.rs), [schema_support.rs](src/schema_support.rs) |

Type declarations and their serde, TypeScript and JSON Schema attributes jointly
define the wire contract. Follow method registration and schema export consumers
when changing one; an unchanged Rust re-export alone does not prove wire
compatibility. [Projection](../app-server-protocol-projection/README.md) and
[schema generation](../app-server-protocol-schema/README.md) are separate owners.
