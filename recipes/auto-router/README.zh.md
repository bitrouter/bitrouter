这个草案 Recipe 封装了可部署的 [`auto-router`](https://github.com/bitrouter/bitrouter/tree/main/templates/auto-router)
模板，不复制第二份配置或 policy lock。Catalog builder 会读取模板中的原始
artifact，并对照当前 Registry 校验 Provider、模型、Preset 与 Lock 引用，最后
将完全相同的文件写入 `dist/recipes/index.json`。

## 路由方式

Policy 消费通用的 `agent_trace/v2|<state>|<risk>` 投影。BitRouter 只从原生
OpenAI / Anthropic 兼容请求历史中推导该投影；benchmark ID、task ID、harness
身份和私有路由 Header 都不会成为决策 key。

- recovery、redo、precision 等受保护状态以及未知状态使用强模型档位；
- 只读 review 与长上下文执行状态可以使用均衡档位；
- 常规 edit、test 和 tool-follow-up 状态可以使用经济档位。

Serving 行为唯一由 policy lock 决定。`policy.mode: frozen` 保持确定性路由，
同时允许记录 observation 和接收外部 Eval；切换到 `adaptive` 也只允许显式发布
经过审查的 candidate，不会开启请求时在线学习。

## 相关机制证据

PR [#768](https://github.com/bitrouter/bitrouter/pull/768) 记录了两条独立、
严格验收的 Terminal-Bench 2.1 short13 lineage。每条 lineage 都使用全新的
policy database，并与固定 GPT-5.6-sol control 配对。这两条 lineage 评估的是
各自独立编译的 R3 lock，而不是本 Recipe 嵌入的当前 starter lock：

| Lineage | Control | Frozen `@auto` R3 | 配对成本变化 |
| --- | --- | --- | ---: |
| 1 | 10/13，$5.070774 | 11/13，$4.102794 | -19.09% |
| 2 | 11/13，$6.105430 | 11/13，$5.233069 | -14.29% |

原始合并数据为：通过数 21/26 对 22/26，名义模型总成本 $11.176204 对
$9.335863，即合计成本 -16.5%、准确率 +3.8 个百分点；两条配对成本变化的
均值是 -16.69%。

这些数据不会写入 `recipe.yaml`。当前 starter lock 将八条 route 保持为
compiler-owned experiment，而两条 accepted R3 lock 都带有独立的 admitted
evidence lineage 和更保守的 strong route。把 R3 结果归因到 starter artifact
会违反 Recipe catalog 的精确 artifact provenance 规则。

另有一条 lineage 因 Kimi K3 连续返回 503、504 和 429 而被拒绝并完整保留；
它不进入效果统计。该失败属于重要的可用性证据，而不是模型语义质量结果。

## 证据边界

因此本 Recipe 保持 `draft`，直到完全相同的 config 和 lock 完成 accepted
comparative evaluation。现有结果只是机制证据，不是公开排行榜结果，也不是跨
workload 承诺。成本指标是冻结的 Codex subscription 名义价格，与实际 Provider
费用分开记录。两条 accepted R3 中 171 个请求有 169 个仍由 GPT-5.6 完成，
当前节省主要来自轨迹和 token 效率，而不是大规模替换为弱模型。跨 Agent 质量
仍未验证。

在广泛发布前，应使用自己的 task-native outcome 评估 frozen policy；缺失结果
必须保持 unknown，被拒绝的尝试必须保留；只有 accepted run 的 config 与
policy-lock digest 和 catalog 完全一致时，Recipe 才能发布。
