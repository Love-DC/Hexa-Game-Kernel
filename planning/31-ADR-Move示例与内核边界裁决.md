# ADR-31 — Move 示例与内核边界裁决

- 裁决编号：ADR-31
- 状态：已批准
- 生效日期：2026-10-18
- 关联裁决链：GK-ARCH-AUDIT-01 → GK-ARCH-VERIFY-01 → GK-ARCH-DECISION-01~03 → GK-P4-SAVE-004-FINAL-BOUNDARY → GK-P4-SAVE-004-APPROVAL

## 背景

一名开发者要求从零开始实现一个"最小 P1 闭环"，包含 `PlayerPos { x: f32, ... }`、`Command::Move` 及直接坐标推进逻辑。此请求在保留仓库前提下被拒绝，原因如下。

## 已确认冲突

1. **P1 已完成并验收**（planning/07 + planning/08）。World + Command + Tick + 快照/哈希 的全部基础闭环已有实现与 232 个测试覆盖，不存在从零实现空间。
2. **`ComponentValue` 不含浮点**（`Integer | Boolean | Text`）。浮点是否入内核已在 GK-ARCH-DECISION-01/02 中明确缓裁决，SAVE-004 前冻结。任何含 `f32` 坐标的演示方案都违反此冻结。
3. **Kernel 不含游戏特定业务逻辑**（Kernel Contract §1）。`PlayerPos` 属于具体游戏内容，不属于 kernel-core。
4. **覆盖性冲突**：在仓库根新建 `Cargo.toml`/`src/main.rs` 将直接覆盖现有 workspace 结构。

## 裁决

### 方向：2（使用现有已验收 API 写演示/测试）

- 不推倒重来、不覆盖结构、不引入浮点。
- 在 `kernel-core` 的既有框架内，使用 `ComponentValue::Integer` + `Command::SetResource` + `StepInput` 外部命令管线演示坐标推进。
- 产出物归入 `kernel-core/examples/` 与 `kernel-core/tests/`——属于仓库补充，不引入新 crate。

### 已实现产出

| 文件 | 性质 | 说明 |
|---|---|---|
| `crates/kernel-core/examples/move_demo.rs` | 可运行示例 | `cargo run -p kernel-core --example move_demo` |
| `crates/kernel-core/tests/move_command_pipeline.rs` | 集成测试 | 走命令管线的推进 + 确定性 + 错误拒绝 |

### 演示机制

- 坐标由三个命名资源承载：`demo.move/pos_x`、`demo.move/pos_y`、`demo.move/pos_z`，类型 `ComponentValue::Integer`。
- 每一 Tick 的"移动"被宿主翻译为**三个外部 `Command::SetResource`**，通过 `StepInput` 提交。
- 宿主在提交前**读取当前世界状态**计算绝对目标值；真正修改由内核命令管线执行（排序、校验、应用、事件生成、哈希更新）。
- 错误路径验证：命令 ID 的 `tick` 字段不等于当前世界 tick 时，管线拒绝（`WrongTargetTick`），状态不变化。

### 对未来 AI 的约束

1. **P1/P2 已完成并验收**：不要再提出"从零实现 kernel 最小闭环"类请求。如有能力不足，使用仓库已有 `seeded_kernel` 或 `demo` 模式。
2. **不要引入浮点到内核**：任何涉及坐标/比率/连续量的演示必须使用 `Integer`（或等待浮点扩展裁决），否则自动违反已冻结契约。
3. **不要在仓库根覆盖文件**：所有补充示例/测试必须放入各 crate 的 `examples/`/`tests/` 目录。
4. **区分"可运行演示"与"已验收契约"**：示例代码的教育价值不等于架构验收；不能因为有演示就声称功能已通过。
