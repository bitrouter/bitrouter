---
title: 插件（Plugins）
description: 插件把一个或多个钩子（以及所需的迁移）打包，并通过一次调用安装进 BitRouter 流水线。
sourceHash: d88f0e88401dc5a409303c6a1597c27c1b52a1c6626eb3d76cefea9ba19d5295
---

**插件**是把一个或多个[钩子（Hook）](/docs/concepts/hooks)——以及它们所需的任何数据库迁移——打包，并通过一次调用安装进路由器的单位。它让护栏（guardrails）、可观测性或证明（attestation）这类能力，以一个可安装的整体发布，而非一堆零散的回调。

## 流水线

每个插件都针对三条流水线之一，取决于它作用于哪类流量：

- **`language_model`** —— 主 LLM 流水线，包含完整的钩子集合。
- **`mcp`** —— Model Context Protocol 路由。
- **`acp`** —— Agent Client Protocol 路由。

## 一种便利，而非原子单位

插件是一个**便利打包**：它把一组相关的钩子与迁移捆在一起并一并安装。它*并非*原子单位——每个插件都可以通过逐个注册其钩子来复现。捆绑只是让一项能力一步到位、可复现且可安装。核心就以这种方式发布了若干插件，包括 `bitrouter-guardrails`、`bitrouter-observe` 和 `bitrouter-attestation`。

要基于 Rust SDK 编写一个插件，见 [构建插件](/docs/guides/build-a-plugin)。
