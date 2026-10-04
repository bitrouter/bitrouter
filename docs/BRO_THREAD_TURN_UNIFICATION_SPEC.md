# BRO Thread Turn 统一重构设计

版本：**v0.1**。日期：**2026-10-02**。状态：**U1–U5 已实现，本地验收通过**。

本文定义 PR #945 中 `service` 的收敛方案：所有原生执行统一为
`Thread → Turn → Item`，删除独立单次 Task 的接受、存储、观察和恢复路径。
一次执行的 CLI 体验保留，由客户端使用相同的 Thread/Turn 接口完成。
重构同时补齐闲置 Thread 的卸载与重新加载，并收敛重复的完成事件。

用户已同意统一方向，并确认旧 Task API 和数据没有必须兼容的外部用户或部署。
用户已批准本文并要求按 U1–U5 实施；阶段结果、验收和未验证边界见
[独立实施记录](BRO_THREAD_TURN_UNIFICATION_IMPLEMENTATION.md)。

原生 UI 和本地协议已由后续 [Conversation UI 契约](BRO_CONVERSATION_UI_SPEC.md)
更新；本文保留 Thread/Turn 统一设计和当时的验证基线。

## 范围和设计依据

源码审查基线为 [PR #945](https://github.com/bitrouter/bitrouter/pull/945) 的
`aa7cdacaf858a6cc4f1171ff1996ca4a6a579dd2`。实施基线已同步到
`aa7cdaca`；下面的源码链接固定到审查基线，不能用本地旧行号代替。

本文是已批准的 [runtime spec](BRO_AGENT_RUNTIME_SPEC.md) 重构契约，
替换旧 Task 兼容、事件重复和客户端交付的相关条款。
现有 [实现记录](BRO_AGENT_RUNTIME_IMPLEMENTATION.md) 是历史验证证据，
不能作为重构后的验收结果。[core 迁移说明](BRO_AGENT_RUNTIME_HANDOFF.md)
中的 core/harness 所有权边界继续有效；本文不实施 core 集成或改变其契约。

本次包含 service、数据库适配、原生 CLI/TUI、本地协议和可选 HTTP 接入。
不包含原生多 agent 调度、自适应模型/上下文路由、inbound ACP、OS sandbox，
以及丢失 owner 或未知副作用的调查和操作员解决流程。

## 当前问题

以下是审查基线的实际行为；后文是目标设计。

| 问题 | 源码证据 | 重构理由 |
| --- | --- | --- |
| 两条独立接受路径 | [submit](https://github.com/bitrouter/bitrouter/blob/aa7cdaca/crates/bitrouter-orchestrator/src/service.rs#L777) 与 [admit_turn](https://github.com/bitrouter/bitrouter/blob/aa7cdaca/crates/bitrouter-orchestrator/src/service/threads.rs#L837) | 都建立 worker 状态，但权限、幂等和存储根不同 |
| 权限规则漂移 | [Thread 检查 profile](https://github.com/bitrouter/bitrouter/blob/aa7cdaca/crates/bitrouter-orchestrator/src/service/threads.rs#L367)，[Task 自行选择 Ask](https://github.com/bitrouter/bitrouter/blob/aa7cdaca/crates/bitrouter-orchestrator/src/service.rs#L951) | 仅授予 ReadOnly 的工作区仍可通过 Task 入口接受 coding 请求 |
| TUI 不继承对话 | [每次发 Submit](https://github.com/bitrouter/bitrouter/blob/aa7cdaca/apps/bitrouter/src/native_code.rs#L228)，[模型历史为空](https://github.com/bitrouter/bitrouter/blob/aa7cdaca/crates/bitrouter-orchestrator/src/service.rs#L1002) | 屏幕历史与连续模型上下文不一致 |
| 热 Thread 没有卸载 | [上限检查](https://github.com/bitrouter/bitrouter/blob/aa7cdaca/crates/bitrouter-orchestrator/src/service/threads.rs#L361)，[prune 只删 Task](https://github.com/bitrouter/bitrouter/blob/aa7cdaca/crates/bitrouter-orchestrator/src/service.rs#L714) | 同一进程累计创建 32 个 Thread 后没有释放名额的路径 |
| 完成结果重复保存 | [Agent 完整响应](https://github.com/bitrouter/bitrouter/blob/aa7cdaca/crates/bitrouter-orchestrator/src/agent.rs#L823)，[展示事件转换](https://github.com/bitrouter/bitrouter/blob/aa7cdaca/crates/bitrouter-orchestrator/src/service.rs#L1735)，[Thread 投影](https://github.com/bitrouter/bitrouter/blob/aa7cdaca/crates/bitrouter-orchestrator/src/service/observation.rs#L301) | 同一结果经 fact 和 Task event 两次提交，各自再保存公共投影 |

Task 和 Thread Turn 已共用 `run_task` 与 `Agent`，不需要重写模型执行器。
目标是删除外围的双重契约，保留执行与安全保证。当前原生入口均属于该 draft PR，
并不在当前 `main` 中；无需为它们增加兼容别名或双协议服务。

## 统一后的对象和职责

每个 Turn 必须属于一个 Thread，每个 Item 必须能归属到原 Thread/Turn。
Thread 是会话与持久化根；Turn 是一次输入对应的执行；Item 是消息、模型尝试、
工具调用等内容的稳定身份。Provider call ID 仍独立于 BRO Item ID。

| 组件 | 目标职责 |
| --- | --- |
| `ThreadService` | 一套接受、授权、FIFO/control、提交、worker、结算和观察规则 |
| `ThreadRecord` | caller、配置、权限、累计已结算上下文、队列、Thread 提交版本 |
| `TurnRecord` | 必填 thread_id、取消、审批、启动屏障、worker 和执行状态 |
| `Agent` | 使用给定上下文推进模型和工具循环，提交事实并等待确认 |
| `ExecutionStore` | Thread 根的事务、接受键、owner/version fencing 和有界读取 |
| App 层 | 数据库实现、资源组装和传输适配，所有调用进入同一服务 |

`TaskService` 改名为 `ThreadService`；其共用执行记录和快照改为 Turn 命名。
`TaskStatus` 改为 `TurnStatus`，保留真实状态含义。Agent 的 `RunStatus` 继续表示
模型循环结果，Turn 状态还要计入验证和结算，两者不能机械合并。
不增加新 crate、第二个 scheduler、通用 Actor 框架或未消费的抽象接口。

依赖方向保持为：App 组装服务和数据库，服务调用 Agent，Agent 调用 SDK 和工具。
SDK 不依赖 orchestrator；所有原生模型请求继续排除 SDK server tool loop。

## 服务接口和接受契约

下表是目标服务能力。复用现有 Thread DTO，删除 Task 专用 DTO；签名在 Rust
实现时按调用方需要收敛，不新增与此表等价的第二套公开方法。

| 操作 | 契约 |
| --- | --- |
| `create_thread` | 保存调用者、工作区、固定配置和权限；不执行模型或工具 |
| `start_turn` | 只接受 idle 且无已接受排队输入的 Thread；容量不足时返回原因 |
| `enqueue_turn` | 持久接受 FIFO 输入；实际启动由同一调度路径完成 |
| `steer` | 定位当前 Turn，封住尚未 dispatch 的调用，在下一模型边界应用 |
| `cancel_turn` / `cancel_queued_turn` | 分别取消已启动执行或撤回未启动输入 |
| `answer_thread_input` | 校验调用者、Thread、Turn、审批 ID、当前 profile 和幂等键 |
| `resume_queue` | 显式恢复暂停队列；加载和重连不能替代该操作 |
| `read_thread_view` / `thread_history` / `observe_thread` | 一套快照、历史和观察契约 |
| `read_turn` | 在指定 Thread 内查询 Turn；冷读取有界，不建立独立执行根 |
| `load_thread` / `recover_thread` | 保留检查与显式安全恢复的区分 |
| `unload_thread` | 释放符合下文条件的热资源，不删除会话或改变持久状态 |

所有对会话的公开操作必须携带 authenticated caller，并检查当前 server instance。
知道 Thread/Turn ID 不等于有权读取、批准或取消。传输层不得合成权限提升，
也不能调用跳过 caller/profile 检查的公开底层方法。

接受键统一保存到数据库，作用域是 caller、Thread 和操作；创建 Thread 的作用域
不含尚不存在的 Thread ID。键、输入事实和相关公共变化在同一事务提交。
相同键与相同输入返回原身份或 receipt；不同输入返回 conflict。重试不会建立
新 Turn、重新批准工具或再次应用 steering。热缓存卸载不删除接受键。

配置和 workspace/profile 检查走同一接受函数。ReadOnly 必须隐藏并拒绝 effectful
工具和验证命令；审批不能突破服务器 grant。重新加载、重新附着和批准时重新
校验 grant。现有实例隔离、输入大小、worker 和上下文预算保持有效。

## 客户端统一

### 一次执行的 CLI

保留 `bro task run` 的命令名和一次执行后退出的体验；`task` 仅是用户操作名称，
不再对应独立领域对象。客户端顺序调用 `create_thread`、`start_turn`，观察对应
Thread 的 Turn，输出最终验证与结果。不得在服务中恢复一个独立 `submit` 实现。

一次 CLI 请求保留一个稳定请求身份，派生分别用于创建和启动的有界接受键。
响应丢失时用原键取回 receipt；不能重新生成键或凭超时推断未执行。创建成功但
启动明确未被接受（例如验证或容量拒绝）时，只留下没有执行的 Thread；客户端
报告 thread_id，释放可卸载热资源。存储错误、断线或响应丢失可能发生在接受后，
必须通过原键和权威记录确认结果；尚不明确时报告不确定，不能按空 Thread 卸载。
此处两次提交的中间态是明确允许的，不引入额外原子复合协议。

NDJSON 保留 accepted、event、terminal 的输出阶段，身份字段统一为 thread_id、
turn_id，事件使用 Thread cursor。自动审批行为只沿用现有本地 headless 客户端的
明确策略，仍调用同一带身份和 grant 校验的审批接口。失败、取消、恢复阻塞退出非零。
退出或连接断开不能删除记录、取消正在运行的 Turn，或把未知效果变成已知。

### 原生 TUI

一次 bare `bro code` 会话持有一个 Thread。后续用户输入进入同一个 Thread，
模型继承已结算上下文和验证证据。普通输入在有活动 Turn 时排队；修改当前执行
必须使用显式 steering 动作，不能把排队文本自动转换为 steering。

用 `--thread-id` 替换 `--task-id`，不保留旧 flag 别名。新 Thread 才使用 model、
workspace、read-only 和 check 参数；重新附着不得覆盖已存配置或升级权限。
同实例断线按 cursor 重连。跨实例先展示恢复状态，不能自动重提输入或继续队列。
离开 TUI 只 detach，保留服务端排队输入、待审批和执行事实。同一客户端进程内
detach/reconnect 保留 composer 草稿；本轮不新增退出或重启后的草稿持久化。

### 本地协议和 HTTP

本地协议从 v13 升到 **v14**，保留既有 socket 路径和 OS 用户授权。
删除 Submit 和 task_id 资源操作，映射到表中的 Thread/Turn 操作。
旧版本返回明确 unsupported version；不自动回退到 Task 或重复提交。

可选 HTTP API 使用 **`/agent/v2`**，撤下 `/agent/v1/tasks` 及其子路径。
保留 loopback、独立 bearer credential、允许工作区、body/admission 限制。
HTTP routes 只调用服务操作，不自行执行、重建模型上下文或维护独立队列。

| HTTP 资源 | 操作 |
| --- | --- |
| `GET /agent/v2/capabilities` | 当前实例、协议和资源上限 |
| `POST /agent/v2/threads` | 创建 Thread |
| `GET /agent/v2/threads/{thread_id}` | 会话快照 |
| `POST /agent/v2/threads/{thread_id}/turns` | 接受 Turn，body 明确选择 start 或 enqueue |
| `GET /agent/v2/threads/{thread_id}/turns/{turn_id}` | 指定 Turn 快照 |
| `POST /agent/v2/threads/{thread_id}/turns/{turn_id}/cancel` | 指定 Turn；body 的 mode 明确选择 active 或 queued，重试保留原操作 |
| `POST /agent/v2/threads/{thread_id}/steer` | body 指定预期活动 Turn |
| `POST /agent/v2/threads/{thread_id}/inputs` | body 指定 Turn 和审批 ID |
| `POST /agent/v2/threads/{thread_id}/resume` | 显式恢复队列 |
| `GET /agent/v2/threads/{thread_id}/history` | 固定 cutoff 的有界历史 |
| `GET /agent/v2/threads/{thread_id}/observe` | SSE 快照、已提交变化和 volatile live 输出 |

`load_thread` 的检查能力由冷快照读取使用；显式 `recover_thread` 本轮只保留 Rust
host 接口，不新增 HTTP 恢复/操作员流程。卸载是服务的资源管理能力，无需新增
CLI flag 或 HTTP endpoint。所有客户端共享权限、接收确认和取消语义。

## 热 Thread 的卸载和重新加载

`hot_threads` 是内存驻留上限，不是进程一生可创建的会话总数。
数据库会话、接受键和历史不随热缓存淘汰而消失。

只允许卸载满足全部条件的 Thread：

- 已提交的状态为 Idle，或无排队输入且最后 Turn 已安全结算的 Paused。
- 没有活动 Turn、待审批、正在执行或清理中的 worker。
- 没有未完成接受/提交操作、storage error、未知副作用或恢复阻塞。
- 没有观察订阅或正在读取该热记录的短期引用。
- 最后 checkpoint 和工作区释放已确认；卸载不负责释放尚未释放的 fence。

在 Thread commit gate 下重新检查条件，移除热 Thread、可回收 Turn 缓存和
ready queue 残留。不能删除执行记录、接受键、owner 信息或 workspace marker。
卸载不写入新的业务状态，不推进 cursor，也不伪造关闭或恢复事件。

卸载、观察者注册、start/enqueue、reload 和热名额预约必须使用同一 admission/state
协调规则，再使用该 Thread 唯一的 commit gate。完成最终检查到移除之间，不能插入
新订阅或输入；reload 不能为仍被操作引用的 Thread 建立第二个 gate。
新建、加载和容量回收遵循同一锁顺序；只有 Thread gate 而无这项协调不满足要求。

一次执行客户端读取 terminal 后 detach，并请求释放该 Thread 的热资源。
即使客户端异常退出，在新建/加载遇到热容量压力时，服务也要自动淘汰最久未使用
的可卸载 Thread。挑选一次有界候选集并在 gate 下复查；无需后台通用缓存服务。
所有记录都被活动执行或订阅占用时，返回 overloaded，不抢占或隐式断开观察者。

冷快照、历史和 receipt 查询按现有 reader/page/count/byte 预算读取，不为了查询
所有旧会话而驻留其完整上下文。需要观察或执行时才占用热名额。

同实例、当前 owner 下刚卸载的 Thread 可从权威记录重新加载。必须验证格式、
cursor、caller/grant、合法上下文、原暂停状态和已确认 release；加载不启动任何
模型、工具或队列。未卸载之前的状态不能用陈旧内存副本代替数据库事实。
重新加载与同 Thread 的其他操作串行，并占用有界 reader 和热容量预约。

跨实例、旧 owner 或恢复阻塞的记录继续走 `load_thread` 检查与显式恢复流程。
不能为了实现缓存 reload 而取消 stopped-owner、effect 或累计预算证明。
Unload 和 reload 不改变 Thread/Turn/Item ID、幂等键、pause 或累计预算。

## 执行事实和公共事件

选择执行事实作为模型输入和恢复的权威来源；保留每次事务的一个持久化
`ThreadEvent`，用于稳定分页与观察。公共投影与事实同事务提交，不能独立写入。
这仍可能物理保存两份结果内容，目标并非强制零复制，而是取消第二套完成事件提交。

| 内容 | 保存和展示方式 |
| --- | --- |
| 完整模型响应 | 一次 `ModelResponse` 事实提交，生成对应的公共变化 |
| 工具/验证结果 | 一次权威结果提交，生成对应的公共变化 |
| 接受、运行、审批、取消、最终结算 | 紧凑的 Turn/control 生命周期事实，生成公共变化 |
| 文本和 shell delta | 有界 volatile live 输出，不推进 durable cursor |
| 安全 checkpoint | 继续保留上下文和累计预算，仍在原安全边界提交 |

删除 `ExecutionRecord::Accepted` 单次根、旧 `ExecutionRecord::Event`/TaskEvent
完成路径、`ThreadChange::TurnEvent` 包装，以及为同一完整结果另行持久化的
AssistantMessage、ToolFinished 展示事件。必要的开始、审批和终态信息转换为
Turn 生命周期事实，不得随着旧 Event 删除而丢失。

展示完整/中断响应、工具结果和最终状态只能发生在对应事务提交成功后。
提交失败不得先更新成功快照；live 已显示的部分只能作为非完整证据。
Item 身份、工具 call/result 配对、历史 cutoff、observer lag 和重同步语义保留。
公共投影有界但不作为模型上下文；重新加载从事实和 checkpoint 重建。

本轮保留完整 ModelRequest 和安全 RunCheckpoint 的现有内容与频率。
压缩、增量 checkpoint、artifact 引用或减少模型请求审计内容都另行评估，
不以缩小文件为由削弱精确输入、预算或恢复证明。

## 删除范围和安全不变量

删除独立 `TaskRequest`/`submit`、Task 级内存幂等表、Task 专用观察和资源协议操作，
以及 `LegacyTaskProjection`、`LegacyState`、Task 转 Thread 转换和 identity alias。
每个 `TurnRecord.thread_id` 必填，Thread owns commit version 和 commit gate；
删除根据有无 thread_id 选择 root、锁和写入方式的分支。

Turn 的 worker、取消、审批和已结算结果缓存仍有用途，应保留实际需要的字段。
活跃执行索引、稳定调用身份和模型/客户端历史的区别也应保留。
不能将业务 Thread 状态与 OS 执行权混为一谈。

以下是不允许削弱的执行条件：

- 单 Thread 和单 canonical workspace 最多一个活动 Turn。
- 请求、完整响应、工具 intent/result 和结算仍有持久化确认屏障。
- 读工具有界并发；修改、shell、验证保持顺序屏障。
- 取消/超时/store failure 封住新启动并 join 已启动 worker，确认清理后才释放资源。
- DB owner fencing 防止旧 writer 写入；OS 锁防止并行执行；marker 保留清理状态。
- 已确认结果可以重建上下文，不再次执行；未知 owner/effect/usage/budget 保持阻塞。
- 恢复保留原身份和累计预算；PID 消失、新 epoch、加载成功都不构成恢复授权。

不承诺改成完全无锁的服务。先消除 Task 分支，明确 admission、state、Thread gate
和 launch fence 的锁顺序。同步 state/launch 锁不跨 I/O；Thread gate 可覆盖有序
提交；全局 admission 的缩短须保留跨 Thread 容量预约与工作区互斥。
不会用未经验证的并发改写换取较少代码行。

## 持久化格式和开发数据

本轮不兼容重构前的 runtime 日志格式，包括旧 Task 根和旧 Thread 展示事件。
引入明确的 runtime root 格式版本，重构后新建记录使用版本 2；普通迁移只增加
所需版本元数据，不能把旧根标记为版本 2，或重写旧 owner/执行事实来表示安全。

初始化和显式读取遇到旧或未知格式时，报告 `recovery_required`，附带
`unsupported_runtime_format` 原因；在执行前停止。格式拒绝读取有界 root/owner
信息即可，不保留旧 Task 上下文重建或转换实现。
格式拒绝不能修改旧记录/epoch、伪造 stopped proof 或清除 marker；无法检查的
旧执行保持阻塞，不能通过普通 owner 停止路径把该拒绝解释为安全结算。

没有外部兼容需求不等于旧 shell 效果可以忽略。活跃 owner 和未释放 workspace
marker 仍阻止新执行。测试使用全新临时 runtime store。现有开发数据库如需清理，
必须另行明确授权并确认旧执行停止；本 spec 不授权删除数据库、marker 或 owner。
不能为了清 runtime 数据而丢弃计量、配置或其他 app 数据。

## 实施顺序

| 阶段 | 实际改动 | 退出条件 |
| --- | --- | --- |
| U1 统一领域与接受 | ThreadService/Turn 命名、单一权限和接受键、删除独立 submit/root 分支、测试 fixture 转换 | 核心执行与控制均走 Thread；权限和幂等回归通过 |
| U2 补齐热生命周期 | 安全卸载、容量压力回收、冷查询与同实例 reload | 超过 32 次已结算会话不会永久饱和；卸载不触发执行 |
| U3 收敛事件与格式 | canonical 事实加公共投影、删除重复完成 Event 和 legacy、格式拒绝 | 历史和恢复一致；结果只经一次事务提交 |
| U4 迁移所有客户端 | CLI、TUI、本地 v14、HTTP v2；旧路径全部移除 | 实际客户端使用统一接口，TUI 第二轮保留上下文 |
| U5 验证与同步 | 完整 workspace 检查、process/PTY/backend/HTTP 证据、文档与 skill/plugin 同步 | 新基线的验收证据独立记录，残余未验证项明确列出 |

各阶段可使用临时内部适配以保持构建，但最终 diff 中不能残留旧公开 API、旧
wire path、legacy 解码/转换或 unused scaffold。U1–U4 不能分别宣称完整可交付。
实现期间不重启生产 daemon，不合并用户当前的 route/model discovery 未提交改动。

## 验收条件

| 编号 | 必须证明的行为 |
| --- | --- |
| A01 | 所有原生执行入口只创建 Thread/Turn；源码无独立 Task admission、root、幂等或 legacy 转换路径 |
| A02 | ReadOnly-only grant 经 CLI、local、HTTP 和 Rust 服务均不能创建 effectful profile，也不能借审批升级权限 |
| A03 | 重复创建、Turn、steering、取消、审批请求返回原身份/receipt；卸载后和响应丢失后不重复执行；start 接受后的断线/存储错误不能被当作未接受 |
| A04 | 实际 TUI 两轮请求使用同一 Thread；第二轮模型输入包含第一轮已结算上下文，失败/中断内容仍遵循合法上下文规则 |
| A05 | 连续至少 64 个一次执行请求在同一服务内结算，无热 Thread 名额永久耗尽；成功和已知失败均覆盖 |
| A06 | 有执行、队列、审批、订阅、未确认清理或恢复阻塞时不卸载；观察注册/输入/reload 与淘汰竞争不丢失引用或建立双 gate；全部候选不可卸载则明确 overloaded |
| A07 | 同实例 unload/reload 保留 ID、cursor、pause、接受键、配置和上下文；跨实例加载不自动执行或恢复队列 |
| A08 | 每个完整模型/工具结果只有一份权威事实和同事务公共投影，没有第二次完成 Event 提交；部分 live 输出不进入完整上下文 |
| A09 | 注入提交失败不会显示已完成或启动依赖工作；工具结果已提交但观察断开时不重跑工具，后续查询可恢复展示 |
| A10 | FIFO、steering、指定取消、审批竞争和验证期间 steering 的原语义保持，包括累计预算和未启动调用结算 |
| A11 | owner 转移、跨数据库 workspace 互斥、启动发现及现有进程故障窗口保留；未知效果不得因去掉 legacy 或卸载而放行 |
| A12 | 新协议客户端拒绝旧版本；旧 HTTP Task routes 不再提供执行；epoch mismatch 不导致重提输入或本地 fallback |
| A13 | SeaORM 与 memory store 的格式、原子键/事实/投影、有界冷读取一致；旧格式明确拒绝且原始记录不被修改 |
| A14 | TUI detach/reconnect 不取消执行、不回答审批、不提交 composer；服务端队列和同一客户端进程内草稿保持；实际 PTY 证据覆盖 |

测试应围绕权限、事务失败、重试、实际上下文、资源释放和故障窗口，而不是固定
类型名称或行数。删除纯旧 API 契约测试，把其中有价值的清理/竞态/执行证明迁移到
统一路径。单元测试通过不替代真实 process、数据库 reopen、HTTP 或 PTY 验证。

提交实现前运行 workspace all-features nextest（缺失时 cargo test）、Clippy、fmt，
并运行 doc tests；已有严格 all-targets/warnings checks 继续保留。
CLI flag、协议和行为变化实现时，同步 `docs/CLI.md`、`docs/DEVELOPMENT.md`、
`skills/bitrouter/` 与 agent-plugin manifests。Skill 文档只描述实际可用的 CLI，
spec 审阅阶段没有提前改写运行说明；本次实现已同步这些文件。

## 已批准并实施的具体选择

统一 Thread/Turn、删除旧 Task 兼容、保留安全保证是已同意的方向。
以下方案已在本地实现与验证。Hosted CI、平台与真实 provider 的边界见实施记录。

| 选择 | 实施选择 | 代价或边界 |
| --- | --- | --- |
| 一次执行入口 | 保留 `bro task run` 名称，用两次幂等调用完成 create/start | 允许创建成功但尚未启动的空 Thread；不新增复合事务协议 |
| Native 重新附着 | `--thread-id` 替换 `--task-id` | 明确 breaking change，不维持旧 flag |
| 协议 | local v14 与 HTTP v2，仅提供新资源 | 无双协议兼容；客户端必须同批迁移 |
| 卸载策略 | 显式安全卸载加容量压力下的最久未使用淘汰 | 观察者和活跃执行可占满容量，此时拒绝新驻留 |
| 公共历史 | 保留与事实同事务的一个持久化 ThreadEvent | 接受结果与公共投影两份内容，取消后续重复完成事件 |
| 旧开发数据 | 不迁移旧 runtime 格式，明确拒绝 | 需要单独处理开发 store；不自动清除执行安全证据 |
| 范围 | Rust runtime 与已存在的 native 客户端一起统一 | core 集成、inbound ACP 和未知效果 operator recovery 继续独立 |

验收不以删除多少行或达到某个文件长度为目标。完成标准是一套接受与执行契约、
实际客户端使用它、热资源可以回收、历史可以重建，而且原有执行保证仍有证据。
