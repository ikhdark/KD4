use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;
use std::fmt;
use thiserror::Error;
use unicode_normalization::is_nfc;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ContractError {
    #[error("invalid JSON: {0}")]
    InvalidJson(String),
    #[error("canonical JSON does not permit floating-point numbers")]
    FloatNotPermitted,
    #[error("string is not NFC: {0}")]
    NonNfc(String),
    #[error("input is not the exact canonical JSON encoding")]
    NonCanonicalJson,
    #[error("invalid SHA-256 hex value")]
    InvalidSha256,
    #[error("invalid identifier: {0}")]
    InvalidIdentifier(String),
    #[error("invalid repository path: {0}")]
    InvalidRepositoryPath(String),
    #[error("invalid contract: {0}")]
    InvalidContract(String),
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct Sha256HexV1(String);

impl Sha256HexV1 {
    pub fn parse(value: impl Into<String>) -> Result<Self, ContractError> {
        let value = value.into();
        if value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            Ok(Self(value))
        } else {
            Err(ContractError::InvalidSha256)
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Sha256HexV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Sha256HexV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(serde::de::Error::custom)
    }
}

/// Domain-separated hash used by all validation contracts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProofHashV1 {
    pub domain: String,
    pub sha256: Sha256HexV1,
}

/// A required JSON field whose only permitted value is `null`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MustBeNullV1;

pub fn validate_nfc(value: &str) -> Result<(), ContractError> {
    if is_nfc(value) {
        Ok(())
    } else {
        Err(ContractError::NonNfc(value.to_owned()))
    }
}

pub fn validate_nonempty_nfc(value: &str, label: &str) -> Result<(), ContractError> {
    validate_nfc(value)?;
    if value.is_empty() {
        Err(ContractError::InvalidContract(format!(
            "{label} must be a nonempty NFC string"
        )))
    } else {
        Ok(())
    }
}

pub fn validate_sorted_unique_nfc_strings(
    values: &[String],
    label: &str,
    require_nonempty: bool,
) -> Result<(), ContractError> {
    if (require_nonempty && values.is_empty())
        || values
            .iter()
            .any(|value| validate_nonempty_nfc(value, label).is_err())
        || values.windows(2).any(|pair| pair[0] >= pair[1])
    {
        Err(ContractError::InvalidContract(format!(
            "{label} must be {}nonempty NFC strings sorted and unique",
            if require_nonempty {
                "a nonempty set of "
            } else {
                ""
            }
        )))
    } else {
        Ok(())
    }
}

pub fn validate_identifier(value: &str) -> Result<(), ContractError> {
    let mut bytes = value.bytes();
    let Some(first) = bytes.next() else {
        return Err(ContractError::InvalidIdentifier(value.to_owned()));
    };
    if !first.is_ascii_alphanumeric()
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(ContractError::InvalidIdentifier(value.to_owned()));
    }
    Ok(())
}

pub fn canonical_jcs(value: &Value) -> Result<Vec<u8>, ContractError> {
    let mut output = Vec::new();
    write_value(value, &mut output)?;
    Ok(output)
}

pub fn canonical_jcs_of<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>, ContractError> {
    let value = serde_json::to_value(value)
        .map_err(|error| ContractError::InvalidJson(error.to_string()))?;
    canonical_jcs(&value)
}

pub fn parse_canonical_jcs(bytes: &[u8]) -> Result<Value, ContractError> {
    if bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
        return Err(ContractError::NonCanonicalJson);
    }
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|error| ContractError::InvalidJson(error.to_string()))?;
    if canonical_jcs(&value)? != bytes {
        return Err(ContractError::NonCanonicalJson);
    }
    Ok(value)
}

pub fn proof_hash<T: Serialize + ?Sized>(
    domain: &str,
    value: &T,
) -> Result<Sha256HexV1, ContractError> {
    validate_domain(domain)?;
    let canonical = canonical_jcs_of(value)?;
    proof_hash_bytes(domain, &canonical)
}

pub fn proof_hash_bytes(domain: &str, canonical: &[u8]) -> Result<Sha256HexV1, ContractError> {
    validate_domain(domain)?;
    let mut hasher = Sha256::new();
    hasher.update(domain.as_bytes());
    hasher.update([0]);
    hasher.update(canonical);
    Sha256HexV1::parse(format!("{:x}", hasher.finalize()))
}

fn validate_domain(domain: &str) -> Result<(), ContractError> {
    if domain.is_empty() || !domain.is_ascii() || domain.as_bytes().contains(&0) {
        Err(ContractError::InvalidContract(
            "hash domain must be nonempty ASCII without NUL".to_owned(),
        ))
    } else {
        Ok(())
    }
}

fn write_value(value: &Value, output: &mut Vec<u8>) -> Result<(), ContractError> {
    match value {
        Value::Null => output.extend_from_slice(b"null"),
        Value::Bool(true) => output.extend_from_slice(b"true"),
        Value::Bool(false) => output.extend_from_slice(b"false"),
        Value::Number(number) => {
            if let Some(value) = number.as_i64() {
                if !(-(1_i64 << 53) + 1..=(1_i64 << 53) - 1).contains(&value) {
                    return Err(ContractError::InvalidContract(
                        "integer is outside the exact I-JSON range".to_owned(),
                    ));
                }
                output.extend_from_slice(value.to_string().as_bytes());
            } else if let Some(value) = number.as_u64() {
                if value > (1_u64 << 53) - 1 {
                    return Err(ContractError::InvalidContract(
                        "integer is outside the exact I-JSON range".to_owned(),
                    ));
                }
                output.extend_from_slice(value.to_string().as_bytes());
            } else {
                return Err(ContractError::FloatNotPermitted);
            }
        }
        Value::String(value) => {
            validate_nfc(value)?;
            output.extend_from_slice(
                serde_json::to_string(value)
                    .map_err(|error| ContractError::InvalidJson(error.to_string()))?
                    .as_bytes(),
            );
        }
        Value::Array(values) => {
            output.push(b'[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    output.push(b',');
                }
                write_value(value, output)?;
            }
            output.push(b']');
        }
        Value::Object(values) => {
            let mut entries = values.iter().collect::<Vec<_>>();
            for (key, _) in &entries {
                validate_nfc(key)?;
            }
            entries.sort_by(|(left, _), (right, _)| left.encode_utf16().cmp(right.encode_utf16()));
            output.push(b'{');
            for (index, (key, value)) in entries.iter().enumerate() {
                if index > 0 {
                    output.push(b',');
                }
                output.extend_from_slice(
                    serde_json::to_string(key)
                        .map_err(|error| ContractError::InvalidJson(error.to_string()))?
                        .as_bytes(),
                );
                output.push(b':');
                write_value(value, output)?;
            }
            output.push(b'}');
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn proof_hash_adds_exactly_one_separator() {
        let value = json!({"a": 1});
        let actual = proof_hash("kd4.example.v1", &value).expect("hash");
        let mut hasher = Sha256::new();
        hasher.update(b"kd4.example.v1\0{\"a\":1}");
        assert_eq!(actual.as_str(), format!("{:x}", hasher.finalize()));
    }

    #[test]
    fn canonical_parser_rejects_noncanonical_or_float_input() {
        assert_eq!(
            parse_canonical_jcs(br#"{"b":2,"a":1}"#),
            Err(ContractError::NonCanonicalJson)
        );
        assert_eq!(
            parse_canonical_jcs(br#"{"a":1.5}"#),
            Err(ContractError::FloatNotPermitted)
        );
    }
}
