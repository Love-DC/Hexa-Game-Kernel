//! `SAVE-005`: save inspection — the load-side parsing and integrity gate.
//!
//! This module is the load-side half of the fifth increment of the P4 Save
//! System (`SAVE-005`). The frozen implementation contract is
//! `planning/34-P4-SAVE-005-存档加载与兼容性判定实施契约草案.md`; where this
//! module states a rule, that document is authoritative.
//!
//! [`inspect_save`] parses a save container, runs the `SAVE-003` integrity
//! verification, decodes the Manifest and the system manifest, and — only when
//! every check passes — yields an [`InspectedSave`] ("construction is
//! evidence"). Compatibility judgement lives in [`compat`](crate::compat),
//! which consumes [`InspectedSave`]; this module never judges compatibility
//! and depends on nothing from `compat`.
//!
//! ## Scope boundary (frozen for this increment)
//!
//! * **Only `fs::read`.** The single I/O operation of this module — and the
//!   crate's only *read* entry point — is [`inspect_save_file`]'s
//!   `std::fs::read`. No writes, no network, no clock, no process access
//!   (DC-10; enforced by the crate-level discipline test, whose whitelist
//!   gains exactly this file).
//! * **Single corruption exit.** A digest disagreement is
//!   [`LoadError::Corrupted`]; no Manifest or payload of a corrupted save is
//!   ever exposed (`GK-P4-SAVE-005-DESIGN-DECISION-01` decision 3). The
//!   compatibility assessor never produces a `Corrupted` verdict.
//! * **No half-initialised world.** This module never touches `kernel-core`
//!   state; it only inspects bytes and exposes them read-only.
//! * **No migration, no restore.** The migration path is an *input* of the
//!   assessor (`SAVE-006` owns chain lookup); restoring a world is a later
//!   stage's responsibility.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use crate::integrity::{self, IntegrityError, IntegrityReport};
use crate::partition::{ModNamespace, PartitionError, SaveLayout, SavePartitionId};
use crate::writer::{SystemManifestError, system_manifest};
use crate::{SaveManifest, SaveManifestError};

// ---------------------------------------------------------------------------
// Load error layering
// ---------------------------------------------------------------------------

/// The I/O stage a load failure occurred at (frozen: exactly one).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoadIoStage {
    /// Reading the save file's bytes.
    Read,
}

impl LoadIoStage {
    /// A short diagnostic label (not a frozen stable code).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
        }
    }
}

/// A failure of the load-side inspection path (contract §2.1/§3).
///
/// `SAVE-001`/`SAVE-002`/`SAVE-003` errors and the `SAVE-004` system-manifest
/// error propagate unchanged; a digest disagreement carries the full
/// [`IntegrityReport`]; I/O failures are structured by [`LoadIoStage`], path
/// and source. Error style follows `SAVE-001`~`SAVE-004`: hand-written
/// `Display`/`Error`, stable codes carried by the propagated inner errors.
#[derive(Debug)]
pub enum LoadError {
    /// A save file I/O failure.
    Io {
        /// The stage at which the failure occurred.
        stage: LoadIoStage,
        /// The path involved.
        path: PathBuf,
        /// The underlying I/O error.
        source: io::Error,
    },
    /// `SAVE-002` container/layout failure, propagated unchanged.
    Layout(PartitionError),
    /// `SAVE-001` Manifest failure, propagated unchanged.
    ///
    /// Structurally unreachable on the public path (`OWNER-DECISION-03`):
    /// [`inspect_save`] verifies integrity first, and verification already
    /// parses the Manifest payload, so a Manifest that cannot parse fails
    /// earlier as [`LoadError::Integrity`]. The variant is retained to keep
    /// the propagation chain complete; no bypass constructor is provided.
    Manifest(SaveManifestError),
    /// `SAVE-003` structural integrity failure, propagated unchanged.
    Integrity(IntegrityError),
    /// The container did not verify (digest disagreement); nothing further is
    /// decoded and no payload is exposed.
    ///
    /// The report is boxed to keep `LoadError` small on the success path (the
    /// workspace's `clippy::result_large_err` gate), mirroring
    /// [`SaveError::Verify`](crate::writer::SaveError::Verify).
    Corrupted(Box<IntegrityReport>),
    /// System manifest (`SM` v1) load-side failure (`Missing` / `Invalid`).
    SystemManifest(SystemManifestError),
}

impl fmt::Display for LoadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io {
                stage,
                path,
                source,
            } => write!(
                formatter,
                "load I/O failed at stage `{}` for `{}`: {source}",
                stage.as_str(),
                path.display()
            ),
            Self::Layout(error) => write!(formatter, "{error}"),
            Self::Manifest(error) => write!(formatter, "{error}"),
            Self::Integrity(error) => write!(formatter, "{error}"),
            Self::Corrupted(report) => write!(
                formatter,
                "save container failed integrity verification (manifest root `{}`, computed root `{}`)",
                report.manifest_root, report.computed_root
            ),
            Self::SystemManifest(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for LoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Layout(error) => Some(error),
            Self::Manifest(error) => Some(error),
            Self::Integrity(error) => Some(error),
            Self::SystemManifest(error) => Some(error),
            Self::Corrupted(_) => None,
        }
    }
}

// ---------------------------------------------------------------------------
// InspectedSave
// ---------------------------------------------------------------------------

/// A save that passed structure, integrity, Manifest and system-manifest
/// inspection (contract §2.1).
///
/// Construction is evidence: an `InspectedSave` exists only when every
/// load-side check passed, so its accessors are infallible. The carried bytes
/// are exposed read-only; nothing here writes to or extends the save.
#[derive(Debug)]
pub struct InspectedSave {
    layout: SaveLayout,
    manifest: SaveManifest,
    system_ids: Vec<String>,
    integrity_report: IntegrityReport,
}

impl InspectedSave {
    /// The parsed save Manifest.
    pub fn manifest(&self) -> &SaveManifest {
        &self.manifest
    }

    /// The system IDs recorded by the system manifest, in canonical order.
    pub fn system_ids(&self) -> &[String] {
        &self.system_ids
    }

    /// The integrity report this inspection was built from (always intact).
    pub fn integrity_report(&self) -> &IntegrityReport {
        &self.integrity_report
    }

    /// The verified partition layout, exposed read-only to save-owned
    /// validators and diagnostic adapters.
    pub(crate) fn layout(&self) -> &SaveLayout {
        &self.layout
    }

    /// The raw world snapshot bytes (opaque; `SAVE-002` `B-1`).
    ///
    /// Integrity verification covers the partitions actually present, so a
    /// save is not required to carry a world partition; an absent one yields
    /// an empty slice.
    pub fn world_payload(&self) -> &[u8] {
        self.layout
            .get(&SavePartitionId::World)
            .map_or(&[], |partition| partition.payload.as_bytes())
    }

    /// The persistent payload of one Mod, looked up by its stable Mod ID
    /// (`OWNER-APPROVAL-01` `B-6`: read-only; does not extend the save with
    /// write or restore duties).
    ///
    /// `None` when the layout carries no partition for `mod_id`, including the
    /// case where `mod_id` is not a valid Mod namespace.
    pub fn mod_payload(&self, mod_id: &str) -> Option<&[u8]> {
        let namespace = ModNamespace::parse(mod_id).ok()?;
        self.layout
            .get(&SavePartitionId::Mod(namespace))
            .map(|partition| partition.payload.as_bytes())
    }
}

// ---------------------------------------------------------------------------
// Inspection entry points
// ---------------------------------------------------------------------------

/// Inspects save container bytes: parses the layout, verifies integrity,
/// decodes the Manifest and the system manifest.
///
/// The check order is frozen (contract §3): layout → integrity → Manifest →
/// system manifest. A structural failure propagates its own layer's error
/// unchanged; a digest disagreement stops at [`LoadError::Corrupted`] and
/// nothing further is decoded.
pub fn inspect_save(bytes: &[u8]) -> Result<InspectedSave, LoadError> {
    let layout = SaveLayout::from_bytes(bytes).map_err(LoadError::Layout)?;
    let report = integrity::verify(&layout).map_err(LoadError::Integrity)?;
    if !report.is_intact() {
        return Err(LoadError::Corrupted(Box::new(report)));
    }
    // `verify` requires the Manifest partition (it is the comparison
    // authority), so an intact report implies the partition is present.
    let manifest_partition = layout
        .get(&SavePartitionId::Manifest)
        .expect("an intact layout carries a Manifest partition");
    // Unreachable on this path: verification already parsed these exact bytes
    // (`OWNER-DECISION-03`). The propagation is kept for chain completeness.
    let manifest = SaveManifest::from_bytes(manifest_partition.payload.as_bytes())
        .map_err(LoadError::Manifest)?;
    let system_ids = system_manifest(&layout).map_err(LoadError::SystemManifest)?;
    Ok(InspectedSave {
        layout,
        manifest,
        system_ids,
        integrity_report: report,
    })
}

/// Reads a save file and inspects it.
///
/// The crate's only file-reading entry point (DC-10); its single I/O
/// operation is the `std::fs::read` below.
pub fn inspect_save_file(path: &Path) -> Result<InspectedSave, LoadError> {
    let bytes = std::fs::read(path).map_err(|source| LoadError::Io {
        stage: LoadIoStage::Read,
        path: path.to_path_buf(),
        source,
    })?;
    inspect_save(&bytes)
}
