use std::fs::{self};
use std::io::Read;
use std::io::Write;
use std::net::SocketAddr;
use std::net::TcpListener;
use std::num::NonZeroUsize;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use clap::Parser;
use codex_http_client::BlockingHttpClient;
use codex_http_client::BlockingHttpClientBuilder;
use http::HeaderMap;
use http::HeaderName;
use http::HeaderValue;
use http::Uri;
use http::header::AUTHORIZATION;
use http::header::HOST;
use serde::Serialize;
use tiny_http::Header;
use tiny_http::Method;
use tiny_http::Request;
use tiny_http::Response;
use tiny_http::Server;
use tiny_http::StatusCode;

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

    /// Maximum concurrent forwarded requests; excess requests receive HTTP 503.
    #[arg(long, default_value = "64")]
    pub max_in_flight: NonZeroUsize,

    /// Maximum accepted request body in bytes; larger requests receive HTTP 413.
    #[arg(long, default_value = "67108864", value_parser = clap::value_parser!(u64).range(1..u64::MAX))]
    pub max_body_bytes: u64,
}

#[derive(Serialize)]
struct ServerInfo {
    port: u16,
    pid: u32,
}

#[derive(Debug)]
struct ForwardConfig {
    upstream_url: String,
    host_header: HeaderValue,
}

/// Entry point for the library main, for parity with other crates.
pub fn run_main(args: Args) -> Result<()> {
    let auth_header = read_auth_header_from_stdin()?;

    let forward_config = Arc::new(parse_forward_config(args.upstream_url)?);
    let dump_dir = args
        .dump_dir
        .map(ExchangeDumper::new)
        .transpose()
        .context("creating --dump-dir")?
        .map(Arc::new);

    let (listener, bound_addr) = bind_listener(args.port)?;
    let server = Server::from_listener(listener, None)
        .map_err(|err| anyhow!("creating HTTP server: {err}"))?;
    let client = Arc::new(
        BlockingHttpClientBuilder::new()
            // Disable the transport's default timeout so long-lived response streams keep flowing.
            .request_timeout(None)
            .build_with_transport_default_proxy()
            .context("building HTTP client")?,
    );
    if let Some(path) = args.server_info.as_ref() {
        write_server_info(path, bound_addr.port())?;
    }

    eprintln!("responses-api-proxy listening on {bound_addr}");

    let http_shutdown = args.http_shutdown;
    let in_flight = Arc::new(AtomicUsize::new(0));
    for request in server.incoming_requests() {
        if http_shutdown && request.method() == &Method::Get && request.url() == "/shutdown" {
            request.respond(Response::new_empty(StatusCode(200)))?;
            std::process::exit(0);
        }
        let Some(permit) = InFlightPermit::acquire(&in_flight, args.max_in_flight.get()) else {
            if let Err(err) = request.respond(Response::new_empty(StatusCode(503))) {
                eprintln!("writing overload response: {err}");
            }
            continue;
        };
        let client = client.clone();
        let forward_config = forward_config.clone();
        let dump_dir = dump_dir.clone();
        std::thread::Builder::new()
            .spawn(move || {
                let _permit = permit;
                if let Err(e) = forward_request(
                    &client,
                    auth_header,
                    &forward_config,
                    dump_dir.as_deref(),
                    args.max_body_bytes,
                    request,
                ) {
                    eprintln!("forwarding error: {e}");
                }
            })
            .context("spawning forwarding worker")?;
    }

    Err(anyhow!("server stopped unexpectedly"))
}

struct InFlightPermit(Arc<AtomicUsize>);

impl InFlightPermit {
    fn acquire(active: &Arc<AtomicUsize>, limit: usize) -> Option<Self> {
        active
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                (count < limit).then(|| count + 1)
            })
            .ok()?;
        Some(Self(Arc::clone(active)))
    }
}

impl Drop for InFlightPermit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

fn parse_forward_config(upstream_url: String) -> Result<ForwardConfig> {
    let upstream_uri = upstream_url
        .parse::<Uri>()
        .context("parsing --upstream-url")?;
    if !matches!(upstream_uri.scheme_str(), Some("http") | Some("https")) {
        return Err(anyhow!("upstream URL must use http or https"));
    }
    let host = match (upstream_uri.host(), upstream_uri.port_u16()) {
        (Some(host), Some(port)) => format!("{host}:{port}"),
        (Some(host), None) => host.to_string(),
        _ => return Err(anyhow!("upstream URL must include a host")),
    };
    let host_header =
        HeaderValue::from_str(&host).context("constructing Host header from upstream URL")?;

    Ok(ForwardConfig {
        upstream_url,
        host_header,
    })
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
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let builder = tempfile::Builder::new();
    #[cfg(unix)]
    let mut builder = builder;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Match File::create permissions, including the process umask, so a
        // privileged proxy can publish readiness for its unprivileged caller.
        builder.permissions(fs::Permissions::from_mode(0o666));
    }
    let mut f = builder.tempfile_in(parent)?;
    if let Ok(metadata) = fs::metadata(path) {
        f.as_file().set_permissions(metadata.permissions())?;
    }
    f.write_all(data.as_bytes())?;
    f.persist(path)?;
    Ok(())
}

fn forward_request(
    client: &BlockingHttpClient,
    auth_header: &'static str,
    config: &ForwardConfig,
    dump_dir: Option<&ExchangeDumper>,
    max_body_bytes: u64,
    mut req: Request,
) -> Result<()> {
    // Only allow POST /v1/responses exactly, no query string.
    let method = req.method().clone();
    let url_path = req.url().to_string();
    let allow = method == Method::Post && url_path == "/v1/responses";

    if !allow {
        let resp = Response::new_empty(StatusCode(403));
        return req.respond(resp).context("writing route rejection");
    }

    // Read request body
    let mut body = Vec::new();
    if req
        .body_length()
        .is_some_and(|length| length as u64 > max_body_bytes)
    {
        return req
            .respond(Response::new_empty(StatusCode(413)))
            .context("writing body size rejection");
    }
    if let Err(err) = req
        .as_reader()
        .take(max_body_bytes + 1)
        .read_to_end(&mut body)
    {
        req.respond(Response::new_empty(StatusCode(400)))
            .context("writing body read failure")?;
        return Err(err).context("reading request body");
    }
    if body.len() as u64 > max_body_bytes {
        return req
            .respond(Response::new_empty(StatusCode(413)))
            .context("writing body size rejection");
    }

    let exchange_dump = dump_dir.and_then(|dump_dir| {
        dump_dir
            .dump_request(&method, &url_path, req.headers(), &body)
            .map_err(|err| {
                eprintln!("responses-api-proxy failed to dump request: {err}");
                err
            })
            .ok()
    });

    // Reconstruct framing and strip connection-local fields before forwarding.
    let request_connection_fields = connection_fields(
        req.headers()
            .iter()
            .filter(|header| header.field.equiv("connection"))
            .map(|header| header.value.as_str()),
    );
    let mut headers = HeaderMap::new();
    for header in req.headers() {
        let name_ascii = header.field.as_str();
        let lower = name_ascii.to_ascii_lowercase();
        if lower.as_str() == "authorization"
            || lower.as_str() == "host"
            || is_connection_header(lower.as_str(), &request_connection_fields)
        {
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

    let upstream_resp = match client
        .post(&config.upstream_url)
        .headers(headers)
        .body(body)
        .send()
    {
        Ok(response) => response,
        Err(err) => {
            req.respond(Response::new_empty(StatusCode(502)))
                .context("writing gateway failure")?;
            return Err(err).context("forwarding request to upstream");
        }
    };

    // The shared blocking response implements `Read`, so it can be used directly
    // as the body of the `tiny_http::Response`.
    let status = upstream_resp.status();
    let mut response_headers = Vec::new();
    let response_connection_fields = connection_fields(
        upstream_resp
            .headers()
            .get_all(http::header::CONNECTION)
            .iter()
            .filter_map(|value| value.to_str().ok()),
    );
    for (name, value) in upstream_resp.headers().iter() {
        // Skip headers that tiny_http manages itself.
        if is_connection_header(name.as_str(), &response_connection_fields) {
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

    req.respond(response).context("writing upstream response")
}

fn connection_fields<'a>(values: impl Iterator<Item = &'a str>) -> Vec<String> {
    values
        .flat_map(|value| value.split(','))
        .map(|field| field.trim().to_ascii_lowercase())
        .collect()
}

fn is_connection_header(name: &str, connection_fields: &[String]) -> bool {
    matches!(
        name,
        "content-length"
            | "transfer-encoding"
            | "connection"
            | "trailer"
            | "upgrade"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
    ) || connection_fields.iter().any(|field| field == name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpStream;
    use std::time::Duration;

    fn proxy_exchange(
        upstream: String,
        body: &[u8],
        max_body_bytes: u64,
        extra_headers: &str,
    ) -> (String, Result<()>) {
        let server = Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_ip().unwrap();
        let worker = std::thread::spawn(move || {
            let client = BlockingHttpClientBuilder::new()
                .timeout(Duration::from_secs(5))
                .build_direct()
                .unwrap();
            let config = parse_forward_config(upstream).unwrap();
            let request = server
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            forward_request(
                &client,
                "Bearer test-key",
                &config,
                None,
                max_body_bytes,
                request,
            )
        });
        let mut stream = TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        write!(stream, "POST /v1/responses HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n{extra_headers}\r\n", body.len()).unwrap();
        stream.write_all(body).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        (response, worker.join().unwrap())
    }

    #[test]
    fn forwarding_strips_connection_headers_and_preserves_body_and_auth() {
        use std::io::BufRead;
        let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = upstream.local_addr().unwrap();
        let upstream_worker = std::thread::spawn(move || {
            let (mut stream, _) = upstream.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = std::io::BufReader::new(&mut stream);
            let mut headers = String::new();
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).unwrap() > 0);
                if line == "\r\n" {
                    break;
                }
                headers.push_str(&line);
            }
            let mut body = [0; 2];
            reader.read_exact(&mut body).unwrap();
            assert_eq!(&body, b"{}");
            let headers = headers.to_ascii_lowercase();
            assert!(headers.contains("authorization: bearer test-key\r\n"));
            assert!(headers.contains("x-end-to-end: retained\r\n"));
            assert!(!headers.contains("x-private-hop:"));
            assert!(!headers.contains("proxy-authorization:"));
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close, x-upstream-hop\r\nx-upstream-hop: secret\r\nx-end-to-end: retained\r\n\r\nOK").unwrap();
        });
        let (response, result) = proxy_exchange(
            format!("http://{addr}/v1/responses"),
            b"{}",
            2,
            "Connection: x-private-hop\r\nx-private-hop: secret\r\nProxy-Authorization: secret\r\nAuthorization: ignored\r\nx-end-to-end: retained\r\n",
        );
        result.unwrap();
        upstream_worker.join().unwrap();
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.ends_with("OK"), "{response}");
        assert!(
            response
                .to_ascii_lowercase()
                .contains("x-end-to-end: retained")
        );
        assert!(!response.to_ascii_lowercase().contains("x-upstream-hop:"));
    }

    #[test]
    fn oversized_request_is_rejected_before_upstream_contact() {
        let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1/responses", upstream.local_addr().unwrap());
        let (response, result) = proxy_exchange(url, b"large", 4, "");
        result.unwrap();
        assert!(response.starts_with("HTTP/1.1 413"), "{response}");
        upstream.set_nonblocking(true).unwrap();
        assert_eq!(
            upstream.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn upstream_failure_returns_explicit_bad_gateway() {
        let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1/responses", upstream.local_addr().unwrap());
        drop(upstream);
        let (response, result) = proxy_exchange(url, b"{}", 2, "");
        assert!(response.starts_with("HTTP/1.1 502"), "{response}");
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("forwarding request to upstream")
        );
    }

    #[test]
    fn in_flight_permits_bound_work_and_release_capacity() {
        let active = Arc::new(AtomicUsize::new(0));
        let permit = InFlightPermit::acquire(&active, 1).unwrap();
        assert!(InFlightPermit::acquire(&active, 1).is_none());
        drop(permit);
        let permit = InFlightPermit::acquire(&active, 1).expect("released capacity");
        assert_eq!(active.load(Ordering::Relaxed), 1);
        drop(permit);
        assert_eq!(active.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn server_info_replaces_existing_file_with_complete_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server-info.json");
        fs::write(&path, "previous").unwrap();
        write_server_info(&path, 1234).unwrap();
        let info: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(
            info,
            serde_json::json!({"port": 1234, "pid": std::process::id()})
        );
    }

    #[cfg(unix)]
    #[test]
    fn server_info_preserves_normal_creation_and_existing_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let reference = dir.path().join("reference");
        fs::write(&reference, "reference").unwrap();
        let path = dir.path().join("server-info.json");
        write_server_info(&path, 1234).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode(),
            fs::metadata(reference).unwrap().permissions().mode()
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        write_server_info(&path, 5678).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
        let info: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(info["port"], 5678);
    }

    #[test]
    fn forward_config_uses_http_uri_and_preserves_explicit_port() {
        let config = parse_forward_config("http://127.0.0.1:4318/v1/responses".to_string())
            .expect("parse forward config");

        assert_eq!(config.upstream_url, "http://127.0.0.1:4318/v1/responses");
        assert_eq!(config.host_header.to_str().unwrap(), "127.0.0.1:4318");
    }

    #[test]
    fn forward_config_rejects_non_http_scheme() {
        let error = parse_forward_config("file:///tmp/responses".to_string())
            .expect_err("file URL should be rejected");

        assert!(error.to_string().contains("must use http or https"));
    }
}
