---
title: 护栏
description: 命名的正则规则，对请求与响应中匹配的内容执行拦截或脱敏——在路由器内部强制执行，无需额外的模型调用。
sourceHash: 86d6e83b4e26ff5831ec2aee39c1d5d278b2252e27e7d49a2c496aea163bdbb4
---

**护栏（guardrail）**是一条带动作的命名正则规则。BitRouter 在入站时扫描请求 prompt、在出站时扫描响应流；当某条规则命中时，它要么**拦截**请求（或中止响应流），要么**脱敏**命中的片段。强制执行发生在路由器内部、在代理这一跳上——无需额外的模型调用，也不依赖外部服务。

它**没有内置分类器**——没有 PII 检测器、毒性模型或内容分类。护栏严格按它的正则行事，因此你需要自带对你的工作负载真正重要的模式。

## 配置

规则位于插件配置的 `custom_patterns` 下。每条规则有一个 `name`、一个 `pattern`（[Rust 正则](https://docs.rs/regex/latest/regex/#syntax)）和一个 `action`：

```yaml
plugins:
  bitrouter-guardrails:
    custom_patterns:
      - name: ssn
        pattern: '\d{3}-\d{2}-\d{4}'
        action: redact
      - name: provider-key
        pattern: 'sk-[a-zA-Z0-9]+'
        # 省略 action → 默认为 block
```

模式在路由器加载配置时编译——无效的正则会在启动时即刻失败，而不是等到请求时。

## 两种动作

| 动作 | 命中时 |
| --- | --- |
| **`block`** *（默认）* | 拒绝该请求，或中止响应流，并指明触发的规则。省略 `action` 时的默认值——block 是安全的兜底。 |
| **`redact`** | 把命中的片段替换为 `[REDACTED]`，并放行内容。 |

## 请求 vs 响应

两侧是刻意不对称的：

- **请求（入站）。** 仅 `block` 规则生效——一个 prompt 要么放行、要么被拒，而不会被改写。扫描覆盖系统指令、每条消息的文本与推理内容、**工具结果输出以及工具调用参数**。二进制部分（图像、音频、文档）与引用来源不含可扫描文本，会被跳过。
- **响应（出站，流式）。** `redact` 规则在流动过程中把命中替换为 `[REDACTED]`；`block` 命中则中止整个流。流式从不被阻塞——每个增量都在自己的轮次中发出。

<Callout type="info">
**block 跨流式分片检测，redact 按分片处理。** 一个跨越两个流式增量的 `block` 模式仍会被捕获（会保留一小段尾部窗口用于检测）。而一个*恰好*跨越增量边界的 `redact` 模式则是已知的缺口——block 这一安全关键动作，才是能保证跨边界的那个。
</Callout>

## 在云端

以上一切都在开源二进制中、基于你的配置文件运行。**BitRouter Cloud** 把同样的 block/redact 规则作为一条按工作区的策略替你托管，你在控制台里编辑——无需部署配置文件。参见[工作区](/docs/features/namespaces)。
