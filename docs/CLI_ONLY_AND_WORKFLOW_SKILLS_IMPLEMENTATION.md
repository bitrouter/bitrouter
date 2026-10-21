# CLI-only 与 Workflow Skills：实现记录

状态：**G00–G11 约定本地范围完成；测试与文档精简已复验**。

[Spec](CLI_ONLY_AND_WORKFLOW_SKILLS_SPEC.md) 定义行为与 A01–A28；
[MCP 附件](CLI_ONLY_MCP_SKILLS_ACCEPTANCE.md) 定义 M01–M10；
[G11 验收](CLI_ONLY_AND_WORKFLOW_SKILLS_G11_ACCEPTANCE.md) 是唯一完整的当前
A/M/C/T 测试映射。本文只保留阶段结果、必要决策与验证来源。

完整的中间状态、失败日志、修正过程和阶段快照保存在
[精简前实现记录](https://github.com/bitrouter/bitrouter/blob/264765f4d6928e8039ebe44ecef4c94c3b5287d4/docs/CLI_ONLY_AND_WORKFLOW_SKILLS_IMPLEMENTATION.md)。
该不可变提交仍是 PR 的祖先；本次压缩不删除 Git 历史或降低验收要求。

## 1. 起点与范围

- 设计源码基线：`54c822a34d4dbfd75d484ca35e108b497048efef`。
- G00 统计：790 个 tracked Rust 文件，468581 物理行，含测试、注释与空行。
- G11 原始 Rust/Cargo 摘要：
  `7b3a3f450dbef05915b6021b957fb220540175aaf9f7bf641b0c9223ebb87069`。
- G11 验收及同步 main 后的提交：`264765f4d6928e8039ebe44ecef4c94c3b5287d4`。
- 旧快照的完整通过记录与本轮精简后的复验分别记录，不把同一 HEAD 或相同
  生产代码当作测试 helper 已验证的证据。远端 CI 以对应提交为准。

## 2. 阶段状态

| 阶段 | 状态 | 已交付范围 |
| --- | --- | --- |
| G00 | complete | 起点消费者、执行身份、依赖与业务测试迁移审计 |
| G01 | complete | 共享 ACP permission/prompt/plain/cost 迁入非 UI 所有者 |
| G02 | complete | 行式 onboarding、后台 readiness、取消与脚本输出 |
| G03 | complete | 当前连接快照，区分 catalog、历史和查询失败 |
| G04 | complete | native/ACP 单次控制、队列、审批、恢复与兼容 |
| G05 | complete | 删除第一方 TUI、旧入口和绘制依赖；迁移业务测试 |
| G06 | complete | MCP 配置 CRUD、原文保护、CAS 与 saved/running 边界 |
| G07 | complete | 本地/Git 完整 bundle、scope、版本 receipt 与事务 |
| G08 | complete | 原 NativeSession/journal 迁入 daemon，无嵌入式回退 |
| G09 | complete | 固定 MCP 扩展、native workflow 选择、批准、加载和恢复 |
| G10 | complete | 独立 Codex/Claude 本地 prompt 交付及 unsupported 边界 |
| G11 | complete | 可选资源 onboarding、两条真实集成链、完整本地验收 |

阶段完成只说明原卡片的本地范围；真实服务、host、平台与发布资格不由此推定。
原执行卡片和共同规则见 [阶段索引](CLI_ONLY_AND_WORKFLOW_SKILLS_GOALS.md)。

## 3. G00 已审计能力矩阵

[G00 审计附件](CLI_ONLY_AND_WORKFLOW_SKILLS_G00_AUDIT.md) 固定记录起点，
包括七类执行路径、C01–C08、T01–T15、旧入口与依赖图。它不是当前缺口清单。
最终替代入口和实际测试分别见 G11 的 C/T 表，不在本文复制第二份矩阵。

## 4. A01–A28 证据映射

唯一完整映射见 [G11 A 项](CLI_ONLY_AND_WORKFLOW_SKILLS_G11_ACCEPTANCE.md#3-a01a28-当前本地证据)。
M01–M10 与 C/T 在同一附件中记录 owner、测试名、结果与资格边界。
本轮精简保留测试函数和业务断言，复验后更新 G11 的精简记录。

## 5. G00 执行记录

只读核对源码、消费者与实际依赖，明确共享逻辑必须先迁再删。
本阶段不构建、不执行 agent、不认证或调用真实 provider。
起点全文及旧到新消费者映射保留在 G00 附件与不可变实现历史。

## 6. G01 执行记录

permission option/policy、命令展开、text/quiet/NDJSON、journal projection
和费用常量归入 orchestrator ACP 所有者，现有 client/supervisor 继续消费。
实际迁移入口包括 `acp/permission.rs`、`commands.rs`、`plain.rs`、`journal.rs`。
不重建会话服务，也不通过删除原业务断言达到解耦。

## 7. G02 执行记录（本地完成）

裸 bro 完成有限问答或 ensure-start；后台就绪后返回 shell。
保留 provider/auth、默认 agent/model 配置、显式 native launch 与 saved-only
报告；并发/重复启动复用同一目标，配置、运行实例和客户端环境分别处理。
真实 PTY 验证隐藏输入、EOF/Ctrl-C、默认确认与终端恢复；headless 不等待输入。

## 8. G03 执行记录（本地完成）

裸 agents 是原 daemon 所有者的只读 liveness 投影，不能用历史 run 或 catalog
伪造当前连接。idle worker、失联 owner、未启动实例与查询失败分别报告。
managed_native_session 的活 driver 在 G08 接入同一投影。

## 9. G04 执行记录（本地完成）

threads 暴露原 ThreadService 的目录、事件、FIFO、steering、pending/answer、
取消、load/close/recover；agents 暴露原 supervisor 的设置、路由和会话控制。
操作继续绑定原 caller、boot、Turn、epoch、lease 和幂等 request key。
旧协议在新操作产生效果前拒绝，不提供隐式 local fallback 或 lost-owner 接管。
普通命令批准不授权 MCP skill 内容；native v19/HTTP 显式协商保护此区别。

## 10. G05 执行记录（本地完成）

删除 `bitrouter-tui`、app chat/native_code/dashboard、code actions 与旧 inspector。
code/tui/chat/agents attach 只返回迁移错误，在配置或 daemon I/O 前停止。
原 permission、background、evolution、FIFO、timeout、history 和 recovery
业务迁移到非 UI suites；只删除画面、光标、composer 和热键断言。
G05 物理行统计为 439345，较 G00 净减 29236；实际依赖图无 Ratatui/Crossterm。
旧测试到新路径的审计保留在历史记录与 G11 的 T01–T15。

## 11. G06 执行记录（本地完成）

MCP list/show/add/edit/remove 共用 config validators、publication lock、CAS
和原子 writer。完整 HTTP headers/stdio env 可保存，默认 receipt 不返回敏感值。
保留原文、注释、顺序和未插值引用；stale/冲突/失败不覆盖已保存来源。
保存不是运行时生效；MCP 启动所有权保持不变，必要时显式 restart。

## 12. G07 执行记录（本地完成）

安装快照包含 references/scripts/assets/空目录，版本摘要覆盖整个 bundle。
Git 读取对象而不 checkout、运行 hooks/filters 或执行 bundle scripts。
project/global/unmanaged 保持独立；alias 与 receipt 原子发布，更新采用 CAS，
卸载只移除 alias，保留在用版本。没有 GC、MCP URI 安装或全局 harness 写入。
嵌套 Git root、显式 workspace 和损坏/不安全索引不回退的行为分别验证。

## 13. G08 执行记录（本地完成）

task managed 由原 daemon 持有 NativeSession/Core 和已有 SQLite journal。
CLI 只提交、观察、取消和回答精确请求，不组装第二个 App、执行循环或数据库。
原 session/input/acceptance/epoch、ACK 不确定性和条件 release 语义保持。
未知工具效果、旧 boot 与失联 owner 不允许自动重放、强制释放或客户端接管。

## 14. G09 执行记录（本地完成）

规范固定为 MCP 2026-07-28 与 ext-skills commit
`c1aa87e4c381c82b99aa37471d0eae4ab50f9985`；正文摘要与义务见 MCP 附件。
gateway 按当前连接声明门控，完整转发 entry/缓存/分页并保留聚合来源。
native host 另行核验 caller/workspace 来源许可、原 URI 和完整 held manifest；
批准前不读取正文，每次网络/缓存使用验证 size/hash 与完整 frontmatter。
Core 复用 DecisionRuntime/WorkUnit 选择并冻结版本、预算和独立 skill consent。
根与支持文件作为带来源的 user 数据进入实际模型输入；allowed-tools 不扩大 grant。
支持文件按需读取，完整 entry 在指令仍留于上下文时保留；新 listing 不替换旧绑定。
冷 explain 只读原数据库，认证恢复重验原字节；缺失/篡改/撤销时 fail closed。
M01–M10 的真实 gateway/host/Core/CLI 证据只维护在 G11 附件。

## 15. G10 执行记录（本地完成）

Codex text 与 Claude 协商 embeddedContext 使用独立 Rust peer/parser 验证。
原 supervisor/journal 保留 receipt/version/session/input、预算、批准与取消事实。
MCP-origin 外部交付明确 unsupported，不能通过 prompt 或本地落盘降为 local。
本地 prompt 接收不证明 native activation、受保护支持文件或 vendor history/token
准入；真实 maintained adapter、provider 和远端互操作仍是独立资格。

## 16. G11 执行记录（本地完成）

可选 MCP/skills onboarding 调用 G06/G07 原事务。Router 先保存；资源失败保留
安全 receipt 和进度，headless 输出单个部分结果、非零退出且停止后续动作。
交互允许显式重试/跳过；取消停止 Finish；TTY --yes 也不能隐式选择 bundle。
resource-only no-op 保留 Router 原文，成功资源不会被后续 Router 保存覆盖。

两条贯穿链分别验证本地安装和获许可 MCP：init/start → 原 daemon/native
任务 → 路由与真实模型输入 → pending/answer → explain → restart/recovery。
MCP 链确认保存不是来源许可，上游撤销不改写历史，新加载仍受当前能力限制。
Codex/Claude 各自保留 onboarding → 安装 → 原 supervisor → 实际 prompt 的旅程。
完整范围、独立外部资格与 A/M/C/T 对应关系见 G11 附件。

### 原 G11 验证

| 检查 | 原始实际结果 |
| --- | --- |
| workspace/all-features nextest，4 test threads | 4368 passed / 22 skipped |
| workspace/all-features/all-targets Clippy | exit 0；两处既有 PTY byte_char_slices 建议 |
| workspace/all-features doctests | 6 passed / 1 ignored |
| fmt、dist-helper check | exit 0 |
| locked/offline metadata 与 all-target normal/build/dev tree | 629 nodes / 7 members，无 TUI 绘制依赖 |
| nextest discovery | 4390 个用例，22 ignored 与源码对应，未运行 ignored |
| CLI 与分发审计 | 11 组真实二进制输出，200 行主 skill 与三个 manifests 同步 |

环境是本机 macOS aarch64/rustc 1.97.0，jobs=2、incremental=0、dev/test debug=0，
显式同一 RUSTC/RUSTDOC；Cargo 串行。上述命令和日志关联原 G11 摘要，
不代替精简后的新运行。完整可复验命令见 G11 第 7 节。

### 关键失败与回归

| 问题 / 修正 | 当前回归入口 |
| --- | --- |
| 迁移遗漏 headless permission/prompt/plain 依赖 | G11 T01/T02/T09 |
| 只保存 MCP，却误报为运行时已生效 | A07；configuration_state 的 MCP restart journey |
| stale writer、失败发布或冲突覆盖旧 alias/config | A06/A10；MCP 与 skills lifecycle suites |
| coding host 向 resource-only skill 来源请求 Tools | M01/M04；mcp_skill_host 的 capability/source cases |
| manifest、缓存、权限变化仍复用旧同意 | M05–M08；Core native_skills 与 files suites |
| 父批准被用于子任务，或普通命令批准隐式批准 skill | A16/A18；managed child 与 HTTP journey |
| 付费决策 intent/ACK 不确定时重放或 dispatch | A14/A19；Core/managed/ACP 的 ACK 与 cancel cases |
| 新 G11 测试误假定被动 show 会补 workspace grant | 保留拒绝；两条 integration 用显式 load/recover |
| 新测试错误依赖资源 receipt 数组顺序 | 按 kind 查找；onboarding 资源报告用例 |

所有中间失败的完整终态、实际命令、阶段摘要和修正后结果仍可从不可变历史读取。

### 测试与文档精简

本轮仅改测试、测试支持代码、文档和插件描述，生产 CLI/runtime 与协议不变。
共享 PTY 收发/恢复断言及 MCP 进程/日志 I/O；wire、来源、字节和角色预期仍由
各 suite 独立断言，Codex/Claude parser 不合并。
ACP fixture 在配置后首次启动，普通 case 直接安装 skill；两个 adapter 的
onboarding happy-path 继续经过 init。预算/取消/错误用例不再依赖 provider 向导。
4390 个测试名称与 ignored 标记保持；本轮完整复验结果见 G11 第 10 节。

## 17. 后续变更的记录规则

只记录变更、owner/身份、对应要求、源码、命令/结果和剩余资格。
完整证据映射留在 G11；阶段记录不重复复制。旧模板与 goal 提示词保留于 Git 历史。
测试重构后重跑受影响 suites，并在最终源码上取得完整 gates；失败不冒充通过。

## 18. 外部资格与最终交付

G11 第 6–8 节是交付范围与外部资格的唯一详细表。真实 provider、vendor adapter、
远端 MCP/host/Git auth 与其他 OS 不由本地结果推定通过。
PR #989 的 hosted CI 以各次提交的实时 checks 为准；G11 原始“未推送”描述是
该本地验收时点的事实，不表示当前仍没有 PR。
没有新增 origin/GC/远程安装/ACP CRUD 或自动 owner 接管；managed verification
的既有非零 exit_status 限制仍保留。合并、发布和部署不包含在本轮范围中。
