// Isolated exact-source check; URI/sandbox adapters are test stubs, not integration proof.
// Source SHA256: 2408863d3107611475f5489822bc442a1690722bae9cd4f1bfd71404e7ef4496
use std::io;
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;
type FileSystemResult<T> = io::Result<T>;
type FileSystemSandboxContext = ();
#[derive(Clone, Copy)]
struct RemoveOptions { recursive: bool, force: bool }
struct PathUri(PathBuf);
impl PathUri {
    fn from_host_native_path(path: impl AsRef<Path>) -> io::Result<Self> { Ok(Self(path.as_ref().to_path_buf())) }
    fn to_abs_path(&self) -> io::Result<PathBuf> { Ok(self.0.clone()) }
}
struct LocalFileSystem;
impl LocalFileSystem {
    fn unsandboxed() -> Self { Self }
    async fn remove(
        &self,
        path: &PathUri,
        options: RemoveOptions,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<()> {
        reject_sandbox_context(sandbox)?;
        let path = path.to_abs_path()?;
        // Metadata and deletion share one worker handoff. Check cancellation
        // again before the destructive step, as the old await boundary allowed.
        run_cancellable_file_system_task(move |cancel| {
            match std::fs::symlink_metadata(path.as_path()) {
                Ok(metadata) => {
                    check_file_system_cancelled(&cancel)?;
                    let file_type = metadata.file_type();
                    use std::os::windows::fs::FileTypeExt;

                    if file_type.is_symlink_dir() {
                        std::fs::remove_dir(path.as_path())?;
                    } else if file_type.is_dir() {
                        if options.recursive {
                            std::fs::remove_dir_all(path.as_path())?;
                        } else {
                            std::fs::remove_dir(path.as_path())?;
                        }
                    } else {
                        std::fs::remove_file(path.as_path())?;
                    }
                    Ok(())
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound && options.force => Ok(()),
                Err(err) => Err(err),
            }
        })
        .await
    }

}
/// Keep the drop guard in the awaiting task: dropping a caller stops queued work
/// before it starts, and running work at its next cooperative checkpoint.
async fn run_cancellable_file_system_task<T: Send + 'static>(
    work: impl FnOnce(CancellationToken) -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    let cancel = CancellationToken::new();
    let _guard = cancel.clone().drop_guard();
    tokio::task::spawn_blocking(move || {
        check_file_system_cancelled(&cancel)?;
        work(cancel)
    })
    .await
    .map_err(io::Error::other)?
}

fn check_file_system_cancelled(cancel: &CancellationToken) -> io::Result<()> {
    if cancel.is_cancelled() {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "filesystem operation cancelled",
        ));
    }
    Ok(())
}

fn reject_sandbox_context(sandbox: Option<&FileSystemSandboxContext>) -> io::Result<()> {
    if sandbox.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "direct filesystem operations do not accept sandbox context",
        ));
    }
    Ok(())
}

#[tokio::test]
async fn removal_preserves_force_recursive_and_directory_link_behavior() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let fs = LocalFileSystem::unsandboxed();
    let path = dir.path().join("target");
    let uri = PathUri::from_host_native_path(&path)?;
    let options = RemoveOptions {
        recursive: false,
        force: false,
    };
    assert_eq!(
        fs.remove(&uri, options, None)
            .await
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::NotFound
    );
    fs.remove(
        &uri,
        RemoveOptions {
            force: true,
            ..options
        },
        None,
    )
    .await?;
    std::fs::create_dir(&path)?;
    std::fs::write(path.join("child"), b"keep")?;
    assert!(fs.remove(&uri, options, None).await.is_err());
    let link = dir.path().join("link");
    std::os::windows::fs::symlink_dir(&path, &link)?;
    fs.remove(
        &PathUri::from_host_native_path(&link)?,
        RemoveOptions {
            recursive: true,
            force: false,
        },
        None,
    )
    .await?;
    assert_eq!(std::fs::read(path.join("child"))?, b"keep");
    fs.remove(
        &uri,
        RemoveOptions {
            recursive: true,
            force: false,
        },
        None,
    )
    .await?;
    assert!(!path.exists());
    std::fs::write(&path, b"file")?;
    fs.remove(&uri, options, None).await?;
    assert!(!path.exists());
    Ok(())
}

#[test]
fn cancelled_queued_removal_leaves_file_intact() -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()?;
    runtime.block_on(async {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("keep");
        std::fs::write(&path, b"keep")?;
        let uri = PathUri::from_host_native_path(&path)?;
        let fs = LocalFileSystem::unsandboxed();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let worker = tokio::task::spawn_blocking(move || {
            let _ = started_tx.send(());
            let _ = release_rx.recv_timeout(std::time::Duration::from_secs(5));
        });
        started_rx.await?;
        let mut removal = Box::pin(fs.remove(&uri, RemoveOptions { recursive: false, force: false }, None));
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(removal.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        }).await;
        drop(removal);
        release_tx.send(())?;
        worker.await?;
        tokio::task::spawn_blocking(|| ()).await?;
        assert_eq!(std::fs::read(path)?, b"keep");
        Ok(())
    })
}


