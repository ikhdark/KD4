This is notify 9.0.0-rc.5 (upstream 853729a63960392a02fd5fc2aae099e0e25bc0a9),
vendored under its original CC0 license.

Local changes: report terminal Linux poll errors and tolerate disconnected
cleanup; report terminal Windows completion errors and fence callback
self-retirement with the existing stopping-token identity; gate the
per-directory watch bookkeeping in `src/paths.rs` to the platforms that compile
the inotify or kqueue backends (a path dependency reports the dead code that
Cargo hides for registry dependencies).

Native coverage reconciliation and requested-namespace ownership remain in
codex-file-watcher, which stages replacement backends after coverage loss.
