# codex-process-hardening

This crate provides `pre_main_hardening()`, which is designed to be called pre-`main()` (using `#[ctor::ctor]`) to perform Windows process hardening steps:

- permanently enabling Data Execution Prevention (DEP)
- disabling legacy extension-point DLL injection

Initialization fails closed: a process exits before `main()` if either mitigation cannot be applied.
