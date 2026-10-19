//! `SAVE-010`: explicit cross-Mod reference inventory and fail-closed validation.
//!
//! Mod payloads remain opaque to save-manager. A trusted platform adapter
//! extracts stable references and exported targets from each owner's bytes;
//! this module checks ownership and existence without rewriting references.

use std::collections::{BTreeMap, BTreeSet};

use crate::SaveManifest;
use crate::loader::InspectedSave;
use crate::partition::{ModNamespace, SaveLayout, SavePartitionId};
use crate::transaction::{MigrationCandidateValidator, ValidationFailure};

/// A stable target identity that includes its owning Provider.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ReferenceTarget {
    pub provider_mod: String,
    pub target_type: String,
    pub stable_id: String,
}

/// One reference originating in the Mod whose extractor returned it.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct StableReference {
    pub source_id: String,
    pub provider_mod: String,
    pub target_type: String,
    pub stable_id: String,
}

/// Complete explicit reference inventory for one Mod's persisted bytes.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ModReferenceSnapshot {
    pub exports: Vec<ReferenceTarget>,
    pub references: Vec<StableReference>,
}

/// Adapter contract for decoding only the owning Mod's payload.
pub trait ModReferenceExtractor {
    fn extract(
        &self,
        owner_mod: &str,
        payload: &[u8],
    ) -> Result<ModReferenceSnapshot, ReferenceExtractionFailure>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReferenceExtractionFailure {
    pub code: String,
    pub message: String,
}

/// Registry of trusted per-Mod extractors, supplied by the platform assembly.
#[derive(Default)]
pub struct ReferenceExtractorRegistry {
    extractors: BTreeMap<String, Box<dyn ModReferenceExtractor>>,
}

impl ReferenceExtractorRegistry {
    pub fn register(
        &mut self,
        mod_id: impl Into<String>,
        extractor: Box<dyn ModReferenceExtractor>,
    ) -> Result<(), ReferenceValidationError> {
        let mod_id = mod_id.into();
        validate_identifier(&mod_id, "mod_id")?;
        if self.extractors.contains_key(&mod_id) {
            return Err(ReferenceValidationError::new(
                "save.reference.duplicate_extractor",
                format!("extractor already registered for {mod_id}"),
            ));
        }
        self.extractors.insert(mod_id, extractor);
        Ok(())
    }

    fn get(&self, mod_id: &str) -> Option<&dyn ModReferenceExtractor> {
        self.extractors.get(mod_id).map(Box::as_ref)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReferenceValidationReport {
    pub checked_mods: usize,
    pub checked_references: usize,
    pub exported_targets: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReferenceValidationError {
    pub code: &'static str,
    pub message: String,
}

impl ReferenceValidationError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// Validates every active Mod's declared references against installed saved
/// Providers and their explicit target inventories. Orphaned Mod partitions
/// outside the active Manifest remain opaque and untouched.
pub fn validate_cross_mod_references(
    manifest: &SaveManifest,
    layout: &SaveLayout,
    extractors: &ReferenceExtractorRegistry,
) -> Result<ReferenceValidationReport, ReferenceValidationError> {
    let installed: BTreeSet<_> = manifest.mods.iter().map(|item| item.id.as_str()).collect();
    let mut inventories = BTreeMap::new();
    let mut targets = BTreeSet::new();
    let mut reference_count = 0;

    let mut owners: Vec<_> = manifest.mods.iter().collect();
    owners.sort_by(|left, right| left.id.cmp(&right.id));
    for owner in owners {
        let namespace = ModNamespace::parse(&owner.id).map_err(|error| {
            ReferenceValidationError::new("save.reference.owner_invalid", error.to_string())
        })?;
        let partition_id = SavePartitionId::Mod(namespace);
        let partition = layout.get(&partition_id).ok_or_else(|| {
            ReferenceValidationError::new(
                "save.reference.mod_data_missing",
                format!("active Mod {} has no persisted data partition", owner.id),
            )
        })?;
        let extractor = extractors.get(&owner.id).ok_or_else(|| {
            ReferenceValidationError::new(
                "save.reference.extractor_missing",
                format!("no reference extractor is registered for {}", owner.id),
            )
        })?;
        let mut snapshot = extractor
            .extract(&owner.id, partition.payload.as_bytes())
            .map_err(|failure| {
                ReferenceValidationError::new(
                    "save.reference.extraction_failed",
                    format!("{}: {}", failure.code, failure.message),
                )
            })?;
        snapshot.exports.sort();
        snapshot.references.sort();
        for target in &snapshot.exports {
            validate_target(target)?;
            if target.provider_mod != owner.id {
                return Err(ReferenceValidationError::new(
                    "save.reference.ownership_mismatch",
                    format!(
                        "{} exported a target owned by {}",
                        owner.id, target.provider_mod
                    ),
                ));
            }
            if !targets.insert(target.clone()) {
                return Err(ReferenceValidationError::new(
                    "save.reference.duplicate_target",
                    format!(
                        "duplicate target {}:{}:{}",
                        target.provider_mod, target.target_type, target.stable_id
                    ),
                ));
            }
        }
        for reference in &snapshot.references {
            validate_reference(reference)?;
            reference_count += 1;
        }
        inventories.insert(owner.id.as_str(), snapshot);
    }

    for snapshot in inventories.values() {
        for reference in &snapshot.references {
            if !installed.contains(reference.provider_mod.as_str()) {
                return Err(ReferenceValidationError::new(
                    "save.reference.provider_missing",
                    format!(
                        "reference {} targets missing Provider {}",
                        reference.source_id, reference.provider_mod
                    ),
                ));
            }
            let target = ReferenceTarget {
                provider_mod: reference.provider_mod.clone(),
                target_type: reference.target_type.clone(),
                stable_id: reference.stable_id.clone(),
            };
            if !targets.contains(&target) {
                return Err(ReferenceValidationError::new(
                    "save.reference.target_missing",
                    format!(
                        "reference {} points to missing target {}:{}:{}",
                        reference.source_id,
                        target.provider_mod,
                        target.target_type,
                        target.stable_id
                    ),
                ));
            }
        }
    }

    Ok(ReferenceValidationReport {
        checked_mods: inventories.len(),
        checked_references: reference_count,
        exported_targets: targets.len(),
    })
}

/// Required whole-candidate validator used by SAVE-007. It composes explicit
/// world invariant validation with cross-Mod reference extraction; there is no
/// permissive default implementation.
pub struct MigrationValidationPipeline {
    world_validator: Box<dyn WorldInvariantValidator>,
    reference_extractors: ReferenceExtractorRegistry,
}

pub trait WorldInvariantValidator {
    fn validate_world(
        &mut self,
        manifest: &SaveManifest,
        world: &[u8],
    ) -> Result<(), ValidationFailure>;
}

impl MigrationValidationPipeline {
    pub fn new(
        world_validator: Box<dyn WorldInvariantValidator>,
        reference_extractors: ReferenceExtractorRegistry,
    ) -> Self {
        Self {
            world_validator,
            reference_extractors,
        }
    }
}

impl MigrationCandidateValidator for MigrationValidationPipeline {
    fn validate(
        &mut self,
        manifest: &SaveManifest,
        layout: &SaveLayout,
    ) -> Result<(), ValidationFailure> {
        let world = layout
            .get(&SavePartitionId::World)
            .ok_or_else(|| ValidationFailure {
                code: "save.reference.world_missing".into(),
                message: "candidate has no World partition".into(),
            })?;
        self.world_validator
            .validate_world(manifest, world.payload.as_bytes())?;
        validate_cross_mod_references(manifest, layout, &self.reference_extractors)
            .map(|_| ())
            .map_err(|error| ValidationFailure {
                code: error.code.into(),
                message: error.message,
            })
    }
}

/// Produces a report from an already inspected archive and registered trusted
/// extractors. Integrity inspection remains the caller's first gate.
pub fn inspect_references(
    inspected: &InspectedSave,
    extractors: &ReferenceExtractorRegistry,
) -> Result<ReferenceValidationReport, ReferenceValidationError> {
    validate_cross_mod_references(inspected.manifest(), inspected.layout(), extractors)
}

fn validate_target(target: &ReferenceTarget) -> Result<(), ReferenceValidationError> {
    validate_identifier(&target.provider_mod, "provider_mod")?;
    validate_identifier(&target.target_type, "target_type")?;
    validate_identifier(&target.stable_id, "stable_id")
}

fn validate_reference(reference: &StableReference) -> Result<(), ReferenceValidationError> {
    validate_identifier(&reference.source_id, "source_id")?;
    validate_identifier(&reference.provider_mod, "provider_mod")?;
    validate_identifier(&reference.target_type, "target_type")?;
    validate_identifier(&reference.stable_id, "stable_id")
}

fn validate_identifier(value: &str, field: &'static str) -> Result<(), ReferenceValidationError> {
    if value.is_empty() || value.len() > 128 || value.bytes().any(|byte| byte.is_ascii_control()) {
        return Err(ReferenceValidationError::new(
            "save.reference.identifier_invalid",
            format!("{field} must be 1..=128 bytes without control characters"),
        ));
    }
    Ok(())
}
