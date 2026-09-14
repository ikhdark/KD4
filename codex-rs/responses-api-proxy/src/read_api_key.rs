use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use zeroize::Zeroize;

/// Use a generous buffer size to avoid truncation and to allow for longer API
/// keys in the future.
const BUFFER_SIZE: usize = 1024;
const AUTH_HEADER_PREFIX: &[u8] = b"Bearer ";

pub(crate) fn read_auth_header_from_stdin() -> Result<&'static str> {
    use std::io::Read;

    // Standard input may buffer a copy of the key internally.
    read_auth_header_with(|buffer| std::io::stdin().read(buffer))
}

fn read_auth_header_with<F>(mut read_fn: F) -> Result<&'static str>
where
    F: FnMut(&mut [u8]) -> std::io::Result<usize>,
{
    // Zeroize the temporary buffer after parsing. The header allocation lives
    // for the process lifetime; this function does not lock memory against swap.
    let mut buf = [0u8; BUFFER_SIZE];
    buf[..AUTH_HEADER_PREFIX.len()].copy_from_slice(AUTH_HEADER_PREFIX);

    let prefix_len = AUTH_HEADER_PREFIX.len();
    let capacity = buf.len() - prefix_len;
    let mut total_read = 0usize; // number of bytes read into the token region
    let mut saw_newline = false;
    let mut saw_eof = false;

    while total_read < capacity {
        let slice = &mut buf[prefix_len + total_read..];
        let read = match read_fn(slice) {
            Ok(n) => n,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => {
                buf.zeroize();
                return Err(err.into());
            }
        };

        if read == 0 {
            saw_eof = true;
            break;
        }

        // Search only the newly written region for a newline.
        let newly_written = &slice[..read];
        if let Some(pos) = newly_written.iter().position(|&b| b == b'\n') {
            total_read += pos + 1; // include the newline for trimming below
            saw_newline = true;
            break;
        }

        total_read += read;

        // Continue loop; if buffer fills without newline/EOF we'll error below.
    }

    // If buffer filled and we did not see newline or EOF, error out.
    if total_read == capacity && !saw_newline && !saw_eof {
        buf.zeroize();
        return Err(anyhow!(
            "API key is too large to fit in the {BUFFER_SIZE}-byte buffer"
        ));
    }

    let mut total = prefix_len + total_read;
    while total > prefix_len && (buf[total - 1] == b'\n' || buf[total - 1] == b'\r') {
        total -= 1;
    }

    if total == AUTH_HEADER_PREFIX.len() {
        buf.zeroize();
        return Err(anyhow!(
            "API key must be provided via stdin (e.g. printenv OPENAI_API_KEY | codex responses-api-proxy)"
        ));
    }

    if let Err(err) = validate_auth_header_bytes(&buf[AUTH_HEADER_PREFIX.len()..total]) {
        buf.zeroize();
        return Err(err);
    }

    let header_str = match std::str::from_utf8(&buf[..total]) {
        Ok(value) => value,
        Err(err) => {
            // In theory, validate_auth_header_bytes() should have caught
            // any invalid UTF-8 sequences, but just in case...
            buf.zeroize();
            return Err(err).context("reading Authorization header from stdin as UTF-8");
        }
    };

    let header_value = String::from(header_str);
    buf.zeroize();

    Ok(header_value.leak())
}

/// The key should match /^[A-Za-z0-9\-_]+$/. Ensure there is no funny business
/// with NUL characters and whatnot.
fn validate_auth_header_bytes(key_bytes: &[u8]) -> Result<()> {
    if key_bytes
        .iter()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Ok(());
    }

    Err(anyhow!(
        "API key may only contain ASCII letters, numbers, '-' or '_'"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::io;

    #[test]
    fn retries_interrupted_reads_without_losing_partial_key() {
        let mut calls = 0;
        let result = read_auth_header_with(|buf| {
            calls += 1;
            let data: &[u8] = match calls {
                1 => b"sk-",
                2 => return Err(io::ErrorKind::Interrupted.into()),
                3 => b"abc123\n",
                _ => panic!("unexpected read"),
            };
            buf[..data.len()].copy_from_slice(data);
            Ok(data.len())
        })
        .unwrap();
        assert_eq!(result, "Bearer sk-abc123");
    }

    #[test]
    fn reads_key_with_no_newlines() {
        let mut sent = false;
        let result = read_auth_header_with(|buf| {
            if sent {
                return Ok(0);
            }
            let data = b"sk-abc123";
            buf[..data.len()].copy_from_slice(data);
            sent = true;
            Ok(data.len())
        })
        .unwrap();

        assert_eq!(result, "Bearer sk-abc123");
    }

    #[test]
    fn reads_key_with_short_reads() {
        let mut chunks: VecDeque<&[u8]> =
            VecDeque::from(vec![b"sk-".as_ref(), b"abc".as_ref(), b"123\n".as_ref()]);
        let result = read_auth_header_with(|buf| match chunks.pop_front() {
            Some(chunk) if !chunk.is_empty() => {
                buf[..chunk.len()].copy_from_slice(chunk);
                Ok(chunk.len())
            }
            _ => Ok(0),
        })
        .unwrap();

        assert_eq!(result, "Bearer sk-abc123");
    }

    #[test]
    fn reads_key_and_trims_newlines() {
        let mut sent = false;
        let result = read_auth_header_with(|buf| {
            if sent {
                return Ok(0);
            }
            let data = b"sk-abc123\r\n";
            buf[..data.len()].copy_from_slice(data);
            sent = true;
            Ok(data.len())
        })
        .unwrap();

        assert_eq!(result, "Bearer sk-abc123");
    }

    #[test]
    fn errors_when_no_input_provided() {
        let err = read_auth_header_with(|_| Ok(0)).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("must be provided"));
    }

    #[test]
    fn errors_when_buffer_filled() {
        let err = read_auth_header_with(|buf| {
            let data = vec![b'a'; BUFFER_SIZE - AUTH_HEADER_PREFIX.len()];
            buf[..data.len()].copy_from_slice(&data);
            Ok(data.len())
        })
        .unwrap_err();
        let message = format!("{err:#}");
        let expected_error =
            format!("API key is too large to fit in the {BUFFER_SIZE}-byte buffer");
        assert!(message.contains(&expected_error));
    }

    #[test]
    fn propagates_io_error() {
        let err = read_auth_header_with(|_| Err(io::Error::other("boom"))).unwrap_err();

        let io_error = err.downcast_ref::<io::Error>().unwrap();
        assert_eq!(io_error.kind(), io::ErrorKind::Other);
        assert_eq!(io_error.to_string(), "boom");
    }

    #[test]
    fn errors_on_invalid_utf8() {
        let mut sent = false;
        let err = read_auth_header_with(|buf| {
            if sent {
                return Ok(0);
            }
            let data = b"sk-abc\xff";
            buf[..data.len()].copy_from_slice(data);
            sent = true;
            Ok(data.len())
        })
        .unwrap_err();

        let message = format!("{err:#}");
        assert!(message.contains("API key may only contain ASCII letters, numbers, '-' or '_'"));
    }

    #[test]
    fn errors_on_invalid_characters() {
        let mut sent = false;
        let err = read_auth_header_with(|buf| {
            if sent {
                return Ok(0);
            }
            let data = b"sk-abc!23";
            buf[..data.len()].copy_from_slice(data);
            sent = true;
            Ok(data.len())
        })
        .unwrap_err();

        let message = format!("{err:#}");
        assert!(message.contains("API key may only contain ASCII letters, numbers, '-' or '_'"));
    }
}
