//! `SAVE-006`: deterministic, in-memory migration registration and planning.
//!
//! This module never loads or executes migrator code. Targets and installed
//! Mod IDs are injected by the assembly layer; execution belongs to SAVE-007.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::SaveFormatVersion;
use crate::compat::{MigrationHop, MigrationPath};
use crate::loader::InspectedSave;

const DISCOVERY_LIMIT: usize = 3;

/// The isolated data domain a migration operates on.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MigrationDomain {
    /// World data, owned by the save's Game Definition.
    World { game_definition_id: String },
    /// Persistent data belonging to one Mod.
    Mod { mod_id: String },
}

impl MigrationDomain {
    fn owner_id(&self) -> &str {
        match self {
            Self::World { game_definition_id } => game_definition_id,
            Self::Mod { mod_id } => mod_id,
        }
    }
}

/// Declared read/write data scope. SAVE-006 carries this metadata unchanged.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct MigrationDataScope {
    pub world: bool,
    pub mods: BTreeSet<String>,
}

/// Required verification strategy declaration for a migrator.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MigrationVerification {
    RoundTrip,
    GoldenDigest,
    Custom,
}

/// Complete SAVE specification §8.1 migration declaration.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MigratorDescriptor {
    pub id: String,
    pub domain: MigrationDomain,
    pub from_schema: SaveFormatVersion,
    pub to_schema: SaveFormatVersion,
    pub migrator_version: String,
    pub requires: BTreeSet<String>,
    pub input_scope: MigrationDataScope,
    pub output_scope: MigrationDataScope,
    pub changes_world_data: bool,
    pub has_irreversible_ops: bool,
    pub resource_budget: BTreeMap<String, u64>,
    pub verification: MigrationVerification,
}

/// Field names used in stable invalid-descriptor diagnostics.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DescriptorField {
    Id,
    Owner,
    MigratorVersion,
    FromSchema,
    ToSchema,
}

/// Registration failures with stable machine-readable codes.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "code")]
pub enum MigrationRegistryError {
    #[serde(rename = "save.migration.duplicate_migrator_id")]
    DuplicateMigratorId { id: String },
    #[serde(rename = "save.migration.conflicting_edge")]
    ConflictingMigrationEdge {
        domain: MigrationDomain,
        from: SaveFormatVersion,
        to: SaveFormatVersion,
        existing_id: String,
        new_id: String,
    },
    #[serde(rename = "save.migration.invalid_descriptor")]
    InvalidDescriptor {
        id: Option<String>,
        field: DescriptorField,
        reason: String,
    },
}

impl MigrationRegistryError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::DuplicateMigratorId { .. } => "save.migration.duplicate_migrator_id",
            Self::ConflictingMigrationEdge { .. } => "save.migration.conflicting_edge",
            Self::InvalidDescriptor { .. } => "save.migration.invalid_descriptor",
        }
    }
}

impl fmt::Display for MigrationRegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateMigratorId { id } => write!(f, "duplicate migrator id `{id}`"),
            Self::ConflictingMigrationEdge {
                existing_id,
                new_id,
                ..
            } => write!(
                f,
                "migration edge conflicts between `{existing_id}` and `{new_id}`"
            ),
            Self::InvalidDescriptor { id, field, reason } => {
                write!(f, "invalid migrator {id:?} field {field:?}: {reason}")
            }
        }
    }
}

impl std::error::Error for MigrationRegistryError {}

/// A selected, ordered chain. The hops use SAVE-005's frozen summary type.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MigrationChain {
    pub hops: Vec<MigrationStep>,
}

/// Serializable SAVE-006 hop snapshot; converted to SAVE-005's frozen type at the boundary.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MigrationStep {
    pub from_version: SaveFormatVersion,
    pub to_version: SaveFormatVersion,
    pub migrator_id: String,
}

/// Missing dependency evidence, ordered by migrator ID then dependency ID.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Ord, PartialOrd, Serialize)]
pub struct UnsatisfiedDependency {
    pub migrator_id: String,
    pub dependency_id: String,
}

/// Path lookup failures, each with a stable wire code.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "code")]
pub enum MigrationLookupError {
    #[serde(rename = "save.migration.no_path")]
    NoMigrationPath {
        domain: MigrationDomain,
        from: SaveFormatVersion,
        to: SaveFormatVersion,
    },
    #[serde(rename = "save.migration.ambiguous_path")]
    AmbiguousMigrationPath {
        domain: MigrationDomain,
        from: SaveFormatVersion,
        to: SaveFormatVersion,
        discovered_chains: Vec<MigrationChain>,
        truncated: bool,
    },
    #[serde(rename = "save.migration.incompatible_step")]
    IncompatibleStep {
        domain: MigrationDomain,
        from: SaveFormatVersion,
        to: SaveFormatVersion,
        unsatisfied: Vec<UnsatisfiedDependency>,
    },
}

impl MigrationLookupError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::NoMigrationPath { .. } => "save.migration.no_path",
            Self::AmbiguousMigrationPath { .. } => "save.migration.ambiguous_path",
            Self::IncompatibleStep { .. } => "save.migration.incompatible_step",
        }
    }
}

impl fmt::Display for MigrationLookupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoMigrationPath { domain, from, to } => {
                write!(f, "no migration path in {domain:?} from {from} to {to}")
            }
            Self::AmbiguousMigrationPath {
                domain, from, to, ..
            } => {
                write!(
                    f,
                    "ambiguous migration path in {domain:?} from {from} to {to}"
                )
            }
            Self::IncompatibleStep {
                domain, from, to, ..
            } => write!(
                f,
                "migration path in {domain:?} from {from} to {to} requires unavailable Mods"
            ),
        }
    }
}

impl std::error::Error for MigrationLookupError {}

/// Immutable registry indexed by domain and starting schema.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct MigrationRegistry {
    descriptors: BTreeMap<String, MigratorDescriptor>,
}

impl MigrationRegistry {
    /// Validates every declaration and constructs an order-independent registry.
    pub fn build(mut descriptors: Vec<MigratorDescriptor>) -> Result<Self, MigrationRegistryError> {
        descriptors.sort_by(|a, b| {
            (&a.id, &a.domain, a.from_schema, a.to_schema).cmp(&(
                &b.id,
                &b.domain,
                b.from_schema,
                b.to_schema,
            ))
        });
        let mut by_id: BTreeMap<String, MigratorDescriptor> = BTreeMap::new();
        let mut by_edge: BTreeMap<(MigrationDomain, SaveFormatVersion, SaveFormatVersion), String> =
            BTreeMap::new();
        for descriptor in descriptors {
            let invalid = |field, reason: String| MigrationRegistryError::InvalidDescriptor {
                id: Some(descriptor.id.clone()),
                field,
                reason,
            };
            if descriptor.id.trim().is_empty() {
                return Err(invalid(DescriptorField::Id, "must not be empty".into()));
            }
            if descriptor.domain.owner_id().trim().is_empty() {
                return Err(invalid(DescriptorField::Owner, "must not be empty".into()));
            }
            if semver::Version::parse(&descriptor.migrator_version).is_err() {
                return Err(invalid(
                    DescriptorField::MigratorVersion,
                    "must be valid SemVer".into(),
                ));
            }
            if descriptor.from_schema == descriptor.to_schema {
                return Err(invalid(
                    DescriptorField::ToSchema,
                    "identity migration edges are forbidden".into(),
                ));
            }
            if by_id.contains_key(&descriptor.id) {
                return Err(MigrationRegistryError::DuplicateMigratorId { id: descriptor.id });
            }
            let edge = (
                descriptor.domain.clone(),
                descriptor.from_schema,
                descriptor.to_schema,
            );
            if let Some(existing_id) = by_edge.get(&edge) {
                return Err(MigrationRegistryError::ConflictingMigrationEdge {
                    domain: edge.0,
                    from: edge.1,
                    to: edge.2,
                    existing_id: existing_id.clone(),
                    new_id: descriptor.id,
                });
            }
            by_edge.insert(edge, descriptor.id.clone());
            by_id.insert(descriptor.id.clone(), descriptor);
        }
        Ok(Self { descriptors: by_id })
    }

    /// Registered declarations in stable migrator-ID order.
    pub fn descriptors(&self) -> impl Iterator<Item = &MigratorDescriptor> {
        self.descriptors.values()
    }

    /// Returns a declaration by its stable identifier.
    pub fn descriptor(&self, id: &str) -> Option<&MigratorDescriptor> {
        self.descriptors.get(id)
    }

    /// Finds the unique usable simple path, rejecting missing, incompatible or ambiguous graphs.
    pub fn find_path(
        &self,
        domain: &MigrationDomain,
        from: SaveFormatVersion,
        to: SaveFormatVersion,
        installed_mod_ids: &BTreeSet<String>,
    ) -> Result<MigrationChain, MigrationLookupError> {
        if from == to {
            return Ok(MigrationChain { hops: Vec::new() });
        }
        let mut paths = Vec::<Vec<&MigratorDescriptor>>::new();
        let mut current = Vec::new();
        let mut visited = BTreeSet::from([from]);
        let mut unsatisfied = BTreeSet::new();
        self.search(
            domain,
            from,
            to,
            installed_mod_ids,
            &mut visited,
            &mut current,
            &mut paths,
            &mut unsatisfied,
        );
        let mut chains: Vec<MigrationChain> = paths
            .into_iter()
            .map(|path| MigrationChain {
                hops: path
                    .into_iter()
                    .map(|descriptor| MigrationStep {
                        from_version: descriptor.from_schema,
                        to_version: descriptor.to_schema,
                        migrator_id: descriptor.id.clone(),
                    })
                    .collect(),
            })
            .collect();
        chains.sort_by(|a, b| hop_ids(a).cmp(&hop_ids(b)));
        if chains.len() >= 2 {
            return Err(MigrationLookupError::AmbiguousMigrationPath {
                domain: domain.clone(),
                from,
                to,
                truncated: chains.len() == DISCOVERY_LIMIT,
                discovered_chains: chains,
            });
        }
        if let Some(chain) = chains.pop() {
            return Ok(chain);
        }
        if !unsatisfied.is_empty() {
            return Err(MigrationLookupError::IncompatibleStep {
                domain: domain.clone(),
                from,
                to,
                unsatisfied: unsatisfied.into_iter().collect(),
            });
        }
        Err(MigrationLookupError::NoMigrationPath {
            domain: domain.clone(),
            from,
            to,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn search<'a>(
        &'a self,
        domain: &MigrationDomain,
        node: SaveFormatVersion,
        target: SaveFormatVersion,
        installed: &BTreeSet<String>,
        visited: &mut BTreeSet<SaveFormatVersion>,
        current: &mut Vec<&'a MigratorDescriptor>,
        paths: &mut Vec<Vec<&'a MigratorDescriptor>>,
        unsatisfied: &mut BTreeSet<UnsatisfiedDependency>,
    ) {
        if paths.len() >= DISCOVERY_LIMIT {
            return;
        }
        let mut outgoing: Vec<_> = self
            .descriptors
            .values()
            .filter(|descriptor| &descriptor.domain == domain && descriptor.from_schema == node)
            .collect();
        outgoing.sort_by_key(|descriptor| (descriptor.to_schema, descriptor.id.as_str()));
        for descriptor in outgoing {
            if paths.len() >= DISCOVERY_LIMIT {
                return;
            }
            if visited.contains(&descriptor.to_schema) {
                continue;
            }
            let missing: Vec<_> = descriptor.requires.difference(installed).cloned().collect();
            if !missing.is_empty() {
                if self.can_reach(domain, descriptor.to_schema, target) {
                    for dependency_id in missing {
                        unsatisfied.insert(UnsatisfiedDependency {
                            migrator_id: descriptor.id.clone(),
                            dependency_id,
                        });
                    }
                }
                continue;
            }
            current.push(descriptor);
            if descriptor.to_schema == target {
                paths.push(current.clone());
            } else {
                visited.insert(descriptor.to_schema);
                self.search(
                    domain,
                    descriptor.to_schema,
                    target,
                    installed,
                    visited,
                    current,
                    paths,
                    unsatisfied,
                );
                visited.remove(&descriptor.to_schema);
            }
            current.pop();
        }
    }

    fn can_reach(
        &self,
        domain: &MigrationDomain,
        from: SaveFormatVersion,
        to: SaveFormatVersion,
    ) -> bool {
        let mut stack = vec![from];
        let mut seen = BTreeSet::new();
        while let Some(node) = stack.pop() {
            if node == to {
                return true;
            }
            if !seen.insert(node) {
                continue;
            }
            stack.extend(
                self.descriptors
                    .values()
                    .filter(|edge| &edge.domain == domain && edge.from_schema == node)
                    .map(|edge| edge.to_schema),
            );
        }
        false
    }
}

fn hop_ids(chain: &MigrationChain) -> Vec<&str> {
    chain
        .hops
        .iter()
        .map(|hop| hop.migrator_id.as_str())
        .collect()
}

/// Converts SAVE-006's result to the frozen SAVE-005 injection type.
pub fn to_compatibility_path(
    result: Result<&MigrationChain, &MigrationLookupError>,
) -> MigrationPath {
    match result {
        Ok(chain) => MigrationPath::Available(
            chain
                .hops
                .iter()
                .map(|hop| MigrationHop {
                    from_version: hop.from_version,
                    to_version: hop.to_version,
                    migrator_id: hop.migrator_id.clone(),
                })
                .collect(),
        ),
        Err(_) => MigrationPath::Unavailable,
    }
}

/// Schema targets supplied by the assembly layer; no version is inferred.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MigrationTargets {
    pub world_schema_version: SaveFormatVersion,
    pub mod_schema_versions: BTreeMap<String, SaveFormatVersion>,
}

/// One execution-plan step, containing the exact descriptor snapshot.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PlannedMigration {
    pub descriptor: MigratorDescriptor,
}

/// Immutable, deterministic input to SAVE-007.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MigrationPlan {
    pub save_id: String,
    pub source_root_hash: String,
    pub steps: Vec<PlannedMigration>,
}

/// Error identifying which domain prevented complete plan construction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MigrationPlanError {
    MissingInstalledModTarget {
        mod_id: String,
    },
    Lookup {
        domain: MigrationDomain,
        source: MigrationLookupError,
    },
}

impl fmt::Display for MigrationPlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingInstalledModTarget { mod_id } => {
                write!(
                    f,
                    "installed Mod `{mod_id}` has no injected target Schema version"
                )
            }
            Self::Lookup { domain, source } => {
                write!(f, "cannot plan migration for {domain:?}: {source}")
            }
        }
    }
}

impl std::error::Error for MigrationPlanError {}

/// Builds a complete plan from an inspected save and explicitly injected targets.
pub fn build_migration_plan(
    save: &InspectedSave,
    registry: &MigrationRegistry,
    targets: &MigrationTargets,
    installed_mod_ids: &BTreeSet<String>,
) -> Result<MigrationPlan, MigrationPlanError> {
    let manifest = save.manifest();
    let mut grouped = BTreeMap::<MigrationDomain, Vec<PlannedMigration>>::new();
    let world_target = targets.world_schema_version;
    let world_from = SaveFormatVersion::parse(&manifest.world_schema_version)
        .expect("InspectedSave contains validated manifest");
    if world_from != world_target {
        let domain = MigrationDomain::World {
            game_definition_id: manifest.game_definition.id.clone(),
        };
        let chain = registry
            .find_path(&domain, world_from, world_target, installed_mod_ids)
            .map_err(|source| MigrationPlanError::Lookup {
                domain: domain.clone(),
                source,
            })?;
        grouped
            .entry(domain)
            .or_default()
            .extend(chain.hops.iter().map(|hop| {
                let descriptor = registry
                    .descriptors
                    .get(&hop.migrator_id)
                    .expect("chain hops originate from this registry")
                    .clone();
                PlannedMigration { descriptor }
            }));
    }
    for saved_mod in &manifest.mods {
        if !installed_mod_ids.contains(&saved_mod.id) {
            continue;
        }
        let Some(&target) = targets.mod_schema_versions.get(&saved_mod.id) else {
            return Err(MigrationPlanError::MissingInstalledModTarget {
                mod_id: saved_mod.id.clone(),
            });
        };
        let from = SaveFormatVersion::parse(&saved_mod.schema_version)
            .expect("InspectedSave contains validated manifest");
        if from == target {
            continue;
        }
        let domain = MigrationDomain::Mod {
            mod_id: saved_mod.id.clone(),
        };
        let chain = registry
            .find_path(&domain, from, target, installed_mod_ids)
            .map_err(|source| MigrationPlanError::Lookup {
                domain: domain.clone(),
                source,
            })?;
        grouped
            .entry(domain)
            .or_default()
            .extend(chain.hops.iter().map(|hop| {
                let descriptor = registry
                    .descriptors
                    .get(&hop.migrator_id)
                    .expect("chain hops originate from this registry")
                    .clone();
                PlannedMigration { descriptor }
            }));
    }
    let steps = grouped.into_values().flatten().collect();
    Ok(MigrationPlan {
        save_id: manifest.save_id.clone(),
        source_root_hash: manifest.integrity.root_hash.clone(),
        steps,
    })
}
