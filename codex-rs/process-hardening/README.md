# codex-process-hardening

This crate provides `pre_main_hardening()`, which is designed to be called pre-`main()` (using `#[ctor::ctor]`) to perform various process hardening steps, such as

- disabling core dumps
- disabling ptrace attach on Linux and macOS
- removing dangerous environment variables such as `LD_PRELOAD` and `DYLD_*`

`pre_main_hardening()` is unsafe: Unix callers must exclude concurrent native
environment access. Call it after dotenv loading and before starting threads.
`harden_process()` applies only the native process policy and does not mutate
the environment. Ordinary CLI/app-server launches do not opt into proxy policy.
