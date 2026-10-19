# P4-SAVE-009 Mod 变更策略局部验收报告

- **状态：** 策略规划器与合同测试已实现；端到端变更编排未验收。
- **依据：** Save §6–§7、§15；`planning/22`。
- **变更：** `crates/save-manager/src/mod_changes.rs`、`crates/save-manager/src/lib.rs`、`crates/save-manager/tests/mod_changes_contract.rs`。

规划器给出 direct / requires migration / blocked 三类决定。纯扩展只有在兼容结论通过且初始化不触碰既有状态时允许直接继续；Schema 改变必须走迁移；未确认的世界语义变化阻止加入；移除 Mod 时保留其持久化分区并在存在依赖/引用时阻止；Mod 替换要求显式声明、迁移及引用映射。

定向合同测试覆盖纯扩展、初始化、迁移门禁、语义确认、移除数据保留/依赖/引用/永久删除阻止、替换映射要求。工作区 Clippy 与 save-manager 全套合同测试实际通过。

**边界：** 本模块只生成决策，不改档案；真实 Mod 安装/移除/替换执行、运行时授权和平台流程仍须调用 SAVE-005/007/008/010 组装，不能用规划结果直接视为迁移已执行。最终 SAVE-009 验收待 P4 集成门禁。
