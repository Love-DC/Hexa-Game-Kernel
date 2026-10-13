use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use game_platform::{
    LoadedWasmMod, ModPackage, create_wasm_engine, discover_mods, resolve_load_plan,
};
use kernel_core::{
    Command, CommandEnvelope, CommandId, ComponentValue, Kernel, KernelError, StepInput, Tick,
    World, WorldId,
};

fn project_path(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)
}

fn grants() -> BTreeSet<String> {
    BTreeSet::from(["world.read".to_owned(), "world.command".to_owned()])
}

fn register_counter(kernel: &mut Kernel) -> LoadedWasmMod {
    let package = ModPackage::load(project_path("../../examples/mods/p1-counter")).unwrap();
    let runtime = create_wasm_engine().unwrap();
    let loaded = LoadedWasmMod::compile(&package, &grants(), &runtime).unwrap();
    kernel.register_system(Box::new(loaded.system())).unwrap();
    loaded
}

fn new_counter_kernel() -> Kernel {
    let mut world = World::new(WorldId(42));
    world
        .seed_resource("org.game/counter", ComponentValue::Integer(0))
        .unwrap();
    Kernel::new(world).unwrap()
}

#[test]
fn discovery_lock_execution_and_snapshot_replay_form_one_vertical_slice() {
    let packages = discover_mods(project_path("../../examples/mods")).unwrap();
    assert_eq!(packages.len(), 1);
    let plan = resolve_load_plan(&packages).unwrap();
    assert_eq!(plan.mods[0].id, "org.game.p1-counter");

    let mut uninterrupted = new_counter_kernel();
    let loaded = register_counter(&mut uninterrupted);
    for _ in 0..3 {
        uninterrupted.step(StepInput::default()).unwrap();
    }
    assert_eq!(
        uninterrupted.world().resource_i64("org.game/counter"),
        Some(3)
    );
    let snapshot = uninterrupted.snapshot().unwrap();

    for _ in 0..4 {
        uninterrupted.step(StepInput::default()).unwrap();
    }
    let expected_hash = uninterrupted.step(StepInput::default()).unwrap().state_hash;

    let mut restored = Kernel::from_snapshot(&snapshot).unwrap();
    restored.register_system(Box::new(loaded.system())).unwrap();
    let mut actual_hash = None;
    for _ in 0..5 {
        actual_hash = Some(restored.step(StepInput::default()).unwrap().state_hash);
    }

    assert_eq!(actual_hash.unwrap(), expected_hash);
    assert_eq!(restored.world().tick(), Tick(8));
    assert_eq!(restored.world().resource_i64("org.game/counter"), Some(8));
}

#[test]
fn wasm_trap_rolls_back_external_commands_and_requires_reconstruction() {
    let package = ModPackage::load(project_path("tests/fixtures/fuel-loop")).unwrap();
    let engine = create_wasm_engine().unwrap();
    let loaded = LoadedWasmMod::compile(&package, &grants(), &engine).unwrap();
    let mut kernel = new_counter_kernel();
    kernel.register_system(Box::new(loaded.system())).unwrap();

    let command = CommandEnvelope {
        id: CommandId {
            tick: Tick(0),
            source: "external/test".to_owned(),
            sequence: 0,
        },
        command: Command::SetResource {
            key: "org.game/counter".to_owned(),
            value: ComponentValue::Integer(99),
        },
    };
    let result = kernel.step(StepInput {
        commands: vec![command],
    });

    let failure = match result {
        Err(KernelError::SystemFailed(failure)) => failure,
        other => panic!("expected stable Mod failure, got {other:?}"),
    };
    assert_eq!(failure.format_version, 1);
    // GK-P3-06: runtime fuel exhaustion now reports its frozen stable code
    // (frozen design §3.3 C.1) instead of the blanket `platform.mod.execution_failed`.
    assert_eq!(failure.code, "platform.mod.resource.fuel_exceeded");
    assert_eq!(failure.system, "org.game.fuel-loop");
    assert_eq!(failure.tick, Tick(0));
    assert_eq!(
        failure.source_digest.as_deref(),
        Some(package.content_digest.as_str())
    );
    let report = serde_json::to_value(&failure).unwrap();
    assert_eq!(report["format_version"], 1);
    assert_eq!(report["code"], "platform.mod.resource.fuel_exceeded");
    assert_eq!(report["tick"], 0);
    assert_eq!(kernel.world().tick(), Tick(0));
    assert_eq!(kernel.world().resource_i64("org.game/counter"), Some(0));
    assert!(matches!(
        kernel.step(StepInput::default()),
        Err(KernelError::Faulted(_))
    ));
}

#[test]
fn package_entrypoint_cannot_escape_its_root() {
    let result = ModPackage::load(project_path("tests/fixtures/path-escape"));
    assert!(result.is_err());
}
