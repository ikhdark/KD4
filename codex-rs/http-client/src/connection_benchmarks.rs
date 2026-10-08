//! Opt-in loopback measurements; timings are diagnostic, connection counts are assertions.
use super::*;
use pretty_assertions::assert_eq;

trait ReadWrite: Read + Write {}
impl<T: Read + Write> ReadWrite for T {}

fn serve_requests(
    listener: TcpListener,
    tls: Option<Arc<rustls::ServerConfig>>,
    requests: usize,
) -> usize {
    let mut served = 0;
    let mut connections = 0;
    let deadline = Instant::now() + Duration::from_secs(30);
    while served < requests {
        let (socket, _) = loop {
            match listener.accept() {
                Ok(socket) => break socket,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "benchmark server timed out");
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("accept: {error}"),
            }
        };
        socket.set_nonblocking(false).unwrap();
        socket.set_nodelay(true).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut stream: Box<dyn ReadWrite> = match tls.as_ref() {
            Some(config) => Box::new(rustls::StreamOwned::new(
                rustls::ServerConnection::new(Arc::clone(config)).unwrap(),
                socket,
            )),
            None => Box::new(socket),
        };
        connections += 1;
        while served < requests {
            let mut header = Vec::new();
            let mut byte = [0];
            while !header.ends_with(b"\r\n\r\n") {
                match stream.read(&mut byte) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => header.push(byte[0]),
                }
                assert!(header.len() < 16384);
            }
            if !header.ends_with(b"\r\n\r\n") {
                break;
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                .unwrap();
            stream.flush().unwrap();
            served += 1;
        }
    }
    connections
}

#[tokio::test]
#[ignore = "narrow loopback benchmark; run explicitly with --run-ignored only"]
async fn connection_reuse_benchmark() {
    codex_utils_rustls_provider::ensure_rustls_crypto_provider();
    let cert =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()])
            .unwrap();
    let pem = cert.cert.pem();
    let tls = Arc::new(
        rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.cert.der().clone()],
                rustls_pki_types::PrivateKeyDer::Pkcs8(cert.signing_key.serialize_der().into()),
            )
            .unwrap(),
    );
    const REQUESTS: usize = 24;
    let mut report = String::new();
    for secure in [false, true] {
        for mode in ["fresh", "clone", "pool"] {
            let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            listener.set_nonblocking(true).unwrap();
            let port = listener.local_addr().unwrap().port();
            let server_tls = secure.then(|| Arc::clone(&tls));
            let server = std::thread::spawn(move || serve_requests(listener, server_tls, REQUESTS));
            let url = format!(
                "{}://127.0.0.1:{port}/responses",
                if secure { "https" } else { "http" }
            );
            let builder = HttpClientBuilder::new()
                .tls_certs_only_pem(pem.as_bytes())
                .unwrap()
                .timeout(Duration::from_secs(5));
            let pool = RouteAwareClientPool::with_builder(
                HttpClientFactory::new(OutboundProxyPolicy::RespectSystemProxy),
                ClientRouteClass::Api,
                builder.clone(),
            );
            let shared = builder.clone().build_direct().unwrap();
            let mut elapsed = Vec::new();
            for _ in 0..REQUESTS {
                let start = Instant::now();
                let client = match mode {
                    "fresh" => builder.clone().build_direct().unwrap(),
                    "clone" => shared.clone(),
                    _ => {
                        pool.client_for_url_with_resolver(&url, |_| async {
                            Ok(OutboundProxyRoute::Direct)
                        })
                        .await
                        .unwrap()
                        .1
                    }
                };
                assert_eq!(
                    client
                        .get(&url)
                        .send()
                        .await
                        .unwrap()
                        .bytes()
                        .await
                        .unwrap(),
                    "ok"
                );
                elapsed.push(start.elapsed().as_micros());
            }
            let connections = server.join().unwrap();
            let cold_us = elapsed[0];
            elapsed.remove(0);
            elapsed.sort_unstable();
            let row = format!(
                "connection_benchmark tls={secure} mode={mode} requests={REQUESTS} connections={connections} first_us={cold_us} warm_median_us={} warm_p95_us={}\n",
                elapsed[elapsed.len() / 2],
                elapsed[elapsed.len() * 95 / 100]
            );
            eprint!("{row}");
            report.push_str(&row);
            assert_eq!(connections, if mode == "fresh" { REQUESTS } else { 1 });
        }
    }
    if let Some(path) =
        std::env::var_os("KD4_CONNECTION_BENCHMARK_OUTPUT").filter(|path| !path.is_empty())
    {
        std::fs::write(path, report).unwrap();
    }
}

#[test]
#[ignore = "narrow proxy key representation benchmark"]
fn proxy_key_representation_benchmark() {
    use sha2::Digest;
    use sha2::Sha256;
    use std::hint::black_box;
    let url = "https://provider.example/v1/responses?api-version=2026-01-01";
    let mut report = String::new();
    for hex in [true, false] {
        let mut samples = Vec::new();
        for _ in 0..7 {
            let start = Instant::now();
            for _ in 0..20_000 {
                let mut hash = Sha256::new();
                hash.update(b"system-proxy-cache-v1\0");
                hash.update(black_box(url).as_bytes());
                if hex {
                    black_box(format!("{:x}", hash.finalize()));
                } else {
                    let key: [u8; 32] = hash.finalize().into();
                    black_box(key);
                }
            }
            samples.push(start.elapsed().as_nanos() / 20_000);
        }
        samples.sort_unstable();
        report.push_str(&format!(
            "proxy_key_benchmark hex={hex} samples=7 iterations=20000 median_ns={}\n",
            samples[3]
        ));
    }
    eprint!("{report}");
    if let Some(path) =
        std::env::var_os("KD4_PROXY_KEY_BENCHMARK_OUTPUT").filter(|path| !path.is_empty())
    {
        std::fs::write(path, report).unwrap();
    }
}
