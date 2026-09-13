use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;

use anyhow::Result;
use futures::future::BoxFuture;
use pretty_assertions::assert_eq;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio_tungstenite::accept_async;

use super::ExecServerClient;
use super::spawn_stdio_command;
use crate::ExecServerError;
use crate::NoiseChannelIdentity;
use crate::NoiseChannelPublicKey;
use crate::NoiseRendezvousConnectBundle;
use crate::NoiseRendezvousConnectProvider;
use crate::client_api::StdioExecServerCommand;

struct SequenceNoiseConnectProvider {
    bundles: Mutex<VecDeque<NoiseRendezvousConnectBundle>>,
    returned_urls: Mutex<Vec<String>>,
}

impl SequenceNoiseConnectProvider {
    fn new(bundles: Vec<NoiseRendezvousConnectBundle>) -> Self {
        Self {
            bundles: Mutex::new(bundles.into()),
            returned_urls: Mutex::new(Vec::new()),
        }
    }

    fn returned_urls(&self) -> Vec<String> {
        self.returned_urls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl NoiseRendezvousConnectProvider for SequenceNoiseConnectProvider {
    fn connect_bundle(
        &self,
        _: NoiseChannelPublicKey,
    ) -> BoxFuture<'_, Result<NoiseRendezvousConnectBundle, ExecServerError>> {
        let result = self
            .bundles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
            .ok_or_else(|| ExecServerError::Protocol("test Noise provider exhausted".to_string()));
        if let Ok(bundle) = &result {
            self.returned_urls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(bundle.websocket_url.clone());
        }
        Box::pin(async move { result })
    }
}

fn test_bundle(websocket_url: String) -> Result<NoiseRendezvousConnectBundle> {
    Ok(NoiseRendezvousConnectBundle {
        websocket_url,
        environment_id: "environment".to_string(),
        executor_registration_id: "registration".to_string(),
        executor_public_key: NoiseChannelIdentity::generate()?.public_key(),
        harness_key_authorization: "authorization".to_string(),
    })
}

#[tokio::test]
async fn initial_noise_connection_refreshes_bundle_after_unauthorized_handshake() -> Result<()> {
    let unauthorized_listener = TcpListener::bind("127.0.0.1:0").await?;
    let unauthorized_url = format!("ws://{}", unauthorized_listener.local_addr()?);
    let unauthorized_server = tokio::spawn(async move {
        let (mut socket, _) = unauthorized_listener.accept().await?;
        let mut request = [0_u8; 4096];
        let _ = socket.read(&mut request).await?;
        socket
            .write_all(
                b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await?;
        socket.shutdown().await?;
        anyhow::Ok(())
    });
    let accepted_listener = TcpListener::bind("127.0.0.1:0").await?;
    let accepted_url = format!("ws://{}", accepted_listener.local_addr()?);
    let accepted_server = tokio::spawn(async move {
        let (socket, _) = accepted_listener.accept().await?;
        let _websocket = accept_async(socket).await?;
        anyhow::Ok(())
    });
    let sequence = Arc::new(SequenceNoiseConnectProvider::new(vec![
        test_bundle(unauthorized_url.clone())?,
        test_bundle(accepted_url.clone())?,
    ]));
    let provider: Arc<dyn NoiseRendezvousConnectProvider> = sequence.clone();
    let identity = NoiseChannelIdentity::generate()?;

    let _connection =
        ExecServerClient::open_initial_noise_rendezvous_connection(&provider, &identity).await?;

    assert_eq!(
        sequence.returned_urls(),
        vec![unauthorized_url, accepted_url]
    );
    unauthorized_server.await??;
    accepted_server.await??;
    Ok(())
}

#[tokio::test]
async fn stdio_command_uses_declared_environment_and_managed_process_tree() -> Result<()> {
    let program = std::env::var("SystemRoot")
        .map(std::path::PathBuf::from)?
        .join("System32")
        .join("cmd.exe");
    let command = StdioExecServerCommand {
        program: program.to_string_lossy().into_owned(),
        args: vec![
            "/D".to_string(),
            "/S".to_string(),
            "/C".to_string(),
            "if defined PATH (exit 9) else (exit 0)".to_string(),
        ],
        env: HashMap::new(),
        cwd: None,
    };

    let (mut child, managed_root) = spawn_stdio_command(&command).await?;
    assert!(managed_root.id() > 0);
    let status = child.wait().await?;

    assert_eq!(status.code(), Some(0));
    Ok(())
}

// Occupying Tokio's only blocking worker models slow CA file/root loading without
// replacing the connection code or depending on a particular machine's CA store.
fn assert_websocket_deadline_covers_tls_preparation(noise: bool) -> Result<()> {
    use crate::NoiseRendezvousConnectArgs;
    use crate::RemoteExecServerConnectArgs;
    use std::time::Duration;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()?;
    runtime.block_on(async {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let websocket_url = format!("ws://{}", listener.local_addr()?);
        let deadline = Duration::from_millis(20);
        let (release, held) = std::sync::mpsc::channel::<()>();
        let (started, ready) = tokio::sync::oneshot::channel();
        let blocker = tokio::task::spawn_blocking(move || {
            let _ = started.send(());
            let _ = held.recv();
        });
        ready.await?;
        let connection = async {
            if noise {
                ExecServerClient::connect_noise_rendezvous(NoiseRendezvousConnectArgs {
                    bundle: test_bundle(websocket_url.clone())?,
                    harness_identity: NoiseChannelIdentity::generate()?,
                    client_name: "tls-deadline-test".to_string(),
                    connect_timeout: deadline,
                    initialize_timeout: Duration::from_secs(1),
                    resume_session_id: None,
                }).await.map_err(anyhow::Error::from)
            } else {
                let mut args = RemoteExecServerConnectArgs::new(websocket_url.clone(), "tls-deadline-test".to_string());
                args.connect_timeout = deadline;
                ExecServerClient::connect_websocket(args).await.map_err(anyhow::Error::from)
            }
        };
        let result = tokio::time::timeout(Duration::from_secs(1), connection).await;
        // Release even on a failing assertion so runtime shutdown cannot hang.
        drop(release);
        blocker.await?;
        let error = match result {
            Ok(Err(error)) => error,
            Ok(Ok(_)) => anyhow::bail!("connection unexpectedly succeeded"),
            Err(_) => anyhow::bail!("connection deadline did not include TLS preparation"),
        };
        assert!(matches!(error.downcast_ref::<ExecServerError>(),
            Some(ExecServerError::WebSocketConnectTimeout { timeout, .. }) if *timeout == deadline));
        assert!(tokio::time::timeout(Duration::from_millis(50), listener.accept()).await.is_err(),
            "an expired TLS preparation must not start a network connection");
        anyhow::Ok(())
    })
}

#[test]
fn websocket_deadline_covers_tls_preparation_without_network_side_effects() -> Result<()> {
    assert_websocket_deadline_covers_tls_preparation(false)
}

#[test]
fn noise_websocket_deadline_covers_tls_preparation_without_network_side_effects() -> Result<()> {
    assert_websocket_deadline_covers_tls_preparation(true)
}
