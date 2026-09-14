use super::BindConnectionAttribution;
use super::write_attribution_frame;
use crate::config::NetworkProxyConfig;
use crate::runtime::network_proxy_state_for_policy;
use crate::state::NetworkProxyState;
use pretty_assertions::assert_eq;
use rama_core::Service;
use rama_core::error::BoxError;
use rama_core::extensions::ExtensionsRef;
use rama_core::service::service_fn;
use rama_tcp::TcpStream as RamaTcpStream;
use std::io;
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::net::TcpStream;

#[test]
fn attribution_frame_has_bounded_binary_prefix() -> io::Result<()> {
    let mut frame = Vec::new();
    write_attribution_frame(&mut frame, "token-1")?;

    assert_eq!(&frame[..8], b"\0CDXPXY1");
    assert_eq!(u16::from_be_bytes([frame[8], frame[9]]), 7);
    assert_eq!(&frame[10..], b"token-1");
    Ok(())
}

#[tokio::test]
async fn framed_connection_receives_registered_execution_state() -> Result<(), BoxError> {
    let state = Arc::new(network_proxy_state_for_policy(NetworkProxyConfig::default()));
    state.register_execution("token-1", "local", "execution-1");

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let client = tokio::spawn(async move {
        let mut stream = TcpStream::connect(addr).await?;
        let mut frame = Vec::new();
        write_attribution_frame(&mut frame, "token-1")?;
        stream.write_all(&frame).await
    });

    let (stream, _) = listener.accept().await?;
    let service = BindConnectionAttribution::new(
        service_fn(|stream: RamaTcpStream| async move {
            let state = stream.extensions().get::<Arc<NetworkProxyState>>().cloned();
            Ok::<_, io::Error>(state)
        }),
        state,
        Some("local".to_string()),
    );
    let actual = service
        .serve(RamaTcpStream::new(stream))
        .await?
        .expect("connection state");
    client.await??;

    assert_eq!(actual.environment_id(), Some("local"));
    assert_eq!(actual.execution_id().as_deref(), Some("execution-1"));
    Ok(())
}

#[test]
fn attribution_frame_checks_empty_and_maximum_byte_lengths_before_writing() {
    for token in [
        String::new(),
        "a".repeat(super::MAX_ATTRIBUTION_TOKEN_LEN + 1),
        "é".repeat(65),
    ] {
        let mut frame = Vec::new();
        assert_eq!(
            write_attribution_frame(&mut frame, &token)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert!(
            frame.is_empty(),
            "invalid token must not emit a partial frame"
        );
    }
    let token = "a".repeat(super::MAX_ATTRIBUTION_TOKEN_LEN);
    let mut frame = Vec::new();
    write_attribution_frame(&mut frame, &token).unwrap();
    assert_eq!(u16::from_be_bytes([frame[8], frame[9]]), 128);
    assert_eq!(&frame[10..], token.as_bytes());
}

#[tokio::test]
async fn attribution_rejects_unknown_and_cross_environment_tokens() -> Result<(), BoxError> {
    for (token, expected) in [
        ("unknown", "unknown network proxy attribution token"),
        ("known", "network proxy attribution environment mismatch"),
    ] {
        let state = Arc::new(network_proxy_state_for_policy(NetworkProxyConfig::default()));
        state.register_execution("known", "other", "execution");
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let mut client = TcpStream::connect(listener.local_addr()?).await?;
        let (stream, _) = listener.accept().await?;
        let mut frame = Vec::new();
        write_attribution_frame(&mut frame, token)?;
        client.write_all(&frame).await?;
        let service = BindConnectionAttribution::new(
            service_fn(|_: RamaTcpStream| async {
                panic!("rejected attribution must not reach proxy service");
                #[allow(unreachable_code)]
                Ok::<(), io::Error>(())
            }),
            state,
            Some("local".to_string()),
        );
        let error = service.serve(RamaTcpStream::new(stream)).await.unwrap_err();
        assert!(error.to_string().contains(expected), "{error}");
    }
    Ok(())
}

#[tokio::test]
async fn attribution_leaves_unframed_payload_untouched() -> Result<(), BoxError> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let mut client = TcpStream::connect(listener.local_addr()?).await?;
    let (stream, _) = listener.accept().await?;
    client.write_all(b"GET / HTTP/1.0\r\n\r\n").await?;
    client.shutdown().await?;
    let service = BindConnectionAttribution::new(
        service_fn(|mut stream: RamaTcpStream| async move {
            let mut payload = Vec::new();
            stream.read_to_end(&mut payload).await?;
            Ok::<_, io::Error>(payload)
        }),
        Arc::new(network_proxy_state_for_policy(NetworkProxyConfig::default())),
        None,
    );
    assert_eq!(
        service.serve(RamaTcpStream::new(stream)).await?,
        b"GET / HTTP/1.0\r\n\r\n"
    );
    Ok(())
}

#[tokio::test]
async fn attribution_times_out_idle_and_partial_frames() -> Result<(), BoxError> {
    for prefix in [b"".as_slice(), b"\0CDX".as_slice()] {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let mut client = TcpStream::connect(listener.local_addr()?).await?;
        let (stream, _) = listener.accept().await?;
        client.write_all(prefix).await?;
        let mut stream = RamaTcpStream::new(stream);
        let error = tokio::time::timeout(
            super::ATTRIBUTION_FRAME_TIMEOUT + std::time::Duration::from_secs(2),
            super::read_attribution_token(&mut stream),
        )
        .await
        .expect("parser must terminate")
        .unwrap_err();
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::TimedOut
        );
    }
    Ok(())
}
