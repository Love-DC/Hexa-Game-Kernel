# Game Kernel — AI Engineering Baseline v1.0

- Document Type: AI Engineering and Repository Baseline
- Version: 1.0.0
- Status: Proposed baseline — adapt to the actual repository before enforcement
- Parent Specification: `Game Kernel — 完整项目规范 v1.0.md`
- Audience: Project-planning AI, architecture AI, coding AI, testing AI, maintenance AI

> This document defines how AI agents inspect, plan, change, test, and report work in the Game Kernel repository. It does not claim that the proposed directory structure or automation already exists. First inspect the actual repository, then add or adapt files without overwriting useful existing work.

## 1. Purpose

The project must be maintainable by AI through explicit boundaries, reproducible tooling, small tasks, automated tests, and truthful reports.

The baseline exists to make development auditable and repeatable. It must not become a reason to create unnecessary documents or delay a minimal runnable implementation.

## 2. Authority and conflict resolution

Follow this order when requirements conflict:

1. User-confirmed product goals and constraints.
2. Frozen project contracts.
3. Approved module specifications.
4. Approved implementation plans and task cards.
5. Temporary implementation choices proposed by an AI agent.

If requirements at a higher level conflict, stop the affected work and report the conflict. Do not silently choose one side.

The parent project specification defines project-wide goals. `Kernel Contract v1.0.md` refines Kernel behavior. Platform-specific specifications refine their own domains. More specific documents may detail a parent requirement but must not silently contradict it.

Before treating any requirement as final, label it with one of:

- `CONFIRMED`
- `PROPOSED`
- `PENDING`
- `IMPLEMENTED`
- `VERIFIED`
- `DEPRECATED`

`IMPLEMENTED` does not mean `VERIFIED`.

## 3. Required AI workflow

For every task, the agent must:

1. Read the root `AGENTS.md` and the applicable local instructions.
2. Inspect the relevant source, tests, configuration, and documentation.
3. Identify dependencies, constraints, and the exact files likely to change.
4. State the goal, scope, and acceptance criteria in the task card or work plan.
5. Implement the smallest coherent change.
6. Run the required formatter, static checks, and tests that are available for the changed scope.
7. Inspect the diff for unrelated edits, placeholders, weakened tests, and accidental contract changes.
8. Report the actual commands run, their results, remaining failures, and known limitations.
9. Update documentation when public behavior, contracts, or developer workflows change.

If a command cannot be run, report it as not run. Never report a skipped or failed test as passed.

## 4. Repository baseline

The following is a proposed logical structure, not permission to overwrite an existing repository layout. Compare it with the actual project and record any approved differences.

```text
game-kernel/
├── AGENTS.md
├── README.md
├── rust-toolchain.toml            # or existing rust-toolchain file; choose one convention
├── Cargo.toml                     # workspace root, if Rust workspace is used
├── Cargo.lock                     # commit for applications/workspace reproducibility
├── .cargo/
│   └── config.toml
├── docs/
│   ├── specifications/
│   ├── architecture/
│   ├── decisions/
│   └── plans/
├── kernel/
│   ├── AGENTS.md
│   ├── kernel-core/
│   │   ├── AGENTS.md
│   │   └── src/
│   └── kernel-api/
│       └── AGENTS.md
├── platform/
│   └── AGENTS.md
├── runtime/
│   └── AGENTS.md
├── launcher/
│   └── AGENTS.md
├── game/
│   └── AGENTS.md
├── sdk/
│   └── AGENTS.md
├── tests/
│   └── AGENTS.md
├── tools/
│   └── AGENTS.md
└── .github/
    └── workflows/
```

### 4.1 Structure rules

- Keep the Kernel free of dependencies on concrete game content and peripheral services.
- Keep Platform responsibilities outside Kernel.
- Put public contracts in documented public API crates or modules.
- Put integration and scenario tests in a clearly discoverable location.
- Keep architecture decision records under `docs/decisions/`.
- Do not create empty crates or directories solely to match this tree. Create them when the first task requires them.
- Do not rename an existing toolchain file without checking CI and local tooling. If the repository currently uses `rust-toolchain`, preserve it until a deliberate migration is approved.
- Exact crate names and workspace membership must be confirmed against the actual implementation.

## 5. AGENTS.md hierarchy

Use local instructions to narrow the root rules, not to contradict them.

Proposed hierarchy:

```text
AGENTS.md
├── kernel/AGENTS.md
│   ├── kernel-core/AGENTS.md
│   └── kernel-api/AGENTS.md
├── platform/AGENTS.md
├── runtime/AGENTS.md
├── launcher/AGENTS.md
├── game/AGENTS.md
├── sdk/AGENTS.md
├── tests/AGENTS.md
└── tools/AGENTS.md
```

### 5.1 Root instructions must cover

- Project purpose and authoritative documents.
- Dependency and ownership rules.
- Required workflow before and after changes.
- Testing and reporting discipline.
- Prohibition on claiming unverified success.
- Rules for handling conflicts and scope changes.

### 5.2 Local instructions must cover

Every local `AGENTS.md` should state:

- The directory's responsibility.
- What it must not own.
- Allowed dependencies.
- Public interfaces and invariants.
- Required tests and checks.
- Relevant specifications.
- Special failure or compatibility rules.

Do not copy the entire root document into every local file. Link to the authoritative specification by repository-relative path.

## 6. Module and dependency discipline

Each module must have a written contract covering:

- Responsibility and exclusions.
- Dependencies.
- Public interfaces.
- Inputs and outputs.
- State ownership.
- Error behavior.
- Determinism implications.
- Tests and acceptance criteria.

Dependency direction must follow the project architecture. At minimum:

- Kernel does not depend on Mods, Launcher, network transport, rendering, or save-file management.
- Platform consumes Kernel public interfaces.
- Game Definitions and Mods consume documented public APIs.
- Mods must not depend on other Mods' private implementation.
- Launcher must not mutate authoritative World state directly.

Use workspace boundaries, visibility controls, dependency checks, and tests to enforce these rules where practical. Documentation alone is not sufficient when an automated check is feasible.

## 7. Task-card standard

Every non-trivial engineering task must have a unique ID and include:

```markdown
# TASK-XXXX: <short title>

- Status: PROPOSED | READY | IN_PROGRESS | BLOCKED | DONE
- Owner: <agent or role>
- Related specifications: <repository-relative paths>
- Dependencies: <task IDs or none>

## Goal
What observable result must be delivered?

## Scope
Which modules/files may change?

## Out of scope
What must not be changed in this task?

## Steps
1. ...
2. ...

## Tests
- [ ] Required automated tests
- [ ] Required static checks
- [ ] Manual check, if automation is not possible

## Acceptance criteria
- [ ] Observable and testable criterion
- [ ] No unrelated contract changes
- [ ] Documentation updated where required

## Result
- Files changed:
- Commands executed:
- Test results:
- Known limitations:
- Follow-up tasks:
```

A task must be small enough to review and verify independently. If a task changes a public contract, create or update a decision record and identify downstream impacts before implementation.

## 8. Testing and quality gates

The exact command set must be confirmed from the repository and toolchain. For a Rust workspace, evaluate the following baseline commands and adapt them to the actual crates and CI:

```bash
cargo fmt --all -- --check
cargo check --workspace --all-targets
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

If a command is unsupported by the current toolchain or repository layout, document the reason and the approved replacement. Do not invent successful output.

As the project grows, evaluate additional tools such as `cargo-nextest`, dependency/license auditing, unused-dependency checks, and reproducible build checks. These are tool choices to validate, not mandatory dependencies until adopted by the project.

### 8.1 Test layers

Use the smallest relevant tests first, then broader tests as required:

1. Unit tests.
2. Module contract tests.
3. Integration tests.
4. Determinism and replay tests.
5. Save/restore and migration tests.
6. Security and resource-limit tests.
7. Multiplayer tests.
8. Performance and long-running tests.

A passing unit test does not replace a required integration or acceptance test.

### 8.2 Test integrity

Agents must not:

- Delete or weaken a failing test merely to get a green result.
- Mark an unexecuted test as passed.
- Replace required behavior with a placeholder and report completion.
- Hide a failure by suppressing diagnostics.
- Claim performance, security, compatibility, or determinism without relevant evidence.

A bug fix should include a regression test when feasible.

## 9. Build and toolchain reproducibility

The repository must declare its supported Rust toolchain and required development commands.

Requirements:

- Pin or otherwise explicitly define the toolchain used by CI.
- Keep dependency resolution reproducible for the project type.
- Record required external tools and installation instructions.
- Provide a documented environment verification procedure.
- Ensure CI uses the documented toolchain.
- Avoid requiring cloud services for core local development unless explicitly approved.
- Do not update the toolchain or broad dependency set as an unrelated side effect of a feature task.

The initial baseline must inspect existing files such as `rust-toolchain`, `rust-toolchain.toml`, `Cargo.toml`, `Cargo.lock`, and CI workflows before changing them.

## 10. Error handling, logs, and diagnostics

- Errors must be explicit and actionable.
- Logs should include useful context such as task, module, phase, Tick, System, or Command when available.
- Do not log secrets or sensitive local data.
- Distinguish user input errors, Mod failures, Platform failures, and Kernel invariant violations.
- If authoritative state consistency is unknown, do not silently continue normal simulation.
- A diagnostic message should explain the cause, impact, and likely next action where possible.

## 11. Documentation and decision records

Update documentation whenever a change alters:

- Public API or behavior.
- Module boundaries or dependencies.
- Save compatibility or migration behavior.
- Mod Manifest or lifecycle.
- Determinism or replay semantics.
- Security permissions or resource limits.
- Build, test, or release procedures.

A decision record should include:

- Context and problem.
- Considered alternatives.
- Selected approach and reason.
- Costs and risks.
- Affected modules and contracts.
- Verification plan.
- Conditions that would justify revisiting the decision.

Do not create a decision record for every trivial implementation detail.

## 12. Git and change management

- Work on a clearly identified branch or task scope where the repository workflow supports it.
- Keep changes focused and reviewable.
- Do not overwrite user changes or discard uncommitted work.
- Do not run destructive cleanup without checking the affected files and receiving required approval.
- Do not include generated binaries or local secrets unless explicitly required.
- Inspect the final diff before reporting completion.
- Commit only when the user or established workflow authorizes commits; never claim a commit exists unless it was actually created.

### 12.1 Build cache cleanup

This refines the destructive-cleanup rule above. It may allow cleaning regenerable Rust build caches, but it does not create a general file-deletion permission.

**Default method**

- Clean Rust build caches with `cargo clean`, not through the Windows file manager.
- Before running any cleanup, confirm the project root, the Cargo workspace, and the effective build output directory.
- If a custom `target-dir` or `CARGO_TARGET_DIR` is configured, determine and state the exact cleaning scope before acting.
- If the target directory cannot be confirmed, stop and report; never guess a path.

**Objects that may be cleaned automatically**

- Only build artifacts that are confirmed to belong to this project, are regenerable, and contain no user data — for example Cargo's `target` output.
- Do not decide that a file is deletable from its name, extension, or directory name alone.

**Objects that must be protected**

Unless the user explicitly authorizes a specific target, do not clean or overwrite:

- Source code, tests, and project configuration.
- `AGENTS.md`, specifications, design documents, and task records.
- `.git`, commit history, branches, and existing changes in the Git working tree.
- User archives, test samples, imported files, and other persistent data.
- Files outside the project and other projects' build artifacts.

Do not use broad recursive deletion commands to clean the entire project directory.

**Permanent deletion and permission boundary**

- For artifacts that meet the rules above, prefer `cargo clean`, and do not route them through the Windows Recycle Bin.
- Other temporary files may be cleaned only after their origin, actual path, and deletion scope are confirmed.
- Do not expand the build-cache cleanup permission into a permission to permanently delete arbitrary files.
- Stop and request user confirmation when the target is unclear, may contain user data, the deletion scope exceeds the current project, or the operation may affect other projects.

**Post-cleanup verification**

After cleaning, the agent must:

1. Report the command executed, the working directory, the actual cleaning scope, and the result.
2. Confirm that source, specifications, and Git history were not cleaned.
3. Check Git status and confirm the cleanup caused no unintended file change or loss.
4. Re-run the corresponding build or test commands only when further development requires them.
5. Never clean caches unconditionally or periodically just to reduce file count.

## 13. Public contract changes

Before changing a frozen contract:

1. Identify the exact contract and affected consumers.
2. Explain the reason and compatibility impact.
3. Propose the replacement and migration path.
4. Update tests and documentation.
5. Obtain the required approval.
6. Implement and validate the change.
7. Record the decision.

AI agents must not silently change public semantics to simplify implementation.

## 14. Security and untrusted inputs

Treat Mod packages, manifests, save files, network messages, migration inputs, and community content as untrusted.

- Validate inputs at boundaries.
- Apply explicit resource budgets where relevant.
- Do not assume that a signature proves code safety.
- Do not run untrusted native code without an actual, verified isolation boundary.
- Do not allow APIs, Events, background tasks, or network requests to bypass the authoritative Command Pipeline.
- Report security checks that are missing or not executed.

## 15. Performance discipline

- Establish reproducible benchmarks before making performance claims.
- Record environment, scenario, data scale, method, actual results, and variability.
- Measure startup, Tick, scheduling, command/event throughput, memory growth, save/restore, migration, replay, and server load as those capabilities are implemented.
- Do not claim support for 300+ Mods solely because the architecture permits it.
- Performance optimizations must preserve established simulation semantics and include regression tests where feasible.

## 16. Phase completion report

At the end of each task or phase, provide:

1. **Result:** completed, blocked, or partial.
2. **Changed files:** exact repository-relative paths.
3. **Behavior delivered:** observable behavior, not aspirations.
4. **Commands run:** exact commands.
5. **Verification:** pass/fail/not run, with relevant output summary.
6. **Known limitations:** unresolved issues and unverified claims.
7. **Contract impact:** any changed or newly clarified public semantics.
8. **Next steps:** concrete follow-up tasks and blockers.

Do not report only “done” or “all tests pass” without evidence.

## 17. Initial engineering baseline tasks

Before substantial feature development:

1. Inventory actual files and current repository state.
2. Compare existing documents against the parent specification.
3. Identify duplicate, conflicting, missing, and undecided requirements.
4. Confirm the workspace layout and Rust toolchain without destructive changes.
5. Define the first runnable vertical slice.
6. Create the first task cards with dependencies and acceptance tests.
7. Add only the instruction files needed for the current repository structure.
8. Establish working build, formatting, and test commands.
9. Record the actual baseline results.
10. Ask the project owner only about material product or irreversible architecture decisions.

The first runnable vertical slice should prove the essential flow: construct a Kernel, advance a Tick, run a System, submit and commit a Command, observe an Event, save/restore a minimal state, and run automated tests. Full multiplayer, a Mod marketplace, complete SDK tooling, and 300+ Mod stress acceptance are not prerequisites for this first slice.

## 18. Acceptance criteria

This baseline is adopted only when:

- The actual repository layout has been inspected.
- Root and relevant local instructions exist and do not contradict the parent specification.
- The documented build and test commands have been executed or explicitly marked unavailable.
- Task cards use testable acceptance criteria.
- Agents report commands and results truthfully.
- Contract changes are visible and traceable.
- No design target is described as verified without evidence.

**Current status: Proposed baseline — repository-specific adoption and verification pending.**
