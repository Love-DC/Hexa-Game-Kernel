//! Contract tests for `Kernel::registered_system_ids` (C-1).

use kernel_core::{Command, Kernel, KernelError, System, SystemError, Tick, World, WorldId};

struct StaticSystem(&'static str);

impl System for StaticSystem {
    fn id(&self) -> &str {
        self.0
    }

    fn execute(&self, _world: &World, _tick: Tick) -> Result<Vec<Command>, SystemError> {
        Ok(Vec::new())
    }
}

fn kernel() -> Kernel {
    Kernel::new(World::new(WorldId(1))).unwrap()
}

#[test]
fn empty_kernel_yields_empty_manifest() {
    let kernel = kernel();
    let mut ids = kernel.registered_system_ids();
    assert_eq!(ids.len(), 0);
    assert_eq!(ids.next(), None);
}

#[test]
fn ids_follow_lexicographic_execution_order() {
    let mut kernel = kernel();
    for id in ["org.game/zeta", "org.game/alpha", "org.game/mid"] {
        kernel.register_system(Box::new(StaticSystem(id))).unwrap();
    }

    let ids: Vec<&str> = kernel.registered_system_ids().collect();
    assert_eq!(ids, ["org.game/alpha", "org.game/mid", "org.game/zeta"]);
}

#[test]
fn manifest_reflects_only_successful_registrations() {
    let mut kernel = kernel();
    kernel
        .register_system(Box::new(StaticSystem("org.game/beta")))
        .unwrap();
    kernel
        .register_system(Box::new(StaticSystem("org.game/alpha")))
        .unwrap();

    // Duplicate registration is rejected and must not change the manifest.
    let duplicate = kernel.register_system(Box::new(StaticSystem("org.game/alpha")));
    assert!(matches!(duplicate, Err(KernelError::DuplicateSystemId(_))));
    // Invalid IDs are rejected and must not change the manifest.
    let invalid = kernel.register_system(Box::new(StaticSystem("Org.Game/INVALID")));
    assert!(matches!(invalid, Err(KernelError::InvalidSystemId(_))));

    let ids: Vec<&str> = kernel.registered_system_ids().collect();
    assert_eq!(ids, ["org.game/alpha", "org.game/beta"]);
}

#[test]
fn iterator_reports_exact_len_while_consuming() {
    let mut kernel = kernel();
    for id in ["org.game/a", "org.game/b", "org.game/c"] {
        kernel.register_system(Box::new(StaticSystem(id))).unwrap();
    }

    let mut ids = kernel.registered_system_ids();
    assert_eq!(ids.len(), 3);
    assert_eq!(ids.next(), Some("org.game/a"));
    assert_eq!(ids.len(), 2);
    assert_eq!(ids.next(), Some("org.game/b"));
    assert_eq!(ids.len(), 1);
    assert_eq!(ids.next(), Some("org.game/c"));
    assert_eq!(ids.len(), 0);
    assert_eq!(ids.next(), None);
}

#[test]
fn reads_are_repeatable_and_side_effect_free() {
    let mut kernel = kernel();
    kernel
        .register_system(Box::new(StaticSystem("org.game/alpha")))
        .unwrap();

    let first: Vec<&str> = kernel.registered_system_ids().collect();
    let second: Vec<&str> = kernel.registered_system_ids().collect();
    assert_eq!(first, second);
    // The snapshot the save flow would capture is unaffected by the read.
    let before = kernel.snapshot().unwrap();
    let _ = kernel.registered_system_ids().count();
    assert_eq!(kernel.snapshot().unwrap(), before);
}
