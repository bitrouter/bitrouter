---
title: Policy
description: 由运维方掌控、用来决定一个循环如何路由的规格——确定性、路径中没有 LLM，且默认关闭。
---

**策略（policy）**是用来决定一个循环如何路由的规格。它是由运维方掌控的配置，而非一个模型：路由决策是确定性的，不会在路径中新增任何 LLM 调用，并且每个部署出厂时它都**默认关闭**。它处于 BitRouter [act → observe → evaluate → learn](/docs/get-started/introduction) 循环的中心——路由器依据它执行（act），学习（learn）再把结果写回其中：由智能体（或你自己）编辑的那个文件，用来只在真正值得时才动用更强的模型，其余时候一律用更便宜的。

## 绑定到 preset 的 policy lock

路由策略默认位于 `bitrouter.yaml` 同级的 `policy-lock.yaml`。把一份命名策略绑定到 preset 后，只有请求该 preset 时才会启用策略。裸模型请求以及没有 `policy:` 的 preset 会保持原有行为。

```yaml
# bitrouter.yaml
policy:
  # path: routing/team-policy.yaml  # 可选；相对于本文件
  writeback: locked

presets:
  coding:
    model: anthropic/claude-opus-4.8
    policy: coding
```

```yaml
# policy-lock.yaml
lockfileVersion: 1
policies:
  coding:
    key_strategy: workflow_state
    tiers:
      economy: moonshotai/kimi-k2.7-code
      strong: anthropic/claude-opus-4.8
    routes: {}
    default_tier: strong
    tool_use_tier: strong
    tool_safe_tiers: [strong]
    adequacy:
      enabled: true
      escalation_tier: strong
      explore_enabled: true
      explore_tier: economy
      explore_threshold: 3
```

使用 `@coding`（或 `@coding:variant`）来启用策略。preset 的 prompt 默认值和 variant 的 provider 偏好会先被解析，并在策略选择有效模型之后继续保留。

lock 文件只保存当前生效的策略。Git 负责文件历史，数据库负责在线证据。映射按稳定顺序序列化，语义摘要不包含注释、时间戳和运行时 id，因此同一份策略会生成同一份产物。

## 进化与热重载

`locked` 表示 BitRouter 程序不能写回 `policy-lock.yaml`，但不会阻止人工或 Git 修改，也不会阻止热重载。执行 `policy unlock` 后，经过验证的进化候选才可以原子发布。

```bash
bitrouter policy init coding --preset coding \
  --economy moonshotai/kimi-k2.7-code
bitrouter policy check
bitrouter policy status
bitrouter policy evolve             # 默认 dry-run
bitrouter policy unlock
bitrouter policy evolve --apply
bitrouter policy reload             # 无需重启 daemon
bitrouter policy lock
```

优化器只消费带策略 namespace 的充分性 row，并且只补充当前缺失、已满足门槛的 route。它不会覆盖或删除任何已有 route，因此人为和 Git 修改始终具有最高优先级。负反馈仍会影响运行时充分性路由；在数据库具备独立 writeback provenance 之前，删除已物化 route 是一次明确的人为或 Git 修改。发布会在同目录原子替换前检查语义摘要；如果检测到中途修改，就会中止，而不是静默覆盖。无效 reload 会被拒绝，daemon 继续使用上一份有效策略快照。

## 策略表语义

在最核心处，一份策略就是一张静态、由运维方掌控的**表**，它逐请求地挑选模型，而不是照单全收调用方所请求的模型：

- 依据模型最近一次发言，从规范化提示中**指纹化**当前的智能体循环步骤——`opening`、`after_<tool>`（例如 `after_read_file`）或 `midstream`。
- 将指纹 → 层级 → 模型 id 逐级**解析**，并改写请求的模型。未命中的指纹回退到 `default_tier`。
- **硬性工具使用护栏：** 带有工具的请求会被上钳到一个工具安全层级，因此降级永远不会把某次工具调用搁浅在无法胜任的模型上。
- **显式限定作用域：**裸模型与直接的 `provider:model` 请求不会进入命名策略。调用方一旦选择绑定了策略的 preset，该策略就拥有 preset 的有效 base model，即便它带有 provider 前缀。`bitrouter/fusion` 等 server-tool 流仍会原样透传。

这些字段在 `policy-lock.yaml` 的每份命名策略下都可用。为兼容旧配置，顶层 `policy_table:` 仍作为全局 ingress transform 保留；新配置应优先使用 preset 绑定策略，因为它拥有明确的 opt-in 边界。

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

证据规则是非对称的：负向证据会立即升级，而更便宜的 route 必须先通过重复的请求级成功以及配置的语义成功门槛才会生效。已经学到的 route 一旦失败，会重新解锁并升级。学习和探索都是按需开启的，因此关闭 `adequacy` 时，行为与确定性策略表完全一致。

## 与 Cloud 策略并非同一回事

本页讲的是本地路由器中的**路由**策略。BitRouter Cloud 有一个*独立的*策略面——`bitrouter cloud policy` 管理绑定到某个 API 密钥或工作区的预算、限速、护栏与预设。相关命令见 [CLI](/docs/concepts/cli)。

## 相关

- [供应商选择](/docs/features/provider-selection)——被选中模型背后的各供应商是如何排名的。
- [模型回退](/docs/features/model-fallback)——失败时沿一份有序的模型列表逐个尝试。
- [Models](/docs/concepts/models)——为什么一个模型是策略据以路由的聚合体。
