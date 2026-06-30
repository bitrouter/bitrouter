---
title: 命名空间
description: 将 API 密钥、策略和用量限定在单个部署内的隔离边界——适用于任何自托管或本地 BitRouter 节点。
sourceHash: ef661d25cfb34736fe3297d140ebb9fff2a7a22dc5d7009358630c6f1fe474d1
---

**命名空间**是 BitRouter 的隔离基元。API 密钥、策略和使用数据全部作用于单个命名空间。在自托管或本地节点上，你可以运行多个命名空间，将不同的项目、环境或 Agent 部署彼此隔离。

## 凭证模型：命名空间级密钥

BitRouter 签发的每个凭证都是**命名空间级（namespace-scoped）**的——在签发时即绑定到某一个命名空间。该绑定是永久的，事后无法扩大范围。

| 能力 | 命名空间级凭证 |
|---|---|
| **调用推理** | 是 |
| **管理命名空间内的密钥** | 是 |
| **管理命名空间内的策略** | 是 |
| **读取命名空间内的用量** | 是 |
| **访问其他命名空间** | 否 |

因为没有任何凭证可以跨越命名空间，一个被攻破的密钥或行为异常的 Agent 只会影响它所属的命名空间。爆炸半径从设计上就被严格限制。

## 列出与查看命名空间

查看当前会话绑定到哪个命名空间：

```bash
bitrouter cloud namespace current   # 离线读取本地凭证
bitrouter auth whoami               # 同时打印已绑定的命名空间
```

列出节点上的所有命名空间：

```bash
bitrouter cloud namespace list      # 所有命名空间，当前激活的标有 (active)
```

```
ns_01jxyz…  default       (active)
ns_01jabc…  production
ns_01jdef…  staging
```

完整的 `bitrouter cloud` 命令列表请参阅 [CLI 参考](/docs/concepts/cli)。

## 在命名空间内管理密钥

凭证对其绑定的命名空间拥有完整的操作权。它可以签发子密钥、列出现有密钥并撤销它们——同时无法获取对其他命名空间的访问权。

```bash
bitrouter cloud keys mint --name my-agent \
  --scope "inference:invoke keys:read policy:read usage:read"
bitrouter cloud keys list
bitrouter cloud keys revoke <id>
```

> [!NOTE]
> `bitrouter cloud keys mint` 是推荐 Agent 为其工具签发子密钥的方式。签发的密钥与调用方绑定到同一命名空间，且无法超出调用方自身的 scope 范围。

## 策略

护栏、限速和预设策略绑定同样限定于命名空间范围。凭证可在其所属命名空间内读写策略：

```bash
bitrouter cloud policy list
bitrouter cloud policy bind <policy-id> --principal-type api_key --principal-id <key-id>
bitrouter cloud budget create --name daily-cap --window day --limit-micro-usd 5000000
```

策略的编写方式请参阅[护栏](/docs/features/guardrails)与[预设](/docs/features/presets)文档。

## 用量报告

命名空间维度的请求归因在请求时完成，可通过 CLI 查询：

```bash
bitrouter cloud usage                                          # 最近 30 天
bitrouter cloud usage --from 2026-05-01T00:00:00Z --to 2026-06-01T00:00:00Z
bitrouter cloud requests --limit 25                           # 分页请求日志
```

使用命名空间级凭证的 Agent 或 CI 任务只能读取自身命名空间的用量——跨命名空间聚合需要由节点运维人员签发更宽泛的凭证。
