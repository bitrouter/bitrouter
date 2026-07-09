---
title: Agent Skill
description: /bitrouter Agent Skill——让 AI 编码智能体能自行安装、配置、迁移到并排查 BitRouter。
---

**Agent Skill** 是驱动 BitRouter 的第三种方式，与 [CLI](/docs/concepts/cli) 和 [MCP](/docs/concepts/mcp) 并列。BitRouter 附带一个 `/bitrouter` [Agent Skill](https://agentskills.io)，让 AI 编码智能体能够**自行**安装、配置、迁移到并排查 BitRouter——这个技能承载了那些容易漂移的事实（监听端口、环境变量名、`provider/model` 斜杠写法、存在哪些子命令），从而让智能体不必去猜。

它位于主仓库的 `skills/bitrouter/`，并在同一次改动中与代码保持同步——因此这个技能绝不会描述一个已经与二进制不再匹配的 CLI。

## 安装

```bash
bitrouter skills add bitrouter        # 通过 BitRouter 自带的安装器
npx skills add bitrouter/bitrouter    # 通过通用的 skills CLI
```

两条命令都会把该技能放入你的智能体技能目录；此后智能体便能驱动 BitRouter，而无需你手写安装步骤。

## 并非 AgentSkills 网关

请把 BitRouter 与技能的两种角色区分开：

- **本页——所附带的技能。** `/bitrouter` 是一个*驱动* BitRouter 的技能：智能体通过阅读它来正确操作路由器。
- **[AgentSkills 网关](/docs/concepts/tools)。** BitRouter 把技能作为受治理、可路由的资源*提供*给其背后的智能体——就像 [MCP 网关](/docs/concepts/tools)公布工具那样安装并公布技能。

一个教智能体如何使用 BitRouter，另一个让 BitRouter 把技能交给智能体。网关见 [Tools](/docs/concepts/tools)。
