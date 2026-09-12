use codex_protocol::models::PermissionProfile;
use codex_protocol::permissions::FileSystemSandboxPolicy;
use codex_protocol::permissions::NetworkSandboxPolicy;
use codex_utils_path_uri::PathUri;
use pretty_assertions::assert_eq;
use tokio::io;

use super::*;

#[tokio::test]
async fn sandboxed_file_system_rejects_non_native_uri_as_invalid_input() {
    let runtime_paths = ExecServerRuntimePaths::new(std::env::current_exe().expect("current exe"))
        .expect("runtime paths");
    let file_system = SandboxedFileSystem::new(runtime_paths);
    let sandbox = FileSystemSandboxContext::from_permission_profile(
        PermissionProfile::from_runtime_permissions(
            &FileSystemSandboxPolicy::restricted(Vec::new()),
            NetworkSandboxPolicy::Restricted,
        ),
    );

    let error = file_system
        .read_file(&non_native_uri(), Some(&sandbox))
        .await
        .expect_err("non-native URI should be rejected");

    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
}

fn non_native_uri() -> PathUri {
    let uri = "file:///usr/local/file.txt";

    match PathUri::parse(uri) {
        Ok(uri) => uri,
        Err(err) => panic!("valid non-native URI should parse: {err}"),
    }
}

#[cfg(windows)]
#[tokio::test]
async fn disabled_windows_sandbox_rejects_file_access_before_helper_spawn() {
    let directory = tempfile::tempdir().expect("temp directory");
    let existing_path = directory.path().join("existing.txt");
    let new_path = directory.path().join("new.txt");
    std::fs::write(&existing_path, b"original contents").expect("write original");
    // An attempted helper spawn would fail differently from the required sandbox rejection.
    let runtime_paths = ExecServerRuntimePaths::new(directory.path().join("missing-helper.exe"))
        .expect("runtime paths");
    let file_system = crate::LocalFileSystem::with_runtime_paths(runtime_paths);
    let cwd = codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(directory.path())
        .expect("absolute directory");
    let existing = PathUri::from_abs_path(
        &codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(&existing_path)
            .expect("absolute existing path"),
    );
    let new = PathUri::from_abs_path(
        &codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(&new_path)
            .expect("absolute new path"),
    );
    let sandbox = FileSystemSandboxContext::from_permission_profile_with_cwd(
        PermissionProfile::from_runtime_permissions(
            &FileSystemSandboxPolicy::restricted(Vec::new()),
            NetworkSandboxPolicy::Restricted,
        ),
        PathUri::from_abs_path(&cwd),
    );
    assert_eq!(
        sandbox.windows_sandbox_level,
        codex_protocol::config_types::WindowsSandboxLevel::Disabled
    );

    let read_error = ExecutorFileSystem::read_file(&file_system, &existing, Some(&sandbox))
        .await
        .expect_err("requested sandbox must reject reads when unavailable");
    assert_eq!(read_error.kind(), io::ErrorKind::InvalidInput);
    assert_eq!(
        read_error.to_string(),
        "sandbox intent cannot be enforced on this executor"
    );
    for path in [&existing, &new] {
        let write_error = ExecutorFileSystem::write_file(
            &file_system,
            path,
            b"forbidden replacement".to_vec(),
            Some(&sandbox),
        )
        .await
        .expect_err("requested sandbox must reject writes when unavailable");
        assert_eq!(write_error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(
            write_error.to_string(),
            "sandbox intent cannot be enforced on this executor"
        );
    }
    assert_eq!(
        std::fs::read(&existing_path).expect("original remains"),
        b"original contents"
    );
    assert!(!new_path.exists(), "rejected write must not create a file");

    assert_eq!(
        ExecutorFileSystem::read_file(&file_system, &existing, None)
            .await
            .expect("explicit unsandboxed read"),
        b"original contents"
    );
    ExecutorFileSystem::write_file(&file_system, &new, b"allowed contents".to_vec(), None)
        .await
        .expect("explicit unsandboxed write");
    assert_eq!(
        std::fs::read(&new_path).expect("allowed write exists"),
        b"allowed contents"
    );
}
