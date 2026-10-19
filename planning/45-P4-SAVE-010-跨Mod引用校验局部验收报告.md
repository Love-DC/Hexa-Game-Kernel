# P4-SAVE-010 跨 Mod 引用校验局部验收报告

- **状态：** Save 侧验证合同与 SAVE-007 候选验证组合器已实现；Platform 对真实 Mod 持久化数据的提取接线未验收。
- **依据：** Save §4.3、§7.2–§7.3、§9.2、§15；`planning/22`。
- **变更：** `crates/save-manager/src/references.rs`、`crates/save-manager/src/loader.rs`（仅 crate 内只读布局访问）、`crates/save-manager/src/lib.rs`、`crates/save-manager/tests/references_contract.rs`。

Mod 数据仍是 save-manager 的不透明字节。受信任的每 Mod `ModReferenceExtractor` 只解析该 Mod 自己的 payload，显式输出 Provider、目标类型和稳定 ID。Save 校验器拒绝缺少 extractor/数据分区、身份越权、Provider 缺失、目标缺失及重复导出；不自动重定向引用。`MigrationValidationPipeline` 强制同时注入 World 不变量校验器和引用提取器，作为 SAVE-007 候选验证器，没有默认放行实现。未知/孤立 Mod 分区保留不动。

定向测试通过：有效 Provider/目标、缺失 Provider 与缺失目标区分、候选世界校验、重复 extractor 注册不替换既有注册。save-manager 全套测试和 workspace Clippy 已通过。

**边界：** 当前 Mod 平台尚未提供通用持久化 payload/reference ABI，因此集成 fixture 使用受信任测试 extractor。端到端 Mod 运行时引用清单、实际坏引用故障注入和迁移候选全链验收仍待完成，SAVE-010 未标为阶段验收。
