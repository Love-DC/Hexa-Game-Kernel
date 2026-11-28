//! Regression tests for collisions between external and Kernel-generated CommandIds.

use std::collections::BTreeSet;

use kernel_core::{
    Command, CommandEnvelope, CommandId, CommandRejectionCode, ComponentValue, Kernel, StepInput,
    System, SystemError, Tick, World, WorldId,
};

struct CounterSystem;

impl System for CounterSystem {
    fn id(&self) -> &str {
        "org.game/counter-writer"
    }

    fn execute(&self, _world: &World, _tick: Tick) -> Result<Vec<Command>, SystemError> {
        Ok(vec![Command::SetResource {
            key: "org.game/counter".to_owned(),
            value: ComponentValue::Integer(1),
        }])
    }
}

#[test]
fn external_command_cannot_impersonate_a_kernel_generated_system_command() {
    let mut world = World::new(WorldId(1));
    world
        .seed_resource("org.game/counter", ComponentValue::Integer(0))
        .unwrap();
    let mut kernel = Kernel::new(world).unwrap();
    kernel.register_system(Box::new(CounterSystem)).unwrap();

    let colliding_id = CommandId {
        tick: Tick(0),
        source: "system/org.game/counter-writer".to_owned(),
        sequence: 0,
    };
    let output = kernel
        .step(StepInput {
            commands: vec![CommandEnvelope {
                id: colliding_id.clone(),
                command: Command::SetResource {
                    key: "org.game/counter".to_owned(),
                    value: ComponentValue::Integer(999),
                },
            }],
        })
        .unwrap();

    assert_eq!(output.rejected_commands.len(), 1);
    assert_eq!(output.rejected_commands[0].id, colliding_id);
    assert_eq!(
        output.rejected_commands[0].code,
        CommandRejectionCode::InvalidSourceId
    );

    let generated_id = CommandId {
        tick: Tick(0),
        source: "system/org.game/counter-writer".to_owned(),
        sequence: 0,
    };
    assert_eq!(output.committed_commands, [generated_id]);
    let unique_ids: BTreeSet<_> = output.committed_commands.iter().collect();
    assert_eq!(unique_ids.len(), output.committed_commands.len());

    assert_eq!(output.events.len(), 1);
    assert_eq!(output.events[0].source, "system/org.game/counter-writer");
    assert_eq!(kernel.world().resource_i64("org.game/counter"), Some(1));
}
