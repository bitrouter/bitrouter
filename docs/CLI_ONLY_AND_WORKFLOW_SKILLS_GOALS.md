# CLI-only 与 Workflow Skills：Goal 执行计划

状态：**G00–G11 全部本地完成；外部资格另列**。更新：2026-10-20。

产品与技术契约见 [spec](CLI_ONLY_AND_WORKFLOW_SKILLS_SPEC.md)，实际状态与证据见
[实现记录](CLI_ONLY_AND_WORKFLOW_SKILLS_IMPLEMENTATION.md)。本文是执行入口，不重复
定义另一套 runtime、权限或完成标准。这里的 `/goal` 是开发助手目标，不是新增
BRO runtime 实体。

## 1. 使用方式

每次启动一个阶段，完成并核验后再启动下一阶段。G00–G05 完成 UI 解耦与删除；
G06–G10 完成资源增强；G11 做跨阶段验收。默认顺序为 G00 → G01 → … → G11。
这个顺序服务于可审查的增量，不要求先完成新增技能分配才能去掉 TUI。

下面每条 `/goal` 都是给用户复制的提示词，阅读示例不构成执行授权。阶段编号
是本文标识，不是 Codex 内置的阶段解析器。每条提示词都要求读取本节共同规则。
当前阶段进度见实现记录；G00 审计证据见
[基线附件](CLI_ONLY_AND_WORKFLOW_SKILLS_G00_AUDIT.md)。文档不会自动推进阶段。

阅读入口：先核对实现记录的阶段表，再读当前卡片及其对应 spec 章节，最后使用
第 4 节的提示词。G00–G11 已完成约定本地验收；最终 A/M/C/T 逐项证据、完整检查与整个 spec 的交付范围见 [G11 附件](CLI_ONLY_AND_WORKFLOW_SKILLS_G11_ACCEPTANCE.md)。
真实 provider/adapter、远端互操作、hosted CI 与其他 OS 资格仍分别未验证。本文的状态说明随实现记录同步。

Goal 应有可检查的结果、约束与证据，而不能只写“继续优化”。执行约定参考
[OpenAI Docs：Using Goals in Codex](https://developers.openai.com/cookbook/examples/codex/using_goals_in_codex)。
本文的阶段、默认顺序与交付边界是本项目的设计。

v0.5 新增 [MCP Skills 独立验收](CLI_ONLY_MCP_SKILLS_ACCEPTANCE.md)：A27 为
gateway 协议，A28 为 host 加载。G09 先做 M01–M03，再做 native M04–M10；
G07 本地安装完成与历史 gateway 结果均不自动满足新验收。G08 的宿主迁移范围
不变，G09 纳入显式允许的 MCP 来源，不新增本地 origin MCP server。

执行时从实现记录中第一个未完成且前置证据有效的阶段继续，不从 G00 重新开始。
存在已启动的 goal 时，先核对其目标与当前阶段；文档修订不自动完成、暂停或
替换它。用户要求整理设计时只改文档，不因看到下面的提示词就开始运行代码。

## 2. 每个阶段共同遵守的规则

### 开始

1. 读取根 `AGENTS.md`、spec、当前阶段卡片和实现记录；核对实际 HEAD 与工作区。
   保留已有草案及无关编辑，不 reset/clean，也不假定初始基线仍是当前源码。
2. 检查前一阶段的完成证据是否适用于当前代码。如果已实现，核验并复用；
   如果后续变化使其失效，先修复该阶段依赖，不重复制造另一套实现。
3. 在实现记录中把本阶段改为 `in_progress`，写明起始 revision/工作区差异、
   计划改动范围与对应 A 编号。G00 只做源码和文档审计。
4. 对复杂阶段按卡片中的内部检查点推进。沿用已有实现和已验证结果；检查点
   用于定位剩余工作，不是新 goal，也不缩小用户已经启动的阶段范围。

### 执行范围

- 仅完成当前阶段及它所需的局部修复，不自动进入下一阶段。
- CLI/host 调用现有服务。保留 Thread/Turn/Item、委派、队列、审批、context
  routing、验证、持久化和恢复；不新增竞争性调度器、模型循环或状态数据库。
- 共享逻辑先迁移后删 UI。迁移不算删除收益，业务测试不能因文件名含 TUI 就丢弃。
- 公共命令/flags/default config/wiring 变化时，同步分发 skill、CLI reference
  和实际受影响的 plugin manifests。文档只描述该阶段真正实现的行为。
- 遵守仓库 Rust 约束，不用 allow/unwrap/expect/panic 绕过检查，不新增无消费者
  的抽象或 Python fixture。协议/进程测试用 Cargo-built Rust fixture。
- 测试使用临时配置、技能库和子进程环境；不改用户真实凭据、全局 harness 配置，
  不停止已有 daemon 或清理无关 worktree。真实服务、付费模型与远程发布需另有
  用户授权；默认使用本地 fixture。
- 交付物是本地可审查 diff 和证据，不要求自动 commit/push/PR/merge/deploy。
  若用户另行要求提交/PR，遵守 conventional title 与仓库交付规则。

### 验证与完成

先运行有意义的定向测试，修复失败。源代码阶段在结束前运行仓库要求的：

```sh
cargo nextest run --all-features --no-fail-fast --test-threads 4
cargo clippy --all-features
cargo fmt -- --check
git diff --check
```

没有 nextest 时用 `cargo test --all-features`。若 nextest 不覆盖 doctests，补
`cargo test --all-features --doc`。变更影响 schema/生成物时运行相应 dist-helper
生成与检查。全量检查在阶段收敛后执行，不要求每个局部编辑都重跑。
完整进程测试期间串行安排其他 Cargo 构建/检查，避免资源竞争干扰 readiness
与进程终态验证；可限制构建并发。不得通过改生产超时、放宽权限或跳过失败测试
取得通过。先前失败及修正后的结果都保留在记录中。

G00 和纯文档修订仅运行相关文档检查。测试不通过、命令未运行、缺失工具或
provider 未验证都不是通过证据；记录实际命令、退出结果和证据范围。

完成本阶段需同时满足：卡片完成条件、该阶段对应 A 项、保持项回归检查，以及
实现记录更新。公共 CLI 变化时必须保留真实二进制 help/JSON/human 使用证据。
日志引用关联实际 revision；无提交时记录 HEAD、变更文件和可重现命令。

每条完成证据至少回答：覆盖哪个 C/T/A 要求、通过哪个真实入口、由哪个测试或
命令证明、对应哪份源码，以及还有什么未验证。G00 的 C01–C08/T01–T15 是
追踪索引；旧测试迁移后保留旧名到新测试名的映射。`cargo check`、parser/help
成功或单个 helper 测试，不能替代 daemon/agent 实际收到操作的业务证据。

跨轮继续时更新“已验证 / 已实现待验证 / 失败 / 未开始”和下一步。失败要保留
终态、错误原因与复现命令；不只留下 timeout。全量检查前固定当前源码范围，
后续源码变化使相关证据失效时重验受影响范围，再取得最终阶段 gate。
无提交的工作区应记录被验证文件的内容摘要或等价源码快照；同一 HEAD 下存在
不同本地 diff 时，HEAD 不能单独标识测试对象。纯文档修订不冒领源码 gate，
也不要求把没有源码变化的整个实现重新执行一遍。

每次 goal 结束报告本阶段改变、验证结果、剩余问题与下一阶段编号。阶段完成
不等于整个 spec 完成；只能在本阶段的 required work 都完成后完成对应 goal。
文档中的进度状态不代替宿主的 Goal 生命周期，也不授权擅自暂停用户的 goal。

### 跨阶段交接与验收切片

“前置通过”意味着所依赖的结果仍然有效，不要求重跑此前全部步骤。下表约束
交接时必须留下什么，以及哪些验收仍属于后续阶段；不要求提前编写下阶段代码。

| 交接 | 必须交付的契约与证据 | 留给后续阶段 |
| --- | --- | --- |
| G04 → G05 | C01–C08 对应的无 UI 操作、身份与 T 测试迁移映射；Ask 实际可答 | G05 证明删除后的回归与依赖清理 |
| G07 → G08/G09 | 安装清单实际格式、scope/安装 ID、catalog revision、bundle 路径与 digest、provenance、alias/retained 语义及事务证据 | A11 的运行绑定/恢复和 A12 的 daemon 授权候选仍由 G09 验证；G07 不新增执行绑定服务 |
| G08 → G09 | managed 的 daemon owner、既有 session/store/epoch 映射、协议版本及丢 ACK/恢复结果 | A19 的 workflow 资源准入由 G09 补齐，宿主迁移本身不证明 skill 选择 |
| G09 内部协议 → host | A27/M01–M03 的真实上游能力声明、实际请求和来源隔离/缓存证据 | 不把 gateway 转发当作 A28 的 host 批准或加载；M04–M10 另验 |
| G09 → G10 | native 准入/选择/指令装配位置、本地版本与 MCP held entry/来源/批准边界、实际请求证据和 explain | A15/A20/A28 的外部交付由 G10 逐 adapter 验证 |
| G06–G10 → G11 | 各阶段完整 gates、当前 CLI/skill 文档、已知限制与未验证资格 | onboarding 可选资源接入、跨入口 journey 和 A01–A28/M01–M10 的本地整体验收 |

A 编号是跨阶段的产品要求，卡片负责其中明确的切片。例如 G07 的 A11 只覆盖
alias 移除和版本保留；它通过后 A11 在总表中仍是 partial，直到 G09/G11 取得
运行恢复证据。不能要求 G07 提前实现 G09，也不能把 A11 全部改为通过。

### 遇到问题

常规实现选择、已授权修复和等价迁移自行完成并记录，不为它们反复征求确认。
只有需要改变产品行为/权限/数据保留、引入范围外副作用，或真实外部条件阻塞
必需验证时才报告具体缺口。继续当前阶段仍可独立完成的工作；不得把阻塞改成
放宽验收、静默回退、自动重放任务或执行后续阶段。Goal 状态按当前宿主规则处理。

## 3. 阶段卡片

### G00 — 当前能力与无 UI 调用基线

- 前置：无。允许源码读取、文档更新和已有工具的只读查询；不实现功能或新增测试。
- 产物：补齐实现记录的 capability matrix：能力、当前消费者、保留的服务/API、
  目标 CLI、旧测试/替代验证、执行身份。列全 native、supervised ACP、独立
  controller、native ACP ingress、原生 launcher 和 inference-only 路径。
- 核对 TUI crate 反向依赖、所有旧入口、共享权限/prompt/plain/cost 逻辑，记录
  哪些协议操作已有 transport、哪些仅有 runtime API、哪些尚未实现。
- 核对 dependency graph，包括可能由第三方 ACP feature 引入的终端依赖；
  不能为了移除绘制依赖而删除协议功能。可用 `cargo tree --locked --offline`
  做只读核对；缺缓存如实记录，不为审计改 lockfile。
- 完成：第 6.4 节能力逐项有迁移/验证落点，命令和身份映射可供 G01–G05 使用；
  无未说明的功能删除。这里只完成基线，A26 的最终无回归仍未证明。
- 禁止：改源代码、启动用户 agent、以旧 CLI 帮助作为新命令已实现的证据。

### G01 — 迁出共享非 UI 逻辑

- 前置：G00。目标：headless、supervisor 与共享 actions 不依赖 TUI 的业务类型。
- 范围：权限决策/答复、prompt expansion、普通文本 presenter、cost provenance、
  session/client adapter。配置依赖的 prompt 展开仍归 app；协议与权限数据归
  适当的 orchestrator/ACP 所有者，不把 app 配置导入 SDK/Core。
- 保持旧 UI 可编译运行，迁走相应测试；只做必要命名/调用替换，不改审批默认、
  会话身份、输出格式或资源选择。不要先删 UI 来迫使全部调用者一起重写。
- 验证：ACP permissions、deny/cancel、prompt 参数/别名、NDJSON/text/quiet 和
  attributable cost 的已有行为；普通文本输出不得经过 Ratatui Line/Buffer。
- 完成：所有保留的非 UI 业务引用都已解耦；残余 TUI 引用均有明确 UI 用途。
  对应 A21、A23、A26 的共享部分。

### G02 — 简单 onboarding 与裸 bro 启动

- 前置：G01。范围：prompt、onboarding、裸命令 dispatch 与已有 start readiness。
- 采用标准行输入的编号/名称/回车搜索与确认，保留 provider/auth 选择和重复添加。
  选择/配置 agent 与资源可跳过；不再用 `chat.agent` 判断是否完成首次配置。
- 隐藏回显用最小独立支持，依赖不得引入 Ratatui/Crossterm；不实现另一套按键
  编辑器。此时 app 其他 UI 仍可保留终端依赖，统一删除放在 G05。
- 裸 `bro` ensure-start 同一配置的后台 daemon，检查真实就绪后退出；重复启动
  幂等；已验证的运行实例不依赖当前客户端仍有启动凭据或 YAML。
  首次向导成功后继续启动；非 TTY 缺输入报错，配置可用则正常启动。
- 验证：临时配置的首次/重复/取消、认证失败后重试、重定向、JSON/human、
  密钥回显、并发启动、child 提前退出与 readiness 超时。协议 fixture 不用真实账户。
- 完成：A02–A04、A07、A24；原生 launcher 和显式 `serve/start/init` 仍可用。

### G03 — connected agents 只读清单

- 前置：G01–G02。范围：现有 runtime 状态投影、daemon 查询、agents 默认分派。
- 裸 `bro agents` 返回 spec 第 6.3 节的带 kind 实例快照，覆盖 G00 记录的各类
  daemon 可观测连接；必要的 liveness API 放在真实所有者，不造镜像状态权威。
- 保留 `agents list` 的 catalog/config 语义和 `agents sessions` 的运行记录语义。
  返回 daemon 是否运行、采样来源/范围，未知状态不伪装成零连接。
- 验证：两个同类 session、idle 连接、活跃 native worker、关闭/失败/停止历史、
  仅配置项、旧 lease、冷 Thread、daemon stopped、身份隔离和分页。
- 完成：A25，以及 A01 中裸 agents 的行为；查询不启动 daemon/agent/MCP。

### G04 — 无 UI 的会话、队列与审批控制

- 前置：G01–G03。范围：第 10 节 CLI、既有 local protocol/supervisor/ACP adapter。
- 实现历史/事件、start/enqueue/steer、pending/answer、cancel/stop/resume 以及
  当前支持的 session mode/config/route 操作。复用已有操作、身份、ACK/幂等键、
  caller/epoch 和 lease；需要 transport 扩展时明确版本协商。
- 流式命令只追加文本/NDJSON，不恢复交互聊天。原生与 ACP 身份不得互相解释。
  原有 readonly 与审批默认不变，观察 disconnect 不自动取消 daemon 已接受任务。
- 将 evolution/check/review/publication/rollback 等 UI 触发的业务验收转成非 UI
  调用；优先复用 `pty_acp_fixture.rs` 和现有 Rust fixtures，不删业务断言。
- 完成：A18、A21、A23、A26，以及 G00 capability matrix 的非 UI 操作覆盖。
  Ask 必须实际可回答；不能用 approve-all 或拒绝全部来替代审批实现。

内部检查点按下列顺序收敛；全部完成才结束 G04。已有代码先核对，不重复实现。

| 检查点 | 可审查结果与必需证据 |
| --- | --- |
| G04.1 身份与 transport | native / supervised ACP / raw ACP 操作映射及版本兼容明确；支持、unsupported 与未实现分开。旧版本拒绝新操作时不降级重提；不为 raw ACP 的 fork/auth/扩展另造虚假 typed 能力 |
| G04.2 Native 完整操作 | CLI → ThreadService 验证 Ask 答复、FIFO/steer、active/queued cancel、queue resume、close、load/unload 与 checkpoint recovery；覆盖 readonly、错 Turn、过期请求、观察断开、精确 verification 和未知副作用不重放 |
| G04.3 ACP 完整操作 | CLI → supervisor → Cargo-built peer 验证 pending/answer、prompt/cancel、history/events、review/stop/remove、mode/config/route 和已支持 session lifecycle；覆盖并发审批、能力/确认失败、lease/claim 冲突、ACK 丢失、旧 boot/client/generation 和默认审批差异 |
| G04.4 非 UI 业务迁移 | T06/T13/T15 中会话、candidate、check/review、publication/rollback 的业务断言有明确非 UI 落点。重用现有 owner 与 fixture；发布确认、精确 revision、失败不改变 baseline 等断言继续存在 |
| G04.5 交付核验 | C01–C08 每项列出保留 API/CLI 与测试；真实 help/JSON/human、skill/manifests 与协议版本一致；定向测试和完整 gates 通过，记录 A18/A21/A23/A26 的本阶段切片与剩余范围 |

完成条件是上述操作的业务闭环，不是新增多少命令。两条执行路径的单独成功
不能互相代替；失败 fixture 先查终态和配置，不通过放宽生产权限或延长等待掩盖。

### G05 — 删除 TUI、旧入口与绘制依赖

- 前置：G01–G04 的必需证据齐备。范围：spec 第 11.1 节的剩余 UI 消费者与 crate。
- 删除 code/tui/chat、TUI attach、控制台、composer、picker、writer/render 等。
  裸 agents 已是快照，不删整个 agents 命令族。保留协议连接/attach 的后台语义。
- 删除 workspace 中的 `bitrouter-tui`，清理 Ratatui/Crossterm 及只服务 UI 的
  直接依赖；核对实际依赖图。保留 `similar`、仍使用的 shell parser、PTY 基础设施。
- 删除画面/热键断言；G04 已保留的业务验收不得消失。旧配置可解释迁移；
  headless 仍使用的 `chat.model/commands` 不连同旧 UI 配置一起盲删。
- 完成：A01、A22–A24、A26；更新分发 skill/CLI/旧 spec 的替代关系。
  记录实际删除/迁移/新增行数，行数不作为通过门槛。
- 删除前逐条核对 G04.5 的映射：每个仍承载业务断言的 UI/PTY 测试都有新落点
  或继续保留。删除后用实际 CLI journey 与依赖图复核；UI 不再编译不能成为
  删除其仍有用的执行服务、协议或恢复测试的理由。

### G06 — MCP 配置 CRUD

- 前置：G05。范围：MCP CLI、单条配置编辑与报告；不迁移整个 SDK MCP gateway。
- 实现 list/show/add/edit/remove，保留 check/tools 及 gateway/relay 的既有行为。
  遵循 spec 第 6.1 节，快捷 transport 与 `--from` 互斥，edit 替换单条配置。
- 编辑未插值的源文档，保留其他节点/注释/顺序；不支持的 YAML 结构明确拒绝。
  写入前校验、并发检查并原子替换；普通报告隐藏 headers/env 的秘密值。
- 验证：HTTP/stdio、字段默认、名称冲突、无效候选、并发修改、失败恢复、
  原文引用保留和 saved_only/restart 要求；查询/写入不启动 MCP 进程。
- 完成：A05–A07、A24；实际 CLI 与 doc/skill/schema 产物一致。

### G07 — 版本化技能库与生命周期

- 前置：G06。范围：技能源获取、完整 bundle、安装清单与 CLI 第 7 节。
- 实现本地/Git install、list/show、update、uninstall，scope/目录/上限遵循
  spec。测试 Git 来源使用本地 fixture repository，不要求连接第三方 registry。
- bundle 发布不可变，保存 provenance/ref/commit/digest；检测路径、文件数/大小、
  多 skill 歧义与格式问题。安装不执行脚本，更新失败保留旧 alias。
- 卸载先移除候选 alias。第一版允许保守保留版本并报告 retained 状态；不为
  未来 GC 新建无消费者的 lease/抽象。G09 接入实际运行绑定后补齐持久引用验证。
- 完成：A08/A10 的安装生命周期，A09 的完整 bundle/不执行脚本，A11 的 alias
  移除/版本保留，A12 的 CLI scope/unmanaged 隔离，以及 A03/A24 的 CLI 与文档
  切片。不提前声称 daemon 候选授权、运行绑定或交付完成。

| 检查点 | 可审查结果与必需证据 |
| --- | --- |
| G07.1 本地 bundle | 完整文件/相对路径 digest、格式与配额校验、不可变发布；references/assets 变化影响版本，路径逃逸/链接被拒绝，scripts 不执行 |
| G07.2 Git 版本 | 本地 repository fixture 验证 branch 更新、tag/commit 固定、明确新 ref、来源与 commit；歧义/获取失败不改变旧 alias；不调用来源 hooks/filter/安装脚本 |
| G07.3 scope 与事务 | 实际 CLI 验证项目根/workspace/global、同名隔离、unmanaged 保留、并发 CAS/锁、失败无覆盖；卸载后新进程仍可检查 retained 版本 |
| G07.4 交接与 gates | 记录现有清单/版本格式及读取边界；真实 help/JSON/human、分发文档同步、定向与完整 gates 通过；A11/A12 的后续切片保持 partial |

先证明安装器正确再接入选择；安装成功只声明 installed，不声明 selected/activated。
`init` 保持源文件脚手架语义；不为了安装生命周期改成隐式注册或安装。

### G08 — managed 命令由 daemon 托管

- 前置：G07。目标：`bro task managed` 是 daemon 客户端，使用既有 Core/NativeSession。
- 先核对 CLI session name、caller、数据库/journal、workspace 与 epoch 的关系，
  再增加版本化宿主操作。沿用 session/store 身份，不建第二套运行记录。
- 处理启动、重连、重复提交/ACK 丢失、取消、释放和恢复；原 CLI 默认、结果与
  验证证据兼容或给出明确迁移。CLI 不再自行创建旁路的生成执行服务。
- 保留库消费者自行嵌入 NativeSession 的能力；本阶段迁移的是产品 CLI 宿主。
  不重放已有任务，不自动退役 lost owner，不推断未知副作用可重试。
- 完成：A19、A23，以及相关 A18/A26；不能仅因两边调用同一 helper 就宣称完成。

内部顺序：G08.1 核对既有 session/store/epoch 与启动、释放、恢复契约 →
G08.2 版本化 daemon 操作和 CLI 接入 → G08.3 真实进程中的重复提交、断线、
丢 ACK、取消、重启与未知效果验证。任何阶段都不新建镜像任务库；旧任务不重放。

### G09 — MCP Skills 对齐、workflow 选择与 native 激活

- 前置：G07–G08。范围：先完成 spec 7.4 与附件的 gateway 协议验收，再接入
  本地受管理库/明确获许可的 MCP 来源 → workflow assessment → 有界选择
  → Turn 绑定 → Harness 激活与 explain，覆盖 task run 和 managed native。
- gateway 调用前检查实际连接的上游声明，重连重新判定；第一版可继续不声明
  directoryRead。方法白名单不能替代能力门控，也不为本阶段恢复 OSS origin。
- host 保留 server/URI/完整 entry，元数据准入后按需加载；验证文件原始字节与
  完整 frontmatter，约束原 server/manifest；MCP 缓存不进入本地安装/发现目录。
- 复用现有决策链、caller/workspace 授权、审批和 durable store；显式约束优先。
  安装 ID 与 MCP 来源身份分别保留，不起第二个选择 agent、任务库或权限服务。
- 在 Turn 最终 context/model 绑定前完成准入和技能加载，工具结果/steering
  仍可追加。持久批准绑定完整 manifest；MCP host 执行须逐 skill 批准，远端
  allowed-tools 不增加 grant，嵌套 skill 不继承激活同意。
- MCP 正文作为带完整来源的任务数据，不能进入 system 指令；上下文预算、
  重建与子任务共享必须保留该来源和批准边界。native pending/answer 与 managed
  单次 CLI 答复共用原 Core receipt；冷查询只重读已提交答复，不恢复执行权。
- 验证本地更新/卸载后的原版本恢复，以及 MCP entry/缓存/来源权限恢复。
  stale 文件、缺原字节或撤销授权明确阻塞，不用新版本补偿原 Turn；第一版
  拒绝 dynamic 并报告必需失败/可选排除。
- 完成：A11–A19 中本阶段原有切片、A24、A27 和 A28 native 切片；实际协议
  fixture 与模型请求证明结果。G10 的外部 host/adapter 资格仍单独保留。

| 检查点 | 可审查结果与必需证据 |
| --- | --- |
| G09.1 gateway 协议 | A27/M01–M03；真实上游未声明/false 时零方法调用，声明支持/重连能力变化、分页/get/read、来源隔离与缓存字段通过；先修已知缺口再接远端 host |
| G09.2 native host 加载 | A28/M04–M09；明确来源许可、直接 URI、按需请求计数、逐文件/完整 frontmatter 校验、原 server/manifest 限制、批准与缓存/动态策略通过 |
| G09.3 workflow 选择 | 复用准入与决策路径，对本地/MCP 候选记录事实、scope、预算、排除和理由；无匹配可为空，必需项缺失不静默跳过 |
| G09.4 持久绑定 | Turn/子任务绑定本地精确版本或完整 MCP held entry、来源和内容批准；原 acceptance/owner/fence 保留，不重放任务 |
| G09.5 实际激活 | A15 与 A28/M10 的 CLI→daemon→本地/上游来源→native 模型请求含确切正文、版本、来源和预算；仅 catalog/material/helper 成功不算通过 |
| G09.6 恢复与 gates | explain 冷查无新连接；更新/卸载、manifest 改变、批准撤销、缓存篡改、取消、重启与正文仍在上下文时的 entry 保留通过；完成完整源码 gates 和文档同步 |

每一步记录输入事实、owner、测试 revision 与输出。G09.1 通过只代表协议切片；
全部必需检查点通过才完成 G09。实际状态见实现记录，未取得证据前不预填结果。

### G10 — 第一批外部 ACP skill 交付

- 前置：G09。范围：Codex/Claude ACP delivery adapter，以及其他路径的能力报告。
- 选择 adapter 实际支持的上下文/材料交付方式，明确它证明的是 native skill
  activation、prompt instruction 交付还是 bundle 访问；不可混成同一成功状态。
- 不改全局 harness 配置，不恢复 OSS origin MCP，不把 inference-only 流量路由
  宣称为交付。不支持的新 skill 能力显式拒绝必需项，同时保留全部既有 ACP 操作。
- fixture 验证准确内容、版本、路径、预算、取消和 unsupported；安装了真实
  adapter 且用户授权后另做受控真实会话验证，不自动安装/登录来完成测试。
- MCP-origin 内容交付必须保留来源、held manifest 与执行批准责任，不能因
  落地为文件/目录或附加到 prompt 就获得本地来源待遇；adapter 不能保证时明确
  unsupported，必需项在执行前拒绝。原 ACP 会话能力继续保留。
- 完成：两个 adapter 的本地契约与独立客户端 fixture 验证、A20/A24/A28 的相应范围。
  live/platform 未验证必须保留在 qualification 列；缺必需 fixture 验证不能完成。

内部顺序：G10.1 逐 adapter 核对交付能力与边界 → G10.2 分别实现并验证 Codex
和 Claude 的精确内容/版本交付 → G10.3 取消/失败/unsupported 与未验证资格。
不能用一个通用 mock 同时证明两个 adapter；原生 launcher 与 inference-only
路径保留能力报告，不因没有动态交付而删除既有运行功能。

### G11 — 集成审计与本地交付

- 前置：G00–G10。范围：跨阶段回归、A01–A28/M01–M10、兼容、依赖、文档和本地交付证据。
- 运行一条真实 CLI 到 daemon 的 fixture journey：onboarding/start → resources
  → native/ACP 任务 → pending/answer → skill explain → terminal result → 重连/恢复。
  分别验证模型路由和 context/resource 交付，不把 helper 单测替代集成。
- 分别跑本地安装来源和获许可的 MCP 来源；重验上游能力拒绝、延迟支持文件
  校验、同名来源、批准撤销与重启后的来源/held entry，不把协议转发算作 host 激活。
- 核对全部现有能力的替代入口、配置与历史保留、同一 caller/epoch/fence、
  事件游标、断线/取消与未知副作用。清理真正无消费者的旧代码，禁止恢复 UI。
- 将 onboarding 的可选 MCP/skills 步骤接入 G06/G07 已有事务，验证完成/跳过/
  失败后重试；不复制配置写入器或安装器，不让可选资源失败丢失 Router 配置。
- 完成：所有 A 项的本地范围有当前证据；运行完整检查与真实依赖图，汇总实际
  diff/行数/风险。文档明确 local complete、hosted CI、real provider/adapter、
  Windows/Linux/macOS 的各自状态，不将缺少的真实资格标成通过。
- 本阶段目标是本地集成交付，不隐含推送/PR/合并/部署或付费 qualification。
  若用户另行要求 release-qualified，必须补齐对应外部证据，不能用本阶段代替。

## 4. 可复制的 goal 提示词

以下提示词均包含第 2 节共同规则。路径相对于当前 BitRouter 仓库。

```text
/goal 完成 docs/CLI_ONLY_AND_WORKFLOW_SKILLS_GOALS.md 的 G00。先读 spec 与实现记录，遵守共同规则，只做源码/依赖/能力与验证基线审计，不改运行代码。完成矩阵和证据记录后结束本 goal，不进入 G01。
```

```text
/goal 完成 docs/CLI_ONLY_AND_WORKFLOW_SKILLS_GOALS.md 的 G01。读取 spec/实现记录并遵守共同规则，迁出共享非 UI 逻辑，保持现有行为与 UI 可编译，取得卡片要求的测试和检查证据后更新记录。不删除 UI，不进入 G02。
```

```text
/goal 完成 docs/CLI_ONLY_AND_WORKFLOW_SKILLS_GOALS.md 的 G02。读取 spec/实现记录并遵守共同规则，实现无 Ratatui/Crossterm 的简单 onboarding 和裸 bro 后台 ensure-start，验证真实就绪、幂等、取消、非 TTY 与输出契约。完成证据后结束，不进入 G03。
```

```text
/goal 完成 docs/CLI_ONLY_AND_WORKFLOW_SKILLS_GOALS.md 的 G03。读取 spec/实现记录并遵守共同规则，使裸 bro agents 返回 daemon 当前连接实例的真实只读快照，覆盖基线列出的接入路径，区分 catalog/历史/不可用。完成证据后结束，不进入 G04。
```

```text
/goal 完成 docs/CLI_ONLY_AND_WORKFLOW_SKILLS_GOALS.md 的 G04。读取 spec/实现记录并遵守共同规则，补齐非 UI 会话/队列/审批控制，迁移业务验收，保留全部既有 orchestration/ACP 能力、身份、权限与恢复保证。完成证据后结束，不进入 G05。
```

```text
/goal 完成 docs/CLI_ONLY_AND_WORKFLOW_SKILLS_GOALS.md 的 G05。读取 spec/实现记录并遵守共同规则，先核验 G01–G04，再删除第一方 TUI、旧入口和绘制依赖；保留 headless/API/launcher、业务测试和历史。完成依赖图与回归证据后结束，不进入资源增强阶段。
```

```text
/goal 完成 docs/CLI_ONLY_AND_WORKFLOW_SKILLS_GOALS.md 的 G06。读取 spec/实现记录并遵守共同规则，实现 MCP 配置 CRUD，证明无损/原子/并发编辑、敏感值处理和 saved/running 边界，保留 MCP gateway/relay/check。完成证据后结束，不进入 G07。
```

```text
/goal 完成 docs/CLI_ONLY_AND_WORKFLOW_SKILLS_GOALS.md 的 G07。读取 spec/实现记录并遵守共同规则，实现本地/Git 版本化 skills 安装、更新、卸载，证明完整 bundle、scope、事务、冲突与版本保留，不执行安装脚本。完成证据后结束，不进入 G08。
```

```text
/goal 完成 docs/CLI_ONLY_AND_WORKFLOW_SKILLS_GOALS.md 的 G08。读取 spec/实现记录并遵守共同规则，将 task managed 产品 CLI 迁为 daemon 客户端，保留 session/store 身份与恢复条件，验证幂等、ACK 丢失、取消、释放且不重放任务。完成证据后结束，不进入 G09。
```

```text
/goal 完成 docs/CLI_ONLY_AND_WORKFLOW_SKILLS_GOALS.md 的 G09。读取 spec、MCP Skills 验收附件与实现记录并遵守共同规则，先完成 A27/M01–M03 的 gateway 能力门控和协议验收，再完成显式允许的 MCP 来源与本地库的 native host 加载、workflow 选择、Turn/版本/entry/预算绑定，验证 M04–M10 的校验、批准、缓存、实际模型上下文和恢复，提供 explain。按 G09.1–G09.6 推进，不恢复 origin MCP，不把安装或转发当作激活。完成本阶段证据后结束，不进入 G10。
```

```text
/goal 完成 docs/CLI_ONLY_AND_WORKFLOW_SKILLS_GOALS.md 的 G10。读取 spec、MCP Skills 验收附件与实现记录并遵守共同规则，实现 Codex/Claude ACP 的 skill delivery adapter 和明确的能力报告，用独立本地 fixture 验证实际交付及 MCP 来源/manifest/权限边界；不能保证时报告 unsupported。不改全局 harness，不自动付费或安装真实 adapter。完成本地证据并保留 live 未验证项后结束，不进入 G11。
```

```text
/goal 完成 docs/CLI_ONLY_AND_WORKFLOW_SKILLS_GOALS.md 的 G11。读取 spec、MCP Skills 验收附件与实现记录并遵守共同规则，完成 A01–A28 与 M01–M10 的本地集成验收、完整检查和实际依赖图审计，分别记录 gateway、native/外部 host、本地安装与未验证的外部互操作资格；不缩减验收，不推送/发布。仅在本地 required work 完成后结束，明确整个 spec 的交付范围。
```
