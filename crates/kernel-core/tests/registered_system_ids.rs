//! Contract tests for `Kernel::registered_system_ids` (C-1).

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use kernel_core::{
    Command, ComponentValue, Kernel, KernelError, StepInput, System, SystemError, Tick, World,
    WorldId,
};

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
fn duplicate_inside_batch_leaves_registration_set_unchanged() {
    let mut kernel = kernel();
    let result = kernel.register_systems(vec![
        Box::new(StaticSystem("org.game/alpha")),
        Box::new(StaticSystem("org.game/alpha")),
    ]);

    assert!(matches!(result, Err(KernelError::DuplicateSystemId(id)) if id == "org.game/alpha"));
    assert_eq!(kernel.registered_system_ids().count(), 0);
}

#[test]
fn conflict_with_existing_system_leaves_batch_unregistered() {
    let mut kernel = kernel();
    kernel
        .register_system(Box::new(StaticSystem("org.game/alpha")))
        .unwrap();

    let result = kernel.register_systems(vec![
        Box::new(StaticSystem("org.game/beta")),
        Box::new(StaticSystem("org.game/alpha")),
    ]);

    assert!(matches!(result, Err(KernelError::DuplicateSystemId(id)) if id == "org.game/alpha"));
    assert_eq!(
        kernel.registered_system_ids().collect::<Vec<_>>(),
        ["org.game/alpha"]
    );
}

#[test]
fn invalid_late_id_leaves_batch_unregistered() {
    let mut kernel = kernel();
    let result = kernel.register_systems(vec![
        Box::new(StaticSystem("org.game/alpha")),
        Box::new(StaticSystem("Org.Game/INVALID")),
    ]);

    assert!(matches!(result, Err(KernelError::InvalidSystemId(id)) if id == "Org.Game/INVALID"));
    assert_eq!(kernel.registered_system_ids().count(), 0);
}

#[test]
fn successful_batch_is_sorted_by_frozen_id() {
    let mut kernel = kernel();
    kernel
        .register_systems(vec![
            Box::new(StaticSystem("org.game/zeta")),
            Box::new(StaticSystem("org.game/alpha")),
            Box::new(StaticSystem("org.game/mid")),
        ])
        .unwrap();

    assert_eq!(
        kernel.registered_system_ids().collect::<Vec<_>>(),
        ["org.game/alpha", "org.game/mid", "org.game/zeta"]
    );
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

struct DriftingSystem {
    initial_id: &'static str,
    drifted_id: &'static str,
    value: i64,
    drift: Arc<AtomicBool>,
    id_calls: Arc<AtomicUsize>,
}

impl System for DriftingSystem {
    fn id(&self) -> &str {
        self.id_calls.fetch_add(1, Ordering::SeqCst);
        if self.drift.load(Ordering::SeqCst) {
            self.drifted_id
        } else {
            self.initial_id
        }
    }

    fn execute(&self, _world: &World, _tick: Tick) -> Result<Vec<Command>, SystemError> {
        self.drift.store(true, Ordering::SeqCst);
        Ok(vec![Command::SetResource {
            key: "org.game/result".to_owned(),
            value: ComponentValue::Integer(self.value),
        }])
    }
}

#[test]
fn registration_freezes_identity_for_listing_order_and_command_source() {
    let mut kernel = kernel();
    let alpha_drift = Arc::new(AtomicBool::new(false));
    let alpha_calls = Arc::new(AtomicUsize::new(0));
    let zeta_drift = Arc::new(AtomicBool::new(false));
    let zeta_calls = Arc::new(AtomicUsize::new(0));

    // Register in reverse order. After execution the live IDs drift in the
    // opposite direction, which must not change identity or execution order.
    kernel
        .register_system(Box::new(DriftingSystem {
            initial_id: "org.game/zeta",
            drifted_id: "org.game/aaa",
            value: 2,
            drift: Arc::clone(&zeta_drift),
            id_calls: Arc::clone(&zeta_calls),
        }))
        .unwrap();
    kernel
        .register_system(Box::new(DriftingSystem {
            initial_id: "org.game/alpha",
            drifted_id: "org.game/zulu",
            value: 1,
            drift: Arc::clone(&alpha_drift),
            id_calls: Arc::clone(&alpha_calls),
        }))
        .unwrap();

    assert_eq!(
        kernel.registered_system_ids().collect::<Vec<_>>(),
        ["org.game/alpha", "org.game/zeta"]
    );
    let output = kernel.step(StepInput::default()).unwrap();

    assert_eq!(
        output
            .committed_commands
            .iter()
            .map(|id| id.source.as_str())
            .collect::<Vec<_>>(),
        ["system/org.game/alpha", "system/org.game/zeta"]
    );
    assert_eq!(
        kernel.world().resource("org.game/result"),
        Some(&ComponentValue::Integer(2))
    );
    assert_eq!(alpha_calls.load(Ordering::SeqCst), 1);
    assert_eq!(zeta_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        kernel.registered_system_ids().collect::<Vec<_>>(),
        ["org.game/alpha", "org.game/zeta"]
    );
}

struct InvalidCommandSystem {
    drift: Arc<AtomicBool>,
    id_calls: Arc<AtomicUsize>,
}

impl System for InvalidCommandSystem {
    fn id(&self) -> &str {
        self.id_calls.fetch_add(1, Ordering::SeqCst);
        if self.drift.load(Ordering::SeqCst) {
            "org.game/changed"
        } else {
            "org.game/registered"
        }
    }

    fn execute(&self, _world: &World, _tick: Tick) -> Result<Vec<Command>, SystemError> {
        self.drift.store(true, Ordering::SeqCst);
        Ok(vec![Command::SetResource {
            key: "INVALID".to_owned(),
            value: ComponentValue::Integer(1),
        }])
    }
}

#[test]
fn invalid_system_command_error_uses_frozen_identity() {
    let mut kernel = kernel();
    let id_calls = Arc::new(AtomicUsize::new(0));
    kernel
        .register_system(Box::new(InvalidCommandSystem {
            drift: Arc::new(AtomicBool::new(false)),
            id_calls: Arc::clone(&id_calls),
        }))
        .unwrap();

    let result = kernel.step(StepInput::default());
    assert!(matches!(
        result,
        Err(KernelError::InvalidSystemCommand { ref system, .. })
            if system == "org.game/registered"
    ));
    assert_eq!(id_calls.load(Ordering::SeqCst), 1);
}

/// Records the order in which the Kernel invokes each System.
///
/// [`StaticSystem`] returns an empty command set, so it cannot show *when* the
/// Kernel called a System. This fixture pushes its ID into a shared log on every
/// invocation, letting the batch/individual equivalence claim be asserted
/// directly instead of inferred from the ID manifest alone.
struct RecordingSystem {
    id: &'static str,
    log: Arc<Mutex<Vec<String>>>,
}

impl System for RecordingSystem {
    fn id(&self) -> &str {
        self.id
    }

    fn execute(&self, _world: &World, _tick: Tick) -> Result<Vec<Command>, SystemError> {
        self.log.lock().unwrap().push(self.id.to_owned());
        Ok(Vec::new())
    }
}

#[test]
fn batch_and_individual_registration_yield_identical_manifest_and_order() {
    // Batch path: supplied in reverse lexicographic order.
    let mut batched = kernel();
    let batch_log = Arc::new(Mutex::new(Vec::new()));
    batched
        .register_systems(vec![
            Box::new(RecordingSystem {
                id: "org.game/zeta",
                log: Arc::clone(&batch_log),
            }),
            Box::new(RecordingSystem {
                id: "org.game/alpha",
                log: Arc::clone(&batch_log),
            }),
            Box::new(RecordingSystem {
                id: "org.game/mid",
                log: Arc::clone(&batch_log),
            }),
        ])
        .unwrap();

    // Individual path: the same set, in yet another insertion order.
    let mut individual = kernel();
    let individual_log = Arc::new(Mutex::new(Vec::new()));
    for id in ["org.game/mid", "org.game/zeta", "org.game/alpha"] {
        individual
            .register_system(Box::new(RecordingSystem {
                id,
                log: Arc::clone(&individual_log),
            }))
            .unwrap();
    }

    let expected = ["org.game/alpha", "org.game/mid", "org.game/zeta"];

    // The two registration paths must agree with each other...
    assert_eq!(
        batched.registered_system_ids().collect::<Vec<_>>(),
        individual.registered_system_ids().collect::<Vec<_>>()
    );
    // ...and with the documented lexicographic manifest order.
    assert_eq!(
        individual.registered_system_ids().collect::<Vec<_>>(),
        expected
    );

    // Execution order must agree as well, not just the manifest.
    batched.step(StepInput::default()).unwrap();
    individual.step(StepInput::default()).unwrap();
    assert_eq!(*batch_log.lock().unwrap(), *individual_log.lock().unwrap());
    assert_eq!(*individual_log.lock().unwrap(), expected);
}

#[test]
fn empty_batch_is_accepted_and_leaves_the_kernel_untouched() {
    let mut fresh = kernel();
    let fresh_result = fresh.register_systems(Vec::new());
    assert!(fresh_result.is_ok());
    assert_eq!(fresh.registered_system_ids().count(), 0);

    // A populated Kernel must survive an empty batch unchanged.
    let mut populated = kernel();
    populated
        .register_system(Box::new(StaticSystem("org.game/alpha")))
        .unwrap();
    let before = populated.snapshot().unwrap();

    assert!(populated.register_systems(Vec::new()).is_ok());
    assert_eq!(
        populated.registered_system_ids().collect::<Vec<_>>(),
        ["org.game/alpha"]
    );
    assert_eq!(populated.snapshot().unwrap(), before);
}

#[test]
fn mixed_individual_and_batch_registration_keeps_lexicographic_order() {
    let mut mixed = kernel();
    let log = Arc::new(Mutex::new(Vec::new()));

    mixed
        .register_system(Box::new(RecordingSystem {
            id: "org.game/mid",
            log: Arc::clone(&log),
        }))
        .unwrap();
    mixed
        .register_systems(vec![
            Box::new(RecordingSystem {
                id: "org.game/zeta",
                log: Arc::clone(&log),
            }),
            Box::new(RecordingSystem {
                id: "org.game/beta",
                log: Arc::clone(&log),
            }),
        ])
        .unwrap();
    mixed
        .register_system(Box::new(RecordingSystem {
            id: "org.game/alpha",
            log: Arc::clone(&log),
        }))
        .unwrap();

    let expected = [
        "org.game/alpha",
        "org.game/beta",
        "org.game/mid",
        "org.game/zeta",
    ];
    assert_eq!(mixed.registered_system_ids().collect::<Vec<_>>(), expected);

    mixed.step(StepInput::default()).unwrap();
    assert_eq!(*log.lock().unwrap(), expected);
}
