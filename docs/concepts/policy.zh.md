---
title: Policy
description: 由运维方掌控、用来决定一个循环如何路由的规格——确定性、路径中没有 LLM，且默认关闭。
---

**策略（policy）**是用来决定一个循环如何路由的规格。它是由运维方掌控的配置，而非一个模型：路由决策是确定性的，不会在路径中新增任何 LLM 调用，并且每个部署出厂时它都**默认关闭**。它是 BitRouter [观察 → 评估 → 行动](/docs/get-started/introduction) 循环中的「行动」面——由智能体（或你自己）编辑的那个文件，用来只在真正值得时才动用更强的模型，其余时候一律用更便宜的。

## 策略表

在最核心处，一份策略就是一张静态、由运维方掌控的**表**，它逐请求地挑选模型，而不是照单全收调用方所请求的模型：

- 依据模型最近一次发言，从规范化提示中**指纹化**当前的智能体循环步骤——`opening`、`after_<tool>`（例如 `after_read_file`）或 `midstream`。
- 将指纹 → 层级 → 模型 id 逐级**解析**，并改写请求的模型。未命中的指纹回退到 `default_tier`。
- **硬性工具使用护栏：** 带有工具的请求会被上钳到一个工具安全层级，因此降级永远不会把某次工具调用搁浅在无法胜任的模型上。
- **幂等，且会让路**给它并不拥有的路由——显式的 `provider:` 或 `claude-code:` 目标，或 `bitrouter/fusion` 别名，都会原样透传。

```yaml
policy_table:
  tiers: { cheap: openai:gpt-4o, capable: anthropic:claude-sonnet-4-6 }
  fingerprints: { after_read_file: cheap }
  default_tier: capable
  tool_use_tier: capable
  tool_safe_tiers: [capable]
```

仅这张表本身就是一个完整、确定性的路由器。本页余下部分讲的是*自适应*的那一半——完全按需开启。

## 充分性账本

打开 `adequacy`，路由器便会在线学习，逐请求进行，不存在任何轮次结构。一个观察器会重新计算每个已服务请求的指纹，把已服务的模型映射回其层级，并——**仅针对真正的降级**——记录该请求是否硬失败：

- 在连续 `escalation_threshold` 次失败后，该指纹会被**钉住**并升级到更强的层级。钉住状态在本地持久化，并在**冷却后衰减**。
- 开启 `explore_enabled` 后，守护进程会周期性地对你留在更强层级上的指纹**试探**性地改用便宜层级，并把那些持续成功的**锁定**下来——从而自动发现安全的降级。一次失败的试探会触发升级并停止。工具使用护栏仍会钳制任何针对工具请求的试探。

```yaml
  adequacy:
    enabled: true
    escalation_tier: capable
    escalation_threshold: 2
    pin_cooldown_secs: 1800
    explore_enabled: true     # 激进旋钮
    explore_tier: cheap
    explore_threshold: 3
    explore_interval: 5
```

## 保证

这条规则在方向上是不对称的：学习器永远只能让路由比你的表**更保守**。一个被证明不充分的降级——无论是你配置的*还是*守护进程发现的——都会被升级并停留在那里；只有持续成功的降级才会被保留。因此请求永远不会持久地路由到更差的层级，与此同时更便宜的路由仍在被追寻。两半都是按需开启的，所以一个关闭了 `adequacy` 的部署，其行为与静态表逐字节一致。

## 与 Cloud 策略并非同一回事

本页讲的是本地路由器中的**路由**策略。BitRouter Cloud 有一个*独立的*策略面——`bitrouter cloud policy` 管理绑定到某个 API 密钥或工作区的预算、限速、护栏与预设。相关命令见 [CLI](/docs/concepts/cli)。

## 相关

- [供应商选择](/docs/features/provider-selection)——被选中模型背后的各供应商是如何排名的。
- [模型回退](/docs/features/model-fallback)——失败时沿一份有序的模型列表逐个尝试。
- [模型路由](/docs/concepts/models)——为什么一个模型是策略据以路由的聚合体。
