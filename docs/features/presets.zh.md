---
title: 预设（Presets）
description: 在命名空间上保存一个命名的 @preset——一个可复用的「基础模型 + 系统提示 + 参数 + 路由规则」组合，用 @name 内联调用。
sourceHash: 90a1b5a2af25e5fe3408474b108b5de98300d306800452813d8c56ab31f27d29
---

**预设（preset）** 是你在命名空间上保存一次、之后通过在 `model` 字段里写 `@<name>` 来内联调用的一份命名路由配置。[模型变体](/docs/features/model-variants)（`:cost`）只为单个请求对供应商重新排序；而预设还能 **替换基础模型**、**注入系统提示**、**设置默认生成参数**，并 **限制可用的供应商**——全部收敛在一个简短的 token 后面。

和变体一样，这个 token 本身就是 `model` 字符串的一部分，因此既不需要请求体字段、也不需要 SDK——在 OpenAI、Anthropic、Google 三套接口上行为一致。使用 `@fast` 的请求与任何普通请求别无二致；预设会在路由之前在服务端解析完成。

## 调用一个预设

在原本填模型 ID 的位置改填 `@<name>`。语法为 `@<name>[/<base-model>][:<profile>]`：

| `model` 取值 | 解析为 |
| --- | --- |
| `@fast` | 预设 `fast`；应用它保存的基础模型与各项覆盖。 |
| `@fast:cost` | 预设 `fast`，并用 [`:cost` 变体](/docs/features/model-variants) 覆盖预设自身的 `sort`。 |
| `@fast/openai/gpt-5` | 预设 `fast`，但路由到 `openai/gpt-5`，而非预设保存的模型。 |

不带 `@` 前缀的裸模型 ID——`anthropic/claude-sonnet-4.6`——保持不变，路由方式与今天完全一致。预设是纯增量的。

## 一个预设能设置什么

每个字段都是可选的。空预设也是合法的（它只是把基础模型原样解析出来）。

| 字段 | 作用 |
| --- | --- |
| `model` | 要路由到的基础模型（如 `openai/gpt-5-mini`）。省略时，请求必须内联提供基础模型（`@name/<model>`）。 |
| `system_prompt` | 当请求自身未设置系统提示时应用的系统提示。 |
| `params` | 默认生成参数（`temperature`、`max_tokens`、`top_p` 等），仅对请求未设置的键进行合并。 |
| `routing.sort` | 默认路由 profile（`balanced` / `cost` / `latency` / `throughput`）——与[模型变体](/docs/features/model-variants)相同的维度。 |
| `routing.only` | 供应商白名单。路由被限制在这些 `provider_name` 之内。 |
| `routing.ignore` | 供应商黑名单。这些供应商会从候选链中剔除。 |

## 预设是默认值；请求永远优先

预设提供的是 *默认值*。调用方在请求上显式设置的任何内容都优先生效：

- **基础模型**——内联的 `@name/<model>`（或请求体里已经指定的模型）会覆盖预设的 `model`。如果预设和请求都没有提供基础模型，请求会被拒绝并返回 `400`。
- **Profile**——显式的 `:profile` 后缀会覆盖预设的 `routing.sort`；两者都没有时，路由为 `balanced`。
- **系统提示**——预设的 `system_prompt` 仅在请求未发送系统提示时才应用。显式的 system 消息始终优先。
- **参数**——预设参数按键逐个合并，且只合并请求省略的键。请求体里的 `temperature` 会压过预设的。

## 创建一个预设

预设按命名空间隔离。可在控制台 **Settings → Routing Presets** 中创建，或使用管理 API：

```bash
curl -X POST http://127.0.0.1:4356/v1/namespaces/{nsid}/routing-presets \
  -H "Authorization: Bearer $BRK_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "name": "fast",
    "model": "openai/gpt-5-mini",
    "system_prompt": "Be terse.",
    "params": { "temperature": 0.1 },
    "routing": { "sort": "latency", "only": ["openai"] }
  }'
```

然后在任意推理接口上调用它：

```bash
curl http://127.0.0.1:4356/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "@fast",
    "messages": [{"role": "user", "content": "Summarize this in one line."}]
  }'
```

完整的 CRUD 接口——`list`、`get`、`create`、`update`、`delete`，以及 `disable`/`enable`——记录在 [管理 API](/docs/reference/management/listRoutingPresets) 中。读取预设需要 `routing_preset:read` scope；创建或修改需要 `routing_preset:write`。

<Callout type="info">
**`name` 就是那个 `@token`。** 预设名必须匹配 `[A-Za-z0-9_-]+`（与 `@name` 语法接受的字符集一致），因此 `my-fast_v2` 可以，而 `my preset` 会在创建时被拒绝——一个永远无法被调用的名字根本不会被存下来。
</Callout>

## 启用与停用

预设可以在不删除的前提下停用（`POST …/routing-presets/{id}/disable`，用 `/enable` 重新启用，或在控制台里切换）。被停用的预设等同于不存在：调用它的 `@name` 会返回与未知预设相同的 `400`，而定义本身会保留，方便你随时切回。

## 预设不会改变授权

解析发生在策略执行 *之前*，且预设只能 **收窄** 一把密钥原本就能做的事——绝不会放宽：

- [护栏（Guardrail）](/docs/features/guardrails) 的模型允许/拒绝名单与 BYOK 规则判定的是 **解析后的基础模型**，因此一个把模型替换为 `openai/gpt-5` 的预设，会被当作你直接请求 `openai/gpt-5` 一样校验。预设无法绕过模型黑名单偷渡请求。
- `routing.only` / `routing.ignore` 只能从候选集中 *移除* 供应商——绝不能添加一个请求原本无权触达的供应商。[BYOK](/docs/features/byok) 供应商仍然排在平台供应商之前。
- 计费不变——你按所选供应商对解析后基础模型的费率付费。

## 错误

| 情形 | 结果 |
| --- | --- |
| `@name` 在命名空间中不存在或已停用 | `400`（与未知模型的 `404` 区分开） |
| 预设没有 `model` 且请求也没有提供基础模型 | `400` |
| `routing.only` / `routing.ignore` 导致没有可用供应商 | `400`（在预设约束下无可用供应商） |
| 创建/更新时：`name` 非法、`routing.sort` 不是已知 profile，或 `params` 的键与传输控制项（`model` / `messages` / `stream`）冲突 | `400` |

## 预设 vs. 模型变体

这两个能力是有意重叠的——按需选用：

- [**模型变体**](/docs/features/model-variants)（`openai/gpt-4o:cost`）是匿名、零配置的：它只为单个请求沿一个维度重排供应商，仅此而已。
- **预设**（`@fast`）是命名、可保存的：它把基础模型、提示、参数和供应商约束一次性固化下来，于是调用方用名字调用一份经过验证的配置，而无需每次重复。

二者可组合——`@fast:cost` 先应用预设，再用内联变体覆盖它的路由 profile。
