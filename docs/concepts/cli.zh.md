---
title: CLI
description: 运行 BitRouter 的单个本地二进制——一个供你运行时指向的端点、一个由你掌控的守护进程，以及一个可脚本化的云端登录与账户操作面。
sourceHash: 35a2dabbe22e0eb9fec67078169dab4fb7c0b5cc47d34d9b24aa959c5453f90a
---

BitRouter 以单个**静态二进制** `bitrouter` 的形式发布，无需安装任何依赖。它扮演两个角色：既运行你 agent 所对话的**本地路由器**，又是你托管账户的**命令行操作面**。

## 本地端点

你的 agent 从不直接与远程 API 对话——它指向本地运行的二进制，默认地址为 `http://127.0.0.1:4356`。本文档中的其余一切——四种模型协议、MCP 与 ACP 网关——都从这一个端点对外提供。

你以守护进程的形式运行它，并掌控其生命周期：

- `bitrouter serve` —— 在前台运行路由器。
- `bitrouter start` / `stop` / `status` —— 以后台守护进程方式管理它。

除了这些守护进程控制命令之外，v1 使用 `bitrouter cloud …` 处理 OAuth 登录与账户操作。

## 登录 BitRouter Cloud

`bitrouter cloud login` 可以执行 RFC 8628 设备授权流程，也可以存储已有的 BitRouter API key。两种凭证都保存在 `$XDG_DATA_HOME/bitrouter/account-credentials.json`（Unix 上权限为 `0600`），并由原始 API 请求、管理命令、内置 provider 与 telemetry 归因共同复用。API-key 形式不发起网络请求，适合 CI 环境。

```bash
bitrouter cloud login
bitrouter cloud login --api-key "$BITROUTER_API_KEY"
bitrouter cloud whoami
```

交互式 OAuth 的浏览器授权页面会让你选择 CLI 会话绑定的工作区。如需切换工作区，重新执行 `bitrouter cloud login` 并选择目标工作区即可。OAuth access token 会在过期前 60 秒内自动续期。默认授权服务器为 `https://api.bitrouter.ai`；可通过 `--oauth-as <URL>`（或 `BITROUTER_OAUTH_AS`）切换到自托管部署。

默认 OAuth scope 集合覆盖 `inference:invoke`、`usage:read`、`keys:read`/`keys:write`、`billing:read`、`policy:read`/`policy:write`、`byok:read`/`byok:write` 与 `namespace:read`。`billing:write`、`user:write`、`namespace:write` 等敏感控制面 scope 需通过 `--scope` 显式申请。`--api-key` 与 OAuth 专用的 `--client-id`、`--scope` 冲突。

`bitrouter cloud whoami` 直接读取本地凭证文件，不访问网络。使用 `bitrouter cloud logout` 退出登录：OAuth 凭证会在删除本地文件前尽力撤销，API key 则只在本地删除。

权威 receipt 结算也可以直接复用这个受保护的凭证文件，无需把 bearer 导出到环境变量。给 `bitrouter workflow-state reconcile-metering` 传入 `--credentials-file "$XDG_DATA_HOME/bitrouter/account-credentials.json"` 即可；未临近过期的 OAuth token 会直接使用，临近过期时会先刷新。省略该参数仍保持原有的 `--api-key-env BITROUTER_API_KEY` 行为。

<Callout type="info">
使用任一方式登录后，零配置模式都会自动启用 `bitrouter` provider——账户内的每个可用模型都可作为 `bitrouter:<model-id>` 路由，无需进一步配置。
</Callout>

## 直接调用 Cloud API

`bitrouter cloud api` 模仿 `gh api`。它接受登录 origin 上的任意相对端点，注入已存储的 bearer，并直接流式输出响应，不要求本地 daemon 运行。

```bash
bitrouter cloud api /v1/models
bitrouter cloud api /v1/chat/completions --input request.json
bitrouter cloud api /v1/responses -f model=openai/gpt-5 -F stream=true
```

该命令支持 `-X/--method`、可重复的 `-H/--header`、`-f/--raw-field`、`-F/--field`、`--input`、`-i/--include`、`--silent` 与 `--verbose`。为确保凭证只留在登录 origin，绝对 URL 与 fragment 会被拒绝，redirect 跟随也被禁用。非 TTY 的 JSON 与 SSE 字节会原样传递。各首批协议与字段/输入语义详见 [Cloud API 指南](/docs/guides/cloud-api)。

## 管理账户：`bitrouter cloud`

每个叶子命令都支持 `--json` 输出原始响应；默认输出对单个资源采用 `systemctl` 风格的键值块，对列表采用紧凑表格。当服务器返回 `missing required scope: <s>` 的 403 时，OAuth 用户会收到可直接粘贴的 `bitrouter cloud login --scope "<现有> <s>"` 提示；API-key 用户则会收到创建或选择具备该 scope 的 key 的提示。

OAuth 凭证是 namespace-baked 的。API-key 凭证在工作区级管理路由中使用服务器的 `me` namespace 别名。`whoami` 会报告 `oauth` 或 `api_key`，但绝不打印凭证；API-key logout 只删除本地文件。

凭证是**工作区级（namespace-baked）**的——密钥、用量和策略全部作用于登录时选择的工作区，`{nsid}` 路径段由 CLI 隐式解析。`billing` 和 `byok` 是用户级的，跨所有工作区生效。

### `bitrouter cloud whoami`

本机存储的身份信息、已绑定的工作区，以及该 CLI 将访问的 `/v1/*` 基础 URL。离线读取。

### `bitrouter cloud namespace` — 工作区

查看你拥有的所有工作区及当前 CLI 会话所绑定的工作区。工作区的创建和删除是控制面操作，仅限 Console。

```bash
bitrouter cloud namespace list
bitrouter cloud namespace current
```

若凭证早于工作区功能上线，`current` 会打印 `(no namespace — run \`bitrouter cloud login\`)`。

### `bitrouter cloud keys` — API 密钥

管理当前工作区内的 `brk_` API 密钥。签发的密钥与调用方绑定到同一工作区，且无法超出调用方自身的 scope 范围。

```bash
bitrouter cloud keys list
bitrouter cloud keys mint --name ci --scope "policy:read usage:read"
bitrouter cloud keys revoke <id>
```

`mint` 仅在响应中返回一次 `brk_…` 明文 token——服务器只保存其 SHA-256 哈希。请求的 scope 必须是当前生效 scope 的子集（RFC 6749 §3.3 禁止上调）。

### `bitrouter cloud usage` / `bitrouter cloud requests`

```bash
bitrouter cloud usage
bitrouter cloud usage --from 2026-05-01T00:00:00Z --to 2026-06-01T00:00:00Z
bitrouter cloud requests --limit 50 --offset 0
```

`usage` 汇总当前工作区的花费（微美元）和 token 计数。`requests` 分页展示工作区维度的请求历史。

### `bitrouter cloud billing` — 余额 + 充值

用户级——非工作区级；反映账号整体余额，与当前登录的工作区无关。

```bash
bitrouter cloud billing balance
bitrouter cloud billing checkout --amount-cents 2000
```

`checkout` 返回托管的 Stripe URL，需要 `billing:write` scope（不在默认集合中——请使用 `--scope` 重新登录）。

### `bitrouter cloud policy` — 通用策略 CRUD

```bash
bitrouter cloud policy list [--kind budget|rate-limit|guardrail|preset]
bitrouter cloud policy get <id>
bitrouter cloud policy create --name nightly-cap --kind budget --spec spec.json
bitrouter cloud policy update <id> [--name X] [--spec spec.json]
bitrouter cloud policy delete <id>
bitrouter cloud policy bind <id> --principal-type api_key --principal-id <key-id>
bitrouter cloud policy unbind <id> <binding-id>
bitrouter cloud policy disable <id>
bitrouter cloud policy enable <id>
bitrouter cloud policy bindings <id>
bitrouter cloud policy effective --principal-type api_key --principal-id <key-id>
bitrouter cloud policy for-principal api_key <key-id>
```

`--spec` 读取 JSON 文件（或用 `-` 读取标准输入），内容为扁平的内层 spec 主体——例如 budget 的 `{"window": "day", "limit_micro_usd": 5000000}`。Principal 类型：`namespace`、`api_key`、`oauth_token`、`oauth_client`。`effective` 和 `for-principal` 会在不实际发起推理请求的情况下，回答"该 principal 的请求会被如何处理"。`disable` 暂存策略而不删除——引擎在请求时跳过已禁用的行。

### `bitrouter cloud budget` / `bitrouter cloud preset` — 类型化简写

针对 budget 类与 preset 类策略的扁平 wire shape：

```bash
bitrouter cloud budget create --name nightly-cap --window day --limit-micro-usd 5000000
bitrouter cloud preset create --name engineering --guardrail guardrail.json --budget budget.json
```

写入的数据库行与 `bitrouter cloud policy create --kind budget|preset` 完全相同——按调用方更方便的形式选用即可。

### `bitrouter cloud byok` — BYOK provider 密钥

用户级——非工作区级；BYOK provider 密钥作用于账号整体。ciphertext 必须先用云端当前的 X25519 公钥封装——服务器只存储已加密的字节。封装前先通过 `GET /v1/byok/encryption-pubkey` 获取当前公钥。

```bash
bitrouter cloud byok list
bitrouter cloud byok set --provider anthropic \
  --ciphertext-b64 <base64> --kek-id <current-kek> --key-prefix sk-ant-
bitrouter cloud byok delete <provider>
```

## 驱动 BitRouter 的其他方式

CLI 并非通往守护进程的唯一界面。智能体也可以经 [MCP](/docs/concepts/mcp) 驱动 BitRouter——即公布 `complete`、`list_models` 与 `status` 作为工具的源服务器——或经所附带的 [`/bitrouter` Agent Skill](/docs/concepts/agent-skill)，它会教一个编码智能体自行安装并操作 BitRouter。
