# GK-KERNEL-01 — System 身份冻结与调度排序

- **状态：** 本轮实施任务卡（范围由负责人于 2026-10-19 批准）
- **阶段：** P4 并行的 Kernel 契约缺口收敛；不改变 P2 已验收的确定性范围
- **依据：** `Kernel Contract v1.0.md` §5–6；`planning/02-规范缺口与冲突清单.md` GAP-22；P2 已验收的字典序串行调度规则

## 目标

消除 Kernel 对 `System::id()` 在注册之后仍稳定、可重复的隐式依赖。Kernel 在成功注册时读取一次 ID，并将其作为该 System 实例在当前 Kernel 生命周期内的冻结身份；重复检测、清单读取、调度排序、Kernel 生成的非法命令诊断和 System 命令来源均使用冻结身份。

## 范围

- `kernel-core`：为已注册 System 保存冻结 ID；保持成功注册的 System 按冻结 ID 字典序串行执行。
- 契约文档：说明 ID 的格式校验发生在注册时，注册后的身份不再从 `System::id()` 重新读取。
- 聚焦契约测试：覆盖可变 ID 在注册后漂移时，身份、排序和命令来源仍保持冻结；保留重复与非法 ID 的既有拒绝行为。

## 非目标

- 不增加 System 移除/替换、依赖图、调度阶段、并行调度、访问描述或事件回调。
- 不改变 Command、World、Tick、Event、Snapshot 格式或 P2 的故障/提交语义。
- 不修改 Platform、Save Manager、依赖协调器或 P4 SAVE 任务产物。

## 验收条件

1. 注册成功时仅从新 System 读取一次 ID；此后不再调用其 `id()`。
2. Kernel 的注册清单与执行顺序按冻结 ID 字典序稳定。
3. System 输出命令的来源 ID、Kernel 生成的非法命令错误归属均使用冻结 ID；System 自身构造的 `SystemError` 载荷保持原样。
4. 重复 ID 与无效 ID 仍在注册时拒绝，失败注册不改变已注册集合。
5. 新增聚焦测试；本轮遵循执行指令，不运行测试/门禁命令，后续运行结果不得预先声称通过。

## 文件范围

- `crates/kernel-core/src/lib.rs`
- `crates/kernel-core/tests/registered_system_ids.rs`
- `Kernel Contract v1.0.md`
- `planning/02-规范缺口与冲突清单.md`
- 本任务卡
