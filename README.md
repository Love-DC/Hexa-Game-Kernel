# Game Kernel headless prototype

This workspace contains the approved P1 headless vertical slice and the accepted P2 Kernel contract hardening. P3 Mod Platform work is underway. It contains a small deterministic Kernel and a Platform adapter that discovers a manifest-driven WebAssembly Mod, checks its requested capabilities, and lets it submit a world command through a narrow Host API.

This is a prototype. It does not implement the complete v1.0 Kernel, Mod, Save, or Multiplayer specifications. In particular, the current demonstration supports only integer counter state, one restricted Mod Host API, and development WAT as well as Wasm bytecode. It does not include a Launcher UI, networking, migration, a store, hot loading, or native Mod execution.

## Requirements

- Rust 1.99.0 (toolchain pinned in `rust-toolchain.toml`)
- Git

## Run

```powershell
cargo run -p game-platform -- demo
```

The demo loads `examples/mods/p1-counter`, validates its manifest against [`schemas/mod-manifest-v1.schema.json`](schemas/mod-manifest-v1.schema.json) and Platform rules, checks permissions, resolves and locks the Mod package, executes three fixed logical ticks, saves a snapshot, restores it, and verifies the resumed state hash.

## Verify

```powershell
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
cargo run --locked -p game-platform -- demo
```

See `planning/` for the project review, architecture responsibilities, remaining specification gaps, phase plan, task cards, owner decisions, AI acceptance workflow, [P1 implementation and acceptance report](planning/07-P1实施与验收报告.md), and [P2 Kernel contract report](planning/08-P2-Kernel契约硬化草案.md).
