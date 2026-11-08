//! Opt-in content-free index revisions. Payload references include their existing
//! content digests; this never rereads or rehashes payload artifacts.
use sha2::{Digest, Sha256};

#[derive(Default)]
pub(crate) struct Revision {
    hash: Sha256,
    records: u64,
    bytes: u64,
}
impl Revision {
    pub(crate) fn push(&mut self, line: &str, terminated: bool) {
        self.hash.update((line.len() as u64).to_le_bytes());
        self.hash.update(line.as_bytes());
        self.hash.update([u8::from(terminated)]);
        self.records = self.records.saturating_add(1);
        self.bytes = self.bytes.saturating_add(line.len() as u64);
    }
    pub(crate) fn value(&self) -> serde_json::Value {
        serde_json::json!({"scheme":"normalized_utf8_index_records_v1",
            "sha256":hex::encode(self.hash.clone().finalize()),
            "records":self.records, "content_bytes":self.bytes,
            "scope":"index_records_before_payload_hydration; not_payload_bytes_or_state_db"})
    }
}

pub(crate) fn store_id(path: &std::path::Path) -> String {
    hex::encode(Sha256::digest(
        crate::compression::plain_rollout_path(path)
            .to_string_lossy()
            .as_bytes(),
    ))
}
