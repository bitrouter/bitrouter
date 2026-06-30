---
title: 服务端工具
description: 让 BitRouter 在服务端替你运行工具调用循环——包括 Advisor、SubAgent 与 Fusion 这些由模型支撑的工具。
sourceHash: 86118153c9642705e3357ef78f6a501633c2bf90c8adea85ececbd994d4b4e06
---

通常是你的 agent 在运行工具调用循环：模型请求调用某个工具，你的 harness 执行它、追加结果，再次调用模型。**服务端工具**把这个循环搬进 BitRouter。你声明一组工具，BitRouter 把它们公布给模型；当模型调用其中之一时，BitRouter **自己执行该工具并把结果喂回**——如此循环，直到模型不再调用它们。在调用方看来，这就像一次单独的响应。

## 循环如何运行

BitRouter 把声明的工具注入到出站请求中，拦截模型对它们的调用、运行它们、追加结果，再重新调用上游——重复此过程，直到模型返回一个不含工具调用的答案，或触及某个上限。该循环是有界的，以防失控：

| 上限 | 默认值 | 含义 |
| --- | --- | --- |
| `max_iterations` | 10 | 循环停止前的最大工具轮数。 |
| `tool_timeout` | 30s | 单个工具的执行超时。 |
| `total_budget` | 120s | 整个循环的墙钟时间预算。 |
| `max_consecutive_errors` | 3 | 连续这么多次工具失败后停止。 |

在每次调用前，一项**审批策略**会决定该工具是否可以运行。默认放行一切；被拒绝的调用会向模型返回一个"执行被拒"的结果，而不会真正运行。

## 逐请求启用服务端工具

你通过在请求的 `tools` 数组中声明工具来开启服务端工具——无需改动配置。BitRouter 识别三个内置的、由模型支撑的工具，并且只公布你声明了的那些：

```json
{
  "tools": [
    { "type": "bitrouter:advisor",  "args": { "model": "anthropic/claude-opus-4.8", "instructions": "..." } },
    { "type": "bitrouter:subagent", "args": { "model": "openai/gpt-4o-mini", "instructions": "..." } },
    { "type": "bitrouter:fusion",   "args": { "panel": [{ "model": "..." }], "judge": { "model": "..." } } }
  ]
}
```

MCP 服务器工具则改为通过配置接入——把 `server_tools.mcp_servers` 设为那些其工具应由 BitRouter 在循环内运行的服务器。

## 由模型支撑的工具

三个内置工具包装的是**嵌套的模型调用**，而非外部副作用。每个都有独立页面：

- [**Advisor（顾问）**](/docs/features/advisor) —— 正在运行的模型在生成途中就某个困难子问题咨询一个更强的模型，而不必升级整个请求。
- [**Sub-agent（子代理）**](/docs/features/subagent) —— 模型把一个自包含的任务委派给更便宜、更快的工作模型，后者在隔离环境中运行并只返回结果。
- [**Fusion（融合）**](/docs/features/fusion) —— 一组模型并行作答，裁判模型对它们的答案进行*比较*（而非合并），调用方模型据此写出最终回复。

<Callout type="info">
Advisor、Sub-agent 与 Fusion 各自由嵌套在你请求内部的模型调用支撑。它们的开销等同于其底层模型调用的开销，并且会像任何其他调用一样出现在你的用量历史中。
</Callout>

## 工具如何打包：toolset

**toolset（工具集）**是让路由器运行的工具与供应商无关的那道接缝。每个 toolset 为一次请求回答两个问题——**公布哪些工具**，以及**如何执行对其中某个工具的调用**——而 BitRouter 把若干 toolset 组合成模型最终看到的那一组工具。

BitRouter 维护一个 toolset 注册表；当模型调用某个工具时，注册表把该调用路由到**拥有**该名称的 toolset。由于可同时有多个 toolset 处于激活状态，工具名会被**加前缀**以避免冲突——`demo` 服务器的 `search` 工具会以 `demo__search` 的名义公布。

toolset 分三类：

- **MCP 支撑** —— 每个上游 MCP 服务器对应一个；其工具就是该服务器的工具，经网关运行。
- **模型支撑** —— Advisor、Sub-agent 与 Fusion，各自包装一次嵌套的模型调用。
- **进程内** —— 直接在路由器中实现的工具。

toolset 不必在每次请求上都公布工具。它可以检查调用方**声明**了什么，否则保持沉默——这正是模型支撑工具的工作方式：只有当请求声明了 `bitrouter:advisor` 时 Advisor 才会出现，Sub-agent 与 Fusion 同理。模型永远只会看到该次请求实际选用的工具。
