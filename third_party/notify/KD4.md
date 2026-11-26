This is notify 9.0.0-rc.5 (upstream 853729a63960392a02fd5fc2aae099e0e25bc0a9),
vendored under its original CC0 license.

Local changes: report terminal Linux poll errors and tolerate disconnected
cleanup; report terminal Windows completion errors and fence callback
self-retirement with the existing stopping-token identity; gate the
per-directory watch bookkeeping in `src/paths.rs` to the platforms that compile
the inotify or kqueue backends (a path dependency reports the dead code that
Cargo hides for registry dependencies).

Backported the 9.0.0-rc.6/rc.7 Windows missing-watch and pending-completion
shutdown fixes. The wakeup semaphore uses shared standard-library handle
ownership rather than rc.7's wake-before-stop ordering: KD4 waits indefinitely
for actions, so it must keep send-before-wake without racing handle closure.

Native coverage reconciliation and requested-namespace ownership remain in
codex-file-watcher, which stages replacement backends after coverage loss.

Windows backend: each `ReadDirectoryChangesW` request uses a 64 KiB buffer
(upstream 16 KiB; 64 KiB is the network-path maximum), and `FILE_ACTION_ADDED`
reports `Create(CreateKind::Any)` without querying the new path's metadata on
the notification thread. codex-file-watcher matches `Create(_)` only. In a KDR
burst workload (4,096 creates into a watched directory) this removed overflow
rescans (3 of 12 bursts before, 0 of 12 after) and shortened the creating
process's burst (balanced blocks, pooled median about -27%); see the KDR case
kd4-io-filesystem-latency-ab.

Private FSEvents cleanup removes the unused collecting translator and imprecise
translation mode. Unsupported runtime configuration uses the unchanged Watcher
trait default instead of a no-op forwarding layer.
