# CLI-only 与 Workflow Skills：实现记录

状态：**G00–G11 全部本地完成；外部资格另列**。更新：2026-10-20。

[Spec](CLI_ONLY_AND_WORKFLOW_SKILLS_SPEC.md) 定义目标，
[Goal 执行计划](CLI_ONLY_AND_WORKFLOW_SKILLS_GOALS.md) 定义阶段。
本文只记录已经执行的工作与证据，不是另一份执行队列，也不代替 Codex Goal 状态。
G04 的非 UI 会话/队列/审批控制与 G05 的 TUI/旧入口删除已完成本地实现与验收。
G06 的 MCP 配置 CRUD 与 G07 版本化 skills 安装已完成本地实现与验收；
G08 的 task managed daemon 宿主迁移与 G09 native workflow skills 已完成本地验收；
G10 的外部 local prompt 交付与 G11 跨阶段集成已完成本地验收。
v0.5 的 A27 与 A28 native 切片按独立 M01–M10 验证，包含
明确来源、完整 entry/逐文件校验、选择/批准、实际模型上下文、预算和原缓存
恢复/冷 explain。第 14–16 节及 [G11 验收附件](CLI_ONLY_AND_WORKFLOW_SKILLS_G11_ACCEPTANCE.md) 记录最终源码、逐项验收和完整 gates。真实外部 adapter、
真实 provider/上游 MCP、hosted CI 和跨平台资格不由本地结果推定通过。

## 1. 起点与范围

- 源码基线：`54c822a34d4dbfd75d484ca35e108b497048efef`。
- 起点已有工作区文件：本 spec、docs 索引及本轮新增的执行计划/实现记录。
  后续阶段先核对实际差异，不能把这些未提交文档作为无关文件清理。
- G00 结束时完成设计整理、源码/依赖审计和文档检查，当时没有运行代码改动、
  构建或功能验收。G01 的源码变更与当前验证状态另记于第 6 节。
- 静态统计：790 个 tracked Rust 文件、468,581 物理行，含测试/注释/空行。
  TUI crate 为 23,354 行，其中非 cfg(test) 15,453、内联测试 7,901。
  G00 重新统计确认 tracked Rust 文件/总物理行和 TUI 总物理行与此一致；
  cfg(test) 拆分仍为前次估算。这些数字不作为删除目标或验收门槛。

## 2. 阶段状态

`not_started` / `in_progress` / `complete` 仅描述阶段交付。必须按对应卡片取得
required evidence 后才能写 complete；实现但未验证的内容写在阶段记录内。

| 阶段 | 依赖/默认顺序 | 状态 | revision / evidence |
| --- | --- | --- | --- |
| G00 | 无 | complete | HEAD `54c822a34d4dbfd75d484ca35e108b497048efef`；[审计附件](CLI_ONLY_AND_WORKFLOW_SKILLS_G00_AUDIT.md) 与第 5 节记录；仅 docs 本地变更 |
| G01 | G00 | complete | 同一 HEAD 的本地 diff；第 6 节列出迁移、完整 gates 与边界审计；旧 UI 保留 |
| G02 | G01 | complete | 保留 G00/G01 diff；行式 onboarding、后台 ensure-start、取消/非 TTY/输出/来源保护通过；最终 4487 passed，见第 7 节 |
| G03 | G02 | complete | 同一 HEAD 的本地 diff；四类 owner 的只读 liveness 投影、daemon 查询、裸 agents 分派；4494 passed，A25 与 A01 裸 agents 部分通过，见第 8 节 |
| G04 | G03 | complete | 同一 HEAD 的本地 diff，保留 G00–G03；native / ACP 控制、版本协商、非 UI 业务迁移及本阶段 A18/A21/A23/A26 通过；最终 4508 passed，见第 9 节 |
| G05 | G01–G04 | complete | 同一 HEAD 的本地 diff，保留 G00–G04；UI/旧入口与绘制依赖删除，业务测试迁移，最终 4195 passed / 22 skipped；依赖、兼容和分发证据见第 10 节 |
| G06 | G05 | complete | 同一 HEAD 的本地 diff，保留 G00–G05；MCP CRUD、事务、脱敏与 saved/running 边界通过；最终 4210 passed / 22 skipped，见第 11 节 |
| G07 | G06 | complete | 同一 HEAD，保留 G00–G06；本地/Git 完整 bundle、事务、scope、无执行与版本保留；最终 4224 passed / 22 skipped，832 路径源码无漂移，见第 12 节 |
| G08 | G07 | complete | 同一 HEAD，保留 G00–G07/v0.5；原 NativeSession/journal 由 daemon 托管；4235 passed / 22 skipped、测试 helper 整理后 11 项复验、最终静态/文档/范围审计通过，见第 13 节 |
| G09 | G07–G08 | complete | 保留 G00–G08/v0.5 全部差异；M01–M10 gateway/native、workflow 选择、独立批准、根/支持文件实际交付、预算与原绑定恢复/冷 explain，本地完整 gates 通过；最终源码与逐项证据见第 14 节末；外部交付与集成分别见 G10/G11 |
| G10 | G09 | complete | Codex/Claude 独立本地 prompt 载体、能力/unsupported、原 journal/决策/权限/取消边界通过；全量 4359 passed / 22 skipped；测试 helper 清理后 29 passed 与 Clippy/fmt 再验通过，见第 15 节；最终集成见 G11 |
| G11 | G00–G10 | complete | G10 起点零漂移；onboarding 复用原资源事务，两条 native 全链与 Codex/Claude 本地 ACP 通过；A28/M10/C8/T15 的当前 149 个证据测试全部 PASS；全量 4368 passed / 22 skipped，8 项最终 gates 与依赖/CLI/文档/物理行审计完成，见第 16 节与 G11 附件；不推送/发布 |

## 3. G00 已审计能力矩阵

详细源码链接、真实函数、全部入口/消费者类别、执行身份、transport/CLI 缺口和
T01–T15 测试落点见 [G00 审计附件](CLI_ONLY_AND_WORKFLOW_SKILLS_G00_AUDIT.md)。
下表为索引；“源码已核对”不表示测试已经运行或目标行为已实现。
本表固定记录 G00 起点；G01 后的当前迁移与验证证据见第 6 节，不能把起点缺口
理解为新的阶段状态，也不代替 G02–G11 的实际实现与验收记录。

| 能力 | 已核对的当前事实 | 保留/迁移目标与验证落点 | 证明状态 |
| --- | --- | --- | --- |
| native Thread/Turn/队列 | ThreadClient / ThreadService；local v17 已有 start/enqueue/steer/input/cancel/resume 等操作 | G04 threads CLI；附件 C01/C04、T03/T05 | 源码已核对；CLI 待实现 |
| Core 调度/委派/上下文 | native run_native 已适配 Core；NativeSession 与 managed API 也保留 Core authority | 保留 spawn/delegate/wait、预算与 fencing；C02/C03、T04 | 源码已核对；回归未运行 |
| supervised ACP 会话 | supervisor v1 已有 snapshot/events/permission/mode/config/route transport；List 包含历史 runs | G03 真实连接投影，G04 单次控制；C04/C06/C07、T06/T09/T11 | 源码已核对；新 CLI 缺 |
| 独立 ACP controller/client | SessionHost + Controller 双向 dispatch；typed client 支持部分 lifecycle，fork 由 controller 透传 | 保留 stdio/headless 与能力协商；C06、T01/T09 | 源码已核对；不扩大协议子集 |
| native ACP ingress | daemon NativeAcpServer 接同一 ThreadService；v1/v2、owner/attachment 与重连 | 保留 EOF detach、旧答复拒绝、queue pause；T10 | 源码已核对；不能用冷 session list 作连接 |
| native launcher / inference-only | launcher per-process overrides；SDK inference 只有请求/路由身份 | 保留原生界面/API；动态 skill delivery 无 adapter 时 unsupported | 源码已核对；真实 harness/provider 未验证 |
| embedded task managed | managed_task::run 当前在 CLI 内 build App/NativeSession | G05 保留；G08 单独迁 daemon，保留 store/session/epoch | 源码已核对；不得称当前 daemon-owned |
| 权限/prompt/plain/cost | acp_cli、supervisor、Wire 等非 UI 消费 TUI；plain 仍依赖 Ratatui | G01 逐消费者迁移；附件第 6 节、T01/T02/T11 | 完整迁移清单已列；尚未迁代码 |
| MCP client/gateway/relay | Harness 连接/权限与 SDK MCP、launcher gateway 各有职责；旧 mcp add 只打印 stub | 保留 C05/T07，G06 才做源配置 CRUD | 源码已核对；CRUD 未实现 |
| skills/AGENTS.md | bounded metadata/hash/material 与 startup instructions 已有；selected_materials 只取 required 输入 | 保留 C05/T08；G07–G10 做安装、选择、指令激活和交付 | 发现不等于选择/激活；新增行为未实现 |
| evolution/check/review | operator/runtime 已有；部分 candidate/review 操作仅 UI adapter 暴露 | G04 非 UI 调用 + 业务测试迁移；T13/T15 | 源码已核对；不能删整个 PTY 目录 |
| verification/artifact/recovery | service/Core 精确 invocation、checkpoint/未知效果保护；close/recover 不全有 local transport | 保留 C08、T05/T12；G04 如需扩展则做版本协商 | 源码已核对；A26 未运行 |

附件第 2 节映射七条执行路径，第 3 节逐项覆盖 spec 6.4，第 4 节分别列出
已有 transport、仅 service API 和尚缺 CLI；第 8 节记录实际离线依赖图。

## 4. A01–A28 证据映射

当前全部 A 项的约定本地范围已取得 G11 冻结源码证据。责任阶段用于定位来源；
逐项真实测试、owner、PASS、依赖和输出审计见 [G11 验收附件](CLI_ONLY_AND_WORKFLOW_SKILLS_G11_ACCEPTANCE.md#3-a01a28-当前本地证据)。
`local_pass` 不表示真实 provider、真实 adapter、远端互操作或跨平台已验证；
unsupported、产品限制和外部资格继续分别列出。

| 验收 | 责任阶段 | 状态 | 当前证据 |
| --- | --- | --- | --- |
| A01 | G03/G05/G11 | local_pass | 旧入口在配置/daemon I/O 前拒绝；连接、catalog、历史分离；当前具体入口与结果见 [G11](CLI_ONLY_AND_WORKFLOW_SKILLS_G11_ACCEPTANCE.md#3-a01a28-当前本地证据) |
| A02 | G02/G11 | local_pass | 首次问答、并发重复 ensure-start、真实后台 readiness 返回 shell；当前具体入口与结果见 [G11](CLI_ONLY_AND_WORKFLOW_SKILLS_G11_ACCEPTANCE.md#3-a01a28-当前本地证据) |
| A03 | G02/G04/G06/G07/G11 | local_pass | 非 TTY 参数不齐或歧义失败；脚本资源选项、Git ref、bundle/scope 与后续动作抑制；当前具体入口与结果见 [G11](CLI_ONLY_AND_WORKFLOW_SKILLS_G11_ACCEPTANCE.md#3-a01a28-当前本地证据) |
| A04 | G02/G11 | local_pass | 秘密不回显、EOF/Ctrl-C 恢复；资源取消保留进度；单 JSON/human/broken pipe；当前具体入口与结果见 [G11](CLI_ONLY_AND_WORKFLOW_SKILLS_G11_ACCEPTANCE.md#3-a01a28-当前本地证据) |
| A05 | G06/G11 | local_pass | HTTP/stdio 完整字段、默认值、敏感值、名称/参数校验；当前具体入口与结果见 [G11](CLI_ONLY_AND_WORKFLOW_SKILLS_G11_ACCEPTANCE.md#3-a01a28-当前本地证据) |
| A06 | G06/G11 | local_pass | 原文注释/未插值引用；并发和失败不覆盖；onboarding resource-only 不重写 Router；当前具体入口与结果见 [G11](CLI_ONLY_AND_WORKFLOW_SKILLS_G11_ACCEPTANCE.md#3-a01a28-当前本地证据) |
| A07 | G02/G06/G11 | local_pass | 保存只报 saved_only；运行实例/代次不变；显式 restart；资源失败无 Finish；当前具体入口与结果见 [G11](CLI_ONLY_AND_WORKFLOW_SKILLS_G11_ACCEPTANCE.md#3-a01a28-当前本地证据) |
| A08 | G07/G11 | local_pass | 本地/Git/multi bundle、格式/路径/配额与明确失败；当前具体入口与结果见 [G11](CLI_ONLY_AND_WORKFLOW_SKILLS_G11_ACCEPTANCE.md#3-a01a28-当前本地证据) |
| A09 | G07/G09/G11 | local_pass | 完整 references/scripts/assets/binary 与 whole-bundle 版本；不执行 scripts/hooks；当前具体入口与结果见 [G11](CLI_ONLY_AND_WORKFLOW_SKILLS_G11_ACCEPTANCE.md#3-a01a28-当前本地证据) |
| A10 | G07/G11 | local_pass | 固定 tag/commit、branch 更新、CAS、冲突与失败保留旧 alias；当前具体入口与结果见 [G11](CLI_ONLY_AND_WORKFLOW_SKILLS_G11_ACCEPTANCE.md#3-a01a28-当前本地证据) |
| A11 | G07/G09/G11 | local_pass | 卸载阻止新选择，活跃/恢复绑定仍读取原完整版本；当前具体入口与结果见 [G11](CLI_ONLY_AND_WORKFLOW_SKILLS_G11_ACCEPTANCE.md#3-a01a28-当前本地证据) |
| A12 | G03/G07/G09/G11 | local_pass | project/global/unmanaged 与同名本地/两个 MCP 来源独立；caller/workspace 不串用；当前具体入口与结果见 [G11](CLI_ONLY_AND_WORKFLOW_SKILLS_G11_ACCEPTANCE.md#3-a01a28-当前本地证据) |
| A13 | G09/G11 | local_pass | 按实际任务/职责独立选择，可选空集，显式必需/排除优先；不验证真实模型语义质量；当前具体入口与结果见 [G11](CLI_ONLY_AND_WORKFLOW_SKILLS_G11_ACCEPTANCE.md#3-a01a28-当前本地证据) |
| A14 | G09/G11 | local_pass | 原 Turn 版本/预算冻结；工具结果/steering 仍可追加，旧 intent 不重复付费；当前具体入口与结果见 [G11](CLI_ONLY_AND_WORKFLOW_SKILLS_G11_ACCEPTANCE.md#3-a01a28-当前本地证据) |
| A15 | G09/G10/G11 | local_pass | 实际 SDK 模型请求/独立 ACP 输入收到约定版本；安装或 catalog 本身不算激活；当前具体入口与结果见 [G11](CLI_ONLY_AND_WORKFLOW_SKILLS_G11_ACCEPTANCE.md#3-a01a28-当前本地证据) |
| A16 | G09/G11 | local_pass | 子任务独立选择，显式父约束保留；父 MCP 同意不授权子任务；当前具体入口与结果见 [G11](CLI_ONLY_AND_WORKFLOW_SKILLS_G11_ACCEPTANCE.md#3-a01a28-当前本地证据) |
| A17 | G09/G11 | local_pass | 冷读无新连接/执行；认证恢复重验原缓存；失联/未知 owner 保持阻塞；当前具体入口与结果见 [G11](CLI_ONLY_AND_WORKFLOW_SKILLS_G11_ACCEPTANCE.md#3-a01a28-当前本地证据) |
| A18 | G04/G08/G09/G11 | local_pass | 逐 skill 同意不扩大 grant；错 Turn/fence/epoch/撤销/取消拒绝；普通审批独立；当前具体入口与结果见 [G11](CLI_ONLY_AND_WORKFLOW_SKILLS_G11_ACCEPTANCE.md#3-a01a28-当前本地证据) |
| A19 | G08/G09/G11 | local_pass | 产品 managed CLI 由 daemon 托管；原 session/epoch/journal 保持，旧 boot/未知 ACK 无本地回退；当前具体入口与结果见 [G11](CLI_ONLY_AND_WORKFLOW_SKILLS_G11_ACCEPTANCE.md#3-a01a28-当前本地证据) |
| A20 | G10/G11 | local_pass | 分别交付本地 prompt；未知/未协商/预算/取消/不确定结果可解释；MCP-origin unsupported；当前具体入口与结果见 [G11](CLI_ONLY_AND_WORKFLOW_SKILLS_G11_ACCEPTANCE.md#3-a01a28-当前本地证据) |
| A21 | G01/G04/G05/G11 | local_pass | permissions/prompt/NDJSON/text/quiet/cost 和原运行身份保留；当前具体入口与结果见 [G11](CLI_ONLY_AND_WORKFLOW_SKILLS_G11_ACCEPTANCE.md#3-a01a28-当前本地证据) |
| A22 | G05/G08/G11 | local_pass | 全 workspace 无 TUI crate；bro normal/build/dev、all-target/all-feature 图无 Ratatui/Crossterm，原 UI drivers 已删除；当前具体入口与结果见 [G11](CLI_ONLY_AND_WORKFLOW_SKILLS_G11_ACCEPTANCE.md#3-a01a28-当前本地证据) |
| A23 | G01/G04/G05/G08/G09/G11 | local_pass | 保留旧配置/历史/acceptance；未知副作用不可因 UI 删除或冷恢复重放；当前具体入口与结果见 [G11](CLI_ONLY_AND_WORKFLOW_SKILLS_G11_ACCEPTANCE.md#3-a01a28-当前本地证据) |
| A24 | 各源码阶段/G11 | local_pass | 实际 help、JSON/human、主 skill 200 行、manifest/AGENTS/CLI/defaults/wiring 一致；当前具体入口与结果见 [G11](CLI_ONLY_AND_WORKFLOW_SKILLS_G11_ACCEPTANCE.md#3-a01a28-当前本地证据) |
| A25 | G03/G08/G11 | local_pass | idle 活连接、活 worker、停止历史、未启动 catalog、daemon 不可用与查询失败分开；查询不造连接；当前具体入口与结果见 [G11](CLI_ONLY_AND_WORKFLOW_SKILLS_G11_ACCEPTANCE.md#3-a01a28-当前本地证据) |
| A26 | G00/G01/G04/G05/G08/G11 | local_pass | CLI/API/headless 全能力迁移与回归；事件、路由、费用、委派、恢复与未知效果不丢失；当前具体入口与结果见 [G11](CLI_ONLY_AND_WORKFLOW_SKILLS_G11_ACCEPTANCE.md#3-a01a28-当前本地证据) |
| A27 | G09/G11 | local_pass | M01-M03 当前连接门控、完整协议/缓存、聚合来源与反向读取；不是 host 激活；当前具体入口与结果见 [G11](CLI_ONLY_AND_WORKFLOW_SKILLS_G11_ACCEPTANCE.md#3-a01a28-当前本地证据) |
| A28 | G09/G10/G11 | local_pass | M04-M10 来源/完整 held entry/延迟文件/批准/原缓存与实际 native 模型；外部 MCP 交付拒绝；当前具体入口与结果见 [G11](CLI_ONLY_AND_WORKFLOW_SKILLS_G11_ACCEPTANCE.md#3-a01a28-当前本地证据) |

## 5. G00 执行记录

- 阶段范围：只读源码/依赖查询与文档更新；起始 HEAD 见第 1 节，已有草案保留。
- 覆盖：daemon native、embedded managed、supervised ACP、独立 ACP controller、
  native ACP ingress、native launcher、inference-only 的所有者和身份；spec 6.4
  八项能力；旧入口、共享逻辑、协议可达性、业务测试保留与后续缺口。
- 依赖查询：附件第 8 节的 `cargo tree --locked --offline` 命令全部退出 0。
  app 显式开启 orchestrator ACP feature，其 conductor/trace-viewer 链未引入
  Ratatui/Crossterm；不需为了删除 TUI 而关闭 ACP。`similar` 仍有 orchestrator 消费。
- 运行边界：未启动 daemon/agent/MCP，未认证、联网或调用付费模型；未编译/运行
  功能测试。测试符号仅做存在性与源码核对，不产生绿灯证据。
- 本地修改：docs 索引、spec、goals、implementation 与 G00 audit；不改 Rust、
  Cargo manifests/lock、分发 skill 或 plugin manifests，公共 CLI 当前行为不变。
- 文档检查：`git diff --check` 退出 0；`node --input-type=module` 静态检查退出 0，
  核对四份 CLI-only 文档的相对链接/代码围栏/空白、G00–G11 共 12 张卡、
  A01–A26 共 26 条、C01–C08 共 8 项和 T01–T15 共 15 组；附件列出的测试
  符号在对应源码存在。`git status --short --branch` 无 docs 外变更，
  `git diff --quiet HEAD --` 对 Cargo.lock 与根/app/orchestrator/TUI manifests 退出 0。
- 完成判定：G00 卡片要求均有文档落点；未实施 runtime 或取得 A 项运行证据。
  下一阶段为 G01，须由用户另行启动；本轮没有进入 G01–G11。

## 6. G01 执行记录

- 起始 HEAD：`54c822a34d4dbfd75d484ca35e108b497048efef`；保留已有五份 CLI-only
  文档差异。G00 的源码基线仍适用，未发现外部源码变更。
- 目标：迁出 headless/supervisor/actions 的 TUI 业务类型、普通文本 presenter 和
  session client adapter；对应 A21/A23/A26 的共享部分，不改变 CLI/审批默认/身份/资源。
- 范围：现有 ACP 权限与 wire、纯 prompt 描述/解析、费用元数据、普通文本/诊断输出、
  background client 与 UI adapter 分离。旧 UI 保持可编译，G02 onboarding 不进入。
- 完成范围：共享迁移与当前代码的本地回归；不表示 CLI-only 整体、UI 删除或
  workflow skills 已交付。A21/A23/A26 仅回填本阶段覆盖的部分证据。

### 当前实现与迁移落点

| 内容 | 当前落点 / 保留行为 |
| --- | --- |
| 权限数据、policy 与准确回复 | orchestrator `acp/permission.rs`；Prompt/Decision/Mode/Policy 与业务测试迁入，`answer_request` 直接 resolve 收到的请求；没有 UI Effect/队列。默认 DenyAll、supervisor Ask、exact option 与取消仍分别保留 |
| TUI permission 适配 | TUI `permission.rs` 只画 prompt、解析编号键；其余消费者直接引用 ACP 类型，不做公共 re-export |
| prompt 描述与解析 | orchestrator `acp/commands.rs` 的 Command/PromptCommand/Resolution/resolve、aliases 与三项解析测试；配置校验、构造仍归 app `actions/session.rs`，不导入 SDK/Core |
| 协议 projection 与普通文本 | Journal 及其测试迁到 `acp/journal.rs`；`acp/plain.rs` 从 journal 的 message/tool/plan 生成 String 行，保留 patch、顺序、输出游标、工具结果/内容描述/diff 与 plan，不经过 Ratatui Line/Buffer/cache |
| 文本输出契约 | NDJSON/quiet 分支保留；text 仍在 Turn settled 后输出 transcript，session/permission/result 标记保留。原终端 Markdown 的样式/viewport 布局不作为 plain 输出协议；正文直接保留源文本，不调用 UI markdown renderer |
| cost provenance | supervisor 与 TUI 直接使用 controller 唯一常量；删除“两份重复常量相等”的测试，保留实际 UsageUpdate 归因/未知与未报告费用测试 |
| session client | `agent_sessions.rs` 保留 transport/scopes/lease/start/list/stop/remove 与客户端身份测试；`agent_sessions/ui.rs` 留下 UI effect/projection/deck，旧 UI 入口转到该模块 |
| 共享诊断 | `session_diagnostics.rs` 独立保留 session log 路径与尾部输出；main/headless 不再为了日志调用 chat 的 UI renderer |

依赖变化仅为 TUI 消费 orchestrator ACP 类型，以及 plain argv 展示使用已有
`shell-words`；没有新增第三方包版本。没有 CLI flag、listen、env、默认配置、
schema 或 harness wiring step 变化，因此本阶段无需提前改分发 skill/plugin manifests。
旧 UI 命令、onboarding、实例投影、控制 CLI、资源选择仍保持当前行为；不进入 G02。
源码 guide 与 `ACP_SAFETY_INVARIANTS.md` 已同步实际所有者；旧 renderer/parity
spec/plan 增补 G01 本地迁移说明，保留其历史修订证据，不把旧 Wire/TUI 类型路径
继续作为当前实现入口。

### 验证进度

- 中间编译：`cargo check --all-features -p bitrouter -p bitrouter-tui --tests` 退出 0，
  前两次发现迁移引用遗漏后已修复；最终源码检查仍以完整 gates 的对应结果为准。
- ACP 定向：`cargo nextest run --all-features -p bitrouter-orchestrator --lib -E 'test(acp::)'`
  退出 0；96 passed、195 filtered/skipped。包含共享 policy、解析、journal/plain、
  exact permission、cancel/timeout、controller route/cost 与 native ACP 重连。
  后续补充的 rich transcript wire 测试将在完整 suite 验证。
- 第一次完整 `cargo nextest run --all-features` 编译退出 101：磁盘满，LLVM 报
  `No space left on device`，不是测试通过。确认没有存活的该构建进程后，仅回收
  此 goal 开始后创建的 98 个本 worktree incremental cache 目录（约 11 GiB）；
  不删除旧缓存、用户文件或其他 checkout。此前新 UI 测试 import warning 已修复。
- 使用 `CARGO_INCREMENTAL=0 CARGO_PROFILE_TEST_DEBUG=0 CARGO_PROFILE_DEV_DEBUG=0`
  重跑同一完整 nextest 命令；仅减少构建产物，无 feature/filter/测试范围变化。
  退出 0：4,474 passed、22 skipped，含 23 个 slow 用例，运行阶段 777.854 秒。
  新增 rich transcript wire 测试、旧 Code/PTY 与 client identity/scope 回归均通过。
  另回收本 goal 旧 profile 留下的 codegen object；按
  生成时间排除当前构建产物，未动源码或其他任务。链接器报告大型 `__eh_frame`
  compact-unwind warning，构建成功；不使用 allow 或修改源代码压制该 warning。
- 源码边界检查：10 个非 UI 源文件无 `bitrouter_tui::/ratatui::/crossterm::` 引用；
  新迁移文件未增加 allow/unwrap/expect/panic/public re-export。`git diff --check`
  退出 0，orchestrator all-features normal 依赖图仍无 Ratatui/Crossterm。
  `git show HEAD:crates/bitrouter-tui/src/journal.rs` 与新 `acp/journal.rs` 字节相同。
  client 的 start/attach_unlocked/stop/remove/release/mutate/stop_with_fence/request/grant
  九个方法体也与起点字节相同。
- 同一构建环境下 `cargo clippy --all-features` 退出 0；未新增项目 lint warning，
  Cargo 仍提示第三方 `proc-macro-error2 2.0.1` 的 future-incompatibility。
- `cargo test --all-features --doc` 退出 0：6 passed、1 ignored、0 failed。
- `cargo fmt -- --check` 与 `git diff --check` 均退出 0。
- 可重现命令与本地日志：

  ```sh
  CARGO_INCREMENTAL=0 CARGO_PROFILE_TEST_DEBUG=0 CARGO_PROFILE_DEV_DEBUG=0 cargo nextest run --all-features
  CARGO_INCREMENTAL=0 CARGO_PROFILE_TEST_DEBUG=0 CARGO_PROFILE_DEV_DEBUG=0 cargo clippy --all-features
  CARGO_INCREMENTAL=0 CARGO_PROFILE_TEST_DEBUG=0 CARGO_PROFILE_DEV_DEBUG=0 cargo test --all-features --doc
  cargo fmt -- --check
  git diff --check
  ```

  原始输出位于本机 `/tmp/bitrouter-g01-nextest-compact.log`、
  `/tmp/bitrouter-g01-clippy.log`、`/tmp/bitrouter-g01-doctest.log`；关联本节起始 HEAD
  与当前未提交源码 diff，不作为 hosted CI 或真实 provider/adapter 资格。

### 完成审计

| G01 要求 | 证据与判定 |
| --- | --- |
| headless/supervisor/shared actions 不依赖 TUI 业务类型 | source boundary guard 与 10 文件检查通过；所有保留的非 UI 消费者用 ACP 类型 |
| 权限/答复、prompt、plain、cost、session adapter 迁出 | 上表列出实际所有者与调用点；配置校验仍在 app，protocol/Core 无 app 配置依赖 |
| 保留当前 UI、默认、身份与资源选择 | 全 workspace 测试包含旧 UI/PTY、Ask/DenyAll、native/ACP、队列/恢复；Journal 与核心 client 方法体对比相同；未改 host/Core 执行或资源配置 |
| permissions/deny/cancel/参数别名/三种输出/cost 行为 | ACP 单测与 app integration、UI cost、supervisor/background 回归通过；text 直接产出 String，无 Ratatui Line/Buffer |
| 测试与依赖同步 | 非 UI 业务测试随所有者迁移，client scope/identity 测试留在纯客户端；依赖只增加必要内部边与已有 shell parser，无删协议 feature |
| 检查与阶段边界 | 完整 nextest、doctests、Clippy、fmt/diff 与文档检查通过；G02–G11 保持 not_started，没有提交/推送/发布 |

残余 TUI 引用均用于旧 native/ACP Code、deck UI adapter、UI action panels，或
onboarding/auth 的 selector（G02）、相应 UI 测试/注释。`lib.rs` 的 crate 名字符串
仅用于边界测试。它们在当前阶段有真实 UI 消费者，统一删除仍属于 G05。

## 7. G02 执行记录

状态：**本地完成**。起始/当前 HEAD 均为
`54c822a34d4dbfd75d484ca35e108b497048efef`，所有结果来自当前未提交工作区。
保留 G00/G01 的全部既有改动；没有 reset/clean、提交、推送、PR 或发布。

### 已实现与保留边界

- `prompt.rs` 使用编号/名称、提交关键词筛选和 Y/n 确认。Unix 使用已有 rustix
  的 canonical input/poll/termios，Windows 使用安全 console mode wrapper 与单一
  行输入通道。没有新 raw-key editor；prompt/onboarding/auth 不再引用
  `bitrouter_tui` 或 Crossterm。隐藏 key 和 OAuth redirect；EOF/Ctrl-C 恢复回显。
- 向导保留 provider/auth 选择、重复添加、失败重试、可跳过的 agent 和 Finish。
  不再默认选 Codex，不以 `chat.agent` 作为配置完成条件。`agents_selected` 只报告
  选择，旧 `harnesses_installed` 保留为空的 deprecated 字段，不声称安装或连接。
- 裸 `bro` 与 `init --after serve` 复用 lifecycle source、跨进程 startup lock、
  detached child 与真实 control Status readiness。启动锁内记录 pending source/PID，
  取消或超时后不重复 spawn；PID 单独不代表 ready。超时不杀 child，也不声称已停止。
  来源未核验的其他 endpoint 不自动接管。
- 已验证的同配置运行实例本身足以复用，不依赖客户端还保留一次性启动凭据、
  YAML 或 chat agent。只有 `Default` source 能使用零配置输入直接首次启动；
  缺失的显式 `BITROUTER_HOME` 不被凭据存在绕过。完整 entry 包含可取消的
  discovery/readiness 阶段，不暗中使用其他配置或远程目标。
- Ready 报告取目标 runtime 的 `config_state`；保存仍报告 `saved_only`。
  修改 socket、invalid/missing YAML 时不自动 reload/restart。Snippet 使用运行地址
  和 shell key 引用，不输出导出的真实密钥。
- `--after launch` 显式交给 native launcher。Headless setup 即使连着终端也拒绝
  缺失的 native executable，不弹安装确认；可选 launch 失败保留已保存的 Router
  配置。保留显式 `serve/start/init`、native launcher 与其他已存在的 ACP/任务入口。
- 没有迁移 connected agents、控制或资源 CLI，没有进入 G03。全局 TUI/Code/
  agents console 与其依赖保留到后续阶段；不把本阶段称为完整 TUI 删除。

### 本地验收

Cargo-built fixture：`apps/bitrouter/examples/onboarding_fixture.rs`；
二进制集成：`apps/bitrouter/tests/onboarding.rs`，14 项；
真实 PTY：`apps/bitrouter/tests/onboarding_pty.rs`，7 项；
startup/readiness 专项单测：`apps/bitrouter/src/daemon.rs`，4 项。以上均通过。

| 范围 | 具体证据 |
| --- | --- |
| A02 | 首次裸命令保存后后台启动；并发/重复调用相同 PID；不含 chat agent 的有效配置；无客户端凭据和 YAML 时复用同 source 的运行实例；显式 native fixture launch |
| A03 | stdin/stderr 非 TTY、完整 flags、缺少配置/显式 home、provider key 配对、JSON/human、broken stdout、显式 remote context 拒绝、headless native 缺失工具无安装提示 |
| A04 | 编号非法重试、提交关键词筛选、名称/回车选择、provider/auth 选择、空 key 失败重试、重复添加、Y/n、key 不回显、EOF/Ctrl-C 恢复、分离 stdout JSON |
| A07 | saved_only 与 ready 分离；旧 socket locator 与 restart_required；invalid/missing YAML；拒绝其他 source 的 endpoint；cancelled lock waiter、pending PID、child 退出、readiness deadline/timeout |
| A24 | README/development、CLI reference、skill/references、三个 plugin manifest 与本阶段实际行为一致 |

### 冻结源码完整检查

使用 `CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0`，
最终构建并发 `CARGO_BUILD_JOBS=2`；不修改功能、超时或测试范围来适应磁盘。
11 个源码/依赖文件的 SHA-256 在完整检查前后不变：
`/tmp/bitrouter-g02-source-checkpoint.json`。之后只更新文档状态。

| 检查 | 最终结果与日志 |
| --- | --- |
| `cargo nextest run --all-features --no-fail-fast` | 退出 0；**4487 passed / 22 skipped**，70 binaries，18 slow，runtime `645.361s`；`/tmp/bitrouter-g02-nextest-final-source.log` |
| `cargo test --all-features --doc` | 退出 0；**6 passed / 1 ignored**；`/tmp/bitrouter-g02-doctest-final-source.log` |
| `cargo clippy --all-features` | 退出 0；`/tmp/bitrouter-g02-clippy-final-source.log` |
| `cargo fmt -- --check` / `git diff --check` | 均退出 0 |
| 文档/分发 | 3 个 plugin JSON 可解析；skill 199 行；goal/spec/记录与 skill 的本地相对链接有效 |
| 源码/依赖边界 | prompt/onboarding/auth 无 TUI/Crossterm 引用；全局依赖仍由现有 UI 消费；shared permissions、ACP、MCP、Core 与恢复业务回归保留 |

### 修正与资格边界

首次完整链接遇到 ENOSPC，随后定点删除本 worktree 的 95 个过期 G01 测试
executable（约 5239 MiB，时间窗口、`.d` 来源和 G02 替代 hash 均核对；
清单 `/tmp/bitrouter-g02-removed-g01-executables.txt`）以及两个不完整 G02 链接产物。
没有删除源码、其他 worktree、用户 daemon 或其他任务数据，没有 chmod/跳过失败。

验证中修正了合成 registry fixture 的必填 env、旧 ACP 提示断言，并在最终审阅中
补上运行实例不依赖客户端当前凭据/YAML 的重复调用边界。前序通过记录不替代上述
最终冻结源码 gate；该边界单独回归日志为 `/tmp/bitrouter-g02-reuse-regression.log`。

工具链仍有 Apple linker 的大型 `__eh_frame` 提示及第三方 `proc-macro-error2`
future-incompatibility；没有新增 allow 或绕过检查。

资格仅为**本地 macOS 与 Cargo-built process/protocol/terminal fixtures**。
native Codex 使用临时命名的 fixture executable；没有实际账户 OAuth、真实
Codex/Claude、真实 provider 请求、Windows/Linux、hosted CI 或生产资格。
未自动安装或登录，不把 fixture 当作真实 provider/adapter 证明。

完成判定：G02 卡片的本地实现、A02–A04/A07/A24 对应范围及完整检查通过。
G03–G11 保持 `not_started`，须用户另行启动。

## 8. G03 执行记录（本地完成）

起始 HEAD 仍为 `54c822a34d4dbfd75d484ca35e108b497048efef`，保留全部 G00–G02
未提交差异，不自动 commit/push。前序共享逻辑和 onboarding 调用链已复核并复用。
本阶段只处理 A25、A01 的裸 agents 部分，以及必要的文档/身份保持项。

最终实现并通过本地 gates 的范围：

- `connected_agents` 定义只读报告与 CLI options；裸 `bro agents` 不再进入 manager，
  不要求 TTY。`agents list`/`agents sessions` 和显式 attach 的现有语义保留。
- local control 增量 `ConnectedAgents` 命令复用 owner-only endpoint，不申请控制 lease、
  不做兼容性 handoff 或启动。旧 daemon 的 unsupported/解码/超时错误不伪装成零连接。
- 现有所有者提供 liveness；不增加表、调度器、持久化、请求重放或镜像注册表。
  `ThreadService::active_workers` 读 TurnWorker guard；supervisor 检查实际 handle、
  command loop 和 child reaping；native ACP 读 live Weak attachment/cancellation；
  managed Core 同时检查原 registry 与真实 RemotePort connection。
- kind/实例身份与 Thread/Turn、run/native/agent session、connection、父 run、
  harness/epoch/执行实例分别保留。control boot 与 native runtime instance 分开。
  不按 agent 名或 harness session ID 合并；managed principal 的身份用不泄漏凭据的摘要。
- 请求的 native model selector 独立于 confirmed route；未知 model/resources 为 null。
  resource binding 仅来自已存在的 Turn inventory；不宣称安装即激活或已交付 skills。
- 按 kind/实例 ID 排序，页面 1–16 条，游标绑定 control boot；各页重新采样，
  各 owner 间不承诺原子快照。JSON/human 包含来源、采样时间和 coverage。

### 8.1 所有接入路径的覆盖处理

| G00 接入路径 / 补充消费者 | G03 行 / 范围 | 存活或排除依据 |
| --- | --- | --- |
| daemon native task / Code / HTTP agent API | `native_worker`，跨 daemon callers 的 worker metadata | 同一 ThreadService resident worker guards；冷/仅加载 Thread、queued Turn 不计入 |
| supervised ACP | `supervised_acp`；idle/detached 继续显示 | initialized handle、运行 controller loop、未 reaped child；starting 无 session、停止/失败/历史与 lease 不计入 |
| native ACP ingress v1/v2 | `native_acp_attachment` | delivery-owner 的实际 live attachment；EOF/close 后移除，未 attach session 的 initialized 连接明确不在范围内；worker 可独立继续 |
| daemon managed Core HTTP/WS | `managed_core_channel` | 复核 C02 消费者时补齐；现有 channel owner 提供连接、harness/session/epoch 绑定，不宣称远端 harness worker 状态 |
| CLI 内 embedded `task managed` | coverage `not_observable` | 宿主迁移仍属 G08；本阶段不冒称 daemon 托管 |
| 独立 ACP controller/client | coverage `not_observable` | 外部 CLI 拥有 transport；不把 router route lease 当作连接 |
| 原生 launcher | coverage `not_observable` | 独立 harness 进程没有 daemon-owned 连接证明；仍保留显式 launcher |
| inference-only API/客户端 | coverage `not_observable` | model request 不推导 agent/session 存活 |

### 8.2 定向验证与 CLI 证据

- 早期定向 owner 测试：`cargo test -p bitrouter-orchestrator --all-features --lib connected_`，
  3 passed；日志 `/tmp/bitrouter-g03-owner-tests.log`。其中新增 worker 与 ACP attachment
  两项，另有一项现有 disconnected ownership 回归；不以此替代最终全量检查。
- 早期真实 CLI fixture：`cargo test -p bitrouter --all-features --test connected_agents`，
  2 passed；日志 `/tmp/bitrouter-g03-cli-tests.log`。覆盖同类同名 session 独立身份、
  idle、catalog/历史/停止/失败、配置损坏后的 verified locator、分页和错误非零。
- 最终源码清单及 SHA-256：`/tmp/bitrouter-g03-source-checkpoint.json`（19 个源/测试文件）。
  最终完整检查见第 8.3 节；完成后的 source drift 为空。
- 真实 CLI 补查发现显式空 `BITROUTER_HOME` 尚无 YAML 时的误报错；现在只有缺失文件
  使用常规 endpoint 做只读查询，已有但无效的配置仍报错，verified locator 优先。
  集成用例已补“向导前无文件、不创建配置/daemon”断言。前一轮 nextest 完成构建后
  主动中止（exit 130；`/tmp/bitrouter-g03-nextest-before-empty-home-fix.log`），不算通过；
  修复后重新冻结并取得定向及完整检查证据。
- 补验 `cargo test -p bitrouter --all-features --test connected_agents`：2 passed，
  `/tmp/bitrouter-g03-missing-config.log`；包含向导前空 home 的调用。
  owner app 定向测试 3 passed（`/tmp/bitrouter-g03-app-owner-tests.log`）；
  real CLI + native ACP v1/v2 进程集成 3 passed（`/tmp/bitrouter-g03-integration-final.log`）。
  最终 nextest 重新覆盖 7 个新增测试及 EOF 后 worker 继续存在的真实进程断言。
- 真实二进制 help 已保存 `/tmp/bitrouter-g03-cli-help.log`（裸 agents/list/sessions）。
  临时空 home 的 stopped JSON 为 `/tmp/bitrouter-g03-stopped.json`；临时无 provider
  daemon 的 running JSON/human 为 `/tmp/bitrouter-g03-running.json` 与
  `/tmp/bitrouter-g03-running-human.log`。四个 owner scope 均 observed，running 为
  空数组、stopped 为 null；fixture daemon 已停止并移除其专用临时目录。
- README、CLI、development、分发 skill/reference 及三个 plugin manifests 已同步；
  SKILL.md 仍为 199 行。无 registry、配置 schema 或 durable format 变化；新 local
  control 命令不是 remote control inventory/HTTP API 的扩展，不需重生成那些 schema。

当前资格仅为本地 macOS fixtures；没有真实 provider/账户、Windows/Linux、hosted CI
或生产资格。不把 fixture 当作真实账户/provider 或跨平台交付证明。

### 8.3 最终冻结源码 gates

统一使用 `CARGO_BUILD_JOBS=2 CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0
CARGO_PROFILE_TEST_DEBUG=0` 限制本地构建资源，不改变 feature 或测试过滤。
最终源码 manifest 的 19 个源/测试文件与本轮构建匹配。

- `cargo nextest run --all-features --no-fail-fast`：退出 0，**4494 passed、22 skipped**，
  71 个二进制，21 slow，运行 807.325 秒；`/tmp/bitrouter-g03-nextest-final.log`。
- `cargo test --all-features --doc`：退出 0，6 passed、1 ignored；
  `/tmp/bitrouter-g03-doctest-final.log`。
- `cargo clippy --all-features`：退出 0；`/tmp/bitrouter-g03-clippy-final.log`。
- `cargo fmt -- --check`：退出 0；`/tmp/bitrouter-g03-fmt-final.log`。
- `git diff --check`：退出 0；`/tmp/bitrouter-g03-diff-final.log`。
- 文档相关本地链接、三个 manifest JSON、199 行 skill、源码 hash 核对通过；
  `/tmp/bitrouter-g03-doc-check.log`。已有 Apple linker / 第三方 proc-macro-error2
  提示保留，未新增 lint 豁免、测试跳过或 chmod 来绕过检查。旧裸 manager 入口断言
  已替换为只读快照断言；显式 attach 的 PTY、恢复终端与 run 生存期断言继续保留。

完成判定：G03 卡片、A25 与 A01 裸 agents 范围及保持项回归通过。
本轮仅本地 diff，未提交/推送；G04–G11 保持 not_started，不自动推进。

## 9. G04 执行记录（本地完成）

起始 HEAD 仍为 `54c822a34d4dbfd75d484ca35e108b497048efef`，保留 G00–G03
全部本地差异。已复核 G03 的完成记录与源码范围；沿用原有执行服务。

| 必需范围 | 实现与验证落点 | 状态 |
| --- | --- | --- |
| native list/create/show/history/events/start/enqueue/steer/pending/answer/cancel/resume | `thread_cli.rs` → ThreadClient → 原 ThreadService；真实 daemon/HTTP fixture、稳定 receipt、模型实际收到 steering | 定向与最终完整回归通过 |
| native unload/close/load/recover | local v18；显式 workspace grant、原服务的 close/join/checkpoint 恢复，queue resume 不替代 recovery | 定向通过，含崩溃后未知 verification 效果阻塞 |
| ACP show/events/pending/prompt/answer/cancel/review/stop/remove | `agent_sessions/control.rs` 与既有客户端 → supervisor；并发审批、观察断开、失败状态/原生历史保留 | 定向通过 |
| ACP mode/config/route/native lifecycle | agent 实际报告/确认，当前 run/session 作用域、NativeList/SelectSession；保留 lease、claim、direct/timeout | 定向通过 |
| evolution/check/review/candidate/publication/rollback | operator 的非 UI adapter；真实 CLI candidate/preview/register/restore、recorded checkpoint 修订；原生 campaign 迁移 | 本地定向通过；6 项真实 adapter 资格保持未运行 |
| 默认与恢复保证 | foreground DenyAll（全拒绝 exit 5）、background Ask、旧 task 默认；旧 Turn/fence 拒绝、FIFO、断开不取消、unknown effects 不重放 | 定向与最终 Core/API/ACP 完整回归通过 |
| 交付证据 | CLI/README/development/skill/manifests/current protocol 同步，help/JSON/human，A18/A21/A23/A26 本阶段范围 | 文档/help 与最终 gates 通过 |

### C01–C08 无 UI 可达性

| 能力 | 现有权威与当前入口 | 业务证据 |
| --- | --- | --- |
| C01 Thread/Turn/Item、历史/FIFO | 原 ThreadService/Core，`threads` 与保留的 `task run`、HTTP、native ACP | 新 native CLI 3 个流程；T03/T05 原 service/Core/context suites |
| C02 委派、独立 review、结果回收 | 原 Core scheduler；headless/API 任务触发，CLI view/history/events 检查原事实 | T04 collaboration/authority/context_decisions；没有新增 CLI scheduler |
| C03 预算、workspace、owner/epoch | 原 admission/workspace/Core grants、supervisor claims/fences | 新 foreground claim、他人 lease、旧 boot/client/generation 拒绝；T04/T05 原预算/caller 回归 |
| C04 队列、steer、审批与控制 | `threads` 单次操作；`agents prompt/pending/answer/cancel`、既有 stop/remove | Native 模型实际收到 steering；FIFO/审批/close/断开；ACP 多审批、timeout、exact ACK |
| C05 MCP/skills/AGENTS.md | 原 Harness/Core 与 SDK gateway/relay；headless/API 使用，已有管理/发现命令保留 | T07/T08 资源/readonly/权限/冷读回归；本阶段没有实现后续安装或 workflow 选择 |
| C06 ACP lifecycle/auth/fork/config | 原 controller raw dispatch、AcpClient、NativeAcpServer；`agents session/mode/config` 和 `acp serve` | 新 session/settings 用例；T09/T10 raw 透明性、更新与重连审批；未伪造 typed fork |
| C07 session route/费用 | 原 controller route/cost bridge、AcpRuntime 与 metering；`agents route/show/events` | 新 run/session RouteSet/Reset 确认与失败；T09/T11 的归因/费用来源回归，未知仍为未知 |
| C08 verification/artifact/恢复 | 原 ThreadService/Core/store 与 evolution owner；`threads show/load/recover`、`acp evolution`、现有 API | 新精确 check、unknown effects、冷恢复、checkpoint/history/restore；T12 与迁移后的 coding/publication 断言 |

原测试的业务断言全部保留，非 ignored 用例已在最终完整 gate 通过。真实 provider/adapter 的资格以独立
qualification 栏为准，不把本地 fixture 或源码可达性当作真实加载/效果证明。

### 定向验证与修正

- `cargo test -p bitrouter --all-features --test management_cli --test native_evolution_cli`
  退出 0：**12 个 management CLI 用例通过；1 个真实 Rust 工具结果用例通过，
  6 项真实 adapter 用例维持 ignored**。日志：`/tmp/bitrouter-g04-targets-final.log`。
  使用 `CARGO_BUILD_JOBS=2 CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0`。
- native 主流程覆盖 Ask、精确 request/Turn、队列取消/恢复、独立 verification 审批、
  分页历史、unload/reload 与原 checkpoint 恢复；独立用例用已确认到达的模型请求
  控制 steering 边界，检验模型实际收到追加输入。close 不重复执行、退出观察不取消。
- 另一 native 用例在 verification 已写 marker 后只终止 fixture 自己的 daemon；
  重启/load 仍有 `effect_unconfirmed` 与 unknown_effect，recover/queue resume
  拒绝，没有第二次模型调用或 marker 写入。没有接触用户进程或真实配置。
- ACP 用实际 Cargo-built peer 测试两个同时 pending、错误 option/request 拒绝、
  新 Turn 不复用旧审批、取消、review/stop/remove、真实设置确认与失败、通知投影、
  mode/config unsupported、run/session 路由、session list/new/load/resume、
  前台 owner worktree claim、另一 controller lease 不被抢占、direct 与 timeout 继承。
- ACK 代理在原 mutation 已执行后丢弃首个答复；两次 wire command 完全相同，
  只有一次新 prompt。原 fence 的 action 查询/精确重试不再次执行；错 client、
  control boot 与旧 generation 拒绝。native v17 在 create 之前被拒绝且目录仍空；
  supervisor v1 的既有读操作保留，v2 action_status 在 v1 中明确拒绝。
- 调试修正：fixture 使用当前实际路由解析支持的 `mock:model`；真实 endpoint
  preflight 使用确定的临时监听地址；CLI 的 JSON 字符串/bool 正确转换为 ACP value。
  close replay 保留原操作身份，但服务允许 snapshot cursor 更新；不把快照冻结
  当作幂等契约。EOF 改为有类型的 UnexpectedEof，使精确 ACK 重试能识别断开。
- 初始失败日志 `/tmp/bitrouter-g04-management.log` 保留。后续通过记录取代当时的
  timeout/fixture 错误，不能把早先失败或局部通过当作最终 gate。

### 业务测试迁移与保留映射

| G00 索引 | 当前非 UI 落点 / 保留证据 |
| --- | --- |
| T03/T05/T12 | `management_cli` 的 native Ask/FIFO/steering/close/recovery 3 个流程；既有 Core queue、service、managed_native、artifact/recovery tests 保留并纳入全量回归 |
| T06 | `acp_cli_permissions_observation_cancel_and_retained_lifecycle`、`acp_cli_defaults_unsupported_and_permission_timeout_remain_explicit`、`acp_cli_settings_sessions_routes_and_controller_lease_are_scoped`、`headless_foreground_owner_retains_worktree_claim_against_cli_background_start` |
| T09/T10/T11 | 既有 raw ACP/controller/native ingress/cost suites 保留；新 CLI 设置/路由、协议兼容和 ACK 用例追加验证，未另造 typed fork 或扩大支持子集 |
| T13 operator/历史 | `management_cli/evolution.rs` 两项真实 CLI 流程：exact preview/generation、注册与 restore publication/reason、checkpoint freeze/review/submit、幂等与旧 revision 拒绝、历史与当前 reward 分离 |
| T13 native coding/campaign | 从 `code_tui_pty/evolution/native*` 迁到 `tests/native_evolution_cli.rs` 及其子模块。6 个 maintained Codex/Claude 的反馈/工具执行/自动 publication/rollback 用例保留原资格标记；运行/审批/scoring/policy 操作脱离 terminal，真实 adapter 尚未运行 |
| T15 | 新 native 与 ACP 单次操作证明接受后客户端退出仍由 daemon 执行，observer 丢失不取消，取消/队列/审批/timeout 不依赖 UI；纯屏幕/热键断言和其他 PTY 测试仍留给 G05 逐项处置 |

`code_tui_pty/background.rs` 中本来就使用 headless CLI 的 schema、no-wait、
remote-context 等业务回归继续保留；G05 拆分测试根时只能迁移，不能按目录名删除。

迁移后的 campaign 保留实际文件/单元测试结果、cost/latency/accounting、默认 bandit
参数与 cohort 16、至少 20/20 两组样本、monitor 与 trial 隔离、自动 promote 与
adopted_quality_rollback、禁止 operator_restore 替代自动回滚、回到 strong baseline
的实际请求断言。`trial_values`、`processed`、`verify_checkpoint` 三个业务函数
与迁移前逐字规范化比较一致；只有 driver/展示被替换，无 TUI/Ratatui/Crossterm 引用。
初始 supervisor 启动复用既有 API，并通过原 LaunchOptions 清除宿主全部 ambient
环境；之后逐次使用 CLI 控制与精确 one-time permission，未采用 approve-all。

### G04.5 完整检查与 boot fence 补验（完成）

第一次冻结及完整检查后，身份审计发现两个需修复的边界，因此旧检查不作为
最终 gate：ACP CLI 发现 boot 与领取 grant 之间可能切换；evolution 原先只在
客户端检查 boot，写入端缺少同一实例约束。

- 已用真实 daemon + Rust 协议代理复现 ACP 竞态：带旧 boot pin 的 review
  被接受。`/tmp/bitrouter-g04-boot-fence-before.log` 退出非零，错误为
  `unexpectedly accepted ... previous-control-boot`，是有效失败证据。
- ACP 新修复：领取一次 boot-local grant 后再次确认 boot。这个 client 从不
  自动刷新 grant；后续重启使原授权失效，不能在新实例重新授权并重放。
- Evolution 新增 `EvolutionHello` / `EvolutionFenced` 的版本化 v1 local 控制。
  CLI 协商版本并把 control instance 交给写入端核对；旧 `Evolution` 留给原
  客户端，不作为新 CLI 的回退。旧 daemon、未知版本或错误实例在执行前失败。
- 回归 `acp_cli_rejects_control_boot_change_during_authorization_before_mutation`
  检查 0 次 mutation 和原 unread；`evolution_cli_checks_control_boot_at_the_mutation_owner`
  检查服务端拒绝、mode 不变，并覆盖无 handshake/未知版本无 legacy 回退。
  修复后运行：`/tmp/bitrouter-g04-boot-fence-after.log`，退出 0；两项通过。

`/tmp/bitrouter-g04-source-checkpoint.json` 是补验前快照，对应旧完整检查
`/tmp/bitrouter-g04-nextest-final.log`：4506 passed / 22 skipped，23 slow，970.636s。
这份历史结果保留，最终 gate 使用下面的修复后源码与日志。

补验后冻结 `/tmp/bitrouter-g04-source-checkpoint-final.json`：820 个 Rust/Cargo
路径，含 6 个历史迁移/删除路径。最终完整检查后逐项核对 SHA-256 和路径集合，
没有漂移或未冻结的新源码。所有源码检查使用同一 HEAD 的本地 diff；文档收尾
不改变该源码范围。构建环境沿用定向测试的四个 Cargo 变量。

| 检查 | 当前结果 |
| --- | --- |
| `cargo nextest run --all-features --no-fail-fast` | 修复后通过：4508 passed / 22 skipped，73 binaries，21 slow，715.900s；`/tmp/bitrouter-g04-nextest-post-fence.log`，含全部 14 项 management CLI 用例和迁移后的 Rust 工具结果用例 |
| `cargo test --all-features --doc` | 修复后退出 0：6 passed / 1 ignored；`/tmp/bitrouter-g04-doctest-post-fence.log` |
| `cargo clippy --all-features` | 修复后退出 0；`/tmp/bitrouter-g04-clippy-post-fence.log` |
| app all-targets Clippy | 修复前退出 0；仅原 G02 onboarding_pty 两个 byte_char_slices 风格提示；`/tmp/bitrouter-g04-clippy-target-final.log` |
| `cargo fmt -- --check` | 修复后退出 0；`/tmp/bitrouter-g04-fmt-post-fence.log` |
| `git diff --check` / 文档静态核验 | 文档收尾后退出 0；11 份 Markdown 的代码块与 227 个本地文件链接通过，1 个既有站点路由单列；12 张阶段卡/提示词、A01–A26 与状态一致性通过 |
| CLI / 文档分发 | 8 组实际 help 退出 0；`/tmp/bitrouter-g04-cli-help.log`；skill 199 行，3 个 manifest JSON 有效；新 evolution 握手与 ACP grant/boot 契约已同步 CLI、development 和 skill reference |

完成判定：G04.1–G04.5 均取得对应业务与交付证据；C01–C08 和 T01–T15 的
保留/迁移映射见上表。A18/A21/A23/A26 仅本阶段切片完成，完整 spec 尚未验收。
下一阶段为 G05，由用户另行启动；本阶段保留现有 UI 及其依赖。
22 个 skipped 不算通过；其中迁移的 6 项真实 Codex/Claude adapter 用例继续
保留原 ignored 标记，未取得真实 adapter、provider 或 hosted CI 资格。

### Spec v0.3 文档细化（此前文档修订）

此前的设计修订只调整 spec、Goal 计划、实现记录与 docs 索引：统一当时 G04
进行中的状态，补充控制契约、复杂阶段内部检查点、分阶段 A 项覆盖和跨轮接续
记录。没有运行代码变更或新的功能验收；没有放宽 G04 的范围或完成门槛。
G00 审计及 G01–G03 的历史完成证据保留；内部检查点不取代原阶段 goal。
本次文档检查通过：4 份文档的本地链接与代码块、12 张阶段卡和对应 goal 提示词、
spec 与证据表的 A01–A26 完整性和当前状态一致性；`git diff --check` 退出 0。
纯文档修订未重新运行 Rust 功能测试，不产生新的运行资格。

本轮不删除 UI、不进入 G05，不提交/推送，不访问真实付费 provider 或用户配置。

## 10. G05 执行记录（本地完成）

- 起始 HEAD：`54c822a34d4dbfd75d484ca35e108b497048efef`，保留 G00–G04 全部本地差异。
- 前置核验：G04 最终 820 路径快照逐项 SHA-256 一致；最终 all-features、
  Clippy、doctest、fmt 日志保留。G01–G03 的行为已在 G04 最终回归覆盖。
- 删除前备份：`/tmp/bitrouter-g05-baseline/` 与 `/tmp/bitrouter-g05-baseline.json`，
  保存当前 Rust/Cargo、开发文档和分发 manifest，供迁移对照及物理行统计。
- 范围：旧 code/tui/chat/agents attach UI，renderer/composer/deck 与 TUI crate；
  清理其依赖和配置说明，保留原有 headless/API/launcher、数据、权限和恢复行为。
- 验收：A01/A22–A24/A26 的 G05 切片；先记录 UI/PTY 业务断言的迁移映射，
  再删除展示与快捷键断言。删除后的真实 CLI、依赖图与完整回归均通过，见下。
- 本阶段不进入 G06，不提交、推送或调用真实 provider/adapter。

### 删除前后的业务测试映射

| 原落点 / 断言 | 保留或迁移后的证据 | G05 状态 |
| --- | --- | --- |
| `code_tui_pty/background.rs` 的 Ask/显式 policy、permission timeout、schema 失败、load/resume、no-wait、非 TTY、remote gate、stop/remove/native history | 迁到 `tests/background_cli/lifecycle.rs`，断言保留；foreground worktree claim 由 G04 `headless_foreground_owner_retains_worktree_claim_against_cli_background_start` 走无 UI owner 验证 | 定向及最终回归通过 |
| `code_tui_pty.rs` 的 peer 精确多行输入、并发审批与 cancellation、后台 client exit 不结束任务 | 迁到 `tests/background_cli.rs`；Rust peer/独立配置保留，没有 PTY、writer 或屏幕解析 | 定向及最终回归通过 |
| remote operations 的 authenticated status/models/requests/route、错误不回退本地 | 改为 `remote_cli_reads_use_authenticated_remote_actions` 与 `remote_cli_errors_do_not_fall_back_to_local_models`，保留 bearer 和实际 request body 断言 | 定向及最终回归通过 |
| native/ACP UI 审批、队列、steer、cancel、observer disconnect、settings/session route/lifecycle | G04 `management_cli` 的实际 daemon/peer 流程及原 Core/service/native ACP suites；UI 本地编辑器队列随 composer 删除，native durable FIFO 保留 | 最终回归通过 |
| native UI 的 history/reattach 与 workflow/status projection | G04 native cold-load/history/close/recover、G03 connected instances 与 Core context_decisions；删除屏幕布局、spinner 和导航按键断言 | 业务覆盖保留 |
| `chat_on_a_pipe_is_plain_text` | 改为真实 `run - --format text` subprocess；检查 peer 回复、无 ESC 与成功退出。原 serve fixture 故意向 native-b 发 update，不适用于首个 native-a；改用匹配 Session ID 的 Cargo peer，没有放宽生产过滤 | fixture/CWD 修复后定向及最终回归通过 |
| private evolution UI adapter 的 manual review/revision、unknown verification、旧 prefix/history/retraction/owner | G04 CLI checkpoint/history/idempotency；本阶段追加错误 verification evidence 与 unknown bounds；原 `evolution::tests::{missing_verification_is_unknown_and_claims_cannot_be_scored_as_tests,partial_scores_keep_unknown_weight_in_denominator,fabricated_or_cross_checkpoint_citations_are_rejected}`、checkpoint store CAS/retractions/deletion tests 保留 | CLI 与保留业务最终回归通过 |
| private candidate UI adapter 的 preview/register/revise/restore、mode/owner/route fences、joint rules、balanced account 脱敏 | G04 exact candidate/restore CLI；本阶段追加 `candidate_cli_preserves_joint_rules_judge_fences_balanced_accounts_and_recovery`，检验真实 CLI、精确 digest、两条规则、judge identity、重复提交、冷启动后旧 route 拒绝和新 review；后端 publication/bandit tests 保留 | 定向及最终回归通过 |
| native campaign 的执行、反馈、自动 promote/rollback、20/20 样本、成本及基线请求 | G04 已迁 `tests/native_evolution_cli*`；6 个真实 maintained adapter 用例保持原 ignored 标记，Rust judge 仍运行 | 三个 campaign 文件与 G05 基线逐字节一致；非 ignored 回归通过，真实 adapter 未验证 |
| TUI cost provenance 与缺失不是零 | supervisor UsageUpdate 仍按 controller 唯一 provenance 标记保留，client/telemetry/费用业务测试保留；仅删除 footer 样式和截断断言 | 所有者未改，最终回归通过 |
| UI parity 的 SessionPorts、菜单、editor/clipboard、hotkeys、宽度/resize/frame/focus/signal restoration | 删除纯展示/输入适配器及其重复 report/屏幕断言；保留原 status/models/route actions、CLI/HTTP inventory 与共享 prompt validation；onboarding/launcher PTY 和进程基础设施继续运行 | UI 删除，保留入口最终回归通过 |

`ACTIONS.tui_command` 改为非 UI 的 `session_command` 供 ACP discovery 和保留的
prompt 名称校验使用；route_set/reset 指向已有 CLI leaf。旧 evaluator ID/version
字符串继续作为既有 measurement contract 身份，不因 UI 删除重写历史或标签。
协议 Attach/lease/heartbeat/release 保留；新增无 UI attachment journey 验证其
保留历史、续约、不变的 generation 和释放后任务仍运行。

旧 code/tui/chat/agents attach 仅保留隐藏的迁移错误解析，不再有执行 driver。
错误在 context、config、daemon 查询前返回。旧 `chat.model/commands` 仍用于
headless，`chat.agent` 仍用于显式 onboarding native launch；config validate 的
migration_hint 解释该边界，不读取旧 code-hotkeys 文件或覆盖配置。

### 定向检查、源码与依赖证据

- 删除后的实际入口验证：`background_cli` 12 passed、`management_cli` 18 passed，
  见 `/tmp/bitrouter-g05-targets-corrected.log`；`acp` 19 passed，
  见 `/tmp/bitrouter-g05-acp-final.log`。本阶段 49 项定向 journey 分别通过。
- 保留失败终态：前几次定向运行暴露了 pipe fixture 的 native-a/native-b 不匹配、
  Rust 文件 module 的 proc-macro 位置、fixture 缺少 agent name、remote handshake
  的 capability 请求顺序，以及有效 owner fence 需使用完整 digest 的负向测试。
  修复测试配置/调用方式后重验；没有关闭生产 session 过滤、身份检查或权限。
  最后一个 pipe 失败来自旧测试会改进程 CWD；复用已有 CwdGuard，在同步 fixture
  构建时持有，await 前释放。19 项并行 ACP 测试随后通过。相关 `g05-targets*.log`
  与失败终态保留，不把中间结果当最终 gate。
- `cargo run -p dist-helper -- generate-schema` 与 `cargo run -p dist-helper -- check`
  退出 0：schema 仅更新 chat namespace 的说明，字段/默认值不变；registry 未变。
  日志为 `/tmp/bitrouter-g05-schema-generate.log`、`/tmp/bitrouter-g05-dist-check.log`。
- `cargo tree -p bitrouter --all-features --target all --locked --offline --prefix none --edges normal,build,dev`
  退出 0，见 `/tmp/bitrouter-g05-dependency-tree.log`。normal/build/dev 和各 target
  的图中均无 bitrouter-tui、Ratatui、Crossterm；Cargo.lock 同样没有这些 package。
  `cargo metadata --no-deps --format-version 1 --locked --offline` 确认 workspace
  为 7 个成员，无 TUI crate。ACP/MCP、similar、shell-words 与 PTY dev 设施保留。
- 最终源码冻结：`/tmp/bitrouter-g05-source-checkpoint-final.json`，823 路径、51 个已删除
  路径，覆盖 Rust/Cargo 与生成 schema；包含 G04 尚未跟踪、现已删除的 UI adapter。
  最终回归、Clippy/doctest/fmt 后逐项核验无漂移，也没有新增未冻结源码路径。
- 第一次全量编译因磁盘满退出 101，尚未运行测试：`/tmp/bitrouter-g05-nextest-final.log`。
  先确认进程终止，再对本 worktree 的 target 执行 Cargo clean，回收 18.8 GiB；
  不改源码、用户配置、数据或其他 worktree。源码 hash 仍一致。
  `/tmp/bitrouter-g05-nextest-post-clean.log` 随后在测试开始前主动中止（退出 130）：
  补充审计发现 Rust peer 尚有只服务旧画面的分支。删除无人使用的七类 scenario、
  backlog/refusal/手动 permission 控制与旧 background-isolation 特例，保留六类
  CLI/协议场景，并拒绝未知 scenario。只有该 fixture 相对第一次冻结变化；
  重新冻结后完整检查先使用 `/tmp/bitrouter-g05-nextest-after-fixtures.log`，结果见下。
- fixture 清理后的第一次完整运行退出 100：4194 passed / 1 failed / 22 skipped，
  71 binaries，23 slow，812.447s。唯一失败为旧 help 契约仍要求公开显示 code，
  日志 `/tmp/bitrouter-g05-nextest-after-fixtures.log`。其余业务及容量/恢复回归通过。
  这不是最终 gate。旧断言已改为 threads 公开、code 隐藏，并检查 agents attach
  隐藏；compatibility-flags 的公开命令覆盖也改为 threads。没有跳过该用例，
  没有放宽旧 UI 入口的迁移错误要求。三项定向契约退出 0（3 passed；其余为 filter/ignored），日志
  `/tmp/bitrouter-g05-help-contract-fixed.log`。最终全量使用
  `/tmp/bitrouter-g05-nextest-verified.log`，4195 passed / 22 skipped，退出 0；source checkpoint 已更新，
  相对前次仅 main.rs 的测试契约变化，旧快照另存 before-help-fix。
- 修复 help 断言前 `cargo clippy --all-features` 退出 0（2m47s），
  日志 `/tmp/bitrouter-g05-clippy-initial.log`；最终源码再次通过，见下表。
- 真实 binary：17 组 help 和 5 组旧命令迁移报错验证通过，
  `/tmp/bitrouter-g05-cli-help-and-retirement.log`；旧命令均为 bad_request / exit 1，
  空临时 home 在全部调用后仍为空，未进入本地/remote config 或启动任何服务。
- 文档静态核验：43 份 Markdown 的代码块与 542 个当前文件链接通过；历史已删除
  源码链接指向核验过的 Git 对象，无法核验的旧位置明确记为文字，未伪装当前文件。
  12 张阶段卡及提示词、198 行分发 skill、3 个 manifest JSON 通过；完成状态回填后再次核验。

### 最终冻结源码 gates 与完成判定

所有 Rust/Cargo/schema 检查对应同一 HEAD 的本地 diff 和 823 路径最终快照。
下列命令使用 `CARGO_BUILD_JOBS=2 CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0
CARGO_PROFILE_TEST_DEBUG=0` 限制本地构建缓存；不改变 feature 或测试语义。
日志均保存在本次运行的 `/tmp`，不作为已提交或 hosted CI 的证据。

| gate | 终态与证据 |
| --- | --- |
| `cargo nextest run --all-features --no-fail-fast` | 退出 0；71 binaries，4195 passed / 22 skipped，15 slow，731.483s；`/tmp/bitrouter-g05-nextest-verified.log` |
| `cargo clippy --all-features` | 退出 0，58.01s；仅既有第三方 proc-macro-error2 future-compat 提示；`/tmp/bitrouter-g05-clippy-final.log` |
| `cargo test --all-features --doc` | 退出 0；6 passed / 1 ignored；`/tmp/bitrouter-g05-doctest-final.log` |
| `cargo fmt -- --check` | 退出 0；`/tmp/bitrouter-g05-fmt-final.log` |
| schema generation / dist-helper check | 均退出 0；去除 description 后与原 schema 的字段、结构及默认值完全相同 |
| 全 target 依赖图 / workspace metadata | 均退出 0；7 个成员，无 TUI crate / Ratatui / Crossterm，保留 ACP/MCP 及所需 CLI/PTY 依赖 |
| 真实 help / 迁移错误 | 17 组 help、5 组旧入口通过；另有 management_cli 验证 JSON/human、历史与配置不变 |
| 完成审计 | 823 路径无源码漂移、11 处已退休路径不存在、6 个关键 CLI journey 在最终日志通过、原 campaign 文件不变；`/tmp/bitrouter-g05-completion-audit.json` |
| 文档静态核验 / `git diff --check` | 收尾后退出 0；43 份 Markdown、542 个当前文件链接、12 张阶段卡/提示词、A01–A26 索引、3 个 JSON manifest 与状态一致 |

完成判定：G05 卡片及 A01/A21–A24/A26 的本阶段切片通过；UI 删除后的原有
orchestration、headless/API/launcher、权限、持久化历史与恢复仍有本地运行证据。
没有本阶段 required work 遗留。G05 结束时 G06–G11 尚未开始；后续阶段状态见阶段总表。
真实 provider/maintained adapter、hosted CI 和跨平台资格仍按第 16 节单列。

### 代码量口径（源码冻结时）

只统计 workspace Rust 文件的 UTF-8 物理行，含内联测试、注释与空行；不含
target、Cargo.lock、schema、文档或外部依赖。与 G05 开始前的完整本地快照比较：

| 项目 | 物理行 / 文件 |
| --- | --- |
| G04 结束 / G05 开始 | 474,934 行，804 个 Rust 文件 |
| G05 完成 | 439,345 行，762 个 Rust 文件 |
| 文件移除 | 44 个 Rust 文件，36,311 行 |
| 新文件 | 2 个 CLI 测试文件，1,525 行，以既有 fixture/业务测试迁移为主 |
| 既有文件修改 | 16 个，增加 699 行、删除 1,502 行 |
| G05 Rust 净变化 | **−35,589 行** |
| 相对 G00 最初基线 | 468,581 → 439,345，**净减 29,236 行**；包含 G01–G04 新 CLI/迁移的增量 |

迁移行同时出现在旧文件删除和新文件增加中，净值已经抵消，不另算删除收益。
这些数字用于审阅范围；完成条件仍是能力、身份、权限、历史和回归证据。

## 11. G06 执行记录（本地完成）

- 起始 HEAD：`54c822a34d4dbfd75d484ca35e108b497048efef`；保留 G00–G05 全部本地差异。
- 前置核验：G05 的 823 路径最终源码快照无漂移；4195 passed / 22 skipped、
  Clippy、doctest、fmt 与依赖图日志仍适用于起点。备份见
  `/tmp/bitrouter-g06-baseline/` 和 `/tmp/bitrouter-g06-baseline.json`。
- 范围：MCP list/show/add/edit/remove、单条结构完整输入、行式问答/EDITOR、
  无损候选、同目标 publication lock 与原子写入、默认脱敏和 saved_only 报告。
  保留 check/tools、gateway/relay；不启动配置中的 MCP 进程，不自动重启 daemon。
- 对应 A03/A05–A07/A24 的本阶段切片，已取得实际入口与最终完整 gates 证据。
- 已实现：本地 list/show/add/edit/remove（search 保留独立隐藏 registry 浏览）；
  快捷 HTTP/stdio、完整 YAML/JSON --from、默认字段、同名冲突与完整替换。
  终端 add 使用行式选择/输入，edit 使用私有单条文件和 shell-words 解析的 $EDITOR；
  stdout 保持 JSON，编辑器输出转 stderr。
- 写入：未插值原文上的 block-YAML 节点编辑，完整语义对照确认只改目标；
  配置/MCP/host 校验和相对原来源的 policy 校验；共享 publication lock 保护原文
  CAS 与 atomic replace。保留权限，新增文件私有；普通路由 reload 不更新 native MCP。
  onboarding 保存现在也使用同一个配置 publication lock。
- 普通报告保留 source map key（id）和声明 name，旧别名可读且可显式修复；
  隐藏 headers/env/args 值与 URL userinfo/query/fragment；不输出原文、
  diff、内容哈希或底层解析器回显的秘密值。读取只显示 configured，写入只证明
  saved_only/running unknown；实际 restart_required 由 daemon status 另行证明。
- 已验证：CLI/状态/check 共 12 项、真实终端 3 项定向通过，见
  `/tmp/bitrouter-g06-targets-final.log`；此前同类检查与失败终态保留。
  document 边界 6 项也已通过，见 `/tmp/bitrouter-g06-document-final.log`；
  alias 补充后的 5 项真实 CLI 再次通过，见 `/tmp/bitrouter-g06-alias-target.log`。
  同一最终源码的完整回归/Clippy/doctest/fmt 与分发文档验收通过，见下表。
- 保留中间失败：初次编译发现 Output::emit 返回类型和 bad_request 参数类型
  不匹配；修复为显式错误传播。相对路径测试因 macOS /private 临时目录别名
  比较失败，改为核验绝对路径及同一 canonical 文件，后续定向已通过。
  日志为 `/tmp/bitrouter-g06-document-tests.log` 与
  `/tmp/bitrouter-g06-cli-and-pty.log`，不作为最终 gates。
- 补充审计：保留 header 中字面 {} 注释，防止误当 empty map；保留/删除多行
  scalar 中以 # 开头的内容时按 entry 缩进判断，防止重新解释成 YAML 注释。
  对应单独 document regression，完整候选仍执行语义对照。
- 初次完整 gate 在编译成功、尚未执行测试时因上述 alias 标识审计主动中止
  （退出 130），保留 `/tmp/bitrouter-g06-nextest-final.log`。已核验 nextest PID 与
  本 worktree cwd 后发送 SIGINT，没有终止无关进程。补充 id/alias 修复 journey
  后重新冻结；之后两次实际完整运行的终态分列如下，不复用旧 gate。
- 本阶段不进入 G07，不提交、推送或访问真实 provider/用户配置。


### G06 要求与实际证据入口

| 要求 | 实现 / 实际验收 | 当前状态 |
| --- | --- | --- |
| A03/A05：CLI、transport 与完整字段 | `mcp_config::Command/execute`；`tests/mcp_config.rs` 走真实二进制，HTTP/stdio shortcuts、--from file/stdin、默认值、冲突、缺项、id/name、配置优先与绝对路径、help/JSON/human | 5 项最终定向及完整回归通过 |
| A05：秘密不进入普通输出 | 读取未插值原文；headers/env/args/URL 凭据脱敏，解析/静态验证错误不嵌入值；实际 CLI 检查输出与失败路径 | 定向及完整回归通过 |
| A06：原文保留 / 不支持则拒绝 | document 候选只替换目标片段，再对完整 JSON 语义对照；CRLF、注释、顺序、引用、header 字面 {}、多行 scalar 的 # 内容；flow/anchor/复杂布局拒绝 | 6 项 document 与 5 项 CLI 中对应场景通过 |
| A06：并发与原子写入 | 读取快照后收集输入/编辑，commit 使用同一 publication lock 跨原文 CAS 和原子 rename；创建使用 noclobber；两个同版本线程只允许一个提交，真实编辑器等待期间的外部修改会拒绝 | unit/PTY 通过；沿用既有 writer 的阶段语义 |
| A06：失败与恢复边界 | 校验/编辑器/旧版本/并发出现/目标被换成链接在 publication 前失败，原文不变；rename 后目录同步失败属于已保存结果不确定，错误明确要求检查保存结果，不声称未保存或自动重放 | 本地故障与失败 journey 通过；目录 sync 原语沿用现有实现 |
| A07：saved / running | 写 receipt 仅 saved_only / running unknown；`mcp_cli_save_requires_restart_and_never_reloads_the_running_target` 走真实 CLI + 运行 IPC，实例/代次不变，status 标记 mcp_servers restart_required，routing reload 明确拒绝 | 定向及完整回归通过 |
| C05/T07：保留 MCP runtime | SDK MCP、orchestrator Harness、tools、gateway、host、reload 的 19 个重点文件相对 G06 基线 hash 不变；全部 crates 的 375 个 Rust/manifest 同样未改；实际 check/tools 与独立 gateway/relay/launcher suites 由完整 gate 覆盖 | 静态、定向及完整回归通过 |
| A24：分发一致 | CLI reference、README/development、199 行 skill/references、三个 plugin manifests 已同步；既有 schema/字段/default 不变，skills 安装未启动 | 43 份 Markdown / 544 个本地链接与 manifest 静态、dist、真实 binary 检查通过 |

首轮完整运行出现一项失败：既有
`local_cli::tests::probes_compatible_old_failed_and_hung_workers` 的第一个有效版本
worker 在 2 秒 probe 窗口内返回 false（用例耗时 2.178s）。该模块相对 G06 基线
未改，不能把它算作通过或把生产 timeout 放宽。保留完整失败输出；单独复现见
`/tmp/bitrouter-g06-local-worker-repro.log`，原代码/原 2 秒 timeout 下独立通过
（1 passed，2.238s；4231 skipped 为 filter/ignored）。这是对负载/启动时限敏感的
推断，不能用单项通过替代完整 gate。首轮完整 gate 已退出 100：4210 run，4209 passed（21 slow）/ 1 failed / 22 skipped，
73 binaries，907.712s。日志 `/tmp/bitrouter-g06-nextest-verified.log` 保留，
不将其算作通过。其他 Cargo 检查均已结束后，完整 all-features 复验仅将测试
并发设为 4，最终退出 0：4210 passed（10 slow）/ 22 skipped，73 binaries，
860.095s，日志 `/tmp/bitrouter-g06-nextest-bounded.log`。原 probe 同样通过
（2.137s）；未跳过用例、未改生产 2 秒 probe 或其断言，未缩小 features 范围。

最终源码：`/tmp/bitrouter-g06-source-checkpoint-final.json`，827 个路径（含 G05 的
51 个已删除路径）；相对本阶段第一次冻结，alias 修复仅涉及 mcp_config、
对应测试和 MCP help 说明。
最终通过使用 `/tmp/bitrouter-g06-nextest-bounded.log` 与
`/tmp/bitrouter-g06-clippy-verified.log`。先前 Clippy 两次退出 0，但不代替冻结后 gates。

### 最终 gates 与完成判定

所有源码检查对应同一 HEAD 的本地 diff 和上述 827 路径快照，最终逐项 SHA-256
及新路径集合核验无漂移。构建统一使用 `CARGO_BUILD_JOBS=2 CARGO_INCREMENTAL=0
CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0`；日志保留在本次 `/tmp`，
不代表已提交、hosted CI 或真实 provider 资格。

| gate | 终态与证据 |
| --- | --- |
| `cargo nextest run --all-features --no-fail-fast --test-threads 4` | 退出 0；4210 passed / 22 skipped，73 binaries，10 slow，860.095s；`/tmp/bitrouter-g06-nextest-bounded.log` |
| `cargo clippy --all-features` | 退出 0，2m46s（含构建锁等待）；仅既有 proc-macro-error2 future-compat 提示；`/tmp/bitrouter-g06-clippy-verified.log` |
| `cargo test --all-features --doc` | 退出 0；6 passed / 1 ignored；`/tmp/bitrouter-g06-doctest-final.log` |
| `cargo fmt -- --check` | 退出 0；`/tmp/bitrouter-g06-fmt-final.log` |
| `cargo run -p dist-helper -- check` | 退出 0，schema 与 registry 产物均 up to date；生成 schema 相对 G05 字节不变；`/tmp/bitrouter-g06-dist-check.log` |
| 全 target / 全 features normal/build/dev 依赖图与 metadata | 均退出 0；7 个成员、无 TUI/Ratatui/Crossterm，Cargo.lock 只增加 app 对既有 shell-words 的直接引用，无版本升级；`/tmp/bitrouter-g06-dependency-tree.log` |
| 实际 binary 输出 | 8 组 help 与 7 组 CRUD/JSON/human 通过、无 ANSI、秘密值未进入输出，临时 home 仅有来源文件和 publication lock；`/tmp/bitrouter-g06-help-and-crud-evidence.log` |
| 文档 / 分发 / `git diff --check` | 收尾退出 0；43 份 Markdown / 544 个当前文件链接、12 张卡/提示词、A01–A26、199 行 skill 和 3 个 JSON manifests 与阶段状态一致 |

代码量：766 个 Rust 文件、440,732 物理行，含测试、注释与空行；相对 G05 增加
1,387 行，相对 G00 的 468,581 行仍净减 27,849 行。不以行数判断完成。

完成判定：G06 卡片及 A03/A05–A07/A24 本阶段切片通过，C05/T07 保持项回归通过；
没有本阶段 required work 遗留。G06 结束时 G07–G11 尚未开始；G07 后续启动状态见第 12 节。
普通 block YAML 可无损修改；其他布局/链接目标明确拒绝，原有 gateway/relay/check
仍可用。真实服务、hosted CI 与其他平台资格按第 16 节单列，目录同步后的保存结果
不确定性按上面的 publication 边界处理。


## 12. G07 执行记录（本地完成）

- 起始 HEAD：`54c822a34d4dbfd75d484ca35e108b497048efef`，保留 G00–G06 全部本地差异。
- G06 前置证据：827 路径最终源码快照无漂移；最终 4210 passed / 22 skipped
  与 Clippy/doctest/fmt/dist 日志仍适用于起点。备份见
  `/tmp/bitrouter-g07-baseline/` 和 `/tmp/bitrouter-g07-baseline.json`。
- 范围：本地/Git 来源、完整不可变 bundle、项目/用户 scope、安装清单、CLI
  install/list/show/update/uninstall；保留 init 与 unmanaged 检查。
- 对应 A08–A12/A24 的安装切片；第一版保留卸载后的历史版本，不新增 GC/lease
  或宣称运行绑定/技能激活完成。选择与真实绑定仍属于 G09。
- 实现选择：Git 只读取固定 commit 的对象，不 checkout、递归 submodule 或调用
  安装 hooks/scripts；单个清单的原子提交同时发布 receipt 与 active alias，
  版本目录先准备完整再发布，未登记临时目录不属于候选。
- 根 AGENTS 中“BitRouter 本身不安装 skills”为旧产品事实；本次用户 G07 目标
  明确要求 bro 安装，已同步修订该事实并保留仅分发 shippable skills 的约束。
- 当前检查点：G07.1–G07.4 本地完成；实际证据与最终 gates 见下。
- 不进入 G08，不提交/推送，不改用户真实 unmanaged 内容或全局 harness 配置。

### 验证过程与保留的失败

早期 v0.4 文档整理仅核对当时日志。以下保留历史结果与修复过程；最终完成
判定采用本节后面的冻结源码 gates，不以初步结果代替。

- 实际 CLI lifecycle 测试：`cargo test -p bitrouter --all-features --test skills_lifecycle -- --nocapture`，
  **8 passed / 1 failed / 0 ignored，未通过**。日志：
  `/tmp/bitrouter-g07-lifecycle-first.log`。
- 已通过的初步用例包括完整 bundle/更新/卸载保留、scope/unmanaged、格式/配额、
  链接与已安装内容冲突、Git branch/tag/commit、获取失败、hooks/filter 不执行，
  以及两个实际 CLI 同时更新时仅一个清单提交成功。
- 失败用例：`project_root_workspace_override_foreign_index_and_init_remain_explicit`，
  首个项目根路径比较断言失败，后续断言尚未执行。接续时先核对 canonical 路径
  与 fixture 路径的表达及实际 workspace 归属，不通过放宽 scope 隔离来修复。
- 当时待完成：修复并重验该用例，审查安装事务与配额边界，同步实际 CLI 的
  skill/manifests/AGENTS/reference，记录 G07 交接格式并运行最终完整 gates。
  当时 A08–A12/A24 尚未根据最终证据更新；最终切片状态见第 4 节及下文。

后续进展：路径断言已按 canonical 项目根修正，原 9 项全部通过。增加清单提交
失败/孤立完整版本、重装身份、四类全局 unmanaged 根、损坏 receipt、Git
链接/submodule/配额与终端选择/取消后，定向验证 **12 lifecycle + 4 PTY passed**，
退出 0；日志 `/tmp/bitrouter-g07-targeted-final.log`。原始失败保留，不用新结果覆盖。
Clippy 指出的一处新增 collapsible-if 已修正；832 路径的最终源码快照见
`/tmp/bitrouter-g07-source-checkpoint-final.json`。完整 gates 的最终终态见下文。

首次冻结源码全量运行使用 4 个 test threads，结果 **4223 passed / 1 failed /
22 skipped**，退出 100；总命令 1345.399 秒，测试执行 1098.431 秒。完整终态见
`/tmp/bitrouter-g07-nextest-initial.log`，当时源码与 gates 分别保留在
`/tmp/bitrouter-g07-source-checkpoint-initial.json` 和
`/tmp/bitrouter-g07-verification-record-initial.json`。唯一失败为
`mcp_config_pty::guided_add_and_eof_leave_canonical_terminal_and_json_output`
失败：取消进程的 stdout 为空，JSON 解析报 EOF。源码定位到测试使用的
`wait("HTTP URL")` 同时匹配上一层菜单的 `Streamable HTTP URL` 说明，可能在真正
输入步骤前发送 Ctrl-C。同步点已收紧为实际的 `HTTP URL:` 提示；生产超时、
权限与退出契约未改。后续新增 FIFO 拒绝/非阻塞打开、卸载后 valid 检查、Git
fixture 全局配置隔离，以及空 CODEX_HOME 不得成为 cwd 全局根的隔离用例。

更新后的源码定向验收 **16 passed**，退出 0，日志
`/tmp/bitrouter-g07-targeted-final.log`。FIFO setup 曾误用 Rustix 在当前 Apple
目标不提供的 mkfifo/mkfifoat，两个编译失败日志保留为
`/tmp/bitrouter-g07-targeted-fifo-api-failure.log` 和
`/tmp/bitrouter-g07-targeted-fifo-apple-api-failure.log`；现用系统 mkfifo 创建测试
数据，无 shell/Python peer 或安装脚本执行。

重复验证脚本首次直接执行 test binary 时未继承 Cargo 的工具链环境，导致
sqlx 的 rust-toolchain.toml 选择 Rust 1.78，编译因 check-cfg 不兼容而失败。
这不是 MCP 业务终态；日志保留在
`/tmp/bitrouter-g07-pty-stress-compiler-environment-failure.log`。脚本现固定本次
stable rustc/rustdoc；误下载的 1.78 toolchain 已卸载，原有默认工具链保留。

本地磁盘构建余量降到约 2 GiB 后，仅清理本工作区中已有新版本替代、未打开、
单链接的 43 个旧 test executable cache，回收 4,166,171,312 字节。源码、证据、
库与当前测试产物均保留；计划/结果为 `/tmp/bitrouter-g07-cache-reclamation-*.json`。

固定 stable 工具链后，原 MCP 取消用例连续 **10/10 passed**，每次均运行实际
CLI、EOF/Ctrl-C 与终端恢复断言；日志为
`/tmp/bitrouter-g07-pty-stress-01-final.log` 至 `-10-final.log`。新版 16 项定向
验收覆盖空 CODEX_HOME、FIFO、retained validity 等增强，仍全部通过。
最终 gates 使用相同的 832 路径源码快照并在每个检查前后验证无漂移；完整终态
记录为 `/tmp/bitrouter-g07-verification-record.json`。统一执行 session `67081`
已退出 0，所有检查均已结束，最终结果如下。

独立交付证据：`/tmp/bitrouter-g07-cli-review-evidence.json` 记录实际
`target/debug/bro` 的 SHA-256、冻结源码引用、14 条命令的 argv/退出码及原始
stdout/stderr。包含 6 个动词的 help、初始 unmanaged inventory、本地安装、
按安装 ID show、human 输出、references-only 更新、卸载及新进程检查两份 valid
retained 版本、global 查询；unmanaged sentinel 原字节保持，activation 为
not_requested。临时目录在验证后清理，不涉及用户真实配置或 daemon。

### G07 要求与实际证据入口

主要业务证据均通过实际 `bro skills` 子进程，见
[lifecycle 验收](../apps/bitrouter/tests/skills_lifecycle.rs)；终端行为复用
[Cargo-built terminal fixture 验收](../apps/bitrouter/tests/mcp_config_pty.rs)。
以下描述已取得的业务证据；同一冻结源码的完整 gates 均已通过，终态见下文。

| 要求/切片 | 已验证的业务结果与实际用例 |
| --- | --- |
| A08 本地/Git 与标准格式 | `invalid_ambiguous_and_oversized_sources_publish_nothing`、`git_root_bundles_multiple_selection_and_fetch_failure_keep_aliases`：多来源/相对选择、root bundle、无效 frontmatter/名称、非终端歧义和获取失败有明确错误，无 alias 发布 |
| A08 路径/配额 | 本地与 Git 的字节/普通文件数、SKILL.md 上限、越界 symlink、Git gitlink/submodule 与 FIFO 特殊文件拒绝；目录与路径限制由 bundle/Git 元数据校验执行 |
| A09 完整与无执行 | `local_complete_snapshot_updates_and_retains_versions_after_uninstall_and_restart`、`git_hooks_filters_and_bundle_scripts_are_data_only`：binary asset、空目录、references、可执行 Cargo payload 与 attributes 原样保存，payload/hook marker 不出现 |
| A10 ref/来源与不变版本 | `git_branch_tracks_exact_commits_tags_and_commits_stay_pinned`：branch 精确新 commit、无变化 revision 不动、移动 tag 不改 pin、明确新 ref、来源消失后 pin 仍可离线检查 |
| A10 来源/内容冲突 | 无效更新、丢失 repo/ref、已安装内容改写/链接均拒绝覆盖；逐字节确认旧 index、用户源文件与旧版本保持 |
| A10 CAS/创建事务 | `simultaneous_git_updates_compare_and_swap_one_manifest` 用 Cargo Git gate 同时准备两个实际 CLI 更新，仅一个提交；`skills::managed::tests::stale_initial_installer_cannot_replace_a_new_scope_manifest` 补齐清单首次创建的 stale writer |
| A10 提交失败 | `failed_manifest_commit_keeps_alias_and_reinstall_retains_distinct_receipts`：完整新版本已发布后 revision 耗尽，旧 alias/index 逐字节保持；孤立版本不被列为已安装，显式重试可复用 |
| A11 安装部分 | 卸载移除 alias；新 CLI 进程 show 的 retained 版本仍 valid，旧/新目录与 receipt 保留；重装产生新 ID 并保留原 receipt。运行/恢复任务绑定仍待 G09 |
| A12 CLI scope | 同名项目/用户安装 ID 独立，最近 Git root、明确 workspace、错误 principal/外来 index 拒绝；空 CODEX_HOME 不成为 cwd 的全局根；global/workspace flags 互斥 |
| A12 unmanaged | 项目兼容来源和 HOME .agents/.codex/.claude、CODEX_HOME 来源独立保留；修改/卸载不触碰其原字节，managed library 不能放入其全局 scan root |
| A03 交互/输出 | 真实 help/JSON/human；多来源无 TTY 明确报错；PTY 编号选择、EOF/Ctrl-C、无 alternate screen、独立 JSON stdout 和 termios 恢复通过 |
| A24 同步 | CLI reference、主 skill 200 行/深度 reference、三个 plugin manifests、AGENTS、README/development 同步；init 只生成源文件，未宣称 workflow selection/activation/delivery |

对应实现分别位于 [bundle](../apps/bitrouter/src/skills/bundle.rs)、
[source](../apps/bitrouter/src/skills/source.rs)、
[managed store](../apps/bitrouter/src/skills/managed.rs) 与
[CLI](../apps/bitrouter/src/skills/cli.rs)。仍复用原 parser、publication lock 与
atomic writer；没有新增执行服务、任务数据库或 GC/lease。

### G07 → G09 的实际安装契约

- app 的私有 `skills/managed.rs` 维护 `installed.json` format 1：`revision`、
  `scope {kind, root, library, principal}`、`aliases {name: installation_id}` 与
  `installations {installation_id: receipt}`。ID 使用 `skill_<UUID>`。
- receipt 为 `id/name/description/current/versions`；每个版本包含 `bundle` 与
  `source`。bundle 是 `digest/bytes/directories/files`，file 是
  `path/bytes/digest/executable`；严格校验路径、排序、配额与整体摘要。
- 摘要使用 `bitrouter-skill-bundle-v1` 域，覆盖有序目录和每个文件的相对路径、
  大小、内容 SHA-256 与执行位。二进制 assets、未知 frontmatter 字段与空目录保留；
  Git 管理数据和当前安装管理目录不进入源快照。
- `source.kind=local` 保存绝对来源根与 `skill_path`；`kind=git` 保存 repository、
  requested/resolved ref、commit、tracking 与 skill_path。固定 ref 更新无需联网；
  branch 更新仍保存原请求 ref 的语义。历史来源目录删除不影响已安装版本读取。
- 唯一的已安装候选登记是 committed `aliases`；临时目录、无 alias 的 receipt、
  清单提交失败留下的完整版本均不能自动进入候选。更新前及锁内提交前重验目标
  历史 bundle；同一事务提交 receipt 与 alias。卸载保留版本和 receipt，重装新 ID。
- 项目/用户 scope 和本地 principal 是安装归属。G09 仍须由真实 daemon owner
  独立校验调用者/workspace 可见范围，不能把磁盘上的字符串当成授权凭据。
  当前没有 managed catalog runtime 接入、运行绑定、GC、选择或 activation 服务。

### Spec v0.4 文档修订

- 新增保留/移除/重构表，明确 onboarding 资源事务复用和 G11 的最终接入责任。
- 明确 `init` 与安装身份、第一版保留版本、installed/selected/activated/delivered
  的证据边界；没有据此声称新的运行能力已完成。
- Goal 计划补充跨阶段交接及 A 项切片、G07 四个检查点、脏工作区的源码快照
  和串行完整检查约定；本次只完成 G07，不另建或自动推进 goal。
- 当次文档修订检查通过：4 份文件、97 个本地链接、26 个 A 项、12 个 R 项、12 个阶段卡片
  和 12 条 goal 提示词；围栏/结尾换行/尾随空白及 `git diff --check` 通过。
  与修订前 1,191 个工作区路径的内容快照比较，只有上述 4 份文档变化，运行代码
  与其他既有编辑保持原样。修订前快照：`/tmp/bitrouter-cli-spec-v04-review-baseline.json`。

### 最终 gates 与完成判定

所有源码检查对应 HEAD `54c822a34d4dbfd75d484ca35e108b497048efef` 加本地差异。
最终源码清单包含 832 个路径（781 个存在、51 个已删除），清单文件 SHA-256 为
`e7e70404ce73120472523c1bd0b42e757a19650127005fbb2054be0f1e578785`；逐路径内容摘要
保存在 `/tmp/bitrouter-g07-source-checkpoint-final.json`。全部检查前后及文档收尾时
均确认零漂移，未使用旧源码的成功结果覆盖本次失败。

| 检查 | 实际终态 | 日志 |
| --- | --- | --- |
| `cargo nextest run --all-features --no-fail-fast --test-threads 4 -E 'binary(mcp_config_pty) \| binary(skills_lifecycle)'` | 16 passed，退出 0；含构建 494.268 秒 | `/tmp/bitrouter-g07-targeted-final.log` |
| 编译后的 `mcp_config_pty` 实际取消用例重复执行 | 10/10 passed，全部退出 0；含实际 CLI、EOF/Ctrl-C、JSON 与终端恢复 | `/tmp/bitrouter-g07-pty-stress-01-final.log` 至 `-10-final.log` |
| `cargo clippy --all-features --all-targets` | 退出 0，77.335 秒 | `/tmp/bitrouter-g07-clippy-final.log` |
| `cargo fmt -- --check` | 退出 0，2.905 秒 | `/tmp/bitrouter-g07-fmt-final.log` |
| `cargo test --all-features --doc` | 退出 0，70.497 秒 | `/tmp/bitrouter-g07-doctest-final.log` |
| `cargo run -p dist-helper -- check` | schema/registry 均 up to date，退出 0，3.293 秒 | `/tmp/bitrouter-g07-dist-final.log` |
| `cargo nextest run --all-features --no-fail-fast --test-threads 4` | 74 binaries，4224 passed / 22 skipped，退出 0；测试 703.842 秒，含构建 917.281 秒 | `/tmp/bitrouter-g07-nextest-final.log` |
| `cargo tree --locked --offline --all-features --target all --edges normal,build,dev --prefix none` | 退出 0；无 bitrouter-tui、Ratatui、Crossterm | `/tmp/bitrouter-g07-dependency-tree-final.log` |

完整 nextest 于 `2026-10-19T20:06:08.358Z` 开始，`20:21:25.646Z` 结束。
检查固定 stable-aarch64-apple-darwin 的 rustc/rustdoc、2 个 build jobs、4 个 test
threads、关闭 incremental 和 dev/test debug info；没有更改用户默认工具链。
Clippy 保留起点已有的 `onboarding_pty.rs:244,324` 两处 byte string 建议；构建
仍有已记录的 Apple linker unwind 提示和 proc-macro-error2 future-compat 提示。
G07 未增加 allow/忽略检查来通过验收。

14 条独立 CLI 操作验证 help/JSON/human 与实际安装、更新、卸载、retained 查询；
源码与依赖图核验确认原 C01–C08/T01–T15 能力边界保留，C05/T08 的旧有发现/
读取接口仍由原消费者调用。orchestrator 只共用格式校验与有界读取，没有改变
执行 owner、权限、ACP/MCP 协议或提前接入 workflow 选择。Cargo.lock 与 G07 起点
一致，schema 与 G06 最终版本一致。

CLI 文档、README/development、200 行主 skill 与深度 reference、三个 plugin
manifests、AGENTS 及四份 spec/goal/状态索引已同步。最终文档检查核对本地链接、
围栏、换行、空白、manifest JSON，以及 26 个 A 项、12 个 R 项、12 个阶段卡片和
12 条 goal 提示词；`git diff --check` 通过。已有网站路由
`/docs/guides/cloud-api` 不作为本地文件验证，也未据此宣称网站验收。

G07.1–G07.4 均本地完成，A03/A08/A09/A10 及 A11/A12/A24 的安装器切片满足；
A11/A12 的总体状态保持 partial，运行版本绑定、恢复与 daemon 授权候选由 G09
继续验证。22 项 skipped 保留原有 adapter/provider 资格边界；本地 Git fixtures
不证明远端认证，完整本地通过也不证明 hosted CI、真实 provider/harness 或其他
操作系统资格。没有本阶段 required work 遗留；G07 结束时 G08–G11 尚未开始；G08 后续进度见第 13 节。

### Spec v0.5 MCP Skills 对齐修订（仅文档）

用户要求按规范核对建议更新设计。本次复核官方 overview/stable specification
及当前源码；保留 G07 已完成的安装范围，未启动或推进 G08/G09 goal。

- 增加 [独立验收附件](CLI_ONLY_MCP_SKILLS_ACCEPTANCE.md)，区分本地安装器、
  gateway、native host、外部 adapter 与继续移除的本地 origin；A27/A28 分别
  跟踪协议与 host，M01–M10 提供实际 fixture 的完成条件。
- 已核对的缺口：`rmcp_executor::dispatch` 的扩展分支只检查方法白名单，
  `send_custom_request` 直接发请求；当前连接未先通过上游 Skills/Resources/
  directoryRead 声明门控。G09.1 要求证明未声明能力时零对应请求及重连撤销。
- 当前 gateway 已有 capabilities、entry/URI 命名空间、分页与缓存处理；这是
  已有源码基础，不把历史 conformance 标签或 G07 全量通过冒领为 A27 新证据。
- G09 按六个检查点先完成协议，再实现明确获许可的 MCP 来源 host 加载，
  然后做 workflow/native 选择、Turn 绑定、实际请求和恢复。G10 另验外部交付，
  G11 汇总 A01–A28/M01–M10；12 个阶段编号保留，G08 宿主迁移范围不变。
- 固定远端身份、完整 held entry、按需读取/逐次校验、原 server/manifest
  约束、内容绑定批准、host 执行/嵌套激活同意、隔离缓存和重启来源保留。
  第一版 host 拒绝 dynamic；gateway 仍可转发。未新增 MCP URI 安装或本地发布。
- 更新旧 MCP 文档的角色说明，保留原实施记录和 OSS origin 边界；新 native
  host 契约为目标状态，未宣称执行已实现或外部互操作已通过。

本次仅修改设计/计划/证据索引与相关旧文档说明；没有运行代码、CLI、默认配置、
skill 或 plugin manifests 变更。文档检查命令
`node /tmp/bitrouter-mcp-skills-spec-check.cjs` 与 `git diff --check` 均退出 0：
7 份文档，124 处本地链接（含 2 个章节锚点）、28 个当前 A 定义/证据行、13 个
R 决策、12 个阶段/goal 提示词、10 个 M 项及 6 个 G09 检查点均一致；围栏、
换行与尾随空白通过。旧 MCP 文档仅验证新增行的链接，既有 origin 源码链接作为
历史保留；未把外部网站链接或外部互操作计为本地文档通过。
对修订前 1,191 路径快照比较，仅上述 7 份文档变化，其余内容/删除状态保持。
快照与结果为 `/tmp/bitrouter-mcp-skills-spec-update-baseline.json` 和
`/tmp/bitrouter-mcp-skills-spec-validation.json`。初次检查误将历史 A 表计入当前
矩阵而失败，原记录保留在 `-validation-initial.json`；修正为只计第 4 节后通过，
没有为了满足计数改写历史记录。
此前 4,224 passed / 22 skipped 是 G07 冻结源码的历史证据，本次没有重跑或新增
协议、host、provider、跨平台通过声明。

## 13. G08 执行记录（本地完成）

- 起始 HEAD：`54c822a34d4dbfd75d484ca35e108b497048efef`；保留 G00–G07 及
  v0.5 文档修订全部差异。1,192 路径起点摘要/备份为
  `/tmp/bitrouter-g08-baseline.json` 与 `/tmp/bitrouter-g08-baseline/`。
- G07 的 832 路径源码快照在 G08 起点核对零漂移；先前完整 gate 只适用于
  起点，不能用于后续 G08 改码的完成声明。起始检查点为 G08.1 身份契约核对。
- 对应 A19/A23、相关 A18/A26 的宿主迁移切片。保留原本地 caller、显式 workspace/
  mode、session name 对应的 `bro_managed_sessions` CAS/journal 和既有 Core authority。
- 起点源码确认：产品 CLI 自行 build App/open NativeSession；HTTP ManagedCoreApi
  是远端 Harness channel，不能替代本地 NativeSession/NativeResources 的运行归属。
  目标由 daemon 持有原执行对象，CLI 只提交/查询/取消；不建镜像任务库。
- `NativeSession::open` 只接续 released=true 且 workspace/mode 匹配的 journal，
  保留 session/harness ID 并增加 epoch；lost owner、未知效果与未确认释放仍拒绝。
  迁移不得自动退役旧 owner、换数据库或把状态查询解释成重新执行。
- 开始时计划先固定协议版本、daemon instance、操作键与 ACK/取消/释放证据，再
  实现真实 CLI/daemon/模型与工具 fixture；当时尚无 G08 测试通过声明。
- 不进入 G09，不修改 MCP Skills 协议/host 选择，不提交、推送或使用真实服务。

### G08.1/G08.2 实现与初步证据（历史过程）

- 产品 CLI 已移除 build App、数据库打开和 NativeSession 执行；新增独立
  owner-local managed v1 endpoint，保留 Thread v18/HTTP managed channel。
  daemon 通过原 NativeSession/DatabaseNativeStore 执行，仍为 local caller。
- 复用 Core 持久 input receipt 验证同操作键/输入；live registry 只持有驱动，
  不写镜像任务库。干净 released journal 才可接续；未确认释放不接管。
  session/harness ID、原 row name、workspace/mode 和递增 epoch 保留。
- 新增 `task managed-status` 冷查询与绑定原 boot/input 的 `managed-cancel`；
  原提交增加 request-id/server-instance，NDJSON 原字段保留并补充身份。CLI
  断开不取消；Ctrl-C 请求原任务取消。daemon 生命周期负责取消与 join，即使
  某个 ingress 提前失败也不能丢掉 native driver 的清理所有权。
- connected agents 增加实际 `managed_native_session` 投影，冷/未释放 journal
  不等同于 live owner。NativeSession 的原 open/run/close API 与 journal format
  继续保留；新增被 daemon 实际消费的分离 start/drive、取消准备和被动 inspect。
- command frame 允许 4 MiB，以保留原 2 MiB native input 加元数据；reply 使用
  原 native store 上限。没有把原输入偷偷缩为复制 Thread transport 的 64 KiB。
  当前能力声明最多 4 个 active native sessions，超限在开资源之前拒绝。

首次 cargo check 退出 101：host 的 server_instance_id 移入 future 后仍用于
locator；已复制到独立 managed_instance。日志 `/tmp/bitrouter-g08-check-first.log`。
首个进程测试初步失败：fixture 对 stream=false 返回 SSE，观察到原 managed
生成路径要求普通 JSON；只修正 fixture response，生产语义未改。诊断辅助曾把
wiremock::Request 当 Serialize 编译失败，改为显式字段。原终态日志分别为
`/tmp/bitrouter-g08-managed-daemon-first.log`、`-diagnostic.log` 和
`-diagnostic-fixed.log`；原因/原 checkpoint 保留于
`/tmp/bitrouter-g08-first-runtime-diagnostic.json`。

修正后的第一条真实 CLI/daemon 用例 **1 passed**，退出 0；日志
`/tmp/bitrouter-g08-managed-daemon-json-fixture.log`。它覆盖重复输入不新增模型
调用、改输入冲突、released 接续同 session/harness ID 并增加 epoch、旧 boot
拒绝、重启后同键不重放和历史上下文保留。后续源码扩展仍须重验。

三项定向结果 **2 passed / 1 failed**：实际 submit ACK 丢弃后只查原键、不再次
submit，以及身份/重启用例通过；已产生 shell 效果的取消用例被测试错误地要求
released。原 shell 明确将 launched/cancelled 的效果保留 Unknown，实际返回
RecoveryRequired 与 released=false，不能为通过测试放宽。已把该场景改为验证
EffectUnknown、不释放、不重放，并保留既有 pending-approval 无效果取消可释放
的独立 library 测试。失败日志 `/tmp/bitrouter-g08-managed-daemon-controls.log`。

首次 G08.3 扩展的 CLI/daemon、原生库与 connected agents 定向检查使用：
`cargo test -p bitrouter --all-features --test managed_daemon --test managed_native --test connected_agents -- --nocapture --test-threads 4`。
日志 `/tmp/bitrouter-g08-targeted-ownership.log`；当时未据初步结果完成阶段，仍须
取得实际终态、修复/补齐业务证据、完整 gates、分发/文档与源码范围审计。

上述第一次扩展检查实际为 connected agents **2 passed**、managed daemon
**9 passed / 1 failed**；cargo 退出 101，后续 managed_native 未运行。失败发生于
测试在已发 Stop 后继续向正在关闭的 daemon 请求 Status，得到 EOF；不把它当成
生产任务成功或修改生产超时。临时 fixture 改为等自己的 socket/PID 文件清理，
持有 Child 的路径仍等待原进程退出。修正后的同命令退出 0：**2 + 10 + 5 passed**，
分别为 connected agents、managed daemon、managed_native；日志
`/tmp/bitrouter-g08-targeted-final.log`。同时覆盖模式/workspace 不匹配时原 row
无变化、完成后的精确重复取消、human 状态报告、只读工具集和输出上限。
最后另补 CLI model/effort/check 向原 checkpoint 传递及 verification failure
的真实路径；该新增测试与全部冻结源码还需取得最终完整 gate。

该新增测试初次退出 101（`/tmp/bitrouter-g08-verification-options.log`）：错误
假定 shell 正常返回 exit 7 就会产生 Failed outcome。源码核对确认 shell 返回
Json(exit_status)，原 NativeResources 按 Completed + 非 ErrorJson/ErrorText 记作
Succeeded；Core verification 据工具 outcome 判定。G08 保留原执行层判定，不改
全局 shell 或 Core 语义；把非零退出码限制明确写入 CLI/spec/skill。测试改为证明
原始 exit_status=7、model/effort/output/check 参数和 verification invocation 均在
原 checkpoint，并另验无法执行的空检查导致 task failed 且可确认释放。

修正后的参数/verification 用例退出 0、**1 passed**，日志
`/tmp/bitrouter-g08-verification-options-corrected.log`。完整回归的 **836 路径**
摘要保留于 `/tmp/bitrouter-g08-source-checkpoint-full-suite.json`；后续仅整理两处
测试 helper 返回语句，最终源码摘要为 `/tmp/bitrouter-g08-source-checkpoint-final.json`。
G08.1/G08.2 与以下 G08.3 切片已有真实本地证据；最终 gates 与差异审计见下文。

| 要求切片 | 真实入口与身份 | 定向测试（managed_daemon.rs，未另注者） | 已验证范围 / 未覆盖 |
| --- | --- | --- | --- |
| A19 宿主、默认与启动 | CLI → selected daemon managed v1 → 原 NativeSession/SQLite row | cli_startup_and_passive_status_use_the_selected_daemon_without_embedded_fallback | 自动启动/被动 offline 查询；workflow 选择待 G09 |
| A23 session/store 接续 | 原嵌入式库写 format 1，CLI 由 daemon 接续 | original_embedded_journal_continues_in_daemon_without_identity_migration | 原 session/harness、workspace、epoch、上下文保留；无数据库迁移 |
| A19/A23 幂等、重连 | 显式 request-id、原 Core input receipt、daemon boot | daemon_cli_retries_and_restarts_preserve_native_identity_without_replay | 重复/改输入、旧 boot、重启、历史 receipt、模式/workspace 拒绝、human；模型调用计数证明无重放 |
| A23 submit ACK 不明 | 实际 socket bridge 丢弃一个 Submit reply | dropped_submit_ack_queries_original_operation_without_resubmit | 一次 Submit/一次模型请求；仅查原键 |
| A23 durable ACK 不明 | 原 NativeStore 已写 input.accepted 后返回 CommitStatus::Unknown | durable_input_ack_uncertainty_remains_blocked_without_replay_or_forced_release | 原 row 不变、零 provider 调用、不接管/不重放 |
| A18/A26 断线与精确取消 | killed CLI、重复 observer、live agents 与 boot/input cancel | detached_cli_keeps_one_driver_and_cancel_preserves_unknown_effects | 一个 driver、一个真实 shell start；错误键/boot 拒绝；未知效果不释放 |
| A23 daemon 丢失 | SIGKILL 仅测试拥有的 daemon，再开同配置 | abrupt_daemon_loss_keeps_original_journal_and_effects_recovery_blocked | 原 grant/row 不变，无 live owner；拒绝旧/新输入接管 |
| A18/A26 终止清理 | 真实 SIGINT 与 graceful Stop | ctrl_c_and_daemon_shutdown_join_native_effects_without_forcing_release | 取消/join 不伪造 release；未知效果仍阻塞 |
| A19 资源准入 | 四个独立 workspace/session 同时准入，第五个提交 | managed_capacity_rejects_a_fifth_owner_before_native_admission | v1 声明 4；第五个无 row/receipt/provider 调用 |
| A26 参数/verification 兼容 | 真实 CLI flags → daemon → 原 checkpoint 与 shell invocation | cli_options_and_verification_evidence_remain_in_the_original_native_checkpoint | model/effort/output/check 原值、exit_status=7 原行为、未执行检查失败且释放；不宣称修复非零退出码语义 |
| A18/A23/A26 库与既有 owner | 原公开 NativeSession open/run/close、数据库 CAS、连接快照 | managed_native.rs 的 5 项；connected_agents.rs 的 2 项 | 无效果取消可释放、拒绝写入、workspace fence、commit 取消无死锁、durable 接续与旧连接投影保留 |

五条真实二进制 help 命令均退出 0，未写空临时 HOME；flags/command 名称见
`/tmp/bitrouter-g08-help-evidence.json`。JSON/status、human 与 foreground NDJSON
由上述实际 CLI journey 验证；原 managed_session/terminal 字段保留，新增身份。

### G08.4 最终 gates 与范围审计

完整回归 `cargo nextest run --all-features --no-fail-fast --test-threads 4`
退出 0：**4,235 passed / 22 skipped**，测试运行 826.114 秒，日志
`/tmp/bitrouter-g08-nextest-final.log`。22 个既有 skipped 未增加；真实 adapter/
provider、hosted CI 和其他平台仍按第 16 节单列。全量源码摘要 SHA-256 为
`e89bca0a55cef0415eeaf053927faebeac476b23039e8a4278495b2f3116c57f`。

首次 all-targets Clippy 退出 0，保留于 `/tmp/bitrouter-g08-clippy-before-test-tidy.log`；
除已有 onboarding_pty 的两条 byte_char_slices 建议，还指出本次测试 helper 的
两处 Ok(...?) 可简化。只移除等价的返回包装，无生产源码变化，没有使用 lint
抑制。完整回归之后的唯一源码差异是 managed_daemon.rs 的这两处表达式；
`/tmp/bitrouter-g08-post-suite-audit.json` 与 `-test-only.diff` 保存范围/原 diff。
受影响的整个 integration target 已重验：**11 passed，退出 0**，日志
`/tmp/bitrouter-g08-managed-daemon-post-clippy.log`；未重复无变化的全库业务测试。
最终格式检查已退出 0，日志 `/tmp/bitrouter-g08-fmt-final.log`。
最终源码摘要 SHA-256 为
`8da09982add8925c58d46f3a0d148851f8c8bd507d6e26e9d4af995f121d6953`。

| 最终检查 | 实际结果 | 日志 / 说明 |
| --- | --- | --- |
| cargo nextest run --all-features --no-fail-fast --test-threads 4 | 退出 0；4235 passed / 22 skipped | `/tmp/bitrouter-g08-nextest-final.log`；完整回归快照及后续仅测试差异见上文 |
| cargo test -p bitrouter --all-features --test managed_daemon -- --nocapture --test-threads 4 | 退出 0；11 passed | `/tmp/bitrouter-g08-managed-daemon-post-clippy.log`；最终测试源码 |
| cargo clippy --all-features --all-targets | 退出 0；本次新增代码无警告 | `/tmp/bitrouter-g08-clippy-final.log`；保留已有 onboarding_pty 两条 byte_char_slices 和第三方 future-compat 提示 |
| cargo fmt -- --check | 退出 0 | `/tmp/bitrouter-g08-fmt-final.log`；两处测试整理后执行 |
| cargo test --all-features --doc | 退出 0；6 passed / 1 ignored | `/tmp/bitrouter-g08-doctest-final.log` |
| cargo run -p dist-helper -- check | 退出 0；schema/registry dist up to date | `/tmp/bitrouter-g08-dist-final.log`；未生成新差异 |
| cargo tree --workspace --all-features --target all --edges normal,build,dev --locked --offline | 退出 0；无 bitrouter-tui/Ratatui/Crossterm | `/tmp/bitrouter-g08-dependency-tree-final.log` |

本节 Cargo 命令统一使用 `CARGO_BUILD_JOBS=2`、`CARGO_INCREMENTAL=0`、
`CARGO_PROFILE_DEV_DEBUG=0`、`CARGO_PROFILE_TEST_DEBUG=0`，并固定
`RUSTUP_TOOLCHAIN=stable-aarch64-apple-darwin`，RUSTC/RUSTDOC 分别指向
`/Users/kelsen/.rustup/toolchains/stable-aarch64-apple-darwin/bin/rustc` 和同目录的
`rustdoc`。不把本地 fixture 或编译成功提升为 hosted/provider/跨平台资格。

最终阶段/文档检查脚本 `/tmp/bitrouter-g08-check.cjs` 核对 28 个 A 项、13 个 R 项、
12 张 goal 卡片/提示词、10 个 M 项、6 个 G09 检查点、链接/anchors、JSON manifests、
200 行 skill、源码快照与 `git diff --check`；结果见
`/tmp/bitrouter-g08-scope-docs-validation.json`。相对 1,192 路径起点，改动仅在明确的
23 路径内（11 个 Rust 文件、文档/skill 与 3 个 manifests）；其余既有编辑保留。
Cargo manifests/lock、schema/default、原存储表和 journal 格式不变；新增 Rust 无
allow/expect/panic/unwrap 或 pub-use 绕过，证据见 `/tmp/bitrouter-g08-source-audit.json`。

G08.1–G08.3、A19/A23 与相关 A18/A26 的宿主迁移切片完成，本阶段无 required
work 遗留。`--check` 非零 shell 退出码的既有判定限制保留并已明示，未声称修复。
不提交/推送、不进入 G09；A27/A28 与 workflow 选择仍为 not_run/未实现。

## 14. G09 执行记录（本地完成）

本节按执行顺序保留各次源码、失败和当时的进度；下文早期小节的“进行中/待
完成”是对应快照的历史状态。当前结论以第 2/4 节和末尾 G09.6 的最终证据为准。

- 起始 HEAD：`54c822a34d4dbfd75d484ca35e108b497048efef`；G08 的 836 路径最终
  源码快照核对零漂移。保留 G00–G08 与 v0.5 全部本地差异，不提交/推送。
- G09 起点 1,197 路径摘要与普通文件备份为 `/tmp/bitrouter-g09-baseline.json`
  和 `/tmp/bitrouter-g09-baseline/`。原有通过记录只证明起点，不证明新增能力。
- 官方 ext-skills stable 规范固定 commit
  `c1aa87e4c381c82b99aa37471d0eae4ab50f9985`，正文 SHA-256
  `6e33a33762783a02dc4c13c889dda5b8d7c525649012cf77da12d80c04ce6851`；
  本地原文与 revision 证据为 `/tmp/bitrouter-g09-skills-spec.mdx`、
  `/tmp/bitrouter-g09-skills-spec-revision.json`。规范基于 MCP 2026-07-28；协议 fixture
  仍须记录实际协商版本，不能把读取规范当作验收。
- 起始检查点 G09.1：起点 relay 白名单缺少上游能力门控；计划以当前连接的
  Resources/Skills 声明决定方法准入，目录另需 directoryRead=true。该切片已取得
  下节实际 wire 请求计数与 M01–M03 证据；当前下一检查点为 G09.2。
- 后续完整范围保持 G09.2–G09.6：获许可来源/按需校验、现有 workflow 决策链、
  Turn/子任务/版本/entry/预算绑定、批准与缓存、实际模型指令、explain 与恢复。
  不恢复 origin MCP，不增第二个 agent/任务库，不把安装/转发称为激活。
- A27/M01–M03 已取得下节本地协议证据；A28/M04–M10 与后续 native 检查点尚未
  取得验收，不进入 G10。所有服务/进程验证使用临时状态、Cargo Rust fixtures
  与本地响应，不修改用户凭据或全局 harness。

### G09.1 Gateway 协议本地通过

- 复用 rmcp 当前 PeerInfo：每次 relay 检查该连接的 Resources 与 Skills 声明；
  目录另需 directoryRead=true。没有独立能力缓存，重连不沿用原 PeerInfo。
- 逐 entry 校验必需 frontmatter、URI、清单、size/digest 格式、重复文件、SKILL.md
  存在与 512 文件/16 MiB 上限；逐页校验完整 entry，保留未知字段，直接 get 核对
  返回 URI。只校验元数据，不把 digest 视为信任，也不在 gateway 激活内容。
- 兼容策略保留：旧上游缺缓存提示时归一化 complete/ttlMs=0/private；不补造清单、
  文件/大小/摘要。合法 directory response 被 rmcp untagged union 识别为
  ListResourcesResult 时，按实际方法接回目录 relay；gateway 仍不声明目录能力。
- 四路径快照为 `/tmp/bitrouter-g09-gateway-checkpoint.json`。改动是 SDK relay/
  wire metadata 校验与独立 Rust JSON-RPC fixture、integration target；未恢复 origin。
- `cargo test -p bitrouter-sdk --all-features --test mcp_skills_gateway -- --nocapture --test-threads 4`
  退出 0，**7 passed**；日志 `/tmp/bitrouter-g09-gateway-phase1-final.log`。
  覆盖能力缺失/false 的零越权请求、目录门控、重连撤销、全部现代请求 _meta、
  两来源同 URI 不同原始字节与支持文件路由、完整分页、缓存策略、未知/未列出 URI、
  HTTP/SSE 错误 -32602、空列表、scheme 的 direct fallback、错误成员诊断，及
  单 entry 的 512 个文件/16 MiB 完整元数据且零内容预取。dynamic 仍可被 gateway 转发。
- `cargo test -p bitrouter-sdk --all-features --lib mcp:: -- --test-threads 4`
  退出 0，**115 passed**；`/tmp/bitrouter-g09-gateway-sdk-regression.log`。
- `cargo fmt -- --check` 与
  `cargo clippy -p bitrouter-sdk --all-features --all-targets` 均退出 0；后者无警告。
  日志 `/tmp/bitrouter-g09-gateway-fmt.log`、`-clippy-final.log`。Cargo 继续使用
  G08 的构建并发/无 debug/incremental 与固定 stable aarch64 toolchain 环境。
- G09.1 结束时，M01–M03 / A27 的本地协议切片已取得证据；当时 M04–M10、workflow、
  激活、explain 与恢复尚未实现。G09 保持 active，随后按 G09.2 继续；最终全量
  gates 在阶段收敛后执行。

保留的失败终态：首次 fixture 编译错误为借用 String 转 io::Error 与 sha2 输出
不实现 LowerHex（`/tmp/bitrouter-g09-gateway-first.log`、`-second.log`）；已修正
测试代码。初始 **3 passed / 2 failed**（`-third.log`）定位合法目录响应的 typed
union 处理缺口，以及 HTTP fixture 缺 Mcp-Method（`-http-diagnostic.log`）；前者修复
生产 relay，后者补齐测试客户端必需 headers。`-invalid-entry-red.log` 明确复现
缺 manifest 的 entry 被旧 relay 接受，加入实际 wire 校验后通过。
`-metadata-green.log` 为 **5 passed / 1 failed**，最后一项是测试客户端尚未解析
合法 SSE；补齐 SSE 解码，没有放宽服务端规则。`-sse-baseline.log` 是增强双来源
fixture 时临时 String 借用的编译错误，改为 owned 值后最终 7 项通过。上述短名
均使用 `/tmp/bitrouter-g09-gateway` 前缀，未删除或把失败记为通过。

### 规范审阅同步（文档）

- 主 spec 与 MCP 验收附件改用上述固定 commit，保留 overview 作为阅读入口。
  附件区分规范义务、规范允许的实现选择与 BitRouter 的来源/Turn/预算策略。
- M04 明确所有 URI scheme 的根目录/name 结构校验与 authority 非网络解析语义；
  这是待实现的 host 验收要求，不记为新增通过。
- 本次同步仅修改文档与状态表述；不替换 active G09，不标记完成或推进 G10。
  当前协议通过范围仍限 G09.1，native 加载与实际激活仍待后续证据。
- 文档检查退出 0：4 个文档的 116 个本地链接存在、代码围栏成对，
  `git diff --check` 通过；G09.1 四个源码文件与通过时快照摘要一致。
  记录为 `/tmp/bitrouter-g09-spec-review-validation.json`；没有重跑或新增 runtime 验收。

### G09.2 Native 来源与验证基础（进行中）

- SDK 增加独立于 config-file feature 的 `skills::SkillsConfig`，主配置的
  `skills.mcp_sources.<mcp_servers 配置键>` 默认空。来源声明精确绝对 workspaces、
  synthetic-local 或认证 key/user IDs；连接时仍由原执行 owner 传入实际 caller。
  无许可/错 caller/错 workspace 时，只读执行不建立来源连接。残留许可不能创建
  已删除的 MCP 连接，也不阻止正常 MCP 配置移除。
- 复用原 Harness MCP 连接和进程生命周期；不另建连接池/执行循环。来源身份绑定
  host 配置键、规范化配置摘要与 caller/workspace。当前连接须声明 Resources/Skills；
  metadata 只进入原 HarnessInventory/managed signals，不代表激活。新增数据为空时
  保持原 inventory 序列化与 binding digest，既有无来源许可执行路径不自动连源。
- `HeldMcpSkill` 保留完整 entry/未知字段，摘要覆盖完整 frontmatter、文件 URI/size/digest
  集合及其他 entry 字段。支持其他层级 URI scheme；结构、根/name、清单范围与原 server
  均受约束。list/get 不读取内容；读取 SKILL.md 时校验完整 frontmatter，支持文件按需
  读取。网络结果和调用方提供的每次缓存命中均重验原始 size/hash。
- metadata 原子保留基准 512 文件/16 MiB；entry 元数据上限 2 MiB，单来源 listing
  上限 16 MiB/256 skills，同时保留原 256 KiB Harness inventory 上限。超元数据预算
  或读取预算明确拒绝，不预取正文或截断 entry。dynamic 作为可选候选显式排除，直接
  get 拒绝；binary/non-UTF-8 不能直接作为文本上下文。嵌套 SKILL.md 仅作为支持数据。
- `NativeResources::discover_for_caller` 和原 NativeSession 接入真实 caller；旧
  `discover` API 不推定匿名调用者有来源许可。标准 native Thread 的原资源准备入口
  同样接入 caller。提供来源限定的 get/read API，尚未把内容装成 activation 指令。
- 检查中实际复现并修复两项边界：来源策略变更被旧 reloader 误报为
  `reload_required/unclassified`；coding 宿主向仅声明 Resources/Skills 的来源发送
  `tools/list`。前者纳入原 startup-only classifier 和固定公开 `skills` 字段，新增/
  撤销都需重启；后者只对声明 Tools 的连接发现工具，skill 来源不再依赖 Tools。
- 分发 skill 保持 200 行，深度 CLI reference、三个 plugin manifests、CLI 与
  Harness 文档同步来源许可/只读行为和未完成范围；无 origin MCP、全局 harness 配置
  或用户实际凭据变更。新配置 schema 与本检查点最终回归证据在下方记录。

尚未完成的完整 G09 范围保持不变：受管理本地库 adapter、既有 workflow/decision
选择链、Turn/子任务精确绑定、原 pending/answer 上的逐 skill 同意、持久 cache/
恢复、实际模型指令与冷 explain。当前提供的完整 entry 摘要不是批准记录，传入
verified-cache bytes 不是持久缓存，resource API 成功不是模型激活；G09.2 尚未整体完成。

规范追查还发现共享格式校验沿用 ASCII 名称限制；当前
[Agent Skills name 规则](https://agentskills.io/specification#name-field) 包含 Unicode
小写字母/数字。G09 完成前须核对并对齐共享名称校验、URI 编码/父目录匹配和相关
CLI 报错/分发文档，加入真实 wire 用例；不能把当前 ASCII fixture 当作全部格式兼容。

本检查点实际验证（均为本地 fixture，未使用真实上游/模型凭据）：

- `cargo test -p bitrouter-orchestrator --all-features --lib --test mcp_skill_host -- --test-threads 4`
  退出 0：**294 lib + 11 host integration passed**；日志
  `/tmp/bitrouter-g09-native-loader-orchestrator-final.log`。包含原 Core、Thread、审批、
  恢复/大 checkpoint、ACP 与 Harness 回归，以及真实 stdio 的权限/来源/清单/字节/
  元数据和读取预算/动态/binary/嵌套数据/资源专用来源检查。
- `cargo test -p bitrouter-sdk --all-features --lib --test mcp_skills_gateway -- --test-threads 4`
  退出 0：**725 lib passed / 2 ignored + 7 gateway passed**；日志
  `/tmp/bitrouter-g09-native-loader-sdk-regression.log`。G09.1 协议切片在新增 host
  基础上重新通过，不扩展成完整 host 激活的结论。
- `cargo test -p bitrouter --all-features --test configuration_state -- --nocapture --test-threads 4`
  退出 0：**6 passed**；日志 `/tmp/bitrouter-g09-native-source-restart-green.log`。
  真实控制 socket 的保存/运行态与拒绝 reload 覆盖来源许可新增和撤销，并只公开
  固定 `skills` 字段，不泄露动态 source/user IDs。
- `cargo clippy --all-features --all-targets` 退出 0；日志
  `/tmp/bitrouter-g09-native-loader-clippy-final.log`。新实现无警告；保留既有
  onboarding_pty.rs 两条 byte_char_slices 提示（已与 G09 起点逐行核对，未改该文件），
  以及依赖 proc-macro-error2 的 future-incompat 提示，没有 allow 绕过。
- `cargo fmt -- --check`、`git diff --check` 均退出 0；fmt 日志
  `/tmp/bitrouter-g09-native-loader-fmt.log`。`cargo run -p dist-helper -- generate-schema`
  和 `-- check` 退出 0；日志 `/tmp/bitrouter-g09-native-source-schema.log`、
  `/tmp/bitrouter-g09-native-source-dist-check.log`，已更新 committed-path schema；
  catalog 生成物没有改动。
- 11 份文档/分发 reference 的 169 个相对文件链接存在、围栏成对，3 个 manifest
  JSON/边界与主 skill 200 行检查通过；1 个站点绝对路由不按本地文件验。
  `/tmp/bitrouter-g09-native-source-docs.json` 记录口径。首次检查误把
  `/docs/guides/cloud-api` 站点路由按本地路径查找而退出 1；修正检查口径后通过，
  没有删除有效站点链接来消除报错。
- 源码/文档相对 G09 起点的当前摘要为
  `/tmp/bitrouter-g09-native-loader-checkpoint.json`；不更改 HEAD 或初始 symlink。
  本节通过记录只证明以上范围。全工作区 nextest/doctest 和 G09 完成审计仍在整个
  阶段收敛后执行，不能据当前通过项关闭 goal 或进入 G10。

保留的失败终态：最初编译将 source policy 放入 config-file feature 下，导致
orchestrator 独立编译 E0433（`/tmp/bitrouter-g09-native-loader-check-first.log`）；
改成 SDK 的独立纯类型模块后编译通过（`-check-second.log`），两条新 public type
缺文档提示随后补齐。最初 5/8/9/10 项定向 target 逐次通过的日志分别为
`/tmp/bitrouter-g09-native-loader-first.log`、`-second.log`、`-third.log`、
`-target-final.log`；其中 10 项日志是加入 resource-only 用例前的切片，不是最终
11 项证明。`/tmp/bitrouter-g09-native-source-restart-red.log` 为 **0/1 failed**，
实际复现错误 `reload_required/unclassified`；`/tmp/bitrouter-g09-native-resource-only-red.log`
为 **0/1 failed**，实际请求计数为 tools/list=1。两者都在最终相应回归中取得修正后
证据。链接 bro 时还有 macOS `__eh_frame` compact-unwind 大小提示，未通过删功能、
放宽权限、改生产超时或跳过失败测试消除提示。

### G09.2 受管理本地库读取接入（进行中）

- 核对上一个 38 路径 G09 快照零漂移后继续。`harness::skills::library::SkillLibrary`
  是只读注入边界，应用的 `skills/native` 适配现有 G07 Store；不新增安装器、清单
  writer、任务库、执行循环或 origin MCP。原 install/update/uninstall 仍使用原发布事务。
- catalog 只枚举 active aliases，记录 project/global 库位置、库 revision（空库亦保留）、
  安装 ID、whole-bundle version、原完整 receipt/provenance 和当前 caller/workspace。
  同名 scope 不覆盖；不预取正文。不健康 scope 单独报告，不隐藏另一个可读 scope。
- 读取重新验证当前 caller/workspace 和原库位置，查找安装历史中的精确 receipt；
  不要求 alias 仍指向它。每个文件限原 bundle manifest，逐次重验真实目录、常规文件、
  原始 size/hash、执行位和读取预算。SKILL.md 还校验 UTF-8/格式和原身份；不读取源目录
  充当已安装版本，不用新版本代替缺失的旧版本，不执行 scripts。
- native host 默认装配该 adapter；原 HarnessInventory/managed signals/material resolution
  发布可用元数据与版本材料。读取层支持序列化绑定在新资源实例中回读原版本；这不是
  Core Turn 绑定或 daemon 故障恢复证据。整个源码仅使用临时库与本地 fixture 验证。
- native 执行资源兼容检查仍比较 transport/policy binding、工具、instructions 与原
  unmanaged resources；将 managed/MCP 候选 catalog 的刷新独立出来，避免 alias 更新
  阻断下一 Turn。已选择内容的冻结权仍需由后续 Core skill selection/binding 实现。
  当前没有自动选中/激活内容，也未扩大任何工具 grant。
- 新增五个 native 库用例：project/global 同名独立；只改支持文件仍改变 bundle 版本
  而 SKILL.md digest 不变；更新/卸载后原 receipt/支持文件回读；caller/workspace、
  清单外路径、预算与实际字节篡改拒绝；目录替换为匹配字节的外部 symlink 拒绝；
  global 不可用时 project 仍可读。另一个用例通过原 ThreadService/Core 连续跑三个
  native Turns，验证安装/更新/卸载的 catalog 刷新、原 Thread 和工具授权保留。
  此用例的模型脚本不证明 skill 指令已进入模型。
- `cargo test -p bitrouter --all-features --lib skills:: -- --nocapture --test-threads 4`
  退出 0，**19 passed**（其中 5 项 native 库用例）；日志
  `/tmp/bitrouter-g09-local-library-regression-final.log`。先前 18 项通过记录保留为
  `/tmp/bitrouter-g09-local-library-regression.log`。
- `cargo test -p bitrouter-orchestrator --all-features --lib harness -- --test-threads 4`
  退出 0，**28 passed**；`/tmp/bitrouter-g09-local-harness-regression.log`。
  `cargo test -p bitrouter-orchestrator --all-features --test mcp_skill_host -- --test-threads 4`
  退出 0，**11 passed**；`/tmp/bitrouter-g09-local-mcp-regression.log`。
- `cargo clippy --all-features --all-targets` 退出 0；
  `/tmp/bitrouter-g09-local-library-clippy.log`。新代码无警告，仍有此前记录的两条
  onboarding byte_char_slices 和 proc-macro-error2 future-incompat 提示，未绕过检查。
- `cargo test -p bitrouter --all-features --test skills_lifecycle -- --test-threads 4`
  退出 0，**12 passed**；`/tmp/bitrouter-g09-local-cli-regression.log`。覆盖实际 CLI 的
  本地/Git 安装更新、scope、保留、并发 CAS、配额/链接与 hooks/scripts 不执行等 G07
  生命周期；大包/Git fixture 运行到终态后才记录通过，没有因观察超时重启。
- `cargo fmt -- --check`、`git diff --check` 退出 0；fmt 记录为
  `/tmp/bitrouter-g09-local-library-fmt.log`。当前不改变 SDK config schema 或 catalog
  数据。主 skill 仍为 200 行，CLI/development/Harness reference 与三个 manifests
  同步 native metadata/原版本读取能力及尚未完成的激活范围。
- 初始 `cargo check -p bitrouter --all-features` 退出 0，日志
  `/tmp/bitrouter-g09-local-library-first.log`。首次 unit target 编译退出 101，测试把
  `Result<_, String>` 直接交给 anyhow `?`（`/tmp/bitrouter-g09-local-library-tests-first.log`）；
  改为可承载这两类错误的测试返回类型后，原两项退出 0（`-tests-second.log`），
  之后增强为当前五项。保留失败记录，不改生产错误/权限来取得通过。

本检查点仍是 G09.2 的资源层。后续需接入原 Core 的 workflow 决策、显式约束、
Turn/子任务冻结选择、MCP 逐 skill 批准、持久 cache/恢复、实际模型指令与冷 explain，
并补齐上节 Unicode/URI 格式差距。阶段结束前按完整 G09 契约验收，当前不关闭 goal，
不进入 G10。

继续实现时还须核对 native workspace 与技能 project-root 的映射：当前 reader 以
已准入的显式 workspace 解析 project 库；从 Git 子目录启动且安装使用 Git root 时，
需在既有授权边界内取得明确的资源根，不能借自动发现扩大原工具 workspace。
此项与 Core 绑定/选择一起完成，不能把当前同根 fixture 当作所有项目布局的证据。

### G09.3–G09.5 本地 Core 选择与 native 宿主切片（进行中）

这一检查点把本地包从“可读材料”接入真正的指令装配，但不关闭 G09，也不替代
MCP 来源的批准/cache/激活与实际 CLI→daemon 验收。

实现事实：

- 新增 `core/skills.rs` 的 scope/installation ID 或 server/URI 显式选择器、完整来源
  binding、catalog/预算/选择记录。`RoutingSettings.skills` 是默认省略的可选字段，
  旧 accepted input 的 JSON/fingerprint 不因默认字段而改变。当前 CLI 尚无选择器
  flags；不可把库 API 当作已经交付的 CLI。
- `core/session/skills.rs` 在原 Turn 接受后、context/model 准备前冻结元数据；只用
  name/description/当前任务/用户约束发送一批 typed Noul 问题，附真实 caller、
  workspace/permission revision、agent task_scope/父关系、catalog revision 与
  host 能力；无 workflow 事实保留 unknown。复用现有配置的
  DecisionRuntime、context store 的 DecisionReceipt、成本/活动时间 ledger 和
  durable dispatch gate。没有 backend 时，可选自动集合为空且记录理由；显式
  必需项仍可按 scope/安装身份选择，缺失不静默跳过。
- 选择与读取分开提交；完整选择 checkpoint ACK 后才读取原 receipt 的 SKILL.md。
  默认最多四项、64 KiB，再取现有 context 剩余额度；预算计算包含正文和实际
  source/receipt 指令 envelope，超预算不截断。当前尚无独立配置调整入口。
- 记录放在原 context store 的 `WorkUnit.skills`，沿既有 Turn ID 保留历史，
  不增加镜像任务库。原 journal history validator 固定 catalog/policy、选定集合
  和已加载正文；模型每个后续步骤前重读并重验精确原字节，恢复时清除内存验证
  标记。更新 alias 不替换当前 Turn；下一 Turn 独立选新版本。
- instruction 装配与 generic MaterialRef 的 evidence 语义分开：系统指令包含已
  验证的正文、完整原来源/receipt 和不扩展工具权限的约束。`ModelStep`/context
  commitment 在这之后捕获真实 prompt。自动父选择不复制给子任务；显式
  TaskInput 用户约束继续沿原 assignment 继承。
- standard native 与 managed native 的原 HarnessPort 实现供给元数据和原绑定
  字节；managed 只为实际启用库/来源的宿主协商已有 context feature，并在其
  既有 capabilities 中如实声明。原 NativeStore/SQLite row、owner、epoch、ACK
  和 release 路径不替换。MCP 候选保留完整 held entry，但当前尚未接入逐 skill
  同意，自动匹配记录 `mcp_skill_consent_required`，必需 MCP 项明确拒绝，
  两种 native port 均不绕过同意去取正文。

当前本地证据：

- `cargo test -p bitrouter-orchestrator --all-features --test core_execution
  native_skills -- --nocapture --test-threads 4`：**8 passed**，退出 0；
  `/tmp/bitrouter-g09-core-skills-tests-final.log`。覆盖实际模型 prompt、同 Turn
  原版本/新 Turn 新版本与历史保留、缺 backend、显式必需/缺失、可选预算零
  预取、选择 ACK 失败零取正文/零模型、下一步骤篡改拒绝、子任务独立选择。
- `cargo test -p bitrouter --all-features --lib skills:: -- --nocapture
  --test-threads 4`：**20 passed**，退出 0；
  `/tmp/bitrouter-g09-core-skills-native-hosts-second.log`。新增的两条真实宿主
  证据使用 G07 实际安装库：Thread 三次 Turn 的模型请求分别含 V1/V2/空集；
  managed NativeSession 的模型请求含完整原 receipt，原 SQLite journal 在
  alias 更新/卸载后的 cold inspect 和更高 epoch reopen 仍保留原来源，且不新增
  模型调用。这是已完成任务的原 journal 恢复，不能冒充活跃/中断任务恢复。
- 原 `native_thread_catalog_refresh_preserves_thread_and_tool_authority` 用例增强并
  改名为 `native_thread_skill_selection_refreshes_versions_without_changing_tool_authority`；
  继续断言 Thread 身份、工具 grant/digest 不因 catalog 变化而改变。
- 中间快照的 orchestrator 回归退出 0：**294 lib + 318 Core + 11 MCP host**，
  `/tmp/bitrouter-g09-core-skills-orchestrator-regression.log`。随后把临时当前 Turn
  字段迁至保留历史的原 WorkUnit，并补三项定向测试；上面的 8 项和 20 项已经
  验证当前实现，阶段最终全量 gates 仍需在 G09 完整收敛后执行。
- 保留失败：首轮 Core test target 缺三项测试 imports（`-tests-first.log`）；
  修复后原五项通过（`-tests-second.log`）。增强的子任务 fixture 只脚本化三次
  响应，父消费子结果需第四次，导致 7 passed/1 failed（`-tests-work-unit.log`、
  `-child-diagnostic.log`）；补齐 fixture 后八项通过，没有放松生产调度。
  首次宿主 test target 的错误分支以 anyhow::bail 返回错误，与 Box TestResult
  不匹配（`-native-hosts-first.log`）；仅修测试返回类型，第二轮二十项通过。

补充验证与准入修正：

- WorkUnit 版本的完整 Core 回归退出 0：**321 passed**，MCP host **11 passed**，
  `/tmp/bitrouter-g09-core-skills-core-regression-final.log`；对应持久 WorkUnit
  版本、尚未增加下面的明确准入失败事实与决策 scope 字段的源码快照
  `/tmp/bitrouter-g09-core-skills-source-checkpoint.json`，该附件保留不覆盖。
- 后续补上 `skills.admission.rejected` 与 `SkillSelection.failure`：缺失必需项等
  确定的准入拒绝写入原 WorkUnit，再让既有 scheduler 收束失败 Turn/子任务，
  保留原 accepted operation/run/Turn 身份。强化测试验证最终 Core run=Failed、
  原 `missing-skill` acceptance 仍在、拒绝原因可重读、零正文/零模型。
  不是只把错误返回给调用者、把原 run 留在 Running。
- 上述修正及真实 scope/role/capability 决策事实的定向复验：同一 `native_skills`
  target **8 passed**，退出 0，`/tmp/bitrouter-g09-core-skills-admission-final.log`；
  较早的拒绝收束切片也为八项通过（`-admission-tests.log`）。
- `cargo test -p bitrouter --all-features --test managed_daemon -- --test-threads 4`
  退出 0，**11 passed**，`/tmp/bitrouter-g09-core-skills-managed-daemon-regression.log`。
  这是原 CLI daemon 生命周期/身份/幂等/丢 ACK/取消与未知效果保护的保持回归，
  不冒充 CLI 选定 skills 的新端到端 journey。
- 当前修正后的 `cargo clippy --all-features --all-targets` 退出 0，
  `/tmp/bitrouter-g09-core-skills-clippy-final.log`；无新增代码警告，保留既有两项
  onboarding byte_char_slices 与依赖 future-incompat 提示。`cargo fmt -- --check`
  和 `git diff --check` 退出 0，三个分发 manifests 可解析，主 skill 为 200 行。
- 该准入修正的 Rust/Cargo 源码集合摘要：
  `83b7fb73b72d318cf17349e82ce91a86ba79c22c92aafac1b0eb31caa434b627`；
  `/tmp/bitrouter-g09-core-skills-admission-source-checkpoint.json` 记录 1,208 路径，
  相对 G09 起点 58 路径有差异，包含此前 G09.1/加载层；不把 G00–G08 的既有
  差异重新归入当前切片。完整 G09 nextest/doctest/schema gates 尚待全阶段收敛。
- 再补一项恢复窗口：原 `skills.admission.rejected` 已 ACK、`agent.failed` 尚未
  提交时，在确认旧 owner 停止后恢复原 journal prefix。恢复继续收束原失败
  Turn，不再选择、不取正文、不启动模型，也不更换 accepted 身份。修正后
  定向 target **9 passed**，退出 0：
  `/tmp/bitrouter-g09-core-skills-rejection-recovery.log`。
  当前源码仅在上述检查点之后改变 session/skills 与其定向测试两个 Rust 文件；
  Rust/Cargo 集合摘要为
  `a0afed17e116f4662e636becbfe81abe6c15045c3012dc654f3c7f30ecb19a79`，
  对应 `/tmp/bitrouter-g09-core-skills-rejection-recovery-source-checkpoint.json`。
  该版本的 workspace `cargo clippy --all-features --all-targets` 退出 0
  （`/tmp/bitrouter-g09-core-skills-recovery-clippy.log`），fmt 与 diff check 退出 0；
  无新增警告，既有两项 onboarding 警告与依赖提示同前述检查点。

尚需完成并验证：MCP 显式 URI 纳入选择、逐 skill pending/answer/撤销、持久缓存
与离线/中断恢复、支持文件工具读取、CLI 选择器与冷 explain、CLI→daemon 两类
真实 skill journey。还须处理未决选择遇到 steering/signal/权限变化的失效/重新
评估边界，并验证显式父约束的多 agent 组合；不能用主 Turn 的 happy path 推定。
上节嵌套 Git 资源根与 Unicode/URI 差距仍保留，G09.1–G09.6 整体验收尚未完成。

### G09.2–G09.5 MCP 精确批准与任务数据交付（进行中）

继续同一 active G09，保留前节 `a0afed17…` 源码快照和 G00–G08 差异。以下是
新增切片，不代表完成 G09 或开始 G10；最新 source digest、日志终态随验证补录。

- 批准直接存放原 `WorkUnit.skills.candidates[].consents` 与 Core operation
  ledger；完整 source/entry、请求 nonce、agent Turn/run、owner epoch、权限版本
  和 tool manifest digest 共同绑定。批准意图与答复分别 ACK，之后才读取正文。
  答复 receipt 的 fingerprint 也参与恢复校验；没有镜像批准库或第二个 executor。
- Native Thread 将 `mcp_skill.activate` 接入既有 pending/answer，与普通工具审批
  串行化。`task run` 不自动批准该请求；普通工具原策略保留。managed 的
  `NativeApproval::approve_skill` 不继承通用命令批准，产品 owner 等待明确的
  `task managed-answer --session --request-id --server-instance --consent-id --decision`。
  `managed-status` 从原状态投影完整请求；取消使待答复过期。
- `managed-answer` 校验原 input/run/agent Turn/entry/epoch，幂等答复使用稳定
  operation key。没有 live owner 时只可重读已经提交且 fingerprint 相同的
  receipt；不恢复 owner、接受新批准、重新选择或读取 MCP。明确选择重启后的
  daemon boot 可查询历史 receipt，该操作不续租旧批准。
- Native local 升至 **v19**，阻止旧 v18 task-run 的全 pending 自动允许策略
  误批准 skill。HTTP v2 声明 `explicit_skill_consent`，对 skill 的 `/inputs`
  答复要求 `skill_consent: true`。Managed v1 Hello 新增默认缺省 false 的
  `skill_consent` 能力，旧 endpoint 不会收到新的答复 mutation。
- 新 `native_skills_v1` required feature 标记持久 skill 状态；当前 native
  hosts 同时协商 `context_views_v1`。恢复先验证原 checkpoint 的 required
  features，旧 capabilities 不能丢弃技能/批准事实后继续。没有该 feature 的
  session 不选择 skills；已经运行的 legacy Turn 不事后获得新集合。
- MCP 原文与完整 entry 进入带来源的 **user 数据消息**；system 只附加固定处理
  策略。Root 原始字节仍按 manifest 核对；正文、来源 envelope 和策略共同计入
  指令预算。Context evidence 保存不可变 `skill_binding`，禁止自动共享给未批准
  的子任务，重建偏移也计算新增数据帧。这里不声称模型一定遵循正文。
- 可选拒绝没有正文读取，仍可运行原任务；必需拒绝保留 accepted input 并收束
  失败。待批准请求取消、权限变化、新 owner 恢复都使用既有通知与 fence；恢复
  待批准请求更换 nonce/epoch，仍指向原完整 entry，旧答复不产生新批准。

验证记录（本切片检查已收束；完整 G09 仍未完成）：

- 前序 Core 定向 17 项通过：`/tmp/bitrouter-g09-skill-consent-core-fences.log`。
  加入 receipt fingerprint / checkpoint feature 后，完整定向回归退出 0：
  orchestrator lib **294 passed**、Core execution **330 passed**（含 skill 17 项）、
  MCP host **12 passed**，日志 `/tmp/bitrouter-g09-skill-consent-core-regression.log`。
  原 pending 恢复用例还验证不含 `native_skills_v1` 的旧 capabilities 拒绝恢复，
  拒绝前零正文/模型；随后完整 capabilities 恢复同一 entry 并重新发出 nonce。
- 真实 Rust MCP → Thread → 模型 observe hook 的 host target 12 项通过：
  `/tmp/bitrouter-g09-skill-consent-real-host-final.log`。其中 ReadOnly/AllowEffects
  均等待独立批准，批准前零正文/模型，批准后根正文与完整 entry 为 user 数据，
  system 不含正文，支持文件尚未读取；当前新 feature 后复验包含在上一条。
- 加入 v19/HTTP/required feature 后，`managed_daemon` **15 passed**、
  `management_cli` **18 passed**，同一命令退出 0，日志
  `/tmp/bitrouter-g09-skill-consent-cli-version-final.log`。前者包括四项新的
  real CLI/HTTP journeys：managed 精确 entry 与历史 receipt、拒绝/取消、
  task run 的 v18 自动答复拒绝与 v19 明确批准，以及 HTTP 缺少显式 flag 时拒绝、
  加 flag 后只执行一次。原 daemon ACK/未知效果/身份/释放保护与 native/ACP
  控制回归均保留；这些结果不冒充支持文件与离线恢复验收。
- 上述 Core/CLI 回归对应 Rust/Cargo 集合摘要
  `da9a0dec08d0d44ba88e7632f91c9c5748e2bc264bdcb630a582ae381b5a5d66`；
  当时 `/tmp/bitrouter-g09-skill-consent-verified-source-checkpoint.json` 记录 1,211 路径，
  相对 G09 起点 83 路径变化，相对前个 Core 恢复检查点 55 路径变化。保留旧
  `a0afed17…` 附件；没有将 G00–G08 差异重新归为本切片。
- `cargo nextest run --all-features --no-fail-fast --test-threads 4` 退出 0：
  **4,282 passed、22 skipped**，日志 `/tmp/bitrouter-g09-skill-consent-workspace-nextest.log`，
  nextest run ID `0a0508eb-ef9a-49e3-8679-cb833ba2ffa1`。10 项 slow 提示最终
  均通过；保持既有 `core-tree-pressure` 串行组与超时，未跳过或放宽用例。
  完整构建时仅清理本轮 5 个已被新版本替代的 test executables，回收
  633,608,368 bytes；当前可执行文件、源码和 fixture 保留。
- Clippy 的两条新增 `collapsible_if` 提示仅涉及 native/managed 批准 bridge，
  合并条件后源码摘要为
  `e7e646d789a86aae07f28249b48e7039c7ea6c6211ec5b1d1d36277ae90df759`。
  与全量通过版本的可执行源码差异仅这两个条件表达式；未重写批准/权限逻辑。
  后续续跑重新核对全部 1,211 路径的 Rust/Cargo 摘要与该值一致；下节的读取失败
  修正使用独立的新摘要，不沿用该全量通过标签。
- 续跑时原进程句柄和 `/tmp` 附件已不可读；先前实际观察的终态与摘要保留为
  历史证据，不再声称临时文件当前存在。未确认终态的最后一次 bridge 复验已
  重新运行：`cargo test -p bitrouter --all-features --test managed_daemon skills::
  -- --nocapture --test-threads 4` **4 passed**，退出 0。新日志保存到
  `target/g09-evidence/consent-cleanup-journeys.log`，当前摘要附件为同目录
  `consent-current-source-checkpoint.json`，避免继续只引用易失临时路径。
- 该 e7e646d7 摘要下 `cargo clippy --all-features --all-targets`、
  `cargo test --all-features --doc`、`cargo fmt -- --check`、
  `cargo run -p dist-helper -- check` 和 `git diff --check` 均退出 0。
  Clippy 只有原 onboarding 两条 byte-char-slices 与依赖 future-incompat 提示；
  schema/registry 未过期。日志为 `target/g09-evidence/consent-cleanup-{clippy,doctests,dist}.log`。
  文档测试含 SDK 五项编译用例；不把零用例的 crate 标为新的运行时覆盖。

保留失败以便复现：Core 可选拒绝的旧 validator 要求未激活项也必须有正文，已
修复；初次 native 模型 fixture 误用非 streaming 响应，随后 host target 通过；
managed port 初版未转发新的 consent message，真实 CLI 测试发现并补齐。Task
CLI 测试误写单数 `thread`，已统一为实际 `threads`；HTTP 重放测试最初错误地
要求包含当前 Turn status 的整个投影相等，已改为验证原身份与零重复执行。
失败日志分别保留在 `/tmp/bitrouter-g09-skill-consent-*.log`，不删用例或放宽生产权限。

剩余 required work 保持完整：选择层直接 URI、MCP 持久缓存及离线/中断恢复、
绑定 bundle/manifest 的支持文件工具、CLI include/exclude 与 cold explain；
未决评估遇到 steering/signal/权限变化的失效/重评估，读取失败的准确 durable
终态，嵌套 Git 资源根、Unicode/URI 对齐，以及本地库 CLI 和 daemon 多 agent/
撤销/恢复组合。最终完整 nextest/Clippy/fmt/doctest/schema/dist gates 仍待全阶段
收敛。没有调用真实 provider/远端 MCP，没有 commit/push，也不进入 G10。

### G09.2 / G09.6 读取失败、状态变化与恢复事实（进行中）

继续原 G09，修复已确认的失败路径：先前 `skill_instructions`、字节校验或 UTF-8
错误直接冒泡，Core run 仍为 Runnable，外层可能错误地报告未知效果。现在复用
原 checkpoint/WorkUnit，而不是新增等待队列或恢复服务：

- `skills.instructions.unavailable` 在 ACK 后发布原 Turn/step、context/signal/
  permission/tool-manifest 版本及失败原因。远端诊断最多保留 2,048 UTF-8 bytes，
  不让诊断文本占满正常收束容量。
- 尚未加载、未生成过模型步骤的可选项保留选中意图，将不可用事实写入原
  `SkillCandidate.failure`，不提供正文、不重新选别的版本。该失败为不可变历史，
  后续恢复不能清除它后偷偷加载新 alias。
- 必需项，或已经保存正文/进入过执行步骤的项无法重验时，沿用原
  `SkillSelection.failure` 收束失败；保留 accepted input、原版本和历史正文。
  不把校验失败当作未知本地工具效果，也不让 run 一直留在 Runnable。
- 读取期间先观察并校验原 manifest、signals、context revision、Turn/step 和
  pending steering。变化会取消旧读取；提交正文与失败事实前再做同样校验，
  旧读取结果不能跨权限/输入边界写入。MCP 如需新批准，仍绑定原 held entry。
- 可选项的读取失败也必须 ACK，不能在未知提交结果下静默降级并调用模型。

当前切片源码摘要：
`8e9656fde5431f3cda9454b783e5388723af9e845529be7e3047420236c43686`。
附件 `target/g09-evidence/skill-load-failures-source-checkpoint.json` 包含 1,211 路径，
相对 e7e646d7 检查点变更 6 个 Rust 文件及 4 个文档/skill 文件；未提交。

| 要求与切片 | 实际入口/证据 | 当前结果与范围 |
| --- | --- | --- |
| A14/A18、M05/M07：校验失败准入 | `cargo test -p bitrouter-orchestrator --all-features --test core_execution native_skills -- --nocapture --test-threads 4`；`target/g09-evidence/skill-load-failures-final.log` | **22 passed**，退出 0；可选初读排除、必需初读失败、已使用版本损坏后阻止下一模型、失败 ACK 丢失时零模型 |
| A14/A17、M10：取消旧读取与原身份恢复 | 同一 Core target；`a_permission_change_during_a_skill_read_cancels_old_bytes_before_reapproval`、`recovery_preserves_an_optional_load_failure_without_reading_a_new_version` | 旧 token 已取消、零旧正文提交，重新批准仍为原 entry；已 ACK 的可选失败恢复保留原 source/input，零新版本读取；不冒充 MCP 离线缓存恢复 |
| A15/A28、M05：实际损坏 MCP 字节 | `cargo test -p bitrouter --all-features --test managed_daemon skills:: -- --nocapture --test-threads 4`；`target/g09-evidence/skill-load-failures-cli.log` | **5 passed**，退出 0；新增真实 CLI→daemon→Rust MCP 在批准后返回损坏 root，原 SQLite 保存失败，最终模型不含原/坏正文，正常 release，无未知效果；另四项批准/取消/兼容链路保留 |
| A24：静态/文档同步 | `cargo clippy --all-features --all-targets`、`cargo fmt -- --check`、`git diff --check` | 退出 0；Clippy 无新增提示，仅原 onboarding 两项与依赖 future-incompat；日志 `target/g09-evidence/skill-load-failures-clippy.log`。Spec/资源契约/managed skill reference 同步；未改 CLI flags/config schema |

本切片尚未重跑完整 nextest；上节 4,282 项结果只覆盖其具名源码摘要。G09 最终
检查仍要在全阶段收敛后重跑，不能用本节 22/5 项代替完整验收。原 G09 保持
active，MCP 持久缓存/离线恢复、绑定支持文件、选择层直接 URI、CLI constraints/
explain、未决评估失效/重评估、Git 根/Unicode/URI 对齐与完整组合仍是 required
work。没有进入 G10，也没有真实 provider/MCP 互操作或发布资格声明。

### G09.2 / G09.6 原 Turn 的 MCP 根正文缓存（进行中）

在 `8e9656fd…` 读取失败切片之后继续，同一 goal 保持 active。根正文缓存使用
原 WorkUnit 中已 ACK 的 `SkillCandidate.content`，不新增磁盘缓存目录、安装
receipt、候选来源或数据库。Core 只为 MCP 绑定传递原字节；本地 bundle 继续
逐次读原版本以发现缺失或损坏。两种 native port 均接入原缓存并重验完整 entry。

- 每个新模型步骤重新检查完整 manifest、原 size/hash/frontmatter；认证恢复已
  清除旧 `validated_step_count`，因此恢复也经过宿主授权和逐字节验证。
- 静态配置身份与当前连接分开保留；资源专用来源初始化失败且进程清理已确认时
  可以缺席新 catalog。旧 held entry 在配置、caller/workspace 和来源 grant
  不变时可使用原缓存；新 Turn 不从缓存制造候选，缺少缓存时不能假装可读取。
- 当前连接明确撤销 Resources/Skills 声明时仍拒绝缓存。删除服务器、撤销来源
  grant、改变配置摘要或 caller/scope，均不能用原缓存绕过检查。
- 工具/服务指令连接保留原 catalog 失效检查；资源专用连接结束不再无条件使
  整个工具目录失效。Coding 模式初始化失败仍明确拒绝，不能静默删除潜在工具。
  缓存只提供字节，不授予执行权限，也不改变原 owner 恢复与清理门槛。
- 不宣称支持文件缓存、所有连接超时恢复或完整 CLI 重启执行恢复已经通过。

源码摘要 `ac4399b90dbff1efca7de26b05d918dd57c3842106f5608a7742c3c64e8ab904`，
对应 `target/g09-evidence/mcp-root-cache-source-checkpoint.json`（1,211 路径；
相对上一切片 10 个 Rust 文件和 3 个文档/skill 文件变化）。未提交。

| 要求/实际入口 | 证据 | 结果与范围 |
| --- | --- | --- |
| A14/A17、M08/M10：原绑定缓存与恢复 | `cargo nextest run -p bitrouter-orchestrator --all-features --test core_execution -E 'test(native_skills::)' --no-fail-fast --test-threads 4`；`target/g09-evidence/mcp-root-cache-core.log` | **24 passed**，退出 0；新增下一模型步骤原来源文件消失后复用缓存，以及已 ACK 加载边界的 Core 认证恢复，保留原 entry/正文/批准/输入，零新版本读取 |
| M04/M05/M08：独立进程离线与验证 | 同包 `--test mcp_skill_host`；`target/g09-evidence/mcp-root-cache-host.log` | **15 passed**，退出 0；原配置下新进程初始化失败后原缓存可读、catalog 保持空；缓存篡改/取消/错文件/配置删除/来源撤销拒绝；资源来源读后退出仍可用原缓存，Coding 初始化失败仍拒绝并确认清理 |
| A15/A28、M08/M10：实际 CLI→daemon→Rust MCP→两次模型请求 | `cargo nextest run -p bitrouter --all-features --test managed_daemon -E 'test(skills::)' --no-fail-fast --test-threads 4`；`target/g09-evidence/mcp-root-cache-cli-fixed.log` | **6 passed**，退出 0；来源首次 root read 后退出，真实 native 工具步骤后的第二次请求保留原 MCP user frame/entry，system 无正文；总计一次 root read，SQLite 保留原绑定/字节，owner 正常 release |

首次 CLI 检查日志 `target/g09-evidence/mcp-root-cache-cli.log` 为 **5 passed / 1 failed**：
缓存用例命中了先注册的普通完成 mock，计数为零，未覆盖预期两步。仅修正该
mock 的优先级，保留“两次实际模型请求/一次资源读取”的断言，再取得上表结果。
没有放宽生产校验或删减验收。`cargo clippy --all-features --all-targets` 退出 0，
日志 `target/g09-evidence/mcp-root-cache-clippy.log`；仅原 onboarding 两项提示及
依赖 future-incompat，未新增警告。fmt/diff 检查通过。完整阶段 nextest/doctest/
schema gates 仍待全部 G09 required work 收敛。

剩余包含绑定支持文件、选择层显式 URI、CLI constraints/cold explain、未决评估
失效与重评估、Git 根/Unicode/URI 对齐，以及完整本地库/daemon/多 agent/
撤销/重启恢复组合。M04–M10 仍未整体通过，不进入 G10。

### G09.2–G09.5 显式 URI 与 CLI skills 约束（进行中）

接续 `ac4399b9…` 根缓存切片；当前源码摘要
`adbb057037c9e224fba067671f95887e7304f2376982e0971a376debf1544ec1`，
完整路径摘要为 `target/g09-evidence/explicit-uri-cli-source-checkpoint.json`。

- Core 在冻结 catalog 前，为必需且未列出的 MCP URI 调用现有 native
  `skills/get` 路径；仍经过原配置/caller/workspace/能力门控，不读文件。
  完整 entry 进入原 catalog/选择/批准记录，unknown/未许可项在原 acceptance
  下收束失败。显式 URI 不是安装或新的候选发现来源。
- `task run/managed` 新增可重复 `--skill`/`--exclude-skill`，精确选择
  `project:INSTALLATION_ID`、`global:INSTALLATION_ID`、`mcp:SERVER:URI`。
  矛盾约束提交前拒绝，必需项缺失不调用模型；排除项不读取/批准/装配正文。
  native 保存原 Thread config；managed 保存原 TaskInput 和重试指纹。
- native runtime 与 managed Hello 声明 `skill_constraints`，客户端对缺失声明
  按 false 处理并拒绝带约束提交，避免旧 daemon 静默忽略。原 v19 的旧自动批准
  客户端拒绝机制与独立 MCP 内容批准保留。HTTP CreateBody 接收相同类型的约束。
- 没有改变 scope grant、工具权限、模型执行循环、原 owner 或任务库；显式
  选择仍在 context/model 生成之前，子任务沿用原显式约束继承。

| 要求/实际入口 | 证据 | 当前结果 |
| --- | --- | --- |
| M04/M05/M07、A14/A18：未列出 URI | `cargo nextest run -p bitrouter-orchestrator --all-features --test core_execution -E 'test(native_skills::)' --no-fail-fast --test-threads 4`；`target/g09-evidence/explicit-uri-core.log` | **26 passed**，退出 0；未列出必需 URI 元数据先于批准、零语义选择调用；未知 URI 保留失败/原输入，零正文/模型 |
| A15/A28、M04/M10：实际产品选择 | `cargo nextest run -p bitrouter --all-features --test managed_daemon --lib -E 'test(skills::) \| test(explicit_skill_constraints_require_advertised_native_support)' --no-fail-fast --test-threads 4`；`target/g09-evidence/explicit-uri-cli-journeys.log` | **30 passed**，退出 0：21 个 lib 用例及 9 个实际 CLI/HTTP journeys；task run 与 managed 均加载未列出 URI；显式排除、未知 URI、未许可来源、矛盾选择拒绝；更改选择不能重用原 managed 输入键；旧 native capability 拒绝带约束创建 |
| CLI/skill/plugin 契约 | `target/debug/bro task run --help` 与 `task managed --help`；`target/g09-evidence/explicit-uri-task-*-help.txt` | 真实二进制两处 help 均显示新 flags；CLI reference、主 spec、分发 skill 与三个 manifests 同步，主 SKILL.md 保持 200 行 |
| A24：源码静态检查 | `cargo check --all-features --all-targets`、`cargo clippy --all-features --all-targets`；`target/g09-evidence/explicit-uri-cli-check.log` / `explicit-uri-cli-clippy.log` | 均退出 0；Clippy 仅原 onboarding 两项及依赖 future-incompat；fmt/diff/manifest JSON 检查通过 |

以上不关闭 G09。绑定支持文件/cold explain、未决评估失效/重评估、Git 根与
Unicode/URI 对齐、完整本地库/多 agent/撤销/超时/重启恢复组合，以及全阶段
最终 gates 仍需完成。所有通过范围均为本地 fixture，不声明真实 provider/
远端 MCP/托管 CI 或发布资格；不进入 G10。

### G09.2 共享 Unicode 名称与 URI 父目录校验（进行中）

官方 [Agent Skills specification](https://agentskills.io/specification) 明确按
Unicode lowercase alphanumeric characters 限定名称，长度为 1–64 字符。
2026-10-20 保存的官方 Markdown 为
`target/g09-evidence/agent-skills-specification.md`，SHA-256
`4c649bdf0e0a51c9e215d9f91009ecdca05ee9d073edb6f37c20265bbd829e11`。
本节修复此前记录的 ASCII/字节计数差距，不扩展工具授权或来源许可。

- SDK 与 orchestrator/CLI 复用一个名称谓词；中文等无大小写字母、小写重音
  字母和 Unicode 数字可用，长度按字符数，保留原名称。大写/titlecase、emoji、
  首尾/重复连字符仍拒绝，不做改变身份的大小写转换或 Unicode 归一化。
- MCP gateway 与 native host 共用单 segment percent 解码；编码父目录与完整
  frontmatter 名称比较，原 URI、namespace、完整 metadata 和资源 digest 保持。
  host 原有越界/非法编码/清单限制继续适用，不解析 authority 为 DNS 主机。
- CLI 名称诊断、内部 CLI reference、分发 skill 的详细参考和 M 验收约束同步。
  MCP server label 的 ASCII 身份规则未被改成技能名称规则。

当前源码摘要 `0e2b8be044bf99e906ede6e4cab673536498d7989621d3f868f4832022d65ce6`，
对应 `target/g09-evidence/unicode-source-checkpoint.json`；相对上一 URI/CLI 切片
有 6 个 Rust 文件变化，未提交。

| 要求/实际入口 | 证据 | 当前结果 |
| --- | --- | --- |
| M02/M03/M04：共享名称、字符边界与 namespace 原身份 | `cargo nextest run -p bitrouter-sdk --all-features --lib -E 'test(mcp::skills::tests::)' --no-fail-fast --test-threads 4`；`target/g09-evidence/unicode-sdk.log` | **20 passed**，退出 0；64 个多字节字符可用、65 个拒绝，大小写/titlecase/emoji 拒绝；编码中文 URI 聚合/反向映射保持原身份和 frontmatter |
| A27/M01–M03：真实协议回归 | 同包 `--test mcp_skills_gateway`；`target/g09-evidence/unicode-gateway.log` | **7 passed**，退出 0；能力门控/重连/完整分页/显式 URI/同名来源反向读取与基准清单保留 |
| A14/A28、M04/M05：native 与 Core | orchestrator 的 `--test core_execution --test mcp_skill_host -E 'test(native_skills::) \| binary(mcp_skill_host)'`；`target/g09-evidence/unicode-native.log` | **42 passed**（26 Core + 16 host），退出 0；独立 MCP 进程返回编码中文 URI，元数据零文件读取，随后根正文与相对支持文件校验通过；原缓存/批准/缺失来源与实际 native 模型链路回归通过 |
| A08–A12/A15：实际 CLI 安装与任务回归 | bitrouter 的 `--lib --test skills_lifecycle --test managed_daemon -E 'binary(skills_lifecycle) \| test(skills::) \| test(explicit_skill_constraints_require_advertised_native_support)'`；`target/g09-evidence/unicode-cli.log` | **43 passed**（21 lib + 13 生命周期 + 9 CLI/HTTP），退出 0；中文/Unicode 数字名称真实 install/show/uninstall 与原版本保留；原事务/Git/script-not-executed/并发更新和 daemon 约束/缓存链路保留。3 个原生命周期慢例均最终通过，81.193 秒测试运行时间 |

本轮四个定向命令共 112 项通过，不等同整个工作区 nextest 或完整 G09。
`cargo clippy --all-features --all-targets` 退出 0，日志
`target/g09-evidence/unicode-clippy.log`；仅原 onboarding 两项和依赖
future-incompat 提示。`cargo fmt -- --check`、`git diff --check` 和三个 plugin
manifests 的 JSON 解析通过；主 SKILL.md 保持 200 行。支持文件的 native 模型工具交付、
cold explain、未决评估重评估、嵌套 Git 项目库根、其余 URI 结构与模型预算、
完整批准撤销/超时/重启恢复组合及最终阶段 gates 仍需完成，不进入 G10。

### G09.6 原始技能事实的冷 explain（进行中）

新增 `bro skills explain --thread ID --turn ID` 与
`--managed-session NAME --request-id KEY`；`--agent-turn ID` 可限制到原输入内
的具体 Core WorkUnit。两种身份不互相替代，也不接受 ACP 历史作为 native 事实。
查询沿原 Thread 分页/checkpoint replay 或 managed journal 读取，不构造 App、
连接来源、取得 owner/lease 或恢复执行；named remote-control context 明确拒绝。

- 结果包含原 catalog/约束、评估理由和 receipt、完整来源/版本/entry、批准、
  预算、失败及已验证正文的摘要和长度，不输出正文或完整模型 prompt。
- 模型证据从原已准备的 SDK prompt 中匹配确切正文块；MCP 必须是完整 user
  数据帧，不能用 selection/content 存在代替交付。prepared、已收到 report、
  已收到模型结果分别报告；退休步骤缺少 attempt 记录时为 unknown/null。
- 配置只解析 database 地址并只对其替换环境变量；其他 provider/MCP 配置
  损坏、来源文件或 workspace 已删除不会要求重新加载。显式 `--database-url`
  可查已有库；SQLite 强制 mode=ro，不迁移、不创建缺失数据库。
- managed 并发冷查询首次复跑暴露两次 SELECT 间版本切换的真实竞态：
  `skills-explain-cli-final.log` 为 10 passed/1 failed，错误为读取期间版本变化。
  生产 load 改为同一只读快照内先检查长度再取 payload；保留原大小上限和
  CAS，不以重试掩盖不一致。SQLite 用默认读事务，其他后端请求 repeatable
  read/read only；本轮仅实际验证 SQLite，未声明其他数据库运行资格。

当前源码摘要 `24b5e5177e116ec1d90cf8b380b31c6d3a98d6ea8140a840351b4f03ab218af4`，
HEAD 仍为 `54c822a34d4dbfd75d484ca35e108b497048efef`，未提交；完整文件摘要与
相对 Unicode 切片的 13 个 Rust 文件变化记录在
`target/g09-evidence/skills-explain-source-checkpoint.json`。

| 要求/实际入口 | 证据 | 当前结果 |
| --- | --- | --- |
| A15/A19/A28、M10：Core 原绑定和精确模型证据 | `cargo nextest run -p bitrouter-orchestrator --all-features --lib --test core_execution -E 'test(native_skills::) \| test(service::skills::tests::)' --no-fail-fast --test-threads 4`；`skills-explain-core-final.log` | **29 passed**（27 Core + 2 cold reader），退出 0；未批准时无正文/请求，确切 user 帧与实际 report 分开，历史 run 保留原版本；无 App 的 caller/Turn 拒绝与截断 checkpoint 拒绝 |
| A15/A19/A28、M05/M08/M10：实际产品 CLI/HTTP | `cargo nextest run -p bitrouter --all-features --test managed_daemon -E 'test(skills::)' --no-fail-fast --test-threads 4`；`skills-explain-cli-snapshot.log` | **11 passed**，退出 0；真实 JSON/human、native Thread 与 managed 选择、历史输入、停机、失去 owner 均走原库；删除来源/workspace、损坏无关配置后仍可查，错身份拒绝；行字节/版本、MCP/模型调用计数保持，未恢复执行 |
| A24：实际并发存储 | `cargo nextest run -p bitrouter --all-features --lib -E 'test(managed_store::tests::)' --no-fail-fast --test-threads 4`；`skills-explain-store.log` | **1 passed**，退出 0；256 次并发提交/读取的 bounded metadata 与 payload 始终属于同一版本 |
| CLI 契约 | 真实 `target/debug/bro skills explain --help`；`skills-explain-help.txt` | 两种 runtime selector、agent Turn、config/显式数据库存在；CLI reference、spec、README、架构文档、分发 skill 和三个 plugin manifests 同步；主 SKILL.md 200 行 |

首次 `skills-explain-check.log` 因误用不存在的 serde_yaml 依赖而失败；改用仓库
已有 serde_saphyr 后 `skills-explain-check-fixed.log` 的 all-features/all-targets
检查退出 0。`cargo clippy --all-features --all-targets` 退出 0，日志为
`skills-explain-clippy.log`；仅原 onboarding 两项及依赖 future-incompat 提示。
fmt/diff 与 manifest JSON 检查通过。
本节共 41 项定向用例通过，不能替代整个阶段最终 gates。

G09 仍进行中。绑定支持文件的实际模型工具交付、未决选择失效/重评估、嵌套 Git
项目库根、其余 URI 结构及更小模型预算、完整本地库/多 agent/撤销/超时/重启
执行恢复组合和最终阶段 gates 尚需完成。cold explain 的失去 owner 用例只证明
无恢复的历史检查；未释放 owner 继续 recovery-blocked。不进入 G10。

### G09.3 未决评估失效与安全重评估（进行中）

Core 的原评估 source 在 catalog 时捕获，遇 steering/signal 后仍保持旧值，
导致 `skills.selected` 持续返回 StaleRevision。新回归
`changed_workflow_reassesses_unselected_skills_without_replacing_the_catalog_or_replaying_paid_work`
在真实 Core 评估等待期间提交 steering 和 signal，先复现该失败：
`skills-reassessment-red.log` 为 0 passed/1 failed，明确报 source changed。

- 未选定且尚未失败的 selection 在原调度器安全边界重新捕获当前 prompt、
  context/signal/manifest revision 和剩余指令预算。catalog、约束、policy、
  原 acceptance 和先前 decision/用量 receipt 不变，不重新扫描或换 alias。
- 新评估请求包含当前 signal facts 与已应用的用户指令；旧评估结果先写原
  ledger，stale 结果不用于选择。同一个已提交 intent 不重新发送；改变的
  assessment 可以创建新 receipt，仍受原运行的决策次数/用量边界约束。
- journal 的不可变校验继续冻结 catalog/约束/policy；评估 source 与剩余
  预算在最终 selected 时一起冻结，之后不能变化。原 decision intent/outcome
  各自保持不可变，恢复不删除 stale 评估，也不再次执行已记录的评估。
- 首次修复后的 `skills-reassessment-green.log` 仍失败：测试错误地把选择时
  revision 等同于模型回答追加后的 revision。改为验证新 source 与原 source
  不同、最新用户/signal 事实进入新请求，并增加从 selected checkpoint 的
  完整 journal-tail 恢复，确认原目录、两个评估和准确选择均保留。
- `skills-reassessment-core.log` 的完整 Core 回归为 **340 passed/1 failed**
  （3 个原大树容量慢例通过）。失败项是已提交 rejection 的恢复被带入重评估。
  修正为仅重评估 `failure=None` 的未决选择；已拒绝任务直接结算原失败。
  该失败源码摘要为 `10ece8eaef26c7c63ed8ab7900c4591151f7a3c773b6bb3986b9081c0643f751`，
  对应 `skills-reassessment-before-rejection-fix-source-checkpoint.json`，保留失败证据。

修正后当前源码摘要
`7a008d142b96022fdd026c4ca4f809fd130202383aef5ac8e497a0a4a9355dbe`，
HEAD 未变，未提交；对应 `target/g09-evidence/skills-reassessment-source-checkpoint.json`，
相对 cold explain 切片变化 4 个 Rust 文件。

| 要求/实际入口 | 证据 | 当前结果 |
| --- | --- | --- |
| A13/A14/A17、M10：未决失效、原选择与恢复 | `cargo nextest run -p bitrouter-orchestrator --all-features --lib --test core_execution -E 'test(native_skills::) \| test(service::skills::tests::)' --no-fail-fast --test-threads 4`；`skills-reassessment-native-final.log` | **30 passed**（28 Core + 2 cold reader），退出 0；新评估用最新事实、catalog 更新只影响后续 Turn，原请求身份/旧用量保留，恢复后不重选；原 rejection 恢复用例也已通过 |
| A15/A17–A19/A28、M10：产品实际入口回归 | `cargo nextest run -p bitrouter --all-features --test managed_daemon -E 'test(skills::)' --no-fail-fast --test-threads 4`；`skills-reassessment-cli.log` | **11 passed**，退出 0；真实 task run/managed 的来源准入、内容批准、确切 user 上下文、离线根缓存、拒绝/取消及 cold explain 回归通过 |
| A24：静态检查与分发同步 | `cargo clippy --all-features --all-targets`；`skills-reassessment-clippy.log`；`cargo fmt -- --check`；`skills-reassessment-fmt.log`；`git diff --check` 与三个 plugin manifests JSON 解析 | 全部退出 0；Clippy 仅原 onboarding 两项及依赖 future-incompat 提示，主 SKILL.md 保持 200 行 |

spec、分发 skill 的 managed-core reference 与总表同步。本轮当前源码的 41 项
定向用例通过；之前完整 Core 回归的 340/341 结果及修正后定向通过分开记录。
以上不代表最新源码
完整工作区 gates 通过；全阶段检查仍在 G09 收敛后执行。本地 fixture 的评估
executor/模型请求不构成真实付费 provider 或远端 MCP 的资格。

G09 仍需完成绑定支持文件的实际模型工具交付、嵌套 Git 项目库根、其余 URI
结构、更小 host/model 预算、完整本地库/daemon/多 agent/批准撤销/超时/重启
执行恢复组合与最终 gates。没有进入 G10。

### G09.2/G09.5 嵌套 Git 项目库与显式 workspace（进行中）

起点 native adapter 固定使用实际 workspace 的 `.bitrouter/skills`，而 CLI
默认 install 使用最近 Git root。从仓库子目录安装后执行任务因此没有候选。
`skills-git-root-red.log` 的新回归先复现 **0 passed/1 failed**：候选数实际 0、
预期 1。没有改安装器默认路径或把项目库导出到 harness 发现目录。

- native discovery 优先存在的显式 workspace index，否则使用最近 Git root；
  无 Git 时仍为 workspace。空、损坏或不安全的显式库不会回退到更大范围。
- 精确绑定保存原 library 和实际任务 workspace。后续子目录安装不会把原
  Git-root 版本重定向到新库；读取仍验证原 caller/workspace、scope/receipt、
  路径/字节/权限与当前 Git 根关系。Git 关系失效后拒绝原父库读取，不替换来源。
- 主 spec、内部 CLI reference、分发 skill reference、架构文档与三个 plugin
  描述同步；未添加新的 flags、调度器、数据库或工具授权。

当前源码摘要 `815205ce18ec5da4442ac98976cad6502ae133e823cb10176a7cd23fe1255a6b`，
对应 `target/g09-evidence/skills-git-root-source-checkpoint.json`；相对重评估切片
有 3 个 Rust 文件变化，HEAD 不变、未提交。

| 要求/实际入口 | 证据 | 当前结果 |
| --- | --- | --- |
| A11/A12/A14：库范围与原版本读取 | `cargo nextest run -p bitrouter --all-features --lib -E 'test(skills::native::tests::)' --no-fail-fast --test-threads 4`；`skills-git-root-native.log` | **8 passed**，退出 0；Git 根发现、子库优先、空库/损坏清单/坏 symlink 不回退；创建/卸载子库后原绑定仍读原版本，Git 关系消失则拒绝；原更新/卸载/SQLite/Thread/caller/字节回归保留 |
| A12/A15/A17/A19：CLI→daemon→实际模型 | `cargo nextest run -p bitrouter --all-features --test managed_daemon -E 'test(skills::)' --no-fail-fast --test-threads 4`；`skills-git-root-cli.log` | **12 passed**，退出 0；新增真实 CLI 从 Git 子目录默认 install→必需 ID→daemon→HTTP 模型含根库正文，再显式安装子库→新输入含子库正文且无旧根正文；卸载子库后根 ID 必需任务失败、零新模型；停机冷 explain 保留两个原安装身份；原 MCP/缓存/批准/冷查询回归通过 |

`cargo clippy --all-features --all-targets` 退出 0，日志为
`skills-git-root-clippy.log`；仅原 onboarding 两项及依赖 future-incompat 提示。
`cargo fmt -- --check`（`skills-git-root-fmt.log`）、`git diff --check` 和三个
plugin JSON 解析通过；主 SKILL.md 保持 200 行。
20 项本地定向用例通过；不等于完整 G09、真实 provider 或远端 Git 认证资格。
Git-root 映射不再是已知未实现项；完整本地库更新/活跃中断/执行恢复组合仍需
验收。绑定支持文件、其余 URI 结构和更小 host/model 预算、完整多 agent/
撤销/超时/重启组合及最终阶段 gates 继续保留，不进入 G10。

### G09.2 URI 身份与按清单解析相对文件（进行中）

复核固定规范的 Resource Mapping/Relative References 后，修复起点依赖 URL
解析器的限制：rootless 路径被直接拒绝，URL join 可能改写来源拼写，相对 Unicode
文件不能按清单解码匹配。固定规范仍为 commit
`c1aa87e4c381c82b99aa37471d0eae4ab50f9985`；本轮保存
`target/g09-evidence/mcp-skills-stable.mdx`，SHA-256
`6e33a33762783a02dc4c13c889dda5b8d7c525649012cf77da12d80c04ce6851`。

- SDK、native host 与 CLI selector 共用通用 URI/IRI 语法；复用锁文件已有的
  fluent-uri 0.4.1，只新增 SDK 的直接依赖，没有升级其他包。按上一源码快照
  的 Cargo.lock 摘要核验，去掉这一条直接依赖后与原锁文件完全一致。
- 所有 scheme 均检查 URI 路径内的明确 SKILL.md 与父目录名；保留原 scheme、
  authority、大小写、query/fragment 和完整 URI。后两者参与原资源/entry/批准
  身份，但不当作目录路径，也不通过 URI authority 建立网络或文件系统访问。
- 相对文件按原 skill 根内的文件路径处理，匹配解码后的原清单并返回清单中
  的确切 URI；参数引用可精确命中原 URI。编码别名或参数版本对应同一相对
  文件时要求完整 URI，不能猜测默认版本。绝对引用也必须精确属于 held entry；
  越界、非清单和跨来源引用在资源 RPC 前拒绝，嵌套内容仍是数据。
- gateway 的命名空间和反向映射同样保留完整参数/片段；不改 frontmatter、
  摘要、大小、缓存字段或扩展字段。SDK/host 的具体结构错误原因继续向上传递。

失败记录：`skills-uri-paths-red.log` 首次因 fixture JSON 键移动 String 而编译
失败；修正 fixture 后 `skills-uri-paths-red-fixed-fixture.log` 真实复现
**0 passed/1 failed**，报 hierarchical-only 限制。第一次 host/Core 回归
`skills-uri-host.log` 为 **45 passed/1 failed**，其中一个 Core 用例标记 leaky；
失败来自 SDK 提前校验抹掉具体结构诊断，已改为保留原因。后续 46 项与本轮
最终 48 项均无 leak 标记；未修改泄漏/生产超时或跳过用例，首次标记保留，
最终阶段全量 gates 仍需观察。

当前源码摘要 `825c5cfc56d16a435973060d17edd91f87efec47fb2cf474492dfa09092e5be2`，
对应 `target/g09-evidence/skills-uri-source-checkpoint.json`；相对 Git-root 切片
有 6 个 Rust 文件和 3 个 Cargo 文件变化，HEAD 未变、未提交。

| 要求/实际入口 | 证据 | 当前结果 |
| --- | --- | --- |
| A27/M01–M03：SDK 与真实 gateway | `cargo nextest run -p bitrouter-sdk --all-features --lib --test mcp_skills_gateway -E 'test(mcp::) \| binary(mcp_skills_gateway)' --no-fail-fast --test-threads 4`；`skills-uri-gateway-final.log` | **126 passed**（118 MCP unit + 8 gateway），退出 0；新增真实参数/片段的 list/get/read 双向映射；原门控、重连、512 文件/16 MiB、分页、来源隔离与普通 resources 保留 |
| A14/A17/A28、M04–M06/M10 本地切片 | `cargo nextest run -p bitrouter-orchestrator --all-features --lib --test mcp_skill_host --test core_execution -E 'binary(mcp_skill_host) \| test(native_skills::) \| test(service::skills::tests::)' --no-fail-fast --test-threads 4`；`skills-uri-native-final.log` | **48 passed**（28 Core + 18 host + 2 cold reader），退出 0；真实 peer 覆盖 8 种 URI 结构、大小写/参数/片段原身份、中文相对路径、精确参数版本、歧义与越界零 RPC；原批准/缓存/恢复与冷查询保留 |
| A12/A15/A18/A19/A28：真实产品链路 | `cargo nextest run -p bitrouter --all-features --lib --test managed_daemon -E 'test(skills::) \| test(explicit_skill_constraints_require_advertised_native_support)' --no-fail-fast --test-threads 4`；`skills-uri-cli-final.log` | **36 passed**（23 lib + 13 CLI/HTTP），退出 0；未列出的带参数 rootless URI 经 CLI→daemon→metadata→精确批准→根正文实际 HTTP user 数据，system 无正文；读计数 1，停机 explain 保留原 URI；原本地 Git scope、MCP 门控和缓存链路保留 |
| A24：静态与分发 | `cargo clippy --all-features --all-targets`；`skills-uri-clippy.log`；fmt `skills-uri-fmt-final.log`、diff 与三个 manifest JSON 解析 | 全部退出 0；Clippy 仅原 onboarding 两项与依赖 future-incompat，主 SKILL.md 200 行；spec、CLI、资源架构、分发 reference 与三个 plugin 描述同步 |

当前源码三组定向命令共 **210 passed**，不等于完整工作区测试、真实 provider
或远端 MCP 资格。已验证的 URI 结构不再保留为未实现项；M04–M10 全部组合
尚未关闭。下一步是绑定支持文件的实际 native 模型工具交付，另需更小 host/model
预算、完整本地库/多 agent/撤销/超时/重启执行恢复及最终阶段 gates。不进入 G10。

### G09.5 绑定支持文件的模型工具交付（本地切片通过，G09 继续）

起点为 URI 切片 `825c5cfc56d16a435973060d17edd91f87efec47fb2cf474492dfa09092e5be2`，
保留所有已有工作区差异。范围对应 A14–A18/A28、M05–M10：复用原 Core 工具
intent/结果和 WorkUnit，支持按原本地 receipt/MCP held entry 按需读取。工具
只返回读取凭据，正文作为带来源的数据进入当前绑定的后续模型请求；不向历史
普通工具结果写入可被下一 Turn 误用的正文。新缓存状态须有协商 feature，旧
core 不能忽略后继续运行。没有新 registry、调度器、数据库、脚本执行或 TUI。

实现事实：

- 原 Core `skill_read {skill_id, reference}` intent/结果绑定当前 agent Turn、
  已选中且仍获批准的父来源；本地文件取原 bundle receipt，MCP 文件取原
  held entry。只解析原清单，未知/跨源/越界路径在资源读取前拒绝。
- 原 WorkUnit 的 candidate 按规范路径/确切 URI 保留已校验 UTF-8 字节，
  tool result 仅有 digest/size/父身份/交付事实。后续上下文使用受保护的 user
  数据帧，包含父 scope/版本或 MCP origin/entry digest 和原文件承诺；不会
  把正文遗留为普通历史工具结果，也不向下一 Turn 或未获同意的子任务共享。
- Native manifest/capabilities 增加 `native_skill_files_v1`，文件预算随原
  selection 冻结。旧 Turn 没有预算时不补工具；旧 core 拒绝恢复该状态。
  本地每次重读 retained 版本；MCP 每次缓存使用重验来源许可、原 size/hash。
  immutable history 校验保留逐文件内容与预算，恢复仍使用原 owner/fence。
- 读取前检查清单大小、host 单文件上限与累计带来源文件数据预算；完整工具
  批次可在提交文件前按原 pairing 预览显式 context 上限。混合工具批次仍在
  最终上下文/model 准入统一检查，不能截断、丢弃受保护正文或换模型绕过。
  已知源文件损坏/不可验证使已激活父来源的原工作失败，并经原取消/效果收束
  路径结束；取消/权限边界改变丢弃旧字节。未知 ACK 阻止下一模型和重复读取。
- 冷 explain 增加原文件 reference/sha256/bytes 与每个确切 SDK 请求中出现的
  文件承诺；human 输出同样列文件事实。不输出正文、不重新读源文件、不恢复
  owner。本地 update/uninstall 后继续解释原版本。

失败和审查记录：`skill-files-check.log` 第一次 check 因四处 limits 字段位置和
一个 port 委托缺失而编译失败；修正后 `skill-files-check-2.log` 退出 0。
`skill-files-core-1.log` 因测试子模块路径而编译失败；修正后的
`skill-files-core-2.log` 为 **2 passed / 2 failed**：未配对的工具历史导致预览
失败，另一处为测试错误码大小写错误。改用原 pairing 和真实 snake_case 后，
`skill-files-core-3.log` **32 passed**，随后 native/host/reader 回归
`skill-files-native-final.log` **54 passed**，CLI/lib/HTTP 回归
`skill-files-cli-final.log` **38 passed**（含两条真实支持文件 journey）。这些是
中间快照证据，最新源码的完整结果以下列 workspace 检查为准。

审查随后发现旧 evidence validator 只接受 MCP 主指令帧，未接纳已提交的文件
帧。首次 workspace nextest 在编译阶段主动中断，退出 **130**，日志保留为
`skill-files-workspace-nextest-interrupted.log`，无全量通过声明。校验器现要求
原父来源、MCP 的匹配批准，以及精确主指令或原文件 user 数据帧；不接受任意
来源文本。新增完整 journal 再恢复用例，不只恢复读结果提交前的 prefix。

最新 Rust/Cargo 集合摘要：
`876e6966b88b45db9430eb1d36670f973c40d599b6cb2300b38faf8e6fc3a854`，
`target/g09-evidence/skill-files-source-checkpoint.json`。相对 URI 起点改变 18 个
Rust 文件（含三个新文件），没有新增 Cargo 依赖或配置 schema 字段。

| 检查与日志 | 实际结果与范围 |
| --- | --- |
| `cargo nextest run -p bitrouter-orchestrator --all-features --test core_execution -E 'test(native_skills::files::)' --no-fail-fast --test-threads 4`；`skill-files-recovery-final.log` | **9 passed**，退出 0；实际上下文与下一 Turn 隔离、非法路径零读、篡改失败、单文件预算零读、取消、缓存恢复/feature gate/完整 evidence 再恢复、最终模型 count 拒绝且零新调用、未 ACK 不交付/重读、UTF-8/NUL 政策 |
| `cargo nextest run --all-features --no-fail-fast --test-threads 4`；`skill-files-workspace-nextest.log` | 对上述 `876e6966…` 快照 **4,330 passed / 22 skipped**，退出 0，含 10 项 slow；完整结束，无失败/超时/泄漏。耗时 824.528 秒；随后新增的同批 wait/read 用例与修正须单独验证，不复用此完整通过声明 |
| Clippy / doctest / fmt / dist/schema / diff / manifests | 同批 wait/read 修正后的 `689b1e8c…` 验证通过，见下节；不反填为 `876e6966…` 的新结果 |

spec、内部 CLI、资源架构、主 skill（保持 200 行）及其 references、三个 plugin
描述同步了真实工具与资格边界。G09 尚不完成，G10 未开始。仍须完成预算与
权限/manifest 改变、子任务、活跃本地更新/卸载、重启/失联 owner 的组合证据，
并审计 A11–A19/A24/A27/A28/M04–M10 与全部源码 gates；这些不由本节 happy path
或较旧的全量通过次数替代。

### G09.5 同批 wait/read 的进展边界（本地通过，G09 继续）

在完整 `876e6966…` 回归结束后补充 A14/A16 场景：父 Turn 已获 MCP 批准，
子任务保持其自己的未批准状态；父模型同一批次请求 `wait_agent` 与
`skill_read`。测试先让子任务稳定等待批准，避免用短 wait 超时掩盖读取循环。
首次测试编译失败仅为 fixture 的 `Work` 类型误当 JSON 使用
（`skill-files-mixed-wait-red.log`）；修正 fixture 后，旧生产实现真实失败于
5 秒进展期限（`skill-files-mixed-wait-red-compiled.log`，**0 passed/1 failed**），
随后取消并收束原任务，无泄漏或放松期限。

原因是读取边界允许 Runnable/WaitingTool，却把同批 wait 产生的 WaitingMessage
视为失效，反复取消读取而未结算 intent。现在同一原状态、步骤、manifest、signal
和 context 边界下也允许 WaitingMessage；读取结果提交后仍按剩余 wait 保持该
状态。没有提前结束 wait、让未批准子任务执行、启动额外模型或扩大权限。

相对全量快照只改三个 Rust 文件；最新源码摘要
`689b1e8c6e84b35792754f45919b4186be609d01f68ad3f7ad0396536348da24`，
记录于 `target/g09-evidence/skill-files-wait-source-checkpoint.json`。
当前源码实际终态：

| 命令与日志 | 实际结果 |
| --- | --- |
| `cargo nextest run -p bitrouter-orchestrator --all-features --lib --test core_execution --test mcp_skill_host -E 'test(native_skills::) \| test(service::skills::tests::) \| binary(mcp_skill_host)' --no-fail-fast --test-threads 4`；`skill-files-mixed-wait-final.log` | **58 passed**（38 Core native skills + 18 host + 2 cold reader），退出 0；新场景 0.806 秒内结算文件、保持等待，零未批准子模型与额外调用；保留原 5 秒期限 |
| `cargo clippy --all-features --all-targets`；`skill-files-clippy.log` | 退出 0；仅原 onboarding 两个 byte_char_slices 与依赖 future-incompat 提示，无新增代码警告 |
| `cargo test --workspace --all-features --doc`；`skill-files-doctest.log` | **6 passed / 1 ignored**，退出 0 |
| `cargo fmt -- --check`；`skill-files-fmt.log` | 退出 0 |
| `cargo run -p dist-helper -- check`；`skill-files-dist.log` | 退出 0；配置 schema 和 registry dist 均为最新，无生成差异 |
| `git diff --check`、三个 plugin manifest JSON 解析、主 SKILL 行数 | 退出 0；JSON 可解析，SKILL 保持 200 行；源码集合未在检查中漂移 |

逐命令时间、exit code 与日志关联保存在
`target/g09-evidence/skill-files-check-results.json`。完整 4,330 次通过只属于前一
快照；最终 G09 收敛时仍须按最终源码重跑完整 gates。G09 保持 active，未进入
G10；此处没有提交、推送、PR 或真实 provider/远端 MCP 资格声明。

### G09.6 最终组合验收与收敛（本地完成）

本次沿用 `689b1e8c…` 的实现与证据，补齐卡片的边界组合。测试仍只使用临时
配置、库、SQLite 与 Cargo-built Rust/本地 HTTP fixtures；没有连接用户 daemon、
真实 provider 或远端 MCP。新增/强化的可复现入口如下，最终源码 gate 另列：

| 要求 | 测试入口与实际断言 |
| --- | --- |
| M04 / A12 | `native_skills::same_named_local_and_two_mcp_sources_keep_separate_consent_and_model_provenance`：同名本地与两个同 URI 的 MCP 来源保持三个 ID、两个独立批准和不同 system/user provenance |
| M08 / A14/A18 | `native_skills::any_support_manifest_change_requires_new_consent_even_when_root_bytes_are_identical`：支持清单分别增/删/改、根字节不变；下一 Turn 不复用旧批准，实际请求保留各自完整 entry |
| M07 / A18 | `native_skills::files::permission_or_tool_manifest_changes_cancel_support_and_fail_the_original_turn`：读取中分别改变权限 revision 或工具清单，取消原读取、零缓存/后续模型，原 Turn 明确失败；`a_nested_skill_file_is_data_and_never_gets_activation_or_tool_grants`：嵌套 SKILL.md 仅为数据 |
| M09 / A13/A14 | `native_skills::four_skill_bound_excludes_optional_sources_and_rejects_five_required_sources`、`initial_context_budget_excludes_optional_roots_and_fails_required_roots_before_reads`、`files::a_smaller_turn_context_rejects_support_without_caching_or_truncating_it`：四项/初始上下文/支持文件上下文分别计量，拒绝或排除不截断 |
| A11/A15/A17 | `managed_daemon::skills::active_local_support_survives_update_uninstall_and_keeps_original_file_evidence`：真实 CLI 安装并加载根/支持文件，任务仍活跃时更新且卸载，后续模型仍收到 V1；新任务必需项失败，停机冷 explain 保留原文件摘要；`skills::native::tests::managed_native_model_and_reopened_sqlite_keep_the_original_skill_receipt`：同一 SQLite 重开与 NativeSession 恢复保留原根/文件，无新模型 |
| M07/M10 / A16/A18/A19 | `managed_daemon::skills::managed_cli_child_inherits_required_selector_but_waits_for_its_own_skill_consent`：真实 CLI→daemon→父模型 spawn→子待批准；父批准重试不放行子模型，子单独批准后父子完成，冷 explain 保留两份绑定与批准 |

组合验证保留了失败终态，而不是将失败改成通过：

- `g09-audit-files-1.log` 为 **12 passed / 1 failed**。原测试错误预期已使用根
  正文的 Turn 可在权限改变后原地重新批准；源码实际且应当取消读取、失败原 Turn。
  `g09-permission-diagnostic.log` 核实原批准/字节没有替换；修正断言后，
  `g09-audit-native-final.log` 为 **64 passed / 1 failed**，唯一失败是新 fixture
  计算完整 entry digest 时未按 URI 排序；修正为与 wire 校验一致的规范摘要。
- `g09-reopen-and-child.log` 为 **1 passed / 1 failed**；SQLite 重开通过，真实子
  任务暴露批准通知竞争。`managed-status` 可在异步通知取得锁之前提交原请求的
  有效答复；旧通知随后把已离开 WaitingMaterial 的状态当作致命错误。现通知在
  已通过原 owner/dispatch/budget/commit 门后识别已结算请求，无新授权或执行。
- 修正通知后 `g09-audit-cli-final.log` 为 **38 passed / 1 failed**，子任务已完成，
  父任务失败于 `context rebuild changed session attribution`。共享子结果改变
  首条 User 的低置信推断指纹；现在仅当新旧投影均来自该指纹时保留原观察，
  显式会话/native 身份与 principal/controller/route-lease 门保持原约束。
  `session_identity::tests::context_revalidation_*` 检查共享结果成功且冻结观察不变，
  以及新增/删除/改变显式 workflow 或改变 native 身份仍拒绝。

修正后的 `g09-attribution-revalidation.log` 为 **3 passed**，包含两项身份校验与
实际父子任务；`g09-final-native-targeted.log` 为 **65 passed**，退出 0。后者
Nextest 对纯 MemoryExecutionStore 的 truncated-checkpoint 冷查询用例报告一次
非失败的 LEAK/输出句柄等待提示；该入口不构造 App、来源或子进程，完整回归
另行复验，不以此推断已执行任何 MCP/模型请求。

最终源码摘要为 `1bcd4cc175c2e91321b772408f332865daf81a305b3a9c0660e2612ed4ff3d7a`，
相对 `689b1e8c…` 六个 Rust 文件变化，生产修复仅 `skill_consents.rs` 与
`session_identity.rs`，其余为新增/强化测试。完整集合记录在
`target/g09-evidence/g09-final-source-checkpoint.json`；最终完整 gates 已在该快照通过，逐项结果如下。
G09 本地完成；G10 未启动，没有将失联 owner 自动恢复、接管或重放。重启恢复遵守 G08 的原
所有权契约：经授权 Core/NativeSession 恢复原缓存，与 daemon 失联 owner 保持
recovery-blocked、冷 explain 零执行，分别验收，不互相冒领。

最终 gate 记录在 `target/g09-evidence/g09-final-check-results.json`，与上述源码
摘要一致；本次最终完整结果为 **4,341 passed / 22 skipped**。

| 命令与日志（均位于 target/g09-evidence） | 实际终态 |
| --- | --- |
| `cargo nextest run --workspace --all-features --no-fail-fast --test-threads 4`；`g09-final-workspace-nextest.log` | 退出 0；源码集合未变化 |
| `cargo clippy --all-features --all-targets`；`g09-final-clippy.log` | 退出 0；源码集合未变化 |
| `cargo test --workspace --all-features --doc`；`g09-final-doctest.log` | 退出 0；源码集合未变化 |
| `cargo fmt -- --check`；`g09-final-fmt.log` | 退出 0；源码集合未变化 |
| `cargo run -p dist-helper -- check`；`g09-final-dist.log` | 退出 0；源码集合未变化 |

完整 Nextest 的 22 项 skipped 为既有 opt-in/ignored 资格；未增加跳过或改产品
超时。最终日志未报告 LEAK；定向日志的一次提示保留在上文。编译保留链接器
`__eh_frame` compact-unwind 提示及依赖 `proc-macro-error2` future-compat 提示；
Clippy 的原 onboarding 两个 byte_char_slices 提示未在本阶段扩大范围修改。
文档测试 **6 passed / 1 ignored**；schema/registry dist 为最新。
本地工具链为 rustc **1.97.0 (2d8144b78)** / aarch64-apple-darwin，见
`g09-final-toolchain.log`；Cargo 构建并发 2、incremental 关闭，dev/test debug
设为 0，测试并发 4；同一轮进程测试与其他 Cargo 检查串行安排。

四条真实二进制 help（`g09-final-help-{run,managed,answer,explain}.log`）均退出 0，
核对 include/exclude、原答复 identity 与两类冷 explain 参数。JSON/human 和
实际模型请求由同一完整回归中的 CLI/HTTP fixtures 验证；没有用 help 代替
业务链路。文档审计 `g09-final-doc-audit.json` 检查 178 条仓库文件链接、
fence、三个 JSON manifest 与 200 行主 SKILL；既有 `/docs/guides/cloud-api`
站内路由不按本地文件判定，也未据此声称远端已验证。`git diff --check` 通过；
文档修订不改变源码摘要，最终文件摘要在文档核验后刷新。

G09 的最终本地切片逐项对应如下；这里的“通过”限定为固定规范与临时本地
fixtures，不包含真实远端 MCP/provider、外部 ACP adapter 或 hosted CI。

| 要求 | 真实入口与可复现证据 | 本地判定 / 后续边界 |
| --- | --- | --- |
| M01 | SDK `mcp_skills_gateway::current_connection_declarations_gate_methods_without_upstream_requests`、`reconnect_uses_new_declaration_and_resources_stay_independent`；host `native_loading_rechecks_connection_capabilities_and_rejects_undeclared_methods` | 连接实际声明与重连复验通过；gateway 不声明可选 directoryRead |
| M02 | gateway `real_pages_and_colliding_origins_preserve_entries_and_route_reads`、`http_gateway_direct_lookup_is_independent_of_listing_and_preserves_errors`、`metadata_validation_rejects_incomplete_entries_without_inventing_manifests` | 完整 entry/缓存/分页/get/error 通过；零补造清单 |
| M03 | gateway `unsupported_members_and_unaggregatable_schemes_keep_explicit_diagnostics`、`query_and_fragment_uris_keep_exact_identity_through_real_gateway_mapping` | 原来源、精确 URI/IRI 与反向 read 通过；原 OSS origin 边界保留 |
| M04 | host `explicit_source_caller_and_workspace_grants_precede_metadata_and_direct_lookup`、`source_identity_scope_and_manifest_bound_reads_cannot_be_replaced`；Core 同名三来源测试；CLI unlisted/rootless URI 测试 | 精确 caller/workspace/配置来源和直接 URI 准入通过；默认无 MCP skill 来源许可 |
| M05 | host `every_network_and_cached_use_checks_bytes_and_complete_frontmatter`；Core `mcp_consent_is_metadata_only_then_binds_the_exact_entry_in_the_model_request`、`files::supporting_file_is_lazy_verified_user_data_and_does_not_leak_to_the_next_turn`；managed 支持文件模型 HTTP journey | 元数据/批准零文件读取，按需逐字节校验和实际模型 user 数据通过；正文不提升为 system 指令 |
| M06 | host manifest-bound、URI ambiguity、nested-data 测试；Core root/file cache restore 与 protected history；后续 Turn 原 WorkUnit 与冷 explain | 仅 held manifest/原来源，越界/未知/歧义拒绝；原 entry 在正文留存期间不被 refresh 替换 |
| M07 | native v19/HTTP/managed-answer 独立批准；Core nested file、tool/permission-change、unconsenting-child；实际父子分别批准 journey | 各自 Turn/epoch/完整 entry 同意，父批准与 allowed-tools 不扩大 grant；旧答复/撤销/取消不放行 |
| M08 | Core 支持清单增删改重新批准、root/file corruption 与恢复；host offline/config-change tests；SQLite 原缓存重开 | 每次复验 bytes/size/hash，完整 manifest 改变不复用旧批准；缓存不参与 filesystem discovery，撤销来源/config 不恢复权限 |
| M09 | gateway/host 完整 512 文件/16 MiB manifest；host dynamic/metadata/binary；Core 四项/64 KiB/初始与支持上下文、provider input-count tests | 元数据与内容预算分离；dynamic 必需失败/可选排除；超限/二进制/编码失败不截断、不预取 |
| M10 / A11–A19 | task run、native Thread、managed CLI→daemon→Rust MCP→模型请求；活跃本地更新/卸载、精确 SQLite/认证 Core 原文件恢复、取消和停机/失联 owner 冷 explain | native 本地完成；失联 daemon owner 仍 recovery-blocked，未自动接管/重放；外部 adapter 交付属于 G10，跨阶段整体验收属于 G11 |
| A24 / A27 / A28 | CLI/reference、200 行主 SKILL、三个 plugin manifest 与 schema/dist；完整工作区/Clippy/fmt/doc gates | A27 与 A28 native 切片本地完成；A15/A28 外部交付资格继续单列，未提前进入 G10 |

## 15. G10 执行记录（本地完成）

- 用户已启动 G10；G11 未启动。G09 的最终源码摘要
  `1bcd4cc175c2e91321b772408f332865daf81a305b3a9c0660e2612ed4ff3d7a`
  与当前 Rust/Cargo 集合一致，G09 五项最终 gates 均退出 0。保留所有 G00–G09
  本地差异，不提交/推送；起始快照为 `target/g10-evidence/baseline.json`。
- 现有外部 ACP 属于 app supervisor / SessionHost / AcpClient；其运行、控制
  fence、队列、权限和取消继续由原所有者负责。G09 native Core 选择与批准
  不被推定为外部 adapter 已交付。
- G10.1 正在核对 registry 固定的 Codex ACP 1.10.0 与 Claude Agent ACP 0.75.1
  上下文路径和实际协商能力。只读核对上游源码，不安装、登录或运行真实 adapter。
- 本地交付目标包括精确来源/版本/字节/路径、预算、取消、能力和 unsupported
  结果；MCP 来源若不能保持 host 批准及完整 held manifest 责任则拒绝，不能
  通过 prompt 或文件伪装为本地技能。Codex 与 Claude 用独立 fixture 分别证明。
- 起点未改动运行源码或执行本阶段测试；后续进度与实际证据分别记录如下。

### G10.1 载体契约与共享策略（定向本地通过）

- 只读核对 registry 固定版本的源代码：Codex tag `v1.10.0` 对应 commit
  `061f9a4a2e463a220d7a3ab2ae5e9732837085ef`，`src/CodexAcpClient.ts` 的
  `buildPromptItems` 将 text 原文转为模型 text input；Claude tag `v0.75.1`
  对应 `3e23c5b960b66a6d2c892e7524c952e731c076a7`，`src/acp-agent.ts`
  声明 embeddedContext，并在 `promptToClaude` 把 text resource 作为 user context。
  原文与 tag/tree 只读材料在 `target/g10-evidence/`，未运行 npm、登录或实际 adapter。
- `acp::skills` 分别实现 Codex text / Claude embedded-context prompt 载体。
  只有 host 配置的 invocation 识别 profile，不能由 agentInfo.name 取得权限；
  Claude 另检查本连接声明的 embeddedContext。能力明确不包括 native 激活、
  受保护 bundle 访问和 MCP-origin host 责任，后者拒绝而不是转为本地来源。
- 根正文重验原字节/UTF-8/摘要与 caller/workspace；传递完整原安装 receipt、
  whole-bundle version、原版本目录与具体 ACP session/Turn，四项/64 KiB 和
  序列化 prompt 上限不截断。候选问题、Noul 结果应用和预算 policy 提为
  `core::skills::selection` 纯函数，原 Core 决策 ledger 与所有权不迁移。
- `acp_codex_skill_fixture.rs` 与 `acp_claude_skill_fixture.rs` 是两份独立 Rust
  进程/解析器及模型输入转换断言，既不运行真实 vendor SDK，也不以通用 echo
  代替两条载体。`carrier-and-native-targeted.log`：**70 passed**，退出 0，
  包含 5 项独立载体/准确字节与版本路径/未声明能力/坏字节与错 scope/小预算/
  两条取消路径，以及原 65 项 native/host/cold-reader 回归。
  对应源码摘要为 `ab6c37f604d26d13e8cb7868040a61aeb51f84aad71516ae4cf789563a50e2f1`，
  详见 `carrier-source-checkpoint.json`。首次 compile 失败仅是新 fixture 错用
  Tokio oneshot 类型；修正为既有 AgentProcess 使用的 futures oneshot 后重验，
  `carrier-and-native-compile-red.log` 保留原终态，不删除失败。

### G10.2 Supervisor/CLI 接入（已实现并完成本地验收）

- `run --background` 增加相同 scoped `--skill`/`--exclude-skill` 约束；非 daemon
  的直接 prompt/transparent serve 不静默忽略新约束。MCP 必需项在执行前报
  unsupported，普通 `--approve-all` 不能改变该准入。其他 ACP 会话操作继续存在。
- 外部 owner 从原安装库获取元数据，复用原 DecisionRuntime 和共享策略；没有
  backend 时可选集为空。原 supervisor ledger 在决策 dispatch 前记录 intent，
  之后记录原 outcome/用量，并在准备回执持久确认后才发送 ACP prompt。
  未确认结果不重放；session/输入代次及原 manifest/whole-bundle 身份不被替换。
- 原 ledger 增加最多 64 份/4 MiB 的每输入事实，达到容量明确失败而不丢弃历史；
  重启仅恢复观测事实，不把 interrupted owner 变为 runnable。新 per-input
  cancellation token 在发现/评估/读取期间可取消；执行后仍由原 ACP cancel 收束。
- supervised local protocol 为 v3，继续接收 v1/v2 普通操作；旧版请求携带新
  约束时拒绝，新客户端不会让旧 daemon 静默吞掉约束。Native Thread v19、
  HTTP v2 的技能批准规则不变。`agents list` 增加各路径交付能力，实际连接
  能力与输入 facts 由 `agents show` 查询；launcher/inference 等保留功能且报告
  新技能分配 unsupported。
- `supervisor-check.log` 中途 all-targets check 退出 0，仅为当时编译检查，不
  代替旅程与阶段 gates。当前实际 CLI→daemon→两条 Rust fixture 的验证正在运行，
  日志 `supervisor-journey.log` 与内容摘要 `supervisor-journey-source.json` 关联。
  以下 G10.3 记录后续旅程与修正；中途编译及前一快照的 70 项结果不单独构成完成。

### G10.3 边界旅程与文档同步（本地通过）

- 两条真实 CLI→临时 daemon→独立 Rust ACP peer 路径已验证 workflow 自动
  选择/不匹配/显式排除、原 DecisionRuntime 的 intent/outcome 与用量、完整
  安装 receipt 与版本路径。Codex 支持文件单独更新会产生新 whole-bundle
  version；旧输入绑定保持原值，卸载阻止后续必需选择，cold show 不重放请求。
- 两个 profile 分别验证工具执行审批的负例和正例：skill 中 `allowed-tools`
  不能覆盖 ACP execute 请求，只有实际获批的 fixture effect 才生成。交付后
  cancel 保持原 receipt/唯一 prompt；超预算必需项、未知 profile、Claude 未声明
  embeddedContext、非 background 约束及 MCP 必需项都无 prompt dispatch。
- 门控本地 HTTP 评估服务精确停在原 intent ACK 之后，分别验证评估期间取消
  （结果保留 may_have_run，不继续发送）和将 fixture 自有 ledger 目标替换为
  目录后的持久确认失败（原 durable intent 保留，未发送 ACP）。旧 v1/v2
  Start 即使携带空 skills 对象，也在授权/进程副作用前拒绝。交付后错误保留
  uncertain 与原 receipt，cold restart 不重放；`supervisor-final-targeted.log`
  最终 **26 passed**，退出 0。
- `supervisor-barriers.log`：**25 passed**，退出 0；包含以上 CLI 边界、原
  supervisor 权限/fence/取消/ledger 回归。前序失败如实保留：
  `supervisor-journey-install-report-red.log` 使用错 skills install JSON 外壳，
  `supervisor-journey-cli-shape-red.log` 使用错 follow-up CLI 参数；修正 fixture
  为真实输出及 `--prompt-file`。`supervisor-boundaries.log` 22/23 通过的失败
  暴露能力表只接入 fallback；已补到普通 `agents list` 实际 daemon report。
  `supervisor-current.log` 22/23 的取消失败来自读取未写完的 NDJSON 尾行；现在
  只读取换行提交的完整记录，完整坏记录仍报错，修正后上述 25 项通过。
- `carrier-final-targeted.log`：**72 passed**，退出 0；保留原 65 项 native/host
  回归，新增两条独立载体、未协商/坏字节/错 scope/预算/取消、有效完整 MCP
  held entry 与仅支持文件变化后的拒绝、原生 slash command 原消息形状。
  不以同一通用 echo 证明两条路径，也不把 wire shape 测试说成真实厂商命令成功。
- CLI/reference、两份 harness reference、sessions、README/架构、分发 skill
  与三份 manifests 同步新 flags、supervisor v3、能力边界和真实会话未验证。
  SKILL.md 保持 200 行。native Thread v19/HTTP v2 没有因此改版。
- `model_context_budget_verified: false` 明确只保证本地序列化字节准入；外部
  vendor token admission、history 保留/压缩、后续 Turn 移除旧文本均不作保证。
  Prompt 载体不提供 native activation/受保护支持文件访问；MCP 来源始终
  unsupported，不因落文件、URI 或普通执行批准而转成本地来源。
- 最终完整 gates、源码摘要、help/JSON/human 和文档审计结果见 G10.4。

### G10.4 最终源码与逐项验收（本地完成）

- HEAD 保持 `54c822a34d4dbfd75d484ca35e108b497048efef`；未提交/推送。相对
  G09 最终集合，本阶段只改变 20 个 Rust 文件，未改变 Cargo manifests/lock。
  完整可重现集合、各文件摘要与 G09 比较见
  `target/g10-evidence/g10-final-source-checkpoint.json`。
- 完整 gates 的 Rust/Cargo 摘要：
  `724e7d56c6d9aca57a2548c8ddabe3ff122cfcd02ac55de602d7531b417a105a`。
  定向后修正载体 fixture 的同类完整行读取；完整 gates 使用该集合，
  各 gate 前后检查同一摘要。之后的测试 helper 清理及最终集合另记如下。
  原 G00–G09 编辑保持，G11 未启动。

| 范围 | 真实入口 / owner 与证据 | 本地结果 / 保留边界 |
| --- | --- | --- |
| A20 Codex | CLI install → run background → 原 supervisor → 独立 Codex parser/model input；`codex_cli_delivers_exact_local_binding_and_retains_two_versions_after_restart` | 交付原根字节/receipt/路径，支持文件更新改变 whole-bundle version，旧输入保留；cold query 无重放 |
| A20 Claude | 同一原 owner、独立 Claude embedded resource → user message；`claude_cli_uses_negotiated_embedded_data_and_keeps_source_receipt_in_original_ledger` | 本连接须声明 embeddedContext；user data 不提升为 system/native activation |
| A20 选择与预算 | `workflow_choice_and_explicit_exclusion_use_the_original_daemon_decision_runtime`；`each_adapter_rejects_required_over_budget_instructions_without_partial_prompt` | 原决策 intent/outcome/用量与选择/排除；超预算必需项无 prompt，不截断 |
| A20 取消与失败 | `both_adapters_block_dispatch_after_cancel_or_unacknowledged_decision_outcome`；`both_daemon_owned_adapters_cancel_the_original_prepared_delivery_without_replay`；`both_adapters_retain_uncertain_original_delivery_after_error_and_cold_restart` | 两条 adapter 的评估取消/持久化 ACK 失败阻止 dispatch；执行后取消和 uncertain 保留原身份，不重放 |
| A20 原操作与权限 | `both_adapters_keep_tool_approval_separate_from_local_skill_allowed_tools`；`native_slash_controls_keep_their_original_single_text_shape`；`legacy_supervisor_constraints_are_rejected_before_authority_or_adapter_effects` | 实际 permission effect 正/负例；原命令 wire shape；旧协议在效果前拒绝新约束，其他 ACP/native 回归保留 |
| A28 外部切片 | `valid_mcp_entries_remain_unsupported_for_both_carriers_without_laundering`；`mcp_constraint_and_unsupervised_delivery_are_rejected_before_adapter_requests` | 有效原 held manifest 及仅支持文件变更均保留 MCP 来源并拒绝；approve-all 不改变准入；不声称 MCP host 激活 |
| A24 分发 | 实际 `bro run/agents show/agents list --help`，真实 CLI JSON/human 断言；CLI/skill/harness references/三份 manifests 同步 | 本地链接、JSON 和 SKILL 200 行审计；能力 contract 与实际连接分开，未验证项不隐藏 |

上述测试由 `carrier-final-targeted.log`（72 passed）和
`supervisor-final-targeted.log`（26 passed）记录；最终完整运行包含新 18 个测试。
可复现定向命令：

```sh
cargo nextest run -p bitrouter-orchestrator --all-features --lib --test acp_skill_delivery --test core_execution --test mcp_skill_host -E 'test(native_skills::) | test(service::skills::tests::) | binary(mcp_skill_host) | binary(acp_skill_delivery)' --no-fail-fast --test-threads 4
cargo nextest run -p bitrouter --all-features --lib --test management_cli -E 'test(skill_delivery::) | test(supervisor::tests::) | test(management_protocol_versions_reject_new_operations_before_effects)' --no-fail-fast --test-threads 4
```

完整 gates 的 `g10-final-check-results.json` 均退出 0，前后源码摘要一致：

| 命令 | 实际结果 | 日志（`target/g10-evidence/`） |
| --- | --- | --- |
| `cargo nextest run --workspace --all-features --no-fail-fast --test-threads 4` | 4,359 passed / 22 skipped，6 slow，退出 0 | `g10-final-workspace-nextest.log` |
| `cargo clippy --all-features --all-targets` | 退出 0；两条已有 onboarding byte-char 建议和一条新测试 helper 重复 module 提示 | `g10-final-clippy.log` |
| `cargo test --workspace --all-features --doc` | 6 passed / 1 ignored，退出 0 | `g10-final-doctest.log` |
| `cargo fmt -- --check` | 退出 0 | `g10-final-fmt.log` |
| `cargo run -p dist-helper -- check` | schema/registry 均 up to date，退出 0 | `g10-final-dist.log` |

Clippy 提示后的清理仅将 `tests/support/management.rs` 已有 example builder
模块开放给同测试 crate 的 sibling，并让 `management_cli/skill_delivery.rs`
复用该模块；不重复载入同一文件。两个文件的差异在快照中逐项确认，生产
Rust、示例和 Cargo 文件与上述全量集合完全相同。完整 gate 快照保留为
`g10-workspace-source-checkpoint.json`，最终快照是
`g10-final-source-checkpoint.json`，摘要
`1df4bc3e92a2c8d2b1d6c1fe7b5cf7fac0a7c78d248a4b35a17dd8deff9c4a00`。
清理后的完整 management_cli 二进制 **29 passed / 0 skipped**；all-targets
Clippy 与 fmt 再次退出 0，重复 module 提示消除，仅保留两条已有 onboarding
byte-char 建议及原依赖 future-compatibility 提示。结果单独保留于
`g10-fixture-cleanup-check-results.json`，不把旧摘要的全量结果说成新的测试
模块集合已重跑全 workspace。最终文档审计为 183 个本地链接、三份 manifest
JSON、200 行主 SKILL 和源码未漂移通过；`git diff --check` 退出 0。

完成判定：G10 卡片的两个独立载体本地契约、A20/A24/A28 相应切片与保持项
均有证据；本阶段无 required work 遗留。真实 Codex/Claude、provider/MCP、
hosted CI、其他 OS 和 vendor 模型历史/token 准入仍未验证。没有安装真实
adapter、登录/付费、修改用户全局 harness、提交或发布；G11 未启动。

## 16. G11 执行记录（本地完成）

- 用户已启动 G11，保持全部 G00–G10 本地差异与未提交文档。HEAD 为
  `54c822a34d4dbfd75d484ca35e108b497048efef`，起始 Rust/Cargo 摘要
  `1df4bc3e92a2c8d2b1d6c1fe7b5cf7fac0a7c78d248a4b35a17dd8deff9c4a00`
  与 G10 最终集合一致，起始快照在 `target/g11-evidence/baseline.json`。
- 范围：onboarding 可选 MCP/skills 复用已有事务；真实 CLI→daemon 的本地库
  与获许可 MCP 两条集成链；全部 A/M 的当前证据、替代入口与协议/权限/恢复
  边界；最终完整检查、normal/build/dev 实际依赖图和物理行数审计。
- 真实 adapter/provider/远端互操作/hosted CI 与其他 OS 资格独立，保持未验证。
  不修改全局 harness，不安装真实 adapter，不推送、发布或额外付费调用。
- 规范与阶段卡片已核对；本阶段 required 本地工作完成。整个 spec 的交付范围与未验证资格见 G11 附件第 6–8 节。

### G11.1 Onboarding 事务与端到端链（本地完成）

- `onboarding/resources.rs` 提供可选 MCP/skill 步骤与脚本 flags，MCP 共用
  `mcp_config::publish` 的原 source snapshot/CAS/校验/原子出版；skills 共用
  原 Store 安装事务，仅增加每次操作的交互许可，不创建新安装库/执行服务。
- Router 在资源步骤前保存。资源专用 no-op 保留原文与注释，已有配置没有
  新 provider 时不补造空节点；完成、跳过与失败保留原安全 receipt。headless
  部分失败只输出一个 onboarding report、非零退出，after 为 exit；后续操作
  不继续。交互失败可明确重试/继续；EOF/Ctrl-C 保留已保存进度并停止 Finish。
- `--yes` 的交互许可传入原本地/Git bundle 选择器，即使处于 TTY 也不等待
  多 bundle 选择。CLI reference、主 skill、三个 manifests 与历史 onboarding
  文档边界已同步；主 skill 保持 200 行。
- 原始红灯 `onboarding-resources-red.log` 保留未实现资源 flags 的失败；
  `onboarding-resources-initial.log` 与 `onboarding-resource-transactions.log`
  保留新 frontend 编译所需 Clone/私有模块边界错误。现通过 skills 根模块的
  实际 setup 调用入口复用私有 Store，不暴露内部类型或新增重导出。
- `onboarding-resource-transactions-fixed.log` **60 passed / 1 failed**：
  MCP/installer 原事务和全部新 PTY 用例通过；唯一失败是新测试对 receipt
  数组顺序的假设，修为按 kind 检查。随后
  `onboarding-native-acp-integration.log` **73 passed / 2 failed**：资源原文、
  取消/重试/隐藏输入/TTY --yes、人类输出和两种 ACP 经 onboarding 安装后的
  原交付/取消/版本/权限路径通过。
- 两个新 native 全链从实际 init/start、来源/安装、模型路由和 skill 数据、
  pending/answer、原版本卸载保留、explain 走到 daemon restart 时，测试错误
  假定被动 show 会恢复 workspace grant，收到真实权限拒绝。保留原拒绝；
  修正为显式 load，以及本地路径按实际检查点 recover 的正确契约。
  `native-integrated-recovery.log` 随后 **2 passed**；最终完整运行也通过这两条链。
  没有放宽被动查询权限、恢复 fence 或原 checkpoint 身份。
- 固定规范原文摘要 `6e33a33762783a02dc4c13c889dda5b8d7c525649012cf77da12d80c04ce6851`
  与附件一致。首轮 actual metadata（629 resolve nodes / 7 workspace members）
  与 normal/build/dev、all-features/all-target 依赖图未发现 TUI/Ratatui/Crossterm。
  这是首轮图证据；最终冻结集合的同项复验见 G11.2。
- 首次物理行统计（当时）验证 G00 的 790 文件/468,581 行基线，集合为 806 文件/
  465,393 行、净减 3,188 行；包含新增资源实现、测试与迁移，最终冻结计数见 G11.3。
  不以此前估计或删除行数代替验收，也不通过丢弃业务测试降低总量。

### G11.2 冻结源码与最终验收

- 最终 Rust/Cargo 摘要：`7b3a3f450dbef05915b6021b957fb220540175aaf9f7bf641b0c9223ebb87069`。
  G11 相对 G10 仅改 12 个 Rust 文件（其中 5 个测试文件）；Cargo manifests/lock
  无本阶段变更，Core/native/SDK/ACP 协议 runtime 未另造所有者或提升版本。
  原本地 v19、HTTP v2、supervisor v3 的协商和 fail-closed 边界保留。
- 4 条 onboarding flags/partial/human/Git-ref/bundle/user-scope 定向用例最终
  **4 passed**（`onboarding-final-options.log`）；3 条新 PTY retry/cancel/TTY
  --yes 用例、2 条新 native 全链和上述 4 条均包含在最终 4368 项运行中。
  原 11 条外部 skill-delivery 用例现在经真实 init 安装后再进入原 supervisor、
  独立 Codex/Claude parser 和模型输入；仍只证明本地 prompt 载体。
- 完整最终 gates：`cargo nextest run --workspace --all-features --no-fail-fast --test-threads 4`
  **4368 passed / 22 skipped**（6 slow）；`cargo clippy --workspace --all-features --all-targets`、
  `cargo test --workspace --all-features --doc`（**6 passed / 1 ignored**）、
  `cargo fmt -- --check`、`cargo run -p dist-helper -- check` 均 exit 0。
  另执行 locked/offline all-features metadata、all-target normal/build/dev tree、
  nextest JSON discovery，三者也均 exit 0；8 项 gate 前后摘要完全相同。
  使用本机 rustc 1.97.0 stable-aarch64-apple-darwin，jobs=2、incremental=0、
  dev/test debug=0，显式同一 RUSTC/RUSTDOC，Cargo 串行。命令、时间、终态、
  文件摘要和日志在 `target/g11-evidence/g11-final-check-results.json` 与
  `g11-final-source-checkpoint.json`；完整可复验命令亦见 G11 附件第 7 节。
- 既有两处 PTY byte_char_slices 建议、proc-macro-error2 的 future-incompatibility
  及 discovery 构建的 macOS compact-unwind 链接提示已记录，均不构成失败；
  未通过 allow、跳过业务测试或修改 timeout 消除问题。
- A01–A28、M01–M10、C01–C08、T01–T15 共 **61** 项，按实际请求、状态、
  字节和业务副作用断言核对边界；引用 **149** 个当前唯一测试，再逐项匹配本次
  真实 binary/package/module 的 PASS，不用函数存在或全量总数代替证明。
  `coverage-manifest.json`、`coverage-results.json` 保留细目，源码可复验入口
  完整列于 [G11 验收附件](CLI_ONLY_AND_WORKFLOW_SKILLS_G11_ACCEPTANCE.md)。
- 实际 metadata 的 **629 resolve nodes / 7 workspace members**、完整 tree、
  Cargo.lock、workspace manifests 与根 module/旧 driver 删除核验均无
  bitrouter-tui、Ratatui、Crossterm。similar、unicode-width、unicode-segmentation
  仍有非 UI 依赖，不按名字误删。all-target 图不等于各 OS 已执行。
- 真实 bro 的 **11** 组 help/JSON/human/部分失败/旧命令检查通过；source-safe
  receipt、单 report/nonzero、原 Router 注释、无隐式 daemon/正文/秘密输出验证
  通过。CLI、200 行主 skill、三个 plugin manifests、AGENTS、默认值与 wiring
  同步，23 份当前文档的相对链接/围栏、JSON 和 diff 空白审计通过。最终链接
  数见 G11 附件及 `g11-final-doc-audit.json`，不将站点路由当本地文件检查。
- 22 个 ignored 经真实 discovery 与源码逐一核对：maintained ACP campaign 10、
  真实 Hermes/Codex/Claude workflow-state 9、BYOK search/extraction 2、长时
  Docker HTTP fault 1；一个 doctest 示例另行 ignored。它们没有运行，也没有
  计为通过。真实 adapter/provider/MCP/Git auth、hosted CI、Linux/Windows 仍
  unverified，外部 ACP MCP-origin allocation 仍明确 unsupported。

### G11.3 实际行数与整个 spec 的完成范围

G00 HEAD 的 790 个 Rust 文件 / 468581 物理行，最终为 **806 / 465518**，
净减 **3063** 行。删除整个文件 49 个 / 38538 行，新增整个文件 65 个 /
33902 行，修改 82 个仍存在的文件 +5299 / −3726；总计 **+39201 / −42264**。
口径含测试、注释、空行；tracked HEAD 对当前 tracked + 非忽略 untracked `.rs`，
不含 docs、lock、schema、target 或第三方依赖。逐文件统计及相同源码摘要在
`source-statistics.json`。G05 UI 删除阶段为 439345 行（净 −29236）；G06–G11
管理、安装、host/adapter 与业务验收净新增 26173 行。最终数字取代此前估算，
不通过删除业务测试追求净减少量。

整个 spec 的第一方 UI 删除、有限行式 onboarding/后台 readiness、只读 connected
agents、MCP 配置 CRUD、版本化本地/Git lifecycle、原 native/ACP orchestration
控制与恢复、native workflow skills/MCP host、本地 Codex/Claude prompt carrier
均已完成约定本地范围。gateway、本地安装、native activation、外部 carrier 的
资格保持独立；完整支持矩阵、512 文件/16 MiB manifest 与上下文预算的区别、
动态/二进制等产品限制、managed verification 既有非零 exit_status 限制、
无 GC/origin/远程执行安装/ACP CRUD 等明确边界见 G11 附件第 6–8 节。

没有本阶段 required 本地工作遗留；没有提交、推送、PR、发布、真实适配器安装、
真实登录/付费或用户全局 harness 修改。外部 qualification 仍是后续独立工作，
不被当前 complete 结论冒充。

后续用户已另行要求提交到 main；PR 基线同步、相同 Rust/Cargo 摘要和新增
registry 检查见 [G11 附件第 9 节](CLI_ONLY_AND_WORKFLOW_SKILLS_G11_ACCEPTANCE.md#9-提交到-main-的基线同步)。
上述未提交/未推送是 G11 验收时点的历史边界，不替代 PR 的实际远端状态。

## 17. 每阶段记录模板

阶段开始时复制并填写本模板；不要提前填入通过次数、命令结果或不存在的日志。

```text
阶段 / 状态：
当前内部检查点、已验证 / 已实现待验证 / 失败 / 未开始：
起始 HEAD、已有工作区差异：
完成 HEAD 或本地变更范围：
无提交时的源码快照/内容摘要及日志关联：
修改目的、保留行为、对应 A 编号：
实际文件/接口/命令变化：
源码/依赖核验和采用的内部实现选择：
定向验证命令、测试名称、退出结果：
完整检查及对应 revision：
CLI help/JSON/human 与协议 journey 证据：
失败、未运行项、阻塞及仍可独立完成的工作：
保留的失败终态、复现命令、下一步与对应 C/T/A：
文档/skill/manifests/schema 同步：
实际删除、迁移、新增量（G05/G11，物理行口径）：
完成判定、限定范围、下一阶段：
```

每阶段按实际需要使用以下逐项证据表，不提前把未执行项填为通过。完整日志可
作为附件；表内保留可复现命令、终态和限定范围，临时日志丢失后不能只凭路径宣称通过。

| C/T/A 要求与本阶段切片 | 真实入口与 owner/身份 | 测试或命令 | 源码快照 | 实际结果 | 后续未覆盖范围 |
| --- | --- | --- | --- | --- | --- |
| 按阶段填写 | 按实际路径填写 | 可复现名称/命令 | revision + 本地内容摘要 | passed/failed/not_run | 指明责任阶段或外部资格 |

## 18. 外部资格与最终交付

这些资格不因 G00–G11 的本地 fixture 通过而自动变成 verified。

| 资格 | 状态 | revision / 环境 / 证据 |
| --- | --- | --- |
| hosted CI | unverified | — |
| 真实 native provider | unverified | — |
| 真实 Codex ACP skill delivery | unverified | — |
| 真实 Claude ACP skill delivery | unverified | — |
| 真实 MCP Skills 上游 | unverified | G09 已执行本地 Rust MCP fixture；未验证真实远端实现，见第 14 节与固定规范边界 |
| 外部 host 的 MCP Skills 互操作 | unverified | 独立于 gateway/native 本地 fixture；须记录具体 host、版本、transport 与规范 revision |
| 远端 Git/HTTPS/SSH authentication | unverified | G07 使用本地 Git repository fixtures；未做凭据或远端 transport 资格验证 |
| macOS 进程/终端行为（本地 fixtures） | local_complete | G02–G11 CLI/PTY/process/protocol、CRUD/lifecycle、native 两条集成链与独立 ACP 本地载体通过，见第 7–16 节和 G11 附件；真实 harness 仍分别 unverified |
| Linux 进程/终端行为 | unverified | — |
| Windows 进程/终端行为 | unverified | — |

最终报告分别写明本地完成、未验证资格、已知限制与是否存在 required work。
若用户另行要求 release qualification，缺失的资格是后续必做工作；不能把它们
删掉或改为通过以结束目标。实际发布、合并与部署只在相应用户授权范围内执行。
