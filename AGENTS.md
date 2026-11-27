# Game Kernel engineering rules

## Project status

- P1 headless vertical slice and P2 Kernel contract hardening have been approved and accepted; see `planning/07-P1实施与验收报告.md` and `planning/08-P2-Kernel契约硬化草案.md` for evidence and boundaries.
- P3 Mod Platform baseline ended with a **conditional pass** on 2026-10-14. Its acceptance boundaries and limitations are recorded in `planning/09-P3-Mod平台基线任务卡.md` and the authoritative exit ruling `planning/19-P3-07-第一阶段执行报告.md` §9; do not treat the wider Mod Platform specification as fully implemented.
- The current engineering/evidence stage is **P4 Save & Compatibility**. The repository records P4 local acceptance in `planning/49-P4-阶段本地最终验收报告.md` and `planning/21-全仓库现状与能力验收报告.md` §10. Those records do not establish Gitee Go remote acceptance; preserve the distinction between local, GitHub Actions, Gitee Go, and cross-platform evidence.
- `planning/22-P4-存档兼容与重放任务卡草案.md` is now a **controlled P4 task card** under R-18, executed on 2026-11-25; see `planning/26-P4-00-裁决决定记录.md` §8.6. Its control covers only the existing P4 Save & Compatibility scope and already approved contracts. It does not authorize new gameplay/product decisions, frozen-contract changes, public API or runtime-permission expansion, P5/P6/P7, or production operations. Gitee Go/Linux remote validation remains unrun; preserve the distinction between local acceptance and GitHub Actions evidence. The Owner delegated overall project coordination to the legacy RAE system on 2026-11-25. That management delegation was superseded when the Owner retired RAE on 2026-11-27. The approved P4 scope and contract boundaries remain in force; the legacy RAE task pool, Worker routing, RPCs, and control plane are no longer valid. Do not infer P5 readiness or start P5 from this status pointer.
- Current code is a prototype and does not prove the full v1.0 specifications.
- Read the relevant planning task card before changing a module. Preserve the status distinctions in `planning/06-AI开发与验收流程.md`.
- `Game Kernel — 完整项目规范 v1.0.md` is the umbrella specification. Root-level domain specifications refine it; `planning/` task cards define phase scope and acceptance. The accepted Kernel behavior is recorded in `Kernel Contract v1.0.md` and `planning/08-P2-Kernel契约硬化草案.md`. A specification's existence does not mean its full scope is implemented; follow the current task card and report unresolved conflicts before coding.

## Boundaries

- `kernel-core` owns authoritative world state and is independent of Platform and Wasmtime.
- `platform-core` validates Mod packages and adapts a restricted Wasm guest to the Kernel `System`/`Command` API.
- Mods never receive mutable World access. All authoritative writes pass through Kernel commands.
- Platform runtime imports are an allow-list. Do not add WASI, host filesystem, network, process, or native-module access without a separately reviewed security design.
- Runtime errors must remain visible. Do not convert failures into a successful Tick or silently discard required work.

## Development

- Use the pinned Rust toolchain in `rust-toolchain.toml` and commit `Cargo.lock` for this application workspace.
- Run `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace` before reporting P1 work complete.
- Add focused tests for behavior changes, failure paths, ordering, and snapshot/replay behavior.
- Update schemas, examples, documentation, and tests together when a public format changes.
- Never claim a command passed unless it was actually run in the current checkout.
- Clean Rust build caches with `cargo clean`, never through the Windows file manager. Confirm the project root, the Cargo workspace, and the effective build output directory first; if a custom `target-dir` or `CARGO_TARGET_DIR` is set, state the exact cleaning scope. If the target directory cannot be confirmed, stop and report instead of guessing a path.
- Only clean regenerable build artifacts that are confirmed to belong to this project (for example Cargo's `target` output). Never decide that a file is deletable from its name, extension, or directory name alone. Do not use broad recursive deletion on the project tree.
- Unless the user explicitly authorizes a specific target, never clean or overwrite: source code, tests, and project configuration; `AGENTS.md`, specifications, design docs, and task records; `.git`, history, branches, and existing uncommitted work; user archives, test samples, imported files, and other persistent data; files outside the project or another project's build artifacts.
- After cleaning, report the command, working directory, actual scope, and result; confirm source, specs, and Git history are untouched; re-check `git status` for unintended changes; and re-run the affected build or test commands only when further work requires them. Never clean caches unconditionally or periodically just to reduce file count.
- The build-cache cleanup permission is not a general permanent-delete permission. Stop and ask the user when the target is unclear, may contain user data, or reaches outside the current project.


## Engineering coordination

- **AEP is the sole engineering coordination system** for this project: [Love-DC/recursive-engineering-platform](https://github.com/Love-DC/recursive-engineering-platform). As of 2026-11-27, the AEP Project `hexa-game-kernel` and GitHub repository Resource `Love-DC/Hexa-Game-Kernel` are registered and verified in an isolated non-production Supabase instance. Its canonical Work Registry owns work identity, task state, dependencies, claims, execution state, session recovery, and delivery evidence. GitHub Issues, pull requests, branches, and Actions are linked work/delivery surfaces, not a second task registry. This is a trusted single-Owner A0 pilot, not a production authorization boundary or production write API.
- **Legacy RAE was retired by the Owner on 2026-11-27.** Do not read from or write to the legacy RAE Supabase project; do not use its task tables, RPCs (including `claim_task`), Workers, governance manifest, or old session-dispatch workflow.
- AEP registration is verified from persisted state. Before continuing an existing Work in a new ChatGPT session, recover its current state through AEP rather than relying on prior chat history. Do not create a replacement local queue, Worker system, or issue-based dispatch process. Sessions are manually started; AEP does not launch background ChatGPT workers or schedules.
- Repository instructions and specifications remain authoritative for Game Kernel product goals, approved contracts, architecture boundaries, and validation. AEP coordinates engineering work; it does not replace or reinterpret those project decisions.
- Sessions are manually started. Do not claim that this repository has a background AI worker, scheduler, or automatic ChatGPT session launcher.
