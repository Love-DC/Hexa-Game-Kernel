//! Contract tests for `SAVE-004` — the normal save path
//! (`planning/30-P4-SAVE-004-正常保存路径实施契约.md`, approved by
//! `GK-P4-SAVE-004-INTERFACE-APPROVAL-02`).
//!
//! Coverage map (contract §14):
//! * normal round trip: save → read back → `SAVE-003` `verify` intact, with
//!   Manifest/Integrity partitions agreeing on the final root;
//! * `SM` v1 codec: round trip, empty-vs-missing distinction, and per-rule
//!   rejections with the frozen stable codes;
//! * error layering: `Manifest` / `Layout` / `SystemManifest` / `Io` stages;
//! * old-archive protection (E-9a Windows readonly, E-9b Unix chmod, plus a
//!   pre-I/O rejection case) and the E-9c directory-target behavioural case;
//! * `create_new` exclusivity (negative) and S1 concurrency;
//! * the crate-level I/O discipline scan (§13).
//!
//! Registered gaps (contract §8 constraint 7 / §16): the `Sync` failure path
//! and the read-back `Verify` mismatch path have no fault-injection seam in
//! this stage and are covered by code review, not by an automated test.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use kernel_core::Tick;
use save_manager::integrity::{decode_checksums, verify};
use save_manager::partition::{
    ModNamespace, PartitionErrorCode, SaveLayout, SavePartition, SavePartitionId,
};
use save_manager::writer::{
    self, IoStage, MAX_SYSTEM_MANIFEST_ENTRIES, SYSTEM_MANIFEST_ENCODING_TAG,
    SYSTEM_MANIFEST_ENCODING_VERSION, SaveError, SystemManifestErrorCode, save,
};
use save_manager::{
    GameDefinition, Integrity, SaveFormatVersion, SaveManifest, SaveManifestErrorCode,
    SaveModEntry, SimulationConfig,
};

/// The format-legal placeholder `root_hash` approved for the caller's
/// Manifest (contract §6.1.1): 64 lowercase hexadecimal zeros.
const PLACEHOLDER_ROOT: &str = "0000000000000000000000000000000000000000000000000000000000000000";

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn manifest() -> SaveManifest {
    SaveManifest {
        save_format_version: SaveFormatVersion::new(1, 0),
        save_id: "save004.contract".to_owned(),
        created_at: "2026-10-18T00:00:00Z".to_owned(),
        last_saved_at: "2026-10-18T00:00:00Z".to_owned(),
        platform_version: "0.1.0".to_owned(),
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

fn mod_partitions() -> Vec<SavePartition> {
    vec![SavePartition::new(
        SavePartitionId::Mod(ModNamespace::parse("org.example.mod").unwrap()),
        b"mod payload v1".to_vec(),
    )]
}

fn test_dir(tag: &str) -> PathBuf {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "gksave-save004-contract-{tag}-{}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("create test dir");
    dir
}

fn assert_no_temp_files(dir: &Path, target_name: &str) {
    let prefix = format!(".{target_name}.tmp-");
    let leftovers: Vec<String> = std::fs::read_dir(dir)
        .expect("read test dir")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(&prefix))
        .collect();
    assert!(leftovers.is_empty(), "leftover temp files: {leftovers:?}");
}

fn read_target(target: &Path) -> Vec<u8> {
    std::fs::read(target).expect("formal archive exists")
}

// ---------------------------------------------------------------------------
// Normal path
// ---------------------------------------------------------------------------

#[test]
fn normal_save_roundtrip_verifies_and_partitions_agree() {
    let dir = test_dir("roundtrip");
    let target = dir.join("world.gksave");
    let world = b"opaque kernel snapshot bytes";

    let outcome = save(
        &target,
        &manifest(),
        world,
        &mod_partitions(),
        &["org.game/alpha", "org.game/beta"],
    )
    .expect("normal save succeeds");

    // The returned root is real, not the placeholder.
    assert_ne!(outcome.root_hash, PLACEHOLDER_ROOT);
    assert_eq!(outcome.root_hash.len(), 64);

    // `bytes_written` is the verified (read-back) container length, which
    // equals the committed file length.
    let committed = read_target(&target);
    assert_eq!(outcome.bytes_written, committed.len() as u64);

    // The container decodes and passes `SAVE-003` verification.
    let layout = SaveLayout::from_bytes(&committed).expect("container decodes");
    let report = verify(&layout).expect("verify runs");
    assert!(report.is_intact(), "report: {report:?}");

    // The Manifest partition carries the final root; the Integrity partition
    // agrees with it.
    let written = SaveManifest::from_bytes(
        layout
            .get(&SavePartitionId::Manifest)
            .expect("manifest partition")
            .payload
            .as_bytes(),
    )
    .expect("written manifest parses");
    assert_eq!(written.integrity.root_hash, outcome.root_hash);
    assert_eq!(report.manifest_root, outcome.root_hash);
    assert_eq!(report.computed_root, outcome.root_hash);
    let checksums = decode_checksums(
        layout
            .get(&SavePartitionId::Integrity)
            .expect("integrity partition")
            .payload
            .as_bytes(),
    )
    .expect("checksums decode");
    assert_eq!(checksums.root_hash, outcome.root_hash);

    // Carried payloads are preserved byte-for-byte.
    assert_eq!(
        layout
            .get(&SavePartitionId::World)
            .expect("world partition")
            .payload
            .as_bytes(),
        world
    );
    assert_eq!(
        layout
            .get(&SavePartitionId::Mod(
                ModNamespace::parse("org.example.mod").unwrap()
            ))
            .expect("mod partition")
            .payload
            .as_bytes(),
        b"mod payload v1"
    );

    // The system manifest records the IDs in execution (lexicographic) order.
    assert_eq!(
        writer::system_manifest(&layout).expect("system manifest"),
        vec!["org.game/alpha", "org.game/beta"]
    );

    assert_no_temp_files(&dir, "world.gksave");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn empty_system_list_is_stored_and_distinct_from_missing() {
    let dir = test_dir("empty-sm");
    let target = dir.join("world.gksave");

    save(&target, &manifest(), b"world", &[], &[]).expect("save with empty list");
    let layout = SaveLayout::from_bytes(&read_target(&target)).expect("container decodes");
    assert_eq!(
        writer::system_manifest(&layout).expect("manifest present"),
        Vec::<String>::new(),
        "empty list encodes as count = 0, not as a missing payload"
    );

    // A layout without the metadata partition reports `Missing`, never an
    // empty list.
    let bare = SaveLayout::new(vec![SavePartition::new(SavePartitionId::World, b"w")]);
    let error = writer::system_manifest(&bare).expect_err("missing metadata partition");
    assert_eq!(error.code(), SystemManifestErrorCode::Missing);
    assert_eq!(error.code_str(), "save.system_manifest_missing");

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// `SM` v1 codec
// ---------------------------------------------------------------------------

#[test]
fn system_manifest_codec_roundtrips() {
    let ids = ["a", "org.game/alpha", "org.game/beta", "z"];
    let encoded = writer::encode_system_manifest(&ids).expect("encode");
    assert_eq!(&encoded[..2], &SYSTEM_MANIFEST_ENCODING_TAG);
    assert_eq!(encoded[2], SYSTEM_MANIFEST_ENCODING_VERSION);
    assert_eq!(
        writer::decode_system_manifest(&encoded).expect("decode"),
        ids
    );
}

#[test]
fn system_manifest_rejects_a_foreign_tag() {
    let mut bytes = writer::encode_system_manifest(&["a"]).expect("encode");
    bytes[0] = b'X';
    let error = writer::decode_system_manifest(&bytes).expect_err("bad tag");
    assert_eq!(error.code(), SystemManifestErrorCode::Invalid);
    assert_eq!(error.code_str(), "save.system_manifest_invalid");
    assert_eq!(error.path(), "/system_manifest/tag");
}

#[test]
fn system_manifest_rejects_an_unknown_version() {
    let mut bytes = writer::encode_system_manifest(&["a"]).expect("encode");
    bytes[2] = 9;
    let error = writer::decode_system_manifest(&bytes).expect_err("bad version");
    assert_eq!(error.code(), SystemManifestErrorCode::Invalid);
    assert_eq!(error.path(), "/system_manifest/version");
}

#[test]
fn system_manifest_rejects_truncation() {
    let encoded = writer::encode_system_manifest(&["a", "b"]).expect("encode");
    let truncated = &encoded[..encoded.len() - 2];
    let error = writer::decode_system_manifest(truncated).expect_err("truncated");
    assert_eq!(error.code(), SystemManifestErrorCode::Invalid);
}

#[test]
fn system_manifest_rejects_trailing_bytes() {
    let mut bytes = writer::encode_system_manifest(&["a"]).expect("encode");
    bytes.push(0);
    let error = writer::decode_system_manifest(&bytes).expect_err("trailing byte");
    assert_eq!(error.code(), SystemManifestErrorCode::Invalid);
    assert_eq!(error.path(), "/system_manifest");
}

#[test]
fn system_manifest_rejects_non_utf8_and_malformed_ids() {
    // Non-UTF-8 ID bytes.
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&SYSTEM_MANIFEST_ENCODING_TAG);
    bytes.push(SYSTEM_MANIFEST_ENCODING_VERSION);
    bytes.extend_from_slice(&1_u32.to_be_bytes());
    bytes.extend_from_slice(&2_u16.to_be_bytes());
    bytes.extend_from_slice(&[0xFF, 0xFE]);
    let error = writer::decode_system_manifest(&bytes).expect_err("non-UTF-8 ID");
    assert_eq!(error.code(), SystemManifestErrorCode::Invalid);

    // A zero-length ID.
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&SYSTEM_MANIFEST_ENCODING_TAG);
    bytes.push(SYSTEM_MANIFEST_ENCODING_VERSION);
    bytes.extend_from_slice(&1_u32.to_be_bytes());
    bytes.extend_from_slice(&0_u16.to_be_bytes());
    let error = writer::decode_system_manifest(&bytes).expect_err("empty ID");
    assert_eq!(error.code(), SystemManifestErrorCode::Invalid);

    // An ID longer than 128 bytes.
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&SYSTEM_MANIFEST_ENCODING_TAG);
    bytes.push(SYSTEM_MANIFEST_ENCODING_VERSION);
    bytes.extend_from_slice(&1_u32.to_be_bytes());
    bytes.extend_from_slice(&129_u16.to_be_bytes());
    bytes.extend_from_slice(&[b'a'; 129]);
    let error = writer::decode_system_manifest(&bytes).expect_err("overlong ID");
    assert_eq!(error.code(), SystemManifestErrorCode::Invalid);

    // An ID violating the stable-ID character rule.
    let error = writer::encode_system_manifest(&["Org.Game/Upper"]).expect_err("invalid ID");
    assert_eq!(error.code(), SystemManifestErrorCode::Invalid);
}

#[test]
fn system_manifest_rejects_duplicate_and_out_of_order_ids() {
    let error = writer::encode_system_manifest(&["a", "a"]).expect_err("duplicate");
    assert_eq!(error.code(), SystemManifestErrorCode::Invalid);
    let error = writer::encode_system_manifest(&["b", "a"]).expect_err("out of order");
    assert_eq!(error.code(), SystemManifestErrorCode::Invalid);
}

#[test]
fn system_manifest_enforces_the_entry_count_limit() {
    let ids: Vec<String> = (0..=MAX_SYSTEM_MANIFEST_ENTRIES)
        .map(|index| format!("s{index:05}"))
        .collect();
    let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
    let error = writer::encode_system_manifest(&refs).expect_err("over the limit");
    assert_eq!(error.code(), SystemManifestErrorCode::Invalid);
    assert_eq!(error.path(), "/system_manifest/entries");

    // A declared count the bytes cannot encode is rejected before allocation.
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&SYSTEM_MANIFEST_ENCODING_TAG);
    bytes.push(SYSTEM_MANIFEST_ENCODING_VERSION);
    bytes.extend_from_slice(&100_u32.to_be_bytes());
    let error = writer::decode_system_manifest(&bytes).expect_err("impossible count");
    assert_eq!(error.code(), SystemManifestErrorCode::Invalid);
}

// ---------------------------------------------------------------------------
// Error layering and stage classification
// ---------------------------------------------------------------------------

#[test]
fn an_invalid_manifest_is_rejected_before_any_io() {
    let dir = test_dir("manifest-err");
    let target = dir.join("world.gksave");

    let mut bad = manifest();
    bad.integrity.root_hash = "IMPLEMENTATION_GENERATED".to_owned();
    let error = save(&target, &bad, b"world", &[], &[]).expect_err("invalid manifest");
    match &error {
        SaveError::Manifest(inner) => {
            assert_eq!(inner.code(), SaveManifestErrorCode::InvalidField);
            assert_eq!(inner.code_str(), "save.invalid_field");
            assert_eq!(inner.path(), "/integrity/root_hash");
        }
        other => panic!("expected SaveError::Manifest, got {other:?}"),
    }
    assert!(!target.exists(), "no formal file may be created");
    assert_no_temp_files(&dir, "world.gksave");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn invalid_system_ids_are_rejected_before_any_io() {
    let dir = test_dir("sm-err");
    let target = dir.join("world.gksave");

    let error = save(&target, &manifest(), b"world", &[], &["b", "a"]).expect_err("unsorted IDs");
    match &error {
        SaveError::SystemManifest(inner) => {
            assert_eq!(inner.code(), SystemManifestErrorCode::Invalid);
        }
        other => panic!("expected SaveError::SystemManifest, got {other:?}"),
    }
    assert!(!target.exists());
    assert_no_temp_files(&dir, "world.gksave");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_invalid_partition_set_is_a_layout_error_before_any_io() {
    let dir = test_dir("layout-err");
    let target = dir.join("world.gksave");

    // A second `World` partition duplicates the identity the flow adds.
    let intruder = vec![SavePartition::new(SavePartitionId::World, b"intruder")];
    let error = save(&target, &manifest(), b"world", &intruder, &[]).expect_err("duplicate World");
    match &error {
        SaveError::Layout(inner) => {
            assert_eq!(inner.code(), PartitionErrorCode::DuplicatePartition);
            assert_eq!(inner.code_str(), "save.duplicate_partition");
        }
        other => panic!("expected SaveError::Layout, got {other:?}"),
    }
    assert!(!target.exists());
    assert_no_temp_files(&dir, "world.gksave");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_missing_parent_directory_is_a_create_stage_error() {
    let dir = test_dir("create-err");
    let target = dir.join("no/such/dir/world.gksave");

    let error = save(&target, &manifest(), b"world", &[], &[]).expect_err("create must fail");
    match &error {
        SaveError::Io { stage, path, .. } => {
            assert_eq!(*stage, IoStage::Create);
            assert!(path.starts_with(dir.join("no/such/dir")));
        }
        other => panic!("expected SaveError::Io{{Create}}, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Old-archive protection and rename semantics
// ---------------------------------------------------------------------------

/// E-9a (Windows): a read-only formal archive cannot be replaced; the failure
/// is `IoStage::Rename` and the old archive's bytes and verifiability are
/// untouched.
///
/// The cleanup clears the readonly flag — the correct, platform-specific way
/// to make the file removable on Windows — so the
/// `permissions_set_readonly_false` lint (aimed at Unix `0o666` semantics)
/// does not apply.
#[cfg(windows)]
#[test]
#[allow(clippy::permissions_set_readonly_false)]
fn rename_over_a_readonly_target_fails_and_preserves_the_old_archive() {
    let dir = test_dir("e9a");
    let target = dir.join("world.gksave");
    save(&target, &manifest(), b"old world", &mod_partitions(), &[]).expect("first save");
    let old_bytes = read_target(&target);

    let mut readonly = std::fs::metadata(&target).expect("metadata").permissions();
    readonly.set_readonly(true);
    std::fs::set_permissions(&target, readonly).expect("set readonly");

    let error = save(&target, &manifest(), b"new world", &mod_partitions(), &[])
        .expect_err("rename over a readonly target must fail");
    assert!(
        matches!(
            error,
            SaveError::Io {
                stage: IoStage::Rename,
                ..
            }
        ),
        "expected a rename-stage failure, got {error:?}"
    );

    // The three old-archive invariants: bytes, digest verification, readability.
    assert_eq!(read_target(&target), old_bytes, "old bytes untouched");
    let layout = SaveLayout::from_bytes(&old_bytes).expect("old archive decodes");
    assert!(verify(&layout).expect("old archive verifies").is_intact());
    assert_no_temp_files(&dir, "world.gksave");

    let mut writable = std::fs::metadata(&target).expect("metadata").permissions();
    writable.set_readonly(false);
    std::fs::set_permissions(&target, writable).expect("clear readonly");
    let _ = std::fs::remove_dir_all(&dir);
}

/// E-9b (Unix): with the directory at `0o555` the commit fails with a
/// structured I/O error and the old archive is untouched. The temporary file
/// lives in the same directory, so the failure surfaces at `Create` here; the
/// Unix `Rename`-stage mapping is covered by the directory-target test below.
#[cfg(unix)]
#[test]
fn an_unwritable_directory_fails_and_preserves_the_old_archive() {
    use std::os::unix::fs::PermissionsExt;

    let dir = test_dir("e9b");
    let target = dir.join("world.gksave");
    save(&target, &manifest(), b"old world", &mod_partitions(), &[]).expect("first save");
    let old_bytes = read_target(&target);

    let mut locked = std::fs::metadata(&dir).expect("metadata").permissions();
    locked.set_mode(0o555);
    std::fs::set_permissions(&dir, locked).expect("lock directory");

    let error = save(&target, &manifest(), b"new world", &mod_partitions(), &[])
        .expect_err("save into a locked directory must fail");
    assert!(
        matches!(error, SaveError::Io { .. }),
        "expected a structured I/O failure, got {error:?}"
    );

    assert_eq!(read_target(&target), old_bytes, "old bytes untouched");
    let layout = SaveLayout::from_bytes(&old_bytes).expect("old archive decodes");
    assert!(verify(&layout).expect("old archive verifies").is_intact());
    assert_no_temp_files(&dir, "world.gksave");

    let mut unlocked = std::fs::metadata(&dir).expect("metadata").permissions();
    unlocked.set_mode(0o755);
    std::fs::set_permissions(&dir, unlocked).expect("unlock directory");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A rejection before any I/O (here: invalid system IDs) must leave an
/// existing archive byte-identical.
#[test]
fn a_pre_io_rejection_preserves_the_old_archive() {
    let dir = test_dir("pre-io");
    let target = dir.join("world.gksave");
    save(&target, &manifest(), b"old world", &mod_partitions(), &[]).expect("first save");
    let old_bytes = read_target(&target);

    let error = save(
        &target,
        &manifest(),
        b"new world",
        &mod_partitions(),
        &["Bad/ID"],
    )
    .expect_err("invalid system IDs");
    assert!(matches!(error, SaveError::SystemManifest(_)));
    assert_eq!(read_target(&target), old_bytes, "old bytes untouched");
    assert_no_temp_files(&dir, "world.gksave");
    let _ = std::fs::remove_dir_all(&dir);
}

/// E-9c (behavioural, all platforms): a directory sitting at the target path
/// makes the final `rename` fail; the error maps to `IoStage::Rename`, the
/// directory is untouched, and no temporary file is left behind. This test is
/// not old-archive evidence.
#[test]
fn a_directory_target_fails_at_rename_and_leaves_no_temp_file() {
    let dir = test_dir("e9c");
    let target = dir.join("world.gksave");
    std::fs::create_dir(&target).expect("create directory target");

    let error = save(&target, &manifest(), b"world", &mod_partitions(), &[])
        .expect_err("rename onto a directory must fail");
    match &error {
        SaveError::Io { stage, path, .. } => {
            assert_eq!(*stage, IoStage::Rename);
            assert_eq!(path, &target);
        }
        other => panic!("expected SaveError::Io{{Rename}}, got {other:?}"),
    }
    assert!(target.is_dir(), "the directory target is untouched");
    assert_no_temp_files(&dir, "world.gksave");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Concurrency (S1)
// ---------------------------------------------------------------------------

#[test]
fn concurrent_saves_commit_whole_generations_and_last_writer_wins() {
    let dir = test_dir("s1");
    let target = dir.join("world.gksave");

    let mut handles = Vec::new();
    for tag in *b"AB" {
        let target = target.clone();
        handles.push(std::thread::spawn(move || {
            let world = vec![tag; 64];
            for _ in 0..8 {
                save(
                    &target,
                    &manifest(),
                    &world,
                    &mod_partitions(),
                    &["org.game/alpha"],
                )
                .expect("concurrent saves never collide on temp names");
            }
        }));
    }
    for handle in handles {
        handle.join().expect("writer thread");
    }

    // S1: the final archive is one complete generation — never torn data.
    let committed = read_target(&target);
    let layout = SaveLayout::from_bytes(&committed).expect("final archive decodes");
    assert!(verify(&layout).expect("final archive verifies").is_intact());
    let world = layout
        .get(&SavePartitionId::World)
        .expect("world partition")
        .payload
        .as_bytes();
    assert!(
        world == vec![b'A'; 64] || world == vec![b'B'; 64],
        "final world payload must be one complete writer's generation"
    );
    assert_no_temp_files(&dir, "world.gksave");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Crate-level I/O discipline (contract §13)
// ---------------------------------------------------------------------------

/// `writer.rs` is the only production module allowed to perform save file
/// I/O. Every other `src/*.rs` file must stay free of filesystem, network,
/// process and time access, and of `unsafe`.
///
/// Registered exception (contract §13 allows a documented exception list):
/// the crate root's `#![forbid(unsafe_code)]` attribute contains the token
/// `unsafe` and is scrubbed before the scan.
#[test]
fn only_writer_performs_save_io() {
    const FORBIDDEN: [&str; 6] = [
        "std::fs",
        "std::net",
        "std::process",
        "std::time",
        "SystemTime",
        "unsafe",
    ];

    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut scanned = 0;
    for entry in std::fs::read_dir(&src).expect("src directory") {
        let entry = entry.expect("directory entry");
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "rs")
            && path.file_name().is_some_and(|name| name != "writer.rs")
        {
            let text = std::fs::read_to_string(&path).expect("read source file");
            let scrubbed: String = text
                .lines()
                .filter(|line| !line.contains("forbid(unsafe_code)"))
                .collect::<Vec<_>>()
                .join("\n");
            for token in FORBIDDEN {
                assert!(
                    !scrubbed.contains(token),
                    "{} must not contain `{token}` (save I/O is confined to writer.rs)",
                    path.display()
                );
            }
            scanned += 1;
        }
    }
    assert!(
        scanned >= 3,
        "expected at least lib/partition/integrity sources"
    );

    // writer.rs itself must stay within its charter: file I/O and the process
    // ID only — no network, no clock, no process spawning, no unsafe code.
    let writer_path = src.join("writer.rs");
    let writer = std::fs::read_to_string(&writer_path).expect("read writer.rs");
    for token in [
        "std::net",
        "std::time",
        "SystemTime",
        "std::process::Command",
        "unsafe",
    ] {
        assert!(
            !writer.contains(token),
            "writer.rs must not contain `{token}`"
        );
    }
}
