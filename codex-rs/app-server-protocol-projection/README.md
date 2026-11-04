# Protocol projection ownership

This crate translates core events and persisted rollout history into app-server
types. Public functions are re-exported by [lib.rs](src/lib.rs), then the
[compatibility facade](../app-server-protocol/README.md).

| Concern | Source |
| --- | --- |
| Stateless event-to-notification conversion | [event_mapping.rs](src/event_mapping.rs) |
| Stateful rollout replay and turn/item reconstruction | [thread_history.rs](src/thread_history.rs) |
| Tool lifecycle item projection | [thread_history/tools.rs](src/thread_history/tools.rs) |
| Collaboration and subagent activity projection | [thread_history/collaboration.rs](src/thread_history/collaboration.rs) |
| Incremental snapshots and ordered batch-change coalescing | [thread_history/changes.rs](src/thread_history/changes.rs) |

`ThreadHistoryBuilder` is the replay entry point; the `build_turns_from_*`
functions materialize history views. Explicit and legacy lifecycle boundaries,
late updates, rollback, synthetic IDs and item ordering are part of replay
behavior, not just presentation formatting.

The `changes` child module materializes recorded positions into final snapshots;
the parent reducer retains event order, rollback and the sole replay-state owner.

Wire type declarations belong in
[protocol-types](../app-server-protocol-types/README.md), not here. Live thread
execution belongs in [core](../core/README.md), and export formatting belongs in
[protocol-schema](../app-server-protocol-schema/README.md).
