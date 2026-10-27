// KD4: production-only 4fffad30 vendor; test/doc examples omitted, handshake log redacted. See README.md.
//! Proxy support for blocking client connections.
//!
//! Async users can reuse `ProxyConfig::from_env` to resolve proxy settings and then
//! establish proxy streams with their preferred async crates.

use std::{
    env,
    io::{Read, Write},
    net::{TcpStream, ToSocketAddrs},
};

use http::Uri;

use crate::{
    error::{Error, Result, UrlError},
    stream::Mode,
};

const MAX_CONNECT_RESPONSE_SIZE: usize = 8192;

/// Proxy scheme supported by tungstenite.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProxyScheme {
    /// HTTP CONNECT proxy.
    Http,
    /// SOCKS5 proxy with remote DNS resolution.
    Socks5,
    /// SOCKS5 proxy with local DNS resolution.
    Socks5h,
}

/// Proxy authentication credentials.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProxyAuth {
    /// Username for basic or SOCKS5 auth.
    pub username: String,
    /// Password for basic or SOCKS5 auth.
    pub password: String,
}

/// Resolved proxy configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProxyConfig {
    /// Proxy scheme.
    pub scheme: ProxyScheme,
    /// Proxy host.
    pub host: String,
    /// Proxy port.
    pub port: u16,
    /// Proxy authentication credentials.
    pub auth: Option<ProxyAuth>,
}

impl ProxyConfig {
    /// Resolve proxy configuration for the given WebSocket URI using environment variables.
    ///
    /// Honors `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, and `NO_PROXY` (case-insensitive).
    /// Supported proxy schemes: `http://`, `socks5://`, `socks5h://`.
    pub fn from_env(uri: &Uri) -> Result<Option<Self>> {
        let mode = super::client::uri_mode(uri)?;
        let host = uri.host().ok_or(Error::Url(UrlError::NoHostName))?;
        let port = uri.port_u16().unwrap_or(match mode {
            Mode::Plain => 80,
            Mode::Tls => 443,
        });

        proxy_from_env_for_host(host, port, mode)
    }

    /// Parse a proxy configuration from a proxy URL.
    pub fn parse(value: &str) -> Result<Self> {
        parse_proxy_config(value)
    }

    /// Render the proxy authority as `host:port`.
    pub fn authority(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

pub(crate) fn connect_proxy_stream(uri: &Uri, host: &str, port: u16) -> Result<Option<TcpStream>> {
    let mode = super::client::uri_mode(uri)?;
    let Some(proxy) = proxy_from_env_for_host(host, port, mode)? else {
        return Ok(None);
    };

    let stream = match proxy.scheme {
        ProxyScheme::Http => connect_http_proxy(&proxy, host, port)?,
        ProxyScheme::Socks5 | ProxyScheme::Socks5h => connect_socks5_proxy(&proxy, host, port)?,
    };

    Ok(Some(stream))
}

fn proxy_from_env_for_host(host: &str, port: u16, mode: Mode) -> Result<Option<ProxyConfig>> {
    if should_bypass_proxy(host, port)? {
        return Ok(None);
    }

    let proxy = match mode {
        Mode::Plain => get_env_first(&["HTTP_PROXY", "http_proxy"]),
        Mode::Tls => get_env_first(&["HTTPS_PROXY", "https_proxy"])
            .or_else(|| get_env_first(&["HTTP_PROXY", "http_proxy"])),
    }
    .or_else(|| get_env_first(&["ALL_PROXY", "all_proxy"]));

    let Some(proxy) = proxy else {
        return Ok(None);
    };

    parse_proxy_config(&proxy).map(Some)
}

fn get_env_first(keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| env::var(key).ok()).filter(|value| !value.is_empty())
}

fn should_bypass_proxy(host: &str, port: u16) -> Result<bool> {
    let no_proxy = get_env_first(&["NO_PROXY", "no_proxy"]);
    let Some(no_proxy) = no_proxy else {
        return Ok(false);
    };

    let host = normalize_host(host);
    let no_proxy = no_proxy.trim();
    if no_proxy.is_empty() {
        return Ok(false);
    }
    if no_proxy == "*" {
        return Ok(true);
    }

    for token in no_proxy.split(',') {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }

        let (token_host, token_port) = split_host_port(token);
        if let Some(token_port) = token_port {
            if token_port != port {
                continue;
            }
        }

        let token_host = normalize_host(token_host);

        if host == token_host {
            return Ok(true);
        }

        if token_host.starts_with('.') {
            let token_host = &token_host[1..];
            if host == token_host || host.ends_with(&format!(".{token_host}")) {
                return Ok(true);
            }
        } else if host.ends_with(&format!(".{token_host}")) {
            return Ok(true);
        }
    }

    Ok(false)
}

fn split_host_port(token: &str) -> (&str, Option<u16>) {
    let token = token.trim();
    if token.starts_with('[') {
        if let Some(close) = token.find(']') {
            let host = &token[..=close];
            let remainder = &token[close + 1..];
            if let Some(port) = remainder.strip_prefix(':') {
                return (host, port.parse().ok());
            }
            return (host, None);
        }
        return (token, None);
    }

    if token.matches(':').count() == 1 {
        if let Some((host, port)) = token.rsplit_once(':') {
            return (host, port.parse().ok());
        }
    }

    (token, None)
}

fn normalize_host(host: &str) -> &str {
    host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host)
}

fn parse_proxy_config(value: &str) -> Result<ProxyConfig> {
    let value = value.trim();
    let uri: Uri =
        value.parse().map_err(|_| Error::Url(UrlError::InvalidProxyConfig(value.into())))?;

    let scheme = match uri.scheme_str() {
        Some("http") => ProxyScheme::Http,
        Some("socks5") => ProxyScheme::Socks5,
        Some("socks5h") => ProxyScheme::Socks5h,
        Some(_) | None => return Err(Error::Url(UrlError::UnsupportedProxyScheme)),
    };

    let authority = uri
        .authority()
        .ok_or_else(|| Error::Url(UrlError::InvalidProxyConfig(value.into())))?
        .as_str();

    let (userinfo, hostport) = split_userinfo(authority);
    let (host, port) = parse_host_port(hostport, &scheme)?;

    let auth = userinfo.map(parse_userinfo).transpose()?;

    Ok(ProxyConfig { scheme, host, port, auth })
}

fn split_userinfo(authority: &str) -> (Option<&str>, &str) {
    let mut iter = authority.rsplitn(2, '@');
    let hostport = iter.next().unwrap_or(authority);
    let userinfo = iter.next();
    (userinfo, hostport)
}

fn parse_host_port(hostport: &str, scheme: &ProxyScheme) -> Result<(String, u16)> {
    let uri: Uri = format!("http://{hostport}")
        .parse()
        .map_err(|_| Error::Url(UrlError::InvalidProxyConfig(hostport.into())))?;

    let host = uri
        .host()
        .ok_or_else(|| Error::Url(UrlError::InvalidProxyConfig(hostport.into())))?
        .to_string();

    let port = uri.port_u16().unwrap_or(match scheme {
        ProxyScheme::Http => 80,
        ProxyScheme::Socks5 | ProxyScheme::Socks5h => 1080,
    });

    Ok((host, port))
}

fn parse_userinfo(userinfo: &str) -> Result<ProxyAuth> {
    let (user, pass) = userinfo.split_once(':').unwrap_or((userinfo, ""));
    let username = percent_decode(user)?;
    let password = percent_decode(pass)?;
    Ok(ProxyAuth { username, password })
}

fn percent_decode(value: &str) -> Result<String> {
    let mut output = Vec::with_capacity(value.len());
    let mut chars = value.as_bytes().iter().copied();
    while let Some(byte) = chars.next() {
        if byte == b'%' {
            let hi = chars
                .next()
                .ok_or_else(|| Error::Url(UrlError::InvalidProxyConfig(value.into())))?;
            let lo = chars
                .next()
                .ok_or_else(|| Error::Url(UrlError::InvalidProxyConfig(value.into())))?;
            let decoded = (from_hex(hi)? << 4) | from_hex(lo)?;
            output.push(decoded);
        } else {
            output.push(byte);
        }
    }
    String::from_utf8(output).map_err(|_| Error::Url(UrlError::InvalidProxyConfig(value.into())))
}

fn from_hex(byte: u8) -> Result<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(Error::Url(UrlError::InvalidProxyConfig("invalid percent-encoding".into()))),
    }
}

fn connect_http_proxy(proxy: &ProxyConfig, host: &str, port: u16) -> Result<TcpStream> {
    let mut stream = connect_to_proxy(proxy)?;
    http_connect(&mut stream, host, port, proxy.auth.as_ref())?;
    Ok(stream)
}

fn connect_socks5_proxy(proxy: &ProxyConfig, host: &str, port: u16) -> Result<TcpStream> {
    let mut stream = connect_to_proxy(proxy)?;
    socks5_handshake(&mut stream, host, port, proxy.auth.as_ref())?;
    Ok(stream)
}

fn connect_to_proxy(proxy: &ProxyConfig) -> Result<TcpStream> {
    let addrs = (proxy.host.as_str(), proxy.port).to_socket_addrs()?;
    for addr in addrs {
        if let Ok(stream) = TcpStream::connect(addr) {
            return Ok(stream);
        }
    }
    Err(Error::Url(UrlError::ProxyConnect(format!(
        "unable to connect to proxy {}:{}",
        proxy.host, proxy.port
    ))))
}

fn basic_auth_header(auth: &ProxyAuth) -> Result<String> {
    let token = format!("{}:{}", auth.username, auth.password);
    let encoded = data_encoding::BASE64.encode(token.as_bytes());
    Ok(format!("Basic {encoded}"))
}

fn read_connect_response(reader: &mut impl Read) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 512];
    loop {
        if buf.len() >= MAX_CONNECT_RESPONSE_SIZE {
            return Err(Error::Url(UrlError::ProxyConnect(
                "HTTP CONNECT response too large".into(),
            )));
        }

        let read = reader.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..read]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }

    Ok(buf)
}

/// Build the bytes for an HTTP CONNECT request.
pub fn build_http_connect_request(authority: &str, auth: Option<&ProxyAuth>) -> Result<Vec<u8>> {
    let mut request = Vec::new();
    request.extend_from_slice(format!("CONNECT {authority} HTTP/1.1\r\n").as_bytes());
    request.extend_from_slice(format!("Host: {authority}\r\n").as_bytes());
    request.extend_from_slice(b"Proxy-Connection: Keep-Alive\r\n");
    if let Some(auth) = auth {
        let token = basic_auth_header(auth)?;
        request.extend_from_slice(format!("Proxy-Authorization: {token}\r\n").as_bytes());
    }
    request.extend_from_slice(b"\r\n");
    Ok(request)
}

/// Parse an HTTP CONNECT response and return the status code.
pub fn parse_http_connect_response(response: &[u8]) -> Result<u16> {
    let text = std::str::from_utf8(response).map_err(|_| {
        Error::Url(UrlError::ProxyConnect("HTTP CONNECT response not valid UTF-8".into()))
    })?;

    let mut lines = text.lines();
    let status_line = lines.next().ok_or_else(|| {
        Error::Url(UrlError::ProxyConnect("HTTP CONNECT response missing status line".into()))
    })?;

    let mut parts = status_line.split_whitespace();
    let _version = parts.next();
    let code = parts.next().ok_or_else(|| {
        Error::Url(UrlError::ProxyConnect("HTTP CONNECT response missing status code".into()))
    })?;
    code.parse::<u16>().map_err(|_| {
        Error::Url(UrlError::ProxyConnect("HTTP CONNECT response invalid status code".into()))
    })
}

fn http_connect(
    stream: &mut (impl Read + Write),
    host: &str,
    port: u16,
    auth: Option<&ProxyAuth>,
) -> Result<()> {
    let authority = format!("{host}:{port}");
    let request = build_http_connect_request(&authority, auth)?;
    stream.write_all(&request)?;
    stream.flush()?;

    let response = read_connect_response(stream)?;
    let status = parse_http_connect_response(&response)?;
    if !(200..300).contains(&status) {
        return Err(Error::Url(UrlError::ProxyConnect(format!(
            "HTTP CONNECT failed with status {status}"
        ))));
    }
    Ok(())
}

fn socks5_handshake(
    stream: &mut (impl Read + Write),
    host: &str,
    port: u16,
    auth: Option<&ProxyAuth>,
) -> Result<()> {
    let mut methods = vec![0x00];
    if auth.is_some() {
        methods.push(0x02);
    }

    stream.write_all(&[0x05, methods.len() as u8])?;
    stream.write_all(&methods)?;
    stream.flush()?;

    let mut choice = [0u8; 2];
    stream.read_exact(&mut choice)?;
    if choice[0] != 0x05 {
        return Err(Error::Url(UrlError::ProxyConnect("SOCKS5: invalid response version".into())));
    }

    match choice[1] {
        0x00 => {}
        0x02 => {
            let auth = auth.ok_or_else(|| {
                Error::Url(UrlError::ProxyConnect(
                    "SOCKS5: proxy requested auth, but none provided".into(),
                ))
            })?;
            socks5_userpass_auth(stream, auth)?;
        }
        0xFF => {
            return Err(Error::Url(UrlError::ProxyConnect(
                "SOCKS5: no acceptable authentication method".into(),
            )));
        }
        _ => {
            return Err(Error::Url(UrlError::ProxyConnect(
                "SOCKS5: unsupported authentication method".into(),
            )));
        }
    }

    send_socks5_connect(stream, host, port)?;
    Ok(())
}

fn socks5_userpass_auth(stream: &mut (impl Read + Write), auth: &ProxyAuth) -> Result<()> {
    let username = auth.username.as_bytes();
    let password = auth.password.as_bytes();

    if username.len() > u8::MAX as usize || password.len() > u8::MAX as usize {
        return Err(Error::Url(UrlError::ProxyConnect("SOCKS5 auth credentials too long".into())));
    }

    let mut buf = Vec::with_capacity(3 + username.len() + password.len());
    buf.push(0x01);
    buf.push(username.len() as u8);
    buf.extend_from_slice(username);
    buf.push(password.len() as u8);
    buf.extend_from_slice(password);

    stream.write_all(&buf)?;
    stream.flush()?;

    let mut response = [0u8; 2];
    stream.read_exact(&mut response)?;
    if response[0] != 0x01 || response[1] != 0x00 {
        return Err(Error::Url(UrlError::ProxyConnect("SOCKS5 authentication failed".into())));
    }

    Ok(())
}

fn send_socks5_connect(stream: &mut (impl Read + Write), host: &str, port: u16) -> Result<()> {
    let mut request = Vec::new();
    request.push(0x05);
    request.push(0x01);
    request.push(0x00);

    if let Ok(addr) = host.parse::<std::net::Ipv4Addr>() {
        request.push(0x01);
        request.extend_from_slice(&addr.octets());
    } else if let Ok(addr) = host.parse::<std::net::Ipv6Addr>() {
        request.push(0x04);
        request.extend_from_slice(&addr.octets());
    } else {
        let host_bytes = host.as_bytes();
        if host_bytes.len() > u8::MAX as usize {
            return Err(Error::Url(UrlError::ProxyConnect("SOCKS5 domain name too long".into())));
        }
        request.push(0x03);
        request.push(host_bytes.len() as u8);
        request.extend_from_slice(host_bytes);
    }

    request.extend_from_slice(&port.to_be_bytes());
    stream.write_all(&request)?;
    stream.flush()?;

    let mut header = [0u8; 4];
    stream.read_exact(&mut header)?;
    if header[0] != 0x05 {
        return Err(Error::Url(UrlError::ProxyConnect("SOCKS5: invalid response version".into())));
    }

    if header[1] != 0x00 {
        return Err(Error::Url(UrlError::ProxyConnect(format!(
            "SOCKS5: connection failed with code {}",
            header[1]
        ))));
    }

    let addr_len = match header[3] {
        0x01 => 4,
        0x03 => {
            let mut len = [0u8; 1];
            stream.read_exact(&mut len)?;
            len[0] as usize
        }
        0x04 => 16,
        _ => return Err(Error::Url(UrlError::ProxyConnect("SOCKS5: invalid address type".into()))),
    };

    let mut discard = vec![0u8; addr_len + 2];
    stream.read_exact(&mut discard)?;
    Ok(())
}

