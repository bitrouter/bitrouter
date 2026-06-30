---
title: 工具（Tools）
description: 工具是 agent 在运行时获得的能力——MCP 服务器与 Agent Skills——两者都通过单个 BitRouter 网关对外提供。
sourceHash: 5db598556756983832c0a835dcdcb78f53cd8aa6ad1d70a117cfe1a84ddb5e32
---

在 BitRouter 上，**工具**是 agent 在运行时为完成工作而获得的能力。它们分为两类，而 BitRouter 通过单一端点同时对外提供二者，于是 agent 只需连接一次，无需手动逐个接好每个来源：

- **MCP 服务器** —— 可调用的工具（搜索 API、数据库、文件系统、支付通道）。
- **Agent Skills** —— 可加载的专业知识：agent 按需拉取的流程与指令。

## MCP 网关

agent 可调用的工具就是 [MCP](https://modelcontextprotocol.io) 服务器。每个 MCP 服务器暴露一组工具，agent 可以发现并调用它们。问题在于，这些服务器分散在各处，各有各的地址、鉴权和端点——一个想使用十个工具服务器的 agent，通常得知道十个端点。

BitRouter 的 **MCP 网关**位于这些服务器之前并对其进行代理。你的 agent 连接到一个 BitRouter 端点；在其背后，网关把**工具发现**和**工具调用**转发到正确的上游 host，并将响应回传。一个连接，多个工具服务器——这与 BitRouter 处理模型的方式如出一辙。

把工具统一经由单个网关路由，能为你带来三样原本需要为每个 agent 重复搭建的东西：

- **统一鉴权** —— agent 只需向 BitRouter 鉴权一次，而无需为每个上游服务器各自携带凭证。
- **发现** —— 跨 host 的工具在同一处呈现，agent 无需被预先接线到每个服务器即可发现可用项。
- **策略** —— 每次工具调用都经过网关，这里是统一执行规则的天然位置。

### 一个端点，多个服务器

网关对外暴露一个聚合端点，它会**扇出**到每个已配置的服务器。一次 list 调用会查询每个服务器并返回合并后的目录；一次工具调用则路由到归属它的服务器。为避免名称冲突，每个服务器的工具都会**带前缀做命名空间隔离**——`demo` 服务器上的 `search` 工具对外公布为 `demo__search`。某个服务器可以**退出**聚合，从而仅在自己的路由上可达；廉价的 list 调用会被**短暂缓存**，以保持发现的快速。前缀与缓存设置可按服务器分别配置。

## Agent Skills

agent 获得的第二种能力是**专业知识**。**Agent Skills** 是 agent 按需加载的即插即用能力——一个打包好的流程、一组指令、一套工作流——其被发现与拉取的方式与工具相同，都经由网关。如果说 MCP 工具是 agent *调用*的东西，那么 Skill 就是 agent *学会*的东西：知识随 agent 一同迁移，而不是留在某个人的配置笔记里。一个常见的 Skill，就是教会 agent 如何驱动 BitRouter 本身。

## 了解如何使用

- [服务端工具](/docs/features/server-tools) —— 让 BitRouter 在服务端替你运行工具调用循环。
- [工具集](/docs/features/server-tools#how-tools-are-bundled-toolsets) —— 一次请求中对外暴露的工具如何被打包与命名空间化。
