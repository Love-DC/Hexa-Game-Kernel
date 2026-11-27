# Game Kernel headless prototype

This workspace contains the approved P1 headless vertical slice and the accepted P2 Kernel contract hardening. The P3 Mod Platform baseline ended with a **conditional pass** on 2026-10-14; see the [authoritative P3 exit ruling](planning/19-P3-07-第一阶段执行报告.md) §9. The current engineering/evidence stage is **P4 Save & Compatibility**. P4 has a recorded local acceptance report in [planning/49](planning/49-P4-阶段本地最终验收报告.md) and the P4 audit update in [planning/21 §10](planning/21-全仓库现状与能力验收报告.md). The current GitHub Actions matrix checks have passed on Ubuntu 24.04 and Windows 2025; GitHub Actions is the current CI authority, and this result is limited to those checks rather than a production or general cross-platform guarantee. The P4 task card [planning/22](planning/22-P4-存档兼容与重放任务卡草案.md) is now a **controlled P4 task card** under R-18, executed on 2026-11-25. Its control covers only the existing P4 Save & Compatibility scope and already approved contracts; it does not authorize new product/gameplay decisions, frozen-contract changes, runtime-permission expansion, or P5/P6/P7. The former Gitee Go workflow was retired from the active repository on 2026-11-27; historical mentions remain dated records only and do not define current CI or P4 acceptance. The legacy RAE management delegation was superseded by the Owner's retirement of RAE on 2026-11-27. The approved P4 scope and contract boundaries remain in force; the old RAE queue is not an active task authority.


## Engineering coordination

This repository is coordinated natively by [AEP](https://github.com/Love-DC/recursive-engineering-platform). The AEP Project `hexa-game-kernel` and GitHub repository Resource `Love-DC/Hexa-Game-Kernel` were registered and verified in an isolated non-production Supabase instance on 2026-11-27. AEP's canonical Work Registry owns task identity, status, dependencies, claims, execution recovery, and delivery evidence; GitHub Issues, pull requests, branches, and Actions are linked delivery surfaces, not a second task registry. This is a trusted single-Owner A0 pilot, not a production authorization boundary or production write API. Legacy RAE was retired on 2026-11-27; do not use its queue, Workers, RPCs, or Supabase project. Do not create a local task queue as a substitute. Project specifications and approved contracts in this repository continue to govern product and technical boundaries. Sessions remain manually triggered; no background AI worker, scheduler, or automatic ChatGPT session launcher is provided.

This remains a prototype, not a complete implementation of the v1.0 Kernel, Mod, Save, or Multiplayer specifications. The headless P1 demo is deliberately narrow: it exercises integer counter state and one restricted Mod Host API through a small WebAssembly Mod, followed by Kernel snapshot/restore verification. It does not demonstrate the full P4 Save/Migration workflow.

The workspace also contains a bounded P4 Save & Compatibility subsystem: `crates/save-manager` provides save manifests, World/Mod data partitions, integrity checks, loading and compatibility evaluation, migration planning and transaction handling, backup/recovery, diagnostics, and same-boundary replay support. `crates/platform-core/src/migration_runtime.rs` provides the restricted Wasm migration adapter used by the P4 migration path. These capabilities have task-specific contracts and tests; they do not imply that the complete v1.0 Save specification, every storage/durability scenario, large-scale Mod operation, or cross-version/cross-platform replay has been delivered. See [planning/49](planning/49-P4-阶段本地最终验收报告.md) and [planning/21 §10](planning/21-全仓库现状与能力验收报告.md) for exact scope and limitations. The former Gitee Go workflow is retired; current remote CI evidence is provided by the GitHub Actions matrix above.

The repository still does not include P5 networking, a Launcher UI, an SDK/store workflow, hot loading, or native Mod execution. Runtime capabilities remain limited to the approved contracts and allow-list.

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
