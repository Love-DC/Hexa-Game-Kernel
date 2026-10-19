//! `SAVE-007`: migration transactions, recovery copies, append-only journal,
//! candidate validation, atomic replacement and crash recovery.
//!
//! Migrator code is never loaded here. A trusted assembly adapter implements
//! [`MigrationRunner`]; a future platform sandbox must enforce the same scope.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::integrity;
use crate::loader::{InspectedSave, LoadError, inspect_save, inspect_save_file};
use crate::migration::{MigrationDomain, MigrationPlan, MigrationRegistry, PlannedMigration};
use crate::partition::{ModNamespace, SaveLayout, SavePartition, SavePartitionId};
use crate::{SaveManifest, SaveManifestError};

static TRANSACTION_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Immutable bytes authorized by one migrator's input scope.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MigrationInput {
    pub world: Option<Vec<u8>>,
    pub mods: BTreeMap<String, Vec<u8>>,
}

/// Data returned by a migration runner; only declared output scopes are accepted.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MigrationOutput {
    pub world: Option<Vec<u8>>,
    pub mods: BTreeMap<String, Vec<u8>>,
}

/// A caller supplied migration execution boundary. `save-manager` scopes the
/// data passed to this trait; the implementation remains responsible for its
/// execution sandbox and determinism policy.
pub trait MigrationRunner {
    fn run(
        &mut self,
        step: &PlannedMigration,
        input: MigrationInput,
    ) -> Result<MigrationOutput, RunnerFailure>;
}

/// Required post-migration validator for world invariants and cross-Mod references.
/// SAVE-007 has no permissive default; SAVE-010 supplies the reference checks.
pub trait MigrationCandidateValidator {
    fn validate(
        &mut self,
        manifest: &SaveManifest,
        layout: &SaveLayout,
    ) -> Result<(), ValidationFailure>;
}

/// Structured failure from candidate validation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidationFailure {
    pub code: String,
    pub message: String,
}

/// Structured failure returned by a runner.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunnerFailure {
    pub code: String,
    pub message: String,
}

/// Persisted transaction protocol phase.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum TransactionPhase {
    Started,
    BackedUp,
    Executing,
    CandidateReady,
    CommitStarted,
    Committed,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct JournalFailure {
    code: String,
    detail_code: Option<String>,
    stage: String,
    step_index: Option<usize>,
    migrator_id: Option<String>,
}

/// One append-only journal record. Each complete line is independently parseable.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct JournalRecord {
    protocol_version: u32,
    transaction_id: String,
    sequence: u64,
    phase: TransactionPhase,
    save_id: String,
    original_root_hash: String,
    candidate_root_hash: Option<String>,
    plan_digest: String,
    completed_steps: usize,
    failure: Option<JournalFailure>,
}

/// Recovery result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecoveryOutcome {
    NoTransaction,
    OriginalPreserved { backup: Option<PathBuf> },
    CandidateCommitted { root_hash: String, backup: PathBuf },
    OriginalRestored { root_hash: String, backup: PathBuf },
}

/// Transaction failure with stable category, stage and optional migration step.
#[derive(Debug)]
pub enum TransactionError {
    Io {
        stage: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    Load(LoadError),
    Manifest(SaveManifestError),
    Input {
        code: &'static str,
        message: String,
    },
    Runner {
        step: usize,
        migrator_id: String,
        failure: RunnerFailure,
    },
    Validation {
        failure: ValidationFailure,
    },
    Journal {
        code: &'static str,
        message: String,
    },
    Recovery {
        code: &'static str,
        message: String,
    },
}

impl TransactionError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Io { .. } => "save.migration.io",
            Self::Load(_) => "save.migration.source_invalid",
            Self::Manifest(_) => "save.migration.manifest_invalid",
            Self::Input { code, .. } => code,
            Self::Runner { .. } => "save.migration.runner_failed",
            Self::Validation { .. } => "save.migration.validation_failed",
            Self::Journal { code, .. } | Self::Recovery { code, .. } => code,
        }
    }
}

impl std::fmt::Display for TransactionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io {
                stage,
                path,
                source,
            } => write!(
                f,
                "migration I/O at {stage} for {}: {source}",
                path.display()
            ),
            Self::Load(error) => write!(f, "migration source is invalid: {error}"),
            Self::Manifest(error) => write!(f, "migration manifest is invalid: {error}"),
            Self::Input { code, message } => write!(f, "{code}: {message}"),
            Self::Runner {
                step,
                migrator_id,
                failure,
            } => write!(
                f,
                "migration step {step} `{migrator_id}` failed ({}): {}",
                failure.code, failure.message
            ),
            Self::Validation { failure } => write!(
                f,
                "candidate validation failed ({}): {}",
                failure.code, failure.message
            ),
            Self::Journal { code, message } | Self::Recovery { code, message } => {
                write!(f, "{code}: {message}")
            }
        }
    }
}

impl std::error::Error for TransactionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Load(error) => Some(error),
            Self::Manifest(error) => Some(error),
            _ => None,
        }
    }
}

/// Result of a committed migration. The recovery copy is deliberately retained.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MigrationOutcome {
    pub save_id: String,
    pub root_hash: String,
    pub bytes_written: u64,
    pub recovery_copy: PathBuf,
}

/// Inputs that must agree before a transaction can start.
pub struct MigrationRequest<'a> {
    pub inspected: &'a InspectedSave,
    pub registry: &'a MigrationRegistry,
    pub plan: &'a MigrationPlan,
    pub installed_mod_ids: &'a std::collections::BTreeSet<String>,
    pub last_saved_at: &'a str,
}

/// Executes a SAVE-006 plan without mutating the original until candidate commit.
pub fn migrate<R: MigrationRunner>(
    target: &Path,
    request: MigrationRequest<'_>,
    runner: &mut R,
    validator: &mut impl MigrationCandidateValidator,
) -> Result<MigrationOutcome, TransactionError> {
    let MigrationRequest {
        inspected,
        registry,
        plan,
        installed_mod_ids,
        last_saved_at,
    } = request;
    let paths = TransactionPaths::new(target)?;
    let _target_lock = acquire_target_lock(&paths)?;
    let source_bytes =
        fs::read(target).map_err(|source| io_error("read_source", target, source))?;
    let actual = inspect_save(&source_bytes).map_err(TransactionError::Load)?;
    let original_root = actual.manifest().integrity.root_hash.clone();
    if inspected.manifest().save_id != actual.manifest().save_id
        || inspected.manifest().integrity.root_hash != original_root
        || plan.save_id != actual.manifest().save_id
        || plan.source_root_hash != original_root
    {
        return Err(input_error(
            "save.migration.source_changed",
            "inspected save, plan and target identity/hash must match",
        ));
    }
    for step in &plan.steps {
        if registry.descriptor(&step.descriptor.id) != Some(&step.descriptor) {
            return Err(input_error(
                "save.migration.plan_mismatch",
                format!(
                    "step `{}` differs from the registered descriptor",
                    step.descriptor.id
                ),
            ));
        }
        if let Some(missing) = step
            .descriptor
            .requires
            .difference(installed_mod_ids)
            .next()
        {
            return Err(input_error(
                "save.migration.requirement_missing",
                format!(
                    "step `{}` requires unavailable Mod `{missing}`",
                    step.descriptor.id
                ),
            ));
        }
    }
    let mut manifest = actual.manifest().clone();
    if manifest.save_id != plan.save_id {
        return Err(input_error(
            "save.migration.plan_mismatch",
            "plan save_id does not match source",
        ));
    }
    manifest.last_saved_at = last_saved_at.to_owned();
    manifest.validate().map_err(TransactionError::Manifest)?;

    let transaction_id = format!(
        "{}-{}",
        std::process::id(),
        TRANSACTION_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    validate_transaction_id(&transaction_id)?;
    let journal = paths.journal.clone();
    let mut log = OpenOptions::new()
        .read(true)
        .append(true)
        .create_new(true)
        .open(&journal)
        .map_err(|source| io_error("journal_create", &journal, source))?;
    let plan_digest = digest_bytes(
        &serde_json::to_vec(plan)
            .map_err(|error| journal_error("save.migration.journal_invalid", error.to_string()))?,
    );
    let mut record = JournalRecord {
        protocol_version: 1,
        transaction_id: transaction_id.clone(),
        sequence: 0,
        phase: TransactionPhase::Started,
        save_id: manifest.save_id.clone(),
        original_root_hash: original_root.clone(),
        candidate_root_hash: None,
        plan_digest,
        completed_steps: 0,
        failure: None,
    };
    append_record(&mut log, &journal, &record)?;

    // Establish and verify the immutable recovery copy before invoking any migrator.
    write_exclusive_synced(
        &paths.backup(&transaction_id),
        &source_bytes,
        "backup_create",
    )?;
    let backup =
        inspect_save_file(&paths.backup(&transaction_id)).map_err(TransactionError::Load)?;
    if backup.manifest().integrity.root_hash != original_root {
        return Err(input_error(
            "save.migration.backup_invalid",
            "recovery copy hash differs from source",
        ));
    }
    advance(
        &mut log,
        &journal,
        &mut record,
        TransactionPhase::BackedUp,
        0,
        None,
    )?;

    let mut layout = SaveLayout::from_bytes(&source_bytes)
        .map_err(|error| input_error("save.migration.source_invalid", error.to_string()))?;
    for (index, step) in plan.steps.iter().enumerate() {
        if let Err(error) = validate_step_schema(&manifest, step) {
            record_failure(
                &mut log,
                &journal,
                &mut record,
                JournalFailure {
                    code: error.code().to_owned(),
                    detail_code: None,
                    stage: "step_validation".to_owned(),
                    step_index: Some(index),
                    migrator_id: Some(step.descriptor.id.clone()),
                },
            )?;
            return Err(error);
        }
        advance(
            &mut log,
            &journal,
            &mut record,
            TransactionPhase::Executing,
            index,
            None,
        )?;
        let input = authorized_input(&layout, step)?;
        let output = match runner.run(step, input) {
            Ok(output) => output,
            Err(failure) => {
                record_failure(
                    &mut log,
                    &journal,
                    &mut record,
                    JournalFailure {
                        code: "save.migration.runner_failed".to_owned(),
                        detail_code: Some(failure.code.clone()),
                        stage: "runner".to_owned(),
                        step_index: Some(index),
                        migrator_id: Some(step.descriptor.id.clone()),
                    },
                )?;
                return Err(TransactionError::Runner {
                    step: index,
                    migrator_id: step.descriptor.id.clone(),
                    failure,
                });
            }
        };
        if let Err(error) = apply_output(&mut layout, step, output) {
            record_failure(
                &mut log,
                &journal,
                &mut record,
                JournalFailure {
                    code: error.code().to_owned(),
                    detail_code: None,
                    stage: "output_validation".to_owned(),
                    step_index: Some(index),
                    migrator_id: Some(step.descriptor.id.clone()),
                },
            )?;
            return Err(error);
        }
        if let Err(error) = update_schema(&mut manifest, step) {
            record_failure(
                &mut log,
                &journal,
                &mut record,
                JournalFailure {
                    code: error.code().to_owned(),
                    detail_code: None,
                    stage: "schema_update".to_owned(),
                    step_index: Some(index),
                    migrator_id: Some(step.descriptor.id.clone()),
                },
            )?;
            return Err(error);
        }
        update_partition_schema(&mut layout, step);
        replace_partition(
            &mut layout,
            SavePartition::new(SavePartitionId::Manifest, manifest.to_bytes()),
        );
        record.completed_steps = index + 1;
    }
    manifest.last_saved_at = last_saved_at.to_owned();
    manifest.validate().map_err(TransactionError::Manifest)?;
    replace_partition(
        &mut layout,
        SavePartition::new(SavePartitionId::Manifest, manifest.to_bytes()),
    );
    if let Err(failure) = validator.validate(&manifest, &layout) {
        record_failure(
            &mut log,
            &journal,
            &mut record,
            JournalFailure {
                code: "save.migration.validation_failed".to_owned(),
                detail_code: Some(failure.code.clone()),
                stage: "candidate_validation".to_owned(),
                step_index: None,
                migrator_id: None,
            },
        )?;
        return Err(TransactionError::Validation { failure });
    }
    let (candidate_bytes, candidate_root) = finalize_layout(layout, &manifest)?;
    let work = paths.work(&transaction_id);
    write_exclusive_synced(&work, &candidate_bytes, "candidate_create")?;
    let candidate = inspect_save_file(&work).map_err(TransactionError::Load)?;
    if candidate.manifest().integrity.root_hash != candidate_root {
        return Err(input_error(
            "save.migration.candidate_invalid",
            "candidate root differs after read-back",
        ));
    }
    advance(
        &mut log,
        &journal,
        &mut record,
        TransactionPhase::CandidateReady,
        plan.steps.len(),
        Some(candidate_root.clone()),
    )?;
    advance(
        &mut log,
        &journal,
        &mut record,
        TransactionPhase::CommitStarted,
        plan.steps.len(),
        Some(candidate_root.clone()),
    )?;
    fs::rename(&work, target).map_err(|source| io_error("commit_rename", target, source))?;
    let committed = inspect_save_file(target).map_err(TransactionError::Load)?;
    if committed.manifest().integrity.root_hash != candidate_root {
        return Err(input_error(
            "save.migration.commit_verification_failed",
            "target root differs from verified candidate",
        ));
    }
    advance(
        &mut log,
        &journal,
        &mut record,
        TransactionPhase::Committed,
        plan.steps.len(),
        Some(candidate_root.clone()),
    )?;
    drop(log);
    fs::remove_file(&journal).map_err(|source| io_error("journal_remove", &journal, source))?;
    Ok(MigrationOutcome {
        save_id: manifest.save_id,
        root_hash: candidate_root,
        bytes_written: u64::try_from(candidate_bytes.len()).expect("usize fits u64"),
        recovery_copy: paths.backup(&transaction_id),
    })
}

/// Resolves an interrupted migration from the active append-only journal.
pub fn recover_migration(target: &Path) -> Result<RecoveryOutcome, TransactionError> {
    let paths = TransactionPaths::new(target)?;
    let _target_lock = acquire_target_lock(&paths)?;
    if !paths.journal.exists() {
        return Ok(RecoveryOutcome::NoTransaction);
    }
    let record = read_journal(&paths.journal)?;
    let backup = paths.backup(&record.transaction_id);
    let work = paths.work(&record.transaction_id);
    let target_root = inspect_save_file(target)
        .ok()
        .map(|save| save.manifest().integrity.root_hash.clone());
    if target_root.as_deref() == Some(&record.original_root_hash) {
        if record.phase == TransactionPhase::Committed {
            return Err(recovery_error(
                "save.migration.journal_conflict",
                "journal says committed but target has the original root",
            ));
        }
        let retained_backup = if record.phase != TransactionPhase::Started {
            let backup_save = inspect_save_file(&backup).map_err(|error| {
                recovery_error("save.migration.backup_invalid", error.to_string())
            })?;
            if backup_save.manifest().integrity.root_hash != record.original_root_hash {
                return Err(recovery_error(
                    "save.migration.backup_invalid",
                    "backup root does not match journal",
                ));
            }
            Some(backup.clone())
        } else if backup.exists()
            && inspect_save_file(&backup)
                .ok()
                .is_none_or(|save| save.manifest().integrity.root_hash != record.original_root_hash)
        {
            remove_if_exists(&backup);
            None
        } else if backup.exists() {
            Some(backup.clone())
        } else {
            None
        };
        let candidate_ready = retained_backup.is_some()
            && matches!(
                record.phase,
                TransactionPhase::CandidateReady | TransactionPhase::CommitStarted
            )
            && record
                .candidate_root_hash
                .as_deref()
                .is_some_and(|expected| {
                    inspect_save_file(&work)
                        .ok()
                        .is_some_and(|save| save.manifest().integrity.root_hash == expected)
                });
        if candidate_ready {
            fs::rename(&work, target)
                .map_err(|source| io_error("recovery_commit", target, source))?;
            let committed = inspect_save_file(target).map_err(TransactionError::Load)?;
            fs::remove_file(&paths.journal)
                .map_err(|source| io_error("journal_remove", &paths.journal, source))?;
            return Ok(RecoveryOutcome::CandidateCommitted {
                root_hash: committed.manifest().integrity.root_hash.clone(),
                backup: retained_backup.expect("candidate readiness requires verified backup"),
            });
        }
        remove_if_exists(&work);
        fs::remove_file(&paths.journal)
            .map_err(|source| io_error("journal_remove", &paths.journal, source))?;
        return Ok(RecoveryOutcome::OriginalPreserved {
            backup: retained_backup,
        });
    }
    let already_committed = matches!((
        record.candidate_root_hash.as_deref(),
        target_root.as_deref(),
    ), (Some(expected), Some(actual)) if expected == actual);
    if already_committed {
        let backup_save = inspect_save_file(&backup)
            .map_err(|error| recovery_error("save.migration.backup_invalid", error.to_string()))?;
        if backup_save.manifest().integrity.root_hash != record.original_root_hash {
            return Err(recovery_error(
                "save.migration.backup_invalid",
                "backup root does not match journal",
            ));
        }
        let root_hash = target_root
            .expect("committed target has verified root hash")
            .to_owned();
        fs::remove_file(&paths.journal)
            .map_err(|source| io_error("journal_remove", &paths.journal, source))?;
        return Ok(RecoveryOutcome::CandidateCommitted { root_hash, backup });
    }
    let backup_save = inspect_save_file(&backup)
        .map_err(|error| recovery_error("save.migration.backup_invalid", error.to_string()))?;
    if backup_save.manifest().integrity.root_hash != record.original_root_hash {
        return Err(recovery_error(
            "save.migration.backup_invalid",
            "backup root does not match journal",
        ));
    }
    let restore = paths.restore(&record.transaction_id);
    let bytes = fs::read(&backup).map_err(|source| io_error("backup_read", &backup, source))?;
    write_exclusive_synced(&restore, &bytes, "restore_create")?;
    inspect_save_file(&restore)
        .map_err(|error| recovery_error("save.migration.restore_invalid", error.to_string()))?;
    fs::rename(&restore, target).map_err(|source| io_error("restore_rename", target, source))?;
    let restored = inspect_save_file(target).map_err(TransactionError::Load)?;
    fs::remove_file(&paths.journal)
        .map_err(|source| io_error("journal_remove", &paths.journal, source))?;
    remove_if_exists(&work);
    Ok(RecoveryOutcome::OriginalRestored {
        root_hash: restored.manifest().integrity.root_hash.clone(),
        backup,
    })
}

fn validate_step_schema(
    manifest: &SaveManifest,
    step: &PlannedMigration,
) -> Result<(), TransactionError> {
    let current = match &step.descriptor.domain {
        MigrationDomain::World { game_definition_id } => {
            if game_definition_id != &manifest.game_definition.id {
                return Err(input_error(
                    "save.migration.plan_mismatch",
                    "World owner differs from source Game Definition",
                ));
            }
            crate::SaveFormatVersion::parse(&manifest.world_schema_version)
                .expect("validated manifest")
        }
        MigrationDomain::Mod { mod_id } => manifest
            .mods
            .iter()
            .find(|entry| &entry.id == mod_id)
            .map(|entry| {
                crate::SaveFormatVersion::parse(&entry.schema_version).expect("validated manifest")
            })
            .ok_or_else(|| {
                input_error(
                    "save.migration.plan_mismatch",
                    format!("Mod `{mod_id}` is absent from source Manifest"),
                )
            })?,
    };
    if current != step.descriptor.from_schema {
        return Err(input_error(
            "save.migration.step_version_mismatch",
            format!(
                "step `{}` expects {}, source is {current}",
                step.descriptor.id, step.descriptor.from_schema
            ),
        ));
    }
    Ok(())
}

fn authorized_input(
    layout: &SaveLayout,
    step: &PlannedMigration,
) -> Result<MigrationInput, TransactionError> {
    let mut input = MigrationInput::default();
    if step.descriptor.input_scope.world {
        input.world = Some(
            layout
                .get(&SavePartitionId::World)
                .ok_or_else(|| {
                    input_error(
                        "save.migration.input_missing",
                        "declared World input partition is absent",
                    )
                })?
                .payload
                .as_bytes()
                .to_vec(),
        );
    }
    for mod_id in &step.descriptor.input_scope.mods {
        let namespace = ModNamespace::parse(mod_id)
            .map_err(|error| input_error("save.migration.scope_invalid", error.to_string()))?;
        let Some(partition) = layout.get(&SavePartitionId::Mod(namespace)) else {
            continue;
        };
        input
            .mods
            .insert(mod_id.clone(), partition.payload.as_bytes().to_vec());
    }
    Ok(input)
}

fn apply_output(
    layout: &mut SaveLayout,
    step: &PlannedMigration,
    output: MigrationOutput,
) -> Result<(), TransactionError> {
    if output.world.is_some()
        && (!step.descriptor.output_scope.world || !step.descriptor.changes_world_data)
    {
        return Err(input_error(
            "save.migration.output_scope_violation",
            format!(
                "step `{}` returned undeclared World output",
                step.descriptor.id
            ),
        ));
    }
    if step.descriptor.changes_world_data && !step.descriptor.output_scope.world {
        return Err(input_error(
            "save.migration.output_scope_violation",
            format!(
                "step `{}` declares World changes without World output scope",
                step.descriptor.id
            ),
        ));
    }
    if let Some(world) = output.world {
        let existing = layout.get(&SavePartitionId::World).ok_or_else(|| {
            input_error(
                "save.migration.output_missing",
                "World output partition is absent",
            )
        })?;
        let mut replacement = SavePartition::new(SavePartitionId::World, world);
        replacement.schema_version = existing.schema_version.clone();
        replace_partition(layout, replacement);
    }
    for (mod_id, bytes) in output.mods {
        if !step.descriptor.output_scope.mods.contains(&mod_id) {
            return Err(input_error(
                "save.migration.output_scope_violation",
                format!(
                    "step `{}` returned undeclared Mod `{mod_id}` output",
                    step.descriptor.id
                ),
            ));
        }
        let namespace = ModNamespace::parse(&mod_id)
            .map_err(|error| input_error("save.migration.scope_invalid", error.to_string()))?;
        let id = SavePartitionId::Mod(namespace);
        let schema_version = layout
            .get(&id)
            .and_then(|partition| partition.schema_version.clone());
        let mut replacement = SavePartition::new(id, bytes);
        replacement.schema_version = schema_version;
        replace_partition(layout, replacement);
    }
    Ok(())
}

fn update_schema(
    manifest: &mut SaveManifest,
    step: &PlannedMigration,
) -> Result<(), TransactionError> {
    match &step.descriptor.domain {
        MigrationDomain::World { .. } => {
            manifest.world_schema_version = step.descriptor.to_schema.to_string()
        }
        MigrationDomain::Mod { mod_id } => {
            let entry = manifest
                .mods
                .iter_mut()
                .find(|entry| &entry.id == mod_id)
                .ok_or_else(|| {
                    input_error(
                        "save.migration.plan_mismatch",
                        format!("Mod `{mod_id}` is absent from Manifest"),
                    )
                })?;
            entry.schema_version = step.descriptor.to_schema.to_string();
        }
    }
    Ok(())
}

fn update_partition_schema(layout: &mut SaveLayout, step: &PlannedMigration) {
    let id = match &step.descriptor.domain {
        MigrationDomain::World { .. } => SavePartitionId::World,
        MigrationDomain::Mod { mod_id } => match ModNamespace::parse(mod_id) {
            Ok(namespace) => SavePartitionId::Mod(namespace),
            Err(_) => return,
        },
    };
    if let Some(partition) = layout
        .partitions
        .iter_mut()
        .find(|partition| partition.id == id)
    {
        partition.schema_version = Some(step.descriptor.to_schema.to_string());
    }
}

fn replace_partition(layout: &mut SaveLayout, replacement: SavePartition) {
    if let Some(existing) = layout
        .partitions
        .iter_mut()
        .find(|partition| partition.id == replacement.id)
    {
        *existing = replacement;
    } else {
        layout.partitions.push(replacement);
    }
    *layout = SaveLayout::new(std::mem::take(&mut layout.partitions));
}

fn finalize_layout(
    mut layout: SaveLayout,
    manifest: &SaveManifest,
) -> Result<(Vec<u8>, String), TransactionError> {
    replace_partition(
        &mut layout,
        SavePartition::new(SavePartitionId::Manifest, manifest.to_bytes()),
    );
    if layout.get(&SavePartitionId::Integrity).is_none() {
        return Err(input_error(
            "save.migration.candidate_invalid",
            "source layout has no Integrity partition",
        ));
    }
    let generated = integrity::generate(&layout)
        .map_err(|error| input_error("save.migration.candidate_invalid", error.to_string()))?;
    let mut final_manifest = manifest.clone();
    final_manifest.integrity.root_hash = generated.root.clone();
    final_manifest
        .validate()
        .map_err(TransactionError::Manifest)?;
    replace_partition(
        &mut layout,
        SavePartition::new(SavePartitionId::Manifest, final_manifest.to_bytes()),
    );
    replace_partition(
        &mut layout,
        SavePartition::new(SavePartitionId::Integrity, generated.checksums),
    );
    let bytes = layout
        .to_bytes()
        .map_err(|error| input_error("save.migration.candidate_invalid", error.to_string()))?;
    let verified = inspect_save(&bytes).map_err(TransactionError::Load)?;
    if !verified.integrity_report().is_intact() {
        return Err(input_error(
            "save.migration.candidate_invalid",
            "candidate integrity did not verify",
        ));
    }
    Ok((bytes, generated.root))
}

fn append_record(
    file: &mut File,
    path: &Path,
    record: &JournalRecord,
) -> Result<(), TransactionError> {
    let mut bytes = serde_json::to_vec(record)
        .map_err(|error| journal_error("save.migration.journal_invalid", error.to_string()))?;
    bytes.push(b'\n');
    file.write_all(&bytes)
        .map_err(|source| io_error("journal_write", path, source))?;
    file.sync_all()
        .map_err(|source| io_error("journal_sync", path, source))
}

fn try_exclusive_lock(file: &File, path: &Path) -> Result<(), TransactionError> {
    match file.try_lock() {
        Ok(()) => Ok(()),
        Err(TryLockError::WouldBlock) => Err(journal_error(
            "save.migration.transaction_busy",
            format!(
                "another process owns the migration journal `{}`",
                path.display()
            ),
        )),
        Err(TryLockError::Error(source)) => Err(io_error("journal_lock", path, source)),
    }
}

fn advance(
    file: &mut File,
    path: &Path,
    record: &mut JournalRecord,
    phase: TransactionPhase,
    completed_steps: usize,
    candidate_root: Option<String>,
) -> Result<(), TransactionError> {
    record.sequence += 1;
    record.phase = phase;
    record.completed_steps = completed_steps;
    if candidate_root.is_some() {
        record.candidate_root_hash = candidate_root;
    }
    append_record(file, path, record)
}

fn record_failure(
    file: &mut File,
    path: &Path,
    record: &mut JournalRecord,
    failure: JournalFailure,
) -> Result<(), TransactionError> {
    record.sequence += 1;
    record.phase = TransactionPhase::Failed;
    record.failure = Some(failure);
    append_record(file, path, record)
}

fn read_journal(path: &Path) -> Result<JournalRecord, TransactionError> {
    let bytes = fs::read(path).map_err(|source| io_error("journal_read", path, source))?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|error| journal_error("save.migration.journal_invalid", error.to_string()))?;
    let lines: Vec<_> = text.lines().collect();
    let mut last: Option<JournalRecord> = None;
    for (index, line) in lines.iter().enumerate() {
        match serde_json::from_str::<JournalRecord>(line) {
            Ok(record) => {
                if record.protocol_version != 1 || !valid_transaction_id(&record.transaction_id) {
                    return Err(journal_error(
                        "save.migration.journal_invalid",
                        "unsupported protocol or invalid transaction ID",
                    ));
                }
                if record.save_id.trim().is_empty()
                    || !valid_root_hash(&record.original_root_hash)
                    || record
                        .candidate_root_hash
                        .as_deref()
                        .is_some_and(|hash| !valid_root_hash(hash))
                    || !valid_root_hash(&record.plan_digest)
                {
                    return Err(journal_error(
                        "save.migration.journal_invalid",
                        "journal identity or digest field is invalid",
                    ));
                }
                if (record.phase == TransactionPhase::Failed) != record.failure.is_some() {
                    return Err(journal_error(
                        "save.migration.journal_invalid",
                        "failure diagnostics do not match the journal phase",
                    ));
                }
                if let Some(previous) = &last {
                    if record.transaction_id != previous.transaction_id
                        || record.sequence != previous.sequence + 1
                        || !valid_transition(previous.phase, record.phase)
                        || record.save_id != previous.save_id
                        || record.original_root_hash != previous.original_root_hash
                        || record.plan_digest != previous.plan_digest
                        || record.completed_steps < previous.completed_steps
                    {
                        return Err(journal_error(
                            "save.migration.journal_invalid",
                            "journal sequence or phase transition is invalid",
                        ));
                    }
                } else if record.sequence != 0 || record.phase != TransactionPhase::Started {
                    return Err(journal_error(
                        "save.migration.journal_invalid",
                        "journal does not start at sequence zero",
                    ));
                }
                last = Some(record);
            }
            Err(_error) if index + 1 == lines.len() && !text.ends_with('\n') => break,
            Err(error) => {
                return Err(journal_error(
                    "save.migration.journal_invalid",
                    error.to_string(),
                ));
            }
        }
    }
    last.ok_or_else(|| {
        journal_error(
            "save.migration.journal_invalid",
            "journal has no complete record",
        )
    })
}

fn valid_transition(from: TransactionPhase, to: TransactionPhase) -> bool {
    matches!(
        (from, to),
        (TransactionPhase::Started, TransactionPhase::BackedUp)
            | (TransactionPhase::BackedUp, TransactionPhase::Executing)
            | (TransactionPhase::Executing, TransactionPhase::Executing)
            | (TransactionPhase::BackedUp, TransactionPhase::CandidateReady)
            | (
                TransactionPhase::Executing,
                TransactionPhase::CandidateReady
            )
            | (
                TransactionPhase::CandidateReady,
                TransactionPhase::CommitStarted
            )
            | (TransactionPhase::CommitStarted, TransactionPhase::Committed)
            | (TransactionPhase::Started, TransactionPhase::Failed)
            | (TransactionPhase::BackedUp, TransactionPhase::Failed)
            | (TransactionPhase::Executing, TransactionPhase::Failed)
    )
}

struct TransactionPaths {
    journal: PathBuf,
    lock: PathBuf,
    parent: PathBuf,
    base: String,
}

impl TransactionPaths {
    fn new(target: &Path) -> Result<Self, TransactionError> {
        let parent = target
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."))
            .to_path_buf();
        let base = target
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                input_error(
                    "save.migration.path_invalid",
                    "target must have a UTF-8 file name",
                )
            })?
            .to_owned();
        Ok(Self {
            journal: parent.join(format!(".{base}.migration-journal")),
            lock: parent.join(format!(".{base}.migration-lock")),
            parent,
            base,
        })
    }
    fn backup(&self, id: &str) -> PathBuf {
        self.parent
            .join(format!(".{}.migration-{id}.backup", self.base))
    }
    fn work(&self, id: &str) -> PathBuf {
        self.parent
            .join(format!(".{}.migration-{id}.work", self.base))
    }
    fn restore(&self, id: &str) -> PathBuf {
        self.parent
            .join(format!(".{}.migration-{id}.restore", self.base))
    }
}

fn acquire_target_lock(paths: &TransactionPaths) -> Result<File, TransactionError> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&paths.lock)
        .map_err(|source| io_error("target_lock_open", &paths.lock, source))?;
    try_exclusive_lock(&file, &paths.lock)?;
    Ok(file)
}

fn write_exclusive_synced(
    path: &Path,
    bytes: &[u8],
    stage: &'static str,
) -> Result<(), TransactionError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|source| io_error(stage, path, source))?;
    if let Err(source) = file.write_all(bytes) {
        let _ = fs::remove_file(path);
        return Err(io_error(stage, path, source));
    }
    if let Err(source) = file.sync_all() {
        let _ = fs::remove_file(path);
        return Err(io_error(stage, path, source));
    }
    Ok(())
}

fn remove_if_exists(path: &Path) {
    if path.exists() {
        let _ = fs::remove_file(path);
    }
}

fn validate_transaction_id(id: &str) -> Result<(), TransactionError> {
    if valid_transaction_id(id) {
        Ok(())
    } else {
        Err(input_error(
            "save.migration.transaction_id_invalid",
            "transaction ID contains disallowed path characters",
        ))
    }
}

fn valid_transaction_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

fn digest_bytes(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn valid_root_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn input_error(code: &'static str, message: impl Into<String>) -> TransactionError {
    TransactionError::Input {
        code,
        message: message.into(),
    }
}
fn journal_error(code: &'static str, message: impl Into<String>) -> TransactionError {
    TransactionError::Journal {
        code,
        message: message.into(),
    }
}
fn recovery_error(code: &'static str, message: impl Into<String>) -> TransactionError {
    TransactionError::Recovery {
        code,
        message: message.into(),
    }
}
fn io_error(stage: &'static str, path: &Path, source: io::Error) -> TransactionError {
    TransactionError::Io {
        stage,
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::partition::{ModNamespace, PartitionPayload};
    use crate::{GameDefinition, Integrity, SaveFormatVersion, SaveModEntry, SimulationConfig};
    use kernel_core::Tick;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn archive(mod_schema: &str, mod_bytes: &[u8]) -> Vec<u8> {
        let mut manifest = SaveManifest {
            save_format_version: SaveFormatVersion::new(1, 0),
            save_id: "save007.recovery".into(),
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
                schema_version: mod_schema.into(),
            }],
            simulation_config: SimulationConfig::new(),
            integrity: Integrity {
                algorithm: "sha256".into(),
                root_hash: "0".repeat(64),
            },
        };
        let initial = SaveLayout::new(vec![
            SavePartition::new(SavePartitionId::Manifest, manifest.to_bytes()),
            SavePartition::new(SavePartitionId::World, b"world".to_vec()),
            SavePartition::new(
                SavePartitionId::Mod(ModNamespace::parse("org.example.mod").unwrap()),
                mod_bytes.to_vec(),
            ),
            SavePartition::new(
                SavePartitionId::Metadata,
                crate::writer::encode_system_manifest(&[]).unwrap(),
            ),
            SavePartition::new(SavePartitionId::Integrity, Vec::new()),
        ]);
        let generated = integrity::generate(&initial).unwrap();
        manifest.integrity.root_hash = generated.root;
        SaveLayout::new(
            initial
                .partitions
                .into_iter()
                .map(|mut partition| {
                    match partition.id {
                        SavePartitionId::Manifest => {
                            partition.payload = PartitionPayload::new(manifest.to_bytes())
                        }
                        SavePartitionId::Integrity => {
                            partition.payload = PartitionPayload::new(generated.checksums.clone())
                        }
                        _ => {}
                    }
                    partition
                })
                .collect(),
        )
        .to_bytes()
        .unwrap()
    }

    fn target() -> PathBuf {
        std::env::temp_dir().join(format!(
            "gksave-save007-recovery-{}-{}.gksave",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn start_journal(
        paths: &TransactionPaths,
        id: &str,
        original_root: String,
    ) -> (File, JournalRecord) {
        let mut file = OpenOptions::new()
            .append(true)
            .create_new(true)
            .open(&paths.journal)
            .unwrap();
        let record = JournalRecord {
            protocol_version: 1,
            transaction_id: id.into(),
            sequence: 0,
            phase: TransactionPhase::Started,
            save_id: "save007.recovery".into(),
            original_root_hash: original_root,
            candidate_root_hash: None,
            plan_digest: "00".repeat(32),
            completed_steps: 0,
            failure: None,
        };
        append_record(&mut file, &paths.journal, &record).unwrap();
        (file, record)
    }

    #[test]
    fn recovery_promotes_ready_candidate_without_reexecuting_migrator() {
        let target = target();
        let source = archive("1.0", b"before");
        let candidate = archive("2.0", b"after");
        fs::write(&target, &source).unwrap();
        let paths = TransactionPaths::new(&target).unwrap();
        let id = "unit-recovery";
        write_exclusive_synced(&paths.backup(id), &source, "backup").unwrap();
        write_exclusive_synced(&paths.work(id), &candidate, "work").unwrap();
        let original_root = inspect_save(&source)
            .unwrap()
            .manifest()
            .integrity
            .root_hash
            .clone();
        let candidate_root = inspect_save(&candidate)
            .unwrap()
            .manifest()
            .integrity
            .root_hash
            .clone();
        let (mut log, mut record) = start_journal(&paths, id, original_root);
        advance(
            &mut log,
            &paths.journal,
            &mut record,
            TransactionPhase::BackedUp,
            0,
            None,
        )
        .unwrap();
        advance(
            &mut log,
            &paths.journal,
            &mut record,
            TransactionPhase::Executing,
            1,
            None,
        )
        .unwrap();
        advance(
            &mut log,
            &paths.journal,
            &mut record,
            TransactionPhase::CandidateReady,
            1,
            Some(candidate_root.clone()),
        )
        .unwrap();
        advance(
            &mut log,
            &paths.journal,
            &mut record,
            TransactionPhase::CommitStarted,
            1,
            Some(candidate_root.clone()),
        )
        .unwrap();
        drop(log);
        assert!(
            matches!(recover_migration(&target).unwrap(), RecoveryOutcome::CandidateCommitted { root_hash, .. } if root_hash == candidate_root)
        );
        assert_eq!(
            inspect_save_file(&target)
                .unwrap()
                .manifest()
                .integrity
                .root_hash,
            candidate_root
        );
        assert!(inspect_save_file(&paths.backup(id)).is_ok());
        assert_eq!(fs::read(paths.backup(id)).unwrap(), source);
        let _ = fs::remove_file(paths.backup(id));
        let _ = fs::remove_file(target);
        let _ = fs::remove_file(paths.lock);
    }

    #[test]
    fn recovery_restores_verified_backup_when_target_is_corrupt() {
        let target = target();
        let source = archive("1.0", b"before");
        fs::write(&target, b"damaged target").unwrap();
        let paths = TransactionPaths::new(&target).unwrap();
        let id = "unit-restore";
        write_exclusive_synced(&paths.backup(id), &source, "backup").unwrap();
        let original_root = inspect_save(&source)
            .unwrap()
            .manifest()
            .integrity
            .root_hash
            .clone();
        let (mut log, mut record) = start_journal(&paths, id, original_root.clone());
        advance(
            &mut log,
            &paths.journal,
            &mut record,
            TransactionPhase::BackedUp,
            0,
            None,
        )
        .unwrap();
        drop(log);
        assert!(
            matches!(recover_migration(&target).unwrap(), RecoveryOutcome::OriginalRestored { root_hash, .. } if root_hash == original_root)
        );
        assert_eq!(fs::read(&target).unwrap(), source);
        let _ = fs::remove_file(paths.backup(id));
        let _ = fs::remove_file(target);
        let _ = fs::remove_file(paths.lock);
    }

    #[test]
    fn recovery_recognizes_candidate_committed_before_final_journal_record() {
        let target = target();
        let original = archive("1.0", b"before");
        let candidate = archive("2.0", b"after");
        fs::write(&target, &candidate).unwrap();
        let paths = TransactionPaths::new(&target).unwrap();
        let id = "unit-post-commit";
        write_exclusive_synced(&paths.backup(id), &original, "backup").unwrap();
        let original_root = inspect_save(&original)
            .unwrap()
            .manifest()
            .integrity
            .root_hash
            .clone();
        let candidate_root = inspect_save(&candidate)
            .unwrap()
            .manifest()
            .integrity
            .root_hash
            .clone();
        let (mut log, mut record) = start_journal(&paths, id, original_root);
        advance(
            &mut log,
            &paths.journal,
            &mut record,
            TransactionPhase::BackedUp,
            0,
            None,
        )
        .unwrap();
        advance(
            &mut log,
            &paths.journal,
            &mut record,
            TransactionPhase::Executing,
            1,
            None,
        )
        .unwrap();
        advance(
            &mut log,
            &paths.journal,
            &mut record,
            TransactionPhase::CandidateReady,
            1,
            Some(candidate_root.clone()),
        )
        .unwrap();
        advance(
            &mut log,
            &paths.journal,
            &mut record,
            TransactionPhase::CommitStarted,
            1,
            Some(candidate_root.clone()),
        )
        .unwrap();
        drop(log);
        assert!(
            matches!(recover_migration(&target).unwrap(), RecoveryOutcome::CandidateCommitted { root_hash, .. } if root_hash == candidate_root)
        );
        assert_eq!(fs::read(&target).unwrap(), candidate);
        assert!(paths.backup(id).exists());
        assert!(inspect_save_file(&paths.backup(id)).is_ok());
        assert_eq!(fs::read(paths.backup(id)).unwrap(), original);
        let _ = fs::remove_file(paths.backup(id));
        let _ = fs::remove_file(target);
        let _ = fs::remove_file(paths.lock);
    }

    #[test]
    fn recovery_rejects_corrupt_backup_without_touching_target_or_backup() {
        let target = target();
        let original = archive("1.0", b"before");
        let damaged_target = b"damaged target";
        let damaged_backup = b"damaged backup";
        fs::write(&target, damaged_target).unwrap();
        let paths = TransactionPaths::new(&target).unwrap();
        let id = "unit-bad-backup";
        fs::write(paths.backup(id), damaged_backup).unwrap();
        let original_root = inspect_save(&original)
            .unwrap()
            .manifest()
            .integrity
            .root_hash
            .clone();
        let (mut log, mut record) = start_journal(&paths, id, original_root);
        advance(
            &mut log,
            &paths.journal,
            &mut record,
            TransactionPhase::BackedUp,
            0,
            None,
        )
        .unwrap();
        drop(log);
        let error = recover_migration(&target).unwrap_err();
        assert_eq!(error.code(), "save.migration.backup_invalid");
        assert_eq!(fs::read(&target).unwrap(), damaged_target);
        assert_eq!(fs::read(paths.backup(id)).unwrap(), damaged_backup);
        let _ = fs::remove_file(paths.backup(id));
        let _ = fs::remove_file(paths.journal);
        let _ = fs::remove_file(target);
        let _ = fs::remove_file(paths.lock);
    }
}
