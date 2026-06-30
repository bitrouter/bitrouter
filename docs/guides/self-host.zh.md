---
title: 自托管 BitRouter
description: 在自有基础设施上运行自托管 BitRouter 的生产实践 —— 配置文件、密钥、守护进程生命周期、指标与加固。
sourceHash: 8c7accdceb945d6dfd258bdc9d2e9117263e3bc41481462feb3e65831502810b
---

# 自托管 BitRouter

这是在你自有基础设施上运行 BitRouter 的 **生产** 路径：一个提交进仓库的配置文件、真实的提供商密钥、
作为受管守护进程运行的路由器、指标导出，以及基本加固。如果你只想在 60 秒内把它跑起来，请从
[安装](/docs/get-started/installation) 开始 —— 本指南承接其后。在自托管与托管产品之间做选择？
参见 [自托管 vs 云](/docs/get-started/self-hosted-vs-cloud)。

路由器默认监听 `127.0.0.1:4356` —— 仅回环，除非你明确选择对外开放。

## 1. 生成并组织 `bitrouter.yaml`

生成一个带注释的初始文件：

```bash
bitrouter init                 # 写入 ./bitrouter.yaml
bitrouter init -c /etc/bitrouter/bitrouter.yaml
```

`bitrouter init` 会写入一个带 `skip_auth: true` 的初始配置。编辑它以配置提供商、路由等内容。把
`bitrouter.yaml` 当作基础设施即代码：提交它、评审变更、并将机密信息排除在外（使用 `${VAR}` 引用，
在加载时从环境解析）。

配置由若干顶层小节构成 —— 你最先会接触的是 `server`、`providers` 和 `models`：

```yaml
# yaml-language-server: $schema=https://bitrouter.dev/schema/v<VERSION>/config.schema.json

server:
  # 默认回环；只有当你确实打算在所有网卡上暴露路由器时才设为 0.0.0.0。
  listen: 127.0.0.1:4356
  log_level: info

providers:
  openai:
    api_base: https://api.openai.com/v1
    api_key: ${OPENAI_API_KEY}
    models:
      - id: gpt-4o

  anthropic:
    api_base: https://api.anthropic.com
    api_key: ${ANTHROPIC_API_KEY}
    # `api_protocol` 是一个 glob 前缀模式列表：每组的头部即首选出站协议。
    api_protocol:
      - "*": messages
    models:
      - id: claude-sonnet-4-6

# 一个虚拟模型，按声明顺序从一个提供商回退到另一个（默认 `priority` 策略）。
models:
  smart:
    strategy: priority
    endpoints:
      - provider: anthropic
        service_id: claude-sonnet-4-6
      - provider: openai
        service_id: gpt-4o
```

**值得注意的结构：**

- `providers` 是一个 **以提供商 id 为键的映射**（`openai`、`anthropic`……）。每个条目接受
  `api_base`（上游基础 URL）、`api_key`（通常是 `${VAR}` 引用）、可选的 `api_protocol` 模式列表，
  以及一个 `models` 列表，其每个条目都必须有 `id`。
- `api_protocol` 按提供商选择出站传输协议 —— 例如 Anthropic 用 `messages`。已知取值包括
  `chat_completions`、`messages`、`generate_content` 和 `responses`。
- `models` 声明 **虚拟模型**：带 `strategy`（默认 `priority`）的命名别名，以及一个有序的
  `endpoints` 列表，每项指向一个 `provider` + `service_id`。这就是实现回退的方式。
- `server` 承载 `listen`、`log_level`、可选的 `skip_auth` 以及可选的 `control_socket` 路径。

<Callout type="info">
完整的顶层小节集合为 `server`、`providers`、`models`、`presets`、`variants`、`mcp`、
`mcp_servers`、`agents`、`server_tools`、`database`、`plugins` 和 `inherit_defaults`。权威 schema
位于核心仓库的 `schemas/bitrouter.config.schema.json`，而 `# yaml-language-server` 头部会为编辑器
提供自动补全与内联校验。
</Callout>

发布前先校验：

```bash
bitrouter config validate -c bitrouter.yaml
```

## 2. 环境与密钥

机密信息保留在环境中，绝不写入提交的文件。`bitrouter.yaml` 中的 `${VAR}` 占位符在加载时解析：

```bash
export OPENAI_API_KEY=sk-...
export ANTHROPIC_API_KEY=sk-ant-...
```

在生产环境，请通过进程管理器的环境（systemd 的 `EnvironmentFile`、机密挂载等）下发这些变量，而非
shell 配置文件。轮换密钥时无需重启 —— `bitrouter reload` 会把当前环境中存在的提供商 API 密钥转发
给正在运行的守护进程，因此 `export OPENAI_API_KEY=…; bitrouter reload` 会立即生效。

<Callout type="warn">
`bitrouter init` 写入 `skip_auth: true`，会放行无凭据的请求。这对回环之后的本地开发没问题，但对于
任何可被 localhost 以外访问的生产部署，你必须在路由器之前加上鉴权（反向代理 / 网关，或实现自有鉴权
hook 的部署）。不要在没有鉴权层的情况下，将 `skip_auth: true` 的路由器暴露在 `0.0.0.0` 上。
</Callout>

## 3. 作为守护进程运行

生产环境中，你希望路由器以脱离终端、受监管的方式运行。守护进程生命周期命令（已对照
[CLI 参考](/docs/concepts/cli) 核实）：

```bash
bitrouter start        # 将 `serve` 作为脱离终端的后台守护进程启动
bitrouter status       # pid、监听地址、可路由模型数、socket 路径
bitrouter reload       # 热重载配置 + 路由表（亦可经 SIGHUP 触发）
bitrouter restart      # 排空在途请求（最多 30 秒），再全新启动
bitrouter stop         # 停止守护进程
```

- `bitrouter serve` 在 **前台** 运行服务（日志输出到 stdout）—— 这正是你让 systemd 单元或容器
  entrypoint 指向的命令。
- `bitrouter start` 启动一个 **脱离终端** 的守护进程，若已有一个在运行则拒绝启动。日志默认写入配置
  文件旁的 `bitrouter.log`。
- `bitrouter reload` 在 **不中断连接** 的情况下热重载配置与路由表。

所有命令都接受 `-c / --config <path>`；守护进程控制命令（`stop`、`reload`、`status`）还接受
`--socket <path>` 以覆盖 Unix 控制 socket。在进程监管器之下，建议以 `bitrouter serve` 作为前台
entrypoint，并让监管器处理重启：

```ini
# /etc/systemd/system/bitrouter.service
[Service]
ExecStart=/usr/local/bin/bitrouter serve -c /etc/bitrouter/bitrouter.yaml
EnvironmentFile=/etc/bitrouter/bitrouter.env
Restart=on-failure
```

## 4. 导出遥测

BitRouter 原生支持 OpenTelemetry：`bitrouter-observe` 插件通过 **OTLP**（HTTP 或
gRPC）将 trace 与指标推送到任意 OpenTelemetry 后端。用一个 `otel` 块（或对应的环境
变量）把它指向你的 collector：

```yaml
plugins:
  bitrouter-observe:
    otel:
      endpoint: "http://otel-collector:4318"
      service_name: "bitrouter"
```

没有 Prometheus 抓取端点——指标仅通过 OTLP 推送。基于 Prometheus 的技术栈请通过
OpenTelemetry Collector 接入。用 `bitrouter observe status` 确认导出器已启用。你也可
以在请求命中上游之前追踪某个模型名是如何解析的：

```bash
bitrouter route gpt-4o     # 打印某个模型的完整回退链
```

span 模型、按请求归因与各后端的导出配置见
[OpenTelemetry](/docs/features/opentelemetry)。

## 5. 基本加固

- **审慎绑定。** 保持 `listen: 127.0.0.1:4356`，除非你有理由对外暴露。若设为 `0.0.0.0`，请在其前
  加上带 TLS 与鉴权的反向代理。
- **生产环境勿带 `skip_auth: true`**（见 §2）——任何非回环部署都不要带它。
- **机密保留在环境中。** 提交的 `bitrouter.yaml` 中只应出现 `${VAR}` 引用；配置在调试输出中会对
  `api_key` 做脱敏。
- **在 CI 中校验。** 对每次变更运行 `bitrouter config validate -c bitrouter.yaml`，并将
  `# yaml-language-server` schema URL 固定到你的版本。
- **加上内容防火墙**，用 [护栏](/docs/features/guardrails) 来拦截或脱敏请求/响应内容。
- **配置变更用 reload，而非 restart**，以免丢弃在途请求。

## 后续步骤

<Cards>
  <Card title="安装" href="/docs/get-started/installation" description="若你跳过了：60 秒本地安装。" />
  <Card title="自托管 vs 云" href="/docs/get-started/self-hosted-vs-cloud" description="选择适合你的部署模型。" />
  <Card title="OpenTelemetry" href="/docs/features/opentelemetry" description="OTLP trace + 指标导出与按请求归因。" />
  <Card title="护栏" href="/docs/features/guardrails" description="面向请求与响应的内容防火墙。" />
</Cards>
