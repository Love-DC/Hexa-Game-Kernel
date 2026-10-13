//! GK-P3-03 mod-lock.json contract tests against real on-disk Mod packages.
//!
//! These tests prove the lock file is a reproducible, verifiable artifact and
//! not just an in-memory report: the same input produces byte-identical files,
//! files round-trip, disk-restored Mod combinations match the original
//! `LoadPlan`, and every divergence (package, version, content, dependency,
//! order, Kernel API, runtime, digest) is rejected with a stable code. A
//! corrupted file must be rejected without overwriting anything.

use std::fs;
use std::path::{Path, PathBuf};

use game_platform::{
    LoadPlan, LockFile, LockFileCode, PlatformError, discover_mods, resolve_load_plan,
};

fn fixture_path(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)
}

fn load_plan(result: Result<LoadPlan, PlatformError>) -> LoadPlan {
    match result {
        Ok(plan) => plan,
        Err(error) => panic!("expected a load plan, got {error:?}"),
    }
}

fn lock_failure(result: Result<LoadPlan, PlatformError>) -> game_platform::LockFileFailure {
    match result {
        Err(PlatformError::LockFile(failure)) => *failure,
        other => panic!("expected a lock file failure, got {other:?}"),
    }
}

fn parse_failure(result: Result<LockFile, PlatformError>) -> game_platform::LockFileFailure {
    match result {
        Err(PlatformError::LockFile(failure)) => *failure,
        other => panic!("expected a lock file failure, got {other:?}"),
    }
}

/// A unique scratch directory under the target dir; removed on drop.
struct ScratchDir {
    path: PathBuf,
}

impl ScratchDir {
    fn new(name: &str) -> Self {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/lockfile-tests")
            .join(format!("{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn dep_graph() -> Vec<game_platform::ModPackage> {
    discover_mods(fixture_path("tests/fixtures/dep-graph")).unwrap()
}

#[test]
fn identical_input_produces_byte_identical_lock_files() {
    let packages = dep_graph();
    let plan = load_plan(resolve_load_plan(&packages));
    let lock = plan.to_lock_file();
    assert_eq!(lock.to_bytes().unwrap(), lock.to_bytes().unwrap());
    assert_eq!(
        plan.to_lock_file().to_bytes().unwrap(),
        lock.to_bytes().unwrap()
    );

    // Re-resolving the same packages yields the same document bytes.
    let again = load_plan(resolve_load_plan(&packages)).to_lock_file();
    assert_eq!(again.to_bytes().unwrap(), lock.to_bytes().unwrap());
}

#[test]
fn lock_file_round_trips_through_disk_and_matches_the_plan() {
    let packages = dep_graph();
    let plan = load_plan(resolve_load_plan(&packages));
    let scratch = ScratchDir::new("round-trip");
    let path = scratch.join("mod-lock.json");

    plan.to_lock_file().write_to(&path).unwrap();
    let read = LockFile::read_from(&path).unwrap();
    assert_eq!(read, plan.to_lock_file());

    let restored = read.verify(&packages).unwrap();
    assert_eq!(restored, plan, "restored plan must equal the original");
    assert_eq!(
        restored
            .mods
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>(),
        plan.mods.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        "mod order must be preserved from the lock file"
    );
}

#[test]
fn verify_rejects_a_missing_field() {
    let packages = dep_graph();
    let plan = load_plan(resolve_load_plan(&packages));
    let mut value = serde_json::to_value(plan.to_lock_file()).unwrap();
    value.as_object_mut().unwrap().remove("config_digest");

    let failure = parse_failure(LockFile::from_bytes(&serde_json::to_vec(&value).unwrap()));
    assert_eq!(failure.code, LockFileCode::InvalidFormat);
    assert_eq!(failure.code.as_str(), "platform.lock.invalid_format");
}

#[test]
fn verify_rejects_an_unknown_field() {
    let packages = dep_graph();
    let plan = load_plan(resolve_load_plan(&packages));
    let mut value = serde_json::to_value(plan.to_lock_file()).unwrap();
    value
        .as_object_mut()
        .unwrap()
        .insert("surprise".to_owned(), serde_json::json!(true));

    let failure = parse_failure(LockFile::from_bytes(&serde_json::to_vec(&value).unwrap()));
    assert_eq!(failure.code, LockFileCode::InvalidFormat);
}

#[test]
fn verify_rejects_an_unknown_lock_version() {
    let packages = dep_graph();
    let plan = load_plan(resolve_load_plan(&packages));
    let mut lock = plan.to_lock_file();
    lock.lock_version = 99;

    let failure = lock_failure(lock.verify(&packages));
    assert_eq!(failure.code, LockFileCode::UnknownVersion);
    assert_eq!(failure.code.as_str(), "platform.lock.unknown_version");
}

#[test]
fn verify_rejects_a_config_digest_mismatch() {
    let packages = dep_graph();
    let plan = load_plan(resolve_load_plan(&packages));
    let mut lock = plan.to_lock_file();
    lock.config_digest = format!("sha256:{}", "0".repeat(64));

    let failure = lock_failure(lock.verify(&packages));
    assert_eq!(failure.code, LockFileCode::ConfigDigestMismatch);
}

#[test]
fn verify_rejects_a_content_digest_mismatch() {
    let packages = dep_graph();
    let plan = load_plan(resolve_load_plan(&packages));
    let mut lock = plan.to_lock_file();
    // Tamper with one Mod's content digest only.
    lock.mods[0].content_digest = format!("sha256:{}", "1".repeat(64));
    let tampered_id = lock.mods[0].id.clone();

    let failure = lock_failure(lock.verify(&packages));
    assert_eq!(failure.code, LockFileCode::ContentDigestMismatch);
    assert_eq!(
        failure.code.as_str(),
        "platform.lock.content_digest_mismatch"
    );
    assert_eq!(failure.mod_id.as_deref(), Some(tampered_id.as_str()));
}

#[test]
fn verify_rejects_a_dependency_mismatch() {
    let packages = dep_graph();
    let plan = load_plan(resolve_load_plan(&packages));
    let mut lock = plan.to_lock_file();
    // Drop a dependency from the app entry. The dependency check compares the
    // recorded set against the package's real manifest, so it fires on its own
    // code even though the config digest would also change.
    let app = lock
        .mods
        .iter_mut()
        .find(|locked| locked.id == "org.game.app")
        .unwrap();
    app.dependencies.pop();

    let failure = lock_failure(lock.verify(&packages));
    assert_eq!(failure.code, LockFileCode::DependencyMismatch);
    assert_eq!(failure.code.as_str(), "platform.lock.dependency_mismatch");
    assert_eq!(failure.mod_id.as_deref(), Some("org.game.app"));
}

#[test]
fn verify_rejects_a_dependency_mismatch_even_when_the_digest_is_resealed() {
    // Build a lock file whose recorded digest is recomputed over the (wrong)
    // dependency list, so only the direct manifest comparison can catch it.
    let packages = dep_graph();
    let mut plan = load_plan(resolve_load_plan(&packages));
    let app = plan
        .mods
        .iter_mut()
        .find(|locked| locked.id == "org.game.app")
        .unwrap();
    app.dependencies.pop();
    // Re-seal the digest over the tampered plan so the digest check passes;
    // the dependency comparison must still reject the file on its own code.
    plan.config_digest = plan.recompute_config_digest().unwrap();

    let lock = plan.to_lock_file();
    // Sanity: the recorded digest matches the tampered list.
    assert_eq!(lock.config_digest, plan.config_digest);

    let failure = lock_failure(lock.verify(&packages));
    assert_eq!(failure.code, LockFileCode::DependencyMismatch);
    assert_eq!(failure.mod_id.as_deref(), Some("org.game.app"));
}

#[test]
fn verify_rejects_a_package_that_is_missing_from_disk() {
    let packages = dep_graph();
    let plan = load_plan(resolve_load_plan(&packages));
    let mut lock = plan.to_lock_file();
    // Add a Mod to the lock file that has no corresponding package on disk; the
    // package comparison must reject it independently of the digest.
    lock.mods.push(game_platform::LockedMod {
        id: "org.game.ghost".to_owned(),
        version: "9.9.9".to_owned(),
        content_digest: format!("sha256:{}", "1".repeat(64)),
        dependencies: Vec::new(),
    });

    let failure = lock_failure(lock.verify(&packages));
    assert_eq!(failure.code, LockFileCode::PackageMismatch);
    assert_eq!(failure.code.as_str(), "platform.lock.package_mismatch");
    assert_eq!(failure.mod_id.as_deref(), Some("org.game.ghost"));
}

#[test]
fn verify_rejects_a_lock_file_that_omits_a_resolved_mod() {
    let packages = dep_graph();
    let plan = load_plan(resolve_load_plan(&packages));
    let mut lock = plan.to_lock_file();
    lock.mods.retain(|locked| locked.id != "org.game.core");

    let failure = lock_failure(lock.verify(&packages));
    assert_eq!(failure.code, LockFileCode::PackageMismatch);
    assert_eq!(failure.mod_id.as_deref(), Some("org.game.core"));
}

#[test]
fn verify_rejects_a_version_mismatch() {
    let packages = dep_graph();
    let plan = load_plan(resolve_load_plan(&packages));
    let mut lock = plan.to_lock_file();
    lock.mods[0].version = "9.9.9".to_owned();

    let failure = lock_failure(lock.verify(&packages));
    assert_eq!(failure.code, LockFileCode::PackageMismatch);
    assert_eq!(failure.mod_id.as_deref(), Some(lock.mods[0].id.as_str()));
}

#[test]
fn verify_rejects_a_reordered_mod_list() {
    let packages = dep_graph();
    let plan = load_plan(resolve_load_plan(&packages));
    let mut lock = plan.to_lock_file();
    lock.mods.reverse();

    let failure = lock_failure(lock.verify(&packages));
    assert_eq!(failure.code, LockFileCode::OrderMismatch);
    assert_eq!(failure.code.as_str(), "platform.lock.order_mismatch");
}

#[test]
fn verify_rejects_a_runtime_mismatch() {
    let packages = dep_graph();
    let plan = load_plan(resolve_load_plan(&packages));
    let mut lock = plan.to_lock_file();
    lock.runtime = "wasmtime-0.0.0".to_owned();

    let failure = lock_failure(lock.verify(&packages));
    assert_eq!(failure.code, LockFileCode::ConfigDigestMismatch);
}

#[test]
fn verify_rejects_a_kernel_api_mismatch() {
    let packages = dep_graph();
    let plan = load_plan(resolve_load_plan(&packages));
    let mut lock = plan.to_lock_file();
    lock.kernel_api = "9.9".to_owned();

    let failure = lock_failure(lock.verify(&packages));
    assert_eq!(failure.code, LockFileCode::ConfigDigestMismatch);
}

#[test]
fn a_corrupted_file_is_rejected_and_the_original_is_not_overwritten() {
    let scratch = ScratchDir::new("corrupt");
    let path = scratch.join("mod-lock.json");

    // Write a valid lock file, then corrupt it in place.
    let packages = dep_graph();
    let plan = load_plan(resolve_load_plan(&packages));
    plan.to_lock_file().write_to(&path).unwrap();
    let valid_bytes = fs::read(&path).unwrap();

    fs::write(&path, b"{ not valid json").unwrap();
    let corrupted_before = fs::read(&path).unwrap();

    // Reading the corrupted file fails and leaves the file untouched.
    let failure = parse_failure(LockFile::read_from(&path));
    assert_eq!(failure.code, LockFileCode::InvalidFormat);
    assert_eq!(
        fs::read(&path).unwrap(),
        corrupted_before,
        "a failed read must not modify the lock file"
    );

    // A failing write (missing parent is created, but an invalid path is not)
    // must not truncate an existing file: the atomic writer stages a temp file
    // first. Restore the valid file and confirm a fresh write still works.
    fs::write(&path, &valid_bytes).unwrap();
    plan.to_lock_file().write_to(&path).unwrap();
    assert_eq!(fs::read(&path).unwrap(), valid_bytes);
}

#[test]
fn the_lock_file_is_persisted_as_a_real_file() {
    let scratch = ScratchDir::new("persist");
    let path = scratch.join("mod-lock.json");
    let packages = dep_graph();
    let plan = load_plan(resolve_load_plan(&packages));
    plan.to_lock_file().write_to(&path).unwrap();

    assert!(path.is_file(), "lock file must exist on disk");
    let text = fs::read_to_string(&path).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed["lock_version"], serde_json::json!(1));
    assert!(
        parsed["config_digest"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );
    assert!(text.ends_with('\n'), "lock file must end with a newline");
}
