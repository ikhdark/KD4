# codex-utils-pty

Helpers for spawning processes under a PTY (ConPTY on Windows) or regular pipes behind one
session handle for stdin, output, resize, signals, and termination. Windows roots run inside a
Job Object so termination covers the process tree, while a normal root exit preserves
background descendants.

## API surface

- `spawn_pty_process(program, args, cwd, env, arg0, size)` → `SpawnedProcess`
- `spawn_pipe_process(program, args, cwd, env, arg0)` → `SpawnedProcess`
- `spawn_pipe_process_no_stdin(program, args, cwd, env, arg0)` → `SpawnedProcess` (stdin reads EOF)
- `spawn_from_driver(ProcessDriver)` → `SpawnedProcess` for integrations with their own process
  transport
- `SpawnedProcess` bundles `session`, split `stdout_rx`/`stderr_rx`, and `exit_rx` (oneshot exit
  code). The split receivers deliver every byte of each stream in order; a PTY merges both
  streams into `stdout_rx`.
- `combine_output_receivers(stdout_rx, stderr_rx)` → lossy `broadcast::Receiver<Vec<u8>>`. Slow
  consumers can lag and lose chunks, so use the split receivers for exact output.
- `ProcessHandle` exposes:
  - `writer_sender()` → `mpsc::Sender<Vec<u8>>` (stdin) and `close_stdin()`
  - `resize(TerminalSize)` and `signal(ProcessSignal)`
  - `has_exited()` and `exit_code()`
  - `request_terminate()` kills the process tree and leaves the readers draining
  - `finish()` releases an exited child without signalling it; `terminate()` does both
  - `release_pty_after_exit()` closes stdin and releases the pseudoconsole
- `conpty_supported()` → `bool` (Windows build check; always true elsewhere)
- `ManagedRootProcess` and `install_managed_root_admission_reclaimer` bound the number of live
  managed process trees.

## Output lifecycle

Output can arrive after the exit notification, so drain the split receivers until they close.
Pipe output closes once every holder of the child's pipe handles exits. ConPTY keeps its output
pipe open until the pseudoconsole is released, so the PTY waiter releases it when the root exits;
the reader then delivers the final frame and closes. `finish()` aborts pipe readers because
descendants can hold inherited handles indefinitely, but lets a ConPTY reader drain for up to two
seconds.

## Usage example

```rust
use std::collections::HashMap;
use std::path::Path;

use codex_utils_pty::SpawnedProcess;
use codex_utils_pty::TerminalSize;
use codex_utils_pty::spawn_pty_process;

async fn run() -> anyhow::Result<(Vec<u8>, i32)> {
    let env: HashMap<String, String> = std::env::vars().collect();
    let SpawnedProcess {
        session,
        mut stdout_rx,
        exit_rx,
        ..
    } = spawn_pty_process(
        "cmd.exe",
        &["/D".into(), "/C".into(), "echo hello".into()],
        Path::new("."),
        &env,
        &None,
        TerminalSize::default(),
    )
    .await?;

    // The PTY merges stderr into stdout; the stream closes after the final frame.
    let mut output = Vec::new();
    while let Some(chunk) = stdout_rx.recv().await {
        output.extend_from_slice(&chunk);
    }
    let exit_code = exit_rx.await.unwrap_or(-1);
    drop(session);
    Ok((output, exit_code))
}
```

Swap in `spawn_pipe_process` for a non-TTY subprocess with separate stdout and stderr; the rest
of the API stays the same.

## Tests

Tests live beside the modules they cover (`src/tests.rs`, `src/pipe_tests.rs`,
`src/windows_tests.rs`, and each module's unit tests) plus `tests/conpty_search_path.rs`. Some
Windows process tests need Python or PowerShell; set `CODEX_REQUIRE_WINDOWS_SANDBOX_PROCESS_TESTS`
to turn a missing prerequisite into a failure. Run with:

```text
just test-fast -p codex-utils-pty
```
