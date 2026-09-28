#![cfg(windows)]

use std::fs;
use std::io::Write;
use std::process::Command;
use std::process::Stdio;

use codex_apply_patch::CODEX_CORE_APPLY_PATCH_ARG1;
use codex_arg0::arg0_dispatch_helper;

// Exercise helper dispatch before libtest parses arguments, as production does
// before starting its runtime or mutating the inherited environment.
// SAFETY: this runs before libtest starts threads, matching binary startup.
#[ctor::ctor(unsafe)]
fn dispatch_helper() {
    arg0_dispatch_helper();
}

#[test]
fn patch_helpers_dispatch_before_normal_startup() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let workdir = root.path().join("working directory with spaces");
    fs::create_dir(&workdir)?;
    let home = root.path().join("must not be initialized");
    let exe = std::env::current_exe()?;

    for name in ["apply_patch.exe", "APPLYPATCH.EXE", "renamed-codex.exe"] {
        let helper = workdir.join(name);
        fs::copy(&exe, &helper)?;
        for stdin in [false, true] {
            let patch =
                "*** Begin Patch\n*** Add File: result with spaces.txt\n+naïve café\n*** End Patch";
            let mut command = Command::new(&helper);
            command.current_dir(&workdir).env("CODEX_HOME", &home);
            if name == "renamed-codex.exe" {
                command.arg(CODEX_CORE_APPLY_PATCH_ARG1);
            }
            if stdin {
                command.stdin(Stdio::piped());
            } else {
                command.arg(patch).stdin(Stdio::null());
            }
            let mut child = command
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?;
            if stdin {
                child
                    .stdin
                    .take()
                    .expect("piped stdin")
                    .write_all(patch.as_bytes())?;
            }
            let output = child.wait_with_output()?;
            assert!(
                output.status.success(),
                "{name}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                String::from_utf8(output.stdout)?
                    .starts_with("Success. Updated the following files:")
            );
            assert_eq!(
                fs::read_to_string(workdir.join("result with spaces.txt"))?,
                "naïve café\n"
            );
            assert!(
                !home.exists(),
                "helper must not initialize normal CODEX_HOME"
            );
            fs::remove_file(workdir.join("result with spaces.txt"))?;
        }
        let mut command = Command::new(&helper);
        command.current_dir(&workdir).env("CODEX_HOME", &home);
        if name == "renamed-codex.exe" {
            command.arg(CODEX_CORE_APPLY_PATCH_ARG1);
        }
        let output = command.args(["patch", "extra"]).output()?;
        assert_eq!(output.status.code(), Some(2));
        assert!(String::from_utf8(output.stderr)?.contains("accepts exactly one argument"));
        assert!(!home.exists());
    }
    Ok(())
}

#[test]
fn unrelated_executable_names_do_not_dispatch() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let unrelated = root.path().join("apply_patch-other.exe");
    fs::copy(std::env::current_exe()?, &unrelated)?;
    let output = Command::new(unrelated)
        .args(["--list", "--format", "terse"])
        .output()?;
    assert!(output.status.success());
    assert!(
        String::from_utf8(output.stdout)?
            .contains("patch_helpers_dispatch_before_normal_startup: test")
    );
    assert!(output.stderr.is_empty());
    Ok(())
}
