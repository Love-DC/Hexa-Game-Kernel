# Game Kernel engineering rules

## Project status

- The approved first implementation scope is the P1 headless vertical slice.
- Current code is a prototype and does not prove the full v1.0 specifications.
- Read the relevant planning task card before changing a module. Preserve the status distinctions in `planning/06-AI开发与验收流程.md`.
- The four v1.0 specifications are present in the workspace. Treat them and the approved P1 task card as the source baseline; the planning review records the project state at its review date, not the current implementation status.

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
