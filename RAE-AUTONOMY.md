# RAE Autonomous Engineering Policy

- Project key: `hexa-game-kernel`
- Policy mode: `active_session_autonomy`
- Owner: the repository owner
- Scope: engineering work within this repository's approved product goals, specifications, frozen contracts, and accepted task cards.

## Purpose

RAE coordinates evidence-based engineering work for this repository. An active Coordinator session may inspect the project, identify bounded engineering work, create and route project-scoped tasks when the RAE operational controls permit it, execute eligible work, run verification, review delivery independently, and merge eligible engineering-only pull requests under the RAE review protocol.

This is session-driven autonomy. It does not claim that a background scheduler or automatic ChatGPT launcher exists.

## Authority and protected decisions

The repository's existing authority order remains binding:

1. Owner-confirmed product goals and constraints.
2. Frozen project contracts.
3. Approved module specifications.
4. Approved implementation plans and task cards.
5. Temporary engineering choices.

RAE may autonomously make ordinary implementation and maintenance decisions inside an already approved boundary. It must not silently:

- Change the product goal, gameplay or player experience, public product behavior, or an Owner-confirmed design decision.
- Change a frozen Kernel, Platform, Manifest, Save, Multiplayer, security, or public API contract.
- Expand runtime capabilities or permissions beyond the approved scope (including WASI, filesystem, network, process, native Mod execution, or hot reload).
- Introduce a new phase, feature family, compatibility promise, platform-support promise, or performance target that is not already approved.
- Perform production deployment, credential changes, destructive data operations, or other reserved operations.

When work requires one of those decisions, preserve the evidence and alternatives in a proposal or Owner decision request. Do not turn it into an executable implementation task until authorized.

## Task discovery and activation

A Coordinator session should inspect, in order:

1. Existing non-terminal Supabase tasks and their dependencies.
2. Open engineering PRs, exact-head CI, unresolved review findings, and recent merged changes.
3. Repository task cards, acceptance reports, tests, and specifications for concrete, reproducible gaps.
4. Read-only dependency, security, and quality signals that are already available.

Eligible autonomous task candidates are limited to:

- Reproducible defects or failing checks within an approved contract.
- Incomplete acceptance criteria explicitly present in an approved task card.
- Missing regression tests for an implemented, approved behavior.
- Documentation or CI drift that can be corrected without changing product or public contract semantics.
- Bounded engineering maintenance with demonstrable impact and a repeatable validation plan.

Every candidate needs exact evidence, impact, risk, allowed paths, exclusions, acceptance criteria, validation commands, and a stable project-scoped fingerprint. Deduplicate against existing tasks, reports, and PRs. Do not bulk-import GitHub Issues; an Issue alone is not authorization.

Create no more than three new discovery tasks per project per scan. Use `requires_review=true` unless the repository's accepted workflow explicitly permits otherwise. Route implementation and review to Workers belonging to this project only. The independent Coordinator review remains mandatory before merge.

### Existing planning-status discrepancy

The current roadmap (`planning/03-分阶段开发路线图.md`) describes P3 as conditionally passed and points to `planning/19` for the exit ruling, while the P3 task card (`planning/09-P3-Mod平台基线任务卡.md`) still labels P3 as executing. Before selecting work whose eligibility depends on the P3 exit state, inspect the cited final acceptance evidence and reconcile the status from repository evidence. Do not infer completion or reopen completed work from either stale status label alone.

## RAE operational controls

Repository policy and Supabase operational controls are separate:

- This document authorizes bounded, active-session engineering discovery within the scope above.
- Supabase `autonomy.paused` is a fail-closed operational stop switch.
- Supabase `autonomy.max_active_executions` is a concurrency cap and must remain within the database-enforced range.
- A Coordinator must obey the live project controls before task creation, claim, or lifecycle mutation. If paused or unverified, continue read-only discovery and repository-only maintenance; do not write directly to task tables or bypass the RPC boundary.
- Register this project with its own project-scoped Workers. Never reuse Worker UUIDs from RAE, Colony Sim, or another project.

## Engineering and delivery workflow

1. Read `AGENTS.md`, the root product specification, the Kernel Contract, the AI Engineering Baseline, the relevant phase/task card, and all applicable local rules.
2. Pin the repository revision and read every declared governance file at that exact SHA.
3. Decompose only at real module, file-ownership, interface, or validation boundaries. Avoid overlapping writers.
4. Work on a unique task branch from the authorized base SHA. Preserve existing work and use small, reviewable commits.
5. Run the repository's applicable checks. The current CI workflow is `.github/workflows/ci.yml`; its exact-head results are the source of CI evidence.
6. Inspect the complete diff and update specifications, schemas, examples, and tests together when a permitted change affects them.
7. Open a PR and report its exact head SHA, changed paths, commands actually run, CI run identifiers, and remaining limitations.
8. The Coordinator performs an independent review against the exact head SHA. Fix findings and re-review. Merge only eligible engineering-only PRs after required checks pass and the PR is mergeable.
9. Verify the merge commit and post-merge checks. Persist truthful execution evidence and do not claim that unrun checks passed.

## Required verification commands

Use the pinned Rust toolchain in `rust-toolchain.toml` and the locked workspace dependencies:

```bash
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
cargo run --locked -p game-platform -- demo
```

Run only commands relevant to the changed scope when appropriate, but run the full set for phase-level acceptance. Record commands that were not run as not run.

## Completion and reporting

A task is complete only when its observable acceptance criteria are met, required checks have actually run, the exact delivery revision is reviewed, and its evidence is persisted. Keep these states distinct: implemented, locally tested, CI-verified, independently reviewed, merged, and deployed.

No production deployment or Supabase project activation is implied by this policy.
