# Router 替代 Preset：配置与身份迁移子批次

状态：**R1–R6 已实现，本地批次验收通过；尚未执行远端 CI 和真实外部 harness 互操作验收。**

日期：2026-09-15。源码基线：`f1f29db0`，`1.0.0-alpha.30`。

本文记录原始 M0–M1 第一批中的路由配置与身份迁移子批次，不代表原始六项已全部完成。
整体配置状态契约另见 [CONFIGURATION_STATE_CONTRACT_SPEC.md](CONFIGURATION_STATE_CONTRACT_SPEC.md)。
检查回执、HTTP checker、checker 诊断和 guardrails 独立制品仍待实现。
首批目标是：用户直接配置、调用、诊断一个 router，而不必同时理解 preset。
默认 coding router 复用现有 policy-lock；本批不开发新的模型选择算法。

## 1. 已确认的产品决定

1. `apps/bitrouter` 默认只提供一个最小 coding router；其他 router 由用户定义。
2. 用户选择 router 后，主要将模型选择交给它。
3. Router 替代 preset，成为唯一公开的具名请求处理配置对象；不要求再建立同名 preset。
4. Router 可以包含请求默认值、选择策略与能力绑定；它们的覆盖权限分别定义。
5. 默认 coding router 复用现有 policy-lock，由初始化流程建立绑定；缺少模型或策略时明确未就绪。
6. 首批沿现有 SDK、应用装配与查询路径实施；双模式、client/server 和独立 extensions 的长期边界保留。

这修订了此前 v0.6 设计中“preset 绑定 router、M1 不开放任意
`bitrouter/<name>`”的前提。旧文档的两个 router 场景改为：一个默认 coding，
加一个用户创建的 router；第二个 router 仅出现在用户配置或测试 fixture 中。
此前文档、旧例子不能覆盖本节决定。

## 2. 第一批交付与范围

交付演示：初始化 coding → 调用 `bitrouter/coding` → 用户创建第二个 router →
直接调用其地址 → 查询配置来源、运行绑定及有记录请求的实际模型。

批次内：统一配置语义、直接寻址、policy 绑定、默认 coding 初始化、旧 preset
兼容输入与迁移、CLI/远程诊断、既有成功与失败行为的回归。

下一批才交付 HTTP checker 和覆盖提前拒绝路径的独立执行回执；它们依赖本批
建立的 router 身份。本批的请求历史只声明现有 settlement 记录的覆盖范围，
不能声称已完成原 M1 的完整回执保证。

本批不引入通用 DSL、程序继承、热替换框架、router 套 router、任务调度、
ACP workflow 或默认 `bro code` 语义切换。新的 router 配置不接受尚未实现的
`checks`、`workflow` 等字段；不得静默忽略后宣称已提供保护。
Guardrails 独立发布及其他 extension 默认排除仍是后续发布门槛。

## 3. 当前实现与迁移接点

| 已核验接点 | 当前事实 | 本批行动 |
| --- | --- | --- |
| [config/mod.rs](../crates/bitrouter-sdk/src/config/mod.rs) / [presets.rs](../crates/bitrouter-sdk/src/config/presets.rs) | Preset 混合 model、policy、prompt/params、provider 偏好；解析必须产生 model | 引入单一有效 router 定义；旧格式转换到它 |
| [routing.rs](../crates/bitrouter-sdk/src/language_model/routing.rs) / [pipeline.rs](../crates/bitrouter-sdk/src/language_model/pipeline.rs) | ModelResolution 保存默认值及 policy；resolve_route 再执行 selector | 固定 router 身份，保留已有 selector、fallback、交付与计量 |
| [context.rs](../crates/bitrouter-sdk/src/language_model/context.rs) | 请求显式参数优先；system 缺失时才填默认值 | 保持默认值语义，避免重命名时改变请求 |
| [policy_lock.rs](../apps/bitrouter/src/policy_lock.rs) | 加载、校验、配置编辑、初始化遍历 config.presets；policy 绑定需要文件配置与 base model | 逐项迁移消费者，不能只修改 resolver |
| [actions/administration.rs](../apps/bitrouter/src/actions/administration.rs) / [actions/route.rs](../apps/bitrouter/src/actions/route.rs) | 已有 policy 绑定查询；route preview 的 live/config 能力不同 | 复用 action，明确来源和模拟范围 |
| [reload.rs](../apps/bitrouter/src/reload.rs) | 有 participant、mixed、unknown、restart-required | 对新 router 配置做明确分类，不宣称完整原子热替换 |
| [paths.rs](../apps/bitrouter/src/paths.rs) / [onboarding.rs](../apps/bitrouter/src/onboarding.rs) | 支持文件配置和零配置；初始化管理已保存设置 | 只在显式初始化时写文件，升级不自动替换已有设置 |
| [metering](../apps/bitrouter/src/metering/entities/requests.rs) | 已有实际 provider/model 和 session/route-scope 归属 | 新增 router 归属，不复用含义不同的 route_scope_id |
| [schema helper](../helpers/dist-helper/src/schema.rs) | 从 Config 生成已提交 JSON Schema | schema 随新增配置字段一起更新 |

重要顺序事实：当前 pipeline 在 pre-request hooks 之后才应用 preset 默认值和
进行 policy selection。本批不顺手重排 auth/policy/guardrails。下一批 checker
必须在真实调用链上保证：认证/授权完成、有效输入已形成、selector 尚未执行，
再检查输入；本批不能由新增 Router 类型推断该顺序已正确。

权限契约沿用现有 requested-selector 检查：授权 `bitrouter/coding` 表示允许调用
这个 router，并由其绑定策略选择模型；只授权物理模型不会自动授权这个地址。
当前流程不会在选模后再次执行物理模型 allowlist 检查。本批不把 routing.only
解释成权限白名单，也不声称已提供选模后的逐模型授权。

## 4. 配置语义

以下 YAML 已在本分支实现；发布前仍需合并与 CI 验证。
现有 providers、models、policy.path/mode 等配置继续按现有语义提供。

```yaml
routers:
  coding:
    selection:
      kind: policy
      policy: coding
      base_model: "fixture:strong"
    defaults:
      params:
        temperature: 0.2
```

`fixture:strong` 仅为示意 selector，必须由有效 provider/model 配置支持；
`coding` policy 必须已存在于有效 policy-lock。不得复制示例模型作为发行默认值。
`base_model` 保留现有 policy runtime 的真实输入语义；它不是请求者每次选择模型，
也不是声称存在的故障兜底。后续能否从 policy 推导它，需要另行验证行为等价。

同时支持最简单的固定模型形式，用于旧 preset 迁移及用户的确定性需求：

```yaml
routers:
  project-coding:
    selection:
      kind: model
      model: "fixture:base"
```

这里的 model 是现有合法 selector，可引用现有 virtual model/provider cascade；
不再新增第三套候选列表机制。不得引用 router/preset 从而隐式递归。

| 配置部分 | 规则 |
| --- | --- |
| selection | `model` 与 `policy` 两种已使用的形式互斥；未知 kind/字段拒绝 |
| defaults.system_prompt / defaults.params | 保持当前“只补缺失值”的语义及规范化协议字段处理；不当作强制约束 |
| selection.routing（候选名称） | 承接旧 provider 偏好与排序；保留原合并行为，不声称是授权白名单 |
| 执行约束 | 继续执行既有宿主约束；新配置不暴露可放宽它们的任意参数 |

第一版不提供任意 defaults 合并、extends 或 program 模板功能。
Router config 有效与 provider 当前可达分别报告；瞬时网络失败不等于配置语法无效。

## 5. 默认 coding 与初始化

- 默认产品只有 coding 这一项基线定义/初始化路径，没有默认 general/research router。
- coding 使用普通 router/policy 契约，执行核心不按名字硬编码特殊分支。
- 初始化使用用户明确选择的模型建立现有格式的 policy-lock 与 router 绑定；
  若已有等价配置则复用，遇到不一致先报告，不能覆盖已有策略或凭据。
- 无凭据/无模型/无 policy 时报告 coding 未初始化或未就绪；不会偷偷选择目录首项、
  启用付费 provider、伪造 policy 或改用其他算法。
- 尚未初始化的产品入口与“用户显式配置了损坏的必需 policy”不同：后者继续使
  配置校验/激活失败；前者不能让原本可用的零配置物理模型代理整体停止。
- 新增 coding 不能改变旧文件的默认模型、已有 `chat.model`、harness 会话或全局
  policy.mode。初始化 mode 的选择沿用明确的现有流程并报告，不借迁移调整算法行为。
- 用户已有 preset/router 名为 coding 时，不注入第二份定义、不自动重命名。
  从旧 preset 导入的 coding 保持原语义；“默认提供”不是覆盖用户配置的优先权。
- 配置文件写入仍是显式操作；服务启动和读取诊断不自动保存迁移结果。

## 6. 寻址与兼容

新规范地址为 `bitrouter/<router-id>`。router id 的候选语法为
`[a-z][a-z0-9_-]{0,63}`；大小写敏感，拒绝嵌套路径、冒号与无效编码。
旧 preset 中不符合此语法的名字仍可经旧地址使用，迁移必须给出显式重命名方案。

入口解析结果区分原始 selector、router id、绑定身份、有效模型；fallback 不改变
router id。普通模型/显式 provider pin 保持兼容路径，不在本批偷偷改为 coding
自适应选模，也不宣称这些请求已受未来 router checker 保护。

| 输入 | 第一批行为 |
| --- | --- |
| `bitrouter/coding`、用户新 router 地址 | 直接解析对应 router，无同名 preset 要求 |
| 未知 `bitrouter/<id>` | 明确错误；不降级为 provider 查找或 default router |
| `@name`、旧 `:variant` | 兼容窗口内可解析 legacy preset 或新 router，使用同一有效定义；因此迁移后旧调用仍有效；保留已知/未知 variant 的旧行为 |
| 规范地址 `bitrouter/<id>:variant` | 首批不开放（auto 例外）；明确报不支持，不把后缀隐式吞掉 |
| `bitrouter/auto[:variant]` | 保留旧 policy-bound 入口的行为和错误；迁移后可绑定 routers.auto，但不能改指向 coding |
| `bitrouter/fusion` | 保留原 ingress alias；不允许新 router 抢占 fusion 名称 |
| 原生 harness/ACP session | 保持模式及会话身份；不因新地址启动持久任务 workflow |

auto 是兼容特例：首批 routers.auto 仍需 policy selection，不能用固定模型配置
改变旧 public alias 契约。兼容地址不作为第二类对象重复展示。

一次请求使用已解析的 router 定义，不在 fallback 中重新读取 latest router 配置。
首批 router 定义变更按 restart-required 处理；旧 preset 的既有 reload 行为保留，
并明确显示兼容差异。未来局部热替换另行交付，动态凭据撤销等仍沿现有约束生效。
这不保证冻结外部 provider 状态，也不自动冻结可变 policy runtime。

## 7. 单一运行定义与迁移

实现只保留一个有效 router 模型及调用路径；旧 PresetConfig 可以在兼容窗口保留为
输入结构。配置转换在装配/候选配置准备时执行，结果有实际 resolver/policy/query
消费者，避免先搭建无调用者框架。

Config 当前能由 Rust 消费者直接构建/修改；不得在反序列化时生成一份永不更新的
隐藏缓存。所有装配入口和 reload 候选走同一转换/校验函数，读取固定快照。
SDK 公共结构体变化按 alpha API 迁移明确记录，workspace 内消费者同 PR 更新；
不新增 pub use facade 违反仓库规则。

本批 Rust 源码兼容说明：`Config` 新增 `routers`；构建完整 struct literal 的
使用者需要补空 map（使用 `..Config::default()` 的调用者自然继承空值）。
`ModelResolution` / `PresetResolution` 新增可空 `router` 身份；自定义 routing
table 对普通模型返回 `None`，或使用已有 passthrough 构造器。新配置类型从
`config::router` 模块引用。`PipelineContext::router_identity()` 及同类型的
`RouterRequestIdentity` event 提供一次请求的 router 归属，不改变 settlement
调用次数。应用的 metering/report 完整 literal 也需补新增可空字段；JSON 读取
对旧 daemon/旧记录保留缺失字段兼容。

| 旧输入 | 归一化规则 |
| --- | --- |
| preset.model，无 policy | model selection |
| preset.model + preset.policy | policy selection，精确保留 base model |
| system_prompt / params | defaults；继续沿用显式请求优先 |
| routing | selection 偏好；不升级成新的授权保证 |
| variant | 兼容解析记录，不复制出多个 router |

`presets.x` 与 `routers.x` 同时存在时，即使内容看似相同也拒绝激活；不同名可在
过渡期共存，诊断报告来源与迁移建议。Router 自引用/互引用及未实现配置提前拒绝。

迁移工具先输出报告和候选文件；原文件默认不变。应用时校验完整候选与 policy
引用，检测源文件并发变化、保留备份并原子替换。不能通过 JSON 重写整个 YAML
静默丢失注释；不支持的 YAML 结构明确诊断。迁移不修改 signed policy 内容、
policy digest、凭据、模型列表或 mode；多文件初始化沿用已有锁与恢复语义。

兼容时间：本批新增 routers 与旧输入诊断；至少保留一个已发布 alpha 的迁移窗口。
旧 presets/@ 地址的删除另设 breaking PR，需迁移覆盖和发布说明齐全后确定版本。
新文档、初始化输出与新示例只引导 router；不能无限期维护两套产品概念。

## 8. 诊断、发现与请求归属

复用 status/models/route/requests 和 administration action，不新增平行控制服务。
CLI 新参数和 JSON 字段名称在对应 PR 固定；以下是报告语义，不是已存在的命令。

- Router 查询：id、配置来源（默认/用户/legacy）、就绪状态与原因、selection、
  脱敏默认值摘要、saved/running 差异和 restart-required。
- Route preview：原始 selector、router id、绑定身份、source、可解析的候选；
  明确说明是否执行了动态 policy 决策。当前 live preview 无 policy replay，
  本批不为展示理由而伪造“会选中哪个模型”。
- Model discovery：规范 router 地址可被兼容客户端发现；内部报告区分 router 与
  真实模型。保持原模型目录，不因 config.models 非空而隐藏 router 地址。
- Request history：给新记录增加可空 router id、binding digest、原始 selector，
  实际 provider/model 和现有 usage 保持原含义；旧记录为 unknown/null，不能
  按当前配置倒推历史归属。`route_scope_id` 是既有 session/policy 归属，不可挪用。
- 不在数据库或遥测中重复计费；history 只覆盖已有记录路径。Exporter 关闭时，
  本地已有记录仍可查询；提前 auth/check 拒绝的完整回执在下一批完成。
- Binding digest 使用带版本的稳定序列化及 SHA-256，仅覆盖 router id、selection、
  routing 与默认字段的名称/是否存在；排除所有 prompt/params 值以及它们的散列。
  它是路由绑定摘要，不能识别敏感默认值变更，也不代表完整配置修订。
  saved/running 比较在进程内比较完整配置，报告只返回变更状态。policy artifact
  digest 单独记录；不能用一个 digest 冒充全部运行状态或远端实现版本。
- Local/remote 共用报告语义；远程不读取本地 Config/provider 环境作为补偿。

## 9. PR 拆分与依赖

顺序：R1 → R2 → R3 → R4 → R5 → R6。每项完成后仍可发布 alpha；单项过大时
沿列出的消费者拆为连续小 PR，不将 schema、模型算法和 crate 重组混在一起。

| PR | 建议标题 | 范围与主要文件 | 退出条件 |
| --- | --- | --- | --- |
| R1 | `refactor(router): normalize legacy preset bindings` | 有效 router 定义、旧输入归一化；config/presets、routing_table；迁移 policy 的只读绑定枚举与校验消费者 | 旧地址、默认值、policy/variant、错误、显式模型行为等价；无新用户语法 |
| R2 | `feat(router): add named router configuration` | routers schema、两种 selection、直接地址、冲突/递归检查；SDK config、resolver、pipeline context、dist/schema | 不建立 preset 即可经真实 HTTP 调用用户 router；错误请求零上游；保留稳定身份 |
| R3 | `feat(router): initialize the default coding router` | policy 配置编辑与初始化、onboarding/default composition、模板和 CLI；复用现有原子写入和 policy runtime | 只默认提供 coding；用户 router 同路径执行；缺失绑定准确诊断；旧配置不被覆盖 |
| R4 | `feat(router): expose bindings in routing diagnostics` | action reports、daemon/remote、model discovery、reload 分类、request identity 与数据库增量迁移、必要的 observation schema 更新 | saved/running/preview/历史各有真实来源；fallback 身份稳定；旧记录不伪造归属 |
| R5 | `feat(router): migrate legacy preset configuration` | 显式迁移报告、候选文件、并发检查/备份/应用；legacy 警告；policy init 兼容参数；文档/skill | 迁移前后行为等价；冲突和不支持输入可操作；失败不破坏原文件 |
| R6 | `test(router): verify routing migration end to end` | 跨协议、stream/non-stream、policy/continuation、原生 harness 接线与发布验收；补齐批次缺口 | 下表 B01–B16 完成；列明仍未通过的外部互操作验证 |

R4 可拆为“查询/发现”“历史字段迁移”两个 PR，数据库采用可空新增字段，避免
要求历史回填。R6 是批次验收，不是把前五项的测试拖到最后；每个 PR 均有本项测试。

R1 的具体执行顺序：先添加旧行为 characterization cases → 引入私有归一化定义
及现有 resolver 消费者 → 接上 policy 绑定枚举/校验 → 删除重复只读解析 → 运行
针对性与仓库必需检查。暂不改配置写入器、CLI、默认值或生成 schema。
遇到无法在该边界内消除的 source dependency，先记录并缩小接口，不新建空 crate。

## 10. 验收 ledger

以下场景由新增测试与已有回归共同验证；最终命令结果见第 12 节。
真实外部 harness/提供商互操作与远端 CI 未执行，本地 mock HTTP 不能替代它们。

| ID | 验收场景 | 对应 PR |
| --- | --- | --- |
| B01 | 旧 model-only / model+policy preset 的 defaults、variant、fallback 与错误保持 | R1、R5 |
| B02 | 新 router 直接调用，不依赖同名 preset；默认仅 coding，第二个来自用户 fixture | R2、R3 |
| B03 | 重名、新配置未知字段、非法 id、缺失/递归引用提前拒绝，零上游调用 | R2 |
| B04 | 旧 @、auto、fusion、provider pin 与 bare-model 契约保持；新后缀错误明确 | R2、R5 |
| B05 | coding 复用真实 policy-lock，base/effort/mode/decision/continuation 语义保持 | R3、R6 |
| B06 | 显式请求参数优先；默认值规范化及跨协议安全检查不退化 | R1、R2 |
| B07 | 无模型/凭据/policy 与损坏配置分别报告；已有 coding 不被默认值覆盖 | R3 |
| B08 | 不改变已有 chat.model、harness 执行位置/模式；未知效果不被显示为成功 | R3、R6 |
| B09 | 新请求 router 身份贯穿 fallback/stream 终结；policy 和实际模型可区分 | R2、R4 |
| B10 | saved/running/restart 来源准确；remote 无本地补偿；preview 不冒充真实决策 | R4 |
| B11 | router discovery 覆盖有/无 config.models；就绪诊断与可调用目录一致 | R4 |
| B12 | history 老数据为 unknown；新数据身份准确，无重复计费；关闭 exporter 仍可查已有记录 | R4 |
| B13 | 迁移幂等；并发修改/无效 YAML/写入失败安全退出；备份和回退可用 | R5 |
| B14 | 新旧配置经 Chat/Responses/Messages/Gemini 的受支持 stream/non-stream 路径行为一致 | R6 |
| B15 | schema、CLI help、skill、plugin 引用、模板和 SDK API 说明与实现一致 | 每个改变公开面的 PR |
| B16 | 单纯 model 代理无需新 task store；未声称 checker/独立 extensions/持久 workflow 已完成 | R6 |

## 11. 检查与发布纪律

源码 PR 按范围先运行能捕获行为差异的定向测试，再完成仓库要求：

```sh
cargo nextest run --workspace --all-features
cargo clippy --workspace --all-features --tests -- -D warnings
cargo fmt -- --check
cargo test --doc --workspace --all-features
RUSTDOCFLAGS='-D warnings' cargo doc --workspace --all-features --no-deps
cargo run -p dist-helper -- check
git diff --check
```

无 nextest 时使用 `cargo test --workspace --all-features`。
改变 schema 时先执行 `cargo run -p dist-helper -- generate-schema` 并提交生成制品；
不改 registry 数据时不为了本批重建或改写 catalog。
SDK 改动还需相应 no-default-features / feature-isolation / public-API 检查，遵循
[CI](../.github/workflows/ci.yml) 的现有命令与工具版本，不把本地通过等同于全平台 CI。

CLI、默认 config 或 harness 接线变化在同一 PR 更新 [仓库 Skill](../skills/bitrouter/SKILL.md)
及相应 references，检查 `.claude-plugin/`、`.codex-plugin/` 和 marketplace 引用。
Skill 只描述已实现内容；不提前发布本设计的提议命令。产品文档在 bitrouter-docs
另行同步；此文件属于内部工程设计。提交与 PR 标题采用 conventional 格式。

## 12. 实施状态与证据

| 项目 | 本分支结果 |
| --- | --- |
| R1 归一化 | 已提交 `06d2a685`；保留 legacy preset 公开类型及请求默认值语义 |
| R2 配置和地址 | 已提交 `52a0ef2f`；严格 schema、新旧地址、Google 斜杠路径、稳定 router 身份 |
| R3 coding 初始化 | 已提交 `c12aa814`；复用 policy-lock，幂等和失败恢复；不改 chat/harness/mode |
| R4 诊断和历史 | 已提交 `2392afbe`，R6 补齐候选就绪检查；动态 preview 不执行选模；router 修改需要重启；历史字段可空 |
| R5 迁移 | 已提交 `55d14463`，R6 补齐相对路径；候选、摘要、备份、原子发布；不支持的布局要求人工迁移 |
| R6 集成回归 | 已实现；四协议 × 流式/非流式 × 新旧配置，fallback 和 Responses continuation |

具体验收入口：

- B01–B06：SDK `config::presets` / `config::router` / routing-table tests；
  `named_router_migration_protocol_matrix` 使用不同 base/selected 模型，验证真正经过 policy。
- B07–B08：`policy_lock::tests::router_*` 和 `actions::models` 的未初始化/缺少 policy
  诊断；初始化回归验证原 mode/chat 配置保留。原生 harness 启动方式未改动。
- B09–B12：`named_router_fallback_keeps_one_settled_identity`、
  `router_continuation` 集成测试、DB migration/metering tests、daemon status 和 reload tests。
  只新增本地 settlement 字段，不扩展遥测 attribute schema，也不声称覆盖提前拒绝。
- B13：`router_migration::tests` 和 `router_migration_cli`；候选被修改、源文件变化、
  幂等、私有备份、权限、CRLF、symlink 拒绝、语义等价检查。底层发布锁约束协作写入者；
  不宣称与不遵守锁的外部文件替换具有文件系统事务隔离。
- B14：`named_router_migration_protocol_matrix` 经真实本地 HTTP/mock upstream 验证四协议；
  `router_continuation` 验证第二次 Responses 请求携带对应提供商 continuation id。
- B15：生成 schema、CLI parsing、skill/references、模板同步；plugin manifests 只分发 skill，
  无需修改。SDK 新字段对使用 struct literal 的下游 Rust 调用者是源码变更，需要补字段
  或使用现有构造路径；不宣称 Rust struct-literal 源码兼容。
- B16：没有新 task store、checker、workflow runtime 或 extension 发布系统。

本地验证结果（2026-09-15，macOS arm64）：

| 检查 | 结果 |
| --- | --- |
| `cargo nextest run --workspace --all-features --no-fail-fast` | 3,381 passed；22 skipped；0 failed |
| `cargo clippy --workspace --all-features --tests -- -D warnings` | 通过 |
| `cargo fmt -- --check`、`git diff --check` | 通过 |
| `cargo test --doc --workspace --all-features` | 5 passed；1 ignored；0 failed |
| `RUSTDOCFLAGS='-D warnings' cargo doc --workspace --all-features --no-deps` | 通过 |
| `cargo run -p dist-helper -- check` | schema 与 registry 制品一致 |
| SDK `--no-default-features`，以及分别启用 `config_file` / `server` / `acp` | 全部通过 |
| telemetry 分别启用 `otel-http` / `otel-grpc` / `otel,server` | 全部通过 |
| pinned nightly `2026-05-05` + `cargo-public-api 0.52.0` | API listing 非空、sentinel 存在、无 OTel 泄漏、公开依赖集合不变（15 crates） |

所有 Cargo 验证使用 `CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=4` 控制本地构建资源。
编译仍提示既有 `proc-macro-error2` future-incompatibility 与 macOS linker unwind-table
大小警告；上述检查均成功退出。没有调用真实付费模型，也没有将 mock HTTP 通过
计作外部提供商或原生 harness 的发布认证。

验收中实际修正了迁移候选相对路径的父目录同步、policy 目标不可路由时的 readiness，
并补上异步 settlement 完成后读取历史的回归。策略选模、鉴权次序与原生 harness
执行位置保持既有契约；本批未实现的新架构边界见第 2 节。
