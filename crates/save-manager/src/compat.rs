//! `SAVE-005`: compatibility assessment — a pure judgement over an inspected
//! save and a caller-injected environment.
//!
//! This module is the judgement half of the fifth increment of the P4 Save
//! System (`SAVE-005`). The frozen implementation contract is
//! `planning/34-P4-SAVE-005-存档加载与兼容性判定实施契约草案.md`; where this
//! module states a rule, that document is authoritative.
//!
//! [`assess_compatibility`] implements the nine-step priority pipeline of the
//! *Save & Compatibility Specification v1.0* `§5.3` exactly as the frozen
//! contract expands it (contract §4). It is a **pure function**: no I/O, no
//! side effects, no environment reads — every version, Mod descriptor,
//! migration query result and policy is injected by the caller
//! (`planning/33` §3.5), and repeated evaluation of the same inputs yields an
//! identical report (spec `§5.2`).
//!
//! ## Scope boundary (frozen for this increment)
//!
//! * **Zero I/O.** This module never touches the filesystem, network, clock
//!   or process state (DC-10; enforced by the crate-level discipline test).
//! * **No `Corrupted` verdict.** Structural/integrity corruption is rejected
//!   earlier by [`inspect_save`](crate::loader::inspect_save) as
//!   [`LoadError::Corrupted`](crate::loader::LoadError::Corrupted)
//!   (`GK-P4-SAVE-005-DESIGN-DECISION-01` decision 3). The
//!   [`CompatibilityVerdict::Corrupted`] value is retained for contract
//!   completeness but is never produced here.
//! * **No migration execution or chain lookup.** The migration *query result*
//!   is injected as [`MigrationPath`] (`GK-P4-SAVE-005-DESIGN-DECISION-01`
//!   decision 2); executing or finding migrations is `SAVE-006`/`SAVE-007`.
//! * **Shallow step 8.** Ownership/reference conclusions of `SAVE-002` are
//!   reused as-is; cross-Mod reference analysis is `SAVE-010`
//!   (`GK-P4-SAVE-005-DESIGN-DECISION-01` decision 4).
//! * **Zero-trigger reserved codes.** [`CompatReasonCode::ModVersionIncompatible`]
//!   and the [`PolicyFlag::Degraded`] / [`PolicyFlag::Discouraged`] flags are
//!   frozen values that this stage never produces
//!   (`GK-P4-SAVE-005-OWNER-APPROVAL-01` `B-1`; `DESIGN-DECISION-05`
//!   amendment b).

use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::loader::InspectedSave;
use crate::{SUPPORTED_SAVE_FORMAT_VERSION, SaveFormatVersion};

// ---------------------------------------------------------------------------
// Judgement inputs (all caller-injected; the assessor reads no environment)
// ---------------------------------------------------------------------------

/// The environment a compatibility judgement is made against.
///
/// Every field is injected by the caller (the assembly layer); the assessor
/// never reads the process environment, files or crate metadata
/// (`planning/33` §3.5).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompatibilityEnv {
    /// Current platform version (SemVer). Injected by the assembly layer
    /// only; a pseudo-source such as a crate version is forbidden.
    pub platform_version: String,
    /// Current Kernel API version; matched as a `MAJOR.MINOR` numeric pair.
    pub kernel_api_version: SaveFormatVersion,
    /// The currently enabled Mod set, converted by the assembly layer.
    pub installed_mods: Vec<InstalledModInfo>,
    /// The migration path query result (`SAVE-006` owns the lookup).
    pub migration: MigrationPath,
    /// The active loading policy.
    pub policy: LoadingPolicy,
}

/// One installed Mod as the judgement needs to see it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstalledModInfo {
    /// Stable Mod identifier.
    pub id: String,
    /// Mod release version (SemVer). **Not a judgement predicate**
    /// (`OWNER-APPROVAL-01` `B-1`); carried for diagnostics and for `VersionReq`
    /// evaluation of other Mods' dependency requirements.
    pub version: String,
    /// The Mod's persistent data Schema version: equal to the save's record
    /// passes, different consults migration.
    pub schema_version: SaveFormatVersion,
    /// The Mod's declared Kernel API requirement (`MAJOR.MINOR`); matched
    /// against the current Kernel API as an exact numeric pair (`B-2`).
    pub kernel_api_requirement: String,
    /// The Mod's dependency requirements, evaluated as SemVer `VersionReq`
    /// expressions (`B-1a`).
    pub dependencies: Vec<ModDependencyRef>,
}

/// One dependency requirement declared by an installed Mod.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModDependencyRef {
    /// The depended-on Mod's stable identifier.
    pub id: String,
    /// SemVer `VersionReq` expression (`B-1a`).
    pub version_requirement: String,
}

/// The migration path query result injected by the caller.
///
/// Modelled as a three-state query *result* (`DESIGN-DECISION-01` decision 2):
/// chain lookup, migrator registration and cycle detection remain `SAVE-006`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MigrationPath {
    /// No migration is required. Contradicts any Schema difference — see
    /// [`CompatInputError::ContradictoryMigrationInput`].
    NotRequired,
    /// A declared migration path exists. The assessor does not re-verify the
    /// chain; the caller guarantees it was validated (`planning/33` §2.3).
    Available(Vec<MigrationHop>),
    /// No migration path exists.
    Unavailable,
}

/// One hop of a declared migration path (display-only in this stage).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MigrationHop {
    /// The Schema version migrated from (save-domain version type).
    pub from_version: SaveFormatVersion,
    /// The Schema version migrated to.
    pub to_version: SaveFormatVersion,
    /// Stable identifier of the migrator providing the hop.
    pub migrator_id: String,
}

/// The loading policy (contract §7): exhaustively frozen to two values — no
/// `#[non_exhaustive]`, and any third value requires a formal contract
/// revision first.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoadingPolicy {
    /// The pipeline result is reported unchanged.
    Normal,
    /// The migration gate: a pipeline result of `CompatibleWithMigration` is
    /// rewritten to `Blocked` + flags := {`rejected`} +
    /// `save.compat.policy_rejected` (`OWNER-APPROVAL-01` `B-4`). Every other
    /// pipeline verdict passes through unchanged.
    Strict,
}

// ---------------------------------------------------------------------------
// Machine-readable result vocabulary
// ---------------------------------------------------------------------------

/// The four machine-readable verdicts (spec §5.1), serialised as the frozen
/// snake_case wire literals.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompatibilityVerdict {
    /// The save can be loaded directly.
    Compatible,
    /// The save can be loaded after a confirmed migration.
    CompatibleWithMigration,
    /// The save cannot be loaded.
    Blocked,
    /// The save is structurally corrupt. **Never produced by this stage** —
    /// corruption is rejected by
    /// [`inspect_save`](crate::loader::inspect_save); retained for contract
    /// completeness only.
    Corrupted,
}

/// Policy-level refinement flags (spec §5.4), serialised as the frozen
/// snake_case literals.
///
/// [`Degraded`](Self::Degraded) and [`Discouraged`](Self::Discouraged) are
/// frozen but zero-trigger in this stage (`DESIGN-DECISION-05` amendment b);
/// contract tests pin their absence.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyFlag {
    /// Migration requires user confirmation before continuing.
    RequiresUserConfirmation,
    /// A required Mod or dependency is missing.
    MissingDependency,
    /// Loadable with degraded functionality (zero-trigger in this stage).
    Degraded,
    /// Loadable but discouraged (zero-trigger in this stage).
    Discouraged,
    /// Loading must be refused.
    Rejected,
}

/// The frozen stable reason codes (contract §6), serialised as the exact
/// dot-separated literals (per-variant `rename`: the literals are not a
/// snake_case derivation).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum CompatReasonCode {
    /// All checks passed.
    #[serde(rename = "save.compat.compatible")]
    Compatible,
    /// A Schema difference exists and a migration path is available.
    #[serde(rename = "save.compat.migration_available")]
    MigrationAvailable,
    /// The save format version is newer than this build supports. Defensive:
    /// unreachable through the public API (`inspect_save` rejects such saves
    /// first, `OWNER-DECISION-02`); retained as the assessor's own guard.
    #[serde(rename = "save.compat.unsupported_format_version")]
    UnsupportedFormatVersion,
    /// The save's Kernel API version differs from the current one.
    #[serde(rename = "save.compat.kernel_api_incompatible")]
    KernelApiIncompatible,
    /// The save's platform version is newer than the current platform.
    #[serde(rename = "save.compat.platform_incompatible")]
    PlatformIncompatible,
    /// A Mod the save requires is not installed.
    #[serde(rename = "save.compat.mod_missing")]
    ModMissing,
    /// Zero-trigger reserved code (`B-1`: a Mod's own `version` is not a
    /// judgement predicate); never produced by this stage.
    #[serde(rename = "save.compat.mod_version_incompatible")]
    ModVersionIncompatible,
    /// An installed Mod's dependency requirement is unsatisfied.
    #[serde(rename = "save.compat.mod_dependency_unsatisfied")]
    ModDependencyUnsatisfied,
    /// An installed Mod's Kernel API requirement mismatches the current one.
    #[serde(rename = "save.compat.mod_api_incompatible")]
    ModApiIncompatible,
    /// A Schema difference exists and no migration path is available.
    #[serde(rename = "save.compat.schema_mismatch_no_migration")]
    SchemaMismatchNoMigration,
    /// A migration path exists but the `Strict` policy refused it.
    #[serde(rename = "save.compat.policy_rejected")]
    PolicyRejected,
}

impl CompatReasonCode {
    /// The frozen wire representation.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Compatible => "save.compat.compatible",
            Self::MigrationAvailable => "save.compat.migration_available",
            Self::UnsupportedFormatVersion => "save.compat.unsupported_format_version",
            Self::KernelApiIncompatible => "save.compat.kernel_api_incompatible",
            Self::PlatformIncompatible => "save.compat.platform_incompatible",
            Self::ModMissing => "save.compat.mod_missing",
            Self::ModVersionIncompatible => "save.compat.mod_version_incompatible",
            Self::ModDependencyUnsatisfied => "save.compat.mod_dependency_unsatisfied",
            Self::ModApiIncompatible => "save.compat.mod_api_incompatible",
            Self::SchemaMismatchNoMigration => "save.compat.schema_mismatch_no_migration",
            Self::PolicyRejected => "save.compat.policy_rejected",
        }
    }

    /// Every frozen code, for exhaustiveness checks.
    pub const ALL: [Self; 11] = [
        Self::Compatible,
        Self::MigrationAvailable,
        Self::UnsupportedFormatVersion,
        Self::KernelApiIncompatible,
        Self::PlatformIncompatible,
        Self::ModMissing,
        Self::ModVersionIncompatible,
        Self::ModDependencyUnsatisfied,
        Self::ModApiIncompatible,
        Self::SchemaMismatchNoMigration,
        Self::PolicyRejected,
    ];
}

impl fmt::Display for CompatReasonCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Structured input errors (judgement does not start)
// ---------------------------------------------------------------------------

/// The version-bearing field whose injected value failed to parse
/// (`OWNER-APPROVAL-01` `B-3`; parse scope frozen to exactly these four).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VersionField {
    /// `CompatibilityEnv::platform_version` (SemVer).
    PlatformVersion,
    /// `InstalledModInfo::kernel_api_requirement` (`MAJOR.MINOR`).
    KernelApiRequirement,
    /// `ModDependencyRef::version_requirement` (SemVer `VersionReq`).
    DependencyVersionRequirement,
    /// `InstalledModInfo::version` (SemVer; `VersionReq` evaluation input,
    /// still not a judgement predicate).
    InstalledModVersion,
}

impl VersionField {
    /// A short diagnostic label (not a frozen stable code).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PlatformVersion => "platform_version",
            Self::KernelApiRequirement => "kernel_api_requirement",
            Self::DependencyVersionRequirement => "dependency_version_requirement",
            Self::InstalledModVersion => "installed_mod_version",
        }
    }
}

/// A structured input error: the caller-injected data is defective, so the
/// judgement does not start and no verdict is produced (`B-3`).
///
/// Stable codes share the `save.compat.input.*` prefix; the human-readable
/// message is never part of the protocol.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CompatInputError {
    /// The same Mod ID appears twice (environment side or save side).
    DuplicateModId {
        /// The duplicated Mod ID.
        mod_id: String,
    },
    /// An injected version string failed to parse.
    InvalidVersionFormat {
        /// Which field failed.
        field: VersionField,
        /// The rejected value.
        value: String,
    },
    /// A Mod ID (environment side or save side) is empty.
    EmptyModId,
    /// `migration == NotRequired` while a Schema version differs.
    ContradictoryMigrationInput,
}

impl CompatInputError {
    /// The stable machine code.
    pub const fn code_str(&self) -> &'static str {
        match self {
            Self::DuplicateModId { .. } => "save.compat.input.duplicate_mod_id",
            Self::InvalidVersionFormat { .. } => "save.compat.input.invalid_version_format",
            Self::EmptyModId => "save.compat.input.empty_mod_id",
            Self::ContradictoryMigrationInput => "save.compat.input.contradictory_migration_input",
        }
    }
}

impl fmt::Display for CompatInputError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateModId { mod_id } => write!(
                formatter,
                "{}: mod id `{mod_id}` is declared more than once",
                self.code_str()
            ),
            Self::InvalidVersionFormat { field, value } => write!(
                formatter,
                "{}: {} value `{value}` is not a valid version string",
                self.code_str(),
                field.as_str()
            ),
            Self::EmptyModId => {
                write!(formatter, "{}: a mod id is empty", self.code_str())
            }
            Self::ContradictoryMigrationInput => write!(
                formatter,
                "{}: migration is declared NotRequired but a schema version differs",
                self.code_str()
            ),
        }
    }
}

impl std::error::Error for CompatInputError {}

// ---------------------------------------------------------------------------
// Compatibility report (spec §13 diagnostic fields)
// ---------------------------------------------------------------------------

/// The outcome of a compatibility judgement (contract §2.3).
///
/// `verdict` / `reason_code` / `flags` form the machine protocol;
/// `failed_condition` / `available_actions` are human-readable and never part
/// of the protocol.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CompatibilityReport {
    /// The machine-readable verdict.
    pub verdict: CompatibilityVerdict,
    /// The stable reason code of the first failing step.
    pub reason_code: CompatReasonCode,
    /// Policy-level refinement flags (may be empty).
    pub flags: BTreeSet<PolicyFlag>,
    /// The Mod IDs involved, for Mod-level reasons only.
    pub affected_mods: Vec<String>,
    /// The save's recorded version, when relevant to the reason.
    pub save_version: Option<String>,
    /// The current version, when relevant to the reason.
    pub current_version: Option<String>,
    /// Human-readable description of the first failing condition.
    pub failed_condition: String,
    /// Human-readable handling suggestions for the reason.
    pub available_actions: Vec<String>,
    /// Whether loading should be preceded by a backup (computed from the
    /// pipeline result, before any policy rewrite).
    pub needs_backup: bool,
    /// Whether loading may lose data (same computation baseline as
    /// `needs_backup`).
    pub may_lose_data: bool,
    /// Whether loading may continue without user interaction (computed from
    /// the final, post-policy result).
    pub may_auto_continue: bool,
}

// ---------------------------------------------------------------------------
// The assessment pipeline (contract §4)
// ---------------------------------------------------------------------------

/// The pipeline outcome before the step-9 policy gate.
struct PipelineOutcome {
    verdict: CompatibilityVerdict,
    reason_code: CompatReasonCode,
    flags: BTreeSet<PolicyFlag>,
    affected_mods: Vec<String>,
    save_version: Option<String>,
    current_version: Option<String>,
}

impl PipelineOutcome {
    fn compatible() -> Self {
        Self {
            verdict: CompatibilityVerdict::Compatible,
            reason_code: CompatReasonCode::Compatible,
            flags: BTreeSet::new(),
            affected_mods: Vec::new(),
            save_version: None,
            current_version: None,
        }
    }
}

/// Environment values parsed once during step-0 validation.
struct ParsedEnv<'a> {
    platform_version: semver::Version,
    mods: Vec<ParsedMod<'a>>,
}

struct ParsedMod<'a> {
    info: &'a InstalledModInfo,
    version: semver::Version,
    kernel_api_requirement: SaveFormatVersion,
    dependencies: Vec<(&'a str, semver::VersionReq)>,
}

/// Judges an inspected save against an injected environment.
///
/// Pure: no I/O, no side effects; the same inputs always yield an identical
/// report. Defective inputs stop the judgement with a structured
/// [`CompatInputError`] before any verdict is produced.
pub fn assess_compatibility(
    inspected: &InspectedSave,
    env: &CompatibilityEnv,
) -> Result<CompatibilityReport, CompatInputError> {
    let parsed = validate_input(inspected, env)?;
    let pipeline = run_pipeline(inspected, env, &parsed);

    // Contract §8: `needs_backup` / `may_lose_data` are computed from the
    // pipeline result, before the step-9 policy rewrite.
    let needs_backup = pipeline.verdict == CompatibilityVerdict::CompatibleWithMigration;
    let may_lose_data = needs_backup || pipeline.flags.contains(&PolicyFlag::Degraded);

    // Step 9: the policy gate (contract §7).
    let final_outcome = match env.policy {
        LoadingPolicy::Strict
            if pipeline.verdict == CompatibilityVerdict::CompatibleWithMigration =>
        {
            PipelineOutcome {
                verdict: CompatibilityVerdict::Blocked,
                reason_code: CompatReasonCode::PolicyRejected,
                flags: BTreeSet::from([PolicyFlag::Rejected]),
                affected_mods: Vec::new(),
                save_version: None,
                current_version: None,
            }
        }
        _ => pipeline,
    };

    // Contract §8: `may_auto_continue` is computed from the final result.
    let may_auto_continue = matches!(
        final_outcome.verdict,
        CompatibilityVerdict::Compatible | CompatibilityVerdict::CompatibleWithMigration
    ) && !final_outcome.flags.contains(&PolicyFlag::Rejected)
        && !final_outcome.flags.contains(&PolicyFlag::Discouraged)
        && !final_outcome
            .flags
            .contains(&PolicyFlag::RequiresUserConfirmation);

    let (failed_condition, available_actions) = render(
        final_outcome.reason_code,
        &final_outcome.affected_mods,
        final_outcome.save_version.as_deref(),
        final_outcome.current_version.as_deref(),
    );

    Ok(CompatibilityReport {
        verdict: final_outcome.verdict,
        reason_code: final_outcome.reason_code,
        flags: final_outcome.flags,
        affected_mods: final_outcome.affected_mods,
        save_version: final_outcome.save_version,
        current_version: final_outcome.current_version,
        failed_condition,
        available_actions,
        needs_backup,
        may_lose_data,
        may_auto_continue,
    })
}

/// Step 0: deterministic input validation. The first defect in the frozen
/// order — `EmptyModId` → `DuplicateModId` → `InvalidVersionFormat` →
/// `ContradictoryMigrationInput` — stops the judgement.
fn validate_input<'a>(
    inspected: &InspectedSave,
    env: &'a CompatibilityEnv,
) -> Result<ParsedEnv<'a>, CompatInputError> {
    // 0a: empty Mod IDs (environment side first, then the save side). The
    // save-side check is defensive: `SAVE-001` validation already rejects
    // empty identifiers during inspection.
    if env.installed_mods.iter().any(|m| m.id.is_empty())
        || inspected.manifest().mods.iter().any(|m| m.id.is_empty())
    {
        return Err(CompatInputError::EmptyModId);
    }

    // 0b: duplicate Mod IDs (environment side first, then the save side; the
    // save-side check is likewise defensive against `SAVE-001`).
    if let Some(mod_id) = first_duplicate(env.installed_mods.iter().map(|m| m.id.as_str())) {
        return Err(CompatInputError::DuplicateModId { mod_id });
    }
    if let Some(mod_id) = first_duplicate(inspected.manifest().mods.iter().map(|m| m.id.as_str())) {
        return Err(CompatInputError::DuplicateModId { mod_id });
    }

    // 0c: version parsing, frozen to the minimal parse set (contract §2.2) in
    // the contract table's order: platform_version, each Mod's
    // kernel_api_requirement, each dependency's version_requirement, each
    // Mod's version. The first failure is returned.
    let platform_version = semver::Version::parse(&env.platform_version).map_err(|_| {
        CompatInputError::InvalidVersionFormat {
            field: VersionField::PlatformVersion,
            value: env.platform_version.clone(),
        }
    })?;
    let mut kernel_requirements = Vec::with_capacity(env.installed_mods.len());
    for m in &env.installed_mods {
        kernel_requirements.push(SaveFormatVersion::parse(&m.kernel_api_requirement).map_err(
            |_| CompatInputError::InvalidVersionFormat {
                field: VersionField::KernelApiRequirement,
                value: m.kernel_api_requirement.clone(),
            },
        )?);
    }
    let mut dependencies = Vec::with_capacity(env.installed_mods.len());
    for m in &env.installed_mods {
        let mut parsed_deps = Vec::with_capacity(m.dependencies.len());
        for d in &m.dependencies {
            let requirement = semver::VersionReq::parse(&d.version_requirement).map_err(|_| {
                CompatInputError::InvalidVersionFormat {
                    field: VersionField::DependencyVersionRequirement,
                    value: d.version_requirement.clone(),
                }
            })?;
            parsed_deps.push((d.id.as_str(), requirement));
        }
        dependencies.push(parsed_deps);
    }
    let mut versions = Vec::with_capacity(env.installed_mods.len());
    for m in &env.installed_mods {
        versions.push(semver::Version::parse(&m.version).map_err(|_| {
            CompatInputError::InvalidVersionFormat {
                field: VersionField::InstalledModVersion,
                value: m.version.clone(),
            }
        })?);
    }

    // 0d: `NotRequired` contradicting a Schema difference.
    if matches!(env.migration, MigrationPath::NotRequired)
        && first_schema_difference(inspected, env).is_some()
    {
        return Err(CompatInputError::ContradictoryMigrationInput);
    }

    let mods = env
        .installed_mods
        .iter()
        .zip(kernel_requirements)
        .zip(dependencies)
        .zip(versions)
        .map(
            |(((info, kernel_api_requirement), dependencies), version)| ParsedMod {
                info,
                version,
                kernel_api_requirement,
                dependencies,
            },
        )
        .collect();
    Ok(ParsedEnv {
        platform_version,
        mods,
    })
}

fn first_duplicate<'a>(mut ids: impl Iterator<Item = &'a str>) -> Option<String> {
    let mut seen = BTreeSet::new();
    ids.find(|id| !seen.insert(*id)).map(str::to_owned)
}

/// Runs steps 2-8 in the frozen order (contract §4); the first failing step
/// decides the outcome.
fn run_pipeline(
    inspected: &InspectedSave,
    env: &CompatibilityEnv,
    parsed: &ParsedEnv<'_>,
) -> PipelineOutcome {
    let manifest = inspected.manifest();

    // Step 2 (defensive layer, `OWNER-DECISION-02`): the format-version gate.
    // Unreachable on the public path — `inspect_save` rejects a future format
    // during integrity verification — retained as the assessor's own guard.
    if !manifest.save_format_version.is_supported() {
        return PipelineOutcome {
            verdict: CompatibilityVerdict::Blocked,
            reason_code: CompatReasonCode::UnsupportedFormatVersion,
            flags: BTreeSet::from([PolicyFlag::Rejected]),
            affected_mods: Vec::new(),
            save_version: Some(manifest.save_format_version.to_string()),
            current_version: Some(SUPPORTED_SAVE_FORMAT_VERSION.to_string()),
        };
    }

    // Step 3: Kernel API exact numeric-pair match, then the platform one-way
    // gate (`DESIGN-DECISION-05` judgement B).
    let save_kernel_api = save_domain_version(&manifest.kernel_api_version);
    if save_kernel_api != env.kernel_api_version {
        return PipelineOutcome {
            verdict: CompatibilityVerdict::Blocked,
            reason_code: CompatReasonCode::KernelApiIncompatible,
            flags: BTreeSet::new(),
            affected_mods: Vec::new(),
            save_version: Some(manifest.kernel_api_version.clone()),
            current_version: Some(env.kernel_api_version.to_string()),
        };
    }
    let save_platform = semver::Version::parse(&manifest.platform_version)
        .expect("`SAVE-001` validated platform_version during inspection");
    if save_platform > parsed.platform_version {
        return PipelineOutcome {
            verdict: CompatibilityVerdict::Blocked,
            reason_code: CompatReasonCode::PlatformIncompatible,
            flags: BTreeSet::new(),
            affected_mods: Vec::new(),
            save_version: Some(manifest.platform_version.clone()),
            current_version: Some(env.platform_version.clone()),
        };
    }

    // Step 4: Mod set comparison. A Mod the save requires but the environment
    // lacks blocks the load; a Mod's own `version` is never a predicate
    // (`B-1`). Extra installed Mods are the "new Mod into an old save"
    // scenario (spec §6): recorded, not judged, in this stage.
    let missing: Vec<String> = manifest
        .mods
        .iter()
        .filter(|entry| !parsed.mods.iter().any(|m| m.info.id == entry.id))
        .map(|entry| entry.id.clone())
        .collect();
    if !missing.is_empty() {
        return PipelineOutcome {
            verdict: CompatibilityVerdict::Blocked,
            reason_code: CompatReasonCode::ModMissing,
            flags: BTreeSet::from([PolicyFlag::MissingDependency]),
            affected_mods: missing,
            save_version: None,
            current_version: None,
        };
    }

    // Step 5: dependency `VersionReq` evaluation, then each Mod's Kernel API
    // requirement. `affected_mods` lists the Mods whose own requirements
    // failed (first-occurrence order).
    let mut unsatisfied: Vec<String> = Vec::new();
    for m in &parsed.mods {
        for (dependency_id, requirement) in &m.dependencies {
            let satisfied = parsed.mods.iter().any(|other| {
                other.info.id == *dependency_id && requirement.matches(&other.version)
            });
            if !satisfied && !unsatisfied.contains(&m.info.id) {
                unsatisfied.push(m.info.id.clone());
            }
        }
    }
    if !unsatisfied.is_empty() {
        return PipelineOutcome {
            verdict: CompatibilityVerdict::Blocked,
            reason_code: CompatReasonCode::ModDependencyUnsatisfied,
            flags: BTreeSet::from([PolicyFlag::MissingDependency]),
            affected_mods: unsatisfied,
            save_version: None,
            current_version: None,
        };
    }
    let api_mismatched: Vec<String> = parsed
        .mods
        .iter()
        .filter(|m| m.kernel_api_requirement != env.kernel_api_version)
        .map(|m| m.info.id.clone())
        .collect();
    if !api_mismatched.is_empty() {
        return PipelineOutcome {
            verdict: CompatibilityVerdict::Blocked,
            reason_code: CompatReasonCode::ModApiIncompatible,
            flags: BTreeSet::new(),
            affected_mods: api_mismatched,
            save_version: None,
            current_version: None,
        };
    }

    // Steps 6/7: Schema comparison, then the injected migration query result.
    if let Some((save_version, current_version)) = first_schema_difference(inspected, env) {
        match &env.migration {
            MigrationPath::Available(_) => {
                return PipelineOutcome {
                    verdict: CompatibilityVerdict::CompatibleWithMigration,
                    reason_code: CompatReasonCode::MigrationAvailable,
                    flags: BTreeSet::from([PolicyFlag::RequiresUserConfirmation]),
                    affected_mods: Vec::new(),
                    save_version: Some(save_version),
                    current_version: Some(current_version),
                };
            }
            MigrationPath::Unavailable => {
                return PipelineOutcome {
                    verdict: CompatibilityVerdict::Blocked,
                    reason_code: CompatReasonCode::SchemaMismatchNoMigration,
                    flags: BTreeSet::new(),
                    affected_mods: Vec::new(),
                    save_version: Some(save_version),
                    current_version: Some(current_version),
                };
            }
            // Step 0d rejected this combination before the judgement started.
            MigrationPath::NotRequired => {
                unreachable!("step 0d rejects `NotRequired` with a Schema difference")
            }
        }
    }

    // Step 8 (shallow, `DESIGN-DECISION-01` decision 4): `SAVE-002`'s
    // ownership/consistency conclusions are reused as-is — integrity already
    // covers the carried bytes, and no reference-graph analysis (`SAVE-010`)
    // or verdict change happens in this stage.

    PipelineOutcome::compatible()
}

/// Detects the first Schema difference in a deterministic order (contract §4
/// step 6): first the save's `world_schema_version` against each save Mod's
/// `schema_version` in Manifest order, then each save Mod's `schema_version`
/// against its installed counterpart in Manifest order.
///
/// Returns `(save_version, current_version)` display strings for the report:
/// a world-vs-Mod difference reports the save's world Schema version as
/// `save_version` and the differing Mod Schema version as `current_version`;
/// a save-vs-installed difference reports the save's record and the installed
/// value. Mods missing from the environment are skipped here — step 4 owns
/// that failure.
fn first_schema_difference(
    inspected: &InspectedSave,
    env: &CompatibilityEnv,
) -> Option<(String, String)> {
    let manifest = inspected.manifest();
    let world_schema = save_domain_version(&manifest.world_schema_version);
    for entry in &manifest.mods {
        if save_domain_version(&entry.schema_version) != world_schema {
            return Some((
                manifest.world_schema_version.clone(),
                entry.schema_version.clone(),
            ));
        }
    }
    for entry in &manifest.mods {
        if let Some(installed) = env.installed_mods.iter().find(|m| m.id == entry.id)
            && installed.schema_version != save_domain_version(&entry.schema_version)
        {
            return Some((
                entry.schema_version.clone(),
                installed.schema_version.to_string(),
            ));
        }
    }
    None
}

/// Parses a save-side `MAJOR.MINOR` field that `SAVE-001` validation already
/// accepted during inspection; the parse therefore cannot fail.
fn save_domain_version(value: &str) -> SaveFormatVersion {
    SaveFormatVersion::parse(value).expect("`SAVE-001` validated this field during inspection")
}

/// Renders the human-readable fields from the frozen static templates
/// (contract §2.3): `{ids}` / `{save}` / `{current}` placeholders are
/// interpolated from the report fields.
fn render(
    reason_code: CompatReasonCode,
    affected_mods: &[String],
    save_version: Option<&str>,
    current_version: Option<&str>,
) -> (String, Vec<String>) {
    let actions = |items: &[&str]| items.iter().map(|item| (*item).to_owned()).collect();
    let ids = || affected_mods.join(", ");
    let save = || save_version.unwrap_or("<unknown>");
    let current = || current_version.unwrap_or("<unknown>");
    match reason_code {
        CompatReasonCode::Compatible => (String::new(), actions(&["直接加载"])),
        CompatReasonCode::MigrationAvailable => (
            "存在 Schema 差异，已找到可用迁移路径".to_owned(),
            actions(&["备份存档", "确认并执行迁移", "放弃加载"]),
        ),
        CompatReasonCode::UnsupportedFormatVersion => (
            format!("存档格式版本 {} 高于本包支持上限 {}", save(), current()),
            actions(&["使用支持该格式版本的更新平台"]),
        ),
        CompatReasonCode::KernelApiIncompatible => (
            format!("存档内核 API 版本 {} 与当前 {} 不一致", save(), current()),
            actions(&["切换到与存档匹配的内核版本", "放弃加载"]),
        ),
        CompatReasonCode::PlatformIncompatible => (
            format!("存档平台版本 {} 高于当前平台 {}", save(), current()),
            actions(&["升级平台后重试", "放弃加载"]),
        ),
        CompatReasonCode::ModMissing => (
            format!("存档所需 Mod 缺失：{}", ids()),
            actions(&["安装缺失的 Mod", "放弃加载"]),
        ),
        // Zero-trigger reserved code (`B-1`): never produced in this stage,
        // so its template stays disabled per contract §2.3.
        CompatReasonCode::ModVersionIncompatible => (String::new(), Vec::new()),
        CompatReasonCode::ModDependencyUnsatisfied => (
            format!("已安装 Mod 的依赖要求未满足：{}", ids()),
            actions(&["安装或升级满足依赖要求的 Mod", "放弃加载"]),
        ),
        CompatReasonCode::ModApiIncompatible => (
            format!("Mod 的内核 API 要求与当前内核不一致：{}", ids()),
            actions(&["更换与当前内核匹配的 Mod 版本", "放弃加载"]),
        ),
        CompatReasonCode::SchemaMismatchNoMigration => (
            "存在 Schema 差异且无可用迁移路径".to_owned(),
            actions(&["放弃加载", "提供迁移器后重试"]),
        ),
        CompatReasonCode::PolicyRejected => (
            "迁移路径存在但被 Strict 策略拒绝".to_owned(),
            actions(&["以 Normal 策略重新评估并确认迁移", "放弃加载"]),
        ),
    }
}
