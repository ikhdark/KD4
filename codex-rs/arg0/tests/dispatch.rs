#![cfg(windows)]

use std::fs;
use std::io::Write;
use std::process::Command;
use std::process::Stdio;

use anyhow::Context;
use codex_apply_patch::CODEX_CORE_APPLY_PATCH_ARG1;
use codex_arg0::arg0_dispatch;
use codex_arg0::arg0_dispatch_helper;

const TEST_GENERATED_ALIASES_ARG: &str = "--test-generated-patch-aliases";

// Exercise helper dispatch before libtest parses arguments, as production does
// before starting its runtime or mutating the inherited environment.
// SAFETY: this runs before libtest starts threads, matching binary startup.
#[ctor::ctor(unsafe)]
fn dispatch_helper() {
    arg0_dispatch_helper();
    if std::env::args_os().nth(1).as_deref() == Some(TEST_GENERATED_ALIASES_ARG.as_ref()) {
        match exercise_generated_aliases() {
            Ok(()) => std::process::exit(0),
            Err(error) => {
                eprintln!("{error:#}");
                std::process::exit(1);
            }
        }
    }
}

fn exercise_generated_aliases() -> anyhow::Result<()> {
    let guard = arg0_dispatch().context("create generated aliases")?;
    let path = std::env::var_os("PATH").context("PATH must be set")?;
    let alias_dir = std::env::split_paths(&path)
        .next()
        .context("generated aliases must be first on PATH")?;
    let patch = "*** Begin Patch\n*** Add File: result with spaces.txt\n+naïve café\n*** End Patch";
    for name in ["apply_patch", "applypatch"] {
        for stdin in [false, true] {
            let mut command = if stdin {
                let mut command = Command::new("cmd.exe");
                command.args(["/d", "/c", name]).stdin(Stdio::piped());
                command
            } else {
                let mut command = Command::new("powershell.exe");
                command
                    .args(["-NoProfile", "-NonInteractive", "-Command"])
                    .arg(format!(
                        "& {name} $env:CODEX_ARG0_TEST_PATCH; exit $LASTEXITCODE"
                    ))
                    .env("CODEX_ARG0_TEST_PATCH", patch)
                    .stdin(Stdio::null());
                command
            };
            let mut child = command
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?;
            if stdin {
                child
                    .stdin
                    .take()
                    .context("piped stdin")?
                    .write_all(patch.as_bytes())?;
            }
            let output = child.wait_with_output()?;
            anyhow::ensure!(
                output.status.success(),
                "{name} (stdin={stdin}): {}",
                String::from_utf8_lossy(&output.stderr)
            );
            anyhow::ensure!(
                fs::read("result with spaces.txt")? == "naïve café\n".as_bytes(),
                "generated alias must preserve the entire patch"
            );
            fs::remove_file("result with spaces.txt")?;
        }
    }
    anyhow::ensure!(
        fs::read_dir(alias_dir.parent().context("alias parent")?)?.count() == 1,
        "helpers must not run normal startup or create more alias directories"
    );
    drop(guard);
    anyhow::ensure!(!alias_dir.exists(), "session aliases must be cleaned up");
    Ok(())
}

#[test]
fn generated_aliases_preserve_multiline_patches_and_unicode_paths() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let install_dir = root.path().join("工程 install");
    let home = root.path().join("工程 home");
    let workdir = root.path().join("工程 work");
    for path in [&install_dir, &home, &workdir] {
        fs::create_dir(path)?;
    }
    let exe = install_dir.join("codex-test.exe");
    fs::copy(std::env::current_exe()?, &exe)?;
    let output = Command::new(&exe)
        .arg(TEST_GENERATED_ALIASES_ARG)
        .env("CODEX_HOME", &home)
        .current_dir(&workdir)
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        exe.is_file(),
        "alias cleanup must preserve the source executable"
    );
    assert!(!workdir.join("result with spaces.txt").exists());
    Ok(())
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
