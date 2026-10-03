# BRO Thread/Turn 统一实施与验收

实施日期：2026-10-02–2026-10-03。状态：**U1–U5 已完成，本地验收通过**。
实施基线：`aa7cdacaf858a6cc4f1171ff1996ca4a6a579dd2`，PR #945。
契约：[获准实施的 spec](BRO_THREAD_TURN_UNIFICATION_SPEC.md)。本文只记录本次
U1–U5，旧 runtime 的验证留在 [历史实施记录](BRO_AGENT_RUNTIME_IMPLEMENTATION.md)。

## 分阶段结果

| 阶段 | 实现 | 证据入口 |
| --- | --- | --- |
| U1 | `ThreadService` 统一创建、输入接受、权限、队列、worker 和 Thread 提交 gate；Turn 必填 thread_id；移除公开 Task submit、独立根、旧观察协议及转换 | `service.rs`、`service/threads.rs`、既有权限/幂等/工具/控制回归 |
| U2 | 闲置安全卸载、容量压力下 LRU 回收、无执行的冷公共读取和同 owner reload；保留 durable keys 和日志 | `service/unification_tests.rs`、`service/recovery.rs` |
| U3 | canonical 执行事实和同事务的一个 ThreadEvent；完整完成结果不再经第二次事件提交；root format 2，旧根 default 0 并拒绝解码 | `service/observation.rs`、`store.rs`、`agent_store.rs`、migration 000026 |
| U4 | 本地 v14、HTTP `/agent/v2`；CLI create/start；原生 TUI 同 Thread 的多 Turn、排队/steer/resume、审批与重连 | `agent_local.rs`、`agent_api.rs`、`native_code.rs`、process/PTY 测试 |
| U5 | 新行为、skill 与三个 plugin manifests 同步；保留原执行/恢复故障测试并加入新验收 | 下文验证记录 |

没有引入第二个 scheduler、Actor 框架或新 crate。SDK 模型路由与 provider 执行、
Agent 的模型/工具循环、服务的执行权限与提交确认、App 的数据库和传输职责继续分开。
旧测试 fixture 现在通过 create/start 接口创建真实 Thread，不是 production 兼容层。
仅删除独立 Task 观察契约和旧 Task 格式转换测试，执行清理、事务失败、权限竞态、
owner/process/workspace 故障窗口继续在统一路径中运行。

## 关键实现选择

`ThreadRecord` 是唯一提交版本和 gate 的持有者。`TurnRecord` 保存执行、取消、
审批、启动 fence 和终态快照。worker 的注册一直保留到运行函数及队列推进返回，
卸载不能仅凭终态事件已发出判断清理完成。卸载还检查 queue、subscriber、gate
引用、错误/恢复状态及具体所属的 workspace lease；另一 Thread 的 lease 不会
错误地固定本 Thread 的闲置缓存，也不会被本 Thread 卸载删除。

热上限是驻留容量。64 次连续执行覆盖成功和已知安全失败；热点回收后，创建与
Turn 键仍返回原身份。冷 view/Turn/history 只扫描有界公共投影，不装入完整 SDK
上下文、不安装 worker、不提交日志。需要继续会话的 load 则重建上下文；它在
扫描期间不占全局 admission，安装前重新检查当前驻留状态与 root cutoff。

同实例 reload 要求相同的 active owner generation、完整终态 checkpoint、合法
上下文和无队列/活动执行/未知效果。未配置费用上限时，终态的可选缺失 usage
及已明确中断的模型请求可以保留为历史，不阻止该 owner 开始新 Turn。这个判断
不会用于丢失执行的跨 owner continuation；跨实例加载继续展示 recovery_required。

ThreadEvent 与对应事实同事务保存。事务内多个 lifecycle 变化全部应用后，才推进
Turn cursor 一次。volatile assistant/shell deltas 不推进 durable cursor，完整事实
清除对应 live 视图。完整 ModelRequest 与 RunCheckpoint 的内容和频率保持不变。
本次减少的是外围兼容和重复完成提交，不是省略模型输入或执行恢复证据。

CLI 的 create/start 各保留稳定键，丢失响应用原键和原 epoch 重试一次。创建成功
而 start 明确拒绝时报告 Thread ID，并尝试释放其安全热资源。不确定结果保留身份
供检查，不能把它当作没有执行。TUI 草稿只在明确接受后清空；接受结果不确定后，
即使下一次 receipt 查询遇到 overload 或权限变化，也保留原键。队列与 steer
分开；只有显式 Ctrl-Enter 修改当前 Turn，Ctrl-R 使用稳定键恢复队列。

HTTP 取消请求显式携带 `mode: active|queued`，重试不会因为目标状态变化切换
操作。HTTP 与本地 caller 隔离，读取和审批仍检查 stored profile 与当前 grant。
本地 ReadThread/ReadTurn 使用冷查询，observe 和后续执行才按需 load。

migration 000026 只增加格式列，旧根保留 0；新根以 2 原子创建。memory 与 DB
读取/写入都在解码或改变 key/journal 前检查格式。旧格式拒绝报告
`recovery_required` 和 `unsupported_runtime_format`，不 backfill、不改旧 owner、
不清 marker，也不让一次无法验证的启动失败产生 stopped proof。

## 验收覆盖

| Spec 条件 | 验证 |
| --- | --- |
| A01 | 公开执行只有 Thread/Turn；源码检索旧 Task API、独立存储 Event、legacy 转换为零 |
| A02 | ReadOnly-only grant、local 注册不提权、foreign caller 拒绝、HTTP read-only verification 拒绝和 caller 隔离 |
| A03 | 原键创建/输入/control；卸载后重试；真实 Unix socket 丢失 create/start 回复；TUI receipt overload/权限变化后保留草稿与原键 |
| A04 | 实际 PTY 第二轮用同 Thread；捕获路由模型请求包含第二轮输入、第一轮用户输入和第一轮完整回答 |
| A05 | 同服务连续 64 个会话，成功/已知失败交替；驻留不超过 32，原键和持久版本不丢失 |
| A06 | worker/gate/subscriber/queue/approval/recovery 的卸载保护；观察注册/卸载竞态；同工作区 foreign lease 保留；所有热名额被订阅占用时明确 overloaded |
| A07 | 同 owner reload 无日志增长、ID/cursor/context/pause 保留；可选 usage 与明确中断；跨 owner 无自动执行 |
| A08 | canonical model/tool 完成由同事务公共事实投影；完整提交失败不发成功视图，live 不成为完整上下文 |
| A09 | 既有 request/response/tool/verification/terminal 提交失败与观察断开回归保留 |
| A10 | FIFO、steer、指定撤回、审批竞态、验证期间 steer、原 Turn 累计预算回归保留 |
| A11 | owner fencing/转移、不同数据库 workspace 互斥、cold discovery、实际子进程故障窗口回归保留 |
| A12 | 本地版本/epoch 拒绝；HTTP v1 Task 路径 404；不同 epoch 不重提或 fallback |
| A13 | 实际 SQLite migration/reopen；旧 JSON 在解码前拒绝，root/index/owner/journal 不改；memory 格式拒绝与有界查询 |
| A14 | 实际 PTY 观察连接断开/同进程重连保留草稿；排队输入/审批不变；detach 不提交草稿、不取消或回答；重复 detach/reattach 释放订阅 |

## 本地验证记录

完整 workspace 检查全部成功，测试和文档编译使用 Rust 1.97.0。构建使用
`CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0`，
避免本地巨型测试二进制的调试文件占满磁盘。严格 Clippy 使用 CI 同样的 Rust
1.97.0；CI 已说明 Rust 1.99 的 async-trait double_must_use 误报，不添加 lint allow。

| 检查 | 结果 |
| --- | --- |
| 聚焦 runtime/DB/local/process/HTTP/PTY 回归 | 129/129；随后 reviewer 边界、64 会话与扩展 PTY 的聚焦回归 13/13，全部纳入最终 workspace |
| Workspace all-features nextest | **3,688 passed、22 skipped，无 leak 提示**；此前一轮的提示及复查留在下文 |
| Workspace all-features doc tests | **5 passed、1 ignored** |
| Workspace/all-targets/all-features Clippy `-D warnings` | **通过**，Rust 1.97.0 |
| rustdoc all-features `-D warnings` | **通过** |
| fmt / diff / docs 与 manifest 一致性 | **通过**；CLI help、文档链接、skill 和三个 JSON manifests 已核对 |

执行命令：

```sh
cargo nextest run --workspace --all-features --no-fail-fast
cargo test --workspace --all-features --doc
cargo +1.97.0 clippy --workspace --all-targets --all-features -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps
cargo fmt --all -- --check
git diff --check
```

Clippy 使用本次独立构建目录 `/tmp/bro-unification-clippy197`。最终日志为
`/tmp/bro-unification-final-{nextest,clippy}.log` 和
`/tmp/bro-unification-acceptance-{doctest,rustdoc}.log`。
此前一轮对未修改的 `policy_lock::tests::tier_target_cannot_reference_the_reserved_namespace`
标记 `LEAK`，测试本身通过。该函数只做同步文档校验，没有子进程创建；此前两次
完整 workspace 运行没有这个提示。单独复查该测试，保留结果，不删掉该提示。
该轮 run ID：`671a1964-b4c0-44d0-bc2a-2f4f5b5e760c`。该 policy 测试单独
复查 **1/1 passed，无 leak 提示**，run ID：
`f861a74b-134b-4607-9584-26823085c539`；日志为
`/tmp/bro-unification-leak-recheck.log`。
补齐所有热名额均不可卸载的验收后，完整 workspace 最终重跑通过且无 leak 提示，
run ID：`04d835ec-29ea-4fb7-a0cf-2ec288a9ca38`。
构建还报告外部 `proc-macro-error2` future incompat 和 macOS 大型二进制 linker
unwind table 提示；未使用 lint allow 绕过检查。

## 证据边界

process/HTTP/PTY 使用本机实际 `bro` 和临时 SQLite 数据库，路由 provider 是受控
wiremock fixture。它们证明客户端/服务/存储与模型上下文传递路径，不是 credentialed
provider、真实费用、生产运行或跨平台证明。新 diff 的 hosted CI 尚未验证；旧 head
的 Linux/macOS 结果不能算作本次通过。本机 Unix/PTY 不替代 Windows named-pipe、
PowerShell、Windows CI 或 Linux CI。

未实施 core/harness 集成、inbound native ACP、多 agent 调度、丢失 owner 的操作员
调查/解除接口、OS sandbox 或 power-loss 保证。未知 owner/effect/accounting 的
活动恢复仍阻塞；没有启动自动恢复队列。未删除开发数据库、owner 或 workspace
marker，未重启生产 daemon。工作区原有 `actions/route.rs` 和 `commands.rs` 未提交
修改保留并排除在本次提交范围之外。
上述验证在保留这两份既有改动的工作区进行。
