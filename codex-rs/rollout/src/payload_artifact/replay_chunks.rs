//! Ordered, one-level manifest of immutable zstd blocks. The enclosing reference
//! continues to authenticate the complete original JSON byte stream.
use super::*;
use std::io::Cursor;

pub(super) const KEY: &str = "replay_chunks_v1";
pub(super) const SUFFIX: &str = "replay-zst";
const KIB: usize = 1024;
const MIB: usize = 1024 * KIB;
const MAX_BLOCK: usize = 16 * MIB;
const MAX_STORED: usize = MAX_BLOCK + MIB;
const MAX_BLOCKS: usize = 90;
type Block = (String, usize, usize);

pub(super) fn manifest(payload: &serde_json::Value) -> io::Result<Option<Vec<Block>>> {
    let Some(value) = payload.get(KEY) else {
        return Ok(None);
    };
    let invalid = || io::Error::other("invalid chunked replay reference");
    let blocks: Vec<Block> = serde_json::from_value(value.clone())?;
    let bytes = payload["bytes"].as_u64().ok_or_else(invalid)?;
    if payload["item_type"] != "context_replay"
        || bytes > MAX_PAYLOAD_BYTES
        || blocks.is_empty()
        || blocks.len() > MAX_BLOCKS
        || blocks.iter().any(|(hash, stored, raw)| {
            !valid_hash(hash)
                || !(1..=MAX_BLOCK).contains(raw)
                || !(1..=MAX_STORED).contains(stored)
        })
        || blocks.iter().map(|b| b.2 as u64).sum::<u64>() != bytes
    {
        return Err(invalid());
    }
    Ok(Some(blocks))
}

fn valid_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

const fn gear() -> [u64; 256] {
    let mut table = [0; 256];
    let mut x = 0x92d68ca2f4823467_u64;
    let mut i = 0;
    while i < 256 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        table[i] = x;
        i += 1;
    }
    table
}
const GEAR: [u64; 256] = gear();

pub(super) fn store(
    path: &Path,
    item: &impl serde::Serialize,
    timestamp: serde_json::Value,
    format: serde_json::Value,
) -> io::Result<Option<Vec<u8>>> {
    let mut sink = Sink {
        path,
        raw: Vec::new(),
        blocks: Vec::new(),
        emitted: 0,
        rolling: 0,
    };
    let (bytes, sha) = {
        let mut guard = PayloadWriter::new(io::BufWriter::new(&mut sink));
        let serialized = serde_json::to_writer(&mut guard, item);
        if guard.limit_reached {
            return Ok(None);
        }
        serialized?;
        guard.flush()?;
        let bytes = guard.bytes;
        let sha = hex::encode(guard.hasher.finalize());
        (bytes, sha)
    };
    if sink.blocks.is_empty() && sink.raw.len() < 128 * KIB {
        publish_payload(path, &sha, bytes, || stage_bytes(path, &sink.raw))?;
        return payload_reference(
            sha,
            bytes,
            "context_replay".into(),
            Some(timestamp),
            Some(format),
        )
        .map(Some);
    }
    sink.emit()?;
    let mut line = serde_json::to_vec(&serde_json::json!({
        "timestamp": timestamp, "format_version": format, "type": KIND,
        "payload": {"item_type":"context_replay", "bytes":bytes, "sha256":sha,
                    "replay_chunks_v1":sink.blocks}
    }))?;
    line.push(b'\n');
    if line.len() >= INLINE_BYTES {
        return Err(io::Error::other("chunked replay reference too large"));
    }
    Ok(Some(line))
}

fn stage_bytes(path: &Path, bytes: &[u8]) -> io::Result<tempfile::NamedTempFile> {
    let directory = root(path);
    std::fs::create_dir_all(&directory)?;
    let mut temp = payload_stage(&directory)?;
    temp.write_all(bytes)?;
    Ok(temp)
}

struct Sink<'a> {
    path: &'a Path,
    raw: Vec<u8>,
    blocks: Vec<Block>,
    emitted: usize,
    rolling: u64,
}

impl Sink<'_> {
    fn emit(&mut self) -> io::Result<()> {
        if self.raw.is_empty() {
            return Ok(());
        }
        let encoded = zstd::bulk::compress(&self.raw, 1)?;
        let hash = digest(&encoded);
        publish_object(self.path, &hash, encoded.len() as u64, SUFFIX, || {
            stage_bytes(self.path, &encoded)
        })?;
        self.blocks.push((hash, encoded.len(), self.raw.len()));
        self.emitted += self.raw.len();
        self.raw.clear();
        self.rolling = 0;
        Ok(())
    }
}

impl Write for Sink<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut rest = buf;
        while !rest.is_empty() {
            // Increasing minima keep even a 256 MiB body's complete manifest
            // within the existing index-line bound, without a second pass.
            let min = match self.emitted {
                n if n < 4 * MIB => 128 * KIB,
                n if n < 32 * MIB => MIB,
                _ => 8 * MIB,
            };
            if self.raw.len() < min {
                let n = (min - self.raw.len()).min(rest.len());
                self.raw.extend_from_slice(&rest[..n]);
                rest = &rest[n..];
                continue;
            }
            let n = rest.len().min(2 * min - self.raw.len());
            let split = rest[..n].iter().position(|&b| {
                self.rolling = self.rolling.wrapping_shl(1).wrapping_add(GEAR[b as usize]);
                self.rolling & 0x1ffff == 0
            });
            let take = split.map_or(n, |i| i + 1);
            self.raw.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
            if split.is_some() || self.raw.len() == 2 * min {
                self.emit()?
            }
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) fn read(directory: &Path, blocks: &[Block], bytes: u64) -> io::Result<Vec<u8>> {
    let mut body = Vec::with_capacity(bytes as usize);
    for (hash, stored, raw) in blocks {
        let encoded = read_object(&directory.join(format!("{hash}.{SUFFIX}")), *stored as u64)?;
        if digest(&encoded) != *hash {
            return Err(io::Error::other("rollout replay block checksum mismatch"));
        }
        let start = body.len();
        let mut decoder = zstd::stream::read::Decoder::new(Cursor::new(encoded))?;
        decoder.window_log_max(24)?;
        decoder.take(*raw as u64 + 1).read_to_end(&mut body)?;
        if body.len() - start != *raw {
            return Err(io::Error::other(
                "rollout replay block decoded length mismatch",
            ));
        }
    }
    Ok(body)
}
