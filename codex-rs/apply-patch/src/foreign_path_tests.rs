use super::*;
use codex_exec_server::CopyOptions;
use codex_exec_server::ExecutorFileSystemFuture;
use codex_exec_server::FileSystemReadStream;
use codex_exec_server::LOCAL_FS;
use codex_exec_server::ReadDirectoryEntry;

// A POSIX executor on any host. Every operation asserts the URI convention;
// the backing temporary directory is only an implementation detail of the fixture.
struct PosixFileSystem {
    root: PathUri,
    late_write: Option<LateWrite>,
}

struct LateWrite {
    path: &'static str,
    replacement: Option<&'static [u8]>,
    fired: std::sync::atomic::AtomicBool,
}

impl PosixFileSystem {
    fn local(&self, path: &PathUri) -> PathUri {
        assert_eq!(
            path.infer_path_convention(),
            Some(codex_utils_path_uri::PathConvention::Posix)
        );
        let path = path.inferred_native_path_string();
        let relative = if path == "/workspace" {
            ""
        } else {
            path.strip_prefix("/workspace/")
                .expect("executor POSIX path")
        };
        self.root.join(relative).unwrap()
    }
}

impl ExecutorFileSystem for PosixFileSystem {
    fn mutate_file_if_matches<'a>(
        &'a self,
        path: &'a PathUri,
        expected: Option<Vec<u8>>,
        replacement: Option<Vec<u8>>,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, codex_exec_server::FileMutationOutcome> {
        Box::pin(async move {
            let local = self.local(path);
            if let Some(late) = &self.late_write
                && path.inferred_native_path_string() == format!("/workspace/{}", late.path)
                && replacement.as_deref() == late.replacement
                && !late.fired.swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                // Nonparticipating write after the engine's last content and
                // ownership check, before the actual native mutation boundary.
                std::fs::write(local.to_abs_path()?.as_path(), b"foreign\n")?;
            }
            LOCAL_FS
                .mutate_file_if_matches(&local, expected, replacement, sandbox)
                .await
        })
    }
    fn canonicalize<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, PathUri> {
        Box::pin(async move {
            LOCAL_FS.canonicalize(&self.local(path), sandbox).await?;
            Ok(path.clone())
        })
    }
    fn read_file<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Vec<u8>> {
        Box::pin(async move { LOCAL_FS.read_file(&self.local(path), sandbox).await })
    }
    fn read_file_stream<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, FileSystemReadStream> {
        Box::pin(async move { LOCAL_FS.read_file_stream(&self.local(path), sandbox).await })
    }
    fn write_file<'a>(
        &'a self,
        path: &'a PathUri,
        contents: Vec<u8>,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Box::pin(async move {
            LOCAL_FS
                .write_file(&self.local(path), contents, sandbox)
                .await
        })
    }
    fn create_directory<'a>(
        &'a self,
        path: &'a PathUri,
        options: CreateDirectoryOptions,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Box::pin(async move {
            LOCAL_FS
                .create_directory(&self.local(path), options, sandbox)
                .await
        })
    }
    fn get_metadata<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, FileMetadata> {
        Box::pin(async move { LOCAL_FS.get_metadata(&self.local(path), sandbox).await })
    }
    fn read_directory<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Vec<ReadDirectoryEntry>> {
        Box::pin(async move { LOCAL_FS.read_directory(&self.local(path), sandbox).await })
    }
    fn remove<'a>(
        &'a self,
        path: &'a PathUri,
        options: RemoveOptions,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Box::pin(async move { LOCAL_FS.remove(&self.local(path), options, sandbox).await })
    }
    fn copy<'a>(
        &'a self,
        source: &'a PathUri,
        destination: &'a PathUri,
        options: CopyOptions,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Box::pin(async move {
            LOCAL_FS
                .copy(
                    &self.local(source),
                    &self.local(destination),
                    options,
                    sandbox,
                )
                .await
        })
    }
}

#[tokio::test]
async fn cancellation_rolls_back_posix_executor_changes_without_host_path_conversion() {
    for operation in ["add", "delete", "update", "move", "partial_move"] {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        let destination = directory.path().join("destination");
        std::fs::write(&source, "before\n").unwrap();
        std::fs::write(&destination, "previous\n").unwrap();
        let fs = PosixFileSystem {
            root: PathUri::from_host_native_path(directory.path()).unwrap(),
            late_write: None,
        };
        let first = match operation {
            "add" => "*** Add File: added\n+after",
            "delete" => "*** Delete File: source",
            "update" => "*** Update File: source\n@@\n-before\n+after",
            _ => "*** Update File: source\n*** Move to: destination\n@@\n-before\n+after",
        };
        let patch =
            format!("*** Begin Patch\n{first}\n*** Add File: later\n+not admitted\n*** End Patch");
        let failure = apply_patch_with_cancellation(
            &patch,
            &"file:///workspace".parse::<PathUri>().unwrap(),
            &mut Vec::new(),
            &mut Vec::new(),
            &fs,
            None,
            &|| match operation {
                "add" => directory.path().join("added").exists(),
                "delete" | "move" => !source.exists(),
                "update" => std::fs::read(&source).unwrap() == b"after\n",
                _ => std::fs::read(&destination).unwrap() == b"after\n",
            },
        )
        .await
        .unwrap_err();
        assert!(failure.delta().is_exact(), "{operation}");
        assert!(failure.delta().is_empty(), "{operation}");
        assert_eq!(std::fs::read(&source).unwrap(), b"before\n", "{operation}");
        assert_eq!(
            std::fs::read(&destination).unwrap(),
            b"previous\n",
            "{operation}"
        );
        assert!(!directory.path().join("added").exists());
        assert!(!directory.path().join("later").exists());
    }
}

#[tokio::test]
async fn late_native_writes_survive_update_delete_move_and_rollback() {
    for operation in [
        "update",
        "delete",
        "move_destination",
        "move_source",
        "rollback",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        let destination = directory.path().join("destination");
        std::fs::write(&source, b"before\n").unwrap();
        std::fs::write(&destination, b"previous\n").unwrap();
        let first = match operation {
            "delete" => "*** Delete File: source",
            "move_destination" | "move_source" => {
                "*** Update File: source\n*** Move to: destination\n@@\n-before\n+after"
            }
            _ => "*** Update File: source\n@@\n-before\n+after",
        };
        let late = LateWrite {
            path: if operation == "move_destination" {
                "destination"
            } else {
                "source"
            },
            replacement: match operation {
                "delete" | "move_source" => None,
                "rollback" => Some(b"before\n"),
                _ => Some(b"after\n"),
            },
            fired: false.into(),
        };
        let fs = PosixFileSystem {
            root: PathUri::from_host_native_path(directory.path()).unwrap(),
            late_write: Some(late),
        };
        let patch = format!("*** Begin Patch\n{first}\n*** Add File: later\n+never\n*** End Patch");
        let failure = apply_patch_with_cancellation(
            &patch,
            &"file:///workspace".parse::<PathUri>().unwrap(),
            &mut Vec::new(),
            &mut Vec::new(),
            &fs,
            None,
            &|| operation == "rollback" && std::fs::read(&source).unwrap() == b"after\n",
        )
        .await
        .unwrap_err();
        assert!(
            fs.late_write
                .as_ref()
                .unwrap()
                .fired
                .load(std::sync::atomic::Ordering::SeqCst)
        );
        let affected = if operation == "move_destination" {
            &destination
        } else {
            &source
        };
        assert_eq!(
            std::fs::read(affected).unwrap(),
            b"foreign\n",
            "{operation}"
        );
        assert!(
            !failure.delta().is_exact(),
            "{operation}: must not export a false exact effect inventory"
        );
        assert!(!directory.path().join("later").exists());
    }
}
