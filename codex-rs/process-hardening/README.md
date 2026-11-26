# codex-process-hardening

This crate provides `pre_main_hardening()`, designed for single-threaded startup
(including pre-`main()` callers using `#[ctor::ctor]`). On supported Unix targets,
it performs process hardening such as

- disabling core dumps
- disabling ptrace attach on Linux and macOS
- removing dangerous environment variables such as `LD_PRELOAD` and `DYLD_*`

`pre_main_hardening()` is unsafe: Unix callers must exclude concurrent native
environment access. Call it after dotenv loading and before starting threads.
`harden_process()` applies only the native process policy and does not mutate
the environment. Its Windows policy hook is currently a no-op; the Unix protections
above are not implemented by that hook on Windows. Ordinary CLI/app-server
launches do not opt into proxy policy.

The separate `disable_execution_speed_throttling()` helper opts Windows processes
out of automatic execution-speed throttling while preserving other power settings.
It returns an I/O error if the native operation fails, does not raise scheduling
priority or change timer resolution, and is a no-op on other platforms.

`prefer_largest_last_level_cache()` runs from `codex-arg0` at startup, before
runtime threads exist. On a single-processor-group Windows machine whose
last-level caches differ in size (for example a Ryzen X3D part with one
stacked-cache CCD), it sets this process's default CPU Sets to the logical
processors sharing the uniquely largest cache. CPU Sets are not inherited, so
commands, builds and other child processes keep the whole machine and their
normal affinity. Nothing changes when the caches are single or uniform, when
the process already has a narrower affinity mask or CPU Sets, or on other
platforms. Set `CODEX_CPU_SET_PREFERENCE=off` to keep default placement.
On a Ryzen 9 9950X3D app-server soak this cut app-server CPU cycles 13.8% and
median turn time 35%; placing the same work on the smaller-cache CCD gave no gain.
