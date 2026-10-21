# CLI-only 与 Workflow Skills：阶段索引

状态：**G00–G11 约定本地范围已完成**。

当前行为由 [spec](CLI_ONLY_AND_WORKFLOW_SKILLS_SPEC.md) 与
[MCP 验收契约](CLI_ONLY_MCP_SKILLS_ACCEPTANCE.md) 定义；
[实现摘要](CLI_ONLY_AND_WORKFLOW_SKILLS_IMPLEMENTATION.md) 记录已交付结果，
[G11](CLI_ONLY_AND_WORKFLOW_SKILLS_G11_ACCEPTANCE.md) 保存唯一完整证据映射。

本执行计划已完成。原共同规则、交接表、详细卡片与 12 条可复制 /goal 提示词见
[不可变执行计划](https://github.com/bitrouter/bitrouter/blob/264765f4d6928e8039ebe44ecef4c94c3b5287d4/docs/CLI_ONLY_AND_WORKFLOW_SKILLS_GOALS.md)。
本文保留阶段范围与完成要求，避免已完成的操作提示成为第二份当前待办。

## 使用与验证规则

- 每次变更先核对实际源码、工作区和既有证据；保留无关编辑，复用原实现。
- CLI/host 调用原服务，保留 Thread/Turn/Item、Core、审批、预算、持久化和恢复。
  共享业务先迁出再删 UI；不新增竞争性调度器、执行循环或状态数据库。
- 公共 CLI/defaults/env/wiring 变化同步 CLI reference、分发 skill 与受影响 manifests。
- 遵守 AGENTS.md。fixture 使用 Cargo-built Rust、临时配置与隔离环境，保持
  真实服务、用户凭据、全局 harness 和本地 fixtures 的边界。
- 先运行有意义的定向测试；源码收敛后完成 nextest/all-features、Clippy、fmt，
  补 doctests 和受影响的生成物检查。进程 suites 期间串行安排 Cargo；不改生产
  timeout、放宽权限或跳过失败场景以获得通过。
- 证据写明要求、真实入口、owner、源码、命令与结果；无提交时用内容快照。
  保留关键失败与修正来源，不把 check/help 或旧运行代替当前业务验证。
- 本地通过、hosted CI、真实 provider/adapter、远端互操作与各 OS 资格分别报告。
  goal、提交、PR、合并和发布是不同操作；阅读历史提示词不启动工作。

## G00 — 起点与迁移审计

固定所有执行路径、消费者、C01–C08/T01–T15、协议可达性和依赖图。
产物是 [G00 附件](CLI_ONLY_AND_WORKFLOW_SKILLS_G00_AUDIT.md)，不宣称实现目标行为。

## G01 — 共享非 UI 逻辑

将 ACP permission/prompt/plain/journal/cost 迁到原非 UI 所有者；原消费者与测试保留。
完成后 headless 路径不依赖绘制库，UI 删除仍由 G05 负责。

## G02 — 行式 onboarding 与后台启动

有限 provider/agent/Finish 流程、脚本参数、隐藏输入与 EOF/Ctrl-C；裸 bro 等待
后台 readiness 后返回。重复启动复用实例，已保存配置和运行实例不被静默替换。

## G03 — 当前连接快照

裸 agents 只读实际 owner；区分 live、catalog、历史和错误，不以查询制造连接。

## G04 — 单次编排控制

保留原 native/ACP 的队列、steering、审批、取消、设置、路由、历史和条件恢复。
操作必须保持 caller/boot/Turn/epoch/fence，旧协议在效果前拒绝不支持的操作。

## G05 — 删除第一方 TUI

删除渲染 crate、第一方对话/控制台和旧入口；业务测试迁移后保留，纯展示断言退役。
code/tui/chat/agents attach 提供迁移错误；实际 all-feature/all-target 图无绘制依赖。

## G06 — MCP 配置管理

完整 HTTP/stdio CRUD 共用原校验、锁、CAS 与原子发布；保留原文与引用并脱敏报告。
保存事实和 daemon 生效事实分开，运行时变化按原 reload/restart 所有权处理。

## G07 — 版本化 skills 安装

本地/Git 完整 bundle、明确 scope/ref/选择、不可变版本与 receipt、原子 alias。
更新/卸载保留原版本；脚本不执行，unmanaged/harness 内容不受安装器管理。

## G08 — Managed daemon 宿主

产品 managed CLI 使用原 daemon NativeSession/Core/journal，保留幂等与 ACK/epoch
保护；没有客户端嵌入式回退、未知效果重放或失联 owner 自动接管。

## G09 — MCP Skills 对齐、workflow 选择与 native 激活

先验证 gateway M01–M03，再验证 native M04–M10；规范固定，不能用安装或转发
替代 host 加载。明确来源许可、完整 held entry、逐 skill 同意、按需字节校验、
原 Turn/预算/缓存和实际模型数据，恢复与冷 explain 保持原来源与只读边界。
默认候选为本地库；MCP 来源必须明确获准，不新增本地 origin server。

## G10 — 外部 ACP 交付

Codex/Claude 以独立 peer/parser 验证本地 prompt 指令及 negotiated 能力，复用
原 supervisor/ledger。无法维持来源、权限、预算或 manifest 约束时报告 unsupported。
本地 fixture 不证明真实 vendor、native activation 或受保护 bundle 访问。

## G11 — 集成审计与本地交付

分别运行本地安装和获许可 MCP 的真实 CLI→daemon→任务→pending/answer→
explain→terminal→重连/恢复；模型路由与资源实际交付分别断言。
可选资源 onboarding 复用 G06/G07 事务，覆盖完成/跳过/失败/重试与已保存进度。
全部 A01–A28、M01–M10 与 C/T 保持项取得当前证据，完整 gates 和真实依赖图通过。
交付范围、实际行数、已知限制与外部资格分别记录，不缩小验收清单。

## 后续使用

新增工作明确其变更范围与验收，不把已完成阶段重新标成待办。
历史阶段的详细失败、快照、原提示词和交接切片可从不可变计划与实现记录追溯。
本轮测试/文档精简的复验见 G11 附件第 10 节；它不改变原产品契约。
