use super::*;
use pretty_assertions::assert_eq;
use rama_http::StatusCode;
use rama_tls_rustls::dep::pki_types::CertificateDer;
use rama_tls_rustls::dep::pki_types::PrivateKeyDer;
use rama_tls_rustls::dep::pki_types::pem::PemObject;
use rama_tls_rustls::dep::rcgen::BasicConstraints;
use rama_tls_rustls::dep::rcgen::CertificateParams;
use rama_tls_rustls::dep::rcgen::DistinguishedName;
use rama_tls_rustls::dep::rcgen::DnType;
use rama_tls_rustls::dep::rcgen::ExtendedKeyUsagePurpose;
use rama_tls_rustls::dep::rcgen::IsCa;
use rama_tls_rustls::dep::rcgen::Issuer;
use rama_tls_rustls::dep::rcgen::KeyPair;
use rama_tls_rustls::dep::rcgen::KeyUsagePurpose;
use rama_tls_rustls::dep::rcgen::PKCS_ECDSA_P256_SHA256;
use rama_tls_rustls::dep::tokio_rustls::TlsAcceptor;
use std::collections::HashMap;
use std::fs;
use std::sync::Arc;
use std::time::Duration;
use tempfile::tempdir;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::time::timeout;

#[test]
fn proxy_env_uses_first_present_casing_even_when_unusable() {
    let values = HashMap::from([
        ("HTTPS_PROXY", ""),
        ("https_proxy", "http://lower.example:8080"),
    ]);
    assert_eq!(
        read_proxy_env_with(&["HTTPS_PROXY", "https_proxy"], |key| {
            values
                .get(key)
                .map(|value| (*value).to_string())
                .ok_or(std::env::VarError::NotPresent)
        }),
        None
    );

    let values = HashMap::from([
        ("HTTPS_PROXY", "http://["),
        ("https_proxy", "http://lower.example:8080"),
    ]);
    assert_eq!(
        read_proxy_env_with(&["HTTPS_PROXY", "https_proxy"], |key| {
            values
                .get(key)
                .map(|value| (*value).to_string())
                .ok_or(std::env::VarError::NotPresent)
        }),
        None
    );

    let non_unicode = std::ffi::OsString::from("synthetic non-Unicode environment error");
    assert_eq!(
        read_proxy_env_with(&["HTTPS_PROXY", "https_proxy"], |key| {
            if key == "HTTPS_PROXY" {
                Err(std::env::VarError::NotUnicode(non_unicode.clone()))
            } else {
                Ok("http://lower.example:8080".to_string())
            }
        }),
        None
    );
}

#[tokio::test]
async fn direct_client_rejects_an_inherited_proxy_route() {
    let client = UpstreamClient::direct_with_allow_local_binding(
        true,
        Arc::new(rustls::RootCertStore::empty()),
    );
    let mut request = Request::builder()
        .uri("http://127.0.0.1:1/")
        .body(Body::empty())
        .unwrap();
    request
        .extensions_mut()
        .insert(ProxyAddress::try_from("http://127.0.0.1:2").unwrap());
    let error = client
        .serve(request)
        .await
        .expect_err("direct routing must reject proxy overrides");
    assert_eq!(
        error.to_string(),
        "direct upstream request contains a proxy route"
    );
}

fn generate_ca(common_name: &str) -> (String, KeyPair) {
    let mut params = CertificateParams::default();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    let mut distinguished_name = DistinguishedName::new();
    distinguished_name.push(DnType::CommonName, common_name);
    params.distinguished_name = distinguished_name;
    let key_pair = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
    let cert = params.self_signed(&key_pair).unwrap();
    (cert.pem(), key_pair)
}

#[tokio::test]
async fn mitm_upstream_client_trusts_startup_custom_ca() {
    ensure_rustls_crypto_provider();
    let temp_dir = tempdir().unwrap();
    let startup_ca_path = temp_dir.path().join("startup-ca.pem");
    let managed_ca_path = temp_dir.path().join("managed-ca.pem");
    let (startup_ca_pem, startup_ca_key) = generate_ca("startup CA");
    let (managed_ca_pem, _) = generate_ca("managed MITM CA");
    fs::write(&startup_ca_path, &startup_ca_pem).unwrap();
    fs::write(&managed_ca_path, managed_ca_pem).unwrap();

    let issuer = Issuer::from_ca_cert_pem(&startup_ca_pem, startup_ca_key).unwrap();
    let mut server_params = CertificateParams::new(vec!["localhost".to_string()]).unwrap();
    server_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    server_params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    let server_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
    let server_cert = server_params.signed_by(&server_key, &issuer).unwrap();
    let server_cert = CertificateDer::from_pem_slice(server_cert.pem().as_bytes()).unwrap();
    let server_key = PrivateKeyDer::from_pem_slice(server_key.serialize_pem().as_bytes()).unwrap();
    let mut server_config =
        rustls::ServerConfig::builder_with_protocol_versions(rustls::ALL_VERSIONS)
            .with_no_client_auth()
            .with_single_cert(vec![server_cert], server_key)
            .unwrap();
    server_config.alpn_protocols = vec![b"http/1.1".to_vec()];

    let env = HashMap::from([(
        "SSL_CERT_FILE",
        startup_ca_path.to_string_lossy().into_owned(),
    )]);
    let roots =
        crate::certs::upstream_tls_root_store_for_cert_path(&managed_ca_path, &env).unwrap();
    let baseline_roots =
        crate::certs::upstream_tls_root_store_for_cert_path(&managed_ca_path, &HashMap::new())
            .unwrap();
    assert_eq!(roots.len(), baseline_roots.len() + 1);

    let acceptor = TlsAcceptor::from(Arc::new(server_config));
    timeout(Duration::from_secs(10), async {
        for (roots, trusted) in [(baseline_roots, false), (roots, true)] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = async {
                let (stream, _) = listener.accept().await.unwrap();
                let handshake = acceptor.accept(stream).await;
                if !trusted {
                    assert!(
                        handshake.is_err(),
                        "untrusted certificate should abort the handshake"
                    );
                    return;
                }
                let mut stream = handshake.unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    request.push(stream.read_u8().await.unwrap());
                }
                assert!(request.starts_with(b"GET / HTTP/1.1\r\n"));
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .await
                    .unwrap();
            };
            let client = async {
                let client = UpstreamClient::direct_with_allow_local_binding(
                    /*allow_local_binding*/ true, roots,
                );
                let request = Request::builder()
                    .uri(format!("https://localhost:{}/", address.port()))
                    .body(Body::empty())
                    .unwrap();
                let result = client.serve(request).await;
                if trusted {
                    assert_eq!(result.unwrap().status(), StatusCode::OK);
                } else {
                    let err = result.expect_err("baseline roots must reject startup CA");
                    assert!(
                        format!("{err:?}").contains("UnknownIssuer"),
                        "unexpected TLS error: {err:?}"
                    );
                }
            };
            tokio::join!(server, client);
        }
    })
    .await
    .expect("TLS exchanges should finish");
}

#[tokio::test]
async fn request_failure_does_not_include_uri_secrets() {
    timeout(Duration::from_secs(10), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = async {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(stream.read_u8().await.unwrap());
            }
            assert!(request.starts_with(b"GET /private-path?token=query-secret HTTP/1.1\r\n"));
            stream
                .write_all(b"invalid HTTP response\r\n\r\n")
                .await
                .unwrap();
        };
        let client = async {
            let client = UpstreamClient::direct_with_allow_local_binding(
                /*allow_local_binding*/ true,
                Arc::new(rustls::RootCertStore::empty()),
            );
            let request = Request::builder()
                .uri(format!("http://{address}/private-path?token=query-secret"))
                .body(Body::empty())
                .unwrap();
            let err = client
                .serve(request)
                .await
                .expect_err("malformed response must fail");
            let diagnostic = format!("{err:?}");
            assert!(diagnostic.contains(&format!("http request failure for upstream: {address}")));
            assert!(!diagnostic.contains("private-path"));
            assert!(!diagnostic.contains("query-secret"));
        };
        tokio::join!(server, client);
    })
    .await
    .expect("HTTP exchange should finish");
}
