#![allow(clippy::expect_used)]

mod common;

#[path = "file_system/shared.rs"]
mod shared;
#[path = "file_system/support.rs"]
mod support;

use std::path::Path;
use std::process::Command;

use anyhow::Result;
use codex_exec_server::FileSystemSandboxContext;
use codex_protocol::config_types::WindowsSandboxLevel;
use codex_protocol::protocol::SandboxPolicy;
use codex_utils_path_uri::PathUri;
use test_case::test_case;

use crate::support::FileSystemImplementation;
use crate::support::create_file_system_context;

fn create_directory_junction(target: &Path, alias: &Path) -> Result<()> {
    let output = Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(alias)
        .arg(target)
        .output()?;
    if !output.status.success() {
        anyhow::bail!(
            "mklink /J failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout).trim(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

#[test_case(FileSystemImplementation::Local ; "local")]
#[test_case(FileSystemImplementation::Remote ; "remote")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_system_canonicalize_resolves_directory_junction(
    implementation: FileSystemImplementation,
) -> Result<()> {
    shared::assert_canonicalize_resolves_directory_alias(implementation, create_directory_junction)
        .await
}

#[test_case(FileSystemImplementation::Local ; "local")]
#[test_case(FileSystemImplementation::Remote ; "remote")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_system_sandboxed_canonicalize_resolves_directory_junction(
    implementation: FileSystemImplementation,
) -> Result<()> {
    shared::assert_sandboxed_canonicalize_resolves_directory_alias(
        implementation,
        create_directory_junction,
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_system_remote_fs_helper_respects_windows_sandbox_write_policy() -> Result<()> {
    let context = create_file_system_context(FileSystemImplementation::Remote).await?;
    let file_system = context.file_system;
    let tmp = tempfile::TempDir::new()?;
    let readonly_dir = tmp.path().join("readonly");
    std::fs::create_dir_all(&readonly_dir)?;

    let mut sandbox = read_only_sandbox_for_cwd(readonly_dir.clone())?;
    sandbox.windows_sandbox_level = WindowsSandboxLevel::RestrictedToken;

    let readable_file = readonly_dir.join("readable.txt");
    std::fs::write(&readable_file, b"readable")?;
    let mut read_result = file_system
        .read_file(
            &PathUri::from_host_native_path(&readable_file)?,
            Some(&sandbox),
        )
        .await;
    // Some local Windows hosts cannot safely run the restricted-token backend.
    // Reaching a fail-closed backend error still proves the remote fs helper
    // went through the Windows sandbox launcher; before the wrapper fix this
    // read would have run unsandboxed. The write policy is then checked with
    // the elevated backend the other sandboxed tests in this binary use.
    if is_unsupported_restricted_token_host(&read_result) {
        sandbox = crate::support::read_only_sandbox(readonly_dir.clone());
        read_result = file_system
            .read_file(
                &PathUri::from_host_native_path(&readable_file)?,
                Some(&sandbox),
            )
            .await;
    }
    assert_eq!(read_result?, b"readable");

    let blocked_file = readonly_dir.join("blocked.txt");
    let error = file_system
        .write_file(
            &PathUri::from_host_native_path(&blocked_file)?,
            b"blocked".to_vec(),
            Some(&sandbox),
        )
        .await
        .expect_err("write outside the sandbox should fail");
    assert!(
        !blocked_file.exists(),
        "sandboxed fs helper must not create blocked file after error: {error}"
    );

    Ok(())
}

fn read_only_sandbox_for_cwd(cwd: std::path::PathBuf) -> Result<FileSystemSandboxContext> {
    Ok(FileSystemSandboxContext::from_legacy_sandbox_policy(
        SandboxPolicy::new_read_only_policy(),
        PathUri::from_host_native_path(cwd)?,
    )?)
}

#[test_case(FileSystemImplementation::Local ; "local")]
#[test_case(FileSystemImplementation::Remote ; "remote")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_system_read_only_capture_admission(
    implementation: FileSystemImplementation,
) -> Result<()> {
    let context = create_file_system_context(implementation).await?;
    let tmp = tempfile::TempDir::new()?;
    let path = tmp.path().join("source.txt");
    std::fs::write(&path, b"source bytes")?;
    let uri = PathUri::from_host_native_path(&path)?;
    let sandbox = crate::support::read_only_sandbox(tmp.path().to_path_buf());
    let started = std::time::Instant::now();
    assert_eq!(
        context
            .file_system
            .read_file_bounded(&uri, 1024, Some(&sandbox))
            .await?,
        Some(b"source bytes".to_vec()),
    );
    let (bytes, source) = context
        .file_system
        .read_file_bounded_with_source(&uri, 1024, Some(&sandbox))
        .await?
        .expect("read-only source inspection must acquire the file");
    assert_eq!(bytes, b"source bytes");
    assert_eq!(
        std::fs::canonicalize(source.expect("opened source identity"))?,
        std::fs::canonicalize(&path)?,
    );
    assert!(
        context
            .file_system
            .get_metadata(&uri, Some(&sandbox))
            .await?
            .is_file
    );
    // A stalled writer must remain an immediate, explicit transient failure,
    // never a missing-file observation or a wait on that writer.
    let writer = codex_file_system::try_acquire_patch_capture(&path)?;
    let contended = std::time::Instant::now();
    let error = context
        .file_system
        .read_file_bounded(&uri, 1024, Some(&sandbox))
        .await
        .expect_err("captured source must not be readable");
    assert!(
        error
            .to_string()
            .contains("native patch capture is in progress"),
        "{error}"
    );
    assert!(contended.elapsed() < std::time::Duration::from_secs(10));
    drop(writer);
    assert_eq!(
        context
            .file_system
            .read_file_bounded(&uri, 1024, Some(&sandbox))
            .await?,
        Some(b"source bytes".to_vec()),
    );
    let blocked = tmp.path().join("blocked.txt");
    assert!(
        context
            .file_system
            .write_file(
                &PathUri::from_host_native_path(&blocked)?,
                b"blocked".to_vec(),
                Some(&sandbox),
            )
            .await
            .is_err()
    );
    assert!(!blocked.exists());
    assert_eq!(std::fs::read_dir(tmp.path())?.count(), 1);
    eprintln!(
        "read-only native capture workload ({implementation}): {:?}",
        started.elapsed()
    );
    Ok(())
}

fn is_unsupported_restricted_token_host<T>(result: &std::io::Result<T>) -> bool {
    result.as_ref().err().is_some_and(|err| {
        let message = err.to_string();
        message.contains("windows sandbox failed: CreateRestrictedToken failed: 87")
            || message.contains(
                "legacy Windows restricted-token sandbox cannot safely enforce delete boundaries",
            )
    })
}
