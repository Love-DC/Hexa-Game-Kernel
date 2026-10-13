# GK-P3-04 执行记录：API Registry / Catalog 最小目录

- 阶段：P3
- 任务：GK-P3-04 — API Registry / Catalog 最小目录
- 状态：本地实现与门禁完成；提交前针对性核对通过；待用户验收后提交
- 规范依据：`Mod Platform Specification v1.0.md` §8（8.1 标识、8.2 注册、8.3 调用、8.4 目录）与 §9.2 权限分类；`planning/09-P3-Mod平台基线任务卡.md` 中 GK-P3-04 条目
- 代码基线：HEAD `51c424e`（`Add mod lockfile contract`）

## 一、目标

建立宿主内置 Host API 的统一 Registry，并导出机器可读的 API Catalog。使编译期导入校验、Manifest 权限白名单与导出的目录三者基于**同一份 API 描述数据**，从而消除原先散落在多处的手写映射，且实际 Wasm Linker 注册与 Registry 保持一一对应。

## 二、实现内容

实现文件：`crates/platform-core/src/lib.rs`。

- **单一事实源：** 新增 `HOST_APIS: &[HostApiDescriptor]`，为唯一注册表。`HostApiDescriptor` 记录稳定 ID、Wasm 导入模块/字段、`api_version`、provider、guest 签名（参数/返回值类型）、所需权限、调用阶段、确定性、失败分类与简要说明。
- **三处派生：**
  - `compile()` 的导入校验改用 `find_host_api()`，并在注册/执行前比对注册表签名与模块导入的实际 `FuncType`；
  - `validate_manifest` 的允许权限集合改用 `all_host_api_permissions()`（由 `HOST_APIS` 聚合）；
  - `api_catalog()` 由注册表生成（条目按 API ID 排序）。
- **签名模型：** 新增 `HostApiSignature` / `HostApiValueList`（定长 0/1 参数与返回值列表，以 `WasmValueKind` 表达类型），并将类型映射自 `wasmtime::ValType`。
- **版本校验语义：** `api_version` 具备真实校验语义。`ApiCatalog::verify()` 拒绝不支持的 `catalog_version`（`platform.api_catalog.unknown_version`）与任何非 `HOST_API_VERSION` 的 `api_version`（`platform.api_catalog.unsupported_api_version`）；`validate_registry()` / `validate_api_catalog_value()` 对源注册表与导出值做同样的自洽校验。
- **目录导出：** 新增 `api_catalog() -> ApiCatalog` 与确定性 `to_bytes()`（pretty JSON + 末尾换行）；本任务不自动写盘。
- **错误码：** 优先复用现有码，仅新增最小必要集合——`platform.api_signature_mismatch`（`ApiSignatureMismatch`，携带 mod_id/api_id/expected/found，签名不符需可诊断）与 `platform.api_catalog`（`ApiCatalog`，`ApiCatalogFailure` 载荷装箱）。未知导入沿用 `platform.wasm_module`，未申请权限沿用 `platform.permission_not_requested`——两者均在 Mod 逻辑执行前拒绝。
- **Linker 同源校验：** 新增测试钩子 `linker_registration_probe()`，通过 `new_linker()` 与 `Linker::iter` 枚举**真实注册项**（Wasmtime 48.0.5 支持），供契约测试断言「Registry 集合 == 真实 Linker 集合」。`new_linker` 保持显式 `func_wrap`（闭包类型各异），由测试强制同源，未引入动态注册框架。
- **交付文件：**
  - `crates/platform-core/tests/api_registry_contract.rs`（13 项契约测试）
  - `schemas/mod-api-catalog-v1.schema.json`（Draft 2020-12；`additionalProperties:false`；`module::field` 模式；params/results `maxItems:1`；phase/determinism/failure 枚举）

## 三、测试与门禁

### 3.1 新增测试

- 契约测试 `crates/platform-core/tests/api_registry_contract.rs` 13 项：Catalog 与**真实 Wasmtime Linker** 注册集合逐一相等（经 `linker_registration_probe()` 枚举 `Linker::iter`，非手工键列表）、签名与真实注册签名一致、未知导入拒绝、签名不符拒绝（`ApiSignatureMismatch`）、未申请权限拒绝（`PermissionNotRequested`）、不支持 catalog/api 版本拒绝、Catalog 权限集合与实际校验一致、排序与序列化字节稳定、导出结果通过 Schema 且 Schema 能拒绝错型输入、Catalog 仅含两个已实现 API、注册表自洽。
- `lib.rs` 单元新增 4 项：`registry_is_internally_consistent`、`api_catalog_is_sorted_stable_and_verifies`、`api_catalog_only_exports_implemented_apis`、`api_catalog_rejects_unsupported_version`。

### 3.2 本地门禁结果（均实跑通过）

| 检查 | 结果 |
|---|---|
| `cargo fmt --all -- --check` | 通过 |
| `cargo clippy --locked --workspace --all-targets -- -D warnings` | 退出 0（仅 Windows 增量锁文件 `os error 5` 环境警告，非代码问题） |
| `cargo test --locked --workspace` | **79 项 0 失败**（Platform 单元 29、`api_registry_contract` 13、`lockfile_contract` 17、`p1_vertical_slice` 3、`resolver_contract` 3、Kernel 14） |
| `cargo run --locked -p game-platform -- demo` | `verified=true` |

基线对照：GK-P3-04 前为 62 项，本次新增 17 项（13 契约 + 4 单元）。

### 3.3 提交前针对性核对（只读 + 对抗性探针）

以临时对抗性探针（9 项，全部通过后已删除）逐项确认：

1. **Registry 与真实 Linker 注册**：探针枚举的是真实 `new_linker()` 注册项（实测 `gk::counter_read () -> (i64)`、`gk::counter_set (i64) -> ()`），额外/缺失/模块名/字段名/签名不一致均可被同源断言检出。
2. **Wasm 签名验证**：参数数量、参数类型、返回值数量、返回值类型全部逐项验证；多返回值、错误参数类型、错误参数数量、非函数导入均在 Mod 逻辑执行前被 `ApiSignatureMismatch` 拒绝。
3. **API 版本语义**：`catalog_version` 与逐条 `api_version` 分离校验；反序列化未知版本仍被 `verify()` 拒绝；`api_catalog()` 产物恒满足自身版本规则。
4. **权限一致性**：`compile()`、`validate_manifest`、Catalog 均派生自同一 `HOST_APIS`，无独立硬编码分叉。
5. **Schema 与序列化**：Schema 完整约束必需字段/类型/枚举/未知字段，能拒绝错型输入；`to_bytes()` 按 API ID 排序 + 末尾换行，字节稳定。
6. **错误处理与兼容性**：与 HEAD 逐字节比对确认已有错误码字符串与语义未变；仅新增上述两个码；未新增 Host API 或权限。
7. **工程与回归**：无重复实现/接口泄漏；79 项测试实测通过。

## 四、验收结论

- **本地技术验收：通过。** 7 项核对全部为「通过」，**无阻塞项、无必须修复项**。
- **范围：未扩大。** 仅登记当前真正实现的 2 个宿主 API（provider = `game-platform`）；不新增 Host API、不扩展权限面、不修改 Kernel 核心、Kernel Contract、Mod Manifest Schema 或锁文件格式。
- **未决边界：** 仅覆盖宿主内置 API；Mod 提供/消费 API 的注册与关联（§8.2 关联、§8.3 异步与事件）不在本任务；§9.2 中尚未实现的权限类别不得出现在 Catalog。
- **远端门禁：** 尚未推送，未验证。

## 五、提交范围

本次提交仅包含以下 4 个文件：

- `crates/platform-core/src/lib.rs`
- `crates/platform-core/tests/api_registry_contract.rs`
- `schemas/mod-api-catalog-v1.schema.json`
- `planning/10-P3-04-API-Registry执行记录.md`（本文件）

`planning/09-P3-Mod平台基线任务卡.md` 的既有未提交改动与本任务无关，不在本次提交范围内，保持工作区原样。
