use std::io::Read;
use std::io::Write;
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use pretty_assertions::assert_eq;
use tempfile::TempDir;
use tokio::sync::watch;
use tokio::time::Instant;
use tokio::time::sleep;
use tokio::time::timeout;

use super::UpdateLoopControl;
use super::install_latest_standalone_from_url;
use super::update_modes_for_identities;
use crate::RestartMode;
use crate::UpdaterRefreshMode;
use crate::managed_install::executable_identity_from_bytes;

const CANCELLATION_TIMEOUT: Duration = Duration::from_secs(2);

struct TestHttpServer {
    url: String,
    response_sent: mpsc::Receiver<()>,
    release: Option<mpsc::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl TestHttpServer {
    fn start(response_prefix: Vec<u8>, hold_connection: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test HTTP server");
        let address = listener.local_addr().expect("read test HTTP address");
        let (response_sent_tx, response_sent) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let server_thread = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept updater request");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("set updater request timeout");
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request);
            stream
                .write_all(&response_prefix)
                .expect("write updater response prefix");
            stream.flush().expect("flush updater response prefix");
            response_sent_tx
                .send(())
                .expect("report updater response prefix");
            if hold_connection {
                let _ = release_rx.recv_timeout(Duration::from_secs(5));
            }
        });
        Self {
            url: format!("http://{address}/install.sh"),
            response_sent,
            release: Some(release_tx),
            thread: Some(server_thread),
        }
    }

    fn wait_until_response_sent(&self) {
        self.response_sent
            .recv_timeout(Duration::from_secs(5))
            .expect("updater request did not reach test HTTP server");
    }
}

impl Drop for TestHttpServer {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
        if let Some(server_thread) = self.thread.take() {
            server_thread.join().expect("join test HTTP server");
        }
    }
}

#[test]
fn unchanged_updater_uses_version_based_restart() {
    assert_eq!(
        update_modes_for_identities(
            &executable_identity_from_bytes(b"same"),
            &executable_identity_from_bytes(b"same"),
        ),
        (RestartMode::IfVersionChanged, UpdaterRefreshMode::None)
    );
}

#[test]
fn changed_updater_forces_refresh_even_when_version_may_match() {
    assert_eq!(
        update_modes_for_identities(
            &executable_identity_from_bytes(b"old"),
            &executable_identity_from_bytes(b"new"),
        ),
        (
            RestartMode::Always,
            UpdaterRefreshMode::ReexecIfManagedBinaryChanged,
        )
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_interrupts_stalled_updater_headers() {
    assert_stalled_download_cancels(Vec::new()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_interrupts_stalled_updater_body() {
    assert_stalled_download_cancels(
        b"HTTP/1.1 200 OK\r\nContent-Length: 1024\r\nConnection: close\r\n\r\n".to_vec(),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_interrupts_blocked_script_write_and_kills_installer_group() {
    let temp_dir = TempDir::new().expect("create updater test directory");
    let marker = temp_dir.path().join("installer.pid");
    let mut script = format!("echo $$ > '{}'\nsleep 60\n#", marker.display()).into_bytes();
    script.resize(1024 * 1024, b'x');

    assert_installer_cancels_and_exits(script, &marker).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_interrupts_installer_wait_and_kills_installer_group() {
    let temp_dir = TempDir::new().expect("create updater test directory");
    let marker = temp_dir.path().join("installer.pid");
    let script = format!("echo $$ > '{}'\nsleep 60\n", marker.display()).into_bytes();

    assert_installer_cancels_and_exits(script, &marker).await;
}

async fn assert_stalled_download_cancels(response_prefix: Vec<u8>) {
    let server = TestHttpServer::start(response_prefix, true);
    let (terminate_tx, mut terminate_rx) = watch::channel(false);
    let install_url = server.url.clone();
    let install = tokio::spawn(async move {
        install_latest_standalone_from_url(&install_url, &mut terminate_rx).await
    });

    server.wait_until_response_sent();
    terminate_tx
        .send(true)
        .expect("signal updater cancellation");
    let control = timeout(CANCELLATION_TIMEOUT, install)
        .await
        .expect("updater did not stop promptly")
        .expect("join updater task")
        .expect("cancel updater");
    assert!(matches!(control, UpdateLoopControl::Stop));
}

async fn assert_installer_cancels_and_exits(script: Vec<u8>, marker: &std::path::Path) {
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        script.len()
    );
    let mut response = response.into_bytes();
    response.extend_from_slice(&script);
    let server = TestHttpServer::start(response, false);
    let (terminate_tx, mut terminate_rx) = watch::channel(false);
    let install_url = server.url.clone();
    let install = tokio::spawn(async move {
        install_latest_standalone_from_url(&install_url, &mut terminate_rx).await
    });

    let deadline = Instant::now() + Duration::from_secs(5);
    while !marker.exists() && Instant::now() < deadline {
        sleep(Duration::from_millis(10)).await;
    }
    let installer_pid: i32 = std::fs::read_to_string(marker)
        .expect("installer did not write its pid")
        .trim()
        .parse()
        .expect("installer pid should be numeric");

    terminate_tx
        .send(true)
        .expect("signal updater cancellation");
    let control = timeout(CANCELLATION_TIMEOUT, install)
        .await
        .expect("updater did not stop promptly")
        .expect("join updater task")
        .expect("cancel updater");
    assert!(matches!(control, UpdateLoopControl::Stop));

    let deadline = Instant::now() + CANCELLATION_TIMEOUT;
    while process_group_exists(installer_pid) && Instant::now() < deadline {
        sleep(Duration::from_millis(10)).await;
    }
    assert!(
        !process_group_exists(installer_pid),
        "installer process group {installer_pid} survived cancellation"
    );
}

fn process_group_exists(process_group_id: i32) -> bool {
    let result = unsafe { libc::kill(-process_group_id, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}
