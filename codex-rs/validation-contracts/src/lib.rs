//! Pure, production-dormant wire contracts for KD4 validation policy.
//!
//! This crate deliberately performs no process launch, filesystem access, repository
//! mutation, or proof-state publication. Runtime owners consume these closed types in
//! later activation stages.

pub mod applicability;
pub mod canonical;
pub mod inventory_v2;
pub mod path;
pub mod receipts;
pub mod recovery;
pub mod runner;
pub mod selection;

pub use canonical::ContractError;
pub use canonical::MustBeNullV1;
pub use canonical::ProofHashV1;
pub use canonical::Sha256HexV1;
pub use canonical::canonical_jcs;
pub use canonical::parse_canonical_jcs;
pub use canonical::proof_hash;
