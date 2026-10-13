//! GK-P3-04 host API registry / catalog contract tests.
//!
//! These tests prove the machine-readable catalog and the runtime allow-list are
//! the same source, not two hand-maintained lists kept in sync by hand:
//!
//! * The **real** Wasmtime `Linker` built by the Platform is enumerated via
//!   [`execute_with_linker_probe`], and its registered `(module, field)` set and
//!   actual `FuncType` signatures are compared against the Registry/Catalog.
//! * Unknown imports, wrong signatures and unrequested permissions are rejected
//!   before any Mod logic runs.
//! * The Catalog is deterministic, schema-valid and limited to implemented APIs.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use game_platform::{
    ApiCatalogCode, ApiCatalogEntry, ModPackage, PlatformErrorCode, api_catalog,
    create_wasm_engine, validate_api_catalog_value, validate_registry,
};
use serde_json::Value;

/// Scratch Mod directory under the target dir; removed on drop.
struct ScratchDir {
    path: PathBuf,
}

impl ScratchDir {
    fn new(name: &str) -> Self {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/api-registry-tests")
            .join(format!("{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    /// Writes a Mod package (manifest + entrypoint) into this scratch dir.
    fn write_mod(&self, manifest: &str, wat: &str) -> PathBuf {
        let manifest_json: Value = serde_json::from_str(manifest).unwrap();
        let entrypoint = manifest_json["entrypoint"]
            .as_str()
            .expect("manifest must declare an entrypoint")
            .to_owned();
        fs::write(self.path.join("mod.json"), manifest).unwrap();
        fs::write(self.path.join(&entrypoint), wat).unwrap();
        self.path.clone()
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn grants_both() -> BTreeSet<String> {
    BTreeSet::from(["world.read".to_owned(), "world.command".to_owned()])
}

/// Compiles a package and returns the resulting `PlatformError`, panicking if
/// compilation unexpectedly succeeded. Avoids requiring `Debug` on
/// `LoadedWasmMod`.
fn compile_failure(
    package: &ModPackage,
    grants: &BTreeSet<String>,
    engine: &game_platform::WasmEngine,
) -> game_platform::PlatformError {
    match game_platform::LoadedWasmMod::compile(package, grants, engine) {
        Ok(_) => panic!("expected compilation to fail, but it succeeded"),
        Err(error) => error,
    }
}

/// The real Linker registration surface, extracted from Wasmtime itself.
struct LinkerProbe {
    entries: Vec<(String, String, String)>, // (module, field, rendered signature)
}

/// Enumerates the Platform's actual Wasmtime `Linker` entries.
///
/// The Platform exposes no public accessor for the linker, so this uses the
/// exported `execute_with_linker_probe` hook (test-only, see lib.rs) that walks
/// `Linker::iter` on the real linker and reports each import's true signature.
fn probe_real_linker() -> LinkerProbe {
    let entries = game_platform::linker_registration_probe()
        .expect("the Platform must expose its real linker registrations for contract testing");
    LinkerProbe { entries }
}

// ---------------------------------------------------------------------------
// 1. Registry / Linker sameness — the core acceptance requirement.
// ---------------------------------------------------------------------------

#[test]
fn catalog_and_real_linker_are_the_same_set() {
    let catalog = api_catalog();
    let probe = probe_real_linker();

    let catalog_keys = catalog
        .apis
        .iter()
        .map(|entry| {
            let (module, field) = entry
                .id
                .split_once("::")
                .expect("catalog id must be `<module>::<field>`");
            (module.to_owned(), field.to_owned())
        })
        .collect::<BTreeSet<_>>();

    let linker_keys = probe
        .entries
        .iter()
        .map(|(module, field, _)| (module.clone(), field.clone()))
        .collect::<BTreeSet<_>>();

    assert_eq!(
        catalog_keys, linker_keys,
        "the catalog and the real Wasm Linker must register exactly the same imports"
    );
    assert!(
        !linker_keys.is_empty(),
        "the linker must actually register the counter host APIs"
    );
}

#[test]
fn catalog_signatures_match_real_linker_signatures() {
    let catalog = api_catalog();
    let probe = probe_real_linker();

    for (module, field, actual_signature) in &probe.entries {
        let entry = catalog
            .apis
            .iter()
            .find(|entry| entry.id == format!("{module}::{field}"))
            .unwrap_or_else(|| panic!("linker entry `{module}::{field}` missing from catalog"));
        let expected = render_signature(entry);
        assert_eq!(
            &expected, actual_signature,
            "catalog signature for `{module}::{field}` must equal the real registered signature"
        );
    }
}

fn render_signature(entry: &ApiCatalogEntry) -> String {
    let params = entry
        .params
        .iter()
        .map(|kind| kind.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let results = entry
        .results
        .iter()
        .map(|kind| kind.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    format!("({params}) -> ({results})")
}

// ---------------------------------------------------------------------------
// 2. Registry self-consistency + rejection of unimplemented surface.
// ---------------------------------------------------------------------------

#[test]
fn registry_is_internally_consistent() {
    validate_registry().expect("the built-in registry must be self-consistent");
}

#[test]
fn catalog_only_contains_implemented_host_apis() {
    let catalog = api_catalog();
    let ids = catalog
        .apis
        .iter()
        .map(|entry| entry.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(ids, vec!["gk::counter_read", "gk::counter_set"]);
    // Permissions declared in the wider specification but not implemented must
    // not appear.
    for entry in &catalog.apis {
        assert!(
            entry.permission == "world.read" || entry.permission == "world.command",
            "catalog must not claim an unimplemented permission: {}",
            entry.permission
        );
    }
}

#[test]
fn catalog_permissions_equal_the_manifest_allow_set() {
    let catalog = api_catalog();
    let catalog_permissions = catalog
        .apis
        .iter()
        .map(|entry| entry.permission.clone())
        .collect::<BTreeSet<_>>();

    // A Mod requesting exactly the catalog's permissions must load; one
    // requesting anything outside must be rejected. This ties the catalog's
    // permission set to the enforcement path.
    let engine = create_wasm_engine().unwrap();
    let scratch = ScratchDir::new("perm-set");

    let okay = scratch.write_mod(
        r#"{
  "manifest_version": 1,
  "id": "org.game.perm-ok",
  "version": "0.1.0",
  "kernel_api": "0.1",
  "entrypoint": "module.wat",
  "requested_permissions": ["world.read", "world.command"],
  "dependencies": []
}"#,
        "(module (import \"gk\" \"counter_read\" (func $r (result i64))) (func (export \"on_tick\")))",
    );
    let package = ModPackage::load(&okay).unwrap();
    game_platform::LoadedWasmMod::compile(&package, &grants_both(), &engine)
        .expect("a Mod requesting exactly the catalog permissions must compile");

    // Every catalog permission is one the manifest validator accepts; anything
    // else is rejected. Confirm the rejection path for an unknown permission.
    let bad_scratch = ScratchDir::new("perm-bad");
    let bad = bad_scratch.write_mod(
        r#"{
  "manifest_version": 1,
  "id": "org.game.perm-bad",
  "version": "0.1.0",
  "kernel_api": "0.1",
  "entrypoint": "module.wat",
  "requested_permissions": ["event.subscribe"],
  "dependencies": []
}"#,
        "(module (func (export \"on_tick\")))",
    );
    let failure = ModPackage::load(&bad).unwrap_err();
    assert_eq!(failure.code(), PlatformErrorCode::InvalidManifest);
    assert_eq!(catalog_permissions.len(), 2);
}

// ---------------------------------------------------------------------------
// 3. Unknown import / wrong signature / unrequested permission rejections.
// ---------------------------------------------------------------------------

#[test]
fn unknown_import_is_rejected_before_execution() {
    let engine = create_wasm_engine().unwrap();
    let scratch = ScratchDir::new("unknown-import");
    let dir = scratch.write_mod(
        r#"{
  "manifest_version": 1,
  "id": "org.game.unknown-import",
  "version": "0.1.0",
  "kernel_api": "0.1",
  "entrypoint": "module.wat",
  "requested_permissions": ["world.read", "world.command"],
  "dependencies": []
}"#,
        "(module (import \"gk\" \"counter_reset\" (func $r)) (func (export \"on_tick\")))",
    );
    let package = ModPackage::load(&dir).unwrap();
    let failure = compile_failure(&package, &grants_both(), &engine);
    assert_eq!(failure.code(), PlatformErrorCode::WasmModule);
}

#[test]
fn wrong_import_signature_is_rejected() {
    let engine = create_wasm_engine().unwrap();
    let scratch = ScratchDir::new("wrong-signature");
    // `counter_read` is registered as `() -> i64`; declare it as `(i32) -> i32`.
    let dir = scratch.write_mod(
        r#"{
  "manifest_version": 1,
  "id": "org.game.wrong-signature",
  "version": "0.1.0",
  "kernel_api": "0.1",
  "entrypoint": "module.wat",
  "requested_permissions": ["world.read", "world.command"],
  "dependencies": []
}"#,
        "(module (import \"gk\" \"counter_read\" (func $r (param i32) (result i32))) (func (export \"on_tick\")))",
    );
    let package = ModPackage::load(&dir).unwrap();
    let failure = compile_failure(&package, &grants_both(), &engine);
    assert_eq!(failure.code(), PlatformErrorCode::ApiSignatureMismatch);
}

#[test]
fn unrequested_permission_is_rejected() {
    let engine = create_wasm_engine().unwrap();
    let scratch = ScratchDir::new("unrequested-perm");
    let dir = scratch.write_mod(
        r#"{
  "manifest_version": 1,
  "id": "org.game.unrequested",
  "version": "0.1.0",
  "kernel_api": "0.1",
  "entrypoint": "module.wat",
  "requested_permissions": ["world.read"],
  "dependencies": []
}"#,
        "(module (import \"gk\" \"counter_read\" (func $r (result i64))) (import \"gk\" \"counter_set\" (func $w (param i64))) (func (export \"on_tick\")))",
    );
    let package = ModPackage::load(&dir).unwrap();
    // Grants include world.command, but the Manifest did not request it.
    let failure = compile_failure(&package, &grants_both(), &engine);
    assert_eq!(failure.code(), PlatformErrorCode::PermissionNotRequested);
}

// ---------------------------------------------------------------------------
// 4. Catalog versioning, determinism, sorting and schema.
// ---------------------------------------------------------------------------

#[test]
fn unsupported_catalog_version_is_rejected() {
    let mut catalog = api_catalog();
    catalog.catalog_version = 999;
    let failure = catalog
        .verify()
        .expect_err("unknown catalog version must fail");
    assert_eq!(failure.code(), PlatformErrorCode::ApiCatalog);
    match failure {
        game_platform::PlatformError::ApiCatalog(failure) => {
            assert_eq!(failure.code(), ApiCatalogCode::UnknownVersion);
        }
        other => panic!("expected an API catalog failure, got {other:?}"),
    }
}

#[test]
fn unsupported_api_version_is_rejected() {
    let mut catalog = api_catalog();
    catalog.apis[0].api_version = 999;
    let failure = catalog
        .verify()
        .expect_err("an unsupported API version must fail");
    match failure {
        game_platform::PlatformError::ApiCatalog(failure) => {
            assert_eq!(failure.code(), ApiCatalogCode::UnsupportedApiVersion);
        }
        other => panic!("expected an API catalog failure, got {other:?}"),
    }
}

#[test]
fn catalog_bytes_are_deterministic_and_sorted() {
    let catalog = api_catalog();
    let first = catalog.to_bytes().unwrap();
    let second = api_catalog().to_bytes().unwrap();
    assert_eq!(
        first, second,
        "repeated catalog export must be byte-identical"
    );
    assert!(
        first.ends_with(b"\n"),
        "catalog bytes must end with a newline"
    );

    let ids = catalog
        .apis
        .iter()
        .map(|entry| entry.id.clone())
        .collect::<Vec<_>>();
    let mut sorted = ids.clone();
    sorted.sort();
    assert_eq!(ids, sorted, "catalog entries must be sorted by API ID");
}

#[test]
fn catalog_export_satisfies_the_published_schema() {
    let catalog = api_catalog();
    let value: Value = serde_json::from_slice(&catalog.to_bytes().unwrap()).unwrap();
    validate_api_catalog_value(&value).expect("catalog export must satisfy the v1 catalog schema");

    // The schema must actually reject malformed catalogs, not accept anything.
    let mut broken = value.clone();
    broken["apis"][0]["permission"] = Value::from(123);
    assert!(
        validate_api_catalog_value(&broken).is_err(),
        "the schema must reject a mistyped entry"
    );
}

#[test]
fn catalog_rejects_duplicate_api_ids() {
    let mut catalog = api_catalog();
    let duplicate = catalog.apis[0].clone();
    catalog.apis.push(duplicate);
    let failure = catalog.verify().expect_err("duplicate IDs must fail");
    match failure {
        game_platform::PlatformError::ApiCatalog(failure) => {
            assert_eq!(failure.code(), ApiCatalogCode::DuplicateApiId);
        }
        other => panic!("expected an API catalog failure, got {other:?}"),
    }
}
