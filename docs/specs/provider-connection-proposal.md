# 命名 Provider Connection

状态：**提案，供审阅。未实现其中的新行为。** 本文不是当前支持的新格式，也不构成兼容性承诺。要求用语：必须、不得、应当、可以，分别对应 MUST、MUST NOT、SHOULD、MAY。

当前只规定第一段：**用户把本地或在线的自定义端点接进现有网关。** 发布、relay、调度目的地和未知结果的持久记录都以后再写。它们不进入这一段的实现。

审阅通过之后，这一段若需要新的检查结果报文，并入 [服务 API](service-api/README.md)；行为边界并入 [网关指南](../development/gateway.md) 与 [CLI 参考](../CLI.md)。不新增配置对象，因此不另写一份字段目录。并入后删除本文。

## 实现顺序

| 顺序 | 功能 | 这一段 |
| --- | --- | --- |
| 1 | 接入：本地或在线 custom provider，沿用现有网关转发，并补上发现与检查 | 现在规定 |
| 2 | 调度目的地改为引用这条 provider，不再抄写 endpoint | 不规定 |
| 3 | 本地引擎上未知发送的持久记录 | 不规定 |
| 4 | 发布：出站隧道与自托管 relay | 不规定 |

第 2 到第 4 段的名字只标记边界，避免把发布的报文提前冻进这一段。

## 已经存在的接入

自定义端点今天就是 `providers.<id>`。本地和在线用同一组字段。`bro serve` 已经把 Chat Completions、Responses、Messages 的请求转到 `api_base`，再把响应流转回调用方。不需要 Cloud 账号。

```yaml
providers:
  ollama:
    api_base: "http://127.0.0.1:11434/v1"
    models:
      - { id: "llama3.1:8b" }

  groq:
    api_base: "https://api.groq.com/openai/v1"
    api_key: "${GROQ_API_KEY}"
    auto_discover: true
```

`ollama` 的端点在本机，`groq` 的端点在线上。两者都是消费 connection：id 就是 provider 名称，端点是 `api_base`，凭证是现有的 `api_key` 引用。本地端点可以没有密钥。模型要么写在 `models` 里，要么用 `auto_discover` 在启动和重载时拉取。协议用现有的 `api_protocol`；省略时保持今天的推断。

多 account 时，connection id 由 provider 名称和 account 标签组成。account 只覆盖凭证和可选的 `api_base`。模型、协议和限流仍属于 provider。

显式路由、`routers` 和注册表里的 provider 保持原样。`openai: {}` 这种继承注册表的条目继续由 `bro providers login` 管理。那个命令只认识带有既定登录方式的 provider，不用来登记任意 `api_base`。

`bro providers list` 里的 `active` 只表示该 provider 已进入路由配置。它不表示端点可达。

## 这一段要补的行为

这一段不新增 YAML 键，不新增推理入口，也不写第二份密钥。新行为只有对**已经写在配置里的** provider 做发现和检查。

操作者必须能够对一个 provider id：

1. 解析它的端点和凭证引用。凭证引用无法解析时，检查失败，并且不得改配置、不得发推理。
2. 按该 provider 现有的协议和超时，请求它的模型列表。
3. 得到一份结果：端点是否按该协议回答了列表，以及看到的模型 id。

检查不得提交生成请求，不得占用调度容量，不得因为失败就把该 provider 标成不可路由。模型列表成功只说明端点回答了列表，不证明每个模型都能完成生成。

发现到的模型默认只出现在检查结果里。写回 `bitrouter.yaml` 不属于这一段；今天已有的 `auto_discover` 仍只在启动和重载时生效。

本地 HTTP 和在线 HTTPS 走同一条检查。检查使用网关已经用来访问该 provider 的认证与超时，不另选一套客户端。

## 不在这一段

- 出站隧道、relay、发布令牌、调用方凭证。
- `inference_routing` 的目的地形状。
- 按 request id 去重，以及崩溃之后保留未知发送。
- vLLM 指标和容量估算。
- 新的 `providers add` 命令。任意端点继续由操作者写进现有的 `providers.<id>`。
- tenant 按请求换凭证、共享网关的跨进程调度、市场挂牌。

## 兼容性

没有新的配置键。今天的 `bitrouter.yaml` 对新旧读者含义不变。注册表 provider、账号标签和显式路由策略保持现在的解释。

检查是新的只读操作。旧客户端不调用它时，转发行为不变。检查结果的字段表在并入服务 API 或 CLI 输出契约时再审；这一段只规定上面的成败语义。

## 审阅焦点

1. 第一段是否只做「已有 provider 配置上的发现和检查」，本地和在线同一条规则。
2. 是否同意这一段不增加 YAML 键，也不增加 `providers add`。
3. 发现到的模型是否只报告、不写回配置。
