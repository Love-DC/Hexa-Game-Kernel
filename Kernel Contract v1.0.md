# Kernel Contract v1.0

- Document Type: Kernel Module Contract
- Version: 1.0.0
- Status: Design Baseline — P2 kernel subset implemented and verified; GK-KERNEL-01/02 are scoped post-P2 extensions, locally validated on 2026-10-20 (Kernel layer only; platform integration not covered)
- Parent Specification: `Game Kernel — 完整项目规范 v1.0.md`
- Scope: Kernel responsibilities, authoritative state, Tick execution, commands, events, identity, errors, and determinism

> This document refines the Kernel requirements in the parent specification. The accepted P2 subset is identified explicitly and its evidence is recorded in `planning/08-P2-Kernel契约硬化草案.md`. GK-KERNEL-01/02 add narrowly scoped System registration guarantees; implementation and focused tests are recorded in `planning/drafts/`. Local validation was performed on 2026-10-20: `cargo test -p kernel-core` exited 0, with `registered_system_ids` 14/14 passed, `kernel-core` unit tests 14/14 passed, and the canonical v2 golden vector `state_hash_uses_canonical_v2_encoding` passed. This evidence covers the Kernel layer only — it is local Windows evidence, it does not cover the platform integration path, and it does not close the coordinator-consistency question tracked as GAP-23. Other requirements outside those scopes remain design targets, not verified implementation claims.

## 1. Purpose

The Kernel is the smallest runtime responsible for advancing authoritative game-world state under explicit time and execution rules.

The Kernel contains only these core modules:

1. `Time`
2. `Scheduler`
3. `System`
4. `World`
5. `Command`
6. `Event`

The Kernel must not contain game-specific business logic, Mod discovery or loading, network transport, rendering, save-file management, community distribution, or Launcher behavior.

## 2. Authority and boundaries

### 2.1 Authoritative state

- `World` owns the authoritative simulation state.
- Systems may read only the state access permitted by their declared contract.
- Systems must not directly mutate authoritative state.
- All authoritative mutations must pass through the Command Pipeline.
- Events describe facts that have already occurred. Events must not provide a second mutation path.
- Platform services may coordinate saving, networking, Mod lifecycle, compatibility, and diagnostics through public Kernel interfaces; they must not bypass the mutation contract.

### 2.2 Dependency rules

- Kernel modules must not depend on concrete Mods, Game Definitions, Launcher, networking implementations, rendering implementations, or save-file formats.
- Module dependencies must be explicit and checked automatically where practical.
- No module may access another module's private implementation as a substitute for a public contract.

## 3. Core concepts and identifiers

The implementation must define stable types or equivalent abstractions for:

- `Tick`
- `EntityId`
- `ComponentTypeId`
- `ResourceTypeId`
- `SystemId`
- `CommandId`
- `CommandEnvelope`
- `EventRecord`
- `StateHash`
- `KernelError`

Each identifier must define its scope, creation rules, lifetime, serialization requirements where applicable, and collision behavior.

### 3.1 Entity identity

Entity identity must not depend on memory addresses, thread race outcomes, or unnormalized container iteration.

The accepted P2 Entity ID representation is `(WorldId, index, generation)`. Index allocation is monotonic and does not reuse indices; generation is retained in the identity but remains fixed at zero in the current implementation. Any future reuse policy requires a versioned decision and tests.

## 4. Time contract

`Time` defines simulation time and Tick boundaries.

Requirements:

- A Tick has an explicit, stable identity.
- Tick advancement must not be implicitly controlled by wall-clock time, operating-system scheduling, or thread timing.
- The current Tick and any state needed to resume simulation must be exposed through a defined contract.
- Save/restore must preserve the logical simulation time.
- Failed Tick behavior must follow the commit and recovery rules in Section 8.

The accepted P2 Tick representation is `u64`; increment uses checked arithmetic, and overflow returns an error without committing the Tick. External clock-to-simulation policy belongs to the Platform/host and may not change authoritative state except through ordered inputs.

## 5. Scheduler contract

The accepted P2 scheduler executes registered Systems serially in stable System ID order. Under GK-KERNEL-01, the Kernel captures each System ID once during registration and uses the captured ID for duplicate detection, scheduling order, the registered-ID manifest, Kernel-generated command diagnostics, and command attribution. Under GK-KERNEL-02, `register_systems` validates a complete batch before committing any member; a returned validation error leaves the registered set unchanged. `register_system` uses the same single-item path. A dependency graph, phases, and parallel scheduling are future capabilities, not supported by the current Kernel API.

Requirements:

- System registration and removal have explicit lifecycle rules.
- If System dependencies are introduced, they must be validated before execution and cycles rejected before a plan is used.
- Execution order is deterministic and does not depend on incidental map order or thread scheduling.
- A cached plan may be used only while its invalidation conditions remain correct.
- Scheduling errors must identify the affected System and relevant dependency.
- Any parallel execution must preserve the observable simulation semantics defined by this contract.

The first implementation may execute Systems serially. Parallel scheduling is not required for the minimum runnable version.

Parallel read/write conflict rules and scheduling metadata are not applicable to the serial P2 scheduler; they must be specified before parallel execution is implemented.

## 6. System contract

A System performs simulation logic for a defined execution phase.

The full System contract must declare or otherwise make testable:

- Stable identity.
- Registration and execution conditions.
- Required dependencies.
- Permitted World reads.
- Commands it may emit.
- Relevant deterministic requirements.
- Error behavior.

The accepted P2 subset provides stable System identity, serial execution, read-only World access, returned Commands, and failure reporting. On each registration attempt, the Kernel calls `System::id()` at most once; a successful registration freezes the validated ID for the lifetime of that registration. Later changes to the value returned by the System do not change its identity or execution order. The Kernel does not call `id()` again for an already registered System. Kernel-generated command diagnostics and command attribution use the frozen ID. Batch registration validates IDs before changing the registered set and either registers every member or returns an error without registering any member. The Kernel does not yet implement execution conditions, access descriptors, or dependency metadata.
- Tests proving its contract.

During a System's execution, it may read permitted state and produce command requests. It must not directly modify authoritative World state.

A System's result must not depend on unrecorded wall-clock time, thread race order, unspecified container iteration, or untracked external side effects.

## 7. World contract

`World` owns authoritative simulation state, including entities, components, and resources.

The implementation must define:

- Entity creation and destruction lifecycle.
- Component ownership and type identity.
- Resource ownership and type identity.
- Read access and write access rules.
- Validation and consistency invariants.
- Snapshot representation.
- Restore behavior.
- Canonical logical-state representation for hashing and comparison.

World data must not expose mutable internals that allow callers to bypass the Command Pipeline.

The precise storage layout is an implementation choice unless it changes public semantics or determinism.

Snapshot capture is defined only at the quiescent point after a committed Tick; a snapshot never exposes staged mid-Tick state. Mutual exclusion between `step` and `snapshot` on a single Kernel instance follows from Rust borrowing (`&mut self` vs `&self`); synchronizing cross-thread sharing of a Kernel is the caller's responsibility — the Kernel provides no synchronization primitives.

## 8. Command Pipeline and Tick commit semantics

### 8.1 General rules

- Every authoritative state change must pass through the Command Pipeline.
- Systems emit commands; they do not mutate World directly.
- Commands must have explicit validation, application results, and error types.
- Command ordering must be deterministic.
- The pipeline must record enough information to diagnose rejected or failed commands.
- Resource mutations follow the same authoritative mutation policy. P2 implements `Command::SetResource`; new resource value types or mutation semantics require a separate contract decision.

### 8.2 Accepted P2 command semantics

The following behavior is the accepted v1 contract and is implemented by the P2 baseline:

1. External commands target the current Tick and are sorted by `(source, sequence)`.
2. Duplicate command IDs, invalid source IDs, wrong-Tick commands, and non-increasing per-source sequences are rejected individually.
3. Valid commands apply in order to staged state; later commands observe earlier successful commands.
4. An external command rejection does not block later valid commands and does not mutate the rejected command's target.
5. Entity IDs are `(WorldId, index, generation)`; indices increase monotonically and are not reused in the current contract.
6. Systems execute serially by stable System ID and receive a read-only view after external commands.
7. Each System's commands apply in sequence to per-System staged state. A System error or invalid System command discards the whole Tick and faults the Kernel.
8. Command outcomes are returned in `StepOutput`; successful mutations produce ordered Events.

### 8.3 Accepted P2 Tick commit semantics

- The Kernel clones the current World into staged state before applying Tick inputs.
- A successful Tick advances the checked `u64` Tick by one, computes the canonical state hash, atomically replaces the visible World, and returns `StepOutput`.
- An external command rejection is isolated to that command; the Tick may still succeed.
- A System execution error or invalid System command discards all staged changes for that Tick, leaves the visible World and Tick unchanged, and places the Kernel in `Faulted`.
- A faulted Kernel rejects later `step` calls. Recovery constructs a new Kernel from a trusted snapshot and re-registers Systems.
- Events are returned only in a successful `StepOutput`; Event handlers and recursive Event-triggered mutation are not part of this contract.

### 8.4 Tick success and failure

`Kernel::step()` succeeds only after external command processing, every System execution and output application, checked Tick advancement, and state-hash computation complete. Success replaces the visible World and returns one `StepOutput`. A rejected external command is reported in that output but does not by itself fail the Tick. System failure or invalid System output returns an error, discards staged state, leaves the visible Tick unchanged, and faults the Kernel. The caller must create a new Kernel from a trusted snapshot to resume.

This commit model is verified for the accepted P2 scope. It does not roll back arbitrary external side effects; Systems must express authoritative changes as returned Commands.

## 9. Event contract

Events represent facts that have already happened.

Requirements:

- An Event cannot directly mutate World.
- Event identity, ordering, visibility point, and consumption rules must be defined.
- Event payloads must have explicit ownership and versioning rules where they cross public API or persistence boundaries.
- Event handler failure must have a defined effect and must not silently undo or corrupt already committed authoritative state.
- Event order must be deterministic wherever it can affect authoritative simulation.

For the accepted P2 scope, Events are ordered records in a successful `StepOutput`, visible after the staged World commits. A failed Tick exposes no Events from discarded staged state. Event subscriptions, callbacks, and recursive mutation are out of scope and require a separate contract.

## 10. Determinism and replay

Within a declared compatibility boundary, identical initial authoritative state, simulation configuration, and authoritative input sequence must produce identical logical simulation results.

The implementation must normalize or control:

- Tick advancement.
- System execution order.
- Command order.
- Entity ID generation.
- Random-number state.
- Event order.
- Numeric behavior where relevant.
- Container iteration and data ordering.
- Timing of asynchronous results entering simulation.
- External inputs and their record/replay representation.

State hashes must be computed from canonical logical state, not memory layout or randomized container order.

Cross-version replay is a separate capability; save compatibility does not imply replay compatibility.

## 11. Errors and invariants

Kernel errors must be typed and distinguish at least:

- Invalid input.
- Invalid entity or resource reference.
- Command rejection.
- Scheduling or dependency failure.
- System execution failure.
- World invariant violation.
- Snapshot or restore failure.
- State consistency unknown.

If authoritative state integrity cannot be confirmed, the Kernel must stop normal advancement or enter a documented recovery process. It must not silently continue as if the state were valid.

Error values and diagnostics must provide enough context to identify the phase, Tick, System or Command involved, without requiring consumers to inspect private internals.

## 12. Public interface requirements

The Kernel public API must expose only the minimum required operations for:

- Constructing and configuring a Kernel instance.
- Registering or removing Systems under defined lifecycle rules.
- Enumerating registered System identities in execution order (read-only).
- Advancing one Tick.
- Submitting or consuming permitted authoritative inputs.
- Inspecting permitted state.
- Reading Events and command outcomes.
- Capturing and restoring canonical state through an explicitly defined interface.
- Obtaining a canonical state hash.
- Receiving structured errors and diagnostics.

Exact Rust signatures are an implementation deliverable and must be accompanied by API tests. This document intentionally does not invent signatures before the ownership and error semantics are settled.

## 13. Minimum contract tests

The full Kernel contract requires automated tests for the following. P2 verification evidence is recorded in the P2 report; unsupported future features are not implied to exist:

- Tick advancement and failure behavior.
- Deterministic System ordering.
- Cycle detection in scheduling dependencies if dependency scheduling is added.
- Command ordering and visibility, including external command rejection behavior.
- `Spawn` followed by `Set`.
- Entity ID reproducibility and collision handling.
- Resource mutation rules.
- Event visibility and ordering.
- World invariants.
- Snapshot and restore.
- Canonical state hashing.
- Repeated runs with identical initial state and inputs.
- System failure rollback, Kernel faulting, and recovery by constructing a new Kernel.

A test that has been written but not executed is not evidence of verification.

## 14. Scope boundaries and future decisions

P2 resolved the decisions required by the current implementation: Tick is `u64` with checked increment; Systems run serially in stable ID order; command visibility and rejection behavior are explicit; System failure discards the staged Tick and faults the Kernel; Entity IDs use `(WorldId, index, generation)`; Events are returned after commit; and snapshot/hash formats are versioned and defined in the P2 report.

The following capabilities are explicitly outside this v1 contract and require a new decision before implementation: parallel System execution, scheduler dependency metadata, Event callbacks or recursive Event mutation, Entity index reuse, and cross-platform bitwise determinism beyond the accepted build-configuration boundary. These exclusions do not reopen accepted P2 behavior.

The AI should propose options with trade-offs for future capabilities. Only decisions that materially affect product goals, public compatibility, or irreversible architecture need project-owner approval.

## 15. Acceptance criteria

The accepted P2 subset is implemented and verified as recorded in `planning/08-P2-Kernel契约硬化草案.md`. Remaining project-wide requirements in the parent specification are not thereby claimed as implemented; they must be assigned to a later phase and verified against their own acceptance criteria. Build/test evidence and known limitations must be recorded, and no unverified capability may be described as `VERIFIED`.
