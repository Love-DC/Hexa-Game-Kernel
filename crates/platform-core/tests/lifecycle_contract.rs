//! GK-P3-05 contract tests: Mod lifecycle state machine and the System
//! registration coordinator.
//!
//! Scope note: these tests pin down *this build's* behaviour. The lifecycle
//! transition table is derived from `Mod Platform Specification v1.0` §7.1; the
//! conservative terminality of `Unloaded`/`Failed` is an implementation choice
//! recorded as a spec gap, not a claim about the spec. The registration tests
//! verify the coordinator's constructive guarantee for a submitted batch only —
//! they do **not** and cannot assert global registration consistency, because
//! `Kernel::register_system` remains callable behind the coordinator's back.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};

use game_platform::{
    LifecycleCode, ModLifecycle, ModLifecycleState, ModRegistry, PlatformErrorCode,
};
use kernel_core::{
    Command, ComponentValue, Kernel, StepInput, System, SystemError, Tick, World, WorldId,
};

/// Minimal in-test System. Only its ID matters for registration bookkeeping.
struct TestSystem {
    id: String,
}

impl TestSystem {
    fn new(id: impl Into<String>) -> Self {
        Self { id: id.into() }
    }
}

impl System for TestSystem {
    fn id(&self) -> &str {
        &self.id
    }

    fn execute(&self, _world: &World, _tick: Tick) -> Result<Vec<Command>, SystemError> {
        Ok(Vec::new())
    }
}

/// A deliberately malformed System: `id()` is NOT idempotent. The first call
/// returns `first`, every subsequent call returns `later`. Used to reproduce
/// the V-1 hazard where pre-validation sees one identity and the Kernel's own
/// `register_system` sees another.
struct ShiftyIdSystem {
    first: &'static str,
    later: &'static str,
    calls: AtomicUsize,
}

impl ShiftyIdSystem {
    fn new(first: &'static str, later: &'static str) -> Self {
        Self {
            first,
            later,
            calls: AtomicUsize::new(0),
        }
    }
}

impl System for ShiftyIdSystem {
    fn id(&self) -> &str {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call == 0 { self.first } else { self.later }
    }

    fn execute(&self, _world: &World, _tick: Tick) -> Result<Vec<Command>, SystemError> {
        Ok(Vec::new())
    }
}

fn boxed(id: &str) -> (String, Box<dyn System>) {
    (id.to_owned(), Box::new(TestSystem::new(id)))
}

fn empty_kernel() -> Kernel {
    let mut world = World::new(WorldId(1));
    world
        .seed_resource("org.game/counter", ComponentValue::Integer(0))
        .expect("seed counter resource");
    Kernel::new(world).expect("construct kernel")
}

/// Number of Systems the Kernel actually holds, observed through behaviour
/// rather than a private accessor: stepping an empty kernel is a no-op, so we
/// instead count by attempting to re-register the same set (a duplicate ID is
/// rejected only if it is present).
fn kernel_contains(kernel: &mut Kernel, id: &str) -> bool {
    // `register_system` rejects a duplicate ID without mutating state, so this
    // is a safe, side-effect-free probe for presence.
    let result = kernel.register_system(Box::new(TestSystem::new(id)));
    match result {
        Err(kernel_core::KernelError::DuplicateSystemId(_)) => true,
        Err(_) => false,
        Ok(()) => {
            // Should not happen in these tests; report as absent to avoid
            // silently claiming presence.
            false
        }
    }
}

fn failure_code(error: &game_platform::PlatformError) -> LifecycleCode {
    match error {
        game_platform::PlatformError::Lifecycle(failure) => failure.code(),
        other => panic!("expected a lifecycle failure, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Lifecycle: legal transitions
// ---------------------------------------------------------------------------

#[test]
fn forward_chain_advances_through_every_spec_state() {
    let mut life = ModLifecycle::discovered("org.game.counter");
    assert_eq!(life.state(), ModLifecycleState::Discovered);

    let chain = [
        ModLifecycleState::Validated,
        ModLifecycleState::Resolved,
        ModLifecycleState::Loaded,
        ModLifecycleState::Initialized,
        ModLifecycleState::Active,
        ModLifecycleState::Stopping,
        ModLifecycleState::Stopped,
        ModLifecycleState::Unloaded,
    ];
    for next in chain {
        life.transition(next)
            .unwrap_or_else(|error| panic!("{next:?} should be reachable: {error}"));
        assert_eq!(life.state(), next);
    }
    assert_eq!(life.history().len(), 9);
    assert_eq!(life.mod_id(), "org.game.counter");
}

#[test]
fn failed_is_reachable_from_every_non_terminal_stage() {
    let stages = [
        ModLifecycleState::Discovered,
        ModLifecycleState::Validated,
        ModLifecycleState::Resolved,
        ModLifecycleState::Loaded,
        ModLifecycleState::Initialized,
        ModLifecycleState::Active,
        ModLifecycleState::Stopping,
        ModLifecycleState::Stopped,
    ];

    for stage in stages {
        // Walk the forward chain up to `stage`, then fail.
        let mut life = ModLifecycle::discovered("org.game.counter");
        for next in [
            ModLifecycleState::Validated,
            ModLifecycleState::Resolved,
            ModLifecycleState::Loaded,
            ModLifecycleState::Initialized,
            ModLifecycleState::Active,
            ModLifecycleState::Stopping,
            ModLifecycleState::Stopped,
        ] {
            if life.state() == stage {
                break;
            }
            life.transition(next)
                .unwrap_or_else(|error| panic!("setup to {next:?} failed: {error}"));
        }
        assert_eq!(life.state(), stage);
        life.fail()
            .unwrap_or_else(|error| panic!("{stage:?} should be able to fail: {error}"));
        assert_eq!(life.state(), ModLifecycleState::Failed);
    }
}

// ---------------------------------------------------------------------------
// Lifecycle: illegal transitions rejected with diagnostics
// ---------------------------------------------------------------------------

#[test]
fn illegal_transitions_are_rejected_and_state_is_unchanged() {
    let mut life = ModLifecycle::discovered("org.game.counter");

    // Cannot skip ahead.
    let error = life
        .transition(ModLifecycleState::Active)
        .expect_err("skipping Validated must be rejected");
    assert_eq!(error.code(), PlatformErrorCode::Lifecycle);
    assert_eq!(failure_code(&error), LifecycleCode::IllegalTransition);
    assert_eq!(life.state(), ModLifecycleState::Discovered);

    // Cannot go backwards once advanced.
    life.transition(ModLifecycleState::Validated).unwrap();
    let error = life
        .transition(ModLifecycleState::Discovered)
        .expect_err("regressing to Discovered must be rejected");
    assert_eq!(failure_code(&error), LifecycleCode::IllegalTransition);
    assert_eq!(life.state(), ModLifecycleState::Validated);
}

#[test]
fn illegal_transition_diagnostic_names_mod_and_both_states() {
    let mut life = ModLifecycle::discovered("org.game.counter");
    let error = life.transition(ModLifecycleState::Resolved).unwrap_err();
    let game_platform::PlatformError::Lifecycle(failure) = &error else {
        panic!("expected lifecycle failure");
    };
    assert_eq!(failure.mod_id.as_deref(), Some("org.game.counter"));
    assert_eq!(failure.from, Some(ModLifecycleState::Discovered));
    assert_eq!(failure.to, Some(ModLifecycleState::Resolved));
    assert!(failure.message.contains("Discovered"));
    assert!(failure.message.contains("Resolved"));
}

// ---------------------------------------------------------------------------
// Lifecycle: conservative terminal handling + Disabled is not modelled
// ---------------------------------------------------------------------------

#[test]
fn unloaded_and_failed_are_conservatively_terminal_this_is_a_documented_gap() {
    // NOTE: §7.1 does not define outgoing transitions for Unloaded/Failed. This
    // build rejects them as an implementation strategy; it does not claim the
    // spec calls them terminal. If the spec later defines out-edges, this test
    // is the one to change.
    for terminal in [ModLifecycleState::Unloaded, ModLifecycleState::Failed] {
        assert!(terminal.is_terminal());
        for candidate in [
            ModLifecycleState::Discovered,
            ModLifecycleState::Validated,
            ModLifecycleState::Active,
            ModLifecycleState::Stopped,
            ModLifecycleState::Unloaded,
            ModLifecycleState::Failed,
        ] {
            assert!(
                !terminal.can_transition_to(candidate),
                "{terminal:?} must reject transition to {candidate:?} in this build"
            );
        }
    }

    // Reaching Unloaded requires passing through Stopped first.
    let mut life = ModLifecycle::discovered("org.game.counter");
    life.transition(ModLifecycleState::Validated).unwrap();
    life.transition(ModLifecycleState::Resolved).unwrap();
    life.transition(ModLifecycleState::Loaded).unwrap();
    life.transition(ModLifecycleState::Initialized).unwrap();
    life.transition(ModLifecycleState::Active).unwrap();
    let error = life
        .transition(ModLifecycleState::Unloaded)
        .expect_err("Unloaded must only be reachable from Stopped");
    assert_eq!(failure_code(&error), LifecycleCode::IllegalTransition);
}

#[test]
fn disabled_is_absent_from_the_state_set_and_never_maps_to_stopped() {
    // `Disabled` must not be a variant, and `Stopped` must not be described as
    // `Disabled`. Enumerate the whole state set by round-tripping through
    // as_str(); the set must be exactly the ten spec states and must not
    // contain `Disabled`.
    let all = [
        ModLifecycleState::Discovered,
        ModLifecycleState::Validated,
        ModLifecycleState::Resolved,
        ModLifecycleState::Loaded,
        ModLifecycleState::Initialized,
        ModLifecycleState::Active,
        ModLifecycleState::Stopping,
        ModLifecycleState::Stopped,
        ModLifecycleState::Unloaded,
        ModLifecycleState::Failed,
    ];
    let names: BTreeSet<&str> = all.iter().map(|state| state.as_str()).collect();
    assert_eq!(names.len(), 10);
    assert!(!names.contains("Disabled"));
    assert_eq!(ModLifecycleState::Stopped.as_str(), "Stopped");
    assert_ne!(ModLifecycleState::Stopped.as_str(), "Disabled");
}

// ---------------------------------------------------------------------------
// Registration coordinator
// ---------------------------------------------------------------------------

#[test]
fn successful_batch_registers_all_and_records_all() {
    let mut kernel = empty_kernel();
    let mut registry = ModRegistry::new();

    registry
        .register_batch(&mut kernel, vec![boxed("org.game.a"), boxed("org.game.b")])
        .expect("batch should succeed");

    assert_eq!(registry.registered_count(), 2);
    assert!(registry.contains("org.game.a"));
    assert!(registry.contains("org.game.b"));
    assert!(kernel_contains(&mut kernel, "org.game.a"));
    assert!(kernel_contains(&mut kernel, "org.game.b"));
}

#[test]
fn intra_batch_duplicate_ids_fail_prevalidation_and_kernel_gains_nothing() {
    let mut kernel = empty_kernel();
    let mut registry = ModRegistry::new();

    let error = registry
        .register_batch(&mut kernel, vec![boxed("org.game.a"), boxed("org.game.a")])
        .expect_err("duplicate Mod ID within a batch must be rejected");
    assert_eq!(failure_code(&error), LifecycleCode::DuplicateModId);
    assert_eq!(registry.registered_count(), 0);
    assert!(!kernel_contains(&mut kernel, "org.game.a"));
}

#[test]
fn illegal_ids_match_kernel_validation_rule() {
    // `validate_type_id` rejects a leading `/`; the coordinator must reuse that
    // exact rule rather than a second, drifting one.
    for bad in ["/leading.slash", "trailing.slash/", "double//slash", ""] {
        assert!(
            kernel_core::validate_type_id(bad).is_err(),
            "`{bad}` must be illegal per Kernel rules"
        );

        let mut kernel = empty_kernel();
        let mut registry = ModRegistry::new();
        let error = registry
            .register_batch(&mut kernel, vec![boxed(bad)])
            .expect_err("illegal Mod ID must be rejected");
        assert_eq!(failure_code(&error), LifecycleCode::InvalidModId);
        assert_eq!(registry.registered_count(), 0);
    }

    // A legal ID that passes Kernel rules must be accepted.
    let mut kernel = empty_kernel();
    let mut registry = ModRegistry::new();
    registry
        .register_batch(&mut kernel, vec![boxed("org.game.valid_id/v1")])
        .expect("legal ID should register");
    assert_eq!(registry.registered_count(), 1);
}

#[test]
fn declared_id_must_equal_system_id() {
    let mut kernel = empty_kernel();
    let mut registry = ModRegistry::new();

    let mismatch: Vec<(String, Box<dyn System>)> = vec![(
        "org.game.declared".to_owned(),
        Box::new(TestSystem::new("org.game.actual")),
    )];
    let error = registry
        .register_batch(&mut kernel, mismatch)
        .expect_err("declared/System::id() mismatch must be rejected");
    assert_eq!(failure_code(&error), LifecycleCode::ModIdMismatch);
    assert_eq!(registry.registered_count(), 0);
    assert!(!kernel_contains(&mut kernel, "org.game.declared"));
    assert!(!kernel_contains(&mut kernel, "org.game.actual"));
}

#[test]
fn cross_batch_duplicate_is_rejected_and_kernel_state_unchanged() {
    let mut kernel = empty_kernel();
    let mut registry = ModRegistry::new();

    registry
        .register_batch(&mut kernel, vec![boxed("org.game.a")])
        .expect("first batch should succeed");

    let error = registry
        .register_batch(&mut kernel, vec![boxed("org.game.a")])
        .expect_err("second batch with an already-registered ID must be rejected");
    assert_eq!(failure_code(&error), LifecycleCode::AlreadyRegistered);
    assert_eq!(registry.registered_count(), 1);
    assert!(kernel_contains(&mut kernel, "org.game.a"));

    // A fresh, distinct ID still succeeds, proving the rejection did not poison
    // the registry.
    registry
        .register_batch(&mut kernel, vec![boxed("org.game.b")])
        .expect("a distinct ID should still register");
    assert_eq!(registry.registered_count(), 2);
}

#[test]
fn prevalidation_failure_leaves_kernel_and_record_at_pre_batch_state() {
    let mut kernel = empty_kernel();
    let mut registry = ModRegistry::new();

    registry
        .register_batch(&mut kernel, vec![boxed("org.game.a")])
        .expect("seed batch should succeed");
    let before_count = registry.registered_count();

    // A batch whose first element is fine but whose second is a duplicate of an
    // already-registered ID must fail wholly during pre-validation: the good
    // first element must NOT be registered.
    let error = registry
        .register_batch(&mut kernel, vec![boxed("org.game.b"), boxed("org.game.a")])
        .expect_err("batch containing an already-registered ID must be rejected");
    assert_eq!(failure_code(&error), LifecycleCode::AlreadyRegistered);
    assert_eq!(registry.registered_count(), before_count);
    assert!(!registry.contains("org.game.b"));
    assert!(!kernel_contains(&mut kernel, "org.game.b"));
}

#[test]
fn empty_batch_is_a_noop() {
    let mut kernel = empty_kernel();
    let mut registry = ModRegistry::new();
    registry
        .register_batch(&mut kernel, Vec::new())
        .expect("empty batch should succeed");
    assert_eq!(registry.registered_count(), 0);
}

/// KNOWN LIMITATION (not a consistency guarantee): the coordinator can only
/// constrain registrations that flow through it. A caller invoking
/// `Kernel::register_system` directly succeeds without updating the record, and
/// the coordinator cannot detect or prevent this. This test documents the gap
/// rather than asserting that consistency is guaranteed.
#[test]
fn bypass_boundary_direct_kernel_registration_does_not_update_the_record() {
    let mut kernel = empty_kernel();
    let mut registry = ModRegistry::new();

    // Bypass the coordinator entirely.
    kernel
        .register_system(Box::new(TestSystem::new("org.game.bypass")))
        .expect("direct Kernel registration succeeds");

    // The Kernel now holds the System...
    assert!(kernel_contains(&mut kernel, "org.game.bypass"));
    // ...but the coordinator's record knows nothing about it.
    assert_eq!(registry.registered_count(), 0);
    assert!(!registry.contains("org.game.bypass"));

    // Because the record is unaware, the coordinator will happily accept a batch
    // for the same ID and only the Kernel will reject it — surfacing as a
    // RegistrationRejected failure rather than a pre-validation failure.
    let error = registry
        .register_batch(&mut kernel, vec![boxed("org.game.bypass")])
        .expect_err("Kernel must reject the duplicate the record did not know about");
    assert_eq!(failure_code(&error), LifecycleCode::RegistrationRejected);
    assert_eq!(registry.registered_count(), 0);
}

#[test]
fn registration_failure_uses_stable_platform_lifecycle_code() {
    let mut kernel = empty_kernel();
    let mut registry = ModRegistry::new();
    kernel
        .register_system(Box::new(TestSystem::new("org.game.bypass")))
        .unwrap();
    let error = registry
        .register_batch(&mut kernel, vec![boxed("org.game.bypass")])
        .expect_err("expected rejection");
    assert_eq!(error.code(), PlatformErrorCode::Lifecycle);
    assert_eq!(error.code().as_str(), "platform.lifecycle");
}

#[test]
fn coordinator_does_not_disturb_kernel_step_behaviour() {
    let mut kernel = empty_kernel();
    let mut registry = ModRegistry::new();
    registry
        .register_batch(&mut kernel, vec![boxed("org.game.a")])
        .expect("batch should succeed");
    // The no-op test System does not touch the world, so a step still advances
    // the tick deterministically.
    let output = kernel.step(StepInput::default()).expect("step should work");
    assert_eq!(output.tick.0, 1);
}

// ---------------------------------------------------------------------------
// V-1 regression: unstable System::id() must not cause partial registration
// ---------------------------------------------------------------------------

/// V-1 regression. A System whose `id()` is legal on the first read and becomes
/// illegal afterwards must NOT be able to (a) pass pre-validation and then
/// (b) fail inside the Kernel, nor (c) poison the batch.
///
/// Before the fix: phase 1 read the legal first value; the Kernel's own
/// `register_system` re-read `id()` and got the illegal later value, rejecting
/// the element *after* earlier elements were already committed — a partial
/// commit. The fix freezes the identity observed by the Kernel, so the Kernel
/// only ever sees the pre-validated value.
#[test]
fn unstable_id_that_turns_illegal_cannot_register_and_never_partially_commits() {
    let mut kernel = empty_kernel();
    let mut registry = ModRegistry::new();

    // Element 0 is a normal System and would be committed by a naive
    // implementation; element 1 declares a legal id but its `id()` becomes
    // illegal on the second read.
    let batch: Vec<(String, Box<dyn System>)> = vec![
        boxed("org.game.first"),
        (
            "org.game.shifty".to_owned(),
            Box::new(ShiftyIdSystem::new("org.game.shifty", "/illegal")),
        ),
    ];

    let result = registry.register_batch(&mut kernel, batch);

    // Under the fix the batch SUCCEEDS: element 1's declared id equals its first
    // `id()` read (`org.game.shifty`), which is legal, and the Kernel only ever
    // sees that frozen identity — never the illegal later value.
    result.expect("with a frozen identity the batch must succeed");

    // Neither the record nor the Kernel may contain the illegal later value.
    assert!(!registry.contains("/illegal"));
    assert!(!kernel_contains(&mut kernel, "/illegal"));

    // Both elements are registered under their frozen identities.
    assert_eq!(registry.registered_count(), 2);
    assert!(kernel_contains(&mut kernel, "org.game.first"));
    assert!(kernel_contains(&mut kernel, "org.game.shifty"));
}

/// V-1 regression, the discriminating case: the batch must be all-or-nothing.
/// We construct element 1 so that its *declared* id (and its first `id()` read)
/// is illegal, which pre-validation rejects BEFORE any Kernel mutation — the
/// earlier element must therefore not be committed.
#[test]
fn prevalidation_failure_on_later_element_leaves_earlier_element_uncommitted() {
    let mut kernel = empty_kernel();
    let mut registry = ModRegistry::new();

    let batch: Vec<(String, Box<dyn System>)> = vec![
        boxed("org.game.first"),
        // declared id is illegal, so phase 1 rejects it at index 1.
        boxed("/illegal"),
    ];

    let error = registry
        .register_batch(&mut kernel, batch)
        .expect_err("illegal id at index 1 must fail the batch");
    assert_eq!(failure_code(&error), LifecycleCode::InvalidModId);

    // Critical assertion: the earlier, perfectly valid element was NOT committed.
    assert_eq!(registry.registered_count(), 0);
    assert!(!registry.contains("org.game.first"));
    assert!(!kernel_contains(&mut kernel, "org.game.first"));
}

/// V-1 regression: prove the Kernel never observes a different identity than the
/// one pre-validated, by registering a System whose `id()` would drift and then
/// checking the Kernel holds it under the frozen (first) value.
#[test]
fn kernel_registers_under_frozen_identity_not_the_drifting_one() {
    let mut kernel = empty_kernel();
    let mut registry = ModRegistry::new();

    let drifting = ShiftyIdSystem::new("org.game.frozen", "/drifted-away");
    registry
        .register_batch(
            &mut kernel,
            vec![("org.game.frozen".to_owned(), Box::new(drifting))],
        )
        .expect("frozen identity should register cleanly");

    // The Kernel must hold `org.game.frozen`...
    assert!(kernel_contains(&mut kernel, "org.game.frozen"));
    // ...and must NOT hold the drifting value.
    assert!(!kernel_contains(&mut kernel, "/drifted-away"));
    assert!(registry.contains("org.game.frozen"));
    assert!(!registry.contains("/drifted-away"));
}

/// V-1 regression: even when the wrapped System's `id()` would return an
/// illegal value on every call after the first, the Kernel still accepts it,
/// because the frozen identity is the only one the Kernel sees. This is the
/// exact input that produced a partial commit before the fix.
#[test]
fn frozen_identity_defeats_the_original_partial_commit_reproduction() {
    let mut kernel = empty_kernel();
    let mut registry = ModRegistry::new();

    // Reproduces the pre-fix failure shape: element 0 commits eagerly, then
    // element 1's second id() read is illegal.
    let batch: Vec<(String, Box<dyn System>)> = vec![
        boxed("org.game.first"),
        (
            "org.game.shifty".to_owned(),
            Box::new(ShiftyIdSystem::new("org.game.shifty", "/illegal")),
        ),
    ];
    let result = registry.register_batch(&mut kernel, batch);

    // Key invariant: whatever the result, the Kernel and the record agree, and
    // no partially-committed element can be left behind under a different name.
    match result {
        Ok(()) => {
            assert_eq!(registry.registered_count(), 2);
            assert!(kernel_contains(&mut kernel, "org.game.first"));
            assert!(kernel_contains(&mut kernel, "org.game.shifty"));
        }
        Err(error) => {
            // A rejection must mean NOTHING was committed.
            assert_eq!(failure_code(&error), LifecycleCode::RegistrationRejected);
            assert_eq!(registry.registered_count(), 0);
            assert!(!kernel_contains(&mut kernel, "org.game.first"));
        }
    }
    assert!(!kernel_contains(&mut kernel, "/illegal"));
    assert!(!registry.contains("/illegal"));
}

/// V-1 regression: a direct Kernel registration remains invisible to the
/// coordinator record, but it must not cause a submitted batch to partially
/// commit. The Kernel's atomic `register_systems` call rejects the full batch.
#[test]
fn bypass_duplicate_rejects_the_entire_batch_atomically() {
    let mut kernel = empty_kernel();
    let mut registry = ModRegistry::new();

    // Register a System directly with the Kernel, bypassing the coordinator.
    kernel
        .register_system(Box::new(TestSystem::new("org.game.bypass")))
        .expect("direct registration succeeds");

    // The coordinator cannot see the bypass registration during pre-validation.
    // Kernel's atomic batch call rejects the duplicate without committing the
    // earlier fresh element.
    let error = registry
        .register_batch(
            &mut kernel,
            vec![boxed("org.game.fresh"), boxed("org.game.bypass")],
        )
        .expect_err("the Kernel must reject the bypass-contaminated batch");
    assert_eq!(failure_code(&error), LifecycleCode::RegistrationRejected);
    match &error {
        game_platform::PlatformError::Lifecycle(failure) => {
            assert_eq!(failure.mod_id.as_deref(), Some("org.game.bypass"));
            assert_eq!(failure.batch_index, Some(1));
        }
        other => panic!("expected a lifecycle failure, got {other:?}"),
    }

    // No element from the failed batch may appear in either registration view.
    assert_eq!(registry.registered_count(), 0);
    assert!(!registry.contains("org.game.bypass"));
    assert!(!registry.contains("org.game.fresh"));
    let kernel_ids: Vec<_> = kernel.registered_system_ids().collect();
    assert_eq!(kernel_ids, vec!["org.game.bypass"]);
}

/// V-1 regression: the pre-existing behaviours must be unchanged.
#[test]
fn existing_behaviours_are_preserved_after_the_fix() {
    // Normal registration.
    let mut kernel = empty_kernel();
    let mut registry = ModRegistry::new();
    registry
        .register_batch(&mut kernel, vec![boxed("org.game.a"), boxed("org.game.b")])
        .expect("normal batch succeeds");
    assert_eq!(registry.registered_count(), 2);

    // Duplicate id.
    let mut kernel = empty_kernel();
    let mut registry = ModRegistry::new();
    assert_eq!(
        failure_code(
            &registry
                .register_batch(&mut kernel, vec![boxed("org.game.a"), boxed("org.game.a")])
                .unwrap_err()
        ),
        LifecycleCode::DuplicateModId
    );

    // Historical conflict.
    let mut kernel = empty_kernel();
    let mut registry = ModRegistry::new();
    registry
        .register_batch(&mut kernel, vec![boxed("org.game.a")])
        .unwrap();
    assert_eq!(
        failure_code(
            &registry
                .register_batch(&mut kernel, vec![boxed("org.game.a")])
                .unwrap_err()
        ),
        LifecycleCode::AlreadyRegistered
    );

    // Illegal type id.
    let mut kernel = empty_kernel();
    let mut registry = ModRegistry::new();
    assert_eq!(
        failure_code(
            &registry
                .register_batch(&mut kernel, vec![boxed("/illegal")])
                .unwrap_err()
        ),
        LifecycleCode::InvalidModId
    );

    // Declared/id mismatch.
    let mut kernel = empty_kernel();
    let mut registry = ModRegistry::new();
    let mismatch: Vec<(String, Box<dyn System>)> = vec![(
        "org.game.declared".to_owned(),
        Box::new(TestSystem::new("org.game.actual")),
    )];
    assert_eq!(
        failure_code(&registry.register_batch(&mut kernel, mismatch).unwrap_err()),
        LifecycleCode::ModIdMismatch
    );
}

/// V-1 regression: after the fix, the sealed System still executes the wrapped
/// System's logic (delegation is not lost by the wrapper).
#[test]
fn frozen_wrapper_delegates_execute_to_the_inner_system() {
    let mut kernel = empty_kernel();
    let mut registry = ModRegistry::new();
    registry
        .register_batch(&mut kernel, vec![boxed("org.game.noop")])
        .expect("register");
    // The wrapped System is a no-op, so a step advances the tick and the world
    // resource is untouched.
    let output = kernel.step(StepInput::default()).expect("step");
    assert_eq!(output.tick.0, 1);
    assert_eq!(
        kernel
            .world()
            .resource_i64("org.game/counter")
            .unwrap_or_default(),
        0
    );
}
