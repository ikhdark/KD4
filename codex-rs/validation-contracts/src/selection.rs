use crate::applicability::HostTokenV1;
use crate::canonical::ContractError;
use crate::canonical::Sha256HexV1;
use crate::canonical::canonical_jcs_of;
use crate::canonical::parse_canonical_jcs;
use crate::canonical::proof_hash;
use crate::canonical::validate_identifier;
use crate::canonical::validate_nfc;
use crate::path::StrictRepositoryPathV1;
use crate::runner::RunnerSelectorV1;
use crate::runner::TestRouteIdV1;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use std::fmt;

macro_rules! contract_id {
    ($name:ident) => {
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn parse(value: impl Into<String>) -> Result<Self, ContractError> {
                let value = value.into();
                validate_identifier(&value)?;
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::parse(value).map_err(serde::de::Error::custom)
            }
        }
    };
}

contract_id!(ActionIdV1);
contract_id!(ValidationIdV1);

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct TestIdV1(String);

impl TestIdV1 {
    pub fn parse(value: impl Into<String>) -> Result<Self, ContractError> {
        let value = value.into();
        if value.is_empty() {
            return Err(ContractError::InvalidContract(
                "test ID must be nonempty".to_owned(),
            ));
        }
        validate_nfc(&value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TestIdV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for TestIdV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum ExecutableIdentityV1 {
    #[serde(rename = "test")]
    Test {
        route_id: TestRouteIdV1,
        test_id: TestIdV1,
        validation_id: ValidationIdV1,
    },
    #[serde(rename = "action")]
    Action {
        action_id: ActionIdV1,
        validation_id: ValidationIdV1,
    },
}

impl ExecutableIdentityV1 {
    pub fn validate(&self) -> Result<(), ContractError> {
        canonical_jcs_of(self).map(|_| ())
    }

    pub fn test_id(&self) -> Option<&TestIdV1> {
        match self {
            Self::Test { test_id, .. } => Some(test_id),
            Self::Action { .. } => None,
        }
    }

    pub fn validation_id(&self) -> &ValidationIdV1 {
        match self {
            Self::Test { validation_id, .. } | Self::Action { validation_id, .. } => validation_id,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum SelectionRequestSelectorV1 {
    #[serde(rename = "test")]
    Test { test_id: TestIdV1 },
    #[serde(rename = "action")]
    Action { action_id: ActionIdV1 },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionRequestV1 {
    pub schema_version: u8,
    pub selectors: Vec<SelectionRequestSelectorV1>,
}

impl SelectionRequestV1 {
    pub const HASH_DOMAIN: &'static str = "kd4.selection-request.v1";
    pub const MAX_CANONICAL_BYTES: usize = 18_432;
    pub const MAX_SELECTORS: usize = 256;
    pub const MAX_TOKEN_BYTES: usize = 24_576;

    pub fn new(selectors: Vec<SelectionRequestSelectorV1>) -> Result<Self, ContractError> {
        let request = Self {
            schema_version: 1,
            selectors,
        };
        request.validate()?;
        Ok(request)
    }

    pub fn validate(&self) -> Result<(), ContractError> {
        if self.schema_version != 1
            || self.selectors.is_empty()
            || self.selectors.len() > Self::MAX_SELECTORS
        {
            return Err(ContractError::InvalidContract(
                "SelectionRequestV1 requires schema_version 1 and a nonempty selection".to_owned(),
            ));
        }
        ensure_sorted_unique_by_jcs(&self.selectors, "selection request selectors")
    }

    pub fn request_sha256(&self) -> Result<Sha256HexV1, ContractError> {
        self.validate()?;
        proof_hash(Self::HASH_DOMAIN, self)
    }

    pub fn encode_token(&self) -> Result<String, ContractError> {
        self.validate()?;
        let canonical = canonical_jcs_of(self)?;
        if canonical.len() > Self::MAX_CANONICAL_BYTES {
            return Err(ContractError::InvalidContract(
                "SelectionRequestV1 exceeds the size limit".to_owned(),
            ));
        }
        Ok(URL_SAFE_NO_PAD.encode(canonical))
    }

    pub fn decode_token(token: &str) -> Result<Self, ContractError> {
        if token.is_empty()
            || token.len() > Self::MAX_TOKEN_BYTES
            || !token
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(ContractError::InvalidContract(
                "invalid unpadded base64url selection token".to_owned(),
            ));
        }
        let raw = URL_SAFE_NO_PAD
            .decode(token)
            .map_err(|error| ContractError::InvalidContract(error.to_string()))?;
        if raw.len() > Self::MAX_CANONICAL_BYTES {
            return Err(ContractError::InvalidContract(
                "SelectionRequestV1 exceeds the size limit".to_owned(),
            ));
        }
        let value = parse_canonical_jcs(&raw)?;
        let request: Self = serde_json::from_value(value)
            .map_err(|error| ContractError::InvalidJson(error.to_string()))?;
        request.validate()?;
        if request.encode_token()? != token {
            return Err(ContractError::InvalidContract(
                "selection token is not canonical".to_owned(),
            ));
        }
        Ok(request)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InventoryAuthorityRefV1 {
    pub path: StrictRepositoryPathV1,
    pub raw_sha256: Sha256HexV1,
    pub semantic_sha256: Sha256HexV1,
    pub self_hash: Sha256HexV1,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedExecutableEntryV1 {
    pub inventory_entry: Box<crate::inventory_v2::ExecutableInventoryEntryV2>,
    pub inventory_entry_semantic_sha256: Sha256HexV1,
}

impl ResolvedExecutableEntryV1 {
    pub fn validate(&self) -> Result<(), ContractError> {
        self.inventory_entry.validate()?;
        if self.inventory_entry.semantic_sha256()? != self.inventory_entry_semantic_sha256 {
            return Err(ContractError::InvalidContract(
                "resolved executable does not bind the full inventory entry".to_owned(),
            ));
        }
        Ok(())
    }

    pub fn identity(&self) -> &ExecutableIdentityV1 {
        &self.inventory_entry.executable_identity
    }
}

pub(crate) fn validate_identity_selector_binding(
    identity: &ExecutableIdentityV1,
    identity_sha256: &Sha256HexV1,
    runner_selector: &RunnerSelectorV1,
    runner_selector_sha256: &Sha256HexV1,
) -> Result<(), ContractError> {
    identity.validate()?;
    runner_selector.validate()?;
    if proof_hash("kd4.executable-identity.v1", identity)? != *identity_sha256
        || proof_hash("kd4.runner-selector.v1", runner_selector)? != *runner_selector_sha256
    {
        return Err(ContractError::InvalidContract(
            "executable identity or runner-selector hash mismatch".to_owned(),
        ));
    }
    let aligned = match (identity, runner_selector) {
        (ExecutableIdentityV1::Test { route_id, .. }, RunnerSelectorV1::RustNextest { .. }) => {
            *route_id == TestRouteIdV1::RustNextest
        }
        (ExecutableIdentityV1::Test { route_id, .. }, RunnerSelectorV1::RustDoctest { .. }) => {
            *route_id == TestRouteIdV1::RustDoctest
        }
        (ExecutableIdentityV1::Test { route_id, .. }, RunnerSelectorV1::PythonUnittest { .. }) => {
            *route_id == TestRouteIdV1::PythonUnittest
        }
        (ExecutableIdentityV1::Test { route_id, .. }, RunnerSelectorV1::PythonPytest { .. }) => {
            *route_id == TestRouteIdV1::PythonPytest
        }
        (ExecutableIdentityV1::Test { route_id, .. }, RunnerSelectorV1::JavascriptJest { .. }) => {
            *route_id == TestRouteIdV1::JavascriptJest
        }
        (
            ExecutableIdentityV1::Test { route_id, .. },
            RunnerSelectorV1::ArgumentCommentLintNative { .. },
        ) => *route_id == TestRouteIdV1::ArgumentCommentLintNative,
        (
            ExecutableIdentityV1::Test { route_id, .. },
            RunnerSelectorV1::WindowsSandboxSmokeNative { .. },
        ) => *route_id == TestRouteIdV1::WindowsSandboxSmokeNative,
        (
            ExecutableIdentityV1::Action { action_id, .. },
            RunnerSelectorV1::NonTestAction {
                action_id: selected,
            },
        ) => action_id == selected,
        _ => false,
    };
    if aligned {
        Ok(())
    } else {
        Err(ContractError::InvalidContract(
            "executable identity route and runner selector disagree".to_owned(),
        ))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionV1 {
    pub activated_policy_sha256: Sha256HexV1,
    pub host: HostTokenV1,
    pub intended_count: u64,
    pub inventory_authority: InventoryAuthorityRefV1,
    pub repository_identity_sha256: Sha256HexV1,
    pub request_sha256: Sha256HexV1,
    pub resolved_entries: Vec<ResolvedExecutableEntryV1>,
    pub resolved_entries_sha256: Sha256HexV1,
    pub schema_version: u8,
    pub target_applicability_sha256: Sha256HexV1,
    pub validation_execution_contract_sha256: Sha256HexV1,
    pub validation_id: ValidationIdV1,
}

impl SelectionV1 {
    pub const HASH_DOMAIN: &'static str = "kd4.selection.v1";
    pub const ENTRY_SET_HASH_DOMAIN: &'static str = "kd4.resolved-executable-entry-set.v1";

    pub fn validate(&self) -> Result<(), ContractError> {
        if self.schema_version != 1
            || self.resolved_entries.is_empty()
            || self.intended_count != self.resolved_entries.len() as u64
        {
            return Err(ContractError::InvalidContract(
                "SelectionV1 count or version mismatch".to_owned(),
            ));
        }
        for entry in &self.resolved_entries {
            entry.validate()?;
            if entry.identity().validation_id() != &self.validation_id {
                return Err(ContractError::InvalidContract(
                    "every selected entry must map to the requested validation".to_owned(),
                ));
            }
        }
        ensure_sorted_unique_by_identity(&self.resolved_entries)?;
        let expected = proof_hash(Self::ENTRY_SET_HASH_DOMAIN, &self.resolved_entries)?;
        if expected != self.resolved_entries_sha256 {
            return Err(ContractError::InvalidContract(
                "resolved entry-set hash mismatch".to_owned(),
            ));
        }
        Ok(())
    }

    pub fn selection_sha256(&self) -> Result<Sha256HexV1, ContractError> {
        self.validate()?;
        proof_hash(Self::HASH_DOMAIN, self)
    }
}

fn ensure_sorted_unique_by_jcs<T: Serialize>(
    values: &[T],
    label: &str,
) -> Result<(), ContractError> {
    let encoded = values
        .iter()
        .map(canonical_jcs_of)
        .collect::<Result<Vec<_>, _>>()?;
    if encoded.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(ContractError::InvalidContract(format!(
            "{label} must be strictly sorted and unique by canonical JSON bytes"
        )));
    }
    Ok(())
}

fn ensure_sorted_unique_by_identity(
    values: &[ResolvedExecutableEntryV1],
) -> Result<(), ContractError> {
    let encoded = values
        .iter()
        .map(|entry| canonical_jcs_of(entry.identity()))
        .collect::<Result<Vec<_>, _>>()?;
    if encoded.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(ContractError::InvalidContract(
            "resolved entries must be strictly sorted and unique by identity".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_request_requires_canonical_selector_order() {
        let test = SelectionRequestSelectorV1::Test {
            test_id: TestIdV1::parse("test.one").expect("test ID"),
        };
        let action = SelectionRequestSelectorV1::Action {
            action_id: ActionIdV1::parse("action.one").expect("action ID"),
        };
        assert!(SelectionRequestV1::new(vec![action.clone(), test.clone()]).is_ok());
        assert!(SelectionRequestV1::new(vec![test, action]).is_err());
    }
}
