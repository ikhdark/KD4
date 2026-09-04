use crate::canonical::ContractError;
use crate::canonical::validate_nfc;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct StrictRepositoryPathV1(String);

impl StrictRepositoryPathV1 {
    pub fn parse(value: impl Into<String>) -> Result<Self, ContractError> {
        let value = value.into();
        validate_nfc(&value)?;
        let bytes = value.as_bytes();
        let has_drive_prefix =
            bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
        if value.is_empty()
            || value.starts_with('/')
            || value.starts_with('\\')
            || value.contains('\\')
            || value.contains(':')
            || has_drive_prefix
            || value
                .split('/')
                .any(|part| part.is_empty() || matches!(part, "." | ".."))
        {
            return Err(ContractError::InvalidRepositoryPath(value));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for StrictRepositoryPathV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for StrictRepositoryPathV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_repository_paths_reject_escapes_and_ambiguous_spelling() {
        for invalid in ["", "/a", "C:/a", "a\\b", "a//b", "a/./b", "a/../b"] {
            assert!(StrictRepositoryPathV1::parse(invalid).is_err(), "{invalid}");
        }
        assert_eq!(
            StrictRepositoryPathV1::parse("scripts/example.py")
                .expect("valid path")
                .as_str(),
            "scripts/example.py"
        );
    }
}
