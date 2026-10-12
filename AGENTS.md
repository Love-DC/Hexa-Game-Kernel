# Game Kernel engineering rules

## Project status

- P1 headless vertical slice and P2 Kernel contract hardening have been approved and accepted; see `planning/07-P1实施与验收报告.md` and `planning/08-P2-Kernel契约硬化草案.md` for evidence and boundaries.
- P3 Mod Platform baseline is the current stage; follow `planning/09-P3-Mod平台基线任务卡.md` before changing Platform modules.
- Current code is a prototype and does not prove the full v1.0 specifications.
- Read the relevant planning task card before changing a module. Preserve the status distinctions in `planning/06-AI开发与验收流程.md`.
- `Game Kernel — 完整项目规范 v1.0.md` is the current umbrella specification and is marked as a design baseline. The separate Kernel Contract and Project Architecture files are absent from the current working tree; the separate Mod Platform and Save specification files are empty. Do not treat those empty files as requirements or silently restore/delete spec material. Use the completed P1/P2 reports and current P3 task card to scope implementation; the full specification remains the broader product target.

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
