//! Bounded, read-only row extraction shared by files and retained command logs.
use std::io::BufRead;
use std::io::Read;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;

use super::ToolOutputSelector;
use super::ToolOutputSelectorResult;
use super::ToolOutputSelectorStatus;

const MAX_DOCUMENT_BYTES: u64 = 16 * 1024 * 1024;
const MAX_ROW_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const MAX_ROWS: usize = 1_000;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Format {
    Json,
    Jsonl,
    Csv,
    Tsv,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MalformedRows {
    Error,
    Skip,
    Report,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Operator {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Contains,
    Exists,
    Missing,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Filter {
    pub pointer: String,
    pub op: Operator,
    // Keep explicit null distinct from an omitted comparison operand.
    #[serde(
        default,
        deserialize_with = "operand",
        skip_serializing_if = "Option::is_none"
    )]
    pub value: Option<Value>,
}

fn operand<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}

pub(crate) const fn default_start_row() -> usize {
    1
}

pub(crate) const fn default_max_rows() -> usize {
    100
}

fn valid_pointer(pointer: &str) -> bool {
    (pointer.is_empty() || pointer.starts_with('/'))
        && !pointer
            .split('~')
            .skip(1)
            .any(|part| !part.starts_with(['0', '1']))
}

fn compare(left: &Value, right: &Value) -> Option<std::cmp::Ordering> {
    match (left, right) {
        (Value::String(left), Value::String(right)) => Some(left.cmp(right)),
        (Value::Number(left), Value::Number(right)) => {
            let integer = |number: &serde_json::Number| {
                number
                    .as_i64()
                    .map(i128::from)
                    .or_else(|| number.as_u64().map(i128::from))
            };
            if let (Some(left), Some(right)) = (integer(left), integer(right)) {
                Some(left.cmp(&right))
            } else {
                // Refuse lossy ordering rather than round large integer operands.
                let exact = |number: &serde_json::Number| {
                    let value = number.as_f64()?;
                    if number.is_f64() || value.abs() <= ((1_u64 << 53) - 1) as f64 {
                        Some(value)
                    } else {
                        None
                    }
                };
                exact(left)?.partial_cmp(&exact(right)?)
            }
        }
        _ => None,
    }
}

fn matches(row: &Value, filter: &Filter) -> bool {
    let field = row.pointer(&filter.pointer);
    match filter.op {
        Operator::Exists => field.is_some(),
        Operator::Missing => field.is_none(),
        Operator::Eq => field
            .zip(filter.value.as_ref())
            .is_some_and(|(a, b)| a == b),
        Operator::Ne => field
            .zip(filter.value.as_ref())
            .is_some_and(|(a, b)| a != b),
        Operator::Contains => field
            .zip(filter.value.as_ref())
            .is_some_and(|(a, b)| match (a, b) {
                (Value::String(a), Value::String(b)) => a.contains(b),
                (Value::Array(a), b) => a.contains(b),
                _ => false,
            }),
        op => field
            .zip(filter.value.as_ref())
            .and_then(|(a, b)| compare(a, b))
            .is_some_and(|order| match op {
                Operator::Lt => order.is_lt(),
                Operator::Le => order.is_le(),
                Operator::Gt => order.is_gt(),
                Operator::Ge => order.is_ge(),
                _ => false,
            }),
    }
}

fn jsonl_row(reader: &mut impl BufRead) -> Option<Result<Value, String>> {
    let mut line = Vec::new();
    // The row bound excludes its `\n` or `\r\n` terminator, as at EOF.
    match (&mut *reader)
        .take((MAX_ROW_BYTES + 2) as u64)
        .read_until(b'\n', &mut line)
    {
        Ok(0) => None,
        Ok(_)
            if line.len()
                - usize::from(line.ends_with(b"\n"))
                - usize::from(line.ends_with(b"\r\n"))
                > MAX_ROW_BYTES =>
        {
            if line.last() != Some(&b'\n')
                && let Err(error) = reader.skip_until(b'\n')
            {
                return Some(Err(format!("row read failed: {error}")));
            }
            Some(Err(format!("JSONL row exceeds {MAX_ROW_BYTES} bytes")))
        }
        Ok(_) => Some(serde_json::from_slice(&line).map_err(|error| error.to_string())),
        Err(error) => Some(Err(format!("row read failed: {error}"))),
    }
}

/// Yields rows numbered from `start_row`; earlier rows are skipped.
fn rows<'a>(
    reader: impl BufRead + 'a,
    format: Format,
    pointer: &str,
    start_row: usize,
) -> Result<Box<dyn Iterator<Item = Result<Value, String>> + 'a>, String> {
    let skip = start_row.saturating_sub(1);
    if format == Format::Jsonl {
        let mut reader = reader;
        // A JSONL row is one line: skip earlier rows without buffering or
        // parsing them, so paging through a log stays linear.
        for _ in 0..skip {
            match reader.skip_until(b'\n') {
                Ok(0) => break,
                Ok(_) => {}
                Err(error) => return Err(format!("row read failed: {error}")),
            }
        }
        return Ok(Box::new(std::iter::from_fn(move || jsonl_row(&mut reader))));
    }
    // JSON documents and delimited records use a bounded source. JSONL remains
    // streaming, so arbitrarily large retained logs need no whole-log buffer.
    let mut bytes = Vec::new();
    reader
        .take(MAX_DOCUMENT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() as u64 > MAX_DOCUMENT_BYTES {
        return Err(
            "JSON/CSV/TSV extraction supports at most 16 MiB; use JSONL for streaming logs".into(),
        );
    }
    if format == Format::Json {
        let mut document: Value = serde_json::from_slice(&bytes)
            .map_err(|error| format!("invalid JSON document: {error}"))?;
        let selected = document
            .pointer_mut(pointer)
            .ok_or("JSON rows pointer was not found")?
            .take();
        return match selected {
            Value::Array(rows) => Ok(Box::new(rows.into_iter().skip(skip).map(Ok))),
            row @ Value::Object(_) => Ok(Box::new(std::iter::once(row).skip(skip).map(Ok))),
            _ => Err("JSON rows pointer must select an array or object".into()),
        };
    }
    let mut csv = csv::ReaderBuilder::new()
        .delimiter(if format == Format::Tsv { b'\t' } else { b',' })
        .from_reader(std::io::Cursor::new(bytes));
    let headers = csv
        .headers()
        .map_err(|error| format!("invalid header: {error}"))?
        .clone();
    let mut seen = std::collections::BTreeSet::new();
    if headers.is_empty()
        || headers
            .iter()
            .any(|header| header.is_empty() || !seen.insert(header.to_owned()))
    {
        return Err("CSV/TSV requires nonempty, unique column names".into());
    }
    Ok(Box::new(csv.into_records().skip(skip).map(move |record| {
        let record = record.map_err(|error| error.to_string())?;
        Ok(Value::Object(
            headers
                .iter()
                .zip(record.iter())
                .map(|(key, value)| (key.to_owned(), Value::String(value.to_owned())))
                .collect(),
        ))
    })))
}

/// Entries admitted to one page and the counters for its scanned input rows.
#[derive(Default)]
struct Page {
    rows: Vec<Value>,
    errors: Vec<Value>,
    malformed_rows: u64,
    rows_scanned: u64,
}

impl Page {
    fn entries(&self) -> usize {
        self.rows.len() + self.errors.len()
    }

    /// `cursor` is the next unconsumed input row; `None` exhausts the selection.
    fn result(
        self,
        selector: &ToolOutputSelector,
        coverage_complete: bool,
        cursor: Option<usize>,
    ) -> ToolOutputSelectorResult {
        let mut result =
            ToolOutputSelectorResult::state(selector.clone(), ToolOutputSelectorStatus::Ok);
        result.complete = true;
        let mut value = json!({
            "rows": [],
            "errors": [],
            "malformed_rows": self.malformed_rows,
            "rows_scanned": self.rows_scanned,
            "rows_exhausted": cursor.is_none(),
            "coverage_complete": coverage_complete,
        });
        // Move admitted entries into their existing keys instead of copying them.
        value["rows"] = Value::Array(self.rows);
        value["errors"] = Value::Array(self.errors);
        result.value = Some(value);
        result.continuation = cursor.map(|row| {
            let mut next = selector.clone();
            if let ToolOutputSelector::Structured { start_row, .. } = &mut next {
                *start_row = row;
            }
            next
        });
        result
    }
}

/// Projection is derived data, never an exact byte range or acquisition proof.
/// `capacity` is supplied by the existing file/recovery response-budget owner:
/// given the widest empty page, it returns the serialized row/error entry bytes
/// that still fit. Admission keeps a running count instead of re-serializing
/// the accumulated page, and no returned page exceeds that bound.
pub(crate) fn select(
    reader: impl BufRead,
    selector: ToolOutputSelector,
    coverage_complete: bool,
    capacity: impl FnOnce(&ToolOutputSelectorResult) -> usize,
) -> ToolOutputSelectorResult {
    let ToolOutputSelector::Structured {
        format,
        fields,
        filters,
        start_row,
        end_row,
        max_rows,
        malformed_rows,
        pointer,
    } = &selector
    else {
        unreachable!("structured selector owner");
    };
    let invalid = |message: String| {
        let mut invalid =
            ToolOutputSelectorResult::state(selector.clone(), ToolOutputSelectorStatus::Invalid);
        invalid.message = Some(message);
        invalid
    };
    if *start_row == 0
        || end_row.is_some_and(|end| end < *start_row)
        || !(1..=MAX_ROWS).contains(max_rows)
        || fields.len() > 64
        || filters.len() > 64
        || !valid_pointer(pointer)
        || fields.iter().any(|field| !valid_pointer(field))
        || filters.iter().any(|filter| {
            !valid_pointer(&filter.pointer)
                || (matches!(filter.op, Operator::Exists | Operator::Missing)
                    != filter.value.is_none())
        })
        || (*format != Format::Json && !pointer.is_empty())
    {
        return invalid("structured requires 1-based start_row <= end_row, max_rows 1..1000, at most 64 fields/filters, RFC 6901 pointers, and a value for every filter except exists/missing; pointer is JSON-only".into());
    }
    let source = match rows(reader, *format, pointer, *start_row) {
        Ok(rows) => rows,
        Err(error) => return invalid(error),
    };
    // Maximal counters and cursor bound every page's frame; its entries add
    // exactly their serialized bytes plus separating commas.
    let widest = Page {
        malformed_rows: u64::MAX,
        rows_scanned: u64::MAX,
        ..Page::default()
    };
    let capacity = capacity(&widest.result(&selector, coverage_complete, Some(usize::MAX)));
    let mut page = Page::default();
    let mut entry_bytes = 0_usize;
    for (index, row) in source.enumerate() {
        let number = *start_row + index;
        if let Err(error) = &row
            && error.starts_with("row read failed:")
        {
            return invalid(error.clone());
        }
        if end_row.is_some_and(|end| number > end) {
            break;
        }
        // Reserve the actual cursor before admitting a row. No row is dropped
        // when output, rather than input, reaches its existing delivery budget.
        if page.entries() >= *max_rows {
            return page.result(&selector, coverage_complete, Some(number));
        }
        let (error_entry, entry) = match row {
            Ok(row) => {
                if !filters.iter().all(|filter| matches(&row, filter)) {
                    page.rows_scanned += 1;
                    continue;
                }
                let projected = if fields.is_empty() {
                    row
                } else {
                    Value::Object(
                        fields
                            .iter()
                            .filter_map(|field| {
                                row.pointer(field)
                                    .map(|value| (field.clone(), value.clone()))
                            })
                            .collect(),
                    )
                };
                (false, json!({"row":number, "value":projected}))
            }
            Err(error) => {
                if *malformed_rows == MalformedRows::Error {
                    return invalid(format!("malformed row {number}: {error}"));
                }
                if *malformed_rows == MalformedRows::Skip {
                    page.malformed_rows += 1;
                    page.rows_scanned += 1;
                    continue;
                }
                (true, json!({"row":number,"message":error}))
            }
        };
        let occupied = if error_entry {
            !page.errors.is_empty()
        } else {
            !page.rows.is_empty()
        };
        let bytes = serde_json::to_string(&entry)
            .map_or(usize::MAX, |text| text.len())
            .saturating_add(usize::from(occupied));
        if entry_bytes.saturating_add(bytes) > capacity {
            if page.entries() > 0 {
                return page.result(&selector, coverage_complete, Some(number));
            }
            // Retrying from this same row would return this page again, so
            // this is a terminal selector failure rather than a continuation.
            // Like a page over max_bytes, it has no page value: nothing was
            // delivered, and rows_exhausted:false would promise a continuation.
            let mut result = page.result(&selector, coverage_complete, Some(number));
            result.status = ToolOutputSelectorStatus::SelectorTooLarge;
            result.complete = false;
            result.value = None;
            result.continuation = None;
            result.message = Some(format!(
                "projected row {number} cannot fit the remaining response budget; select fewer fields, request this selector without siblings, or use exact bytes/lines"
            ));
            return result;
        }
        entry_bytes += bytes;
        page.rows_scanned += 1;
        if error_entry {
            page.malformed_rows += 1;
            page.errors.push(entry);
        } else {
            page.rows.push(entry);
        }
    }
    page.result(&selector, coverage_complete, None)
}
