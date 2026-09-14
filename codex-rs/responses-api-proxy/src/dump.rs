use std::fs;
use std::io;
use std::io::BufWriter;
use std::io::Read;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use http::HeaderMap;
use serde::Serialize;
use serde_json::Value;
use tiny_http::Header;
use tiny_http::Method;

const AUTHORIZATION_HEADER_NAME: &str = "authorization";
const REDACTED_HEADER_VALUE: &str = "[REDACTED]";
static NEXT_SEQUENCE: AtomicU64 = AtomicU64::new(1);

pub(crate) struct ExchangeDumper {
    dump_dir: PathBuf,
}

impl ExchangeDumper {
    pub(crate) fn new(dump_dir: PathBuf) -> io::Result<Self> {
        fs::create_dir_all(&dump_dir)?;

        Ok(Self { dump_dir })
    }

    pub(crate) fn dump_request(
        &self,
        method: &Method,
        url: &str,
        headers: &[Header],
        body: &[u8],
    ) -> io::Result<ExchangeDump> {
        let sequence = NEXT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let timestamp_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        let pid = std::process::id();
        let prefix = format!("{sequence:06}-{pid}-{timestamp_ns}");

        let request_path = self.dump_dir.join(format!("{prefix}-request.json"));
        let response_path = self.dump_dir.join(format!("{prefix}-response.json"));

        let request_dump = RequestDump {
            method: method.as_str().to_string(),
            url: url.to_string(),
            headers: headers.iter().map(HeaderDump::from).collect(),
            body: dump_body(body),
        };

        write_json_dump(&request_path, &request_dump)?;

        Ok(ExchangeDump { response_path })
    }
}

pub(crate) struct ExchangeDump {
    response_path: PathBuf,
}

impl ExchangeDump {
    pub(crate) fn tee_response_body<R: Read>(
        self,
        status: u16,
        headers: &HeaderMap,
        response_body: R,
    ) -> ResponseBodyDump<R> {
        ResponseBodyDump {
            response_body,
            response_path: self.response_path,
            status,
            headers: headers.iter().map(HeaderDump::from).collect(),
            body: Vec::new(),
            dump_written: false,
        }
    }
}

pub(crate) struct ResponseBodyDump<R> {
    response_body: R,
    response_path: PathBuf,
    status: u16,
    headers: Vec<HeaderDump>,
    body: Vec<u8>,
    dump_written: bool,
}

impl<R> ResponseBodyDump<R> {
    fn write_dump_if_needed(&mut self, termination: CaptureTermination) {
        if self.dump_written {
            return;
        }

        self.dump_written = true;

        let response_dump = ResponseDump {
            status: self.status,
            headers: std::mem::take(&mut self.headers),
            body: dump_body(&self.body),
            termination,
        };

        if let Err(err) = write_json_dump(&self.response_path, &response_dump) {
            eprintln!(
                "responses-api-proxy failed to write {}: {err}",
                self.response_path.display()
            );
        }
    }
}

impl<R: Read> Read for ResponseBodyDump<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let bytes_read = match self.response_body.read(buf) {
            Ok(bytes_read) => bytes_read,
            Err(err) => {
                if err.kind() != io::ErrorKind::Interrupted {
                    self.write_dump_if_needed(CaptureTermination::ReadError);
                }
                return Err(err);
            }
        };
        if bytes_read == 0 {
            self.write_dump_if_needed(CaptureTermination::Eof);
            return Ok(0);
        }

        self.body.extend_from_slice(&buf[..bytes_read]);
        Ok(bytes_read)
    }
}

impl<R> Drop for ResponseBodyDump<R> {
    fn drop(&mut self) {
        self.write_dump_if_needed(CaptureTermination::Unknown);
    }
}

#[derive(Serialize)]
struct RequestDump {
    method: String,
    url: String,
    headers: Vec<HeaderDump>,
    body: Value,
}

#[derive(Serialize)]
struct ResponseDump {
    status: u16,
    headers: Vec<HeaderDump>,
    body: Value,
    termination: CaptureTermination,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum CaptureTermination {
    Eof,
    ReadError,
    // Dropping without an observed EOF does not establish whether the body ended.
    Unknown,
}

#[derive(Debug, Serialize)]
struct HeaderDump {
    name: String,
    value: String,
}

impl From<&Header> for HeaderDump {
    fn from(header: &Header) -> Self {
        let name = header.field.as_str().to_string();
        let value = if should_redact_header(&name) {
            REDACTED_HEADER_VALUE.to_string()
        } else {
            header.value.as_str().to_string()
        };

        Self { name, value }
    }
}

impl From<(&http::HeaderName, &http::HeaderValue)> for HeaderDump {
    fn from(header: (&http::HeaderName, &http::HeaderValue)) -> Self {
        let name = header.0.as_str();
        let value = if should_redact_header(name) {
            REDACTED_HEADER_VALUE.to_string()
        } else {
            String::from_utf8_lossy(header.1.as_bytes()).into_owned()
        };

        Self {
            name: name.to_string(),
            value,
        }
    }
}

fn should_redact_header(name: &str) -> bool {
    name.eq_ignore_ascii_case(AUTHORIZATION_HEADER_NAME)
        || name.to_ascii_lowercase().contains("cookie")
}

fn dump_body(body: &[u8]) -> Value {
    serde_json::from_slice(body)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(body).into_owned()))
}

fn write_json_dump(path: &PathBuf, dump: &impl Serialize) -> io::Result<()> {
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    let mut writer = BufWriter::new(file);
    serde_json::to_writer_pretty(&mut writer, dump)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Cursor;
    use std::io::Read;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    use http::HeaderMap;
    use http::HeaderValue;
    use http::header::AUTHORIZATION;
    use http::header::CONTENT_TYPE;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use tiny_http::Header;
    use tiny_http::Method;

    use super::ExchangeDumper;

    static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn dump_request_writes_redacted_headers_and_json_body() {
        let dump_dir = test_dump_dir();
        let dumper = ExchangeDumper::new(dump_dir.clone()).expect("create dumper");
        let headers = vec![
            Header::from_bytes(&b"Authorization"[..], &b"Bearer secret"[..])
                .expect("authorization header"),
            Header::from_bytes(&b"Cookie"[..], &b"user-session=secret"[..]).expect("cookie header"),
            Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
                .expect("content-type header"),
            Header::from_bytes(&b"x-codex-window-id"[..], &b"thread-1:0"[..])
                .expect("window id header"),
            Header::from_bytes(&b"x-codex-parent-thread-id"[..], &b"parent-thread-1"[..])
                .expect("parent thread id header"),
            Header::from_bytes(&b"x-openai-subagent"[..], &b"collab_spawn"[..])
                .expect("subagent header"),
        ];

        let exchange_dump = dumper
            .dump_request(
                &Method::Post,
                "/v1/responses",
                &headers,
                br#"{"model":"gpt-5.4"}"#,
            )
            .expect("dump request");

        let request_dump = fs::read_to_string(dump_file_with_suffix(&dump_dir, "-request.json"))
            .expect("read request dump");

        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&request_dump).expect("parse request dump"),
            json!({
                "method": "POST",
                "url": "/v1/responses",
                "headers": [
                    {
                        "name": "Authorization",
                        "value": "[REDACTED]"
                    },
                    {
                        "name": "Cookie",
                        "value": "[REDACTED]"
                    },
                    {
                        "name": "Content-Type",
                        "value": "application/json"
                    },
                    {
                        "name": "x-codex-window-id",
                        "value": "thread-1:0"
                    },
                    {
                        "name": "x-codex-parent-thread-id",
                        "value": "parent-thread-1"
                    },
                    {
                        "name": "x-openai-subagent",
                        "value": "collab_spawn"
                    }
                ],
                "body": {
                    "model": "gpt-5.4"
                }
            })
        );
        assert!(
            exchange_dump
                .response_path
                .file_name()
                .expect("response dump file name")
                .to_string_lossy()
                .ends_with("-response.json")
        );

        fs::remove_dir_all(dump_dir).expect("remove test dump dir");
    }

    #[test]
    fn response_body_dump_streams_body_and_writes_response_file() {
        let dump_dir = test_dump_dir();
        let dumper = ExchangeDumper::new(dump_dir.clone()).expect("create dumper");
        let exchange_dump = dumper
            .dump_request(&Method::Post, "/v1/responses", &[], b"{}")
            .expect("dump request");

        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
        headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer secret"));
        headers.insert(
            "set-cookie",
            HeaderValue::from_static("user-session=secret"),
        );

        let mut response_body = String::new();
        exchange_dump
            .tee_response_body(
                /*status*/ 200,
                &headers,
                Cursor::new(b"data: hello\n\n".to_vec()),
            )
            .read_to_string(&mut response_body)
            .expect("read response body");

        let response_dump = fs::read_to_string(dump_file_with_suffix(&dump_dir, "-response.json"))
            .expect("read response dump");

        assert_eq!(response_body, "data: hello\n\n");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&response_dump).expect("parse response dump"),
            json!({
                "status": 200,
                "headers": [
                    {
                        "name": "content-type",
                        "value": "text/event-stream"
                    },
                    {
                        "name": "authorization",
                        "value": "[REDACTED]"
                    },
                    {
                        "name": "set-cookie",
                        "value": "[REDACTED]"
                    }
                ],
                "body": "data: hello\n\n",
                "termination": "eof"
            })
        );

        fs::remove_dir_all(dump_dir).expect("remove test dump dir");
    }

    #[test]
    fn empty_read_does_not_finalize_capture() {
        let dump_dir = test_dump_dir();
        let dumper = ExchangeDumper::new(dump_dir.clone()).unwrap();
        let exchange = dumper
            .dump_request(&Method::Post, "/v1/responses", &[], b"{}")
            .unwrap();
        let path = exchange.response_path.clone();
        let mut body = exchange.tee_response_body(200, &HeaderMap::new(), Cursor::new(b"hello"));
        assert_eq!(body.read(&mut []).unwrap(), 0);
        assert!(!path.exists());
        let mut output = String::new();
        body.read_to_string(&mut output).unwrap();
        assert_eq!(output, "hello");
        let dump: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(dump["body"], "hello");
        assert_eq!(dump["termination"], "eof");
        fs::remove_dir_all(dump_dir).unwrap();
    }

    #[test]
    fn partial_capture_distinguishes_read_error_from_unknown_completion() {
        for fail in [false, true] {
            let dump_dir = test_dump_dir();
            let dumper = ExchangeDumper::new(dump_dir.clone()).unwrap();
            let exchange = dumper
                .dump_request(&Method::Post, "/v1/responses", &[], b"{}")
                .unwrap();
            let path = exchange.response_path.clone();
            struct FailingReader;
            impl Read for FailingReader {
                fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                    Err(std::io::ErrorKind::ConnectionReset.into())
                }
            }
            let mut body = exchange.tee_response_body(
                200,
                &HeaderMap::new(),
                Cursor::new(b"hello").chain(FailingReader),
            );
            let mut output = [0; 5];
            body.read_exact(&mut output).unwrap();
            assert_eq!(&output, b"hello");
            if fail {
                assert_eq!(
                    body.read(&mut output).unwrap_err().kind(),
                    std::io::ErrorKind::ConnectionReset
                );
            }
            drop(body);
            let dump: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
            assert_eq!(dump["body"], "hello");
            assert_eq!(
                dump["termination"],
                if fail { "read_error" } else { "unknown" }
            );
            fs::remove_dir_all(dump_dir).unwrap();
        }
    }

    #[test]
    fn dump_writes_never_overwrite_existing_evidence() {
        let dump_dir = test_dump_dir();
        let path = dump_dir.join("existing.json");
        fs::write(&path, b"original").unwrap();
        let err = super::write_json_dump(&path, &json!({"replacement": true})).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&path).unwrap(), b"original");
        fs::remove_dir_all(dump_dir).unwrap();
    }

    fn test_dump_dir() -> std::path::PathBuf {
        let test_id = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
        let dump_dir = std::env::temp_dir().join(format!(
            "codex-responses-api-proxy-dump-test-{}-{test_id}",
            std::process::id()
        ));
        fs::create_dir_all(&dump_dir).expect("create test dump dir");
        dump_dir
    }

    fn dump_file_with_suffix(dump_dir: &std::path::Path, suffix: &str) -> std::path::PathBuf {
        let mut matches = fs::read_dir(dump_dir)
            .expect("read dump dir")
            .map(|entry| entry.expect("read dump entry").path())
            .filter(|path| path.to_string_lossy().ends_with(suffix))
            .collect::<Vec<_>>();
        matches.sort();

        assert_eq!(matches.len(), 1);
        matches.pop().expect("single dump file")
    }
}
