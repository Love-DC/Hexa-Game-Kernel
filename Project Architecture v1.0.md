# Project Architecture v1.0

**Project:** Game Kernel  
**Document Type:** Architecture Contract  
**Version:** 1.0.0  
**Status:** Architecture Baseline  
**Language:** Rust  
**Primary Goal:** Build a minimal, deterministic, modular game kernel that can be maintained by AI and support a large community-driven Mod ecosystem.

---

## 0. Architecture Authority

This document defines the project-wide engineering architecture.

It governs:

- Repository structure.
- Module responsibilities.
- Dependency direction.
- Runtime boundaries.
- Public API boundaries.
- Mod integration boundaries.
- Build and test organization.
- Architectural change control.

This document must be used together with:

- `Kernel Contract v1.0.md`
- `Mod Platform Specification v1.0.md`
- `Save & Compatibility Specification v1.0.md`
- `Multiplayer Specification v1.0.md`
- `Security, Performance & Diagnostics Specification v1.0.md`
- `AGENTS.md` files throughout the repository.

If documents conflict, the conflict must be recorded and resolved explicitly before implementing the affected behavior.

AI agents MUST NOT silently invent missing contracts or change frozen architecture decisions.

---

## 1. Core Architectural Principles

### 1.1 Minimal Kernel

The Kernel is the smallest runtime that advances world state according to deterministic time and execution rules.

The Kernel consists of:

- Time
- Scheduler
- System
- World
- Command
- Event

The Kernel MUST NOT contain game-specific content or business rules.

### 1.2 Platform Outside the Kernel

The following capabilities belong outside the Kernel core:

- Mod discovery and loading.
- Mod dependency resolution.
- Mod lifecycle management.
- Mod sandboxing.
- Mod API Registry.
- Rendering.
- Audio.
- Input devices.
- Networking.
- Save file management.
- User interface.
- Asset management.
- Logging backends.
- Operating system integration.

These capabilities may interact with the Kernel through explicit public interfaces.

### 1.3 Content as Mods

Official gameplay content MUST be implemented through the same public Mod integration mechanism available to third-party developers.

The official game must not require hidden Kernel modifications to implement ordinary gameplay.

Replacing the official content Mod must be possible when its declared dependencies and public API contracts permit it.

### 1.4 AI Maintainability

The project MUST be organized so that an AI agent can work on an individual subsystem without needing to understand or rewrite the entire repository.

Every subsystem MUST have:

- A documented responsibility.
- Explicit inputs and outputs.
- A defined public interface.
- A limited dependency set.
- Automated tests.
- Clear acceptance criteria.

### 1.5 Stable Boundaries

Cross-module communication MUST use explicit interfaces.

Modules MUST NOT access another module's private implementation details merely because they reside in the same repository.

---

## 2. Workspace Structure

The initial repository MUST use a Rust workspace.

```
game-kernel/
├── AGENTS.md
├── Cargo.toml
├── Cargo.lock
├── rust-toolchain.toml
├── rustfmt.toml
├── .gitignore
├── README.md
│
├── docs/
│   ├── Project Architecture v1.0.md
│   ├── Kernel Contract v1.0.md
│   ├── Mod Platform Specification v1.0.md
│   ├── Save & Compatibility Specification v1.0.md
│   ├── Multiplayer Specification v1.0.md
│   ├── Security, Performance & Diagnostics Specification v1.0.md
│   ├── decisions/
│   └── tasks/
│
├── kernel/
│   ├── AGENTS.md
│   ├── kernel-core/
│   │   ├── AGENTS.md
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── time/
│   │       ├── scheduler/
│   │       ├── system/
│   │       ├── world/
│   │       ├── command/
│   │       └── event/
│   │
│   └── kernel-api/
│       ├── AGENTS.md
│       └── src/
│
├── platform/
│   ├── AGENTS.md
│   ├── mod-manager/
│   ├── mod-runtime/
│   ├── api-registry/
│   ├── save-manager/
│   ├── multiplayer/
│   ├── security/
│   ├── diagnostics/
│   └── assets/
│
├── runtime/
│   ├── AGENTS.md
│   └── src/
│
├── launcher/
│   ├── AGENTS.md
│   └── src/
│
├── game/
│   ├── AGENTS.md
│   ├── official-mods/
│   └── examples/
│
├── tests/
│   ├── architecture/
│   ├── integration/
│   ├── compatibility/
│   ├── multiplayer/
│   ├── security/
│   └── performance/
│
├── tools/
│   ├── mod-validator/
│   ├── save-inspector/
│   └── diagnostics-cli/
│
└── benchmarks/
```

This structure defines architectural boundaries, not a requirement that every directory immediately contain a complete implementation.

Directories and workspace members may be added incrementally when their contracts and responsibilities are defined.

The root documentation files may reside directly in the repository root if existing project conventions require it. The repository MUST use one canonical location for each specification and MUST NOT maintain conflicting duplicate copies.

---

## 3. Workspace Responsibilities

### 3.1 `kernel/kernel-core`

Contains the six minimal Kernel modules.

It MUST NOT depend on:

- Mod Runtime.
- Mod Manager.
- Multiplayer.
- Save Manager.
- Rendering.
- Audio.
- Launcher.
- Game content.
- Operating system-specific platform services.

It MUST NOT contain direct filesystem or network access.

Its public behavior must remain deterministic under the conditions defined by the Kernel Contract.

### 3.2 `kernel/kernel-api`

Defines stable public contracts used by platform services and gameplay Mods.

It may define:

- Public data types.
- Command representations.
- Event representations.
- World access contracts.
- System registration contracts.
- Mod-facing interfaces.
- Versioned API declarations.

It MUST NOT depend on platform implementation crates.

Public interfaces MUST NOT expose private Kernel data structures unless explicitly frozen as part of the public contract.

### 3.3 `platform/mod-manager`

Responsible for:

- Mod discovery.
- Manifest parsing.
- Dependency validation.
- Version constraints.
- Load order input validation.
- Mod lifecycle coordination.
- Enable and disable policies.
- Mod configuration validation.

The launcher supplies the selected and ordered Mod configuration.

The Mod Manager validates and consumes that configuration. It MUST NOT silently replace the launcher's intended order with an unrelated order.

If dependencies or constraints make the requested configuration invalid, the platform must report the conflict.

### 3.4 `platform/mod-runtime`

Responsible for:

- Mod execution.
- WASM runtime integration.
- Host function interfaces.
- Resource quotas.
- Runtime traps.
- Mod execution lifecycle.
- Mod-to-platform communication.

It MUST NOT modify Kernel internals directly.

All world mutations must follow the public command and state-transition contracts.

### 3.5 `platform/api-registry`

Responsible for:

- Registering public Mod APIs.
- Resolving declared API dependencies.
- Validating API versions.
- Exposing API metadata.
- Detecting incompatible registrations.
- Providing developer-facing API discovery information.

The registry MUST distinguish public APIs from private implementation details.

A Mod MUST NOT gain access to an API merely because another Mod happens to be loaded.

### 3.6 `platform/save-manager`

Responsible for:

- Save file reading and writing.
- Snapshot coordination.
- Serialization and deserialization.
- Save metadata.
- Schema versioning.
- Migration orchestration.
- Backup and recovery.
- Save integrity verification.

It MUST NOT bypass Kernel interfaces to mutate the live world.

Save format and migration behavior are governed by the Save & Compatibility Specification.

### 3.7 `platform/multiplayer`

Responsible for:

- Session management.
- Network protocol handling.
- Player identity integration.
- Permission validation.
- Command sequencing.
- State replication.
- Snapshot transport.
- Reconnection.
- Multiplayer version validation.

It MUST NOT introduce network dependencies into `kernel-core`.

### 3.8 `platform/security`

Responsible for:

- Permission enforcement infrastructure.
- Mod capability policies.
- Sandboxing configuration.
- Input validation utilities.
- Security policy enforcement.
- Resource limit integration.
- Security audit events.

Security checks MUST occur at the relevant execution boundary, not solely during Mod installation.

### 3.9 `platform/diagnostics`

Responsible for:

- Structured logging.
- Error code registration.
- Metrics.
- Profiling interfaces.
- Diagnostic report generation.
- Performance tracing.
- Runtime health information.

The Kernel may expose minimal instrumentation hooks, but it MUST NOT require a logging backend to execute correctly.

### 3.10 `platform/assets`

Responsible for asset discovery, validation, caching, and access.

Rendering and audio systems consume assets through platform interfaces.

Asset loading MUST NOT be added to the Kernel core.

### 3.11 `runtime`

Composes the Kernel and platform services into an executable game runtime.

Responsible for:

- Runtime initialization.
- Service construction.
- Dependency injection.
- Lifecycle coordination.
- Runtime shutdown.
- Error propagation between subsystems.

It MUST NOT duplicate the responsibilities of the Kernel or platform services.

### 3.12 `launcher`

Responsible for:

- Selecting Mods.
- Resolving the user's intended configuration.
- Producing a launch plan.
- Displaying compatibility errors.
- Selecting runtime mode.
- Starting or configuring the runtime.
- Providing developer and diagnostic entry points.

The launcher MUST NOT implement game simulation logic.

### 3.13 `game`

Contains official game content implemented as Mods, example Mods, and game-specific configuration.

Gameplay logic MUST NOT be placed in `kernel-core` merely because it is needed by the official game.

### 3.14 `tools`

Contains standalone development utilities.

Tools MUST use documented public interfaces or supported file formats.

A diagnostic tool MUST NOT require unrestricted access to mutate a running game world.

---

## 4. Dependency Direction

### 4.1 Allowed dependency direction

The intended dependency direction is:

```
Launcher
   |
   v
Runtime Composition
   |
   +------> Kernel Core
   |
   +------> Platform Services
                  |
                  +------> Kernel API
                  |
                  +------> Kernel Core
                  |
                  +------> External Libraries

Official Mods
   |
   v
Public Mod API
   |
   v
Mod Runtime / Host Interfaces
   |
   v
Kernel API and Platform Services
```


This diagram expresses logical dependencies. Actual Rust crate edges must be checked against the workspace dependency rules.

### 4.2 Kernel Core isolation

`kernel-core` MUST NOT depend on higher-level platform crates.

The following dependency directions are forbidden:

```
kernel-core -> mod-runtime
kernel-core -> multiplayer
kernel-core -> save-manager
kernel-core -> launcher
kernel-core -> game content
```

Platform crates may depend on Kernel interfaces, but Kernel internals must not depend on platform implementation details.

### 4.3 Dependency cycles

Workspace dependency cycles are forbidden.

If two modules require each other's functionality, their shared contract must be moved to an appropriate lower-level interface or the responsibilities must be separated.

AI MUST NOT solve dependency cycles by introducing unrestricted global state.

### 4.4 Dependency enforcement

The project MUST automate dependency-boundary checks.

The checks must detect:

- Forbidden crate dependencies.
- Cyclic dependencies.
- Platform dependencies in Kernel core.
- Game-specific logic in Kernel core.
- Unapproved public API exposure.

A written rule without an automated check is insufficient for boundaries that can be enforced by the build system.

---

## 5. Kernel Internal Architecture

### 5.1 Time

Responsible for:

- Logical Tick representation.
- Tick progression rules.
- Deterministic time semantics.
- Time-related public types.

It MUST NOT read wall-clock time to determine authoritative game-state evolution.

### 5.2 Scheduler

Responsible for:

- Deterministic task scheduling.
- Task ordering.
- Task eligibility.
- Task lifecycle.
- Defined scheduling failure behavior.

It MUST NOT depend on nondeterministic execution order.

### 5.3 System

Responsible for:

- Defining executable game-state processing units.
- Registering Systems.
- Enforcing declared execution order.
- Running Systems through the Kernel execution contract.

The module MUST NOT contain specific gameplay implementations.

### 5.4 World

Responsible for:

- Authoritative in-memory game state.
- Entity and component ownership.
- World resources.
- Stable state access rules.
- State consistency.

Its exact storage representation may evolve as long as the public contract remains valid.

### 5.5 Command

Responsible for:

- Representing requested world mutations.
- Validating command structure.
- Applying mutations according to the Kernel Contract.
- Enforcing transaction and failure semantics.

Commands MUST NOT directly perform filesystem or network operations.

### 5.6 Event

Responsible for:

- Representing state-transition notifications.
- Defining event visibility and lifetime.
- Defining deterministic event ordering.
- Providing event delivery contracts.

Events MUST NOT be treated as a substitute for authoritative state mutation.

### 5.7 Cross-module rules

All six modules MUST follow the same frozen execution semantics.

No module may independently redefine:

- Tick progression.
- Command visibility.
- Entity identity.
- State ownership.
- Failure rollback.
- Event timing.
- Deterministic ordering.

Those semantics belong to the Kernel Contract.

---

## 6. Mod Loading and Runtime Lifecycle

The platform MUST distinguish these states:

1. Discovered.
2. Parsed.
3. Validated.
4. Dependency-resolved.
5. Selected.
6. Loaded.
7. Initialized.
8. Enabled.
9. Active.
10. Disabled.
11. Unloaded.
12. Failed.

The exact legal transitions must be defined by the Mod Platform Specification.

A Mod being loaded MUST NOT imply that it executes every Tick.

A Mod being enabled MUST NOT automatically grant every capability.

A Mod failing initialization MUST NOT leave partially registered APIs, Systems, or Event subscriptions behind.

The platform must either roll back incomplete registration or mark the runtime unusable when safe rollback cannot be guaranteed.

The launcher provides the requested configuration; the platform validates whether that configuration can be executed safely.

---

## 7. Save Compatibility Boundary

Save compatibility belongs to the platform layer.

The architecture MUST separate:

- Save file format.
- World-state schema.
- Mod-owned persistent schema.
- Mod code version.
- Kernel API version.
- Runtime version.
- Migration implementation.

The Save Manager coordinates migrations but does not invent gameplay rules for old saves.

A Mod is responsible for declaring its persistent schema and supported migrations.

The platform determines whether the complete requested Mod configuration can be loaded safely.

If compatibility cannot be established, the original save MUST remain unchanged and the failure must be reported.

---

## 8. Multiplayer Boundary

Multiplayer belongs to the platform layer.

The architecture MUST separate:

- Network transport.
- Session authority.
- Player identity.
- Command sequencing.
- Kernel execution.
- State replication.
- Save coordination.

The authoritative server must interact with the Kernel through supported interfaces.

The Kernel must not know whether a Command originated from a local player, a remote client, a scripted Mod, or a replay system unless that distinction is explicitly part of a frozen public contract.

All authority and permission checks must happen before untrusted requests can cause unauthorized world changes.

---

## 9. Error Handling

### 9.1 Explicit errors

Public interfaces MUST define their error behavior.

Errors must distinguish, where applicable:

- Invalid input.
- Incompatible version.
- Missing dependency.
- Permission denied.
- Resource limit exceeded.
- Invalid execution state.
- Serialization failure.
- Migration failure.
- Runtime trap.
- Internal invariant violation.

### 9.2 No silent recovery

Modules MUST NOT silently ignore errors that can affect world correctness, save integrity, compatibility, or security.

Recoverable errors may be handled locally only when the recovery behavior is documented and tested.

### 9.3 Error ownership

Each module is responsible for reporting errors originating within its boundary.

Higher layers may add context, but must not erase the original error code or cause.

Fatal invariant violations must stop unsafe execution rather than permit the world to continue in an unknown state.

---

## 10. Rust Engineering Rules

### 10.1 Workspace

The repository MUST use a Rust workspace with a committed `Cargo.lock` for reproducible application builds.

The Rust toolchain MUST be pinned through `rust-toolchain.toml` once the project baseline is frozen.

### 10.2 Public API

Public APIs MUST be intentional and documented.

Internal implementation types SHOULD remain private.

Unnecessary `pub` declarations are forbidden when they expose implementation details without a contract requirement.

### 10.3 Unsafe Rust

`unsafe` code MUST NOT be introduced into `kernel-core` without a documented safety justification and explicit review.

Every unsafe block must state the invariants required for correctness.

Where practical, unsafe code should be isolated into a small, testable boundary.

### 10.4 Dependencies

New third-party dependencies MUST have a documented purpose.

Dependencies MUST be checked for compatibility, maintenance status, licensing, and security risk.

A dependency must not be added solely to avoid implementing a small and stable internal abstraction.

### 10.5 Determinism

Code participating in authoritative state evolution MUST avoid unspecified iteration order and uncontrolled concurrency.

Floating-point behavior, random number generation, and serialization must follow the relevant deterministic contracts.

### 10.6 Testing

Every workspace member MUST have appropriate automated tests.

Public contracts MUST have tests for both successful behavior and documented failure cases.

Integration tests MUST verify that modules work together without violating dependency boundaries.

---

## 11. Build, Test, and Continuous Integration

The project MUST provide reproducible commands for:

- Formatting.
- Compilation.
- Unit tests.
- Integration tests.
- Documentation tests.
- Dependency-boundary checks.
- Dependency security checks.
- Linting.
- Performance benchmarks.
- Compatibility tests.

The initial CI pipeline SHOULD run the following categories:

1. Formatting and static checks.
2. Workspace compilation.
3. Unit tests.
4. Integration tests.
5. Architecture boundary checks.
6. Dependency and security checks.
7. Documentation checks.

Long-running performance and stress tests may run in a separate CI stage, but must remain reproducible and required for the relevant release gates.

Exact commands and tool versions must be defined by the Development Baseline.

AI agents MUST report the actual commands executed and their results.

---

## 12. Architectural Change Control

The following changes require explicit architecture review:

- Adding a dependency to `kernel-core`.
- Changing the six Kernel module boundaries.
- Changing the Kernel public execution contract.
- Introducing global mutable state.
- Changing deterministic ordering rules.
- Changing Command transaction semantics.
- Changing Entity ID semantics.
- Moving networking or saving into the Kernel.
- Introducing an unrestricted Mod host interface.
- Changing public API compatibility rules.
- Introducing a new authoritative state mutation path.

Architecture changes must include:

1. Motivation.
2. Affected modules.
3. Compatibility impact.
4. Security impact.
5. Performance impact.
6. Migration plan.
7. Tests.
8. Decision record.

Approved changes must update the relevant specifications and `AGENTS.md` instructions.

AI agents MUST NOT treat architecture review as permission to modify unrelated contracts.

---

## 13. Required Acceptance Tests

The architecture is accepted only when automated checks prove:

- `kernel-core` has no forbidden platform dependencies.
- Workspace dependency cycles are absent.
- All six Kernel modules exist and follow their documented boundaries.
- Platform services interact with Kernel through supported interfaces.
- Official gameplay content resides outside Kernel core.
- Mod execution cannot bypass public mutation interfaces.
- Save Manager cannot silently bypass migration contracts.
- Multiplayer does not introduce network dependencies into Kernel core.
- Public APIs are documented and versioned.
- The build and tests are reproducible under the pinned toolchain.
- AI agents can validate changes using documented commands.

A directory structure alone does not constitute architecture compliance.

---

## 14. AI Agent Execution Rules

Before implementing any feature, the AI agent MUST:

1. Read the root `AGENTS.md`.
2. Read the relevant subsystem `AGENTS.md`.
3. Identify the governing specification.
4. Identify the allowed modification scope.
5. Identify the public interfaces involved.
6. Write or update the task card.
7. Define acceptance criteria.
8. Implement the smallest complete change.
9. Run the required tests.
10. Report modified files and actual test results.

AI agents MUST NOT:

- Rewrite unrelated modules.
- Add undocumented dependencies.
- Move features into Kernel core for convenience.
- Change public interfaces without updating their contracts.
- Replace failing tests with weaker tests merely to obtain a pass.
- Claim that unexecuted tests passed.
- Claim that planned features already exist.
- Silently change frozen architecture decisions.

If the task cannot be completed without changing a frozen contract, the agent must stop the affected work and propose a documented change.

---

## 15. Architecture Freeze Checklist

- Repository and workspace structure defined.
- Kernel core isolated.
- Six Kernel modules have explicit responsibilities.
- Kernel API boundary defined.
- Platform service boundaries defined.
- Mod lifecycle boundary defined.
- Save compatibility boundary defined.
- Multiplayer boundary defined.
- Security and diagnostics boundaries defined.
- Dependency direction documented.
- Forbidden dependency rules automated.
- Rust engineering baseline defined.
- Build and test workflow reproducible.
- Architectural change control established.
- AI implementation and acceptance rules established.

**Final Principle:**

The Kernel defines deterministic world-state execution. The platform supplies external capabilities. Mods supply gameplay. The launcher selects the intended configuration. Runtime composition connects these parts through explicit contracts.

The architecture must remain small at its core, explicit at its boundaries, and testable at every layer.

**Next Document:** `Game Kernel — AI Engineering Baseline v1.0.md`, which freezes the repository-wide AI development workflow, toolchain, task cards, testing rules, Git conventions, and acceptance process.
