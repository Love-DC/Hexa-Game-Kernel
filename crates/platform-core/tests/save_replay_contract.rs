use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use game_platform::{LoadedWasmMod, ModPackage, create_wasm_engine};
use kernel_core::{ComponentValue, Kernel, StepInput, Tick, World, WorldId};
use save_manager::loader::inspect_save_file;
use save_manager::partition::{ModNamespace, SavePartition, SavePartitionId};
use save_manager::writer;
use save_manager::{
    GameDefinition, Integrity, SaveFormatVersion as V, SaveManifest, SaveModEntry, SimulationConfig,
};

static NEXT: AtomicU64 = AtomicU64::new(0);

fn project_path(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)
}

fn save_path() -> PathBuf {
    std::env::temp_dir().join(format!(
        "gksave-replay-{}-{}.gksave",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

fn kernel_with_counter() -> (Kernel, LoadedWasmMod) {
    let package = ModPackage::load(project_path("../../examples/mods/p1-counter")).unwrap();
    let loaded = LoadedWasmMod::compile(
        &package,
        &BTreeSet::from(["world.read".to_owned(), "world.command".to_owned()]),
        &create_wasm_engine().unwrap(),
    )
    .unwrap();
    let mut world = World::new(WorldId(921));
    world
        .seed_resource("org.game/counter", ComponentValue::Integer(0))
        .unwrap();
    let mut kernel = Kernel::new(world).unwrap();
    kernel.register_system(Box::new(loaded.system())).unwrap();
    (kernel, loaded)
}

#[test]
fn save_restore_and_same_boundary_input_replay_match_state_hash() {
    let (mut uninterrupted, loaded) = kernel_with_counter();
    for _ in 0..3 {
        uninterrupted.step(StepInput::default()).unwrap();
    }
    let snapshot = uninterrupted.snapshot().unwrap();
    let target = save_path();
    let manifest = SaveManifest {
        save_format_version: V::new(1, 0),
        save_id: "save.replay.golden".into(),
        created_at: "2026-10-19T00:00:00Z".into(),
        last_saved_at: "2026-10-19T00:00:00Z".into(),
        platform_version: "1.0.0".into(),
        game_definition: GameDefinition {
            id: "org.game.example".into(),
            version: "1.0.0".into(),
        },
        kernel_api_version: "0.1".into(),
        world_schema_version: "1.0".into(),
        logical_tick: uninterrupted.world().tick(),
        mods: vec![SaveModEntry {
            id: "org.game.p1-counter".into(),
            version: "0.1.0".into(),
            schema_version: "1.0".into(),
        }],
        simulation_config: SimulationConfig::new(),
        integrity: Integrity {
            algorithm: "sha256".into(),
            root_hash: "0".repeat(64),
        },
    };
    writer::save(
        &target,
        &manifest,
        &snapshot,
        &[SavePartition::new(
            SavePartitionId::Mod(ModNamespace::parse("org.game.p1-counter").unwrap()),
            b"",
        )],
        &["org.game.p1-counter"],
    )
    .unwrap();
    let inspected = inspect_save_file(&target).unwrap();
    assert_eq!(
        inspected.manifest().integrity.root_hash,
        "2a19eb2ef1e4828fabaac7249183a92c5c922938d4b4cb2cdd475aa9f6029970",
        "the P4 replay archive must remain a stable golden vector"
    );
    assert_eq!(inspected.world_payload(), snapshot);
    let mut expected_hash = None;
    for _ in 0..5 {
        expected_hash = Some(uninterrupted.step(StepInput::default()).unwrap().state_hash);
    }

    let mut restored = Kernel::from_snapshot(inspected.world_payload()).unwrap();
    restored.register_system(Box::new(loaded.system())).unwrap();
    let mut actual_hash = None;
    for _ in 0..5 {
        actual_hash = Some(restored.step(StepInput::default()).unwrap().state_hash);
    }
    assert_eq!(actual_hash, expected_hash);
    assert_eq!(restored.world().tick(), Tick(8));
    assert_eq!(restored.world().resource_i64("org.game/counter"), Some(8));
    let _ = std::fs::remove_file(target);
}
