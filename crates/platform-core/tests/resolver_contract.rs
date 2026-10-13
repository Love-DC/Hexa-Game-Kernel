//! GK-P3-02 resolver contract tests against real on-disk Mod packages.
//!
//! The unit tests in `platform_core` cover the failure taxonomy on synthetic
//! packages; this file proves the same contract end to end from
//! `discover_mods` to `resolve_load_plan` using directories a Launcher would
//! actually hand over.

use std::path::{Path, PathBuf};

use game_platform::{
    DependencyResolutionCode, DependencyResolutionFailure, LoadPlan, PlatformError, discover_mods,
    resolve_load_plan,
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

fn dependency_failure(result: Result<LoadPlan, PlatformError>) -> DependencyResolutionFailure {
    match result {
        Err(PlatformError::DependencyResolution(failure)) => *failure,
        other => panic!("expected a dependency resolution failure, got {other:?}"),
    }
}

fn ordered_ids(plan: &LoadPlan) -> Vec<&str> {
    plan.mods.iter().map(|locked| locked.id.as_str()).collect()
}

#[test]
fn dependency_graph_is_locked_in_one_stable_order() {
    let packages = discover_mods(fixture_path("tests/fixtures/dep-graph")).unwrap();
    assert_eq!(
        packages
            .iter()
            .map(|package| package.manifest.id.as_str())
            .collect::<Vec<_>>(),
        [
            "org.game.app",
            "org.game.core",
            "org.game.lib-a",
            "org.game.lib-b"
        ]
    );

    let plan = load_plan(resolve_load_plan(&packages));
    assert_eq!(
        ordered_ids(&plan),
        [
            "org.game.core",
            "org.game.lib-a",
            "org.game.lib-b",
            "org.game.app"
        ],
        "required dependencies must be initialized before their dependents"
    );
    let app = plan
        .mods
        .iter()
        .find(|locked| locked.id == "org.game.app")
        .unwrap();
    assert_eq!(
        app.dependencies,
        ["org.game.lib-a", "org.game.lib-b"],
        "locked dependencies must be canonical even though the manifest lists lib-b first"
    );
    assert_eq!(app.version, "1.0.0");
    assert!(app.content_digest.starts_with("sha256:"));
    assert!(plan.config_digest.starts_with("sha256:"));
}

#[test]
fn every_candidate_order_yields_the_same_locked_plan() {
    let packages = discover_mods(fixture_path("tests/fixtures/dep-graph")).unwrap();
    let expected = load_plan(resolve_load_plan(&packages));

    // Rotations cover every position for the first candidate and, combined
    // with a reversed slice, every relative order the Launcher could supply.
    for offset in 0..packages.len() {
        let mut rotated = packages.clone();
        rotated.rotate_left(offset);
        assert_eq!(load_plan(resolve_load_plan(&rotated)), expected);
    }
    let mut reversed = packages.clone();
    reversed.reverse();
    assert_eq!(load_plan(resolve_load_plan(&reversed)), expected);

    for _ in 0..3 {
        assert_eq!(load_plan(resolve_load_plan(&packages)), expected);
    }
}

#[test]
fn missing_required_dependency_fails_the_whole_plan_with_identities() {
    let packages = discover_mods(fixture_path("tests/fixtures/dep-graph-broken")).unwrap();
    let failure = dependency_failure(resolve_load_plan(&packages));
    assert_eq!(failure.code, DependencyResolutionCode::MissingDependency);
    assert_eq!(
        failure.code.as_str(),
        "platform.dependency.missing_dependency"
    );
    assert_eq!(failure.mod_id.as_deref(), Some("org.game.app"));
    assert_eq!(failure.dependency_id.as_deref(), Some("org.game.absent"));
    assert_eq!(failure.requirement.as_deref(), Some("^1.0.0"));
    assert!(failure.cycle.is_empty());
    assert_eq!(
        PlatformError::DependencyResolution(Box::new(failure))
            .code()
            .as_str(),
        "platform.dependency_resolution"
    );
}
