---
title: MCP
description: 从任何 MCP 客户端驱动 BitRouter——把 complete、list_models 与 status 作为工具，暴露到同一个本地端点上的源 MCP 服务器。
---

MCP 是驱动 BitRouter 的第二种方式，与 [CLI](/docs/concepts/cli) 并列。CLI 是通往本地守护进程的命令行界面，而 MCP 服务器则是通往同一端点的**工具界面**——因此任何 MCP 客户端（Claude、Cursor，或你自己的智能体）都能把 BitRouter 当作一组工具来对话，而无需另起子进程。

## 源 MCP 服务器

`bitrouter mcp serve` 把 BitRouter 自身暴露为一个**源（origin）** MCP 服务器——一小组固定的工具，直接对接位于 `127.0.0.1:4356` 的本地守护进程：

- `complete` —— 经路由器运行一次补全，具备与 HTTP API 完全相同的路由、回退与策略行为。
- `list_models` —— 枚举你的账户或密钥可触达的模型。
- `status` —— 报告守护进程的健康状况与配置。

```bash
bitrouter mcp serve                    # stdio → 位于 127.0.0.1:4356 的本地守护进程
bitrouter mcp install --client claude  # 打印可粘贴的 mcpServers 配置块
```

追加 `--transport http` 可改为对接多租户云后端，而非本地守护进程。

## 并非 MCP 网关

请把 BitRouter 与 MCP 的两种角色区分开：

- **本页——源服务器。** BitRouter *就是*那个工具。客户端调用 `complete` / `list_models` / `status` 来驱动路由器。
- **[MCP 网关](/docs/concepts/tools)。** BitRouter 站在你*自己的* MCP 服务器*前面*，带着鉴权、访问控制与身份转发，把它们的工具代理给智能体。在那里 BitRouter 是中间层，而非端点。

一个是你如何触达 BitRouter，另一个是 BitRouter 如何触达你的工具。网关见 [Tools](/docs/concepts/tools)，智能体驱动 BitRouter 的第三种方式见 [Agent Skill](/docs/concepts/agent-skill)。
