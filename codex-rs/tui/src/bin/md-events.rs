use std::io::Read;
use std::io::{self};

const MAX_INPUT_BYTES: u64 = 64 * 1024 * 1024;

fn read_input(reader: impl Read, limit: u64) -> io::Result<String> {
    let mut bytes = Vec::new();
    reader
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Markdown input exceeds the {limit}-byte limit"),
        ));
    }
    String::from_utf8(bytes).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

fn main() {
    let input = read_input(io::stdin().lock(), MAX_INPUT_BYTES).unwrap_or_else(|err| {
        eprintln!("failed to read stdin: {err}");
        std::process::exit(1);
    });

    let parser = pulldown_cmark::Parser::new(&input);
    for event in parser {
        println!("{event:?}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_exact_byte_limit() -> io::Result<()> {
        assert_eq!(read_input("é!".as_bytes(), 3)?, "é!");
        Ok(())
    }

    #[test]
    fn rejects_overflow_after_only_one_extra_byte() {
        let mut reader = io::Cursor::new(b"123456789");
        let error = read_input(&mut reader, 3).expect_err("oversized input must be rejected");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(error.to_string(), "Markdown input exceeds the 3-byte limit");
        assert_eq!(reader.position(), 4);
    }

    #[test]
    fn rejects_invalid_utf8() {
        let error = read_input(&b"\xff"[..], 3).expect_err("input must be UTF-8");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
