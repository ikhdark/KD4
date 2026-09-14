use anyhow::Context;
use anyhow::Result;
use std::io;
use std::io::Read;
use std::io::Write;
use std::net::TcpListener;
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::thread;
use std::thread::JoinHandle;
use std::time::Duration;
use std::time::Instant;

pub(super) struct LoopbackResponsesServer {
    base_url: String,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl LoopbackResponsesServer {
    pub(super) fn start() -> Result<Self> {
        let listener =
            TcpListener::bind("127.0.0.1:0").context("bind loopback Responses API server")?;
        listener
            .set_nonblocking(true)
            .context("set loopback Responses API server nonblocking")?;
        let address = listener.local_addr()?;
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = Arc::clone(&shutdown);
        let thread = thread::spawn(move || {
            while !thread_shutdown.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        if let Err(err) = handle_model_connection(stream, &thread_shutdown) {
                            eprintln!("loopback Responses API server error: {err}");
                        }
                    }
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(err) => {
                        eprintln!("loopback Responses API accept error: {err}");
                        break;
                    }
                }
            }
        });
        Ok(Self {
            base_url: format!("http://{address}"),
            shutdown,
            thread: Some(thread),
        })
    }

    pub(super) fn base_url(&self) -> &str {
        &self.base_url
    }
}

impl Drop for LoopbackResponsesServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn handle_model_connection(mut stream: TcpStream, shutdown: &AtomicBool) -> io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let request = read_http_request(
        &mut stream,
        shutdown,
        Instant::now() + Duration::from_secs(2),
    )?;
    let request_line = request
        .split(|byte| *byte == b'\n')
        .next()
        .and_then(|line| std::str::from_utf8(line).ok())
        .unwrap_or_default();
    if request_line.starts_with("POST ") && request_line.contains("/responses ") {
        let body = concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-plugin-analytics\"}}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-plugin-analytics\",\"usage\":{\"input_tokens\":0,\"input_tokens_details\":null,\"output_tokens\":0,\"output_tokens_details\":null,\"total_tokens\":0}}}\n\n"
        );
        write_http_response(&mut stream, "200 OK", "text/event-stream", body)
    } else {
        write_http_response(
            &mut stream,
            "404 Not Found",
            "application/json",
            r#"{"error":"not found"}"#,
        )
    }
}

const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

fn read_http_request(
    stream: &mut TcpStream,
    shutdown: &AtomicBool,
    deadline: Instant,
) -> io::Result<Vec<u8>> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    let header_end = loop {
        let read = read_request_chunk(stream, &mut buffer, shutdown, deadline)?;
        request.extend_from_slice(&buffer[..read]);
        if let Some(position) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            if position + 4 > MAX_HEADER_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "HTTP headers too large",
                ));
            }
            break position + 4;
        }
        if request.len() >= MAX_HEADER_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "HTTP headers too large",
            ));
        }
    };
    let content_length = parse_content_length(&request[..header_end])?;
    header_end.checked_add(content_length).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "HTTP request length overflow")
    })?;
    if content_length > MAX_BODY_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "HTTP body too large",
        ));
    }
    let mut remaining = content_length.saturating_sub(request.len() - header_end);
    request.truncate(header_end);
    while remaining > 0 {
        let length = remaining.min(buffer.len());
        remaining -= read_request_chunk(stream, &mut buffer[..length], shutdown, deadline)?;
    }
    Ok(request)
}

fn read_request_chunk(
    stream: &mut TcpStream,
    buffer: &mut [u8],
    shutdown: &AtomicBool,
    deadline: Instant,
) -> io::Result<usize> {
    loop {
        if shutdown.load(Ordering::Relaxed) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "loopback server shutting down",
            ));
        }
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::TimedOut, "HTTP request deadline expired")
            })?;
        stream.set_read_timeout(Some(remaining.min(Duration::from_millis(100))))?;
        match stream.read(buffer) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "incomplete HTTP request",
                ));
            }
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            result => return result,
        }
    }
}

fn parse_content_length(headers: &[u8]) -> io::Result<usize> {
    let headers = std::str::from_utf8(headers)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    let mut length = None;
    for line in headers.lines().skip(1).filter(|line| !line.is_empty()) {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid HTTP header"))?;
        if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unsupported HTTP transfer encoding",
            ));
        }
        if name.eq_ignore_ascii_case("content-length") {
            let value = value.trim();
            if length.is_some()
                || value.is_empty()
                || !value.bytes().all(|byte| byte.is_ascii_digit())
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid HTTP Content-Length",
                ));
            }
            length = Some(
                value
                    .parse()
                    .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?,
            );
        }
    }
    Ok(length.unwrap_or(0))
}

fn write_http_response(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &str,
) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_request_rejects_partial_and_invalid_framing() -> io::Result<()> {
        for (request, expected) in [
            ("POST /responses HTTP/1.1\r\n", io::ErrorKind::UnexpectedEof),
            (
                "POST /responses HTTP/1.1\r\nContent-Length: 2\r\n\r\nx",
                io::ErrorKind::UnexpectedEof,
            ),
            (
                "POST /responses HTTP/1.1\r\nContent-Length: nope\r\n\r\n",
                io::ErrorKind::InvalidData,
            ),
            (
                "POST /responses HTTP/1.1\r\nContent-Length: +2\r\n\r\n{}",
                io::ErrorKind::InvalidData,
            ),
            (
                "POST /responses HTTP/1.1\r\nContent-Length: 0\r\nContent-Length: 0\r\n\r\n",
                io::ErrorKind::InvalidData,
            ),
            (
                "POST /responses HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n",
                io::ErrorKind::InvalidData,
            ),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0")?;
            let mut client = TcpStream::connect(listener.local_addr()?)?;
            client.write_all(request.as_bytes())?;
            client.shutdown(std::net::Shutdown::Write)?;
            let (mut server, _) = listener.accept()?;
            let error = read_http_request(
                &mut server,
                &AtomicBool::new(false),
                Instant::now() + Duration::from_secs(1),
            )
            .unwrap_err();
            assert_eq!(error.kind(), expected, "{request}");
        }
        Ok(())
    }

    #[test]
    fn http_request_stalled_body_obeys_absolute_deadline() -> io::Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let mut client = TcpStream::connect(listener.local_addr()?)?;
        client.write_all(b"POST /responses HTTP/1.1\r\nContent-Length: 2\r\n\r\nx")?;
        let (mut server, _) = listener.accept()?;
        let started = Instant::now();
        let error = read_http_request(
            &mut server,
            &AtomicBool::new(false),
            started + Duration::from_millis(80),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(1));
        Ok(())
    }

    #[test]
    fn http_request_rejects_overflowing_length_and_reads_valid_body() -> io::Result<()> {
        for content_length in [usize::MAX, 2] {
            let listener = TcpListener::bind("127.0.0.1:0")?;
            let mut client = TcpStream::connect(listener.local_addr()?)?;
            write!(
                client,
                "POST /responses HTTP/1.1\r\nContent-Length: {content_length}\r\n\r\n{{}}"
            )?;
            client.shutdown(std::net::Shutdown::Write)?;
            let (mut server, _) = listener.accept()?;
            server.set_read_timeout(Some(Duration::from_secs(1)))?;
            let result = read_http_request(
                &mut server,
                &AtomicBool::new(false),
                Instant::now() + Duration::from_secs(1),
            );
            if content_length == usize::MAX {
                let error = result.expect_err("overflowing body length must be rejected");
                assert_eq!(error.kind(), io::ErrorKind::InvalidData);
                assert_eq!(error.to_string(), "HTTP request length overflow");
            } else {
                assert_eq!(
                    result?,
                    b"POST /responses HTTP/1.1\r\nContent-Length: 2\r\n\r\n"
                );
            }
        }
        Ok(())
    }
}
