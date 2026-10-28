# CLI-only 与 skills 目录：本轮验收

状态：实现完成；本地逐项 PASS 覆盖完整，最终全量和 doctests 被 macOS 启动阻塞。基线：`c122b437e1ceae64b07151cf8577d0af0214a0bd`。

## 原 G11 证据边界

原 61 项 A/M/C/T、149 个唯一测试及 4368 passed / 22 skipped 对应当时含安装器、
版本库与自动选择的源码，不是本轮结果。完整保留于
[不可变 G11 验收](https://github.com/bitrouter/bitrouter/blob/c122b437e1ceae64b07151cf8577d0af0214a0bd/docs/CLI_ONLY_AND_WORKFLOW_SKILLS_G11_ACCEPTANCE.md)。
本轮按 [最新 spec](CLI_ONLY_AND_WORKFLOW_SKILLS_SPEC.md) 核验；旧用例退役不改为
ignored，也不冒充仍通过。

## 本轮范围

保留 native 无界面执行引擎与 SDK 模型路由；删除 BRO skills acquisition/installer、
Git/ref/alias、版本库存储，以及 native/ACP 自动 skills 和 native 自动上下文决策。
CLI 与运行时直接使用现有本地目录；MCP 的来源、批准与完整性验证保留。

## Required 本地证据

| 边界 | 验证入口 |
| --- | --- |
| 目录发现、读取、原子写入，无安装索引 | `tests/skills_lifecycle.rs` |
| 同名、非法 root 写入、越界与 symlink | `tests/skills_lifecycle.rs` |
| 原 caller/manifest、修改字节拒绝 | `skills/native.rs` 单元回归与 Core native_skills |
| Setup 仅 MCP、保存进度、TTY 取消 | onboarding / onboarding_pty / mcp_config_pty |
| 显式目录 → native 模型、工具审批、支持文件与 cold explain | managed_daemon/skills 与 integration |
| MCP 来源/metadata-only consent、拒绝/撤销/stale/child/cache/URI | Core native_skills、mcp_skill_host、managed_daemon |
| 原生配置 backend 后零语义请求，完整上下文、policy 默认与原 learn 证据保留 | agent/tests_native、core_execution/context_decisions、orchestrator_core |
| ACP 显式本地载体、原 ledger、审批/取消/超限/错误/冷读；零自动决策 | management_cli/skill_delivery、acp_skill_delivery |
| SDK 路由独立评估保持 | SDK language_model/tests_unified_routing 与全量 SDK tests |
| 原执行、持久化、容量和恢复 | 全工作区 nextest |

## 本轮最终验证

最终 Rust/Cargo 源码摘要：`71ca157a932ee8f9e1097fe93dc3089996804c00906ffe7e83d22af3e8394936`。
严格 Clippy、fmt、已有 Cargo-built dist-helper、metadata、依赖树与最终二进制
测试发现通过。完整复跑和 doctests 未通过本轮本地门禁，不能写成八项全绿。

| 检查 | 实际结果 |
| --- | --- |
| 最终全工作区 / all-features nextest | 4202 passed；3 个 fixture 启动超时、4 个同类运行中用例随后中断，123 项未重跑；22 skipped |
| 全工作区 / all-features / all-targets Clippy | `-D warnings` 通过 |
| doctests | 未完成：ring build script 同样停在程序入口，已停止该构建；不沿用旧 6 passed / 1 ignored |
| fmt 与 dist-helper check | 通过；后者使用已有 Cargo-built executable，配置 schema 与 registry 均匹配 |
| locked/offline metadata、全目标依赖树 | 629 resolved nodes / 7 workspace members；无 bitrouter-tui、Ratatui、Crossterm |
| 最终二进制 discovery | 对已有最终构建逐个执行 --list / --ignored，4354 项；原 ignored 集合完全相同 |
| SDK 独立模型路由 | 3 项 unified-routing 用例实际 PASS；保留 decision assessment |

完整日志、源码快照、测试发现与逐项 PASS 对照保存在本地
`target/skills-convergence/`，不随 PR 分发。文档审计另核验本地链接、
anchors、skill 行数与三个 plugin manifests 的描述/wiring；运行证据不等于真实服务资格。

首轮完整执行为 4329 passed / 3 failed / 22 skipped。三个失败对应旧自动分类、旧
版本路径、旧隐式 MCP 选择预期；修正后定向三项全部通过。两轮之间只修改三个
集成测试文件，生产 Rust/Cargo 未变化；这三个二进制的全部当前用例又在最终复跑
通过。按原完整运行、修正定向和最终复跑组合，4354 项发现中的 4332 个 active
名称都有实际 PASS，22 个 ignored 不变。该逐项覆盖不等于一次最终全量 exit 0。

最终阻塞时，fixture 的请求日志为空，进程采样停在 macOS dyld 入口，未进入
fixture main；不把它记作协议断言失败或产品通过。CI 单独验证同一提交；Linux
新增 cli-orchestration 分片，覆盖此前未列入该平台的六个 CLI 集成目标。

## 用例退役与替换

相对 `c122b437` 的 4390 个名称，本轮删除/改名
57 个、增加/替换 21 个，
合计 4354 个。名称变化只发生在下列已移除行为或对应替代路径；
旧 installer/自动调用期望不能用“仍通过”表述。

| 原名称所属范围 | 删除或改名数 | 当前边界与替代证据 |
| --- | --- | --- |
| 安装器与版本库 | 22 | 退役 Git/ref/alias/版本保留；实际目录 CLI、manifest/字节与独立目录错误回归 |
| 自动上下文选择与完整视图替代 | 11 | 退役语义请求/付费 intent 调度；完整上下文、显式 recall/派生与 serving model 检查 |
| ACP 目录交付与零自动调用 | 4 | 两个独立 adapter 的载体/账本/取消/超限保留；配置 backend 后零自动调用 |
| 自动 skills 选择与显式 include 替代 | 10 | 退役可选自动选择；显式包括/排除、未选来源零加载与必需项数量预算 |
| Native 目录与明确批准 | 4 | 目录进入模型、支持文件与 cold evidence；必需 MCP 损坏在模型前失败 |
| 安装型 onboarding | 6 | 退役技能安装菜单/flags；保留 MCP 部分失败、进度、重试与 TTY 取消 |

保留的用例与 ignored 标志逐项对比；新行为不靠放宽原审批、manifest、MCP 来源、
文件字节、执行/恢复或容量边界通过。恢复及 cold explain 另拒绝没有显式 include
的已选来源。新目录无磁盘快照：修改/移除原文件可使旧 Turn 失败；旧安装 ID 和
`managed` 安装来源不自动迁移为目录绑定。历史数据不删除。

定向迁移时发现并补上 `threads create --skill/--exclude-skill` 的 CLI 入口；
旧自动调用/可选拒绝断言改为显式必需项或零调用断言。中间失败、编译取消与后续修复
不计为最终通过；上表分别报告完整执行、修正回归、逐项覆盖与平台阻塞。

## 外部资格

本地 macOS fixtures 不证明真实 provider 语义质量、Codex/Claude vendor 会话、远端
MCP/host/Git auth 或 Linux/Windows 执行。MCP-origin 外部 ACP 交付仍 unsupported。
Host CI 单独看 [PR #989 checks](https://github.com/bitrouter/bitrouter/pull/989/checks)。
本轮不合并、发布或部署。
