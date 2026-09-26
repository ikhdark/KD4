use std::time::Duration;

use pretty_assertions::assert_eq;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;

use super::*;

#[tokio::test]
async fn prepare_private_socket_directory_creates_directory() {
    let temp_dir = tempfile::TempDir::new().expect("temp dir");
    let socket_dir = temp_dir.path().join("app-server-control");

    prepare_private_socket_directory(&socket_dir)
        .await
        .expect("socket dir should be created");

    assert!(socket_dir.is_dir());
}

#[tokio::test]
async fn bound_listener_path_is_stale_socket_path() {
    let temp_dir = tempfile::TempDir::new().expect("temp dir");
    let socket_path = temp_dir.path().join("socket");
    assert!(
        !is_stale_socket_path(&socket_path)
            .await
            .expect("missing path check")
    );
    let _listener = tokio::time::timeout(Duration::from_secs(10), UnixListener::bind(&socket_path))
        .await
        .expect("bind deadline")
        .expect("bind socket");

    assert!(
        is_stale_socket_path(&socket_path)
            .await
            .expect("stale socket check should succeed")
    );
}

#[tokio::test]
async fn non_socket_paths_are_not_stale_socket_paths() {
    let temp_dir = tempfile::TempDir::new().expect("temp dir");
    let file_path = temp_dir.path().join("regular");
    std::fs::write(&file_path, b"keep me").expect("write regular file");
    // Windows refuses AF_UNIX connections to regular files exactly as it refuses a dead
    // listener, so callers rely on this check before removing a refused path.
    assert_eq!(
        UnixStream::connect(&file_path)
            .await
            .err()
            .map(|err| err.kind()),
        Some(std::io::ErrorKind::ConnectionRefused)
    );

    for path in [file_path.as_path(), temp_dir.path()] {
        assert!(
            !is_stale_socket_path(path)
                .await
                .expect("non-socket path check"),
            "{} must not be treated as a socket",
            path.display()
        );
    }
    assert_eq!(
        std::fs::read(&file_path).expect("regular file remains"),
        b"keep me"
    );
}

#[tokio::test]
async fn stream_round_trips_data_between_listener_and_client() {
    let temp_dir = tempfile::TempDir::new().expect("temp dir");
    let socket_path = temp_dir.path().join("socket");
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut listener = UnixListener::bind(&socket_path).await.expect("bind socket");
        let server = async {
            let mut stream = listener.accept().await.expect("accept connection");
            let mut request = Vec::new();
            stream
                .read_to_end(&mut request)
                .await
                .expect("read through client EOF");
            assert_eq!(request, b"request");
            stream
                .write_all(b"response")
                .await
                .expect("write response after client EOF");
        };
        let client = async {
            let mut stream = UnixStream::connect(&socket_path)
                .await
                .expect("connect client");
            stream.write_all(b"request").await.expect("write request");
            stream
                .shutdown()
                .await
                .expect("half-close client write side");
            let mut response = [0; 8];
            stream
                .read_exact(&mut response)
                .await
                .expect("read response after half-close");
            assert_eq!(&response, b"response");
        };
        tokio::join!(server, client);
    })
    .await
    .expect("socket exchange deadline");
}
