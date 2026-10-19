use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use kernel_core::Tick;
use save_manager::backup::{
    BackupPolicy, BackupPurpose, create_backup, inspect_backup, prune_history, restore_backup,
};
use save_manager::loader::inspect_save;
use save_manager::partition::{
    ModNamespace, PartitionPayload, SaveLayout, SavePartition, SavePartitionId,
};
use save_manager::writer;
use save_manager::{
    GameDefinition, Integrity, SaveFormatVersion as V, SaveManifest, SaveModEntry, SimulationConfig,
};

static NEXT: AtomicU64 = AtomicU64::new(0);

fn target() -> PathBuf {
    std::env::temp_dir().join(format!(
        "gksave-backup-{}-{}.gksave",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

fn save_bytes(world: &[u8]) -> Vec<u8> {
    let path = target();
    let manifest = SaveManifest {
        save_format_version: V::new(1, 0),
        save_id: "save.backup.contract".into(),
        created_at: "2026-10-19T00:00:00Z".into(),
        last_saved_at: "2026-10-19T00:00:00Z".into(),
        platform_version: "1.0.0".into(),
        game_definition: GameDefinition {
            id: "org.example.game".into(),
            version: "1.0.0".into(),
        },
        kernel_api_version: "1.0".into(),
        world_schema_version: "1.0".into(),
        logical_tick: Tick(1),
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
    writer::save(
        &path,
        &manifest,
        world,
        &[SavePartition::new(
            SavePartitionId::Mod(ModNamespace::parse("org.example.mod").unwrap()),
            b"mod-data",
        )],
        &["game/system"],
    )
    .unwrap();
    let bytes = fs::read(&path).unwrap();
    fs::remove_file(path).unwrap();
    bytes
}

fn archive_path(source: &Path, id: &str) -> PathBuf {
    source.with_file_name(format!(
        ".{}.backup-{id}.sav",
        source.file_name().unwrap().to_string_lossy()
    ))
}

fn meta_path(source: &Path, id: &str) -> PathBuf {
    source.with_file_name(format!(
        ".{}.backup-meta-{id}",
        source.file_name().unwrap().to_string_lossy()
    ))
}

fn cleanup(source: &Path, ids: &[&str], target: &Path) {
    let _ = fs::remove_file(source);
    let _ = fs::remove_file(target);
    for id in ids {
        let _ = fs::remove_file(archive_path(source, id));
        let _ = fs::remove_file(meta_path(source, id));
    }
}

#[test]
fn backup_roundtrip_restores_verified_archive_and_keeps_source_backup() {
    let source = target();
    let original = save_bytes(b"original");
    fs::write(&source, &original).unwrap();
    let outcome = create_backup(
        &source,
        "manual-1",
        "2026-10-19T01:00:00Z",
        BackupPurpose::Manual,
        None,
        None,
    )
    .unwrap();
    assert_eq!(fs::read(&outcome.archive_path).unwrap(), original);
    assert_eq!(
        inspect_backup(&source, "manual-1").unwrap().metadata,
        outcome.metadata
    );
    fs::write(&source, save_bytes(b"changed")).unwrap();
    restore_backup(&source, "manual-1", &source).unwrap();
    assert_eq!(fs::read(&source).unwrap(), original);
    assert!(outcome.archive_path.exists());
    cleanup(&source, &["manual-1"], &source);
}

#[test]
fn corrupt_or_mismatched_backup_is_refused_without_overwriting_target() {
    let source = target();
    let destination = target();
    fs::write(&source, save_bytes(b"original")).unwrap();
    let outcome = create_backup(
        &source,
        "manual-corrupt",
        "2026-10-19T01:00:00Z",
        BackupPurpose::Manual,
        None,
        None,
    )
    .unwrap();
    let target_bytes = save_bytes(b"keep-current");
    fs::write(&destination, &target_bytes).unwrap();
    fs::write(&outcome.archive_path, b"truncated").unwrap();
    assert!(restore_backup(&source, "manual-corrupt", &destination).is_err());
    assert_eq!(fs::read(&destination).unwrap(), target_bytes);
    assert!(inspect_save(&fs::read(&source).unwrap()).is_ok());
    assert!(inspect_save(&fs::read(&destination).unwrap()).is_ok());
    assert!(outcome.archive_path.exists());
    cleanup(&source, &["manual-corrupt"], &destination);
}

#[test]
fn checksum_mismatched_backup_is_refused_and_both_live_saves_remain_recoverable() {
    let source = target();
    let destination = target();
    let source_bytes = save_bytes(b"original-valid-save");
    let destination_bytes = save_bytes(b"keep-current-valid-save");
    fs::write(&source, &source_bytes).unwrap();
    fs::write(&destination, &destination_bytes).unwrap();
    let backup = create_backup(
        &source,
        "manual-tampered",
        "2026-10-19T01:00:00Z",
        BackupPurpose::Manual,
        None,
        None,
    )
    .unwrap();
    let mut altered = fs::read(&backup.archive_path).unwrap();
    let index = altered.len() / 2;
    altered[index] ^= 1;
    fs::write(&backup.archive_path, altered).unwrap();

    assert!(restore_backup(&source, "manual-tampered", &destination).is_err());
    assert_eq!(fs::read(&source).unwrap(), source_bytes);
    assert_eq!(fs::read(&destination).unwrap(), destination_bytes);
    assert!(inspect_save(&fs::read(&source).unwrap()).is_ok());
    assert!(inspect_save(&fs::read(&destination).unwrap()).is_ok());
    cleanup(&source, &["manual-tampered"], &destination);
}

#[test]
fn corrupted_mod_partition_is_rejected_without_touching_the_confirmed_save() {
    let confirmed = target();
    let corrupted_candidate = target();
    let source = save_bytes(b"world");
    fs::write(&confirmed, &source).unwrap();
    let layout = SaveLayout::from_bytes(&source).unwrap();
    let partitions = layout
        .partitions
        .into_iter()
        .map(|mut partition| {
            if matches!(partition.id, SavePartitionId::Mod(_)) {
                let mut payload = partition.payload.into_bytes();
                payload[0] ^= 1;
                partition.payload = PartitionPayload::new(payload);
            }
            partition
        })
        .collect();
    let corrupted = SaveLayout::new(partitions).to_bytes().unwrap();
    fs::write(&corrupted_candidate, corrupted).unwrap();

    assert!(inspect_save(&fs::read(&corrupted_candidate).unwrap()).is_err());
    assert_eq!(fs::read(&confirmed).unwrap(), source);
    assert!(inspect_save(&fs::read(&confirmed).unwrap()).is_ok());
    cleanup(&confirmed, &[], &corrupted_candidate);
}

#[test]
fn pruning_touches_only_successful_history_and_never_the_last_recovery_point() {
    let source = target();
    fs::write(&source, save_bytes(b"current")).unwrap();
    create_backup(
        &source,
        "history-old",
        "2026-10-19T01:00:00Z",
        BackupPurpose::SuccessfulSaveHistory,
        None,
        None,
    )
    .unwrap();
    create_backup(
        &source,
        "history-new",
        "2026-10-19T02:00:00Z",
        BackupPurpose::SuccessfulSaveHistory,
        None,
        None,
    )
    .unwrap();
    create_backup(
        &source,
        "migration-preflight",
        "2026-10-19T02:30:00Z",
        BackupPurpose::MigrationPreflight,
        Some("ab".repeat(32)),
        Some("2.0".into()),
    )
    .unwrap();
    assert!(
        prune_history(
            &source,
            BackupPolicy {
                max_history_count: 1,
                max_history_bytes: u64::MAX,
            },
            "different-save-id",
        )
        .is_err()
    );
    assert!(archive_path(&source, "history-old").exists());
    let removed = prune_history(
        &source,
        BackupPolicy {
            max_history_count: 1,
            max_history_bytes: u64::MAX,
        },
        "save.backup.contract",
    )
    .unwrap();
    assert_eq!(removed, vec![archive_path(&source, "history-old")]);
    assert!(!archive_path(&source, "history-old").exists());
    assert!(archive_path(&source, "history-new").exists());
    assert!(archive_path(&source, "migration-preflight").exists());
    assert!(
        prune_history(
            &source,
            BackupPolicy {
                max_history_count: 0,
                max_history_bytes: 0
            },
            "save.backup.contract"
        )
        .is_err()
    );
    cleanup(
        &source,
        &["history-old", "history-new", "migration-preflight"],
        &source,
    );
}

#[test]
fn backup_ids_and_migration_metadata_fail_closed() {
    let source = target();
    fs::write(&source, save_bytes(b"source")).unwrap();
    assert!(
        create_backup(
            &source,
            "../escape",
            "2026-10-19T01:00:00Z",
            BackupPurpose::Manual,
            None,
            None
        )
        .is_err()
    );
    assert!(
        create_backup(
            &source,
            "migration-missing-plan",
            "2026-10-19T01:00:00Z",
            BackupPurpose::MigrationPreflight,
            None,
            Some("2.0".into())
        )
        .is_err()
    );
    cleanup(&source, &[], &source);
}

#[test]
fn existing_backup_temp_file_simulates_exclusive_write_failure() {
    let source = target();
    let bytes = save_bytes(b"source");
    fs::write(&source, &bytes).unwrap();
    let temp = source.with_file_name(format!(
        ".{}.backup-write-failure.tmp",
        source.file_name().unwrap().to_string_lossy()
    ));
    fs::write(&temp, b"pre-existing; must not be truncated").unwrap();
    let error = create_backup(
        &source,
        "write-failure",
        "2026-10-19T01:00:00Z",
        BackupPurpose::Manual,
        None,
        None,
    )
    .unwrap_err();
    assert_eq!(error.code(), "save.backup.io");
    assert_eq!(fs::read(&source).unwrap(), bytes);
    assert_eq!(
        fs::read(&temp).unwrap(),
        b"pre-existing; must not be truncated"
    );
    let _ = fs::remove_file(temp);
    cleanup(&source, &["write-failure"], &source);
}

#[test]
fn large_archive_and_many_mod_partitions_roundtrip_and_backup() {
    let target = target();
    let mod_entries: Vec<_> = (0..128)
        .map(|index| SaveModEntry {
            id: format!("org.example.mod{index:03}"),
            version: "1.0.0".into(),
            schema_version: "1.0".into(),
        })
        .collect();
    let partitions: Vec<_> = mod_entries
        .iter()
        .map(|entry| {
            SavePartition::new(
                SavePartitionId::Mod(ModNamespace::parse(&entry.id).unwrap()),
                vec![entry.id.as_bytes()[0]; 64],
            )
        })
        .collect();
    let manifest = SaveManifest {
        save_format_version: V::new(1, 0),
        save_id: "save.large.contract".into(),
        created_at: "2026-10-19T00:00:00Z".into(),
        last_saved_at: "2026-10-19T00:00:00Z".into(),
        platform_version: "1.0.0".into(),
        game_definition: GameDefinition {
            id: "org.example.game".into(),
            version: "1.0.0".into(),
        },
        kernel_api_version: "1.0".into(),
        world_schema_version: "1.0".into(),
        logical_tick: Tick(8),
        mods: mod_entries,
        simulation_config: SimulationConfig::new(),
        integrity: Integrity {
            algorithm: "sha256".into(),
            root_hash: "0".repeat(64),
        },
    };
    let world = vec![0xA5; 16 * 1024 * 1024];
    writer::save(&target, &manifest, &world, &partitions, &[]).unwrap();
    let inspected = save_manager::loader::inspect_save_file(&target).unwrap();
    assert_eq!(inspected.world_payload().len(), world.len());
    assert_eq!(inspected.manifest().mods.len(), 128);
    let backup = create_backup(
        &target,
        "large-manual",
        "2026-10-19T01:00:00Z",
        BackupPurpose::Manual,
        None,
        None,
    )
    .unwrap();
    assert!(backup.bytes_written > 16 * 1024 * 1024);
    assert_eq!(
        inspect_backup(&target, "large-manual").unwrap().metadata,
        backup.metadata
    );
    cleanup(&target, &["large-manual"], &target);
}
