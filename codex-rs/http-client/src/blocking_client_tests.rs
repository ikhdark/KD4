use std::io::Read;
use std::io::Write;
use std::net::TcpListener;
use std::thread;

use http::HeaderMap;
use http::HeaderValue;
use http::StatusCode;
use pretty_assertions::assert_eq;

use super::BlockingHttpClientBuilder;
use crate::BuildCustomCaTransportError;
use crate::custom_ca::CustomCaPolicy;

#[test]
fn exclusive_tls_roots_select_explicit_root_policy() {
    let ca_pem = include_bytes!("../tests/fixtures/test-ca.pem");

    let client = BlockingHttpClientBuilder::new()
        .tls_certs_only_pem(ca_pem)
        .expect("valid CA certificate")
        .build_inner_using(/*direct*/ false, |builder, custom_ca_policy| {
            assert_eq!(custom_ca_policy, CustomCaPolicy::ExplicitRootSet);
            builder
                .build()
                .map_err(BuildCustomCaTransportError::BuildClientWithExplicitRoots)
        });

    assert!(client.is_ok());
}

#[test]
fn blocking_client_streams_request_and_response_without_exposing_transport_types() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback server");
    let address = listener.local_addr().expect("read loopback address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept request");
        let mut request = Vec::new();
        let mut chunk = [0_u8; 1024];
        loop {
            let bytes_read = stream.read(&mut chunk).expect("read request");
            assert_ne!(bytes_read, 0, "request ended before its body arrived");
            request.extend_from_slice(&chunk[..bytes_read]);
            let Some(header_end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
                continue;
            };
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|value| value.trim().parse::<usize>().ok())
                })
                .expect("request content length");
            if request.len() >= header_end + 4 + content_length {
                break;
            }
        }
        stream
            .write_all(
                b"HTTP/1.1 201 Created\r\ncontent-type: text/plain\r\ncontent-length: 7\r\nconnection: close\r\n\r\ncreated",
            )
            .expect("write response");
        String::from_utf8_lossy(&request).into_owned()
    });

    let mut headers = HeaderMap::new();
    headers.insert("x-codex-test", HeaderValue::from_static("blocking"));
    let client = BlockingHttpClientBuilder::new()
        .build_direct()
        .expect("build blocking client");
    let mut response = client
        .post(format!("http://{address}/upload"))
        .headers(headers)
        .body(b"payload".to_vec())
        .send()
        .expect("send request");

    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(response.content_length(), Some(7));
    assert_eq!(
        response.headers().get("content-type"),
        Some(&HeaderValue::from_static("text/plain"))
    );
    let mut body = String::new();
    response.read_to_string(&mut body).expect("read response");
    assert_eq!(body, "created");

    let request = server.join().expect("join server");
    assert!(request.starts_with("POST /upload HTTP/1.1\r\n"));
    assert!(
        request
            .to_ascii_lowercase()
            .contains("x-codex-test: blocking")
    );
    assert!(request.ends_with("payload"));
}
