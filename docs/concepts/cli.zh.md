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

除了这些守护进程控制命令之外，v1 提供了两个面向云端的入口：`bitrouter auth …` 用于 OAuth 登录，`bitrouter cloud …` 用于登录后的所有账户操作。

## 登录 BitRouter Cloud

`bitrouter auth login` 针对已配置的授权服务器执行 RFC 8628 设备授权流程，打印审批 URL，并将凭证持久化到 `$XDG_DATA_HOME/bitrouter/account-credentials.json`（Unix 上权限为 `0600`）。浏览器授权页面会让你选择将 CLI 绑定到哪个**工作区**——生成的凭证被烙入该工作区，此后所有 `bitrouter cloud` 命令都隐式指向它，无需任何 `--workspace` 参数。如需切换工作区，重新执行 `bitrouter auth login` 并选择目标工作区即可。访问令牌在过期前 60 秒内自动续期；续期不会改变工作区绑定。

```bash
bitrouter auth login
# 在浏览器中打开以下链接，选择要登录的工作区：
#   https://cloud.bitrouter.ai/oauth/device?user_code=ABCD-EFGH
# Waiting for authorization (the code expires in 600s)…
```

默认授权服务器为 `https://api.bitrouter.ai`，可通过 `--oauth-as <URL>`（或 `BITROUTER_OAUTH_AS`）切换到自托管部署。默认 scope 集合覆盖推理及 keys、usage、billing-read、policy、BYOK、namespace-read 的读写——如需敏感 scope（`billing:write`、`user:write`、`clients:read`、`clients:write`），在登录时传入 `--scope "<现有> clients:write"`。

`bitrouter auth whoami` 直接读取本地凭证文件，不访问网络，同时打印已绑定的工作区。退出登录（尝试在 AS 端撤销并删除本地文件）使用 `bitrouter auth logout`。

<Callout type="info">
执行 `bitrouter auth login` 之后，零配置模式会自动启用 `bitrouter` provider——账户内的每个可用模型都可作为 `bitrouter:<model-id>` 路由，无需进一步配置。
</Callout>

## 管理账户：`bitrouter cloud`

每个叶子命令都支持 `--json` 输出原始响应；默认输出对单个资源采用 `systemctl` 风格的键值块，对列表采用紧凑表格。当服务器返回 `missing required scope: <s>` 的 403 时，CLI 会打印一条可直接粘贴的 `bitrouter auth login --scope "<现有> <s>"` 提示。

凭证是**工作区级（namespace-baked）**的——密钥、用量、策略和 OAuth 客户端全部作用于登录时选择的工作区，`{nsid}` 路径段由 CLI 隐式解析。`billing` 和 `byok` 是用户级的，跨所有工作区生效。

### `bitrouter cloud whoami`

本机存储的身份信息、已绑定的工作区，以及该 CLI 将访问的 `/v1/*` 基础 URL。离线读取。

### `bitrouter cloud namespace` — 工作区

查看你拥有的所有工作区及当前 CLI 会话所绑定的工作区。工作区的创建和删除是控制面操作，仅限 Console。

```bash
bitrouter cloud namespace list      # 所有工作区，当前激活的标有 (active)
bitrouter cloud namespace current   # 离线——读取本地凭证，无网络请求
```

若凭证早于工作区功能上线，`current` 会打印 `(no namespace — run \`bitrouter auth login\`)`。

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
bitrouter cloud usage                                       # 最近 30 天
bitrouter cloud usage --from 2026-05-01T00:00:00Z --to 2026-06-01T00:00:00Z
bitrouter cloud requests --limit 50 --offset 0
```

`usage` 汇总当前工作区的花费（微美元）和 token 计数。`requests` 分页展示工作区维度的请求历史。

### `bitrouter cloud billing` — 余额 + 充值

用户级——非工作区级；反映账号整体余额，与当前登录的工作区无关。

```bash
bitrouter cloud billing balance
bitrouter cloud billing checkout --amount-cents 2000       # 需要 billing:write
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

### `bitrouter cloud oauth-client` — OAuth 客户端注册

```bash
bitrouter cloud oauth-client list
bitrouter cloud oauth-client register \
  --name "my-agent" --type confidential \
  --grant authorization_code --grant refresh_token \
  --scope inference:invoke --scope policy:read \
  --redirect-uri https://my-agent.example.com/cb
bitrouter cloud oauth-client update <client-id> --name "renamed"
bitrouter cloud oauth-client delete <client-id>
```

需要 `clients:read` / `clients:write` scope，不在默认集合中。机密客户端新生成的 `client_secret` 仅在 `register` 响应中返回一次。
