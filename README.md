# Game Kernel headless prototype

This workspace contains the approved P1 headless vertical slice and the accepted P2 Kernel contract hardening. The P3 Mod Platform baseline ended with a **conditional pass** on 2026-10-14; see the [authoritative P3 exit ruling](planning/19-P3-07-第一阶段执行报告.md) §9. The current engineering/evidence stage is **P4 Save & Compatibility**. The repository records P4 local acceptance in the [P4 local final acceptance report](planning/49-P4-阶段本地最终验收报告.md) and [whole-repository status report](planning/21-全仓库现状与能力验收报告.md) §10. This is local-only evidence: Gitee Go remote validation for the P4 workspace has not run, and cross-platform acceptance is not established. The P4 task card [planning/22-P4-存档兼容与重放任务卡草案.md](planning/22-P4-存档兼容与重放任务卡草案.md) remains **DRAFT** and R-18 is unexecuted; this status does not authorize blanket P4 implementation or P5. Proceed only under a separately authorized task and its approved contract.

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
