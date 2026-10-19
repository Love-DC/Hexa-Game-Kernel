//! Contract tests for `SAVE-005` — save loading and compatibility assessment
//! (`planning/34-P4-SAVE-005-存档加载与兼容性判定实施契约草案.md`, frozen
//! 2026-10-19 against baseline HEAD `06d4660`).
//!
//! Coverage map (contract §10, all 20 groups):
//! * verdict branches: Compatible / CompatibleWithMigration / Blocked carry
//!   the frozen field values; Corrupted is a type-level value only;
//! * priority short-circuit: the first failing step wins (steps 3 < 4 < 5);
//! * future format version: `inspect_save` ⇒ `LoadError::Integrity` with the
//!   `SAVE-001` stable code `save.unsupported_format_version`
//!   (`OWNER-DECISION-02`);
//! * system manifest load side: missing partition ⇒ `Missing`, malformed
//!   payload ⇒ `Invalid`;
//! * corruption: one tampered byte ⇒ `LoadError::Corrupted`, nothing decoded;
//! * dual-layer mapping: static literal alignment of spec §5.4's seven rows,
//!   five `PolicyFlag` literals, eleven reason codes, four verdict literals;
//!   `degraded` / `discouraged` / `mod_version_incompatible` zero-trigger
//!   assertions;
//! * three booleans: the frozen computation baselines (contract §8);
//! * Strict gate: migration ⇒ Blocked + {rejected} + policy_rejected; Normal
//!   passes through;
//! * repeatability: same inputs ⇒ field-by-field identical reports;
//! * I/O discipline: covered by `save_write_contract.rs`'s
//!   `save_io_is_confined_to_writer_and_loader` (DC-10 whitelist gains exactly
//!   `loader.rs`, read-only `std::fs::read`);
//! * env input errors: the four `CompatInputError` cases and their codes;
//! * Mod version is not a predicate (`B-1`);
//! * Mod kernel_api_requirement exact match (`B-2`);
//! * dependency `VersionReq` satisfaction (`B-1a`) and the two
//!   `InvalidVersionFormat` fields;
//! * `LoadError::Io{Read}` paths: missing file / directory / (Unix) locked
//!   file;
//! * `LoadError::Layout` path: malformed and truncated containers;
//! * invalid Manifest path: `LoadError::Integrity` with `save.invalid_manifest`
//!   (`OWNER-DECISION-03`);
//! * step-0 priority: Empty → Duplicate → InvalidFormat → Contradictory;
//! * platform one-way gate boundary;
//! * Strict pass-through for non-migration results.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use kernel_core::Tick;
use save_manager::compat::{
    CompatInputError, CompatReasonCode, CompatibilityEnv, CompatibilityReport,
    CompatibilityVerdict, InstalledModInfo, LoadingPolicy, MigrationHop, MigrationPath,
    ModDependencyRef, PolicyFlag, VersionField, assess_compatibility,
};
use save_manager::integrity;
use save_manager::loader::{
    InspectedSave, LoadError, LoadIoStage, inspect_save, inspect_save_file,
};
use save_manager::partition::{
    ModNamespace, PartitionErrorCode, PartitionPayload, SaveLayout, SavePartition, SavePartitionId,
};
use save_manager::writer::{self, SystemManifestErrorCode};
use save_manager::{
    GameDefinition, Integrity, SaveFormatVersion, SaveManifest, SaveManifestErrorCode,
    SaveModEntry, SimulationConfig,
};

/// The format-legal placeholder `root_hash` for the caller's Manifest: 64
/// lowercase hexadecimal zeros (same rule as the `SAVE-004` contract tests).
const PLACEHOLDER_ROOT: &str = "0000000000000000000000000000000000000000000000000000000000000000";

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn base_manifest() -> SaveManifest {
    SaveManifest {
        save_format_version: SaveFormatVersion::new(1, 0),
        save_id: "save005.contract".to_owned(),
        created_at: "2026-10-19T00:00:00Z".to_owned(),
        last_saved_at: "2026-10-19T00:00:00Z".to_owned(),
        platform_version: "1.0.0".to_owned(),
        game_definition: GameDefinition {
            id: "org.example.game".to_owned(),
            version: "1.0.0".to_owned(),
        },
        kernel_api_version: "1.0".to_owned(),
        world_schema_version: "1.0".to_owned(),
        logical_tick: Tick(7),
        mods: vec![SaveModEntry {
            id: "org.example.mod".to_owned(),
            version: "1.0.0".to_owned(),
            schema_version: "1.0".to_owned(),
        }],
        simulation_config: SimulationConfig::new(),
        integrity: Integrity {
            algorithm: "sha256".to_owned(),
            root_hash: PLACEHOLDER_ROOT.to_owned(),
        },
    }
}

fn base_mod_partitions() -> Vec<SavePartition> {
    vec![SavePartition::new(
        SavePartitionId::Mod(ModNamespace::parse("org.example.mod").unwrap()),
        b"mod payload v1".to_vec(),
    )]
}

/// Assembles a save container in memory, mirroring `writer::save`'s assembly
/// steps (`SAVE-004` contract §6 steps 3-6) without performing any file I/O:
/// Manifest/Integrity partitions are added by this helper; `body` carries the
/// World, Mod and (optional) Metadata partitions.
fn assemble_container(manifest: &SaveManifest, mut body: Vec<SavePartition>) -> Vec<u8> {
    let mut partitions = vec![SavePartition::new(
        SavePartitionId::Manifest,
        manifest.to_bytes(),
    )];
    partitions.append(&mut body);
    partitions.push(SavePartition::new(SavePartitionId::Integrity, Vec::new()));
    let initial = SaveLayout::new(partitions);

    let generated = integrity::generate(&initial).expect("integrity generates");
    let mut final_manifest = manifest.clone();
    final_manifest.integrity.root_hash = generated.root;
    let final_partitions: Vec<SavePartition> = initial
        .partitions
        .into_iter()
        .map(|mut partition| {
            match partition.id {
                SavePartitionId::Manifest => {
                    partition.payload = PartitionPayload::new(final_manifest.to_bytes());
                }
                SavePartitionId::Integrity => {
                    partition.payload = PartitionPayload::new(generated.checksums.clone());
                }
                _ => {}
            }
            partition
        })
        .collect();
    SaveLayout::new(final_partitions)
        .to_bytes()
        .expect("layout encodes")
}

fn build_save_bytes(
    manifest: &SaveManifest,
    world: &[u8],
    mod_partitions: &[SavePartition],
    system_ids: &[&str],
) -> Vec<u8> {
    let system_manifest = writer::encode_system_manifest(system_ids).expect("system ids encode");
    let mut body = vec![SavePartition::new(SavePartitionId::World, world.to_vec())];
    body.extend(mod_partitions.iter().cloned());
    body.push(SavePartition::new(
        SavePartitionId::Metadata,
        system_manifest,
    ));
    assemble_container(manifest, body)
}

/// Replaces the Manifest partition payload of an otherwise valid container
/// without recomputing any digest.
fn with_manifest_payload(container: &[u8], payload: Vec<u8>) -> Vec<u8> {
    let layout = SaveLayout::from_bytes(container).expect("container decodes");
    let mut partitions = layout.partitions;
    for partition in &mut partitions {
        if partition.id == SavePartitionId::Manifest {
            *partition = SavePartition::new(SavePartitionId::Manifest, payload.clone());
        }
    }
    SaveLayout::new(partitions)
        .to_bytes()
        .expect("layout encodes")
}

fn inspect_base() -> InspectedSave {
    inspect_save(&build_save_bytes(
        &base_manifest(),
        b"world snapshot",
        &base_mod_partitions(),
        &["org.game/alpha"],
    ))
    .expect("base save inspects")
}

fn installed_mod() -> InstalledModInfo {
    InstalledModInfo {
        id: "org.example.mod".to_owned(),
        version: "1.0.0".to_owned(),
        schema_version: SaveFormatVersion::new(1, 0),
        kernel_api_requirement: "1.0".to_owned(),
        dependencies: vec![],
    }
}

fn base_env() -> CompatibilityEnv {
    CompatibilityEnv {
        platform_version: "1.0.0".to_owned(),
        kernel_api_version: SaveFormatVersion::new(1, 0),
        installed_mods: vec![installed_mod()],
        migration: MigrationPath::NotRequired,
        policy: LoadingPolicy::Normal,
    }
}

fn migration_available() -> MigrationPath {
    MigrationPath::Available(vec![MigrationHop {
        from_version: SaveFormatVersion::new(1, 0),
        to_version: SaveFormatVersion::new(2, 0),
        migrator_id: "org.example.migrator".to_owned(),
    }])
}

/// A save whose Mod Schema differs from the installed one, with a declared
/// migration path.
fn migration_scenario(policy: LoadingPolicy) -> CompatibilityReport {
    let inspected = inspect_base();
    let mut env = base_env();
    env.installed_mods[0].schema_version = SaveFormatVersion::new(2, 0);
    env.migration = migration_available();
    env.policy = policy;
    assess_compatibility(&inspected, &env).expect("input is valid")
}

fn test_dir(tag: &str) -> PathBuf {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "gksave-save005-contract-{tag}-{}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("create test dir");
    dir
}

// ---------------------------------------------------------------------------
// Inspection accessors and the file entry point
// ---------------------------------------------------------------------------

#[test]
fn an_inspected_save_exposes_read_only_accessors() {
    let inspected = inspect_base();
    assert_eq!(inspected.manifest().save_id, "save005.contract");
    assert_eq!(inspected.system_ids().len(), 1);
    assert_eq!(inspected.system_ids()[0], "org.game/alpha");
    assert!(inspected.integrity_report().is_intact());
    assert_eq!(inspected.world_payload(), b"world snapshot");
    assert_eq!(
        inspected.mod_payload("org.example.mod"),
        Some(&b"mod payload v1"[..])
    );
    assert_eq!(inspected.mod_payload("org.example.absent"), None);
    assert_eq!(inspected.mod_payload("Not A Namespace"), None);
}

#[test]
fn inspect_save_file_reads_and_inspects() {
    let dir = test_dir("roundtrip");
    let target = dir.join("world.gksave");
    std::fs::write(
        &target,
        build_save_bytes(&base_manifest(), b"world", &base_mod_partitions(), &[]),
    )
    .expect("write fixture");
    let inspected = inspect_save_file(&target).expect("file inspects");
    assert_eq!(inspected.manifest().save_id, "save005.contract");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Verdict branches (group 判定分支)
// ---------------------------------------------------------------------------

#[test]
fn a_fully_matching_save_is_compatible() {
    let inspected = inspect_base();
    let report = assess_compatibility(&inspected, &base_env()).expect("input is valid");
    assert_eq!(report.verdict, CompatibilityVerdict::Compatible);
    assert_eq!(report.reason_code, CompatReasonCode::Compatible);
    assert_eq!(report.reason_code.as_str(), "save.compat.compatible");
    assert!(report.flags.is_empty());
    assert!(report.affected_mods.is_empty());
    assert_eq!(report.save_version, None);
    assert_eq!(report.current_version, None);
    assert_eq!(report.failed_condition, "");
    assert_eq!(report.available_actions, ["直接加载"]);
    assert!(!report.needs_backup);
    assert!(!report.may_lose_data);
    assert!(report.may_auto_continue);
}

#[test]
fn a_schema_difference_with_a_migration_path_is_compatible_with_migration() {
    let report = migration_scenario(LoadingPolicy::Normal);
    assert_eq!(
        report.verdict,
        CompatibilityVerdict::CompatibleWithMigration
    );
    assert_eq!(report.reason_code, CompatReasonCode::MigrationAvailable);
    assert_eq!(
        report.reason_code.as_str(),
        "save.compat.migration_available"
    );
    assert_eq!(
        report.flags,
        BTreeSet::from([PolicyFlag::RequiresUserConfirmation])
    );
    assert!(report.affected_mods.is_empty());
    assert_eq!(report.save_version.as_deref(), Some("1.0"));
    assert_eq!(report.current_version.as_deref(), Some("2.0"));
    assert_eq!(
        report.failed_condition,
        "存在 Schema 差异，已找到可用迁移路径"
    );
    assert_eq!(
        report.available_actions,
        ["备份存档", "确认并执行迁移", "放弃加载"]
    );
    assert!(report.needs_backup);
    assert!(report.may_lose_data);
    assert!(
        !report.may_auto_continue,
        "requires_user_confirmation forbids auto-continue (contract §9)"
    );
}

#[test]
fn a_world_mod_schema_difference_also_consults_migration() {
    // Contract §4 step 6: the save's `world_schema_version` is compared with
    // every save Mod's `schema_version`; a difference consults migration even
    // when the installed Mod matches its save record.
    let mut manifest = base_manifest();
    manifest.world_schema_version = "2.0".to_owned();
    let inspected = inspect_save(&build_save_bytes(
        &manifest,
        b"world",
        &base_mod_partitions(),
        &[],
    ))
    .expect("save inspects");

    let mut env = base_env();
    env.migration = MigrationPath::Unavailable;
    let report = assess_compatibility(&inspected, &env).expect("input is valid");
    assert_eq!(report.verdict, CompatibilityVerdict::Blocked);
    assert_eq!(
        report.reason_code,
        CompatReasonCode::SchemaMismatchNoMigration
    );
    assert_eq!(
        report.reason_code.as_str(),
        "save.compat.schema_mismatch_no_migration"
    );
    assert!(report.flags.is_empty());
    assert_eq!(report.save_version.as_deref(), Some("2.0"));
    assert_eq!(report.current_version.as_deref(), Some("1.0"));
    assert_eq!(report.failed_condition, "存在 Schema 差异且无可用迁移路径");
    assert_eq!(report.available_actions, ["放弃加载", "提供迁移器后重试"]);

    let mut env = base_env();
    env.migration = migration_available();
    let report = assess_compatibility(&inspected, &env).expect("input is valid");
    assert_eq!(
        report.verdict,
        CompatibilityVerdict::CompatibleWithMigration
    );
    assert_eq!(report.reason_code, CompatReasonCode::MigrationAvailable);
}

#[test]
fn a_superfluous_migration_declaration_is_not_an_error() {
    // N-2 (contract §12): `Available` / `Unavailable` with no Schema
    // difference is simply not consumed.
    let inspected = inspect_base();
    for migration in [migration_available(), MigrationPath::Unavailable] {
        let mut env = base_env();
        env.migration = migration;
        let report = assess_compatibility(&inspected, &env).expect("input is valid");
        assert_eq!(report.verdict, CompatibilityVerdict::Compatible);
    }
}

#[test]
fn a_missing_mod_is_blocked_with_missing_dependency() {
    let dir = test_dir("missing-mod-preserves-save");
    let target = dir.join("confirmed.gksave");
    let source = build_save_bytes(
        &base_manifest(),
        b"world snapshot",
        &base_mod_partitions(),
        &["org.game/alpha"],
    );
    std::fs::write(&target, &source).unwrap();
    let inspected = inspect_save_file(&target).unwrap();
    let mut env = base_env();
    env.installed_mods.clear();
    let report = assess_compatibility(&inspected, &env).expect("input is valid");
    assert_eq!(report.verdict, CompatibilityVerdict::Blocked);
    assert_eq!(report.reason_code, CompatReasonCode::ModMissing);
    assert_eq!(report.reason_code.as_str(), "save.compat.mod_missing");
    assert_eq!(
        report.flags,
        BTreeSet::from([PolicyFlag::MissingDependency])
    );
    assert_eq!(report.affected_mods, ["org.example.mod"]);
    assert_eq!(report.save_version, None);
    assert_eq!(report.current_version, None);
    assert_eq!(
        report.failed_condition,
        "存档所需 Mod 缺失：org.example.mod"
    );
    assert_eq!(report.available_actions, ["安装缺失的 Mod", "放弃加载"]);
    assert!(!report.needs_backup);
    assert!(!report.may_lose_data);
    assert!(!report.may_auto_continue);
    assert_eq!(std::fs::read(&target).unwrap(), source);
    assert!(inspect_save_file(&target).is_ok());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn corrupted_verdict_is_a_type_level_value_only() {
    // Decision 3 (planning/33 §4.5): the single corruption exit is
    // `LoadError::Corrupted`; the assessor never produces a `Corrupted`
    // verdict. The value is retained for contract completeness and pins its
    // wire literal.
    assert_eq!(
        serde_json::to_value(CompatibilityVerdict::Corrupted).expect("serializes"),
        "corrupted"
    );
}

// ---------------------------------------------------------------------------
// Priority short-circuit (group 优先级短路)
// ---------------------------------------------------------------------------

#[test]
fn the_first_failing_step_wins() {
    // Step 3 (kernel API) precedes step 4 (Mod set): both fail ⇒ only the
    // kernel reason is reported, with no Mod-level flags.
    let inspected = inspect_base();
    let mut env = base_env();
    env.kernel_api_version = SaveFormatVersion::new(2, 0);
    env.installed_mods.clear();
    let report = assess_compatibility(&inspected, &env).expect("input is valid");
    assert_eq!(report.verdict, CompatibilityVerdict::Blocked);
    assert_eq!(report.reason_code, CompatReasonCode::KernelApiIncompatible);
    assert_eq!(
        report.reason_code.as_str(),
        "save.compat.kernel_api_incompatible"
    );
    assert!(report.flags.is_empty(), "step-3 failures carry no flags");
    assert!(report.affected_mods.is_empty());
    assert_eq!(report.save_version.as_deref(), Some("1.0"));
    assert_eq!(report.current_version.as_deref(), Some("2.0"));
    assert_eq!(
        report.failed_condition,
        "存档内核 API 版本 1.0 与当前 2.0 不一致"
    );
    assert_eq!(
        report.available_actions,
        ["切换到与存档匹配的内核版本", "放弃加载"]
    );

    // Within step 3 the kernel check precedes the platform gate.
    let mut env = base_env();
    env.kernel_api_version = SaveFormatVersion::new(2, 0);
    env.platform_version = "0.9.9".to_owned();
    let report = assess_compatibility(&inspected, &env).expect("input is valid");
    assert_eq!(report.reason_code, CompatReasonCode::KernelApiIncompatible);

    // Step 4 precedes step 5: a missing Mod beats a mismatched
    // kernel_api_requirement.
    let mut env = base_env();
    env.installed_mods[0].id = "org.example.other".to_owned();
    env.installed_mods[0].kernel_api_requirement = "9.9".to_owned();
    let report = assess_compatibility(&inspected, &env).expect("input is valid");
    assert_eq!(report.reason_code, CompatReasonCode::ModMissing);

    // Step 4 precedes steps 6/7: a missing Mod beats a Schema difference.
    let mut manifest = base_manifest();
    manifest.world_schema_version = "2.0".to_owned();
    let inspected = inspect_save(&build_save_bytes(
        &manifest,
        b"w",
        &base_mod_partitions(),
        &[],
    ))
    .expect("save inspects");
    let mut env = base_env();
    env.installed_mods.clear();
    env.migration = migration_available();
    let report = assess_compatibility(&inspected, &env).expect("input is valid");
    assert_eq!(report.reason_code, CompatReasonCode::ModMissing);
}

// ---------------------------------------------------------------------------
// LoadError paths (groups 未来版本 / SM 加载侧 / Corrupted / Layout / Manifest / Io)
// ---------------------------------------------------------------------------

#[test]
fn a_future_format_version_is_rejected_by_inspection() {
    // OWNER-DECISION-02: `save_format_version = 2.0` is rejected inside
    // `verify`'s canonical Manifest parse — `LoadError::Integrity` carrying
    // the `SAVE-001` stable code `save.unsupported_format_version`. The
    // assessor's step-2 gate is a defensive layer the public API cannot reach.
    let mut tampered = base_manifest().to_value();
    tampered["save_format_version"] = "2.0".into();
    let container = build_save_bytes(&base_manifest(), b"world", &base_mod_partitions(), &[]);
    let container = with_manifest_payload(
        &container,
        serde_json::to_vec(&tampered).expect("value serializes"),
    );

    let error = inspect_save(&container).expect_err("future format rejected");
    match &error {
        LoadError::Integrity(inner) => {
            assert_eq!(inner.code_str(), "save.unsupported_format_version");
            assert!(inner.manifest_error().is_some());
        }
        other => panic!("expected LoadError::Integrity, got {other:?}"),
    }
}

#[test]
fn a_missing_metadata_partition_is_system_manifest_missing() {
    let body = vec![SavePartition::new(
        SavePartitionId::World,
        b"world".to_vec(),
    )];
    let container = assemble_container(&base_manifest(), body);
    let error = inspect_save(&container).expect_err("missing system manifest");
    match &error {
        LoadError::SystemManifest(inner) => {
            assert_eq!(inner.code(), SystemManifestErrorCode::Missing);
            assert_eq!(inner.code_str(), "save.system_manifest_missing");
        }
        other => panic!("expected LoadError::SystemManifest, got {other:?}"),
    }
}

#[test]
fn a_malformed_metadata_payload_is_system_manifest_invalid() {
    let body = vec![
        SavePartition::new(SavePartitionId::World, b"world".to_vec()),
        SavePartition::new(SavePartitionId::Metadata, b"not an SM payload".to_vec()),
    ];
    let container = assemble_container(&base_manifest(), body);
    let error = inspect_save(&container).expect_err("invalid system manifest");
    match &error {
        LoadError::SystemManifest(inner) => {
            assert_eq!(inner.code(), SystemManifestErrorCode::Invalid);
            assert_eq!(inner.code_str(), "save.system_manifest_invalid");
        }
        other => panic!("expected LoadError::SystemManifest, got {other:?}"),
    }
}

#[test]
fn a_single_tampered_byte_is_corruption_and_exposes_nothing() {
    let container = build_save_bytes(
        &base_manifest(),
        b"world",
        &base_mod_partitions(),
        &["org.game/alpha"],
    );
    let layout = SaveLayout::from_bytes(&container).expect("container decodes");
    let mut partitions = layout.partitions;
    for partition in &mut partitions {
        if partition.id == SavePartitionId::World {
            let mut bytes = partition.payload.as_bytes().to_vec();
            bytes[0] ^= 0xFF;
            *partition = SavePartition::new(SavePartitionId::World, bytes);
        }
    }
    let tampered = SaveLayout::new(partitions)
        .to_bytes()
        .expect("layout encodes");

    // Construction-is-evidence: an `Err` makes the Manifest and every payload
    // unreachable at the type level (no `Ok`, no `InspectedSave`).
    let error = inspect_save(&tampered).expect_err("tampered save is corrupt");
    match &error {
        LoadError::Corrupted(report) => {
            assert!(!report.is_intact());
            assert!(report.corrupted_partitions().count() > 0);
        }
        other => panic!("expected LoadError::Corrupted, got {other:?}"),
    }
}

#[test]
fn a_malformed_container_is_a_layout_error() {
    let error = inspect_save(b"not a layout").expect_err("garbage bytes");
    match &error {
        LoadError::Layout(inner) => {
            assert_eq!(inner.code(), PartitionErrorCode::InvalidLayout);
            assert_eq!(inner.code_str(), "save.invalid_layout");
        }
        other => panic!("expected LoadError::Layout, got {other:?}"),
    }

    let container = build_save_bytes(&base_manifest(), b"world", &base_mod_partitions(), &[]);
    let truncated = &container[..container.len() - 3];
    let error = inspect_save(truncated).expect_err("truncated container");
    assert!(
        matches!(error, LoadError::Layout(_)),
        "expected LoadError::Layout, got {error:?}"
    );
}

#[test]
fn an_invalid_manifest_is_an_integrity_error_with_the_save001_code() {
    // OWNER-DECISION-03: an unparsable Manifest is intercepted inside
    // `verify`'s canonical parse, so the public path reports
    // `LoadError::Integrity` carrying `save.invalid_manifest`.
    // `LoadError::Manifest` stays structurally unreachable.
    let container = build_save_bytes(&base_manifest(), b"world", &base_mod_partitions(), &[]);
    let container = with_manifest_payload(&container, b"{ not valid json".to_vec());

    let error = inspect_save(&container).expect_err("invalid manifest rejected");
    match &error {
        LoadError::Integrity(inner) => {
            assert_eq!(inner.code_str(), "save.invalid_manifest");
            let manifest_error = inner.manifest_error().expect("SAVE-001 error propagated");
            assert_eq!(
                manifest_error.code(),
                SaveManifestErrorCode::InvalidManifest
            );
        }
        other => panic!("expected LoadError::Integrity, got {other:?}"),
    }
}

#[test]
fn a_missing_file_is_an_io_read_error() {
    let dir = test_dir("io-missing");
    let target = dir.join("no-such-save.gksave");
    let error = inspect_save_file(&target).expect_err("missing file");
    match &error {
        LoadError::Io {
            stage,
            path,
            source,
        } => {
            assert_eq!(*stage, LoadIoStage::Read);
            assert_eq!(path, &target);
            assert_eq!(source.kind(), std::io::ErrorKind::NotFound);
        }
        other => panic!("expected LoadError::Io, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_directory_target_is_an_io_read_error() {
    let dir = test_dir("io-dir");
    let error = inspect_save_file(&dir).expect_err("a directory cannot be read as a save");
    assert!(
        matches!(
            error,
            LoadError::Io {
                stage: LoadIoStage::Read,
                ..
            }
        ),
        "expected LoadError::Io{{Read}}, got {error:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Reading a permission-locked file fails with `LoadError::Io{Read}`.
///
/// Root and other privileged environments bypass file permission bits, so the
/// test first probes whether the locked file actually rejects reads. When it
/// does not, the permission-denial scenario cannot be simulated here: the
/// test reports itself as skipped and returns without running the failure
/// assertions (mirrors the `SAVE-004` E-9b probe).
#[cfg(unix)]
#[test]
fn an_unreadable_file_is_an_io_read_error() {
    use std::os::unix::fs::PermissionsExt;

    let dir = test_dir("io-denied");
    let target = dir.join("world.gksave");
    std::fs::write(
        &target,
        build_save_bytes(&base_manifest(), b"world", &base_mod_partitions(), &[]),
    )
    .expect("write fixture");

    let mut locked = std::fs::metadata(&target).expect("metadata").permissions();
    locked.set_mode(0o000);
    std::fs::set_permissions(&target, locked).expect("lock file");

    // Runtime capability check: elevated environments ignore permission bits.
    let read_blocked = match std::fs::read(&target) {
        Ok(_) => false,
        Err(error) => {
            assert_eq!(
                error.kind(),
                std::io::ErrorKind::PermissionDenied,
                "probe read failed with a non-permission error: {error}"
            );
            true
        }
    };

    if !read_blocked {
        eprintln!(
            "SKIP: running with elevated privileges; a 0o000 file can still be \
             read, so the permission-denial scenario is not applicable"
        );
        let mut unlocked = std::fs::metadata(&target).expect("metadata").permissions();
        unlocked.set_mode(0o644);
        std::fs::set_permissions(&target, unlocked).expect("unlock file");
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }

    let error = inspect_save_file(&target).expect_err("unreadable file");
    assert!(
        matches!(
            error,
            LoadError::Io {
                stage: LoadIoStage::Read,
                ..
            }
        ),
        "expected LoadError::Io{{Read}}, got {error:?}"
    );

    let mut unlocked = std::fs::metadata(&target).expect("metadata").permissions();
    unlocked.set_mode(0o644);
    std::fs::set_permissions(&target, unlocked).expect("unlock file");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Dual-layer mapping and zero-trigger pins (group 双层映射)
// ---------------------------------------------------------------------------

/// One report per publicly reachable reason code — everything except the
/// zero-trigger `mod_version_incompatible` (`B-1`) and the defensive
/// `unsupported_format_version` (`OWNER-DECISION-02`).
fn reachable_reports() -> Vec<CompatibilityReport> {
    let inspected = inspect_base();
    let mut reports = Vec::new();

    reports.push(assess_compatibility(&inspected, &base_env()).expect("valid"));

    for policy in [LoadingPolicy::Normal, LoadingPolicy::Strict] {
        let mut env = base_env();
        env.installed_mods[0].schema_version = SaveFormatVersion::new(2, 0);
        env.migration = migration_available();
        env.policy = policy;
        reports.push(assess_compatibility(&inspected, &env).expect("valid"));
    }

    let mut env = base_env();
    env.installed_mods[0].schema_version = SaveFormatVersion::new(2, 0);
    env.migration = MigrationPath::Unavailable;
    reports.push(assess_compatibility(&inspected, &env).expect("valid"));

    let mut env = base_env();
    env.installed_mods.clear();
    reports.push(assess_compatibility(&inspected, &env).expect("valid"));

    let mut env = base_env();
    env.kernel_api_version = SaveFormatVersion::new(2, 0);
    reports.push(assess_compatibility(&inspected, &env).expect("valid"));

    let mut env = base_env();
    env.platform_version = "0.9.9".to_owned();
    reports.push(assess_compatibility(&inspected, &env).expect("valid"));

    let mut env = base_env();
    env.installed_mods[0].dependencies = vec![ModDependencyRef {
        id: "org.example.absent".to_owned(),
        version_requirement: "^1.0".to_owned(),
    }];
    reports.push(assess_compatibility(&inspected, &env).expect("valid"));

    let mut env = base_env();
    env.installed_mods[0].kernel_api_requirement = "9.9".to_owned();
    reports.push(assess_compatibility(&inspected, &env).expect("valid"));

    reports
}

#[test]
fn dual_layer_mapping_literals_are_frozen() {
    // The five `PolicyFlag` wire literals (spec §5.4, frozen by
    // DESIGN-DECISION-05 amendment A).
    let flag_literals = [
        (
            PolicyFlag::RequiresUserConfirmation,
            "requires_user_confirmation",
        ),
        (PolicyFlag::MissingDependency, "missing_dependency"),
        (PolicyFlag::Degraded, "degraded"),
        (PolicyFlag::Discouraged, "discouraged"),
        (PolicyFlag::Rejected, "rejected"),
    ];
    for (flag, literal) in flag_literals {
        assert_eq!(serde_json::to_value(flag).expect("serializes"), literal);
        assert_eq!(
            serde_json::from_value::<PolicyFlag>(literal.into()).expect("deserializes"),
            flag
        );
    }

    // The four `CompatibilityVerdict` wire literals (spec §5.1).
    let verdict_literals = [
        (CompatibilityVerdict::Compatible, "compatible"),
        (
            CompatibilityVerdict::CompatibleWithMigration,
            "compatible_with_migration",
        ),
        (CompatibilityVerdict::Blocked, "blocked"),
        (CompatibilityVerdict::Corrupted, "corrupted"),
    ];
    for (verdict, literal) in verdict_literals {
        assert_eq!(serde_json::to_value(verdict).expect("serializes"), literal);
        assert_eq!(
            serde_json::from_value::<CompatibilityVerdict>(literal.into()).expect("deserializes"),
            verdict
        );
    }

    // The eleven frozen reason-code literals; the serde wire form equals
    // `as_str`.
    let reason_literals = [
        (CompatReasonCode::Compatible, "save.compat.compatible"),
        (
            CompatReasonCode::MigrationAvailable,
            "save.compat.migration_available",
        ),
        (
            CompatReasonCode::UnsupportedFormatVersion,
            "save.compat.unsupported_format_version",
        ),
        (
            CompatReasonCode::KernelApiIncompatible,
            "save.compat.kernel_api_incompatible",
        ),
        (
            CompatReasonCode::PlatformIncompatible,
            "save.compat.platform_incompatible",
        ),
        (CompatReasonCode::ModMissing, "save.compat.mod_missing"),
        (
            CompatReasonCode::ModVersionIncompatible,
            "save.compat.mod_version_incompatible",
        ),
        (
            CompatReasonCode::ModDependencyUnsatisfied,
            "save.compat.mod_dependency_unsatisfied",
        ),
        (
            CompatReasonCode::ModApiIncompatible,
            "save.compat.mod_api_incompatible",
        ),
        (
            CompatReasonCode::SchemaMismatchNoMigration,
            "save.compat.schema_mismatch_no_migration",
        ),
        (
            CompatReasonCode::PolicyRejected,
            "save.compat.policy_rejected",
        ),
    ];
    assert_eq!(CompatReasonCode::ALL.len(), reason_literals.len());
    for (code, literal) in reason_literals {
        assert_eq!(code.as_str(), literal);
        assert_eq!(serde_json::to_value(code).expect("serializes"), literal);
        assert_eq!(
            serde_json::from_value::<CompatReasonCode>(literal.into()).expect("deserializes"),
            code
        );
    }

    // Spec §5.4's seven mapping rows, pinned verbatim — including the rows
    // this stage cannot produce at runtime (N-1).
    let mapping_rows: [(&str, &str, &[&str]); 7] = [
        ("可直接加载", "compatible", &[]),
        ("需要迁移", "compatible_with_migration", &[]),
        (
            "需要用户确认",
            "compatible_with_migration",
            &["requires_user_confirmation"],
        ),
        ("需要补充依赖", "blocked", &["missing_dependency"]),
        ("存在功能降级", "compatible", &["degraded"]),
        ("不建议加载", "blocked", &["discouraged"]),
        ("必须拒绝加载", "blocked", &["rejected"]),
    ];
    let verdict_wire: Vec<&str> = verdict_literals
        .iter()
        .map(|(_, literal)| *literal)
        .collect();
    let flag_wire: Vec<&str> = flag_literals.iter().map(|(_, literal)| *literal).collect();
    for (strategy_level, machine_level, flags) in mapping_rows {
        assert!(!strategy_level.is_empty());
        assert!(
            verdict_wire.contains(&machine_level),
            "unknown machine literal `{machine_level}`"
        );
        for flag in flags {
            assert!(flag_wire.contains(flag), "unknown flag literal `{flag}`");
        }
    }
}

#[test]
fn zero_trigger_values_never_appear_in_reachable_reports() {
    let reports = reachable_reports();
    let distinct: BTreeSet<&str> = reports
        .iter()
        .map(|report| report.reason_code.as_str())
        .collect();
    assert_eq!(
        distinct.len(),
        reports.len(),
        "each scenario must cover a distinct reachable reason code"
    );
    for report in &reports {
        assert!(
            !report.flags.contains(&PolicyFlag::Degraded),
            "degraded is zero-trigger (amendment b): {report:?}"
        );
        assert!(
            !report.flags.contains(&PolicyFlag::Discouraged),
            "discouraged is zero-trigger (amendment b): {report:?}"
        );
        assert_ne!(
            report.reason_code,
            CompatReasonCode::ModVersionIncompatible,
            "mod_version_incompatible is zero-trigger (B-1): {report:?}"
        );
        assert_ne!(
            report.reason_code,
            CompatReasonCode::UnsupportedFormatVersion,
            "unsupported_format_version is a defensive layer unreachable via the public API"
        );
    }
}

// ---------------------------------------------------------------------------
// Three booleans and the Strict gate (groups 三布尔 / Strict 闸门 / Strict 透传)
// ---------------------------------------------------------------------------

#[test]
fn the_three_booleans_follow_the_frozen_baselines() {
    // Compatible ⇒ (false, false, true).
    let report = assess_compatibility(&inspect_base(), &base_env()).expect("valid");
    assert_eq!(
        (
            report.needs_backup,
            report.may_lose_data,
            report.may_auto_continue
        ),
        (false, false, true)
    );

    // CompatibleWithMigration under Normal ⇒ (true, true, false).
    let report = migration_scenario(LoadingPolicy::Normal);
    assert_eq!(
        (
            report.needs_backup,
            report.may_lose_data,
            report.may_auto_continue
        ),
        (true, true, false)
    );

    // Strict rewrite ⇒ the final verdict is Blocked: `may_auto_continue` is
    // computed from the final result (false); `needs_backup` / `may_lose_data`
    // keep the pre-rewrite pipeline values (true, true).
    let report = migration_scenario(LoadingPolicy::Strict);
    assert_eq!(report.verdict, CompatibilityVerdict::Blocked);
    assert_eq!(
        (
            report.needs_backup,
            report.may_lose_data,
            report.may_auto_continue
        ),
        (true, true, false)
    );

    // Blocked for a non-migration cause ⇒ (false, false, false).
    let inspected = inspect_base();
    let mut env = base_env();
    env.installed_mods.clear();
    let report = assess_compatibility(&inspected, &env).expect("valid");
    assert_eq!(
        (
            report.needs_backup,
            report.may_lose_data,
            report.may_auto_continue
        ),
        (false, false, false)
    );
}

#[test]
fn the_strict_gate_rewrites_migration_to_policy_rejected() {
    let report = migration_scenario(LoadingPolicy::Strict);
    assert_eq!(report.verdict, CompatibilityVerdict::Blocked);
    assert_eq!(report.reason_code, CompatReasonCode::PolicyRejected);
    assert_eq!(report.reason_code.as_str(), "save.compat.policy_rejected");
    assert_eq!(
        report.flags,
        BTreeSet::from([PolicyFlag::Rejected]),
        "B-4: flags := exactly {{rejected}}"
    );
    assert!(report.affected_mods.is_empty());
    assert_eq!(report.save_version, None);
    assert_eq!(report.current_version, None);
    assert_eq!(report.failed_condition, "迁移路径存在但被 Strict 策略拒绝");
    assert_eq!(
        report.available_actions,
        ["以 Normal 策略重新评估并确认迁移", "放弃加载"]
    );
    assert!(!report.may_auto_continue);

    // The same input under Normal passes through unchanged.
    let normal = migration_scenario(LoadingPolicy::Normal);
    assert_eq!(
        normal.verdict,
        CompatibilityVerdict::CompatibleWithMigration
    );
    assert_eq!(normal.reason_code, CompatReasonCode::MigrationAvailable);
    assert_eq!(
        normal.flags,
        BTreeSet::from([PolicyFlag::RequiresUserConfirmation])
    );
}

#[test]
fn strict_policy_passes_non_migration_results_through() {
    // Strict + Compatible ⇒ unchanged.
    let inspected = inspect_base();
    let mut env = base_env();
    env.policy = LoadingPolicy::Strict;
    let report = assess_compatibility(&inspected, &env).expect("valid");
    assert_eq!(report.verdict, CompatibilityVerdict::Compatible);
    assert_eq!(report.reason_code, CompatReasonCode::Compatible);
    assert!(report.flags.is_empty());
    assert!(report.may_auto_continue);

    // Strict + Blocked (non-migration cause) ⇒ unchanged, not a blanket
    // rejection: the reason and flags stay as the pipeline produced them.
    let mut env = base_env();
    env.policy = LoadingPolicy::Strict;
    env.installed_mods.clear();
    let report = assess_compatibility(&inspected, &env).expect("valid");
    assert_eq!(report.verdict, CompatibilityVerdict::Blocked);
    assert_eq!(report.reason_code, CompatReasonCode::ModMissing);
    assert_eq!(
        report.flags,
        BTreeSet::from([PolicyFlag::MissingDependency])
    );
}

// ---------------------------------------------------------------------------
// Repeatability (group 可重复性)
// ---------------------------------------------------------------------------

#[test]
fn assessment_is_repeatable() {
    let inspected = inspect_base();
    let mut env = base_env();
    env.installed_mods[0].schema_version = SaveFormatVersion::new(2, 0);
    env.migration = migration_available();
    let first = assess_compatibility(&inspected, &env).expect("valid");
    let second = assess_compatibility(&inspected, &env).expect("valid");
    assert_eq!(
        first, second,
        "same inputs must yield a field-by-field identical report"
    );
}

// ---------------------------------------------------------------------------
// Structured input errors (groups Env 输入错误 / 步 0 优先级)
// ---------------------------------------------------------------------------

#[test]
fn an_empty_mod_id_is_a_structured_input_error() {
    let inspected = inspect_base();
    let mut env = base_env();
    env.installed_mods[0].id = String::new();
    let error = assess_compatibility(&inspected, &env).expect_err("empty id rejected");
    assert_eq!(error, CompatInputError::EmptyModId);
    assert_eq!(error.code_str(), "save.compat.input.empty_mod_id");
}

#[test]
fn a_duplicate_mod_id_is_a_structured_input_error() {
    let inspected = inspect_base();
    let mut env = base_env();
    let duplicate = env.installed_mods[0].clone();
    env.installed_mods.push(duplicate);
    let error = assess_compatibility(&inspected, &env).expect_err("duplicate id rejected");
    assert_eq!(
        error,
        CompatInputError::DuplicateModId {
            mod_id: "org.example.mod".to_owned()
        }
    );
    assert_eq!(error.code_str(), "save.compat.input.duplicate_mod_id");
}

#[test]
fn an_invalid_platform_version_is_a_structured_input_error() {
    let inspected = inspect_base();
    let mut env = base_env();
    env.platform_version = "1.0".to_owned(); // not a full SemVer triple
    let error = assess_compatibility(&inspected, &env).expect_err("invalid version rejected");
    assert_eq!(
        error,
        CompatInputError::InvalidVersionFormat {
            field: VersionField::PlatformVersion,
            value: "1.0".to_owned()
        }
    );
    assert_eq!(error.code_str(), "save.compat.input.invalid_version_format");
}

#[test]
fn not_required_with_a_schema_difference_is_a_structured_input_error() {
    let inspected = inspect_base();
    let mut env = base_env();
    env.installed_mods[0].schema_version = SaveFormatVersion::new(2, 0);
    env.migration = MigrationPath::NotRequired; // contradicts the difference
    let error = assess_compatibility(&inspected, &env).expect_err("contradiction rejected");
    assert_eq!(error, CompatInputError::ContradictoryMigrationInput);
    assert_eq!(
        error.code_str(),
        "save.compat.input.contradictory_migration_input"
    );
}

#[test]
fn input_errors_are_reported_in_the_frozen_order() {
    let inspected = inspect_base();

    // Empty beats Duplicate: two empty (hence also duplicated) IDs.
    let mut env = base_env();
    env.installed_mods[0].id = String::new();
    let duplicate = env.installed_mods[0].clone();
    env.installed_mods.push(duplicate);
    let error = assess_compatibility(&inspected, &env).expect_err("rejected");
    assert_eq!(error, CompatInputError::EmptyModId);

    // Duplicate beats InvalidVersionFormat.
    let mut env = base_env();
    let duplicate = env.installed_mods[0].clone();
    env.installed_mods.push(duplicate);
    env.platform_version = "not-semver".to_owned();
    let error = assess_compatibility(&inspected, &env).expect_err("rejected");
    assert_eq!(
        error,
        CompatInputError::DuplicateModId {
            mod_id: "org.example.mod".to_owned()
        }
    );

    // InvalidVersionFormat beats ContradictoryMigrationInput.
    let mut env = base_env();
    env.installed_mods[0].version = "1.0".to_owned(); // invalid
    env.installed_mods[0].schema_version = SaveFormatVersion::new(2, 0); // differs
    env.migration = MigrationPath::NotRequired; // contradicts the difference
    let error = assess_compatibility(&inspected, &env).expect_err("rejected");
    assert_eq!(
        error,
        CompatInputError::InvalidVersionFormat {
            field: VersionField::InstalledModVersion,
            value: "1.0".to_owned()
        }
    );
}

// ---------------------------------------------------------------------------
// Version predicates (groups Mod 版本 / Mod API / 依赖满足 / platform_version 边界)
// ---------------------------------------------------------------------------

#[test]
fn a_mod_version_difference_alone_stays_compatible() {
    // B-1: a Mod's own `version` is not a judgement predicate — a version
    // difference with equal Schema versions stays Compatible, and the
    // zero-trigger `mod_version_incompatible` code never appears.
    let inspected = inspect_base();
    let mut env = base_env();
    env.installed_mods[0].version = "2.3.4".to_owned();
    let report = assess_compatibility(&inspected, &env).expect("valid");
    assert_eq!(report.verdict, CompatibilityVerdict::Compatible);
    assert_eq!(report.reason_code, CompatReasonCode::Compatible);
    assert_ne!(report.reason_code, CompatReasonCode::ModVersionIncompatible);
}

#[test]
fn a_mod_kernel_api_requirement_must_match_exactly() {
    // B-2: the positive case ("1.0" == 1.0) is covered by the Compatible
    // baseline; here the mismatch is Blocked with no flags.
    let inspected = inspect_base();
    let mut env = base_env();
    env.installed_mods[0].kernel_api_requirement = "2.0".to_owned();
    let report = assess_compatibility(&inspected, &env).expect("valid");
    assert_eq!(report.verdict, CompatibilityVerdict::Blocked);
    assert_eq!(report.reason_code, CompatReasonCode::ModApiIncompatible);
    assert_eq!(
        report.reason_code.as_str(),
        "save.compat.mod_api_incompatible"
    );
    assert!(report.flags.is_empty());
    assert_eq!(report.affected_mods, ["org.example.mod"]);
    assert_eq!(
        report.failed_condition,
        "Mod 的内核 API 要求与当前内核不一致：org.example.mod"
    );
    assert_eq!(
        report.available_actions,
        ["更换与当前内核匹配的 Mod 版本", "放弃加载"]
    );
}

fn env_with_dependency() -> CompatibilityEnv {
    let mut env = base_env();
    env.installed_mods[0].dependencies = vec![ModDependencyRef {
        id: "org.example.lib".to_owned(),
        version_requirement: "^1.0".to_owned(),
    }];
    env.installed_mods.push(InstalledModInfo {
        id: "org.example.lib".to_owned(),
        version: "1.2.0".to_owned(),
        schema_version: SaveFormatVersion::new(1, 0),
        kernel_api_requirement: "1.0".to_owned(),
        dependencies: vec![],
    });
    env
}

#[test]
fn a_satisfied_dependency_version_req_passes() {
    // The save requires only `org.example.mod`; the extra installed
    // `org.example.lib` is the "new Mod" scenario (spec §6) — recorded, not
    // judged. `lib@1.2.0` satisfies `^1.0`.
    let inspected = inspect_base();
    let env = env_with_dependency();
    let report = assess_compatibility(&inspected, &env).expect("valid");
    assert_eq!(report.verdict, CompatibilityVerdict::Compatible);
    assert_eq!(report.reason_code, CompatReasonCode::Compatible);
}

#[test]
fn an_unsatisfied_dependency_blocks_with_missing_dependency() {
    // B-1a: `lib@0.9.0` does not satisfy `^1.0`.
    let inspected = inspect_base();
    let mut env = env_with_dependency();
    env.installed_mods[1].version = "0.9.0".to_owned();
    let report = assess_compatibility(&inspected, &env).expect("valid");
    assert_eq!(report.verdict, CompatibilityVerdict::Blocked);
    assert_eq!(
        report.reason_code,
        CompatReasonCode::ModDependencyUnsatisfied
    );
    assert_eq!(
        report.reason_code.as_str(),
        "save.compat.mod_dependency_unsatisfied"
    );
    assert_eq!(
        report.flags,
        BTreeSet::from([PolicyFlag::MissingDependency])
    );
    assert_eq!(report.affected_mods, ["org.example.mod"]);
    assert_eq!(
        report.failed_condition,
        "已安装 Mod 的依赖要求未满足：org.example.mod"
    );
    assert_eq!(
        report.available_actions,
        ["安装或升级满足依赖要求的 Mod", "放弃加载"]
    );

    // A dependency whose target is not installed at all is equally
    // unsatisfied.
    let mut env = base_env();
    env.installed_mods[0].dependencies = vec![ModDependencyRef {
        id: "org.example.absent".to_owned(),
        version_requirement: "^1.0".to_owned(),
    }];
    let report = assess_compatibility(&inspected, &env).expect("valid");
    assert_eq!(report.verdict, CompatibilityVerdict::Blocked);
    assert_eq!(
        report.reason_code,
        CompatReasonCode::ModDependencyUnsatisfied
    );
    assert_eq!(
        report.flags,
        BTreeSet::from([PolicyFlag::MissingDependency])
    );
}

#[test]
fn an_invalid_dependency_requirement_is_an_input_error() {
    let inspected = inspect_base();
    let mut env = base_env();
    env.installed_mods[0].dependencies = vec![ModDependencyRef {
        id: "org.example.lib".to_owned(),
        version_requirement: "not-a-req".to_owned(),
    }];
    let error = assess_compatibility(&inspected, &env).expect_err("invalid requirement rejected");
    assert_eq!(
        error,
        CompatInputError::InvalidVersionFormat {
            field: VersionField::DependencyVersionRequirement,
            value: "not-a-req".to_owned()
        }
    );
}

#[test]
fn an_invalid_installed_mod_version_is_an_input_error() {
    let inspected = inspect_base();
    let mut env = base_env();
    env.installed_mods[0].version = "1.0".to_owned(); // not a full SemVer triple
    let error = assess_compatibility(&inspected, &env).expect_err("invalid version rejected");
    assert_eq!(
        error,
        CompatInputError::InvalidVersionFormat {
            field: VersionField::InstalledModVersion,
            value: "1.0".to_owned()
        }
    );
}

#[test]
fn the_platform_gate_is_one_way() {
    let inspected = inspect_base(); // save platform_version = 1.0.0

    // save == current ⇒ pass.
    let report = assess_compatibility(&inspect_base(), &base_env()).expect("valid");
    assert_eq!(report.verdict, CompatibilityVerdict::Compatible);

    // save < current ⇒ pass (one-way gate).
    let mut env = base_env();
    env.platform_version = "1.2.3".to_owned();
    let report = assess_compatibility(&inspected, &env).expect("valid");
    assert_eq!(report.verdict, CompatibilityVerdict::Compatible);

    // save > current ⇒ Blocked + platform_incompatible.
    let mut env = base_env();
    env.platform_version = "0.9.9".to_owned();
    let report = assess_compatibility(&inspected, &env).expect("valid");
    assert_eq!(report.verdict, CompatibilityVerdict::Blocked);
    assert_eq!(report.reason_code, CompatReasonCode::PlatformIncompatible);
    assert_eq!(
        report.reason_code.as_str(),
        "save.compat.platform_incompatible"
    );
    assert!(report.flags.is_empty());
    assert!(report.affected_mods.is_empty());
    assert_eq!(report.save_version.as_deref(), Some("1.0.0"));
    assert_eq!(report.current_version.as_deref(), Some("0.9.9"));
    assert_eq!(
        report.failed_condition,
        "存档平台版本 1.0.0 高于当前平台 0.9.9"
    );
    assert_eq!(report.available_actions, ["升级平台后重试", "放弃加载"]);
}
