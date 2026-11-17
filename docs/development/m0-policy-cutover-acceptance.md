# M0 Policy 契约切换：剩余实施计划与验收标准

状态：**待实施的验收计划，不是当前能力声明，也不是验收通过记录。**
本文按本轮明确要求保存剩余计划；正式格式仍只在 `docs/specs/` 定义，测试日志和审查原始记录留在 PR / CI。

核查基线：PR [#1055](https://github.com/bitrouter/bitrouter/pull/1055) 的
`a8d48f313ad3237443342d8cf8bbc7aeb4f238d3`，基于 main
`5ba47d51dec8567bdaa9f36546cb65d1a35ca99d`。
此前 CI 成功只证明此前测试集通过，不能作为本文新增标准的通过证据。

## 1. 本次决策与范围

M0 要交付一个贯通默认值、文件、CLI、SDK、API 和持久化的 model-only Policy 产品。
允许破坏性更改，不再把旧 Policy、旧配置别名或跨版本恢复作为兼容目标。
不得为了保留历史行为，维护第二套公共 Policy 格式、隐式转换器或“本地不受新契约约束”的例外。

- `bitrouter` 保留 gateway、`bitrouter/auto` 模型选择、租户隔离的 Eval / Policy 管理与显式更新。
- context 改写、agent 编排、试验性探索和学习算法归 BRO。旧实现可以删除；保留的实验实现须有显式 harness 入口和独立所有权，不能继续读写旧 Policy 契约。
- 本阶段完成必要的模块与入口隔离；大规模 crate / binary 拆包留到下一阶段。
- `bitrouter-ai` 保存轻量契约；应用保存校验、编译、执行和存储实现；SDK 是服务客户端。
- 本轮只产出计划和验收文档，不修改生产代码、数据、云部署或已发布的规范含义。
- P1 多实例新能力、压力测试、性能调优与性能结论不在本阶段。已有 shared gateway 是本次契约切换的现有消费者，必须一起覆盖。

“不兼容旧版本”不等于可以丢失当前版本的事务保障：租户授权、不可变引用、CAS、撤销、幂等、未知用量、流式取消、崩溃恢复仍是必须满足的当前行为。

## 2. 已核实的分歧与处理归属

下表是实施入口清单；同名函数的调用方、生成物和测试必须随之更新，不能仅修改列出的文件。

| ID | 当前事实与入口 | M0 处理 |
| --- | --- | --- |
| F01 | [内置默认值](../../apps/bitrouter/policy/default.yaml) 是 `lockfileVersion: 4`，tiers 含 `context: evidence`；[auto-router 模板](../../templates/auto-router/policy-lock.yaml) 另含 progress guard | 重写为公共 Policy source；默认只选择模型和 effort，保留完整上下文。模板和内置值共用语义校验 |
| F02 | [policy_lock.rs](../../apps/bitrouter/src/policy_lock.rs) 的 `load_for_config`、`load`、`write_atomic*`、history/publish/rollback 直接读写旧 lock；[assemble.rs](../../apps/bitrouter/src/assemble.rs)、[reload.rs](../../apps/bitrouter/src/reload.rs) 继续使用它 | 统一 source 加载与发布；删除旧文件 decoder/writer、v4 校验、转换和回退。保留原子写入、冲突检查与 reload 事务语义 |
| F03 | 同文件 `initialize_*` 创建旧 `PolicyLock` 和 adequacy；[main.rs](../../apps/bitrouter/src/main.rs) 的 `PolicyAction::Compile` 帮助写 v3，实际 compiler 输出 v4 | init 输出新 source；更新全部命令输出。旧 compile/evolve/evidence-verify 退出成熟 Policy 命令，不能只改帮助或序列化字段 |
| F04 | [core/policy](../../apps/bitrouter/src/core/policy/mod.rs)、[core/policy/files.rs](../../apps/bitrouter/src/core/policy/files.rs) 把证书、优化和 guard 写进候选；新服务 `model_policy::source` 反向校验会拒绝非新编译器产物 | 删除旧候选交换链。Eval 事实、候选生成与显式 Policy 更新分开，成熟闭环使用同一公共 source；试验算法归 BRO 或删除 |
| F05 | [config/mod.rs](../../apps/bitrouter/src/config/mod.rs) 接受 `writeback`、`locked`、`evolve` 别名；[config/router.rs](../../apps/bitrouter/src/config/router.rs) 将旧 presets 归一化；[router_migration.rs](../../apps/bitrouter/src/router_migration.rs) 提供历史迁移 | 删除这些路由/Policy 兼容入口，统一 `routers` 和 `bitrouter/<id>`。旧 `--preset`、旧路由地址及配置迁移命令不再提供转换 |
| F06 | [main.rs](../../apps/bitrouter/src/main.rs) 的 `is_legacy_local_policy_read` 保留另一条 status/show 输出路径；[actions/administration.rs](../../apps/bitrouter/src/actions/administration.rs) 暴露旧 artifact/certificates/含 context 的 target；[output/reports/policy.rs](../../apps/bitrouter/src/output/reports/policy.rs) 仍打印 `@preset` | 统一 active/disk 视图及 source DTO，报告明确来源和修订；CLI、远端控制、SDK 不暴露旧运行时 IR |
| F07 | [model_policy.rs](../../apps/bitrouter/src/model_policy.rs) 将新 source 编译为旧 `PolicyLock`，依赖 compiler marker 和再编译相等恢复 source；`PolicyRuntime::preserves_context` 依赖这个 marker | 引入服务私有、只表达模型选择的编译结果。保存公共 source，取消旧结构的编译/反编译桥；上下文保留由类型和执行边界保证 |
| F08 | [shared/publication.rs](../../apps/bitrouter/src/gateway/shared/publication.rs) 持久化旧 `PolicyLock`；[operator DTO](../../crates/bitrouter-ai/src/gateway/operator.rs) 用 `policy_lock: Value` 携带新 source | 新共享发布字段命名为 `model_policy`，使用公共 source 类型；旧字段拒绝。ACL 的 `policies` 集合保持独立，不冒充路由 Policy |
| F09 | [tenant-service](tenant-service.md) 明确保留历史完整性读取、paid recovery 和旧本地 harness 格式；[CI producer](../../.github/scripts/build-pre-m0-producer.sh) 编译旧 commit，测试经旧程序生成的恢复数据 | 撤销本次切换范围内的历史兼容承诺，删旧 producer/patch/缓存/矩阵及专用 reader。改为旧数据拒绝和当前版本恢复测试，详见第 5 节 |
| F10 | [SDK control](../../crates/bitrouter-sdk/src/control.rs) 的 `can_action` 在缺 descriptor 时对 status/models/route/requests 回退旧 `actions`；Policy action 已要求 descriptor；[remote_control.rs](../../apps/bitrouter/src/remote_control.rs) 仍发布 legacy names | 作为同批控制协议兼容清理，删除旧发现回退并显式拒绝旧服务。此项不是已发现的 Policy 授权绕过，不能以此声称现有 Policy 可越权 |
| F11 | [Configuration spec](../specs/configuration/README.md) 把旧 lock 列为本地正式输入；[templates](../../templates/README.md)、[CLI skill](../../skills/bitrouter/references/cli.md)、[生成 schema](../../dist/schema/bitrouter.config.schema.json) 仍引导旧链路 | 正式 spec、schema、模板、CLI 帮助、skill、插件文档和分发测试同批切换；移除“先迁移成 v4”的操作指引 |
| F12 | [PolicyConfig](../../apps/bitrouter/src/config/mod.rs) 仅使用 `serde(default)`，没有禁止未知字段；单纯删除 mode/alias 字段会把旧输入变成静默忽略 | 新 Policy 配置对象严格拒绝未知/已删除字段；schema 和原始 YAML 测试一起更新，不能把“已删除功能”实现成静默无效 |
| F13 | [EvalDecisionRef](../../crates/bitrouter-ai/src/eval.rs) 只有 policy name/digest；[route_evidence](../../apps/bitrouter/src/core/eval/compiler.rs) 按 policy name/projection 聚合；[租户 Eval HTTP](../../apps/bitrouter/src/tenancy/eval/http.rs) 是事实交换，不是运行评估的服务 | 明确客户端执行评估，服务接收有精确资源归属的结果。禁止把同名不同修订的事实混成可更新的证据；移除旧聚合链或为保留消费者重做身份 |
| F14 | [私有 Route 定义](../../apps/bitrouter/src/tenancy/resources/manifest.rs) 可独立携带 `core_choice`、`assessment`、`block`；[require_capabilities](../../apps/bitrouter/src/tenancy/resources/execution.rs) 主要按 `core+harness` 放行；[tenant HTTP](../../apps/bitrouter/src/tenancy/http.rs) 的 Eval 又受 core feature 限制 | 成熟服务按运行产品边界拒绝实验性 Route，包括 policy-free route。Eval exchange 从实验 core feature 解耦；不能靠只测试 no-default-features 证明边界 |
| F15 | [公共资源 Definition::Policy](../../crates/bitrouter-ai/src/tenancy/resources.rs) 的 document 是 `Value`；[PolicyDetail](../../crates/bitrouter-ai/src/control/administration.rs) 含 context/certificates；[PolicyModelTarget::model_only](../../crates/bitrouter-ai/src/routing.rs) 实际默认 Evidence；[prepare_route](../../apps/bitrouter/src/tenancy/resources/compiler.rs) 还有 `policy:variant` 找不到时退回 base 的行为 | typed source 写入、typed source 读出；BRO treatment type 分开；模型 Policy 精确绑定 selected name，不保留 variant 隐式回退；不能全局修改 BRO context 默认值来掩盖命名错误 |

## 3. 目标契约与必要 spec 工作

### 3.1 Policy 的唯一数据源

公共 source 是 `bitrouter_ai::policy::Document`。文件、HTTP、SDK、默认内置文档、展示与导出使用相同字段和语义。
部署可以使用 YAML 或 JSON；原始输入必须在丢失重复键之前完成严格反序列化，不能先转成会吞掉重复键的 `Value` 再声称完成检查。

应用编译结果是私有 `CompiledPolicy` 一类的执行结构，包含已校验的模型目标、路由索引和必需身份，不包含旧 `lockfileVersion`、历史质量证书、context action、guard 或实验状态。
持久化保存 source 和独立管理元数据；执行视图从已验证 source 构建并缓存。编译结果不作为第二套可编辑/可提交文档，也不通过“改 compiler.id”取得执行资格。

`Definition::Policy.document` 与 source-bearing read response 都使用 `policy::Document`，管理报告里的 target 使用 `policy::Target`。
BRO 的 `PolicyModelTarget`/context treatment 与服务 source 分开；有意保留的 BRO 行为通过明确命名表达，不能共享一个名为 `model_only` 却开启 Evidence 的构造器。
资源绑定必须包含不可变资源引用和 literal selected name。`auto` 与 `auto:cost` 可以作为两种独立名字存在，但请求 variant 不再隐式挑选后者或退回前者；明确 router 绑定哪一个，就执行哪一个，缺失即失败。

`source_digest`、编译缓存身份、Policy resource revision、configuration revision、classifier digest 各自命名并定义用途。摘要不能冒充授权或资源修订号。
所有受影响 receipt/eval 引用须跟随新的身份规则，禁止把旧 IR digest 直接改名为 source digest。

### 3.2 文件与默认值

- 规范文件名改为 `policy.yaml`；`policy.path` 可指定其他文件名，但内容只接受新契约。删除对 `policy-lock.yaml` 的隐式发现和回退。
- 仓库内置默认值由维护者重写，不需要用户转换。保留三档模型、strong 默认与工具规则作为初始产品数据；`routes: {}` 仍表示未提供学习路由，不声称质量提升。
- 内置默认值只作为本地零配置选择或显式租户初始化 source。不能隐式注入租户、跨租户读取本机 policy 文件或覆盖用户配置。
- 显式指定的文件缺失、格式错误或不支持时失败，不回退到 bundled default。
- 删除 `policy.mode` 的旧学习/发布权限开关及别名。source 本身不授予发布权：本地显式文件操作受文件与进程权限控制，服务更新受现有认证、scope 和 CAS 控制。运行时不自动发布学习结果。
- 旧 presets / variants 兼容链退出；需要的现有路由行为用显式 named routers 表达，不提供自动转换。保持普通 provider/model 标识和上游协议规则。

### 3.3 CLI、控制 API 与 Eval 闭环

- `policy init/check/show/status/diff` 操作新 source。`show` 的 source 可以直接重新提交；外层报告可以带身份和来源，但不能混入 source。
- active 与 disk 是显式视图：默认查看所选运行实例的 active；离线文件检查使用 disk/check。不得静默用本地配置替代远端或运行实例的结果。
- `policy publish` 表示显式发布新 source，CAS 作为命令/API 参数传递，不再从旧 artifact 的 `parent_digest` 猜测；不以 core 学习 feature 作为模型 Policy 发布依赖。
- 回滚表示显式选择**当前契约下**先前的不可变 revision/source 并重新激活，不恢复旧 v4 snapshot；保留并发变更拒绝、文件原子替换和失败恢复。
- `compile/evolve/verify --evidence` 的旧算法入口移出成熟 Policy 命令。BRO 如保留算法，必须显式产生新的 model-only source，或使用独立实验对象；不能重新引入 v4 parser 或把实验 context 数据提交给服务。
- Eval 保留租户范围的 subject/result 提交、查询、冻结证据和结果归属。**客户端/外部 evaluator 执行评估**，通过已有 revision-bound inference 获取结果，再提交事实；本阶段不新增服务端 Eval job runner 或学习算法。
- 新的 model-policy Eval subject 显式绑定 installation、tenant、configuration revision、Policy resource 的完整 `ResourceRef` 和 selected name；source/classifier digest 是附加证据，不替代这些引用。服务以真实配置/执行记录校验绑定，result 必须引用同一租户内已验证的 subject。聚合和导出保留完整身份，禁止仅按名字/projection 合并。
- 闭环是“客户端针对确切 revision 推理/评估 → 提交并冻结结果 → 客户端据此显式编写新 source → 创建新 revision → 独立授权/CAS 激活”。freeze、结果提交和草稿均不改变 active 配置；旧 generic compiler 不作为该闭环的隐式后端。
- ACL 与 routing Policy 分开：当前 `policy create` 写 ACL 的行为需移到明确的 access-policy 命令命名空间，移除旧重载命令；不得删除鉴权或用路由 Policy 替代 ACL。

Eval subject 的新 schema 2 使用**封闭的身份 profile**，不是把 model-policy 身份改为一组可选字段。
所有现行 subject producer 同步写 schema 2，旧 schema 1 不作 fallback；不同 profile 是当前不同用途，不是新旧版本兼容。

| 入口 / profile | schema 与必需身份 | 接受与拒绝边界 |
| --- | --- | --- |
| Tenant Eval / `tenant_model_policy` | subject schema 2；installation、tenant、configuration revision、Policy ResourceRef、literal selected name，加既有 subject/evidence 身份 | 服务验证精确 tuple；缺字段、错租户/修订/名字、generic 冒充模型 Policy 证据均拒绝 |
| Tenant Eval / `tenant_generic`（无 Policy 的评估） | subject schema 2；installation、tenant、subject_id、evidence 身份；关联 execution 时校验其真实租户/配置归属 | 不要求不存在的 Policy 引用；不能带伪 model-policy identity，也不能进入 model-policy 修订的证据聚合/更新资格 |
| Gateway Eval / `gateway_generic` | subject schema 2；既有 gateway subject/evidence 身份与 Eval scope/authority，不伪造 tenant 或 Policy ResourceRef | 保留 gateway 的独立认证与 evaluator authority；tenant profile 不在该入口接受，gateway profile 不在 tenant 入口接受 |

结果与 freeze 继承已验证 subject 的 profile/身份；generic 和 model-policy 不得在聚合或授权判断中相互替代。
既有 gateway subject、无 Policy tenant subject 的正例与跨 profile 负例必须同时保留。本阶段不把 generic Eval 变成新的服务端执行器。

### 3.4 产品与 feature 的运行边界

成熟 tenant/shared gateway 的 resource/import/configuration/execution 入口拒绝 `core_choice`、paid `assessment`、ACP `block` 及其隐藏依赖；即使 all-features 编译、route 不含 Policy，也不能取得实验执行资格。
这些能力若保留，只能由明确的 BRO 本地 harness 入口持有，BRO 不因此获得租户服务接口。无需先拆 crate，但运行时产品入口不能用 `cfg!(core && harness)` 代替授权/能力判断。

成熟 model-only Policy 与 tenant Eval exchange 在 gateway 最小构建中可用，不依赖实验 `core`/`harness` feature；需将必要 ledger/DTO/transport 从该 feature 所拥有的模块中解耦。
本阶段不重新定义全部 feature 名。显式 server-tools、正常 provider continuation 及其现有权限独立保留；Policy 选择本身不能发起工具执行或 context 改写。

### 3.5 版本分配与真实 reader 选择

| 对象 | 本次要求 |
| --- | --- |
| `policy/1.0.0-beta.1` | 当前 PR 尚未合并且 source 字段不变时保留该标识；若实施改变已发布 source 含义，必须递增 beta，不覆写已发布语义 |
| `configuration/1.0.0-beta.4` | 默认路径、模式/别名和旧路由配置删除是破坏性变化，准备下一 beta；以实施时最新已发布 revision 为基准分配，不与并行工作抢号 |
| `service-api/1.0.0-beta.4` | `model_policy` 字段、控制能力发现、Policy 展示和持久化身份变化统一准备下一 beta，DTO、SDK、schema、能力信息同步 |
| `registry/1.0.0-beta.7` | 保留当前有效契约；其 integer `version` 和 `spec_revision` 是已定义 envelope，本次不因命名不同重做 registry |
| 存储格式 | 在现有配置/服务 spec 中定义当前 store format 与拒绝边界；不用 package version、Policy SemVer 或 migration 个数推断是否可打开 |
| 上游协议、resource revision、CAS、classifier/内容摘要 | 保持各自含义，不批量改为 Policy 版本、不新造 transport 专属 spec |

不能只更新 prose beta。以下实际分派点必须在 S1 固定并形成正反向契约用例（下列数字以核查基线为准；main 如已占用，只顺延号码，不保留旧 reader）：

| 消息 | 实际 wire 选择与切换规则 |
| --- | --- |
| Policy 文件/提交 | `spec_version` 精确等于 Policy beta.1；旧 lock discriminator 不参与 fallback。typed resource 字段不新增一套平行 Policy version |
| Control capability envelope | `bitrouter-control` 的 `protocol_version` 从 1 升至 2；新 SDK 在调用前拒绝 1 或缺失/错误 descriptors，旧 SDK 必须在 capability 校验拒绝 2；不保留 legacy actions 协商 |
| Policy show/status | `Action::PolicyShow/PolicyStatus` descriptor version 从 1 升至 2，typed source/report schema 一起变更。所有调用路径都执行版本检查，不只 CLI 预检查 |
| 其他 control action | 对本次变更 wire shape/含义的 models、route、reload 逐项升级对应 descriptor；未变 action 的 version 保持原值。S1 输出逐 action 的最终表及旧新 SDK refusal fixture |
| Shared publication | `Publication.format` 从 1 升至 2，当前只接受 typed `model_policy`；1、旧字段、混合字段均在 mutation 前拒绝，不能在同一 format 下换字段语义 |
| Eval subject | `EvalSubject.schema_version` 从 1 升至 2，按 §3.3 的封闭 profile 验证身份；全部现行 subject producer 同步升级、旧 subject 1 拒绝。subject 与 result 的常量拆开，不将所有 `EVAL_SCHEMA_VERSION` 消费者一起升级 |
| Eval result/measurement | 未改变 shape 的 result schema 1 与 route-measurement schema 2 保留；result 通过验证后的 subject 绑定身份。若实现需要新增字段，先相应递增并测试，不擅自改变旧字段意义。BRO 的 experiment discriminator 独立判断 |
| 私有存储/导入 | store format、受影响 Policy-bearing manifest/compiler identity 显式切换；未变化的普通/profiled/native route 类型仍按自身有效 discriminator 选择，不能把格式编号当历史先后全部合并 |

旧 reader/new writer 与新 reader/old writer 都必须通过实际 SDK/HTTP raw message fixture 证明拒绝或合法保留。
不承诺任意无类型客户端会自检，但服务不能把已知旧发布/修改请求当成成功的新请求。

先在现有 [Policy](../specs/policy/README.md)、[Configuration](../specs/configuration/README.md)、[Service API](../specs/service-api/README.md) 中完成这些决策、错误码、身份和新旧双方拒绝矩阵的评审，再实现代码。
本文不创建第五份格式规范。规范提交须明确哪些条款尚待代码实现。

## 4. 删除清单与保留边界

| 类别 | 删除或替换 | 必须保留的当前保证 |
| --- | --- | --- |
| Policy 类型与序列化 | `PolicyLock` v4 公共/文件/持久化链、旧 compiler marker 桥、旧文档反解、旧质量证书授权假设 | 严格 source、完整依赖闭包资格、明确不可变绑定、source 原样语义往返 |
| 配置和 CLI | 旧 presets/variants 兼容、`--preset`、`writeback/locked/evolve`、v4 migration、旧 show 输出分流 | 显式路由、配置检查、原子 reload、操作冲突拒绝 |
| 学习与 context | 通过默认策略或服务 Policy 激活的 context/guard/adequacy/exploration，旧 compile/evolve 交换协议 | 模型分类、effort、工具适配、评估事实；BRO 的独立实验须显式隔离 |
| 历史读写与恢复 | 为读取本次拒绝的旧 Policy/旧执行对象而存在的 decoder/兼容分支、旧源码 producer 及专用恢复测试 | 当前格式的 restart、续流生命周期、Unknown、receipt、事务与幂等 |
| 控制发现 | 缺失 descriptors 时回退旧 names 的协商 | 当前 descriptor/version/scope、安装实例绑定、权限最小化 |
| 测试 | 成功读取旧 Policy 的正例、仅验证旧转换器的测试、跨旧 runtime 的大 fixture/build | 小型旧输入拒绝用例，以及从旧测试提取并用新数据重建的通用不变量测试 |

删除依据必须是实际生产/消费链。`v1`、`legacy`、`context` 字符串本身不是删除依据：
上游 API 版本、模型名称、JSON Schema 方言、正常 request context、现行 usage evidence、工具声明、provider continuation 和 ACL 都可能仍有独立用途。
对与本次 Policy 链无关的凭据导入、第三方协议适配、扩展 hook、历史 BRO session 格式建立有路径和消费者的分类清单；不把未知用途留成隐式兼容例外，也不在 M0 顺手重写整个项目。

以下已核实分支须分别处理，不能按版本数字整体删除：

| 路径/对象 | 已确认的消费者与处置 |
| --- | --- |
| resource manifest format 1–3、compiled envelope 1–6；`resources/decoding.rs::LegacyDefinition` | 当前 writers 仍按普通/profiled/native 能力选型，所谓 LegacyDefinition 还处理普通 route 和 credential。替换 Policy 表示，保留或重命名其他有效分支，不删当前凭据/路由能力 |
| admission-profile v1/v2、native-route v3 | 独立的现行流控/路由契约，保留其身份及授权；不能与本次删除的 context experiment 混同 |
| `tenancy/receipts/store/format.rs::{Format,Terminal}` v1/v2 | 两者有当前消费者；保留当前用途及 digest domain 分离，只有真正无当前 writer 的读取分支才能退役 |
| `tenancy/learning/store.rs::valid_record` | v1 是历史 terminal 支持，可随断点删除；v2 仍有条件 writer，v3 是当前 capture，v4 有 native 实验消费者。后几类先明确迁至 BRO/保留何种当前恢复，再同步改 writer/reader；不能按 `v1..v4` 一次删除 |
| `tenancy/receipts/recovery.rs::Seed` | 缺 `usage_evidence_version` 的历史 fallback 退役；当前恢复丢失证据产生的 `Unavailable` 保留 |
| `tenancy/resources/lifecycle.rs::check_in` | 缺 lifecycle metadata 也可能由当前普通 import 产生；先更新当前 producer，再按新规则拒绝，不把它误报成纯历史 reader |
| `Publication.policies`、principal `policy_id` | ACL，保留；tenant Eval 和 gateway Eval 的独立身份与 scope 也保留，不因统一 DTO 混用授权 |
| `baseline-runtime-v7`、BRO session 重建 fixture | 与 pre-M0 Policy producer 不是同一测试依赖。只有替代/退出相应当前功能后才删，物理拆分留下一阶段 |

## 5. 数据切换与恢复

采用明确的版本断点：**新程序只服务当前格式的数据集；不提供旧数据的自动迁移、双读、降级写入或运行时兼容开关。**

这里的 store 指本次变更涉及的服务运行库、租户资源/执行库和共享权威库；不自动扩展到无关的 OS keychain、外部 credential store 或独立 BRO session 文件。
使用该 store 已有元数据位置表达唯一的格式判别，不能增加两个可独立修改、含义重叠的版本开关。
格式标记也不是完整性证明：导入、离线恢复、backup restore 和 bootstrap 都须验证当前 source/envelope，不能通过给旧数据补一个新标记绕过拒绝。

1. 分别为 file-owned SQLite tenant store 与 PostgreSQL shared namespace 确定当前格式、bootstrap/join/restart/restore 检查。shared 的 `gateway_shared_migrations`、`Head.format` 和 encrypted publication 必须一起检查；拒绝发生在 `Engine::join` 注册实例、heartbeat、恢复写入之前。为一致性检查获取物理锁/custody 可以进行，不等于取得执行 authority。旧 store、缺标记的非空 store、部分初始化、foreign layout、未知 future format 在 listener、claim、恢复或有副作用 migration 前拒绝。
2. 同一 store 内即使 active 配置已不含 Policy，只要保留本次不支持的数据格式，也不能据此放行。禁止只检查 active head 然后靠晚期 decoder 失败来兜底。
3. 旧部署停止接收新请求，在旧程序上完成可完成的工作；旧数据和旧程序留在仓库外供运营者归档或自行查询。新实例使用显式的新位置/数据库与新 installation identity，通过当前 API 重新配置资源。
4. 新程序不得删除、清空、就地重写旧数据库或用户 policy 文件；不得因打开失败创建看似成功的空库。拒绝测试比较逻辑记录、authority、文件内容和 provider 调用数，避免数据库锁文件等无关物理差异。
5. 删除兼容读取后，不再承诺新程序恢复旧 paid work。尚未结清的旧任务须在旧部署处理；新程序不能替旧任务重发、退款、补造 receipt 或宣称已恢复。
6. 当前版本仍必须恢复自己的任务。按实际 dispatch 状态保持 no-send / observed / unknown 等含义，缺少 provider 证据时不推断成功。已知 receipt/output/usage 保持原值，恢复不能选中 assessment head；只有具有排除、cleanup/stop-set 等所需证明时才可将缺失 terminal 转成 Unknown，用量仍未知，不能补零。证明缺失/矛盾时停止 ready/dispatch，不执行猜测修复。
7. 可释放的 capacity reservation 与 custody 仅在所需 settlement/cleanup 确认后释放；事务 begin/rollback/commit 错误、取消或 lost acknowledgement 留住未知义务。每个 configuration revision 的**累计 accepted-job 计数不因完成、失败、取消、Unknown 或恢复而回退、退款或重置**。通过 attempts/counters/receipt 的数量和身份验证不重复发布/接受，不声称证明外部支付对账。精简 feature 不得忽略无法处理的当前义务，须明确拒绝 ready。
8. 若当前 DB DDL 依赖历史 migration，先把当前结构/验证器从历史转换逻辑中分离，再建立 current baseline。具体审查 030/032/033/034/036 的 ownership/scope、042 archive、043 unknown layout 与 backfill、044 authority schema validator、045 dispatch intent/receipt metadata、046–048 writer fences，以及 shared 独立 schema ledger。删除 backfill/转换程序，保留当前 DDL 和可复用验证器；baseline 与 format marker 原子初始化，不给已有非空旧库盖新标记。
9. 回退部署使用旧程序及其原数据副本；不能把新库交给旧程序，也不能把复制旧 rows 称为支持的迁移。产品文档须明确这一破坏性发布边界。

S5 必须逐个替换的 Policy 解码链包括：`tenancy/resources/manifest/decoding.rs::Definition::deserialize`、`manifest::Import::validate`、`resources/storage::authenticate`、`execution/retained::decode_with_operation` 中把不合格变成 bool 的分支、`Resources::open_paid_recovery`，以及 `resources/recovery{,/v2}`、`tenancy/audit/recovery{,/v2}` 的调用方。
保留当前格式的 `gateway/shared/engine/recovery.rs::Engine::{reconcile,reconcile_one}`；`tenancy/actor/learning_startup` 与 `learning/store/recovery::{prepare,apply}` 中仍承担当前义务的校验须迁移到正确的服务/BRO 所有者，不能随历史 decoder 消失。

旧测试依赖按下面整组退役，而不是仅从 CI 中跳过失败测试：

- `.github/scripts/build-pre-m0-producer.sh`、`.github/fixtures/pre-m0-terminal-export.patch`，`ci.yml` 中 producer 的 cache/build/archive 依赖。
- `distribution-acceptance.yml` 的旧 producer 与跨版本 transition campaigns；`paid/recovery/process.rs::historical_producer` 及其 integrity/reduced/terminal 消费者。
- `paid/revision_budget.rs` 的历史生产/消费用途，`tests/configuration/trajectory_features.rs` 的历史 guarded-draft consumer，以及旧 tenant-service 升级指引。
- 先为 `tests/execution/native/capture/learning/recovery/`、`tests/learning_startup/driver.rs` 及共享 engine 的当前不变量建立新格式覆盖；实验功能若退役，注明其 producer 和公开入口一并删除，不能留下无恢复能力的 producer。

每个删除项在实施 PR 中提供“producer → reader → 新测试或退役入口”的映射。旧格式负例保留小型字节 fixture 即可，CI 不再需要旧可执行文件、revision sidecar、旧 checkout 或 producer 环境变量。

## 6. 实施顺序与交付物

| 步骤 | 内容与主要路径 | 依赖与退出条件 |
| --- | --- | --- |
| S1 契约冻结 | `docs/specs/{policy,configuration,service-api}`；版本、命名、source/存储身份、切换和删除清单；规范 conformance cases | 先于代码；所有入口有唯一归属与新行为，无未决兼容分支 |
| S2 公共类型与执行结构 | `bitrouter-ai::policy`、`tenancy::resources::Definition`、control administration、Eval identity、共享 operator DTO、`model_policy`、Policy runtime、tenant/shared 存储模型 | S1；所有新写入保存 source，精确 selected name；模型执行不依赖旧 PolicyLock；AI 不增加运行时依赖 |
| S3 默认值与本地读写链 | `apps/bitrouter/policy`、`templates`、config/router、assemble/reload、main/actions/output；删 router_migration 与旧 aliases | S2；零配置、文件、init/show/publish/reload/rollback 同一契约，失败不覆盖 active |
| S4 服务与 Eval 闭环 | tenant resources/execution/cache、shared publication/operator、SDK/control、Eval exchange 从实验 core 解耦；Policy-free context/实验入口隔离 | S2–S3；客户端驱动的两租户评估更新、完整证据身份、精确修订绑定、缓存冷/热、all-features 运行边界通过 |
| S5 历史兼容删除 | 旧 source/执行 reader、store preflight、recovery、migration baseline、旧 fixtures/producer/patch/CI cache | S2–S4；先建新格式恢复和拒绝测试，再删除旧测试。每项删除都有不变量承接或明确退役理由 |
| S6 分发与验收 | specs/schema/dist、skills/plugins、SDK consumer、packaged process tests、CI 与独立审查 | S1–S5；同一最终 HEAD 完成下面全部门槛，不能复用 a8d48f31 的成功标记 |

步骤可拆为有清晰依赖的提交，但最终候选不得同时发布相互矛盾的 source writer / reader。
不要求在 M0 实现新的自动学习算法；也不能把删除旧兼容解释成删除用户需要的 model-only Eval/update 产品闭环。

## 7. 验收矩阵

所有条目均为**未验收**。实施 PR 为每个 ID 附上实际测试定位及同一候选 HEAD 的结果；本文不预填通过状态。

| ID / 对应发现 | 验收场景 | 通过判据 |
| --- | --- | --- |
| A01 / F01–F15 | 消费者与删除清单 | 默认值、CLI、文件、SDK、API、存储、恢复、导出、schema 和分发均有处理归属；每个保留兼容分支有独立当前消费者，不能写“以后再说” |
| A02 / F01,F02 | 零配置默认 `bitrouter/auto` | 实际打包二进制使用新 source；mock provider 证明默认模型、工具 fallback 与 caller effort 规则；无历史文件也可运行 |
| A03 / F01,F11 | 所有发行模板 | 从临时目录按 README 完成初始化和启动；模板 source 同时可被服务提交；无 v4/context/guard 默认值及错误的 v3 帮助 |
| A04 / F02,F03 | 本地完整读写链 | init → check → show/export → diff → 显式发布 → reload → 当前格式 rollback 全程可用；导出语义等价于已加载 source |
| A05 / F02,F05,F12 | 拒绝旧文件与别名 | v1–v4 lock、旧路由配置/地址、旧 mode/flags、缺版本、错误 namespace/beta 和显式缺失文件均失败；嵌套旧字段不能被静默忽略；不 fallback、不覆写 |
| A06 / F02,F08 | YAML / JSON / API 严格输入 | 各层重复键、未知字段、null/省略差异、超限和 classifier mismatch 在写入前失败；证明原始字节路径而非只测 Value |
| A07 / F07 | 编译和存储单一来源 | 当前 store 持有 source 与独立元数据；不持久化旧 IR；改 marker、删 context、替换 digest 都无法获得资格；新启动与缓存冷/热结果一致 |
| A08 / F06,F10 | 检查接口统一 | CLI active/disk、远端控制和 SDK 输出清楚区分来源及 revision；show 的 source 可重新提交；旧 capability response 明确拒绝 |
| A09 / F08 | 共享发布 | HTTP/SDK 提交 typed `model_policy` 成功，旧 `policy_lock` 失败；不支持输入不改变 head、配置、credential binding 或事务快照 |
| A10 / F04,F07,F15 | 两租户 Policy 生命周期 | A 无法读写、评估或激活 B 的 Policy；创建 immutable revision、CAS、撤销、restart 正确；同一文档 `auto`/`auto:cost` 精确选择、名字缺失拒绝、同名不同资源不混绑；当前格式先前 revision 持续有效 |
| A11 / F04,F13 | Eval/update 真实闭环 | 客户端通过真实 revision-bound inference 评估并提交/冻结事实，再独立提交 source 和 CAS 激活；证据完整绑定 installation/tenant/config/Policy ResourceRef/name。同名跨资源/跨修订、伪造归属、跨租户结果拒绝；不依赖旧 compiler 或新增服务 job runner，不自动改 active |
| A12 / F01,F07 | model-only 运行边界 | 默认/显式分类/工具 fallback/fixed/bypass/retry/续接均保留完整 admitted context；Policy 选择不启动 context rewrite、guard、adequacy 付费探索或工具执行。显式授权 server-tools 的独立执行仍正常，不据此误判 Policy 越界 |
| A13 / F02,F03 | 并发更新与原子性 | stale CAS、并发文件修改、部分写入、编译失败、reload 失败不造成 source/runtime 分裂；故障恢复维持可解释的 active 状态 |
| A14 / F07,F09 | 当前配置与缓存完整性 | 未选中的非法依赖也拒绝；同 revision 换 authenticated source 不复用旧视图；撤销/停用仍实时校验；无效请求不误伤健康 authority |
| A15 / F09 | 旧数据断点 | tenant store 与 shared namespace 分别验证；在 listener、dispatch、claim、shared join/heartbeat 和 DDL/ledger mutation 前拒绝。覆盖 inactive/hidden/terminal history、分页后才出现的旧记录、伪 current marker、partial/future/foreign format、离线导入/restore/bootstrap；逻辑记录、authority、migration ledger、publication head 不变，provider 调用为零 |
| A16 / F09 | 当前版本恢复 | 当前 producer 的已知 output/usage/receipt 不变、不选 assessment head；无证明或矛盾证明 fence，Unknown 不补零。begin/rollback/commit/cancel/lost-ack 故障保留 custody 与 reservation 直到确认 settlement；每个配置修订的累计 accepted-job 计数在恢复前后及两次 restart 后严格相等，无增量、退款或重置；无新 provider send 或重复 terminal。精简 profile 拒绝无法处理的当前义务 |
| A17 / F09 | 新安装 schema baseline | file-owned tenancy 的 SQLite、shared gateway 的 PostgreSQL、现有通用存储支持的 PG/MySQL 分别验证，不虚构 MySQL tenant profile；tables/indexes/collations/constraints/authority 初始化对齐，baseline+marker 原子。旧 store 拒绝前无 DDL/ledger mutation；复用 validator 与历史 backfill 解耦 |
| A18 / F09 | 历史测试退役 | 删除 pre-m0 producer、patch、cache 和必须 checkout 旧源码的 CI；每个旧测试的当前幂等/隔离/事务保证有新格式测试承接，过滤器零匹配必须失败 |
| A19 / F05,F06 | 路由与 ACL 分离 | named router 正常工作；旧地址无静默归一化；access-policy CLI/API 与 routing policy source 不混淆，虚拟 key/RBAC/scope 的现有保护不退化 |
| A20 / F07,F11,F14 | Feature 与包边界 | gateway 最小、default、core-only、harness-only、all-features、shared 及 HTTP/1.1/h2c 现有组合通过；model-only Policy 和 tenant Eval exchange 在最小构建可用，AI/SDK 不依赖应用实现 |
| A21 / F11 | 规范及生成物 | version/spec/DTO/schema/examples/CLI help/skills/plugins 一致；执行 policy schema drift、dist-helper check、公共 API 和 SDK consumer 检查；旧操作指引无残留 |
| A22 / 全部 | 最终候选验证 | fmt、Clippy、完整现有 CI 和针对性进程/数据库/SDK 测试在相同 HEAD 通过；明确列出删除的测试和替代证据，不以测试数量减少说明成功 |
| A23 / 全部 | 独立对抗性审查 | 审核至少从“旧数据误接受、隐藏默认绕过、Eval 写入不被读取、缓存跨修订/跨租户复用、恢复重复发送、feature 绕过”攻击新边界；所有阻断项关闭后才能声明 M0 完成 |
| A24 / F14 | 无 Policy 的实验执行旁路 | all-features 实际服务对带 core_choice/assessment/block 的 import/configuration/执行入口拒绝，隐藏依赖也拒绝；无副作用。BRO 显式本地入口的保留行为有单独正例，不能因 tenant 拒绝而默认宣称已删除 |
| A25 / F06,F08,F10,F13 | 真正的 wire 版本切换 | control protocol/action descriptors、publication format、Eval subject 各自运行新读旧/旧读新 fixture；无 version-missing fallback；未改 result/measurement/admission-profile/native-route 的现行独立格式仍可工作 |
| A26 / F09 | 删除与替代逐项对应 | 第 4/5 节每个分支有 producer→reader→替代测试/退役入口映射；无仍会创建但不能恢复的当前任务，无因测试含 legacy/v1 而误删的现行能力；干净 checkout 的 CI 无旧构建依赖，测试选择非零 |
| A27 / F13 | Eval profile 正反例 | model-policy tenant、policy-free tenant、gateway generic 均以新 subject schema 2 通过正确入口；profile 的必需身份按 §3.3 检查，跨 profile/跨 endpoint/缺 model-policy tuple 拒绝。旧 subject 1 不 fallback，未变 result/measurement 契约保持；generic 事实不能成为其他 Policy 修订的评估证明 |

A12 对比规范化后的语义载荷，允许正常 provider 协议转换及已明确的模型/effort 修改；不把不同协议的 JSON 字节相同作为标准。
A16 是当前版本故障恢复，不要求跨旧版本运行；新 format 的 retained revisions 仍然是有效历史，不能一并删除。
A20 沿用目前单 crate 的 feature 名作为验证入口，不暗示已经完成 BRO 拆包；检查按实际依赖支持组合执行，不要求互斥 feature 被强行同时运行。

## 8. 审查和交付说明

本计划经过两名独立 agent 的源码审查与修订后复查，已将 Eval 身份/执行归属、Policy-free 实验旁路、wire 版本选择、两类存储断点和当前恢复保证纳入验收。两名 reviewer 均确认计划无剩余阻断项，可以进入 S1；这不是代码或实现验收通过的结论。
实施前若 main 前进，重新核查上述路径与新 consumer；不得机械删除同名新增功能。
发布说明必须明确旧文件、旧客户端和旧 store 的拒绝范围、重新配置流程及回退边界。
在 A01–A27 获得证据前，M0 的整体产品契约统一仍未完成；此前 PR 的 CI 结果不覆盖新增验收义务。
