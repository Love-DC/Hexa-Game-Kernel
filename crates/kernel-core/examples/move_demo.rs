//! Move demo: drives world state exclusively through the Kernel command
//! pipeline. Not an authoritative module — see planning/31 for the ruling.

use kernel_core::{
    Command, CommandEnvelope, CommandId, ComponentValue, Kernel, StepInput, Tick, World, WorldId,
};

const SOURCE: &str = "demo.move/host";

fn current_pos(world: &World) -> (i64, i64, i64) {
    (
        world.resource_i64("demo.move/pos_x").unwrap_or(0),
        world.resource_i64("demo.move/pos_y").unwrap_or(0),
        world.resource_i64("demo.move/pos_z").unwrap_or(0),
    )
}

/// Translates one Move intent into absolute SetResource commands. The host
/// reads current state; the Kernel pipeline validates, orders, and commits.
fn move_commands(
    tick: Tick,
    dx: i64,
    dy: i64,
    dz: i64,
    at: (i64, i64, i64),
) -> Vec<CommandEnvelope> {
    let axes = [
        ("demo.move/pos_x", at.0 + dx),
        ("demo.move/pos_y", at.1 + dy),
        ("demo.move/pos_z", at.2 + dz),
    ];
    axes.into_iter()
        .enumerate()
        .map(|(sequence, (key, value))| CommandEnvelope {
            id: CommandId {
                tick,
                source: SOURCE.to_owned(),
                sequence: sequence as u64,
            },
            command: Command::SetResource {
                key: key.to_owned(),
                value: ComponentValue::Integer(value),
            },
        })
        .collect()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut world = World::new(WorldId(1));
    for key in ["demo.move/pos_x", "demo.move/pos_y", "demo.move/pos_z"] {
        world.seed_resource(key, ComponentValue::Integer(0))?;
    }
    let mut kernel = Kernel::new(world)?;

    let (x, y, z) = current_pos(kernel.world());
    println!("initial: ({x},{y},{z})");

    for (dx, dy, dz) in [(1, 0, 0), (0, 0, -2)] {
        let tick = kernel.world().tick();
        let output = kernel.step(StepInput {
            commands: move_commands(tick, dx, dy, dz, current_pos(kernel.world())),
        })?;
        let (x, y, z) = current_pos(kernel.world());
        println!(
            "tick={} move=({dx},{dy},{dz}) pos=({x},{y},{z}) committed={} rejected={} hash={}",
            output.tick.0,
            output.committed_commands.len(),
            output.rejected_commands.len(),
            output.state_hash.to_hex()
        );
    }
    Ok(())
}
