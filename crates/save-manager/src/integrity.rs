//! `SAVE-003`: integrity digest generation and verification.
//!
//! This module is the third increment of the P4 Save System. It turns the
//! integrity metadata *carried* by `SAVE-001`/`SAVE-002` into something that can
//! be **produced** and **checked**, as the *Save & Compatibility Specification
//! v1.0* requires (`§3.1` `integrity/checksums`, `§3.3`, `§11.2` step 5,
//! `§12.1`, `§12.3`, `§15.1`, `§15.3`).
//!
//! The frozen design is `planning/29-P4-SAVE-003-完整性摘要与校验设计提案.md`
//! (`GK-P4-SAVE-003-DESIGN-03`). Where this module states a rule, that document
//! is authoritative.
//!
//! ## Frozen digest domain (`A1`, `A2`)
//!
//! * `partition_digest = SHA-256(PARTITION_DIGEST_DOMAIN ‖ partition record)`
//!   — the record carries the partition identity, its recorded owner, its
//!   optional Schema version and its opaque payload. It never carries the
//!   partition's position in the layout.
//! * `root = SHA-256(ROOT_DIGEST_DOMAIN ‖ canonical layout encoding)` where the
//!   layout is the set of **all partitions except** [`SavePartitionId::Integrity`],
//!   re-ordered canonically, with the Manifest partition's payload replaced by
//!   its canonical bytes ([`manifest_canonical_bytes`]).
//!
//! The two domains differ, so a record can never be replayed from one context
//! into the other.
//!
//! ## Order independence (`A3`)
//!
//! The root is computed over the **canonical layout encoding**, which re-sorts
//! partitions by identity, and no digest input carries a position. The root is
//! therefore independent of the in-memory insertion order of
//! [`SaveLayout::partitions`].
//!
//! ## Scope boundary
//!
//! * **No I/O of any kind.** Every entry point is a pure function of in-memory
//!   values; nothing here opens, reads, writes or creates a path.
//! * **Never writes back.** All entry points borrow the layout, so the carried
//!   partition payloads — including the Manifest's original bytes — are
//!   provably unchanged (`save-manager` relies on the borrow checker for this,
//!   not on convention).
//! * **No physical container format.** The on-disk container is `SAVE-004`; this
//!   module covers logical save bytes only, and claims no container-level digest.
//! * **No compatibility verdict.** Integrity failure is a *hard* prerequisite,
//!   never one of the four compatibility classes (`SAVE-005`).
//! * **No migration, no backup, no restore, no diagnostics report.** Those are
//!   `SAVE-006`/`SAVE-007`/`SAVE-008`/`SAVE-011`; this module only exports data.
//!
//! ## Checksums payload (`A7`)
//!
//! The `integrity/checksums` partition payload is the internal binary encoding
//! `SC` version 1 (see [`encode_checksums`] / [`decode_checksums`]). It is
//! **not** a published JSON Schema — the contract is carried by Rust types and
//! contract tests, exactly as `SAVE-002` decided (`B-5`).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use sha2::{Digest, Sha256};

use crate::partition::{
    ModNamespace, PartitionError, PartitionErrorCode, PartitionPayload, SaveLayout, SavePartition,
    SavePartitionId,
};
use crate::{Integrity, SUPPORTED_INTEGRITY_ALGORITHM, SaveManifest, SaveManifestError};

/// Domain-separation prefix of a per-partition digest (`GK-P4-SAVE-003-DESIGN-03`).
pub const PARTITION_DIGEST_DOMAIN: &[u8] = b"GKSAVE-PARTITION-DIGEST-v1\x1f";

/// Domain-separation prefix of the layout root digest (`GK-P4-SAVE-003-DESIGN-03`).
pub const ROOT_DIGEST_DOMAIN: &[u8] = b"GKSAVE-ROOT-DIGEST-v1\x1f";

/// Encoding tag of the `integrity/checksums` payload.
pub const CHECKSUMS_ENCODING_TAG: [u8; 2] = *b"SC";

/// Version of the `integrity/checksums` payload encoding.
pub const CHECKSUMS_ENCODING_VERSION: u8 = 1;

/// Length of a digest in its canonical text form (`R-FMT-3`).
pub const DIGEST_TEXT_LEN: usize = 64;

/// The smallest number of bytes one encoded checksum entry can occupy.
///
/// A `Manifest`/`World`/`Metadata` entry is a kind tag (1) plus a digest (64);
/// a `Mod` entry is larger. [`decode_checksums`] uses this bound to reject an
/// input-controlled entry count *before* allocating anything, so the capacity
/// request is always proportional to the bytes actually present.
pub const MIN_ENCODED_CHECKSUM_ENTRY_LEN: usize = 65;

/// Length of the canonical layout header (`tag` + `version` + `count`).
const LAYOUT_HEADER_LEN: usize = 7;

/// The constant `root_hash` substituted into the Manifest while its canonical
/// bytes are derived (frozen rule `M-α′`).
const INTEGRITY_DIGEST_SENTINEL: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";

// ---------------------------------------------------------------------------
// Stable error codes
// ---------------------------------------------------------------------------

/// Stable, machine-readable integrity error codes (frozen: exactly three).
///
/// The set is disjoint from the `SAVE-001` Manifest codes and from the
/// `SAVE-002` partition codes; a contract test proves the disjointness.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntegrityErrorCode {
    /// The recorded digest algorithm is not one this build understands.
    AlgorithmUnsupported,
    /// Integrity cannot be established at all: the layout carries no Manifest
    /// partition (no comparison authority) or no Integrity partition (no
    /// expected per-partition digests). Never a substitute for "intact".
    Missing,
    /// The `integrity/checksums` payload is malformed: wrong tag or version,
    /// truncation, trailing bytes, invalid or non-canonical digest text,
    /// duplicate or out-of-order entries, or an entry for the Integrity
    /// partition itself.
    PayloadInvalid,
}

impl IntegrityErrorCode {
    /// The frozen wire representation.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AlgorithmUnsupported => "save.integrity_algorithm_unsupported",
            Self::Missing => "save.integrity_missing",
            Self::PayloadInvalid => "save.integrity_payload_invalid",
        }
    }

    /// Every frozen code, for exhaustiveness checks.
    pub const ALL: [Self; 3] = [
        Self::AlgorithmUnsupported,
        Self::Missing,
        Self::PayloadInvalid,
    ];
}

/// A failure to generate or verify integrity.
///
/// An error is either one of the three frozen `SAVE-003` codes, or a
/// `SAVE-001`/`SAVE-002` failure **propagated unchanged** so that the stable
/// code a caller sees is the code that actually identified the problem.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IntegrityError {
    /// One of the three frozen `SAVE-003` integrity codes.
    Integrity {
        /// The stable machine code.
        code: IntegrityErrorCode,
        /// The logical pointer to the offending field.
        path: String,
        /// The human-readable message. Not part of the protocol.
        message: String,
    },
    /// A `SAVE-001` Manifest failure, propagated unchanged.
    Manifest(SaveManifestError),
    /// A `SAVE-002` partition failure, propagated unchanged.
    Partition(PartitionError),
}

impl IntegrityError {
    /// Builds an error from one of the three frozen `SAVE-003` codes.
    pub fn new(
        code: IntegrityErrorCode,
        path: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self::Integrity {
            code,
            path: path.into(),
            message: message.into(),
        }
    }

    /// The stable machine code, whichever layer produced it.
    pub fn code_str(&self) -> &'static str {
        match self {
            Self::Integrity { code, .. } => code.as_str(),
            Self::Manifest(error) => error.code_str(),
            Self::Partition(error) => error.code_str(),
        }
    }

    /// The logical pointer to the offending field.
    pub fn path(&self) -> &str {
        match self {
            Self::Integrity { path, .. } => path,
            Self::Manifest(error) => error.path(),
            Self::Partition(error) => error.path(),
        }
    }

    /// The human-readable message. Not part of the protocol.
    pub fn message(&self) -> &str {
        match self {
            Self::Integrity { message, .. } => message,
            Self::Manifest(error) => error.message(),
            Self::Partition(error) => error.message(),
        }
    }

    /// The propagated `SAVE-001` failure, when there is one.
    pub fn manifest_error(&self) -> Option<&SaveManifestError> {
        match self {
            Self::Manifest(error) => Some(error),
            _ => None,
        }
    }

    /// The propagated `SAVE-002` failure, when there is one.
    pub fn partition_error(&self) -> Option<&PartitionError> {
        match self {
            Self::Partition(error) => Some(error),
            _ => None,
        }
    }
}

impl From<SaveManifestError> for IntegrityError {
    fn from(error: SaveManifestError) -> Self {
        Self::Manifest(error)
    }
}

impl From<PartitionError> for IntegrityError {
    fn from(error: PartitionError) -> Self {
        Self::Partition(error)
    }
}

impl fmt::Display for IntegrityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Integrity {
                code,
                path,
                message,
            } => {
                write!(formatter, "{} at `{path}`: {message}", code.as_str())
            }
            Self::Manifest(error) => write!(formatter, "{error}"),
            Self::Partition(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for IntegrityError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Manifest(error) => Some(error),
            Self::Partition(error) => Some(error),
            Self::Integrity { .. } => None,
        }
    }
}

fn missing(path: impl Into<String>, message: impl Into<String>) -> IntegrityError {
    IntegrityError::new(IntegrityErrorCode::Missing, path, message)
}

fn payload_invalid(path: impl Into<String>, message: impl Into<String>) -> IntegrityError {
    IntegrityError::new(IntegrityErrorCode::PayloadInvalid, path, message)
}

// ---------------------------------------------------------------------------
// Result model
// ---------------------------------------------------------------------------

/// The verification status of one partition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PartitionIntegrity {
    /// The recorded digest matches the recomputed one.
    Verified,
    /// A digest was recorded for this partition and does not match.
    Mismatch {
        /// The digest recorded in the checksums payload.
        expected: String,
        /// The digest recomputed from the carried bytes.
        actual: String,
    },
    /// A digest was recorded for a partition the layout does not carry.
    Missing,
    /// The layout carries a partition the checksums payload does not cover.
    Unexpected,
}

/// The outcome of [`verify`].
///
/// This is a **report**, never a verdict: integrity failure is a hard
/// prerequisite for `SAVE-005`, which owns the four compatibility classes.
///
/// [`Self::is_intact`] is the only sanctioned "passes" predicate. It requires
/// all three roots to agree **and** at least one partition actually verified,
/// so an empty report can never be vacuously "intact".
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IntegrityReport {
    /// The algorithm recorded by the checksums payload.
    pub algorithm: String,
    /// The root recorded by the Manifest (`integrity.root_hash`).
    pub manifest_root: String,
    /// The root recorded by the checksums payload itself.
    pub checksums_root: String,
    /// The root recomputed from the carried bytes.
    pub computed_root: String,
    /// Whether `computed_root == manifest_root`.
    pub root_matches: bool,
    /// Whether `checksums_root == manifest_root`; a mismatch means the
    /// checksums payload's own self-description was altered.
    pub checksums_root_matches: bool,
    /// One entry per partition identity seen in either source, canonical order.
    pub partitions: Vec<(SavePartitionId, PartitionIntegrity)>,
}

impl IntegrityReport {
    /// Whether the save is intact.
    ///
    /// Requires `manifest_root == computed_root == checksums_root` **and** a
    /// non-empty set of partitions that all verified.
    pub fn is_intact(&self) -> bool {
        self.root_matches
            && self.checksums_root_matches
            && !self.partitions.is_empty()
            && self
                .partitions
                .iter()
                .all(|(_, status)| matches!(status, PartitionIntegrity::Verified))
    }

    /// Every partition that did not verify, in canonical order.
    pub fn corrupted_partitions(
        &self,
    ) -> impl Iterator<Item = &(SavePartitionId, PartitionIntegrity)> {
        self.partitions
            .iter()
            .filter(|(_, status)| !matches!(status, PartitionIntegrity::Verified))
    }
}

/// The decoded content of an `integrity/checksums` payload.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChecksumRecord {
    /// The algorithm identifier recorded by the payload.
    pub algorithm: String,
    /// The root digest recorded by the payload.
    pub root_hash: String,
    /// The per-partition digests, in canonical order.
    pub entries: Vec<(SavePartitionId, String)>,
}

/// What [`generate`] produces for the caller to write back.
///
/// This module never writes: the caller places `root` in
/// `Manifest.integrity.root_hash` and `checksums` in the
/// [`SavePartitionId::Integrity`] partition payload.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GeneratedIntegrity {
    /// The layout root digest.
    pub root: String,
    /// The encoded `integrity/checksums` payload.
    pub checksums: Vec<u8>,
}

// ---------------------------------------------------------------------------
// Digest primitives
// ---------------------------------------------------------------------------

fn hash_digest(domain: &[u8], payload: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(payload);
    let output = hasher.finalize();
    hex_lower(&output)
}

const HEX_DIGITS: [u8; 16] = *b"0123456789abcdef";

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().saturating_mul(2));
    for &byte in bytes {
        out.push(char::from(HEX_DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(HEX_DIGITS[usize::from(byte & 0x0F)]));
    }
    out
}

fn is_lower_hex_digest(value: &[u8]) -> bool {
    value.len() == DIGEST_TEXT_LEN
        && value
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
}

fn text(bytes: &[u8], path: &str) -> Result<String, IntegrityError> {
    std::str::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|_| payload_invalid(path, "checksums payload contains a non-UTF-8 string"))
}

/// Hashes a single partition's canonical record (`A2`).
///
/// The record is produced by the frozen `SAVE-002` encoder itself — this module
/// never re-implements it — by encoding a one-partition layout and dropping the
/// fixed header. A violation of the carrier contract (a duplicate identity, an
/// ownership violation, an unencodable field) therefore surfaces as the
/// unchanged `SAVE-002` error.
pub fn partition_digest(partition: &SavePartition) -> Result<String, IntegrityError> {
    let record = partition_record(partition)?;
    Ok(hash_digest(PARTITION_DIGEST_DOMAIN, &record))
}

fn partition_record(partition: &SavePartition) -> Result<Vec<u8>, IntegrityError> {
    let encoded = SaveLayout::new(vec![partition.clone()])
        .to_bytes()
        .map_err(IntegrityError::from)?;
    let record = encoded.get(LAYOUT_HEADER_LEN..).ok_or_else(|| {
        IntegrityError::from(PartitionError::new(
            PartitionErrorCode::InvalidLayout,
            "/partitions",
            "canonical layout encoding did not contain a partition record",
        ))
    })?;
    Ok(record.to_vec())
}

/// Derives the canonical Manifest bytes `CMB` from a carried Manifest payload.
///
/// `CMB = to_bytes(from_bytes(payload) with integrity.root_hash := 64 × "0")`.
/// The `algorithm` member keeps the parsed value (frozen rule `M-α′`), so the
/// business fields — and only they — are covered.
///
/// A payload that is not a valid Manifest is rejected with the unchanged
/// `SAVE-001` error, so a malformed Manifest can never yield a report that looks
/// valid.
pub fn manifest_canonical_bytes(payload: &[u8]) -> Result<Vec<u8>, IntegrityError> {
    let manifest = SaveManifest::from_bytes(payload).map_err(IntegrityError::from)?;
    Ok(canonical_bytes_of(&manifest))
}

fn canonical_bytes_of(manifest: &SaveManifest) -> Vec<u8> {
    let mut canonical = manifest.clone();
    canonical.integrity = Integrity {
        algorithm: manifest.integrity.algorithm.clone(),
        root_hash: INTEGRITY_DIGEST_SENTINEL.to_owned(),
    };
    canonical.to_bytes()
}

/// The digest inputs derived from a layout, in canonical order.
struct CanonicalView {
    /// Every partition except the Integrity partition, with the Manifest
    /// partition's payload replaced by `CMB`.
    partitions: Vec<SavePartition>,
    /// The parsed Manifest, when the layout carries one.
    manifest: Option<SaveManifest>,
}

fn canonical_view(layout: &SaveLayout) -> Result<CanonicalView, IntegrityError> {
    let mut partitions = Vec::with_capacity(layout.len());
    let mut manifest = None;
    for partition in &layout.partitions {
        if partition.id == SavePartitionId::Integrity {
            continue;
        }
        let mut carried = partition.clone();
        if carried.id == SavePartitionId::Manifest {
            let parsed = SaveManifest::from_bytes(partition.payload.as_bytes())
                .map_err(IntegrityError::from)?;
            carried.payload = PartitionPayload::new(canonical_bytes_of(&parsed));
            manifest = Some(parsed);
        }
        partitions.push(carried);
    }
    Ok(CanonicalView {
        partitions,
        manifest,
    })
}

fn encode_layout(partitions: &[SavePartition]) -> Result<Vec<u8>, IntegrityError> {
    SaveLayout::new(partitions.to_vec())
        .to_bytes()
        .map_err(IntegrityError::from)
}

fn root_of(partitions: &[SavePartition]) -> Result<String, IntegrityError> {
    let encoded = encode_layout(partitions)?;
    Ok(hash_digest(ROOT_DIGEST_DOMAIN, &encoded))
}

fn digest_entries(
    partitions: &[SavePartition],
) -> Result<Vec<(SavePartitionId, String)>, IntegrityError> {
    let mut entries = Vec::with_capacity(partitions.len());
    for partition in partitions {
        entries.push((partition.id.clone(), partition_digest(partition)?));
    }
    Ok(entries)
}

fn require_partition<'a>(
    layout: &'a SaveLayout,
    id: &SavePartitionId,
) -> Result<&'a SavePartition, IntegrityError> {
    layout.get(id).ok_or_else(|| {
        missing(
            "/partitions",
            format!(
                "layout carries no `{}` partition, so integrity cannot be established",
                id.logical_path()
            ),
        )
    })
}

/// Computes the layout root: `SHA-256(ROOT_DIGEST_DOMAIN ‖ canonical encoding)`.
///
/// Covers every partition **except** [`SavePartitionId::Integrity`] (`A1`). An
/// empty layout has a defined root; the root does not depend on the partitions'
/// insertion order (`A3`), on the Manifest's `root_hash`, or on the Integrity
/// partition's content.
pub fn layout_root(layout: &SaveLayout) -> Result<String, IntegrityError> {
    layout.validate().map_err(IntegrityError::from)?;
    let view = canonical_view(layout)?;
    root_of(&view.partitions)
}

/// Generates the root and the `integrity/checksums` payload (**without** writing
/// either). Requires a Manifest partition (the comparison authority) and an
/// Integrity partition (the carrier this payload belongs to).
pub fn generate(layout: &SaveLayout) -> Result<GeneratedIntegrity, IntegrityError> {
    layout.validate().map_err(IntegrityError::from)?;
    require_partition(layout, &SavePartitionId::Manifest)?;
    require_partition(layout, &SavePartitionId::Integrity)?;

    let view = canonical_view(layout)?;
    let root = root_of(&view.partitions)?;
    let entries = digest_entries(&view.partitions)?;
    let checksums = encode_checksums(&root, &entries)?;
    Ok(GeneratedIntegrity { root, checksums })
}

/// Verifies a layout against its recorded integrity metadata.
///
/// Structural and precondition failures (an invalid layout, an unparsable
/// Manifest, a malformed checksums payload, a missing partition) are returned as
/// [`Err`]; a *digest disagreement* is a successful report whose
/// [`IntegrityReport::is_intact`] is `false`.
pub fn verify(layout: &SaveLayout) -> Result<IntegrityReport, IntegrityError> {
    layout.validate().map_err(IntegrityError::from)?;
    require_partition(layout, &SavePartitionId::Manifest)?;
    let integrity_partition = require_partition(layout, &SavePartitionId::Integrity)?;

    let view = canonical_view(layout)?;
    let Some(manifest) = view.manifest else {
        // Unreachable: the Manifest partition was required above, and
        // `canonical_view` parses exactly that partition.
        return Err(missing(
            "/partitions",
            "layout carries no Manifest partition, so integrity cannot be established",
        ));
    };
    let manifest_root = manifest.integrity.root_hash.clone();

    let ChecksumRecord {
        algorithm,
        root_hash: checksums_root,
        entries,
    } = decode_checksums(integrity_partition.payload.as_bytes())?;

    let computed_root = root_of(&view.partitions)?;

    let mut expected: BTreeMap<SavePartitionId, String> = BTreeMap::new();
    let mut ids: BTreeSet<SavePartitionId> = BTreeSet::new();
    for (id, digest) in entries {
        ids.insert(id.clone());
        expected.insert(id, digest);
    }
    let mut actual: BTreeMap<SavePartitionId, String> = BTreeMap::new();
    for partition in &view.partitions {
        ids.insert(partition.id.clone());
        actual.insert(partition.id.clone(), partition_digest(partition)?);
    }

    let mut partitions = Vec::with_capacity(ids.len());
    for id in ids {
        let status = match (actual.get(&id), expected.get(&id)) {
            (Some(actual_digest), Some(expected_digest)) => {
                if actual_digest == expected_digest {
                    PartitionIntegrity::Verified
                } else {
                    PartitionIntegrity::Mismatch {
                        expected: expected_digest.clone(),
                        actual: actual_digest.clone(),
                    }
                }
            }
            (Some(_), None) => PartitionIntegrity::Unexpected,
            (None, Some(_)) => PartitionIntegrity::Missing,
            // Unreachable: `ids` is the union of both key sets.
            (None, None) => continue,
        };
        partitions.push((id, status));
    }

    let root_matches = computed_root == manifest_root;
    let checksums_root_matches = checksums_root == manifest_root;

    Ok(IntegrityReport {
        algorithm,
        manifest_root,
        checksums_root,
        computed_root,
        root_matches,
        checksums_root_matches,
        partitions,
    })
}

// ---------------------------------------------------------------------------
// Checksums payload (`SC` v1)
// ---------------------------------------------------------------------------

fn push_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn push_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}

/// Encodes an `integrity/checksums` payload.
///
/// Layout (`SC` v1): tag `SC` ‖ version `1` ‖ `u16` algorithm length ‖
/// algorithm ‖ 64-byte root digest text ‖ `u32` entry count ‖ entries. Every
/// integer is big-endian; every digest is the 64-character lowercase hex text
/// of `R-FMT-3`, never a raw 32-byte value. Entries must be in canonical order,
/// free of duplicates, and must not include the Integrity partition.
///
/// A value the fixed-width encoding cannot represent, or an entry list that
/// violates the ordering rules, is reported rather than truncated.
pub fn encode_checksums(
    root: &str,
    entries: &[(SavePartitionId, String)],
) -> Result<Vec<u8>, IntegrityError> {
    let algorithm = SUPPORTED_INTEGRITY_ALGORITHM;
    if !is_lower_hex_digest(root.as_bytes()) {
        return Err(payload_invalid(
            "/checksums/root_hash",
            "root digest must be exactly 64 lowercase hexadecimal characters",
        ));
    }
    let algorithm_len = u16::try_from(algorithm.len()).map_err(|_| {
        payload_invalid(
            "/checksums/algorithm",
            "integrity algorithm identifier exceeds the 65535-byte encoding limit",
        )
    })?;
    let count = u32::try_from(entries.len()).map_err(|_| {
        payload_invalid(
            "/checksums/entries",
            format!(
                "{} checksum entries exceed the {}-entry encoding limit",
                entries.len(),
                u32::MAX
            ),
        )
    })?;

    let mut out = Vec::new();
    out.extend_from_slice(&CHECKSUMS_ENCODING_TAG);
    out.push(CHECKSUMS_ENCODING_VERSION);
    push_u16(&mut out, algorithm_len);
    out.extend_from_slice(algorithm.as_bytes());
    out.extend_from_slice(root.as_bytes());
    push_u32(&mut out, count);

    let mut previous: Option<&SavePartitionId> = None;
    for (index, (id, digest)) in entries.iter().enumerate() {
        let path = format!("/checksums/entries/{index}");
        if id == &SavePartitionId::Integrity {
            return Err(payload_invalid(
                format!("{path}/kind"),
                "checksums must not contain an entry for the integrity partition",
            ));
        }
        if !previous.is_none_or(|previous_id| id > previous_id) {
            return Err(payload_invalid(
                path,
                "checksum entries must be in canonical order and free of duplicates",
            ));
        }
        previous = Some(id);
        if !is_lower_hex_digest(digest.as_bytes()) {
            return Err(payload_invalid(
                format!("{path}/digest"),
                "checksum entry digest must be exactly 64 lowercase hexadecimal characters",
            ));
        }
        encode_entry(&mut out, id, digest)?;
    }
    Ok(out)
}

fn encode_entry(
    out: &mut Vec<u8>,
    id: &SavePartitionId,
    digest: &str,
) -> Result<(), IntegrityError> {
    match id {
        SavePartitionId::Manifest => out.push(0),
        SavePartitionId::World => out.push(1),
        SavePartitionId::Mod(namespace) => {
            out.push(2);
            let bytes = namespace.as_str().as_bytes();
            let length = u16::try_from(bytes.len()).map_err(|_| {
                payload_invalid(
                    "/checksums/entries/namespace",
                    "Mod namespace exceeds the 65535-byte encoding limit",
                )
            })?;
            push_u16(out, length);
            out.extend_from_slice(bytes);
        }
        SavePartitionId::Metadata => out.push(3),
        SavePartitionId::Integrity => {
            return Err(payload_invalid(
                "/checksums/entries/kind",
                "checksums must not contain an entry for the integrity partition",
            ));
        }
    }
    out.extend_from_slice(digest.as_bytes());
    Ok(())
}

/// Decodes and strictly validates an `integrity/checksums` payload.
///
/// The mirror of [`encode_checksums`]. Anything not exactly canonical — a wrong
/// tag or version, truncation, trailing bytes, a non-UTF-8 string, an
/// unsupported algorithm, digest text that is not 64 lowercase hex characters,
/// a duplicate or out-of-order entry, or an entry for the Integrity partition —
/// is rejected.
pub fn decode_checksums(bytes: &[u8]) -> Result<ChecksumRecord, IntegrityError> {
    let mut reader = Reader::new(bytes);

    if reader.take(2, "/checksums/tag")? != CHECKSUMS_ENCODING_TAG {
        return Err(payload_invalid(
            "/checksums/tag",
            "checksums payload does not start with the `SC` encoding tag",
        ));
    }
    let version = reader.u8("/checksums/version")?;
    if version != CHECKSUMS_ENCODING_VERSION {
        return Err(payload_invalid(
            "/checksums/version",
            format!("unsupported checksums payload version {version}"),
        ));
    }

    let algorithm_len = usize::from(reader.u16("/checksums/algorithm")?);
    let algorithm = text(
        reader.take(algorithm_len, "/checksums/algorithm")?,
        "/checksums/algorithm",
    )?;
    if algorithm != SUPPORTED_INTEGRITY_ALGORITHM {
        return Err(IntegrityError::new(
            IntegrityErrorCode::AlgorithmUnsupported,
            "/checksums/algorithm",
            format!(
                "checksums payload records algorithm `{algorithm}`; expected `{SUPPORTED_INTEGRITY_ALGORITHM}`"
            ),
        ));
    }

    let root_hash = digest_text(&mut reader, "/checksums/root_hash")?;

    let count = usize::try_from(reader.u32("/checksums/entries")?).map_err(|_| {
        payload_invalid(
            "/checksums/entries",
            "checksum entry count does not fit this platform",
        )
    })?;
    // Reject a declared count the remaining bytes could not possibly encode
    // *before* allocating, so an input-controlled count cannot force a large
    // up-front allocation from a small stream.
    let max_encodable = reader.remaining() / MIN_ENCODED_CHECKSUM_ENTRY_LEN;
    if count > max_encodable {
        return Err(payload_invalid(
            "/checksums/entries",
            format!(
                "declared checksum entry count {count} exceeds the {max_encodable} entry(ies) the remaining {} byte(s) can encode",
                reader.remaining()
            ),
        ));
    }

    let mut entries = Vec::with_capacity(count);
    let mut previous: Option<SavePartitionId> = None;
    for index in 0..count {
        let path = format!("/checksums/entries/{index}");
        let id = decode_entry_id(&mut reader, &path)?;
        if !previous
            .as_ref()
            .is_none_or(|previous_id| &id > previous_id)
        {
            return Err(payload_invalid(
                path,
                "checksum entries must be in canonical order and free of duplicates",
            ));
        }
        previous = Some(id.clone());
        let digest = digest_text(&mut reader, &format!("{path}/digest"))?;
        entries.push((id, digest));
    }

    if !reader.is_exhausted() {
        return Err(payload_invalid(
            "/checksums",
            format!(
                "trailing bytes after the last checksum entry (offset {})",
                reader.position
            ),
        ));
    }

    Ok(ChecksumRecord {
        algorithm,
        root_hash,
        entries,
    })
}

fn decode_entry_id(reader: &mut Reader<'_>, path: &str) -> Result<SavePartitionId, IntegrityError> {
    let kind_path = format!("{path}/kind");
    match reader.u8(&kind_path)? {
        0 => Ok(SavePartitionId::Manifest),
        1 => Ok(SavePartitionId::World),
        2 => {
            let namespace_path = format!("{path}/namespace");
            let length = usize::from(reader.u16(&namespace_path)?);
            let raw = reader.take(length, &namespace_path)?;
            let name = text(raw, &namespace_path)?;
            ModNamespace::parse(&name)
                .map(SavePartitionId::Mod)
                .map_err(|_| {
                    payload_invalid(
                        &namespace_path,
                        "checksum entry namespace must equal a stable Mod ID",
                    )
                })
        }
        3 => Ok(SavePartitionId::Metadata),
        4 => Err(payload_invalid(
            kind_path,
            "checksums must not contain an entry for the integrity partition",
        )),
        other => Err(payload_invalid(
            kind_path,
            format!("unknown checksum entry kind tag {other}"),
        )),
    }
}

fn digest_text(reader: &mut Reader<'_>, path: &str) -> Result<String, IntegrityError> {
    let raw = reader.take(DIGEST_TEXT_LEN, path)?;
    if !is_lower_hex_digest(raw) {
        return Err(payload_invalid(
            path,
            "digest must be exactly 64 lowercase hexadecimal characters",
        ));
    }
    text(raw, path)
}

/// A bounds-checked cursor over a checksums payload.
struct Reader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Reader<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn remaining(&self) -> usize {
        self.bytes.len() - self.position
    }

    fn is_exhausted(&self) -> bool {
        self.position == self.bytes.len()
    }

    fn take(&mut self, length: usize, path: &str) -> Result<&'a [u8], IntegrityError> {
        if self.remaining() < length {
            return Err(payload_invalid(
                path,
                format!(
                    "checksums payload is truncated: need {length} byte(s) at offset {}, only {} remain",
                    self.position,
                    self.remaining()
                ),
            ));
        }
        let slice = &self.bytes[self.position..self.position + length];
        self.position += length;
        Ok(slice)
    }

    fn u8(&mut self, path: &str) -> Result<u8, IntegrityError> {
        Ok(self.take(1, path)?[0])
    }

    fn u16(&mut self, path: &str) -> Result<u16, IntegrityError> {
        let bytes = self.take(2, path)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn u32(&mut self, path: &str) -> Result<u32, IntegrityError> {
        let bytes = self.take(4, path)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domains_are_distinct_and_terminated() {
        assert_ne!(PARTITION_DIGEST_DOMAIN, ROOT_DIGEST_DOMAIN);
        assert_eq!(PARTITION_DIGEST_DOMAIN.len(), 27);
        assert_eq!(ROOT_DIGEST_DOMAIN.len(), 22);
        assert_eq!(PARTITION_DIGEST_DOMAIN.last(), Some(&0x1f));
        assert_eq!(ROOT_DIGEST_DOMAIN.last(), Some(&0x1f));
    }

    #[test]
    fn digest_text_rule_rejects_non_canonical_forms() {
        assert!(is_lower_hex_digest(&[b'a'; DIGEST_TEXT_LEN]));
        assert!(!is_lower_hex_digest(&[b'a'; DIGEST_TEXT_LEN - 1]));
        assert!(!is_lower_hex_digest(&[b'A'; DIGEST_TEXT_LEN]));
        assert!(!is_lower_hex_digest(&[b'g'; DIGEST_TEXT_LEN]));
    }

    #[test]
    fn checksum_entry_minimum_bound_is_frozen() {
        assert_eq!(MIN_ENCODED_CHECKSUM_ENTRY_LEN, 1 + DIGEST_TEXT_LEN);
    }
}
