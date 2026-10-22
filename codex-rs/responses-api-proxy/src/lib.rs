use std::fs::File;
use std::fs::{self};
use std::io::Read;
use std::io::Write;
use std::net::SocketAddr;
use std::net::TcpListener;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use clap::Parser;
use codex_http_client::BlockingHttpClient;
use codex_http_client::BlockingHttpClientBuilder;
use http::header::AUTHORIZATION;
use http::header::HOST;
use http::HeaderMap;
use http::HeaderName;
use http::HeaderValue;
use serde::Serialize;
use tiny_http::Header;
use tiny_http::Method;
use tiny_http::Request;
use tiny_http::Response;
use tiny_http::Server;
use tiny_http::StatusCode;
use url::Url;

mod dump;
mod read_api_key;
use dump::ExchangeDumper;
use read_api_key::read_auth_header_from_stdin;

/// CLI arguments for the proxy.
#[derive(Debug, Clone, Parser)]
#[command(name = "responses-api-proxy", about = "Minimal OpenAI responses proxy")]
pub struct Args {
    /// Port to listen on. If not set, an ephemeral port is used.
    #[arg(long)]
    pub port: Option<u16>,

    /// Path to a JSON file to write startup info (single line). Includes {"port": <u16>}.
    #[arg(long, value_name = "FILE")]
    pub server_info: Option<PathBuf>,

    /// Enable HTTP shutdown endpoint at GET /shutdown
    #[arg(long)]
    pub http_shutdown: bool,

    /// Absolute URL the proxy should forward requests to (defaults to OpenAI).
    #[arg(long, default_value = "https://api.openai.com/v1/responses")]
    pub upstream_url: String,

    /// Directory where request/response dumps should be written as JSON.
    #[arg(long, value_name = "DIR")]
    pub dump_dir: Option<PathBuf>,

    /// Maximum admitted forwarding workers; excess requests receive 503.
    #[arg(long, default_value_t = 16, value_parser = clap::value_parser!(u16).range(1..))]
    pub max_in_flight: u16,

    /// Maximum buffered upload bytes, including chunked request bodies.
    #[arg(long, default_value_t = 134_217_728, value_parser = clap::value_parser!(u64).range(1..))]
    pub max_request_bytes: u64,
}

#[derive(Serialize)]
struct ServerInfo {
    port: u16,
    pid: u32,
}

struct ForwardConfig {
    upstream_url: Url,
    host_header: HeaderValue,
    max_request_bytes: u64,
}

struct ForwardPermit(Arc<AtomicUsize>);

impl ForwardPermit {
    fn try_acquire(active: &Arc<AtomicUsize>, limit: usize) -> Option<Self> {
        active.try_update(Ordering::AcqRel, Ordering::Acquire,
            |count| (count < limit).then_some(count + 1)).ok()?;
        Some(Self(Arc::clone(active)))
    }
}

impl Drop for ForwardPermit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn read_bounded_upload(reader: &mut dyn Read, limit: u64) -> std::io::Result<Option<Vec<u8>>> {
    let mut body = Vec::new();
    reader.take(limit.saturating_add(1)).read_to_end(&mut body)?;
    Ok((body.len() as u64 <= limit).then_some(body))
}

/// Entry point for the library main, for parity with other crates.
pub fn run_main(args: Args) -> Result<()> {
    let auth_header = read_auth_header_from_stdin()?;

    let upstream_url = Url::parse(&args.upstream_url).context("parsing --upstream-url")?;
    let host = match (upstream_url.host_str(), upstream_url.port()) {
        (Some(host), Some(port)) => format!("{host}:{port}"),
        (Some(host), None) => host.to_string(),
        _ => return Err(anyhow!("upstream URL must include a host")),
    };
    let host_header =
        HeaderValue::from_str(&host).context("constructing Host header from upstream URL")?;

    let forward_config = Arc::new(ForwardConfig {
        upstream_url,
        host_header,
        max_request_bytes: args.max_request_bytes,
    });
    let dump_dir = args
        .dump_dir
        .map(ExchangeDumper::new)
        .transpose()
        .context("creating --dump-dir")?
        .map(Arc::new);

    let (listener, bound_addr) = bind_listener(args.port)?;
    if let Some(path) = args.server_info.as_ref() {
        write_server_info(path, bound_addr.port())?;
    }
    let server = Server::from_listener(listener, None)
        .map_err(|err| anyhow!("creating HTTP server: {err}"))?;
    let client = Arc::new(
        BlockingHttpClientBuilder::new()
            // Keep long-lived response streams independent of the connection deadline.
            .request_timeout(None)
            .connect_timeout(Duration::from_secs(30))
            .build_with_transport_default_proxy()
            .context("building HTTP client")?,
    );

    eprintln!("responses-api-proxy listening on {bound_addr}");

    let http_shutdown = args.http_shutdown;
    let active = Arc::new(AtomicUsize::new(0));
    for request in server.incoming_requests() {
        // Keep shutdown reachable even when every forwarding slot is stalled.
        if http_shutdown && request.method() == &Method::Get && request.url() == "/shutdown" {
            let _ = request.respond(Response::new_empty(StatusCode(200)));
            std::process::exit(0);
        }
        let Some(permit) = ForwardPermit::try_acquire(&active, usize::from(args.max_in_flight)) else {
            let _ = request.respond(Response::new_empty(StatusCode(503)));
            continue;
        };
        let client = client.clone();
        let forward_config = forward_config.clone();
        let dump_dir = dump_dir.clone();
        std::thread::spawn(move || {
            let _permit = permit;

            if let Err(e) = forward_request(
                &client,
                auth_header,
                &forward_config,
                dump_dir.as_deref(),
                request,
            ) {
                eprintln!("forwarding error: {e}");
            }
        });
    }

    Err(anyhow!("server stopped unexpectedly"))
}

fn bind_listener(port: Option<u16>) -> Result<(TcpListener, SocketAddr)> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port.unwrap_or(0)));
    let listener = TcpListener::bind(addr).with_context(|| format!("failed to bind {addr}"))?;
    let bound = listener.local_addr().context("failed to read local_addr")?;
    Ok((listener, bound))
}

fn write_server_info(path: &Path, port: u16) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }

    let info = ServerInfo {
        port,
        pid: std::process::id(),
    };
    let mut data = serde_json::to_string(&info)?;
    data.push('\n');
    let mut f = File::create(path)?;
    f.write_all(data.as_bytes())?;
    Ok(())
}

fn forward_request(
    client: &BlockingHttpClient,
    auth_header: &'static str,
    config: &ForwardConfig,
    dump_dir: Option<&ExchangeDumper>,
    mut req: Request,
) -> Result<()> {
    // Only allow POST /v1/responses exactly, no query string.
    let method = req.method().clone();
    let url_path = req.url().to_string();
    let allow = method == Method::Post && url_path == "/v1/responses";

    if !allow {
        let resp = Response::new_empty(StatusCode(403));
        let _ = req.respond(resp);
        return Ok(());
    }

    if req
        .body_length()
        .is_some_and(|length| length as u64 > config.max_request_bytes)
    {
        let _ = req.respond(Response::new_empty(StatusCode(413)));
        return Ok(());
    }
    let Some(body) = read_bounded_upload(req.as_reader(), config.max_request_bytes)? else {
        let _ = req.respond(Response::new_empty(StatusCode(413)));
        return Ok(());
    };

    let request_connection_fields = connection_fields(
        req.headers()
            .iter()
            .filter(|header| header.field.equiv("connection"))
            .map(|header| header.value.as_str()),
    );
    let incoming_headers = req
        .headers()
        .iter()
        .filter(|header| {
            !is_hop_header(
                header.field.as_str().to_ascii_lowercase().as_str(),
                &request_connection_fields,
            )
        })
        .cloned()
        .collect::<Vec<_>>();
    let exchange_dump = dump_dir.and_then(|dump_dir| {
        dump_dir
            .dump_request(&method, &url_path, &incoming_headers, &body)
            .map_err(|err| {
                eprintln!("responses-api-proxy failed to dump request: {err}");
                err
            })
            .ok()
    });

    // Forward only end-to-end fields, replacing origin authentication below.
    let mut headers = HeaderMap::new();
    for header in &incoming_headers {
        let name_ascii = header.field.as_str();
        let lower = name_ascii.to_ascii_lowercase();
        if lower.as_str() == "authorization" || lower.as_str() == "host" {
            continue;
        }

        let header_name = match HeaderName::from_bytes(lower.as_bytes()) {
            Ok(name) => name,
            Err(_) => continue,
        };
        if let Ok(value) = HeaderValue::from_bytes(header.value.as_bytes()) {
            headers.append(header_name, value);
        }
    }

    // As part of our effort to to keep `auth_header` secret, we use a
    // combination of `from_static()` and `set_sensitive(true)`.
    let mut auth_header_value = HeaderValue::from_static(auth_header);
    auth_header_value.set_sensitive(true);
    headers.insert(AUTHORIZATION, auth_header_value);

    headers.insert(HOST, config.host_header.clone());

    let upstream_resp = client
        .post(config.upstream_url.as_str())
        .headers(headers)
        .body(body)
        .send()
        .context("forwarding request to upstream")?;

    // The HTTP facade response implements Read, so forwarding remains streaming.
    let status = upstream_resp.status();
    let connection_fields = connection_fields(
        upstream_resp
            .headers()
            .get_all("connection")
            .iter()
            .filter_map(|value| value.to_str().ok()),
    );
    let mut response_headers = Vec::new();
    for (name, value) in upstream_resp.headers().iter() {
        // Skip headers that tiny_http manages itself.
        if is_hop_header(name.as_str(), &connection_fields) {
            continue;
        }

        if let Ok(header) = Header::from_bytes(name.as_str().as_bytes(), value.as_bytes()) {
            response_headers.push(header);
        }
    }

    let content_length = upstream_resp.content_length().and_then(|len| {
        if len <= usize::MAX as u64 {
            Some(len as usize)
        } else {
            None
        }
    });

    let response_body: Box<dyn Read + Send> = if let Some(exchange_dump) = exchange_dump {
        let headers = upstream_resp.headers().clone();
        Box::new(exchange_dump.tee_response_body(status.as_u16(), &headers, upstream_resp))
    } else {
        Box::new(upstream_resp)
    };

    let response = Response::new(
        StatusCode(status.as_u16()),
        response_headers,
        response_body,
        content_length,
        None,
    );

    let _ = req.respond(response);
    Ok(())
}

fn connection_fields<'a>(values: impl Iterator<Item = &'a str>) -> Vec<String> {
    values
        .flat_map(|value| value.split(','))
        .map(|field| field.trim().to_ascii_lowercase())
        .collect()
}

fn is_hop_header(name: &str, connection_fields: &[String]) -> bool {
    matches!(
        name,
        "connection"
            | "proxy-connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "content-length"
    ) || connection_fields
        .iter()
        .any(|field| field.eq_ignore_ascii_case(name))
}

#[cfg(test)]
mod admission_tests {
    use super::*;

    #[test]
    fn forwarding_removes_credentials_and_all_connection_nominated_fields() {
        let origin = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin_address = origin.local_addr().unwrap();
        let proxy = Server::http("127.0.0.1:0").unwrap();
        let proxy_address = proxy.server_addr().to_ip().unwrap();
        let origin_task = std::thread::spawn(move || {
            let (mut stream, _) = origin.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut headers = Vec::new();
            while !headers.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                headers.push(byte[0]);
            }
            let headers = String::from_utf8(headers).unwrap().to_ascii_lowercase();
            assert!(!headers.contains("proxy-authorization:"));
            assert!(!headers.contains("x-hop-private:"));
            assert!(!headers.contains("x-second-hop:"));
            assert!(headers.contains("authorization: bearer origin-secret\r\n"));
            stream.read_exact(&mut [0; 2]).unwrap();
            // tiny_http discards Connection on responses; write it on the wire so this
            // fixture exercises response-nominated header sanitation too.
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: x-response-private, close\r\nx-response-private: hidden\r\n\r\nok").unwrap();
        });
        let proxy_task = std::thread::spawn(move || {
            let client = BlockingHttpClientBuilder::new().build_direct().unwrap();
            let request = proxy.recv_timeout(Duration::from_secs(2)).unwrap().unwrap();
            forward_request(
                &client,
                "Bearer origin-secret",
                &ForwardConfig {
                    upstream_url: Url::parse(&format!("http://{origin_address}/v1/responses"))
                        .unwrap(),
                    host_header: HeaderValue::from_str(&origin_address.to_string()).unwrap(),
                    max_request_bytes: 1024,
                },
                None,
                request,
            )
            .unwrap();
        });
        let mut headers = HeaderMap::new();
        headers.insert(
            "proxy-authorization",
            HeaderValue::from_static("Basic caller-secret"),
        );
        headers.append("connection", HeaderValue::from_static("X-Hop-Private"));
        headers.append("connection", HeaderValue::from_static(" x-second-hop"));
        headers.insert("x-hop-private", HeaderValue::from_static("private"));
        headers.insert("x-second-hop", HeaderValue::from_static("private"));
        let response = BlockingHttpClientBuilder::new()
            .build_direct()
            .unwrap()
            .post(format!("http://{proxy_address}/v1/responses"))
            .headers(headers)
            .body(b"{}".to_vec())
            .send()
            .unwrap();
        assert_eq!(response.status(), http::StatusCode::OK);
        assert!(!response.headers().contains_key("x-response-private"));
        assert_eq!(response.text().unwrap(), "ok");
        proxy_task.join().unwrap();
        origin_task.join().unwrap();
    }

    #[test]
    fn blocked_worker_retains_one_slot_without_unbounded_admission() {
        let active = Arc::new(AtomicUsize::new(0));
        let permit = ForwardPermit::try_acquire(&active, 1).unwrap();
        let (release, wait) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let _permit = permit;
            wait.recv_timeout(Duration::from_secs(5)).unwrap();
        });
        for _ in 0..1000 { assert!(ForwardPermit::try_acquire(&active, 1).is_none()); }
        assert_eq!(active.load(Ordering::Acquire), 1);
        release.send(()).unwrap();
        worker.join().unwrap();
        assert!(ForwardPermit::try_acquire(&active, 1).is_some());
        assert_eq!(active.load(Ordering::Acquire), 0);
    }

    #[test]
    fn upload_limit_accepts_exact_boundary_and_reads_only_one_excess_byte() {
        let mut exact = std::io::Cursor::new(b"12345678");
        assert_eq!(read_bounded_upload(&mut exact, 8).unwrap(), Some(b"12345678".to_vec()));
        let mut excess = std::io::Cursor::new(b"123456789abcdef");
        assert!(read_bounded_upload(&mut excess, 8).unwrap().is_none());
        assert_eq!(excess.position(), 9);
    }
}
