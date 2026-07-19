---
title: Claude subscription
description: 让你的 Claude Pro 或 Max 套餐经 BitRouter 路由——OAuth 登录，无需 Anthropic API 密钥，也没有按 token 计费的账单。
sourceHash: 2ede6363832e91c518560ddcd0a44a5e496bb5fbf021f60d8675abdf75a194fa
---

已经在付费用 Claude Pro 或 Max？把这份套餐当作一个模型来源来用。`bitrouter providers login claude-code` 会采用本机 Claude Code 会话，保存一个可自动刷新的凭据，并把它附加到路由至 `claude-code` 供应商的请求上——这样你的订阅用量就能覆盖这些 token，也无需管理 `ANTHROPIC_API_KEY`。

<Callout type="info">
**这是订阅，不是 API 密钥。** 这是 OAuth 路径——它会计入你的 Claude 套餐用量。如果你更想按 token 付费、使用 Anthropic API 密钥，跳过登录步骤，直接在环境变量中设置 `ANTHROPIC_API_KEY` 即可；单独的 `anthropic` 供应商会自动识别它。
</Callout>

## 登录

```bash
bitrouter providers login claude-code
```

当本机已有 Claude Code 会话时，这会直接复用该会话；如有需要，也会驱动 Claude Code 自己的登录流程。凭据保存到 `$XDG_DATA_HOME/bitrouter/oauth-tokens.json` 并自动刷新——只需登录一次。如需稍后移除：

```bash
bitrouter providers logout claude-code      # 移除已保存的凭据
```

### 用于服务器和 CI 的长期 token

Claude Code 的 `claude setup-token` 流程会为无头环境生成长期凭据。请在
BitRouter 启动前，通过官方进程环境变量提供它：

```bash
CLAUDE_CODE_OAUTH_TOKEN="$CLAUDE_CODE_OAUTH_TOKEN" bitrouter start
```

BitRouter 会在构建订阅供应商时捕获这个 token。它只存在于进程内且不可
刷新，因此应在密钥来源中轮换后重启 BitRouter。请把值保存在密钥管理器
或仅所有者可读的环境文件中；绝不要放入 `bitrouter.yaml`、客户端配置、
命令参数、日志或 benchmark 产物。

<Callout type="warn">
**每次请求只用一种鉴权方式。** 路由到 `claude-code:<model>` 的请求使用你的 OAuth 订阅；路由到 `anthropic` 供应商的请求使用 `ANTHROPIC_API_KEY`。运行 `bitrouter providers logout claude-code` 即可移除订阅路由。
</Callout>

### 多账号

每个凭据都以 `(provider, label)` 为键。传入 `--label` 即可让多个 Claude 账号并存：

```bash
bitrouter providers login claude-code --label work
bitrouter providers login claude-code --label personal
```

## 路由到它

无需任何 `bitrouter.yaml` 配置块——当已保存凭据存在时，`claude-code` 供应商会自动启用。通过显式订阅供应商路由来指定 Claude 模型：

```bash
bitrouter route claude-code:claude-sonnet-4-6
```

然后[启动 BitRouter 并发送请求](/docs/integrations/models#start-bitrouter-and-send-a-request)。使用显式 id `claude-code:claude-sonnet-4-6` 可以把请求固定到你的订阅上；也可以使用下方 Claude Code 运行容器流程，让 BitRouter 识别真正的 Claude Code 流量，并把裸 Claude 模型名改写到订阅供应商。

标准 Anthropic 客户端和非 Claude Code agent 可以使用这个显式路由，
而无需伪装成 Claude Code。它们向 BitRouter 发送普通 Messages API 请求；
BitRouter 会补齐订阅上游要求的 OAuth、Claude Code agent 画像、版本和客户
端请求头：

```bash
curl http://127.0.0.1:4356/v1/messages \
  -H "Content-Type: application/json" \
  -H "anthropic-version: 2023-06-01" \
  -d '{
    "model": "claude-code:claude-sonnet-4-6",
    "max_tokens": 64,
    "messages": [{"role": "user", "content": "Reply with exactly READY."}]
  }'
```

带供应商限定的模型名就是显式选择边界。非 Claude Code 客户端发送裸 Claude
模型时，绝不会静默消耗你的订阅；它仍遵循普通供应商级联，而订阅供应商
不参与该级联。

## 让 Claude Code 经 BitRouter 运行（附带遥测）

把 BitRouter 当作 Claude Code CLI 的透明运行容器——让 BitRouter 处于请求路径中纯粹是为了附带效果：今天是可观测性，未来或许是可选的模型重路由。从一个刚安装好的 `bitrouter` 开始：

**1. 把你已有的 Claude Code 会话接入为 `claude-code` 订阅供应商**（如果你尚未登录，会驱动 `claude` CLI 自身的登录流程）：

```bash
bitrouter providers login claude-code
```

**2. 开启完整的第一方遥测**（默认关闭）——创建 `~/.bitrouter/bitrouter.yaml`：

```yaml
server:
  skip_auth: true          # 本地守护进程：放行无凭据的 spawn 流量
plugins:
  bitrouter-observe:
    telemetry:
      enabled: true        # 除非你主动开启，否则不会导出任何内容
      level: full          # 元数据 + 请求/响应内容（用 `metadata` 可省略内容本身）
                           # 省略 endpoint → 默认使用 https://telemetry.bitrouter.ai
```

**3. 在后台启动守护进程并确认其已就绪：**

```bash
bitrouter start            # 后台分离运行；日志写入 ~/.bitrouter/bitrouter.log
bitrouter status           # running: yes — listen 127.0.0.1:4356
bitrouter observe status   # 遥测导出端点及其状态
```

**4. 启动一个指向 BitRouter 的交互式 Claude Code 会话：**

```bash
bitrouter launch -a claude  # 交互式；结束后运行 `bitrouter stop`
```

真正的 Claude Code 流量——通过其 `anthropic-beta: claude-code-*` agent 画像标记识别——会被路由到你的订阅；其他流量则会流向你配置的其他供应商。遥测数据会归属到一个匿名的安装 id 下。*（可选：先运行 `bitrouter cloud login`，即可同时用你的 BitRouter Cloud 账号为非 Claude Code 模型提供服务。）*

## 延伸阅读

- [Claude Code](/docs/integrations/claude-code)——把 Claude Code CLI 指向 BitRouter（作为运行容器），这与上面「把套餐当模型来源使用」是两回事。
- [模型](/docs/concepts/models)——完整的 `provider/model` id 命名方案。
- [模型回退](/docs/features/model-fallback)——在过载时从你的订阅回退到某个托管模型。
