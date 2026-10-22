#![cfg(windows)]
// The constructor helper below is test support that runs before libtest, so
// clippy does not see it as a test; its failures must abort the child loudly.
#![allow(clippy::unwrap_used)]

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
    if let Some(marker) = std::env::var_os("ARG0_TEST_DECOY_MARKER")
        && std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .ends_with("other")
    {
        fs::write(marker, b"wrong executable").unwrap();
        std::process::exit(99);
    }
    arg0_dispatch_helper();
    if std::env::args_os().nth(1).as_deref()
        == Some(std::ffi::OsStr::new("--arg0-test-pre-runtime"))
    {
        let startup_thread = std::thread::current().id();
        codex_arg0::arg0_dispatch_or_else_with_pre_runtime(move || {
            assert_eq!(std::env::var("ARG0_TEST_PHASE").unwrap(), "dotenv");
            assert!(tokio::runtime::Handle::try_current().is_err());
            // SAFETY: The constructor/pre-runtime hook has not started threads.
            unsafe { std::env::set_var("ARG0_TEST_PHASE", "prepared") };
            move |_| async move {
                assert_ne!(std::thread::current().id(), startup_thread);
                assert_eq!(std::env::var("ARG0_TEST_PHASE").unwrap(), "prepared");
                Ok(())
            }
        })
        .unwrap();
        std::process::exit(0);
    }
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() == Some(std::ffi::OsStr::new("--arg0-test-alias")) {
        // Run the real alias producer and keep its guard alive while cmd uses
        // the generated PATH. No hand-authored batch fixture substitutes for it.
        let _guard = codex_arg0::arg0_dispatch();
        let status = Command::new("cmd.exe")
            .args(["/D", "/V:ON", "/C"])
            .args(args)
            .status()
            .unwrap();
        std::process::exit(status.code().unwrap_or(1));
    }
}

#[test]
fn startup_preparation_follows_dotenv_and_precedes_runtime() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    fs::write(home.path().join(".env"), "ARG0_TEST_PHASE=dotenv\n")?;
    let output = Command::new(std::env::current_exe()?)
        .arg("--arg0-test-pre-runtime")
        .env("CODEX_HOME", home.path())
        .env("ARG0_TEST_PHASE", "inherited")
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

#[cfg(debug_assertions)]
#[test]
fn generated_aliases_preserve_literal_executable_path_and_status() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let home = root.path().join("home");
    let decoy = root.path().join("other");
    fs::create_dir(&home)?;
    fs::create_dir(&decoy)?;
    let marker = root.path().join("decoy-ran");
    let exe = std::env::current_exe()?;
    fs::copy(&exe, decoy.join("codex-fixture.exe"))?;

    for directory in ["normal path", "%KD4_SLOT%", "!KD4_SLOT!"] {
        let intended = root.path().join(directory);
        fs::create_dir(&intended)?;
        let helper = intended.join("codex-fixture.exe");
        fs::copy(&exe, &helper)?;
        for alias in ["apply_patch", "applypatch"] {
            let mut command = Command::new(&helper);
            command
                .args(["--arg0-test-alias", alias])
                .current_dir(root.path())
                .env("CODEX_HOME", &home)
                .env("KD4_SLOT", "other")
                .env("ARG0_TEST_DECOY_MARKER", &marker)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = command.spawn()?;
            child.stdin.take().unwrap().write_all(
                b"*** Begin Patch\n*** Add File: alias-result.txt\n+literal target\n*** End Patch\n",
            )?;
            let output = child.wait_with_output()?;
            assert!(
                output.status.success(),
                "{directory}/{alias}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                fs::read_to_string(root.path().join("alias-result.txt"))?,
                "literal target\n"
            );
            fs::remove_file(root.path().join("alias-result.txt"))?;
            let output = Command::new(&helper)
                .args(["--arg0-test-alias", alias, "patch", "extra"])
                .current_dir(root.path())
                .env("CODEX_HOME", &home)
                .env("KD4_SLOT", "other")
                .env("ERRORLEVEL", "99")
                .env("ARG0_TEST_DECOY_MARKER", &marker)
                .output()?;
            assert_eq!(output.status.code(), Some(2));
            assert!(String::from_utf8(output.stderr)?.contains("accepts exactly one argument"));
            assert!(!marker.exists(), "expanded path selected the decoy");
        }
    }
    Ok(())
}

#[test]
fn patch_helper_names_and_hidden_flag_dispatch_to_apply_patch() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let workdir = root.path().join("working directory with spaces");
    fs::create_dir(&workdir)?;
    let exe = std::env::current_exe()?;

    for name in ["apply_patch.exe", "APPLYPATCH.EXE", "renamed-codex.exe"] {
        let helper = workdir.join(name);
        fs::copy(&exe, &helper)?;
        for stdin in [false, true] {
            let patch =
                "*** Begin Patch\n*** Add File: result with spaces.txt\n+naïve café\n*** End Patch";
            let mut command = Command::new(&helper);
            command.current_dir(&workdir);
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
            fs::remove_file(workdir.join("result with spaces.txt"))?;
        }
        let mut command = Command::new(&helper);
        command.current_dir(&workdir);
        if name == "renamed-codex.exe" {
            command.arg(CODEX_CORE_APPLY_PATCH_ARG1);
        }
        let output = command.args(["patch", "extra"]).output()?;
        assert_eq!(output.status.code(), Some(2));
        assert!(String::from_utf8(output.stderr)?.contains("accepts exactly one argument"));
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
            .contains("patch_helper_names_and_hidden_flag_dispatch_to_apply_patch: test")
    );
    assert!(output.stderr.is_empty());
    Ok(())
}
