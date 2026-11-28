//! Regression tests for Kernel-owned System failure provenance (Kernel Contract §11).

use kernel_core::{
    Command, Kernel, KernelError, StepInput, System, SystemError, Tick, World, WorldId,
};

struct MisreportingSystem;

impl System for MisreportingSystem {
    fn id(&self) -> &str {
        "org.game/actual"
    }

    fn execute(&self, _world: &World, _tick: Tick) -> Result<Vec<Command>, SystemError> {
        Err(
            SystemError::new(
                "org.game/spoofed",
                Tick(999),
                "test.system_failure",
                "synthetic failure",
            )
            .with_source_digest("source-digest-v1"),
        )
    }
}

#[test]
fn system_failure_diagnostics_use_frozen_registration_identity_and_current_tick() {
    let mut kernel = Kernel::new(World::new(WorldId(7))).unwrap();
    kernel
        .register_system(Box::new(MisreportingSystem))
        .unwrap();
    let world_before = kernel.snapshot().unwrap();

    let error = match kernel.step(StepInput::default()) {
        Err(KernelError::SystemFailed(error)) => error,
        other => panic!("expected a SystemFailed error, got {other:?}"),
    };

    assert_eq!(error.format_version, 1);
    assert_eq!(error.system, "org.game/actual");
    assert_eq!(error.tick, Tick(0));
    assert_eq!(error.code, "test.system_failure");
    assert_eq!(error.source_digest.as_deref(), Some("source-digest-v1"));
    assert_eq!(error.message, "synthetic failure");

    // A System failure must not advance or partially commit the authoritative World.
    assert_eq!(kernel.world().tick(), Tick(0));
    assert_eq!(kernel.snapshot().unwrap(), world_before);

    // The fault is sticky: a failed Kernel cannot silently continue advancing.
    assert!(matches!(
        kernel.step(StepInput::default()),
        Err(KernelError::Faulted(reason))
            if reason.contains("org.game/actual")
                && reason.contains("test.system_failure")
                && reason.contains("synthetic failure")
    ));
}
