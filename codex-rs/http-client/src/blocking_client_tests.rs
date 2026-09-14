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
fn blocking_client_sends_buffered_request_and_reads_response_without_exposing_transport_types() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback server");
    let address = listener.local_addr().expect("read loopback address");
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    let server = thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let (mut stream, _) = loop {
            match listener.accept() {
                Ok(connection) => break connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(std::time::Instant::now() < deadline, "request must arrive");
                    thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(error) => panic!("accept request: {error}"),
            }
        };
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        stream
            .set_write_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
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
        .timeout(std::time::Duration::from_secs(2))
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

#[test]
fn explicit_none_disables_blocking_transport_timeout() {
    // Reqwest's blocking default is 30 seconds. Only a response beyond that boundary
    // distinguishes an explicit None from accidentally leaving the default in place.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind server");
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let server = thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let (mut stream, _) = loop {
            match listener.accept() {
                Ok(connection) => break connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(std::time::Instant::now() < deadline, "request must arrive");
                    thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(error) => panic!("accept request: {error}"),
            }
        };
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        stream
            .set_write_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            stream.read_exact(&mut byte).unwrap();
            request.push(byte[0]);
        }
        thread::sleep(std::time::Duration::from_secs(31));
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
    });
    let result = BlockingHttpClientBuilder::new()
        .request_timeout(None)
        .build_direct()
        .expect("build without timeout")
        .get(format!("http://{address}/"))
        .send();
    server
        .join()
        .expect("server completes")
        .expect("response written");
    assert_eq!(
        result
            .expect("explicit None permits a response after 30 seconds")
            .status(),
        StatusCode::OK
    );
}
