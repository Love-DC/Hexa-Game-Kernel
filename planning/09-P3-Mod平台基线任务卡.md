# P3 任务卡：Mod Platform 基线

- 阶段：P3
- 状态：阶段规划草案；建议先兼容当前 P1 包、权限默认拒绝、无热重载；具体实施按任务卡依赖推进
- 依据：`Game Kernel — 完整项目规范 v1.0.md` 第 4、5、8、12、13 章；`planning/03-分阶段开发路线图.md`；已验收的 P2 Kernel API
- 当前代码基线：`crates/platform-core` 包含严格 JSON Manifest 解析、本地目录发现、必需依赖 SemVer 校验、确定性拓扑解析、内存 LoadPlan、宿主权限授权和 Wasmtime 适配；目前仅有 `gk::counter_read` / `gk::counter_set` 两个 Host API。
- 规范边界：仓库内独立的 `Mod Platform Specification v1.0.md` 是空文件；本卡以完整项目规范为依据。不得静默填充空文件或将未决字段假装成既有契约。

## 目标与范围

形成能自动校验、确定解析、可复现锁定、可观察生命周期与故障的 Mod Platform MVP。P3 只扩展受控 Wasm 和声明式包的基础契约，不增加权限面或运行时逃逸能力。

### 保守基线

- 保持当前 `manifest_version = 1` 的既有字段与含义：`manifest_version`、`id`、`version`、`kernel_api`、`entrypoint`、`requested_permissions`、`dependencies`。为这份实际支持的子集发布正式 JSON Schema；不把完整项目规范建议字段说成已实现字段。
- 未知 Manifest 字段继续拒绝。新字段或语义变更须提升 Manifest 版本并提供兼容决定；不因补 Schema 改变现有示例包。
- P3 初版依赖只支持必需的本地直接 Mod 依赖和 SemVer 约束；缺失、重复、冲突、版本不符或循环均拒绝整份加载计划。可选依赖、Mod 间公开 API 依赖与 Game Definition 约束先写清规则后再接入。
- 权限按 deny-by-default 处理：Manifest 申请不等于授权，未经宿主授予不注册；P3 先登记现有 Host API 的版本、输入输出、调用阶段、确定性和权限，不扩大权限。
- 运行时生命周期不支持热重载、权限热撤销、WASI、文件/网络/进程访问或进程内原生 Mod；故障后重建 Kernel/Platform 实例。
- 锁文件作为正式复现输入；校验实际包摘要和运行时标识，不静默改写用户选择的组合。

## P3 工作包

### GK-P3-01 — Manifest v1 Schema 与诊断

- **范围：** `crates/platform-core`、`schemas/`、Manifest 示例、契约测试。
- **实施：** 发布 JSON Schema；记录每字段类型、必填/默认值、约束、未知字段策略和版本边界；把校验错误改为稳定码加字段路径/原因，消息文案不作为机器协议。
- **测试：** 正常包往返；必填字段缺失、未知字段、非法 ID/SemVer/权限/入口、超限文件；Schema 和 Rust 校验结果一致。
- **验收：** 当前 P1 示例无需修改即可通过；文档指出尚未支持的总规范建议字段。

### GK-P3-02 — Resolver 与依赖契约

- **范围：** `resolve_load_plan`、依赖模型、Resolver 契约测试。
- **实施：** 冻结候选包输入、重复 Mod ID、版本约束、依赖拓扑排序和冲突处理；所有可选输入排列都产生相同有序计划；未满足必需依赖时整计划失败。
- **测试：** 多个可用版本、重复/缺失依赖、版本不匹配、环、自依赖、输入顺序置换及重复运行。
- **验收：** 错误含稳定 PlatformErrorCode 与关联 Mod/依赖标识；Launcher 候选选择不会被 Platform 静默改变。
- **边界：** 现阶段只有一个同 ID 包可用，因此不猜测多版本 side-by-side；如 P3 要支持多版本，先冻结身份/隔离语义。

### GK-P3-03 — `mod-lock.json` 正式格式

- **范围：** LoadPlan 序列化、锁文件 Schema、摘要计算和锁文件读写 API。
- **实施：** 定义版本化 JSON 格式、规范摘要输入、运行时/API 标识、Mod 版本/内容摘要/依赖列表；加载时重算摘要并比对实际包，拒绝缺字段、未知版本、未知字段和不匹配。
- **测试：** 同输入字节稳定；文件读写往返；任一包/依赖/运行时/Kernel API 变化都会导致校验失败或新摘要；损坏锁文件拒绝且不覆盖原文件。
- **验收：** 从锁文件复现相同 Mod 有序组合；锁定结果不是只存在内存中的报告。

### GK-P3-04 — API Registry / Catalog 最小目录

- **范围：** 现有 Host API 描述符与权限映射；不扩展执行端点。
- **实施：** 为每个 API 登记稳定 ID、版本、提供者、guest 签名、权限、调用阶段、错误分类、确定性属性和状态；Resolver/实例化阶段以 Registry 校验模块导入。
- **测试：** 描述符与实际 Wasm Linker 导入一一对应；未知 ID、错误版本、错误签名或缺权限均在注册前拒绝；清单输出顺序稳定。
- **验收：** 机器可读目录与运行时 allow-list 同源，避免文档和实际导入分叉。

### GK-P3-05 — 生命周期状态机与注册事务

- **范围：** Platform 的 Mod 生命周期及 Kernel System 注册边界。
- **实施：** 至少定义 `Installed → Validated → Resolved → Loaded → Initialized → Active`，并覆盖 `Disabled/Unloaded/Failed`；标出允许操作、权限检查点、状态所有者和失败恢复。加载批次在完整校验后原子注册，不留半激活组合。
- **测试：** 非法状态迁移、重复激活、缺依赖、实例化失败、System 故障和批次中途失败；确认无半注册状态及可观测错误报告。
- **验收：** 状态机表与实现/测试一致；热重载明确为不支持；恢复采用从可信快照重建。

### GK-P3-06 — Platform 故障报告与资源预算

- **范围：** Manifest/Resolver/权限/Runtime/Mod 执行失败以及 Wasm 的 Fuel/Memory 限制报告。
- **实施：** 统一稳定错误码、阶段、Mod ID、内容摘要、Tick（若已进入模拟）、可重试性及恢复建议；避免把权威模拟失败归类成可忽略的非权威错误。
- **测试：** 导入拒绝、燃料超限、内存超限、Guest Trap、非法 Host 输出及恢复重建；报告自身有版本和 Schema 校验。
- **验收：** 每类错误明确“继续/停止/重建”；日志诊断可复现，错误文本不作为协议字段。

### GK-P3-07 — 阶段集成验收

- **范围：** 清洁检出集成测试、Gitee Go 和文档。
- **验收：** 新建包 → 校验 → Resolver → 锁定 → 权限授权 → 加载/激活 → 执行 → 诊断/恢复完整闭环；本地与 Gitee Go 通过；未实现字段和能力清楚列出。

## 依赖与顺序

`P3-01 → P3-02 → P3-03`；`P3-01 → P3-04 → P3-05`；`P3-05 → P3-06`；全部汇入 `P3-07`。实现 API Registry 前先冻结其 descriptor；实现 Lifecycle 前先定义状态转换和批次事务，避免并行改动同一公共契约。

## 本阶段明确不做

商店下载/更新、签名信任根、任意原生 Mod、WASI、网络和文件 API、Mod 间直接调用、热重载、权限热撤销、完整 SDK/Launcher UI，以及未经测量的 Mod 数量或性能承诺。分发签名和信任政策在实施发布仓库前另行审议。

## 阶段完成定义

P3 只有在 P3-01 至 P3-07 的契约和验收逐项通过后才完成。仅有 Manifest struct、内存 LoadPlan 或 Wasm 通过演示，不算正式 Schema、持久锁文件或生命周期系统。
