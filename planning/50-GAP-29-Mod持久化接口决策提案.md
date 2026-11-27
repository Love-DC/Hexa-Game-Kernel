# DRAFT：GAP-29 Mod 自有持久化接口决策提案

- **文档状态：** DRAFT / 供项目负责人裁决；不是冻结合同、正式实施契约或实现授权。
- **提出日期：** 2026-11-27
- **关联候选：** `planning/28-P4-SAVE-002-执行准备与一致性核查.md` §2.2 GAP-29（X-5）
- **当前阶段：** P4 已按既定边界验收；本提案不重开 P4，不启动 P5/P6/P7。
- **约束：** 不修改现有 Frozen Contract，不改变玩法或产品方向，不在本提案中实现任何 Host API。

---

## 1. 决策摘要

六核已经具备 Mod 专属存档分区和 Save Manager 的不透明字节承载能力，但运行中的 guest Mod 还没有通过平台 API 读写自己持久化数据的通道。

这不是 P4 验收失败：P4 的批准裁决 B-3 明确选择“不透明字节承载，不实现平台 Mod 持久化读写 API”，并把读写通道列为 GAP-29 候选。它是一个尚未授权实施的后续能力缺口。

**建议方向（仅建议，待负责人批准）：** 提供最小的、按稳定 Mod ID 隔离的“不透明 payload”读写接口。第一版不做通用 key-value 数据库，不让 guest 传入其他 Mod ID 或任意路径，也不让 guest 直接访问文件系统。Save Manager 继续负责存档格式、备份、恢复和迁移；Mod 只读写自己的 payload。

实现前还必须裁决 Host API 描述模型目前只支持最多一个参数/返回值的问题。自然的线性内存读写接口需要传递指针与长度；不能绕过现有 API Registry / Catalog 合同直接在代码里发明 ABI。

## 2. 已核实的当前事实

1. **Mod Platform Frozen Contract 已定义能力类别。** `Mod Platform Specification v1.0.md` §9.2 定义 `storage.own` 为“读写本 Mod 的独立持久化空间”；§9.3 要求在实际操作时检查权限，权限撤销后后续调用必须失败；§10.1 要求限制持久化数据大小；§14 要求平台能够提供 Mod 自有持久化数据的 Schema 版本。
2. **Save Frozen Contract 已定义数据所有权。** `Save & Compatibility Specification v1.0.md` §4.2 要求每个 Mod 拥有以稳定 Mod ID 为基础的独立命名空间，Mod 只能通过平台公开接口访问自己的数据。
3. **P4 B-3 明确暂缓 guest 读写通道。** `planning/28` §6 批准 Mod 数据载荷为不透明字节，并明确 P4 不实现平台 Mod 持久化读写 API；该通道只作为 GAP-29 候选，补登和实施另待授权。
4. **当前 Host API 目录没有存储 API。** `planning/10-P3-04-API-Registry执行记录.md` 记录的已实现内置 Host API 只有 `gk::counter_read` 与 `gk::counter_set`。当前 `HOST_APIS` 注册表尚无 `storage.own` 对应调用。
5. **资源类别不等于数据接口。** `PersistentBytes` 已出现在 Platform 的资源限额类型中，但它是预算分类，不是 guest 数据读写接口；`planning/28` §2.2 已明确区分。
6. **P4 已验收并保留该边界。** `planning/49` 与 `planning/21` §10 记录 P4 验收通过，同时注明 Mod guest 尚无通用持久化 payload Host API。该缺口不会反向否定 SAVE-001～012 在已批准范围内的完成状态。

## 3. 设计目标与非目标

### 目标

- 每个 guest Mod 只能访问自身、由稳定 Mod ID 确定的持久化命名空间。
- 以不透明字节承载 Mod 自己定义的数据格式；平台不解释 Mod 私有业务结构。
- 权限在实际调用时检查，未授权、权限撤销或预算超限时返回可诊断失败。
- payload 与存档快照、备份、恢复及已定义迁移流程协同，不绕过 Save Manager。
- 同一确定性执行边界内，持久化读写的结果可重复、可保存和恢复。

### 非目标

- 不向 guest 暴露任意文件、目录、路径或其他 Mod 的命名空间。
- 不在第一版实现通用数据库、索引、查询语言、跨 Mod 共享存储或网络存储。
- 不由 Kernel 接管 Mod 存档业务；不将 Save Manager 逻辑塞入 Kernel。
- 不在本提案中决定玩法、Mod 商业策略、P5 联机行为或 P6 Launcher 体验。

## 4. 可行接口方案

### 方案 A：每个 Mod 一个不透明 payload（推荐）

每个 Mod 对自己的唯一 payload 执行读取和整体替换。payload 可以由 Mod 自行序列化为任意字节；平台只保存字节和必要的版本元数据。

**优点：**
- 与 P4 B-3 的不透明字节模型、Save §4.2 的 Mod 专属命名空间直接一致。
- 不要求平台理解 Mod 的数据结构。
- 不引入 key 命名规则、枚举排序、单 key/总量配额、索引或数据库恢复等额外机制。
- 可以复用现有 Save 分区、备份和迁移管线。

**代价：**
- Mod 更新一小部分数据时，需要自行重写完整 payload。
- guest 必须管理自己的序列化格式和 Schema 版本。

### 方案 B：每个 Mod 一个受限 key-value 命名空间

提供读取、写入、删除键值以及可能的键枚举。

**优点：** 小数据更新更方便，平台可以独立限制 key 数量和单项大小。

**代价：** 需要新增 key 编码、排序、重复键规则、单 key 与总量配额、删除语义、迭代稳定性以及更多宿主调用。它比当前 Save 合同要求更强，不建议作为第一版。

### 方案 C：保持当前由上层应用提供 payload

不增加 guest 持久化 API，继续由上层应用/可信适配器提供并回填 Mod payload。

**优点：** 不扩大 guest Host API 和权限面。

**代价：** Mod 无法自行保存状态；每个游戏应用都必须提供特定适配层。该方案不满足 Mod Platform §9.2 的 `storage.own` 能力在 guest 侧可实际使用的目标，只能作为暂缓 GAP-29 的选择。

## 5. 推荐的最小语义（全部待批准）

如负责人选择方案 A，建议把第一版语义限制为：

1. **命名空间：** 由宿主从已加载 Mod 的可信身份取得稳定 Mod ID；guest 调用不传入 Mod ID、目录名或路径。
2. **操作：** 只提供读取自身 payload 与整体替换自身 payload 两类语义。是否需要单独清空操作，可在实现契约中决定；不提供列举其他 Mod、复制或跨 Mod 访问。
3. **版本：** payload 保持不透明，但必须能随 payload 一同读取/保存 Mod 自己声明的 Schema 版本。具体字段归属和版本编码须在实施契约中确定，不能让平台猜测字节内容。
4. **权限：** 复用已定义的 `storage.own` 能力；实际每次调用都检查运行时 grant。Manifest 请求本身不等于自动授予。
5. **配额：** 复用 `PersistentBytes` 资源类别作为预算入口；实现前明确单 Mod 上限、整个运行实例上限，以及写入超限时的稳定错误语义。默认值和硬上限应与 Security/Performance 规范一致，不在本提案中虚构数字。
6. **事务：** 写入必须由宿主暂存并纳入 Save Manager 的一致性边界；不得在 guest 调用时直接执行文件系统写入。推荐同一 Tick 的写入先进入暂存区，只有整次 Kernel Tick 最终提交成功后才提交；任一 System/Mod 执行失败导致该 Tick 不提交时，丢弃本 Tick 的全部暂存写入。不能在某个 Mod 自身成功时就提前提交，否则后续 System 失败会让持久化数据与 Kernel World 状态分叉。最终提交点仍须与 Kernel Tick 和 Save Manager 的实际事务边界核对后再冻结。
7. **恢复与迁移：** 快照恢复后读取到已提交 payload；旧/未知 Schema 数据不得静默清空。迁移由 Save & Compatibility 既有迁移机制决定，guest API 不得绕过备份、验证、回滚和失败恢复。
8. **错误：** 至少区分权限拒绝、缓冲区/参数错误、配额超限、未知 Schema/不兼容、执行阶段不允许以及存储事务失败；稳定错误码由实施契约定义。
9. **确定性：** 在相同 Mod、相同输入与相同已提交 payload 下，读取结果必须一致；写入结果参与 Save/恢复与同边界回归测试。

## 6. 一个必须先解决的 Host API Registry 问题

当前 `planning/10` 的 API Registry / Catalog 契约只允许每个 Host API 有 0 或 1 个参数、0 或 1 个返回值（Schema 对参数与返回值列表设置 `maxItems: 1`）。当前 `HostApiSignature` / `HostApiValueList` 也按这个有限形状建模。

但一个自然的 Wasm 线性内存读写接口通常需要同时表达指针和长度。不能只增加两个 `func_wrap` 调用而不更新 Registry / Catalog 的机器可读签名合同。

可选路线：

- **6A：** 将参数/返回值列表扩展为明确、可验证的多参数签名，并按兼容策略升级 API Catalog Schema 版本；所有旧 API 的现有签名保持不变。
- **6B：** 使用受限的单参数打包/句柄方案，避免多参数签名扩展，但会引入编码约定和额外的缓冲区/句柄语义。

**建议：** 优先评估 6A。它使 Registry 能自然表达后续必要的宿主 API；但必须先评估 Catalog 消费端的版本兼容性，并控制为支持多参数所必需的最小改动，不借机建设通用 ABI 框架。

## 7. 建议的验收矩阵（实施授权后使用）

| 类别 | 必须验证的行为 |
|---|---|
| 所有权隔离 | Mod A 只能读取/写入自己的稳定 ID 命名空间；不存在通过参数切换到 Mod B 的路径 |
| 权限 | 没有 `storage.own` 授权时拒绝；授权撤销后后续调用拒绝 |
| 读写 | 空 payload、首次写入、覆盖写入、读取和保存/恢复往返行为明确 |
| 配额 | 单 Mod/实例总量边界、整数溢出、超限拒绝及旧 payload 保留 |
| 事务 | 同 Tick 写入在成功时提交；Mod/Kernel 执行失败时本次写入不泄漏 |
| 兼容性 | 旧 Schema、未知 Schema、Mod 移除/替换和迁移失败时不静默丢失数据 |
| 确定性 | 相同输入和持久化初始状态得到相同读取结果及存档字节/摘要 |
| 回归 | 现有 `gk::counter_read` / `gk::counter_set` 签名、目录和行为保持不变 |

## 8. 需要负责人裁决的事项

1. 是否将 GAP-29 作为 P4 之后的独立 Mod Platform 能力补齐事项，授权进入实施契约设计？
2. 如继续，是否采用方案 A（每 Mod 一个不透明 payload），而不是 key-value 或继续完全由应用提供？
3. 是否同意先扩展 Host API Registry / Catalog 的参数签名表达能力（6A），再加入存储 API？
4. 是否接受“同 Tick 暂存、成功后提交、失败丢弃”的初步事务方向，作为后续实现契约的基础？

**推荐裁决：** 批准开展 GAP-29 的独立实施契约设计，采用方案 A；同时仅评估完成多参数签名所必需的 Registry / Catalog 变更。当前文件本身不构成上述批准。实施范围、精确 ABI、配额数值与最终事务语义仍须在单独实施契约中得到明确批准。

## 9. 明确保持不变的边界

- P4 SAVE-001～012 已批准的实现与验收结论保持不变。
- P4 B-3 当时“不实现 guest 持久化读写 API”的历史决定保持原样；本提案是针对候选后续工作的建议，不追溯改写 B-3。
- `Mod Platform Specification v1.0.md`、`Save & Compatibility Specification v1.0.md`、Manifest、API Catalog 与 Runtime 当前均不变。
- 不启动 P5 联机、P6 Launcher/SDK、P7 安全/性能/社区试用，不改变任何游戏玩法或产品体验。
