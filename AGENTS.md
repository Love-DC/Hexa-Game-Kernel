# Game Kernel engineering rules

## Project status

- P1 headless vertical slice and P2 Kernel contract hardening have been approved and accepted; see `planning/07-P1实施与验收报告.md` and `planning/08-P2-Kernel契约硬化草案.md` for evidence and boundaries.
- P3 Mod Platform baseline is the current stage; follow `planning/09-P3-Mod平台基线任务卡.md` before changing Platform modules.
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
