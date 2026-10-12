use std::collections::BTreeSet;
use std::error::Error;
use std::path::Path;

use game_platform::{LoadedWasmMod, create_wasm_engine, discover_mods, resolve_load_plan};
use kernel_core::{ComponentValue, Kernel, StepInput, World, WorldId};

fn main() -> Result<(), Box<dyn Error>> {
    let command = std::env::args().nth(1);
    if command.as_deref() != Some("demo") {
        eprintln!("usage: cargo run -p game-platform -- demo");
        std::process::exit(2);
    }

    let mods_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/mods");
    let packages = discover_mods(mods_root)?;
    let load_plan = resolve_load_plan(&packages)?;
    let package = packages
        .iter()
        .find(|package| package.manifest.id == "org.game.p1-counter")
        .ok_or("P1 counter Mod was not discovered")?;
    let grants = BTreeSet::from(["world.read".to_owned(), "world.command".to_owned()]);
    let engine = create_wasm_engine()?;
    let loaded = LoadedWasmMod::compile(package, &grants, &engine)?;

    let mut world = World::new(WorldId(1));
    world.seed_resource("org.game/counter", ComponentValue::Integer(0))?;
    let mut kernel = Kernel::new(world)?;
    kernel.register_system(Box::new(loaded.system()))?;

    for _ in 0..3 {
        let output = kernel.step(StepInput::default())?;
        println!(
            "tick={} counter={} state_hash={}",
            output.tick.0,
            kernel
                .world()
                .resource_i64("org.game/counter")
                .unwrap_or_default(),
            output.state_hash.to_hex()
        );
    }

    let snapshot = kernel.snapshot()?;
    let expected = {
        let mut continued = kernel;
        let mut final_hash = None;
        for _ in 0..3 {
            final_hash = Some(continued.step(StepInput::default())?.state_hash);
        }
        final_hash.ok_or("continuation did not advance")?
    };
    let mut restored = Kernel::from_snapshot(&snapshot)?;
    restored.register_system(Box::new(loaded.system()))?;
    let mut actual = None;
    for _ in 0..3 {
        actual = Some(restored.step(StepInput::default())?.state_hash);
    }
    let actual = actual.ok_or("restore did not advance")?;
    if expected != actual {
        return Err("snapshot continuation did not match uninterrupted execution".into());
    }

    println!(
        "mod={} content_digest={} locked_config={} resumed_hash={} verified=true",
        package.manifest.id,
        loaded.content_digest,
        load_plan.config_digest,
        actual.to_hex()
    );
    Ok(())
}
