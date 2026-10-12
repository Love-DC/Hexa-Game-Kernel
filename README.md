# Game Kernel P1 prototype

This workspace is the first approved headless vertical slice for the Game Kernel project. It contains a small deterministic Kernel and a Platform adapter that discovers one manifest-driven WebAssembly Mod, checks its requested capabilities, and lets it submit a world command through a narrow Host API.

This is a prototype. It does not implement the complete v1.0 Kernel, Mod, Save, or Multiplayer specifications. In particular, the current demonstration supports only integer counter state, one restricted Mod Host API, and development WAT as well as Wasm bytecode. It does not include a Launcher UI, networking, migration, a store, hot loading, or native Mod execution.

## Requirements

- Rust 1.99.0 (toolchain pinned in `rust-toolchain.toml`)
- Git

## Run

```powershell
cargo run -p game-platform -- demo
```

The demo loads `examples/mods/p1-counter`, validates its manifest and permissions, resolves and locks the Mod package, executes three fixed logical ticks, saves a snapshot, restores it, and verifies the resumed state hash.

## Verify

```powershell
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
cargo run --locked -p game-platform -- demo
```

See `planning/` for the project review, architecture responsibilities, remaining specification gaps, phase plan, P1 task cards, owner decisions, AI acceptance workflow, and [P1 implementation and acceptance report](planning/07-P1实施与验收报告.md).
