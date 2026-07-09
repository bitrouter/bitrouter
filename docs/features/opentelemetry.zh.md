---
title: OpenTelemetry
description: BitRouter 原生支持 OpenTelemetry——为每次请求生成 trace 与指标，通过 OTLP 导出到你运行的任意后端。本页的一切都是开源的，运行在你自己的基础设施上。
sourceHash: bdb0ade9182ea76246cb218b8b1d126497ab54b4ab6cd22ed66cdb606e6f8d1a
---

BitRouter **原生支持 OpenTelemetry**。你经路由器发送的每一次请求都会变成一条 **trace**——覆盖从入口、路由、每一次上游尝试（包括故障转移）到结算的完整生命周期——外加一组**指标**，全部遵循 [OpenTelemetry GenAI 语义约定](https://opentelemetry.io/docs/specs/semconv/gen-ai/)，并通过 OTLP 推送到你已经在运行的任意后端。

本页的一切都是**开源的**，完全运行在你自己的基础设施上——中间不存在任何 BitRouter 的遥测端点。它在**你为它指定目标之前都是关闭的**，且默认不导出消息内容。如果你不想运维一个 collector，[BitRouter Cloud](/docs/features/opentelemetry#cloud-activity-hosted) 提供一个无需运维的托管请求视图。

## 一条 trace 长什么样

每次请求产生一棵 span 树：

```
HTTP SERVER  POST /v1/chat/completions        (入口)
└─ chat      (INTERNAL, 入站 —— 整个请求生命周期)
   ├─ route  (INTERNAL —— 路由决策)
   ├─ chat   (CLIENT —— 上游尝试 #1，带 gen_ai.* 属性)
   ├─ chat   (CLIENT —— 故障转移尝试 #2)
   └─ settle (INTERNAL —— 结算汇总)
```

每次请求只有**一个 GenAI generation**——即入站的 `chat` span。每一次上游尝试都是
独立的 `CLIENT` span，因此一条故障转移链会按顺序展示它尝试过的每个 provider，以及
每一跳的延迟与结果。BitRouter 会提取入站的 W3C trace context 并注入出站的
`traceparent`，让路由器的 span 接入来自你的 agent 或网关的父 trace。由于每一次尝试
都是独立的 span，trace 正是故障转移与路由行为变得清晰可读的地方：先尝试了哪个
provider、为何回退、以及整条链上延迟去了哪里。

### span 属性

| 属性 | 说明 |
| --- | --- |
| `gen_ai.provider.name` | 该跳的上游 provider（如 `openai`、`anthropic`） |
| `gen_ai.response.model` | 实际服务该响应的模型 |
| `gen_ai.token.type` | `input` / `output`，标注在 token 度量上 |
| `outcome` | 请求的最终处置 |
| `api_key_id`、`user_id` | 调用方归因（受基数上限约束） |
| `account_label` | 逻辑账户/租户标签 |

## 开启导出

在 `bitrouter-observe` 插件下加一个 `otel` 块并给它一个端点，仅此即可开启导出：

```yaml
plugins:
  bitrouter-observe:
    otel:
      endpoint: "http://localhost:4318"   # 你的 OTLP 端点
      service_name: "bitrouter"
```

把密钥排除在提交的文件之外——任何鉴权 header 都用 `${VAR}` 引用，加载时从环境解析：

```yaml
plugins:
  bitrouter-observe:
    otel:
      endpoint: "https://api.honeycomb.io"
      headers:
        x-honeycomb-team: "${HONEYCOMB_API_KEY}"
```

每个字段都有对应的环境变量覆盖项，因此你无需修改文件即可配置导出——在容器中尤其方
便，你完全可以不写 `otel` 块：

| 环境变量 | 设置 |
| --- | --- |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | OTLP 端点 URL |
| `OTEL_EXPORTER_OTLP_HEADERS` | 鉴权 header（逗号分隔的 `k=v`） |
| `OTEL_SERVICE_NAME` | 资源 service 名称 |
| `OTEL_RESOURCE_ATTRIBUTES` | 额外资源属性（逗号分隔的 `k=v`） |
| `OTEL_TRACES_SAMPLER` | 采样器类型 |
| `OTEL_TRACES_SAMPLER_ARG` | 采样器参数（如比率） |
| `BITROUTER_OBSERVE_CONTENT_CAPTURE` | 内容捕获模式 |

<Callout type="info">
OTLP **传输层**在编译二进制时选择：`otel-http`（OTLP/HTTP + protobuf，默认）或
`otel-grpc`（OTLP/gRPC）。本页的配置对两者完全一致——只有当你的后端只支持其中一种
时才需要关心传输层。
</Callout>

## 指标

除 trace 外，指标也按间隔（默认 60 秒）通过 OTLP 导出：

| 指标 | 类型 | 度量 |
| --- | --- | --- |
| `bitrouter.requests` | Counter | 已处理请求数 |
| `gen_ai.client.operation.duration` | Histogram | 请求延迟 |
| `gen_ai.client.token.usage` | Histogram | token 计数（按 `gen_ai.token.type`） |
| `bitrouter.errors` | Counter | 错误数 |
| `bitrouter.stream_parts` | Counter | 发出的流式分片数 |

维度包括 `gen_ai.provider.name`、`gen_ai.response.model`、`outcome`、
`account_label` 以及调用方标识。为在共享部署上控制指标基数，`api_key_id` 与
`user_id` 设有上限（默认 1024 与 256 个不同值）；超过上限后，值会归入一个溢出桶。

<Callout type="warn">
**没有 Prometheus 抓取端点**。`GET /metrics` 已退役——指标仅通过 OTLP 推送。如果你
的技术栈基于 Prometheus，请通过带 Prometheus exporter 的 OpenTelemetry Collector
接入。
</Callout>

## 各后端配置

下面每个块都是某个常见后端的 `plugins.bitrouter-observe.otel` 配置。

### OpenTelemetry Collector

把所有数据发往本地或集群内的 Collector，再由它分发到你真正的后端（这也是接入基于
Prometheus 的技术栈的途径——Collector 的 Prometheus exporter 弥补了 BitRouter 没有
抓取端点这一点）：

```yaml
otel:
  endpoint: "http://otel-collector:4318"
  service_name: "bitrouter"
  resource_attributes:
    deployment.environment: "prod"
```

### Honeycomb

```yaml
otel:
  endpoint: "https://api.honeycomb.io"
  service_name: "bitrouter"
  headers:
    x-honeycomb-team: "${HONEYCOMB_API_KEY}"
```

### Grafana Cloud / Tempo

Grafana Cloud 的 OTLP 网关使用 basic auth（实例 ID + API token，base64 编码）。自托
管的 Tempo 则指向它的 OTLP 端口并去掉 header。

```yaml
otel:
  endpoint: "https://otlp-gateway-<region>.grafana.net/otlp"
  service_name: "bitrouter"
  headers:
    Authorization: "Basic ${GRAFANA_OTLP_TOKEN}"
```

### Datadog

Datadog 通过 Datadog Agent 而非公开 OTLP URL 接收 OTLP——以启用 OTLP 接收的方式运行
Agent，并把 BitRouter 指向它：

```yaml
otel:
  endpoint: "http://datadog-agent:4318"
  service_name: "bitrouter"
  resource_attributes:
    deployment.environment: "prod"
```

## 调节采样

默认情况下 BitRouter 遵循入站的 trace 决策，否则采样全部（`parentbased_always_on`）。
在高吞吐下，改为采样一部分：

```yaml
otel:
  endpoint: "http://otel-collector:4318"
  sampler: "parentbased_traceidratio"
  sampler_arg: 0.1                       # 保留 10% 的根 trace
```

| `sampler` | 行为 |
| --- | --- |
| `always_on` | 采样每一条 trace |
| `always_off` | 全部不采样 |
| `traceidratio` | 按比例（`sampler_arg`）采样，忽略父级 |
| `parentbased_always_on` | 跟随父级；无父级时采样 *（默认）* |
| `parentbased_always_off` | 跟随父级；无父级时丢弃 |
| `parentbased_traceidratio` | 跟随父级；否则按 `sampler_arg` 采样 |

`parentbased_*` 变体会尊重上游的决策，因此你的 agent 发起的 trace 不会在路由器处被
“半采样”。指标导出间隔与 trace 批处理队列可分别在 `metrics` 与 `traces.batch` 下调节，
以在新鲜度与开销之间权衡。

## 内容捕获

prompt 与响应**内容默认不导出**（`content_capture: off`）。仅当你需要在 span 上看到
prompt 与响应正文用于调试时才开启：

```yaml
otel:
  content_capture: "full"   # off（默认） | full
```

<Callout type="warn">
`full` 会把用户 prompt 与模型响应写入你的遥测后端，这些内容随后将继承该后端的访问
控制与保留策略。在共享或受监管的环境中，请保持 `off`，仅在受限、短时的调试会话中捕获
内容。
</Callout>

## 验证

reload（或重启）路由器，然后询问运行中的守护进程它在做什么：

```bash
bitrouter reload                  # 不中断连接地应用配置变更
bitrouter observe status          # 端点、采样器、基数、在途 span
bitrouter observe status --json
```

如果它报告 `stopped`，说明导出器未接好——检查 `otel` 块是否有 `endpoint`（或是否设置
了 `OTEL_EXPORTER_OTLP_ENDPOINT`），以及二进制是否带某个 OTLP 传输特性编译。然后通过
路由器发一个请求，确认 trace 进入你的后端；你应当看到每次请求有一个入站 `chat` span，
每次上游尝试有一个 `CLIENT` 子 span。

## 下一步

<Cards>
  <Card title="云端追踪" href="/docs/features/opentelemetry#cloud-activity-hosted" description="托管的请求视图——消费、token 与逐请求日志，无需运维。" />
  <Card title="自托管 BitRouter" href="/docs/guides/self-host" description="在生产环境运行路由器，并接好遥测。" />
  <Card title="模型故障转移" href="/docs/features/model-fallback" description="你在每条 trace 里都会看到的故障转移链。" />
  <Card title="Guardrails" href="/docs/features/guardrails" description="请求与响应的内容防火墙。" />
</Cards>

## Cloud Activity（托管）

开源的 [OpenTelemetry](/docs/features/opentelemetry) 导出运行在你自己的后端上。**BitRouter Cloud** 提供托管的替代方案：每一次 `/v1` 请求都会在服务端被追踪进一个 **Activity** 视图——无 collector、无数据仓库、无需运维。内容（prompt 与响应）从不存储。

### Activity 仪表盘

登录 [cloud.bitrouter.ai](https://cloud.bitrouter.ai) 并打开 **Activity**。它以三张 KPI 卡片开场，覆盖你选择的时间窗口——**1 天**、**1 周**、**1 个月**或**全部时间**：

| KPI | 度量 |
| --- | --- |
| **消费（Spend）** | 窗口内累计扣费（USD） |
| **请求（Requests）** | 窗口内请求数 |
| **Token** | 窗口内 prompt + completion token 数 |

每个数字都作用于**当前工作区**（[namespace](/docs/features/namespaces)），因此仪表盘始终反映你登录所在的工作区。

### 请求日志

KPI 下方，请求日志按时间倒序列出每一次 `/v1` 请求。每一行都是一条逐请求的追踪记录：

| 列 | 详情 |
| --- | --- |
| **时间** | 请求到达的时刻 |
| **模型** | 服务该请求的模型 id，流式调用带 `stream` 标记 |
| **Provider** | 服务它的上游 provider |
| **Token** | prompt + completion 合计 |
| **成本** | 最终扣费（USD） |
| **延迟** | 端到端延迟 |
| **来源** | 资金来源（信用余额、BYOK、MPP 会话） |
| **状态** | 成功、错误、被拒、已取消 |

每条记录还带有所用的**路由档位**（`balanced`、`cost`、`latency`、`throughput`）以及触发的受限**能力**（如 `structured_outputs`）——因此一次发生了故障转移或触及预算的请求，无需离开仪表盘即可看清。

<Callout type="info">
**只存回执，不存正文。** Cloud 存储请求的*记录*——模型、provider、token、成本、延迟、状态、路由档位——从不存储 prompt 或响应内容。
</Callout>

### 用量归因与 API

仪表盘中的一切也都可通过管理 API 获取，按工作区作用域，并由 `usage:read` scope 控制：

- **聚合用量**——某个 `[from, to)` 窗口内的消费、token 计数、请求数，以及按能力的细分。
- **请求历史**——分页的请求日志，含路由档位与所用能力。

它们与你从 [CLI](/docs/concepts/cli) 运行的 `bitrouter cloud usage` 和 `bitrouter cloud requests` 是同一套。`usage` 与 `requests` 端点及其字段见 [API 参考](/docs/reference)。

### 深度 trace

Cloud 存储的是逐请求的**回执**，而非 OpenTelemetry span 瀑布图。当你需要完整的 span 树——入口 span、路由决策、以及每次上游尝试的 `CLIENT` span——它存在于**你自己的 OTLP collector** 中。用开源的 [OpenTelemetry](/docs/features/opentelemetry) 导出接好一次，Activity 视图便会链接到它。

### 下一步

<Cards>
  <Card title="OpenTelemetry" href="/docs/features/opentelemetry" description="自托管 OTLP 导出——span 模型、指标与各后端配置。" />
  <Card title="工作区" href="/docs/features/namespaces" description="按工作区作用域管理密钥、用量与策略。" />
  <Card title="CLI" href="/docs/concepts/cli" description="在终端运行 bitrouter cloud usage / requests。" />
  <Card title="API 参考" href="/docs/reference" description="usage 与 requests 管理端点。" />
</Cards>
