# GK-KERNEL-02 — 原子 System 批量注册

- **状态：** 本轮实施任务卡（范围由负责人于 2026-10-19 批准）
- **范围：** `kernel-core` System 注册入口；不包含 Platform 迁移接线
- **依据：** `Kernel Contract v1.0.md` §5–6；`planning/02-规范缺口与冲突清单.md` GAP-21/GAP-23；GK-KERNEL-01 冻结身份规则

## 目标

提供 Kernel 原生的原子 System 批量注册操作，使一批系统在 ID 无效、与既有系统重复或批内重复时整体拒绝，不留下已注册前缀。成功批次中的每个 System ID 仅读取一次并冻结；遇到首个错误时立即返回，且此前尚未修改 Kernel 注册集合。

## 范围

- 新增 `Kernel::register_systems(Vec<Box<dyn System>>)`。
- 单项 `register_system` 复用批量实现，保持原有调用方式和错误分类。
- 先完成整批 ID 捕获、格式校验和重复检查；只有整批有效后才修改 Kernel 注册集合。
- 成功后统一按冻结 ID 字典序排序；空批次为无操作。
- 聚焦测试覆盖：批内重复、与既有 System 冲突、后置非法 ID 的失败原子性，以及成功批次排序。

## 非目标

- 不修改 `platform-core` 的 `ModRegistry` 接线；该层后续应单独立项使用新 API。
- 不改变 System 移除/替换语义，不增加依赖图、调度阶段、并行执行或生命周期状态。
- 不改变 Command、World、Tick、Event、Snapshot 格式或 P2 的执行/故障语义。

## 验收条件

1. 任意可返回的批量注册错误发生时，调用前已注册集合保持不变。
2. 成功批次中的所有 ID 均在整批提交前验证；任何输入 System 的 `id()` 至多读取一次。
3. 批内和既有集合中的重复 ID 返回现有 `KernelError::DuplicateSystemId`；无效 ID 返回现有 `InvalidSystemId`。
4. 成功批次的清单与执行顺序均按冻结 ID 字典序，单项 API 行为兼容。
5. 新增聚焦契约测试；本轮不运行测试/门禁，结果不得声称通过。

## 文件范围

- `crates/kernel-core/src/lib.rs`
- `crates/kernel-core/tests/registered_system_ids.rs`
- `Kernel Contract v1.0.md`
- `planning/02-规范缺口与冲突清单.md`
- 本任务卡
