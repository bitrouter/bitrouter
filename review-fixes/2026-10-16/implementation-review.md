# BRO PR #981：实现侧独立简化审阅

审阅日期：2026-10-16。HEAD `8a44c6b1fc8a7d580de82412ab821e46c07c5d76`，base `54c822a34d4dbfd75d484ca35e108b497048efef`。工作区初始状态干净。只读审阅，未改源码、测试、文档、Git 或 PR；本文件是唯一写入。没有运行构建或完整测试，没有读取测试审阅者的结论。

依据：根 AGENTS.md、README、DEVELOPMENT 的 BRO/宿主边界、重构 spec v0.2、implementation map，以及当前源码和调用方。已检查完整差异清单，重点追踪 Goal 协调、共同 admission/runner/commit/recovery、native/ACP adapter、资源/预算/权限/历史读取；这是实现侧有重点的审阅，不声称对 67k 新增行逐行穷尽证明。测试源码仅用于确认相关不变量已有消费者，未把历史通过记录当本轮运行结果。

## 1. 总体判断

当前实现的主要复杂度有真实来源：多个不同事实账本之间不能原子提交、接纳与 Goal 关联之间可中断、取消不等于效果稳定、权限请求可在传输前后变化、worker 替换需要保留原身份和版本。ThreadService 的共同接纳/执行路径确实存在；不是只给旧 supervisor 调度套一个 Thread 名字。

没有发现值得在本 PR 再引入通用状态 backend、通用工作图、调度 registry、跨 store 事务或统一所有 checkpoint 的理由。这些会扩大工程，不是简化。也没有足够证据支持删掉 legacy stop/reconciliation 分支：新派发走 RequestWorkerDispatch，但旧无绑定记录仍有原动作停止、迟到观察及归档消费者。

值得优先解决的是一个已经出现行为差异的 typed 调用分叉。另有三项具体可选简化，其中只保存必要 Goal 身份最安全；其余应小步做，不能以 DRY 为理由删掉时间上不同的复核。

## 2. 优先发现

### F1 — P2 行为缺口：输入 typed 分类复制执行包装，但未接入已有在途取消合同

**位置**：
- `/Users/kelsen/.codex/worktrees/d91b/bitrouter/apps/bitrouter/src/goal_runtime.rs:669`（至 710，输入分类执行与手写费用换算）
- `/Users/kelsen/.codex/worktrees/d91b/bitrouter/apps/bitrouter/src/goal_runtime/model_control.rs:445`（已有 after_typed_decision 核算入口）
- `/Users/kelsen/.codex/worktrees/d91b/bitrouter/apps/bitrouter/src/goal_runtime/model_control.rs:473`（provider_cancelled 持续检查 gate）
- `/Users/kelsen/.codex/worktrees/d91b/bitrouter/crates/bitrouter-sdk/src/routing/preparation.rs:125`（现有受控 typed 调用）

**当前行为**：drive_input 在 begin_typed 之后创建一个新的 CancellationToken，直接 await executor.execute；该 token 没有任何取消者。它随后单独提取 usage、换算微美元、observe_typed。相同 PR 中 routing preparation 已有 before/after_typed_decision、provider_cancelled、取消后继续取得 executor outcome，以及结果大小上限。ModelControl::after_typed_decision 已经包含 drive_input 手写的费用估算逻辑。

**可确认的影响**：输入分类请求在派发后，Goal 要求版本改变、协调 lease 过期/转移、或 elapsed budget 触限，均不会通过这个 token 中止它；只有 executor 自己返回/超时才继续。gate 的相关规则见 model_control.rs:203、211、223。这个差异是静态调用图可确认的，并非猜测会出现 double execution。

**边界**：TypeSafeExecutor 在 `decision_model/typesafe.rs:151` 自带 timeout，因此不能称为“无限挂起”。explicit_input=true 有意允许在 Goal paused/stopped 时继续理解用户输入；pause 本身不是这里应取消的条件。未现场复现预算/lease 在途变化，也不声称后续 Goal commit 一定接受过期结果。typed 分类有独立 accounting 证据，不能说它完全无记录；但没有普通 managed generation 的 Thread/Turn 生命周期关联，若要求所有模型工作都由 ThreadService 接纳，这一点需要明确合同，而不能藏在“所有生成已统一”的描述里。

**建议**：先提取并复用“执行一个已给定 DecisionRequest”的窄包装：复用现有 NativeExecutionControl 回调、取消和大小限制；input 仍自行构造其分类问题，routing 仍自行构造 assessment，不要直接把 input 塞进 from_prompt_controlled 的路由语义。input 费用结算调用已有 after_typed_decision。不要新增执行器 registry 或另一个调度器。Thread/Turn 归属如需补齐，应作为有独立验收的小改动；本条不是要求建设未来 core persistence。

**保留不变量**：原 attempt key、冻结报价、未知 usage 不归零、取消后 outcome ACK、paused Goal 输入可处理、typed 分数不能直接授权。

**验证**：延迟 typed executor 期间分别失去 lease、到达 elapsed budget、改变要求版本，确认取消和单次 outcome；paused Goal 的正常输入仍可完成；保持现有 typed budget/lost-reply/unknown-usage 用例。收益主要是删除第二份约 40 行调用/核算包装，并消除行为分叉；提取后净行数可能不减少，不给虚假的总行数承诺。

### F2 — 立即可做：ModelControl 无需保存整个 GoalSnapshot

**位置**：`/Users/kelsen/.codex/worktrees/d91b/bitrouter/apps/bitrouter/src/goal_runtime/model_control.rs:19`、`:65`。

**当前行为**：每个 generation/worker control 在 base() 中 clone 整个 GoalSnapshot，保留 workflow、inputs、accounting、budget 等集合。逐一搜索本模块及 activity/quote_wait 子模块，生产代码只使用 `self.goal.id` 和 `self.goal.requirements_revision`。实际权限、来源、预算和 lease 都通过 GoalService 重新读取。GoalSnapshot 的序列化上限是 1 MiB，但这不等于 Rust heap 大小，不能用它直接推导精确内存收益。

**建议**：把字段直接改成 `goal_id: String` 与 `requirements_revision: u64`，仍在构造时固定原始 revision；不必新建一个“GoalContext”抽象。测试中通过 control.goal.control_source_revision 取夹具状态的地方，从夹具原始 snapshot 取值即可，不能为这个测试保留生产完整副本。

**保留不变量**：原始版本与实时 Goal 状态严格区分；worker 来源仍由 GoalWork 承载；每次 gate 继续读当前 Goal。

**风险/验证**：低风险、无序列化/迁移/API 变化。复用 model_control 现有 stale scope、独立分支修改、lease transfer、预算与 ACK 场景。收益是每个控制对象不再复制整个 Goal 的嵌套集合，不是数百行代码减少；未做内存 benchmark。

### F3 — 可选、需谨慎：准入等待不应长期维护第二份容量与资格判断

**位置**：
- `/Users/kelsen/.codex/worktrees/d91b/bitrouter/crates/bitrouter-orchestrator/src/service/waiting.rs:176`（至 247）
- `/Users/kelsen/.codex/worktrees/d91b/bitrouter/crates/bitrouter-orchestrator/src/service/admission.rs:350`（至 431）

**当前行为**：admit_turn 判断 source、grant、closing/recovery、queue、context 和 active/workspace capacity；wait_managed_turn 收到 Conflict/Overloaded 后，managed_capacity_wait 再读取状态并手写另一组条件，以猜测是否是可释放竞争。两处规则目前各有合理目的；等待层还必须排除 unknown/foreign OS workspace owner。未证实当前有错误放行，实际执行仍走 admit_turn。

**建议**：将确定的公共计算（managed source 匹配、输入占用与 context capacity）收敛为私有函数；如果要进一步消除整份重复资格逻辑，让准入在内部保留“可等待的竞争原因”，等待方仅消费该原因并重新调用准入。不要给 UI/公开协议新增通用 error taxonomy，也不要把所有 Conflict/Overloaded 都当成重试许可。后者是较大设计调整，应晚于 F1/F2。

**保留不变量**：admission 锁内再次 authorize；先查原始 body/key receipt；授权失效不等待；未知工作区 owner 不等待；deadline 有界；醒来不代表获准，仍重新准入。

**风险/验证**：中等，涉及并发错误分类。已有 generation/waiting.rs 和 worker/waiting.rs 的原 key、body mismatch、capacity release、revocation、deadline、foreign OS lock 场景须保持。至多能收敛当前约 70 行重判定中的共享部分；不能直接承诺删除全部 70 行，因为等待与 admission 的后置条件并不完全相同。

### F4 — 可选低风险：权限材料在一次处理内反复解码，区分必要新鲜度检查与重复解析

**位置**：
- `/Users/kelsen/.codex/worktrees/d91b/bitrouter/apps/bitrouter/src/goal_permission_material.rs:39`（decision_tool）
- 同文件 `:189`、`:266`、`:392`（checked_value、tool_scope、validate_current）
- `/Users/kelsen/.codex/worktrees/d91b/bitrouter/apps/bitrouter/src/goal_code/projection.rs:693`
- `/Users/kelsen/.codex/worktrees/d91b/bitrouter/apps/bitrouter/src/goal_runtime/permissions/native.rs:222`

**当前行为**：decision_tool 先调用 decision_value 并丢弃已验证结果，再调用 effective_tool→tool_scope→checked_value，对同一 bytes 重新 parse/验证；native 分支再 decode 一遍。validate_current 在无法得到有效 scope 时对已检查的 saved/current 再 checked_value。UI permission_text 也重复这组操作。native delivery 中 selected→validate→native_permission_request 又多次读同一 artifact 与同一源关联。

**建议**：先只做纯解析部分：保留面向原 bytes 的校验入口，把后续 scope projection 改为消费同一次已检查 Value；可加一个私有 `tool_scope_from_checked`，无需公开材料框架。不要完全改为只保留 tolerant ACP typed struct：当前代码明确需要原始 JSON 的 malformed optional fields，避免缺失信息被 SDK decoder 丢弃。native delivery 的 artifact/request 可以在一次 delivery 内复用，但该项收益小且可先不做；实时 pending-input、当前 Goal、权限来源及提交后 receipt 查询继续分别读取。

**保留不变量**：hash/permission/run/tool ID 匹配、原请求与 earlier/post-request 声明分离、晚到变更拒绝、缺失材料不自动批准、批准前实时授权、批准后原 key 对账。

**风险/验证**：纯 JSON 去重低风险；跨 await 缓存权限状态风险较高，不建议混入。使用现有 foreign/late/conflicting declaration、scope widening、oversized context、native identity 与 local path policy 用例。只确认可减少重复解析、哈希和序列化，未证明性能瓶颈，且净 LOC 可能持平。

## 3. 应保留的复杂度

- **GoalStore、ExecutionStore、Core checkpoint、supervisor journal 分开**：要求/协调版本、执行承诺、native 可恢复内部状态、外部进程和协议收据不是同一事实；合并不在 v0.2 授权范围内。
- **稳定 key + 跨账本关联恢复**：native.rs:190 后的原 Thread/Turn 查找，以及 worker.rs 的 admission-lock 查询解决“已接纳但绑定没写入”；不能把 Unknown 直接降为未执行或随机新 key。
- **Goal 协调 lease 与执行 owner fence 同时存在**：一个保护安排提交者，一个保护真实执行 writer；不是双重 owner 竞争。
- **三类 coordinator job**：workflow、input、control 分槽让等待模型/容量时输入与权限/停止仍可推进；它与 ThreadService 执行调度的职责不同。不要只因它叫 schedule 就删成单串行 loop。
- **业务候选、已停止、效果已核对、检查、语义交接分别保存**：worker final answer 不能解锁依赖或完成根 Goal，old task evidence 不能满足新版本。
- **报价 held 与 accounting in_flight**：unknown usage 的已结束调用仍须保留报价；只等待其他当前 worker 可释放的 reservation。把 held 推导自 in_flight 会丢失这类未知费用。
- **ACP 原始权限材料、早期声明和晚到声明分开**：这是阻止 scope/arguments 在授权前变更的安全边界；JSON 重复解析可删，证据区别不能删。
- **旧无绑定执行分支**：有实际历史/停止/对账路径和原身份消费者。RequestDispatch 虽没有新生产派发调用，也仍是已持久化 operation 的可读表示；不能只靠“只有测试调用”就删 enum variant。
- **原归档 operation 完整匹配**：goal_archive::decode 比较 execution、decisions 与 artifact receipt，防止孤立 blob 冒充权威历史；UI 只读读取和有限投影有必要。
- **Box::pin 的特定边界**：当前被组合的 SQLite/provider/recovery future 很大，已有栈问题的记录；这不是有证据可直接清掉的“包装层”。本轮没测 future size，不能建议一律移除。
- **host/runtime 分层**：Git workspace、文件材料、数据库 adapter、supervisor process 与 UI projection 留在 app 有实际依赖理由；不应为“领域都属于 BRO”将它们整体搬入 core。现 service::generation/worker 使用已有 admission/commit owner，优于另起一套 service。

## 4. 推荐最小顺序

1. 独立小提交做 F2：仅保存 ModelControl 真正用到的身份与版本，无协议变化。
2. F1 先补齐 typed input 在途 gate/取消与统一核算包装，再决定是否需要完整 Thread/Turn 归属；保留 System One 分类内容，不把路由 assessment 与输入理解语义混合。
3. F4 仅消除同一 bytes 的纯解析重复；不缓存跨 await 的执行/权限新鲜度。
4. F3 从共享纯判定开始；只有复杂度确实下降时，再用内部明确的可等待原因替代事后猜测。不要为这件事扩展公开错误协议。

不建议本次顺手做：删除 input scope 独立判定、合并所有 input 模型调用、重写大枚举、将每个 Option 都改为 typestate、统一存储、构建通用 executor/plugin registry、把整个 Coordinator 搬进 core。输入多阶段是否值得保留可后续用质量/延迟实测决定；仅因调用数多不足以证明可删掉当前防止范围扩大的一层。

## 5. 证据限制

F1 是静态可确认的取消路径缺失，未执行现场复现；F2–F4 是可选维护性简化，不标为现存 bug。没有发现可基于本次检查宣称的 double-owner、重复启动或错误恢复成功。没有运行完整测试或 hosted CI，也未以 implementation 文档中的历史 4,757 项通过声明作本轮验证。粗略收益只限可核对的重复范围，未估算总开发工时或 PR 总体删行百分比。

