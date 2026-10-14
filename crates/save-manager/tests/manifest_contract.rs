//! SAVE-001 contract tests for the save Manifest model, version model and
//! validation.
//!
//! These tests pin the frozen contract: the required metadata set, the six
//! authoritative version fields, deterministic byte-identical round trips, and
//! stable machine-readable error codes with JSON Pointers. They also prove the
//! dual-track design: the published schema and the Rust model agree on
//! structural violations, while value-level semantics are enforced by Rust.

use kernel_core::Tick;
use save_manager::{
    GameDefinition, Integrity, SAVE_MANIFEST_SCHEMA_JSON, SUPPORTED_INTEGRITY_ALGORITHM,
    SaveFormatVersion, SaveManifest, SaveManifestError, SaveModEntry,
};
use serde_json::{Value, json};

/// A real 64-character lowercase hex digest stands in for the value `SAVE-003`
/// will produce; `SAVE-001` only carries it.
const REAL_ROOT_HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

const REQUIRED_TOP_LEVEL: [&str; 12] = [
    "save_format_version",
    "save_id",
    "created_at",
    "last_saved_at",
    "platform_version",
    "game_definition",
    "kernel_api_version",
    "world_schema_version",
    "logical_tick",
    "mods",
    "simulation_config",
    "integrity",
];

/// The specification example verbatim (`Save & Compatibility Specification`
/// §3.3). Its `root_hash` is the illustrative placeholder, so the whole example
/// MUST be rejected: an example value is not a valid checksum.
const SPEC_EXAMPLE: &str = r#"{
  "save_format_version": "1.0",
  "save_id": "save-example-001",
  "created_at": "2026-01-01T12:00:00Z",
  "last_saved_at": "2026-01-01T12:30:00Z",
  "platform_version": "1.0.0",
  "game_definition": { "id": "org.example.base-game", "version": "1.0.0" },
  "kernel_api_version": "1.0",
  "world_schema_version": "1.0",
  "logical_tick": 72000,
  "mods": [
    { "id": "org.example.base", "version": "1.0.0", "schema_version": "1.0" },
    { "id": "org.example.economy", "version": "1.2.0", "schema_version": "2.0" }
  ],
  "simulation_config": {},
  "integrity": { "algorithm": "sha256", "root_hash": "IMPLEMENTATION_GENERATED" }
}"#;

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

fn valid_value() -> Value {
    valid_manifest().to_value()
}

fn expect_error(result: Result<SaveManifest, SaveManifestError>) -> SaveManifestError {
    match result {
        Ok(_) => panic!("expected the manifest to be rejected"),
        Err(error) => error,
    }
}

fn reject(value: Value) -> SaveManifestError {
    expect_error(SaveManifest::from_value(value))
}

// ---------------------------------------------------------------------------
// Positive path: round trip and determinism
// ---------------------------------------------------------------------------

#[test]
fn valid_manifest_passes_validation() {
    let manifest = valid_manifest();
    manifest.validate().expect("example manifest is valid");
    // The frozen interface contract also exposes the free-function form.
    save_manager::validate_manifest(&manifest).expect("free-function validate_manifest accepts");
}

#[test]
fn manifest_round_trips_byte_identically() {
    let manifest = valid_manifest();
    let bytes = manifest.to_bytes();
    let parsed = SaveManifest::from_bytes(&bytes).expect("bytes round-trip");
    assert_eq!(parsed, manifest);
    assert_eq!(
        parsed.to_bytes(),
        bytes,
        "re-serialization is byte-identical"
    );
}

#[test]
fn serialization_is_deterministic() {
    let manifest = valid_manifest();
    assert_eq!(manifest.to_bytes(), manifest.to_bytes());
}

#[test]
fn simulation_config_keys_are_serialized_in_canonical_order() {
    let mut manifest = valid_manifest();
    manifest
        .simulation_config
        .insert("zeta".to_owned(), json!(1));
    manifest
        .simulation_config
        .insert("alpha".to_owned(), json!({ "nested": true }));
    let bytes = manifest.to_bytes();
    let text = String::from_utf8(bytes).unwrap();
    let alpha = text.find("\"alpha\"").expect("alpha present");
    let zeta = text.find("\"zeta\"").expect("zeta present");
    assert!(
        alpha < zeta,
        "mapping keys must be serialized in sorted order"
    );
}

// ---------------------------------------------------------------------------
// Required metadata
// ---------------------------------------------------------------------------

#[test]
fn specification_example_is_rejected_because_its_hash_is_a_placeholder() {
    let error = expect_error(SaveManifest::from_bytes(SPEC_EXAMPLE.as_bytes()));
    assert_eq!(error.code_str(), "save.invalid_field");
    assert_eq!(error.path(), "/integrity/root_hash");
}

#[test]
fn every_required_top_level_field_is_enforced() {
    for key in REQUIRED_TOP_LEVEL {
        let mut value = valid_value();
        value.as_object_mut().unwrap().remove(key);
        let error = reject(value);
        assert_eq!(
            error.code_str(),
            "save.missing_field",
            "field `{key}` must be required"
        );
        assert_eq!(error.path(), format!("/{key}"));
    }
}

#[test]
fn nested_required_fields_are_enforced() {
    let mut value = valid_value();
    value["game_definition"]
        .as_object_mut()
        .unwrap()
        .remove("id");
    let error = reject(value);
    assert_eq!(error.code_str(), "save.missing_field");
    assert_eq!(error.path(), "/game_definition/id");

    let mut value = valid_value();
    value["integrity"]
        .as_object_mut()
        .unwrap()
        .remove("root_hash");
    let error = reject(value);
    assert_eq!(error.path(), "/integrity/root_hash");

    let mut value = valid_value();
    value["mods"][1]
        .as_object_mut()
        .unwrap()
        .remove("schema_version");
    let error = reject(value);
    assert_eq!(error.path(), "/mods/1/schema_version");

    let mut value = valid_value();
    value.as_object_mut().unwrap().remove("game_definition");
    let error = reject(value);
    assert_eq!(error.code_str(), "save.missing_field");
    assert_eq!(error.path(), "/game_definition");
}

#[test]
fn game_definition_is_required_and_is_metadata() {
    // `game_definition` is required metadata (§3.2) but is NOT one of the six
    // authoritative version fields (§3.4): it carries an id plus its own
    // version and never stands in for `world_schema_version`.
    let value = valid_value();
    assert!(value.get("game_definition").is_some());
    assert!(value["game_definition"].get("id").is_some());
    assert!(value["game_definition"].get("version").is_some());
}

// ---------------------------------------------------------------------------
// Structural violations: schema and model agree
// ---------------------------------------------------------------------------

#[test]
fn unknown_field_is_rejected_by_schema_and_by_model() {
    let mut value = valid_value();
    value
        .as_object_mut()
        .unwrap()
        .insert("surprise".to_owned(), json!("nope"));
    let error = reject(value.clone());
    assert_eq!(error.code_str(), "save.invalid_manifest");
    // The typed model rejects the same document (deny_unknown_fields).
    assert!(serde_json::from_value::<SaveManifest>(value).is_err());
}

#[test]
fn schema_and_model_agree_on_shape_violations() {
    // Shape violations (unknown field, wrong JSON type, missing nested field)
    // are rejected both by the published schema and by the typed model's own
    // deserialization.
    let mut unknown_field = valid_value();
    unknown_field
        .as_object_mut()
        .unwrap()
        .insert("surprise".to_owned(), json!("nope"));

    let mut simulation_config_not_object = valid_value();
    simulation_config_not_object["simulation_config"] = json!([]);

    let mut negative_tick = valid_value();
    negative_tick["logical_tick"] = json!(-1);

    let mut tick_as_string = valid_value();
    tick_as_string["logical_tick"] = json!("72000");

    let mut mods_not_array = valid_value();
    mods_not_array["mods"] = json!({});

    let mut platform_version_as_number = valid_value();
    platform_version_as_number["platform_version"] = json!(1);

    let mut format_version_as_number = valid_value();
    format_version_as_number["save_format_version"] = json!(1);

    let mut game_definition_not_object = valid_value();
    game_definition_not_object["game_definition"] = json!("org.example.base-game");

    let cases = [
        ("unknown field", unknown_field),
        ("non-object simulation_config", simulation_config_not_object),
        ("negative logical_tick", negative_tick),
        ("logical_tick as string", tick_as_string),
        ("mods as object", mods_not_array),
        ("platform_version as number", platform_version_as_number),
        ("save_format_version as number", format_version_as_number),
        ("game_definition as string", game_definition_not_object),
    ];

    for (label, value) in cases {
        let structural = SaveManifest::from_value(value.clone())
            .expect_err("schema must reject the shape violation");
        assert_eq!(
            structural.code_str(),
            "save.invalid_manifest",
            "shape violation must use the schema code: {label}"
        );
        assert!(
            serde_json::from_value::<SaveManifest>(value).is_err(),
            "the typed model must reject: {label}"
        );
    }
}

#[test]
fn schema_and_semantics_agree_on_syntax_violations() {
    // Syntax violations (identifier/version/timestamp formats) are string-typed,
    // so the typed model deserializes them; the rule is enforced by the schema
    // and, in lock-step, by the Rust semantic validator.
    let mut schema_value = valid_value();
    schema_value["save_id"] = json!("Uppercase");
    assert!(save_manager::validate_manifest_value(&schema_value).is_err());
    let mut semantic = valid_manifest();
    semantic.save_id = "Uppercase".to_owned();
    assert!(semantic.validate().is_err());

    let mut schema_value = valid_value();
    schema_value["platform_version"] = json!("1.0");
    assert!(save_manager::validate_manifest_value(&schema_value).is_err());
    let mut semantic = valid_manifest();
    semantic.platform_version = "1.0".to_owned();
    assert!(semantic.validate().is_err());

    let mut schema_value = valid_value();
    schema_value["created_at"] = json!("2026-01-01 12:00:00");
    assert!(save_manager::validate_manifest_value(&schema_value).is_err());
    let mut semantic = valid_manifest();
    semantic.created_at = "2026-01-01 12:00:00".to_owned();
    assert!(semantic.validate().is_err());

    let mut schema_value = valid_value();
    schema_value["mods"][0]["schema_version"] = json!("1");
    assert!(save_manager::validate_manifest_value(&schema_value).is_err());
    let mut semantic = valid_manifest();
    semantic.mods[0].schema_version = "1".to_owned();
    assert!(semantic.validate().is_err());
}

#[test]
fn malformed_save_format_version_syntax_is_rejected() {
    // A malformed format-version string cannot be modelled at all, so the schema
    // is the gate for that field's syntax.
    for candidate in ["1.0.0", "1", "1.", ".0", "v1.0"] {
        let mut value = valid_value();
        value["save_format_version"] = json!(candidate);
        let error = reject(value);
        assert_eq!(error.code_str(), "save.invalid_manifest");
        assert_eq!(error.path(), "/save_format_version");
    }
}

// ---------------------------------------------------------------------------
// Value-level semantics (Rust layer)
// ---------------------------------------------------------------------------

#[test]
fn empty_mod_list_is_rejected() {
    let mut manifest = valid_manifest();
    manifest.mods.clear();
    let error = manifest.validate().unwrap_err();
    assert_eq!(error.code_str(), "save.invalid_field");
    assert_eq!(error.path(), "/mods");
}

#[test]
fn duplicate_mod_id_is_rejected() {
    let mut manifest = valid_manifest();
    manifest.mods[1].id = "org.example.base".to_owned();
    let error = manifest.validate().unwrap_err();
    assert_eq!(error.code_str(), "save.duplicate_mod_id");
    assert_eq!(error.path(), "/mods/1/id");
}

#[test]
fn non_semver_mod_version_is_rejected() {
    let mut manifest = valid_manifest();
    manifest.mods[0].version = "v1".to_owned();
    let error = manifest.validate().unwrap_err();
    assert_eq!(error.code_str(), "save.invalid_mod_entry");
    assert_eq!(error.path(), "/mods/0/version");
}

#[test]
fn malformed_mod_schema_version_is_rejected() {
    let mut manifest = valid_manifest();
    manifest.mods[0].schema_version = "2".to_owned();
    let error = manifest.validate().unwrap_err();
    assert_eq!(error.code_str(), "save.invalid_mod_entry");
    assert_eq!(error.path(), "/mods/0/schema_version");
}

#[test]
fn placeholder_root_hash_is_rejected() {
    let mut manifest = valid_manifest();
    manifest.integrity.root_hash = save_manager::SPEC_EXAMPLE_ROOT_HASH_PLACEHOLDER.to_owned();
    let error = manifest.validate().unwrap_err();
    assert_eq!(error.code_str(), "save.invalid_field");
    assert_eq!(error.path(), "/integrity/root_hash");
}

#[test]
fn empty_root_hash_is_rejected() {
    let mut manifest = valid_manifest();
    manifest.integrity.root_hash = String::new();
    let error = manifest.validate().unwrap_err();
    assert_eq!(error.path(), "/integrity/root_hash");
}

#[test]
fn unsupported_integrity_algorithm_is_rejected() {
    let mut manifest = valid_manifest();
    manifest.integrity.algorithm = "md5".to_owned();
    let error = manifest.validate().unwrap_err();
    assert_eq!(error.code_str(), "save.invalid_field");
    assert_eq!(error.path(), "/integrity/algorithm");
}

#[test]
fn invalid_save_id_is_rejected() {
    for candidate in ["", "-leading", "trailing-", "double--x", "Uppercase"] {
        let mut manifest = valid_manifest();
        manifest.save_id = candidate.to_owned();
        let error = manifest.validate().unwrap_err();
        assert_eq!(error.code_str(), "save.invalid_field");
        assert_eq!(error.path(), "/save_id");
    }
}

#[test]
fn invalid_timestamp_is_rejected() {
    for (field, candidate) in [
        ("created_at", "2026-13-01T12:00:00Z"),
        ("created_at", "not-a-timestamp"),
        ("last_saved_at", "2026-01-01T24:00:00+08:00"),
    ] {
        let mut manifest = valid_manifest();
        if field == "created_at" {
            manifest.created_at = candidate.to_owned();
        } else {
            manifest.last_saved_at = candidate.to_owned();
        }
        let error = manifest.validate().unwrap_err();
        assert_eq!(error.path(), format!("/{field}"));
    }
}

// ---------------------------------------------------------------------------
// Version model and the six authoritative version fields
// ---------------------------------------------------------------------------

#[test]
fn future_save_format_version_is_rejected() {
    // §12.2: a save whose format version is newer than the platform's maximum
    // must be rejected for ordinary use.
    let mut future = valid_manifest();
    future.save_format_version = SaveFormatVersion::new(1, 1);
    let error = future.validate().unwrap_err();
    assert_eq!(error.code_str(), "save.unsupported_format_version");
    assert_eq!(error.path(), "/save_format_version");

    let mut far_future = valid_manifest();
    far_future.save_format_version = SaveFormatVersion::new(9, 9);
    assert_eq!(
        far_future.validate().unwrap_err().code_str(),
        "save.unsupported_format_version"
    );

    // Older formats are not this increment's concern (compatibility is SAVE-005).
    let mut older = valid_manifest();
    older.save_format_version = SaveFormatVersion::new(0, 9);
    assert!(older.validate().is_ok());
}

#[test]
fn six_authoritative_version_fields_are_present_and_required() {
    // §3.4: the authoritative save-domain version set is exactly these six
    // (Save Format, World Schema, Mod Schema, Platform, Kernel API, Mod).
    let value = valid_value();
    let pointers = [
        "/save_format_version",
        "/world_schema_version",
        "/mods/0/schema_version",
        "/platform_version",
        "/kernel_api_version",
        "/mods/0/version",
    ];
    for pointer in pointers {
        let field = value
            .pointer(pointer)
            .unwrap_or_else(|| panic!("authoritative version field `{pointer}` must be present"));
        assert!(
            field.is_string(),
            "authoritative version field `{pointer}` must be a string"
        );
    }

    // Removing any one of them is rejected.
    for (pointer, expected_path) in [
        ("/save_format_version", "/save_format_version"),
        ("/world_schema_version", "/world_schema_version"),
        ("/platform_version", "/platform_version"),
        ("/kernel_api_version", "/kernel_api_version"),
    ] {
        let mut value = valid_value();
        value
            .as_object_mut()
            .unwrap()
            .remove(pointer.trim_start_matches('/'));
        let error = reject(value);
        assert_eq!(error.path(), expected_path);
    }

    // The two per-Mod fields are also required.
    let mut value = valid_value();
    value["mods"][0].as_object_mut().unwrap().remove("version");
    assert_eq!(reject(value).path(), "/mods/0/version");

    let mut value = valid_value();
    value["mods"][0]
        .as_object_mut()
        .unwrap()
        .remove("schema_version");
    assert_eq!(reject(value).path(), "/mods/0/schema_version");
}

// ---------------------------------------------------------------------------
// Schema artifact
// ---------------------------------------------------------------------------

#[test]
fn published_schema_is_draft_2020_12_and_closed() {
    let schema: Value = serde_json::from_str(SAVE_MANIFEST_SCHEMA_JSON).unwrap();
    assert_eq!(
        schema["$schema"],
        json!("https://json-schema.org/draft/2020-12/schema")
    );
    assert_eq!(schema["additionalProperties"], json!(false));
    let required: Vec<String> = schema["required"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry.as_str().unwrap().to_owned())
        .collect();
    for key in REQUIRED_TOP_LEVEL {
        assert!(
            required.contains(&key.to_owned()),
            "schema requires `{key}`"
        );
    }
    // Compiling the schema is itself part of the contract: a valid value must
    // pass through the schema-backed validator.
    save_manager::validate_manifest_value(&valid_value()).expect("schema compiles and accepts");
}
