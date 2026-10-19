//! `SAVE-004`: consistent snapshot and the normal save path.
//!
//! This module is the fourth increment of the P4 Save System. It assembles a
//! save container from a caller-supplied world snapshot, generates the
//! `SAVE-003` integrity digest, and commits the container through the frozen
//! "temporary file → `sync_all` → read-back verify → `rename`" sequence. The
//! single frozen implementation contract is
//! `planning/30-P4-SAVE-004-正常保存路径实施契约.md`; where this module states
//! a rule, that document is authoritative.
//!
//! ## Scope boundary (frozen for this increment)
//!
//! * **This is the only production module in this crate that performs save
//!   file I/O** (contract §13; enforced by a crate-level discipline test).
//! * **No read/load API** (DC-5), no demo wiring (DC-8), no success-record
//!   persistence (§6 step 9 / §16), no orphan temp-file cleanup (SAVE-007),
//!   no production fault-injection API, no parent-directory sync and no
//!   power-loss durability claim (DC-3 constraints).
//! * Concurrent saves to one path follow **S1**: the last successful
//!   committer wins, with no logical-ordering guarantee (DC-11).
//! * Cleanup removes only the exact temporary file this call created
//!   (DC-6); a cleanup failure never replaces the primary error.
//!
//! ## System manifest (`C-1` / `C-2`, contract §5)
//!
//! The registered System IDs are recorded in the `metadata/session-data`
//! partition as an `SM` v1 payload: tag `SM` ‖ version `1` ‖ `u32` big-endian
//! count (≤ [`MAX_SYSTEM_MANIFEST_ENTRIES`]) ‖ count × (`u16` big-endian
//! length ‖ UTF-8 ID). An empty list (`count = 0`) is distinct from a missing
//! payload. IDs must satisfy `kernel-core`'s stable-ID rule
//! ([`validate_type_id`]), be unique, and appear in lexicographic order — the
//! same order [`Kernel::registered_system_ids`] yields.
//!
//! [`Kernel::registered_system_ids`]: kernel_core::Kernel::registered_system_ids

use std::fmt;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use kernel_core::validate_type_id;

use crate::integrity::{self, IntegrityError, IntegrityReport};
use crate::partition::{
    PartitionError, PartitionPayload, SaveLayout, SavePartition, SavePartitionId,
};
use crate::{SaveManifest, SaveManifestError};

/// Encoding tag of the system manifest payload (`SM` v1, contract §5).
pub const SYSTEM_MANIFEST_ENCODING_TAG: [u8; 2] = *b"SM";

/// Version of the system manifest payload encoding.
pub const SYSTEM_MANIFEST_ENCODING_VERSION: u8 = 1;

/// Maximum number of System IDs one manifest payload can record.
pub const MAX_SYSTEM_MANIFEST_ENTRIES: usize = 4096;

/// Maximum byte length of one System ID (aligned with [`validate_type_id`]).
pub const MAX_SYSTEM_ID_LEN: usize = 128;

/// The smallest number of bytes one encoded System ID entry can occupy
/// (`u16` length prefix + one ID byte). Decoding rejects a declared count the
/// remaining bytes could not possibly encode *before* allocating.
const MIN_ENCODED_SYSTEM_ID_LEN: usize = 3;

// ---------------------------------------------------------------------------
// System manifest errors
// ---------------------------------------------------------------------------

/// Stable, machine-readable system manifest error codes (frozen: exactly two).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SystemManifestErrorCode {
    /// The payload or its carrying partition is absent. Not reachable on the
    /// save path (saving always writes the manifest); reserved for `SAVE-005`
    /// load-side detection of old saves without a manifest.
    Missing,
    /// The manifest payload is malformed or its IDs violate the frozen rules
    /// (tag/version/truncation/non-UTF-8/duplicate or out-of-order/over-limit).
    Invalid,
}

impl SystemManifestErrorCode {
    /// The frozen wire representation.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "save.system_manifest_missing",
            Self::Invalid => "save.system_manifest_invalid",
        }
    }

    /// Every frozen code, for exhaustiveness checks.
    pub const ALL: [Self; 2] = [Self::Missing, Self::Invalid];
}

/// A system manifest encoding or decoding failure.
///
/// [`code`](Self::code) and [`path`](Self::path) form the machine protocol;
/// [`message`](Self::message) is human-readable only.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SystemManifestError {
    code: SystemManifestErrorCode,
    path: String,
    message: String,
}

impl SystemManifestError {
    /// Builds a failure from a stable code, a logical pointer and a message.
    pub fn new(
        code: SystemManifestErrorCode,
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
    pub fn code(&self) -> SystemManifestErrorCode {
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

impl fmt::Display for SystemManifestError {
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

impl std::error::Error for SystemManifestError {}

fn invalid_manifest(path: impl Into<String>, message: impl Into<String>) -> SystemManifestError {
    SystemManifestError::new(SystemManifestErrorCode::Invalid, path, message)
}

// ---------------------------------------------------------------------------
// System manifest codec (`SM` v1)
// ---------------------------------------------------------------------------

/// Encodes the registered System IDs as an `SM` v1 payload (contract §5).
///
/// The caller collects the IDs from [`Kernel::registered_system_ids`]; this
/// function re-validates them (stable-ID shape, lexicographic order,
/// uniqueness, count limit) so an invalid list fails with
/// [`SystemManifestErrorCode::Invalid`] before any byte is produced.
///
/// [`Kernel::registered_system_ids`]: kernel_core::Kernel::registered_system_ids
pub fn encode_system_manifest(system_ids: &[&str]) -> Result<Vec<u8>, SystemManifestError> {
    if system_ids.len() > MAX_SYSTEM_MANIFEST_ENTRIES {
        return Err(invalid_manifest(
            "/system_manifest/entries",
            format!(
                "{} system IDs exceed the {}-entry encoding limit",
                system_ids.len(),
                MAX_SYSTEM_MANIFEST_ENTRIES
            ),
        ));
    }

    let mut out = Vec::new();
    out.extend_from_slice(&SYSTEM_MANIFEST_ENCODING_TAG);
    out.push(SYSTEM_MANIFEST_ENCODING_VERSION);
    let count = u32::try_from(system_ids.len()).expect("bounded by MAX_SYSTEM_MANIFEST_ENTRIES");
    out.extend_from_slice(&count.to_be_bytes());

    let mut previous: Option<&str> = None;
    for (index, id) in system_ids.iter().copied().enumerate() {
        let path = format!("/system_manifest/entries/{index}");
        if validate_type_id(id).is_err() {
            return Err(invalid_manifest(
                path,
                format!("system ID `{id}` is not a valid stable system ID"),
            ));
        }
        if !previous.is_none_or(|prior| id > prior) {
            return Err(invalid_manifest(
                path,
                "system IDs must be in lexicographic order and free of duplicates",
            ));
        }
        previous = Some(id);
        let length = u16::try_from(id.len()).expect("bounded by validate_type_id");
        out.extend_from_slice(&length.to_be_bytes());
        out.extend_from_slice(id.as_bytes());
    }
    Ok(out)
}

/// Decodes and strictly validates an `SM` v1 payload.
///
/// The mirror of [`encode_system_manifest`]. Anything not exactly canonical —
/// a wrong tag or version, truncation, trailing bytes, a non-UTF-8 ID, an ID
/// violating the stable-ID rule, or a duplicate/out-of-order ID — is rejected
/// with [`SystemManifestErrorCode::Invalid`].
pub fn decode_system_manifest(bytes: &[u8]) -> Result<Vec<String>, SystemManifestError> {
    let mut reader = Reader::new(bytes);

    if reader.take(2, "/system_manifest/tag")? != SYSTEM_MANIFEST_ENCODING_TAG {
        return Err(invalid_manifest(
            "/system_manifest/tag",
            "system manifest does not start with the `SM` encoding tag",
        ));
    }
    let version = reader.u8("/system_manifest/version")?;
    if version != SYSTEM_MANIFEST_ENCODING_VERSION {
        return Err(invalid_manifest(
            "/system_manifest/version",
            format!("unsupported system manifest version {version}"),
        ));
    }

    let count = usize::try_from(reader.u32("/system_manifest/entries")?).map_err(|_| {
        invalid_manifest(
            "/system_manifest/entries",
            "system manifest entry count does not fit this platform",
        )
    })?;
    let max_encodable = reader.remaining() / MIN_ENCODED_SYSTEM_ID_LEN;
    if count > max_encodable {
        return Err(invalid_manifest(
            "/system_manifest/entries",
            format!(
                "declared system ID count {count} exceeds the {max_encodable} entry(ies) the remaining {} byte(s) can encode",
                reader.remaining()
            ),
        ));
    }

    let mut ids = Vec::with_capacity(count);
    let mut previous: Option<String> = None;
    for index in 0..count {
        let path = format!("/system_manifest/entries/{index}");
        let length = usize::from(reader.u16(&path)?);
        if length == 0 || length > MAX_SYSTEM_ID_LEN {
            return Err(invalid_manifest(
                &path,
                format!(
                    "system ID length {length} is outside the 1..={MAX_SYSTEM_ID_LEN} byte range"
                ),
            ));
        }
        let raw = reader.take(length, &path)?;
        let id = std::str::from_utf8(raw)
            .map(str::to_owned)
            .map_err(|_| invalid_manifest(&path, "system ID is not valid UTF-8"))?;
        if validate_type_id(&id).is_err() {
            return Err(invalid_manifest(
                &path,
                format!("system ID `{id}` is not a valid stable system ID"),
            ));
        }
        if !previous.as_deref().is_none_or(|prior| id.as_str() > prior) {
            return Err(invalid_manifest(
                &path,
                "system IDs must be in lexicographic order and free of duplicates",
            ));
        }
        previous = Some(id.clone());
        ids.push(id);
    }

    if !reader.is_exhausted() {
        return Err(invalid_manifest(
            "/system_manifest",
            format!(
                "trailing bytes after the last system ID (offset {})",
                reader.position
            ),
        ));
    }
    Ok(ids)
}

/// Extracts the system manifest carried by a layout's `metadata/session-data`
/// partition.
///
/// [`SystemManifestErrorCode::Missing`] is returned when the layout carries no
/// such partition — the load-side (`SAVE-005`) distinction between an absent
/// manifest and an empty one.
pub fn system_manifest(layout: &SaveLayout) -> Result<Vec<String>, SystemManifestError> {
    let partition = layout.get(&SavePartitionId::Metadata).ok_or_else(|| {
        SystemManifestError::new(
            SystemManifestErrorCode::Missing,
            "/partitions",
            "layout carries no `metadata/session-data` partition, so the system manifest is missing",
        )
    })?;
    decode_system_manifest(partition.payload.as_bytes())
}

/// A bounds-checked cursor over a system manifest payload.
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

    fn take(&mut self, length: usize, path: &str) -> Result<&'a [u8], SystemManifestError> {
        if self.remaining() < length {
            return Err(invalid_manifest(
                path,
                format!(
                    "system manifest is truncated: need {length} byte(s) at offset {}, only {} remain",
                    self.position,
                    self.remaining()
                ),
            ));
        }
        let slice = &self.bytes[self.position..self.position + length];
        self.position += length;
        Ok(slice)
    }

    fn u8(&mut self, path: &str) -> Result<u8, SystemManifestError> {
        Ok(self.take(1, path)?[0])
    }

    fn u16(&mut self, path: &str) -> Result<u16, SystemManifestError> {
        let bytes = self.take(2, path)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn u32(&mut self, path: &str) -> Result<u32, SystemManifestError> {
        let bytes = self.take(4, path)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }
}

// ---------------------------------------------------------------------------
// Save outcome and error layering
// ---------------------------------------------------------------------------

/// The result of a successful save commit (contract §6.1.2).
///
/// Returning `Ok(SaveOutcome)` itself asserts two facts: the container content
/// was verified (temporary file read back and `SAVE-003` `verify` intact) and
/// the formal file replacement succeeded (`rename` returned `Ok`). No
/// power-loss durability is implied (DC-3 constraints 4/5).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SaveOutcome {
    /// Byte length of the verified container (read back before commit).
    pub bytes_written: u64,
    /// The final `SAVE-003` root digest recorded into the Manifest.
    pub root_hash: String,
}

/// The I/O stage a save failure occurred at (contract §6.1.3).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IoStage {
    /// Exclusive (`create_new`) creation of the temporary file.
    Create,
    /// Writing the container bytes to the temporary file.
    Write,
    /// `sync_all` on the temporary file (DC-3).
    Sync,
    /// Reading the temporary file back for verification.
    Readback,
    /// Replacing the formal archive via `rename`.
    Rename,
}

impl IoStage {
    /// A short diagnostic label (not a frozen stable code).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Write => "write",
            Self::Sync => "sync",
            Self::Readback => "readback",
            Self::Rename => "rename",
        }
    }
}

/// A failure of the normal save path (contract §6.1.4 / §10).
///
/// `SAVE-001`/`SAVE-002`/`SAVE-003` errors propagate unchanged; a read-back
/// digest disagreement carries the full [`IntegrityReport`]; I/O failures are
/// structured by [`IoStage`], path and source.
#[derive(Debug)]
pub enum SaveError {
    /// `SAVE-001` Manifest failure, propagated unchanged.
    Manifest(SaveManifestError),
    /// `SAVE-002` partition/layout failure, propagated unchanged.
    Layout(PartitionError),
    /// `SAVE-003` structural integrity failure, propagated unchanged.
    Integrity(IntegrityError),
    /// The read-back container did not verify (digest disagreement).
    ///
    /// The report is boxed to keep `SaveError` small on the success path
    /// (the workspace's `clippy::result_large_err` gate); it still carries
    /// the full `SAVE-003` report exactly as the contract requires.
    Verify(Box<IntegrityReport>),
    /// System manifest (`SM` v1) failure.
    SystemManifest(SystemManifestError),
    /// A save file I/O failure.
    Io {
        /// The stage at which the failure occurred.
        stage: IoStage,
        /// The path involved (temporary file or formal target).
        path: PathBuf,
        /// The underlying I/O error.
        source: io::Error,
    },
}

impl fmt::Display for SaveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Manifest(error) => write!(formatter, "{error}"),
            Self::Layout(error) => write!(formatter, "{error}"),
            Self::Integrity(error) => write!(formatter, "{error}"),
            Self::Verify(report) => write!(
                formatter,
                "read-back container failed integrity verification (manifest root `{}`, computed root `{}`)",
                report.manifest_root, report.computed_root
            ),
            Self::SystemManifest(error) => write!(formatter, "{error}"),
            Self::Io {
                stage,
                path,
                source,
            } => write!(
                formatter,
                "save I/O failed at stage `{}` for `{}`: {source}",
                stage.as_str(),
                path.display()
            ),
        }
    }
}

impl std::error::Error for SaveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Manifest(error) => Some(error),
            Self::Layout(error) => Some(error),
            Self::Integrity(error) => Some(error),
            Self::SystemManifest(error) => Some(error),
            Self::Io { source, .. } => Some(source),
            Self::Verify(_) => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Temporary files (contract §7)
// ---------------------------------------------------------------------------

/// Process-local monotonic sequence for temporary file names.
static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Derives the temporary file path for `target`: `'.{name}.tmp-{pid}-{seq}'`
/// in the same directory (so `rename` never crosses a filesystem).
fn temp_path_for(target: &Path) -> PathBuf {
    let name = target
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("save"));
    let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temp_name = format!(
        ".{}.tmp-{}-{sequence}",
        name.to_string_lossy(),
        std::process::id()
    );
    match target.parent() {
        Some(dir) => dir.join(temp_name),
        None => PathBuf::from(temp_name),
    }
}

/// Creates a temporary file with hard-exclusive `create_new` semantics: an
/// existing file (a live concurrent writer or an orphan) yields
/// [`io::ErrorKind::AlreadyExists`] and is never truncated.
fn create_temp(path: &Path) -> io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

/// Removes exactly the temporary file this call created, on a best-effort
/// basis (DC-6): never a glob, and a cleanup failure never replaces the
/// primary error. Orphans are `SAVE-007`'s concern.
fn remove_temp(path: &Path) {
    let _ = std::fs::remove_file(path);
}

// ---------------------------------------------------------------------------
// The normal save path (contract §6)
// ---------------------------------------------------------------------------

/// Saves a consistent snapshot to `target` through the frozen commit sequence
/// (contract §6: assemble → digest → validate → temp file → `sync_all` →
/// read-back verify → `rename`).
///
/// The caller supplies the quiescent-point inputs: the `Kernel::snapshot()`
/// bytes (B-1, opaque), the Manifest (with a format-legal placeholder
/// `integrity.root_hash`; the generated root is written into a local copy
/// before serialization), the Mod partitions and the System IDs collected
/// from `Kernel::registered_system_ids`.
///
/// `Ok(SaveOutcome)` is returned only after the formal file replacement has
/// succeeded. Any earlier failure cleans up this call's temporary file and
/// preserves the previous formal archive (DC-3 constraint 3).
pub fn save(
    target: &Path,
    manifest: &SaveManifest,
    world_snapshot: &[u8],
    mod_partitions: &[SavePartition],
    system_ids: &[&str],
) -> Result<SaveOutcome, SaveError> {
    save_impl(
        target,
        manifest,
        world_snapshot,
        mod_partitions,
        system_ids,
        None,
    )
}

fn save_impl(
    target: &Path,
    manifest: &SaveManifest,
    world_snapshot: &[u8],
    mod_partitions: &[SavePartition],
    system_ids: &[&str],
    injected_failure: Option<InjectedFailure>,
) -> Result<SaveOutcome, SaveError> {
    manifest.validate().map_err(SaveError::Manifest)?;
    let system_manifest = encode_system_manifest(system_ids).map_err(SaveError::SystemManifest)?;

    // Step 3: the initial layout already carries the Integrity partition
    // (placeholder payload); `generate` treats its absence as structural.
    let mut partitions = Vec::with_capacity(mod_partitions.len() + 4);
    partitions.push(SavePartition::new(
        SavePartitionId::Manifest,
        manifest.to_bytes(),
    ));
    partitions.push(SavePartition::new(
        SavePartitionId::World,
        world_snapshot.to_vec(),
    ));
    partitions.extend(mod_partitions.iter().cloned());
    partitions.push(SavePartition::new(
        SavePartitionId::Metadata,
        system_manifest,
    ));
    partitions.push(SavePartition::new(SavePartitionId::Integrity, Vec::new()));
    let initial = SaveLayout::new(partitions);
    initial.validate().map_err(SaveError::Layout)?;

    // Step 4: generate the digest, then write it back into a local Manifest
    // copy and rebuild the Integrity partition payload. `generate` never
    // writes back itself, and the placeholder Integrity payload is outside
    // the root digest domain.
    let generated = integrity::generate(&initial).map_err(SaveError::Integrity)?;
    let mut final_manifest = manifest.clone();
    final_manifest.integrity.root_hash = generated.root.clone();
    let mut final_partitions = initial.partitions;
    for partition in &mut final_partitions {
        match partition.id {
            SavePartitionId::Manifest => {
                partition.payload = PartitionPayload::new(final_manifest.to_bytes());
            }
            SavePartitionId::Integrity => {
                partition.payload = PartitionPayload::new(generated.checksums.clone());
            }
            _ => {}
        }
    }

    // Step 5/6: explicit final-layout confirmation, then container encoding
    // (`to_bytes` validates again on its own).
    let final_layout = SaveLayout::new(final_partitions);
    final_layout.validate().map_err(SaveError::Layout)?;
    let bytes = final_layout.to_bytes().map_err(SaveError::Layout)?;

    // Step 7/8: same-directory temporary file, exclusive create, write, sync.
    let temp = temp_path_for(target);
    if injected_failure.is_some_and(|failure| failure.stage == IoStage::Create) {
        return Err(injected_io_error(injected_failure.unwrap(), &temp));
    }
    let mut file = create_temp(&temp).map_err(|source| SaveError::Io {
        stage: IoStage::Create,
        path: temp.clone(),
        source,
    })?;
    if injected_failure.is_some_and(|failure| failure.stage == IoStage::Write) {
        drop(file);
        remove_temp(&temp);
        return Err(injected_io_error(injected_failure.unwrap(), &temp));
    }
    if let Err(source) = file.write_all(&bytes) {
        remove_temp(&temp);
        return Err(SaveError::Io {
            stage: IoStage::Write,
            path: temp,
            source,
        });
    }
    if injected_failure.is_some_and(|failure| failure.stage == IoStage::Sync) {
        drop(file);
        remove_temp(&temp);
        return Err(injected_io_error(injected_failure.unwrap(), &temp));
    }
    if let Err(source) = file.sync_all() {
        remove_temp(&temp);
        return Err(SaveError::Io {
            stage: IoStage::Sync,
            path: temp,
            source,
        });
    }
    drop(file);

    // Step 9: read back, decode and verify. A structural failure propagates
    // its own layer's error; a digest disagreement is `Verify`.
    if injected_failure.is_some_and(|failure| failure.stage == IoStage::Readback) {
        remove_temp(&temp);
        return Err(injected_io_error(injected_failure.unwrap(), &temp));
    }
    let readback = match std::fs::read(&temp) {
        Ok(bytes) => bytes,
        Err(source) => {
            remove_temp(&temp);
            return Err(SaveError::Io {
                stage: IoStage::Readback,
                path: temp,
                source,
            });
        }
    };
    let decoded = match SaveLayout::from_bytes(&readback) {
        Ok(layout) => layout,
        Err(error) => {
            remove_temp(&temp);
            return Err(SaveError::Layout(error));
        }
    };
    let report = match integrity::verify(&decoded) {
        Ok(report) => report,
        Err(error) => {
            remove_temp(&temp);
            return Err(SaveError::Integrity(error));
        }
    };
    if !report.is_intact() {
        remove_temp(&temp);
        return Err(SaveError::Verify(Box::new(report)));
    }

    // Step 10/11: replace the formal archive via rename; only then succeed.
    if injected_failure.is_some_and(|failure| failure.stage == IoStage::Rename) {
        remove_temp(&temp);
        return Err(injected_io_error(injected_failure.unwrap(), target));
    }
    std::fs::rename(&temp, target).map_err(|source| {
        remove_temp(&temp);
        SaveError::Io {
            stage: IoStage::Rename,
            path: target.to_path_buf(),
            source,
        }
    })?;
    Ok(SaveOutcome {
        bytes_written: u64::try_from(readback.len()).expect("usize fits in u64"),
        root_hash: generated.root,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InjectedFailure {
    stage: IoStage,
    kind: io::ErrorKind,
}

fn injected_io_error(failure: InjectedFailure, path: &Path) -> SaveError {
    SaveError::Io {
        stage: failure.stage,
        path: path.to_path_buf(),
        source: io::Error::new(failure.kind, "test-only injected save failure"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::partition::ModNamespace;
    use crate::{GameDefinition, Integrity, SaveFormatVersion, SaveModEntry, SimulationConfig};
    use kernel_core::Tick;

    fn unit_temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "gksave-save004-writer-{tag}-{}-{}",
            std::process::id(),
            TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("create unit-test temp dir");
        dir
    }

    #[test]
    fn temp_path_sits_beside_target_and_matches_frozen_pattern() {
        let target = Path::new("some/dir/world.gksave");
        let temp = temp_path_for(target);
        assert_eq!(temp.parent(), target.parent());
        let name = temp.file_name().expect("temp file name").to_string_lossy();
        assert!(
            name.starts_with(".world.gksave.tmp-"),
            "unexpected temp name `{name}`"
        );
    }

    #[test]
    fn create_temp_is_exclusive_and_preserves_the_first_writer() {
        let dir = unit_temp_dir("exclusive");
        let temp = dir.join(".save.tmp-fixture");

        let mut first = create_temp(&temp).expect("first create");
        first.write_all(b"first").expect("write first");
        first.sync_all().expect("sync first");
        drop(first);

        let collision = create_temp(&temp);
        assert_eq!(
            collision.expect_err("second create must fail").kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(std::fs::read(&temp).expect("read first"), b"first");

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn contract_manifest() -> SaveManifest {
        SaveManifest {
            save_format_version: SaveFormatVersion::new(1, 0),
            save_id: "save004.injected".into(),
            created_at: "2026-10-19T00:00:00Z".into(),
            last_saved_at: "2026-10-19T00:00:00Z".into(),
            platform_version: "1.0.0".into(),
            game_definition: GameDefinition {
                id: "org.example.game".into(),
                version: "1.0.0".into(),
            },
            kernel_api_version: "1.0".into(),
            world_schema_version: "1.0".into(),
            logical_tick: Tick(0),
            mods: vec![SaveModEntry {
                id: "org.example.mod".into(),
                version: "1.0.0".into(),
                schema_version: "1.0".into(),
            }],
            simulation_config: SimulationConfig::new(),
            integrity: Integrity {
                algorithm: "sha256".into(),
                root_hash: "0".repeat(64),
            },
        }
    }

    #[test]
    fn injected_io_failures_at_each_commit_stage_preserve_previous_save() {
        let dir = unit_temp_dir("fault-injection");
        let target = dir.join("world.gksave");
        let manifest = contract_manifest();
        let mod_data = [SavePartition::new(
            SavePartitionId::Mod(ModNamespace::parse("org.example.mod").unwrap()),
            b"persistent-data",
        )];
        let systems = ["org.example.mod.system"];
        save(&target, &manifest, b"previous-world", &mod_data, &systems).unwrap();
        let previous = std::fs::read(&target).unwrap();

        for stage in [
            IoStage::Create,
            IoStage::Write,
            IoStage::Sync,
            IoStage::Readback,
            IoStage::Rename,
        ] {
            let error = save_impl(
                &target,
                &manifest,
                b"replacement-world",
                &mod_data,
                &systems,
                Some(InjectedFailure {
                    stage,
                    kind: io::ErrorKind::Other,
                }),
            )
            .unwrap_err();
            assert!(matches!(error, SaveError::Io { stage: actual, .. } if actual == stage));
            assert_eq!(
                std::fs::read(&target).unwrap(),
                previous,
                "failure at {stage:?} replaced existing save"
            );
            assert!(crate::loader::inspect_save_file(&target).is_ok());
            let entries: Vec<_> = std::fs::read_dir(&dir)
                .unwrap()
                .map(Result::unwrap)
                .collect();
            assert_eq!(
                entries.len(),
                1,
                "failure at {stage:?} left a temporary file"
            );
            assert_eq!(entries[0].path(), target);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn simulated_disk_full_is_reported_at_write_and_preserves_old_save() {
        let dir = unit_temp_dir("disk-full");
        let target = dir.join("active.gksave");
        save(
            &target,
            &contract_manifest(),
            b"last-confirmed-world",
            &[],
            &[],
        )
        .expect("write last confirmed valid save");
        let old = std::fs::read(&target).expect("read previous save");
        let error = save_impl(
            &target,
            &contract_manifest(),
            b"new-world",
            &[],
            &[],
            Some(InjectedFailure {
                stage: IoStage::Write,
                kind: io::ErrorKind::StorageFull,
            }),
        )
        .expect_err("simulated storage exhaustion must fail");
        assert!(matches!(
            error,
            SaveError::Io {
                stage: IoStage::Write,
                source,
                ..
            } if source.kind() == io::ErrorKind::StorageFull
        ));
        let preserved = std::fs::read(&target).expect("read old save");
        assert_eq!(preserved, old);
        assert!(crate::loader::inspect_save(&preserved).is_ok());
        let entries = std::fs::read_dir(&dir)
            .expect("list temp directory")
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(entries, vec![target.file_name().unwrap()]);
        std::fs::remove_file(&target).expect("remove test target");
        std::fs::remove_dir(&dir).expect("remove test directory");
    }
}
