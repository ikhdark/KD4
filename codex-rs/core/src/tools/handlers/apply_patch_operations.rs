//! Durable receipts owned by the dedicated patch handler. A pending record is
//! deliberately not retryable: a lost observer is not evidence of no mutation.
use crate::FunctionCallError;
use codex_protocol::tool_failure::{ToolFailure, ToolFailureCode};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const MAX_JOURNAL_BYTES: usize = 64 * 1024 * 1024;
const RECEIPT_STORAGE: &str = "owned-sha256-v1";

pub(super) enum Admission {
    New(Operation),
    Recovered(Value),
}

pub(super) struct Operation {
    journal: File,
    reservation_bytes: usize,
    payload_path: PathBuf,
}

fn unknown(message: impl Into<String>) -> FunctionCallError {
    FunctionCallError::structured(ToolFailure::new(ToolFailureCode::UncertainEffects, message))
}

/// Dedicated-tool headers are not part of the standalone/shell patch language.
/// Strip only envelope headers, never hunk contents.
pub(super) fn split(input: &str) -> Result<(Option<String>, String), FunctionCallError> {
    // The patch parser trims leading whitespace, so a nested call may send it
    // before the envelope; headers must still be recognised there.
    let mut lines = input.trim_start().split_inclusive('\n');
    let Some(first) = lines.next() else {
        return Ok((None, input.into()));
    };
    let Some(second) = lines.next() else {
        return Ok((None, input.into()));
    };
    if first.trim_end_matches(['\r', '\n']) != "*** Begin Patch" {
        return Ok((None, input.into()));
    }
    let declaration = second
        .trim_end_matches(['\r', '\n'])
        .strip_prefix("*** Operation ID: ");
    if let Some(id) = declaration
        && (id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b)))
    {
        return Err(FunctionCallError::structured(ToolFailure::no_effects(
            ToolFailureCode::InvalidInput,
            "Operation ID must be 1..128 ASCII letters, digits, -, _, . or :.",
        )));
    }
    let next = if declaration.is_some() { lines.next() } else { Some(second) };
    let mut patch = first.to_string();
    if let Some(next) = next
        && !next.starts_with("*** Evidence Review:")
    {
        patch.push_str(next);
    }
    patch.extend(lines);
    Ok((declaration.map(str::to_owned), patch))
}

/// Optional, untrusted self-report; malformed metadata must not reject an edit.
pub(super) fn evidence_review(input: &str) -> Option<&str> {
    let mut lines = input.trim_start().lines();
    if lines.next()? != "*** Begin Patch" { return None; }
    let mut next = lines.next()?;
    if next.starts_with("*** Operation ID: ") { next = lines.next()?; }
    next.strip_prefix("*** Evidence Review:").map(str::trim)
}

pub(super) fn with_id(input: &str, id: Option<&str>) -> Result<String, FunctionCallError> {
    let review = evidence_review(input);
    let (updated_id, patch) = split(input)?;
    if updated_id
        .as_deref()
        .is_some_and(|updated| Some(updated) != id)
    {
        return Err(FunctionCallError::structured(ToolFailure::no_effects(
            ToolFailureCode::InvalidInput,
            "A patch hook cannot replace the declared operation identity.",
        )));
    }
    let patch = if let Some(review) = review {
        let (first, rest) = patch.split_once('\n')
            .ok_or_else(|| unknown("Hook returned an invalid patch envelope"))?;
        format!("{first}\n*** Evidence Review: {review}\n{rest}")
    } else { patch };
    match id {
        Some(id) => {
            let (first, rest) = patch
                .split_once('\n')
                .ok_or_else(|| unknown("Hook returned an invalid patch envelope"))?;
            Ok(format!("{first}\n*** Operation ID: {id}\n{rest}"))
        }
        None => Ok(patch),
    }
}

pub(super) fn begin(
    home: &Path,
    thread: &str,
    id: &str,
    binding: Value,
) -> Result<Admission, FunctionCallError> {
    // This namespace is scoped to the originating thread and Codex home. Never
    // derive a target path from model-controlled operation text.
    let key = hex::encode(Sha256::digest(
        serde_json::to_vec(&(thread, id)).map_err(|e| unknown(e.to_string()))?,
    ));
    let directory = home.join("patch-operations-v1");
    std::fs::create_dir_all(&directory)
        .map_err(|e| unknown(format!("Patch receipt directory unavailable: {e}")))?;
    let path = directory.join(key);
    let payload_path = path.with_extension("receipt-v1");
    let created = match OpenOptions::new()
        .read(true)
        .append(true)
        .create_new(true)
        .open(&path)
    {
        Ok(file) => Some(file),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => None,
        Err(e) => return Err(unknown(format!("Cannot reserve patch operation {id}: {e}"))),
    };
    let is_new = created.is_some();
    let mut file = match created {
        Some(file) => file,
        None => OpenOptions::new()
            .read(true)
            .append(true)
            .open(&path)
            .map_err(|e| unknown(format!("Cannot recover patch operation {id}: {e}")))?,
    };
    file.try_lock().map_err(|_| unknown(format!("Patch operation {id} is owned by a live mutation task. Recover this identity after its owner settles; do not submit another patch.")))?;
    let expected = json!({"version":1, "operation_id":id, "binding":binding});
    if is_new {
        let mut bytes = serde_json::to_vec(&expected).map_err(|e| unknown(e.to_string()))?;
        // Leave room for a terminal record even when the patch text nearly
        // fills the inline journal. The hashed envelope binds the same inputs.
        if bytes.len() >= MAX_JOURNAL_BYTES - 1024 {
            bytes = serde_json::to_vec(&hashed_reservation(id, &bytes))
                .map_err(|e| unknown(e.to_string()))?;
        }
        bytes.push(b'\n');
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .map_err(|e| {
                unknown(format!(
                    "Patch operation reservation was not durably confirmed: {e}"
                ))
            })?;
        return Ok(Admission::New(Operation {
            journal: file,
            reservation_bytes: bytes.len(),
            payload_path,
        }));
    }
    // Bounded receipt acquisition. A torn/malformed journal always fails closed.
    let mut bytes = Vec::new();
    (&mut file)
        .take(MAX_JOURNAL_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| unknown(format!("Cannot read patch receipt: {e}")))?;
    if bytes.len() > MAX_JOURNAL_BYTES || !bytes.ends_with(b"\n") {
        return Err(unknown(format!(
            "Patch operation {id} has an incomplete receipt; reconcile through its mutation owner. Do not reapply."
        )));
    }
    let mut records = bytes.split(|b| *b == b'\n').filter(|line| !line.is_empty());
    let original: Value =
        serde_json::from_slice(records.next().unwrap_or_default()).map_err(|e| {
            unknown(format!(
                "Patch operation {id} has an unreadable reservation: {e}"
            ))
        })?;
    let matches = if original["version"] == 2 {
        let expected = serde_json::to_vec(&expected).map_err(|e| unknown(e.to_string()))?;
        original == hashed_reservation(id, &expected)
    } else {
        original == expected
    };
    if !matches {
        return Err(FunctionCallError::structured(ToolFailure::no_effects(
            ToolFailureCode::InvalidInput,
            format!(
                "Operation ID {id} is already bound to a different patch, guards or execution scope. Conflicting reuse was rejected; the original operation is unchanged."
            ),
        )));
    }
    let receipt = records.next().ok_or_else(|| unknown(format!(
        "Patch operation {id} has a durable pending reservation but no terminal receipt. Its effects are unknown; reconcile through the mutation owner, never apply it again.")))?;
    let receipt: Value = serde_json::from_slice(receipt)
        .map_err(|e| unknown(format!("Patch receipt is incomplete: {e}")))?;
    if records.next().is_some() || receipt["operation_id"] != id {
        return Err(receipt_identity_error());
    }
    let receipt = if receipt["receipt_storage"] == RECEIPT_STORAGE {
        read_receipt_payload(&payload_path, &receipt)?
    } else {
        receipt
    };
    if !receipt["success"].is_boolean() || receipt["operation_id"] != id {
        return Err(receipt_identity_error());
    }
    Ok(Admission::Recovered(receipt))
}

fn hashed_reservation(id: &str, serialized: &[u8]) -> Value {
    json!({
        "version": 2,
        "operation_id": id,
        "binding_sha256": hex::encode(Sha256::digest(serialized)),
    })
}

fn receipt_identity_error() -> FunctionCallError {
    unknown("Patch operation receipt failed identity verification; do not replay.")
}

fn read_receipt_payload(path: &Path, reference: &Value) -> Result<Value, FunctionCallError> {
    let length = reference["length"]
        .as_u64()
        .filter(|length| *length > 0 && *length < u64::MAX)
        .ok_or_else(receipt_identity_error)?;
    let digest = reference["sha256"].as_str().ok_or_else(receipt_identity_error)?;
    let file = File::open(path)
        .map_err(|e| unknown(format!("Cannot recover patch receipt payload: {e}. Do not reapply.")))?;
    if !file.metadata().is_ok_and(|metadata| metadata.is_file() && metadata.len() == length) {
        return Err(receipt_identity_error());
    }
    let mut bytes = Vec::new();
    file.take(length + 1).read_to_end(&mut bytes)
        .map_err(|e| unknown(format!("Cannot read patch receipt payload: {e}. Do not reapply.")))?;
    if bytes.len() as u64 != length || hex::encode(Sha256::digest(&bytes)) != digest {
        return Err(receipt_identity_error());
    }
    serde_json::from_slice(&bytes).map_err(|e| unknown(format!("Patch receipt payload is incomplete: {e}")))
}

impl Operation {
    pub(super) fn finish(mut self, receipt: &Value) -> Result<(), FunctionCallError> {
        let mut bytes = serde_json::to_vec(receipt).map_err(|e| unknown(e.to_string()))?;
        let unconfirmed = |e| unknown(format!("Patch settled but durable receipt publication was not confirmed: {e}. Do not reapply; recover the same operation identity."));
        if bytes.len() >= MAX_JOURNAL_BYTES - self.reservation_bytes {
            // The owner holds the journal lock throughout both writes. The
            // reference is never published before its complete payload.
            codex_file_system::write_bytes_atomically(&self.payload_path, &bytes)
                .map_err(unconfirmed)?;
            bytes = serde_json::to_vec(&json!({
                "receipt_storage": RECEIPT_STORAGE,
                "operation_id": receipt["operation_id"],
                "length": bytes.len(),
                "sha256": hex::encode(Sha256::digest(&bytes)),
            })).map_err(|e| unknown(e.to_string()))?;
        }
        if bytes.len() >= MAX_JOURNAL_BYTES - self.reservation_bytes {
            return Err(unknown("Patch receipt reference exceeds the journal bound; do not reapply."));
        }
        bytes.push(b'\n');
        self.journal.write_all(&bytes).and_then(|()| self.journal.sync_all())
            .map_err(unconfirmed)
    }
}
