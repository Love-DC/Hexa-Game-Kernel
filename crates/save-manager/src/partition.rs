//! `SAVE-002`: save partition carrier contract (world and Mod data partitioning).
//!
//! This module is the second increment of the P4 Save System. It implements the
//! **in-memory carrier contract** for the logical save layout defined by the
//! *Save & Compatibility Specification v1.0* `§3.1` (logical structure), `§4`
//! (data ownership and isolation) and *Game Kernel — 完整项目规范 v1.0* `§6.4`
//! (every Mod owns an explicit namespace; missing Mod data is retained by
//! default).
//!
//! ## Decided boundaries (GK-P4-SAVE-002, decisions `B-1`..`B-7`)
//!
//! * **B-1 — the world partition carries raw snapshot bytes.** The payload of a
//!   [`SavePartitionId::World`] partition is the *opaque byte string* produced by
//!   `Kernel::snapshot()`. This module never parses it, never re-encodes it and
//!   never maps it to [`SaveManifest::world_schema_version`].
//! * **B-2 — `metadata/session-data` is owned by the Save System.** It is
//!   carried opaquely ([`PartitionOwner::SaveSystem`]); no new version field is
//!   introduced for it.
//! * **B-3 — Mod payloads are opaque bytes.** No platform Mod read/write channel
//!   is implemented here.
//! * **B-4 — no I/O.** Every entry point operates on in-memory values.
//! * **B-5 — no published partition-index JSON Schema.** The contract is carried
//!   by Rust types and contract tests only.
//! * **B-6 — structure only, no detection.** This module carries the structural
//!   fields a cross-Mod reference needs, but performs no reference-invalidity
//!   detection (`SAVE-010`) and no compatibility verdict (`SAVE-005`).
//!
//! ## What this module deliberately does **not** do
//!
//! It does not compute digests (`SAVE-003`), does not touch the disk, does not
//! define the physical container format (`SAVE-004`), does not decide
//! compatibility, and does not migrate. It only carries partitions faithfully,
//! enforces the namespace and ownership rules, and **reports — never repairs —
//! the disagreement between a layout and a Manifest**.
//!
//! ## Version mapping (do not conflate)
//!
//! [`SaveManifest::world_schema_version`] (a save-domain `MAJOR.MINOR` value)
//! and the Kernel's snapshot-format version (`kernel-core`'s private
//! `SNAPSHOT_FORMAT_VERSION: u32`) are **unrelated**: there is currently **no
//! mapping** between them and none may be inferred, derived or folded into a
//! single field. The world partition keeps the snapshot bytes exactly as given;
//! the version stays a Manifest-level declaration only.
//!
//! ## The canonical layout encoding
//!
//! [`SaveLayout::to_bytes`] produces a minimal, order-canonical encoding used to
//! prove **determinism** and **byte-for-byte losslessness** of carried data.
//! It is an *internal in-memory encoding*: it is **not** the on-disk save
//! container (that format is `SAVE-004`'s responsibility) and it is **not** a
//! published Schema (`B-5`).
//!
//! Serialization **validates first** and is fallible: an invalid layout, or a
//! field that the fixed-width encoding cannot represent, is reported as a
//! [`PartitionError`] rather than being truncated or allowed to panic. Encoding
//! never loses data silently, and every byte stream this module emits decodes
//! back to an equal layout.

use std::collections::BTreeSet;
use std::fmt;

use crate::SaveManifest;

/// Encoding tag for [`SaveLayout::to_bytes`].
///
/// Identifies the *in-memory* canonical layout encoding, not a save container.
const LAYOUT_ENCODING_TAG: [u8; 2] = *b"SL";

/// Version of the canonical layout encoding produced by [`SaveLayout::to_bytes`].
const LAYOUT_ENCODING_VERSION: u8 = 1;

/// Maximum length, in bytes, of a partition's optional `Schema version` string.
///
/// The canonical layout encoding length-prefixes every string with a `u16`. This
/// bound keeps a `Schema version` far below that limit and is enforced by
/// [`SaveLayout::validate`], so no public construction path can produce a layout
/// whose serialization would be unencodable. It matches the crate's other
/// 128-character identifier bound.
pub const MAX_SCHEMA_VERSION_LEN: usize = 128;

/// The minimum number of bytes one encoded partition can occupy.
///
/// Every partition encodes at least its kind tag (1), owner tag (1), Schema
/// version flag (1) and payload length (4) — seven bytes — because the payload
/// and both strings may be empty. [`SaveLayout::from_bytes`] uses this to reject
/// a declared count that the remaining bytes could not possibly encode before
/// allocating anything.
const MIN_ENCODED_PARTITION_LEN: usize = 7;

// ---------------------------------------------------------------------------
// Stable error codes
// ---------------------------------------------------------------------------

/// Stable, machine-readable save partition error codes.
///
/// Diagnostics change; machines MUST match on these codes. The set is frozen for
/// `SAVE-002` (covered by contract tests) and is disjoint from the frozen
/// `SAVE-001` Manifest codes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PartitionErrorCode {
    /// A partition violates the carrier contract: unknown kind/owner tag, or a
    /// field that cannot be represented.
    InvalidPartition,
    /// The layout byte stream is malformed: wrong tag/version, truncation,
    /// trailing bytes, or a non-UTF-8 string.
    InvalidLayout,
    /// A Mod namespace does not equal a stable Mod ID.
    InvalidNamespace,
    /// A partition's recorded owner disagrees with its canonical owner — for
    /// example a Mod partition owned by another namespace.
    PartitionOwnershipViolation,
    /// Two partitions in one layout share the same [`SavePartitionId`].
    DuplicatePartition,
}

impl PartitionErrorCode {
    /// The frozen wire representation.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidPartition => "save.invalid_partition",
            Self::InvalidLayout => "save.invalid_layout",
            Self::InvalidNamespace => "save.invalid_namespace",
            Self::PartitionOwnershipViolation => "save.partition_ownership_violation",
            Self::DuplicatePartition => "save.duplicate_partition",
        }
    }
}

/// A save partition failure.
///
/// [`code`](Self::code) and [`path`](Self::path) form the machine protocol;
/// [`message`](Self::message) is human-readable only. For layout decoding the
/// `path` is the logical pointer into the layout (`/partitions/<index>/<field>`)
/// and the byte offset is reported in the message.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PartitionError {
    code: PartitionErrorCode,
    path: String,
    message: String,
}

impl PartitionError {
    /// Builds a failure from a stable code, a logical pointer and a message.
    pub fn new(
        code: PartitionErrorCode,
        path: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code,
            path: path.into(),
            message: message.into(),
        }
    }

    /// The stable machine code.
    pub fn code(&self) -> PartitionErrorCode {
        self.code
    }

    /// The frozen wire representation of [`code`](Self::code).
    pub fn code_str(&self) -> &'static str {
        self.code.as_str()
    }

    /// The logical pointer to the offending field.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The human-readable message. Not part of the protocol.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for PartitionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} at `{}`: {}",
            self.code.as_str(),
            self.path,
            self.message
        )
    }
}

impl std::error::Error for PartitionError {}

// ---------------------------------------------------------------------------
// Namespace
// ---------------------------------------------------------------------------

/// A Mod's persistent namespace, which MUST equal the stable Mod ID (`§4.2`).
///
/// Validation reuses the crate's identifier rule, so a namespace is byte-for-byte
/// the same shape as a Mod Manifest `mod_id`. The shape forbids `/`, `..`,
/// absolute paths, empty segments and upper case, which makes it path-safe by
/// construction.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ModNamespace(String);

impl ModNamespace {
    /// Parses and validates a namespace string.
    pub fn parse(value: &str) -> Result<Self, PartitionError> {
        Self::validate_at(value, "/namespace")
    }

    /// Parses a namespace, reporting failures at `path`.
    fn validate_at(value: &str, path: &str) -> Result<Self, PartitionError> {
        if !crate::is_valid_identifier(value) {
            return Err(PartitionError::new(
                PartitionErrorCode::InvalidNamespace,
                path,
                "Mod namespace must equal the stable Mod ID: a dotted lowercase identifier of at most 128 characters",
            ));
        }
        Ok(Self(value.to_owned()))
    }

    /// The namespace as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ModNamespace {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl From<ModNamespace> for String {
    fn from(namespace: ModNamespace) -> Self {
        namespace.0
    }
}

// ---------------------------------------------------------------------------
// Ownership
// ---------------------------------------------------------------------------

/// Who owns a partition (`§4.1`/`§4.2`, decisions `B-2`/`B-3`).
///
/// The owner is recorded explicitly and must agree with the partition identity;
/// this keeps an ownership assertion in the carrier itself rather than making it
/// purely derivable. A disagreement is
/// [`PartitionErrorCode::PartitionOwnershipViolation`].
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum PartitionOwner {
    /// Owned by the Save System (Manifest, session metadata, integrity).
    SaveSystem,
    /// Owned by the Kernel (world data, `§4.1`).
    Kernel,
    /// Owned by a Mod's own namespace (`§4.2`).
    Mod(ModNamespace),
}

// ---------------------------------------------------------------------------
// Partition identity
// ---------------------------------------------------------------------------

/// The identity of a partition in the logical save layout (`§3.1`).
///
/// The five variants correspond one-to-one with the five entries of the `§3.1`
/// logical structure; see [`SavePartitionId::logical_path`].
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum SavePartitionId {
    /// `manifest.json` — the save Manifest (modelled by `SAVE-001`, carried
    /// here as opaque bytes and never re-modelled).
    Manifest,
    /// `world/world-data` — the raw world snapshot bytes (`B-1`).
    World,
    /// `mods/<mod-id>/data` — one Mod's persistent payload (`B-3`).
    Mod(ModNamespace),
    /// `metadata/session-data` — session metadata owned by the Save System
    /// (`B-2`).
    Metadata,
    /// `integrity/checksums` — integrity metadata; carried, never generated
    /// here (`SAVE-003`).
    Integrity,
}

impl SavePartitionId {
    /// A short, stable kind label (no namespace).
    pub const fn kind_str(&self) -> &'static str {
        match self {
            Self::Manifest => "manifest",
            Self::World => "world",
            Self::Mod(_) => "mod",
            Self::Metadata => "metadata",
            Self::Integrity => "integrity",
        }
    }

    /// The owner this partition MUST have.
    pub fn canonical_owner(&self) -> PartitionOwner {
        match self {
            Self::Manifest | Self::Metadata | Self::Integrity => PartitionOwner::SaveSystem,
            Self::World => PartitionOwner::Kernel,
            Self::Mod(namespace) => PartitionOwner::Mod(namespace.clone()),
        }
    }

    /// The Mod namespace, for Mod partitions only.
    pub fn namespace(&self) -> Option<&ModNamespace> {
        match self {
            Self::Mod(namespace) => Some(namespace),
            _ => None,
        }
    }

    /// The `§3.1` logical path of this partition.
    ///
    /// The path is logical: `§3.1` states the structure is not required to be
    /// stored as separate files, so the physical container is `SAVE-004`'s
    /// decision and is not presupposed here.
    pub fn logical_path(&self) -> String {
        match self {
            Self::Manifest => "manifest.json".to_owned(),
            Self::World => "world/world-data".to_owned(),
            Self::Mod(namespace) => format!("mods/{}/data", namespace.as_str()),
            Self::Metadata => "metadata/session-data".to_owned(),
            Self::Integrity => "integrity/checksums".to_owned(),
        }
    }
}

// ---------------------------------------------------------------------------
// Payload
// ---------------------------------------------------------------------------

/// An opaque byte payload.
///
/// The bytes are never interpreted: world snapshot bytes (`B-1`), Mod data
/// (`B-3`) and session metadata (`B-2`) all travel as opaque bytes so that
/// unrecognised data can be preserved without loss (`§4.4`).
#[derive(Clone, Eq, PartialEq)]
pub struct PartitionPayload(Vec<u8>);

impl PartitionPayload {
    /// Wraps raw bytes.
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Self(bytes.into())
    }

    /// The bytes, borrowed.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Consumes the payload, returning the bytes.
    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }

    /// The payload length in bytes.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the payload is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for PartitionPayload {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "PartitionPayload({} bytes)", self.0.len())
    }
}

// ---------------------------------------------------------------------------
// Partition
// ---------------------------------------------------------------------------

/// One carried partition: identity, owner, opaque payload and an optional Schema
/// version.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SavePartition {
    /// The partition identity.
    pub id: SavePartitionId,
    /// The recorded owner; must equal [`SavePartitionId::canonical_owner`].
    pub owner: PartitionOwner,
    /// The opaque payload.
    pub payload: PartitionPayload,
    /// The optional data Schema version for this partition.
    pub schema_version: Option<String>,
}

impl SavePartition {
    /// Builds a partition with its canonical owner and no Schema version.
    pub fn new(id: SavePartitionId, payload: impl Into<Vec<u8>>) -> Self {
        let owner = id.canonical_owner();
        Self {
            id,
            owner,
            payload: PartitionPayload::new(payload),
            schema_version: None,
        }
    }

    /// Builds a partition with an explicit owner.
    ///
    /// Intended for negative/contract testing:
    /// [`SaveLayout::validate`] rejects an owner that disagrees with
    /// [`Self::canonical_owner`].
    pub fn with_owner(
        id: SavePartitionId,
        owner: PartitionOwner,
        payload: impl Into<Vec<u8>>,
    ) -> Self {
        Self {
            id,
            owner,
            payload: PartitionPayload::new(payload),
            schema_version: None,
        }
    }

    /// Records a partition data Schema version.
    ///
    /// The value MUST be at most [`MAX_SCHEMA_VERSION_LEN`] bytes; the bound is
    /// enforced by [`SaveLayout::validate`] before serialization.
    pub fn with_schema_version(mut self, schema_version: impl Into<String>) -> Self {
        self.schema_version = Some(schema_version.into());
        self
    }

    /// The owner this partition MUST have.
    pub fn canonical_owner(&self) -> PartitionOwner {
        self.id.canonical_owner()
    }
}

// ---------------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------------

/// An ordered, deterministic collection of partitions.
///
/// Partitions are kept in **canonical order** (by [`SavePartitionId`], which
/// orders `Manifest < World < Mod(..) < Metadata < Integrity` and sorts Mod
/// namespaces lexically). Serialization re-sorts defensively, so
/// [`Self::to_bytes`] is independent of the in-memory insertion order.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SaveLayout {
    /// The carried partitions, in canonical order.
    pub partitions: Vec<SavePartition>,
}

impl SaveLayout {
    /// Builds a layout, ordering the partitions canonically (without
    /// validating).
    pub fn new(mut partitions: Vec<SavePartition>) -> Self {
        Self::sort_canonical(&mut partitions);
        Self { partitions }
    }

    /// The partitions in canonical order.
    pub fn partitions(&self) -> &[SavePartition] {
        &self.partitions
    }

    /// The number of partitions.
    pub fn len(&self) -> usize {
        self.partitions.len()
    }

    /// Whether the layout carries no partitions.
    pub fn is_empty(&self) -> bool {
        self.partitions.is_empty()
    }

    /// Finds a partition by identity.
    pub fn get(&self, id: &SavePartitionId) -> Option<&SavePartition> {
        self.partitions.iter().find(|partition| &partition.id == id)
    }

    /// Iterates over the carried Mod partitions.
    pub fn mod_partitions(&self) -> impl Iterator<Item = &SavePartition> {
        self.partitions
            .iter()
            .filter(|partition| partition.id.namespace().is_some())
    }

    /// Validates the carrier contract.
    ///
    /// Enforces three rules that are independent of any Manifest: partition
    /// identities are unique, every recorded owner equals its canonical owner,
    /// and every `Schema version` fits the encoding bound
    /// ([`MAX_SCHEMA_VERSION_LEN`]).
    pub fn validate(&self) -> Result<(), PartitionError> {
        let mut seen: BTreeSet<&SavePartitionId> = BTreeSet::new();
        for (index, partition) in self.partitions.iter().enumerate() {
            if !seen.insert(&partition.id) {
                return Err(PartitionError::new(
                    PartitionErrorCode::DuplicatePartition,
                    format!("/partitions/{index}/id"),
                    format!("duplicate partition `{}`", partition.id.logical_path()),
                ));
            }
            if partition.owner != partition.canonical_owner() {
                return Err(PartitionError::new(
                    PartitionErrorCode::PartitionOwnershipViolation,
                    format!("/partitions/{index}/owner"),
                    format!(
                        "partition `{}` must be owned by its canonical owner; a Mod partition may only be owned by its own namespace",
                        partition.id.logical_path()
                    ),
                ));
            }
            let overlong_schema_version = partition
                .schema_version
                .as_deref()
                .filter(|version| version.len() > MAX_SCHEMA_VERSION_LEN);
            if let Some(version) = overlong_schema_version {
                return Err(PartitionError::new(
                    PartitionErrorCode::InvalidPartition,
                    format!("/partitions/{index}/schema_version"),
                    format!(
                        "partition Schema version is {} bytes; the maximum is {MAX_SCHEMA_VERSION_LEN}",
                        version.len()
                    ),
                ));
            }
        }
        Ok(())
    }

    /// Reports how this layout relates to a Manifest — **without changing it**.
    ///
    /// * A Manifest Mod entry with no partition is reported as
    ///   [`PartitionConsistency::missing_mod_partitions`]; it is never
    ///   fabricated (`§6.4`).
    /// * A Mod partition whose namespace is not declared by the Manifest is
    ///   reported as [`PartitionConsistency::unrecognized_partitions`]; it is
    ///   never deleted (`§4.4`).
    ///
    /// The Manifest's Mod IDs must be valid namespaces; a Manifest that has not
    /// been validated may therefore fail here with
    /// [`PartitionErrorCode::InvalidNamespace`].
    pub fn check_against(
        &self,
        manifest: &SaveManifest,
    ) -> Result<PartitionConsistency, PartitionError> {
        let declared: BTreeSet<&str> = manifest
            .mods
            .iter()
            .map(|entry| entry.id.as_str())
            .collect();

        let mut present: BTreeSet<&str> = BTreeSet::new();
        let mut unrecognized = Vec::new();
        for partition in &self.partitions {
            let Some(namespace) = partition.id.namespace() else {
                continue;
            };
            if declared.contains(namespace.as_str()) {
                present.insert(namespace.as_str());
            } else {
                unrecognized.push(partition.id.clone());
            }
        }

        let mut missing = Vec::new();
        for (index, entry) in manifest.mods.iter().enumerate() {
            if !present.contains(entry.id.as_str()) {
                missing.push(ModNamespace::validate_at(
                    &entry.id,
                    &format!("/mods/{index}/id"),
                )?);
            }
        }

        Ok(PartitionConsistency {
            missing_mod_partitions: missing,
            unrecognized_partitions: unrecognized,
        })
    }

    /// Encodes this layout in the canonical layout encoding.
    ///
    /// The layout is validated first, so an invalid layout (a duplicate
    /// identity, an ownership violation, or an over-long `Schema version`) is
    /// reported as a [`PartitionError`] instead of being serialized. A field
    /// that the fixed-width encoding cannot represent — a partition count, a
    /// payload or a string beyond the encoding's integer range — is likewise
    /// reported rather than truncated, and serialization never panics on input
    /// length.
    pub fn to_bytes(&self) -> Result<Vec<u8>, PartitionError> {
        self.validate()?;

        let mut ordered: Vec<&SavePartition> = self.partitions.iter().collect();
        ordered.sort_by(|left, right| left.id.cmp(&right.id));

        // A `usize` partition count is representable by the encoding's `u32`
        // field on every realistic host; the guard is defensive and is only
        // reachable through a structurally constructed value.
        let count = u32::try_from(ordered.len()).map_err(|_| {
            PartitionError::new(
                PartitionErrorCode::InvalidLayout,
                "/partitions",
                format!(
                    "layout carries {} partitions, which exceeds the {}-partition limit of the canonical encoding",
                    ordered.len(),
                    u32::MAX
                ),
            )
        })?;

        let mut out = Vec::new();
        out.extend_from_slice(&LAYOUT_ENCODING_TAG);
        out.push(LAYOUT_ENCODING_VERSION);
        push_u32(&mut out, count);
        for (index, partition) in ordered.into_iter().enumerate() {
            encode_partition(&mut out, partition, index)?;
        }
        Ok(out)
    }

    /// Decodes and validates a layout from the canonical layout encoding.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, PartitionError> {
        let mut reader = Reader::new(bytes);

        if reader.take(2, "/partitions")? != LAYOUT_ENCODING_TAG {
            return Err(PartitionError::new(
                PartitionErrorCode::InvalidLayout,
                "/partitions",
                "layout does not start with the canonical encoding tag",
            ));
        }
        let version = reader.u8("/partitions")?;
        if version != LAYOUT_ENCODING_VERSION {
            return Err(PartitionError::new(
                PartitionErrorCode::InvalidLayout,
                "/partitions",
                format!("unsupported layout encoding version {version}"),
            ));
        }

        let count = usize::try_from(reader.u32("/partitions")?).expect("u32 fits in usize");
        // Reject a declared count that the remaining bytes could not possibly
        // encode *before* allocating, so an input-controlled count cannot force a
        // large up-front allocation from a small stream. The capacity request is
        // therefore always proportional to the bytes actually present.
        let max_encodable = reader.remaining() / MIN_ENCODED_PARTITION_LEN;
        if count > max_encodable {
            return Err(PartitionError::new(
                PartitionErrorCode::InvalidLayout,
                "/partitions",
                format!(
                    "declared partition count {count} exceeds the {max_encodable} partition(s) that the remaining {} byte(s) can encode",
                    reader.remaining()
                ),
            ));
        }

        let mut partitions = Vec::with_capacity(count);
        for index in 0..count {
            partitions.push(decode_partition(&mut reader, index)?);
        }
        if !reader.is_exhausted() {
            return Err(PartitionError::new(
                PartitionErrorCode::InvalidLayout,
                "/partitions",
                format!(
                    "trailing bytes after the last partition (offset {})",
                    reader.position
                ),
            ));
        }

        let layout = Self::new(partitions);
        layout.validate()?;
        Ok(layout)
    }

    fn sort_canonical(partitions: &mut [SavePartition]) {
        partitions.sort_by(|left, right| left.id.cmp(&right.id));
    }
}

/// The relationship between a [`SaveLayout`] and a [`SaveManifest`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PartitionConsistency {
    missing_mod_partitions: Vec<ModNamespace>,
    unrecognized_partitions: Vec<SavePartitionId>,
}

impl PartitionConsistency {
    /// Manifest Mod entries that have no partition. Never fabricated.
    pub fn missing_mod_partitions(&self) -> &[ModNamespace] {
        &self.missing_mod_partitions
    }

    /// Carried Mod partitions not declared by the Manifest. Never deleted.
    pub fn unrecognized_partitions(&self) -> &[SavePartitionId] {
        &self.unrecognized_partitions
    }

    /// Whether the layout and Manifest agree exactly.
    pub fn is_consistent(&self) -> bool {
        self.missing_mod_partitions.is_empty() && self.unrecognized_partitions.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Canonical layout encoding
// ---------------------------------------------------------------------------

fn push_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn push_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}

/// Appends a `u16`-length-prefixed string.
///
/// The canonical encoding length-prefixes strings with a `u16`. A legal layout
/// never exceeds that — namespaces are bounded by construction and `Schema
/// version` strings are bounded by [`MAX_SCHEMA_VERSION_LEN`], enforced by
/// [`SaveLayout::validate`] before serialization — but the conversion is
/// fallible and **never truncates**: an unrepresentable string is reported as
/// [`PartitionErrorCode::InvalidPartition`] at `path`.
fn push_str_u16(out: &mut Vec<u8>, value: &str, path: &str) -> Result<(), PartitionError> {
    let bytes = value.as_bytes();
    let length = u16::try_from(bytes.len()).map_err(|_| {
        PartitionError::new(
            PartitionErrorCode::InvalidPartition,
            path,
            format!(
                "string is {} bytes, which exceeds the {}-byte limit of the canonical encoding",
                bytes.len(),
                u16::MAX
            ),
        )
    })?;
    push_u16(out, length);
    out.extend_from_slice(bytes);
    Ok(())
}

fn encode_partition(
    out: &mut Vec<u8>,
    partition: &SavePartition,
    index: usize,
) -> Result<(), PartitionError> {
    let base = format!("/partitions/{index}");

    match &partition.id {
        SavePartitionId::Manifest => out.push(0),
        SavePartitionId::World => out.push(1),
        SavePartitionId::Mod(namespace) => {
            out.push(2);
            push_str_u16(out, namespace.as_str(), &format!("{base}/namespace"))?;
        }
        SavePartitionId::Metadata => out.push(3),
        SavePartitionId::Integrity => out.push(4),
    }

    match &partition.owner {
        PartitionOwner::SaveSystem => out.push(0),
        PartitionOwner::Kernel => out.push(1),
        PartitionOwner::Mod(namespace) => {
            out.push(2);
            push_str_u16(out, namespace.as_str(), &format!("{base}/owner/namespace"))?;
        }
    }

    match &partition.schema_version {
        Some(version) => {
            out.push(1);
            push_str_u16(out, version, &format!("{base}/schema_version"))?;
        }
        None => out.push(0),
    }

    // A `usize` payload length is representable by the encoding's `u32` field on
    // every realistic host; the guard is defensive and is only reachable through
    // a structurally constructed value.
    let payload_length = u32::try_from(partition.payload.len()).map_err(|_| {
        PartitionError::new(
            PartitionErrorCode::InvalidPartition,
            format!("{base}/payload"),
            format!(
                "partition payload is {} bytes, which exceeds the {}-byte limit of the canonical encoding",
                partition.payload.len(),
                u32::MAX
            ),
        )
    })?;
    push_u32(out, payload_length);
    out.extend_from_slice(partition.payload.as_bytes());
    Ok(())
}

fn decode_partition(
    reader: &mut Reader<'_>,
    index: usize,
) -> Result<SavePartition, PartitionError> {
    let base = format!("/partitions/{index}");

    let kind_path = format!("{base}/kind");
    let id = match reader.u8(&kind_path)? {
        0 => SavePartitionId::Manifest,
        1 => SavePartitionId::World,
        2 => {
            let namespace = decode_namespace(reader, &format!("{base}/namespace"))?;
            SavePartitionId::Mod(namespace)
        }
        3 => SavePartitionId::Metadata,
        4 => SavePartitionId::Integrity,
        other => {
            return Err(PartitionError::new(
                PartitionErrorCode::InvalidPartition,
                kind_path,
                format!("unknown partition kind tag {other}"),
            ));
        }
    };

    let owner_path = format!("{base}/owner");
    let owner = match reader.u8(&owner_path)? {
        0 => PartitionOwner::SaveSystem,
        1 => PartitionOwner::Kernel,
        2 => {
            let namespace = decode_namespace(reader, &format!("{base}/owner/namespace"))?;
            PartitionOwner::Mod(namespace)
        }
        other => {
            return Err(PartitionError::new(
                PartitionErrorCode::InvalidPartition,
                owner_path,
                format!("unknown partition owner tag {other}"),
            ));
        }
    };

    let schema_path = format!("{base}/schema_version");
    let schema_version = match reader.u8(&schema_path)? {
        0 => None,
        1 => Some(reader.string_u16(&schema_path)?),
        other => {
            return Err(PartitionError::new(
                PartitionErrorCode::InvalidPartition,
                schema_path,
                format!("unknown schema-version flag {other}"),
            ));
        }
    };

    let payload_path = format!("{base}/payload");
    let length = usize::try_from(reader.u32(&payload_path)?).expect("u32 fits in usize");
    let payload = PartitionPayload::new(reader.take(length, &payload_path)?.to_vec());

    Ok(SavePartition {
        id,
        owner,
        payload,
        schema_version,
    })
}

fn decode_namespace(reader: &mut Reader<'_>, path: &str) -> Result<ModNamespace, PartitionError> {
    let value = reader.string_u16(path)?;
    ModNamespace::validate_at(&value, path)
}

/// A bounds-checked cursor over a layout byte slice.
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

    fn take(&mut self, length: usize, path: &str) -> Result<&'a [u8], PartitionError> {
        if self.remaining() < length {
            return Err(PartitionError::new(
                PartitionErrorCode::InvalidLayout,
                path,
                format!(
                    "layout is truncated: need {length} byte(s) at offset {}, only {} remain",
                    self.position,
                    self.remaining()
                ),
            ));
        }
        let slice = &self.bytes[self.position..self.position + length];
        self.position += length;
        Ok(slice)
    }

    fn u8(&mut self, path: &str) -> Result<u8, PartitionError> {
        Ok(self.take(1, path)?[0])
    }

    fn u16(&mut self, path: &str) -> Result<u16, PartitionError> {
        let bytes = self.take(2, path)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn u32(&mut self, path: &str) -> Result<u32, PartitionError> {
        let bytes = self.take(4, path)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn string_u16(&mut self, path: &str) -> Result<String, PartitionError> {
        let length = usize::from(self.u16(path)?);
        let bytes = self.take(length, path)?;
        std::str::from_utf8(bytes).map(str::to_owned).map_err(|_| {
            PartitionError::new(
                PartitionErrorCode::InvalidLayout,
                path,
                "layout string is not valid UTF-8",
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn namespace(value: &str) -> ModNamespace {
        ModNamespace::parse(value).expect("test namespace is valid")
    }

    #[test]
    fn canonical_order_is_stable_and_matches_declaration_order() {
        let layout = SaveLayout::new(vec![
            SavePartition::new(SavePartitionId::Integrity, b"i"),
            SavePartition::new(SavePartitionId::Mod(namespace("org.example.z")), b"z"),
            SavePartition::new(SavePartitionId::World, b"w"),
            SavePartition::new(SavePartitionId::Mod(namespace("org.example.a")), b"a"),
            SavePartition::new(SavePartitionId::Metadata, b"m"),
            SavePartition::new(SavePartitionId::Manifest, b"c"),
        ]);
        let kinds: Vec<&str> = layout
            .partitions()
            .iter()
            .map(|partition| partition.id.kind_str())
            .collect();
        assert_eq!(
            kinds,
            ["manifest", "world", "mod", "mod", "metadata", "integrity"]
        );
        let namespaces: Vec<&str> = layout
            .mod_partitions()
            .map(|partition| partition.id.namespace().map(ModNamespace::as_str).unwrap())
            .collect();
        assert_eq!(namespaces, ["org.example.a", "org.example.z"]);
    }

    #[test]
    fn duplicate_identities_are_rejected() {
        let layout = SaveLayout::new(vec![
            SavePartition::new(SavePartitionId::World, b"a"),
            SavePartition::new(SavePartitionId::World, b"b"),
        ]);
        assert_eq!(
            layout.validate().unwrap_err().code(),
            PartitionErrorCode::DuplicatePartition
        );
    }

    #[test]
    fn layout_encoding_rejects_a_foreign_tag() {
        assert_eq!(
            SaveLayout::from_bytes(b"not a layout").unwrap_err().code(),
            PartitionErrorCode::InvalidLayout
        );
    }
}
