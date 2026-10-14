//! SAVE-002 contract tests for the save partition carrier.
//!
//! These tests pin the frozen `SAVE-002` contract: the five partition identities
//! that mirror the `§3.1` logical layout, the namespace/ownership rules of `§4`,
//! the determinism and byte-for-byte losslessness of carried data, the
//! report-only (never repair) Manifest consistency check, and the stable
//! machine-readable error codes that are disjoint from `SAVE-001`.

use kernel_core::Tick;
use save_manager::partition::{
    MAX_SCHEMA_VERSION_LEN, ModNamespace, PartitionConsistency, PartitionErrorCode, PartitionOwner,
    PartitionPayload, SaveLayout, SavePartition, SavePartitionId,
};
use save_manager::{
    GameDefinition, Integrity, SUPPORTED_INTEGRITY_ALGORITHM, SaveFormatVersion, SaveManifest,
    SaveManifestErrorCode, SaveModEntry,
};

const REAL_ROOT_HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn namespace(value: &str) -> ModNamespace {
    ModNamespace::parse(value).expect("test namespace is valid")
}

fn mod_id(value: &str) -> SavePartitionId {
    SavePartitionId::Mod(namespace(value))
}

/// Encodes a layout that is expected to be valid.
///
/// `SaveLayout::to_bytes` is fallible (`GK-P4-SAVE-002-R3`); positive-path tests
/// use this helper, and the negative-path tests match on the error directly.
fn encode(layout: &SaveLayout) -> Vec<u8> {
    layout.to_bytes().expect("a valid layout encodes")
}

fn valid_manifest() -> SaveManifest {
    SaveManifest {
        save_format_version: SaveFormatVersion::new(1, 0),
        save_id: "save-example-001".to_owned(),
        created_at: "2026-01-01T12:00:00Z".to_owned(),
        last_saved_at: "2026-01-01T12:30:00Z".to_owned(),
        platform_version: "1.0.0".to_owned(),
        game_definition: GameDefinition {
            id: "org.example.base-game".to_owned(),
            version: "1.0.0".to_owned(),
        },
        kernel_api_version: "1.0".to_owned(),
        world_schema_version: "1.0".to_owned(),
        logical_tick: Tick(72000),
        mods: vec![
            SaveModEntry {
                id: "org.example.base".to_owned(),
                version: "1.0.0".to_owned(),
                schema_version: "1.0".to_owned(),
            },
            SaveModEntry {
                id: "org.example.economy".to_owned(),
                version: "1.2.0".to_owned(),
                schema_version: "2.0".to_owned(),
            },
        ],
        simulation_config: Default::default(),
        integrity: Integrity {
            algorithm: SUPPORTED_INTEGRITY_ALGORITHM.to_owned(),
            root_hash: REAL_ROOT_HASH.to_owned(),
        },
    }
}

// ---------------------------------------------------------------------------
// Acceptance 1 — partition completeness against the §3.1 logical layout
// ---------------------------------------------------------------------------

#[test]
fn five_partition_kinds_map_to_the_spec_logical_layout() {
    let ids = [
        SavePartitionId::Manifest,
        SavePartitionId::World,
        mod_id("org.example.base"),
        SavePartitionId::Metadata,
        SavePartitionId::Integrity,
    ];
    let paths: Vec<String> = ids.iter().map(SavePartitionId::logical_path).collect();
    assert_eq!(
        paths,
        vec![
            "manifest.json",
            "world/world-data",
            "mods/org.example.base/data",
            "metadata/session-data",
            "integrity/checksums",
        ]
    );

    let kinds: Vec<&str> = ids.iter().map(SavePartitionId::kind_str).collect();
    assert_eq!(
        kinds,
        vec!["manifest", "world", "mod", "metadata", "integrity"]
    );
}

#[test]
fn canonical_owners_encode_the_ownership_model() {
    assert_eq!(
        SavePartitionId::Manifest.canonical_owner(),
        PartitionOwner::SaveSystem
    );
    assert_eq!(
        SavePartitionId::World.canonical_owner(),
        PartitionOwner::Kernel
    );
    assert_eq!(
        mod_id("org.example.base").canonical_owner(),
        PartitionOwner::Mod(namespace("org.example.base"))
    );
    assert_eq!(
        SavePartitionId::Metadata.canonical_owner(),
        PartitionOwner::SaveSystem
    );
    assert_eq!(
        SavePartitionId::Integrity.canonical_owner(),
        PartitionOwner::SaveSystem
    );

    // Only Mod partitions carry a namespace.
    assert!(mod_id("org.example.base").namespace().is_some());
    assert!(SavePartitionId::World.namespace().is_none());
}

// ---------------------------------------------------------------------------
// Acceptance 2 — namespace correctness (equal to the stable Mod ID)
// ---------------------------------------------------------------------------

#[test]
fn namespace_accepts_stable_mod_ids() {
    for good in [
        "a",
        "org.example.base",
        "org.example.base-game",
        "org.community.transport",
    ] {
        let parsed = ModNamespace::parse(good).expect("valid namespace");
        assert_eq!(parsed.as_str(), good);
        assert_eq!(parsed.to_string(), good);
    }
}

#[test]
fn namespace_rejects_path_unsafe_and_malformed_input() {
    for bad in [
        "",
        ".",
        "..",
        "../etc",
        "/absolute",
        "org/example",
        "org..example",
        "org.example.",
        ".org.example",
        "Org.Example",
        "org.example-",
        "org.-example",
        "org example",
        "组织",
    ] {
        let error = ModNamespace::parse(bad).expect_err("namespace must be rejected");
        assert_eq!(error.code(), PartitionErrorCode::InvalidNamespace, "{bad}");
        assert_eq!(error.code_str(), "save.invalid_namespace", "{bad}");
        assert_eq!(error.path(), "/namespace", "{bad}");
    }
}

// ---------------------------------------------------------------------------
// Acceptance 3 — ownership constraint (a Mod partition is owned by its own
// namespace, and only by it)
// ---------------------------------------------------------------------------

#[test]
fn mod_partition_cannot_be_owned_by_another_namespace() {
    let partition = SavePartition::with_owner(
        mod_id("org.example.base"),
        PartitionOwner::Mod(namespace("org.example.other")),
        b"payload",
    );
    let layout = SaveLayout::new(vec![partition]);
    let error = layout
        .validate()
        .expect_err("cross-namespace ownership must be rejected");
    assert_eq!(
        error.code(),
        PartitionErrorCode::PartitionOwnershipViolation
    );
    assert_eq!(error.code_str(), "save.partition_ownership_violation");
    assert_eq!(error.path(), "/partitions/0/owner");
}

#[test]
fn mod_partition_cannot_be_owned_by_the_save_system() {
    let partition = SavePartition::with_owner(
        mod_id("org.example.base"),
        PartitionOwner::SaveSystem,
        b"payload",
    );
    let layout = SaveLayout::new(vec![partition]);
    assert_eq!(
        layout.validate().unwrap_err().code(),
        PartitionErrorCode::PartitionOwnershipViolation
    );
}

#[test]
fn world_partition_cannot_be_owned_by_a_mod_namespace() {
    let partition = SavePartition::with_owner(
        SavePartitionId::World,
        PartitionOwner::Mod(namespace("org.example.base")),
        b"w",
    );
    let layout = SaveLayout::new(vec![partition]);
    assert_eq!(
        layout.validate().unwrap_err().code(),
        PartitionErrorCode::PartitionOwnershipViolation
    );
}

#[test]
fn canonically_owned_partitions_validate() {
    let layout = SaveLayout::new(vec![
        SavePartition::new(SavePartitionId::Manifest, b"manifest-bytes"),
        SavePartition::new(SavePartitionId::World, b"world-bytes"),
        SavePartition::new(mod_id("org.example.base"), b"base-bytes"),
        SavePartition::new(SavePartitionId::Metadata, b"{}"),
        SavePartition::new(SavePartitionId::Integrity, b"checksums"),
    ]);
    layout.validate().expect("canonical owners are valid");
    // `new` derives the canonical owner for every partition.
    for partition in layout.partitions() {
        assert_eq!(partition.owner, partition.canonical_owner());
    }
}

// ---------------------------------------------------------------------------
// Acceptance 4 — unknown data is lossless (byte-for-byte round trip)
// ---------------------------------------------------------------------------

#[test]
fn unrecognized_partition_round_trips_byte_identically() {
    let payload: Vec<u8> = (0u8..=255).collect();
    let partition = SavePartition::new(mod_id("org.legacy.retired"), payload.clone())
        .with_schema_version("3.1");
    let layout = SaveLayout::new(vec![partition]);

    let bytes = encode(&layout);
    let decoded = SaveLayout::from_bytes(&bytes).expect("layout decodes");

    assert_eq!(decoded, layout);
    assert_eq!(
        encode(&decoded),
        bytes,
        "re-serialization must be byte-identical"
    );

    let restored = decoded
        .get(&mod_id("org.legacy.retired"))
        .expect("the unrecognized partition is retained");
    assert_eq!(restored.payload.as_bytes(), payload.as_slice());
    assert_eq!(restored.schema_version.as_deref(), Some("3.1"));
    assert_eq!(
        restored.owner,
        PartitionOwner::Mod(namespace("org.legacy.retired"))
    );
}

#[test]
fn layout_round_trip_is_byte_identical_for_all_partition_kinds() {
    let layout = SaveLayout::new(vec![
        SavePartition::new(SavePartitionId::Manifest, b"{\"manifest\":true}"),
        SavePartition::new(SavePartitionId::World, vec![0x00, 0x53, 0x4E, 0x02, 0xFF]),
        SavePartition::new(mod_id("org.example.base"), b"base"),
        SavePartition::new(mod_id("org.example.economy"), b"economy").with_schema_version("2.0"),
        SavePartition::new(SavePartitionId::Metadata, b"session"),
        SavePartition::new(SavePartitionId::Integrity, b"checksums"),
    ]);

    let bytes = encode(&layout);
    let decoded = SaveLayout::from_bytes(&bytes).expect("layout decodes");
    assert_eq!(decoded, layout);
    assert_eq!(encode(&decoded), bytes);
}

#[test]
fn empty_payloads_are_allowed_and_preserved() {
    let layout = SaveLayout::new(vec![SavePartition::new(
        mod_id("org.example.base"),
        Vec::new(),
    )]);
    let decoded = SaveLayout::from_bytes(&encode(&layout)).expect("layout decodes");
    let partition = decoded.get(&mod_id("org.example.base")).expect("partition");
    assert!(partition.payload.is_empty());
    assert_eq!(partition.payload.len(), 0);
    assert_eq!(partition.payload.as_bytes(), b"");
}

#[test]
fn an_empty_layout_round_trips() {
    let layout = SaveLayout::default();
    assert!(layout.is_empty());
    assert_eq!(layout.len(), 0);
    let decoded = SaveLayout::from_bytes(&encode(&layout)).expect("empty layout decodes");
    assert!(decoded.is_empty());
    assert_eq!(decoded, layout);
}

// ---------------------------------------------------------------------------
// Acceptance 5 — missing Mod partitions are reported, never deleted; extra
// partitions are reported, never fabricated
// ---------------------------------------------------------------------------

#[test]
fn manifest_mods_without_partitions_are_reported_never_fabricated() {
    let manifest = valid_manifest();
    let layout = SaveLayout::new(vec![SavePartition::new(
        mod_id("org.example.base"),
        b"base",
    )]);

    let report = layout.check_against(&manifest).expect("consistency check");
    let missing: Vec<&str> = report
        .missing_mod_partitions()
        .iter()
        .map(ModNamespace::as_str)
        .collect();
    assert_eq!(missing, vec!["org.example.economy"]);
    assert!(report.unrecognized_partitions().is_empty());
    assert!(!report.is_consistent());

    // Nothing was fabricated, and nothing was dropped.
    assert_eq!(layout.len(), 1);
    assert!(layout.get(&mod_id("org.example.economy")).is_none());
}

#[test]
fn unrecognized_mod_partition_is_reported_and_retained() {
    let manifest = valid_manifest();
    let layout = SaveLayout::new(vec![
        SavePartition::new(mod_id("org.example.base"), b"base"),
        SavePartition::new(mod_id("org.example.economy"), b"economy"),
        SavePartition::new(mod_id("org.legacy.retired"), b"legacy-bytes"),
    ]);

    let report = layout.check_against(&manifest).expect("consistency check");
    assert!(report.missing_mod_partitions().is_empty());
    assert_eq!(
        report.unrecognized_partitions().to_vec(),
        vec![mod_id("org.legacy.retired")]
    );

    // Retained verbatim: the check never mutates or deletes carried data.
    let kept = layout
        .get(&mod_id("org.legacy.retired"))
        .expect("unrecognized partition is retained");
    assert_eq!(kept.payload.as_bytes(), b"legacy-bytes");
}

#[test]
fn layout_matching_the_manifest_is_consistent() {
    let manifest = valid_manifest();
    let layout = SaveLayout::new(vec![
        SavePartition::new(SavePartitionId::Manifest, b"manifest"),
        SavePartition::new(SavePartitionId::World, b"world"),
        SavePartition::new(mod_id("org.example.base"), b"base"),
        SavePartition::new(mod_id("org.example.economy"), b"economy"),
    ]);
    let report = layout.check_against(&manifest).expect("consistency check");
    assert!(report.is_consistent());
    assert!(report.missing_mod_partitions().is_empty());
    assert!(report.unrecognized_partitions().is_empty());
}

#[test]
fn check_against_reports_an_invalid_manifest_mod_id() {
    let mut manifest = valid_manifest();
    manifest.mods[1].id = "Org/Example".to_owned();
    let layout = SaveLayout::default();
    let error = layout
        .check_against(&manifest)
        .expect_err("an invalid manifest mod id must be reported");
    assert_eq!(error.code(), PartitionErrorCode::InvalidNamespace);
    assert_eq!(error.path(), "/mods/1/id");
}

// ---------------------------------------------------------------------------
// Acceptance 6 — determinism
// ---------------------------------------------------------------------------

#[test]
fn canonical_ordering_is_independent_of_insertion_order() {
    let partitions = vec![
        SavePartition::new(SavePartitionId::Integrity, b"i"),
        SavePartition::new(mod_id("org.example.zeta"), b"z"),
        SavePartition::new(SavePartitionId::World, b"w"),
        SavePartition::new(mod_id("org.example.alpha"), b"a"),
        SavePartition::new(SavePartitionId::Metadata, b"m"),
        SavePartition::new(SavePartitionId::Manifest, b"c"),
    ];
    let mut reversed = partitions.clone();
    reversed.reverse();
    let mut shuffled = partitions.clone();
    shuffled.swap(0, 3);
    shuffled.swap(1, 4);

    let encoded = encode(&SaveLayout::new(partitions.clone()));
    assert_eq!(encode(&SaveLayout::new(reversed)), encoded);
    assert_eq!(encode(&SaveLayout::new(shuffled)), encoded);

    // The canonical order is observable and stable.
    let layout = SaveLayout::new(partitions);
    let kinds: Vec<&str> = layout
        .partitions()
        .iter()
        .map(|partition| partition.id.kind_str())
        .collect();
    assert_eq!(
        kinds,
        vec!["manifest", "world", "mod", "mod", "metadata", "integrity"]
    );
    let namespaces: Vec<&str> = layout
        .mod_partitions()
        .map(|partition| partition.id.namespace().expect("mod").as_str())
        .collect();
    assert_eq!(namespaces, vec!["org.example.alpha", "org.example.zeta"]);
}

// ---------------------------------------------------------------------------
// Requirement 4 — the world partition is raw snapshot bytes, never a version
// ---------------------------------------------------------------------------

#[test]
fn world_partition_carries_raw_snapshot_bytes_and_never_derives_a_version() {
    let manifest = valid_manifest();
    // Bytes that look nothing like a `MAJOR.MINOR` version.
    let snapshot = vec![0x53, 0x4E, 0x41, 0x50, 0x02, 0x00, 0xFF, 0x7F, 0x00];

    let layout = SaveLayout::new(vec![SavePartition::new(
        SavePartitionId::World,
        snapshot.clone(),
    )]);
    let decoded = SaveLayout::from_bytes(&encode(&layout)).expect("layout decodes");
    let world = decoded
        .get(&SavePartitionId::World)
        .expect("world partition is carried");
    assert_eq!(world.owner, PartitionOwner::Kernel);
    assert_eq!(world.payload.as_bytes(), snapshot.as_slice());
    assert_ne!(world.payload.as_bytes(), b"1.0");

    // The Manifest's declared world Schema version is untouched and unrelated.
    assert_eq!(manifest.world_schema_version, "1.0");
    assert!(world.schema_version.is_none());
}

// ---------------------------------------------------------------------------
// Acceptance 7 — stable, machine-readable error codes and logical pointers
// ---------------------------------------------------------------------------

#[test]
fn duplicate_partition_identities_are_rejected() {
    let layout = SaveLayout::new(vec![
        SavePartition::new(SavePartitionId::World, b"first"),
        SavePartition::new(SavePartitionId::World, b"second"),
    ]);
    let error = layout
        .validate()
        .expect_err("duplicate partitions must be rejected");
    assert_eq!(error.code(), PartitionErrorCode::DuplicatePartition);
    assert_eq!(error.code_str(), "save.duplicate_partition");
    assert_eq!(error.path(), "/partitions/1/id");
}

#[test]
fn malformed_layout_bytes_are_rejected_with_stable_codes() {
    // Foreign tag.
    assert_eq!(
        SaveLayout::from_bytes(b"not a layout").unwrap_err().code(),
        PartitionErrorCode::InvalidLayout
    );

    let good = encode(&SaveLayout::new(vec![SavePartition::new(
        SavePartitionId::World,
        b"abc",
    )]));

    // Truncated stream.
    let truncated = SaveLayout::from_bytes(&good[..good.len() - 1]).unwrap_err();
    assert_eq!(truncated.code(), PartitionErrorCode::InvalidLayout);

    // Trailing bytes.
    let mut with_trailer = good.clone();
    with_trailer.push(0);
    assert_eq!(
        SaveLayout::from_bytes(&with_trailer).unwrap_err().code(),
        PartitionErrorCode::InvalidLayout
    );

    // Unsupported encoding version.
    let mut bad_version = good.clone();
    bad_version[2] = 0xFF;
    assert_eq!(
        SaveLayout::from_bytes(&bad_version).unwrap_err().code(),
        PartitionErrorCode::InvalidLayout
    );
}

#[test]
fn an_unknown_owner_tag_is_rejected_during_decode() {
    let bytes = encode(&SaveLayout::new(vec![SavePartition::new(
        SavePartitionId::World,
        b"abc",
    )]));
    let mut tampered = bytes.clone();
    // header (2 tag + 1 version + 4 count) + kind (1) => the owner tag is at index 8
    tampered[8] = 9;
    let error = SaveLayout::from_bytes(&tampered).unwrap_err();
    assert_eq!(error.code(), PartitionErrorCode::InvalidPartition);
}

#[test]
fn partition_error_codes_are_disjoint_from_save_001_codes() {
    // Pin the frozen SAVE-001 codes.
    let save_001 = [
        SaveManifestErrorCode::InvalidManifest,
        SaveManifestErrorCode::MissingField,
        SaveManifestErrorCode::InvalidField,
        SaveManifestErrorCode::UnsupportedFormatVersion,
        SaveManifestErrorCode::InvalidModEntry,
        SaveManifestErrorCode::DuplicateModId,
        SaveManifestErrorCode::InvalidSchema,
    ]
    .map(SaveManifestErrorCode::as_str);

    let save_002 = [
        PartitionErrorCode::InvalidPartition,
        PartitionErrorCode::InvalidLayout,
        PartitionErrorCode::InvalidNamespace,
        PartitionErrorCode::PartitionOwnershipViolation,
        PartitionErrorCode::DuplicatePartition,
    ];

    for code in save_002 {
        let wire = code.as_str();
        assert!(wire.starts_with("save."), "{wire}");
        assert!(
            !save_001.contains(&wire),
            "{wire} collides with a SAVE-001 code"
        );
    }

    // Distinct within SAVE-002 as well.
    let wires: Vec<&str> = save_002.iter().map(|code| code.as_str()).collect();
    let unique: std::collections::BTreeSet<&str> = wires.iter().copied().collect();
    assert_eq!(wires.len(), unique.len());

    // The frozen wire spellings.
    assert_eq!(save_002[0].as_str(), "save.invalid_partition");
    assert_eq!(save_002[1].as_str(), "save.invalid_layout");
    assert_eq!(save_002[2].as_str(), "save.invalid_namespace");
    assert_eq!(save_002[3].as_str(), "save.partition_ownership_violation");
    assert_eq!(save_002[4].as_str(), "save.duplicate_partition");
}

#[test]
fn errors_carry_a_logical_pointer() {
    let namespace_error = ModNamespace::parse("Bad/Name").unwrap_err();
    assert_eq!(namespace_error.path(), "/namespace");
    assert!(!namespace_error.message().is_empty());

    let layout_error = SaveLayout::from_bytes(b"x").unwrap_err();
    assert_eq!(layout_error.path(), "/partitions");
    assert!(!layout_error.message().is_empty());
}

// ---------------------------------------------------------------------------
// Payload carrier
// ---------------------------------------------------------------------------

#[test]
fn payload_is_opaque_and_reports_its_length() {
    let payload = PartitionPayload::new(vec![1, 2, 3]);
    assert_eq!(payload.len(), 3);
    assert!(!payload.is_empty());
    assert_eq!(payload.as_bytes(), &[1, 2, 3]);
    assert_eq!(payload.clone().into_bytes(), vec![1, 2, 3]);
    // Debug renders the length, not the raw bytes.
    assert_eq!(format!("{payload:?}"), "PartitionPayload(3 bytes)");
}

#[test]
fn an_empty_layout_reports_every_manifest_mod_as_missing() {
    let manifest = valid_manifest();
    let report: PartitionConsistency = SaveLayout::default()
        .check_against(&manifest)
        .expect("consistency check");
    let missing: Vec<&str> = report
        .missing_mod_partitions()
        .iter()
        .map(ModNamespace::as_str)
        .collect();
    assert_eq!(missing, vec!["org.example.base", "org.example.economy"]);
    assert!(report.unrecognized_partitions().is_empty());
    assert!(!report.is_consistent());
}

// ---------------------------------------------------------------------------
// Hardening R1/R3 — P-1: serialization is fallible; an over-long `Schema
// version` fails with a stable error instead of being truncated or panicking
// ---------------------------------------------------------------------------

/// Builds the 7-byte canonical layout header (tag `SL`, version 1, `u32` count)
/// so decode-negative tests can append hand-crafted record bytes.
fn layout_header(count: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"SL");
    bytes.push(1); // canonical layout encoding version
    bytes.extend_from_slice(&count.to_be_bytes());
    bytes
}

#[test]
fn oversized_schema_version_fails_serialization() {
    let oversized = "9".repeat(MAX_SCHEMA_VERSION_LEN + 1);

    // Reached through every public construction path (builder + `SaveLayout::new`).
    let layout = SaveLayout::new(vec![
        SavePartition::new(mod_id("org.example.base"), b"payload").with_schema_version(oversized),
    ]);

    // Validation rejects it with a stable code and logical pointer.
    let error = layout.validate().unwrap_err();
    assert_eq!(error.code(), PartitionErrorCode::InvalidPartition);
    assert_eq!(error.code_str(), "save.invalid_partition");
    assert_eq!(error.path(), "/partitions/0/schema_version");

    // Serialization is fallible: it reports the same stable error instead of
    // panicking or silently truncating the string, and emits no partial stream.
    let error = layout.to_bytes().unwrap_err();
    assert_eq!(error.code(), PartitionErrorCode::InvalidPartition);
    assert_eq!(error.code_str(), "save.invalid_partition");
    assert_eq!(error.path(), "/partitions/0/schema_version");
}

#[test]
fn duplicate_partition_ids_fail_serialization() {
    let layout = SaveLayout::new(vec![
        SavePartition::new(SavePartitionId::World, b"first"),
        SavePartition::new(SavePartitionId::World, b"second"),
    ]);
    let error = layout.to_bytes().unwrap_err();
    assert_eq!(error.code(), PartitionErrorCode::DuplicatePartition);
    assert_eq!(error.code_str(), "save.duplicate_partition");
    assert_eq!(error.path(), "/partitions/1/id");
}

#[test]
fn ownership_violation_fails_serialization() {
    let layout = SaveLayout::new(vec![SavePartition::with_owner(
        mod_id("org.example.base"),
        PartitionOwner::Mod(namespace("org.example.other")),
        b"payload",
    )]);
    let error = layout.to_bytes().unwrap_err();
    assert_eq!(
        error.code(),
        PartitionErrorCode::PartitionOwnershipViolation
    );
    assert_eq!(error.code_str(), "save.partition_ownership_violation");
    assert_eq!(error.path(), "/partitions/0/owner");
}

#[test]
fn canonical_v1_encoding_bytes_are_unchanged() {
    // The v1 canonical layout encoding must not change under the fallible
    // serializer: `SL` tag + version 1 + `u32` count + per-partition records.
    let world = SaveLayout::new(vec![SavePartition::new(SavePartitionId::World, b"abc")]);
    assert_eq!(
        encode(&world),
        vec![
            0x53, 0x4C, // "SL"
            0x01, // layout encoding version
            0x00, 0x00, 0x00, 0x01, // partition count
            0x01, // kind = World
            0x01, // owner = Kernel
            0x00, // no Schema version
            0x00, 0x00, 0x00, 0x03, // payload length
            0x61, 0x62, 0x63, // "abc"
        ]
    );

    let mod_layout = SaveLayout::new(vec![
        SavePartition::new(mod_id("a"), b"x").with_schema_version("1"),
    ]);
    assert_eq!(
        encode(&mod_layout),
        vec![
            0x53, 0x4C, 0x01, // "SL", version 1
            0x00, 0x00, 0x00, 0x01, // partition count
            0x02, 0x00, 0x01, 0x61, // kind = Mod "a"
            0x02, 0x00, 0x01, 0x61, // owner = Mod "a"
            0x01, 0x00, 0x01, 0x31, // Schema version present, "1"
            0x00, 0x00, 0x00, 0x01, 0x78, // payload length 1, "x"
        ]
    );
}

#[test]
fn schema_version_at_the_bound_is_accepted_and_round_trips() {
    let at_bound = "9".repeat(MAX_SCHEMA_VERSION_LEN);
    let layout = SaveLayout::new(vec![
        SavePartition::new(mod_id("org.example.base"), b"payload")
            .with_schema_version(at_bound.clone()),
    ]);

    layout
        .validate()
        .expect("a Schema version at the bound is valid");

    let decoded = SaveLayout::from_bytes(&encode(&layout)).expect("layout decodes");
    let restored = decoded
        .get(&mod_id("org.example.base"))
        .expect("partition is carried");
    assert_eq!(restored.schema_version.as_deref(), Some(at_bound.as_str()));
}

// ---------------------------------------------------------------------------
// Hardening R1 — P-2: a declared count cannot force a large allocation
// ---------------------------------------------------------------------------

#[test]
fn a_declared_count_beyond_the_bytes_is_rejected_before_allocation() {
    // Declares four partitions but carries only seven bytes, which can encode at
    // most one partition (seven bytes each). It must be rejected on the count
    // before any per-partition capacity is reserved.
    let mut bytes = layout_header(4);
    bytes.extend_from_slice(&[0u8; 7]);
    let error = SaveLayout::from_bytes(&bytes).unwrap_err();
    assert_eq!(error.code(), PartitionErrorCode::InvalidLayout);
    assert_eq!(error.code_str(), "save.invalid_layout");
    assert_eq!(error.path(), "/partitions");
}

#[test]
fn the_count_bound_accepts_a_layout_of_minimal_records() {
    // Two partitions with empty payloads encode to exactly seven bytes each, so
    // a declared count of two against fourteen remaining bytes is the tight
    // boundary and must still decode.
    let layout = SaveLayout::new(vec![
        SavePartition::new(SavePartitionId::World, Vec::new()),
        SavePartition::new(SavePartitionId::Metadata, Vec::new()),
    ]);
    let bytes = encode(&layout);
    assert_eq!(bytes.len(), 7 + 7 + 7);

    let decoded = SaveLayout::from_bytes(&bytes).expect("minimal records decode");
    assert_eq!(decoded, layout);
}

// ---------------------------------------------------------------------------
// Hardening R1 — P-5: decode-negative branches not previously pinned
// ---------------------------------------------------------------------------

#[test]
fn an_unknown_partition_kind_tag_is_rejected_during_decode() {
    let mut bytes = layout_header(1);
    bytes.extend_from_slice(&[9, 0, 0, 0, 0, 0, 0]); // kind tag 9 + padding
    let error = SaveLayout::from_bytes(&bytes).unwrap_err();
    assert_eq!(error.code(), PartitionErrorCode::InvalidPartition);
    // Logical pointer follows the documented `/partitions/<index>/<field>` form.
    assert_eq!(error.path(), "/partitions/0/kind");
}

#[test]
fn a_non_utf8_string_field_is_rejected_during_decode() {
    let mut bytes = layout_header(1);
    bytes.push(1); // kind = World
    bytes.push(1); // owner = Kernel
    bytes.push(1); // Schema version present
    bytes.extend_from_slice(&[0x00, 0x01]); // string length = 1
    bytes.push(0xFF); // not valid UTF-8
    bytes.extend_from_slice(&[0, 0, 0, 0]); // payload length (never read)
    let error = SaveLayout::from_bytes(&bytes).unwrap_err();
    assert_eq!(error.code(), PartitionErrorCode::InvalidLayout);
    assert_eq!(error.path(), "/partitions/0/schema_version");
}

#[test]
fn an_unknown_schema_version_flag_is_rejected_during_decode() {
    let mut bytes = layout_header(1);
    bytes.push(1); // kind = World
    bytes.push(1); // owner = Kernel
    bytes.push(7); // unknown Schema version flag
    bytes.extend_from_slice(&[0, 0, 0, 0]); // payload length (never read)
    let error = SaveLayout::from_bytes(&bytes).unwrap_err();
    assert_eq!(error.code(), PartitionErrorCode::InvalidPartition);
    assert_eq!(error.path(), "/partitions/0/schema_version");
}

#[test]
fn a_duplicate_partition_id_is_rejected_when_decoding() {
    // `to_bytes` now refuses to encode duplicates (`duplicate_partition_ids_fail_serialization`),
    // so the stream is hand-crafted: two well-formed World records that share an
    // identity. Decoding must reject it (equivalently to `validate`), not
    // deduplicate silently.
    let mut bytes = layout_header(2);
    bytes.extend_from_slice(&[1, 1, 0, 0, 0, 0, 0]); // World, Kernel, no schema, empty payload
    bytes.extend_from_slice(&[1, 1, 0, 0, 0, 0, 0]);

    let error = SaveLayout::from_bytes(&bytes).unwrap_err();
    assert_eq!(error.code(), PartitionErrorCode::DuplicatePartition);
    assert_eq!(error.code_str(), "save.duplicate_partition");
    assert_eq!(error.path(), "/partitions/1/id");
}
