---
title: 构建插件
description: 基于 BitRouter 基于 trait 的 Rust SDK 编写插件 —— 实现一个 hook，将其打包为 Plugin，并安装到路由器中。
sourceHash: 5c4baeccdc37711fd6c7521c89d26b855671e21d2e7b552c436f0ade975c888e
---

# 构建插件

BitRouter 是一个面向 LLM API 流量的可编程路由器。任意受支持的传输协议上的入站请求会被规范化为
一条统一的处理管线（pipeline），经过一条有序的 **hook** 链处理，分发到上游提供商，再以入站协议
重新渲染返回。**插件（plugin）** 就是将一个或多个 hook（以及可选的数据库迁移）打包、并通过一次
调用安装到路由器中的单元。

本文所述内容均已对照 `bitrouter-sdk` crate 以及随
[核心仓库](https://github.com/bitrouterai/bitrouter) 发布的三个插件进行核实：
`bitrouter-guardrails`、`bitrouter-observe` 和 `bitrouter-attestation`。

<Callout type="info">
插件是一种 **编译期、Rust** 的扩展机制 —— 你需要基于 SDK 将其编译进路由器二进制。它没有动态插件
加载或脚本接口。如果你只是想配置提供商、路由或护栏规则，请改用
[`bitrouter.yaml`](/docs/guides/self-host) —— 你无需编写插件。
</Callout>

## 插件接入的管线

SDK 暴露三条相互独立的协议管线，每条都有 **自己** 的 hook trait（它们刻意不基于一个共享 trait
泛化，因此为某条管线设计的 stage 不会被注册到另一条上）：

- **`language_model`** —— 主 LLM 管线，拥有完整的 hook 集合。
- **`mcp`** —— Model Context Protocol 路由（纯路由，无结算）。
- **`acp`** —— Agent Client Protocol 路由（纯路由，无结算）。

大多数插件面向 `language_model`。一个请求按顺序流经它的各个 stage：

1. **Pre-request** —— 每个 `PreRequestHook` 运行；鉴权、策略、限流与上游护栏可提前拒绝请求。
2. **Route** —— 每个 `RouteHook` 都可改写有序的路由目标链。
3. **Execute** —— executor 调用第一个目标，遇到可重试失败时回退到下一个。流式响应会经过每个
   `StreamHook`。
4. **Settle** —— 每个 `SettlementRecorder` 针对不可变的结算上下文运行（计量、计费、收据）。
5. **Observe** —— `ObserveHook` 只读地观察每个阶段边界；它们永远不会影响请求。

> 来源：`crates/bitrouter-sdk/src/lib.rs`（"Anatomy of a request" 文档）以及
> `crates/bitrouter-sdk/src/language_model/hooks.rs`。

## hook trait

以下是 `language_model` 的 hook trait，定义于
`crates/bitrouter-sdk/src/language_model/hooks.rs`（`SettlementRecorder` 位于 `settlement.rs`）。
它们都是 `Send + Sync` 并使用 `async_trait`。

```rust
// 阶段 1 —— 鉴权、策略、限流、余额、护栏。第一个 Deny 即停止管线。
#[async_trait]
pub trait PreRequestHook: Send + Sync {
    async fn check(&self, ctx: &mut PipelineContext) -> Result<HookDecision>;
}

// 阶段 2 —— 解析 / 改写有序的路由链。
#[async_trait]
pub trait RouteHook: Send + Sync {
    async fn resolve(&self, chain: &mut Vec<RoutingTarget>, ctx: &mut PipelineContext) -> Result<()>;
}

// 阶段 3 —— 执行观测 + 回退控制。
#[async_trait]
pub trait ExecutionHook: Send + Sync {
    async fn on_success(&self, ctx: &PipelineContext, result: &ExecutionResult) -> Result<()>;
    async fn on_failure(&self, ctx: &PipelineContext, error: &BitrouterError) -> FallbackDecision;
}

// 流式 —— 拦截规范化的 stream part（改写 / 丢弃 / 中止）。
#[async_trait]
pub trait StreamHook: Send + Sync {
    fn interest(&self) -> StreamInterest;
    async fn on_part(&self, ctx: &mut StreamContext, part: StreamPart) -> Result<StreamAction>;
    async fn on_stream_end(&self, ctx: &mut StreamContext, outcome: &StreamOutcome) -> Result<()>;
}

// 在每个阶段边界进行只读观测。此处的错误/panic 永远不会影响请求。
#[async_trait]
pub trait ObserveHook: Send + Sync {
    async fn after_phase(&self, phase: Phase, ctx: &PipelineContext);
    async fn on_stream_part(&self, ctx: &StreamContext, part: &StreamPart);
    async fn on_request_end(&self, ctx: &PipelineContext, outcome: &RequestOutcome);
    // 另有默认空实现的 on_hop_start / on_hop_end / stream_interest
}
```

`PreRequestHook` 返回一个 `HookDecision` —— 要么 `Allow`，要么 `Deny(DenyReason)`，其中
`DenyReason` 映射到 HTTP 状态码（`Unauthorized` → 401，`Forbidden` → 403，
`PaymentRequired` → 402，`RateLimited` → 429，`GuardrailViolation` / `BadRequest`
→ 400，或 `Custom(status, message)`）。

## `Plugin` trait

插件是一个 **便捷封装** —— 它将一组相关的 hook 以及可选的 SQL 迁移打包，并通过一次调用安装。它
*不是* 原子单元：每个插件都可以通过逐个调用相应子构建器的 hook 方法来复现。该 trait 位于
`crates/bitrouter-sdk/src/app.rs`：

```rust
pub trait Plugin {
    /// 插件标识（用于配置映射与日志）。
    fn id(&self) -> &PluginId;

    /// 插件携带的数据库迁移。为空 = 无数据库。
    fn migrations(&self) -> Vec<MigrationItem> {
        Vec::new()
    }

    /// 将插件的 hook 安装到 builder。
    fn install(&self, app: &mut AppBuilder);
}
```

在 `install` 中，你通过 `app.language_model_builder()` 取得 `language_model` 子构建器，并用
`pre_request_hook(...)`、`route_hook(...)`、`execution_hook(...)`、`stream_hook(...)`、
`settlement_recorder(...)` 或 `observe_hook(...)` 注册 hook。hook 按注册顺序运行。

## 一个最小的带注释示例

仓库中最小的真实插件是 `bitrouter-attestation`：它注册了单个 `RouteHook`。下面是一个同样形态的最小
插件 —— 一个 `PreRequestHook`，当请求的 system prompt 含有被禁短语时拒绝该请求。它的结构（一个
`id`、一个注册单个 hook 的 `install`）与 `plugins/bitrouter-attestation/src/lib.rs` 和
`plugins/bitrouter-guardrails/src/plugin.rs` 完全一致。

```rust
use async_trait::async_trait;
use bitrouter_sdk::{AppBuilder, Plugin, PluginId, Result};
use bitrouter_sdk::language_model::{
    DenyReason, HookDecision, PipelineContext, PreRequestHook,
};

/// 一个 pre-request hook：当 system prompt 含被禁短语时拒绝请求。
struct BannedPhraseHook {
    phrase: String,
}

#[async_trait]
impl PreRequestHook for BannedPhraseHook {
    async fn check(&self, ctx: &mut PipelineContext) -> Result<HookDecision> {
        if let Some(system) = &ctx.prompt().system {
            if system.contains(&self.phrase) {
                return Ok(HookDecision::Deny(DenyReason::GuardrailViolation(
                    "request blocked by banned-phrase policy".into(),
                )));
            }
        }
        Ok(HookDecision::Allow)
    }
}

/// 插件：一个 id，注册一个 hook。无迁移，因此沿用 trait 的默认 `migrations()`。
pub struct BannedPhrasePlugin {
    id: PluginId,
    phrase: String,
}

impl BannedPhrasePlugin {
    pub fn new(phrase: impl Into<String>) -> Self {
        Self {
            id: PluginId::new("banned-phrase"),
            phrase: phrase.into(),
        }
    }
}

impl Plugin for BannedPhrasePlugin {
    fn id(&self) -> &PluginId {
        &self.id
    }

    fn install(&self, app: &mut AppBuilder) {
        app.language_model_builder()
            .pre_request_hook(BannedPhraseHook { phrase: self.phrase.clone() });
    }
}
```

`Cargo.toml` 依赖 SDK 与 `async-trait`（与已发布的 guardrails 插件相同的两个依赖）：

```toml
[dependencies]
bitrouter-sdk = "..."   # BitRouter SDK
async-trait = "0.1"
```

## 将插件安装到路由器中

插件通过 `AppBuilder::plugin` 安装，它会扩展迁移集合并调用你的 `install`。这与 SDK crate 文档中
展示的 `App::builder()` 流程相同：

```rust
use std::sync::Arc;
use bitrouter_sdk::App;
use bitrouter_sdk::language_model::{HttpExecutor, StaticRoutingTable};

let app = App::builder()
    .language_model(|lm| {
        lm.routing_table(Arc::new(StaticRoutingTable::new()))
          .executor(Arc::new(HttpExecutor::with_defaults()?));
    })
    .plugin(BannedPhrasePlugin::new("forbidden"))
    .build()?;
```

启用 SDK 的 `server` feature 后，`app.serve("127.0.0.1:4356")` 会接好 HTTP 路由器并运行直到收到
SIGTERM。

## 已发布插件如何使用这些 hook

仓库中的三个插件是权威的实战范例：

- **`bitrouter-guardrails`** —— 注册一个 `PreRequestHook`（`GuardrailPreHook`，对命中 `Block`
  规则的请求内容拒绝）和一个 `StreamHook`（`GuardrailStreamHook`，在响应流中对 `Redact` 命中做
  脱敏、对 `Block` 命中中止）。两者都从管线的类型化扩展中读取当前 `RuleSet`。见
  `plugins/bitrouter-guardrails/src/`。
- **`bitrouter-attestation`** —— 注册单个 `RouteHook`，针对每个机密路由目标查询 TEE 证明结论，
  并据此记录它或丢弃未验证的目标（fail-closed）。见 `plugins/bitrouter-attestation/src/lib.rs`。
- **`bitrouter-observe`** —— 一个 OpenTelemetry 导出器（OTLP traces + metrics），安装一个
  `ObserveHook`；同一句柄也作为 app 的 `metrics_renderer` 接好，以便 `GET /metrics` 提供服务。
  它通过传输 feature（`otel-http` / `otel-grpc`）做条件编译。见 `plugins/bitrouter-observe/src/`。

<Callout type="warn">
SDK 是公开、稳定的扩展面，但其支撑类型（`PipelineContext`、`StreamContext`、流式与结算类型）比本指南
展示的更丰富，且仍在演进。请将上面的 trait 签名视为契约，在依赖之前先阅读每个 trait 的 rustdoc 以
获取确切、最新的方法语义。凡有歧义之处 —— 保持你的 hook 尽量精简，而不要臆测。
</Callout>

## 什么应放进插件，什么应放进部署代码

SDK 只对管线数据的正确性有立场，不涉及业务逻辑。鉴权、策略、计费与计量是 **与部署相关** 的 ——
开源的 `bitrouter` 二进制提供了这些 trait 的自有实现，托管部署则编写各自的实现。可复用的横切行为
（护栏、可观测性、证明）才是作为插件发布的内容。

## 后续步骤

<Cards>
  <Card title="自托管 BitRouter" href="/docs/guides/self-host" description="在生产环境运行你编译插件进去的那个路由器。" />
  <Card title="护栏" href="/docs/features/guardrails" description="内容防火墙插件，作为实战参考。" />
  <Card title="OpenTelemetry" href="/docs/features/opentelemetry" description="基于 observe 插件的 OTLP trace + 指标导出。" />
  <Card title="核心仓库" href="https://github.com/bitrouterai/bitrouter" description="bitrouter-sdk crate 与三个参考插件。" />
</Cards>
