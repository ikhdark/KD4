use crate::canonical::ContractError;
use crate::canonical::Sha256HexV1;
use crate::canonical::parse_canonical_jcs;
use serde::Deserialize;
use sha2::Digest;
use sha2::Sha256;
use std::ops::Range;
use thiserror::Error;

pub const FOCUSED_EVIDENCE_HEADER_LEN: usize = 28;
pub const FOCUSED_EVIDENCE_ACK_LEN: usize = 48;
pub const FOCUSED_EVIDENCE_MANIFEST_MAX_BYTES: usize = 16 * 1024;
pub const FOCUSED_EVIDENCE_PAYLOAD_MAX_BYTES: u64 = 128 * 1024 * 1024;

const FOCUSED_EVIDENCE_MAGIC: &[u8; 8] = b"KD4EVID1";
const FOCUSED_EVIDENCE_ACK_MAGIC: &[u8; 8] = b"KD4EVACK";
const FOCUSED_EVIDENCE_VERSION: u8 = 1;
const FOCUSED_EVIDENCE_FLAGS: u8 = 0;
const FOCUSED_EVIDENCE_MEMBER_COUNT: u16 = 6;
const FOCUSED_EVIDENCE_RESERVED: u32 = 0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FocusedEvidenceMemberV1 {
    Catalog,
    Process,
    UnittestCollect,
    UnittestExec,
    PytestCollect,
    PytestExec,
}

impl FocusedEvidenceMemberV1 {
    const ORDERED: [Self; FOCUSED_EVIDENCE_MEMBER_COUNT as usize] = [
        Self::Catalog,
        Self::Process,
        Self::UnittestCollect,
        Self::UnittestExec,
        Self::PytestCollect,
        Self::PytestExec,
    ];

    const fn index(self) -> usize {
        match self {
            Self::Catalog => 0,
            Self::Process => 1,
            Self::UnittestCollect => 2,
            Self::UnittestExec => 3,
            Self::PytestCollect => 4,
            Self::PytestExec => 5,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Catalog => "catalog",
            Self::Process => "process",
            Self::UnittestCollect => "unittest_collect",
            Self::UnittestExec => "unittest_exec",
            Self::PytestCollect => "pytest_collect",
            Self::PytestExec => "pytest_exec",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FocusedEvidenceHeaderV1 {
    raw: [u8; FOCUSED_EVIDENCE_HEADER_LEN],
    manifest_len: usize,
    payload_len: usize,
}

impl FocusedEvidenceHeaderV1 {
    pub fn parse(raw: &[u8]) -> Result<Self, FocusedEvidenceHeaderError> {
        if raw.len() != FOCUSED_EVIDENCE_HEADER_LEN {
            return Err(FocusedEvidenceHeaderError::InvalidLength { actual: raw.len() });
        }
        let raw: [u8; FOCUSED_EVIDENCE_HEADER_LEN] = raw
            .try_into()
            .map_err(|_| FocusedEvidenceHeaderError::InvalidLength { actual: raw.len() })?;
        if &raw[..8] != FOCUSED_EVIDENCE_MAGIC {
            return Err(FocusedEvidenceHeaderError::InvalidMagic);
        }
        if raw[8] != FOCUSED_EVIDENCE_VERSION {
            return Err(FocusedEvidenceHeaderError::UnsupportedVersion(raw[8]));
        }
        if raw[9] != FOCUSED_EVIDENCE_FLAGS {
            return Err(FocusedEvidenceHeaderError::InvalidFlags(raw[9]));
        }
        let member_count = u16::from_be_bytes([raw[10], raw[11]]);
        if member_count != FOCUSED_EVIDENCE_MEMBER_COUNT {
            return Err(FocusedEvidenceHeaderError::InvalidMemberCount(member_count));
        }
        let reserved = u32::from_be_bytes([raw[12], raw[13], raw[14], raw[15]]);
        if reserved != FOCUSED_EVIDENCE_RESERVED {
            return Err(FocusedEvidenceHeaderError::NonzeroReserved(reserved));
        }
        let manifest_len = u32::from_be_bytes([raw[16], raw[17], raw[18], raw[19]]);
        if manifest_len == 0 || manifest_len > FOCUSED_EVIDENCE_MANIFEST_MAX_BYTES as u32 {
            return Err(FocusedEvidenceHeaderError::InvalidManifestLength(
                manifest_len,
            ));
        }
        let payload_len = u64::from_be_bytes([
            raw[20], raw[21], raw[22], raw[23], raw[24], raw[25], raw[26], raw[27],
        ]);
        if payload_len > FOCUSED_EVIDENCE_PAYLOAD_MAX_BYTES {
            return Err(FocusedEvidenceHeaderError::PayloadTooLarge(payload_len));
        }
        let manifest_len = usize::try_from(manifest_len)
            .map_err(|_| FocusedEvidenceHeaderError::InvalidManifestLength(manifest_len))?;
        let payload_len = usize::try_from(payload_len)
            .map_err(|_| FocusedEvidenceHeaderError::PayloadTooLarge(payload_len))?;
        manifest_len
            .checked_add(payload_len)
            .ok_or(FocusedEvidenceHeaderError::BodyLengthOverflow)?;
        Ok(Self {
            raw,
            manifest_len,
            payload_len,
        })
    }

    pub fn body_len(&self) -> usize {
        self.manifest_len + self.payload_len
    }

    pub fn manifest_len(&self) -> usize {
        self.manifest_len
    }

    pub fn payload_len(&self) -> usize {
        self.payload_len
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FocusedEvidenceFrameDigestV1([u8; 32]);

impl FocusedEvidenceFrameDigestV1 {
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

#[derive(Debug)]
pub struct FocusedEvidenceFrameV1 {
    body: Vec<u8>,
    member_ranges: [Range<usize>; FOCUSED_EVIDENCE_MEMBER_COUNT as usize],
    digest: FocusedEvidenceFrameDigestV1,
}

impl FocusedEvidenceFrameV1 {
    pub fn parse_complete(
        header: FocusedEvidenceHeaderV1,
        body: Vec<u8>,
    ) -> Result<Self, FocusedEvidenceBodyError> {
        let expected = header.body_len();
        if body.len() < expected {
            return Err(FocusedEvidenceBodyError::Truncated {
                expected,
                actual: body.len(),
            });
        }
        if body.len() > expected {
            return Err(FocusedEvidenceBodyError::TrailingBytes {
                expected,
                actual: body.len(),
            });
        }

        let mut frame_hasher = Sha256::new();
        frame_hasher.update(header.raw);
        frame_hasher.update(&body);
        let digest = FocusedEvidenceFrameDigestV1(frame_hasher.finalize().into());
        let rejected = |error| FocusedEvidenceBodyError::CompleteRejected { digest, error };

        let manifest_bytes = &body[..header.manifest_len];
        let manifest_value = parse_canonical_jcs(manifest_bytes).map_err(rejected)?;
        let manifest = serde_json::from_value::<FocusedEvidenceManifestV1>(manifest_value)
            .map_err(|error| rejected(ContractError::InvalidJson(error.to_string())))?;
        if manifest.schema_version != 1
            || manifest.members.len() != FOCUSED_EVIDENCE_MEMBER_COUNT as usize
        {
            return Err(rejected(ContractError::InvalidContract(
                "focused evidence manifest envelope is invalid".to_owned(),
            )));
        }

        let mut offsets = [0_usize; FOCUSED_EVIDENCE_MEMBER_COUNT as usize + 1];
        let mut payload_sum = 0_u64;
        for (index, (member, expected_member)) in manifest
            .members
            .iter()
            .zip(FocusedEvidenceMemberV1::ORDERED)
            .enumerate()
        {
            if member.name != expected_member.name() || member.length == 0 {
                return Err(rejected(ContractError::InvalidContract(
                    "focused evidence manifest members are not the exact ordered positive-length set"
                        .to_owned(),
                )));
            }
            payload_sum = payload_sum.checked_add(member.length).ok_or_else(|| {
                rejected(ContractError::InvalidContract(
                    "focused evidence member lengths overflow".to_owned(),
                ))
            })?;
            if payload_sum > header.payload_len as u64 {
                return Err(rejected(ContractError::InvalidContract(
                    "focused evidence member lengths exceed the payload length".to_owned(),
                )));
            }
            offsets[index + 1] = usize::try_from(payload_sum).map_err(|_| {
                rejected(ContractError::InvalidContract(
                    "focused evidence member length is not addressable".to_owned(),
                ))
            })?;
        }
        if payload_sum != header.payload_len as u64 {
            return Err(rejected(ContractError::InvalidContract(
                "focused evidence member lengths do not sum to the payload length".to_owned(),
            )));
        }

        let payload_start = header.manifest_len;
        let member_ranges: [Range<usize>; FOCUSED_EVIDENCE_MEMBER_COUNT as usize] =
            std::array::from_fn(|index| {
                payload_start + offsets[index]..payload_start + offsets[index + 1]
            });
        for (member, range) in manifest.members.iter().zip(&member_ranges) {
            let actual = Sha256HexV1::parse(format!("{:x}", Sha256::digest(&body[range.clone()])))
                .map_err(rejected)?;
            if actual != member.sha256 {
                return Err(rejected(ContractError::InvalidContract(format!(
                    "focused evidence member {} SHA-256 does not match its raw bytes",
                    member.name
                ))));
            }
        }

        Ok(Self {
            body,
            member_ranges,
            digest,
        })
    }

    pub fn digest(&self) -> FocusedEvidenceFrameDigestV1 {
        self.digest
    }

    pub fn member(&self, member: FocusedEvidenceMemberV1) -> &[u8] {
        &self.body[self.member_ranges[member.index()].clone()]
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum FocusedEvidenceHeaderError {
    #[error("focused evidence header must be exactly 28 bytes, got {actual}")]
    InvalidLength { actual: usize },
    #[error("focused evidence header magic is invalid")]
    InvalidMagic,
    #[error("focused evidence header version {0} is unsupported")]
    UnsupportedVersion(u8),
    #[error("focused evidence header flags must be zero, got {0}")]
    InvalidFlags(u8),
    #[error("focused evidence header member count must be six, got {0}")]
    InvalidMemberCount(u16),
    #[error("focused evidence header reserved field must be zero, got {0}")]
    NonzeroReserved(u32),
    #[error("focused evidence manifest length is out of bounds: {0}")]
    InvalidManifestLength(u32),
    #[error("focused evidence payload is too large: {0}")]
    PayloadTooLarge(u64),
    #[error("focused evidence body length overflowed")]
    BodyLengthOverflow,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum FocusedEvidenceBodyError {
    #[error("focused evidence body was truncated: expected {expected} bytes, got {actual}")]
    Truncated { expected: usize, actual: usize },
    #[error("focused evidence body has trailing bytes: expected {expected} bytes, got {actual}")]
    TrailingBytes { expected: usize, actual: usize },
    #[error("complete focused evidence frame was rejected: {error}")]
    CompleteRejected {
        digest: FocusedEvidenceFrameDigestV1,
        #[source]
        error: ContractError,
    },
}

impl FocusedEvidenceBodyError {
    pub fn complete_digest(&self) -> Option<FocusedEvidenceFrameDigestV1> {
        match self {
            Self::CompleteRejected { digest, .. } => Some(*digest),
            Self::Truncated { .. } | Self::TrailingBytes { .. } => None,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FocusedEvidenceManifestV1 {
    schema_version: u32,
    members: Vec<FocusedEvidenceManifestMemberV1>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FocusedEvidenceManifestMemberV1 {
    name: String,
    length: u64,
    sha256: Sha256HexV1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FocusedEvidenceAckReasonV1 {
    Accepted,
    Rejected,
}

impl FocusedEvidenceAckReasonV1 {
    const fn code(self) -> u16 {
        match self {
            Self::Accepted => 0,
            Self::Rejected => 1,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FocusedEvidenceAckV1 {
    reason: FocusedEvidenceAckReasonV1,
    digest: FocusedEvidenceFrameDigestV1,
}

impl FocusedEvidenceAckV1 {
    pub fn accepted(digest: FocusedEvidenceFrameDigestV1) -> Self {
        Self {
            reason: FocusedEvidenceAckReasonV1::Accepted,
            digest,
        }
    }

    pub fn rejected_complete(digest: FocusedEvidenceFrameDigestV1) -> Self {
        Self {
            reason: FocusedEvidenceAckReasonV1::Rejected,
            digest,
        }
    }

    pub fn encode(&self) -> [u8; FOCUSED_EVIDENCE_ACK_LEN] {
        let mut encoded = [0_u8; FOCUSED_EVIDENCE_ACK_LEN];
        encoded[..8].copy_from_slice(FOCUSED_EVIDENCE_ACK_MAGIC);
        encoded[8] = FOCUSED_EVIDENCE_VERSION;
        encoded[9] = FOCUSED_EVIDENCE_FLAGS;
        encoded[10..12].copy_from_slice(&self.reason.code().to_be_bytes());
        encoded[12..16].copy_from_slice(&FOCUSED_EVIDENCE_RESERVED.to_be_bytes());
        encoded[16..].copy_from_slice(self.digest.as_bytes());
        encoded
    }

    pub fn parse_exact(
        raw: &[u8],
        expected: FocusedEvidenceFrameDigestV1,
    ) -> Result<FocusedEvidenceAckReasonV1, FocusedEvidenceAckError> {
        if raw.len() != FOCUSED_EVIDENCE_ACK_LEN {
            return Err(FocusedEvidenceAckError::InvalidLength { actual: raw.len() });
        }
        if &raw[..8] != FOCUSED_EVIDENCE_ACK_MAGIC {
            return Err(FocusedEvidenceAckError::InvalidMagic);
        }
        if raw[8] != FOCUSED_EVIDENCE_VERSION {
            return Err(FocusedEvidenceAckError::UnsupportedVersion(raw[8]));
        }
        if raw[9] != FOCUSED_EVIDENCE_FLAGS {
            return Err(FocusedEvidenceAckError::InvalidFlags(raw[9]));
        }
        let reason = u16::from_be_bytes([raw[10], raw[11]]);
        let reason = match reason {
            0 => FocusedEvidenceAckReasonV1::Accepted,
            1 => FocusedEvidenceAckReasonV1::Rejected,
            reserved => return Err(FocusedEvidenceAckError::ReservedReason(reserved)),
        };
        let reserved = u32::from_be_bytes([raw[12], raw[13], raw[14], raw[15]]);
        if reserved != FOCUSED_EVIDENCE_RESERVED {
            return Err(FocusedEvidenceAckError::NonzeroReserved(reserved));
        }
        if raw[16..] != expected.0[..] {
            return Err(FocusedEvidenceAckError::DigestMismatch);
        }
        Ok(reason)
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum FocusedEvidenceAckError {
    #[error("focused evidence acknowledgement must be exactly 48 bytes, got {actual}")]
    InvalidLength { actual: usize },
    #[error("focused evidence acknowledgement magic is invalid")]
    InvalidMagic,
    #[error("focused evidence acknowledgement version {0} is unsupported")]
    UnsupportedVersion(u8),
    #[error("focused evidence acknowledgement flags must be zero, got {0}")]
    InvalidFlags(u8),
    #[error("focused evidence acknowledgement reason {0} is reserved")]
    ReservedReason(u16),
    #[error("focused evidence acknowledgement reserved field must be zero, got {0}")]
    NonzeroReserved(u32),
    #[error("focused evidence acknowledgement digest does not match the frame")]
    DigestMismatch,
}
