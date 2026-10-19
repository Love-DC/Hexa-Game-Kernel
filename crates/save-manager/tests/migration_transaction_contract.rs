use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use kernel_core::Tick;
use save_manager::integrity;
use save_manager::loader::{inspect_save, inspect_save_file};
use save_manager::migration::{
    MigrationDataScope, MigrationDomain, MigrationPlan, MigrationRegistry, MigrationTargets,
    MigrationVerification, MigratorDescriptor, build_migration_plan,
};
use save_manager::partition::{PartitionPayload, SaveLayout, SavePartition, SavePartitionId};
use save_manager::transaction::{
    MigrationCandidateValidator, MigrationInput, MigrationOutput, MigrationRequest,
    MigrationRunner, RecoveryOutcome, RunnerFailure, TransactionError, ValidationFailure, migrate,
    recover_migration,
};
use save_manager::writer;
use save_manager::{
    GameDefinition, Integrity, SaveFormatVersion as V, SaveManifest, SaveModEntry, SimulationConfig,
};

fn descriptor() -> MigratorDescriptor {
    MigratorDescriptor {
        id: "org.example.mod.migrate.1-2".into(),
        domain: MigrationDomain::Mod {
            mod_id: "org.example.mod".into(),
        },
        from_schema: V::new(1, 0),
        to_schema: V::new(2, 0),
        migrator_version: "1.0.0".into(),
        requires: BTreeSet::new(),
        input_scope: MigrationDataScope {
            world: false,
            mods: BTreeSet::from(["org.example.mod".into()]),
        },
        output_scope: MigrationDataScope {
            world: false,
            mods: BTreeSet::from(["org.example.mod".into()]),
        },
        changes_world_data: false,
        has_irreversible_ops: false,
        resource_budget: BTreeMap::from([("memory.bytes".into(), 4096)]),
        verification: MigrationVerification::GoldenDigest,
    }
}

fn build_save(world: &[u8], mod_data: &[u8]) -> Vec<u8> {
    let mut manifest = SaveManifest {
        save_format_version: V::new(1, 0),
        save_id: "migration.transaction".into(),
        created_at: "2026-10-19T00:00:00Z".into(),
        last_saved_at: "2026-10-19T00:00:00Z".into(),
        platform_version: "1.0.0".into(),
        game_definition: GameDefinition {
            id: "org.example.game".into(),
            version: "1.0.0".into(),
        },
        kernel_api_version: "1.0".into(),
        world_schema_version: "1.0".into(),
        logical_tick: Tick(3),
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
    };
    let initial = SaveLayout::new(vec![
        SavePartition::new(SavePartitionId::Manifest, manifest.to_bytes()),
        SavePartition::new(SavePartitionId::World, world.to_vec()),
        SavePartition::new(
            SavePartitionId::Mod(
                save_manager::partition::ModNamespace::parse("org.example.mod").unwrap(),
            ),
            mod_data.to_vec(),
        ),
        SavePartition::new(
            SavePartitionId::Metadata,
            writer::encode_system_manifest(&["game/system"]).unwrap(),
        ),
        SavePartition::new(SavePartitionId::Integrity, Vec::new()),
    ]);
    let generated = integrity::generate(&initial).unwrap();
    manifest.integrity.root_hash = generated.root;
    let layout = SaveLayout::new(
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
    );
    layout.to_bytes().unwrap()
}

fn temp_target() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "gksave-save007-{}-{}.gksave",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

fn lock_sidecar(target: &std::path::Path) -> PathBuf {
    target.with_file_name(format!(
        ".{}.migration-lock",
        target.file_name().unwrap().to_string_lossy()
    ))
}

struct AppendRunner {
    calls: usize,
}
impl MigrationRunner for AppendRunner {
    fn run(
        &mut self,
        step: &save_manager::migration::PlannedMigration,
        input: MigrationInput,
    ) -> Result<MigrationOutput, RunnerFailure> {
        self.calls += 1;
        assert_eq!(step.descriptor.id, "org.example.mod.migrate.1-2");
        let mut value = input.mods.get("org.example.mod").unwrap().clone();
        value.extend_from_slice(b"-migrated");
        Ok(MigrationOutput {
            world: None,
            mods: BTreeMap::from([("org.example.mod".into(), value)]),
        })
    }
}

struct FailingRunner {
    calls: usize,
}
impl MigrationRunner for FailingRunner {
    fn run(
        &mut self,
        _: &save_manager::migration::PlannedMigration,
        _: MigrationInput,
    ) -> Result<MigrationOutput, RunnerFailure> {
        self.calls += 1;
        Err(RunnerFailure {
            code: "test.failure".into(),
            message: "injected runner failure".into(),
        })
    }
}

struct TestValidator {
    calls: usize,
}

struct BusyCheckingRunner {
    target: PathBuf,
    busy_code: Option<&'static str>,
}
impl MigrationRunner for BusyCheckingRunner {
    fn run(
        &mut self,
        _: &save_manager::migration::PlannedMigration,
        input: MigrationInput,
    ) -> Result<MigrationOutput, RunnerFailure> {
        self.busy_code = Some(recover_migration(&self.target).unwrap_err().code());
        let mut value = input.mods.get("org.example.mod").unwrap().clone();
        value.extend_from_slice(b"-locked");
        Ok(MigrationOutput {
            world: None,
            mods: BTreeMap::from([("org.example.mod".into(), value)]),
        })
    }
}
impl MigrationCandidateValidator for TestValidator {
    fn validate(&mut self, _: &SaveManifest, _: &SaveLayout) -> Result<(), ValidationFailure> {
        self.calls += 1;
        Ok(())
    }
}

fn inputs(
    bytes: &[u8],
) -> (
    save_manager::loader::InspectedSave,
    MigrationRegistry,
    MigrationPlan,
    BTreeSet<String>,
) {
    let save = inspect_save(bytes).unwrap();
    let registry = MigrationRegistry::build(vec![descriptor()]).unwrap();
    let installed = BTreeSet::from(["org.example.mod".into()]);
    let targets = MigrationTargets {
        world_schema_version: V::new(1, 0),
        mod_schema_versions: BTreeMap::from([("org.example.mod".into(), V::new(2, 0))]),
    };
    let plan = build_migration_plan(&save, &registry, &targets, &installed).unwrap();
    (save, registry, plan, installed)
}

#[test]
fn migration_commits_verified_candidate_and_retains_recovery_copy() {
    let target = temp_target();
    let source = build_save(b"world-source", b"mod-source");
    std::fs::write(&target, &source).unwrap();
    let (inspected, registry, plan, installed) = inputs(&source);
    let mut runner = AppendRunner { calls: 0 };
    let mut validator = TestValidator { calls: 0 };
    let outcome = migrate(
        &target,
        MigrationRequest {
            inspected: &inspected,
            registry: &registry,
            plan: &plan,
            installed_mod_ids: &installed,
            last_saved_at: "2026-10-19T01:00:00Z",
        },
        &mut runner,
        &mut validator,
    )
    .unwrap();
    assert_eq!(runner.calls, 1);
    assert_eq!(validator.calls, 1);
    assert_ne!(outcome.root_hash, inspected.manifest().integrity.root_hash);
    assert_eq!(std::fs::read(&outcome.recovery_copy).unwrap(), source);
    let committed = inspect_save(&std::fs::read(&target).unwrap()).unwrap();
    assert_eq!(committed.manifest().mods[0].schema_version, "2.0");
    assert_eq!(
        committed.mod_payload("org.example.mod").unwrap(),
        b"mod-source-migrated"
    );
    assert_eq!(committed.world_payload(), b"world-source");
    assert!(
        !target
            .with_file_name(format!(
                ".{}.migration-journal",
                target.file_name().unwrap().to_string_lossy()
            ))
            .exists()
    );
    let _ = std::fs::remove_file(&target);
    let _ = std::fs::remove_file(outcome.recovery_copy);
    let _ = std::fs::remove_file(lock_sidecar(&target));
}

#[test]
fn runner_failure_keeps_original_and_recovery_does_not_rerun_it() {
    let target = temp_target();
    let source = build_save(b"world-source", b"mod-source");
    std::fs::write(&target, &source).unwrap();
    let (inspected, registry, plan, installed) = inputs(&source);
    let mut runner = FailingRunner { calls: 0 };
    let mut validator = TestValidator { calls: 0 };
    let error = migrate(
        &target,
        MigrationRequest {
            inspected: &inspected,
            registry: &registry,
            plan: &plan,
            installed_mod_ids: &installed,
            last_saved_at: "2026-10-19T01:00:00Z",
        },
        &mut runner,
        &mut validator,
    )
    .unwrap_err();
    assert_eq!(error.code(), "save.migration.runner_failed");
    assert_eq!(runner.calls, 1);
    assert_eq!(std::fs::read(&target).unwrap(), source);
    let journal = target.with_file_name(format!(
        ".{}.migration-journal",
        target.file_name().unwrap().to_string_lossy()
    ));
    let journal_text = std::fs::read_to_string(&journal).unwrap();
    assert!(journal_text.contains("test.failure"));
    assert!(journal_text.contains("org.example.mod.migrate.1-2"));
    let recovery = recover_migration(&target).unwrap();
    let RecoveryOutcome::OriginalPreserved {
        backup: Some(backup),
    } = recovery
    else {
        panic!("original should remain intact")
    };
    assert_eq!(runner.calls, 1);
    let _ = std::fs::remove_file(backup);
    let _ = std::fs::remove_file(&target);
    let _ = std::fs::remove_file(lock_sidecar(&target));
}

#[test]
fn source_change_fails_before_runner() {
    let target = temp_target();
    let source = build_save(b"world-source", b"mod-source");
    std::fs::write(&target, &source).unwrap();
    let (inspected, registry, plan, installed) = inputs(&source);
    std::fs::write(&target, build_save(b"changed-world", b"mod-source")).unwrap();
    let mut runner = AppendRunner { calls: 0 };
    let mut validator = TestValidator { calls: 0 };
    assert!(matches!(
        migrate(
            &target,
            MigrationRequest {
                inspected: &inspected,
                registry: &registry,
                plan: &plan,
                installed_mod_ids: &installed,
                last_saved_at: "2026-10-19T01:00:00Z"
            },
            &mut runner,
            &mut validator
        ),
        Err(TransactionError::Input {
            code: "save.migration.source_changed",
            ..
        })
    ));
    assert_eq!(runner.calls, 0);
    let _ = std::fs::remove_file(&target);
    let _ = std::fs::remove_file(lock_sidecar(&target));
}

#[test]
fn plan_descriptor_must_match_registry_before_runner() {
    let target = temp_target();
    let source = build_save(b"world-source", b"mod-source");
    std::fs::write(&target, &source).unwrap();
    let (inspected, registry, mut plan, installed) = inputs(&source);
    plan.steps[0]
        .descriptor
        .resource_budget
        .insert("memory.bytes".into(), 1);
    let mut runner = AppendRunner { calls: 0 };
    let mut validator = TestValidator { calls: 0 };
    assert!(matches!(
        migrate(
            &target,
            MigrationRequest {
                inspected: &inspected,
                registry: &registry,
                plan: &plan,
                installed_mod_ids: &installed,
                last_saved_at: "2026-10-19T01:00:00Z"
            },
            &mut runner,
            &mut validator,
        ),
        Err(TransactionError::Input {
            code: "save.migration.plan_mismatch",
            ..
        })
    ));
    assert_eq!(runner.calls, 0);
    assert_eq!(std::fs::read(&target).unwrap(), source);
    let _ = std::fs::remove_file(&target);
    let _ = std::fs::remove_file(lock_sidecar(&target));
}

#[test]
fn incomplete_migration_chain_is_rejected_before_any_source_write() {
    let target = temp_target();
    let source = build_save(b"world-source", b"mod-source");
    std::fs::write(&target, &source).unwrap();
    let inspected = inspect_save(&source).unwrap();
    let registry = MigrationRegistry::build(Vec::new()).unwrap();
    let installed = BTreeSet::from(["org.example.mod".to_owned()]);
    let error = build_migration_plan(
        &inspected,
        &registry,
        &MigrationTargets {
            world_schema_version: V::new(1, 0),
            mod_schema_versions: BTreeMap::from([("org.example.mod".into(), V::new(2, 0))]),
        },
        &installed,
    )
    .expect_err("an absent migration edge must stop before transaction start");
    assert!(matches!(
        error,
        save_manager::migration::MigrationPlanError::Lookup {
            source: save_manager::migration::MigrationLookupError::NoMigrationPath { .. },
            ..
        }
    ));
    assert_eq!(std::fs::read(&target).unwrap(), source);
    assert!(inspect_save_file(&target).is_ok());
    let _ = std::fs::remove_file(&target);
    let _ = std::fs::remove_file(lock_sidecar(&target));
}

#[test]
fn recovery_refuses_to_interfere_while_migration_runner_is_active() {
    let target = temp_target();
    let source = build_save(b"world-source", b"mod-source");
    std::fs::write(&target, &source).unwrap();
    let (inspected, registry, plan, installed) = inputs(&source);
    let mut runner = BusyCheckingRunner {
        target: target.clone(),
        busy_code: None,
    };
    let mut validator = TestValidator { calls: 0 };
    let outcome = migrate(
        &target,
        MigrationRequest {
            inspected: &inspected,
            registry: &registry,
            plan: &plan,
            installed_mod_ids: &installed,
            last_saved_at: "2026-10-19T01:00:00Z",
        },
        &mut runner,
        &mut validator,
    )
    .unwrap();
    assert_eq!(runner.busy_code, Some("save.migration.transaction_busy"));
    let _ = std::fs::remove_file(outcome.recovery_copy);
    let _ = std::fs::remove_file(&target);
    let _ = std::fs::remove_file(lock_sidecar(&target));
}
