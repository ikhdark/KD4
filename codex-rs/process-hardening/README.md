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
