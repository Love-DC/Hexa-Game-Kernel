use kernel_core::{
    Command, CommandEnvelope, CommandId, CommandRejectionCode, ComponentValue, Kernel, StepInput,
    Tick, World, WorldId,
};

const SOURCE: &str = "test.move/host";

fn seeded_kernel() -> Kernel {
    let mut world = World::new(WorldId(1));
    for key in ["demo.move/pos_x", "demo.move/pos_y", "demo.move/pos_z"] {
        world
            .seed_resource(key, ComponentValue::Integer(0))
            .unwrap();
    }
    Kernel::new(world).unwrap()
}

fn pos(kernel: &Kernel) -> (i64, i64, i64) {
    let world = kernel.world();
    (
        world.resource_i64("demo.move/pos_x").unwrap_or(0),
        world.resource_i64("demo.move/pos_y").unwrap_or(0),
        world.resource_i64("demo.move/pos_z").unwrap_or(0),
    )
}

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

fn run_script(kernel: &mut Kernel, script: &[(i64, i64, i64)]) {
    for &(dx, dy, dz) in script {
        let output = kernel
            .step(StepInput {
                commands: move_commands(kernel.world().tick(), dx, dy, dz, pos(kernel)),
            })
            .unwrap();
        assert_eq!(output.rejected_commands.len(), 0);
        assert_eq!(output.committed_commands.len(), 3);
    }
}

#[test]
fn move_advances_position_through_command_pipeline() {
    let mut kernel = seeded_kernel();
    assert_eq!(pos(&kernel), (0, 0, 0));

    run_script(&mut kernel, &[(1, 0, 0)]);
    assert_eq!(pos(&kernel), (1, 0, 0));

    run_script(&mut kernel, &[(0, 0, -2)]);
    assert_eq!(pos(&kernel), (1, 0, -2));
}

#[test]
fn move_demo_is_deterministic_across_runs() {
    let script = [(1, 0, 0), (0, 0, -2), (3, 3, 3)];

    let mut first = seeded_kernel();
    run_script(&mut first, &script);
    let mut second = seeded_kernel();
    run_script(&mut second, &script);

    let hash_first = first.step(StepInput::default()).unwrap().state_hash;
    let hash_second = second.step(StepInput::default()).unwrap().state_hash;
    assert_eq!(hash_first, hash_second);
}

#[test]
fn pipeline_rejects_command_for_wrong_tick() {
    let mut kernel = seeded_kernel();
    assert_eq!(pos(&kernel), (0, 0, 0));

    // World starts at Tick(0). A command claiming Tick(1) targets the future.
    let output = kernel
        .step(StepInput {
            commands: move_commands(Tick(1), 1, 0, 0, pos(&kernel)),
        })
        .unwrap();

    assert_eq!(output.committed_commands.len(), 0);
    assert_eq!(output.rejected_commands.len(), 3);
    assert!(
        output
            .rejected_commands
            .iter()
            .all(|rejection| rejection.code == CommandRejectionCode::WrongTargetTick)
    );
    assert_eq!(pos(&kernel), (0, 0, 0));
}
