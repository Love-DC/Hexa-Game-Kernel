use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};

use game_platform::migration_runtime::{WasmMigrationRunner, migration_engine};
use game_platform::{ModFaultReport, RollbackOutcome, validate_fault_report_value};
use kernel_core::{Kernel, Tick, World, WorldId};
use save_manager::SaveFormatVersion;
use save_manager::migration::{
    MigrationDataScope, MigrationDomain, MigrationRegistry, MigrationTargets,
    MigrationVerification, MigratorDescriptor, PlannedMigration, build_migration_plan,
};
use save_manager::partition::{ModNamespace, SavePartition, SavePartitionId};
use save_manager::references::{
    MigrationValidationPipeline, ModReferenceExtractor, ModReferenceSnapshot,
    ReferenceExtractionFailure, ReferenceExtractorRegistry, WorldInvariantValidator,
};
use save_manager::transaction::{
    MigrationInput, MigrationOutput, MigrationRunner, ValidationFailure,
};
use save_manager::{
    GameDefinition, Integrity, SaveManifest, SaveModEntry, SimulationConfig, writer,
};

fn planned() -> PlannedMigration {
    PlannedMigration {
        descriptor: MigratorDescriptor {
            id: "org.example.mod.migrate.1-2".into(),
            domain: MigrationDomain::Mod {
                mod_id: "org.example.mod".into(),
            },
            from_schema: SaveFormatVersion::new(1, 0),
            to_schema: SaveFormatVersion::new(2, 0),
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
            resource_budget: BTreeMap::new(),
            verification: MigrationVerification::GoldenDigest,
        },
    }
}

fn guest(output: &str) -> Vec<u8> {
    let escaped = output.replace('\\', "\\\\").replace('"', "\\\"");
    let wat = format!(
        r#"(module
          (memory (export "memory") 3 3)
          (data (i32.const 100) "{escaped}")
          (func (export "migrate") (param $input i32) (param $input_len i32)
                (param $out i32) (param $capacity i32) (result i32)
            local.get $out
            i32.const 100
            i32.const {}
            memory.copy
            i32.const {}))"#,
        output.len(),
        output.len()
    );
    wat::parse_str(wat).expect("valid migration guest WAT")
}

fn runner(bytes: Vec<u8>) -> WasmMigrationRunner {
    WasmMigrationRunner::new(
        migration_engine().unwrap(),
        BTreeMap::from([("org.example.mod.migrate.1-2".into(), bytes)]),
        32,
    )
    .unwrap()
}

struct KernelSnapshotValidator;
impl WorldInvariantValidator for KernelSnapshotValidator {
    fn validate_world(
        &mut self,
        manifest: &SaveManifest,
        world: &[u8],
    ) -> Result<(), ValidationFailure> {
        let kernel = Kernel::from_snapshot(world).map_err(|error| ValidationFailure {
            code: "save.world.invalid_snapshot".into(),
            message: error.to_string(),
        })?;
        if kernel.world().tick() != manifest.logical_tick {
            return Err(ValidationFailure {
                code: "save.world.tick_mismatch".into(),
                message: "snapshot tick does not match manifest logical tick".into(),
            });
        }
        Ok(())
    }
}

struct EmptyReferenceExtractor;
impl ModReferenceExtractor for EmptyReferenceExtractor {
    fn extract(
        &self,
        _: &str,
        _: &[u8],
    ) -> Result<ModReferenceSnapshot, ReferenceExtractionFailure> {
        Ok(ModReferenceSnapshot::default())
    }
}

fn execute_transaction(
    guest_bytes: Vec<u8>,
) -> (
    std::path::PathBuf,
    Vec<u8>,
    Result<
        save_manager::transaction::MigrationOutcome,
        save_manager::transaction::TransactionError,
    >,
) {
    // Unique per process run. A monotonically increasing counter is used instead
    // of wall-clock time: concurrent test threads share this process ID, and on
    // Windows `SystemTime` resolution (~100ns) is coarse enough that two threads
    // can observe the same instant and therefore build the *same* directory,
    // which then contends for one migration journal lock.
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "gk-migration-runtime-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let target = dir.join("campaign.gksave");
    let manifest = SaveManifest {
        save_format_version: SaveFormatVersion::new(1, 0),
        save_id: "save.runtime.contract".into(),
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
    };
    let partitions = [SavePartition::new(
        SavePartitionId::Mod(ModNamespace::parse("org.example.mod").unwrap()),
        b"before",
    )];
    let world = Kernel::new(World::new(WorldId(703)))
        .unwrap()
        .snapshot()
        .unwrap();
    writer::save(&target, &manifest, &world, &partitions, &[]).unwrap();
    let source = std::fs::read(&target).unwrap();
    let inspected = save_manager::loader::inspect_save(&source).unwrap();
    let registry = MigrationRegistry::build(vec![planned().descriptor]).unwrap();
    let installed = BTreeSet::from(["org.example.mod".to_owned()]);
    let plan = build_migration_plan(
        &inspected,
        &registry,
        &MigrationTargets {
            world_schema_version: SaveFormatVersion::new(1, 0),
            mod_schema_versions: BTreeMap::from([(
                "org.example.mod".into(),
                SaveFormatVersion::new(2, 0),
            )]),
        },
        &installed,
    )
    .unwrap();
    let mut runner = WasmMigrationRunner::new(
        migration_engine().unwrap(),
        BTreeMap::from([("org.example.mod.migrate.1-2".into(), guest_bytes)]),
        32,
    )
    .unwrap();
    let mut references = ReferenceExtractorRegistry::default();
    references
        .register("org.example.mod", Box::new(EmptyReferenceExtractor))
        .unwrap();
    let mut validator =
        MigrationValidationPipeline::new(Box::new(KernelSnapshotValidator), references);
    let result = save_manager::transaction::migrate(
        &target,
        save_manager::transaction::MigrationRequest {
            inspected: &inspected,
            registry: &registry,
            plan: &plan,
            installed_mod_ids: &installed,
            last_saved_at: "2026-10-19T01:00:00Z",
        },
        &mut runner,
        &mut validator,
    );
    (target, source, result)
}

#[test]
fn migration_guest_runs_through_save_runner_and_identical_input_is_monitored() {
    let expected = MigrationOutput {
        world: None,
        mods: BTreeMap::from([("org.example.mod".into(), b"migrated".to_vec())]),
    };
    let encoded = serde_json::to_string(&expected).unwrap();
    let mut runner = runner(guest(&encoded));
    let input = MigrationInput {
        world: None,
        mods: BTreeMap::from([("org.example.mod".into(), b"old".to_vec())]),
    };
    let first = runner.run(&planned(), input.clone()).unwrap();
    let second = runner.run(&planned(), input).unwrap();
    assert_eq!(first, expected);
    assert_eq!(second, expected);
}

#[test]
fn guest_imports_are_rejected_before_execution() {
    let imported = wat::parse_str(
        r#"(module (import "wasi_snapshot_preview1" "clock_time_get"
        (func (param i32 i64 i32) (result i32))))"#,
    )
    .unwrap();
    let error = match WasmMigrationRunner::new(
        migration_engine().unwrap(),
        BTreeMap::from([("migrator".into(), imported)]),
        4,
    ) {
        Ok(_) => panic!("imports must be rejected"),
        Err(error) => error,
    };
    assert!(error.contains("must not import host capabilities"));
}

#[test]
fn malformed_guest_output_fails_closed() {
    let mut runner = runner(guest("not-json"));
    let error = runner
        .run(&planned(), MigrationInput::default())
        .unwrap_err();
    assert_eq!(error.code, "save.migration.output_invalid");
}

#[test]
fn migration_abi_json_matches_the_published_closed_schema() {
    let schema: serde_json::Value =
        serde_json::from_str(game_platform::migration_runtime::MIGRATION_ABI_V1_SCHEMA_JSON)
            .unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    let input = serde_json::to_value(MigrationInput {
        world: Some(vec![0, 255]),
        mods: BTreeMap::from([("org.example.mod".into(), b"input".to_vec())]),
    })
    .unwrap();
    let output = serde_json::to_value(MigrationOutput {
        world: None,
        mods: BTreeMap::from([("org.example.mod".into(), b"output".to_vec())]),
    })
    .unwrap();
    assert!(validator.is_valid(&input));
    assert!(validator.is_valid(&output));
    assert!(!validator.is_valid(&serde_json::json!({
        "world": null,
        "mods": {},
        "unexpected": true
    })));
}

#[test]
fn save_transaction_executes_wasm_migration_and_commits_verified_output() {
    let output = MigrationOutput {
        world: None,
        mods: BTreeMap::from([("org.example.mod".into(), b"after".to_vec())]),
    };
    let (target, source, result) =
        execute_transaction(guest(&serde_json::to_string(&output).unwrap()));
    let outcome = result.expect("Wasm-backed transaction commits");
    let committed = save_manager::loader::inspect_save_file(&target).unwrap();
    assert_eq!(committed.manifest().mods[0].schema_version, "2.0");
    assert_eq!(committed.mod_payload("org.example.mod").unwrap(), b"after");
    assert_ne!(std::fs::read(&target).unwrap(), source);
    let test_dir = target.parent().unwrap().to_path_buf();
    let _ = std::fs::remove_file(outcome.recovery_copy);
    std::fs::remove_dir_all(test_dir).unwrap();
}

#[test]
fn wasm_runner_failure_becomes_rollback_confirmed_platform_diagnostic() {
    let looping_guest = wat::parse_str(
        r#"(module
          (memory (export "memory") 3 3)
          (func (export "migrate") (param i32 i32 i32 i32) (result i32)
            (loop $again br $again)
            i32.const 0))"#,
    )
    .unwrap();
    let (target, source, result) = execute_transaction(looping_guest);
    let error = result.expect_err("exhausted migration must not commit");
    assert_eq!(error.code(), "save.migration.runner_failed");
    assert_eq!(std::fs::read(&target).unwrap(), source);
    save_manager::loader::inspect_save(&source).expect("original archive remains recoverable");
    let report = ModFaultReport::from_save_migration_error(
        "org.example.mod",
        &error,
        RollbackOutcome {
            executed: true,
            succeeded: Some(true),
            discarded_work: Some(true),
        },
    );
    assert_eq!(report.code, "platform.mod.migration_failed");
    assert_eq!(
        report.rollback.as_ref().and_then(|r| r.succeeded),
        Some(true)
    );
    validate_fault_report_value(&serde_json::to_value(report).unwrap()).unwrap();
    let test_dir = target.parent().unwrap().to_path_buf();
    std::fs::remove_dir_all(test_dir).unwrap();
}
