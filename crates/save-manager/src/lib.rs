//! P4 Save & Compatibility System implementation (`SAVE-001` through
//! `SAVE-012`; see `planning/22-P4-存档兼容与重放任务卡草案.md`). It provides
//! the versioned Manifest and partition model, integrity generation and
//! verification, atomic save/load, compatibility assessment, migration
//! planning and transaction recovery, backup/restore, Mod change planning,
//! cross-Mod reference validation, and structured diagnostics.
//!
//! ## Module boundaries
//!
//! * Manifest validation remains dual-track: [`SAVE_MANIFEST_SCHEMA_JSON`]
//!   checks structure and syntax; [`SaveManifest::validate`] checks semantics.
//!   Errors use stable codes and JSON Pointers.
//! * [`integrity`] defines canonical digests; [`writer`] generates and records
//!   them. The `root_hash` field is not trusted as a caller supplied digest.
//! * File I/O is confined to the explicit save/backup/transaction modules;
//!   compatibility, migration registration, Mod change policy, and reference
//!   extraction remain independently testable contracts.
//! * `kernel-core` remains authoritative for World state and does not depend on
//!   this crate. Save code stores Kernel snapshots as opaque bytes until an
//!   explicitly supplied validator restores and validates them.
//! * Mod data is opaque to Save System. Platform integrations provide scoped
//!   payloads and trusted reference extractors; this crate does not give Mods
//!   mutable World access or unrestricted storage APIs.

#![forbid(unsafe_code)]

pub mod backup;
pub mod compat;
pub mod diagnostics;
pub mod integrity;
pub mod loader;
pub mod migration;
pub mod mod_changes;
pub mod partition;
pub mod references;
pub mod transaction;
pub mod writer;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::OnceLock;

use kernel_core::Tick;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The published `save-manifest-v1` JSON Schema (Draft 2020-12).
///
/// Exposed so consumers and contract tests can prove the model's exported bytes
/// satisfy the same schema the crate validates against.
pub const SAVE_MANIFEST_SCHEMA_JSON: &str =
    include_str!("../../../schemas/save-manifest-v1.schema.json");

/// The highest Save Format Version this build understands (`§3.4`, `§12.2`).
///
/// A save whose `save_format_version` is *greater* than this value MUST be
/// rejected for ordinary writes; the platform must never guess the meaning of a
/// future format's fields.
pub const SUPPORTED_SAVE_FORMAT_VERSION: SaveFormatVersion = SaveFormatVersion::new(1, 0);

/// The only integrity algorithm recognised by `SAVE-001`.
///
/// Additional algorithms are introduced together with the task that produces
/// the digest (`SAVE-003`).
pub const SUPPORTED_INTEGRITY_ALGORITHM: &str = "sha256";

/// The placeholder `root_hash` used by the specification example (`§3.3`).
///
/// It is illustrative only and MUST NOT be treated as a valid checksum.
pub const SPEC_EXAMPLE_ROOT_HASH_PLACEHOLDER: &str = "IMPLEMENTATION_GENERATED";

// ---------------------------------------------------------------------------
// Version model
// ---------------------------------------------------------------------------

/// A `MAJOR.MINOR` save-domain version such as `1.0`.
///
/// Used for the Save Format Version, the Kernel API Version, the World Schema
/// Version and each Mod's Schema Version (`§3.4`). It serialises as a JSON
/// string and orders by `(major, minor)` so it can be compared against
/// [`SUPPORTED_SAVE_FORMAT_VERSION`].
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SaveFormatVersion {
    major: u32,
    minor: u32,
}

impl SaveFormatVersion {
    /// Builds a version from its components.
    pub const fn new(major: u32, minor: u32) -> Self {
        Self { major, minor }
    }

    /// The major component.
    pub const fn major(self) -> u32 {
        self.major
    }

    /// The minor component.
    pub const fn minor(self) -> u32 {
        self.minor
    }

    /// Parses a strict `MAJOR.MINOR` string.
    ///
    /// Both components must be non-empty ASCII digit runs; anything else
    /// (missing component, extra dot, leading `v`, whitespace) is rejected.
    pub fn parse(value: &str) -> Result<Self, SaveFormatVersionParseError> {
        let failure = |reason: &'static str| SaveFormatVersionParseError {
            value: value.to_owned(),
            reason,
        };
        let (major, minor) = value
            .split_once('.')
            .ok_or_else(|| failure("expected `MAJOR.MINOR`"))?;
        if minor.contains('.') {
            return Err(failure("expected exactly one `.` separator"));
        }
        if major.is_empty() {
            return Err(failure("major component is empty"));
        }
        if minor.is_empty() {
            return Err(failure("minor component is empty"));
        }
        if !major.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(failure("major component must be ASCII digits"));
        }
        if !minor.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(failure("minor component must be ASCII digits"));
        }
        let major = major
            .parse::<u32>()
            .map_err(|_| failure("major component out of range"))?;
        let minor = minor
            .parse::<u32>()
            .map_err(|_| failure("minor component out of range"))?;
        Ok(Self { major, minor })
    }

    /// Whether this version is at or below the platform's supported maximum.
    pub fn is_supported(self) -> bool {
        self.is_supported_by(SUPPORTED_SAVE_FORMAT_VERSION)
    }

    /// Whether this version is at or below `max` (`§12.2` "unknown future
    /// version" test).
    pub fn is_supported_by(self, max: Self) -> bool {
        self <= max
    }
}

impl fmt::Display for SaveFormatVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}", self.major, self.minor)
    }
}

impl From<SaveFormatVersion> for String {
    fn from(version: SaveFormatVersion) -> Self {
        version.to_string()
    }
}

impl TryFrom<String> for SaveFormatVersion {
    type Error = SaveFormatVersionParseError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

/// Failure to parse a [`SaveFormatVersion`] string.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("invalid save format version `{value}`: {reason}")]
pub struct SaveFormatVersionParseError {
    value: String,
    reason: &'static str,
}

impl SaveFormatVersionParseError {
    /// The rejected input.
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Why the input was rejected.
    pub fn reason(&self) -> &'static str {
        self.reason
    }
}

// ---------------------------------------------------------------------------
// Stable error codes
// ---------------------------------------------------------------------------

/// Stable, machine-readable save Manifest error codes.
///
/// Diagnostics change; machines MUST match on these codes. The set is frozen
/// for `SAVE-001` and covered by contract tests.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SaveManifestErrorCode {
    /// The manifest is not a JSON object, is malformed JSON, or violates the
    /// published structural schema.
    InvalidManifest,
    /// A required metadata field is absent.
    MissingField,
    /// A field is present but its value is invalid (identifier, timestamp,
    /// version syntax, duplicate-free Mod list, integrity algorithm/hash).
    InvalidField,
    /// `save_format_version` is newer than the platform supports (`§12.2`).
    UnsupportedFormatVersion,
    /// A `mods[]` entry is malformed.
    InvalidModEntry,
    /// Two `mods[]` entries share the same Mod ID.
    DuplicateModId,
    /// The embedded `save-manifest-v1` schema could not be compiled.
    InvalidSchema,
}

impl SaveManifestErrorCode {
    /// The frozen wire representation.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidManifest => "save.invalid_manifest",
            Self::MissingField => "save.missing_field",
            Self::InvalidField => "save.invalid_field",
            Self::UnsupportedFormatVersion => "save.unsupported_format_version",
            Self::InvalidModEntry => "save.invalid_mod_entry",
            Self::DuplicateModId => "save.duplicate_mod_id",
            Self::InvalidSchema => "save.invalid_schema",
        }
    }
}

/// A save Manifest validation failure.
///
/// The [`code`](Self::code) and [`path`](Self::path) form the machine protocol;
/// [`message`](Self::message) is human-readable only.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SaveManifestError {
    code: SaveManifestErrorCode,
    path: String,
    message: String,
}

impl SaveManifestError {
    /// Builds a failure from a stable code, a JSON Pointer and a message.
    pub fn new(
        code: SaveManifestErrorCode,
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
    pub fn code(&self) -> SaveManifestErrorCode {
        self.code
    }

    /// The frozen wire representation of [`code`](Self::code).
    pub fn code_str(&self) -> &'static str {
        self.code.as_str()
    }

    /// The JSON Pointer to the offending field (`""` means the document root).
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The human-readable message. Not part of the protocol.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for SaveManifestError {
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

impl std::error::Error for SaveManifestError {}

fn invalid(path: impl Into<String>, message: impl Into<String>) -> SaveManifestError {
    SaveManifestError::new(SaveManifestErrorCode::InvalidField, path, message)
}

fn missing(path: impl Into<String>, message: impl Into<String>) -> SaveManifestError {
    SaveManifestError::new(SaveManifestErrorCode::MissingField, path, message)
}

// ---------------------------------------------------------------------------
// Data model
// ---------------------------------------------------------------------------

/// Determinism-relevant run configuration recorded with the save (`§3.2`).
///
/// Modelled as a JSON object so its keys can evolve without a format change.
/// A [`BTreeMap`] keeps serialization byte-stable regardless of insertion order.
pub type SimulationConfig = BTreeMap<String, serde_json::Value>;

/// The Game Definition a save was created by (`§3.2`).
///
/// This is **metadata**: it is recorded here but is deliberately *not* one of
/// the six authoritative version fields of `§3.4`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GameDefinition {
    /// Stable Game Definition identifier.
    pub id: String,
    /// Game Definition release version (SemVer).
    pub version: String,
}

/// One entry of the save's Mod manifest (`§3.2`).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SaveModEntry {
    /// Mod identifier.
    pub id: String,
    /// Mod release version (SemVer).
    pub version: String,
    /// The Mod's persistent data Schema version (`MAJOR.MINOR`).
    pub schema_version: String,
}

/// Integrity metadata carried by the save (`§3.2`).
///
/// `SAVE-001` never computes [`Integrity::root_hash`]; it only carries the field
/// and rejects missing or placeholder values.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Integrity {
    /// Digest algorithm (only [`SUPPORTED_INTEGRITY_ALGORITHM`] is accepted).
    pub algorithm: String,
    /// The digest itself, a lowercase hex string. Produced by `SAVE-003`.
    pub root_hash: String,
}

/// The save Manifest: the required metadata of a save (`§3.2`, `§3.3`).
///
/// Serialises deterministically (fixed field order; sorted mapping keys) so a
/// round trip through [`SaveManifest::to_bytes`] and [`SaveManifest::from_bytes`]
/// is byte-identical.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SaveManifest {
    /// Save Format Version (container/metadata format).
    pub save_format_version: SaveFormatVersion,
    /// Unique save identifier.
    pub save_id: String,
    /// Creation timestamp (RFC 3339, UTC).
    pub created_at: String,
    /// Most recent successful save timestamp (RFC 3339, UTC).
    pub last_saved_at: String,
    /// Platform release version (SemVer).
    pub platform_version: String,
    /// The Game Definition this save was created by.
    pub game_definition: GameDefinition,
    /// Kernel API contract version.
    pub kernel_api_version: String,
    /// World data Schema version.
    pub world_schema_version: String,
    /// Current logical time (Tick).
    pub logical_tick: Tick,
    /// The Mod manifest recorded at creation time.
    pub mods: Vec<SaveModEntry>,
    /// Determinism-relevant run configuration.
    pub simulation_config: SimulationConfig,
    /// Integrity metadata (carried, not generated here).
    pub integrity: Integrity,
}

impl SaveManifest {
    /// Parses and fully validates a manifest from JSON bytes.
    ///
    /// Runs the dual-track pipeline: required-field check, structural schema,
    /// typed deserialization, then semantic validation.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, SaveManifestError> {
        let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|error| {
            SaveManifestError::new(
                SaveManifestErrorCode::InvalidManifest,
                "",
                format!("manifest is not valid JSON: {error}"),
            )
        })?;
        Self::from_value(value)
    }

    /// Validates and materialises a manifest from a JSON value.
    pub fn from_value(value: serde_json::Value) -> Result<Self, SaveManifestError> {
        validate_manifest_value(&value)?;
        let manifest: Self = serde_json::from_value(value).map_err(|error| {
            SaveManifestError::new(
                SaveManifestErrorCode::InvalidManifest,
                "",
                format!("manifest does not match the SaveManifest model: {error}"),
            )
        })?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Serialises this manifest to deterministic JSON bytes.
    ///
    /// The model is always serializable (no non-string mapping keys), so this is
    /// infallible. Validation is a separate step: see [`Self::validate`].
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("SaveManifest always serializes to JSON")
    }

    /// Serialises this manifest to a JSON value.
    pub fn to_value(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("SaveManifest always serializes to JSON")
    }

    /// Semantic validation of an already-materialised manifest.
    ///
    /// Enforces the value-level rules the structural schema deliberately leaves
    /// open: supported format version, identifier and timestamp syntax, SemVer
    /// fields, the six authoritative version fields, a non-empty duplicate-free
    /// Mod list, and integrity algorithm/hash.
    pub fn validate(&self) -> Result<(), SaveManifestError> {
        if self.save_format_version > SUPPORTED_SAVE_FORMAT_VERSION {
            return Err(SaveManifestError::new(
                SaveManifestErrorCode::UnsupportedFormatVersion,
                "/save_format_version",
                format!(
                    "save format `{}` is newer than the supported `{}`; future formats must be rejected (§12.2)",
                    self.save_format_version, SUPPORTED_SAVE_FORMAT_VERSION
                ),
            ));
        }

        if !is_valid_save_id(&self.save_id) {
            return Err(invalid(
                "/save_id",
                "save_id must be 1-128 characters of one or more lowercase ASCII alphanumeric segments separated by `.`, `_` or `-`",
            ));
        }

        for (path, value) in [
            ("/created_at", self.created_at.as_str()),
            ("/last_saved_at", self.last_saved_at.as_str()),
        ] {
            if !is_valid_timestamp(value) {
                return Err(invalid(
                    path,
                    "timestamp must be RFC 3339 UTC of the form `YYYY-MM-DDThh:mm:ss[.fff]Z`",
                ));
            }
        }

        if semver::Version::parse(&self.platform_version).is_err() {
            return Err(invalid(
                "/platform_version",
                "platform_version must be a valid SemVer string",
            ));
        }

        if !is_valid_identifier(&self.game_definition.id) {
            return Err(invalid(
                "/game_definition/id",
                "game_definition.id must be a dotted lowercase identifier",
            ));
        }
        if semver::Version::parse(&self.game_definition.version).is_err() {
            return Err(invalid(
                "/game_definition/version",
                "game_definition.version must be a valid SemVer string",
            ));
        }

        if SaveFormatVersion::parse(&self.kernel_api_version).is_err() {
            return Err(invalid(
                "/kernel_api_version",
                "kernel_api_version must be `MAJOR.MINOR`",
            ));
        }
        if SaveFormatVersion::parse(&self.world_schema_version).is_err() {
            return Err(invalid(
                "/world_schema_version",
                "world_schema_version must be `MAJOR.MINOR`",
            ));
        }

        if self.mods.is_empty() {
            return Err(invalid(
                "/mods",
                "mods must not be empty: a save always records the Mod manifest it was created with (§3.2)",
            ));
        }
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for (index, entry) in self.mods.iter().enumerate() {
            if !is_valid_identifier(&entry.id) {
                return Err(SaveManifestError::new(
                    SaveManifestErrorCode::InvalidModEntry,
                    format!("/mods/{index}/id"),
                    "mod id must be a dotted lowercase identifier",
                ));
            }
            if semver::Version::parse(&entry.version).is_err() {
                return Err(SaveManifestError::new(
                    SaveManifestErrorCode::InvalidModEntry,
                    format!("/mods/{index}/version"),
                    "mod version must be a valid SemVer string",
                ));
            }
            if SaveFormatVersion::parse(&entry.schema_version).is_err() {
                return Err(SaveManifestError::new(
                    SaveManifestErrorCode::InvalidModEntry,
                    format!("/mods/{index}/schema_version"),
                    "mod schema_version must be `MAJOR.MINOR`",
                ));
            }
            if !seen.insert(entry.id.as_str()) {
                return Err(SaveManifestError::new(
                    SaveManifestErrorCode::DuplicateModId,
                    format!("/mods/{index}/id"),
                    format!("duplicate mod id `{}`", entry.id),
                ));
            }
        }

        if self.integrity.algorithm != SUPPORTED_INTEGRITY_ALGORITHM {
            return Err(invalid(
                "/integrity/algorithm",
                format!(
                    "integrity algorithm `{}` is unsupported; expected `{SUPPORTED_INTEGRITY_ALGORITHM}`",
                    self.integrity.algorithm
                ),
            ));
        }
        if self.integrity.root_hash == SPEC_EXAMPLE_ROOT_HASH_PLACEHOLDER {
            return Err(invalid(
                "/integrity/root_hash",
                "the specification example placeholder must not be used as a real digest (§3.3)",
            ));
        }
        if !is_valid_root_hash(&self.integrity.root_hash) {
            return Err(invalid(
                "/integrity/root_hash",
                "root_hash must be 64 lowercase hexadecimal characters and is generated by SAVE-003, not SAVE-001",
            ));
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Structural validation (required fields + published schema)
// ---------------------------------------------------------------------------

const REQUIRED_TOP_LEVEL: [&str; 12] = [
    "save_format_version",
    "save_id",
    "created_at",
    "last_saved_at",
    "platform_version",
    "game_definition",
    "kernel_api_version",
    "world_schema_version",
    "logical_tick",
    "mods",
    "simulation_config",
    "integrity",
];

fn schema_validator() -> Result<&'static jsonschema::Validator, SaveManifestError> {
    static VALIDATOR: OnceLock<Result<jsonschema::Validator, String>> = OnceLock::new();
    let validator = VALIDATOR.get_or_init(|| {
        let schema: serde_json::Value =
            serde_json::from_str(SAVE_MANIFEST_SCHEMA_JSON).map_err(|error| error.to_string())?;
        jsonschema::validator_for(&schema).map_err(|error| error.to_string())
    });
    validator.as_ref().map_err(|message| {
        SaveManifestError::new(
            SaveManifestErrorCode::InvalidSchema,
            "",
            format!("embedded save-manifest-v1 schema is invalid: {message}"),
        )
    })
}

/// Semantic validation of an already-materialised manifest.
///
/// Free-function form of [`SaveManifest::validate`]; the `SAVE-001` interface
/// contract is frozen with this exact signature.
pub fn validate_manifest(manifest: &SaveManifest) -> Result<(), SaveManifestError> {
    manifest.validate()
}

/// Structural validation of a JSON manifest value.
///
/// Performs the required-field check (producing precise JSON Pointers) and then
/// the published-schema check. Value-level semantics are enforced by
/// [`SaveManifest::validate`].
pub fn validate_manifest_value(value: &serde_json::Value) -> Result<(), SaveManifestError> {
    let Some(object) = value.as_object() else {
        return Err(SaveManifestError::new(
            SaveManifestErrorCode::InvalidManifest,
            "",
            "manifest must be a JSON object",
        ));
    };

    for key in REQUIRED_TOP_LEVEL {
        if !object.contains_key(key) {
            return Err(missing(
                format!("/{key}"),
                format!("required field `{key}` is missing"),
            ));
        }
    }
    if let Some(game) = object
        .get("game_definition")
        .and_then(|value| value.as_object())
    {
        for key in ["id", "version"] {
            if !game.contains_key(key) {
                return Err(missing(
                    format!("/game_definition/{key}"),
                    format!("required field `game_definition.{key}` is missing"),
                ));
            }
        }
    }
    if let Some(integrity) = object.get("integrity").and_then(|value| value.as_object()) {
        for key in ["algorithm", "root_hash"] {
            if !integrity.contains_key(key) {
                return Err(missing(
                    format!("/integrity/{key}"),
                    format!("required field `integrity.{key}` is missing"),
                ));
            }
        }
    }
    if let Some(mods) = object.get("mods").and_then(|value| value.as_array()) {
        for (index, entry) in mods.iter().enumerate() {
            let Some(entry) = entry.as_object() else {
                continue;
            };
            for key in ["id", "version", "schema_version"] {
                if !entry.contains_key(key) {
                    return Err(missing(
                        format!("/mods/{index}/{key}"),
                        format!("required field `mods[{index}].{key}` is missing"),
                    ));
                }
            }
        }
    }

    let validator = schema_validator()?;
    if let Err(error) = validator.validate(value) {
        return Err(SaveManifestError::new(
            SaveManifestErrorCode::InvalidManifest,
            error.instance_path().to_string(),
            format!("manifest violates the save-manifest-v1 schema: {error}"),
        ));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Value syntax helpers
// ---------------------------------------------------------------------------

fn is_lower_alnum(byte: u8) -> bool {
    byte.is_ascii_lowercase() || byte.is_ascii_digit()
}

fn is_valid_id_segment(segment: &str) -> bool {
    let bytes = segment.as_bytes();
    match (bytes.first(), bytes.last()) {
        (Some(&first), Some(&last)) => {
            is_lower_alnum(first)
                && is_lower_alnum(last)
                && bytes
                    .iter()
                    .all(|&byte| is_lower_alnum(byte) || byte == b'-')
        }
        _ => false,
    }
}

fn is_valid_identifier(value: &str) -> bool {
    !value.is_empty() && value.len() <= 128 && value.split('.').all(is_valid_id_segment)
}

fn is_valid_save_id(value: &str) -> bool {
    if value.is_empty() || value.len() > 128 {
        return false;
    }
    let mut previous_was_separator = true;
    let mut last_was_alnum = false;
    for &byte in value.as_bytes() {
        if is_lower_alnum(byte) {
            previous_was_separator = false;
            last_was_alnum = true;
        } else if matches!(byte, b'.' | b'_' | b'-') {
            if previous_was_separator {
                return false;
            }
            previous_was_separator = true;
            last_was_alnum = false;
        } else {
            return false;
        }
    }
    last_was_alnum
}

pub(crate) fn is_valid_timestamp(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() < 20 {
        return false;
    }
    if bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return false;
    }
    let digits_at = |start: usize| -> Option<u32> {
        let high = bytes[start].checked_sub(b'0')?;
        let low = bytes[start + 1].checked_sub(b'0')?;
        if high > 9 || low > 9 {
            return None;
        }
        Some(u32::from(high) * 10 + u32::from(low))
    };
    if !bytes[0..4].iter().all(u8::is_ascii_digit) {
        return false;
    }
    let (Some(month), Some(day), Some(hour), Some(minute), Some(second)) = (
        digits_at(5),
        digits_at(8),
        digits_at(11),
        digits_at(14),
        digits_at(17),
    ) else {
        return false;
    };
    let year = bytes[0..4]
        .iter()
        .fold(0_u32, |year, digit| year * 10 + u32::from(digit - b'0'));
    let days_in_month = match month {
        2 if year % 400 == 0 || (year % 4 == 0 && year % 100 != 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        _ => return false,
    };
    if day == 0 || day > days_in_month || hour > 23 || minute > 59 || second > 60 {
        return false;
    }
    let tail = &bytes[19..];
    if tail.last() != Some(&b'Z') {
        return false;
    }
    let body = &tail[..tail.len() - 1];
    if body.is_empty() {
        return true;
    }
    body[0] == b'.' && body.len() >= 2 && body[1..].iter().all(u8::is_ascii_digit)
}

fn is_valid_root_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_version_parses_and_orders() {
        let version = SaveFormatVersion::parse("1.2").unwrap();
        assert_eq!(version.major(), 1);
        assert_eq!(version.minor(), 2);
        assert_eq!(version.to_string(), "1.2");
        assert!(SaveFormatVersion::new(1, 0) < SaveFormatVersion::new(1, 1));
        assert!(SaveFormatVersion::new(1, 9) < SaveFormatVersion::new(2, 0));
    }

    #[test]
    fn format_version_rejects_malformed_input() {
        for input in ["", "1", "1.", ".0", "1.2.3", "v1.0", "1.0 ", "1.-1", "a.b"] {
            assert!(
                SaveFormatVersion::parse(input).is_err(),
                "expected `{input}` to be rejected"
            );
        }
    }

    #[test]
    fn supported_version_boundary() {
        assert!(SaveFormatVersion::new(1, 0).is_supported());
        assert!(!SaveFormatVersion::new(1, 1).is_supported());
        assert!(!SaveFormatVersion::new(2, 0).is_supported());
    }

    #[test]
    fn identifier_rules() {
        assert!(is_valid_identifier("org.example.base-game"));
        assert!(is_valid_identifier("a"));
        assert!(!is_valid_identifier(""));
        assert!(!is_valid_identifier("Org.Example"));
        assert!(!is_valid_identifier(".org"));
        assert!(!is_valid_identifier("org..example"));
        assert!(!is_valid_identifier("org.example-"));
    }

    #[test]
    fn save_id_rules() {
        assert!(is_valid_save_id("save-example-001"));
        assert!(is_valid_save_id("a"));
        assert!(is_valid_save_id("save.2026_01"));
        assert!(!is_valid_save_id(""));
        assert!(!is_valid_save_id("-leading"));
        assert!(!is_valid_save_id("trailing-"));
        assert!(!is_valid_save_id("double--separator"));
        assert!(!is_valid_save_id("Uppercase"));
    }

    #[test]
    fn timestamp_rules() {
        assert!(is_valid_timestamp("2026-01-01T12:00:00Z"));
        assert!(is_valid_timestamp("2026-12-31T23:59:60Z"));
        assert!(is_valid_timestamp("2026-01-01T12:00:00.500Z"));
        assert!(is_valid_timestamp("2024-02-29T12:00:00Z"));
        assert!(is_valid_timestamp("2026-02-28T12:00:00Z"));
        assert!(is_valid_timestamp("2026-04-30T12:00:00Z"));
        assert!(!is_valid_timestamp("2026-01-01T12:00:00"));
        assert!(!is_valid_timestamp("2026-13-01T12:00:00Z"));
        assert!(!is_valid_timestamp("2026-02-29T12:00:00Z"));
        assert!(!is_valid_timestamp("2026-02-31T12:00:00Z"));
        assert!(!is_valid_timestamp("2026-04-31T12:00:00Z"));
        assert!(!is_valid_timestamp("2026-01-01T24:00:00Z"));
        assert!(!is_valid_timestamp("2026-01-01 12:00:00Z"));
        assert!(!is_valid_timestamp("2026-01-01T12:00:00+08:00"));
    }

    #[test]
    fn root_hash_rules() {
        assert!(is_valid_root_hash(&"a".repeat(64)));
        assert!(!is_valid_root_hash(&"a".repeat(63)));
        assert!(!is_valid_root_hash(&"A".repeat(64)));
        assert!(!is_valid_root_hash(SPEC_EXAMPLE_ROOT_HASH_PLACEHOLDER));
    }
}
