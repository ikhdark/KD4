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
