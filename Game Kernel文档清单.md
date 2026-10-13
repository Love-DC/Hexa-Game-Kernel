# Game Kernel 文档清单

- 文档类型：项目文档索引
- 版本：1.0
- 状态：规范文件已补齐；仍需持续核对内容、状态与仓库实现
- 用途：帮助项目负责人和 AI 快速找到权威规范、开发流程与规划文件

> 本清单是导航索引，不替代任何规范正文。当前列出的规范文件已在根目录；文件存在不等于其要求已实现或验证。具体阶段状态以路线图和任务卡为准。

## 1. 根目录文件

| 文件名 | 用途 | 建议状态 |
|---|---|---|
| `Game Kernel — 完整项目规范 v1.0.md` | 项目总规范；定义项目目标、架构范围、核心原则和最终验收要求 | 权威总规范 |
| `Kernel Contract v1.0.md` | 定义 Kernel 的 Tick、World、Command、System、Event、确定性、错误处理与快照/恢复契约 | 核心专项规范 |
| `Project Architecture v1.0.md` | 定义工程分层、模块职责、依赖方向与目录架构 | 架构专项规范 |
| `Mod Platform Specification v1.0.md` | 定义 Mod 生命周期、依赖解析、运行时、API Registry、权限与资源限制 | 平台专项规范 |
| `Save & Compatibility Specification v1.0.md` | 定义存档元数据、数据所有权、兼容性判断、迁移、备份与恢复 | 平台专项规范 |
| `Multiplayer Specification v1.0.md` | 定义联机权威模型、输入排序、同步、断线恢复和 Mod 联机资格 | 平台专项规范 |
| `Security, Performance & Diagnostics Specification v1.0.md` | 定义安全边界、资源预算、性能测试与诊断报告 | 横向专项规范 |
| `Game Kernel — AI Engineering Baseline v1.0.md` | 定义 AI 开发流程、工具链、目录规则、任务卡、测试、Git 与验收纪律 | AI 工程规范 |
| `README.md` | 项目入口说明、快速开始与文档导航 | 项目说明 |
| `rust-toolchain` 或 `rust-toolchain.toml` | 声明 Rust 工具链版本；实际仓库应统一采用并保留一种约定 | 工具链配置 |

## 2. planning/ 规划文件

以下文件名来自现有规划记录，写入本清单前应核对仓库内的实际路径和名称。

| 文件名 | 用途 | 建议处理 |
|---|---|---|
| `planning/03-分阶段开发路线图.md` | 分阶段开发路线与阶段验收安排 | 保留；与总规范第 19 节对照 |
| `planning/06-AI开发与验收流程.md` | AI 开发和验收工作流程 | 保留或合并重复内容；以本清单所列 AI 工程基线作为统一入口 |

## 3. 文档优先级与冲突处理

发生内容冲突时，按以下原则处理：

1. 用户明确确认的产品目标与约束。
2. 已正式冻结的项目契约。
3. 已批准的专项规范。
4. 已批准的实施计划与任务卡。
5. AI 的临时技术建议。

`Game Kernel — 完整项目规范 v1.0.md` 是项目总规范。专项规范用于细化各自领域，不得静默改变总规范已确认的产品目标。若发现冲突，应记录冲突位置、影响范围和建议解决方案，不得自行掩盖。

## 4. 建议目录结构

```text
game-kernel/
├── README.md
├── AGENTS.md
├── rust-toolchain                 # 或 rust-toolchain.toml，按实际仓库统一
├── Game Kernel — 完整项目规范 v1.0.md
├── Kernel Contract v1.0.md
├── Project Architecture v1.0.md
├── Mod Platform Specification v1.0.md
├── Save & Compatibility Specification v1.0.md
├── Multiplayer Specification v1.0.md
├── Security, Performance & Diagnostics Specification v1.0.md
├── Game Kernel — AI Engineering Baseline v1.0.md
└── planning/
    ├── 03-分阶段开发路线图.md
    └── 06-AI开发与验收流程.md
```

此目录树是建议结构，不代表所有文件已在仓库中实际存在。新增、移动或重命名文件前，应先盘点实际仓库，避免覆盖用户已有内容。

## 5. 首次入库检查清单

- [x] 核对本清单中的每个文件是否真实存在。
- [x] 核对文件名、扩展名和路径是否一致。
- [ ] 确认总规范与专项规范之间没有互相矛盾的规则。
- [ ] 确认 `AGENTS.md`、`planning/06-AI开发与验收流程.md` 与 AI 工程基线之间职责清晰，避免维护三份相互重复的权威流程。
- [ ] 确认各规范中的 `PROPOSED`、`PENDING`、`IMPLEMENTED`、`VERIFIED` 状态没有被混用。
- [ ] 确认规划文件链接到实际存在的规范和任务卡。
- [ ] 将最终文件盘点结果与未解决问题记录下来。

## 6. 维护规则

- 新增正式规范时，更新本清单。
- 规范更名或移动时，更新所有引用路径。
- 废弃规范不得静默删除；应标记为 `DEPRECATED`，注明替代文件。
- 本清单只负责索引，不复制各规范的详细规则。
- 不因追求文档数量而机械拆分文件；每份独立文档都应有明确责任边界。
