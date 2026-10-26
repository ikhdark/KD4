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
  - `writer_sender()` → `mpsc::Sender<Vec<u8>>` (stdin); `close_stdin()` drops the owner's sender
  - `close_stdin_ordered().await` acknowledges pipe input close after queued input. Unix PTYs
    drain accepted input, then return Unsupported instead of injecting VEOF. Drop external sender clones first.
  - `resize(TerminalSize)` and `signal(ProcessSignal)`
  - `has_exited()` and `exit_code()`
  - `request_terminate()` cancels the Windows Job or Unix initial process group and leaves readers draining
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
descendants can hold inherited handles indefinitely, but lets native PTY readers drain for up to
two seconds. `output_capture()` records native EOF separately from forced reader shutdown;
receiver closure alone does not certify complete capture.

Unix `signal(Interrupt)` uses the PTY foreground group on Linux; platforms without an
identity-safe terminal signal operation report Unsupported. Pipe interrupts retain group semantics.
Terminal input cannot provide a reliable pipe-like half-close: terminal state may change and
VEOF may be interpreted as data or a signal. Use pipe stdin when an EOF guarantee is required.

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

## Repository test coverage

This fork does not include test suites or test-only support files. The usage example
above is documentation, not an executable test.
