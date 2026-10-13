# P3 任务卡：Mod Platform 基线

- 阶段：P3
- 状态：P3 执行中；GK-P3-01 已通过本地与 Gitee Go 验收，GK-P3-02 已通过本地门禁与远端 Gitee Go 验收（构建 #11/#13，2026-10-14 确认）
- 依据：`Game Kernel — 完整项目规范 v1.0.md` 第 4、5、8、12、13 章；`Mod Platform Specification v1.0.md`；`Kernel Contract v1.0.md`；`planning/03-分阶段开发路线图.md`；已验收的 P2 Kernel API
- 当前代码基线：`crates/platform-core` 包含严格 JSON Manifest 解析、本地目录发现、必需依赖 SemVer 校验、确定性拓扑解析、内存 LoadPlan、宿主权限授权和 Wasmtime 适配；目前仅有 `gk::counter_read` / `gk::counter_set` 两个 Host API。
- 规范边界：`Mod Platform Specification v1.0.md` 现已作为 P3 的领域规范基线。本卡定义本阶段实际实现的子集；规范中尚未实现的可选依赖、Provider/多候选策略、完整 API Registry、扩展生命周期和运行时资源管理属于后续 P3 工作包或后续阶段，不能仅凭规范存在就标记为已实现。发现与已验收 Kernel 契约冲突时，先登记并对齐再实现。

## 目标与范围

形成能自动校验、确定解析、可复现锁定、可观察生命周期与故障的 Mod Platform MVP。P3 只扩展受控 Wasm 和声明式包的基础契约，不增加权限面或运行时逃逸能力。

### 保守基线

- 保持当前 `manifest_version = 1` 的既有字段与含义：`manifest_version`、`id`、`version`、`kernel_api`、`entrypoint`、`requested_permissions`、`dependencies`。为这份实际支持的子集发布正式 JSON Schema；不把完整项目规范建议字段说成已实现字段。
- 未知 Manifest 字段继续拒绝。新字段或语义变更须提升 Manifest 版本并提供兼容决定；不因补 Schema 改变现有示例包。
- P3 当前实现只支持必需的本地直接 Mod 依赖和 SemVer 约束；缺失、重复、冲突、版本不符或循环均拒绝整份加载计划。可选依赖虽已在领域规范定义，但当前实现不支持；Mod 间公开 API 依赖和 Game Definition 约束也不纳入当前 Resolver。
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
- **测试：** 同 ID 多候选明确拒绝；重复/缺失依赖、版本不匹配、环、自依赖、候选输入顺序置换及重复运行。
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

## 执行记录

### GK-P3-01 — Manifest v1 Schema 与诊断

- **状态：** 本地与 Gitee Go 验收通过。
- **实现：** 新增唯一规范 Schema `schemas/mod-manifest-v1.schema.json`，Platform 加载 Manifest 时先执行 Schema 校验，再执行 Rust 语义校验。错误使用稳定 `platform.invalid_manifest` 码，Schema 错误包含 JSON Pointer 字段路径；内置 Schema 加载失败使用 `platform.invalid_schema`。
- **对齐：** 保持 P1 现有字段、默认空依赖、未知字段拒绝和两个既有权限；示例 Schema 与规范 Schema 通过测试保持逐字节一致。语义校验覆盖 SemVer、依赖范围和跨平台入口路径约束。
- **测试：** Schema Draft 2020-12 元校验、示例包通过、未知字段和重复权限拒绝、ID/SemVer/权限/依赖范围非法拒绝、绝对路径/遍历/反斜线/无效扩展拒绝。
- **本地门禁：** `cargo fmt --all -- --check`、`cargo clippy --locked --workspace --all-targets -- -D warnings`、`cargo test --locked --workspace`、`cargo run --locked -p game-platform -- demo` 均通过。Platform 12 项、Kernel 14 项、集成 3 项测试通过；demo 输出 `verified=true`。
- **远端门禁：** 用户确认提交 `b176d33` 和后续提交 `a872a1c` 的 Gitee Go 构建均成功；新 `jsonschema` 锁定依赖在远端构建通过。Manifest 其他字段保持未支持，不由此 Schema 推断。

## 本阶段明确不做

商店下载/更新、签名信任根、任意原生 Mod、WASI、网络和文件 API、Mod 间直接调用、热重载、权限热撤销、完整 SDK/Launcher UI，以及未经测量的 Mod 数量或性能承诺。分发签名和信任政策在实施发布仓库前另行审议。

## 阶段完成定义

P3 只有在 P3-01 至 P3-07 的契约和验收逐项通过后才完成。仅有 Manifest struct、内存 LoadPlan 或 Wasm 通过演示，不算正式 Schema、持久锁文件或生命周期系统。

### GK-P3-01 — 远端验收

- **状态：** 已通过 Gitee Go（用户确认 `b176d33` 与 `a872a1c` 均构建成功）。
- **结论：** `jsonschema` 锁定依赖在 Gitee Go 环境可用；GK-P3-01 远端门禁通过。`a872a1c` 同时验证了 Resolver 诊断排序增量的云端编译。

### GK-P3-02 — Resolver 与依赖契约（本地门禁与远端 Gitee Go 均已通过）

- **已冻结的既有行为：** 每个 Mod ID 只接受一个候选包；重复 ID 拒绝，不进行版本挑选；必需直接依赖必须存在且满足 SemVer；依赖必须先于依赖者加载；同一拓扑层按 Mod ID 字典序稳定排序；循环（包含自依赖）拒绝整份计划。
- **实现进展：** Resolver 校验 Manifest 依赖时按依赖 ID、版本范围排序，使多个错误同时存在时选出的诊断不依赖 JSON 数组顺序；LoadPlan 中依赖 ID 已排序。提交 `a872a1c` 已通过本地 `cargo check --locked --workspace` 与 Gitee Go。
- **本轮实现：** 依赖解析失败从诊断字符串改为结构化载荷。新增 `DependencyResolutionCode`（`platform.dependency.duplicate_mod_id` / `duplicate_dependency` / `missing_dependency` / `invalid_version_requirement` / `version_incompatible` / `cycle`）与 `DependencyResolutionFailure`（`code`、`mod_id`、`dependency_id`、`requirement`、`found_version`、确定性 `cycle` 路径、仅诊断用的 `message`）。顶层 `PlatformErrorCode::DependencyResolution`（`platform.dependency_resolution`）保持不变；载荷装箱以控制错误变体尺寸。新增 `find_dependency_cycle`：仅在未解析集合内向最小未解析依赖前进，输出与候选输入顺序无关的完整循环路径（自依赖输出 `X -> X`）。重复 Mod ID 明确拒绝且不替调用方挑选候选。
- **测试：** 单元测试覆盖重复 Mod ID（含候选顺序互换后诊断一致）、重复依赖、缺失依赖、SemVer 不匹配（含 requirement/found 字段）、非法版本范围、自依赖、多节点循环、含环子图不掩盖可排序子图、菱形图 24 种候选排列产生同一计划、重复运行一致、故障载荷序列化字段稳定；集成测试 `tests/resolver_contract.rs` 用 `tests/fixtures/dep-graph`（真实磁盘包）验证锁定顺序、旋转/反转候选顺序同计划，用 `tests/fixtures/dep-graph-broken` 验证缺失必需依赖导致整计划失败并携带关联标识。
- **本地门禁：** `cargo fmt --all -- --check`、`cargo clippy --locked --workspace --all-targets -- -D warnings`、`cargo test --locked --workspace`（Platform 单元 23 项、集成 `p1_vertical_slice` 3 项、集成 `resolver_contract` 3 项、Kernel 14 项，全部通过）、`cargo run --locked -p game-platform -- demo`（`verified=true`）均通过。
- **远端门禁（2026-10-14 证据更新，P3-10 文档维护）：** **已确认通过**。最终增量提交 `6e7c7cf`（`Structure dependency resolution failures`，2026-10-13 09:35:18 推送）对应 Gitee Go 构建 **#13**（流水线 `game-kernel-rust-ci`，分支 `main`，Webhook 触发），于 **2026-10-13 09:35:30 (+08:00) 成功**，用户已明确确认。前置增量 `a872a1c` 此前已由构建 **#11** 确认成功（见 GK-P3-01 远端验收）。~~**远端门禁：** 尚未确认。需用户确认本次提交的 Gitee Go 构建。~~（2026-10-13 撰写时的历史记录，已被构建 #13 证据取代。）
- **未决边界：** 多版本 side-by-side、可选依赖、Mod 间公开 API 依赖和 Provider 选择仍不支持；`DependencyResolutionFailure` 的持久报告 Schema 属 GK-P3-06。

### 新规范对齐说明

- **状态：** `Mod Platform Specification v1.0.md` 已加入仓库，作为目标契约基线；其要求不自动代表当前实现已达标。
- **当前 P3 子集：** 依次按 GK-P3-01 至 GK-P3-07 验收。尚未进入的完整 Provider 选择、可选依赖、资源目录与运行时热变更能力不得提前纳入本次 Resolver 行为。
- **依赖关系：** Kernel 命令与 Tick 语义以已验收的 `Kernel Contract v1.0.md` / P2 报告为准；如领域规范内容不一致，先协调文档后再扩展代码。

### GK-P3-04 — API Registry / Catalog 最小目录（本地门禁与远端 Gitee Go 均已通过）

- **状态（2026-10-14 证据更新，GK-P3-20 文档维护）：** 已提交为 `2b3c71a`（`Add host API registry and catalog`）并推送；本地门禁与远端 Gitee Go 均已通过（构建 **#15**，2026-10-14 00:27:49 成功，用户确认；原 P3-04 记录冲突已由 #15 证据关闭，见 `planning/18` §七/§八）。~~本地实现与全部门禁通过；等待用户验收，尚未提交。~~（2026-10-13 撰写时的历史记录）
- **范围（未扩大）：** 仅登记当前真正实现的 2 个宿主 API；不新增 Host API、不扩展权限面、不改动 Kernel/Contract/Manifest Schema/锁文件格式。
- **单一事实源：** `crates/platform-core/src/lib.rs` 新增 `HOST_APIS: &[HostApiDescriptor]`，为唯一注册表。`HostApiDescriptor` 记录稳定 ID、Wasm 导入模块/字段、`api_version`、provider、guest 签名（参数/返回值类型）、所需权限、调用阶段、确定性、失败分类与简要说明。`compile()` 的导入校验、`validate_manifest` 的允许权限集合、以及导出的 Catalog 三者均从该注册表派生；`new_linker` 的显式 `func_wrap` 注册由契约测试与注册表逐一比对（见下）。
- **签名校验：** 新增 `HostApiSignature` / `HostApiValueList`（参数与返回值类型以 `WasmValueKind` 表达）。`compile()` 在注册/执行前比对注册表签名与模块导入的实际 `FuncType`，不符即拒绝（新稳定码 `platform.api_signature_mismatch`）。未知导入仍以 `platform.wasm_module` 拒绝，未申请权限以 `platform.permission_not_requested` 拒绝——均在 Mod 逻辑执行前。
- **版本校验语义：** `api_version` 不是只导出不验证：`ApiCatalog::verify()` 拒绝不支持的 `catalog_version`（`platform.api_catalog.unknown_version`）与任何非 `HOST_API_VERSION` 的 `api_version`（`platform.api_catalog.unsupported_api_version`）；`validate_registry()` 对源注册表做同样的自洽校验，任何不一致都会使测试失败。
- **Catalog 导出：** `api_catalog() -> ApiCatalog` + 确定性 `to_bytes()`（pretty JSON + 末尾换行，条目按 API ID 排序）；不自动写盘。新增 Schema `schemas/mod-api-catalog-v1.schema.json`（Draft 2020-12，`additionalProperties:false`，导出字段/排序/签名表达一致）。
- **错误码：** 优先复用现有码；仅新增必要的最小集合——`platform.api_signature_mismatch`（签名不符需可诊断）与 `platform.api_catalog`（`ApiCatalogFailure` 载荷，装箱）。其余沿用既有码。
- **测试：** 新增 `crates/platform-core/tests/api_registry_contract.rs` 13 项：Catalog 与**真实 Wasmtime Linker** 注册集合逐一相等（经 `linker_registration_probe()` 枚举 `Linker::iter`，非手工键列表）、签名与真实注册签名一致、未知导入拒绝、签名不符拒绝（`ApiSignatureMismatch`）、未申请权限拒绝（`PermissionNotRequested`）、不支持 catalog/api 版本拒绝、Catalog 权限集合与实际校验一致、排序与序列化字节稳定、导出结果通过 Schema 且 Schema 能拒绝错型输入、Catalog 仅含两个已实现 API、注册表自洽。`lib.rs` 单元新增 4 项。
- **本地门禁（均实跑通过）：** `cargo fmt --all -- --check`；`cargo clippy --locked --workspace --all-targets -- -D warnings`（退出 0，仅 Windows 增量锁文件环境警告）；`cargo test --locked --workspace`（Platform 单元 29、`api_registry_contract` 13、`lockfile_contract` 17、`p1_vertical_slice` 3、`resolver_contract` 3、Kernel 14，共 79 项 0 失败）；`cargo run --locked -p game-platform -- demo`（`verified=true`）。
- **未决边界：** 仅覆盖宿主内置 API（provider = `game-platform`）；Mod 提供/消费 API 的注册与关联（§8.2 关联、§8.3 异步与事件）不在本任务；§9.2 中尚未实现的权限类别不得出现在 Catalog。
- **远端门禁：** **已通过**——构建 **#15**（流水线 `game-kernel-rust-ci`，分支 `main`）对应 `2b3c71a`，2026-10-14 00:27:49 (+08:00) 成功（用户经 Web 控制台确认）。~~尚未推送，未验证。~~
