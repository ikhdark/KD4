//! Durable receipts owned by the dedicated patch handler. A pending record is
//! deliberately not retryable: a lost observer is not evidence of no mutation.
use crate::FunctionCallError;
use codex_protocol::tool_failure::{ToolFailure, ToolFailureCode};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

pub(super) enum Admission {
    New(Operation),
    Recovered(Value),
}

pub(super) struct Operation(File);

fn unknown(message: impl Into<String>) -> FunctionCallError {
    FunctionCallError::structured(ToolFailure::new(ToolFailureCode::UncertainEffects, message))
}

/// The operation declaration belongs to this tool, not the patch language used
/// by standalone/shell callers. Strip exactly one header, never hunk contents.
pub(super) fn split(input: &str) -> Result<(Option<String>, String), FunctionCallError> {
    let mut lines = input.split_inclusive('\n');
    let Some(first) = lines.next() else {
        return Ok((None, input.into()));
    };
    let Some(second) = lines.next() else {
        return Ok((None, input.into()));
    };
    if first.trim_end_matches(['\r', '\n']) != "*** Begin Patch" {
        return Ok((None, input.into()));
    }
    let Some(id) = second
        .trim_end_matches(['\r', '\n'])
        .strip_prefix("*** Operation ID: ")
    else {
        return Ok((None, input.into()));
    };
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b))
    {
        return Err(FunctionCallError::structured(ToolFailure::no_effects(
            ToolFailureCode::InvalidInput,
            "Operation ID must be 1..128 ASCII letters, digits, -, _, . or :.",
        )));
    }
    Ok((
        Some(id.into()),
        format!("{first}{}", lines.collect::<String>()),
    ))
}

pub(super) fn with_id(input: &str, id: Option<&str>) -> Result<String, FunctionCallError> {
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
        bytes.push(b'\n');
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .map_err(|e| {
                unknown(format!(
                    "Patch operation reservation was not durably confirmed: {e}"
                ))
            })?;
        return Ok(Admission::New(Operation(file)));
    }
    // Bounded receipt acquisition. A torn/malformed journal always fails closed.
    let mut bytes = Vec::new();
    (&mut file)
        .take(64 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| unknown(format!("Cannot read patch receipt: {e}")))?;
    if bytes.len() > 64 * 1024 * 1024 || !bytes.ends_with(b"\n") {
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
    if original != expected {
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
    if records.next().is_some() || !receipt["success"].is_boolean() || receipt["operation_id"] != id
    {
        return Err(unknown(
            "Patch operation receipt failed identity verification; do not replay.",
        ));
    }
    Ok(Admission::Recovered(receipt))
}

impl Operation {
    pub(super) fn finish(mut self, receipt: &Value) -> Result<(), FunctionCallError> {
        let mut bytes = serde_json::to_vec(receipt).map_err(|e| unknown(e.to_string()))?;
        bytes.push(b'\n');
        self.0.write_all(&bytes).and_then(|()| self.0.sync_all())
            .map_err(|e| unknown(format!("Patch settled but durable receipt publication was not confirmed: {e}. Do not reapply; recover the same operation identity.")))
    }
}
