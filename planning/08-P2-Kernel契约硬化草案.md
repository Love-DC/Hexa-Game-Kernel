# P2 Kernel 契约硬化草案

- 状态：项目负责人已批准建议默认值；P2 契约冻结，首轮实现已完成本地验收
- 依据：`Game Kernel — 完整项目规范 v1.0.md` 第 3、8、9、11 节；`planning/03-分阶段开发路线图.md`；当前 P1 实现
- 目标：在扩展 Kernel API 前，先把状态身份、查询权限、提交语义、快照和确定性范围写成可测试的契约

## 1. P1 当前事实与边界

- `kernel-core` 只有一个 `World`，包含 `WorldId`、`Tick`、单调递增 Entity index、Entity/Component 和 Resource。
- Entity generation 当前固定为 0；Entity index 不回收。`World::entity` 与 `World::resource` 提供只读单项查询；内部 System 可读整个 `&World`，没有声明式读集。
- `Kernel::step` 将外部命令按 `(source, sequence)` 排序；System 按 SystemId 排序；系统输出依序提交。World 在 Tick 暂存值完成后一次替换，因此 System 错误不会提交暂存 World。
- 快照是 JSON envelope，`format_version = 1`；状态摘要是 SHA-256(domain + `serde_json::to_vec(World)`)。这描述了 P1 当前实现，不等于已冻结的长期存档格式。
- 快照不包含 System 实例、Mod 运行时实例、授权记录或输入日志；`from_snapshot` 返回未注册 System 的 Kernel。重放/继续执行必须由上层重新装配相同模拟配置。
- P1 已在本地 Windows 和 Gitee Go 云端 Linux 环境分别运行检查，但尚未用同一黄金输入比较两种环境产生的逐 Tick 摘要；不得据此声明跨平台逐位确定性。
- 当前 Platform 会检查 Manifest 请求的权限是否被授予，但导入检查只限制 Host API 名称，Linker 总是注册 `counter_read` 与 `counter_set`。因此权限语义尚未真正绑定到具体 API；这是 P2 的安全修复项。

## 2. 已批准并冻结的 P2 契约

### 2.1 确定性兼容边界（对应 D-02）

确定性承诺限于**相同受支持构建配置内**：相同 Kernel/Platform 构建、运行时和配置摘要、Mod 内容摘要、初始逻辑状态及按序权威输入，必须产生相同的逻辑状态与逐 Tick StateHash。

- 任何进入模拟的外部输入都必须先变成带来源和序号的权威输入；系统时间、线程完成顺序和未记录外部响应不得直接改变模拟。
- System 执行顺序按稳定 SystemId 排序；同一 System 的命令按其输出序号提交；外部命令继续按 `(source, sequence)` 排序。任何规则变更都必须伴随契约测试。
- 先不承诺不同 OS、CPU 架构、编译器/编译选项或 Wasmtime 版本之间逐位相同。将来只有在跨平台黄金用例通过后，才逐平台扩展承诺。
- 当前数值类型没有浮点；P2 不加入浮点权威状态。若以后加入，必须先定义舍入、NaN、无穷和序列化规则。

### 2.2 快照与状态摘要

**快照传输格式**与**StateHash 逻辑编码**分开定义，JSON 字节不直接作为永久哈希规范。

- 快照 envelope 使用紧凑 UTF-8 JSON，包含 `magic = "GAME_KERNEL_SNAPSHOT"`、`format_version = 2`、World payload 和摘要。未知版本、magic 错误、重复键、未知字段、非法标识、损坏数据及摘要不符均拒绝恢复。
- P2 不读取 P1 `format_version = 1` 快照；v1 是开发期格式。将来如确有已发布存档需要迁移，另立只读导入器决策，不在 v2 解码器中猜测。
- StateHash 使用独立规范化二进制逻辑编码，按小端编码固定宽度整数；字符串为 UTF-8，前置 u64 小端字节长度；集合项按稳定 ID/键顺序排列，前置 u64 小端数量。
- 编码字段顺序固定为 WorldId(u64)、Tick(u64)、next_entity_index(u64)、Entity 数量；每个 Entity 编码 world(u64)、index(u64)、generation(u32)、Component 数量及按 type_id 排序的 Component；最后编码 Resource 数量及按 key 排序的 Resource。
- `ComponentValue` 的类型标签固定为 Integer=1（i64 小端二补码）、Boolean=2（单字节 0/1）、Text=3（u64 长度 + UTF-8 字节）。
- StateHash 为 `SHA-256("game-kernel-world-state-v2\\0" || canonical_world_bytes)`；哈希域或编码字段变化时必须提升版本域。摘要需可逐 Tick 比较。

### 2.3 World 查询与访问控制

- Kernel 内部可信 System 保留只读 `&World` 查询；公开遍历接口必须给出稳定顺序，不暴露可变存储引用。
- Mod 不直接接收 `World` 或任意 Resource/Component 容器。所有访问通过 Platform 暴露的逐 API Host 函数。
- 每个 Host API 声明所需权限。有效权限是 Manifest 请求权限与负责人/宿主授权权限的交集；缺少请求或授权时，在 Mod 注册/实例化之前拒绝，并报告稳定错误码。
- P1 的 `world.read` 对应只读 Host API；`world.command` 对应待提交命令写入。P2 必须验证无权限导入会被拒绝，且 Mod 不能通过别名或未知导入绕过 allow-list。
- P2 不实现权限撤销热更新、跨 Mod 数据共享和细粒度组件字段 ACL；权限仅覆盖受支持的只读查询与 Command 输出面。

### 2.4 Entity 与 Command

- Entity 身份为 `(WorldId, index, generation)`；index 在一个 World 生命周期内单调递增且不复用，generation 保留用于未来回收策略。跨 World、错误 generation 和已销毁实体的目标必须稳定拒绝。
- Spawn、Component/Resource 修改和 Despawn 只能经 Command Pipeline。批次中前序成功命令的变化对后续命令可见；单条无效命令拒绝且不改该命令目标；System 失败使整个 Tick 的暂存状态不可见并使 Kernel 进入可观察故障状态。
- 稳定错误码与面向用户的诊断文本分开：测试和机器协议依赖错误码，不依赖字符串措辞。
- 在没有调度访问描述符前继续串行执行；不因“只读”接口而推断可以并行。

## 3. P2 工作分解与验收

| 项 | 工作 | 验收证据 |
|---|---|---|
| P2-01 | 正式 Kernel API 与错误码契约 | 每个公共类型唯一归属；字段作用域、边界、稳定错误码有文档与契约测试 |
| P2-02 | 权限到 Host API 的绑定 | 请求/授权缺失时拒绝；读写分别验证；未知导入拒绝；无权限 Wasm fixture 自动覆盖 |
| P2-03 | World 身份、查询和提交规则 | Entity 旧 ID/跨 World/溢出、稳定遍历、Command 顺序和 Tick 原子边界有测试 |
| P2-04 | 规范化 StateHash 与逐 Tick 差异 | 固定逻辑编码、域版本和 SHA-256；黄金状态夹具跨重复运行摘要一致 |
| P2-05 | 快照 v2 与输入/装配边界 | 错误版本/损坏/重复键/未知字段/摘要错配明确拒绝；文档说明快照不含的运行时状态 |
| P2-06 | 故障分类与复现报告 | 拒绝、Mod 故障、Kernel 故障、快照错误的错误码/恢复方式明确；失败不会伪装为成功 Tick |
| P2-07 | 阶段集成验收 | 本地与 Gitee Go 检查通过；逐项报告覆盖契约、故障注入及未验证的跨平台边界 |

P2 完成前不新增并行 System 执行、跨版本重放承诺、存档迁移器或更宽权限 API；分别留给后续阶段处理。

## 4. 实现与验收记录（2026-10-13）

首轮实现和本地补充验收已完成。P2 的合同覆盖仍不完整，因此不将整个 P2 标为已验收。

| 项 | 本轮状态 | 证据与缺口 |
|---|---|---|
| P2-01 | 部分实现 | Kernel 与 Platform 错误提供稳定命名空间码；完整公共类型归属和字段边界仍需单独 API 契约表。 |
| P2-02 | 已实现，已验收 | `counter_read` 要求 `world.read`，`counter_set` 要求 `world.command`；Manifest 请求、宿主授权、导入 allow-list 均在实例注册前检查；自动测试覆盖缺少写权限和未知导入拒绝；Gitee Go 已通过。 |
| P2-03 | 已实现，已验收 | 稳定只读实体遍历；跨 World、错误 generation、缺失实体、allocator 溢出的稳定拒绝码；外部命令单条拒绝不阻断后续有效命令；System 故障回滚 Tick。 |
| P2-04 | 已实现，已验收（单一锁定环境） | StateHash 黄金向量覆盖 World 身份/Tick/分配器、Entity ID、资源及组件的 Integer/Boolean/Text；逆序插入得到相同摘要。未比较不同操作系统产物。 |
| P2-05 | 已实现，已验收 | Snapshot v2 magic/version、v1 拒绝、未知字段、重复资源/组件键、重复实体 ID、非法 World、摘要错配和截断 JSON 均有拒绝测试；快照不包括 System/Mod 实例及输入日志。 |
| P2-06 | 部分实现 | Kernel/Platform 顶层错误与 Command rejection 有稳定码；Kernel System 故障/故障态码及不提交暂存 Tick 有测试。Platform Mod trap 的恢复路径已由集成测试覆盖，但稳定的诊断分类与复现报告格式仍需完成。 |
| P2-07 | Windows 本地与 Gitee Go Linux 检查通过；整体阶段仍待验收 | 本地执行记录见下。负责人提供的 Gitee Go 页面截图显示提交 `e69e1ad` Webhook 构建成功。跨平台黄金摘要尚未执行，不能据此承诺跨 OS 确定性。 |

### 本地执行记录

- `cargo fmt --all -- --check`：通过。
- `cargo clippy --locked --workspace --all-targets -- -D warnings`：通过。
- `cargo test --locked --workspace`：通过；Kernel、Platform、集成与文档测试均通过。
- `cargo run --locked -p game-platform -- demo`：通过；演示输出 3 个 Tick 且 `verified=true`。
- 当前环境：Windows 本地工作区；未用跨平台黄金夹具比较云端 Linux 和本地的同一状态摘要。

### 故障恢复语义

| 类别 | 可观察结果 | 恢复方式 |
|---|---|---|
| 单条外部 Command 被拒绝 | `CommandRejectionCode`；拒绝只影响该命令，后续命令仍处理，Tick 可继续完成。 | 修正命令内容/身份后，以有效权威输入重试；原拒绝不回滚其他已接受命令。 |
| System 执行失败或产生非法命令 | Kernel 返回稳定 `KernelErrorCode`，Tick 暂存状态不提交，Kernel 进入 Faulted 状态。 | 从故障前快照创建新 Kernel，并由调用方重新注册相同 System；先修复失败源再继续。 |
| Mod 加载/运行失败 | Platform 加载错误码，或 Kernel 的 System failure；当前 Mod 实例不得继续推进模拟。 | 修正包/权限，或从故障前快照重建 Kernel 与 Platform 运行时。 |
| Snapshot 格式、数据或摘要错误 | `KernelError::code()` 返回稳定快照错误分类；恢复函数不返回半初始化 Kernel。 | 保留原始快照并从可信副本恢复；不得忽略摘要错误或将其当成空 World。 |

## 5. 后续验收门槛

下一步补齐 P2-01 的正式 Kernel API 类型/字段契约表，以及 P2-06 Platform Mod trap 的稳定诊断码与复现报告格式。跨 OS/架构确定性仍是明确未验证项，不作为本轮承诺。

## 6. 已批准的兼容决定

| 决定 | 已批准规则 |
|---|---|---|
| 确定性目标 | 同一锁定构建配置内；跨 OS/架构需黄金用例证明后再承诺 |
| P1 JSON 快照 | P1 为开发期格式；P2 v2 为首个正式格式，不读取 P1 v1 |
| System 读权限模型 | Kernel 内部可信 System 可读只读 World；不可信 Mod 仅经权限绑定的 allow-list Host API 读写 |

本契约不扩大当前授权范围，不作跨平台或 P1 快照向后兼容承诺。P2 按本文件工作分解实施；每项实现完成后更新证据与剩余限制。
