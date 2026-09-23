# Host-oriented Extension 开发体验 Spec

状态：**v0.3 — S1–S4 已实现；本地验证范围见验收记录，尚未发布。** 2026-09-19。

目标：扩展作者实现具体能力，自定义宿主复用 BitRouter 的完整服务生命周期。
普通用户选择 router 后，仍主要将模型选择交给 router。

本文基于当前工作区的 compile-only extension 实现与已完成的内部收敛，
补充 [Router & Extension Spec](ROUTER_EXTENSION_SPEC.md) 和
[Request Checks Spec](REQUEST_CHECKS_SPEC.md)。本版记录已批准的启动接口、类型迁移和
注册规则；运行验证事实见 [验收记录](GUARDRAILS_EXTENSION_ACCEPTANCE.md)。

## 1. 设计依据与取舍

Zed 的语言扩展声明适用语言，并提供启动命令、配置等具体能力；宿主将其接入
自己的语言服务系统。这体现了明确的功能入口和宿主职责。
参见 [Language Extensions](https://zed.dev/docs/extensions/languages)。

Zed 作者 API 使用一个包含多类默认方法的 `Extension` trait；宿主侧通过
按功能划分的代理接入各子系统。本文借鉴这种职责划分，保留 BitRouter 已有的
类型化注册函数，不复制大 trait 或代理框架。
参见 [作者 API](https://github.com/zed-industries/zed/blob/main/crates/extension_api/src/extension_api.rs)
和 [宿主代理](https://github.com/zed-industries/zed/blob/main/crates/extension/src/extension_host_proxy.rs)。

Zed 还支持无需 Rust 代码的资源型扩展；编写新实现与配置已有能力是不同工作。
BitRouter 同样应让使用已编译能力的人通过 router 配置操作，而不要求理解 Rust pipeline。
参见 [Developing Extensions](https://zed.dev/docs/extensions/developing-extensions)。

这里的 capability 指 request check 等**功能契约**。Zed 文档中的
[Extension Capabilities](https://zed.dev/docs/extensions/capabilities)
则描述执行命令、下载等**操作权限**。本提案不引入权限 manifest；同进程 Rust
接口限制耦合，不提供安全沙箱。上述架构归纳是本提案的分析，不是 Zed 的官方命名。

## 2. 实施前后

| 项目 | 实施前 | S1–S3 变化 |
| --- | --- | --- |
| 作者入口 | `ExtensionApi::request_check(id, revision, callback)` | 保留 |
| 业务契约 | `Input → Decision`，host runner 附加 revision | 保留 |
| 执行诊断 | pipeline 执行 fail-closed；专用回执与查询面已移除 | 使用有界 tracing 和通用 daemon 状态 |
| 自定义宿主装配 | `assemble::build_app_with_extensions` | 继续作为低层装配接口 |
| 完整服务启动 | 编排主要在 CLI 私有 `serve()` 中 | 提取到 app crate 的共享前台宿主入口 |
| regex 示例 | 手动启动 HTTP gateway，没有 daemon 管理 socket | 改用共享宿主启动路径 |
| 作者输入类型 | 部分 fragment/coverage 类型位于 `language_model::request_checks` | 迁入能力所属的作者模块 |
| 多余注册 | 配置未声明的注册阻断启动 | 允许保持未激活，提供启动诊断 |
| 已声明但未绑定 | 必须有匹配注册；无请求执行 | 保持 |

当前源码锚点：

- [ExtensionApi](../crates/bitrouter-sdk/src/extension/mod.rs)
- [业务输入和判定](../crates/bitrouter-sdk/src/extension/request_check.rs)
- [投影和 host runner](../crates/bitrouter-sdk/src/language_model/request_checks.rs)
- [共享装配](../apps/bitrouter/src/assemble.rs)
- [共享服务编排](../apps/bitrouter/src/host.rs)
- [CLI 入口](../apps/bitrouter/src/main.rs)
- [运行绑定](../apps/bitrouter/src/request_checks.rs)
- [当前最小示例](../apps/bitrouter/examples/native_regex_checker.rs)

## 3. 作者、宿主与 router 的边界

| 角色 | 负责 | 不需要掌握 |
| --- | --- | --- |
| Router 使用者 | 选择 router，配置可用能力，检查启动和失败诊断 | Rust callback、runner、并发执行 |
| Extension 作者 | 实现能力函数，注册实例，声明代码/规则 revision | listener、control socket、reload、请求生命周期 |
| 自定义宿主维护者 | 选择链接的 crate，加载规则，调用注册函数与共享宿主入口 | 复制官方 daemon 编排 |
| SDK / 产品宿主实现 | 输入投影、调用顺序、限制、身份、诊断和生命周期 | 具体 matcher 的业务规则 |

这些是职责，不新增 `Author`、`HostPlugin` 或 `CapabilityManager` 等运行时对象。
一个人可以同时承担作者和宿主维护者角色。

术语保持具体：`request_check` 是能力；regex checker 是实现；PII 检查是某个实现和
规则集形成的用途；`checkers.<id>` 是部署中声明的实例；router 决定使用哪些实例。
一个 extension 可以注册多个实例，不要求额外的 package ID 或 namespace 配置。

Legacy `Plugin::install(&mut AppBuilder)` 仍属于高级宿主装配接口，保留其 hooks、
migrations、stream/output 语义。它不是普通 extension 的入门路径；本轮不做机械改名。
请求内 Context extensions 保持现有含义。

## 4. 共享前台宿主入口

### 4.1 接口形态

在 `apps/bitrouter` 的 library surface 中提供共享启动函数
`bitrouter::host::serve_with_extensions`。
不新增 host crate 或通用生命周期 builder。

以下为接口使用示意；完整可运行示例见 `apps/bitrouter/examples/native_regex_checker.rs`：

```rust,ignore
let source = bitrouter::paths::resolve_config(config_path.as_deref())?;
let rules = load_company_rules(rules_path)?;

bitrouter::host::serve_with_extensions(&source, move |api| {
    register_company_checks(api, rules)
}).await?;
```

函数接收已有 `ConfigSource` 和一次性的注册闭包，完成前台服务的运行与关闭，返回
已有错误类型体系下的结果。宿主维护者负责解析自己的少量启动参数、加载业务规则；
配置解析与默认值、运行基线、服务 listener 和管理编排由共享入口处理。
官方 `bro serve` 调用同一入口并传入空注册闭包，默认构建仍不依赖 regex matcher。

### 4.2 必须复用的产品行为

共享入口沿用当前 `serve()` 的行为和组件，不重新实现一套等价服务：

1. 配置来源、home 与相对路径规则、现存 daemon 检查，以及运行配置基线。
2. 既有 provider/registry 准备，注册校验、app 装配、数据库和已有后台任务。
3. HTTP 模型服务、本地 control socket、配置启用的 remote control。
4. 既有认证、通用管理 action 和 reload coordinator。
5. 信号与管理 stop、请求收尾、既有后台任务关闭、telemetry flush 和进程资源清理。

HTTP pipeline 必须持有唯一的 request-check runtime；共享启动不能创建第二份
检查实例或运行配置事实。local/remote 管理仍由目标 daemon 提供通用状态，
不读取客户端文件来补充其运行状态。

这是产品 daemon 的入口：首版每个进程运行一个宿主，保留当前进程级 CWD、信号和
tracing 所有权。需要自定义 listener 或嵌入其他运行时的调用者继续使用低层装配接口。
不承诺在同一进程内运行多个独立 daemon。
共享入口不新增强制终止同步 callback 的能力或新的关闭时限；停止接受请求和
后台 callback 真正结束仍是不同事实，保留现有 permit 和关闭等待语义。

### 4.3 启动、就绪与失败

注册函数在每次进程启动时调用一次，不在配置查询、请求执行或 reload 时重复调用。
注册/声明/revision 校验必须在数据库装配和 listener 启动前完成；配置读取、home 准备、
既有 registry 准备以及作者自己加载规则可能先发生，不宣称启动完全没有副作用。

所有要求启用的服务均成功启动后才能宣称就绪。后续启动失败必须关闭已启动的服务，
并清理本次启动持有的 socket、locator、PID 等资源，不能删除其他进程的资源。
数据库迁移等已有持久化副作用不承诺事务回滚。提取时若发现当前路径不满足这一约束，
需单独记录并验证修复，不能仅凭复用了函数就声称通过验收。

### 4.4 后台启动与重启的边界

首批交付完整**前台运行及管理**路径，不复制整套官方 CLI。
现有 detached start 通过 `current_exe` 执行 `serve` 子命令；任意自定义二进制未必支持
这个 CLI，因此共享函数本身不代表其已支持后台 `start/restart`。

自定义宿主必须用自己的二进制重新启动，可由操作者或已有 service manager 管理。
文档不得引导用户用官方 `bro restart` 替换自定义宿主，避免重启后丢失编译注册。
首批示例展示前台运行、指定目标的管理查询和 stop；后台 launcher 统一是后续独立需求。

## 5. 作者 API 按能力组织

作者从 `bitrouter_sdk::extension::ExtensionApi` 注册能力；request-check 作者需要的
完整业务类型归入 `bitrouter_sdk::extension::request_check`。

| 类型或逻辑 | 归属 |
| --- | --- |
| `Input`、`Decision`、`Callback` | 保持在 `extension::request_check` |
| `ContentRole`、`ContentFragmentKind`、`ContentFragment` | 从 language_model 移入同一作者模块 |
| `RequestCheckCoverageScope`、`RequestCheckCoverageStatus`、`RequestCheckCoverage` | 移入同一作者模块，保留现有名称和序列化 |
| 从 Prompt/Role 投影、计数与边界校验 | 保留在 language_model 执行层 |
| `RequestCheckBinding`、`CheckerResult`、`CheckerFailure`、`RequestCheckerRunner` | 保持宿主执行契约，不作为作者入门接口 |
| 有界 tracing、deadline 和并发准入 | 保持宿主执行层 |

通过移动定义和调整 imports 改变归属，不用 public re-export 制造两个公开入口。
普通 callback 的签名不变；直接引用旧 fragment/coverage 路径的作者、测试和宿主需要
修改 imports。这属于 beta Rust 源码兼容变化。配置字段保持不变；专用管理 JSON 已移除。

业务模块不依赖完整 `PipelineContext` 或模型 Prompt；投影层消费业务类型。
作者参考文档以注册、Input、Decision、失败与限制为主，宿主装配和执行接口单独说明。
本轮不增加通用 `Capability` trait、字符串事件总线、JSON callback、反射 registry，
也不预留尚无实现的 selection/eval 方法。

## 6. 注册与激活分离

### 6.1 激活规则

| 启动情况 | 目标行为 | 相对当前实现 |
| --- | --- | --- |
| 合法注册，但 `checkers` 未声明 | 允许，保持未激活 | 改变：当前失败 |
| 配置声明实例，但没有注册 | 启动失败，即使尚无 router 引用 | 不变 |
| 配置 revision 与注册不匹配 | 启动失败，即使尚无 router 引用 | 不变 |
| 配置声明且匹配，但无 router 绑定 | 启动成功，bindings 为空，无实际使用证据 | 不变 |
| router 引用未声明实例 | 配置校验失败 | 不变 |
| 重复 ID、非法 ID/revision、被忽略的注册错误 | 整个注册集合无效，启动失败 | 不变 |
| 同一实例被多个 router 或重复绑定引用 | 沿用各自固定绑定、限制与执行顺序 | 不变 |

注册集合是代码提供的可用实现；配置声明和 router 绑定决定实际使用。
“允许多余注册”不放宽配置引用和 revision 校验，不允许用猜测 ID 或默认实现补齐缺失。

未声明的合法注册不创建执行 semaphore 或绑定，也不调用业务 callback。
注册闭包本身仍会运行，可能已经加载或编译规则；因此不承诺未激活实现完全没有初始化
成本，也不增加 lazy factory 来解决尚未出现的需求。

### 6.2 诊断与配置

通过既有启动诊断机制报告排序后的未激活注册 ID，供维护者发现漏配或拼写错误。
首批不增加“available/configured/enabled/installed”等多套状态字段或新查询命令。
专用 checker inventory、probe 与 request-check receipt 均不保留；配置声明但未绑定
只能解释为能力可用，不能解释为检查已执行。

继续使用 `checkers.<id>.native.revision` 和 `routers.<id>.checks.request`。
不新增顶层 `extensions:`，不复制一份 manifest。声明或绑定变化仍要求 restart，
新编译代码不能通过 reload 激活。revision 由宿主作者维护，不能从配置回读后盲目冒充
实际实现版本；它也不是二进制或外部规则内容的密码学证明。

这一变化便利“一份自定义二进制、多种部署配置”，但降低了多余注册的严格报错程度。
排序启动诊断是对该取舍的补偿，不能完全代替用户审阅配置。

## 7. 必须保持的四类保证

| 保证 | 本轮必须保留的机制 |
| --- | --- |
| 保存成功不等于运行生效 | 启动基线、saved/running/restart 状态及既有 reload 证据 |
| 注册成功不等于实际检查 | 只有 router binding 和真实请求才会调用 callback |
| 检查允许不等于模型成功 | allow 后仍可能在路由、上游或交付阶段失败 |
| 超时不等于 callback 停止 | blocking 工作持有 permit，超时/取消只停止等待 |

继续保留取消后的 permit 所有权和 fail-closed。认证失败及未形成 named-router
binding 的入口不会调用 callback。本轮不统一 checked/unchecked hook 顺序，
不缩小 reload 范围，不新增选模算法。

## 8. 实施顺序与验收

### S1 — 共享宿主启动

提取并复用现有前台 `serve` 路径，官方入口传空注册闭包，自定义示例传真实注册闭包。
低层装配继续保留；首批可调用的能力仍只有 request check。

验收：用 Rust 集成测试启动真实自定义宿主进程，使用临时 home、配置、数据库、端口和
本地 mock provider。通过真实 HTTP 及管理端点确认：

- H01：allow 产生预期上游调用；deny/timeout/非法判定零上游调用，均覆盖 stream/nonstream。
- H02：注册本身不执行 callback；未绑定注册保持未激活。
- H03：通用本地及远端管理沿用现有认证和访问限制；旧 checker 路由返回 404。
- H04：文件修改显示 saved 与运行差异；checker 修改 reload 被拒绝，进程内实现不变。
- H05：注册/配置/revision 错误先于数据库装配和服务就绪；端口冲突或部分启动失败能清理自身资源。
- H06：stop/信号沿同一路径关闭；显式再次启动相同自定义二进制，注册仍在。
- H07：官方空注册宿主保持原有服务行为，默认依赖树不引入 matcher。

实施涉及启动说明时，同步更新 `skills/bitrouter/` 及分发清单中相关描述。
不为本提案新增 Python 测试脚本；也不把本地 mock 验收称作真实模型或远端 CI 验收。

### S2 — 作者类型归位

移动第 5 节列出的定义与 imports，保持业务和管理序列化不变。

- A01：regex checker 的业务实现和输入构造测试只需作者模块中的 request-check 类型。
- A02：SDK no-default-features、config_file 组合及 matcher 最小构建通过。
- A03：覆盖范围、deny reason、身份冻结和 fail-closed 回归通过；迁移说明列出旧/新路径。

### S3 — 允许未声明注册保持未激活

移除“剩余合法注册阻断启动”的规则，添加启动诊断；不增加第二套 inventory。

- R01：同一二进制注册 A/B，仅配置 A 时可启动；B callback 调用数为零。
- R02：B 即使未使用，非法注册或重复注册仍阻断启动；配置声明 B 后仍要求正确 revision。
- R03：声明但未绑定的实例、缺失引用、多 router/重复绑定及实际使用状态保持原有行为。
- R04：未激活 ID 诊断稳定可读；不新增 checker inventory。

### S4 — 移除旧 HTTP checker 管理面

保留 native 配置、router binding、作者 API、共享宿主和 regex 实现；删除专用 receipt
store、progress reporter、checker inventory、local/remote control 路由和 `bro checks`。
执行诊断改为有界 tracing，不记录 prompt、匹配文本、规则名或 callback 原始错误。

- M01：旧 HTTP 配置字段继续显式失败，合法 native declaration 继续生效。
- M02：旧 local/remote checker 管理动作不可达，HTTP 路由返回 404。
- M03：stream/nonstream 的 allow/deny/timeout/invalid 保持 fail-closed 和零绕过。
- M04：custom host 的 stop、restart-required、重启和 SIGTERM 生命周期保持。

每个切片单独可审阅、可验证，不等到四项全部完成才检查默认宿主回归。
实现提交需完成仓库要求的 all-feature tests、clippy、fmt；公共 API 迁移同时检查
doctests、严格 rustdoc 与上述 feature 组合。验收结果写入
[验收记录](GUARDRAILS_EXTENSION_ACCEPTANCE.md)，本文件不预先标记通过。

## 9. 后续方向与本轮边界

Selection 与 evaluation 未来各自定义输入、结果和调用时机：selection 可返回候选选择，
由宿主验证并执行；evaluation 可返回评分，是否更新策略由宿主决定。它们不共享万能
Decision，也不把现有 `PolicyRuntime` 整体替换成一个 callback。本轮不实现这些入口。

本轮已确认的边界：

1. 先交付完整前台宿主复用，后台 launcher 另行设计。
2. fragment/coverage 的 beta Rust import 路径变更，不保留重复公开入口。
3. 未声明合法注册从启动错误变为未激活诊断，同时保留严格配置引用校验。

接口 review 与运行验证分开记录；各验收项的实际覆盖见验收记录，不以文档状态代替测试证据。
