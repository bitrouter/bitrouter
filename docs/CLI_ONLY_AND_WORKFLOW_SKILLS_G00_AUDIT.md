# CLI-only：G00 能力、依赖与验证基线

状态：**G00 文档审计完成；运行实现与回归验证尚未开始**。日期：2026-10-18。
源码 HEAD：`54c822a34d4dbfd75d484ca35e108b497048efef`。

本附件支撑 [spec](CLI_ONLY_AND_WORKFLOW_SKILLS_SPEC.md) 第 6.4 节与
[G00–G11](CLI_ONLY_AND_WORKFLOW_SKILLS_GOALS.md) 的增量执行。
[实现记录](CLI_ONLY_AND_WORKFLOW_SKILLS_IMPLEMENTATION.md) 维护阶段摘要；当前 A/M/C/T 证据统一见 [G11](CLI_ONLY_AND_WORKFLOW_SKILLS_G11_ACCEPTANCE.md)。
这里的“已有”只表示在该 HEAD 找到源码/测试，不表示本次执行过测试或通过资格认证。
后续源码变动时先复核函数与协议，不能把本附件当作永久不变的 API。

## 1. 审计方法与结论范围

- 起点已有 `docs/README.md` 修改及 spec、goals、implementation 三份未跟踪文档。
  本阶段仅补充这些文档和本附件，不改运行源、测试、配置、manifest 或 lockfile。
- 读取入口分派、host 装配、协议、runtime/service、共享 UI 代码和相关测试；
  用 `rg` 检索所有 Rust/manifest 中的 TUI 引用，用离线 `cargo tree` 核对实际依赖。
- 不启动 daemon、agent、MCP、provider 或认证流程；不运行构建/功能测试。
  第 7 节是待执行的回归入口，不是测试结果。A01–A26 均未验收，尤其 A26 未证明。
- native `Agent::run_native` 已经通过 `agent/native.rs` 适配 Core scheduler。
  CLI-only 应迁移控制面，不能重新建立模型循环或把 native 路径当作独立的旧调度器删除。
- 当前缺少统一连接投影、完整单次控制 CLI、MCP 配置 CRUD、技能安装库和 workflow
  skill 自动选择/激活。它们是后续增量工作，不是当前已有能力。

## 2. 执行路径、身份与所有者

下列每条路径都要保留。连接、执行、历史记录、路由 lease 是不同事实；
`bro agents` 不能通过把这些 ID 合并或按 agent 名称去重来实现。

| 路径 / 当前消费者 | 当前装配和执行所有者 | 必须保留的身份与权限 | 无 UI 落点 / 连接投影 |
| --- | --- | --- | --- |
| daemon native：`task run`、native Code | [main.rs](../apps/bitrouter/src/main.rs) `run_native_task` → [agent_local.rs](../apps/bitrouter/src/agent_local.rs) `ThreadClient` → [host.rs](../apps/bitrouter/src/host.rs) `ThreadService` → [agent/native.rs](../crates/bitrouter-orchestrator/src/agent/native.rs) `Agent::run_native` / `CoreSession::bind`、`restore` | local authenticated caller、server instance、Thread/Turn/request、Core session/run/agent、execution epoch；workspace/readonly 不变 | G04 `threads` + 保留 `task run`；G03 从实际运行所有者投影活跃 worker，冷 Thread 不是连接 |
| embedded managed native：`task managed` | [managed_task.rs](../apps/bitrouter/src/managed_task.rs) `run` 在 CLI 内 build App，打开 `DatabaseNativeStore` / `NativeSession` | store name 与 Core session/harness/core-instance/epoch；释放/恢复及未知效果约束 | G05 保留现有命令；G08 单独迁为 daemon 客户端。迁移前不得声称由 daemon 分配 skills 或必然出现在 daemon 清单 |
| daemon supervised ACP：`run --background`、本地 `code <agent>` | [agent_sessions.rs](../apps/bitrouter/src/agent_sessions.rs) `start_background` / `BackgroundClient` → [supervisor.rs](../apps/bitrouter/src/supervisor.rs) `Supervisor` / run controller | `run_id`、`agent_id`、harness native session、parent run；authorized client/scopes、lease generation、action request；审批默认 Ask | G03 活跃进程/连接投影；G04 `agents show/events/pending/prompt/answer/cancel/mode/config/route`；保留 sessions/stop/remove |
| 独立外部 ACP controller/client：`run <agent>`、`acp prompt`、显式 `acp serve <agent>`，以及即将删除的 direct/piped chat | [acp_cli.rs](../apps/bitrouter/src/acp_cli.rs) `SessionHost::prepare/open`、`launch_controlled`；[acp/controller.rs](../crates/bitrouter-orchestrator/src/acp/controller.rs) `Controller::run`；[acp/client.rs](../crates/bitrouter-orchestrator/src/acp/client.rs) `AcpClient` | controller scope、native session、ACP request/permission；外部 harness 掌握 session 能力。headless 默认 DenyAll，显式 flags 可改变 | 保留一次 prompt 和 protocol-pure stdio。G03 对 daemon 未直接观察的连接报告范围限制；路由 lease 不能作活连接证据 |
| native ACP ingress：无 agent 的 `acp serve` | [native_acp.rs](../apps/bitrouter/src/native_acp.rs) `bridge` → daemon socket → [acp/native/mod.rs](../crates/bitrouter-orchestrator/src/acp/native/mod.rs) `NativeAcpServer` → 同一个 ThreadService | trusted caller、server instance、connection principal、delivery owner、attachment、Thread/Turn/approval；clientInfo 不赋权 | 保留 v1/v2 bridge、load/resume/close/cancel 与重连。G03 从真实连接/attachment 所有者取投影；`session/list` 冷目录不能代替 |
| 原生 harness launcher：`launch`、`claude`、`claude-code`、`codex`，旧 spawn alias | [spawn.rs](../apps/bitrouter/src/spawn.rs) `prepare` / `exec_inherited`、`launch_gateways`；[harness.rs](../apps/bitrouter/src/harness.rs) `launch_overlay`；[config_synthesis.rs](../apps/bitrouter/src/config_synthesis.rs) `render` | 每进程 endpoint/auth/model overrides、launch token、子进程；不修改 harness 全局配置 | 保留 harness 自己的终端界面、check 与退出行为。CLI 子进程存在不等于 daemon 已知 ACP 连接；动态技能交付待 G10 adapter 证明 |
| inference-only HTTP agent / SDK client | [SDK server.rs](../crates/bitrouter-sdk/src/server.rs) 与 `language_model/` pipeline；app host 的 HTTP server | caller/API principal、请求与 metering/routing 身份；没有自动获得的 Thread/agent/context grant | 保留模型路由、统计与 API。没有可验证 agent 连接与上下文通道时，不计作 connected agent；skill delivery 为 unsupported |

[thread.rs](../crates/bitrouter-orchestrator/src/thread.rs) `ThreadTarget` 为
`thread_id + server_instance_id`；[Core protocol](../crates/bitrouter-orchestrator/src/core/protocol.rs)
`OwnershipGrant` 为 `session_id + harness_id + core_instance_id + execution_epoch`。
supervisor 的 `ControlFence` 是 `run_id + client_id + lease_generation + action_request_id`。
三种命名空间都不能互换。用户选择的名字只是显示/配置键，不是执行授权。

MCP 的非 UI 消费还包括 [gateways.rs](../apps/bitrouter/src/gateways.rs)
`gateway_servers/to_acp` 的 launcher/ACP 注入，SDK server 的 direct/aggregate
endpoint，及 [rmcp_executor.rs](../crates/bitrouter-sdk/src/mcp/rmcp_executor.rs)
的 `RELAYED_EXTENSION_METHODS`：`skills/list`、`skills/get`、`resources/directory/read`。
这些 upstream 能力保留，不恢复已移除的第一方 origin skills server。
现有连接池按 server name 复用静态上游凭据，不能在本迁移中宣称已支持按 caller
隔离的上游认证。SDK [caching_executor.rs](../crates/bitrouter-sdk/src/mcp/caching_executor.rs)
的 `private_scoped_skills_list_is_not_cached` 与 `public_skills_list_is_cached`
继续作为回归入口，不能因新本地安装库上线而删除 relay 的 cache-scope 保护。

## 3. Spec 6.4 能力矩阵

表中的 T 编号指第 7 节已有测试；“保留”包含所列非 UI 消费者，不能随 UI 文件删掉。

| 能力 | 当前函数、消费者与保留 API | 目标 CLI / 迁移阶段 | 旧测试与替代证明 |
| --- | --- | --- | --- |
| C01 Thread/Turn/Item、上下文连续性与 FIFO | [service/threads.rs](../crates/bitrouter-orchestrator/src/service/threads.rs) `create_thread/read_thread/read_turn/unload_thread`；[admission.rs](../crates/bitrouter-orchestrator/src/service/admission.rs) `start_turn/enqueue_turn`；[observation.rs](../crates/bitrouter-orchestrator/src/service/observation.rs) `read_thread_view/thread_history/observe_thread`。消费者：local native、HTTP agent API、native ACP、native Code；Core 同时服务 managed 执行 | G04 threads list/show/history/events/start/enqueue；保留 task 与 API；G05 只删 native_code 客户端 | T03/T04/T05：跨 Turn 上下文、call/result 配对、FIFO、ACK 丢失仍为同一 acceptance；新 CLI journey 必须读取同一记录 |
| C02 委派、依赖、独立 review 与结果回收 | [core/collaboration.rs](../crates/bitrouter-orchestrator/src/core/collaboration.rs) `Action::parse/apply`、`wait_ready/wait_result`；[core/allocation.rs](../crates/bitrouter-orchestrator/src/core/allocation.rs) allocation 与 dependency/fresh-context 检查；native adapter、NativeSession、managed API 消费 | headless 任务继续触发现有 spawn/delegate/wait；G04/G05 保留检查/事件与控制，不新建 CLI scheduler | T04：local/remote collaboration、并发、上下文隔离、归因；独立 review 仍走 Core allocation 条件 |
| C03 预算、workspace、owner fences | Core protocol `Limits/OwnershipGrant`、`core/session.rs` admission；[service/ownership.rs](../crates/bitrouter-orchestrator/src/service/ownership.rs) `initialize_execution`、[workspace.rs](../crates/bitrouter-orchestrator/src/service/workspace.rs)；supervisor lease/workspace claim；launcher overrides | G01 保留 caller/lease；G04 单次控制包围同一个 lease；G05 保留 task flags/config/readonly，G08 managed 迁移保持 epoch | T04/T05/T06：预算耗尽、失权、workspace 冲突、不同 caller/旧 epoch；不以新身份重提已接受任务 |
| C04 enqueue、steer、审批、取消、暂停/恢复 | [queue.rs](../crates/bitrouter-orchestrator/src/service/queue.rs) `cancel_queued_turn/resume_queue`；[steering.rs](../crates/bitrouter-orchestrator/src/service/steering.rs) `steer`；threads `cancel_turn/answer_thread_input`；supervisor `SessionCommand/SessionAction`；现有 Code/deck 是部分操作的 UI 客户端 | G04 threads 与 agents 两个命令族，详见第 4 节；Ask 实际可答之后才移除 attach | T01/T03/T05/T06：准确 request/option、过期答复、取消暂停、显式恢复、断开不重放；不能用 approve-all/deny-all 代替 Ask 控制 |
| C05 MCP/skills/AGENTS 发现、读取、绑定与 provenance | [harness/mod.rs](../crates/bitrouter-orchestrator/src/harness/mod.rs) `HarnessResources::discover`；[harness/skills](../crates/bitrouter-orchestrator/src/harness/skills/mod.rs) `SkillCatalog::discover`；[instructions.rs](../crates/bitrouter-orchestrator/src/harness/instructions.rs) `InstructionSnapshot::load`；[managed.rs](../crates/bitrouter-orchestrator/src/harness/managed.rs) `NativeResources::discover/signals/material`；host `native_harness_config`。SDK MCP client、aggregate gateway 与 relay 另保留 | G05 保留全部已有资源链；G06 MCP 配置 CRUD；G07 安装与版本；G08 managed 宿主迁移；G09 Core selector/activation；G10 外部 adapter | T07/T08。当前 skill metadata/MaterialRef 为 `required:false`，Core `selected_materials` 只取显式 required_materials/required signals；自动选择和作为指令激活尚缺，不能用通用 evidence JSON 证明 |
| C06 ACP auth/load/resume/fork、mode/config、permissions、cancel/updates | AcpClient typed auth/new/load/resume/list/mode/config/prompt/cancel/subscriptions；Controller 通用双向 dispatch，包括 native lifecycle/unknown extension；supervisor 公开 selection/mode/config；native ingress 支持自己的 v1/v2 子集 | G01 迁出非 UI policy/wire；G04 CLI 控制 + 保留 raw protocol。fork 经 controller 透传；当前没有 AcpClient 的 typed fork helper / supervisor fork action，不伪称已有 CLI | T01/T06/T09/T10：能力协商、原样字段、回调与扩展、重连审批 owner；native ingress 未声明的 fork/mode/config/auth 不补成虚假支持 |
| C07 session route、model 确认、attributable cost | acp_cli route/cost bridge；Controller `route_control/session_cost`；AcpClient `route_list/route_set/route_reset`；supervisor `RouteList/RouteSet/RouteClear` 与 usage projection；[acp_runtime.rs](../apps/bitrouter/src/acp_runtime.rs) 只管理 route lease | G01 迁 cost 常量；G04 agents route 查询/写入保留 run/session/controller scope、确认与 cleanup；全局 `bro route` preview 不能替代 | T09/T11：route 确认失败不显示成功；router/harness/unreported 费用分开；不能把全 daemon spend 分配给单 session |
| C08 verification/artifacts/checkpoint/恢复 | [service/verification.rs](../crates/bitrouter-orchestrator/src/service/verification.rs)、[recovery.rs](../crates/bitrouter-orchestrator/src/service/recovery.rs) `recover_thread/load_thread`、[close.rs](../crates/bitrouter-orchestrator/src/service/close.rs) `close_thread`；Core checkpoint/artifact/recovery；native adapter restore；evolution operator/runtime/checkpoints | G04 结果/历史/事件与显式控制；保留已有恢复 API，补 CLI/transport 时保持安全条件；G05 不删 stores/内容/evolution；G09 保持版本依赖 | T05/T08/T12/T13：verification 精确 invocation、artifact bytes/hash、unknown effect 阻塞、无重放恢复、publication/rollback 的业务断言 |

## 4. 操作可达性与命令映射

`transport 已有` ≠ `目标 CLI 已有`。本节只给 G04 的 parser/adapter 以真实操作为起点，
不新增协议拼写；具体新增 flags、版本协商与 CLI 帮助在 G04 落地并验证。

### 4.1 Native Thread

[agent_local.rs](../apps/bitrouter/src/agent_local.rs) 当前 `CONTRACT_VERSION = 17`；
`ThreadCommand` 带 command ID/server instance。`ThreadClient::create_and_start` 对未知 ACK
使用原 create/start 幂等键查询/重试，不能改成另一条任务。
socket 由 control socket 的 `.agent.sock` 派生；`connect_or_start` 复用 daemon。

| 目标命令 / 保留用途 | 现有 local operation → service | 当前暴露与缺口 |
| --- | --- | --- |
| threads list | `ListThreads` → `list_threads`，after/cutoff/limit | transport 已有；没有 threads 命令族。是冷目录，不代表 connected |
| threads show | `ReadThread/ReadTurn` → `read_thread_view/read_turn` | transport、native Code 和 HTTP read 已有；CLI 缺 |
| threads history / events | `History/Observe` → `thread_history/observe_thread` | transport 与 HTTP history/observe 已有；G04 单次/只追加 follow，保留 cursor/gap/backpressure |
| 保留 task run 创建；threads start | `CreateThread/StartTurn` → `create_thread/start_turn` | task run 已有 create+start；继续现有 Thread 的独立 start CLI 缺 |
| threads enqueue | `EnqueueTurn` → `enqueue_turn` | local transport 已有；HTTP `turn` 按其请求参数处理，不能假定每个 local op 都有独立 HTTP route |
| threads steer | `Steer(expected_turn_id)` → `steer` | transport 与 HTTP 已有；CLI 缺，保留目标 Turn 与 receipt |
| threads pending / answer | `ReadThread/ReadTurn/Observe` 暴露待输入；`Input` → `answer_thread_input` | 没有独立 Pending operation；G04 从已有事实投影，不另存审批。answer 包含 thread/turn/request/approved/幂等键 |
| threads cancel | `CancelTurn/CancelQueuedTurn` → 对应 service 方法 | local 已有两种取消；CLI 必须保留 active/queued 区别，不误取消其他 Turn |
| threads resume | `ResumeQueue` → `resume_queue` | local 与 HTTP resume 已有；仅恢复暂停队列，不自动恢复 unknown effect 或重新执行 |
| unload / close / execution recovery | local `UnloadThread`；service `close_thread/recover_thread/load_thread/read_stored_thread_view` | unload 已有 local transport；close/recover 没有同名 local operation。native ACP 有 close，恢复受 service 条件约束。需要 CLI 暴露时 G04 走版本化 transport 扩展，不能把 unload/resume 偷换为 recover |

[agent_api.rs](../apps/bitrouter/src/agent_api.rs) `/agent/v2` 已有 capabilities、create、
read Thread/Turn、turn、cancel、steer、inputs、resume、history、observe。
不是 local operation 的一一镜像；例如没有该表 ListThreads/unload/close/recover 路由。
保留 HTTP 的鉴权、workspace admission、server-instance fencing 和流式断开语义。

### 4.2 Supervised ACP

`SESSION_PROTOCOL_VERSION = 1`；[supervisor.rs](../apps/bitrouter/src/supervisor.rs)
处理 Authorize、Start、List、PeekAll、Snapshot、Attach、Events、AcquireLease、
Heartbeat、ReleaseLease、Mutate、NativeList、RouteList、Stop、Remove。
Mutate 的 SessionAction 为 Prompt、Cancel、Permission、SelectSession、SetMode、
SetConfig、RouteSet、RouteClear、MarkReviewed。保留 scopes 与 lease/fence 校验。

| 目标命令 / 保留用途 | 当前 transport / client | 当前状态与 G04 要求 |
| --- | --- | --- |
| agents sessions | `List` / `BackgroundClient::summaries` | CLI 已有；包含保留运行记录，并非只列连接。List 投影刻意不带 transcript/options/secrets |
| agents show / events | `Snapshot/Events`，`Attach` 的 retained replay | transport 已有；show/events CLI 缺。查询不必抢控制 lease，不让只读操作获得隐式写权 |
| agents pending / answer | snapshot pending permissions；`Mutate(Permission { permission_id, option_id })` | transport 已有；CLI 缺。答复必须使用该 run 当前请求所提供的 option |
| agents prompt / cancel | `Mutate(Prompt/Cancel)` | transport 已有；CLI 缺。短命客户端必须在 ACK/结果不确定时保留原 action_request_id |
| agents stop / remove | `Stop/Remove`；`BackgroundClient::stop/remove` | CLI 已有；stop 申请 transient lease 且不抢占，remove 只允许合适的已结算状态，保留 native session |
| agents mode / config | `Mutate(SetMode/SetConfig)`；snapshot capability/config | transport 已有，CLI 缺；按协商能力显示 unsupported，不假设所有 harness 支持 |
| agents route | `RouteList` / `Mutate(RouteSet/RouteClear)` | transport 已有，CLI 缺；绑定当前 run 的 native session，不使用全局路由预览替代 |
| session new/load/resume / review | `NativeList/Mutate(SelectSession/MarkReviewed)` | 现有 UI 客户端使用；G04 保留非 UI 调用及 unread/review 语义。最终 flags 随 G04 固定 |
| auth / fork / 原始扩展 | controller 双向 ACP dispatch；client authenticate 等 | 不属于现有 SessionAction 集合。协议透传保留；新增 supervisor 动作需明确能力与版本，不能臆造当前管理命令 |
| agents（裸命令） | 当前 `administration_agents(None)` → `run_standalone`；`Supervisor::list` 返回所有 runs | G03 新增只读实例投影，不能改名调用 sessions 了事 |

native ACP ingress 的 dispatch 在 `acp/native/mod.rs`，wire 协商在 `wire.rs`：
new、load/resume（按版本）、list、prompt、cancel、close 与明确声明的
`_bitrouter/session/resume_queue`。其连接 EOF 只 detach，不代表取消 daemon 已接受执行。
独立 Controller 的生命周期能力由下游声明并透传；不能把 native ingress 子集强加给它。

## 5. 旧入口与配置迁移清单

| 当前入口 / 函数 | 去向 |
| --- | --- |
| 裸 bro → `onboarding::entry`；按 `chat.agent` 转 `start_default_chat` | G02 改成首次 line onboarding，否则 `daemon::start_and_wait` 等既有 readiness 链确保后台启动并退出；保留显式 init/start/serve/restart |
| `code`、隐藏 `tui` → `run_code`；无 agent native_code、显式 agent dashboard/chat、socket/remote operations inspector | G05 删除 UI 入口；native 控制由 G04 threads 承接；ACP 管理由 agents 承接；已有 operations/status/requests/route 等结构化管理继续可用 |
| 隐藏 `chat` 的 TTY 与非 TTY 分支 → `acp_cli::chat/chat_piped` / `chat_plain` | 两种对话循环都删除，不保留行式 REPL；一次 `run`/`acp prompt` 保留 |
| 裸 agents → `agent_sessions::run_standalone`；`agents attach` → `run_attached` | G03 裸 agents 改快照；G04 先交付观察/控制，G05 删 deck、alternate-screen attach；保留底层 Attach/replay/lease 的非 UI 语义 |
| onboarding/provider auth/harness picker → `prompt::select` | G02 用编号/名称/回车搜索，逐行输入与最小隐藏回显；保留登录、重试、重复配置和跳过 agent 的路径 |
| `run` / `spawn -p` / 隐藏 `acp prompt` / `acp serve` | 一次 prompt、结果 schema、timeout、cancel、stdio 协议继续保留；弃用 alias 如另行退役必须另列迁移，不能伴随 TUI 删除静默丢失 |
| `launch`、快捷 `claude/claude-code/codex`、旧 `spawn --agent` | 保留原生 harness 界面与临时 overrides；不是第一方 BRO 对话 UI |
| MCP check、隐藏 registry search/list/add | check 是已有连通探测；旧 list 是远端 registry 列表，旧 add 只打印 YAML stub。G06 新本地 list/add/edit/remove 不能谎报已写配置，保留/迁移 registry 能力到不冲突的明确入口 |
| skills list/init | 当前只发现/展示并 scaffold。G07 增量实现 show/install/update/uninstall，init 不等于登记安装 |

`chat.model` 仍用于 native/headless 默认模型，`chat.commands` 经
`actions::session::prompt_commands` 用于单次 ACP prompt 展开。G01/G05 按消费者迁移；
仅废弃 `chat.agent` 的裸 bro 启动权与纯 UI 配置，不能删除整个 ChatConfig。
首次配置不能再要求保存默认 ACP harness；非 TTY 的旧 inert-success 测试需改为新契约。

G02/G04/G05 的实际 CLI 变化同步 `docs/CLI.md`、README、`skills/bitrouter/`、受影响
plugin manifests、配置 schema/生成物及旧 specs 的替代说明。当前审计不提前改分发帮助。
`CHANGELOG.md` 等历史证据保留；旧实现 spec 可标注 superseded，不抹去历史。

## 6. TUI 引用的完整迁移落点

此 HEAD 下，除 TUI crate 自身，Rust 中命中 TUI/Crossterm 的消费者按下表覆盖。
路径以 `apps/bitrouter/` 为根；纯注释引用单独标明，后续 G05 应重跑检索。

| 消费者 | 共享部分 / 处理与责任阶段 |
| --- | --- |
| `src/acp_cli.rs` | permission Policy/Decision/Prompt、machine resolve、plain Transcript、cost 测试；G01 迁移；G05 只删 chat 前端 |
| `src/supervisor.rs` | PermissionPolicy 决策及 prompt、router cost provenance 常量；G01 迁移，保留 Ask 默认和 runtime 状态 |
| `src/chat/effects.rs` | `Wire::admit/answer/apply` 调用 ACP permission resolver/cancel，持有 outstanding 请求；G01 迁成非 UI adapter，不能随着 chat 删除 |
| `src/chat/session.rs` | log 记忆/失败报告与 plain 输出、piped chat/resolver 混合；main 仍调用 `remember_session_log`。G01 先迁仍需的诊断；G05 删 chat_plain；无消费者部分可删 |
| `src/actions/session.rs`、`src/actions/commands.rs` | Command/PromptCommand、ALIASES、prompt_commands 与 session ports；G01 保留实际 headless/管理数据与展开；G05 去掉纯 UI metadata |
| `src/agent_sessions.rs` | BackgroundClient 与 deck、CodeEffect/State、Crossterm 混合；G01/G04 保留请求、lease、ACK、start/list/stop/remove；G05 删 UI projection/input |
| `src/actions/code.rs`、`src/actions/code/evolution.rs`、`src/actions/code/evolution/candidate.rs`、`src/actions/code/evolution/manual.rs` | NewAgentRunChoices、selectors/inspectors 与真实 candidate/manual review/submission 混合；G04 迁移业务调用，G05 删 panel/菜单；保留 operator/runtime/checkpoint/evidence |
| `src/prompt.rs`、`src/onboarding.rs`、`src/commands.rs` | Select Item/View、EventStream、auth methods picker；G02 行式实现和共享 auth 选择迁移 |
| `src/native_code.rs`、`src/native_code/presentation.rs` | native client + reducer/view；G04 保留服务调用能力，G05 删除 driver/presentation |
| `src/chat/code.rs`、`src/chat/code_controls.rs` | UI driver/selector；G04 先覆盖业务操作，G05 删除 |
| `src/chat/signals.rs` | TUI lifecycle 为文档引用；实际 signal helpers 被 native_code/chat/code/deck 使用。G05 消费者删完后按调用图清理，保留 CLI 所需 Ctrl-C/子进程清理语义 |
| `src/main.rs` | TUI 命令 alias/dispatch、session log 调用，及 `machine::ALIASES/REDUCER_OWNED` 一致性测试；G01 迁共享解析断言，G05 清理旧 UI guard |
| `tests/acp.rs` | headless policy 类型仍来自 TUI；G01 更换 import 并保留权限/输出测试；piped-chat 专属测试随 G05 退役 |
| `src/output/reports/requests.rs` | 历史注释提到 ratatui，不是运行依赖；不把词频当依赖证明 |

TUI crate 内 G01 重点是 `permission.rs` 的非绘制决策/option 选择、`machine.rs`
的命令解析/展开及 `decide`、`cost.rs` 的元数据契约；不迁移整套 reducer/renderer。
`plain::Transcript` 实际依赖 Journal、render Registry、writer Cache、Ratatui Line/Size；
不能简单挪文件。新 text presenter 直接处理协议事件，保留输出顺序、去重和错误语义，
不把 markdown 布局宽度作为产品兼容承诺。NDJSON/quiet 同样保留。

当前审批差异：headless ACP 默认 DenyAll；supervisor 默认 Ask；显式 native task 的
本地调用按既有入口处理自身工具批准，native ACP host 默认为 Ask，readonly 不开放
写/shell/MCP。G01 不能统一默认。G04 的 Ask 答复链必须测试真实 pending → answer。

## 7. 验证入口与旧测试处置

G04 维护注：publication 链接已更新为迁移后的无 UI 文件位置；本表仍记录 G00
起始时的函数名与能力。当前测试映射和资格状态见 G11 验收附件。

本节只列审计确认存在的测试符号/文件，不宣称运行通过。测试过滤名与路径可用于
`cargo test -p <package> --all-features <name>`；app integration 可加 `--test <root>`。
G01–G05 收敛后仍须执行仓库规定的完整检查，不能只靠这些过滤测试放行。

| 编号 | 已有测试入口（代表性符号） | 保留/替代证据 |
| --- | --- | --- |
| T01 | [app tests/acp.rs](../apps/bitrouter/tests/acp.rs)：`prompt_headless_denies_permission_and_completes`、`prompt_approve_all_selects_the_allow_option`、`prompt_permission_policy_title_head_wins`、`prompt_format_text_renders_the_transcript`、`prompt_format_quiet_prints_only_the_text`、`prompt_ndjson` | G01 保留 option/policy、文本和结构化契约；G05 只退役 `chat_on_a_pipe_is_plain_text` 对话入口断言 |
| T02 | [TUI permission.rs](https://github.com/bitrouter/bitrouter/blob/54c822a34d4dbfd75d484ca35e108b497048efef/crates/bitrouter-tui/src/permission.rs)：`an_unanswered_permission_never_resolves_to_consent`、`allow_prefers_once_over_always_and_never_invents`；[machine.rs](https://github.com/bitrouter/bitrouter/blob/54c822a34d4dbfd75d484ca35e108b497048efef/crates/bitrouter-tui/src/machine.rs)：`the_resolver_reads_the_longest_name_first_then_aliases_then_the_agent`、`a_prompt_command_expands_and_never_outranks_a_bitrouter_name` | G01 将仍需行为与测试迁到新所有者；纯 key/layout/reducer 测试 G05 可删。禁止只删测试以解除 TUI 依赖 |
| T03 | [Core root_queue.rs](../crates/bitrouter-orchestrator/tests/core_execution/root_queue.rs)：`queue_preserves_fifo_ids_and_waits_for_owned_tool_cleanup`、`queue_acceptance_and_activation_each_require_their_own_durable_ack`、`uncertain_tool_effects_cannot_be_bypassed_by_resuming_the_queue`；[service steering.rs](../crates/bitrouter-orchestrator/src/service/tests/steering.rs)：`native_core_steering_preserves_thread_receipts_and_fences_stale_effects` | 保留 Core/service suites，G04 新 CLI 覆盖同一 FIFO/receipt/steering 事实；events follow 不应成为重放来源 |
| T04 | [managed API collaboration.rs](../apps/bitrouter/tests/orchestrator_core/managed_api/collaboration.rs)：`local_and_remote_collaboration_preserve_concurrency_context_and_attribution`；[Core authority.rs](../crates/bitrouter-orchestrator/tests/core_execution/authority.rs)：`authority_revoked_during_ack_fences_model_and_tool_dispatch`；[budget.rs](../crates/bitrouter-orchestrator/tests/core_execution/budget.rs)：`active_budget_fences_approvals_then_cleans_up_and_preserves_failure_over_cancel` | 保留 scheduler/委派与预算；UI 删除不缩减并发、独立上下文、caller/owner/epoch 约束 |
| T05 | [managed_native.rs](../apps/bitrouter/tests/managed_native.rs)：`sqlite_native_core_writes_reads_releases_and_continues`、`native_cancel_pending_approval_fences_the_write`；[service recovery.rs](../crates/bitrouter-orchestrator/src/service/tests/recovery.rs)：`terminal_recovery_preserves_paused_fifo_and_refuses_lost_owner_or_foreign_caller`；[service lifecycle.rs](../crates/bitrouter-orchestrator/src/service/tests/lifecycle.rs)：`approval_is_task_bound_and_one_use`、`configured_verification_records_exit_status_and_controls_outcome` | 保留 store/恢复/验证测试；G04 CLI 必须覆盖过期批准、queue resume 与 execution recovery 区别；G08 managed 迁移重跑 |
| T06 | [code_tui_pty/background.rs](https://github.com/bitrouter/bitrouter/blob/54c822a34d4dbfd75d484ca35e108b497048efef/apps/bitrouter/tests/code_tui_pty/background.rs)：`background_permissions_default_to_ask_and_explicit_modes_win`、`permission_wait_counts_toward_background_turn_timeout`、`background_load_and_resume_use_advertised_native_capabilities`、`foreground_claim_rejects_a_background_subdirectory_of_the_same_worktree`、`failed_run_stop_is_idempotent_and_remove_preserves_native_session` | 这些是业务集成测试，G04 移到非 UI suite；保留 Cargo-built ACP fixture 和临时 daemon/config，G05 不能按目录名全部删除 |
| T07 | [service harness.rs](../crates/bitrouter-orchestrator/src/service/tests/harness.rs)：`mcp_approval_and_discovery_cross_native_durable_barriers`、`denied_mcp_call_does_not_reach_upstream`、`read_only_threads_do_not_connect_to_effectful_mcp_servers`、`changed_mcp_catalog_rejects_old_dispatch_without_retry` | 保留 MCP discovery/approval/cancel/catalog fences；G06 配置 CRUD 是另外的写盘验收，不以连通探测代替 |
| T08 | [native_harness_resources.rs](../apps/bitrouter/tests/native_harness_resources.rs)：`sqlite_reopen_retains_resource_inventory_and_known_result_without_connections`；[service instructions.rs](../crates/bitrouter-orchestrator/src/service/tests/instructions.rs)：`deeper_instructions_enter_context_through_native_read`、`instruction_symlinks_stay_within_the_host_read_root` | 保留冷读取无 MCP 连接、AGENTS 优先级/来源/路径限制；G07–G10 新增技能完整 bundle/选择/激活/恢复证据 |
| T09 | [acp/controller.rs](../crates/bitrouter-orchestrator/src/acp/controller.rs)：`native_multi_session_lifecycle_is_transparent`、`callbacks_and_unknown_extensions_are_bidirectional`、`route_extensions_are_confirmed_and_cleanup_tracks_harness_success`、`usage_updates_are_decorated_with_attributed_cost_only`；[acp/client.rs](../crates/bitrouter-orchestrator/src/acp/client.rs)：`prompt_boundary_follows_every_prior_session_update` | 保留原始协议透明性、fork 等下游能力、callbacks、更新顺序和费用归因；不同协议路径分别验收 |
| T10 | [app native_acp.rs](../apps/bitrouter/tests/native_acp.rs)：`stdio_disconnect_restores_same_approval_across_versions_and_daemon_restart`；[service native_acp.rs](../crates/bitrouter-orchestrator/src/service/tests/native_acp.rs)：`wire_cancel_pauses_queue_and_close_withdraws_it_durably`、`replacement_approval_owner_rejects_old_wire_answer_in_both_versions`、`reconnect_keeps_queue_paused_until_explicit_extension` | 保留 v1/v2/stdio/daemon restart、delivery ownership、断开不取消。G03 投影查询不得 load 冷 Thread 或接管 attachment |
| T11 | [app daemon.rs](../apps/bitrouter/tests/daemon.rs)：`api_principal_scoped_acp_routes_roundtrip_over_the_control_socket`、`acp_session_spend_roundtrips_over_the_control_socket`；[acp_cli.rs](../apps/bitrouter/src/acp_cli.rs)：`the_cost_marker_is_spelled_the_same_on_both_sides` | G01 迁 metadata 测试，G04 验证实际 session route 与 attributed cost；不只检查全局 preview |
| T12 | [Core recovery.rs](../crates/bitrouter-orchestrator/tests/core_execution/recovery.rs)：`recovery_unknown_writes_and_shells_block_until_harness_supplies_result`、`recovery_requires_artifact_bytes_and_complete_checkpoint_dependencies`；[artifact_storage.rs](../crates/bitrouter-orchestrator/tests/core_execution/artifact_storage.rs)：`artifact_quota_reserves_essential_evidence_through_unknown_effect_recovery` | 保留 unknown effect、hash/bytes/依赖、checkpoint/ACK、不重放副作用；CLI resume 不能绕过 |
| T13 | [code_tui_pty/evolution.rs](https://github.com/bitrouter/bitrouter/blob/54c822a34d4dbfd75d484ca35e108b497048efef/apps/bitrouter/tests/code_tui_pty/evolution.rs)：`code_operator_restore_runs_through_terminal_review_and_local_publication`、`code_checkpoint_history_displays_recorded_revisions_without_changing_the_current_reward`；[publication.rs](../apps/bitrouter/tests/native_evolution_cli/coding/publication.rs) 的 maintained Codex/Claude coding publication/rollback journeys | G04 通过现有 `acp evolution`、`acp checkpoints`、policy 操作保留业务；UI-only review/candidate 操作补非 UI adapter。保留既有需真实 harness 的 qualification 标记，fixture 不能冒充真实资格 |
| T14 | [onboarding.rs](../apps/bitrouter/tests/onboarding.rs)：`init_preserves_existing_settings_unless_force_is_requested`、`changing_harness_preserves_routes_and_chat_commands`、`bare_unconfigured_emits_inert_envelope_and_exits_zero`、`onboarding_launch_and_next_bare_invocation_both_speak_acp`；[daemon.rs](../apps/bitrouter/tests/daemon.rs) readiness/status 测试 | 前两项保留配置保护；后两项旧产品语义由 G02 新 bare/onboarding 进程测试替代，不能只改期望值。新增幂等/并发启动、child 失败、timeout、TTY/非 TTY、密码不回显/取消 |
| T15 | [code_tui_pty.rs](https://github.com/bitrouter/bitrouter/blob/54c822a34d4dbfd75d484ca35e108b497048efef/apps/bitrouter/tests/code_tui_pty.rs)、[native_agent_tui.rs](https://github.com/bitrouter/bitrouter/blob/54c822a34d4dbfd75d484ca35e108b497048efef/apps/bitrouter/tests/native_agent_tui.rs) | G05 可删纯画面/热键/光标断言；FIFO/审批/timeout/信号/accepted turn detach 断言先迁到 G04。保留 [pty_acp_fixture.rs](../apps/bitrouter/examples/pty_acp_fixture.rs) 的可复用协议部分及其他仍需的 PTY 设施 |

Evolution 已有 `EvolutionAction::{Status,Mode,Register,Revise,Restore,Learning,Improve}`，
以及 `EvolutionOperation::Candidate/Checkpoint` 等底层操作；后者并不全都在当前
顶层 parser 暴露。G04 对照 `actions/code/evolution*` 的实际业务调用补缺，
不因已有 operator enum 就宣称所有 review/candidate CLI journey 可达。

T04 的 context routing 补充入口为
[Core context_decisions.rs](../crates/bitrouter-orchestrator/tests/core_execution/context_decisions.rs)：
`real_decision_http_selects_prompt_and_retains_raw_source_and_cost`、
`committed_decision_survives_restart_without_second_provider_call`、
`routing_inspection_binds_workflow_to_current_step_and_serving_model`。
保留决策准入、上下文来源与实际 serving model 的证据，不以全局模型配置代替。

保留的 native launcher/config-synthesis、SDK inference/routing、MCP gateway/relay
测试继续纳入完整回归。G03 的连接清单没有现成等价测试，必须新增 spec A25 场景；
不能把现有 route lease 或 sessions 测试改名当作通过。

## 8. 依赖图实测

工具：`cargo 1.97.0 (c980f4866 2026-06-30)`。下列均使用 `--locked --offline`，
退出码均为 0；缓存足够，没有联网、安装、编译或修改 lockfile。

| 查询 | 当前结果 / 后续处理 |
| --- | --- |
| `cargo tree --locked --offline -i bitrouter-tui -e normal` | 唯一 normal 反向消费者为 app `bitrouter` |
| `cargo tree --locked --offline -i ratatui -e normal` | Ratatui 0.29.0 ← bitrouter-tui / ratatui-markdown 0.3.6 ← bitrouter-tui ← app |
| `cargo tree --locked --offline -i crossterm -e normal` | Crossterm 0.28.1 ← app、bitrouter-tui、Ratatui；G02 先移 prompts，G05 清理最终绘制/按键链 |
| `cargo tree --locked --offline -i agent-client-protocol-conductor -e normal` | conductor 3.0.0 ← orchestrator ← app；保留 ACP 功能 |
| `cargo tree --locked --offline -p bitrouter-orchestrator --no-default-features --features acp -e normal --prefix none`，完整输出过滤同下 | ACP runtime 3.0.0 / schema 1.10.2 / conductor 3.0.0 / trace-viewer 3.3.0；无所列终端/PTY crate |
| `cargo tree --locked --offline -p bitrouter-orchestrator --all-features -e normal --prefix none`，完整输出过滤 `ratatui/crossterm/conductor/agent-client-protocol/termion/portable-pty` | 有 ACP/conductor/trace-viewer；无所列终端/PTY crate。trace-viewer 名字不代表依赖 Ratatui。当前可以在保留 ACP feature 下移除 app/TUI 绘制链 |
| `cargo tree --locked --offline -i portable-pty -e normal,dev` | portable-pty 0.9.0 仅 app dev dependency；保留有消费者的进程/终端测试用途 |
| `cargo tree --locked --offline -i vt100 -e normal,dev` | vt100 0.15.2 仅 app dev dependency；若 G05 仅剩帧重建用途则清理，不能提前删 PTY 业务测试 |

Manifest 补充：orchestrator 默认 feature 为空，app 显式开启 `acp`；orchestrator
自己依赖 `similar`，不能随 TUI 删除。`shell-words`、unicode-width/segmentation
等按最终调用/反向依赖清理，不按“曾在 TUI 使用”批量删除。G05 在最终图重新执行
反向依赖与 app/orchestrator 全 features 查询，当前结果不能替代修改后的证据。

## 9. 后续阶段的明确缺口与 G00 完成判定

| 阶段 | 从基线接手的必做工作 |
| --- | --- |
| G01 | 第 6 节 permission/prompt/plain/cost/wire/client 的非 UI 解耦；迁移 T01/T02/T11，保留三个入口的审批差异 |
| G02 | 第 5 节裸 bro 与 onboarding；复用 daemon readiness，迁移旧 configured 判断与 T14；不得启动默认聊天 |
| G03 | 第 2 节真实连接所有者的只读投影；历史 runs、冷 Threads、lease、仅配置、inference 请求都不能伪装成当前连接；标明不可观测范围 |
| G04 | 第 4 节目标命令到已有操作的适配、所需版本扩展、pending/answer/lease/ACK 与显式恢复；迁移 T06/T13/T15 的业务断言 |
| G05 | 前述证据齐备后删除 UI/入口/crate/依赖，按第 6 节重跑消费者审计，同步分发文档和配置迁移；无业务 API/历史数据丢失 |
| G06–G10 | MCP 源配置编辑；版本化完整技能包；workflow 选择、指令 activation、durable pinning、managed daemon 迁移、外部 adapter delivery。它们均非本阶段实现 |
| G11 | A01–A26 与所有 retained capabilities 的最终无回归；local fixture、真实 provider/harness、hosted CI、各 OS 资格分别报告 |

G00 文档完成条件已覆盖：七条执行路径（含 embedded managed）、各自身份/所有者、
C01–C08 的迁移和验证、local/supervisor/native ACP/HTTP 的已有与缺失操作、全部
外部 Rust TUI 引用类别、旧入口、业务测试保留以及实际离线依赖图均有落点。
未说明的运行功能删除不在本计划内；后续发现新的消费者应补矩阵并先迁移。

这里的完成仅为源码与验证基线审计。没有完成 G01，没有运行源码回归，没有证明
任何新增 CLI 或 workflow skill delivery 已实现，也没有改变 A26 的 not_run 状态。
