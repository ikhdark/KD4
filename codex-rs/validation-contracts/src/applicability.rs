use crate::canonical::ContractError;
use crate::canonical::Sha256HexV1;
use crate::canonical::canonical_jcs_of;
use crate::canonical::proof_hash;
use crate::canonical::validate_nfc;
use crate::canonical::validate_nonempty_nfc;
use crate::inventory_v2::FrozenTestInventoryV2;
use crate::path::StrictRepositoryPathV1;
use crate::selection::ExecutableIdentityV1;
use crate::selection::InventoryAuthorityRefV1;
use base64::Engine;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use hmac::Hmac;
use hmac::Mac;
use serde::Deserialize;
use serde::Serialize;
use sha2::Sha256;
use std::collections::BTreeSet;
use syn::Ident;
use syn::LitStr;
use syn::Token;
use syn::ext::IdentExt;
use syn::parenthesized;
use syn::parse::Parse;
use syn::parse::ParseStream;

const MAX_CFG_INPUT_BYTES: usize = 16_384;
const MAX_CFG_DEPTH: usize = 64;
const MAX_CFG_NODES: usize = 4_096;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HostTokenV1 {
    Darwin,
    Linux,
    Windows,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum RustCfgPredicateV1 {
    True,
    False,
    Flag { name: String },
    KeyValue { key: String, value: String },
    All { predicates: Vec<RustCfgPredicateV1> },
    Any { predicates: Vec<RustCfgPredicateV1> },
    Not { predicate: Box<RustCfgPredicateV1> },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RustCfgExpressionV1 {
    pub root: RustCfgPredicateV1,
    pub schema_version: u8,
    pub semantic_sha256: Sha256HexV1,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum RustCfgAtomV1 {
    Flag { name: String },
    KeyValue { key: String, value: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum PlatformApplicabilityV1 {
    HostSet { required_hosts: Vec<HostTokenV1> },
    RustCfg { expression: RustCfgExpressionV1 },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CargoBuildContextObservationV1 {
    pub actual_cfg_atoms: Vec<RustCfgAtomV1>,
    pub actual_cfg_atoms_sha256: Sha256HexV1,
    pub cargo_profile: String,
    pub cargo_target_context_spec_sha256: Sha256HexV1,
    pub enabled_features: Vec<String>,
    pub invocation_receipt_sha256: Sha256HexV1,
    pub observation_sha256: Sha256HexV1,
    pub schema_version: u8,
    pub target_features: Vec<String>,
    pub target_triple: String,
    pub test_cfg_present: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApplicabilityVerdictV1 {
    Applicable,
    NotApplicable,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplicabilityResultV1 {
    pub cargo_build_context_observation_sha256: Option<Sha256HexV1>,
    pub cargo_target_context_spec_sha256: Option<Sha256HexV1>,
    pub executable_identity_sha256: Sha256HexV1,
    pub host: HostTokenV1,
    pub platform_applicability_sha256: Sha256HexV1,
    pub result_sha256: Sha256HexV1,
    pub rust_cfg_expression_semantic_sha256: Option<Sha256HexV1>,
    pub schema_version: u8,
    pub verdict: ApplicabilityVerdictV1,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetApplicabilityProjectionEntryV1 {
    pub applicability_result: ApplicabilityResultV1,
    pub identity: ExecutableIdentityV1,
    pub identity_sha256: Sha256HexV1,
    pub platform_applicability_sha256: Sha256HexV1,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetApplicabilityProjectionV1 {
    pub entries: Vec<TargetApplicabilityProjectionEntryV1>,
    pub host: HostTokenV1,
    pub inventory_authority: InventoryAuthorityRefV1,
    pub schema_version: u8,
}

/// A target-applicability projection issued by the trusted inventory authority.
///
/// Ledger rows may carry this receipt, but cannot create or alter its authenticated
/// projection without the authority key supplied by the later activation stage.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActiveHostApplicabilityAuthorityBodyV1 {
    pub authority_nonce: String,
    pub inventory_authority: InventoryAuthorityRefV1,
    pub schema_version: u8,
    pub target_applicability_projection: TargetApplicabilityProjectionV1,
    pub target_applicability_projection_sha256: Sha256HexV1,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActiveHostApplicabilityAuthorityV1 {
    pub authentication_tag: String,
    pub authority_sha256: Sha256HexV1,
    pub body: ActiveHostApplicabilityAuthorityBodyV1,
    pub key_id: String,
    pub schema_version: u8,
}

impl RustCfgExpressionV1 {
    pub const HASH_DOMAIN: &'static str = "kd4.rust-cfg-expression.semantic.v1";

    pub fn parse(source: &str) -> Result<Self, ContractError> {
        if source.len() > MAX_CFG_INPUT_BYTES {
            return Err(invalid("Rust cfg expression exceeds 16,384 UTF-8 bytes"));
        }
        let parsed =
            syn::parse_str::<ParsedCfg>(source).map_err(|error| invalid(error.to_string()))?;
        let root = parsed.root.normalized()?;
        let semantic_sha256 = proof_hash(
            Self::HASH_DOMAIN,
            &RustCfgExpressionHashProjectionV1 {
                root: &root,
                schema_version: 1,
            },
        )?;
        Ok(Self {
            root,
            schema_version: 1,
            semantic_sha256,
        })
    }

    pub fn validate(&self) -> Result<(), ContractError> {
        canonical_jcs_of(self)?;
        if self.schema_version != 1 {
            return Err(invalid("Rust cfg expression must use schema version 1"));
        }
        let mut nodes = 0;
        self.root.validate_at_depth(1, &mut nodes)?;
        if self.root.clone().normalized()? != self.root {
            return Err(invalid("Rust cfg expression is not normalized"));
        }
        let expected = proof_hash(
            Self::HASH_DOMAIN,
            &RustCfgExpressionHashProjectionV1 {
                root: &self.root,
                schema_version: self.schema_version,
            },
        )?;
        if expected != self.semantic_sha256 {
            return Err(invalid("Rust cfg expression semantic hash mismatch"));
        }
        Ok(())
    }

    pub fn evaluate(&self, atoms: &[RustCfgAtomV1]) -> Result<bool, ContractError> {
        self.validate()?;
        validate_cfg_atoms(atoms)?;
        Ok(self.root.evaluate(atoms))
    }
}

impl RustCfgPredicateV1 {
    fn normalized(self) -> Result<Self, ContractError> {
        match self {
            Self::All { predicates } => Ok(Self::All {
                predicates: normalize_predicates(predicates)?,
            }),
            Self::Any { predicates } => Ok(Self::Any {
                predicates: normalize_predicates(predicates)?,
            }),
            Self::Not { predicate } => Ok(Self::Not {
                predicate: Box::new(predicate.normalized()?),
            }),
            other => Ok(other),
        }
    }

    fn validate_at_depth(&self, depth: usize, nodes: &mut usize) -> Result<(), ContractError> {
        if depth > MAX_CFG_DEPTH {
            return Err(invalid("Rust cfg expression exceeds depth 64"));
        }
        *nodes += 1;
        if *nodes > MAX_CFG_NODES {
            return Err(invalid("Rust cfg expression exceeds 4,096 AST nodes"));
        }
        match self {
            Self::Flag { name } => validate_cfg_identifier(name),
            Self::KeyValue { key, value } => {
                validate_cfg_identifier(key)?;
                validate_nfc(value)
            }
            Self::All { predicates } | Self::Any { predicates } => {
                for predicate in predicates {
                    predicate.validate_at_depth(depth + 1, nodes)?;
                }
                Ok(())
            }
            Self::Not { predicate } => predicate.validate_at_depth(depth + 1, nodes),
            Self::True | Self::False => Ok(()),
        }
    }

    fn evaluate(&self, atoms: &[RustCfgAtomV1]) -> bool {
        match self {
            Self::True => true,
            Self::False => false,
            Self::Flag { name } => atoms.iter().any(|atom| matches!(atom, RustCfgAtomV1::Flag { name: found } if found == name)),
            Self::KeyValue { key, value } => atoms.iter().any(|atom| matches!(atom, RustCfgAtomV1::KeyValue { key: found_key, value: found_value } if found_key == key && found_value == value)),
            Self::All { predicates } => predicates.iter().all(|predicate| predicate.evaluate(atoms)),
            Self::Any { predicates } => predicates.iter().any(|predicate| predicate.evaluate(atoms)),
            Self::Not { predicate } => !predicate.evaluate(atoms),
        }
    }
}

impl RustCfgAtomV1 {
    pub fn parse(source: &str) -> Result<Self, ContractError> {
        match RustCfgExpressionV1::parse(source)?.root {
            RustCfgPredicateV1::Flag { name } => Ok(Self::Flag { name }),
            RustCfgPredicateV1::KeyValue { key, value } => Ok(Self::KeyValue { key, value }),
            _ => Err(invalid(
                "cfg stdout line must contain exactly one flag or key-value atom",
            )),
        }
    }
}

pub fn parse_cfg_stdout(stdout: &str) -> Result<Vec<RustCfgAtomV1>, ContractError> {
    let mut atoms = stdout
        .lines()
        .filter(|line| !line.is_empty())
        .map(RustCfgAtomV1::parse)
        .collect::<Result<Vec<_>, _>>()?;
    let mut keyed = atoms
        .drain(..)
        .map(|atom| Ok((canonical_jcs_of(&atom)?, atom)))
        .collect::<Result<Vec<_>, ContractError>>()?;
    keyed.sort_by(|left, right| left.0.cmp(&right.0));
    if keyed.windows(2).any(|pair| pair[0].0 == pair[1].0) {
        return Err(invalid("duplicate Rust cfg stdout atom"));
    }
    let atoms = keyed.into_iter().map(|(_, atom)| atom).collect::<Vec<_>>();
    validate_cfg_atoms(&atoms)?;
    Ok(atoms)
}

pub fn rust_cfg_atom_set_sha256(atoms: &[RustCfgAtomV1]) -> Result<Sha256HexV1, ContractError> {
    validate_cfg_atoms(atoms)?;
    proof_hash("kd4.rust-cfg-atom-set.v1", atoms)
}

impl PlatformApplicabilityV1 {
    pub const HASH_DOMAIN: &'static str = "kd4.platform-applicability.v1";

    pub fn validate(&self) -> Result<(), ContractError> {
        canonical_jcs_of(self)?;
        match self {
            Self::HostSet { required_hosts }
                if required_hosts.is_empty()
                    || required_hosts.windows(2).any(|pair| pair[0] >= pair[1]) =>
            {
                Err(invalid(
                    "required_hosts must be nonempty, sorted, and unique",
                ))
            }
            Self::HostSet { .. } => Ok(()),
            Self::RustCfg { expression } => expression.validate(),
        }
    }

    pub fn semantic_sha256(&self) -> Result<Sha256HexV1, ContractError> {
        self.validate()?;
        proof_hash(Self::HASH_DOMAIN, self)
    }
}

impl CargoBuildContextObservationV1 {
    pub const HASH_DOMAIN: &'static str = "kd4.cargo-build-context-observation.v1";

    pub fn validate(&self) -> Result<(), ContractError> {
        canonical_jcs_of(self)?;
        if self.schema_version != 1
            || self.cargo_profile != "test"
            || self.target_triple.is_empty()
            || !self.test_cfg_present
        {
            return Err(invalid("invalid Cargo build-context observation envelope"));
        }
        validate_nonempty_nfc(&self.target_triple, "Cargo target triple")?;
        validate_cfg_atoms(&self.actual_cfg_atoms)?;
        if rust_cfg_atom_set_sha256(&self.actual_cfg_atoms)? != self.actual_cfg_atoms_sha256 {
            return Err(invalid("Cargo build-context cfg atom-set hash mismatch"));
        }
        let enabled_features = atom_values(&self.actual_cfg_atoms, "feature");
        let target_features = atom_values(&self.actual_cfg_atoms, "target_feature");
        let test_cfg_present = self
            .actual_cfg_atoms
            .iter()
            .any(|atom| matches!(atom, RustCfgAtomV1::Flag { name } if name == "test"));
        if self.enabled_features != enabled_features
            || self.target_features != target_features
            || self.test_cfg_present != test_cfg_present
        {
            return Err(invalid(
                "Cargo build-context derived cfg fields do not match its atom set",
            ));
        }
        let expected = proof_hash(
            Self::HASH_DOMAIN,
            &CargoBuildContextObservationHashProjectionV1 {
                actual_cfg_atoms: &self.actual_cfg_atoms,
                actual_cfg_atoms_sha256: &self.actual_cfg_atoms_sha256,
                cargo_profile: &self.cargo_profile,
                cargo_target_context_spec_sha256: &self.cargo_target_context_spec_sha256,
                enabled_features: &self.enabled_features,
                invocation_receipt_sha256: &self.invocation_receipt_sha256,
                schema_version: self.schema_version,
                target_features: &self.target_features,
                target_triple: &self.target_triple,
                test_cfg_present: self.test_cfg_present,
            },
        )?;
        if expected != self.observation_sha256 {
            return Err(invalid("Cargo build-context observation hash mismatch"));
        }
        Ok(())
    }
}

impl ApplicabilityResultV1 {
    pub const HASH_DOMAIN: &'static str = "kd4.applicability-result.v1";

    pub fn validate(&self) -> Result<(), ContractError> {
        canonical_jcs_of(self)?;
        if self.schema_version != 1 {
            return Err(invalid("applicability result must use schema version 1"));
        }
        let expected = proof_hash(Self::HASH_DOMAIN, &self.hash_projection())?;
        if expected != self.result_sha256 {
            return Err(invalid("applicability result hash mismatch"));
        }
        Ok(())
    }

    pub fn validate_for(
        &self,
        rust_route: bool,
        platform: &PlatformApplicabilityV1,
    ) -> Result<(), ContractError> {
        self.validate()?;
        platform.validate()?;
        if platform.semantic_sha256()? != self.platform_applicability_sha256 {
            return Err(invalid(
                "applicability result does not bind its platform applicability",
            ));
        }
        let valid = match (rust_route, platform) {
            (true, PlatformApplicabilityV1::RustCfg { expression }) => {
                self.cargo_target_context_spec_sha256.is_some()
                    && self.cargo_build_context_observation_sha256.is_some()
                    && self.rust_cfg_expression_semantic_sha256.as_ref()
                        == Some(&expression.semantic_sha256)
            }
            (true, PlatformApplicabilityV1::HostSet { .. }) => {
                self.cargo_target_context_spec_sha256.is_some()
                    && self.cargo_build_context_observation_sha256.is_none()
                    && self.rust_cfg_expression_semantic_sha256.is_none()
            }
            (false, PlatformApplicabilityV1::HostSet { .. }) => {
                self.cargo_target_context_spec_sha256.is_none()
                    && self.cargo_build_context_observation_sha256.is_none()
                    && self.rust_cfg_expression_semantic_sha256.is_none()
            }
            (false, PlatformApplicabilityV1::RustCfg { .. }) => false,
        };
        if valid {
            Ok(())
        } else {
            Err(invalid("invalid route/platform applicability-result shape"))
        }
    }

    fn hash_projection(&self) -> ApplicabilityResultHashProjectionV1<'_> {
        ApplicabilityResultHashProjectionV1 {
            cargo_build_context_observation_sha256: self
                .cargo_build_context_observation_sha256
                .as_ref(),
            cargo_target_context_spec_sha256: self.cargo_target_context_spec_sha256.as_ref(),
            executable_identity_sha256: &self.executable_identity_sha256,
            host: self.host,
            platform_applicability_sha256: &self.platform_applicability_sha256,
            rust_cfg_expression_semantic_sha256: self.rust_cfg_expression_semantic_sha256.as_ref(),
            schema_version: self.schema_version,
            verdict: self.verdict,
        }
    }
}

impl TargetApplicabilityProjectionV1 {
    pub const HASH_DOMAIN: &'static str = "kd4.target-applicability-projection.v1";

    pub fn validate(&self) -> Result<(), ContractError> {
        if self.schema_version != 1 || self.entries.is_empty() {
            return Err(invalid(
                "target applicability projection must be version 1 and nonempty",
            ));
        }
        let identities = self
            .entries
            .iter()
            .map(|entry| canonical_jcs_of(&entry.identity))
            .collect::<Result<Vec<_>, _>>()?;
        if identities.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(invalid(
                "target applicability entries must be sorted and unique by identity",
            ));
        }
        for entry in &self.entries {
            entry.identity.validate()?;
            entry.applicability_result.validate()?;
            if proof_hash("kd4.executable-identity.v1", &entry.identity)? != entry.identity_sha256
                || entry.applicability_result.executable_identity_sha256 != entry.identity_sha256
                || entry.applicability_result.platform_applicability_sha256
                    != entry.platform_applicability_sha256
                || entry.applicability_result.host != self.host
            {
                return Err(invalid("target applicability entry binding mismatch"));
            }
        }
        Ok(())
    }

    pub fn semantic_sha256(&self) -> Result<Sha256HexV1, ContractError> {
        self.validate()?;
        proof_hash(Self::HASH_DOMAIN, self)
    }
}

impl ActiveHostApplicabilityAuthorityV1 {
    pub const HASH_DOMAIN: &'static str = "kd4.active-host-applicability-authority.v1";
    pub const AUTHENTICATION_DOMAIN: &'static str =
        "kd4.active-host-applicability-authority.authentication.v1";

    pub fn validate(&self) -> Result<(), ContractError> {
        self.validate_inner(None)
    }

    pub fn validate_authenticated(&self, authentication_key: &[u8]) -> Result<(), ContractError> {
        self.validate_inner(Some(authentication_key))
    }

    fn validate_inner(&self, authentication_key: Option<&[u8]>) -> Result<(), ContractError> {
        canonical_jcs_of(self)?;
        if self.schema_version != 1 || self.body.schema_version != 1 {
            return Err(invalid(
                "active-host applicability authority must use schema version 1",
            ));
        }
        let key_id = uuid::Uuid::parse_str(&self.key_id)
            .map_err(|_| invalid("active-host applicability authority key ID must be a UUID"))?;
        if key_id.to_string() != self.key_id {
            return Err(invalid(
                "active-host applicability authority key ID must be a lowercase hyphenated UUID",
            ));
        }
        validate_standard_base64_32(&self.authentication_tag, "authentication tag")?;
        validate_standard_base64_32(&self.body.authority_nonce, "authority nonce")?;
        self.body.target_applicability_projection.validate()?;
        if self.body.inventory_authority
            != self
                .body
                .target_applicability_projection
                .inventory_authority
        {
            return Err(invalid(
                "active-host applicability authority does not bind the projection inventory authority",
            ));
        }
        if self
            .body
            .target_applicability_projection
            .semantic_sha256()?
            != self.body.target_applicability_projection_sha256
        {
            return Err(invalid(
                "active-host applicability authority projection hash mismatch",
            ));
        }
        let expected_authority = proof_hash(Self::HASH_DOMAIN, &self.body)?;
        if expected_authority != self.authority_sha256 {
            return Err(invalid("active-host applicability authority hash mismatch"));
        }
        if let Some(authentication_key) = authentication_key {
            if authentication_key.len() != 32 {
                return Err(invalid(
                    "active-host applicability authentication key must be 32 bytes",
                ));
            }
            let payload = ActiveHostApplicabilityAuthenticationProjectionV1 {
                authority_sha256: &self.authority_sha256,
                body: &self.body,
                key_id: &self.key_id,
                schema_version: self.schema_version,
            };
            let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(authentication_key)
                .map_err(|_| invalid("invalid active-host applicability authentication key"))?;
            mac.update(Self::AUTHENTICATION_DOMAIN.as_bytes());
            mac.update(&[0]);
            mac.update(&canonical_jcs_of(&payload)?);
            let supplied = STANDARD_NO_PAD
                .decode(&self.authentication_tag)
                .map_err(|_| invalid("invalid active-host applicability authentication tag"))?;
            mac.verify_slice(&supplied)
                .map_err(|_| invalid("active-host applicability authentication mismatch"))?;
        }
        Ok(())
    }
}

/// Key-holding authority that can only issue a projection derived from a
/// validated Inventory V2 document and validated Cargo observations.
#[derive(Clone, Debug)]
pub struct ActiveHostApplicabilityIssuerV1 {
    authentication_key: [u8; 32],
    key_id: String,
    cargo_observations: Vec<CargoBuildContextObservationV1>,
}

impl ActiveHostApplicabilityIssuerV1 {
    pub fn new(
        key_id: String,
        authentication_key: [u8; 32],
        cargo_observations: Vec<CargoBuildContextObservationV1>,
    ) -> Result<Self, ContractError> {
        let parsed = uuid::Uuid::parse_str(&key_id)
            .map_err(|_| invalid("active-host issuer key ID must be a UUID"))?;
        if parsed.to_string() != key_id {
            return Err(invalid(
                "active-host issuer key ID must be a lowercase hyphenated UUID",
            ));
        }
        for observation in &cargo_observations {
            observation.validate()?;
        }
        let mut contexts = cargo_observations
            .iter()
            .map(|item| item.cargo_target_context_spec_sha256.clone())
            .collect::<Vec<_>>();
        contexts.sort();
        if contexts.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(invalid("duplicate trusted Cargo observation context"));
        }
        Ok(Self {
            authentication_key,
            key_id,
            cargo_observations,
        })
    }

    pub fn issue(
        &self,
        inventory: &FrozenTestInventoryV2,
        host: HostTokenV1,
        authority_nonce: String,
    ) -> Result<ActiveHostApplicabilityAuthorityV1, ContractError> {
        inventory.validate()?;
        validate_standard_base64_32(&authority_nonce, "authority nonce")?;
        let inventory_authority = InventoryAuthorityRefV1 {
            path: StrictRepositoryPathV1::parse(
                ".codex/validation/frozen-test-inventory-v2.json".to_owned(),
            )?,
            raw_sha256: inventory.authority.raw_sha256.clone(),
            semantic_sha256: inventory.authority.semantic_sha256.clone(),
            self_hash: inventory.authority.self_hash.clone(),
        };
        let mut entries = Vec::with_capacity(inventory.declaration_universe.len());
        for declaration in &inventory.declaration_universe {
            let entry = declaration.entry();
            let observation = match &entry.platform_applicability {
                PlatformApplicabilityV1::RustCfg { .. } => {
                    let context = entry
                        .cargo_target_context_spec_sha256
                        .as_ref()
                        .ok_or_else(|| invalid("Rust cfg inventory entry has no Cargo context"))?;
                    Some(
                        self.cargo_observations
                            .iter()
                            .find(|item| &item.cargo_target_context_spec_sha256 == context)
                            .ok_or_else(|| {
                                invalid(
                                    "trusted issuer lacks a Cargo observation for Rust cfg context",
                                )
                            })?,
                    )
                }
                PlatformApplicabilityV1::HostSet { .. } => None,
            };
            let result = evaluate_applicability(
                &entry.executable_identity,
                &entry.platform_applicability,
                host,
                entry.cargo_target_context_spec_sha256.clone(),
                observation,
            )?;
            entries.push(TargetApplicabilityProjectionEntryV1 {
                applicability_result: result,
                identity: entry.executable_identity.clone(),
                identity_sha256: entry.executable_identity_sha256.clone(),
                platform_applicability_sha256: entry.platform_applicability_sha256.clone(),
            });
        }
        entries.sort_by(|left, right| {
            canonical_jcs_of(&left.identity)
                .expect("validated identity canonicalizes")
                .cmp(&canonical_jcs_of(&right.identity).expect("validated identity canonicalizes"))
        });
        let projection = TargetApplicabilityProjectionV1 {
            entries,
            host,
            inventory_authority: inventory_authority.clone(),
            schema_version: 1,
        };
        let body = ActiveHostApplicabilityAuthorityBodyV1 {
            authority_nonce,
            inventory_authority,
            schema_version: 1,
            target_applicability_projection_sha256: projection.semantic_sha256()?,
            target_applicability_projection: projection,
        };
        let authority_sha256 = proof_hash(ActiveHostApplicabilityAuthorityV1::HASH_DOMAIN, &body)?;
        let payload = ActiveHostApplicabilityAuthenticationProjectionV1 {
            authority_sha256: &authority_sha256,
            body: &body,
            key_id: &self.key_id,
            schema_version: 1,
        };
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&self.authentication_key)
            .map_err(|_| invalid("invalid active-host applicability issuer key"))?;
        mac.update(ActiveHostApplicabilityAuthorityV1::AUTHENTICATION_DOMAIN.as_bytes());
        mac.update(&[0]);
        mac.update(&canonical_jcs_of(&payload)?);
        let authority = ActiveHostApplicabilityAuthorityV1 {
            authentication_tag: STANDARD_NO_PAD.encode(mac.finalize().into_bytes()),
            authority_sha256,
            body,
            key_id: self.key_id.clone(),
            schema_version: 1,
        };
        authority.validate_authenticated(&self.authentication_key)?;
        Ok(authority)
    }

    pub fn validate_complete_authority(
        &self,
        authority: &ActiveHostApplicabilityAuthorityV1,
        inventory: &FrozenTestInventoryV2,
    ) -> Result<(), ContractError> {
        authority.validate_authenticated(&self.authentication_key)?;
        if authority.key_id != self.key_id {
            return Err(invalid(
                "active-host authority uses an untrusted issuer key",
            ));
        }
        let expected = self.issue(
            inventory,
            authority.body.target_applicability_projection.host,
            authority.body.authority_nonce.clone(),
        )?;
        if authority != &expected {
            return Err(invalid(
                "active-host authority is not the issuer-derived complete inventory projection",
            ));
        }
        Ok(())
    }
}

fn validate_standard_base64_32(value: &str, label: &str) -> Result<(), ContractError> {
    let decoded = STANDARD_NO_PAD
        .decode(value)
        .map_err(|_| invalid(format!("{label} must be unpadded standard base64")))?;
    if decoded.len() != 32 || STANDARD_NO_PAD.encode(decoded) != value {
        return Err(invalid(format!(
            "{label} must be canonical unpadded base64 for exactly 32 bytes"
        )));
    }
    Ok(())
}

pub fn evaluate_applicability(
    identity: &ExecutableIdentityV1,
    platform: &PlatformApplicabilityV1,
    host: HostTokenV1,
    cargo_target_context_spec_sha256: Option<Sha256HexV1>,
    observation: Option<&CargoBuildContextObservationV1>,
) -> Result<ApplicabilityResultV1, ContractError> {
    identity.validate()?;
    platform.validate()?;
    if let Some(observation) = observation {
        observation.validate()?;
    }
    let rust_route = matches!(
        identity,
        ExecutableIdentityV1::Test {
            route_id: crate::runner::TestRouteIdV1::RustNextest
                | crate::runner::TestRouteIdV1::RustDoctest,
            ..
        }
    );
    let (verdict, observation_hash, expression_hash) = match platform {
        PlatformApplicabilityV1::HostSet { required_hosts } => (
            if required_hosts.contains(&host) {
                ApplicabilityVerdictV1::Applicable
            } else {
                ApplicabilityVerdictV1::NotApplicable
            },
            None,
            None,
        ),
        PlatformApplicabilityV1::RustCfg { expression } if rust_route => {
            let observation = observation.ok_or_else(|| {
                invalid("Rust cfg applicability requires an authenticated Cargo observation")
            })?;
            if Some(&observation.cargo_target_context_spec_sha256)
                != cargo_target_context_spec_sha256.as_ref()
            {
                return Err(invalid(
                    "Rust cfg applicability context/observation mismatch",
                ));
            }
            (
                if expression.evaluate(&observation.actual_cfg_atoms)? {
                    ApplicabilityVerdictV1::Applicable
                } else {
                    ApplicabilityVerdictV1::NotApplicable
                },
                Some(observation.observation_sha256.clone()),
                Some(expression.semantic_sha256.clone()),
            )
        }
        PlatformApplicabilityV1::RustCfg { .. } => {
            return Err(invalid(
                "non-Rust entries cannot use Rust cfg applicability",
            ));
        }
    };
    let mut result = ApplicabilityResultV1 {
        cargo_build_context_observation_sha256: observation_hash,
        cargo_target_context_spec_sha256,
        executable_identity_sha256: proof_hash("kd4.executable-identity.v1", identity)?,
        host,
        platform_applicability_sha256: platform.semantic_sha256()?,
        result_sha256: Sha256HexV1::parse("0".repeat(64))?,
        rust_cfg_expression_semantic_sha256: expression_hash,
        schema_version: 1,
        verdict,
    };
    result.result_sha256 = proof_hash(
        ApplicabilityResultV1::HASH_DOMAIN,
        &result.hash_projection(),
    )?;
    result.validate_for(rust_route, platform)?;
    Ok(result)
}

struct ParsedCfg {
    root: RustCfgPredicateV1,
}

impl Parse for ParsedCfg {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let mut nodes = 0;
        let root = parse_predicate(input, 1, &mut nodes)?;
        if !input.is_empty() {
            return Err(input.error("trailing tokens are not permitted"));
        }
        Ok(Self { root })
    }
}

fn parse_predicate(
    input: ParseStream<'_>,
    depth: usize,
    nodes: &mut usize,
) -> syn::Result<RustCfgPredicateV1> {
    if depth > MAX_CFG_DEPTH {
        return Err(input.error("Rust cfg expression exceeds depth 64"));
    }
    *nodes += 1;
    if *nodes > MAX_CFG_NODES {
        return Err(input.error("Rust cfg expression exceeds 4,096 AST nodes"));
    }
    let identifier: Ident = input.call(Ident::parse_any)?;
    let name = identifier.to_string();
    if name == "true" {
        return Ok(RustCfgPredicateV1::True);
    }
    if name == "false" {
        return Ok(RustCfgPredicateV1::False);
    }
    if !is_cfg_identifier(&name) {
        return Err(syn::Error::new(
            identifier.span(),
            "invalid Rust cfg identifier",
        ));
    }
    let reserved = matches!(name.as_str(), "all" | "any" | "not" | "true" | "false");
    if input.peek(syn::token::Paren) {
        let content;
        parenthesized!(content in input);
        return match name.as_str() {
            "all" | "any" => {
                let mut predicates = Vec::new();
                while !content.is_empty() {
                    predicates.push(parse_predicate(&content, depth + 1, nodes)?);
                    if content.is_empty() {
                        break;
                    }
                    content.parse::<Token![,]>()?;
                }
                if name == "all" {
                    Ok(RustCfgPredicateV1::All { predicates })
                } else {
                    Ok(RustCfgPredicateV1::Any { predicates })
                }
            }
            "not" => {
                let predicate = parse_predicate(&content, depth + 1, nodes)?;
                if !content.is_empty() {
                    return Err(content.error("not accepts exactly one predicate"));
                }
                Ok(RustCfgPredicateV1::Not {
                    predicate: Box::new(predicate),
                })
            }
            _ => Err(syn::Error::new(
                identifier.span(),
                "unknown Rust cfg list operator",
            )),
        };
    }
    if reserved {
        return Err(syn::Error::new(identifier.span(), "reserved Rust cfg name"));
    }
    if input.peek(Token![=]) {
        input.parse::<Token![=]>()?;
        let value: LitStr = input.parse()?;
        let value = value.value();
        if !unicode_normalization::is_nfc(&value) {
            return Err(input.error("Rust cfg string value must be NFC"));
        }
        Ok(RustCfgPredicateV1::KeyValue { key: name, value })
    } else {
        Ok(RustCfgPredicateV1::Flag { name })
    }
}

fn normalize_predicates(
    predicates: Vec<RustCfgPredicateV1>,
) -> Result<Vec<RustCfgPredicateV1>, ContractError> {
    let mut keyed = predicates
        .into_iter()
        .map(|predicate| {
            let predicate = predicate.normalized()?;
            Ok((canonical_jcs_of(&predicate)?, predicate))
        })
        .collect::<Result<Vec<_>, ContractError>>()?;
    keyed.sort_by(|left, right| left.0.cmp(&right.0));
    keyed.dedup_by(|left, right| left.0 == right.0);
    Ok(keyed.into_iter().map(|(_, predicate)| predicate).collect())
}

fn validate_cfg_identifier(value: &str) -> Result<(), ContractError> {
    if is_cfg_identifier(value) && !matches!(value, "all" | "any" | "not" | "true" | "false") {
        Ok(())
    } else {
        Err(invalid(format!(
            "invalid or reserved Rust cfg identifier: {value}"
        )))
    }
}

fn is_cfg_identifier(value: &str) -> bool {
    let mut bytes = value.bytes();
    matches!(bytes.next(), Some(first) if first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn validate_cfg_atoms(atoms: &[RustCfgAtomV1]) -> Result<(), ContractError> {
    if atoms.is_empty() {
        return Err(invalid("Rust cfg atom set must be nonempty"));
    }
    let mut encoded = Vec::with_capacity(atoms.len());
    for atom in atoms {
        match atom {
            RustCfgAtomV1::Flag { name } => validate_cfg_identifier(name)?,
            RustCfgAtomV1::KeyValue { key, value } => {
                validate_cfg_identifier(key)?;
                validate_nfc(value)?;
            }
        }
        encoded.push(canonical_jcs_of(atom)?);
    }
    if encoded.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(invalid("Rust cfg atoms must be strictly sorted and unique"));
    }
    Ok(())
}

fn atom_values(atoms: &[RustCfgAtomV1], key: &str) -> Vec<String> {
    atoms
        .iter()
        .filter_map(|atom| match atom {
            RustCfgAtomV1::KeyValue { key: found, value } if found == key => Some(value.clone()),
            _ => None,
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn invalid(message: impl Into<String>) -> ContractError {
    ContractError::InvalidContract(message.into())
}

#[derive(Serialize)]
struct RustCfgExpressionHashProjectionV1<'a> {
    root: &'a RustCfgPredicateV1,
    schema_version: u8,
}

#[derive(Serialize)]
struct CargoBuildContextObservationHashProjectionV1<'a> {
    actual_cfg_atoms: &'a [RustCfgAtomV1],
    actual_cfg_atoms_sha256: &'a Sha256HexV1,
    cargo_profile: &'a str,
    cargo_target_context_spec_sha256: &'a Sha256HexV1,
    enabled_features: &'a [String],
    invocation_receipt_sha256: &'a Sha256HexV1,
    schema_version: u8,
    target_features: &'a [String],
    target_triple: &'a str,
    test_cfg_present: bool,
}

#[derive(Serialize)]
struct ApplicabilityResultHashProjectionV1<'a> {
    cargo_build_context_observation_sha256: Option<&'a Sha256HexV1>,
    cargo_target_context_spec_sha256: Option<&'a Sha256HexV1>,
    executable_identity_sha256: &'a Sha256HexV1,
    host: HostTokenV1,
    platform_applicability_sha256: &'a Sha256HexV1,
    rust_cfg_expression_semantic_sha256: Option<&'a Sha256HexV1>,
    schema_version: u8,
    verdict: ApplicabilityVerdictV1,
}

#[derive(Serialize)]
struct ActiveHostApplicabilityAuthenticationProjectionV1<'a> {
    authority_sha256: &'a Sha256HexV1,
    body: &'a ActiveHostApplicabilityAuthorityBodyV1,
    key_id: &'a str,
    schema_version: u8,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_normalizes_and_keeps_nested_structure() {
        let expression = RustCfgExpressionV1::parse(
            r#"all(windows,target_os="windows",windows,any(),all(all(unix)),not(not(test)))"#,
        )
        .unwrap();
        expression.validate().unwrap();
        let RustCfgPredicateV1::All { predicates } = expression.root else {
            panic!()
        };
        assert_eq!(predicates.len(), 5);
        assert!(predicates.iter().any(|value| matches!(value, RustCfgPredicateV1::All { predicates } if matches!(predicates.as_slice(), [RustCfgPredicateV1::All { .. }]))));
        assert!(predicates.iter().any(|value| matches!(value, RustCfgPredicateV1::Not { predicate } if matches!(predicate.as_ref(), RustCfgPredicateV1::Not { .. }))));
        let atoms = [RustCfgAtomV1::Flag {
            name: "test".to_owned(),
        }];
        assert!(
            RustCfgExpressionV1::parse("all()")
                .unwrap()
                .evaluate(&atoms)
                .unwrap()
        );
        assert!(
            !RustCfgExpressionV1::parse("any()")
                .unwrap()
                .evaluate(&atoms)
                .unwrap()
        );
    }

    #[test]
    fn parser_rejects_closed_grammar_and_limits() {
        for invalid in [
            "cfg(windows)",
            "all",
            "not",
            "true=\"x\"",
            "target_os=1",
            "target_os=b\"windows\"",
            "unknown(windows)",
            "std::windows",
            "windows!()",
            "r#windows",
            "not(windows,unix)",
            "windows trailing",
        ] {
            assert!(RustCfgExpressionV1::parse(invalid).is_err(), "{invalid}");
        }
        assert!(RustCfgExpressionV1::parse(&"x".repeat(MAX_CFG_INPUT_BYTES + 1)).is_err());
        let too_deep = format!("{}windows{}", "not(".repeat(64), ")".repeat(64));
        assert!(RustCfgExpressionV1::parse(&too_deep).is_err());
        assert!(RustCfgExpressionV1::parse("target_os=\"e\\u{301}\"").is_err());
    }

    #[test]
    fn stdout_atoms_reject_compounds_duplicates_and_empty() {
        assert!(parse_cfg_stdout("").is_err());
        assert!(parse_cfg_stdout("windows\nwindows\n").is_err());
        assert!(parse_cfg_stdout("all(windows)\n").is_err());
        assert_eq!(
            parse_cfg_stdout("windows\ntarget_os=\"windows\"\n")
                .unwrap()
                .len(),
            2
        );
    }
}
