//! Optional smoke tests that hit the real OpenAI /v1/responses endpoint. They are `#[ignore]` by
//! default so CI stays deterministic and free. Developers can run them locally with
//! `just test -p codex-core --test all --run-ignored only live_cli` provided they set a valid
//! `OPENAI_API_KEY`.

use assert_cmd::prelude::*;
use predicates::prelude::*;
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;
use tempfile::TempDir;

fn require_api_key_from(value: Result<String, std::env::VarError>) -> Result<String, String> {
    value.map_err(|_| "OPENAI_API_KEY env var not set".to_string())
}

fn require_api_key() -> String {
    require_api_key_from(std::env::var("OPENAI_API_KEY"))
        .expect("OPENAI_API_KEY env var not set — live test cannot run")
}

fn wait_for_child(
    child: &mut std::process::Child,
    timeout: Duration,
) -> std::io::Result<Option<std::process::ExitStatus>> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(None);
        }
        std::thread::sleep((deadline - now).min(Duration::from_millis(10)));
    }
}

/// Helper that spawns the binary inside a TempDir with minimal flags. Returns (Assert, TempDir).
fn run_live(prompt: &str) -> (assert_cmd::assert::Assert, TempDir) {
    #![expect(clippy::unwrap_used)]
    use std::io::Read;
    use std::io::Write;
    use std::thread;

    let dir = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let codex_home = home.path().join(".codex");
    std::fs::create_dir_all(&codex_home).unwrap();

    // Build a plain `std::process::Command` so we have full control over the underlying stdio
    // handles. `assert_cmd`’s own `Command` wrapper always forces stdout/stderr to be piped
    // internally which prevents us from streaming them live to the terminal (see its `spawn`
    // implementation). Instead we configure the std `Command` ourselves, then later hand the
    // resulting `Output` to `assert_cmd` for the familiar assertions.

    let mut cmd = Command::new(codex_utils_cargo_bin::cargo_bin("codex-rs").unwrap());
    cmd.current_dir(dir.path());
    cmd.env("OPENAI_API_KEY", require_api_key());
    cmd.env("HOME", home.path());
    cmd.env("CODEX_HOME", &codex_home);

    // We want three things at once:
    //   1. live streaming of the child’s stdout/stderr while the test is running
    //   2. captured output so we can keep using assert_cmd’s `Assert` helpers
    //   3. cross‑platform behavior (best effort)
    //
    // To get that we:
    //   • set both stdout and stderr to `piped()` so we can read them programmatically
    //   • spawn a thread for each stream that copies bytes into two sinks:
    //       – the parent process’ stdout/stderr for live visibility
    //       – an in‑memory buffer so we can pass it to `assert_cmd` later

    // Pass the prompt through the `--` separator so the CLI knows when user input ends.
    cmd.arg("--allow-no-git-exec")
        .arg("-v")
        .arg("--")
        .arg(prompt);

    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd.spawn().expect("failed to spawn codex-rs");

    // Send the terminating newline so Session::run exits after the first turn.
    child
        .stdin
        .as_mut()
        .expect("child stdin unavailable")
        .write_all(b"\n")
        .expect("failed to write to child stdin");

    // Helper that tees a ChildStdout/ChildStderr into both the parent’s stdio and a Vec<u8>.
    fn tee<R: Read + Send + 'static>(
        mut reader: R,
        mut writer: impl Write + Send + 'static,
    ) -> thread::JoinHandle<Vec<u8>> {
        thread::spawn(move || {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        writer.write_all(&chunk[..n]).ok();
                        writer.flush().ok();
                        buf.extend_from_slice(&chunk[..n]);
                    }
                    Err(_) => break,
                }
            }
            buf
        })
    }

    let stdout_handle = tee(
        child.stdout.take().expect("child stdout"),
        std::io::stdout(),
    );
    let stderr_handle = tee(
        child.stderr.take().expect("child stderr"),
        std::io::stderr(),
    );

    let status = match wait_for_child(&mut child, Duration::from_secs(5 * 60))
        .expect("failed to wait on child")
    {
        Some(status) => status,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_handle.join();
            let _ = stderr_handle.join();
            panic!("live codex CLI exceeded the five-minute deadline");
        }
    };
    let stdout = stdout_handle.join().expect("stdout thread panicked");
    let stderr = stderr_handle.join().expect("stderr thread panicked");

    let output = std::process::Output {
        status,
        stdout,
        stderr,
    };

    (output.assert(), dir)
}

#[ignore]
#[test]
fn live_create_file_hello_txt() {
    let (assert, dir) = run_live(
        "Use the shell tool with the apply_patch command to create a file named hello.txt containing the text 'hello'.",
    );

    assert.success();

    let path = dir.path().join("hello.txt");
    assert!(path.exists(), "hello.txt was not created by the model");

    let contents = std::fs::read_to_string(path).unwrap();

    assert_eq!(contents.trim(), "hello");
}

#[ignore]
#[test]
fn live_print_working_directory() {
    let (assert, dir) = run_live("Print the current working directory using the shell function.");

    assert
        .success()
        .stdout(predicate::str::contains(dir.path().to_string_lossy()));
}

#[test]
fn missing_api_key_is_an_error() {
    assert_eq!(
        require_api_key_from(Err(std::env::VarError::NotPresent)),
        Err("OPENAI_API_KEY env var not set".to_string())
    );
}

#[test]
fn run_codex_times_out() {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("suite::live_cli::timeout_test_child")
        .arg("--nocapture")
        .env("CODEX_LIVE_CLI_TIMEOUT_CHILD", "1")
        .spawn()
        .expect("spawn timeout test child");

    let status =
        wait_for_child(&mut child, Duration::from_millis(25)).expect("poll timeout test child");
    assert!(
        status.is_none(),
        "child should still be running at the deadline"
    );
    child.kill().expect("kill timeout test child");
    child.wait().expect("reap timeout test child");
}

#[test]
fn timeout_test_child() {
    if std::env::var_os("CODEX_LIVE_CLI_TIMEOUT_CHILD").is_some() {
        std::thread::sleep(Duration::from_secs(30));
    }
}
