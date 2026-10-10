use codex_test_binary_support::Arg0PathEntryGuard;
use codex_test_binary_support::configure_test_binary_dispatch;
use std::process::Command;

const HOME: &str = "CODEX_TEST_ALIAS_HOME";
const FAIL: &str = "CODEX_TEST_ALIAS_FAILURE";

#[ctor::ctor(unsafe)]
static DISPATCH: Option<Arg0PathEntryGuard> = {
    // Dispatch helpers even when alias preparation is deliberately broken.
    let home = std::env::var(HOME).unwrap_or_else(|_| "codex-test-binary-support".into());
    let previous = std::env::var_os("CODEX_HOME");
    if std::env::var_os(FAIL).is_some() {
        let result = std::panic::catch_unwind(|| configure_test_binary_dispatch(&home));
        assert!(result.is_err(), "broken alias setup must fail");
        assert_eq!(std::env::var_os("CODEX_HOME"), previous);
        eprintln!("alias failure restored CODEX_HOME");
        std::process::exit(0);
    }
    let guard = configure_test_binary_dispatch(&home);
    assert_eq!(std::env::var_os("CODEX_HOME"), previous);
    guard
};

#[test]
fn startup_restores_home_and_uses_current_binary() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let broken_home = dir.path().join("broken");
    std::fs::create_dir(&broken_home)?;
    std::fs::write(broken_home.join("tmp"), "not a directory")?;
    for previous in [None, Some(dir.path().join("original").into_os_string())] {
        let mut command = Command::new(std::env::current_exe()?);
        command.env(HOME, &broken_home).env(FAIL, "1");
        match previous {
            Some(value) => {
                command.env("CODEX_HOME", value);
            }
            None => {
                command.env_remove("CODEX_HOME");
            }
        }
        let output = command.output()?;
        assert!(output.status.success(), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("alias failure restored CODEX_HOME")
        );
    }

    let alias_dir = std::env::split_paths(&std::env::var_os("PATH").ok_or("missing PATH")?)
        .next()
        .ok_or("missing alias directory")?;
    let alias = alias_dir.join("apply_patch.bat");
    let script = std::fs::read_to_string(&alias)?;
    assert!(script.contains(&std::env::current_exe()?.display().to_string()));
    assert!(alias_dir.join("applypatch.bat").is_file());

    // An inherited, obsolete alias must not override the new startup alias.
    let stale = dir.path().join("stale");
    std::fs::create_dir(&stale)?;
    std::fs::write(stale.join("apply_patch.bat"), "@exit /b 42\r\n")?;
    let output = Command::new(std::env::current_exe()?)
        .args(["--exact", "child_alias_setup", "--nocapture"])
        .env(HOME, dir.path().join("fresh"))
        .env(
            "PATH",
            std::env::join_paths(std::iter::once(stale).chain(std::env::split_paths(
                &std::env::var_os("PATH").ok_or("missing PATH")?,
            )))?,
        )
        .output()?;
    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));

    // Helper dispatch precedes setup: even a broken alias home cannot block it.
    let output = Command::new(std::env::current_exe()?)
        .arg("--codex-run-as-apply-patch")
        .arg("*** Begin Patch\n*** Add File: dispatched.txt\n+helper ran\n*** End Patch")
        .current_dir(dir.path())
        .env(HOME, &broken_home)
        .env(FAIL, "1")
        .output()?;
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("dispatched.txt"))?,
        "helper ran\n"
    );
    Ok(())
}

#[test]
fn child_alias_setup() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let output = Command::new("cmd.exe")
        .args(["/d", "/c", "apply_patch"])
        .current_dir(dir.path())
        .output()?;
    assert!(!output.status.success(), "a patch is required");
    assert_ne!(output.status.code(), Some(42), "stale alias executed");
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Usage: apply_patch"),
        "{output:?}"
    );
    Ok(())
}

#[test]
fn helper_dispatch_takes_precedence_over_discovery() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let home = dir.path().join("must-not-exist");
    let output = Command::new(std::env::current_exe()?)
        .args([
            "--codex-run-as-apply-patch",
            "*** Begin Patch\n*** End Patch",
            "--list",
        ])
        .env(HOME, &home)
        .env_remove(FAIL)
        .output()?;
    // --list is a helper argument here, not a request to list Rust tests. The
    // helper's one-patch CLI must reject the extra argument without alias setup.
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("accepts exactly one argument"),
        "{output:?}"
    );
    assert!(output.stdout.is_empty(), "must not list tests: {output:?}");
    assert!(!home.exists(), "helper dispatch must not create aliases");
    Ok(())
}

#[test]
fn discovery_does_not_create_an_alias_home() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let home = dir.path().join("must-not-exist");
    let output = Command::new(std::env::current_exe()?)
        .arg("--list")
        .env(HOME, &home)
        .env_remove(FAIL)
        .output()?;
    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("child_alias_setup: test"));
    assert!(!home.exists(), "discovery must not create alias files");
    Ok(())
}
