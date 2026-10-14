//! Contract tests for `SAVE-003` (`GK-P4-SAVE-003-IMPL-01`).
//!
//! The golden vectors are the frozen values of
//! `planning/29-P4-SAVE-003-完整性摘要与校验设计提案.md` `§7.2`. They were
//! derived with three mutually independent implementations (Python `hashlib`,
//! GNU `sha256sum`, OpenSSL `dgst`) — see `§7.1` — and are reproduced here as
//! literals so the suite does not depend on that document being present.

use save_manager::integrity::{
    CHECKSUMS_ENCODING_TAG, CHECKSUMS_ENCODING_VERSION, ChecksumRecord, DIGEST_TEXT_LEN,
    GeneratedIntegrity, IntegrityError, IntegrityErrorCode, IntegrityReport,
    MIN_ENCODED_CHECKSUM_ENTRY_LEN, PARTITION_DIGEST_DOMAIN, PartitionIntegrity,
    ROOT_DIGEST_DOMAIN, decode_checksums, encode_checksums, generate, layout_root,
    manifest_canonical_bytes, partition_digest, verify,
};
use save_manager::partition::{
    ModNamespace, PartitionErrorCode, PartitionOwner, SaveLayout, SavePartition, SavePartitionId,
};
use save_manager::{SUPPORTED_INTEGRITY_ALGORITHM, SaveManifest, validate_manifest_value};
use sha2::{Digest, Sha256};

// ---------------------------------------------------------------------------
// Golden vectors (`§7.2`)
// ---------------------------------------------------------------------------

/// `PARTITION_DIGEST_DOMAIN` (27 bytes).
const PART_DOMAIN_HEX: &str = "474b534156452d504152544954494f4e2d4449474553542d76311f";
/// `ROOT_DIGEST_DOMAIN` (22 bytes).
const ROOT_DOMAIN_HEX: &str = "474b534156452d524f4f542d4449474553542d76311f";

/// The canonical layout encoding of `[World("abc")]`.
const LAYOUT_WORLD_ABC_HEX: &str = "534c010000000101010000000003616263";
/// The canonical layout encoding of an empty layout.
const LAYOUT_EMPTY_HEX: &str = "534c0100000000";
/// The `World("abc")` partition record (no layout header).
const WORLD_ABC_RECORD_HEX: &str = "01010000000003616263";
/// The `World("W")` partition record (the `G5` input).
const WORLD_W_RECORD_HEX: &str = "0101000000000157";
/// The `World("w")` partition record — the same record with a lowercase payload
/// byte (the `G7-world` input).
const WORLD_LOWER_W_RECORD_HEX: &str = "0101000000000177";
/// The `Mod("org.example.base", "m", schema "1.0")` partition record.
const MOD_BASE_RECORD_HEX: &str = concat!(
    "0200106f72672e6578616d706c652e62617365",
    "0200106f72672e6578616d706c652e62617365",
    "010003312e30",
    "000000016d"
);

const G1_DIGEST: &str = "949240706395ac760a095c4aa76ce67b82947a0ce8ee6979929aff31308c4de0";
const G2_DIGEST: &str = "b16ed0bef3224ab1d79d319d0bd3c3989bb29030f179c9317ad6513f5545d0cb";
const G3_DIGEST: &str = "7e09c76dbe0cf40a13afd408c796760c1b55ff396748bb167ba35dd4ee9866cb";
const G4_DIGEST: &str = "1e9d8c879f10213af6f63a0e66cbd06128ebb40498b78bebbc9b5bc2fc64c505";
const G5_DIGEST: &str = "7370704e554032e361a0fe790dd3756fa82ea7201cbd16d72404a8e99dadc2b8";
const G6_DIGEST: &str = "e2279ee1d9610087db8feb0d0015c0680bb15f72a8556be5d088d192ba7cfb0c";
const G7_MANIFEST_DIGEST: &str = "a5cdf0dfc9df4793e905c5bc6b7b2edb10aa627c600e9417c9bc9adc27cdc76b";
const G7_WORLD_DIGEST: &str = "d0d3b4464c8de99680ff01e2245b32b0b169b21dd754c938e8940fe37c3faad6";
const G7_MOD_DIGEST: &str = "4a50c01c8a943cf1b7fd9d0fe66bae8c3cd292ef82fb52679c8476bcd4e5680f";
const G8_DIGEST: &str = "818b98044fd8e1b924988e2225c578fbd2b00eec09311001e0b15e3664eeb3ee";
const G9_DIGEST: &str = "837370244b0a72272abe46d07542d1be145611313a1691dd9777c8c26fa83733";

const G6_LEN: usize = 493;
const G4_LEN: usize = 586;
const G8_LEN: usize = 292;
const G9_LEN: usize = 79;

/// The 64-character all-zero digest used as the canonical-bytes sentinel.
const CANONICAL_ROOT_HASH: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";
/// A valid, non-sentinel digest used as a carried `root_hash`.
const CARRIED_ROOT_HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn from_hex(text: &str) -> Vec<u8> {
    assert!(
        text.len().is_multiple_of(2),
        "hex literal must have an even length"
    );
    (0..text.len() / 2)
        .map(|index| u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).unwrap())
        .collect()
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    to_hex(&hasher.finalize())
}

fn domain_digest(domain: &[u8], payload: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(payload);
    to_hex(&hasher.finalize())
}

fn namespace(value: &str) -> ModNamespace {
    ModNamespace::parse(value).expect("fixture namespace is valid")
}

/// The fixture Manifest JSON with `root_hash` filled in.
fn manifest_json(root_hash: &str) -> String {
    let mut json = String::from(concat!(
        r#"{"save_format_version":"1.0","save_id":"s","created_at":"2026-10-15T00:00:00Z","#,
        r#""last_saved_at":"2026-10-15T00:00:00Z","platform_version":"0.1.0","#,
        r#""game_definition":{"id":"org.example.game","version":"1.0.0"},"#,
        r#""kernel_api_version":"1.0","world_schema_version":"1.0","logical_tick":0,"#,
        r#""mods":[{"id":"org.example.base","version":"1.0.0","schema_version":"1.0"}],"#,
        r#""simulation_config":{},"integrity":{"algorithm":"sha256","root_hash":""#
    ));
    json.push_str(root_hash);
    json.push_str(r#""}}"#);
    json
}

/// `CMB`: the canonical Manifest bytes (sentinel `root_hash`).
fn golden_cmb() -> Vec<u8> {
    manifest_json(CANONICAL_ROOT_HASH).into_bytes()
}

fn manifest_partition(root_hash: &str) -> SavePartition {
    SavePartition::new(
        SavePartitionId::Manifest,
        manifest_json(root_hash).into_bytes(),
    )
}

fn world_partition(payload: &[u8]) -> SavePartition {
    SavePartition::new(SavePartitionId::World, payload.to_vec())
}

fn base_mod_partition(payload: &[u8]) -> SavePartition {
    SavePartition::new(
        SavePartitionId::Mod(namespace("org.example.base")),
        payload.to_vec(),
    )
    .with_schema_version("1.0")
}

fn integrity_partition(payload: impl Into<Vec<u8>>) -> SavePartition {
    SavePartition::new(SavePartitionId::Integrity, payload)
}

/// The record the frozen `SAVE-002` encoder writes for one partition.
fn record_of(partition: &SavePartition) -> Vec<u8> {
    let encoded = SaveLayout::new(vec![partition.clone()])
        .to_bytes()
        .expect("fixture partition is encodable");
    encoded[7..].to_vec() // drop the fixed `SL` ‖ version ‖ count header
}

fn code_of(error: &IntegrityError) -> &'static str {
    error.code_str()
}

// ---------------------------------------------------------------------------
// T-1 / T-1b / T-1c: golden digests and canonical bytes
// ---------------------------------------------------------------------------

/// T-1: the per-partition digest covers identity, metadata and payload.
#[test]
fn t01_partition_digest_matches_the_frozen_golden() {
    let partition = world_partition(b"abc");
    assert_eq!(
        to_hex(&record_of(&partition)),
        WORLD_ABC_RECORD_HEX,
        "the frozen encoder must write the documented record"
    );
    assert_eq!(
        partition_digest(&partition).unwrap(),
        domain_digest(&from_hex(PART_DOMAIN_HEX), &from_hex(WORLD_ABC_RECORD_HEX))
    );
    assert_eq!(partition_digest(&partition).unwrap(), G1_DIGEST);
}

/// T-1b: the record is independent of the partition's position in the layout.
#[test]
fn t01b_partition_record_has_no_position() {
    let world_w = world_partition(b"W");
    assert_eq!(to_hex(&record_of(&world_w)), WORLD_W_RECORD_HEX);
    assert_eq!(partition_digest(&world_w).unwrap(), G5_DIGEST);

    // The recorded owner and Schema version travel in the record.
    let mod_base = base_mod_partition(b"m");
    assert_eq!(to_hex(&record_of(&mod_base)), MOD_BASE_RECORD_HEX);
    assert_eq!(
        domain_digest(&from_hex(PART_DOMAIN_HEX), &from_hex(MOD_BASE_RECORD_HEX)),
        G7_MOD_DIGEST
    );
}

/// T-1c: the canonical Manifest bytes equal the frozen `G6` literal.
#[test]
fn t01c_manifest_canonical_bytes_match_the_frozen_golden() {
    let cmb = manifest_canonical_bytes(manifest_json(CARRIED_ROOT_HASH).as_bytes()).unwrap();
    assert_eq!(cmb, golden_cmb(), "CMB must equal the documented literal");
    assert_eq!(cmb.len(), G6_LEN);
    assert_eq!(sha256_hex(&cmb), G6_DIGEST);
}

// ---------------------------------------------------------------------------
// T-2 / T-3 / T-4: root, domains, degenerate layouts
// ---------------------------------------------------------------------------

/// T-2: the root is `ROOT_DOMAIN` ‖ canonical layout encoding.
#[test]
fn t02_layout_root_matches_the_frozen_golden() {
    let layout = SaveLayout::new(vec![world_partition(b"abc")]);
    assert_eq!(to_hex(&layout.to_bytes().unwrap()), LAYOUT_WORLD_ABC_HEX);
    assert_eq!(
        layout_root(&layout).unwrap(),
        domain_digest(&from_hex(ROOT_DOMAIN_HEX), &from_hex(LAYOUT_WORLD_ABC_HEX))
    );
    assert_eq!(layout_root(&layout).unwrap(), G2_DIGEST);
}

/// T-3: the two domains are separated, so a record cannot be replayed.
#[test]
fn t03_digest_domains_are_separated() {
    assert_eq!(to_hex(PARTITION_DIGEST_DOMAIN), PART_DOMAIN_HEX);
    assert_eq!(to_hex(ROOT_DIGEST_DOMAIN), ROOT_DOMAIN_HEX);
    assert_ne!(PARTITION_DIGEST_DOMAIN, ROOT_DIGEST_DOMAIN);

    let record = from_hex(WORLD_ABC_RECORD_HEX);
    assert_ne!(
        domain_digest(PARTITION_DIGEST_DOMAIN, &record),
        domain_digest(ROOT_DIGEST_DOMAIN, &record),
        "the same record must not hash identically under both domains"
    );
    // And the partition digest must not equal the digest of the bare payload.
    assert_ne!(
        partition_digest(&world_partition(b"abc")).unwrap(),
        sha256_hex(b"abc")
    );
}

/// T-4: an empty layout has a defined root; an empty checksums payload is valid.
#[test]
fn t04_empty_layout_and_empty_checksums_are_defined() {
    let empty = SaveLayout::default();
    assert!(empty.is_empty());
    assert_eq!(to_hex(&empty.to_bytes().unwrap()), LAYOUT_EMPTY_HEX);
    assert_eq!(layout_root(&empty).unwrap(), G3_DIGEST);

    let checksums = encode_checksums(G3_DIGEST, &[]).unwrap();
    assert_eq!(checksums.len(), G9_LEN);
    assert_eq!(sha256_hex(&checksums), G9_DIGEST);
    let decoded = decode_checksums(&checksums).unwrap();
    assert_eq!(decoded.root_hash, G3_DIGEST);
    assert!(decoded.entries.is_empty());
    assert_eq!(
        decoded.algorithm,
        save_manager::SUPPORTED_INTEGRITY_ALGORITHM
    );
}

// ---------------------------------------------------------------------------
// T-5 / T-6: Integrity exclusion and order independence
// ---------------------------------------------------------------------------

/// T-5: a layout carrying only the Integrity partition roots like an empty one,
/// and integrity cannot be established without a Manifest.
#[test]
fn t05_integrity_partition_is_excluded_from_the_root() {
    let only_integrity = SaveLayout::new(vec![integrity_partition(b"anything")]);
    assert_eq!(layout_root(&only_integrity).unwrap(), G3_DIGEST);
    assert_eq!(
        code_of(&generate(&only_integrity).unwrap_err()),
        "save.integrity_missing"
    );
    assert_eq!(
        code_of(&verify(&only_integrity).unwrap_err()),
        "save.integrity_missing"
    );

    // The Integrity payload never influences the root of a richer layout.
    let base = vec![
        manifest_partition(CARRIED_ROOT_HASH),
        world_partition(b"w"),
        base_mod_partition(b"m"),
    ];
    let mut with_integrity = base.clone();
    with_integrity.push(integrity_partition(b"stale"));
    let mut with_other_integrity = base.clone();
    with_other_integrity.push(integrity_partition(b"different"));
    assert_eq!(
        layout_root(&SaveLayout::new(with_integrity)).unwrap(),
        layout_root(&SaveLayout::new(with_other_integrity)).unwrap()
    );
}

/// T-6: the root does not depend on the insertion order — and the canonical
/// order is load-bearing (the falsification below shows it is not automatic).
#[test]
fn t06_root_is_independent_of_insertion_order() {
    let manifest = manifest_partition(CARRIED_ROOT_HASH);
    let world = world_partition(b"w");
    let mod_base = base_mod_partition(b"m");
    let options = [&manifest, &world, &mod_base];

    let mut seen = 0;
    for first in [0usize, 1, 2] {
        for second in [0usize, 1, 2] {
            for third in [0usize, 1, 2] {
                if first == second || second == third || first == third {
                    continue;
                }
                seen += 1;
                let layout = SaveLayout::new(vec![
                    options[first].clone(),
                    options[second].clone(),
                    options[third].clone(),
                ]);
                assert_eq!(
                    layout_root(&layout).unwrap(),
                    G4_DIGEST,
                    "permutation ({first}, {second}, {third}) must root identically"
                );
            }
        }
    }
    assert_eq!(seen, 6, "three distinct partitions");

    // The root is computed over the *canonical* bytes — the Manifest payload
    // replaced by `CMB` — not over the payload as stored.
    let cmb_partition = SavePartition::new(
        SavePartitionId::Manifest,
        manifest_canonical_bytes(manifest_json(CARRIED_ROOT_HASH).as_bytes()).unwrap(),
    );
    let canonical = [
        from_hex(ROOT_DOMAIN_HEX),
        from_hex("534c0100000003"),
        record_of(&cmb_partition),
        record_of(&world),
        record_of(&mod_base),
    ]
    .concat();
    assert_eq!(canonical.len(), G4_LEN);
    assert_eq!(sha256_hex(&canonical), G4_DIGEST);

    // Falsification: the same three records concatenated in a non-canonical
    // order hash differently, so order independence comes from the sort and is
    // not automatic.
    let unsorted = [
        from_hex(ROOT_DOMAIN_HEX),
        from_hex("534c0100000003"),
        record_of(&cmb_partition),
        record_of(&mod_base),
        record_of(&world),
    ]
    .concat();
    assert_eq!(unsorted.len(), canonical.len());
    assert_ne!(sha256_hex(&unsorted), G4_DIGEST);
}

// ---------------------------------------------------------------------------
// Fixtures for the sealed-save tests
// ---------------------------------------------------------------------------

/// A layout whose Manifest records the generated root and whose Integrity
/// partition holds the matching `SC` payload, built only through the public API.
fn seal(partitions: Vec<SavePartition>) -> SaveLayout {
    let mut scratch = partitions.clone();
    scratch.push(SavePartition::new(
        SavePartitionId::Integrity,
        Vec::<u8>::new(),
    ));
    let generated = generate(&SaveLayout::new(scratch)).expect("fixture layout generates");

    let mut sealed = Vec::with_capacity(partitions.len() + 1);
    for partition in partitions {
        if partition.id == SavePartitionId::Manifest {
            let mut manifest = SaveManifest::from_bytes(partition.payload.as_bytes())
                .expect("fixture manifest parses");
            manifest.integrity.root_hash = generated.root.clone();
            sealed.push(SavePartition::new(
                SavePartitionId::Manifest,
                manifest.to_bytes(),
            ));
        } else {
            sealed.push(partition);
        }
    }
    sealed.push(SavePartition::new(
        SavePartitionId::Integrity,
        generated.checksums,
    ));
    SaveLayout::new(sealed)
}

/// The three-partition body (Manifest, World, one Mod) most tests start from.
fn body() -> Vec<SavePartition> {
    vec![
        manifest_partition(CARRIED_ROOT_HASH),
        world_partition(b"w"),
        base_mod_partition(b"m"),
    ]
}

/// `body()` sealed with a correct root and matching checksums.
fn sealed() -> SaveLayout {
    seal(body())
}

fn index_of(layout: &SaveLayout, id: &SavePartitionId) -> usize {
    layout
        .partitions
        .iter()
        .position(|partition| &partition.id == id)
        .unwrap_or_else(|| panic!("layout carries no `{}` partition", id.logical_path()))
}

/// Replaces one partition, leaving the recorded integrity metadata untouched.
fn with_partition(
    layout: &SaveLayout,
    id: &SavePartitionId,
    replacement: SavePartition,
) -> SaveLayout {
    let mut partitions = layout.partitions.clone();
    partitions[index_of(layout, id)] = replacement;
    SaveLayout::new(partitions)
}

/// Replaces the Integrity payload, leaving every other byte alone.
fn with_checksums(layout: &SaveLayout, payload: Vec<u8>) -> SaveLayout {
    with_partition(
        layout,
        &SavePartitionId::Integrity,
        SavePartition::new(SavePartitionId::Integrity, payload),
    )
}

fn status_of<'a>(report: &'a IntegrityReport, id: &SavePartitionId) -> &'a PartitionIntegrity {
    &report
        .partitions
        .iter()
        .find(|(other, _)| other == id)
        .unwrap_or_else(|| panic!("report has no entry for `{}`", id.logical_path()))
        .1
}

/// The `(id, digest)` list the sealed fixture records, in canonical order.
fn sealed_entries() -> Vec<(SavePartitionId, String)> {
    vec![
        (SavePartitionId::Manifest, G7_MANIFEST_DIGEST.to_owned()),
        (SavePartitionId::World, G7_WORLD_DIGEST.to_owned()),
        (
            SavePartitionId::Mod(namespace("org.example.base")),
            G7_MOD_DIGEST.to_owned(),
        ),
    ]
}

/// An `SC` v1 payload assembled by hand, so the encoder's own validation cannot
/// stand in for the decoder's.
fn hand_built_checksums(algorithm: &str, root: &str, count: u32, entries: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&CHECKSUMS_ENCODING_TAG);
    out.push(CHECKSUMS_ENCODING_VERSION);
    out.extend_from_slice(
        &u16::try_from(algorithm.len())
            .expect("fixture algorithm fits a u16")
            .to_be_bytes(),
    );
    out.extend_from_slice(algorithm.as_bytes());
    out.extend_from_slice(root.as_bytes());
    out.extend_from_slice(&count.to_be_bytes());
    out.extend_from_slice(entries);
    out
}

/// Byte offset of the `count` field and of the first entry in an `SC` payload.
fn entry_region(algorithm: &str) -> (usize, usize) {
    let count_offset = CHECKSUMS_ENCODING_TAG.len() + 1 + 2 + algorithm.len() + DIGEST_TEXT_LEN;
    (count_offset, count_offset + 4)
}

// ---------------------------------------------------------------------------
// T-7: tampering with a carried partition
// ---------------------------------------------------------------------------

/// T-7: a change to a partition's payload, identity or recorded metadata moves
/// its digest, and the report locates exactly the partition that moved.
#[test]
fn t07_any_partition_change_is_detected_and_located() {
    let baseline = sealed();
    let report = verify(&baseline).unwrap();
    assert!(report.is_intact(), "the sealed fixture verifies");
    assert_eq!(report.partitions.len(), 3, "Manifest, World and one Mod");

    // (a) the payload bytes
    let tampered = with_partition(&baseline, &SavePartitionId::World, world_partition(b"x"));
    let report = verify(&tampered).unwrap();
    assert!(!report.is_intact());
    assert!(
        !report.root_matches,
        "the layout root moves with the payload"
    );
    assert_eq!(
        status_of(&report, &SavePartitionId::World),
        &PartitionIntegrity::Mismatch {
            expected: G7_WORLD_DIGEST.to_owned(),
            actual: partition_digest(&world_partition(b"x")).unwrap(),
        }
    );
    let corrupted: Vec<&SavePartitionId> =
        report.corrupted_partitions().map(|(id, _)| id).collect();
    assert_eq!(
        corrupted,
        vec![&SavePartitionId::World],
        "only the tampered partition is reported"
    );

    // (b) the Mod namespace is part of the identity, not decoration
    let renamed = SavePartition::new(
        SavePartitionId::Mod(namespace("org.example.base2")),
        b"m".to_vec(),
    )
    .with_schema_version("1.0");
    let report = verify(&with_partition(
        &baseline,
        &SavePartitionId::Mod(namespace("org.example.base")),
        renamed,
    ))
    .unwrap();
    assert_eq!(
        status_of(
            &report,
            &SavePartitionId::Mod(namespace("org.example.base"))
        ),
        &PartitionIntegrity::Missing
    );
    assert_eq!(
        status_of(
            &report,
            &SavePartitionId::Mod(namespace("org.example.base2"))
        ),
        &PartitionIntegrity::Unexpected
    );

    // (c) the recorded Schema version is covered by the digest
    let reschema = base_mod_partition(b"m").with_schema_version("2.0");
    let report = verify(&with_partition(
        &baseline,
        &SavePartitionId::Mod(namespace("org.example.base")),
        reschema,
    ))
    .unwrap();
    assert!(matches!(
        status_of(
            &report,
            &SavePartitionId::Mod(namespace("org.example.base"))
        ),
        PartitionIntegrity::Mismatch { .. }
    ));

    // (d) an added partition is reported as uncovered
    let mut partitions = baseline.partitions.clone();
    partitions.push(SavePartition::new(
        SavePartitionId::Metadata,
        b"session".to_vec(),
    ));
    let report = verify(&SaveLayout::new(partitions)).unwrap();
    assert_eq!(
        status_of(&report, &SavePartitionId::Metadata),
        &PartitionIntegrity::Unexpected
    );
    assert!(!report.is_intact());

    // (e) a removed partition is reported as uncovered
    let mut partitions = baseline.partitions.clone();
    partitions
        .retain(|partition| partition.id != SavePartitionId::Mod(namespace("org.example.base")));
    let report = verify(&SaveLayout::new(partitions)).unwrap();
    assert_eq!(
        status_of(
            &report,
            &SavePartitionId::Mod(namespace("org.example.base"))
        ),
        &PartitionIntegrity::Missing
    );
    assert!(
        !report.root_matches,
        "removing a partition moves the layout root"
    );

    // (f) exchanging two payloads moves both digests
    let swapped = with_partition(
        &with_partition(&baseline, &SavePartitionId::World, world_partition(b"m")),
        &SavePartitionId::Mod(namespace("org.example.base")),
        base_mod_partition(b"w"),
    );
    let report = verify(&swapped).unwrap();
    assert!(matches!(
        status_of(&report, &SavePartitionId::World),
        PartitionIntegrity::Mismatch { .. }
    ));
    assert!(matches!(
        status_of(
            &report,
            &SavePartitionId::Mod(namespace("org.example.base"))
        ),
        PartitionIntegrity::Mismatch { .. }
    ));

    // (g) the recorded `owner` cannot be tampered with at all: a layout whose
    // owner disagrees with the canonical owner is rejected before any digest is
    // computed (see `t11`, `save.partition_ownership_violation`).
}

// ---------------------------------------------------------------------------
// T-8: tampering with the checksums payload itself
// ---------------------------------------------------------------------------

/// T-8: an altered entry digest is a `Mismatch`; an altered `SC.root_hash` is
/// caught by the third leg of the root comparison and can never read as intact.
#[test]
fn t08_checksums_payload_tampering_is_detected() {
    let baseline = sealed();
    let integrity_payload = baseline
        .get(&SavePartitionId::Integrity)
        .expect("sealed layout carries checksums")
        .payload
        .as_bytes()
        .to_vec();
    let record = decode_checksums(&integrity_payload).unwrap();
    assert_eq!(record.entries, sealed_entries());

    // ① a forged per-partition digest: the recorded bytes no longer describe
    // the World partition, but the layout itself is untouched.
    let forged: Vec<(SavePartitionId, String)> = record
        .entries
        .iter()
        .map(|(id, digest)| {
            if id == &SavePartitionId::World {
                (
                    id.clone(),
                    partition_digest(&world_partition(b"forged")).unwrap(),
                )
            } else {
                (id.clone(), digest.clone())
            }
        })
        .collect();
    let tampered = with_checksums(
        &baseline,
        encode_checksums(&record.root_hash, &forged).unwrap(),
    );
    let report = verify(&tampered).unwrap();
    assert!(
        report.root_matches,
        "the carried bytes still root identically"
    );
    assert!(!report.is_intact());
    assert_eq!(
        status_of(&report, &SavePartitionId::World),
        &PartitionIntegrity::Mismatch {
            expected: partition_digest(&world_partition(b"forged")).unwrap(),
            actual: G7_WORLD_DIGEST.to_owned(),
        }
    );
    assert_eq!(
        status_of(&report, &SavePartitionId::Manifest),
        &PartitionIntegrity::Verified
    );

    // ② only the payload's own `root_hash` is forged: every partition digest and
    // the recomputed root still agree with the Manifest, yet the payload's
    // self-description does not — and the third leg catches it.
    let decoy_root = partition_digest(&world_partition(b"decoy")).unwrap();
    assert_ne!(decoy_root, record.root_hash);
    let rerooted = with_checksums(
        &baseline,
        encode_checksums(&decoy_root, &record.entries).unwrap(),
    );
    let report = verify(&rerooted).unwrap();
    assert!(
        report.root_matches,
        "the carried bytes still root identically"
    );
    assert!(
        !report.checksums_root_matches,
        "the `SC` self-description moved"
    );
    assert!(
        !report.is_intact(),
        "a forged `SC` root must not read as intact"
    );
    assert_eq!(report.checksums_root, decoy_root);
    assert!(
        report
            .partitions
            .iter()
            .all(|(_, status)| matches!(status, PartitionIntegrity::Verified))
    );
}

// ---------------------------------------------------------------------------
// T-9 / T-10: the `SC` payload codec
// ---------------------------------------------------------------------------

/// T-9: encoding and decoding are inverse, for empty, single and rich payloads.
#[test]
fn t09_checksums_payloads_round_trip() {
    let single = vec![(SavePartitionId::Manifest, G7_MANIFEST_DIGEST.to_owned())];
    let triple = sealed_entries();
    let rich = vec![
        (SavePartitionId::Manifest, G7_MANIFEST_DIGEST.to_owned()),
        (SavePartitionId::World, G7_WORLD_DIGEST.to_owned()),
        (
            SavePartitionId::Mod(namespace("org.example.base")),
            G7_MOD_DIGEST.to_owned(),
        ),
        (
            SavePartitionId::Mod(namespace("org.example.extra")),
            G1_DIGEST.to_owned(),
        ),
        (SavePartitionId::Metadata, G5_DIGEST.to_owned()),
    ];

    for (root, entries) in [
        (G4_DIGEST, triple.clone()),
        (G3_DIGEST, Vec::new()),
        (G2_DIGEST, single),
        (G1_DIGEST, rich),
    ] {
        let payload = encode_checksums(root, &entries).unwrap();
        assert_eq!(&payload[..2], &CHECKSUMS_ENCODING_TAG[..]);
        assert_eq!(payload[2], CHECKSUMS_ENCODING_VERSION);

        let record: ChecksumRecord = decode_checksums(&payload).unwrap();
        assert_eq!(record.algorithm, SUPPORTED_INTEGRITY_ALGORITHM);
        assert_eq!(record.root_hash, root);
        assert_eq!(record.entries, entries);
        assert_eq!(
            encode_checksums(&record.root_hash, &record.entries).unwrap(),
            payload,
            "re-encoding a decoded payload is byte-identical"
        );
    }

    // The empty payload is exactly `G9`; the three-entry payload is `G8`.
    let empty = encode_checksums(G3_DIGEST, &[]).unwrap();
    assert_eq!(empty.len(), G9_LEN);
    assert_eq!(sha256_hex(&empty), G9_DIGEST);

    let g8 = encode_checksums(G4_DIGEST, &triple).unwrap();
    assert_eq!(g8.len(), G8_LEN);
    assert_eq!(sha256_hex(&g8), G8_DIGEST);
    assert_eq!(
        g8,
        hand_built_checksums(
            SUPPORTED_INTEGRITY_ALGORITHM,
            G4_DIGEST,
            3,
            &[
                vec![0u8],
                G7_MANIFEST_DIGEST.as_bytes().to_vec(),
                vec![1u8],
                G7_WORLD_DIGEST.as_bytes().to_vec(),
                vec![2u8],
                16u16.to_be_bytes().to_vec(),
                b"org.example.base".to_vec(),
                G7_MOD_DIGEST.as_bytes().to_vec(),
            ]
            .concat(),
        ),
        "`G8` is exactly `SC` ‖ algorithm ‖ root ‖ 3 entries, each a kind tag and 64 hex characters"
    );
}

/// T-10: every non-canonical `SC` payload is rejected with
/// `save.integrity_payload_invalid`, from either the encoder or the decoder.
#[test]
fn t10_malformed_checksums_payloads_are_rejected() {
    let valid = encode_checksums(
        G3_DIGEST,
        &[(SavePartitionId::World, G7_WORLD_DIGEST.to_owned())],
    )
    .unwrap();
    let (count_offset, first_entry_offset) = entry_region(SUPPORTED_INTEGRITY_ALGORITHM);

    let cases: [(&str, Vec<u8>); 7] = [
        ("wrong tag", {
            let mut bytes = valid.clone();
            bytes[0] = b'X';
            bytes
        }),
        ("wrong version", {
            let mut bytes = valid.clone();
            bytes[2] = 0xFF;
            bytes
        }),
        ("truncated entry", valid[..valid.len() - 1].to_vec()),
        ("trailing bytes", {
            let mut bytes = valid.clone();
            bytes.push(0);
            bytes
        }),
        ("uppercase digest", {
            let mut bytes = valid.clone();
            bytes[first_entry_offset + 1] = b'A';
            bytes
        }),
        ("over-declared entry count", {
            let mut bytes = valid.clone();
            bytes[count_offset..count_offset + 4].copy_from_slice(&u32::MAX.to_be_bytes());
            bytes
        }),
        ("empty payload", Vec::new()),
    ];
    for (label, payload) in cases {
        let error = decode_checksums(&payload).unwrap_err();
        assert_eq!(
            error.code_str(),
            "save.integrity_payload_invalid",
            "{label}"
        );
        assert!(
            error.path().starts_with("/checksums"),
            "{label}: path was `{}`",
            error.path()
        );
    }

    // The encoder refuses the same class of mistakes rather than writing them.
    let duplicate = vec![
        (SavePartitionId::World, G7_WORLD_DIGEST.to_owned()),
        (SavePartitionId::World, G1_DIGEST.to_owned()),
    ];
    let out_of_order = vec![
        (SavePartitionId::World, G7_WORLD_DIGEST.to_owned()),
        (SavePartitionId::Manifest, G7_MANIFEST_DIGEST.to_owned()),
    ];
    let with_integrity = vec![(SavePartitionId::Integrity, G1_DIGEST.to_owned())];
    for (label, entries) in [
        ("duplicate entry", duplicate),
        ("out-of-order entry", out_of_order),
        ("Integrity entry", with_integrity),
    ] {
        assert_eq!(
            encode_checksums(G3_DIGEST, &entries)
                .unwrap_err()
                .code_str(),
            "save.integrity_payload_invalid",
            "{label}"
        );
    }

    // …and the decoder agrees on the wire form of the same violations.
    let integrity_entry = hand_built_checksums(
        SUPPORTED_INTEGRITY_ALGORITHM,
        G3_DIGEST,
        1,
        &[vec![4u8], G1_DIGEST.as_bytes().to_vec()].concat(),
    );
    assert_eq!(
        decode_checksums(&integrity_entry).unwrap_err().code_str(),
        "save.integrity_payload_invalid"
    );

    let duplicated_on_the_wire = hand_built_checksums(
        SUPPORTED_INTEGRITY_ALGORITHM,
        G3_DIGEST,
        2,
        &[
            vec![1u8],
            G7_WORLD_DIGEST.as_bytes().to_vec(),
            vec![1u8],
            G1_DIGEST.as_bytes().to_vec(),
        ]
        .concat(),
    );
    assert_eq!(
        decode_checksums(&duplicated_on_the_wire)
            .unwrap_err()
            .code_str(),
        "save.integrity_payload_invalid"
    );

    // An unknown version is rejected, never silently interpreted.
    let unknown_version = {
        let mut bytes = valid.clone();
        bytes[2] = CHECKSUMS_ENCODING_VERSION + 1;
        bytes
    };
    assert_eq!(
        decode_checksums(&unknown_version).unwrap_err().code_str(),
        "save.integrity_payload_invalid"
    );
}

// ---------------------------------------------------------------------------
// T-11 / T-12: carrier violations and missing metadata
// ---------------------------------------------------------------------------

/// T-11: a carrier contract violation surfaces as the unchanged `SAVE-002`
/// code. Integrity never re-wraps, downgrades or upgrades it.
#[test]
fn t11_carrier_violations_propagate_save_002_codes() {
    let duplicate = SaveLayout::new(vec![world_partition(b"w"), world_partition(b"w")]);
    let error = verify(&duplicate).unwrap_err();
    assert_eq!(error.code_str(), "save.duplicate_partition");
    assert_eq!(
        error.partition_error().map(|failure| failure.code()),
        Some(PartitionErrorCode::DuplicatePartition)
    );
    assert!(error.manifest_error().is_none());
    assert_eq!(
        layout_root(&duplicate).unwrap_err().code_str(),
        "save.duplicate_partition"
    );
    assert_eq!(
        generate(&duplicate).unwrap_err().code_str(),
        "save.duplicate_partition"
    );

    let bad_owner = SaveLayout::new(vec![
        manifest_partition(CARRIED_ROOT_HASH),
        SavePartition::with_owner(
            SavePartitionId::World,
            PartitionOwner::SaveSystem,
            b"w".to_vec(),
        ),
        SavePartition::new(SavePartitionId::Integrity, Vec::<u8>::new()),
    ]);
    assert_eq!(
        verify(&bad_owner).unwrap_err().code_str(),
        "save.partition_ownership_violation"
    );
    assert_eq!(
        layout_root(&bad_owner).unwrap_err().code_str(),
        "save.partition_ownership_violation"
    );

    let overlong_schema = SaveLayout::new(vec![
        manifest_partition(CARRIED_ROOT_HASH),
        SavePartition::new(
            SavePartitionId::Mod(namespace("org.example.base")),
            b"m".to_vec(),
        )
        .with_schema_version("x".repeat(129)),
        SavePartition::new(SavePartitionId::Integrity, Vec::<u8>::new()),
    ]);
    assert_eq!(
        verify(&overlong_schema).unwrap_err().code_str(),
        "save.invalid_partition"
    );
    assert_eq!(
        generate(&overlong_schema).unwrap_err().code_str(),
        "save.invalid_partition"
    );

    // None of the propagated codes is an integrity code: the layer that found
    // the problem is the layer whose code the caller sees.
    for layout in [&duplicate, &bad_owner, &overlong_schema] {
        if let Err(error) = verify(layout) {
            assert!(
                !IntegrityErrorCode::ALL
                    .iter()
                    .any(|code| code.as_str() == error.code_str()),
                "`{}` must not be re-wrapped as an integrity code",
                error.code_str()
            );
        }
    }
}

/// T-12: integrity that cannot be established is an `Err`, never an intact
/// report — neither a missing Manifest nor a missing checksums partition.
#[test]
fn t12_missing_integrity_metadata_is_an_error_not_a_pass() {
    let no_manifest = SaveLayout::new(vec![
        world_partition(b"w"),
        SavePartition::new(SavePartitionId::Integrity, b"".to_vec()),
    ]);
    assert_eq!(
        generate(&no_manifest).unwrap_err().code_str(),
        "save.integrity_missing"
    );
    assert_eq!(
        verify(&no_manifest).unwrap_err().code_str(),
        "save.integrity_missing"
    );
    // The root of such a layout is still defined; it is verification that cannot
    // proceed, because there is no comparison authority.
    assert!(layout_root(&no_manifest).is_ok());

    let no_checksums = SaveLayout::new(vec![
        manifest_partition(CARRIED_ROOT_HASH),
        world_partition(b"w"),
    ]);
    assert_eq!(
        generate(&no_checksums).unwrap_err().code_str(),
        "save.integrity_missing"
    );
    assert_eq!(
        verify(&no_checksums).unwrap_err().code_str(),
        "save.integrity_missing"
    );
    assert!(layout_root(&no_checksums).is_ok());

    // Neither path can produce an empty, vacuously-intact report.
    assert!(verify(&no_manifest).is_err());
    assert!(verify(&no_checksums).is_err());
}

// ---------------------------------------------------------------------------
// T-13: unsupported algorithms, at each layer
// ---------------------------------------------------------------------------

/// T-13: the two algorithm failures live on different layers: `SC` carries its
/// own identifier, while the Manifest's identifier is `SAVE-001`'s business.
#[test]
fn t13_unsupported_algorithms_are_reported_by_their_own_layer() {
    // ① `SC` records an algorithm this build does not implement.
    let payload = hand_built_checksums("md5", G3_DIGEST, 0, &[]);
    let error = decode_checksums(&payload).unwrap_err();
    assert_eq!(error.code_str(), "save.integrity_algorithm_unsupported");
    assert_eq!(error.path(), "/checksums/algorithm");

    let layout = SaveLayout::new(vec![
        manifest_partition(CARRIED_ROOT_HASH),
        world_partition(b"w"),
        SavePartition::new(SavePartitionId::Integrity, payload),
    ]);
    assert_eq!(
        verify(&layout).unwrap_err().code_str(),
        "save.integrity_algorithm_unsupported"
    );

    // ② The Manifest records an unsupported algorithm: `SAVE-001` owns that
    // field, so its code is propagated unchanged and integrity never runs.
    let bad_manifest =
        manifest_json(CARRIED_ROOT_HASH).replace(r#""algorithm":"sha256""#, r#""algorithm":"md5""#);
    let layout = SaveLayout::new(vec![
        SavePartition::new(SavePartitionId::Manifest, bad_manifest.into_bytes()),
        world_partition(b"w"),
        SavePartition::new(SavePartitionId::Integrity, b"".to_vec()),
    ]);
    let error = verify(&layout).unwrap_err();
    assert_eq!(error.code_str(), "save.invalid_field");
    assert_eq!(error.path(), "/integrity/algorithm");
    assert!(error.manifest_error().is_some());
    assert!(
        !IntegrityErrorCode::ALL
            .iter()
            .any(|code| code.as_str() == error.code_str())
    );
}

// ---------------------------------------------------------------------------
// T-14: read-only by construction
// ---------------------------------------------------------------------------

/// T-14: neither entry point writes anything back. The claim has two legs: a
/// runtime check that the carrier bytes are unchanged, and a compile-time check
/// that every entry point borrows the layout.
#[test]
fn t14_entry_points_are_read_only() {
    // Compile-time: `verify`/`layout_root`/`generate`/`partition_digest` take a
    // shared borrow, so "never writes back" is enforced by the borrow checker.
    let _: fn(&SaveLayout) -> Result<IntegrityReport, IntegrityError> = verify;
    let _: fn(&SaveLayout) -> Result<String, IntegrityError> = layout_root;
    let _: fn(&SaveLayout) -> Result<GeneratedIntegrity, IntegrityError> = generate;
    let _: fn(&SavePartition) -> Result<String, IntegrityError> = partition_digest;

    // Runtime: the carrier is byte-identical before and after.
    let sealed = sealed();
    let before = sealed.to_bytes().unwrap();
    let manifest_before = sealed
        .get(&SavePartitionId::Manifest)
        .unwrap()
        .payload
        .as_bytes()
        .to_vec();

    let report = verify(&sealed).unwrap();
    assert!(report.is_intact());
    let generated = generate(&sealed).unwrap();
    let _ = layout_root(&sealed).unwrap();

    assert_eq!(
        sealed.to_bytes().unwrap(),
        before,
        "the carrier is unchanged"
    );
    assert_eq!(
        sealed
            .get(&SavePartitionId::Manifest)
            .unwrap()
            .payload
            .as_bytes(),
        manifest_before.as_slice(),
        "the Manifest partition still holds its original bytes"
    );
    assert_eq!(sealed.partitions.len(), 4);
    // The carried Manifest still records the caller's root, not a rewritten one.
    assert_ne!(generated.root, CARRIED_ROOT_HASH);
}

// ---------------------------------------------------------------------------
// T-15: the frozen code set
// ---------------------------------------------------------------------------

/// T-15: exactly three integrity codes, disjoint from the twelve codes the two
/// earlier layers already froze.
#[test]
fn t15_integrity_codes_are_disjoint_from_the_earlier_layers() {
    const SAVE_001: [&str; 7] = [
        "save.invalid_manifest",
        "save.missing_field",
        "save.invalid_field",
        "save.unsupported_format_version",
        "save.invalid_mod_entry",
        "save.duplicate_mod_id",
        "save.invalid_schema",
    ];
    const SAVE_002: [&str; 5] = [
        "save.invalid_partition",
        "save.invalid_layout",
        "save.invalid_namespace",
        "save.partition_ownership_violation",
        "save.duplicate_partition",
    ];

    assert_eq!(IntegrityErrorCode::ALL.len(), 3, "exactly three codes");
    let mut texts: Vec<&str> = IntegrityErrorCode::ALL
        .iter()
        .map(|code| code.as_str())
        .collect();
    for text in &texts {
        assert!(text.starts_with("save.integrity_"), "`{text}`");
        assert!(!SAVE_001.contains(text), "`{text}` collides with SAVE-001");
        assert!(!SAVE_002.contains(text), "`{text}` collides with SAVE-002");
    }
    let count = texts.len();
    texts.sort_unstable();
    texts.dedup();
    assert_eq!(texts.len(), count, "the three codes are distinct");
}

// ---------------------------------------------------------------------------
// T-16: module discipline
// ---------------------------------------------------------------------------

/// T-16: the module is pure — no I/O, no unsafe, no panicking constructs — and
/// introduces no new dependency edge of its own.
#[test]
fn t16_the_module_respects_its_discipline() {
    let source = include_str!("../src/integrity.rs");
    for forbidden in [
        "std::io",
        "std::fs",
        "std::path",
        "catch_unwind",
        "unsafe",
        "panic!",
        "unreachable!",
        "#[allow",
    ] {
        assert!(
            !source.contains(forbidden),
            "`integrity.rs` must not contain `{forbidden}`"
        );
    }
    assert!(
        !source.contains("kernel_core"),
        "`integrity.rs` must not reach for the kernel directly"
    );
    // The `kernel-core → []` edge itself is asserted by the pre-existing
    // `tests/dependency_boundary.rs`, which runs in the same workspace gate.
}

// ---------------------------------------------------------------------------
// T-18 / T-19: self-consistency and the vacuous-truth guard
// ---------------------------------------------------------------------------

/// T-18: writing the computed root back into the Manifest does not move it, so
/// the definition is satisfiable rather than self-referential.
#[test]
fn t18_writing_the_root_back_does_not_change_it() {
    let body = body();
    let mut scratch = body.clone();
    scratch.push(SavePartition::new(
        SavePartitionId::Integrity,
        Vec::<u8>::new(),
    ));
    let generated = generate(&SaveLayout::new(scratch)).unwrap();

    // Step 1: write the root back into the Manifest, leaving everything else.
    let mut written_back: Vec<SavePartition> = body
        .iter()
        .map(|partition| {
            if partition.id == SavePartitionId::Manifest {
                let mut manifest = SaveManifest::from_bytes(partition.payload.as_bytes()).unwrap();
                manifest.integrity.root_hash = generated.root.clone();
                SavePartition::new(SavePartitionId::Manifest, manifest.to_bytes())
            } else {
                partition.clone()
            }
        })
        .collect();

    // Step 2: the root recomputed afterwards is the same value.
    assert_eq!(
        layout_root(&SaveLayout::new(written_back.clone())).unwrap(),
        generated.root,
        "the root is invariant under being written back"
    );

    // Step 3: with the checksums payload in place the save verifies.
    written_back.push(SavePartition::new(
        SavePartitionId::Integrity,
        generated.checksums,
    ));
    let sealed = SaveLayout::new(written_back);
    let report = verify(&sealed).unwrap();
    assert!(report.is_intact());
    assert_eq!(report.manifest_root, report.computed_root);
    assert_eq!(report.checksums_root, report.computed_root);
    assert_eq!(report.computed_root, generated.root);
}

/// T-19: an all-zero-partition report is never intact, and the API cannot
/// produce one anyway.
#[test]
fn t19_an_empty_report_is_never_intact() {
    let all_roots_agree = IntegrityReport {
        algorithm: SUPPORTED_INTEGRITY_ALGORITHM.to_owned(),
        manifest_root: G3_DIGEST.to_owned(),
        checksums_root: G3_DIGEST.to_owned(),
        computed_root: G3_DIGEST.to_owned(),
        root_matches: true,
        checksums_root_matches: true,
        partitions: Vec::new(),
    };
    assert!(
        !all_roots_agree.is_intact(),
        "three agreeing roots over zero partitions is not evidence of integrity"
    );
    assert_eq!(all_roots_agree.corrupted_partitions().count(), 0);

    // Every reachable route to such a report is an error instead.
    let no_manifest = SaveLayout::new(vec![
        world_partition(b"w"),
        SavePartition::new(SavePartitionId::Integrity, b"".to_vec()),
    ]);
    let no_checksums = SaveLayout::new(vec![
        manifest_partition(CARRIED_ROOT_HASH),
        world_partition(b"w"),
    ]);
    let only_integrity = SaveLayout::new(vec![SavePartition::new(
        SavePartitionId::Integrity,
        b"stale".to_vec(),
    )]);
    for layout in [&no_manifest, &no_checksums, &only_integrity] {
        assert!(verify(layout).is_err(), "must not yield a report at all");
    }
}

// ---------------------------------------------------------------------------
// T-20 / T-21: fail-closed and the two validation tracks
// ---------------------------------------------------------------------------

/// T-20: a malformed Manifest payload is rejected before any report exists, so
/// no caller can mistake a parse failure for a verdict.
#[test]
fn t20_malformed_manifest_payloads_fail_closed() {
    let base = manifest_json(CARRIED_ROOT_HASH);
    let cases: [(&str, String); 5] = [
        ("not JSON at all", "{\"save_id\":".to_owned()),
        (
            "missing required field",
            base.replace(r#""world_schema_version":"1.0","#, ""),
        ),
        (
            "unknown field",
            base.replace(
                r#""simulation_config":{}"#,
                r#""simulation_config":{},"surprise":"nope""#,
            ),
        ),
        (
            "duplicate key with an invalid winner",
            base.replace(r#""save_id":"s""#, r#""save_id":"s","save_id":"NOPE""#),
        ),
        (
            "logical_tick is not a u64",
            base.replace(r#""logical_tick":0"#, r#""logical_tick":100.0"#),
        ),
    ];

    for (label, json) in cases {
        assert_ne!(json, base, "`{label}` did not change the fixture");
        assert!(
            manifest_canonical_bytes(json.as_bytes()).is_err(),
            "`{label}` must not yield canonical bytes"
        );

        let layout = SaveLayout::new(vec![
            SavePartition::new(SavePartitionId::Manifest, json.clone().into_bytes()),
            world_partition(b"w"),
            SavePartition::new(SavePartitionId::Integrity, Vec::<u8>::new()),
        ]);
        for outcome in [
            verify(&layout).err(),
            generate(&layout).err(),
            layout_root(&layout).err(),
        ] {
            let error = outcome.unwrap_or_else(|| panic!("`{label}` produced a value"));
            assert!(
                error.manifest_error().is_some(),
                "`{label}`: expected a SAVE-001 failure, got `{}`",
                error.code_str()
            );
        }
    }
}

/// T-21: the published schema admits `100.0` and `1e2` as integers; the typed
/// layer rejects them. Trimming either track would let them through.
#[test]
fn t21_dual_track_validation_cannot_be_trimmed() {
    for tick in ["100.0", "1e2"] {
        let json = manifest_json(CARRIED_ROOT_HASH)
            .replace(r#""logical_tick":0"#, &format!(r#""logical_tick":{tick}"#));
        let value: serde_json::Value = serde_json::from_str(&json).expect("fixture is JSON");
        assert!(
            validate_manifest_value(&value).is_ok(),
            "the published schema admits `{tick}` as an integer literal"
        );
        assert_eq!(
            SaveManifest::from_bytes(json.as_bytes())
                .unwrap_err()
                .code_str(),
            "save.invalid_manifest",
            "the typed layer rejects `{tick}`"
        );
    }

    // A plain integer is accepted by both tracks.
    let json = manifest_json(CARRIED_ROOT_HASH);
    let value: serde_json::Value = serde_json::from_str(&json).expect("fixture is JSON");
    assert!(validate_manifest_value(&value).is_ok());
    assert!(SaveManifest::from_bytes(json.as_bytes()).is_ok());
}

// ---------------------------------------------------------------------------
// T-22 / T-23 / T-24: canonical Manifest bytes
// ---------------------------------------------------------------------------

/// T-22: every business field reaches the canonical bytes; the exempt member
/// does not.
#[test]
fn t22_every_business_field_reaches_the_canonical_bytes() {
    let base = manifest_json(CARRIED_ROOT_HASH);
    let expected = golden_cmb();

    let mutations: [(&str, String); 12] = [
        (
            "save_format_version",
            base.replace(
                r#""save_format_version":"1.0""#,
                r#""save_format_version":"0.9""#,
            ),
        ),
        (
            "save_id",
            base.replace(r#""save_id":"s""#, r#""save_id":"t""#),
        ),
        (
            "created_at",
            base.replace(
                r#""created_at":"2026-10-15T00:00:00Z""#,
                r#""created_at":"2026-10-15T00:00:01Z""#,
            ),
        ),
        (
            "last_saved_at",
            base.replace(
                r#""last_saved_at":"2026-10-15T00:00:00Z""#,
                r#""last_saved_at":"2026-10-15T00:00:02Z""#,
            ),
        ),
        (
            "platform_version",
            base.replace(
                r#""platform_version":"0.1.0""#,
                r#""platform_version":"0.1.1""#,
            ),
        ),
        (
            "game_definition.id",
            base.replace(r#""id":"org.example.game""#, r#""id":"org.example.game2""#),
        ),
        (
            "game_definition.version",
            base.replace(
                r#"{"id":"org.example.game","version":"1.0.0"}"#,
                r#"{"id":"org.example.game","version":"1.0.1"}"#,
            ),
        ),
        (
            "kernel_api_version",
            base.replace(
                r#""kernel_api_version":"1.0""#,
                r#""kernel_api_version":"1.1""#,
            ),
        ),
        (
            "world_schema_version",
            base.replace(
                r#""world_schema_version":"1.0""#,
                r#""world_schema_version":"1.1""#,
            ),
        ),
        (
            "logical_tick",
            base.replace(r#""logical_tick":0"#, r#""logical_tick":1"#),
        ),
        (
            "mods[0]",
            base.replace(r#""id":"org.example.base""#, r#""id":"org.example.base2""#),
        ),
        (
            "simulation_config",
            base.replace(
                r#""simulation_config":{}"#,
                r#""simulation_config":{"a":1}"#,
            ),
        ),
    ];

    for (label, mutated) in mutations {
        assert_ne!(mutated, base, "`{label}` did not apply");
        let canonical = manifest_canonical_bytes(mutated.as_bytes())
            .unwrap_or_else(|error| panic!("`{label}`: {}", error.code_str()));
        assert_ne!(
            canonical, expected,
            "`{label}` must change the canonical bytes"
        );
    }

    // The exempt member: two different recorded roots yield the same bytes.
    let other = manifest_json("ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff");
    assert_eq!(
        manifest_canonical_bytes(other.as_bytes()).unwrap(),
        expected
    );
}

/// T-23: whitespace, indentation and key order are not part of the contract.
#[test]
fn t23_equivalent_inputs_produce_identical_canonical_bytes() {
    let expected = golden_cmb();
    let reversed = format!(
        "{{\n  \"integrity\": {{\n    \"root_hash\": \"{CARRIED_ROOT_HASH}\",\n    \"algorithm\": \"sha256\"\n  }},\n  \"simulation_config\": {{}},\n  \"mods\": [\n    {{\n      \"schema_version\": \"1.0\",\n      \"version\": \"1.0.0\",\n      \"id\": \"org.example.base\"\n    }}\n  ],\n  \"logical_tick\": 0,\n  \"world_schema_version\": \"1.0\",\n  \"kernel_api_version\": \"1.0\",\n  \"game_definition\": {{\n    \"version\": \"1.0.0\",\n    \"id\": \"org.example.game\"\n  }},\n  \"platform_version\": \"0.1.0\",\n  \"last_saved_at\": \"2026-10-15T00:00:00Z\",\n  \"created_at\": \"2026-10-15T00:00:00Z\",\n  \"save_id\": \"s\",\n  \"save_format_version\": \"1.0\"\n}}\n"
    );
    assert_ne!(reversed, manifest_json(CARRIED_ROOT_HASH));
    assert!(SaveManifest::from_bytes(reversed.as_bytes()).is_ok());
    assert_eq!(
        manifest_canonical_bytes(reversed.as_bytes()).unwrap(),
        expected,
        "a reversed, indented Manifest is canonically equal"
    );

    let spaced = manifest_json(CARRIED_ROOT_HASH).replace(',', ",\t\n");
    assert_eq!(
        manifest_canonical_bytes(spaced.as_bytes()).unwrap(),
        expected
    );
}

/// T-24: the frozen `M-α′` rule — `algorithm` is kept, `root_hash` is replaced
/// by the 64-character zero sentinel.
#[test]
fn t24_canonical_bytes_use_the_frozen_sentinel_rule() {
    let cmb = manifest_canonical_bytes(manifest_json(CARRIED_ROOT_HASH).as_bytes()).unwrap();
    let text = std::str::from_utf8(&cmb).expect("canonical bytes are UTF-8 JSON");
    assert!(
        text.contains(
            r#""integrity":{"algorithm":"sha256","root_hash":"0000000000000000000000000000000000000000000000000000000000000000"}"#
        ),
        "`algorithm` keeps its parsed value and `root_hash` becomes the sentinel"
    );
    assert!(
        !text.contains(CARRIED_ROOT_HASH),
        "the recorded root is excluded"
    );
    assert_eq!(cmb, golden_cmb());

    // Because `algorithm` keeps the parsed value, a Manifest that recorded any
    // other algorithm cannot contribute canonical bytes at all — the sentinel
    // never pins the field to a value the Manifest did not carry.
    let bad =
        manifest_json(CARRIED_ROOT_HASH).replace(r#""algorithm":"sha256""#, r#""algorithm":"md5""#);
    assert_eq!(
        manifest_canonical_bytes(bad.as_bytes())
            .unwrap_err()
            .code_str(),
        "save.invalid_field"
    );
}

// ---------------------------------------------------------------------------
// T-25 / T-26: the frozen goldens and the digest representation
// ---------------------------------------------------------------------------

/// T-25: the `§7.2` table in the design document is re-derived from the
/// document itself, so a transcription slip cannot survive the suite.
#[test]
fn t25_the_design_document_goldens_recompute() {
    let document = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../planning/29-P4-SAVE-003-完整性摘要与校验设计提案.md"
    ));

    let mut goldens: Vec<(String, Vec<u8>, String)> = Vec::new();
    for line in document.lines() {
        if !line.starts_with("| **G") {
            continue;
        }
        let name = line
            .split("**")
            .nth(1)
            .expect("the row is delimited by `**`")
            .to_owned();
        let cells: Vec<&str> = line.split('`').collect();
        assert!(cells.len() >= 4, "unexpected golden row: {line}");
        let bytes = from_hex(cells[1]);
        let digest = cells[3].to_owned();
        let declared: usize = cells[2]
            .split_whitespace()
            .find_map(|token| token.parse().ok())
            .expect("the row declares a byte length");
        assert_eq!(bytes.len(), declared, "`{name}` declares its byte length");
        assert_eq!(sha256_hex(&bytes), digest, "`{name}` digest");
        goldens.push((name, bytes, digest));
    }
    assert_eq!(goldens.len(), 11, "`§7.2` lists 11 golden vectors");

    fn find(goldens: &[(String, Vec<u8>, String)], name: &str) -> (Vec<u8>, String) {
        goldens
            .iter()
            .find(|(candidate, _, _)| candidate == name)
            .map(|(_, bytes, digest)| (bytes.clone(), digest.clone()))
            .unwrap_or_else(|| panic!("no golden `{name}` in `§7.2`"))
    }

    // §7.2b: the compositional identities, recomputed from the table.
    let part = from_hex(PART_DOMAIN_HEX);
    let root = from_hex(ROOT_DOMAIN_HEX);
    assert_eq!(part.len(), 27);
    assert_eq!(root.len(), 22);
    assert_eq!(part, PARTITION_DIGEST_DOMAIN);
    assert_eq!(root, ROOT_DIGEST_DOMAIN);

    let (g1, _) = find(&goldens, "G1");
    let (g2, _) = find(&goldens, "G2");
    let (g3, _) = find(&goldens, "G3");
    let (g4, g4_digest) = find(&goldens, "G4");
    let (g5, _) = find(&goldens, "G5");
    let (g6, _) = find(&goldens, "G6");
    let (g7_manifest, _) = find(&goldens, "G7-manifest");
    let (g7_world, _) = find(&goldens, "G7-world");
    let (g7_mod, _) = find(&goldens, "G7-mod");
    let (g8, _) = find(&goldens, "G8");
    let (g9, _) = find(&goldens, "G9");

    let record = |whole: &[u8]| whole[part.len()..].to_vec();
    assert_eq!(g1, [part.clone(), from_hex(WORLD_ABC_RECORD_HEX)].concat());
    assert_eq!(g5, [part.clone(), from_hex(WORLD_W_RECORD_HEX)].concat());
    assert_eq!(g2, [root.clone(), from_hex(LAYOUT_WORLD_ABC_HEX)].concat());
    assert_eq!(g3, [root.clone(), from_hex(LAYOUT_EMPTY_HEX)].concat());
    assert_eq!(
        g7_manifest,
        [part.clone(), from_hex("000000000001ed"), g6.clone()].concat(),
        "the Manifest record is `00 00 00` ‖ u32be(493) ‖ CMB"
    );
    assert_eq!(
        g7_world,
        [part.clone(), from_hex(WORLD_LOWER_W_RECORD_HEX)].concat()
    );
    assert_eq!(
        g7_mod,
        [part.clone(), from_hex(MOD_BASE_RECORD_HEX)].concat()
    );
    assert_eq!(
        g4,
        [
            root.clone(),
            from_hex("534c0100000003"),
            record(&g7_manifest),
            record(&g7_world),
            from_hex(MOD_BASE_RECORD_HEX),
        ]
        .concat(),
        "`G4` is the canonical three-partition encoding under the root domain"
    );

    assert_eq!(g8.len(), 292);
    assert_eq!(
        g8,
        hand_built_checksums(SUPPORTED_INTEGRITY_ALGORITHM, &g4_digest, 3, &{
            let (_, manifest_digest) = find(&goldens, "G7-manifest");
            let (_, world_digest) = find(&goldens, "G7-world");
            let (_, mod_digest) = find(&goldens, "G7-mod");
            let mut entries = Vec::new();
            entries.extend_from_slice(&[0u8]);
            entries.extend_from_slice(manifest_digest.as_bytes());
            entries.extend_from_slice(&[1u8]);
            entries.extend_from_slice(world_digest.as_bytes());
            entries.extend_from_slice(&[2u8]);
            entries.extend_from_slice(&16u16.to_be_bytes());
            entries.extend_from_slice(b"org.example.base");
            entries.extend_from_slice(mod_digest.as_bytes());
            entries
        }),
        "`G8` is `SC` ‖ `sha256` ‖ root ‖ 3 entries of 64 hex characters"
    );

    assert_eq!(g9.len(), 79);
    assert_eq!(
        g9,
        hand_built_checksums(
            SUPPORTED_INTEGRITY_ALGORITHM,
            &find(&goldens, "G3").1,
            0,
            &[]
        ),
        "`G9` is `SC` ‖ `sha256` ‖ root ‖ 0 entries"
    );
}

/// T-26: every digest on the wire is 64 lowercase hexadecimal characters, never
/// a raw 32-byte value.
#[test]
fn t26_checksum_digests_are_lowercase_hex_text() {
    assert_eq!(DIGEST_TEXT_LEN, 64);
    assert_eq!(MIN_ENCODED_CHECKSUM_ENTRY_LEN, 1 + DIGEST_TEXT_LEN);

    assert_eq!(
        encode_checksums(&G3_DIGEST.to_uppercase(), &[])
            .unwrap_err()
            .code_str(),
        "save.integrity_payload_invalid"
    );
    for (label, digest) in [
        ("uppercase", "A".repeat(DIGEST_TEXT_LEN)),
        ("non-hex", "g".repeat(DIGEST_TEXT_LEN)),
        ("too short", "0".repeat(DIGEST_TEXT_LEN - 1)),
        ("too long", "0".repeat(DIGEST_TEXT_LEN + 1)),
    ] {
        let error = encode_checksums(G3_DIGEST, &[(SavePartitionId::World, digest)]).unwrap_err();
        assert_eq!(
            error.code_str(),
            "save.integrity_payload_invalid",
            "{label}"
        );
    }

    // The decoder enforces the same rule on an entry digest and on the root.
    let valid = encode_checksums(
        G3_DIGEST,
        &[(SavePartitionId::World, G7_WORLD_DIGEST.to_owned())],
    )
    .unwrap();
    let (_, first_entry_offset) = entry_region(SUPPORTED_INTEGRITY_ALGORITHM);

    let mut uppercase_entry_digest = valid.clone();
    uppercase_entry_digest[first_entry_offset + 1] = b'A';
    assert_eq!(
        decode_checksums(&uppercase_entry_digest)
            .unwrap_err()
            .code_str(),
        "save.integrity_payload_invalid"
    );

    let root_offset = CHECKSUMS_ENCODING_TAG.len() + 1 + 2 + SUPPORTED_INTEGRITY_ALGORITHM.len();
    let mut uppercase_root = valid.clone();
    uppercase_root[root_offset] = b'A';
    assert_eq!(
        decode_checksums(&uppercase_root).unwrap_err().code_str(),
        "save.integrity_payload_invalid"
    );

    // The accepted form is the 64-character lowercase text of `R-FMT-3`, for
    // both the root and every entry.
    let record = decode_checksums(&valid).unwrap();
    for digest in std::iter::once(&record.root_hash).chain(record.entries.iter().map(|(_, d)| d)) {
        assert_eq!(digest.len(), DIGEST_TEXT_LEN);
        assert!(
            digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        );
    }
}
