# PR #981 测试、夹具与 CI 独立审查

审查对象：`8a44c6b1fc8a7d580de82412ab821e46c07c5d76`，基线 `54c822a34d4dbfd75d484ca35e108b497048efef`。只读审查；没有编辑仓库、运行构建/测试、修改 Git 或发布评论。未读取另一位实现审查者的结论。依据现行 spec v0.2、架构图、验收映射及当前调用点判断，未拿历史评审当结论。

## 总体判断

测试体系偏重，但目前没有证据支持大规模删用例或重新设计测试框架。多数复杂度来自真实边界：GoalStore / ExecutionStore 的不同提交窗口、native 与 ACP 的不同执行机制、实际权限/工具/停止证据，以及真实终端模式。`goals/*.rs` 验证数据库领域裁决；`goal_runtime/tests/*` 验证协调器如何驱动真实 SDK/进程与保存证据；TUI reducer 和 PTY 分别验证按键语义与操作系统终端生命周期。这三层不能相互替代。

最适合收敛的是启动模板、纯投影测试的数据库准备、相同入口的重复完整旅程。可确认的维护缺陷还包括一个并发文件读取窗口和 daemon 退出结果检查不完整。它们属于测试可靠性问题，不是已确认的产品缺陷。

本地 receipt 记录 4,757 pass / 24 skip、0 leak、37 个最终 terminal-allowed checks；本次没有重新运行或重新查询 hosted CI。Native 测试实际走 SDK、工具和 SQLite，但模型回复与语义判定是夹具；ACP 测试启动 Cargo 构建的 Rust peer；本轮真实 vendor/provider live gate 仍未通过。当前文档对此区分准确。

## 优先处理的可靠性问题

### T1 / P2：用已有权限观察消除 ACP 私有状态文件的并发解析窗口

- 读端：`/Users/kelsen/.codex/worktrees/d91b/bitrouter/apps/bitrouter/src/goal_runtime/tests/acp_recovery.rs:343`，循环读 `private-session.json` 后立刻反序列化；任何空/半文件都直接 `?` 退出。
- 写端：`/Users/kelsen/.codex/worktrees/d91b/bitrouter/helpers/bitrouter-test-fixtures/src/goal_acp_fixture.rs:275`，`std::fs::write` 原地截断并重写同一个文件。
- 影响：恢复映射已出现但 prompt 处理仍在进行时，两端可并发。静态路径允许偶发 EOF/JSON 错误；本次未重现，不能声称这是历史失败根因。
- 最小修改：删除 `343` 开始的独立文件轮询，先使用紧随其后的已有权限等待（`:354`），然后一次读取并断言 `prompts == 1`。peer 在文件写入完成后才发送该 prompt 的权限请求，现有协议观察已经是所需同步点。
- 保留：原 session/private nonce、原 Thread/Turn、一次 prompt、无 `session/new` fallback、实际权限响应和工具结果。不是把解析错误忽略掉或添加重试。
- 验证：五种 ACP 恢复场景及 restored permission 拒绝/同意分支；Linux/macOS 上零重试执行。净删约 8 行，不需新文件协议或通用同步框架。

### T2 / P2：正常退出检查只验证 timeout 外层

- `/Users/kelsen/.codex/worktrees/d91b/bitrouter/apps/bitrouter/tests/goal_tui_pty.rs:921` 的 `ended.is_ok()` 会接受 `Ok(Err(io_error))` 和 `Ok(Ok(nonzero_exit))`。前面的 stop RPC 也只在传输失败时强杀，并未核对 `DaemonResponse::Ok`。
- 同文件 `:1413` 展开了 wait 的错误，但仍未核对退出码；`:1108`–`:1112` 的 editor 场景已经有更完整的检查。
- 影响：主要交互完成以后，daemon 异常退出仍可算该场景通过。现有最终绿灯不能证明正常 shutdown 断言存在。
- 最小修改：正常关闭路径检查 stop reply、wait 两层 Result 和 `ExitStatus::success()`；失败清理另保留 kill/reap，先保留原始场景错误。适合与下面 S1 一起修。
- 验证：四个 Goal PTY 场景，确认成功路径都验证 exit 0，失败路径仍回收进程、保留原始诊断；无需给普通关闭逻辑新增复杂故障框架。

## 可选精简，按收益/风险排序

### S1：只合并文件内重复的 daemon 启停，不统一所有测试世界

- `/Users/kelsen/.codex/worktrees/d91b/bitrouter/apps/bitrouter/tests/goal_tui_pty.rs:660`、`:954`、`:1143`、`:1432` 重复 `bro serve`、HOME/BITROUTER_HOME、stdio、kill_on_drop、socket readiness、deadline；对应清理分别在 `:917`、`:1098`、`:1409`、`:1601`，已发生 T2 所述检查漂移。
- `/Users/kelsen/.codex/worktrees/d91b/bitrouter/apps/bitrouter/src/goal_code/tests.rs:274` 与 `:380` 重复另一套**进程内** control socket / App / GoalService 组装。
- 最小修改：PTY 文件内一个具体的 daemon handle 和 start/stop helper，接受已写好的配置及少量额外 env；配置、场景动作、terminal 和保留目录仍由每个测试显式拥有。goal_code 另外提一个局部 IPC helper。不要把进程内 IPC、外部 daemon、模型服务器、ACP、PTY 合成泛型 Scenario/trait 框架。
- 保留：每个测试独立 TempDir/DB/端口、现有 readiness 条件与期限、joined cleanup、四个不同终端行为场景，以及 client 真 IPC/实际收据。
- 风险：中低，主要是错误路径与生命周期；不要让 helper 隐式把所有测试改为强杀。
- 验证：四个 PTY 场景和两个 client 场景；按项目要求运行变更后的完整检查。静态看这些重复模板合并后净减约 100–170 行，实际以 diff 为准；未测运行时间。

### S2：三个纯投影测试不必每次迁移 SQLite 并 Create/Get

- `/Users/kelsen/.codex/worktrees/d91b/bitrouter/apps/bitrouter/src/goal_code/tests.rs:564`、`:631`、`:939` 均先建 DB、迁移、Create、Get，随后直接在本地 `GoalSnapshot` 上注入未解决输入、预算、unknown execution/resource wait，再调用纯投影函数；结果从未再经 store 验证。
- 重复的是 seed 建立，不是三组行为断言。最小修改是一个本文件的具体 `projection_snapshot(workspace)`（普通结构体值，公共字段见 `crates/bitrouter-orchestrator/src/goal.rs:38`），三个测试各自保留不同的状态注入和所有断言，可变为同步测试。
- 必须保留 `:237` 的真实数据库收据验证，以及 `:726` 的 durable commit / projection 对照矩阵；后者实际验证跨层一致性，不应换成纯快照。
- 收益：去掉三次不相关 DB/migration/setup，估计净减约 45–60 行；更清楚地说明它们只证明 presentation。无可靠耗时估计。
- 风险：低；helper 应只定义最小正常初始快照，不创建配置丰富的 Builder，不把状态注入藏入 helper。
- 验证：完整 `goal_code::tests`，确认三种 presentation 行为和真实 store/IPC 矩阵仍在。

### S3：入口矩阵保留，完整终端旅程只跑代表路径

- `/Users/kelsen/.codex/worktrees/d91b/bitrouter/apps/bitrouter/tests/goal_tui_pty.rs:1119` 外层 `inherit_defaults = false/true`，`:1452` 内层 bare/code，使粘贴、草稿往返、过小窗口、挂起恢复的同一套旅程跑四次。后面双 Goal 隔离与两个退出信号在每个 defaults 值下各跑一次。
- 两入口实际汇入相同 renderer：`apps/bitrouter/src/onboarding.rs:332` 和 `apps/bitrouter/src/main.rs:5848` 都调用 `goal_code::run`。入口/配置解析仍需单独证明，不能只剩单一入口。
- 最小修改：保留四组合的短启动/不发起工作/退出检查；把完整草稿、只读 Workflow、双 Goal 隔离、SIGINT、SIGTERM 各保留在一个明确代表路径。保留 `:1459` 的 32 次 resize 后立即按键回归，不能用 Ctrl-L 唤醒替代。活跃执行、外部 editor、backlog/native 隔离三个其他 PTY 测试不删。
- 收益主要是执行次数和诊断清晰度，不承诺 LOC 减少。可从“四次相同完整无工作编辑旅程”减为一次，加四个入口 smoke；不能据此承诺具体提速。
- 风险：中低，确实缩小配置交叉组合，所以优先级低于 S1/S2；若 defaults 改动将来影响输入流程，应加针对性交叉测试。
- 验证：Linux/macOS 的全部 Goal PTY；保留 tty 模式、bracketed paste、shell 可用性、未提交 draft 无持久输入的断言。

### S4：全局压力限制只考虑剥离明确轻量测试，暂不总体放开

- `/Users/kelsen/.codex/worktrees/d91b/bitrouter/.config/nextest.toml:30` 用整个 `goal_runtime::tests::` 前缀占满全局 slots。`apps/bitrouter/src/goal_runtime/tests/live_input/corpus.rs:546` 是一个明确反例：只造/迁移/提交种子及核对输入收据，没有模型、ACP 子进程或完整 GoalRuntime。
- 可以把该纯种子合同测试移到不受重型前缀约束的局部模块，或给它更窄的资源分类；其他轻量用例须逐个确认。当前仅识别此例，不建议凭路径批量放开。
- 验收说明已经记录压力、deadline 和 output leak 的失败背景；全局限制、Cargo publication 文件锁、leak detector 都不应以“代码看起来重”为由移除。收益未测，列为后续小优化，不是本轮必须工作。

## 应保留的复杂度与测试

- Goal SQLite 域测试与 runtime 测试两层：例如 `apps/bitrouter/tests/goals/verification.rs:57` 验证 handoff 对检查/版本的裁决；`crates/bitrouter-orchestrator/src/goal/verification/tests.rs:5` 真读文件验证 CheckOutput；runtime 再验证实际执行、冻结依赖和集成交付。这不是三个相同 mock success。
- `apps/bitrouter/src/goal_runtime/tests/threads.rs` 的原 key / 丢 Goal link / 明确无接纳 / MCP 改变矩阵，native recovery 的已结束 read 与 pending intent、活 owner、暂停、迟到撤销，ACP recovery 的五场景与 pre-prompt 限制。不能只保留 happy reopen。
- Native 与 ACP 的独立并行/依赖旅程。`native_parallel.rs:213` 实际比较同 clock 的接收区间、独立 workspace 同路径写、join 实际 read 工具与 handoff 版本；ACP peer 真读左右文件后组合输出。不能用任务 Running 数量替代。
- 权限原始请求材料、稀疏更新、未提供选项、重复/旧 decision、pause 后 late allow、显式 deny、未知工具效果；stop receipt 和业务暂停不是同一件事。
- generation / worker 的 Gate、HeldExecutor、HeldModel 不宜盲目统一：generation HeldExecutor 阻塞非流式 execute 并拒绝 streaming；HeldModel 阻塞首次流式调用。这是不同协议边界，不是同名 mock 应合并。
- SDK typed decision / budget quote 的 wrapper 测试与 core admission tests 保留：前者能发现包装器丢 callback/quote，后者验证 Core attempt 顺序、原 route identity 和 fallback。AI Responses 的 captured terminal + `[DONE]` 成功/非法尾部矩阵也保留。
- `/Users/kelsen/.codex/worktrees/d91b/bitrouter/crates/bitrouter-orchestrator/tests/support/fixture_binary.rs:15`–`:87` 的 Cargo artifact 来源、文件锁、按内容不可变发布和字节验证有明确 ENOENT/发布窗口背景。不要替换为猜 target/debug 路径、跳过 Cargo、Python 或全局预编译路径。三个仅 ACP/JSON 依赖的 Cargo Rust peers 保持。
- 原有 live 输入评估明确 ignored/需凭据，并区分模型夹具与 live 判断；无需因代码多删除，更不能把离线绿灯改叫 live qualification。

## 另外的覆盖/可诊断性缺口

1. Linux integration shard 名单仍手工列举：`/Users/kelsen/.codex/worktrees/d91b/bitrouter/.github/workflows/ci.yml:158`–`:192`。本 PR 补了十个遗漏 target，现有列表不等于未来不会漏。建议一个小型只读 CI guard：比较 Cargo metadata 的 bitrouter integration targets 与所有 shard union，报告遗漏/重复/不存在。保留明确分片和 `build_jobs=1`，不引入动态调度平台。未发现当前仍漏目标的证据，不应作为当前功能缺陷。
2. 失败清理注释与 panic 行为不符：`apps/bitrouter/src/goal_runtime/tests/acp_recovery.rs:461` 声称 assertions 失败也执行 joined cleanup，但上述 `assert_eq!` panic 会直接越过该段；`Fixture::finish` 也只收到返回的 Result。保留目录不等于已经 shutdown/join 或取得 pre-shutdown DB。对这些拥有实际进程/阻塞 semaphore 的场景，可将关键断言改为带上下文的 `ensure!`，使它们进入既有清理，不新建通用 async unwind 框架。此项主要改善失败时证据和泄漏诊断，不否认本次成功路径。
3. `native_parallel.rs:193` 的 100ms sleep 是发送侧让步，不是接收 ACK；最终接收区间断言是真的，但极端调度延迟仍可能造成假失败。没有实测证明它是既往失败原因，不推荐加 sleep/retry。若再出现，应优先用已有公开内容观察建立“双方均收到首块”同步点，再释放末块；不要为测试给生产添加专用调度开关。

## 建议顺序

先 T1 的“复用已有协议同步点”和 T2 的退出结果修正；再 S1 局部启动模板与 S2 纯投影快照；之后独立提交 S3 旅程去重，最后测量后决定 S4。CI target inventory guard 可以单独小提交。

可支持的保守规模是 S1/S2 合计约 145–230 行模板/准备代码（未实施，因此仅静态估计），加 T1 约 8 行轮询。没有证据支持“删数千行测试”或预测总体提速。恢复/故障矩阵、现有 deadline、零重试 native-goals shard、leak detector 和真实终端断言应完整保留。
